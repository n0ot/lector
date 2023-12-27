use super::{clipboard::Clipboard, commands, ext::ScreenExt, perform, speech, view::View};
use anyhow::{anyhow, bail, Context, Result};
use nix::sys::termios;
use phf::phf_map;
use ptyprocess::PtyProcess;
use signal_hook::consts::signal::*;
use signal_hook_mio::v0_8::Signals;
use similar::{Algorithm, ChangeTag, TextDiff};
use std::{
    collections::HashSet,
    io::{ErrorKind, Read, Write},
    iter::FromIterator,
    os::fd::AsRawFd,
    process::Command,
    time,
};

const DIFF_DELAY: u16 = 1;
const MAX_DIFF_DELAY: u16 = 300;

static KEYMAP: phf::Map<&'static str, commands::Action> = phf_map! {
    "\x1BOP" => commands::Action::ToggleHelp,
    "\x1B'" => commands::Action::ToggleAutoRead,
    "\x1B\"" => commands::Action::ToggleReviewCursorFollowsScreenCursor,
    "\x1Bn" => commands::Action::PassNextKey,
    "\x1Bx" => commands::Action::StopSpeaking,
    "\x1Bu" => commands::Action::RevLinePrev,
    "\x1Bo" => commands::Action::RevLineNext,
    "\x1BU" => commands::Action::RevLinePrevNonBlank,
    "\x1BO" => commands::Action::RevLineNextNonBlank,
    "\x1Bi" => commands::Action::RevLineRead,
    "\x1Bm" => commands::Action::RevCharPrev,
    "\x1B." => commands::Action::RevCharNext,
    "\x1B," => commands::Action::RevCharRead,
    "\x1B<" => commands::Action::RevCharReadPhonetic,
    "\x1Bj" => commands::Action::RevWordPrev,
    "\x1Bl" => commands::Action::RevWordNext,
    "\x1Bk" => commands::Action::RevWordRead,
    "\x1By" => commands::Action::RevTop,
    "\x1Bp" => commands::Action::RevBottom,
    "\x1Bh" => commands::Action::RevFirst,
    "\x1B;" => commands::Action::RevLast,
    "\x1Ba" => commands::Action::RevReadAttributes,
    "\x08" => commands::Action::Backspace,
    "\x7F" => commands::Action::Backspace,
    "\x1B[3~" => commands::Action::Delete,
    "\x1B[24~" => commands::Action::SayTime,
    "\x1B[15~" => commands::Action::SetMark,
    "\x1B[17~" => commands::Action::Copy,
    "\x1B[18~" => commands::Action::Paste,
    "\x1Bc" => commands::Action::SayClipboard,
    "\x1B[" => commands::Action::PreviousClipboard,
    "\x1B]" => commands::Action::NextClipboard,
};

#[allow(dead_code)]
pub enum CursorTrackingMode {
    On,
    Off,
    OffOnce,
}

pub struct ScreenReader {
    pub speech: speech::Speech,
    pub help_mode: bool,
    pub auto_read: bool,
    pub review_follows_screen_cursor: bool,
    last_key: Vec<u8>,
    pub cursor_tracking_mode: CursorTrackingMode,
    highlight_tracking: bool,
    pub clipboard: Clipboard,
    pub pass_through: bool,
}

impl ScreenReader {
    pub fn new() -> Result<Self> {
        Ok(ScreenReader {
            speech: speech::new()?,
            help_mode: false,
            auto_read: true,
            review_follows_screen_cursor: true,
            last_key: Vec::new(),
            cursor_tracking_mode: CursorTrackingMode::On,
            highlight_tracking: false,
            clipboard: Default::default(),
            pass_through: false,
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

        // Create a view based on the spawned process's screen
        let (cols, rows) = process.get_window_size()?;
        let mut view = View::new(rows, cols);

        // We also want to separately keep track of incoming bytes, for auto read.
        let mut vte_parser = vte::Parser::new();
        // Store new text to be read.
        let mut text_reporter = perform::TextReporter::new();
        let ansi_csi_re =
            regex::bytes::Regex::new(r"^\x1B\[[\x30-\x3F]*[\x20-\x2F]*[\x40-\x7E--[A-D~]]$")
                .unwrap();

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
        let mut last_pty_update = None;
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
                        // Don't silence speech or set the last key for key echo,
                        // when receiving a CSI dispatch.
                        if !ansi_csi_re.is_match(&buf[0..n]) {
                            self.last_key = buf[0..n].to_owned();
                            self.speech.stop()?;
                        }
                        let pass_through = match self.pass_through {
                            false => match KEYMAP.get(std::str::from_utf8(&buf[0..n])?) {
                                Some(&v) => {
                                    commands::handle_action(self, &mut view, &mut pty_stream, v)?
                                }
                                None => {
                                    if self.help_mode {
                                        self.speech.speak("this key is unmapped", false)?;
                                        false
                                    } else {
                                        true
                                    }
                                }
                            },
                            true => {
                                // Turning pass through on should only apply for one keystroke.
                                self.pass_through = false;
                                true
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
                        if self.auto_read {
                            for b in &buf[0..n] {
                                vte_parser.advance(&mut text_reporter, *b);
                            }
                        }

                        view.process_changes(&buf[0..n]);
                        // Stop blocking indefinitely until this screen is old enough to be
                        // auto read.
                        poll_timeout = Some(time::Duration::from_millis(DIFF_DELAY as u64));
                        last_pty_update = Some(time::Instant::now());
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
                                    view.set_size(term_size.rows, term_size.cols);
                                }
                                _ => unreachable!("unknown signal"),
                            }
                        }
                    }
                    _ => unreachable!("encountered unknown event"),
                }
            }

            // We want to wait till the PTY has stopped sending us data for awhile before reading
            // updates, to give the screen time to stabilize.
            // But if we never stop getting updates, we want to read what we have eventually.
            if let Some(lpu) = last_pty_update {
                if lpu.elapsed().as_millis() > DIFF_DELAY as u128
                    || view.prev_screen_time.elapsed().as_millis() > MAX_DIFF_DELAY as u128
                {
                    poll_timeout = None; // No need to wakeup until we get more updates.
                    last_pty_update = None;
                    if self.highlight_tracking {
                        self.track_highlighting(&view)?;
                    }
                    let read_text = if self.auto_read {
                        self.auto_read(&mut view, &mut text_reporter)?
                    } else {
                        // If the text reporter wasn't drained since auto read was last disabled,
                        // it will be read when auto read is re-enabled, which is not desirable.
                        let _ = text_reporter.get_text();
                        false
                    };
                    if !read_text {
                        self.track_cursor(&mut view)?;
                    }

                    // Track screen cursor movements here, instead of every time the screen
                    // updates,
                    // to give the screen time to stabilize.
                    if self.review_follows_screen_cursor
                        && view.screen().cursor_position() != view.prev_screen().cursor_position()
                    {
                        view.review_cursor_position = view.screen().cursor_position();
                    }

                    view.finalize_changes();
                }
            }
        }
    }

    fn track_cursor(&mut self, view: &mut View) -> Result<()> {
        let (prev_cursor, cursor) = (
            view.prev_screen().cursor_position(),
            view.screen().cursor_position(),
        );

        let mut cursor_report: Option<String> = None;
        if cursor.0 != prev_cursor.0 {
            // It moved to a different line
            let line = view
                .screen()
                .contents_between(cursor.0, 0, cursor.0, view.size().1);
            cursor_report = Some(line);
        } else if cursor.1 != prev_cursor.1 {
            // The cursor moved left or right
            let distance_moved = (cursor.1 as i32 - prev_cursor.1 as i32).abs();
            let prev_word_start = view.screen().find_word_start(prev_cursor.0, prev_cursor.1);
            let word_start = view.screen().find_word_start(cursor.0, cursor.1);
            if word_start != prev_word_start && distance_moved > 1 {
                // The cursor moved to a different word.
                let word_end = view.screen().find_word_end(cursor.0, cursor.1);
                let word =
                    view.screen()
                        .contents_between(cursor.0, word_start, cursor.0, word_end + 1);
                cursor_report = Some(word);
            } else {
                let ch = view
                    .screen()
                    .contents_between(cursor.0, cursor.1, cursor.0, cursor.1 + 1);
                // Avoid randomly saying "space".
                // Unfortunately this means moving the cursor manually over a space will say
                // nothing.
                let ch = if ch.trim().is_empty() {
                    "".to_string()
                } else {
                    ch
                };
                cursor_report = Some(ch);
            }
        }

        match &self.cursor_tracking_mode {
            CursorTrackingMode::On => {
                self.report_application_cursor_indentation_changes(view)?;
                if let Some(s) = cursor_report {
                    self.speech.speak(&s, false)?;
                }
            }
            CursorTrackingMode::OffOnce => self.cursor_tracking_mode = CursorTrackingMode::On,
            CursorTrackingMode::Off => {}
        }

        Ok(())
    }

    fn track_highlighting(&mut self, view: &View) -> Result<()> {
        let (highlights, prev_highlights) = (
            view.screen().get_highlights(),
            view.prev_screen().get_highlights(),
        );
        let prev_hl_set: HashSet<String> = HashSet::from_iter(prev_highlights.iter().cloned());

        for hl in highlights {
            if !prev_hl_set.contains(&hl) {
                self.speech.speak(&hl, false)?;
            }
        }
        Ok(())
    }

    /// Report indentation changes, if any, for the line under the application cursor
    pub fn report_application_cursor_indentation_changes(&mut self, view: &mut View) -> Result<()> {
        let (indent_level, changed) = view.application_cursor_indentation_level();
        if changed {
            self.speech
                .speak(&format!("indent {}", indent_level), false)?;
        }

        Ok(())
    }

    /// Report indentation changes, if any, for the line under the review cursor
    pub fn report_review_cursor_indentation_changes(&mut self, view: &mut View) -> Result<()> {
        let (indent_level, changed) = view.review_cursor_indentation_level();
        if changed {
            self.speech
                .speak(&format!("indent {}", indent_level), false)?;
        }

        Ok(())
    }

    /// Read what's changed between the current and previous screen.
    /// If anything was read, the value in the result will be true.
    fn auto_read(
        &mut self,
        view: &mut View,
        text_reporter: &mut perform::TextReporter,
    ) -> Result<bool> {
        let cursor = view.screen().cursor_position();

        self.report_application_cursor_indentation_changes(view)?;
        if view.screen().contents() == view.prev_screen().contents() {
            return Ok(false);
        }

        // Try to read any incoming text.
        // Fall back to a screen diff if that makes more sense.
        let cursor_moves = text_reporter.cursor_moves;
        let scrolled = text_reporter.scrolled;
        let text = text_reporter.get_text();
        if !text.is_empty() && (cursor_moves == 0 || scrolled) {
            // Don't echo typed keys
            match std::str::from_utf8(&self.last_key) {
                Ok(s) if text == s => {}
                _ => self.speech.speak(text, false)?,
            }

            // We still want to report that text was read when suppressing echo,
            // so that cursor tracking doesn't read the character that follows as we type.
            return Ok(true);
        }

        // Do a diff instead
        let mut text = String::new();
        let old = view.prev_screen().contents_full();
        let new = view.screen().contents_full();

        let line_changes = TextDiff::configure()
            .algorithm(Algorithm::Patience)
            .diff_lines(&old, &new);
        // One deletion followed by one insertion, and no other changes,
        // means only a single line changed. In that case, only report what changed in that
        // line.
        // Otherwise, report the entire lines that were added.
        #[derive(PartialEq)]
        enum DiffState {
            /// Nothing has changed
            NoChanges,
            /// A single line was deleted
            OneDeletion,
            /// One deletion followed by one insertion
            Single,
            /// Anything else (including a single insertion)
            Multi,
        }
        let mut diff_state = DiffState::NoChanges;
        for change in line_changes.iter_all_changes() {
            diff_state = match diff_state {
                DiffState::NoChanges => match change.tag() {
                    ChangeTag::Delete => DiffState::OneDeletion,
                    ChangeTag::Equal => DiffState::NoChanges,
                    ChangeTag::Insert => DiffState::Multi,
                },
                DiffState::OneDeletion => match change.tag() {
                    ChangeTag::Delete => DiffState::Multi,
                    ChangeTag::Equal => DiffState::OneDeletion,
                    ChangeTag::Insert => DiffState::Single,
                },
                DiffState::Single => match change.tag() {
                    ChangeTag::Equal => DiffState::Single,
                    _ => DiffState::Multi,
                },
                DiffState::Multi => DiffState::Multi,
            };
            if change.tag() == ChangeTag::Insert {
                text.push_str(&format!("{}\n", change));
            }
        }

        if diff_state == DiffState::Single {
            let mut graphemes = String::new();
            // If there isn't just a single change, just read the whole line.
            diff_state = DiffState::NoChanges;
            let mut prev_tag = None;
            for change in TextDiff::configure()
                .algorithm(Algorithm::Patience)
                .diff_graphemes(&old, &new)
                .iter_all_changes()
            {
                diff_state = match diff_state {
                    DiffState::NoChanges => match change.tag() {
                        ChangeTag::Delete => DiffState::OneDeletion,
                        ChangeTag::Equal => DiffState::NoChanges,
                        ChangeTag::Insert => DiffState::Single,
                    },
                    DiffState::OneDeletion => match change.tag() {
                        ChangeTag::Delete if prev_tag == Some(ChangeTag::Delete) => {
                            DiffState::OneDeletion
                        }
                        ChangeTag::Equal => DiffState::OneDeletion,
                        ChangeTag::Insert if prev_tag == Some(ChangeTag::Delete) => {
                            DiffState::Single
                        }
                        _ => DiffState::Multi,
                    },
                    DiffState::Single => match change.tag() {
                        ChangeTag::Equal => DiffState::Single,
                        ChangeTag::Insert
                            if prev_tag == Some(ChangeTag::Insert)
                                || prev_tag == Some(ChangeTag::Delete) =>
                        {
                            DiffState::Single
                        }
                        _ => DiffState::Multi,
                    },
                    DiffState::Multi => DiffState::Multi,
                };
                prev_tag = Some(change.tag());
                if diff_state == DiffState::Multi {
                    continue; // Revert to the line diff.
                }
                if change.tag() == ChangeTag::Insert {
                    graphemes.push_str(change.as_str().unwrap_or(""));
                }
            }

            if diff_state != DiffState::Multi {
                text = graphemes;
            }
        }

        // Don't echo typed keys
        match std::str::from_utf8(&self.last_key) {
            // We still want to report that text was read when suppressing echo,
            // so that cursor tracking doesn't read the character that follows as we type.
            Ok(s) if text == s => Ok(true),
            _ => {
                self.speech.speak(&text, false)?;
                Ok(!text.is_empty())
            }
        }
    }
}
