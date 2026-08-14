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
        let input = if let Some(broker) = self.startup_probe_broker.as_mut() {
            broker.ingest(input, self.clock.now_ms())
        } else {
            input.to_vec()
        };
        self.refresh_probed_profile();
        for byte in input {
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
        let forward_to_app = self
            .view_stack
            .active_mut()
            .model()
            .screen()
            .focus_reporting();
        if self.log_enabled {
            eprintln!(
                "focus event: {} (forward_to_app={})",
                if focused { "in" } else { "out" },
                forward_to_app,
            );
        }
        sr.set_terminal_focused(focused)?;
        if forward_to_app {
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
            Event::Key(key_event) => {
                let key = KeyInput::new(key_event, raw);
                self.handle_key_event(sr, key, raw, pty_out, term_out)
            }
            Event::Paste(contents) => {
                self.log_event(&format!("parsed paste event: [{} chars]", contents.len()));
                let view_action = self
                    .view_stack
                    .active_mut()
                    .handle_paste(sr, &contents, pty_out)?;
                self.handle_view_action(sr, view_action, term_out)
            }
            Event::Mouse(mouse) => {
                if let Some(view) = self.view_stack.active_tmux_connection_mut() {
                    if let Some(action) = view.translate_mouse_input(mouse) {
                        self.last_stdin_update = Some(self.clock.now_ms());
                        self.handle_view_action(sr, action, term_out)
                    } else {
                        Ok(())
                    }
                } else {
                    self.handle_raw_bytes(sr, raw, pty_out, term_out)
                }
            }
            _ => self.handle_raw_bytes(sr, raw, pty_out, term_out),
        }
    }

    fn handle_key_event(
        &mut self,
        sr: &mut ScreenReader,
        key: KeyInput,
        raw: &[u8],
        pty_out: &mut dyn Write,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let key_event = key.event();
        let key_id = (key_event.code, key_event.modifiers, key_event.state);
        if key_event.kind == KeyEventKind::Release {
            let command_consumed = self.consumed_key_presses.remove(&key_id);
            let transition_consumed = self.view_transition_key_presses.remove(&key_id);
            if command_consumed || transition_consumed {
                self.log_event("swallowing release for consumed key press");
                return Ok(());
            }
            return self.dispatch_key_to_view(sr, &key, raw, pty_out, term_out);
        }
        self.view_transition_key_presses.remove(&key_id);

        self.update_last_key(sr, raw, true)?;
        if sr.take_pass_through() {
            self.consumed_key_presses.remove(&key_id);
            return self.dispatch_key_to_view(sr, &key, raw, pty_out, term_out);
        }

        if self.handle_tmux_prefix_key(sr, &key, term_out)? {
            self.consumed_key_presses.insert(key_id);
            return Ok(());
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
                    let action = *action;
                    if matches!(action, commands::Action::OpenReview) {
                        if self.view_stack.active_mut().kind() == views::ViewKind::Review {
                            sr.speak("Review already open", false)?;
                        } else {
                            let review = {
                                let active = self.view_stack.active_mut();
                                views::ReviewView::new(active.model())
                            };
                            self.handle_view_action(
                                sr,
                                views::ViewAction::Push(Box::new(review)),
                                term_out,
                            )?;
                        }
                        self.consumed_key_presses.insert(key_id);
                        return Ok(());
                    }
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
                    let tmux_overlay_opened = match action {
                        commands::Action::OpenTmuxConnectionChooser => {
                            Some(self.show_tmux_connection_chooser(sr, term_out)?)
                        }
                        commands::Action::RenameTmuxConnection => {
                            Some(self.show_tmux_connection_rename(sr, term_out)?)
                        }
                        commands::Action::OpenTmuxSessionChooser => {
                            Some(self.show_tmux_session_chooser(sr, term_out)?)
                        }
                        commands::Action::OpenTmuxWindowChooser => {
                            Some(self.show_tmux_window_chooser(sr, term_out)?)
                        }
                        commands::Action::OpenTmuxPaneChooser => {
                            Some(self.show_tmux_pane_chooser(sr, term_out)?)
                        }
                        commands::Action::OpenTmuxCommandPrompt => {
                            Some(self.show_tmux_command_prompt(sr, term_out)?)
                        }
                        commands::Action::DetachTmuxConnection => {
                            Some(self.request_tmux_gateway_action(
                                sr,
                                crate::tmux_lifecycle::GatewayControlAction::GracefulDetach,
                                term_out,
                            )?)
                        }
                        commands::Action::InterruptTmuxGateway => {
                            Some(self.request_tmux_gateway_action(
                                sr,
                                crate::tmux_lifecycle::GatewayControlAction::Interrupt,
                                term_out,
                            )?)
                        }
                        commands::Action::ForceCloseTmuxGateway => {
                            Some(self.request_tmux_gateway_action(
                                sr,
                                crate::tmux_lifecycle::GatewayControlAction::ForceClose,
                                term_out,
                            )?)
                        }
                        commands::Action::SendTmuxSshEscapeDisconnect => {
                            Some(self.request_tmux_gateway_action(
                                sr,
                                crate::tmux_lifecycle::GatewayControlAction::SshEscapeDisconnect,
                                term_out,
                            )?)
                        }
                        commands::Action::SendTmuxSshEscapeHelp => {
                            Some(self.request_tmux_gateway_action(
                                sr,
                                crate::tmux_lifecycle::GatewayControlAction::SshEscapeHelp,
                                term_out,
                            )?)
                        }
                        _ => None,
                    };
                    if tmux_overlay_opened.is_some() {
                        if tmux_overlay_opened == Some(false) {
                            self.emit_physical_bells(term_out, 1)?;
                        }
                        self.consumed_key_presses.insert(key_id);
                        return Ok(());
                    }
                    if matches!(action, commands::Action::LeftClick)
                        && let Some(view_action) = self
                            .view_stack
                            .active_mut()
                            .place_application_cursor_at_review_cursor()
                    {
                        self.last_stdin_update = Some(self.clock.now_ms());
                        self.handle_view_action(sr, view_action, term_out)?;
                        self.consumed_key_presses.insert(key_id);
                        return Ok(());
                    }
                    let mode_before = sr.input_mode();
                    let title = {
                        let active = self.view_stack.active_mut();
                        if let Some(tmux) =
                            active.as_any().downcast_ref::<views::TmuxConnectionView>()
                        {
                            tmux.accessible_title()
                        } else if active.kind() == views::ViewKind::Terminal {
                            active
                                .model()
                                .screen()
                                .title
                                .as_deref()
                                .filter(|title| !title.is_empty())
                                .map_or_else(
                                    || "terminal".to_string(),
                                    |title| format!("terminal, {title}"),
                                )
                        } else {
                            active.title().to_string()
                        }
                    };
                    let consumed = match commands::handle(
                        sr,
                        &title,
                        self.view_stack.active_mut().model(),
                        action,
                    )? {
                        commands::CommandResult::Handled => true,
                        commands::CommandResult::ForwardInput => {
                            self.dispatch_key_to_view(sr, &key, raw, pty_out, term_out)?;
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
                    self.sync_table_setup_layer(mode_before, sr, term_out)?;
                }
                Binding::Lua(lua_binding) => {
                    let mode_before = sr.input_mode();
                    lua_binding.call()?;
                    self.consumed_key_presses.insert(key_id);
                    self.sync_table_setup_layer(mode_before, sr, term_out)?;
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
            self.consumed_key_presses.remove(&key_id);
            self.dispatch_key_to_view(sr, &key, raw, pty_out, term_out)?;
        }
        Ok(())
    }

    fn dispatch_key_to_view(
        &mut self,
        sr: &mut ScreenReader,
        key: &KeyInput,
        input: &[u8],
        pty_out: &mut dyn Write,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let event = key.event();
        if !key.is_release()
            && event.modifiers.is_empty()
            && matches!(event.code, KeyCode::Up | KeyCode::Down)
        {
            sr.set_pending_history_navigation();
        }
        if !key.is_release()
            && let Some(text) = key.text()
        {
            for character in text.chars() {
                sr.record_forwarded_character(character);
            }
        }
        self.log_bytes("dispatching decoded key to active view", input);
        self.last_stdin_update = Some(self.clock.now_ms());
        let action = self
            .view_stack
            .active_mut()
            .handle_key_input(sr, key, input, pty_out)?;
        if matches!(
            action,
            views::ViewAction::Pop | views::ViewAction::PopupResponse(_)
        ) {
            let event = key.event();
            self.view_transition_key_presses
                .insert((event.code, event.modifiers, event.state));
        }
        self.handle_view_action(sr, action, term_out)
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
        sr.clear_pending_history_navigation();
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
            KeyCode::Up => binding.push_str("Up"),
            KeyCode::Down => binding.push_str("Down"),
            KeyCode::Left => binding.push_str("Left"),
            KeyCode::Right => binding.push_str("Right"),
            KeyCode::Home => binding.push_str("Home"),
            KeyCode::End => binding.push_str("End"),
            KeyCode::PageUp => binding.push_str("PageUp"),
            KeyCode::PageDown => binding.push_str("PageDown"),
            KeyCode::Insert => binding.push_str("Insert"),
            KeyCode::F(num) => {
                binding.push_str(&format!("F{num}"));
            }
            _ => return None,
        }

        Some(binding)
    }
}
