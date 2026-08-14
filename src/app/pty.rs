use super::*;

impl App {
    pub fn handle_pty(
        &mut self,
        sr: &mut ScreenReader,
        buf: &[u8],
        term_out: &mut dyn Write,
    ) -> Result<()> {
        self.log_bytes("pty output from source", buf);
        self.tmux_gateway
            .ensure_next_connection_id_at_least(self.next_tmux_connection_id);
        let events = self.tmux_gateway.push(buf)?;
        self.sync_root_tmux_termination_deadline();
        for event in events {
            self.handle_tmux_gateway_event(sr, event, term_out)?;
        }
        Ok(())
    }

    /// Resolves any active control connection when the root PTY transport
    /// reaches EOF. Calling this repeatedly is deliberately harmless.
    pub fn handle_pty_eof(
        &mut self,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let events = self.tmux_gateway.finish_transport();
        self.tmux_termination_deadline_ms = None;
        for event in events {
            self.handle_tmux_gateway_event(sr, event, term_out)?;
        }
        Ok(())
    }

    /// Applies one already-routed control event from any independent transport.
    pub fn handle_tmux_gateway_event(
        &mut self,
        sr: &mut ScreenReader,
        event: crate::tmux_gateway::GatewayEvent,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        match event {
            crate::tmux_gateway::GatewayEvent::DirectOutput(bytes) => {
                self.process_direct_pty_output(sr, &bytes, term_out)
            }
            crate::tmux_gateway::GatewayEvent::ConnectionStarted { connection_id } => {
                self.start_tmux_connection(sr, connection_id, GatewayOrigin::Direct, term_out)
            }
            crate::tmux_gateway::GatewayEvent::Control {
                connection_id,
                event,
            } => self.process_tmux_control(sr, connection_id, event, term_out),
            crate::tmux_gateway::GatewayEvent::ConnectionEnded { connection_id } => {
                self.end_tmux_connection(sr, connection_id, term_out)
            }
            crate::tmux_gateway::GatewayEvent::ConnectionFailed {
                connection_id,
                reason,
            } => self.fail_tmux_connection(sr, connection_id, &reason, term_out),
        }
    }

    fn fail_tmux_connection(
        &mut self,
        sr: &mut ScreenReader,
        connection_id: u64,
        reason: &crate::tmux_gateway::GatewayFailure,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let Some(origin) = self.tmux_hierarchy.origin(connection_id) else {
            return Ok(());
        };
        let location = match origin {
            GatewayOrigin::Direct => "terminal".to_owned(),
            GatewayOrigin::Pane {
                parent_connection_id,
                pane_id,
                ..
            } => format!("parent tmux connection {parent_connection_id}, pane %{pane_id}"),
        };
        self.end_tmux_connection(sr, connection_id, term_out)?;
        sr.speak(
            &format!("tmux connection {connection_id} {reason}; returned to {location}"),
            true,
        )?;
        Ok(())
    }

    fn sync_root_tmux_termination_deadline(&mut self) {
        if self.tmux_gateway.lifecycle_state()
            == crate::tmux_gateway::GatewayLifecycleState::AwaitingTerminator
        {
            self.tmux_termination_deadline_ms.get_or_insert_with(|| {
                self.clock
                    .now_ms()
                    .saturating_add(TMUX_TERMINATOR_TIMEOUT_MS)
            });
        } else {
            self.tmux_termination_deadline_ms = None;
        }
    }

    fn process_direct_pty_output(
        &mut self,
        sr: &mut ScreenReader,
        buf: &[u8],
        term_out: &mut dyn Write,
    ) -> Result<()> {
        self.log_bytes("direct terminal output", buf);
        let bells_before = self
            .view_stack
            .root_mut()
            .model()
            .update_summary()
            .effects
            .bells;
        let replies_before = self
            .view_stack
            .root_mut()
            .model()
            .update_summary()
            .pty_replies
            .len();
        let events_before = self
            .view_stack
            .root_mut()
            .model()
            .update_summary()
            .effects
            .events
            .len();
        self.view_stack.root_mut().handle_pty_output(buf)?;
        let terminal_update = self.view_stack.root_mut().model().update_summary().clone();
        let new_replies = self
            .view_stack
            .root_mut()
            .model()
            .update_summary()
            .pty_replies
            .get(replies_before..)
            .unwrap_or_default()
            .to_vec();
        if !new_replies.is_empty() {
            self.application_replies.queue(ROOT_SOURCE, &new_replies);
        }
        let new_events = self
            .view_stack
            .root_mut()
            .model()
            .update_summary()
            .effects
            .events
            .get(events_before..)
            .unwrap_or_default()
            .to_vec();
        let effect_time = self.clock.now_ms();
        for event in &new_events {
            match self.terminal_effect_policy.disposition(event) {
                crate::terminal_protocol::EffectDisposition::LocalClipboard => {
                    let crate::terminal::TerminalEvent::ClipboardWrite { contents, .. } = event
                    else {
                        continue;
                    };
                    if let Some(text) = contents
                        .iter()
                        .find(|content| content.mime == "text/plain")
                        .and_then(|content| String::from_utf8(content.data.clone()).ok())
                    {
                        sr.push_clipboard(text)?;
                    }
                    if let Some(scheduler) = &mut self.output_scheduler {
                        scheduler.enqueue_terminal_effect(ROOT_SOURCE, event.clone(), effect_time);
                    }
                }
                crate::terminal_protocol::EffectDisposition::Model => {
                    if matches!(event, crate::terminal::TerminalEvent::ProgressReport { .. })
                        && let Some(scheduler) = &mut self.output_scheduler
                    {
                        scheduler.enqueue_terminal_effect(ROOT_SOURCE, event.clone(), effect_time);
                    }
                }
                crate::terminal_protocol::EffectDisposition::Internal
                | crate::terminal_protocol::EffectDisposition::Drop => {}
            }
        }
        let bells_after = self
            .view_stack
            .root_mut()
            .model()
            .update_summary()
            .effects
            .bells;
        let new_bells = bells_after.saturating_sub(bells_before);
        self.render_terminal_update(term_out, new_bells, &terminal_update)?;
        let now_ms = self.clock.now_ms();
        if self.first_pty_update.is_none() {
            self.first_pty_update = Some(now_ms);
        }
        self.last_pty_update = Some(now_ms);
        Ok(())
    }

    fn start_tmux_connection(
        &mut self,
        sr: &mut ScreenReader,
        connection_id: u64,
        origin: GatewayOrigin,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        self.pending_tmux_confirmation = None;
        self.pending_gateway_confirmation = None;
        if origin == GatewayOrigin::Direct {
            self.application_replies.take(ROOT_SOURCE);
        }
        self.tmux_hierarchy
            .insert(connection_id, origin)
            .context("register tmux connection")?;
        if let GatewayOrigin::Pane {
            parent_connection_id,
            pane_id,
            ..
        } = origin
        {
            let portal = self
                .view_stack
                .tmux_connection_mut(parent_connection_id)
                .context("nested tmux parent view is unavailable")?
                .set_pane_portal(crate::tmux_model::PaneId(pane_id), connection_id);
            if let Err(error) = portal {
                self.tmux_hierarchy.remove_connection(connection_id);
                return Err(error).context("create nested tmux pane portal");
            }
        }
        self.next_tmux_connection_id = self
            .next_tmux_connection_id
            .max(connection_id.saturating_add(1));
        self.tmux_connections.push(TmuxConnectionState {
            id: connection_id,
            topology: crate::tmux_model::TmuxTopology::new(connection_id),
            initial_command_seen: false,
            inventory_replies_remaining: crate::tmux_model::INVENTORY_REPLY_COUNT,
            pending_inventory: Vec::new(),
            inventory_failed: false,
            expected_replies: VecDeque::new(),
            has_inventory: false,
            inventory_retry_count: 0,
            command_history: Vec::new(),
            prefix_state: None,
            pane_flow: BTreeMap::new(),
        });
        self.pending_tmux_commands.push_back(PendingTmuxCommand {
            connection_id,
            bytes: TMUX_FLOW_CONTROL_COMMAND.to_vec(),
            expected_replies: vec![ExpectedTmuxReply::Ignored],
            kind: PendingTmuxCommandKind::Ordinary,
        });
        self.queue_tmux_inventory(connection_id);
        self.active_tmux_connection = Some(connection_id);
        self.first_pty_update = None;
        self.last_pty_update = None;
        self.capture_lua_repl_history();
        self.view_stack.clear_overlays();
        let (rows, cols) = self.view_stack.root_mut().model().size();
        self.view_stack
            .push(Box::new(views::TmuxConnectionView::new(
                rows,
                cols,
                connection_id,
            )));
        self.render_active_view(term_out)?;
        self.announce_view_change(sr)
    }

    fn end_tmux_connection(
        &mut self,
        sr: &mut ScreenReader,
        connection_id: u64,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let preserve_connection_chooser = self
            .view_stack
            .active_tmux_connection_chooser_mut()
            .is_some();
        let previously_active = self.active_tmux_connection;
        let ending_origin = self.tmux_hierarchy.origin(connection_id);
        if let Some(GatewayOrigin::Pane {
            parent_connection_id,
            pane_id,
            ..
        }) = ending_origin
            && let Some(parent) = self.view_stack.tmux_connection_mut(parent_connection_id)
        {
            parent.clear_pane_portal(crate::tmux_model::PaneId(pane_id), connection_id);
        }
        let mut removed_connections = self.tmux_hierarchy.remove_connection(connection_id);
        if removed_connections.is_empty() {
            removed_connections.push(connection_id);
        }
        if self
            .pending_tmux_confirmation
            .as_ref()
            .is_some_and(|confirmation| removed_connections.contains(&confirmation.connection_id))
        {
            self.pending_tmux_confirmation = None;
        }
        if self
            .pending_gateway_confirmation
            .as_ref()
            .is_some_and(|confirmation| removed_connections.contains(&confirmation.connection_id))
        {
            self.pending_gateway_confirmation = None;
        }
        self.tmux_connections
            .retain(|connection| !removed_connections.contains(&connection.id));
        self.recent_tmux_bells
            .retain(|(connection_id, _), _| !removed_connections.contains(connection_id));
        if self
            .last_tmux_bell_source
            .as_ref()
            .is_some_and(|source| removed_connections.contains(&source.connection_id))
        {
            self.last_tmux_bell_source = None;
        }
        self.pending_tmux_commands
            .retain(|command| !removed_connections.contains(&command.connection_id));
        self.pending_direct_gateway_input
            .retain(|input| !removed_connections.contains(&input.connection_id));
        self.cleanup_nested_gateway_state(&removed_connections);
        self.active_tmux_connection = previously_active
            .filter(|active| {
                self.tmux_connections
                    .iter()
                    .any(|connection| connection.id == *active)
            })
            .or_else(|| {
                previously_active
                    .is_some()
                    .then(|| self.tmux_connections.last().map(|connection| connection.id))
                    .flatten()
            });
        self.first_pty_update = None;
        self.last_pty_update = None;
        self.capture_lua_repl_history();
        if !preserve_connection_chooser {
            self.view_stack.clear_overlays();
        }
        self.view_stack
            .remove_tmux_connections(&removed_connections);
        if preserve_connection_chooser {
            self.sync_tmux_connection_chooser();
        } else if let Some(connection_id) = self.active_tmux_connection {
            self.view_stack.activate_tmux_connection(connection_id);
        } else {
            self.view_stack.activate_terminal();
        }
        self.render_active_view(term_out)?;
        self.announce_view_change(sr)
    }

    fn process_tmux_control(
        &mut self,
        sr: &mut ScreenReader,
        connection_id: u64,
        event: crate::tmux_control::ControlEvent,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let mut request_resync = false;
        let mut sync_topology = false;
        let mut render_topology = false;
        let mut bootstrap_reply = None;
        let mut pane_resync_reply = None;
        let mut pane_resync_request = None;
        let mut pane_resume_request = None;
        let mut pane_output = None;
        let mut user_command_result = None;
        let mut notification_popup = None;
        let mut destroyed_gateway_panes = Vec::new();
        let mut destroyed_gateway_windows = Vec::new();
        {
            let Some(connection) = self
                .tmux_connections
                .iter_mut()
                .find(|connection| connection.id == connection_id)
            else {
                return Ok(());
            };
            match event {
                crate::tmux_control::ControlEvent::Command { status, output, .. } => {
                    if !connection.initial_command_seen {
                        connection.initial_command_seen = true;
                        return Ok(());
                    }
                    match connection.expected_replies.pop_front() {
                        Some(ExpectedTmuxReply::Inventory) => {
                            connection.inventory_replies_remaining =
                                connection.inventory_replies_remaining.saturating_sub(1);
                            if status == crate::tmux_control::CommandStatus::Success {
                                connection.pending_inventory.extend(output);
                            } else {
                                connection.inventory_failed = true;
                            }
                            if connection.inventory_replies_remaining == 0 {
                                let inventory = std::mem::take(&mut connection.pending_inventory);
                                let previous_panes = connection
                                    .topology
                                    .panes()
                                    .keys()
                                    .copied()
                                    .collect::<Vec<_>>();
                                let previous_windows = connection
                                    .topology
                                    .windows()
                                    .keys()
                                    .copied()
                                    .collect::<Vec<_>>();
                                if connection.inventory_failed
                                    || connection.topology.replace_inventory(&inventory).is_err()
                                {
                                    connection.topology.mark_resync_required();
                                    if connection.inventory_retry_count == 0 {
                                        connection.inventory_retry_count = 1;
                                        connection.inventory_replies_remaining =
                                            crate::tmux_model::INVENTORY_REPLY_COUNT;
                                        request_resync = true;
                                    }
                                } else {
                                    destroyed_gateway_panes.extend(
                                        previous_panes.into_iter().filter(|pane_id| {
                                            connection.topology.pane(*pane_id).is_none()
                                        }),
                                    );
                                    destroyed_gateway_windows.extend(
                                        previous_windows.into_iter().filter(|window_id| {
                                            connection.topology.window(*window_id).is_none()
                                        }),
                                    );
                                    connection.has_inventory = true;
                                    connection.inventory_retry_count = 0;
                                    sync_topology = true;
                                    render_topology = true;
                                }
                                connection.inventory_failed = false;
                            }
                        }
                        Some(ExpectedTmuxReply::Bootstrap(pane_id)) => {
                            bootstrap_reply = Some((pane_id, status, output));
                        }
                        Some(ExpectedTmuxReply::PaneResync(pane_id)) => {
                            pane_resync_reply = Some((pane_id, status, output));
                        }
                        Some(ExpectedTmuxReply::Ignored) => {}
                        Some(ExpectedTmuxReply::UserCommand {
                            description,
                            show_success,
                        }) => {
                            if crate::tmux_prefix::command_may_change_key_configuration(
                                &description,
                            ) && connection.inventory_replies_remaining == 0
                            {
                                connection.topology.mark_resync_required();
                                connection.inventory_replies_remaining =
                                    crate::tmux_model::INVENTORY_REPLY_COUNT;
                                connection.pending_inventory.clear();
                                connection.inventory_failed = false;
                                connection.inventory_retry_count = 0;
                                request_resync = true;
                            }
                            if status == crate::tmux_control::CommandStatus::Error
                                || show_success
                                || !output.is_empty()
                            {
                                user_command_result =
                                    Some((status, description, output, show_success));
                            }
                        }
                        None => {}
                    }
                }
                crate::tmux_control::ControlEvent::Output { pane_id, bytes } => {
                    let pane_id = crate::tmux_model::PaneId(pane_id);
                    let flow = connection.pane_flow.entry(pane_id).or_default();
                    if flow.status == TmuxFlowStatus::Resynchronizing {
                        flow.skipped_incremental_bytes =
                            flow.skipped_incremental_bytes.saturating_add(bytes.len());
                    } else {
                        pane_output = Some((pane_id, bytes));
                    }
                }
                crate::tmux_control::ControlEvent::ExtendedOutput {
                    pane_id,
                    age_ms,
                    bytes,
                    ..
                } => {
                    let pane_id = crate::tmux_model::PaneId(pane_id);
                    let flow = connection.pane_flow.entry(pane_id).or_default();
                    flow.last_extended_output_age_ms = Some(age_ms);
                    if flow.status == TmuxFlowStatus::Resynchronizing {
                        flow.skipped_incremental_bytes =
                            flow.skipped_incremental_bytes.saturating_add(bytes.len());
                    } else if age_ms > TMUX_MAX_EXTENDED_OUTPUT_AGE_MS
                        && connection.topology.pane(pane_id).is_some()
                    {
                        flow.status = TmuxFlowStatus::Resynchronizing;
                        flow.skipped_incremental_bytes =
                            flow.skipped_incremental_bytes.saturating_add(bytes.len());
                        flow.limitations.extend([
                            TmuxResyncLimitation::KittyImages,
                            TmuxResyncLimitation::ParserContinuation,
                            TmuxResyncLimitation::SemanticMetadata,
                        ]);
                        pane_resync_request = Some(pane_id);
                    } else {
                        pane_output = Some((pane_id, bytes));
                    }
                }
                crate::tmux_control::ControlEvent::Pause { pane_id } => {
                    let pane_id = crate::tmux_model::PaneId(pane_id);
                    let flow = connection.pane_flow.entry(pane_id).or_default();
                    if flow.status == TmuxFlowStatus::Running {
                        flow.status = TmuxFlowStatus::Paused;
                    }
                    if !flow.resume_requested {
                        flow.resume_requested = true;
                        pane_resume_request = Some(pane_id);
                    }
                }
                crate::tmux_control::ControlEvent::Continue { pane_id } => {
                    let pane_id = crate::tmux_model::PaneId(pane_id);
                    let flow = connection.pane_flow.entry(pane_id).or_default();
                    flow.resume_requested = false;
                    if flow.status == TmuxFlowStatus::Paused {
                        flow.status = TmuxFlowStatus::Running;
                    }
                }
                crate::tmux_control::ControlEvent::Notification { name, arguments } => {
                    if matches!(name.as_slice(), b"message" | b"config-error") {
                        notification_popup = Some((
                            name == b"config-error",
                            String::from_utf8_lossy(&arguments).into_owned(),
                        ));
                    } else {
                        let previous_panes = connection
                            .topology
                            .panes()
                            .keys()
                            .copied()
                            .collect::<Vec<_>>();
                        let previous_windows = connection
                            .topology
                            .windows()
                            .keys()
                            .copied()
                            .collect::<Vec<_>>();
                        let outcome = connection.topology.apply_notification(&name, &arguments)?;
                        if outcome == crate::tmux_model::ReconcileOutcome::Applied {
                            destroyed_gateway_panes.extend(
                                previous_panes
                                    .into_iter()
                                    .filter(|pane_id| connection.topology.pane(*pane_id).is_none()),
                            );
                            destroyed_gateway_windows.extend(previous_windows.into_iter().filter(
                                |window_id| connection.topology.window(*window_id).is_none(),
                            ));
                        }
                        request_resync = outcome
                            == crate::tmux_model::ReconcileOutcome::ResyncRequired
                            && connection.inventory_replies_remaining == 0;
                        if request_resync {
                            connection.inventory_replies_remaining =
                                crate::tmux_model::INVENTORY_REPLY_COUNT;
                            connection.pending_inventory.clear();
                            connection.inventory_failed = false;
                            connection.inventory_retry_count = 0;
                        }
                        sync_topology = connection.has_inventory
                            && outcome == crate::tmux_model::ReconcileOutcome::Applied;
                        render_topology = sync_topology;
                    }
                }
                _ => {}
            }
        }

        if request_resync {
            self.queue_tmux_inventory(connection_id);
        }
        if let Some(pane_id) = pane_resume_request {
            self.pending_tmux_commands.push_back(PendingTmuxCommand {
                connection_id,
                bytes: crate::tmux_input::continue_pane_command(pane_id),
                expected_replies: vec![ExpectedTmuxReply::Ignored],
                kind: PendingTmuxCommandKind::Ordinary,
            });
        }
        if let Some(pane_id) = pane_resync_request {
            self.queue_tmux_pane_resync(connection_id, pane_id)?;
        }
        self.resolve_destroyed_tmux_gateways(
            sr,
            connection_id,
            &destroyed_gateway_panes,
            &destroyed_gateway_windows,
        )?;
        let chooser_updated = sync_topology && self.sync_tmux_panes(connection_id)?;
        if let Some((pane_id, status, output)) = bootstrap_reply
            && let Some(view) = self.view_stack.tmux_connection_mut(connection_id)
        {
            view.apply_bootstrap(pane_id, status, &output, self.clock.now_ms())?;
            render_topology = view.is_ready() && !view.is_showing_portal();
        }
        if let Some((pane_id, status, output)) = pane_resync_reply {
            let pane_is_present = self
                .tmux_connections
                .iter()
                .find(|connection| connection.id == connection_id)
                .is_some_and(|connection| connection.topology.pane(pane_id).is_some());
            if pane_is_present && status == crate::tmux_control::CommandStatus::Success {
                if let Some(view) = self.view_stack.tmux_connection_mut(connection_id) {
                    view.apply_bootstrap(pane_id, status, &output, self.clock.now_ms())?;
                    render_topology = view.is_ready() && !view.is_showing_portal();
                }
                if let Some(flow) = self
                    .tmux_connections
                    .iter_mut()
                    .find(|connection| connection.id == connection_id)
                    .and_then(|connection| connection.pane_flow.get_mut(&pane_id))
                {
                    flow.status = TmuxFlowStatus::Running;
                    flow.resync_count = flow.resync_count.saturating_add(1);
                }
                sr.speak(
                    &format!(
                        "tmux connection {connection_id} pane {} resynchronized; text and history restored; images, terminal parser continuation, and semantic metadata may be unavailable",
                        pane_id.0
                    ),
                    true,
                )?;
            } else if pane_is_present {
                if let Some(flow) = self
                    .tmux_connections
                    .iter_mut()
                    .find(|connection| connection.id == connection_id)
                    .and_then(|connection| connection.pane_flow.get_mut(&pane_id))
                {
                    flow.status = TmuxFlowStatus::ResyncFailed;
                    flow.resync_failures = flow.resync_failures.saturating_add(1);
                }
                sr.speak(
                    &format!(
                        "tmux connection {connection_id} pane {} resynchronization failed",
                        pane_id.0
                    ),
                    true,
                )?;
            }
        }
        if let Some((pane_id, bytes)) = pane_output {
            self.process_tmux_pane_transport(sr, connection_id, pane_id, &bytes, term_out)?;
        } else if chooser_updated {
            self.render_active_view(term_out)?;
            self.announce_view_change(sr)?;
        } else if render_topology {
            let active_state = self
                .view_stack
                .active_tmux_connection_mut()
                .filter(|view| view.connection_id() == connection_id && !view.is_showing_portal())
                .map(|view| view.is_ready());
            match active_state {
                Some(true) => self.render_tmux_topology_update(term_out)?,
                Some(false) => {
                    self.render_active_view(term_out)?;
                    self.announce_view_change(sr)?;
                }
                None => {}
            }
        }
        if let Some((status, description, output, show_success)) = user_command_result {
            let detail = output
                .iter()
                .map(|line| String::from_utf8_lossy(line))
                .collect::<Vec<_>>()
                .join("\n");
            let message = if detail.is_empty() {
                if show_success && status == crate::tmux_control::CommandStatus::Success {
                    format!("command completed: {description}")
                } else {
                    description
                }
            } else {
                format!("{description}: {detail}")
            };
            if status == crate::tmux_control::CommandStatus::Error {
                self.show_popup_error(sr, "tmux command failed", &message, term_out)?;
            } else {
                self.show_popup_announcement(sr, "tmux command result", &message, term_out)?;
            }
        }
        if let Some((is_error, message)) = notification_popup {
            if is_error {
                self.show_popup_error(sr, "tmux configuration error", &message, term_out)?;
            } else {
                self.show_popup_announcement(sr, "tmux message", &message, term_out)?;
            }
        }
        Ok(())
    }

    fn process_tmux_pane_transport(
        &mut self,
        sr: &mut ScreenReader,
        parent_connection_id: u64,
        pane_id: crate::tmux_model::PaneId,
        bytes: &[u8],
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let key = (parent_connection_id, pane_id.0);
        if !self.nested_tmux_gateways.contains_key(&key) {
            if self.nested_tmux_gateways.len() == MAX_NESTED_TMUX_GATEWAYS {
                anyhow::bail!("nested tmux gateway resource bound exceeded");
            }
            self.nested_tmux_gateways
                .insert(key, NestedTmuxGatewayState::new());
        }
        let events = self
            .nested_tmux_gateways
            .get_mut(&key)
            .expect("a pane gateway was inserted above")
            .router
            .push(bytes)?;
        if let Some(gateway) = self.nested_tmux_gateways.get_mut(&key) {
            if gateway.router.lifecycle_state()
                == crate::tmux_gateway::GatewayLifecycleState::AwaitingTerminator
            {
                gateway.termination_deadline_ms.get_or_insert_with(|| {
                    self.clock
                        .now_ms()
                        .saturating_add(TMUX_TERMINATOR_TIMEOUT_MS)
                });
            } else {
                gateway.termination_deadline_ms = None;
            }
        }
        for event in events {
            match event {
                crate::tmux_gateway::GatewayEvent::DirectOutput(bytes) => {
                    self.process_tmux_pane_output(
                        sr,
                        parent_connection_id,
                        pane_id,
                        &bytes,
                        term_out,
                    )?;
                }
                crate::tmux_gateway::GatewayEvent::ConnectionStarted {
                    connection_id: local_connection_id,
                } => {
                    let connection_id = self.next_tmux_connection_id;
                    self.next_tmux_connection_id = self
                        .next_tmux_connection_id
                        .checked_add(1)
                        .context("tmux connection id space is exhausted")?;
                    if let Some(gateway) = self.nested_tmux_gateways.get_mut(&key) {
                        gateway.active_local_connection_id = Some(local_connection_id);
                        gateway.active_global_connection_id = Some(connection_id);
                    }
                    let window_id = self
                        .tmux_connections
                        .iter()
                        .find(|connection| connection.id == parent_connection_id)
                        .and_then(|connection| connection.topology.pane(pane_id))
                        .map(|pane| pane.window_id.0)
                        .context("nested tmux gateway pane is absent from parent topology")?;
                    self.start_tmux_connection(
                        sr,
                        connection_id,
                        GatewayOrigin::Pane {
                            parent_connection_id,
                            window_id,
                            pane_id: pane_id.0,
                        },
                        term_out,
                    )?;
                }
                crate::tmux_gateway::GatewayEvent::Control {
                    connection_id: local_connection_id,
                    event,
                } => {
                    let connection_id = self
                        .nested_tmux_gateways
                        .get(&key)
                        .filter(|gateway| {
                            gateway.active_local_connection_id == Some(local_connection_id)
                        })
                        .and_then(|gateway| gateway.active_global_connection_id)
                        .context("nested tmux control event has no active global connection")?;
                    self.process_tmux_control(sr, connection_id, event, term_out)?;
                }
                crate::tmux_gateway::GatewayEvent::ConnectionEnded {
                    connection_id: local_connection_id,
                } => {
                    let connection_id = self
                        .nested_tmux_gateways
                        .get(&key)
                        .filter(|gateway| {
                            gateway.active_local_connection_id == Some(local_connection_id)
                        })
                        .and_then(|gateway| gateway.active_global_connection_id)
                        .context("nested tmux end event has no active global connection")?;
                    self.end_tmux_connection(sr, connection_id, term_out)?;
                    if let Some(gateway) = self.nested_tmux_gateways.get_mut(&key) {
                        gateway.active_local_connection_id = None;
                        gateway.active_global_connection_id = None;
                    }
                }
                crate::tmux_gateway::GatewayEvent::ConnectionFailed {
                    connection_id: local_connection_id,
                    reason,
                } => {
                    let connection_id = self
                        .nested_tmux_gateways
                        .get(&key)
                        .filter(|gateway| {
                            gateway.active_local_connection_id == Some(local_connection_id)
                        })
                        .and_then(|gateway| gateway.active_global_connection_id);
                    if let Some(connection_id) = connection_id {
                        self.fail_tmux_connection(sr, connection_id, &reason, term_out)?;
                    }
                    if let Some(gateway) = self.nested_tmux_gateways.get_mut(&key) {
                        gateway.active_local_connection_id = None;
                        gateway.active_global_connection_id = None;
                        gateway.termination_deadline_ms = None;
                    }
                }
            }
        }
        Ok(())
    }

    fn process_tmux_pane_output(
        &mut self,
        sr: &mut ScreenReader,
        connection_id: u64,
        pane_id: crate::tmux_model::PaneId,
        bytes: &[u8],
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let outcome = self
            .view_stack
            .tmux_connection_mut(connection_id)
            .map(|view| view.process_output(pane_id, bytes))
            .transpose()?
            .flatten();
        if let Some(outcome) = &outcome
            && !outcome.replies.is_empty()
        {
            self.queue_tmux_input(connection_id, pane_id, &outcome.replies)?;
        }
        let (is_visible, is_active_pane) =
            self.view_stack
                .active_tmux_connection_mut()
                .map_or((false, false), |view| {
                    let connection_visible =
                        view.connection_id() == connection_id && !view.is_showing_portal();
                    (
                        connection_visible && view.is_pane_visible(pane_id),
                        connection_visible && view.is_active_pane(pane_id),
                    )
                });
        let bells = outcome.as_ref().map_or(0, |outcome| outcome.bells);
        let presented_bells = if bells > 0 {
            self.present_tmux_bell(sr, connection_id, pane_id, is_visible, term_out)?
        } else {
            0
        };
        if is_visible && let Some(outcome) = outcome {
            self.render_tmux_pane_update(term_out, pane_id, presented_bells, &outcome.update)?;
            if is_active_pane {
                let now_ms = self.clock.now_ms();
                if self.first_pty_update.is_none() {
                    self.first_pty_update = Some(now_ms);
                }
                self.last_pty_update = Some(now_ms);
            }
        }
        Ok(())
    }

    fn resolve_destroyed_tmux_gateways(
        &mut self,
        sr: &mut ScreenReader,
        parent_connection_id: u64,
        pane_ids: &[crate::tmux_model::PaneId],
        window_ids: &[crate::tmux_model::WindowId],
    ) -> Result<()> {
        for pane_id in pane_ids {
            self.nested_tmux_gateways
                .remove(&(parent_connection_id, pane_id.0));
        }
        let mut removed = Vec::new();
        for window_id in window_ids {
            removed.extend(
                self.tmux_hierarchy
                    .remove_gateway_window(parent_connection_id, window_id.0),
            );
        }
        for pane_id in pane_ids {
            removed.extend(
                self.tmux_hierarchy
                    .remove_gateway_pane(parent_connection_id, pane_id.0),
            );
        }
        if removed.is_empty() {
            return Ok(());
        }
        let preserve_connection_chooser = self
            .view_stack
            .active_tmux_connection_chooser_mut()
            .is_some();
        if !preserve_connection_chooser {
            self.view_stack.clear_overlays();
        }
        removed.sort_unstable();
        removed.dedup();
        self.tmux_connections
            .retain(|connection| !removed.contains(&connection.id));
        self.pending_tmux_commands
            .retain(|command| !removed.contains(&command.connection_id));
        self.pending_direct_gateway_input
            .retain(|input| !removed.contains(&input.connection_id));
        self.cleanup_nested_gateway_state(&removed);
        if self
            .pending_tmux_confirmation
            .as_ref()
            .is_some_and(|confirmation| removed.contains(&confirmation.connection_id))
        {
            self.pending_tmux_confirmation = None;
        }
        if self
            .pending_gateway_confirmation
            .as_ref()
            .is_some_and(|confirmation| removed.contains(&confirmation.connection_id))
        {
            self.pending_gateway_confirmation = None;
        }
        self.view_stack.remove_tmux_connections(&removed);
        if self
            .active_tmux_connection
            .is_some_and(|connection_id| removed.contains(&connection_id))
        {
            self.active_tmux_connection = self
                .tmux_connections
                .iter()
                .any(|connection| connection.id == parent_connection_id)
                .then_some(parent_connection_id)
                .or_else(|| self.tmux_connections.last().map(|connection| connection.id));
        }
        if preserve_connection_chooser {
            self.sync_tmux_connection_chooser();
        } else if let Some(connection_id) = self.active_tmux_connection {
            self.view_stack.activate_tmux_connection(connection_id);
        } else {
            self.view_stack.activate_terminal();
        }
        sr.speak(
            &format!(
                "nested tmux connection ended because its parent pane or window disappeared; returned to tmux connection {parent_connection_id}"
            ),
            true,
        )?;
        Ok(())
    }

    fn cleanup_nested_gateway_state(&mut self, removed_connections: &[u64]) {
        self.nested_tmux_gateways
            .retain(|(parent_connection_id, _), gateway| {
                if gateway
                    .active_global_connection_id
                    .is_some_and(|id| removed_connections.contains(&id))
                {
                    gateway.active_local_connection_id = None;
                    gateway.active_global_connection_id = None;
                }
                !removed_connections.contains(parent_connection_id)
            });
    }

    pub(super) fn sync_tmux_panes(&mut self, connection_id: u64) -> Result<bool> {
        let Some(connection) = self
            .tmux_connections
            .iter_mut()
            .find(|connection| connection.id == connection_id)
        else {
            return Ok(false);
        };
        connection
            .pane_flow
            .retain(|pane_id, _| connection.topology.pane(*pane_id).is_some());
        for pane_id in connection.topology.panes().keys() {
            connection.pane_flow.entry(*pane_id).or_default();
        }
        let topology = connection.topology.clone();
        self.recent_tmux_bells
            .retain(|(source_connection, pane_id), _| {
                *source_connection != connection_id || topology.pane(*pane_id).is_some()
            });
        if self.last_tmux_bell_source.as_ref().is_some_and(|source| {
            source.connection_id == connection_id && topology.pane(source.pane_id).is_none()
        }) {
            self.last_tmux_bell_source = None;
        }
        let requests = self
            .view_stack
            .tmux_connection_mut(connection_id)
            .map(|view| view.sync_topology(&topology))
            .transpose()?
            .unwrap_or_default();
        for request in requests {
            self.pending_tmux_commands.push_back(PendingTmuxCommand {
                connection_id,
                bytes: request.command,
                expected_replies: vec![ExpectedTmuxReply::Bootstrap(request.pane_id)],
                kind: PendingTmuxCommandKind::Ordinary,
            });
        }
        let chooser_updated = self
            .view_stack
            .active_tmux_chooser_mut()
            .filter(|chooser| chooser.connection_id() == connection_id)
            .map(|chooser| chooser.sync_topology(&topology))
            .is_some();
        Ok(chooser_updated)
    }

    pub(super) fn queue_tmux_input(
        &mut self,
        connection_id: u64,
        pane_id: crate::tmux_model::PaneId,
        input: &[u8],
    ) -> Result<()> {
        if input.is_empty() {
            return Ok(());
        }
        if let Some(command) = self.pending_tmux_commands.back_mut()
            && command.connection_id == connection_id
            && command.kind == PendingTmuxCommandKind::Input(pane_id)
        {
            command.bytes.extend_from_slice(input);
            return Ok(());
        }
        self.pending_tmux_commands.push_back(PendingTmuxCommand {
            connection_id,
            bytes: input.to_vec(),
            expected_replies: Vec::new(),
            kind: PendingTmuxCommandKind::Input(pane_id),
        });
        Ok(())
    }

    fn queue_tmux_pane_resync(
        &mut self,
        connection_id: u64,
        pane_id: crate::tmux_model::PaneId,
    ) -> Result<()> {
        let command = self
            .view_stack
            .tmux_connection_mut(connection_id)
            .and_then(|view| view.pane_capture_command(pane_id))
            .context("tmux pane resync target is unavailable")?;
        self.pending_tmux_commands.push_back(PendingTmuxCommand {
            connection_id,
            bytes: command,
            expected_replies: vec![ExpectedTmuxReply::PaneResync(pane_id)],
            kind: PendingTmuxCommandKind::Ordinary,
        });
        Ok(())
    }

    fn queue_tmux_inventory(&mut self, connection_id: u64) {
        self.pending_tmux_commands.push_back(PendingTmuxCommand {
            connection_id,
            bytes: crate::tmux_model::INVENTORY_COMMAND.as_bytes().to_vec(),
            expected_replies: std::iter::repeat_n(
                ExpectedTmuxReply::Inventory,
                crate::tmux_model::INVENTORY_REPLY_COUNT,
            )
            .collect(),
            kind: PendingTmuxCommandKind::Ordinary,
        });
    }

    pub(super) fn queue_tmux_resize(
        &mut self,
        connection_id: u64,
        geometry: crate::terminal::TerminalGeometry,
    ) {
        self.pending_tmux_commands.retain(|command| {
            command.connection_id != connection_id || command.kind != PendingTmuxCommandKind::Resize
        });
        self.pending_tmux_commands.push_back(PendingTmuxCommand {
            connection_id,
            bytes: crate::tmux_input::refresh_client_command(geometry),
            expected_replies: vec![ExpectedTmuxReply::Ignored],
            kind: PendingTmuxCommandKind::Resize,
        });
    }

    pub(super) fn queue_tmux_user_command(
        &mut self,
        connection_id: u64,
        command: &str,
    ) -> Result<()> {
        self.queue_tmux_command(connection_id, command, false)
    }

    pub(super) fn queue_tmux_prompt_command(
        &mut self,
        connection_id: u64,
        command: &str,
    ) -> Result<()> {
        self.queue_tmux_command(connection_id, command, true)
    }

    fn queue_tmux_command(
        &mut self,
        connection_id: u64,
        command: &str,
        show_success: bool,
    ) -> Result<()> {
        let _ = crate::tmux_prefix::classify_binding(command)?;
        let command = command.to_owned();
        let mut bytes = command.as_bytes().to_vec();
        bytes.push(b'\n');
        self.pending_tmux_commands.push_back(PendingTmuxCommand {
            connection_id,
            bytes,
            expected_replies: vec![ExpectedTmuxReply::UserCommand {
                description: command,
                show_success,
            }],
            kind: PendingTmuxCommandKind::Ordinary,
        });
        Ok(())
    }

    pub fn handle_tick(
        &mut self,
        sr: &mut ScreenReader,
        pty_out: &mut dyn Write,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        self.expire_tmux_gateway_terminators(sr, term_out)?;
        self.drain_direct_gateway_input(pty_out)?;
        if let Some(connection_id) = self.tmux_gateway.active_connection() {
            self.drain_tmux_commands_for(connection_id, pty_out)?;
        }
        let released_probe_input = self
            .startup_probe_broker
            .as_mut()
            .map(|broker| broker.finish_if_timed_out(self.clock.now_ms()))
            .unwrap_or_default();
        self.refresh_probed_profile();
        if !released_probe_input.is_empty() {
            self.handle_stdin(sr, &released_probe_input, pty_out, term_out)?;
        }
        let replies = self.application_replies.take(ROOT_SOURCE);
        if !replies.is_empty() {
            self.log_bytes("virtual terminal replies to app", &replies);
            pty_out
                .write_all(&replies)
                .context("write virtual terminal replies")?;
            pty_out.flush().context("flush virtual terminal replies")?;
        }
        self.flush_pending_input(sr, pty_out, term_out)?;
        let tick_action = self.view_stack.active_mut().tick(sr, pty_out)?;
        self.handle_view_action(sr, tick_action, term_out)
    }

    fn expire_tmux_gateway_terminators(
        &mut self,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let now_ms = self.clock.now_ms();
        if self
            .tmux_termination_deadline_ms
            .is_some_and(|deadline| now_ms >= deadline)
        {
            let events = self.tmux_gateway.expire_termination();
            self.tmux_termination_deadline_ms = None;
            for event in events {
                self.handle_tmux_gateway_event(sr, event, term_out)?;
            }
        }

        let expired = self
            .nested_tmux_gateways
            .iter()
            .filter_map(|(key, gateway)| {
                gateway
                    .termination_deadline_ms
                    .is_some_and(|deadline| now_ms >= deadline)
                    .then_some(*key)
            })
            .collect::<Vec<_>>();
        for key in expired {
            let (events, global_connection_id) = {
                let Some(gateway) = self.nested_tmux_gateways.get_mut(&key) else {
                    continue;
                };
                gateway.termination_deadline_ms = None;
                (
                    gateway.router.expire_termination(),
                    gateway.active_global_connection_id,
                )
            };
            for event in events {
                if let crate::tmux_gateway::GatewayEvent::ConnectionFailed { reason, .. } = event
                    && let Some(connection_id) = global_connection_id
                {
                    self.fail_tmux_connection(sr, connection_id, &reason, term_out)?;
                }
            }
            if let Some(gateway) = self.nested_tmux_gateways.get_mut(&key) {
                gateway.active_local_connection_id = None;
                gateway.active_global_connection_id = None;
            }
        }
        Ok(())
    }

    fn drain_direct_gateway_input(&mut self, output: &mut dyn Write) -> Result<()> {
        let queued = self.pending_direct_gateway_input.len();
        let mut wrote = false;
        for _ in 0..queued {
            let Some(input) = self.pending_direct_gateway_input.pop_front() else {
                break;
            };
            if self.tmux_gateway.active_connection() != Some(input.connection_id) {
                continue;
            }
            // Exceptional controls are intentionally at-most-once. Retrying a
            // partially written SSH escape or signal byte can target a later
            // shell after the original transport has already closed.
            output
                .write_all(&input.bytes)
                .context("write exceptional tmux gateway control")?;
            wrote = true;
        }
        if wrote {
            output
                .flush()
                .context("flush exceptional tmux gateway control")?;
        }
        Ok(())
    }

    /// Drains only the commands owned by one control transport.
    pub fn drain_tmux_commands_for(
        &mut self,
        connection_id: u64,
        output: &mut dyn Write,
    ) -> Result<()> {
        let queued = self.pending_tmux_commands.len();
        let mut wrote = false;
        for _ in 0..queued {
            let Some(command) = self.pending_tmux_commands.pop_front() else {
                break;
            };
            if self.direct_transport_for(command.connection_id) != Some(connection_id) {
                self.pending_tmux_commands.push_back(command);
                continue;
            }
            let Some((root_connection_id, encoded)) = self.route_tmux_command(command)? else {
                continue;
            };
            debug_assert_eq!(root_connection_id, connection_id);
            for bytes in encoded {
                self.log_bytes("tmux control command", &bytes);
                output
                    .write_all(&bytes)
                    .context("write scoped tmux control command")?;
                wrote = true;
            }
        }
        if wrote {
            output
                .flush()
                .context("flush scoped tmux control commands")?;
        }
        Ok(())
    }

    fn direct_transport_for(&self, mut connection_id: u64) -> Option<u64> {
        for _ in 0..=64 {
            match self.tmux_hierarchy.origin(connection_id)? {
                GatewayOrigin::Direct => return Some(connection_id),
                GatewayOrigin::Pane {
                    parent_connection_id,
                    ..
                } => connection_id = parent_connection_id,
            }
        }
        None
    }

    fn route_tmux_command(
        &mut self,
        command: PendingTmuxCommand,
    ) -> Result<Option<(u64, Vec<Vec<u8>>)>> {
        if !self
            .tmux_connections
            .iter()
            .any(|connection| connection.id == command.connection_id)
        {
            return Ok(None);
        }
        let mut connection_id = command.connection_id;
        let mut encoded = match command.kind {
            PendingTmuxCommandKind::Input(pane_id) => {
                crate::tmux_input::encode_send_keys(pane_id, &command.bytes)?
                    .into_iter()
                    .map(|bytes| (bytes, vec![ExpectedTmuxReply::Ignored]))
                    .collect::<Vec<_>>()
            }
            PendingTmuxCommandKind::Ordinary | PendingTmuxCommandKind::Resize => {
                vec![(command.bytes, command.expected_replies)]
            }
        };
        for _ in 0..=64 {
            let Some(connection_index) = self
                .tmux_connections
                .iter()
                .position(|connection| connection.id == connection_id)
            else {
                return Ok(None);
            };
            for (_, expected_replies) in &encoded {
                self.tmux_connections[connection_index]
                    .expected_replies
                    .extend(expected_replies.iter().cloned());
            }
            match self.tmux_hierarchy.origin(connection_id) {
                Some(GatewayOrigin::Direct) => {
                    return Ok(Some((
                        connection_id,
                        encoded.into_iter().map(|(bytes, _)| bytes).collect(),
                    )));
                }
                Some(GatewayOrigin::Pane {
                    parent_connection_id,
                    pane_id,
                    ..
                }) => {
                    let mut parent_commands = Vec::new();
                    for (bytes, _) in encoded {
                        parent_commands.extend(
                            crate::tmux_input::encode_send_keys(
                                crate::tmux_model::PaneId(pane_id),
                                &bytes,
                            )?
                            .into_iter()
                            .map(|bytes| (bytes, vec![ExpectedTmuxReply::Ignored])),
                        );
                    }
                    encoded = parent_commands;
                    connection_id = parent_connection_id;
                }
                None => return Ok(None),
            }
        }
        anyhow::bail!("tmux connection routing exceeds its nesting-depth bound")
    }

    pub fn maybe_finalize_changes(&mut self, sr: &mut ScreenReader) -> Result<bool> {
        let Some(lpu) = self.last_pty_update else {
            return Ok(false);
        };
        let first_pty_update = self.first_pty_update.unwrap_or(lpu);
        let now_ms = self.clock.now_ms();
        let tmux_base_active = self
            .view_stack
            .active_tmux_connection_mut()
            .is_some_and(|view| view.is_ready() && !view.is_showing_portal());
        let overlay_active = !tmux_base_active && self.view_stack.has_overlay();
        let synchronized_output = if tmux_base_active {
            self.view_stack
                .active_mut()
                .model()
                .update_summary()
                .synchronized_output
        } else {
            self.view_stack
                .root_mut()
                .model()
                .update_summary()
                .synchronized_output
        };
        if synchronized_output {
            return Ok(false);
        }
        if now_ms.saturating_sub(lpu) >= DIFF_DELAY as u128
            || now_ms.saturating_sub(first_pty_update) >= MAX_DIFF_DELAY as u128
        {
            self.first_pty_update = None;
            self.last_pty_update = None;
            let recent_input = self
                .last_stdin_update
                .is_some_and(|lsu| now_ms.saturating_sub(lsu) <= MAX_DIFF_DELAY as u128);
            let view = if tmux_base_active {
                self.view_stack.active_mut().model()
            } else {
                self.view_stack.root_mut().model()
            };
            view.with_live_screen(|view| -> Result<()> {
                if !overlay_active {
                    let mut read_text = sr.resolve_pending_delete(view)?;
                    let semantic_history_read = if sr.take_pending_history_navigation() {
                        if let Some(input) = view.active_semantic_input() {
                            sr.speak(if input.is_empty() { "blank" } else { &input }, false)?;
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if semantic_history_read {
                        read_text = true;
                    } else {
                        if sr.highlight_tracking_enabled() {
                            sr.track_highlighting(view)?;
                        }
                        let auto_read_text = if sr.auto_read_enabled() {
                            if recent_input {
                                sr.auto_read_after_input(view)?
                            } else {
                                sr.auto_read(view)?
                            }
                        } else {
                            false
                        };
                        read_text |= auto_read_text;
                    }
                    if recent_input && !read_text {
                        sr.track_cursor(view)?;
                    }
                }

                if sr.review_follows_screen_cursor()
                    && view.screen().cursor_position() != view.prev_screen().cursor_position()
                {
                    let old = view.review_cursor_position();
                    view.follow_application_cursor();
                    sr.hook_on_review_cursor_move(old, view.review_cursor_position())?;
                }
                sr.hook_on_screen_update(view, overlay_active)?;
                view.finalize_changes(now_ms);
                Ok(())
            })?;
            return Ok(true);
        }
        Ok(false)
    }
}
