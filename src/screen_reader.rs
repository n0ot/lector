use super::{attributes, ext::ScreenExt, perform, speech};
use anyhow::{anyhow, bail, Context, Result};
use nix::sys::termios;
use phf::phf_map;
use ptyprocess::PtyProcess;
use signal_hook::consts::signal::*;
use signal_hook_mio::v0_8::Signals;
use similar::{utils::diff_graphemes, Algorithm, ChangeTag, TextDiff};
use std::{
    cmp::min,
    io::{ErrorKind, Read, Write},
    os::fd::AsRawFd,
    process::Command,
    time,
};

const DIFF_DELAY: u16 = 5;

#[derive(Copy, Clone)]
enum Action {
    ToggleHelp,
    StopSpeaking,
    RevLinePrev,
    RevLineNext,
    RevLineRead,
    RevCharPrev,
    RevCharNext,
    RevCharRead,
    RevCharReadPhonetic,
    RevWordPrev,
    RevWordNext,
    RevWordRead,
    RevTop,
    RevBottom,
    RevFirst,
    RevLast,
    RevReadAttributes,
    Backspace,
    Delete,
    SayTime,
}

impl Action {
    fn help_text(&self) -> String {
        match self {
            Action::ToggleHelp => "toggle help".into(),
            Action::StopSpeaking => "stop speaking".into(),
            Action::RevLinePrev => "previous line".into(),
            Action::RevLineNext => "next line".into(),
            Action::RevLineRead => "current line".into(),
            Action::RevCharPrev => "previous character".into(),
            Action::RevCharNext => "next character".into(),
            Action::RevCharRead => "current character".into(),
            Action::RevCharReadPhonetic => "current character phonetically".into(),
            Action::RevWordPrev => "previous word".into(),
            Action::RevWordNext => "next word".into(),
            Action::RevWordRead => "current word".into(),
            Action::RevTop => "top".into(),
            Action::RevBottom => "botom".into(),
            Action::RevFirst => "beginning of line".into(),
            Action::RevLast => "end of line".into(),
            Action::RevReadAttributes => "read attributes".into(),
            Action::Backspace => "backspace".into(),
            Action::Delete => "delete".into(),
            Action::SayTime => "say the time".into(),
        }
    }
}

static KEYMAP: phf::Map<&'static str, Action> = phf_map! {
    "\x1BOP" => Action::ToggleHelp,
    "\x1Bx" => Action::StopSpeaking,
    "\x1Bu" => Action::RevLinePrev,
    "\x1Bo" => Action::RevLineNext,
    "\x1Bi" => Action::RevLineRead,
    "\x1Bm" => Action::RevCharPrev,
    "\x1B." => Action::RevCharNext,
    "\x1B," => Action::RevCharRead,
    "\x1B<" => Action::RevCharReadPhonetic,
    "\x1Bj" => Action::RevWordPrev,
    "\x1Bl" => Action::RevWordNext,
    "\x1Bk" => Action::RevWordRead,
    "\x1By" => Action::RevTop,
    "\x1Bp" => Action::RevBottom,
    "\x1Bh" => Action::RevFirst,
    "\x1B;" => Action::RevLast,
    "\x1Ba" => Action::RevReadAttributes,
    "\x08" => Action::Backspace,
    "\x7F" => Action::Backspace,
    "\x1B[3~" => Action::Delete,
    "\x1B[24~" => Action::SayTime,
};

struct ScreenState {
    screen: vt100::Screen,
    prev_screen: vt100::Screen,
    prev_screen_time: time::Instant,
    review_cursor_position: (u16, u16), // (row, col)
    review_cursor_last_indent_level: u16,
    last_indent_level: u16,
}

impl ScreenState {
    /// Moves the review cursor up a line.
    /// This method will return true only if the cursor moved.
    fn review_cursor_up(&mut self) -> bool {
        if self.review_cursor_position.0 > 0 {
            self.review_cursor_position.0 -= 1;
            true
        } else {
            false
        }
    }

    /// Moves the review cursor down a line.
    /// This method will return true only if the cursor moved.
    fn review_cursor_down(&mut self) -> bool {
        if self.review_cursor_position.0 < self.screen.size().0 - 1 {
            self.review_cursor_position.0 += 1;
            true
        } else {
            false
        }
    }

    /// Moves the cursor to the start of the previous word,
    /// or the beginning of the line if the cursor is in or before the first word.
    /// This method will return true only if the cursor moved to a different word.
    fn review_cursor_prev_word(&mut self) -> bool {
        let (row, col) = self.review_cursor_position;
        // First, find the beginning of this word.
        let col = self.screen.find_word_start(row, col);
        if col == 0 {
            // The current word was the first.
            // Just move to the beginning of the line.
            self.review_cursor_position.1 = 0;
            return false;
        }

        // Now, find the start of the previous word and move to it.
        let col = self.screen.find_word_start(row, col - 1);
        self.review_cursor_position.1 = col;
        true
    }

    /// Moves the cursor to the start of the next word,
    /// or the end of the line if the cursor is in or past the last word.
    /// This method will return true only if the cursor moved to a different word.
    fn review_cursor_next_word(&mut self) -> bool {
        let last = self.screen.size().1 - 1;
        let (row, col) = self.review_cursor_position;
        // First, find the end of this word.
        let col = self.screen.find_word_end(row, col);
        if col >= last {
            // The current word was the last.
            return false;
        }

        self.review_cursor_position.1 = col + 1;
        true
    }

    /// Moves the review cursor left a column.
    /// If the next cell continues a wide character, it will be skipped.
    /// This method will return true only if the cursor moved.
    fn review_cursor_left(&mut self) -> bool {
        if self.review_cursor_position.1 == 0 {
            return false;
        }
        self.review_cursor_position.1 -= 1;
        if let Some((row, col)) = self.screen.rfind_cell(
            |c| !c.is_wide_continuation(),
            self.review_cursor_position.0,
            0,
            self.review_cursor_position.0,
            self.review_cursor_position.1,
        ) {
            self.review_cursor_position = (row, col);
            true
        } else {
            false
        }
    }

    /// Moves the review cursor right a column.
    /// If the next cell continues a wide character, it will be skipped.
    /// This method will return true only if the cursor moved.
    fn review_cursor_right(&mut self) -> bool {
        if self.review_cursor_position.1 >= self.screen.size().1 - 1 {
            return false;
        }
        self.review_cursor_position.1 += 1;
        if let Some((row, col)) = self.screen.find_cell(
            |c| !c.is_wide_continuation(),
            self.review_cursor_position.0,
            self.review_cursor_position.1,
            self.review_cursor_position.0,
            self.screen.size().1 - 1,
        ) {
            self.review_cursor_position = (row, col);
            true
        } else {
            false
        }
    }
}

pub struct ScreenReader {
    speech: speech::Speech,
    help_mode: bool,
    last_key: Vec<u8>,
}

impl ScreenReader {
    pub fn new() -> Result<Self> {
        Ok(ScreenReader {
            speech: speech::new()?,
            help_mode: false,
            last_key: Vec::new(),
        })
    }

    pub fn run(&mut self) -> Result<()> {
        let init_term_attrs = termios::tcgetattr(0)?;
        // Spawn the child process, connect it to a PTY,
        // and set the PTY to match the current terminal attributes.
        let mut process = PtyProcess::spawn(Command::new("bash")).context("spawn child process")?;
        let term_size = termsize::get().ok_or_else(|| anyhow!("cannot get terminal size"))?;
        process
            .set_window_size(term_size.cols, term_size.rows)
            .context("resize PTY")?;
        let pty_stream = process.get_pty_stream().context("get PTY stream")?;
        termios::tcsetattr(
            pty_stream.as_raw_fd(),
            termios::SetArg::TCSADRAIN,
            &init_term_attrs,
        )?;

        let result = self.do_events(&mut process);
        // Clean up before returning the above result.
        termios::tcsetattr(0, termios::SetArg::TCSADRAIN, &init_term_attrs).unwrap();
        let _ = process.kill(ptyprocess::Signal::SIGKILL);
        let _ = process.wait();
        result
    }

    fn do_events(&mut self, process: &mut ptyprocess::PtyProcess) -> Result<()> {
        let mut pty_stream = process.get_pty_stream().context("get PTY stream")?;
        // Set stdin to raw, so that input is read character by character,
        // and so that signals like SIGINT aren't send when pressing keys like ^C.
        ptyprocess::set_raw(0).context("set STDIN to raw")?;

        // Create a parser to track the PTY's screen state.
        let (cols, rows) = process.get_window_size()?;
        let mut parser = vt100::Parser::new(rows, cols, 0);
        let mut screen_state = ScreenState {
            screen: parser.screen().clone(),
            prev_screen: parser.screen().clone(),
            prev_screen_time: time::Instant::now(),
            review_cursor_position: parser.screen().cursor_position(),
            review_cursor_last_indent_level: 0,
            last_indent_level: 0,
        };

        // We also want to separately keep track of incoming bytes, for autoread.
        let mut vte_parser = vte::Parser::new();
        // Store new text to be read.
        let mut text_reporter = perform::TextReporter::new();

        // Set up a mio poll, to select between reading from stdin, and the PTY.
        let mut signals = Signals::new([SIGWINCH])?;
        const STDIN_TOKEN: mio::Token = mio::Token(0);
        const PTY_TOKEN: mio::Token = mio::Token(1);
        const SIGNALS_TOKEN: mio::Token = mio::Token(2);
        let mut poll = mio::Poll::new()?;
        poll.registry().register(
            &mut mio::unix::SourceFd(&std::io::stdin().as_raw_fd()),
            STDIN_TOKEN,
            mio::Interest::READABLE,
        )?;
        poll.registry().register(
            &mut mio::unix::SourceFd(&pty_stream.as_raw_fd()),
            PTY_TOKEN,
            mio::Interest::READABLE,
        )?;
        poll.registry()
            .register(&mut signals, SIGNALS_TOKEN, mio::Interest::READABLE)?;

        self.speech.set_rate(720.0)?;
        self.speech.speak("Welcome to lector", false)?;

        // Main event loop
        let mut stdin = std::io::stdin().lock();
        let mut stdout = std::io::stdout().lock();
        let mut events = mio::Events::with_capacity(1024);
        let mut poll_timeout = None;
        loop {
            poll.poll(&mut events, poll_timeout).or_else(|e| {
                if e.kind() == ErrorKind::Interrupted {
                    events.clear();
                    Ok(())
                } else {
                    Err(e)
                }
            })?;
            for event in events.iter() {
                match event.token() {
                    STDIN_TOKEN => {
                        let mut buf = [0; 8192];
                        let n = match stdin.read(&mut buf) {
                            Ok(n) if n == 0 => return Ok(()),
                            Ok(n) => n,
                            Err(e) => bail!("error reading from input: {}", e),
                        };
                        self.last_key = buf[0..n].to_owned();
                        self.speech.stop()?;
                        let pass_through = match KEYMAP.get(std::str::from_utf8(&buf[0..n])?) {
                            Some(&v) => self.handle_action(&mut screen_state, v)?,
                            None => {
                                if self.help_mode {
                                    self.speech.speak("this key is unmapped", false)?;
                                    false
                                } else {
                                    true
                                }
                            }
                        };
                        if pass_through {
                            pty_stream
                                .write_all(&buf[0..n])
                                .context("copy STDIN to PTY")?;
                            pty_stream.flush().context("flush write to PTY")?;
                        }
                    }
                    PTY_TOKEN => {
                        let mut buf = [0; 8192];
                        let n = match pty_stream.read(&mut buf) {
                            Ok(n) if n == 0 => return Ok(()), // The child process exited
                            Ok(n) => n,
                            Err(e) => bail!("error reading from PTY: {}", e),
                        };
                        stdout.write_all(&buf[0..n]).context("write PTY output")?;
                        stdout.flush().context("flush output")?;
                        for b in &buf[0..n] {
                            vte_parser.advance(&mut text_reporter, *b);
                        }

                        self.process_screen_changes(&mut screen_state, &mut parser, &buf[0..n]);
                        // Stop blocking indefinitely until this screen is old enough to be
                        // autoread.
                        poll_timeout = Some(time::Duration::from_millis(DIFF_DELAY as u64));
                    }
                    SIGNALS_TOKEN => {
                        for signal in signals.pending() {
                            match signal {
                                SIGWINCH => {
                                    let term_size = termsize::get()
                                        .ok_or_else(|| anyhow!("cannot get terminal size"))?;
                                    process
                                        .set_window_size(term_size.cols, term_size.rows)
                                        .context("resize PTY")?;
                                    parser.set_size(term_size.rows, term_size.cols);
                                    screen_state.review_cursor_position = (
                                        min(
                                            screen_state.review_cursor_position.0,
                                            term_size.rows - 1,
                                        ),
                                        min(
                                            screen_state.review_cursor_position.1,
                                            term_size.cols - 1,
                                        ),
                                    );
                                }
                                _ => unreachable!("unknown signal"),
                            }
                        }
                    }
                    _ => unreachable!("encountered unknown event"),
                }
            }

            // Read the screen if it is old enough and was updated.
            if screen_state.prev_screen_time.elapsed().as_millis() >= DIFF_DELAY as u128 {
                poll_timeout = None; // No need to wakeup until we get more updates.
                if !screen_state
                    .screen
                    .state_diff(&screen_state.prev_screen)
                    .is_empty()
                {
                    self.autoread(&mut screen_state, &mut text_reporter)?;
                    screen_state.prev_screen = screen_state.screen.clone();
                    screen_state.prev_screen_time = time::Instant::now();
                }
            }
        }
    }

    fn process_screen_changes(
        &mut self,
        screen_state: &mut ScreenState,
        parser: &mut vt100::Parser,
        buf: &[u8],
    ) {
        parser.process(buf);
        let prev_screen = screen_state.screen.clone();
        screen_state.screen = parser.screen().clone();
        // If the cursor moved, move the review cursor to its location.
        if screen_state.screen.cursor_position() != prev_screen.cursor_position() {
            screen_state.review_cursor_position = screen_state.screen.cursor_position()
        }
        // If the screen's size changed, the cursor may now be out of bounds.
        let term_size = screen_state.screen.size();
        screen_state.review_cursor_position = (
            min(screen_state.review_cursor_position.0, term_size.0),
            min(screen_state.review_cursor_position.1, term_size.1),
        );
    }

    fn autoread(
        &mut self,
        screen_state: &mut ScreenState,
        text_reporter: &mut perform::TextReporter,
    ) -> Result<()> {
        let (prev_cursor, cursor) = (
            screen_state.prev_screen.cursor_position(),
            screen_state.screen.cursor_position(),
        );

        let indent_level = screen_state
            .screen
            .find_cell(
                |c| !c.contents().is_empty() && !c.contents().chars().all(char::is_whitespace),
                cursor.0,
                0,
                cursor.0,
                screen_state.screen.size().1 - 1,
            )
            .map_or(screen_state.last_indent_level, |(_, col)| col);
        if indent_level != screen_state.last_indent_level {
            self.speech
                .speak(&format!("indent {}", indent_level), false)?;
            screen_state.last_indent_level = indent_level;
        }

        // If the cursor wasn't explicitly moved,
        // we can just read what was drawn to the screen.
        // Otherwise, we'll use a screen diff.
        // Keep track of what was autoread, and don't repeat it when tracking the cursor.
        let mut text_read = None;
        let diffing = match text_reporter.get_text() {
            Some(text) => {
                let echoed_char = match std::str::from_utf8(&self.last_key) {
                    Ok(s) if text == s => true,
                    _ => false,
                };

                // If the current and previous screens contain the same content, but there was text
                // drawn, it could be because the application is drawing the same characters over
                // themselves when the cursor was moved, perhaps to change their attributes.
                // Or, it could be because a bunch of text scrolled by, and this screen just so
                // happens to be the same.
                // We don't want to read the former, but we do the latter.
                if screen_state.screen.contents() == screen_state.prev_screen.contents()
                    && screen_state.screen.cursor_position()
                        != screen_state.prev_screen.cursor_position()
                {
                    true
                } else {
                    if !echoed_char {
                        self.speech.speak(&text, false)?;
                        text_read = Some(text);
                    }
                    false
                }
            }
            None => true,
        };

        if diffing {
            let mut text = String::new();
            let old = screen_state
                .prev_screen
                .rows(0, screen_state.prev_screen.size().1)
                .collect::<Vec<String>>()
                .join("\n");
            let new = screen_state
                .screen
                .rows(0, screen_state.screen.size().1)
                .collect::<Vec<String>>()
                .join("\n");

            let line_changes = TextDiff::from_lines(&old, &new);
            // One deletion followed by one insertion, and no other changes,
            // means only a single line changed. In that case, only report what changed in the
            // line.
            // Otherwise, report the entire lines that were added.
            let mut diff_mode_lines = false;
            let mut prev_tag = None;
            let mut insertions = 0;
            for tag in line_changes.iter_all_changes().map(|c| c.tag()) {
                if tag != ChangeTag::Insert {
                    prev_tag = Some(tag);
                    continue;
                }
                if prev_tag != Some(ChangeTag::Delete) || insertions > 0 {
                    diff_mode_lines = true;
                    break;
                }
                insertions += 1;
                prev_tag = Some(tag);
            }

            if diff_mode_lines {
                for change in line_changes
                    .iter_all_changes()
                    .filter(|c| c.tag() == ChangeTag::Insert)
                {
                    text.push_str(&format!("{}\n", change));
                }
            } else {
                for change in diff_graphemes(Algorithm::Myers, &old, &new)
                    .iter()
                    .filter(|c| c.0 == ChangeTag::Insert)
                {
                    text.push_str(&format!("{} ", change.1));
                }
            }

            self.speech.speak(&text, false)?;
            text_read = Some(text);
        }

        let mut cursor_report: Option<String> = None;
        if cursor.0 != prev_cursor.0 {
            // It moved to a different line
            let line = screen_state.screen.contents_between(
                cursor.0,
                0,
                cursor.0,
                screen_state.screen.size().1,
            );
            cursor_report = Some(line);
        } else if cursor.1 != prev_cursor.1 {
            // The cursor moved left or right
            let distance_moved = (cursor.1 as i32 - prev_cursor.1 as i32).abs();
            let prev_word_start = screen_state
                .screen
                .find_word_start(prev_cursor.0, prev_cursor.1);
            let word_start = screen_state.screen.find_word_start(cursor.0, cursor.1);
            if word_start != prev_word_start && distance_moved > 1 {
                // The cursor moved to a different word.
                let word_end = screen_state.screen.find_word_end(cursor.0, cursor.1);
                let word = screen_state.screen.contents_between(
                    cursor.0,
                    word_start,
                    cursor.0,
                    word_end + 1,
                );
                cursor_report = Some(word);
            } else {
                let ch = screen_state.screen.contents_between(
                    cursor.0,
                    cursor.1,
                    cursor.0,
                    cursor.1 + 1,
                );
                cursor_report = Some(ch);
            }
        }

        match cursor_report {
            Some(s) if !text_read.map_or(false, |v| v.contains(&s)) => {
                self.speech.speak(&s, false)?
            }
            _ => (),
        }

        Ok(())
    }

    fn handle_action(&mut self, screen_state: &mut ScreenState, action: Action) -> Result<bool> {
        if let Action::ToggleHelp = action {
            return self.action_toggle_help();
        }
        if self.help_mode {
            self.speech.speak(&action.help_text(), false)?;
            return Ok(false);
        }

        match action {
            Action::StopSpeaking => self.action_stop(),
            Action::RevLinePrev => self.action_review_line_prev(screen_state),
            Action::RevLineNext => self.action_review_line_next(screen_state),
            Action::RevLineRead => self.action_review_line_read(screen_state),
            Action::RevWordPrev => self.action_review_word_prev(screen_state),
            Action::RevWordNext => self.action_review_word_next(screen_state),
            Action::RevWordRead => self.action_review_word_read(screen_state),
            Action::RevCharPrev => self.action_review_char_prev(screen_state),
            Action::RevCharNext => self.action_review_char_next(screen_state),
            Action::RevCharRead => self.action_review_char_read(screen_state),
            Action::RevCharReadPhonetic => self.action_review_char_read_phonetic(screen_state),
            Action::RevTop => self.action_review_top(screen_state),
            Action::RevBottom => self.action_review_bottom(screen_state),
            Action::RevFirst => self.action_review_first(screen_state),
            Action::RevLast => self.action_review_last(screen_state),
            Action::RevReadAttributes => self.action_review_read_attributes(screen_state),
            Action::Backspace => self.action_backspace(screen_state),
            Action::Delete => self.action_delete(screen_state),
            Action::SayTime => self.action_say_time(),
            _ => {
                self.speech.speak("not implemented", false)?;
                Ok(false)
            }
        }
    }
}

// Actions
impl ScreenReader {
    fn action_stop(&mut self) -> Result<bool> {
        self.speech.stop()?;
        Ok(false)
    }

    fn action_toggle_help(&mut self) -> Result<bool> {
        if self.help_mode {
            self.help_mode = false;
            self.speech.speak("exiting help", false)?;
        } else {
            self.help_mode = true;
            self.speech
                .speak("entering help. Press this key again to exit", false)?;
        }
        Ok(false)
    }

    fn action_review_line_prev(&mut self, screen_state: &mut ScreenState) -> Result<bool> {
        if screen_state.review_cursor_up() {
            self.action_review_line_read(screen_state)?;
        } else {
            self.speech.speak("top", false)?;
        }
        Ok(false)
    }

    fn action_review_line_next(&mut self, screen_state: &mut ScreenState) -> Result<bool> {
        if screen_state.review_cursor_down() {
            self.action_review_line_read(screen_state)?;
        } else {
            self.speech.speak("bottom", false)?;
        }
        Ok(false)
    }

    fn action_review_line_read(&mut self, screen_state: &mut ScreenState) -> Result<bool> {
        let row = screen_state.review_cursor_position.0;
        let line = screen_state
            .screen
            .contents_between(row, 0, row, screen_state.screen.size().1);
        let indent_level = screen_state
            .screen
            .find_cell(
                |c| !c.contents().is_empty() && !c.contents().chars().all(char::is_whitespace),
                row,
                0,
                row,
                screen_state.screen.size().1 - 1,
            )
            .map_or(screen_state.review_cursor_last_indent_level, |(_, col)| col);
        if indent_level != screen_state.review_cursor_last_indent_level {
            self.speech
                .speak(&format!("indent {}", indent_level), false)?;
            screen_state.review_cursor_last_indent_level = indent_level;
        }
        if line.is_empty() {
            self.speech.speak("blank", false)?;
        } else {
            self.speech.speak(&line, false)?;
        }
        Ok(false)
    }

    fn action_review_word_prev(&mut self, screen_state: &mut ScreenState) -> Result<bool> {
        if screen_state.review_cursor_prev_word() {
            self.action_review_word_read(screen_state)?;
        } else {
            self.speech.speak("left", false)?;
        }
        Ok(false)
    }

    fn action_review_word_next(&mut self, screen_state: &mut ScreenState) -> Result<bool> {
        if screen_state.review_cursor_next_word() {
            self.action_review_word_read(screen_state)?;
        } else {
            self.speech.speak("right", false)?;
        }
        Ok(false)
    }

    fn action_review_word_read(&mut self, screen_state: &ScreenState) -> Result<bool> {
        let (row, col) = screen_state.review_cursor_position;
        let start = screen_state.screen.find_word_start(row, col);
        let end = screen_state.screen.find_word_end(row, col);

        let word = screen_state
            .screen
            .contents_between(row, start, row, end + 1);
        self.speech.speak(&word, false)?;
        Ok(false)
    }

    fn action_review_char_prev(&mut self, screen_state: &mut ScreenState) -> Result<bool> {
        if screen_state.review_cursor_left() {
            self.action_review_char_read(screen_state)?;
        } else {
            self.speech.speak("left", false)?;
        }
        Ok(false)
    }

    fn action_review_char_next(&mut self, screen_state: &mut ScreenState) -> Result<bool> {
        if screen_state.review_cursor_right() {
            self.action_review_char_read(screen_state)?;
        } else {
            self.speech.speak("right", false)?;
        }
        Ok(false)
    }

    fn action_review_char_read(&mut self, screen_state: &ScreenState) -> Result<bool> {
        let (row, col) = screen_state.review_cursor_position;
        let char = screen_state
            .screen
            .cell(row, col)
            .ok_or_else(|| anyhow!("cannot get cell at row {}, column {}", row, col))?
            .contents();
        if char.is_empty() {
            self.speech.speak("blank", false)?;
        } else {
            self.speech.speak(&char, false)?;
        }
        Ok(false)
    }

    fn action_review_char_read_phonetic(&mut self, screen_state: &ScreenState) -> Result<bool> {
        let (row, col) = screen_state.review_cursor_position;
        let char = screen_state
            .screen
            .cell(row, col)
            .ok_or_else(|| anyhow!("cannot get cell at row {}, column {}", row, col))?
            .contents();
        let char = match char.to_lowercase().as_str() {
            "a" => "Alpha",
            "b" => "Bravo",
            "c" => "Charlie",
            "d" => "Delta",
            "e" => "Echo",
            "f" => "Foxtrot",
            "g" => "Golf",
            "h" => "Hotel",
            "i" => "India",
            "j" => "Juliett",
            "k" => "Kilo",
            "l" => "Lima",
            "m" => "Mike",
            "n" => "November",
            "o" => "Oscar",
            "p" => "Papa",
            "q" => "Quebec",
            "r" => "Romeo",
            "s" => "Sierra",
            "t" => "Tango",
            "u" => "Uniform",
            "v" => "Victor",
            "w" => "Whiskey",
            "x" => "X-ray",
            "y" => "Yankee",
            "z" => "Zulu",
            _ => &char,
        };
        self.speech.speak(char, false)?;
        Ok(false)
    }

    fn action_review_top(&mut self, screen_state: &mut ScreenState) -> Result<bool> {
        screen_state.review_cursor_position.0 = 0;
        self.speech.speak("top", false)?;
        Ok(false)
    }

    fn action_review_bottom(&mut self, screen_state: &mut ScreenState) -> Result<bool> {
        screen_state.review_cursor_position.0 = screen_state.screen.size().0 - 1;
        self.speech.speak("bottom", false)?;
        Ok(false)
    }

    fn action_review_first(&mut self, screen_state: &mut ScreenState) -> Result<bool> {
        screen_state.review_cursor_position.1 = 0;
        self.speech.speak("left", false)?;
        Ok(false)
    }

    fn action_review_last(&mut self, screen_state: &mut ScreenState) -> Result<bool> {
        screen_state.review_cursor_position.1 = screen_state.screen.size().1 - 1;
        self.speech.speak("right", false)?;
        Ok(false)
    }

    fn action_review_read_attributes(&mut self, screen_state: &ScreenState) -> Result<bool> {
        let (row, col) = screen_state.review_cursor_position;
        let cell = screen_state
            .screen
            .cell(row, col)
            .ok_or_else(|| anyhow!("cannot get cell at row {}, column {}", row, col))?;

        let mut attrs = String::new();
        attrs.push_str(&format!(
            "{} {}",
            attributes::describe_color(cell.fgcolor()),
            if let vt100::Color::Default = cell.bgcolor() {
                "".into()
            } else {
                format!("on {}", attributes::describe_color(cell.bgcolor()))
            }
        ));
        attrs.push_str(&format!(
            "{}{}{}{}{}",
            if cell.bold() { "bold " } else { "" },
            if cell.italic() { "italic " } else { "" },
            if cell.underline() { "underline " } else { "" },
            if cell.inverse() { "inverse " } else { "" },
            if cell.is_wide() { "wide " } else { "" },
        ));

        self.speech.speak(&attrs, false)?;
        Ok(false)
    }

    fn action_backspace(&mut self, screen_state: &ScreenState) -> Result<bool> {
        let (row, col) = screen_state.screen.cursor_position();
        if col > 0 {
            let char = screen_state
                .screen
                .cell(row, col - 1)
                .ok_or_else(|| anyhow!("cannot get cell at row {}, column {}", row, col))?
                .contents();
            self.speech.speak(&char, false)?;
        }
        Ok(true)
    }

    fn action_delete(&mut self, screen_state: &ScreenState) -> Result<bool> {
        let (row, col) = screen_state.screen.cursor_position();
        let char = screen_state
            .screen
            .cell(row, col)
            .ok_or_else(|| anyhow!("cannot get cell at row {}, column {}", row, col))?
            .contents();
        self.speech.speak(&char, false)?;
        Ok(true)
    }

    fn action_say_time(&mut self) -> Result<bool> {
        let date = chrono::Local::now();
        self.speech.speak(&format!("{}", date.format("%H:%M")), false)?;
        Ok(false)
    }
}
