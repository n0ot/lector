use super::*;

impl App {
    /// Requests an accessible control for the transport which owns the
    /// currently selected tmux connection. Potentially destructive byte
    /// sequences are staged behind the normal confirmation popup.
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

        if action == crate::tmux_lifecycle::GatewayControlAction::GracefulDetach {
            self.queue_accessible_tmux_detach(connection_id, sr, term_out)?;
            return Ok(true);
        }
        if !action.requires_confirmation() {
            self.queue_gateway_transport_input(connection_id, action)?;
            return Ok(true);
        }

        let (title, message) = match action {
            crate::tmux_lifecycle::GatewayControlAction::ForceClose => (
                "force close tmux gateway",
                format!(
                    "Send Control-backslash to the transport for tmux connection {connection_id}?"
                ),
            ),
            crate::tmux_lifecycle::GatewayControlAction::SshEscapeDisconnect => (
                "disconnect SSH tmux gateway",
                format!(
                    "Send the SSH line-start escape ~. to the transport for tmux connection {connection_id}?"
                ),
            ),
            crate::tmux_lifecycle::GatewayControlAction::SshEscapeHelp => (
                "show SSH gateway escapes",
                format!(
                    "Send the SSH line-start escape ~? to the transport for tmux connection {connection_id}?"
                ),
            ),
            crate::tmux_lifecycle::GatewayControlAction::GracefulDetach
            | crate::tmux_lifecycle::GatewayControlAction::Interrupt => unreachable!(),
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
        self.queue_gateway_transport_input(confirmation.connection_id, confirmation.action)?;
        Ok(true)
    }

    fn queue_accessible_tmux_detach(
        &mut self,
        connection_id: u64,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let client_name = self
            .tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id)
            .and_then(|connection| connection.topology.client_info("client_name"))
            .unwrap_or_default();
        match crate::tmux_lifecycle::ConnectionHierarchy::detach_command(
            self.tmux_connections.len(),
            client_name,
        ) {
            Ok(command) => self.queue_tmux_user_command(connection_id, &command),
            Err(error) => self.show_popup_error(
                sr,
                "tmux detach unavailable",
                &format!("cannot identify the active tmux client: {error}"),
                term_out,
            ),
        }
    }

    fn queue_gateway_transport_input(
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
            .filter(|view| view.is_ready() && !view.is_showing_portal())
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
        let (rows, cols) = self.view_stack.root_mut().model().size();
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
        let (rows, cols) = self.view_stack.root_mut().model().size();
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
        let (rows, cols) = self.view_stack.root_mut().model().size();
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
        let (rows, cols) = self.view_stack.root_mut().model().size();
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
