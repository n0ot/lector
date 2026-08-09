use super::*;

impl App {
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

            match osc_status(self.pending_input.make_contiguous()) {
                SequenceStatus::Complete(osc_len) => {
                    let raw: Vec<u8> = self.pending_input.drain(..osc_len).collect();
                    self.pending_input_last_at = None;
                    self.log_bytes("recognized OSC sequence", &raw);
                    self.handle_raw_bytes(sr, &raw, pty_out, term_out)?;
                    continue;
                }
                SequenceStatus::Incomplete => return Ok(()),
                SequenceStatus::None => {}
            }

            match focus_event_status(self.pending_input.make_contiguous()) {
                SequenceStatus::Complete(focused) => {
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
                SequenceStatus::Incomplete => return Ok(()),
                SequenceStatus::None => {}
            }

            match modify_other_keys_status(self.pending_input.make_contiguous()) {
                ModifyOtherKeysStatus::Event(len, event) => {
                    let raw: Vec<u8> = self.pending_input.drain(..len).collect();
                    if self.pending_input.is_empty() {
                        self.pending_input_last_at = None;
                    }
                    self.log_bytes("parsed terminal event bytes", &raw);
                    self.handle_event(sr, event, &raw, pty_out, term_out)?;
                    continue;
                }
                ModifyOtherKeysStatus::Raw(len) => {
                    let raw: Vec<u8> = self.pending_input.drain(..len).collect();
                    if self.pending_input.is_empty() {
                        self.pending_input_last_at = None;
                    }
                    self.log_bytes("recognized modifyOtherKeys sequence", &raw);
                    self.handle_raw_bytes(sr, &raw, pty_out, term_out)?;
                    continue;
                }
                ModifyOtherKeysStatus::Incomplete => return Ok(()),
                ModifyOtherKeysStatus::None => {}
            }

            if is_invalid_ss3_prefix(self.pending_input.make_contiguous()) {
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
                self.focus_mode.enabled(),
            );
        }
        sr.set_terminal_focused(focused)?;
        if self.focus_mode.enabled() {
            let raw = if focused {
                FOCUS_IN_EVENT
            } else {
                FOCUS_OUT_EVENT
            };
            self.dispatch_to_view(sr, raw, pty_out, term_out)?;
        }
        Ok(())
    }

    pub(super) fn flush_pending_input(
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

        if let Some(event) = timed_out_event(&raw) {
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
        let key_id = (key_event.code, key_event.modifiers);
        if key_event.kind == KeyEventKind::Release {
            if self.consumed_key_presses.remove(&key_id) {
                self.log_event("swallowing release for Lector command");
                return Ok(());
            }
            return self.dispatch_to_view(sr, raw, pty_out, term_out);
        }

        self.update_last_key(sr, raw, true)?;
        if sr.take_pass_through() {
            self.consumed_key_presses.remove(&key_id);
            return self.dispatch_key_to_view(sr, key_event, raw, pty_out, term_out);
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
            sr.key_bindings()
                .binding_for_mode(sr.input_mode(), name.as_str())
        });
        if let Some(binding) = binding {
            if sr.help_mode() {
                if matches!(binding, Binding::Builtin(commands::Action::ToggleHelp)) {
                    // Allow exiting help mode.
                } else {
                    let help = binding.help_text().to_owned();
                    sr.speak(&help, false)?;
                    self.consumed_key_presses.insert(key_id);
                    return Ok(());
                }
            }
            match binding {
                Binding::Builtin(action) => {
                    if matches!(action, commands::Action::OpenLuaRepl) {
                        if self.view_stack.active_mut().kind() == views::ViewKind::LuaRepl {
                            sr.speak("Lua REPL already open", false)?;
                            self.consumed_key_presses.insert(key_id);
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
                        self.consumed_key_presses.insert(key_id);
                        return Ok(());
                    }
                    let mode_before = sr.input_mode();
                    let title = self.view_stack.active_mut().title().to_string();
                    let consumed = match commands::handle(
                        sr,
                        &title,
                        self.view_stack.active_mut().model(),
                        *action,
                    )? {
                        commands::CommandResult::Handled => true,
                        commands::CommandResult::ForwardInput => {
                            self.dispatch_key_to_view(sr, key_event, raw, pty_out, term_out)?;
                            false
                        }
                        commands::CommandResult::Paste(contents) => {
                            let view_action = self
                                .view_stack
                                .active_mut()
                                .handle_paste(sr, &contents, pty_out)?;
                            self.handle_view_action(sr, view_action, term_out)?;
                            true
                        }
                        commands::CommandResult::PtyInput(input) => {
                            self.dispatch_to_view(sr, &input, pty_out, term_out)?;
                            true
                        }
                    };
                    if consumed {
                        self.consumed_key_presses.insert(key_id);
                    } else {
                        self.consumed_key_presses.remove(&key_id);
                    }
                    if mode_before == crate::keymap::InputMode::TableSetup
                        && sr.input_mode() != crate::keymap::InputMode::TableSetup
                    {
                        self.flush_deferred_pty_output(sr, term_out)?;
                    }
                }
                Binding::Lua(lua_binding) => {
                    let mode_before = sr.input_mode();
                    lua_binding.call()?;
                    self.consumed_key_presses.insert(key_id);
                    if mode_before == crate::keymap::InputMode::TableSetup
                        && sr.input_mode() != crate::keymap::InputMode::TableSetup
                    {
                        self.flush_deferred_pty_output(sr, term_out)?;
                    }
                }
            }
        } else if sr.help_mode() {
            sr.speak("this key is unmapped", false)?;
            self.consumed_key_presses.insert(key_id);
        } else {
            if matches!(
                sr.input_mode(),
                crate::keymap::InputMode::Table | crate::keymap::InputMode::TableSetup
            ) {
                if sr.hook_on_key_unhandled(binding_name.as_deref(), sr.input_mode())? {
                    self.consumed_key_presses.insert(key_id);
                    return Ok(());
                }
                self.consumed_key_presses.insert(key_id);
                return Ok(());
            }
            if sr.hook_on_key_unhandled(binding_name.as_deref(), sr.input_mode())? {
                self.consumed_key_presses.insert(key_id);
                return Ok(());
            }
            if self.view_stack.has_overlay()
                && let Some(translated) = Self::overlay_input_bytes_for_key_event(key_event)
            {
                self.consumed_key_presses.remove(&key_id);
                return self.dispatch_key_to_view(sr, key_event, &translated, pty_out, term_out);
            }
            self.consumed_key_presses.remove(&key_id);
            self.dispatch_key_to_view(sr, key_event, raw, pty_out, term_out)?;
        }
        Ok(())
    }

    fn dispatch_key_to_view(
        &mut self,
        sr: &mut ScreenReader,
        key_event: KeyEvent,
        input: &[u8],
        pty_out: &mut dyn Write,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let key_event = key_event.normalize_case();
        let has_non_shift_modifier = key_event.modifiers.contains(KeyModifiers::CTRL)
            || key_event.modifiers.contains(KeyModifiers::ALT)
            || key_event.modifiers.contains(KeyModifiers::META)
            || key_event.modifiers.contains(KeyModifiers::SUPER)
            || key_event.modifiers.contains(KeyModifiers::HYPER);
        if !has_non_shift_modifier && let KeyCode::Char(character) = key_event.code {
            sr.record_forwarded_character(character);
        }
        self.dispatch_to_view(sr, input, pty_out, term_out)
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
        let _ = sr.take_pass_through();
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
        if decoded_key_event || !ANSI_CSI_RE.is_match(raw) {
            sr.record_last_key(raw);
            sr.stop_speaking()?;
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
}
