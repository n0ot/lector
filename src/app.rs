use crate::{commands, keymap::Binding, perform, screen_reader::ScreenReader, views};
use anyhow::{Context, Result};
use std::{collections::VecDeque, io::Write, time};
use terminput::{Event, KeyCode, KeyEvent, KeyModifiers};

pub const DIFF_DELAY: u16 = 30;
pub const MAX_DIFF_DELAY: u16 = 300;
const ESC_TIMEOUT_MS: u128 = 50;
const FOCUS_IN_EVENT: &[u8] = b"\x1B[I";
const FOCUS_OUT_EVENT: &[u8] = b"\x1B[O";
const FOCUS_EVENTS_ENABLE: &[u8] = b"\x1B[?1004h";
const FOCUS_EVENTS_DISABLE: &[u8] = b"\x1B[?1004l";
const OSC_START: u8 = b']';
const ST_ESCAPE: u8 = b'\\';
const MODIFY_OTHER_KEYS_PREFIX: &[u8] = b"\x1B[27;";

fn is_ss3_final(byte: u8) -> bool {
    matches!(byte, b'D' | b'C' | b'A' | b'B' | b'H' | b'F' | b'P'..=b'S')
}

pub trait Clock {
    fn now_ms(&self) -> u128;
}

pub struct StdClock {
    start: time::Instant,
}

impl Default for StdClock {
    fn default() -> Self {
        Self::new()
    }
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
    pending_pty_output: Vec<u8>,
    deferred_pty_output: Vec<u8>,
    app_focus_events_enabled: bool,
    log_enabled: bool,
    lua_repl_history: Vec<String>,
    last_stdin_update: Option<u128>,
    first_pty_update: Option<u128>,
    last_pty_update: Option<u128>,
    clock: Box<dyn Clock>,
}

impl App {
    pub fn new(view_stack: views::ViewStack) -> Result<Self> {
        Self::new_with_clock(view_stack, Box::new(StdClock::new()))
    }

    pub fn new_with_clock(view_stack: views::ViewStack, clock: Box<dyn Clock>) -> Result<Self> {
        let ansi_csi_re =
            regex::bytes::Regex::new(r"^\x1B\[[\x30-\x3F]*[\x20-\x2F]*[\x40-\x7E--[A-D~]]$")
                .context("compile ansi csi regex")?;
        let mut app = Self {
            view_stack,
            vte_parser: vte::Parser::new(),
            reporter: perform::Reporter::new(),
            ansi_csi_re,
            pending_input: VecDeque::new(),
            pending_input_last_at: None,
            pending_pty_output: Vec::new(),
            deferred_pty_output: Vec::new(),
            app_focus_events_enabled: false,
            log_enabled: false,
            lua_repl_history: Vec::new(),
            last_stdin_update: None,
            first_pty_update: None,
            last_pty_update: None,
            clock,
        };
        let now_ms = app.clock.now_ms();
        app.view_stack.active_mut().model().prev_screen_time = now_ms;
        Ok(app)
    }

    pub fn set_logging(&mut self, enabled: bool) {
        self.log_enabled = enabled;
    }

    fn format_bytes(bytes: &[u8]) -> String {
        let mut out = String::new();
        for &byte in bytes {
            match byte {
                b'\x1B' => out.push_str("\\e"),
                b'\r' => out.push_str("\\r"),
                b'\n' => out.push_str("\\n"),
                b'\t' => out.push_str("\\t"),
                0x20..=0x7E => out.push(byte as char),
                _ => out.push_str(&format!("\\x{byte:02X}")),
            }
        }
        out
    }

    fn log_bytes(&self, label: &str, bytes: &[u8]) {
        if self.log_enabled {
            eprintln!(
                "{label}: [{} bytes] {}",
                bytes.len(),
                Self::format_bytes(bytes)
            );
        }
    }

    fn log_event(&self, message: &str) {
        if self.log_enabled {
            eprintln!("{message}");
        }
    }

    pub fn wants_tick(&mut self) -> bool {
        self.view_stack.active_mut().wants_tick()
    }

    pub fn has_overlay(&self) -> bool {
        self.view_stack.has_overlay()
    }

    pub fn on_resize(&mut self, rows: u16, cols: u16, term_out: &mut dyn Write) -> Result<()> {
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
            rows, cols, title, message,
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
        self.log_bytes("stdin from terminal", input);
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
            if self.pending_input.len() == 1 && self.pending_input[0] == b'\x1B' {
                return Ok(());
            }

            match self.pending_osc_status() {
                PendingStatus::Complete(osc_len) => {
                    let raw: Vec<u8> = self.pending_input.drain(..osc_len).collect();
                    self.pending_input_last_at = None;
                    self.log_bytes("recognized OSC sequence", &raw);
                    self.handle_raw_bytes(sr, &raw, pty_out, term_out)?;
                    continue;
                }
                PendingStatus::Incomplete => return Ok(()),
                PendingStatus::None => {}
            }

            match self.pending_focus_event_status() {
                FocusPendingStatus::Complete(focused) => {
                    self.pending_input.drain(..FOCUS_IN_EVENT.len());
                    if self.pending_input.is_empty() {
                        self.pending_input_last_at = None;
                    }
                    self.log_event(if focused {
                        "recognized focus-in sequence"
                    } else {
                        "recognized focus-out sequence"
                    });
                    self.handle_focus_event(sr, focused, pty_out, term_out)?;
                    continue;
                }
                FocusPendingStatus::Incomplete => return Ok(()),
                FocusPendingStatus::None => {}
            }

            match self.pending_modify_other_keys_status() {
                ModifyOtherKeysPendingStatus::Complete(len, event) => {
                    let raw: Vec<u8> = self.pending_input.drain(..len).collect();
                    if self.pending_input.is_empty() {
                        self.pending_input_last_at = None;
                    }
                    self.log_bytes("parsed terminal event bytes", &raw);
                    self.handle_event(sr, event, &raw, pty_out, term_out)?;
                    continue;
                }
                ModifyOtherKeysPendingStatus::CompleteRaw(len) => {
                    let raw: Vec<u8> = self.pending_input.drain(..len).collect();
                    if self.pending_input.is_empty() {
                        self.pending_input_last_at = None;
                    }
                    self.log_bytes("recognized modifyOtherKeys sequence", &raw);
                    self.handle_raw_bytes(sr, &raw, pty_out, term_out)?;
                    continue;
                }
                ModifyOtherKeysPendingStatus::Incomplete => return Ok(()),
                ModifyOtherKeysPendingStatus::None => {}
            }

            if self.pending_input.len() >= 3
                && self.pending_input[0] == b'\x1B'
                && self.pending_input[1] == b'O'
                && !is_ss3_final(self.pending_input[2])
            {
                let raw: Vec<u8> = self.pending_input.drain(..2).collect();
                if self.pending_input.is_empty() {
                    self.pending_input_last_at = None;
                }
                self.log_bytes("reclassified partial SS3 as Alt-O", &raw);
                let event =
                    Event::Key(KeyEvent::new(KeyCode::Char('O')).modifiers(KeyModifiers::ALT));
                self.handle_event(sr, event, &raw, pty_out, term_out)?;
                continue;
            }

            let buf = self.pending_input.make_contiguous();
            match Event::parse_from(buf) {
                Ok(Some(event)) => {
                    let raw = buf.to_vec();
                    self.pending_input.clear();
                    self.pending_input_last_at = None;
                    self.log_bytes("parsed terminal event bytes", &raw);
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
                    self.log_bytes("forwarding undecodable byte", &[raw_byte]);
                    self.handle_raw_bytes(sr, &[raw_byte], pty_out, term_out)?;
                }
            }
        }
    }

    fn pending_osc_status(&mut self) -> PendingStatus {
        if self.pending_input.len() < 2 {
            return PendingStatus::None;
        }
        if self.pending_input[0] != b'\x1B' || self.pending_input[1] != OSC_START {
            return PendingStatus::None;
        }
        let buf = self.pending_input.make_contiguous();
        let mut i = 2usize;
        while i < buf.len() {
            match buf[i] {
                0x07 => return PendingStatus::Complete(i + 1),
                0x1B => {
                    if i + 1 < buf.len() && buf[i + 1] == ST_ESCAPE {
                        return PendingStatus::Complete(i + 2);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        PendingStatus::Incomplete
    }

    fn pending_modify_other_keys_status(&mut self) -> ModifyOtherKeysPendingStatus {
        parse_modify_other_keys(self.pending_input.make_contiguous())
    }

    fn pending_focus_event_status(&mut self) -> FocusPendingStatus {
        let buf = self.pending_input.make_contiguous();
        if buf.starts_with(FOCUS_IN_EVENT) {
            return FocusPendingStatus::Complete(true);
        }
        if FOCUS_IN_EVENT.starts_with(buf) && !buf.is_empty() {
            return FocusPendingStatus::Incomplete;
        }
        if buf.starts_with(FOCUS_OUT_EVENT) {
            return FocusPendingStatus::Complete(false);
        }
        if FOCUS_OUT_EVENT.starts_with(buf) && !buf.is_empty() {
            return FocusPendingStatus::Incomplete;
        }
        FocusPendingStatus::None
    }

    fn handle_focus_event(
        &mut self,
        sr: &mut ScreenReader,
        focused: bool,
        pty_out: &mut dyn Write,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        if self.log_enabled {
            eprintln!(
                "focus event: {} (forward_to_app={})",
                if focused { "in" } else { "out" },
                self.app_focus_events_enabled,
            );
        }
        sr.terminal_focused = focused;
        if !focused && sr.stop_speech_on_focus_loss {
            sr.speech.stop()?;
        }
        if self.app_focus_events_enabled {
            let raw = if focused {
                FOCUS_IN_EVENT
            } else {
                FOCUS_OUT_EVENT
            };
            self.dispatch_to_view(sr, raw, pty_out, term_out)?;
        }
        Ok(())
    }

    fn filter_focus_mode_sequences(&mut self, buf: &[u8]) -> Vec<u8> {
        self.pending_pty_output.extend_from_slice(buf);
        let mut out = Vec::with_capacity(self.pending_pty_output.len());
        let mut i = 0usize;

        while i < self.pending_pty_output.len() {
            let rem = &self.pending_pty_output[i..];
            if rem.starts_with(FOCUS_EVENTS_ENABLE) {
                self.app_focus_events_enabled = true;
                self.log_event("focus mode: app enabled ?1004 passthrough");
                i += FOCUS_EVENTS_ENABLE.len();
                continue;
            }
            if rem.starts_with(FOCUS_EVENTS_DISABLE) {
                self.app_focus_events_enabled = false;
                self.log_event("focus mode: app disabled ?1004 passthrough");
                i += FOCUS_EVENTS_DISABLE.len();
                continue;
            }
            if FOCUS_EVENTS_ENABLE.starts_with(rem) || FOCUS_EVENTS_DISABLE.starts_with(rem) {
                break;
            }
            out.push(self.pending_pty_output[i]);
            i += 1;
        }

        if i > 0 {
            self.pending_pty_output.drain(..i);
        }
        out
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
        self.log_bytes("flushing pending input after timeout", &raw);

        let forced_event = match raw.as_slice() {
            b"\x1B" => Some(Event::Key(KeyCode::Esc.into())),
            b"\x1B[" => Some(Event::Key(
                KeyEvent::new(KeyCode::Char('[')).modifiers(KeyModifiers::ALT),
            )),
            b"\x1B]" => Some(Event::Key(
                KeyEvent::new(KeyCode::Char(']')).modifiers(KeyModifiers::ALT),
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
            Event::Key(key_event) => self.handle_key_event(sr, key_event, raw, pty_out, term_out),
            Event::Paste(contents) => {
                self.log_event(&format!("parsed paste event: [{} chars]", contents.len()));
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
        self.update_last_key(sr, raw, true)?;
        if sr.pass_through {
            sr.pass_through = false;
            return self.dispatch_to_view(sr, raw, pty_out, term_out);
        }

        let binding_name = self.key_event_binding_name(key_event);
        if self.log_enabled {
            eprintln!(
                "parsed key event: binding={} raw={}",
                binding_name.as_deref().unwrap_or("<none>"),
                Self::format_bytes(raw)
            );
        }
        let binding = binding_name.as_ref().and_then(|name| {
            sr.key_bindings
                .binding_for_mode(sr.input_mode, name.as_str())
        });
        if let Some(binding) = binding {
            if sr.help_mode {
                if matches!(binding, Binding::Builtin(commands::Action::ToggleHelp)) {
                    // Allow exiting help mode.
                } else {
                    let help = binding.help_text();
                    sr.speak(&help, false)?;
                    return Ok(());
                }
            }
            match binding {
                Binding::Builtin(action) => {
                    if matches!(action, commands::Action::OpenLuaRepl) {
                        if self.view_stack.active_mut().kind() == views::ViewKind::LuaRepl {
                            sr.speak("Lua REPL already open", false)?;
                            return Ok(());
                        }
                        let (rows, cols) = self.view_stack.active_mut().model().size();
                        let repl =
                            views::LuaReplView::new(rows, cols, self.lua_repl_history.clone())?;
                        self.handle_view_action(
                            sr,
                            views::ViewAction::Push(Box::new(repl)),
                            term_out,
                        )?;
                        return Ok(());
                    }
                    let mode_before = sr.input_mode;
                    let title = self.view_stack.active_mut().title().to_string();
                    match commands::handle(
                        sr,
                        &title,
                        self.view_stack.active_mut().model(),
                        *action,
                    )? {
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
                        commands::CommandResult::PtyInput(input) => {
                            self.dispatch_to_view(sr, &input, pty_out, term_out)?;
                        }
                    }
                    if mode_before == crate::keymap::InputMode::TableSetup
                        && sr.input_mode != crate::keymap::InputMode::TableSetup
                    {
                        self.flush_deferred_pty_output(sr, term_out)?;
                    }
                }
                Binding::Lua(lua_binding) => {
                    let mode_before = sr.input_mode;
                    lua_binding.call()?;
                    if mode_before == crate::keymap::InputMode::TableSetup
                        && sr.input_mode != crate::keymap::InputMode::TableSetup
                    {
                        self.flush_deferred_pty_output(sr, term_out)?;
                    }
                }
            }
        } else if sr.help_mode {
            sr.speak("this key is unmapped", false)?;
        } else {
            if matches!(
                sr.input_mode,
                crate::keymap::InputMode::Table | crate::keymap::InputMode::TableSetup
            ) {
                if sr.hook_on_key_unhandled(binding_name.as_deref(), sr.input_mode)? {
                    return Ok(());
                }
                return Ok(());
            }
            if sr.hook_on_key_unhandled(binding_name.as_deref(), sr.input_mode)? {
                return Ok(());
            }
            if self.view_stack.has_overlay()
                && let Some(translated) = Self::overlay_input_bytes_for_key_event(key_event)
            {
                return self.dispatch_to_view(sr, &translated, pty_out, term_out);
            }
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
        self.log_bytes("forwarding raw bytes to active view", raw);
        self.update_last_key(sr, raw, false)?;
        if sr.pass_through {
            sr.pass_through = false;
        }
        self.dispatch_to_view(sr, raw, pty_out, term_out)
    }

    fn update_last_key(
        &mut self,
        sr: &mut ScreenReader,
        raw: &[u8],
        decoded_key_event: bool,
    ) -> Result<()> {
        sr.clear_pending_delete();
        // A decoded key press should always interrupt speech. In particular, Kitty's
        // keyboard protocol encodes Control and Meta keys as CSI-u sequences, which
        // look like non-key terminal traffic to the raw-byte heuristic below.
        if decoded_key_event || !self.ansi_csi_re.is_match(raw) {
            sr.last_key.clear();
            sr.last_key.extend_from_slice(raw);
            sr.speech.stop()?;
        }
        Ok(())
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

    fn overlay_input_bytes_for_key_event(key_event: KeyEvent) -> Option<Vec<u8>> {
        let key_event = key_event.normalize_case();
        if key_event.modifiers != KeyModifiers::CTRL {
            return None;
        }
        let KeyCode::Char(ch) = key_event.code else {
            return None;
        };
        let byte = match ch {
            '@' | ' ' => 0x00,
            'a'..='z' => (ch as u8) - b'a' + 1,
            '[' => 0x1B,
            '\\' => 0x1C,
            ']' => 0x1D,
            '^' => 0x1E,
            '_' => 0x1F,
            _ => return None,
        };
        Some(vec![byte])
    }

    pub fn handle_pty(
        &mut self,
        sr: &mut ScreenReader,
        buf: &[u8],
        term_out: &mut dyn Write,
    ) -> Result<()> {
        if matches!(sr.input_mode, crate::keymap::InputMode::TableSetup) {
            self.deferred_pty_output.extend_from_slice(buf);
            return Ok(());
        }

        if self.deferred_pty_output.is_empty() {
            self.process_pty_output(sr, buf, term_out)?;
        } else {
            let mut merged = Vec::with_capacity(self.deferred_pty_output.len() + buf.len());
            merged.extend_from_slice(&self.deferred_pty_output);
            merged.extend_from_slice(buf);
            self.deferred_pty_output.clear();
            self.process_pty_output(sr, &merged, term_out)?;
        }
        Ok(())
    }

    fn process_pty_output(
        &mut self,
        sr: &mut ScreenReader,
        buf: &[u8],
        term_out: &mut dyn Write,
    ) -> Result<()> {
        self.log_bytes("pty output from app", buf);
        let terminal_buf = self.filter_focus_mode_sequences(buf);
        if terminal_buf != buf {
            self.log_bytes("pty output after focus filtering", &terminal_buf);
        }
        let overlay_active = self.view_stack.has_overlay();
        self.view_stack.root_mut().handle_pty_output(buf)?;
        if !overlay_active {
            term_out
                .write_all(&terminal_buf)
                .context("write PTY output")?;
            term_out.flush().context("flush output")?;
            if sr.auto_read {
                self.vte_parser.advance(&mut self.reporter, buf);
            }
        }
        let now_ms = self.clock.now_ms();
        if self.first_pty_update.is_none() {
            self.first_pty_update = Some(now_ms);
        }
        self.last_pty_update = Some(now_ms);
        Ok(())
    }

    fn flush_deferred_pty_output(
        &mut self,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        if self.deferred_pty_output.is_empty() {
            return Ok(());
        }
        if matches!(sr.input_mode, crate::keymap::InputMode::TableSetup) {
            return Ok(());
        }
        let deferred = std::mem::take(&mut self.deferred_pty_output);
        self.process_pty_output(sr, &deferred, term_out)
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
        let first_pty_update = self.first_pty_update.unwrap_or(lpu);
        let now_ms = self.clock.now_ms();
        let overlay_active = self.view_stack.has_overlay();
        let root_view = self.view_stack.root_mut();
        let view = root_view.model();
        if now_ms.saturating_sub(lpu) >= DIFF_DELAY as u128
            || now_ms.saturating_sub(first_pty_update) >= MAX_DIFF_DELAY as u128
        {
            self.first_pty_update = None;
            self.last_pty_update = None;
            if !overlay_active {
                let mut read_text = sr.resolve_pending_delete(view)?;
                if sr.highlight_tracking {
                    sr.track_highlighting(view)?;
                }
                let recent_input = self
                    .last_stdin_update
                    .is_some_and(|lsu| now_ms.saturating_sub(lsu) <= MAX_DIFF_DELAY as u128);
                let auto_read_text = if sr.auto_read {
                    if recent_input {
                        sr.auto_read_after_input(view, &mut self.reporter)?
                    } else {
                        sr.auto_read(view, &mut self.reporter)?
                    }
                } else {
                    false
                };
                read_text |= auto_read_text;
                if recent_input && !read_text {
                    sr.track_cursor(view)?;
                }
            }

            if sr.review_follows_screen_cursor
                && view.screen().cursor_position() != view.prev_screen().cursor_position()
            {
                let old = view.review_cursor_position;
                view.review_cursor_position = view.screen().cursor_position();
                sr.hook_on_review_cursor_move(old, view.review_cursor_position)?;
            }

            sr.hook_on_screen_update(view, overlay_active)?;
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
        self.log_bytes("dispatching bytes to active view", input);
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
                self.capture_lua_repl_history();
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

    fn capture_lua_repl_history(&mut self) {
        if self.view_stack.active_mut().kind() != views::ViewKind::LuaRepl {
            return;
        }
        let history = self
            .view_stack
            .active_mut()
            .as_any()
            .downcast_ref::<views::LuaReplView>()
            .map(|repl| repl.history().to_vec());
        if let Some(history) = history {
            self.lua_repl_history = history;
        }
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
        sr.speak(&title, false)?;
        let contents = view.contents_full();
        if contents.trim().is_empty() {
            sr.speak("blank screen", false)?;
        } else {
            sr.speak(&contents, false)?;
        }
        view.finalize_changes(self.clock.now_ms());
        Ok(())
    }

    fn read_active_view_changes(&mut self, sr: &mut ScreenReader) -> Result<()> {
        let now_ms = self.clock.now_ms();
        let overlay_active = self.view_stack.has_overlay();
        let view = self.view_stack.active_mut().model();
        let mut read_text = sr.resolve_pending_delete(view)?;
        let recent_input = self
            .last_stdin_update
            .is_some_and(|lsu| now_ms.saturating_sub(lsu) <= MAX_DIFF_DELAY as u128);
        let auto_read_text = if sr.auto_read {
            let mut reporter = perform::Reporter::new();
            if recent_input {
                sr.auto_read_after_input(view, &mut reporter)?
            } else {
                sr.auto_read(view, &mut reporter)?
            }
        } else {
            false
        };
        read_text |= auto_read_text;
        if recent_input && !read_text {
            sr.track_cursor(view)?;
        }
        if sr.review_follows_screen_cursor
            && view.screen().cursor_position() != view.prev_screen().cursor_position()
        {
            let old = view.review_cursor_position;
            view.review_cursor_position = view.screen().cursor_position();
            sr.hook_on_review_cursor_move(old, view.review_cursor_position)?;
        }
        sr.hook_on_screen_update(view, overlay_active)?;
        view.finalize_changes(now_ms);
        Ok(())
    }

    pub fn debug_active_view_contents(&mut self) -> String {
        self.view_stack.active_mut().model().contents_full()
    }
}

enum PendingStatus {
    None,
    Incomplete,
    Complete(usize),
}

enum FocusPendingStatus {
    None,
    Incomplete,
    Complete(bool),
}

#[derive(Debug, PartialEq)]
enum ModifyOtherKeysPendingStatus {
    None,
    Incomplete,
    Complete(usize, Event),
    CompleteRaw(usize),
}

fn parse_modify_other_keys(buf: &[u8]) -> ModifyOtherKeysPendingStatus {
    if buf.is_empty() {
        return ModifyOtherKeysPendingStatus::None;
    }
    if MODIFY_OTHER_KEYS_PREFIX.starts_with(buf) && buf.len() < MODIFY_OTHER_KEYS_PREFIX.len() {
        return ModifyOtherKeysPendingStatus::Incomplete;
    }
    if !buf.starts_with(MODIFY_OTHER_KEYS_PREFIX) {
        return ModifyOtherKeysPendingStatus::None;
    }

    let mut end = MODIFY_OTHER_KEYS_PREFIX.len();
    while end < buf.len() {
        match buf[end] {
            b'0'..=b'9' | b';' => {
                end += 1;
            }
            b'~' => {
                let raw = &buf[..=end];
                return parse_modify_other_keys_event(raw)
                    .map(|event| ModifyOtherKeysPendingStatus::Complete(raw.len(), event))
                    .unwrap_or(ModifyOtherKeysPendingStatus::CompleteRaw(raw.len()));
            }
            _ => return ModifyOtherKeysPendingStatus::None,
        }
    }

    ModifyOtherKeysPendingStatus::Incomplete
}

fn parse_modify_other_keys_event(raw: &[u8]) -> Option<Event> {
    let body = std::str::from_utf8(&raw[2..raw.len().checked_sub(1)?]).ok()?;
    let mut parts = body.split(';');
    let prefix = parts.next()?;
    let modifiers = parts.next()?;
    let keycode = parts.next()?;
    if prefix != "27" || parts.next().is_some() {
        return None;
    }

    let translated = format!("\x1B[{keycode};{modifiers}u");
    Event::parse_from(translated.as_bytes()).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::{
        ModifyOtherKeysPendingStatus, parse_modify_other_keys, parse_modify_other_keys_event,
    };
    use terminput::{Event, KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn parse_modify_other_keys_shift_enter() {
        assert_eq!(
            parse_modify_other_keys_event(b"\x1B[27;2;13~"),
            Some(Event::Key(
                KeyEvent::new(KeyCode::Enter).modifiers(KeyModifiers::SHIFT)
            ))
        );
    }

    #[test]
    fn parse_modify_other_keys_ctrl_d() {
        assert_eq!(
            parse_modify_other_keys_event(b"\x1B[27;5;100~"),
            Some(Event::Key(
                KeyEvent::new(KeyCode::Char('d')).modifiers(KeyModifiers::CTRL)
            ))
        );
    }

    #[test]
    fn modify_other_keys_prefix_stays_buffered_until_complete() {
        assert_eq!(
            parse_modify_other_keys(b"\x1B[27;2;13"),
            ModifyOtherKeysPendingStatus::Incomplete
        );
    }
}
