use crate::{app::{self, App, Clock}, screen_reader::ScreenReader, speech, views};
use anyhow::{Result, anyhow, bail};
use std::{
    cell::{Cell, RefCell},
    fs,
    io::{self, Read},
    rc::Rc,
};
use std::fmt::Write as FmtWrite;

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
                return Err(anyhow!(
                    "line {}: missing Scenario header",
                    line_no + 1
                ));
            }
            let (prefix, line) = parse_bdd_prefix(line, line_no + 1)?;
            let prefix = match prefix {
                BddPrefix::And => last_prefix.ok_or_else(|| {
                    anyhow!("line {}: And without a previous Given/When/Then", line_no + 1)
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
            let result = match cmd {
                "stdin" => {
                    let bytes = parse_bytes(payload)?;
                    self.app
                        .handle_stdin(&mut self.sr, &bytes, &mut self.pty_out, &mut self.term_out)?;
                    Ok(())
                }
                "pty-stdout" => {
                    let bytes = parse_bytes(payload)?;
                    self.app.handle_pty(&mut self.sr, &bytes, &mut self.term_out)?;
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
                        payload.parse::<u128>().map_err(|_| {
                            anyhow!("line {}: invalid tick value", line_no + 1)
                        })?
                    };
                    self.clock.advance_ms(delta);
                    self.app
                        .handle_tick(&mut self.sr, &mut self.pty_out, &mut self.term_out)?;
                    let _ = self.app.maybe_finalize_changes(&mut self.sr)?;
                    Ok(())
                }
                "advance" => {
                    let delta = payload.parse::<u128>().map_err(|_| {
                        anyhow!("line {}: invalid advance value", line_no + 1)
                    })?;
                    self.clock.advance_ms(delta);
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
                    self.app
                        .on_resize(rows, cols, &mut self.term_out)?;
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
                "expect-stops" => {
                    let expected = payload.parse::<usize>().map_err(|_| {
                        anyhow!("line {}: invalid stop count", line_no + 1)
                    })?;
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
            };
            if let Err(err) = result {
                return Err(anyhow!("{}\n\n{}", err, self.dump_state()));
            }
        }
        Ok(())
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
            let _ = write!(
                &mut remaining_speech,
                "{}: {:?} (interrupt={})\n",
                idx,
                text,
                interrupt
            );
        }
        if remaining_speech.is_empty() {
            remaining_speech = "<none>\n".to_string();
        }
        format!(
            "State:\npty-stdin-remaining: {}\nstdout-remaining: {}\nspeech-remaining:\n{}stops: {}\n",
            pty_remaining,
            term_remaining,
            remaining_speech,
            speaks.stops
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
            out.push(ch as u8);
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
                let byte = u8::from_str_radix(&hex, 16)
                    .map_err(|_| anyhow!("invalid \\x escape"))?;
                out.push(byte);
            }
            _ => return Err(anyhow!("unknown escape \\{}", esc)),
        }
    }
    Ok(out)
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
        if lower.starts_with(prefix) {
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
            | "expect-stdout"
            | "expect-stdout-contains"
            | "expect-speak"
            | "expect-speak-contains"
            | "expect-stops"
    )
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
        let _ = write!(
            &mut out,
            "... ({} bytes more)",
            remaining.len() - LIMIT
        );
    }
    if out.is_empty() {
        out.push_str("<none>");
    }
    out
}
