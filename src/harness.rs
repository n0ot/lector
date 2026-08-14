use crate::{
    app::{self, App, Clock},
    screen_reader::ScreenReader,
    speech,
    terminal::TerminalGeometry,
    views,
};
use anyhow::{Result, anyhow, bail};
use std::fmt::Write as FmtWrite;
use std::{
    cell::{Cell, RefCell},
    fs,
    io::{self, Read},
    rc::Rc,
};

#[derive(Clone, Default)]
pub struct FakeClock {
    now: Rc<Cell<u128>>,
}

impl FakeClock {
    pub fn advance_ms(&self, delta: u128) {
        self.now.set(self.now.get().saturating_add(delta));
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u128 {
        self.now.get()
    }
}

#[derive(Default)]
struct SpeechLog {
    speaks: Vec<(String, bool)>,
    stops: usize,
}

#[derive(Clone, Default)]
struct SpeechRecorder {
    inner: Rc<RefCell<SpeechLog>>,
}

struct HarnessDriver {
    recorder: SpeechRecorder,
}

impl speech::Driver for HarnessDriver {
    fn speak(&mut self, text: &str, interrupt: bool) -> Result<()> {
        self.recorder
            .inner
            .borrow_mut()
            .speaks
            .push((text.to_string(), interrupt));
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.recorder.inner.borrow_mut().stops += 1;
        Ok(())
    }

    fn get_rate(&self) -> f32 {
        0.0
    }

    fn set_rate(&mut self, _rate: f32) -> Result<()> {
        Ok(())
    }
}

pub struct Harness {
    app: App,
    sr: ScreenReader,
    clock: FakeClock,
    pty_out: Vec<u8>,
    term_out: Vec<u8>,
    speak_log: SpeechRecorder,
    pty_cursor: usize,
    term_cursor: usize,
    speak_cursor: usize,
    rows: u16,
    cols: u16,
}

impl Harness {
    pub fn new(rows: u16, cols: u16) -> Result<Self> {
        let recorder = SpeechRecorder::default();
        let driver = HarnessDriver {
            recorder: recorder.clone(),
        };
        let speech = speech::Speech::new(Box::new(driver));
        let sr = ScreenReader::new(speech);
        let view_stack = views::ViewStack::new(Box::new(views::PtyView::new(rows, cols)));
        let clock = FakeClock::default();
        let app = App::new_with_clock(view_stack, Box::new(clock.clone()))?;
        Ok(Self {
            app,
            sr,
            clock,
            pty_out: Vec::new(),
            term_out: Vec::new(),
            speak_log: recorder,
            pty_cursor: 0,
            term_cursor: 0,
            speak_cursor: 0,
            rows,
            cols,
        })
    }

    pub fn run_script(&mut self, script: &str) -> Result<()> {
        let mut scenario_seen = false;
        let mut phase = BddPhase::Given;
        let mut last_prefix: Option<BddPrefix> = None;
        for (line_no, line) in script.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(name) = parse_scenario(line) {
                scenario_seen = true;
                self.reset()?;
                phase = BddPhase::Given;
                last_prefix = None;
                let _ = name;
                continue;
            }
            if !scenario_seen {
                return Err(anyhow!("line {}: missing Scenario header", line_no + 1));
            }
            let (prefix, line) = parse_bdd_prefix(line, line_no + 1)?;
            let prefix = match prefix {
                BddPrefix::And => last_prefix.ok_or_else(|| {
                    anyhow!(
                        "line {}: And without a previous Given/When/Then",
                        line_no + 1
                    )
                })?,
                _ => prefix,
            };
            last_prefix = Some(prefix);
            phase = match (phase, prefix) {
                (BddPhase::Given, BddPrefix::Given) => BddPhase::Given,
                (BddPhase::Given, BddPrefix::When) => BddPhase::When,
                (BddPhase::Given, BddPrefix::Then) => BddPhase::Then,
                (BddPhase::When, BddPrefix::When) => BddPhase::When,
                (BddPhase::When, BddPrefix::Then) => BddPhase::Then,
                (BddPhase::Then, BddPrefix::Then) => BddPhase::Then,
                (BddPhase::When, BddPrefix::Given) => {
                    return Err(anyhow!(
                        "line {}: Given is not allowed after When",
                        line_no + 1
                    ));
                }
                (BddPhase::Then, BddPrefix::Given | BddPrefix::When) => {
                    return Err(anyhow!(
                        "line {}: Given/When is not allowed after Then",
                        line_no + 1
                    ));
                }
                (_, BddPrefix::And) => unreachable!("And should be normalized above"),
            };
            let (cmd, rest) = line
                .split_once(':')
                .ok_or_else(|| anyhow!("line {}: missing ':'", line_no + 1))?;
            let payload = rest.trim_start();
            if matches!(phase, BddPhase::Then) && !is_assert_command(cmd) {
                return Err(anyhow!(
                    "line {}: Then/And must use an assertion command",
                    line_no + 1
                ));
            }
            if !matches!(phase, BddPhase::Then) && is_assert_command(cmd) {
                return Err(anyhow!(
                    "line {}: assertion commands are only allowed after Then",
                    line_no + 1
                ));
            }
            let result = (|| -> Result<()> {
                match cmd {
                    "stdin" => {
                        let bytes = parse_bytes(payload)?;
                        self.app.handle_stdin(
                            &mut self.sr,
                            &bytes,
                            &mut self.pty_out,
                            &mut self.term_out,
                        )?;
                        Ok(())
                    }
                    "pty-stdout" => {
                        let bytes = parse_bytes(payload)?;
                        self.app
                            .handle_pty(&mut self.sr, &bytes, &mut self.term_out)?;
                        Ok(())
                    }
                    "settled" => {
                        self.clock.advance_ms(app::DIFF_DELAY as u128 + 1);
                        let _ = self.app.maybe_finalize_changes(&mut self.sr)?;
                        Ok(())
                    }
                    "tick" => {
                        let delta = if payload.is_empty() {
                            0
                        } else {
                            payload
                                .parse::<u128>()
                                .map_err(|_| anyhow!("line {}: invalid tick value", line_no + 1))?
                        };
                        self.clock.advance_ms(delta);
                        self.app.handle_tick(
                            &mut self.sr,
                            &mut self.pty_out,
                            &mut self.term_out,
                        )?;
                        let _ = self.app.maybe_finalize_changes(&mut self.sr)?;
                        Ok(())
                    }
                    "advance" => {
                        let delta = payload
                            .parse::<u128>()
                            .map_err(|_| anyhow!("line {}: invalid advance value", line_no + 1))?;
                        self.clock.advance_ms(delta);
                        Ok(())
                    }
                    "auto-read" => {
                        self.sr.set_auto_read_enabled(parse_switch(payload)?);
                        Ok(())
                    }
                    "suppress-key-echo" => {
                        self.sr.set_suppress_key_echo(parse_switch(payload)?);
                        Ok(())
                    }
                    "clear-speech" => {
                        self.speak_log.inner.borrow_mut().speaks.clear();
                        self.speak_cursor = 0;
                        Ok(())
                    }
                    "finalize" => {
                        let _ = self.app.maybe_finalize_changes(&mut self.sr)?;
                        Ok(())
                    }
                    "resize" => {
                        let mut parts = payload.split_whitespace();
                        let rows = parts
                            .next()
                            .ok_or_else(|| anyhow!("line {}: missing rows", line_no + 1))?
                            .parse::<u16>()
                            .map_err(|_| anyhow!("line {}: invalid rows", line_no + 1))?;
                        let cols = parts
                            .next()
                            .ok_or_else(|| anyhow!("line {}: missing cols", line_no + 1))?
                            .parse::<u16>()
                            .map_err(|_| anyhow!("line {}: invalid cols", line_no + 1))?;
                        let cell_width_px = parts
                            .next()
                            .map(|value| value.parse::<u32>())
                            .transpose()
                            .map_err(|_| {
                                anyhow!("line {}: invalid cell pixel width", line_no + 1)
                            })?;
                        let cell_height_px = parts
                            .next()
                            .map(|value| value.parse::<u32>())
                            .transpose()
                            .map_err(|_| {
                                anyhow!("line {}: invalid cell pixel height", line_no + 1)
                            })?;
                        if parts.next().is_some()
                            || cell_width_px.is_some() != cell_height_px.is_some()
                        {
                            bail!(
                                "line {}: resize expects rows cols and optional cell-width-px cell-height-px",
                                line_no + 1
                            );
                        }
                        self.app.on_resize_with_geometry(
                            TerminalGeometry::new(
                                rows,
                                cols,
                                cell_width_px.unwrap_or(0),
                                cell_height_px.unwrap_or(0),
                            ),
                            &mut self.term_out,
                        )?;
                        Ok(())
                    }
                    "expect-pty-stdin" => {
                        let expected = parse_bytes(payload)?;
                        consume_expected(
                            &self.pty_out,
                            &mut self.pty_cursor,
                            &expected,
                            "pty-stdin",
                            line_no + 1,
                        )?;
                        Ok(())
                    }
                    "expect-no-pty-stdin" => {
                        let remaining = &self.pty_out[self.pty_cursor..];
                        if !remaining.is_empty() {
                            bail!(
                                "line {}: expected no PTY stdin, got {:?}",
                                line_no + 1,
                                remaining
                            );
                        }
                        Ok(())
                    }
                    "expect-stdout" => {
                        let expected = parse_bytes(payload)?;
                        consume_expected(
                            &self.term_out,
                            &mut self.term_cursor,
                            &expected,
                            "stdout",
                            line_no + 1,
                        )?;
                        Ok(())
                    }
                    "expect-stdout-contains" => {
                        let expected = parse_bytes(payload)?;
                        let remaining = &self.term_out[self.term_cursor..];
                        if !remaining.windows(expected.len()).any(|w| w == expected) {
                            bail!(
                                "line {}: stdout does not contain {:?}",
                                line_no + 1,
                                expected
                            );
                        }
                        Ok(())
                    }
                    "expect-speak" => {
                        let expected = parse_text(payload)?;
                        let (text, _interrupt) = self
                            .next_speak(line_no + 1)
                            .ok_or_else(|| anyhow!("line {}: no speech", line_no + 1))?;
                        if text != expected {
                            bail!(
                                "line {}: expected speech {:?}, got {:?}",
                                line_no + 1,
                                expected,
                                text
                            );
                        }
                        Ok(())
                    }
                    "expect-speak-contains" => {
                        let expected = parse_text(payload)?;
                        let (text, _interrupt) = self
                            .next_speak(line_no + 1)
                            .ok_or_else(|| anyhow!("line {}: no speech", line_no + 1))?;
                        if !text.contains(&expected) {
                            bail!(
                                "line {}: expected speech containing {:?}, got {:?}",
                                line_no + 1,
                                expected,
                                text
                            );
                        }
                        Ok(())
                    }
                    "expect-no-speak" => {
                        if let Some((text, interrupt)) = self.next_speak(line_no + 1) {
                            bail!(
                                "line {}: expected no speech, got {:?} (interrupt={})",
                                line_no + 1,
                                text,
                                interrupt
                            );
                        }
                        Ok(())
                    }
                    "expect-screen-contains" => {
                        let expected = parse_text(payload)?;
                        let actual = self.app.debug_active_view_contents();
                        if !actual.contains(&expected) {
                            bail!(
                                "line {}: active screen does not contain {:?}",
                                line_no + 1,
                                expected
                            );
                        }
                        Ok(())
                    }
                    "expect-root-geometry" => {
                        let expected = parse_geometry(payload, line_no + 1)?;
                        let actual = self.app.debug_root_terminal_geometry();
                        if actual != expected {
                            bail!(
                                "line {}: expected root geometry {:?}, got {:?}",
                                line_no + 1,
                                expected,
                                actual
                            );
                        }
                        Ok(())
                    }
                    "expect-stops" => {
                        let expected = payload
                            .parse::<usize>()
                            .map_err(|_| anyhow!("line {}: invalid stop count", line_no + 1))?;
                        let actual = self.speak_log.inner.borrow().stops;
                        if actual != expected {
                            bail!(
                                "line {}: expected {} stops, got {}",
                                line_no + 1,
                                expected,
                                actual
                            );
                        }
                        Ok(())
                    }
                    _ => Err(anyhow!("line {}: unknown command {}", line_no + 1, cmd)),
                }
            })();
            if let Err(err) = result {
                return Err(anyhow!("{}\n\n{}", err, self.dump_state()));
            }
        }
        Ok(())
    }

    /// Sends one application-output chunk through the same path used by a
    /// harness script. This is useful for byte-boundary and render-oracle
    /// tests that need to retain the original chunk boundaries.
    pub fn handle_pty_output(&mut self, bytes: &[u8]) -> Result<()> {
        self.app.handle_pty(&mut self.sr, bytes, &mut self.term_out)
    }

    /// Returns every presentation byte emitted by the application so far.
    pub fn terminal_output(&self) -> &[u8] {
        &self.term_out
    }

    fn next_speak(&mut self, _line_no: usize) -> Option<(String, bool)> {
        let log = self.speak_log.inner.borrow();
        if self.speak_cursor >= log.speaks.len() {
            return None;
        }
        let entry = log.speaks[self.speak_cursor].clone();
        self.speak_cursor += 1;
        Some(entry)
    }

    fn reset(&mut self) -> Result<()> {
        let rows = self.rows;
        let cols = self.cols;
        *self = Harness::new(rows, cols)?;
        Ok(())
    }

    fn dump_state(&self) -> String {
        let pty_remaining = format_bytes_remaining(&self.pty_out, self.pty_cursor);
        let term_remaining = format_bytes_remaining(&self.term_out, self.term_cursor);
        let speaks = self.speak_log.inner.borrow();
        let mut remaining_speech = String::new();
        for (idx, (text, interrupt)) in speaks.speaks.iter().enumerate().skip(self.speak_cursor) {
            let _ = writeln!(
                &mut remaining_speech,
                "{}: {:?} (interrupt={})",
                idx, text, interrupt
            );
        }
        if remaining_speech.is_empty() {
            remaining_speech = "<none>\n".to_string();
        }
        format!(
            "State:\npty-stdin-remaining: {}\nstdout-remaining: {}\nspeech-remaining:\n{}stops: {}\n",
            pty_remaining, term_remaining, remaining_speech, speaks.stops
        )
    }
}

pub fn run_script_file(path: &str) -> Result<()> {
    let contents = fs::read_to_string(path)?;
    let mut harness = Harness::new(24, 80)?;
    harness.run_script(&contents)
}

pub fn run_script_stdin() -> Result<()> {
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf)?;
    let mut harness = Harness::new(24, 80)?;
    harness.run_script(&buf)
}

fn consume_expected(
    buffer: &[u8],
    cursor: &mut usize,
    expected: &[u8],
    name: &str,
    line_no: usize,
) -> Result<()> {
    if buffer.len().saturating_sub(*cursor) < expected.len() {
        bail!(
            "line {}: {} output too short (need {}, have {})",
            line_no,
            name,
            expected.len(),
            buffer.len().saturating_sub(*cursor)
        );
    }
    let actual = &buffer[*cursor..*cursor + expected.len()];
    if actual != expected {
        bail!(
            "line {}: {} output mismatch: expected {:?}, got {:?}",
            line_no,
            name,
            expected,
            actual
        );
    }
    *cursor += expected.len();
    Ok(())
}

fn parse_text(input: &str) -> Result<String> {
    let bytes = parse_bytes(input)?;
    String::from_utf8(bytes).map_err(|e| anyhow!(e.to_string()))
}

fn parse_bytes(input: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            let mut encoded = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut encoded).as_bytes());
            continue;
        }
        let esc = chars.next().ok_or_else(|| anyhow!("trailing escape"))?;
        match esc {
            'n' => out.push(b'\n'),
            'r' => out.push(b'\r'),
            't' => out.push(b'\t'),
            '\\' => out.push(b'\\'),
            'x' => {
                let hi = chars.next().ok_or_else(|| anyhow!("invalid \\x escape"))?;
                let lo = chars.next().ok_or_else(|| anyhow!("invalid \\x escape"))?;
                let hex = [hi, lo].iter().collect::<String>();
                let byte =
                    u8::from_str_radix(&hex, 16).map_err(|_| anyhow!("invalid \\x escape"))?;
                out.push(byte);
            }
            _ => return Err(anyhow!("unknown escape \\{}", esc)),
        }
    }
    Ok(out)
}

fn parse_switch(input: &str) -> Result<bool> {
    match input {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(anyhow!("expected on or off, got {:?}", input)),
    }
}

#[derive(Copy, Clone)]
enum BddPrefix {
    Given,
    When,
    Then,
    And,
}

#[derive(Copy, Clone)]
enum BddPhase {
    Given,
    When,
    Then,
}

fn parse_bdd_prefix(line: &str, line_no: usize) -> Result<(BddPrefix, &str)> {
    for (prefix, kind) in [
        ("given", BddPrefix::Given),
        ("when", BddPrefix::When),
        ("then", BddPrefix::Then),
        ("and", BddPrefix::And),
    ] {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with(prefix)
            && line[prefix.len()..]
                .chars()
                .next()
                .is_some_and(char::is_whitespace)
        {
            let rest = line[prefix.len()..].trim_start();
            if !rest.is_empty() {
                return Ok((kind, rest));
            }
        }
    }
    Err(anyhow!(
        "line {}: missing BDD prefix (Given/When/Then/And)",
        line_no
    ))
}

fn parse_scenario(line: &str) -> Option<&str> {
    let lower = line.to_ascii_lowercase();
    if !lower.starts_with("scenario") {
        return None;
    }
    let rest = line["scenario".len()..].trim_start();
    if let Some(rest) = rest.strip_prefix(':') {
        let rest = rest.trim_start();
        return Some(rest);
    }
    None
}

fn is_assert_command(cmd: &str) -> bool {
    matches!(
        cmd,
        "expect-pty-stdin"
            | "expect-no-pty-stdin"
            | "expect-stdout"
            | "expect-stdout-contains"
            | "expect-speak"
            | "expect-speak-contains"
            | "expect-no-speak"
            | "expect-screen-contains"
            | "expect-root-geometry"
            | "expect-stops"
    )
}

fn parse_geometry(payload: &str, line_no: usize) -> Result<TerminalGeometry> {
    let values = payload
        .split_whitespace()
        .map(|value| {
            value
                .parse::<u32>()
                .map_err(|_| anyhow!("line {line_no}: invalid geometry value"))
        })
        .collect::<Result<Vec<_>>>()?;
    let [rows, cols, cell_width_px, cell_height_px] = values.as_slice() else {
        bail!("line {line_no}: geometry expects rows cols cell-width-px cell-height-px");
    };
    Ok(TerminalGeometry::new(
        (*rows)
            .try_into()
            .map_err(|_| anyhow!("line {line_no}: rows exceed u16"))?,
        (*cols)
            .try_into()
            .map_err(|_| anyhow!("line {line_no}: cols exceed u16"))?,
        *cell_width_px,
        *cell_height_px,
    ))
}

fn format_bytes_remaining(buffer: &[u8], cursor: usize) -> String {
    const LIMIT: usize = 256;
    let remaining = &buffer[cursor..];
    let shown = &remaining[..remaining.len().min(LIMIT)];
    let mut out = String::new();
    for &b in shown {
        match b {
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7E => out.push(b as char),
            _ => {
                let _ = write!(&mut out, "\\x{:02X}", b);
            }
        }
    }
    if remaining.len() > LIMIT {
        let _ = write!(&mut out, "... ({} bytes more)", remaining.len() - LIMIT);
    }
    if out.is_empty() {
        out.push_str("<none>");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        BddPrefix, FakeClock, Harness, HarnessDriver, SpeechRecorder, consume_expected,
        format_bytes_remaining, is_assert_command, parse_bdd_prefix, parse_bytes, parse_scenario,
        parse_switch, parse_text,
    };
    use crate::{app::Clock, speech::Driver};

    #[test]
    fn byte_parser_supports_utf8_and_every_escape_form() {
        assert_eq!(
            parse_bytes(r"é\n\r\t\\\x1B").unwrap(),
            "é\n\r\t\\\x1B".as_bytes()
        );
        assert_eq!(parse_text("hello").unwrap(), "hello");

        for (input, message) in [
            ("\\", "trailing escape"),
            ("\\q", "unknown escape"),
            ("\\x", "invalid \\x escape"),
            ("\\x0", "invalid \\x escape"),
            ("\\xGG", "invalid \\x escape"),
        ] {
            assert!(
                parse_bytes(input)
                    .unwrap_err()
                    .to_string()
                    .contains(message)
            );
        }
    }

    #[test]
    fn bdd_and_scenario_parsers_require_complete_tokens() {
        for (line, expected) in [
            ("Given stdin: a", "stdin: a"),
            ("WHEN tick:", "tick:"),
            ("then expect-stops: 0", "expect-stops: 0"),
            ("And finalize:", "finalize:"),
        ] {
            let (prefix, rest) = parse_bdd_prefix(line, 7).unwrap();
            assert_eq!(rest, expected);
            assert!(matches!(
                (line.to_ascii_lowercase().split_whitespace().next(), prefix),
                (Some("given"), BddPrefix::Given)
                    | (Some("when"), BddPrefix::When)
                    | (Some("then"), BddPrefix::Then)
                    | (Some("and"), BddPrefix::And)
            ));
        }
        for line in ["Given", "Givenly stdin: a", "unknown stdin: a"] {
            assert!(
                parse_bdd_prefix(line, 7)
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("line 7")
            );
        }

        assert_eq!(parse_scenario("Scenario: name"), Some("name"));
        assert_eq!(parse_scenario("SCENARIO : spaced"), Some("spaced"));
        assert_eq!(parse_scenario("Scenarios: nope"), None);
        assert_eq!(parse_scenario("Scenario name"), None);
        assert!(parse_switch("on").unwrap());
        assert!(!parse_switch("off").unwrap());
        assert!(parse_switch("true").is_err());
    }

    #[test]
    fn assertion_classification_is_exhaustive() {
        for command in [
            "expect-pty-stdin",
            "expect-no-pty-stdin",
            "expect-stdout",
            "expect-stdout-contains",
            "expect-speak",
            "expect-speak-contains",
            "expect-no-speak",
            "expect-screen-contains",
            "expect-root-geometry",
            "expect-stops",
        ] {
            assert!(is_assert_command(command));
        }
        for command in ["stdin", "tick", "expect-unknown"] {
            assert!(!is_assert_command(command));
        }
    }

    #[test]
    fn expected_output_consumption_checks_length_content_and_cursor() {
        let output = b"abcdef";
        let mut cursor = 0;
        consume_expected(output, &mut cursor, b"abc", "test", 3).unwrap();
        assert_eq!(cursor, 3);
        consume_expected(output, &mut cursor, b"def", "test", 4).unwrap();
        assert_eq!(cursor, 6);

        let too_short = consume_expected(output, &mut cursor, b"x", "test", 5).unwrap_err();
        assert!(too_short.to_string().contains("output too short"));
        assert_eq!(cursor, 6);

        cursor = 0;
        let mismatch = consume_expected(output, &mut cursor, b"abd", "test", 6).unwrap_err();
        assert!(mismatch.to_string().contains("output mismatch"));
        assert_eq!(cursor, 0);
    }

    #[test]
    fn remaining_byte_formatting_escapes_controls_and_limits_diagnostics() {
        assert_eq!(
            format_bytes_remaining(b"a\n\r\t\\\x1B", 0),
            "a\\n\\r\\t\\\\\\x1B"
        );
        assert_eq!(format_bytes_remaining(b"abc", 3), "<none>");

        let long = vec![b'x'; 260];
        let formatted = format_bytes_remaining(&long, 0);
        assert_eq!(formatted.matches('x').count(), 256);
        assert!(formatted.ends_with("... (4 bytes more)"));
    }

    fn script_error(script: &str) -> String {
        Harness::new(4, 20)
            .unwrap()
            .run_script(script)
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn scripts_enforce_scenario_bdd_phase_and_command_grammar() {
        for (script, expected) in [
            ("When stdin: a", "missing Scenario header"),
            ("Scenario: x\nAnd stdin: a", "And without a previous"),
            ("Scenario: x\nstdin: a", "missing BDD prefix"),
            ("Scenario: x\nWhen stdin a", "missing ':'"),
            (
                "Scenario: x\nWhen stdin: a\nGiven pty-stdout: a",
                "Given is not allowed after When",
            ),
            (
                "Scenario: x\nThen expect-stops: 0\nWhen stdin: a",
                "Given/When is not allowed after Then",
            ),
            (
                "Scenario: x\nGiven expect-stops: 0",
                "assertion commands are only allowed after Then",
            ),
            (
                "Scenario: x\nThen stdin: a",
                "Then/And must use an assertion command",
            ),
            ("Scenario: x\nWhen unknown: a", "unknown command unknown"),
        ] {
            assert!(script_error(script).contains(expected), "script={script:?}");
        }
    }

    #[test]
    fn scripts_validate_numeric_arguments_and_include_state_on_command_failures() {
        for (script, expected) in [
            ("Scenario: x\nWhen tick: later", "invalid tick value"),
            ("Scenario: x\nWhen advance: later", "invalid advance value"),
            ("Scenario: x\nGiven resize:", "missing rows"),
            ("Scenario: x\nGiven resize: x 2", "invalid rows"),
            ("Scenario: x\nGiven resize: 2", "missing cols"),
            ("Scenario: x\nGiven resize: 2 x", "invalid cols"),
            (
                "Scenario: x\nThen expect-pty-stdin: missing",
                "pty-stdin output too short",
            ),
            (
                "Scenario: x\nWhen stdin: x\nThen expect-no-pty-stdin:",
                "expected no PTY stdin",
            ),
            ("Scenario: x\nThen expect-speak: missing", "no speech"),
            ("Scenario: x\nThen expect-stops: nope", "invalid stop count"),
        ] {
            let error = script_error(script);
            assert!(error.contains(expected), "error={error:?}");
            if expected != "invalid tick value"
                && expected != "invalid advance value"
                && !expected.starts_with("missing ")
                && !expected.starts_with("invalid rows")
                && !expected.starts_with("invalid cols")
            {
                assert!(error.contains("State:"));
            }
        }
    }

    #[test]
    fn multiple_scenarios_reset_state_and_and_reuses_the_previous_prefix() {
        let script = r#"
            Scenario: first
            When stdin: a
            Then expect-pty-stdin: a

            Scenario: second
            Given advance: 50
            And resize: 3 10
            When stdin: b
            And tick:
            And finalize:
            Then expect-pty-stdin: b
            And expect-stops: 1
        "#;
        Harness::new(4, 20).unwrap().run_script(script).unwrap();
    }

    #[test]
    fn fake_clock_saturates_and_harness_driver_control_methods_are_inert() {
        let clock = FakeClock::default();
        clock.advance_ms(u128::MAX);
        clock.advance_ms(1);
        assert_eq!(clock.now_ms(), u128::MAX);

        let recorder = SpeechRecorder::default();
        let mut driver = HarnessDriver {
            recorder: recorder.clone(),
        };
        assert_eq!(driver.get_rate(), 0.0);
        driver.set_rate(2.0).unwrap();
        driver.stop().unwrap();
        driver.speak("text", true).unwrap();
        let log = recorder.inner.borrow();
        assert_eq!(log.stops, 1);
        assert_eq!(log.speaks.as_slice(), [("text".into(), true)]);
    }
}
