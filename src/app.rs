use crate::{commands, perform, screen_reader::ScreenReader, views};
use anyhow::{Context, Result};
use phf::phf_map;
use std::{io::Write, time};

pub const DIFF_DELAY: u16 = 1;
pub const MAX_DIFF_DELAY: u16 = 300;

static KEYMAP: phf::Map<&'static str, commands::Action> = phf_map! {
    "\x1BOP" => commands::Action::ToggleHelp,
    "\x1B'" => commands::Action::ToggleAutoRead,
    "\x1B\"" => commands::Action::ToggleReviewCursorFollowsScreenCursor,
    "\x1Bs" => commands::Action::ToggleSymbolLevel,
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
    "\x1BL" => commands::Action::OpenLuaRepl,
    "\x1B[15~" => commands::Action::SetMark,
    "\x1B[17~" => commands::Action::Copy,
    "\x1B[18~" => commands::Action::Paste,
    "\x1Bc" => commands::Action::SayClipboard,
    "\x1B[" => commands::Action::PreviousClipboard,
    "\x1B]" => commands::Action::NextClipboard,
};

pub trait Clock {
    fn now_ms(&self) -> u128;
}

pub struct StdClock {
    start: time::Instant,
}

impl StdClock {
    pub fn new() -> Self {
        Self {
            start: time::Instant::now(),
        }
    }
}

impl Clock for StdClock {
    fn now_ms(&self) -> u128 {
        self.start.elapsed().as_millis()
    }
}

pub struct App {
    view_stack: views::ViewStack,
    vte_parser: vte::Parser,
    reporter: perform::Reporter,
    ansi_csi_re: regex::bytes::Regex,
    last_stdin_update: Option<u128>,
    last_pty_update: Option<u128>,
    clock: Box<dyn Clock>,
}

impl App {
    pub fn new(view_stack: views::ViewStack) -> Result<Self> {
        Self::new_with_clock(view_stack, Box::new(StdClock::new()))
    }

    pub fn new_with_clock(view_stack: views::ViewStack, clock: Box<dyn Clock>) -> Result<Self> {
        let ansi_csi_re =
            regex::bytes::Regex::new(
                r"^\x1B\[[\x30-\x3F]*[\x20-\x2F]*[\x40-\x7E--[A-D~]]$",
            )
            .context("compile ansi csi regex")?;
        let mut app = Self {
            view_stack,
            vte_parser: vte::Parser::new(),
            reporter: perform::Reporter::new(),
            ansi_csi_re,
            last_stdin_update: None,
            last_pty_update: None,
            clock,
        };
        let now_ms = app.clock.now_ms();
        app.view_stack.active_mut().model().prev_screen_time = now_ms;
        Ok(app)
    }

    pub fn wants_tick(&mut self) -> bool {
        self.view_stack.active_mut().wants_tick()
    }

    pub fn has_overlay(&self) -> bool {
        self.view_stack.has_overlay()
    }

    pub fn on_resize(
        &mut self,
        rows: u16,
        cols: u16,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        self.view_stack.on_resize(rows, cols);
        if self.view_stack.has_overlay() {
            self.render_active_view(term_out)?;
        }
        Ok(())
    }

    pub fn show_message(
        &mut self,
        sr: &mut ScreenReader,
        title: &str,
        message: &str,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let (rows, cols) = self.view_stack.root_mut().model().size();
        self.view_stack.push(Box::new(views::MessageView::new(
            rows,
            cols,
            title,
            message,
        )));
        self.render_active_view(term_out)?;
        self.announce_view_change(sr)?;
        Ok(())
    }

    pub fn handle_stdin(
        &mut self,
        sr: &mut ScreenReader,
        input: &[u8],
        pty_out: &mut dyn Write,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        if !self.ansi_csi_re.is_match(input) {
            sr.last_key.clear();
            sr.last_key.extend_from_slice(input);
            sr.speech.stop()?;
        }
        if sr.pass_through {
            sr.pass_through = false;
            self.dispatch_to_view(sr, input, pty_out, term_out)?;
            return Ok(());
        }

        let action = std::str::from_utf8(input)
            .ok()
            .and_then(|key| KEYMAP.get(key).copied());
        if let Some(action) = action {
            if matches!(action, commands::Action::OpenLuaRepl) {
                if self.view_stack.active_mut().kind() == views::ViewKind::LuaRepl {
                    sr.speech.speak("Lua REPL already open", false)?;
                    return Ok(());
                }
                let (rows, cols) = self.view_stack.active_mut().model().size();
                let repl = views::LuaReplView::new(rows, cols)?;
                self.handle_view_action(
                    sr,
                    views::ViewAction::Push(Box::new(repl)),
                    term_out,
                )?;
                return Ok(());
            }
            match commands::handle(sr, self.view_stack.active_mut().model(), action)? {
                commands::CommandResult::Handled => {}
                commands::CommandResult::ForwardInput => {
                    self.dispatch_to_view(sr, input, pty_out, term_out)?;
                }
                commands::CommandResult::Paste(contents) => {
                    let view_action = self
                        .view_stack
                        .active_mut()
                        .handle_paste(sr, &contents, pty_out)?;
                    self.handle_view_action(sr, view_action, term_out)?;
                }
            }
        } else if sr.help_mode {
            sr.speech.speak("this key is unmapped", false)?;
        } else {
            self.dispatch_to_view(sr, input, pty_out, term_out)?;
        }
        Ok(())
    }

    pub fn handle_pty(
        &mut self,
        sr: &mut ScreenReader,
        buf: &[u8],
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let overlay_active = self.view_stack.has_overlay();
        self.view_stack.root_mut().handle_pty_output(buf)?;
        if !overlay_active {
            term_out.write_all(buf).context("write PTY output")?;
            term_out.flush().context("flush output")?;
            if sr.auto_read {
                self.vte_parser.advance(&mut self.reporter, buf);
            }
        }
        self.last_pty_update = Some(self.clock.now_ms());
        Ok(())
    }

    pub fn handle_tick(
        &mut self,
        sr: &mut ScreenReader,
        pty_out: &mut dyn Write,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let tick_action = self.view_stack.active_mut().tick(sr, pty_out)?;
        self.handle_view_action(sr, tick_action, term_out)
    }

    pub fn maybe_finalize_changes(&mut self, sr: &mut ScreenReader) -> Result<bool> {
        let Some(lpu) = self.last_pty_update else {
            return Ok(false);
        };
        let now_ms = self.clock.now_ms();
        let overlay_active = self.view_stack.has_overlay();
        let root_view = self.view_stack.root_mut();
        let view = root_view.model();
        if now_ms.saturating_sub(lpu) > DIFF_DELAY as u128
            || now_ms.saturating_sub(view.prev_screen_time) > MAX_DIFF_DELAY as u128
        {
            self.last_pty_update = None;
            if !overlay_active {
                if sr.highlight_tracking {
                    sr.track_highlighting(view)?;
                }
                let read_text = if sr.auto_read {
                    sr.auto_read(view, &mut self.reporter)?
                } else {
                    false
                };
                if let Some(lsu) = self.last_stdin_update {
                    if now_ms.saturating_sub(lsu) <= MAX_DIFF_DELAY as u128 && !read_text {
                        sr.track_cursor(view)?;
                    }
                }
            }

            if sr.review_follows_screen_cursor
                && view.screen().cursor_position() != view.prev_screen().cursor_position()
            {
                view.review_cursor_position = view.screen().cursor_position();
            }

            view.finalize_changes(now_ms);
            return Ok(true);
        }
        Ok(false)
    }

    fn dispatch_to_view(
        &mut self,
        sr: &mut ScreenReader,
        input: &[u8],
        pty_out: &mut dyn Write,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        self.last_stdin_update = Some(self.clock.now_ms());
        let action = self
            .view_stack
            .active_mut()
            .handle_input(sr, input, pty_out)?;
        self.handle_view_action(sr, action, term_out)
    }

    fn handle_view_action(
        &mut self,
        sr: &mut ScreenReader,
        action: views::ViewAction,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        match action {
            views::ViewAction::PtyInput => {
                self.last_stdin_update = Some(self.clock.now_ms());
            }
            views::ViewAction::Bell => {
                term_out.write_all(b"\x07").context("write bell")?;
                term_out.flush().context("flush bell")?;
            }
            views::ViewAction::Push(view) => {
                self.view_stack.push(view);
                self.render_active_view(term_out)?;
                self.announce_view_change(sr)?;
            }
            views::ViewAction::Pop => {
                if self.view_stack.pop() {
                    self.render_active_view(term_out)?;
                    self.announce_view_change(sr)?;
                }
            }
            views::ViewAction::Redraw => {
                self.render_active_view(term_out)?;
                self.read_active_view_changes(sr)?;
            }
            views::ViewAction::None => {}
        }
        Ok(())
    }

    fn render_active_view(&mut self, term_out: &mut dyn Write) -> Result<()> {
        let view = self.view_stack.active_mut().model();
        term_out
            .write_all(b"\x1B[2J\x1B[H")
            .context("clear screen")?;
        term_out
            .write_all(&view.screen().contents_formatted())
            .context("render view contents")?;
        term_out
            .write_all(&view.screen().cursor_state_formatted())
            .context("render cursor state")?;
        term_out
            .write_all(&view.screen().input_mode_formatted())
            .context("render input modes")?;
        term_out.flush().context("flush view render")?;
        Ok(())
    }

    fn announce_view_change(&mut self, sr: &mut ScreenReader) -> Result<()> {
        let title = self.view_stack.active_mut().title().to_string();
        let view = self.view_stack.active_mut().model();
        sr.speech.speak(&title, false)?;
        let contents = view.contents_full();
        if contents.trim().is_empty() {
            sr.speech.speak("blank screen", false)?;
        } else {
            sr.speech.speak(&contents, false)?;
        }
        view.finalize_changes(self.clock.now_ms());
        Ok(())
    }

    fn read_active_view_changes(&mut self, sr: &mut ScreenReader) -> Result<()> {
        let now_ms = self.clock.now_ms();
        let view = self.view_stack.active_mut().model();
        let read_text = if sr.auto_read {
            let mut reporter = perform::Reporter::new();
            sr.auto_read(view, &mut reporter)?
        } else {
            false
        };
        if let Some(lsu) = self.last_stdin_update {
            if now_ms.saturating_sub(lsu) <= MAX_DIFF_DELAY as u128 && !read_text {
                sr.track_cursor(view)?;
            }
        }
        if sr.review_follows_screen_cursor
            && view.screen().cursor_position() != view.prev_screen().cursor_position()
        {
            view.review_cursor_position = view.screen().cursor_position();
        }
        view.finalize_changes(now_ms);
        Ok(())
    }
}
