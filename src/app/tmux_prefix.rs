use super::*;

impl App {
    pub(super) fn handle_tmux_prefix_key(
        &mut self,
        sr: &mut ScreenReader,
        key: &KeyInput,
        term_out: &mut dyn Write,
    ) -> Result<bool> {
        if self.view_stack.has_overlay() {
            return Ok(false);
        }
        let Some(connection_id) = self
            .view_stack
            .active_tmux_connection_mut()
            .filter(|view| view.is_ready() && !view.is_showing_connection_portal())
            .map(|view| view.connection_id())
        else {
            return Ok(false);
        };
        let key_name = crate::tmux_prefix::tmux_key_name(key.event());
        let now_ms = self.clock.now_ms();
        let previous = self
            .tmux_connections
            .iter_mut()
            .find(|connection| connection.id == connection_id)
            .and_then(|connection| connection.prefix_state.take())
            .filter(|state| match &state.phase {
                TmuxPrefixPhase::Awaiting { .. } => true,
                TmuxPrefixPhase::Repeating { expires_at_ms, .. } => now_ms <= *expires_at_ms,
            });

        if let Some(state) = previous {
            match state.phase {
                TmuxPrefixPhase::Awaiting { table } => {
                    let Some(key_name) = key_name else {
                        return self.unbound_tmux_table_key(sr, &table);
                    };
                    if table == "prefix" && key_name == "Escape" {
                        return Ok(true);
                    }
                    let binding = self.tmux_binding(connection_id, &table, &key_name);
                    let Some(binding) = binding else {
                        return self.unbound_tmux_table_key(sr, &table);
                    };
                    self.execute_tmux_binding(sr, connection_id, &binding.command, term_out)?;
                    if binding.repeatable {
                        self.set_tmux_prefix_state(
                            connection_id,
                            TmuxPrefixState {
                                phase: TmuxPrefixPhase::Repeating {
                                    table,
                                    expires_at_ms: now_ms
                                        .saturating_add(self.tmux_repeat_time(connection_id)),
                                },
                            },
                        );
                    }
                    return Ok(true);
                }
                TmuxPrefixPhase::Repeating { table, .. } => {
                    if let Some(key_name) = &key_name
                        && let Some(binding) = self.tmux_binding(connection_id, &table, key_name)
                        && binding.repeatable
                    {
                        self.execute_tmux_binding(sr, connection_id, &binding.command, term_out)?;
                        self.set_tmux_prefix_state(
                            connection_id,
                            TmuxPrefixState {
                                phase: TmuxPrefixPhase::Repeating {
                                    table,
                                    expires_at_ms: now_ms
                                        .saturating_add(self.tmux_repeat_time(connection_id)),
                                },
                            },
                        );
                        return Ok(true);
                    }
                }
            }
        }

        let Some(key_name) = key_name else {
            return Ok(false);
        };
        if self.is_tmux_prefix(connection_id, &key_name) {
            self.set_tmux_prefix_state(
                connection_id,
                TmuxPrefixState {
                    phase: TmuxPrefixPhase::Awaiting {
                        table: "prefix".to_owned(),
                    },
                },
            );
            sr.speak("tmux", false)?;
            return Ok(true);
        }
        let table = self.tmux_default_key_table(connection_id);
        if let Some(binding) = self.tmux_binding(connection_id, &table, &key_name) {
            self.execute_tmux_binding(sr, connection_id, &binding.command, term_out)?;
            if binding.repeatable {
                self.set_tmux_prefix_state(
                    connection_id,
                    TmuxPrefixState {
                        phase: TmuxPrefixPhase::Repeating {
                            table,
                            expires_at_ms: now_ms
                                .saturating_add(self.tmux_repeat_time(connection_id)),
                        },
                    },
                );
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn unbound_tmux_table_key(&self, sr: &mut ScreenReader, table: &str) -> Result<bool> {
        if table == "prefix" {
            sr.speak("tmux prefix key unbound", false)?;
            Ok(true)
        } else {
            // tmux sends an unbound root/custom-table key to the pane. Returning
            // false preserves the original terminal bytes through that path.
            Ok(false)
        }
    }

    fn set_tmux_prefix_state(&mut self, connection_id: u64, state: TmuxPrefixState) {
        if let Some(connection) = self
            .tmux_connections
            .iter_mut()
            .find(|connection| connection.id == connection_id)
        {
            connection.prefix_state = Some(state);
        }
    }

    fn tmux_binding(
        &self,
        connection_id: u64,
        table: &str,
        key: &str,
    ) -> Option<crate::tmux_model::TmuxBinding> {
        self.tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id)?
            .topology
            .binding_in_table(table, key)
            .cloned()
    }

    fn tmux_default_key_table(&self, connection_id: u64) -> String {
        self.tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id)
            .and_then(|connection| {
                connection
                    .key_table_override
                    .as_deref()
                    .or_else(|| connection.topology.option("key-table"))
            })
            .unwrap_or("root")
            .to_owned()
    }

    fn is_tmux_prefix(&self, connection_id: u64, key: &str) -> bool {
        let Some(topology) = self
            .tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id)
            .map(|connection| &connection.topology)
        else {
            return false;
        };
        topology.option("prefix") == Some(key)
            || topology
                .option("prefix2")
                .is_some_and(|prefix| prefix != "None" && prefix == key)
    }

    fn tmux_repeat_time(&self, connection_id: u64) -> u128 {
        self.tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id)
            .and_then(|connection| connection.topology.option("repeat-time"))
            .and_then(|value| value.parse::<u128>().ok())
            .unwrap_or(500)
            .min(60_000)
    }

    fn execute_tmux_binding(
        &mut self,
        sr: &mut ScreenReader,
        connection_id: u64,
        command: &str,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        match crate::tmux_prefix::classify_binding(command)? {
            crate::tmux_prefix::BindingAction::OpenReview { page_up } => {
                self.open_review(sr, page_up, term_out)
            }
            crate::tmux_prefix::BindingAction::Execute(command) => {
                match self.classify_tmux_session_change(connection_id, &command) {
                    TmuxSessionChange::Target(session_id) => {
                        if !self.queue_tmux_session_switch(connection_id, session_id, Vec::new()) {
                            sr.speak("tmux session switch is already in progress", true)?;
                        }
                        return Ok(());
                    }
                    TmuxSessionChange::Detach => {
                        return self.begin_graceful_tmux_teardown(connection_id, sr, term_out);
                    }
                    TmuxSessionChange::Unsafe => {
                        return self.show_popup_error(
                            sr,
                            "nested tmux transport",
                            "this command could move the control client without first linking its nested carrier windows; use Lector's session chooser",
                            term_out,
                        );
                    }
                    TmuxSessionChange::NotApplicable => {}
                }
                let scope = self
                    .tmux_connections
                    .iter()
                    .find(|connection| connection.id == connection_id)
                    .map_or(
                        crate::tmux_prefix::SelectWindowScope::NotApplicable,
                        |connection| {
                            crate::tmux_prefix::scope_select_window_command(
                                &connection.topology,
                                &command,
                            )
                        },
                    );
                match scope {
                    crate::tmux_prefix::SelectWindowScope::NotApplicable => {
                        self.queue_tmux_user_command(connection_id, &command)
                    }
                    crate::tmux_prefix::SelectWindowScope::Resolved(command) => {
                        self.queue_tmux_user_command(connection_id, &command)
                    }
                    crate::tmux_prefix::SelectWindowScope::Missing(index) => {
                        sr.speak(&format!("can't find window: {index}"), false)?;
                        Ok(())
                    }
                }
            }
            crate::tmux_prefix::BindingAction::Detach => {
                // A connection may carry nested control clients from any of
                // its tmux sessions. Detach those descendants first, then the
                // invoking connection itself.
                self.begin_graceful_tmux_teardown(connection_id, sr, term_out)
            }
            crate::tmux_prefix::BindingAction::Confirm { command, .. } => {
                self.begin_tmux_confirmation(sr, connection_id, &command, term_out)
            }
            crate::tmux_prefix::BindingAction::SendPrefix => {
                let Some(connection) = self
                    .tmux_connections
                    .iter()
                    .find(|connection| connection.id == connection_id)
                else {
                    return Ok(());
                };
                let prefix = connection.topology.option("prefix").unwrap_or("C-b");
                let pane_id = connection.topology.attached_active_pane();
                let bytes = crate::tmux_prefix::tmux_key_bytes(prefix);
                if let (Some(pane_id), Some(bytes)) = (pane_id, bytes) {
                    self.queue_tmux_input(connection_id, pane_id, &bytes)
                } else {
                    self.queue_tmux_user_command(connection_id, "send-prefix")
                }
            }
            crate::tmux_prefix::BindingAction::ChooseSession => {
                if !self.show_tmux_session_chooser(sr, term_out)? {
                    sr.speak("tmux session chooser is unavailable", false)?;
                }
                Ok(())
            }
            crate::tmux_prefix::BindingAction::ChooseWindow => {
                if !self.show_tmux_window_chooser(sr, term_out)? {
                    sr.speak("tmux window chooser is unavailable", false)?;
                }
                Ok(())
            }
            crate::tmux_prefix::BindingAction::ChoosePane => {
                if !self.show_tmux_pane_chooser(sr, term_out)? {
                    sr.speak("tmux pane chooser is unavailable", false)?;
                }
                Ok(())
            }
            crate::tmux_prefix::BindingAction::CommandPrompt => {
                if !self.show_tmux_command_prompt(sr, term_out)? {
                    sr.speak("tmux command prompt is unavailable", false)?;
                }
                Ok(())
            }
            crate::tmux_prefix::BindingAction::SetKeyTable {
                command,
                table,
                persistent,
            } => {
                self.queue_tmux_user_command(connection_id, &command)?;
                if persistent {
                    if let Some(connection) = self
                        .tmux_connections
                        .iter_mut()
                        .find(|connection| connection.id == connection_id)
                    {
                        connection.key_table_override = Some(table);
                    }
                } else {
                    self.set_tmux_prefix_state(
                        connection_id,
                        TmuxPrefixState {
                            phase: TmuxPrefixPhase::Awaiting { table },
                        },
                    );
                }
                Ok(())
            }
        }
    }

    fn begin_tmux_confirmation(
        &mut self,
        sr: &mut ScreenReader,
        connection_id: u64,
        command: &str,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let Some(topology) = self
            .tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id)
            .map(|connection| &connection.topology)
        else {
            return Ok(());
        };
        let session = topology
            .attached_session()
            .and_then(|session_id| topology.session(session_id));
        let (title, message, target, scoped_command) = match command {
            "kill-pane" => {
                let Some(pane) = topology
                    .attached_active_pane()
                    .and_then(|pane_id| topology.pane(pane_id))
                else {
                    return self.show_popup_error(
                        sr,
                        "tmux pane unavailable",
                        "the active tmux pane no longer exists",
                        term_out,
                    );
                };
                let Some(window) = topology.window(pane.window_id) else {
                    return self.show_popup_error(
                        sr,
                        "tmux pane unavailable",
                        "the active tmux pane's window no longer exists",
                        term_out,
                    );
                };
                let session_context = session.map_or_else(String::new, |session| {
                    format!(" in session ${} {}", session.id.0, session.name)
                });
                (
                    "kill tmux pane",
                    format!(
                        "Kill pane %{} {} in window @{} {}{}?",
                        pane.id.0, pane.title, window.id.0, window.name, session_context
                    ),
                    TmuxConfirmationTarget::Pane(pane.id),
                    format!("kill-pane -t %{}", pane.id.0),
                )
            }
            "kill-window" => {
                let Some(session) = session else {
                    return self.show_popup_error(
                        sr,
                        "tmux window unavailable",
                        "the attached tmux session no longer exists",
                        term_out,
                    );
                };
                let Some(window) = session
                    .active_window
                    .and_then(|window_id| topology.window(window_id))
                else {
                    return self.show_popup_error(
                        sr,
                        "tmux window unavailable",
                        "the active tmux window no longer exists",
                        term_out,
                    );
                };
                (
                    "kill tmux window",
                    format!(
                        "Kill window @{} {} in session ${} {}?",
                        window.id.0, window.name, session.id.0, session.name
                    ),
                    TmuxConfirmationTarget::Window(window.id),
                    format!("kill-window -t @{}", window.id.0),
                )
            }
            _ => {
                return self.show_popup_error(
                    sr,
                    "tmux confirmation unavailable",
                    "this destructive tmux command is not supported",
                    term_out,
                );
            }
        };
        self.pending_tmux_confirmation = Some(PendingTmuxConfirmation {
            connection_id,
            command: scoped_command,
            target,
        });
        self.show_popup_confirmation(sr, title, &message, term_out)
    }

    pub(super) fn tmux_confirmation_target_exists(
        &self,
        confirmation: &PendingTmuxConfirmation,
    ) -> bool {
        let Some(topology) = self
            .tmux_connections
            .iter()
            .find(|connection| connection.id == confirmation.connection_id)
            .map(|connection| &connection.topology)
        else {
            return false;
        };
        match confirmation.target {
            TmuxConfirmationTarget::Pane(pane_id) => topology.pane(pane_id).is_some(),
            TmuxConfirmationTarget::Window(window_id) => topology.window(window_id).is_some(),
        }
    }
}
