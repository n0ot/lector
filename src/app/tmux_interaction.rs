use super::*;

#[derive(Clone, Copy)]
struct TmuxGatewayHop {
    parent_connection_id: u64,
    session_id: crate::tmux_model::SessionId,
    window_id: crate::tmux_model::WindowId,
    pane_id: crate::tmux_model::PaneId,
}

impl App {
    /// Requests lifecycle control for the currently selected tmux connection.
    /// Graceful teardown is ordered by the connection hierarchy. Abandoning a
    /// transport is staged behind the normal confirmation popup.
    pub fn request_tmux_gateway_action(
        &mut self,
        sr: &mut ScreenReader,
        action: crate::tmux_lifecycle::GatewayControlAction,
        term_out: &mut dyn Write,
    ) -> Result<bool> {
        let Some(connection_id) = self.active_tmux_connection.filter(|connection_id| {
            self.tmux_connections
                .iter()
                .any(|connection| connection.id == *connection_id)
        }) else {
            return Ok(false);
        };

        self.request_tmux_gateway_action_for(connection_id, sr, action, term_out)
    }

    pub(super) fn request_tmux_gateway_action_for(
        &mut self,
        connection_id: u64,
        sr: &mut ScreenReader,
        action: crate::tmux_lifecycle::GatewayControlAction,
        term_out: &mut dyn Write,
    ) -> Result<bool> {
        if !self
            .tmux_connections
            .iter()
            .any(|connection| connection.id == connection_id)
        {
            return Ok(false);
        }

        if action == crate::tmux_lifecycle::GatewayControlAction::GracefulDetach {
            self.begin_graceful_tmux_teardown(connection_id, sr, term_out)?;
            return Ok(true);
        }

        // If a deepest-first cascade is waiting on a descendant, uppercase D
        // must address the connection which is actually stuck, not merely the
        // ancestor row from which the cascade was started.
        let connection_id = self
            .pending_graceful_teardown
            .as_ref()
            .and_then(|pending| pending.awaiting)
            .filter(|awaiting| {
                self.tmux_hierarchy
                    .teardown_order(connection_id)
                    .contains(awaiting)
            })
            .unwrap_or(connection_id);

        let (title, message) = match action {
            crate::tmux_lifecycle::GatewayControlAction::ForceAbandon => (
                "expose raw tmux transport",
                format!(
                    "Send Control-backslash to the transport for tmux connection {connection_id}? If it does not exit within {TMUX_FORCE_ABANDON_GRACE_MS} milliseconds, Lector will stop interpreting that transport as tmux control and expose its raw channel."
                ),
            ),
            crate::tmux_lifecycle::GatewayControlAction::GracefulDetach => unreachable!(),
        };
        self.pending_tmux_confirmation = None;
        self.pending_gateway_confirmation = Some(PendingGatewayConfirmation {
            connection_id,
            action,
        });
        self.show_popup_confirmation(sr, title, &message, term_out)?;
        Ok(true)
    }

    pub(super) fn accept_gateway_confirmation(
        &mut self,
        confirmation: PendingGatewayConfirmation,
    ) -> Result<bool> {
        if !self.tmux_hierarchy.contains(confirmation.connection_id) {
            return Ok(false);
        }
        self.pending_graceful_teardown = None;
        self.queue_gateway_transport_input(confirmation.connection_id, confirmation.action)?;
        self.pending_force_abandon = Some(PendingForceAbandon {
            connection_id: confirmation.connection_id,
            deadline_ms: self
                .clock
                .now_ms()
                .saturating_add(TMUX_FORCE_ABANDON_GRACE_MS),
        });
        self.log_event(&format!(
            "armed tmux raw-transport fallback for connection {}",
            confirmation.connection_id
        ));
        Ok(true)
    }

    fn begin_graceful_tmux_teardown(
        &mut self,
        connection_id: u64,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let remaining = self.tmux_hierarchy.teardown_order(connection_id).into();
        self.pending_force_abandon = None;
        self.pending_graceful_teardown = Some(PendingGracefulTeardown {
            remaining,
            awaiting: None,
        });
        self.advance_graceful_tmux_teardown(sr, term_out)
    }

    pub(super) fn advance_graceful_tmux_teardown(
        &mut self,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        if self
            .pending_graceful_teardown
            .as_ref()
            .and_then(|pending| pending.awaiting)
            .is_some_and(|connection_id| self.tmux_hierarchy.contains(connection_id))
        {
            return Ok(());
        }
        if let Some(pending) = self.pending_graceful_teardown.as_mut() {
            pending.awaiting = None;
        }

        let next = loop {
            let Some(pending) = self.pending_graceful_teardown.as_mut() else {
                return Ok(());
            };
            let Some(connection_id) = pending.remaining.pop_front() else {
                self.pending_graceful_teardown = None;
                self.log_event("completed graceful tmux connection teardown");
                return Ok(());
            };
            if self.tmux_hierarchy.contains(connection_id) {
                break connection_id;
            }
        };

        if self.queue_accessible_tmux_detach(next, sr, term_out)? {
            if let Some(pending) = self.pending_graceful_teardown.as_mut() {
                pending.awaiting = Some(next);
            }
            self.log_event(&format!(
                "requested graceful tmux detach for connection {next}"
            ));
        } else {
            self.pending_graceful_teardown = None;
        }
        Ok(())
    }

    fn queue_accessible_tmux_detach(
        &mut self,
        connection_id: u64,
        _sr: &mut ScreenReader,
        _term_out: &mut dyn Write,
    ) -> Result<bool> {
        // A nested control stream is carried as pane output by every ancestor.
        // The connection chooser can cover those panes long enough for tmux's
        // pause-after flow control to stop delivery. Resume the route from the
        // outside in so the child's `%exit` reaches Lector and the graceful
        // cascade can advance to its parent.
        if !self.queue_tmux_gateway_path_resumes(connection_id) {
            return Ok(false);
        }

        // The selected Lector connection already identifies one control-mode
        // client. An unqualified command on that channel detaches that client;
        // retargeting by a client name from another server is both redundant
        // and can make tmux report that the client does not exist.
        self.queue_tmux_user_command(connection_id, "detach-client")?;
        Ok(true)
    }

    fn tmux_gateway_path(&self, connection_id: u64) -> Option<Vec<TmuxGatewayHop>> {
        let mut gateway_path = Vec::new();
        let mut routed_connection_id = connection_id;
        for _ in 0..=64 {
            match self.tmux_hierarchy.origin(routed_connection_id) {
                Some(GatewayOrigin::Direct) => break,
                Some(GatewayOrigin::Pane {
                    parent_connection_id,
                    session_id,
                    window_id,
                    pane_id,
                }) => {
                    gateway_path.push(TmuxGatewayHop {
                        parent_connection_id,
                        session_id: crate::tmux_model::SessionId(session_id),
                        window_id: crate::tmux_model::WindowId(window_id),
                        pane_id: crate::tmux_model::PaneId(pane_id),
                    });
                    routed_connection_id = parent_connection_id;
                }
                None => return None,
            }
        }
        gateway_path.reverse();
        Some(gateway_path)
    }

    pub(super) fn active_tmux_gateway_path(&self) -> Vec<(u64, crate::tmux_model::PaneId)> {
        self.active_tmux_connection
            .and_then(|connection_id| self.tmux_gateway_path(connection_id))
            .map(|path| {
                path.into_iter()
                    .map(|hop| (hop.parent_connection_id, hop.pane_id))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn queue_tmux_gateway_path_resumes(&mut self, connection_id: u64) -> bool {
        let Some(gateway_path) = self.tmux_gateway_path(connection_id) else {
            return false;
        };
        let mut restore_commands = Vec::new();
        for hop in &gateway_path {
            let Some(connection) = self
                .tmux_connections
                .iter()
                .find(|connection| connection.id == hop.parent_connection_id)
            else {
                return false;
            };
            let topology = &connection.topology;
            let Some(session) = topology.session(hop.session_id) else {
                return false;
            };
            let Some(window) = topology.window(hop.window_id) else {
                return false;
            };
            let Some(pane) = topology.pane(hop.pane_id) else {
                return false;
            };
            if pane.window_id != hop.window_id
                || !session
                    .windows
                    .values()
                    .any(|window_id| *window_id == hop.window_id)
            {
                return false;
            }
            if topology.attached_session() != Some(hop.session_id) {
                restore_commands.push((
                    hop.parent_connection_id,
                    format!("switch-client -t ${}", hop.session_id.0),
                ));
            }
            if session.active_window != Some(hop.window_id) {
                restore_commands.push((
                    hop.parent_connection_id,
                    format!("select-window -t @{}", hop.window_id.0),
                ));
            }
            if window.active_pane != Some(hop.pane_id) {
                restore_commands.push((
                    hop.parent_connection_id,
                    format!("select-pane -t %{}", hop.pane_id.0),
                ));
            }
        }
        for (parent_connection_id, command) in restore_commands {
            let mut bytes = command.into_bytes();
            bytes.push(b'\n');
            self.pending_tmux_commands.push_back(PendingTmuxCommand {
                connection_id: parent_connection_id,
                bytes,
                expected_replies: vec![ExpectedTmuxReply::Ignored],
                kind: PendingTmuxCommandKind::Ordinary,
            });
        }
        for hop in gateway_path {
            let parent_connection_id = hop.parent_connection_id;
            let pane_id = hop.pane_id;
            if let Some(flow) = self
                .tmux_connections
                .iter_mut()
                .find(|connection| connection.id == parent_connection_id)
                .and_then(|connection| connection.pane_flow.get_mut(&pane_id))
            {
                flow.resume_requested = true;
            }
            self.pending_tmux_commands.push_back(PendingTmuxCommand {
                connection_id: parent_connection_id,
                bytes: crate::tmux_input::continue_pane_command(pane_id),
                expected_replies: vec![ExpectedTmuxReply::PaneContinue(pane_id)],
                kind: PendingTmuxCommandKind::Ordinary,
            });
        }
        true
    }

    pub(super) fn queue_gateway_transport_input(
        &mut self,
        connection_id: u64,
        action: crate::tmux_lifecycle::GatewayControlAction,
    ) -> Result<()> {
        let bytes = action
            .transport_bytes()
            .context("graceful detach is a tmux command, not transport input")?;
        match self.tmux_hierarchy.origin(connection_id) {
            Some(GatewayOrigin::Direct) => {
                self.pending_direct_gateway_input
                    .push_back(PendingDirectGatewayInput {
                        connection_id,
                        bytes: bytes.to_vec(),
                    });
                Ok(())
            }
            Some(GatewayOrigin::Pane {
                parent_connection_id,
                pane_id,
                ..
            }) => self.queue_tmux_input(
                parent_connection_id,
                crate::tmux_model::PaneId(pane_id),
                bytes,
            ),
            None => Ok(()),
        }
    }

    fn tmux_connection_items(&self) -> Vec<views::TmuxConnectionItem> {
        self.tmux_connections
            .iter()
            .map(|connection| views::TmuxConnectionItem {
                connection_id: connection.id,
                label: connection.topology.label().to_owned(),
                host: connection.topology.host().map(str::to_owned),
            })
            .collect()
    }

    pub(super) fn sync_tmux_connection_chooser(&mut self) -> bool {
        let items = self.tmux_connection_items();
        let active_connection = self.active_tmux_connection;
        let Some(chooser) = self.view_stack.active_tmux_connection_chooser_mut() else {
            return false;
        };
        chooser.sync(items, active_connection);
        true
    }

    fn active_visible_tmux_snapshot(&mut self) -> Option<(u64, crate::tmux_model::TmuxTopology)> {
        let connection_id = self
            .view_stack
            .active_tmux_connection_mut()
            .filter(|view| view.is_ready() && !view.is_showing_connection_portal())
            .map(|view| view.connection_id())?;
        let topology = self
            .tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id && connection.has_inventory)?
            .topology
            .clone();
        Some((connection_id, topology))
    }

    pub fn show_tmux_connection_chooser(
        &mut self,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<bool> {
        if self.tmux_connections.is_empty() {
            return Ok(false);
        }
        if let Some(connection_id) = self.active_tmux_connection
            && let Some(connection) = self
                .tmux_connections
                .iter_mut()
                .find(|connection| connection.id == connection_id)
        {
            connection.prefix_state = None;
        }
        let (rows, cols) = self.view_stack.root_mut().model().live_size();
        let chooser = views::TmuxConnectionChooserView::new(
            rows,
            cols,
            self.tmux_connection_items(),
            self.active_tmux_connection,
        );
        self.handle_view_action(sr, views::ViewAction::Push(Box::new(chooser)), term_out)?;
        Ok(true)
    }

    pub fn show_tmux_connection_rename(
        &mut self,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<bool> {
        let Some(connection_id) = self
            .active_visible_tmux_snapshot()
            .map(|(connection_id, _)| connection_id)
        else {
            return Ok(false);
        };
        let (rows, cols) = self.view_stack.root_mut().model().live_size();
        self.handle_view_action(
            sr,
            views::ViewAction::Push(Box::new(views::TmuxConnectionRenameView::new(
                rows,
                cols,
                connection_id,
            ))),
            term_out,
        )?;
        Ok(true)
    }

    pub(super) fn handle_tmux_connection_rename(
        &mut self,
        sr: &mut ScreenReader,
        connection_id: u64,
        label: String,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let label = label.trim();
        let Some(connection) = self
            .tmux_connections
            .iter_mut()
            .find(|connection| connection.id == connection_id)
        else {
            self.handle_view_action(sr, views::ViewAction::Pop, term_out)?;
            self.emit_physical_bells(term_out, 1)?;
            return Ok(());
        };
        if let Err(error) = connection.topology.set_label(label) {
            self.handle_view_action(sr, views::ViewAction::Pop, term_out)?;
            self.show_popup_error(
                sr,
                "invalid tmux connection label",
                &error.to_string(),
                term_out,
            )?;
            return Ok(());
        }
        self.sync_tmux_panes(connection_id)?;
        self.handle_view_action(sr, views::ViewAction::Pop, term_out)
    }

    pub fn show_tmux_session_chooser(
        &mut self,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<bool> {
        self.show_tmux_chooser(sr, term_out, |rows, cols, connection_id, topology| {
            views::TmuxChooserView::sessions(rows, cols, connection_id, topology)
        })
    }

    pub fn show_tmux_window_chooser(
        &mut self,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<bool> {
        self.show_tmux_chooser(sr, term_out, |rows, cols, connection_id, topology| {
            views::TmuxChooserView::windows(rows, cols, connection_id, topology)
        })
    }

    pub fn show_tmux_pane_chooser(
        &mut self,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<bool> {
        self.show_tmux_chooser(sr, term_out, |rows, cols, connection_id, topology| {
            views::TmuxChooserView::panes(rows, cols, connection_id, topology)
        })
    }

    fn show_tmux_chooser(
        &mut self,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
        create: impl FnOnce(u16, u16, u64, &crate::tmux_model::TmuxTopology) -> views::TmuxChooserView,
    ) -> Result<bool> {
        let Some((connection_id, topology)) = self.active_visible_tmux_snapshot() else {
            return Ok(false);
        };
        let (rows, cols) = self.view_stack.root_mut().model().live_size();
        let chooser = create(rows, cols, connection_id, &topology);
        self.handle_view_action(sr, views::ViewAction::Push(Box::new(chooser)), term_out)?;
        Ok(true)
    }

    pub fn show_tmux_command_prompt(
        &mut self,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<bool> {
        let Some((connection_id, _)) = self.active_visible_tmux_snapshot() else {
            return Ok(false);
        };
        let history = self
            .tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id)
            .map(|connection| connection.command_history.clone())
            .unwrap_or_default();
        let (rows, cols) = self.view_stack.root_mut().model().live_size();
        self.handle_view_action(
            sr,
            views::ViewAction::Push(Box::new(views::TmuxCommandView::new(
                rows,
                cols,
                connection_id,
                history,
            ))),
            term_out,
        )?;
        Ok(true)
    }

    pub(super) fn handle_tmux_chooser_selection(
        &mut self,
        sr: &mut ScreenReader,
        connection_id: u64,
        target: views::TmuxChooserTarget,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let command = self
            .tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id)
            .and_then(|connection| match target {
                views::TmuxChooserTarget::Session(session_id) => connection
                    .topology
                    .session(session_id)
                    .map(|_| format!("switch-client -t {}{}", '$', session_id.0)),
                views::TmuxChooserTarget::Window(window_id) => {
                    let session = connection
                        .topology
                        .attached_session()
                        .and_then(|session_id| connection.topology.session(session_id))?;
                    session
                        .windows
                        .values()
                        .any(|candidate| *candidate == window_id)
                        .then(|| format!("select-window -t @{}", window_id.0))
                }
                views::TmuxChooserTarget::Pane(pane_id) => {
                    let window_id = connection
                        .topology
                        .attached_session()
                        .and_then(|session_id| connection.topology.session(session_id))
                        .and_then(|session| session.active_window)?;
                    connection
                        .topology
                        .pane(pane_id)
                        .is_some_and(|pane| pane.window_id == window_id)
                        .then(|| format!("select-pane -t %{}", pane_id.0))
                }
            });
        let Some(command) = command else {
            self.emit_physical_bells(term_out, 1)?;
            return Ok(());
        };
        self.handle_view_action(sr, views::ViewAction::Pop, term_out)?;
        self.queue_tmux_user_command(connection_id, &command)
    }

    pub(super) fn handle_tmux_command_submit(
        &mut self,
        sr: &mut ScreenReader,
        connection_id: u64,
        command: String,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let Some(connection) = self
            .tmux_connections
            .iter_mut()
            .find(|connection| connection.id == connection_id)
        else {
            self.emit_physical_bells(term_out, 1)?;
            return Ok(());
        };
        if crate::tmux_prefix::classify_binding(&command).is_err() {
            self.handle_view_action(sr, views::ViewAction::Pop, term_out)?;
            self.show_popup_error(
                sr,
                "tmux command rejected",
                "commands cannot contain NUL, carriage return, or newline",
                term_out,
            )?;
            return Ok(());
        }
        if connection.command_history.last() != Some(&command) {
            connection.command_history.push(command.clone());
            const MAX_HISTORY: usize = 100;
            if connection.command_history.len() > MAX_HISTORY {
                let excess = connection.command_history.len() - MAX_HISTORY;
                connection.command_history.drain(..excess);
            }
        }
        self.handle_view_action(sr, views::ViewAction::Pop, term_out)?;
        self.queue_tmux_prompt_command(connection_id, &command)
    }
}
