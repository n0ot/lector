use crate::{
    commands,
    keymap::Binding,
    perform,
    screen_reader::ScreenReader,
    views,
};
use anyhow::{Context, Result};
use std::{collections::VecDeque, io::Write, time};
use terminput::{Event, KeyCode, KeyEvent, KeyModifiers};

pub const DIFF_DELAY: u16 = 1;
pub const MAX_DIFF_DELAY: u16 = 300;
const ESC_TIMEOUT_MS: u128 = 50;

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
    pending_input: VecDeque<u8>,
    pending_input_last_at: Option<u128>,
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
            pending_input: VecDeque::new(),
            pending_input_last_at: None,
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
        for &byte in input {
            self.pending_input_last_at = Some(self.clock.now_ms());
            self.pending_input.push_back(byte);

            if self.pending_input.len() == 1 && self.pending_input[0] == b'\x1B' {
                continue;
            }

            self.parse_pending_input(sr, pty_out, term_out)?;
        }
        Ok(())
    }

    fn parse_pending_input(
        &mut self,
        sr: &mut ScreenReader,
        pty_out: &mut dyn Write,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        loop {
            if self.pending_input.is_empty() {
                return Ok(());
            }

            let buf = self.pending_input.make_contiguous();
            match Event::parse_from(buf) {
                Ok(Some(event)) => {
                    let raw = buf.to_vec();
                    self.pending_input.clear();
                    self.pending_input_last_at = None;
                    self.handle_event(sr, event, &raw, pty_out, term_out)?;
                }
                Ok(None) => {
                    return Ok(());
                }
                Err(_) => {
                    let raw_byte = self
                        .pending_input
                        .pop_front()
                        .expect("pending input should not be empty");
                    if self.pending_input.is_empty() {
                        self.pending_input_last_at = None;
                    }
                    self.handle_raw_bytes(sr, &[raw_byte], pty_out, term_out)?;
                }
            }
        }
    }

    fn flush_pending_input(
        &mut self,
        sr: &mut ScreenReader,
        pty_out: &mut dyn Write,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let Some(last_at) = self.pending_input_last_at else {
            return Ok(());
        };
        if self.pending_input.is_empty() {
            self.pending_input_last_at = None;
            return Ok(());
        }
        if self.clock.now_ms().saturating_sub(last_at) < ESC_TIMEOUT_MS {
            return Ok(());
        }

        let raw: Vec<u8> = self.pending_input.drain(..).collect();
        self.pending_input_last_at = None;

        let forced_event = match raw.as_slice() {
            b"\x1B" => Some(Event::Key(KeyCode::Esc.into())),
            b"\x1B[" => Some(Event::Key(
                KeyEvent::new(KeyCode::Char('[')).modifiers(KeyModifiers::ALT),
            )),
            b"\x1BO" => Some(Event::Key(
                KeyEvent::new(KeyCode::Char('O')).modifiers(KeyModifiers::ALT),
            )),
            _ => None,
        };

        if let Some(event) = forced_event {
            self.handle_event(sr, event, &raw, pty_out, term_out)
        } else {
            self.handle_raw_bytes(sr, &raw, pty_out, term_out)
        }
    }

    fn handle_event(
        &mut self,
        sr: &mut ScreenReader,
        event: Event,
        raw: &[u8],
        pty_out: &mut dyn Write,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        match event {
            Event::Key(key_event) => {
                self.handle_key_event(sr, key_event, raw, pty_out, term_out)
            }
            Event::Paste(contents) => {
                let view_action = self
                    .view_stack
                    .active_mut()
                    .handle_paste(sr, &contents, pty_out)?;
                self.handle_view_action(sr, view_action, term_out)
            }
            _ => self.handle_raw_bytes(sr, raw, pty_out, term_out),
        }
    }

    fn handle_key_event(
        &mut self,
        sr: &mut ScreenReader,
        key_event: KeyEvent,
        raw: &[u8],
        pty_out: &mut dyn Write,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        self.update_last_key(sr, raw)?;
        if sr.pass_through {
            sr.pass_through = false;
            return self.dispatch_to_view(sr, raw, pty_out, term_out);
        }

        let binding = self.binding_for_key_event(sr, key_event);
        if let Some(binding) = binding {
            if sr.help_mode {
                if matches!(binding, Binding::Builtin(commands::Action::ToggleHelp)) {
                    // Allow exiting help mode.
                } else {
                    let help = binding.help_text();
                    sr.speech.speak(&help, false)?;
                    return Ok(());
                }
            }
            match binding {
                Binding::Builtin(action) => {
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
                    match commands::handle(sr, self.view_stack.active_mut().model(), *action)? {
                        commands::CommandResult::Handled => {}
                        commands::CommandResult::ForwardInput => {
                            self.dispatch_to_view(sr, raw, pty_out, term_out)?;
                        }
                        commands::CommandResult::Paste(contents) => {
                            let view_action = self
                                .view_stack
                                .active_mut()
                                .handle_paste(sr, &contents, pty_out)?;
                            self.handle_view_action(sr, view_action, term_out)?;
                        }
                    }
                }
                Binding::Lua(lua_binding) => {
                    lua_binding.call()?;
                }
            }
        } else if sr.help_mode {
            sr.speech.speak("this key is unmapped", false)?;
        } else {
            self.dispatch_to_view(sr, raw, pty_out, term_out)?;
        }
        Ok(())
    }

    fn handle_raw_bytes(
        &mut self,
        sr: &mut ScreenReader,
        raw: &[u8],
        pty_out: &mut dyn Write,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        self.update_last_key(sr, raw)?;
        if sr.pass_through {
            sr.pass_through = false;
        }
        self.dispatch_to_view(sr, raw, pty_out, term_out)
    }

    fn update_last_key(&mut self, sr: &mut ScreenReader, raw: &[u8]) -> Result<()> {
        if !self.ansi_csi_re.is_match(raw) {
            sr.last_key.clear();
            sr.last_key.extend_from_slice(raw);
            sr.speech.stop()?;
        }
        Ok(())
    }

    fn binding_for_key_event<'a>(
        &self,
        sr: &'a ScreenReader,
        key_event: KeyEvent,
    ) -> Option<&'a Binding> {
        let binding = self.key_event_binding_name(key_event)?;
        sr.key_bindings
            .binding_for_mode(sr.input_mode, binding.as_str())
    }

    fn key_event_binding_name(&self, key_event: KeyEvent) -> Option<String> {
        let key_event = key_event.normalize_case();
        let mut binding = String::new();
        let is_char = matches!(key_event.code, KeyCode::Char(_));

        if key_event.modifiers.contains(KeyModifiers::CTRL) {
            binding.push_str("C-");
        }
        if key_event.modifiers.contains(KeyModifiers::ALT)
            || key_event.modifiers.contains(KeyModifiers::META)
        {
            binding.push_str("M-");
        }
        if key_event.modifiers.contains(KeyModifiers::SUPER) {
            binding.push_str("Super-");
        }
        if key_event.modifiers.contains(KeyModifiers::HYPER) {
            binding.push_str("Hyper-");
        }
        if !is_char && key_event.modifiers.contains(KeyModifiers::SHIFT) {
            binding.push_str("S-");
        }

        match key_event.code {
            KeyCode::Char(ch) => {
                binding.push(ch);
            }
            KeyCode::Backspace => binding.push_str("Backspace"),
            KeyCode::Delete => binding.push_str("Delete"),
            KeyCode::Esc => binding.push_str("Esc"),
            KeyCode::Enter => binding.push_str("Enter"),
            KeyCode::Tab => binding.push_str("Tab"),
            KeyCode::F(num) => {
                binding.push_str(&format!("F{num}"));
            }
            _ => return None,
        }

        Some(binding)
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
        self.flush_pending_input(sr, pty_out, term_out)?;
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
