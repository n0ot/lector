use super::*;

impl App {
    fn next_available_tmux_connection_id(&self) -> Result<u64> {
        let mut candidate = 1_u64;
        while self
            .tmux_connections
            .iter()
            .any(|connection| connection.id == candidate)
        {
            candidate = candidate
                .checked_add(1)
                .context("tmux connection id space is exhausted")?;
        }
        Ok(candidate)
    }

    /// Starts one transport-neutral bounded PTY-drain presentation
    /// transaction. Every read still mutates its terminal model and publishes
    /// replies/effects immediately; only scene composition is deferred.
    pub fn begin_pty_presentation_batch(&mut self) {
        debug_assert!(self.pending_presentation_batch.is_none());
        self.pending_presentation_batch = Some(PendingPresentationBatch::default());
    }

    /// Drops presentation bookkeeping after a failed PTY drain. Model changes,
    /// replies, and nonvisual effects have already been applied or dispatched.
    /// Physical bells remain transaction-owned and are canceled with the
    /// visual update so a failed drain cannot emit an orphan notification.
    pub fn cancel_pty_presentation_batch(&mut self) {
        if self
            .pending_presentation_batch
            .take()
            .is_some_and(|batch| batch.has_scene_work())
        {
            // The model is now ahead of the physical presentation. Force the
            // next successful scene publication to reconstruct that gap.
            self.scene_renderer.invalidate();
        }
    }

    /// Presents the current authoritative scene exactly once after one bounded
    /// PTY drain. The common root and single-pane cases retain their semantic
    /// operation hints without allocating a pane map. Topology or overlay
    /// transitions supersede stale incremental damage with a full scene.
    pub fn finish_pty_presentation_batch(&mut self, term_out: &mut dyn Write) -> Result<()> {
        let Some(batch) = self.pending_presentation_batch.take() else {
            return Ok(());
        };
        let bell_count = batch.bell_count;
        if batch.authoritative_scene_required {
            return self.render_full_scene(term_out, bell_count);
        }

        let active_tmux_connection = self
            .view_stack
            .active_tmux_connection_mut()
            .map(|view| view.connection_id());
        if active_tmux_connection.is_none() {
            return batch.root_update.map_or(Ok(()), |update| {
                self.render_terminal_update(term_out, bell_count, &update)
            });
        }

        self.render_tmux_batched_updates(term_out, bell_count, batch.pane_updates())
    }

    fn mark_tmux_pane_capture_required(flow: &mut TmuxPaneFlowState) {
        flow.status = TmuxFlowStatus::Resynchronizing;
        flow.final_resync_requested = false;
        flow.resync_after_ms = None;
        flow.recapture_hard_deadline_ms = None;
        flow.limitations.extend([
            TmuxResyncLimitation::KittyImages,
            TmuxResyncLimitation::SemanticMetadata,
        ]);
    }

    /// Coalesces output which races an authoritative capture without waiting
    /// forever for a continuously active pane to become quiet. The first
    /// raced byte starts a hard deadline; later bytes may move the soft quiet
    /// deadline only up to that fixed boundary.
    fn schedule_tmux_pane_recapture(flow: &mut TmuxPaneFlowState, now_ms: u128) {
        if flow.final_resync_requested {
            return;
        }
        let hard_deadline = *flow
            .recapture_hard_deadline_ms
            .get_or_insert_with(|| now_ms.saturating_add(TMUX_RECOVERY_HARD_DEADLINE_MS));
        flow.resync_after_ms = Some(
            now_ms
                .saturating_add(TMUX_RECOVERY_QUIET_MS)
                .min(hard_deadline),
        );
    }

    fn tmux_flow_retry_delay_ms(consecutive_failures: u32) -> u128 {
        let exponent = consecutive_failures.saturating_sub(1).min(5);
        TMUX_FLOW_RETRY_BASE_MS
            .saturating_mul(1_u128 << exponent)
            .min(TMUX_FLOW_RETRY_MAX_MS)
    }

    fn tmux_capture_line_flags_unsupported(
        status: crate::tmux_control::CommandStatus,
        output: &[Vec<u8>],
    ) -> bool {
        status == crate::tmux_control::CommandStatus::Error
            && output.iter().any(|line| {
                line.windows(b"unknown flag".len())
                    .any(|window| window == b"unknown flag")
                    && line.windows(b"-F".len()).any(|window| window == b"-F")
            })
    }

    fn complete_tmux_pane_resync(flow: &mut TmuxPaneFlowState) {
        flow.status = TmuxFlowStatus::Running;
        flow.is_paused = false;
        flow.pause_requested = false;
        flow.resume_requested = false;
        flow.resync_requested = false;
        flow.resync_in_flight = false;
        flow.final_resync_requested = false;
        flow.resync_after_ms = None;
        flow.recapture_hard_deadline_ms = None;
        flow.resync_count = flow.resync_count.saturating_add(1);
        flow.consecutive_resync_failures = 0;
        flow.resync_failure_announced = false;
    }

    fn tmux_pane_is_presented(
        connection: &TmuxConnectionState,
        connection_is_presented: bool,
        active_gateway_path: &[(u64, crate::tmux_model::PaneId)],
        pane_id: crate::tmux_model::PaneId,
    ) -> bool {
        let attached_window = connection
            .topology
            .attached_session()
            .and_then(|session_id| connection.topology.session(session_id))
            .and_then(|session| session.active_window);
        (connection_is_presented
            && connection
                .topology
                .pane(pane_id)
                .is_some_and(|pane| Some(pane.window_id) == attached_window))
            || active_gateway_path.contains(&(connection.id, pane_id))
    }

    pub fn flush_application_replies(&mut self, pty_out: &mut dyn Write) -> Result<()> {
        // The child can run before the outer terminal answers Lector's
        // startup probes. Hold its generated replies only while the bounded
        // broker can still learn the exact default colours; otherwise an
        // eager TUI could permanently cache Lector's dark fallback on a light
        // terminal. A completed/expired probe always releases the child.
        let color_probe_pending = self
            .startup_probe_broker
            .as_ref()
            .is_some_and(|broker| broker.color_wait_pending(self.clock.now_ms()));
        let mut replies = self.application_replies.take(ROOT_SOURCE);
        if replies.is_empty() {
            return Ok(());
        }
        if color_probe_pending
            && let Some(offset) =
                crate::terminal_protocol::first_virtual_color_reply_offset(&replies)
        {
            let held = replies.split_off(offset);
            self.application_replies.queue(ROOT_SOURCE, &held);
        }
        if let Some(colors) = self.physical_profile.virtual_terminal_colors() {
            replies = crate::terminal_protocol::rewrite_virtual_color_replies(&replies, colors);
        }
        if replies.is_empty() {
            return Ok(());
        }
        self.log_bytes("virtual terminal replies to app", &replies);
        pty_out
            .write_all(&replies)
            .context("write virtual terminal replies")?;
        pty_out.flush().context("flush virtual terminal replies")
    }

    pub fn handle_pty(
        &mut self,
        sr: &mut ScreenReader,
        buf: &[u8],
        term_out: &mut dyn Write,
    ) -> Result<()> {
        self.log_bytes("pty output from source", buf);
        self.log_latency_stage("source-output-read", || format!("bytes={}", buf.len()));
        let events = self.tmux_gateway.push(buf)?;
        self.sync_root_tmux_termination_deadline();
        self.handle_tmux_gateway_events(sr, events, term_out)?;
        self.flush_pending_clipboard_writes(sr, term_out)
    }

    fn handle_tmux_gateway_events(
        &mut self,
        sr: &mut ScreenReader,
        events: impl IntoIterator<Item = crate::tmux_gateway::GatewayEvent>,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let mut coalescer = TmuxGatewayOutputCoalescer::default();
        for event in events {
            let (first, second) = coalescer.route_gateway_event(event);
            if let Some(event) = first {
                self.handle_tmux_gateway_event(sr, event, term_out)?;
            }
            if let Some(event) = second {
                self.handle_tmux_gateway_event(sr, event, term_out)?;
            }
        }
        if let Some(event) = coalescer.finish() {
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
        self.handle_tmux_gateway_events(sr, events, term_out)?;
        self.flush_pending_clipboard_writes(sr, term_out)
    }

    /// Applies one already-routed control event from any independent transport.
    pub fn handle_tmux_gateway_event(
        &mut self,
        sr: &mut ScreenReader,
        event: crate::tmux_gateway::GatewayEvent,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        if self.log_enabled {
            let detail = match &event {
                crate::tmux_gateway::GatewayEvent::DirectOutput(bytes) => {
                    format!("direct-output length={}", bytes.len())
                }
                crate::tmux_gateway::GatewayEvent::ConnectionStarted { connection_id } => {
                    format!("connection-started id={connection_id}")
                }
                crate::tmux_gateway::GatewayEvent::Control {
                    connection_id,
                    event,
                } => match event {
                    crate::tmux_control::ControlEvent::Output { pane_id, bytes } => {
                        format!(
                            "control-output connection={connection_id} pane={pane_id} length={}",
                            bytes.len()
                        )
                    }
                    crate::tmux_control::ControlEvent::ExtendedOutput {
                        pane_id,
                        age_ms,
                        bytes,
                        ..
                    } => format!(
                        "extended-output connection={connection_id} pane={pane_id} age_ms={age_ms} length={}",
                        bytes.len()
                    ),
                    crate::tmux_control::ControlEvent::Command { status, output, .. } => format!(
                        "command connection={connection_id} status={status:?} lines={} bytes={}",
                        output.len(),
                        output.iter().map(Vec::len).sum::<usize>()
                    ),
                    other => format!("control connection={connection_id} event={other:?}"),
                },
                crate::tmux_gateway::GatewayEvent::ConnectionEnded { connection_id } => {
                    format!("connection-ended id={connection_id}")
                }
                crate::tmux_gateway::GatewayEvent::ConnectionFailed {
                    connection_id,
                    reason,
                } => format!("connection-failed id={connection_id} reason={reason}"),
            };
            // Per-record output is already represented by the bounded PTY byte
            // log. Avoid a second line for every record during floods.
            if !detail.starts_with("control-output") && !detail.starts_with("extended-output") {
                crate::diagnostics::event("tmux-gateway", "event", &detail);
            }
        }
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
        let kitty_keyboard_flags_before = self
            .view_stack
            .root_mut()
            .model()
            .live_screen()
            .kitty_keyboard_flags();
        let terminal_update = self
            .view_stack
            .root_mut()
            .model()
            .process_changes_with_batch(buf, true);
        let kitty_keyboard_flags_after = self
            .view_stack
            .root_mut()
            .model()
            .live_screen()
            .kitty_keyboard_flags();
        if kitty_keyboard_flags_before != 0 && kitty_keyboard_flags_after == 0 {
            // The physical terminal may already have encoded one more rapid
            // Ctrl-C under the exiting application's Kitty mode by the time
            // its reset reaches Lector. Keep that stale cycle out of the
            // resumed shell, including when the new press arrives only after
            // the reset was parsed.
            self.kitty_ctrl_c_input_handoff = Some(KittyInputHandoff {
                target: ForwardedInputTarget::RootPty,
                deadline_ms: self
                    .clock
                    .now_ms()
                    .saturating_add(KITTY_CTRL_C_INPUT_HANDOFF_MS),
            });
            self.log_event("arming Ctrl-C input quarantine after Kitty mode reset");
        } else if kitty_keyboard_flags_after != 0
            && self
                .kitty_ctrl_c_input_handoff
                .is_some_and(|handoff| handoff.target == ForwardedInputTarget::RootPty)
        {
            self.kitty_ctrl_c_input_handoff = None;
        }
        let new_replies = &terminal_update.pty_replies;
        if !new_replies.is_empty() {
            self.application_replies.queue(ROOT_SOURCE, new_replies);
        }
        let effect_time = self.clock.now_ms();
        for event in &terminal_update.effects.events {
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
        let output_screen = (
            terminal_update.screen_after,
            terminal_update.screen_before != terminal_update.screen_after,
        );
        let adaptive_quiet_trainable = adaptive_quiet_is_trainable(&terminal_update);
        let bells = terminal_update.effects.bells;
        if let Some(batch) = &mut self.pending_presentation_batch {
            batch.push_root(terminal_update, bells);
        } else {
            self.render_terminal_update(term_out, bells, &terminal_update)?;
        }
        let now_ms = self.clock.now_ms();
        let view_id = self.view_stack.root_mut().model().view_id();
        self.note_pty_update(
            AccessibilityContext {
                view_id,
                screen: output_screen.0,
            },
            now_ms,
            output_screen.1,
            adaptive_quiet_trainable,
        );
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
        self.tmux_connections.push(TmuxConnectionState {
            id: connection_id,
            topology: crate::tmux_model::TmuxTopology::new(connection_id),
            initial_command_seen: false,
            inventory_replies_remaining: 0,
            pending_inventory: Vec::new(),
            pending_inventory_bytes: 0,
            pending_inventory_lines: 0,
            inventory_failed: false,
            inventory_failure_detail: None,
            inventory_invalidated: false,
            inventory_phase: TmuxInventoryPhase::Idle,
            expected_replies: VecDeque::new(),
            has_inventory: false,
            inventory_retry_count: 0,
            command_history: Vec::new(),
            prefix_state: None,
            key_table_override: None,
            pane_flow: BTreeMap::new(),
            pending_pane_captures: BTreeMap::new(),
            flow_control_policy_accepted: None,
            flow_control_verified: None,
            flow_control_warning_announced: false,
            capture_line_flags_supported: None,
            last_announced_location: None,
            preferred_location: None,
        });
        self.pending_tmux_commands.push_back(PendingTmuxCommand {
            connection_id,
            bytes: TMUX_FLOW_CONTROL_COMMAND.to_vec(),
            expected_replies: vec![ExpectedTmuxReply::FlowControlPolicy],
            kind: PendingTmuxCommandKind::Ordinary,
        });
        self.pending_tmux_commands.push_back(PendingTmuxCommand {
            connection_id,
            bytes: TMUX_FLOW_CONTROL_VERIFY_COMMAND.to_vec(),
            expected_replies: vec![ExpectedTmuxReply::FlowControlVerification],
            kind: PendingTmuxCommandKind::Ordinary,
        });
        // Control-mode clients do not inherit their size from the PTY. Tell
        // tmux the current outer geometry before inventory so the initial
        // layouts and pane captures are produced at the displayed size.
        let geometry = self.view_stack.root_mut().model().live_screen().geometry;
        self.queue_tmux_resize(connection_id, geometry);
        self.queue_tmux_inventory(connection_id);
        self.remember_tmux_gateway_parent_locations(connection_id);
        self.active_tmux_connection = Some(connection_id);
        self.cancel_stabilization_bursts();
        self.view_stack.clear_overlays();
        let (rows, cols) = self.view_stack.root_mut().model().live_size();
        self.view_stack
            .push(Box::new(views::TmuxConnectionView::new(
                rows,
                cols,
                connection_id,
            )));
        self.render_active_view(term_out)?;
        sr.speak("tmux", false)?;
        Ok(())
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
            .pending_force_abandon
            .as_ref()
            .is_some_and(|pending| removed_connections.contains(&pending.connection_id))
        {
            self.pending_force_abandon = None;
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
        self.tmux_background_bell_windows
            .retain(|(connection_id, _)| !removed_connections.contains(connection_id));
        if self
            .last_tmux_bell_source
            .as_ref()
            .is_some_and(|source| removed_connections.contains(&source.connection_id))
        {
            self.last_tmux_bell_source = None;
        }
        self.pending_tmux_commands
            .retain(|command| !removed_connections.contains(&command.connection_id));
        self.pending_tmux_session_switches
            .retain(|connection_id, _| !removed_connections.contains(connection_id));
        self.tmux_carrier_leases
            .retain(|(parent_connection_id, _, _), _| {
                !removed_connections.contains(parent_connection_id)
            });
        self.rejected_tmux_carrier_indices
            .retain(|(parent_connection_id, _, _), _| {
                !removed_connections.contains(parent_connection_id)
            });
        self.pending_direct_gateway_input
            .retain(|input| !removed_connections.contains(&input.connection_id));
        self.discard_deferred_tmux_connections(&removed_connections);
        self.cleanup_nested_gateway_state(&removed_connections);
        let carrier_cleanup_parents = self
            .tmux_carrier_leases
            .values()
            .map(|lease| lease.parent_connection_id)
            .collect::<BTreeSet<_>>();
        for parent_connection_id in carrier_cleanup_parents {
            if let Some(attached_session_id) = self
                .tmux_connections
                .iter()
                .find(|connection| connection.id == parent_connection_id)
                .and_then(|connection| connection.topology.attached_session())
            {
                self.queue_obsolete_tmux_carrier_unlinks(parent_connection_id, attached_session_id);
            }
        }
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
        self.cancel_stabilization_bursts();
        if !preserve_connection_chooser {
            self.view_stack.clear_overlays();
        }
        self.view_stack
            .remove_tmux_connections(&removed_connections);
        if self.tmux_connections.is_empty() {
            self.view_stack.activate_terminal();
        } else if preserve_connection_chooser {
            self.sync_tmux_connection_chooser();
        } else if let Some(connection_id) = self.active_tmux_connection {
            self.view_stack.activate_tmux_connection(connection_id);
            self.sync_tmux_panes(connection_id)?;
        } else {
            self.view_stack.activate_terminal();
        }
        self.render_active_view(term_out)?;
        self.announce_view_change(sr)?;
        self.advance_graceful_tmux_teardown(sr, term_out)
    }

    fn process_tmux_control(
        &mut self,
        sr: &mut ScreenReader,
        connection_id: u64,
        event: crate::tmux_control::ControlEvent,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let now_ms = self.clock.now_ms();
        let active_gateway_path = self.live_tmux_gateway_carriers();
        let connection_is_presented = self
            .view_stack
            .presented_tmux_connection_mut()
            .is_some_and(|view| view.connection_id() == connection_id && !view.is_showing_portal());
        let mut request_resync = false;
        let mut sync_topology = false;
        let mut render_topology = false;
        let mut bootstrap_reply = None;
        let mut bootstrap_retry = None;
        let mut pane_resync_probe = None;
        let mut pane_resync_success = None;
        let mut pane_resync_failure = None;
        let mut pane_resync_raced = None;
        let mut pane_resync_request = None;
        let mut pane_resume_request = None;
        let mut resume_after_pane_resync = None;
        let mut pane_output = None;
        let mut user_command_result = None;
        let mut notification_popup = None;
        let mut inventory_terminal_failure = None;
        let mut carrier_lease_create_reply = None;
        let mut carrier_lease_verify_reply = None;
        let mut carrier_unlink_reply = None;
        let mut carrier_session_switch_reply = None;
        let mut carrier_move_reply = None;
        let mut carrier_move_verify_reply = None;
        let mut lost_gateway_carrier = None;
        let mut inventory_applied = false;
        let location_changed;
        let attached_session_changed;
        let mut destroyed_gateway_panes = Vec::new();
        let mut destroyed_gateway_windows = Vec::new();
        let mut native_copy_mode_panes = Vec::new();
        let mut open_review_for_native_copy_mode = false;
        {
            let Some(connection) = self
                .tmux_connections
                .iter_mut()
                .find(|connection| connection.id == connection_id)
            else {
                return Ok(());
            };
            let had_inventory = connection.has_inventory;
            let previous_attached_session = connection.topology.attached_session();
            let previous_location = connection.topology.attached_location();
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
                                connection.append_inventory(output);
                            } else {
                                connection.record_inventory_error(&output);
                            }
                            if connection.inventory_replies_remaining == 0 {
                                connection.inventory_phase = TmuxInventoryPhase::Idle;
                                let inventory = connection.take_inventory();
                                let inventory_invalidated =
                                    std::mem::take(&mut connection.inventory_invalidated);
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
                                let inventory_error = if inventory_invalidated {
                                    None
                                } else if connection.inventory_failed {
                                    Some(
                                        connection.inventory_failure_detail.clone().unwrap_or_else(
                                            || "tmux inventory transaction failed".to_owned(),
                                        ),
                                    )
                                } else {
                                    connection
                                        .topology
                                        .replace_inventory(&inventory)
                                        .err()
                                        .map(|error| error.to_string())
                                };
                                if inventory_invalidated {
                                    // Inventory is a sequence of independent
                                    // tmux commands. A topology notification
                                    // between their replies proves this
                                    // generation may be a mixed snapshot. Drain
                                    // it, retain the last valid model, and begin
                                    // a fresh generation after the notification.
                                    connection.topology.mark_resync_required();
                                    connection.inventory_retry_count = 0;
                                    request_resync = true;
                                } else if let Some(detail) = inventory_error {
                                    connection.topology.mark_resync_required();
                                    if connection.inventory_retry_count == 0 {
                                        connection.inventory_retry_count = 1;
                                        request_resync = true;
                                    } else {
                                        inventory_terminal_failure =
                                            Some((connection.has_inventory, detail));
                                    }
                                } else {
                                    connection.key_table_override = None;
                                    let active_pane = connection.topology.attached_active_pane();
                                    native_copy_mode_panes.extend(
                                        connection
                                            .topology
                                            .panes()
                                            .values()
                                            .filter(|pane| pane.mode == "copy-mode")
                                            .map(|pane| pane.id),
                                    );
                                    open_review_for_native_copy_mode = connection_is_presented
                                        && active_pane.is_some_and(|active| {
                                            native_copy_mode_panes.contains(&active)
                                        });
                                    for pane_id in &native_copy_mode_panes {
                                        connection.topology.clear_native_copy_mode(*pane_id);
                                    }
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
                                    inventory_applied = true;
                                    sync_topology = true;
                                    render_topology = true;
                                    if connection.flow_control_verified == Some(false)
                                        && !connection.flow_control_warning_announced
                                    {
                                        connection.flow_control_warning_announced = true;
                                        notification_popup = Some((
                                            true,
                                            "tmux did not retain the pause-after=1 control-client flag; automatic loss-bounded pane recovery is unavailable in this tmux version"
                                                .to_owned(),
                                        ));
                                    }
                                }
                                if !request_resync {
                                    connection.reset_inventory_attempt();
                                }
                            }
                        }
                        Some(ExpectedTmuxReply::FlowControlPolicy) => {
                            connection.flow_control_policy_accepted =
                                Some(status == crate::tmux_control::CommandStatus::Success);
                        }
                        Some(ExpectedTmuxReply::FlowControlVerification) => {
                            let retained = status == crate::tmux_control::CommandStatus::Success
                                && output.iter().any(|line| {
                                    line.split(|byte| *byte == b',')
                                        .any(|flag| flag == b"pause-after=1")
                                });
                            connection.flow_control_verified = Some(
                                connection.flow_control_policy_accepted == Some(true) && retained,
                            );
                        }
                        Some(ExpectedTmuxReply::Bootstrap {
                            pane_id,
                            line_flags,
                        }) => {
                            if line_flags
                                && Self::tmux_capture_line_flags_unsupported(status, &output)
                                && let Some(pane) = connection.topology.pane(pane_id)
                            {
                                connection.capture_line_flags_supported = Some(false);
                                bootstrap_retry = Some((
                                    pane_id,
                                    crate::tmux_panes::portable_capture_command(pane),
                                ));
                            } else {
                                if line_flags
                                    && status == crate::tmux_control::CommandStatus::Success
                                {
                                    connection.capture_line_flags_supported = Some(true);
                                }
                                bootstrap_reply = Some((pane_id, status, output, line_flags));
                            }
                        }
                        Some(ExpectedTmuxReply::PaneResyncProbe(pane_id)) => {
                            if let Some(flow) = connection.pane_flow.get_mut(&pane_id) {
                                flow.resync_in_flight = false;
                            }
                            let metadata = (status == crate::tmux_control::CommandStatus::Success
                                && output.len() == 1)
                                .then(|| {
                                    crate::tmux_model::parse_pane_capture_metadata(
                                        &output[0], pane_id,
                                    )
                                    .ok()
                                })
                                .flatten();
                            if let Some(metadata) = metadata {
                                connection.pending_pane_captures.insert(
                                    pane_id,
                                    PendingTmuxPaneCapture {
                                        metadata: metadata.clone(),
                                        output: None,
                                        pending_escape: Vec::new(),
                                        line_flags: connection.capture_line_flags_supported
                                            != Some(false),
                                        parser_continuation_available: false,
                                        failed: false,
                                    },
                                );
                                pane_resync_probe = Some(metadata);
                            } else {
                                connection.pending_pane_captures.remove(&pane_id);
                                pane_resync_failure = Some(pane_id);
                            }
                        }
                        Some(ExpectedTmuxReply::PaneResyncContinue(pane_id)) => {
                            let flow = connection.pane_flow.entry(pane_id).or_default();
                            flow.pause_requested = false;
                            flow.resume_requested = false;
                            if status == crate::tmux_control::CommandStatus::Success {
                                flow.is_paused = false;
                            } else if let Some(capture) =
                                connection.pending_pane_captures.get_mut(&pane_id)
                            {
                                // The capture commands later in this batch will
                                // still run. Reject their snapshot: a failed
                                // continue did not establish a clean boundary
                                // with subsequent pane output.
                                capture.failed = true;
                            }
                        }
                        Some(ExpectedTmuxReply::PaneResyncCapture(pane_id)) => {
                            if let Some(capture) =
                                connection.pending_pane_captures.get_mut(&pane_id)
                            {
                                if status == crate::tmux_control::CommandStatus::Success {
                                    capture.output = Some(output);
                                } else {
                                    capture.failed = true;
                                }
                            }
                        }
                        Some(ExpectedTmuxReply::PaneResyncPendingEscape(pane_id)) => {
                            if let Some(capture) =
                                connection.pending_pane_captures.get_mut(&pane_id)
                                && status == crate::tmux_control::CommandStatus::Success
                            {
                                capture.pending_escape = join_tmux_command_output(&output);
                                capture.parser_continuation_available = true;
                            }
                        }
                        Some(ExpectedTmuxReply::PaneResyncVerify(pane_id)) => {
                            if let Some(flow) = connection.pane_flow.get_mut(&pane_id) {
                                flow.resync_in_flight = false;
                            }
                            let post_metadata = (status
                                == crate::tmux_control::CommandStatus::Success
                                && output.len() == 1)
                                .then(|| {
                                    crate::tmux_model::parse_pane_capture_metadata(
                                        &output[0], pane_id,
                                    )
                                    .ok()
                                })
                                .flatten();
                            let capture = connection.pending_pane_captures.remove(&pane_id);
                            match (capture, post_metadata) {
                                (Some(capture), Some(post_metadata))
                                    if !capture.failed
                                        && capture.output.is_some()
                                        && capture
                                            .metadata
                                            .capture_basis_matches(&post_metadata) =>
                                {
                                    if connection
                                        .topology
                                        .update_pane_capture_metadata(&post_metadata)
                                        .is_ok()
                                    {
                                        pane_resync_success = Some((
                                            post_metadata,
                                            capture.output.expect("capture output checked above"),
                                            capture.pending_escape,
                                            capture.line_flags,
                                            capture.parser_continuation_available,
                                        ));
                                    } else {
                                        pane_resync_failure = Some(pane_id);
                                    }
                                }
                                (Some(capture), Some(_))
                                    if !capture.failed && capture.output.is_some() =>
                                {
                                    pane_resync_raced = Some(pane_id);
                                }
                                _ => pane_resync_failure = Some(pane_id),
                            }
                        }
                        Some(ExpectedTmuxReply::PanePause(pane_id)) => {
                            let pane_is_presented = Self::tmux_pane_is_presented(
                                connection,
                                connection_is_presented,
                                &active_gateway_path,
                                pane_id,
                            );
                            let flow = connection.pane_flow.entry(pane_id).or_default();
                            flow.pause_requested = false;
                            if status == crate::tmux_control::CommandStatus::Error {
                                flow.is_paused = false;
                                if flow.final_resync_requested {
                                    flow.final_resync_requested = false;
                                    flow.status = TmuxFlowStatus::ResyncFailed;
                                    flow.consecutive_resync_failures =
                                        flow.consecutive_resync_failures.saturating_add(1);
                                    flow.resync_failures = flow.resync_failures.saturating_add(1);
                                    let retry_delay = Self::tmux_flow_retry_delay_ms(
                                        flow.consecutive_resync_failures,
                                    );
                                    flow.resync_after_ms = pane_is_presented
                                        .then_some(now_ms.saturating_add(retry_delay));
                                } else if flow.status != TmuxFlowStatus::Running {
                                    Self::mark_tmux_pane_capture_required(flow);
                                    if pane_is_presented && !flow.resync_requested {
                                        pane_resync_request = Some(pane_id);
                                    }
                                }
                            } else {
                                flow.is_paused = true;
                                if flow.final_resync_requested
                                    && pane_is_presented
                                    && !flow.resync_requested
                                {
                                    pane_resync_request = Some(pane_id);
                                }
                            }
                        }
                        Some(ExpectedTmuxReply::PaneContinue(pane_id)) => {
                            let pane_is_presented = Self::tmux_pane_is_presented(
                                connection,
                                connection_is_presented,
                                &active_gateway_path,
                                pane_id,
                            );
                            let flow = connection.pane_flow.entry(pane_id).or_default();
                            flow.resume_requested = false;
                            flow.pause_requested = false;
                            if status == crate::tmux_control::CommandStatus::Success {
                                let capture_preceded_resume = flow.resync_requested;
                                flow.is_paused = false;
                                if flow.status != TmuxFlowStatus::Running {
                                    Self::mark_tmux_pane_capture_required(flow);
                                    if capture_preceded_resume {
                                        Self::schedule_tmux_pane_recapture(flow, now_ms);
                                    } else if pane_is_presented {
                                        pane_resync_request = Some(pane_id);
                                    }
                                }
                            } else if (flow.is_paused || flow.pause_requested)
                                && flow.status != TmuxFlowStatus::Running
                            {
                                Self::mark_tmux_pane_capture_required(flow);
                                flow.consecutive_resync_failures =
                                    flow.consecutive_resync_failures.saturating_add(1);
                                flow.resync_failures = flow.resync_failures.saturating_add(1);
                                let retry_delay = Self::tmux_flow_retry_delay_ms(
                                    flow.consecutive_resync_failures,
                                );
                                flow.resync_after_ms =
                                    pane_is_presented.then_some(now_ms.saturating_add(retry_delay));
                            }
                        }
                        Some(ExpectedTmuxReply::CarrierLeaseCreate {
                            session_id,
                            window_id,
                            index,
                        }) => {
                            carrier_lease_create_reply = Some((
                                session_id,
                                window_id,
                                index,
                                status == crate::tmux_control::CommandStatus::Success,
                            ));
                        }
                        Some(ExpectedTmuxReply::CarrierLeaseVerify {
                            session_id,
                            window_id,
                            index,
                        }) => {
                            carrier_lease_verify_reply = Some((
                                session_id,
                                window_id,
                                index,
                                status == crate::tmux_control::CommandStatus::Success,
                                output,
                            ));
                        }
                        Some(ExpectedTmuxReply::CarrierLeaseUnlink {
                            session_id,
                            window_id,
                            index,
                        }) => {
                            carrier_unlink_reply = Some((
                                session_id,
                                window_id,
                                index,
                                status == crate::tmux_control::CommandStatus::Success,
                            ));
                        }
                        Some(ExpectedTmuxReply::CarrierSessionSwitch { session_id }) => {
                            carrier_session_switch_reply = Some((
                                session_id,
                                status == crate::tmux_control::CommandStatus::Success,
                            ));
                        }
                        Some(ExpectedTmuxReply::CarrierLeaseMove {
                            session_id,
                            window_id,
                            old_index,
                            new_index,
                        }) => {
                            carrier_move_reply = Some((
                                session_id,
                                window_id,
                                old_index,
                                new_index,
                                status == crate::tmux_control::CommandStatus::Success,
                            ));
                        }
                        Some(ExpectedTmuxReply::CarrierLeaseMoveVerify {
                            session_id,
                            window_id,
                            old_index,
                            new_index,
                        }) => {
                            carrier_move_verify_reply = Some((
                                session_id,
                                window_id,
                                old_index,
                                new_index,
                                status == crate::tmux_control::CommandStatus::Success,
                                output,
                            ));
                        }
                        Some(ExpectedTmuxReply::Ignored) => {}
                        Some(ExpectedTmuxReply::UserCommand {
                            description,
                            show_success,
                        }) => {
                            if crate::tmux_prefix::command_may_change_key_configuration(
                                &description,
                            ) && connection.inventory_phase == TmuxInventoryPhase::Idle
                            {
                                connection.topology.mark_resync_required();
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
                    connection.pane_flow.entry(pane_id).or_default();
                    pane_output = Some((pane_id, bytes));
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
                    pane_output = Some((pane_id, bytes));
                }
                crate::tmux_control::ControlEvent::Pause { pane_id } => {
                    let pane_id = crate::tmux_model::PaneId(pane_id);
                    if active_gateway_path.contains(&(connection_id, pane_id)) {
                        // tmux's control-mode pause notification means bytes
                        // for this client were discarded. A pane capture can
                        // repair an ordinary terminal, but never a nested
                        // control protocol stream.
                        lost_gateway_carrier = Some(pane_id);
                    } else {
                        let pane_is_presented = Self::tmux_pane_is_presented(
                            connection,
                            connection_is_presented,
                            &active_gateway_path,
                            pane_id,
                        );
                        let flow = connection.pane_flow.entry(pane_id).or_default();
                        flow.pause_requested = false;
                        flow.is_paused = true;
                        if !flow.final_resync_requested {
                            Self::mark_tmux_pane_capture_required(flow);
                        }
                        if pane_is_presented
                            && !flow.final_resync_requested
                            && !flow.resume_requested
                        {
                            flow.resume_requested = true;
                            pane_resume_request = Some(pane_id);
                        }
                    }
                }
                crate::tmux_control::ControlEvent::Continue { pane_id } => {
                    let pane_id = crate::tmux_model::PaneId(pane_id);
                    let pane_is_presented = Self::tmux_pane_is_presented(
                        connection,
                        connection_is_presented,
                        &active_gateway_path,
                        pane_id,
                    );
                    let flow = connection.pane_flow.entry(pane_id).or_default();
                    if flow.final_resync_requested && flow.resync_requested {
                        // This continue is the first command in the final
                        // continue-and-capture batch. Keep recovery active;
                        // the capture reply establishes the stream boundary.
                        flow.pause_requested = false;
                        flow.is_paused = false;
                        flow.resume_requested = false;
                    } else {
                        let capture_preceded_resume = flow.resync_requested;
                        flow.pause_requested = false;
                        flow.is_paused = false;
                        flow.resume_requested = false;
                        if flow.status != TmuxFlowStatus::Running {
                            Self::mark_tmux_pane_capture_required(flow);
                            if capture_preceded_resume {
                                Self::schedule_tmux_pane_recapture(flow, now_ms);
                            } else if pane_is_presented {
                                pane_resync_request = Some(pane_id);
                            }
                        }
                    }
                }
                crate::tmux_control::ControlEvent::Notification { name, arguments } => {
                    if matches!(name.as_slice(), b"message" | b"config-error") {
                        notification_popup = Some((
                            name == b"config-error",
                            String::from_utf8_lossy(&arguments).into_owned(),
                        ));
                    } else {
                        let inventory_phase = connection.inventory_phase;
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
                        if inventory_phase == TmuxInventoryPhase::InFlight
                            && outcome != crate::tmux_model::ReconcileOutcome::Ignored
                        {
                            // Even an incrementally applicable notification is
                            // newer than some unknown subset of the current
                            // multi-command inventory. Do not let that older
                            // generation overwrite the applied rename, focus,
                            // pane exit, or other topology change.
                            connection.inventory_invalidated = true;
                        }
                        if outcome == crate::tmux_model::ReconcileOutcome::ResyncRequired
                            && inventory_phase == TmuxInventoryPhase::Idle
                        {
                            connection.inventory_retry_count = 0;
                            request_resync = true;
                        }
                        sync_topology = connection.has_inventory
                            && outcome == crate::tmux_model::ReconcileOutcome::Applied;
                        render_topology = sync_topology;
                    }
                }
                _ => {}
            }
            location_changed =
                sync_topology && previous_location != connection.topology.attached_location();
            attached_session_changed = had_inventory
                && sync_topology
                && previous_attached_session != connection.topology.attached_session();
            if attached_session_changed {
                // A control client receives pane output only for its attached
                // session. Every pane in the session being entered may
                // therefore have advanced since Lector last observed it.
                // Mark the complete session stale: sync_tmux_panes queues an
                // authoritative capture for its visible panes now and leaves
                // hidden windows to be captured when they are presented.
                let attached_windows = connection
                    .topology
                    .attached_session()
                    .and_then(|session_id| connection.topology.session(session_id))
                    .map(|session| session.windows.values().copied().collect::<BTreeSet<_>>())
                    .unwrap_or_default();
                let stale_panes = connection
                    .topology
                    .panes()
                    .values()
                    .filter(|pane| attached_windows.contains(&pane.window_id))
                    .filter(|pane| !active_gateway_path.contains(&(connection_id, pane.id)))
                    .map(|pane| pane.id)
                    .collect::<Vec<_>>();
                for pane_id in stale_panes {
                    let flow = connection.pane_flow.entry(pane_id).or_default();
                    Self::mark_tmux_pane_capture_required(flow);
                }
            }
        }
        if let Some((session_id, window_id, index, success)) = carrier_lease_create_reply {
            self.handle_tmux_carrier_lease_create_reply(
                connection_id,
                session_id,
                window_id,
                index,
                success,
            );
        }
        if let Some((session_id, window_id, index, success, output)) = carrier_lease_verify_reply {
            self.handle_tmux_carrier_lease_verify_reply(
                connection_id,
                session_id,
                window_id,
                index,
                success,
                &output,
            );
        }
        if let Some((session_id, window_id, index, success)) = carrier_unlink_reply {
            self.handle_tmux_carrier_unlink_reply(
                connection_id,
                session_id,
                window_id,
                index,
                success,
            );
        }
        if let Some((session_id, success)) = carrier_session_switch_reply {
            self.handle_tmux_carrier_session_switch_reply(connection_id, session_id, success);
        }
        if let Some((session_id, window_id, old_index, new_index, success)) = carrier_move_reply {
            self.handle_tmux_carrier_move_reply(
                connection_id,
                session_id,
                window_id,
                old_index,
                new_index,
                success,
            );
        }
        if let Some((session_id, window_id, old_index, new_index, success, output)) =
            carrier_move_verify_reply
        {
            self.handle_tmux_carrier_move_verify_reply(
                connection_id,
                (session_id, window_id, old_index, new_index),
                success,
                &output,
            );
        }
        if inventory_applied {
            self.reconcile_tmux_carrier_leases(connection_id);
        }
        if attached_session_changed
            && let Some(session_id) = self
                .tmux_connections
                .iter()
                .find(|connection| connection.id == connection_id)
                .and_then(|connection| connection.topology.attached_session())
        {
            self.observe_tmux_carrier_session_change(connection_id, session_id);
        }
        if let Some(pane_id) = lost_gateway_carrier {
            let lost_connections = self
                .tmux_connections
                .iter()
                .filter_map(
                    |connection| match self.tmux_hierarchy.origin(connection.id) {
                        Some(GatewayOrigin::Pane {
                            parent_connection_id,
                            pane_id: candidate_pane,
                            ..
                        }) if parent_connection_id == connection_id
                            && candidate_pane == pane_id.0 =>
                        {
                            Some(connection.id)
                        }
                        _ => None,
                    },
                )
                .collect::<Vec<_>>();
            for lost_connection_id in lost_connections {
                let reason = crate::tmux_gateway::GatewayFailure::Protocol(
                    "tmux discarded bytes from the pane carrying this nested control connection"
                        .to_owned(),
                );
                self.fail_tmux_connection(sr, lost_connection_id, &reason, term_out)?;
            }
        }
        let connection_is_user_visible = self.active_tmux_connection == Some(connection_id)
            && self
                .view_stack
                .tmux_connection_mut(connection_id)
                .is_some_and(|view| !view.is_showing_connection_portal());
        if sync_topology
            && connection_is_user_visible
            && let Some(connection) = self
                .tmux_connections
                .iter_mut()
                .find(|connection| connection.id == connection_id)
        {
            connection.preferred_location = connection.topology.attached_location();
        }
        if let Some(window_id) = self
            .tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id)
            .and_then(|connection| connection.topology.attached_location())
            .map(|location| location.window_id)
        {
            // Selecting a window acknowledges its previous bell notice.
            // Leaving it again therefore rearms the first background bell.
            self.tmux_background_bell_windows
                .remove(&(connection_id, window_id));
        }
        let mut announce_tmux_location = location_changed;

        if let Some((pane_id, command)) = bootstrap_retry {
            self.pending_tmux_commands.push_back(PendingTmuxCommand {
                connection_id,
                bytes: command,
                expected_replies: vec![ExpectedTmuxReply::Bootstrap {
                    pane_id,
                    line_flags: false,
                }],
                kind: PendingTmuxCommandKind::Ordinary,
            });
        }
        if request_resync {
            self.queue_tmux_inventory(connection_id);
        }
        for pane_id in native_copy_mode_panes {
            self.pending_tmux_commands.push_back(PendingTmuxCommand {
                connection_id,
                bytes: format!("copy-mode -q -t %{}\n", pane_id.0).into_bytes(),
                expected_replies: vec![ExpectedTmuxReply::Ignored],
                kind: PendingTmuxCommandKind::Ordinary,
            });
        }
        if let Some(metadata) = pane_resync_probe {
            let pane_id = metadata.pane_id;
            let connection = self
                .tmux_connections
                .iter()
                .find(|connection| connection.id == connection_id);
            let resume_before_capture = connection
                .and_then(|connection| connection.pane_flow.get(&pane_id))
                .is_some_and(|flow| flow.final_resync_requested && flow.is_paused);
            let capture_line_flags = connection
                .is_none_or(|connection| connection.capture_line_flags_supported != Some(false));
            let capture_command = if capture_line_flags {
                crate::tmux_panes::capture_command_for_metadata(&metadata)
            } else {
                crate::tmux_panes::portable_capture_command_for_metadata(&metadata)
            };
            let pending_escape_command = crate::tmux_panes::pending_escape_capture_command(pane_id);
            let verification_command = crate::tmux_model::pane_capture_metadata_command(pane_id);
            let mut expected_replies = Vec::new();
            let commands = if resume_before_capture {
                // tmux discards this control client's pane output while paused
                // and resets its stream offset on continue. Sending continue
                // and capture as one parsed tmux command sequence makes the
                // snapshot the exact boundary: paused output is represented by
                // the capture, and later output is delivered incrementally.
                expected_replies.push(ExpectedTmuxReply::PaneResyncContinue(pane_id));
                join_tmux_command_sequence(&[
                    crate::tmux_input::continue_pane_command(pane_id),
                    capture_command,
                    pending_escape_command,
                    verification_command,
                ])
            } else {
                [
                    capture_command,
                    pending_escape_command,
                    verification_command,
                ]
                .concat()
            };
            expected_replies.extend([
                ExpectedTmuxReply::PaneResyncCapture(pane_id),
                ExpectedTmuxReply::PaneResyncPendingEscape(pane_id),
                ExpectedTmuxReply::PaneResyncVerify(pane_id),
            ]);
            self.pending_tmux_commands.push_back(PendingTmuxCommand {
                connection_id,
                bytes: commands,
                expected_replies,
                kind: PendingTmuxCommandKind::Ordinary,
            });
        }
        if let Some((had_inventory, detail)) = inventory_terminal_failure {
            if had_inventory {
                notification_popup = Some((
                    true,
                    format!(
                        "tmux inventory refresh failed; continuing with the last valid state: {detail}"
                    ),
                ));
            } else {
                if let Some(view) = self.view_stack.tmux_connection_mut(connection_id) {
                    view.set_inventory_error(&detail);
                }
                let is_presented = self
                    .view_stack
                    .presented_tmux_connection_mut()
                    .is_some_and(|view| view.connection_id() == connection_id);
                if is_presented {
                    self.render_active_view(term_out)?;
                    sr.speak(
                        &format!("tmux connection could not become ready: {detail}"),
                        true,
                    )?;
                }
            }
        }
        if let Some(pane_id) = pane_resume_request {
            self.pending_tmux_commands.push_back(PendingTmuxCommand {
                connection_id,
                bytes: crate::tmux_input::continue_pane_command(pane_id),
                expected_replies: vec![ExpectedTmuxReply::PaneContinue(pane_id)],
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
        let tmux_review_source_ready = self
            .view_stack
            .active_tmux_connection_mut()
            .is_some_and(|view| view.connection_id() == connection_id && view.is_ready());
        if open_review_for_native_copy_mode
            && tmux_review_source_ready
            && !self.view_stack.has_overlay()
        {
            self.open_review(sr, false, term_out)?;
        }
        if let Some((pane_id, status, output, line_flags)) = bootstrap_reply {
            self.discard_deferred_tmux_pane_output((connection_id, pane_id));
            let bootstrap_replies =
                if let Some(view) = self.view_stack.tmux_connection_mut(connection_id) {
                    let was_ready = view.is_ready();
                    let replies = view.apply_bootstrap_with_line_flags(
                        pane_id,
                        status,
                        &output,
                        line_flags,
                        self.clock.now_ms(),
                    )?;
                    let is_ready = view.is_ready() && !view.is_showing_portal();
                    render_topology = is_ready && (!was_ready || view.is_pane_visible(pane_id));
                    announce_tmux_location |= !was_ready && is_ready;
                    replies
                } else {
                    Vec::new()
                };
            self.queue_tmux_pane_report_replies(connection_id, pane_id, &bootstrap_replies);
            let pane_is_presented = self
                .tmux_connections
                .iter()
                .find(|connection| connection.id == connection_id)
                .is_some_and(|connection| {
                    Self::tmux_pane_is_presented(
                        connection,
                        connection_is_presented,
                        &active_gateway_path,
                        pane_id,
                    )
                });
            let mut bootstrap_resync_completed = false;
            if let Some(flow) = self
                .tmux_connections
                .iter_mut()
                .find(|connection| connection.id == connection_id)
                .and_then(|connection| connection.pane_flow.get_mut(&pane_id))
                && matches!(
                    flow.status,
                    TmuxFlowStatus::Resynchronizing | TmuxFlowStatus::ResyncFailed
                )
            {
                flow.resync_requested = false;
                flow.final_resync_requested = false;
                if status == crate::tmux_control::CommandStatus::Error {
                    flow.consecutive_resync_failures =
                        flow.consecutive_resync_failures.saturating_add(1);
                    flow.resync_failures = flow.resync_failures.saturating_add(1);
                }
                if flow.is_paused || flow.pause_requested {
                    // Initial bootstrap can race with tmux pausing the pane.
                    // Whether that bootstrap succeeded or failed, resume first
                    // and take a final authoritative capture afterward.
                    flow.status = TmuxFlowStatus::Resynchronizing;
                    flow.final_resync_requested = false;
                    flow.resync_after_ms = None;
                    flow.recapture_hard_deadline_ms = None;
                    if pane_is_presented && !flow.resume_requested {
                        resume_after_pane_resync = Some(pane_id);
                    }
                } else if status == crate::tmux_control::CommandStatus::Success
                    && flow.resync_after_ms.is_none()
                {
                    flow.status = TmuxFlowStatus::Running;
                    flow.recapture_hard_deadline_ms = None;
                    flow.resync_count = flow.resync_count.saturating_add(1);
                    flow.consecutive_resync_failures = 0;
                    flow.resync_failure_announced = false;
                    bootstrap_resync_completed = true;
                } else if status == crate::tmux_control::CommandStatus::Error {
                    flow.status = TmuxFlowStatus::ResyncFailed;
                    flow.recapture_hard_deadline_ms = None;
                    let retry_delay =
                        Self::tmux_flow_retry_delay_ms(flow.consecutive_resync_failures);
                    flow.resync_after_ms =
                        pane_is_presented.then_some(now_ms.saturating_add(retry_delay));
                } else {
                    flow.status = TmuxFlowStatus::Resynchronizing;
                }
            }
            if bootstrap_resync_completed {
                self.reset_inactive_tmux_pane_gateway(connection_id, pane_id);
            }
        }
        if let Some((metadata, output, pending_escape, line_flags, parser_continuation_available)) =
            pane_resync_success
        {
            let pane_id = metadata.pane_id;
            let (pane_is_present, pane_is_presented) = self
                .tmux_connections
                .iter()
                .find(|connection| connection.id == connection_id)
                .map_or((false, false), |connection| {
                    (
                        connection.topology.pane(pane_id).is_some(),
                        Self::tmux_pane_is_presented(
                            connection,
                            connection_is_presented,
                            &active_gateway_path,
                            pane_id,
                        ),
                    )
                });
            if pane_is_present {
                if let Some(view) = self.view_stack.tmux_connection_mut(connection_id) {
                    view.apply_resync_capture_with_line_flags(
                        &metadata,
                        &output,
                        &pending_escape,
                        line_flags,
                        self.clock.now_ms(),
                    )?;
                    render_topology = view.is_ready() && !view.is_showing_portal();
                }
                let mut resync_completed = false;
                if let Some(flow) = self
                    .tmux_connections
                    .iter_mut()
                    .find(|connection| connection.id == connection_id)
                    .and_then(|connection| connection.pane_flow.get_mut(&pane_id))
                {
                    if !parser_continuation_available {
                        flow.limitations
                            .insert(TmuxResyncLimitation::ParserContinuation);
                    }
                    if flow.is_paused || flow.pause_requested {
                        // Non-final paused captures still need a fresh snapshot
                        // after resuming. A final capture prefixes its capture
                        // batch with continue, so it reaches this point unpaused.
                        flow.status = TmuxFlowStatus::Resynchronizing;
                        flow.resync_requested = false;
                        flow.resync_after_ms = None;
                        flow.recapture_hard_deadline_ms = None;
                        flow.final_resync_requested = false;
                        if pane_is_presented && !flow.resume_requested {
                            resume_after_pane_resync = Some(pane_id);
                        }
                    } else if !flow.final_resync_requested && flow.resync_after_ms.is_some() {
                        // Output arrived after the authoritative capture was
                        // requested. Keep coalescing until the pane is quiet,
                        // or this recovery epoch reaches its hard deadline,
                        // then take one final snapshot.
                        flow.status = TmuxFlowStatus::Resynchronizing;
                        flow.resync_requested = false;
                    } else {
                        Self::complete_tmux_pane_resync(flow);
                        resync_completed = true;
                    }
                }
                if resync_completed {
                    self.reset_inactive_tmux_pane_gateway(connection_id, pane_id);
                    let parser_note = if parser_continuation_available {
                        "terminal parser continuation restored"
                    } else {
                        "terminal parser continuation may be unavailable"
                    };
                    self.log_event(&format!(
                        "tmux connection {connection_id} pane {} resynchronized; text, history, cursor, geometry, screen mode, and {parser_note}; images and some semantic metadata may be unavailable",
                        pane_id.0,
                    ));
                }
            }
        }
        if let Some(pane_id) = pane_resync_raced {
            let pane_is_presented = self
                .tmux_connections
                .iter()
                .find(|connection| connection.id == connection_id)
                .is_some_and(|connection| {
                    Self::tmux_pane_is_presented(
                        connection,
                        connection_is_presented,
                        &active_gateway_path,
                        pane_id,
                    )
                });
            if let Some(flow) = self
                .tmux_connections
                .iter_mut()
                .find(|connection| connection.id == connection_id)
                .and_then(|connection| connection.pane_flow.get_mut(&pane_id))
            {
                flow.status = TmuxFlowStatus::Resynchronizing;
                flow.resync_requested = false;
                flow.final_resync_requested = false;
                flow.resync_after_ms =
                    pane_is_presented.then_some(now_ms.saturating_add(TMUX_RECOVERY_QUIET_MS));
            }
        }
        if let Some(pane_id) = pane_resync_failure {
            let (pane_is_present, pane_is_presented) = self
                .tmux_connections
                .iter()
                .find(|connection| connection.id == connection_id)
                .map_or((false, false), |connection| {
                    (
                        connection.topology.pane(pane_id).is_some(),
                        Self::tmux_pane_is_presented(
                            connection,
                            connection_is_presented,
                            &active_gateway_path,
                            pane_id,
                        ),
                    )
                });
            if pane_is_present {
                let mut announce_failure = false;
                if let Some(flow) = self
                    .tmux_connections
                    .iter_mut()
                    .find(|connection| connection.id == connection_id)
                    .and_then(|connection| connection.pane_flow.get_mut(&pane_id))
                {
                    flow.status = TmuxFlowStatus::ResyncFailed;
                    flow.resync_requested = false;
                    flow.final_resync_requested = false;
                    flow.recapture_hard_deadline_ms = None;
                    flow.consecutive_resync_failures =
                        flow.consecutive_resync_failures.saturating_add(1);
                    flow.resync_failures = flow.resync_failures.saturating_add(1);
                    let retry_delay =
                        Self::tmux_flow_retry_delay_ms(flow.consecutive_resync_failures);
                    flow.resync_after_ms =
                        pane_is_presented.then_some(now_ms.saturating_add(retry_delay));
                    announce_failure = !flow.resync_failure_announced;
                    flow.resync_failure_announced = true;
                }
                if announce_failure {
                    self.log_event(&format!(
                        "tmux connection {connection_id} pane {} resynchronization failed; retrying with backoff",
                        pane_id.0
                    ));
                }
            }
        }
        if let Some(pane_id) = resume_after_pane_resync {
            if let Some(flow) = self
                .tmux_connections
                .iter_mut()
                .find(|connection| connection.id == connection_id)
                .and_then(|connection| connection.pane_flow.get_mut(&pane_id))
            {
                flow.resume_requested = true;
            }
            self.pending_tmux_commands.push_back(PendingTmuxCommand {
                connection_id,
                bytes: crate::tmux_input::continue_pane_command(pane_id),
                expected_replies: vec![ExpectedTmuxReply::PaneContinue(pane_id)],
                kind: PendingTmuxCommandKind::Ordinary,
            });
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
                .filter(|view| {
                    view.connection_id() == connection_id && !view.is_showing_connection_portal()
                })
                .map(|view| view.is_ready());
            match active_state {
                Some(true) => {
                    self.render_tmux_topology_update(term_out)?;
                    if announce_tmux_location {
                        self.announce_tmux_location_change(sr, connection_id)?;
                    }
                }
                Some(false) => {}
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

    fn announce_tmux_location_change(
        &mut self,
        sr: &mut ScreenReader,
        connection_id: u64,
    ) -> Result<()> {
        if !self.accessibility_announcement_ready() {
            if !self.pending_view_announcement {
                self.pending_tmux_location_announcement = Some(connection_id);
            }
            return Ok(());
        }
        if self.pending_tmux_location_announcement == Some(connection_id) {
            self.pending_tmux_location_announcement = None;
        }
        let Some((previous, current)) = self
            .tmux_connections
            .iter_mut()
            .find(|connection| connection.id == connection_id)
            .and_then(|connection| {
                let current = connection.topology.attached_location()?;
                let previous = connection.last_announced_location.replace(current.clone());
                Some((previous, current))
            })
        else {
            return Ok(());
        };
        if previous.as_ref() == Some(&current) {
            return Ok(());
        }
        if previous.as_ref().is_none_or(|previous| {
            previous.session_id != current.session_id
                || previous.window_id != current.window_id
                || previous.pane_id != current.pane_id
        }) {
            self.cancel_stabilization_bursts();
        }
        match previous {
            None => {
                sr.speak(&current.window_name, false)?;
                self.announce_view_contents(sr)?;
            }
            Some(previous) if previous.session_id != current.session_id => {
                sr.speak(&current.session_name, false)?;
                sr.speak(&current.window_name, false)?;
                self.announce_view_contents(sr)?;
            }
            Some(previous) if previous.window_id != current.window_id => {
                sr.speak(&current.window_name, false)?;
                self.announce_view_contents(sr)?;
            }
            Some(previous) if previous.pane_id != current.pane_id => {
                self.announce_view_contents(sr)?;
            }
            // A rename or other label-only topology update stays silent while
            // the user remains in the same tmux session, window, and pane.
            Some(_) => {}
        }
        Ok(())
    }

    fn process_or_defer_tmux_pane_output(
        &mut self,
        sr: &mut ScreenReader,
        connection_id: u64,
        pane_id: crate::tmux_model::PaneId,
        bytes: &[u8],
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let key = (connection_id, pane_id);
        let pane_is_transport_critical = self.is_live_tmux_gateway_carrier(connection_id, pane_id);
        let pane_is_visible = pane_is_transport_critical
            || self
                .view_stack
                .presented_tmux_connection_mut()
                .is_some_and(|view| {
                    view.connection_id() == connection_id
                        && !view.is_showing_portal()
                        && view.is_pane_visible(pane_id)
                });

        // The outer PTY drain bounds foreground work by bytes and wall-clock
        // time, and the presentation batch composes its final state only once.
        // A second, smaller pane-local budget can discard ordinary foreground
        // output and pause the pane without a notification that guarantees a
        // matching resume. Model visible bytes immediately within the outer
        // bounded turn.
        if pane_is_visible {
            return self.process_tmux_pane_output(sr, connection_id, pane_id, bytes, term_out);
        }

        let has_deferred_output = self.pending_tmux_background_output.contains_key(&key);
        let exceeds_immediate_budget = self
            .tmux_hidden_output_bytes_this_turn
            .saturating_add(bytes.len())
            > TMUX_HIDDEN_IMMEDIATE_BUDGET_BYTES;
        if !has_deferred_output && !exceeds_immediate_budget {
            self.tmux_hidden_output_bytes_this_turn = self
                .tmux_hidden_output_bytes_this_turn
                .saturating_add(bytes.len());
            return self.process_tmux_pane_output(sr, connection_id, pane_id, bytes, term_out);
        }

        self.defer_tmux_pane_output(connection_id, pane_id, bytes);
        Ok(())
    }

    fn discard_stale_tmux_pane_output(
        &mut self,
        connection_id: u64,
        pane_id: crate::tmux_model::PaneId,
        bytes: &[u8],
    ) -> bool {
        // A carrier can be outside the attached session while its child is
        // selected. Its direct terminal bytes are still part of the same pane
        // stream as the nested control protocol, so applying pause-after to
        // that pane would also stop (and eventually lose) child control data.
        let pane_is_transport_critical = self.is_live_tmux_gateway_carrier(connection_id, pane_id);
        let pane_is_visible = pane_is_transport_critical
            || self
                .view_stack
                .presented_tmux_connection_mut()
                .is_some_and(|view| {
                    view.connection_id() == connection_id
                        && !view.is_showing_portal()
                        && view.is_pane_visible(pane_id)
                });
        let pane_needs_resync = self
            .tmux_connections
            .iter_mut()
            .find(|connection| connection.id == connection_id)
            .and_then(|connection| connection.pane_flow.get_mut(&pane_id))
            .is_some_and(|flow| {
                if !matches!(
                    flow.status,
                    TmuxFlowStatus::Resynchronizing | TmuxFlowStatus::ResyncFailed
                ) {
                    return false;
                }
                flow.skipped_incremental_bytes =
                    flow.skipped_incremental_bytes.saturating_add(bytes.len());
                if pane_is_visible
                    && (flow.resync_in_flight
                        || flow.status == TmuxFlowStatus::Resynchronizing
                            && flow.resync_after_ms.is_some())
                {
                    Self::schedule_tmux_pane_recapture(flow, self.clock.now_ms());
                }
                true
            });
        if pane_needs_resync {
            // Once any bytes are missing, the pane parser's starting state is
            // unknowable. Do not interpret even apparently self-contained
            // controls or terminal side effects until an authoritative capture
            // restores the screen and pending parser continuation.
            self.reset_inactive_tmux_pane_gateway(connection_id, pane_id);
            if !pane_is_visible {
                self.queue_tmux_background_pause(connection_id, pane_id);
            }
            return true;
        }
        false
    }

    fn reset_inactive_tmux_pane_gateway(
        &mut self,
        connection_id: u64,
        pane_id: crate::tmux_model::PaneId,
    ) {
        let key = (connection_id, pane_id.0);
        if self
            .nested_tmux_gateways
            .get(&key)
            .is_some_and(|gateway| gateway.active_global_connection_id.is_none())
        {
            self.nested_tmux_gateways
                .insert(key, NestedTmuxGatewayState::new());
        }
    }

    fn mark_tmux_pane_for_resync(
        &mut self,
        connection_id: u64,
        pane_id: crate::tmux_model::PaneId,
        skipped_bytes: usize,
    ) {
        if let Some(flow) = self
            .tmux_connections
            .iter_mut()
            .find(|connection| connection.id == connection_id)
            .and_then(|connection| connection.pane_flow.get_mut(&pane_id))
        {
            flow.status = TmuxFlowStatus::Resynchronizing;
            flow.final_resync_requested = false;
            flow.resync_after_ms = None;
            flow.recapture_hard_deadline_ms = None;
            flow.skipped_incremental_bytes =
                flow.skipped_incremental_bytes.saturating_add(skipped_bytes);
            flow.limitations.extend([
                TmuxResyncLimitation::KittyImages,
                TmuxResyncLimitation::SemanticMetadata,
            ]);
        }
    }

    fn defer_tmux_pane_output(
        &mut self,
        connection_id: u64,
        pane_id: crate::tmux_model::PaneId,
        bytes: &[u8],
    ) {
        let key = (connection_id, pane_id);
        let would_exceed_limit = self
            .pending_tmux_background_bytes
            .saturating_add(bytes.len())
            > TMUX_BACKGROUND_OUTPUT_LIMIT_BYTES;
        if would_exceed_limit {
            let dropped = self.discard_deferred_tmux_pane_output(key);
            self.log_event(&format!(
                "tmux deferred pane output overflow connection={connection_id} pane={} dropped={} rejected={}",
                pane_id.0,
                dropped,
                bytes.len()
            ));
            self.mark_tmux_pane_for_resync(
                connection_id,
                pane_id,
                dropped.saturating_add(bytes.len()),
            );
            return;
        }

        if !self.pending_tmux_background_output.contains_key(&key) {
            self.pending_tmux_background_order.push_back(key);
        }
        self.pending_tmux_background_output
            .entry(key)
            .or_default()
            .extend(bytes);
        self.pending_tmux_background_bytes = self
            .pending_tmux_background_bytes
            .saturating_add(bytes.len());
        if self
            .pending_tmux_background_output
            .get(&key)
            .is_some_and(|queued| queued.len() >= TMUX_BACKGROUND_PAUSE_THRESHOLD_BYTES)
        {
            self.queue_tmux_background_pause(connection_id, pane_id);
        }
    }

    fn queue_tmux_background_pause(
        &mut self,
        connection_id: u64,
        pane_id: crate::tmux_model::PaneId,
    ) {
        let should_pause = self
            .tmux_connections
            .iter_mut()
            .find(|connection| connection.id == connection_id)
            .and_then(|connection| connection.pane_flow.get_mut(&pane_id))
            .is_some_and(|flow| {
                if flow.is_paused || flow.pause_requested {
                    return false;
                }
                flow.pause_requested = true;
                Self::mark_tmux_pane_capture_required(flow);
                true
            });
        if should_pause {
            self.log_event(&format!(
                "pausing overloaded tmux pane connection={connection_id} pane={}",
                pane_id.0
            ));
            self.pending_tmux_commands.push_back(PendingTmuxCommand {
                connection_id,
                bytes: crate::tmux_input::pause_pane_command(pane_id),
                expected_replies: vec![ExpectedTmuxReply::PanePause(pane_id)],
                kind: PendingTmuxCommandKind::Ordinary,
            });
        }
    }

    fn discard_deferred_tmux_pane_output(
        &mut self,
        key: (u64, crate::tmux_model::PaneId),
    ) -> usize {
        let dropped = self
            .pending_tmux_background_output
            .remove(&key)
            .map_or(0, |queued| queued.len());
        self.pending_tmux_background_bytes =
            self.pending_tmux_background_bytes.saturating_sub(dropped);
        self.pending_tmux_background_order
            .retain(|candidate| *candidate != key);
        dropped
    }

    fn discard_deferred_tmux_connections(&mut self, connection_ids: &[u64]) {
        let keys = self
            .pending_tmux_background_output
            .keys()
            .filter(|(connection_id, _)| connection_ids.contains(connection_id))
            .copied()
            .collect::<Vec<_>>();
        for key in keys {
            self.discard_deferred_tmux_pane_output(key);
        }
    }

    fn drain_tmux_background_output(
        &mut self,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let mut remaining = TMUX_BACKGROUND_DRAIN_BUDGET_BYTES;
        while remaining > 0 {
            let Some(key) = self.pending_tmux_background_order.pop_front() else {
                break;
            };
            let Some(queued) = self.pending_tmux_background_output.get_mut(&key) else {
                continue;
            };
            let count = remaining.min(queued.len());
            let bytes = queued.drain(..count).collect::<Vec<_>>();
            let has_more = !queued.is_empty();
            if has_more {
                self.pending_tmux_background_order.push_back(key);
            } else {
                self.pending_tmux_background_output.remove(&key);
            }
            self.pending_tmux_background_bytes =
                self.pending_tmux_background_bytes.saturating_sub(count);
            remaining = remaining.saturating_sub(count);
            if !self.discard_stale_tmux_pane_output(key.0, key.1, &bytes) {
                self.process_tmux_pane_output(sr, key.0, key.1, &bytes, term_out)?;
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
        // A missing prefix can fabricate both terminal controls and Lector's
        // nested-control marker. Establish the pane's loss boundary before
        // either stateful parser sees another byte.
        if self.discard_stale_tmux_pane_output(parent_connection_id, pane_id, bytes) {
            return Ok(());
        }
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
                    self.process_or_defer_tmux_pane_output(
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
                    let connection_id = self.next_available_tmux_connection_id()?;
                    if let Some(gateway) = self.nested_tmux_gateways.get_mut(&key) {
                        gateway.active_local_connection_id = Some(local_connection_id);
                        gateway.active_global_connection_id = Some(connection_id);
                    }
                    let (session_id, window_id) = self
                        .tmux_connections
                        .iter()
                        .find(|connection| connection.id == parent_connection_id)
                        .and_then(|connection| {
                            let pane = connection.topology.pane(pane_id)?;
                            let window = connection.topology.window(pane.window_id)?;
                            let session_id = connection
                                .topology
                                .attached_session()
                                .filter(|session_id| {
                                    connection.topology.session(*session_id).is_some_and(
                                        |session| {
                                            session
                                                .windows
                                                .values()
                                                .any(|candidate| *candidate == pane.window_id)
                                        },
                                    )
                                })
                                .or_else(|| {
                                    window.links.iter().next().map(|link| link.session_id)
                                })?;
                            Some((session_id.0, pane.window_id.0))
                        })
                        .context("nested tmux gateway pane is absent from parent topology")?;
                    self.start_tmux_connection(
                        sr,
                        connection_id,
                        GatewayOrigin::Pane {
                            parent_connection_id,
                            session_id,
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
                        .and_then(|gateway| gateway.active_global_connection_id);
                    if let Some(connection_id) = connection_id {
                        self.end_tmux_connection(sr, connection_id, term_out)?;
                    }
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
                    let recovering = self.nested_tmux_gateways.get(&key).is_some_and(|gateway| {
                        gateway.router.lifecycle_state()
                            == crate::tmux_gateway::GatewayLifecycleState::Recovering
                    });
                    if let Some(connection_id) = connection_id {
                        self.fail_tmux_connection(sr, connection_id, &reason, term_out)?;
                    }
                    if !recovering && let Some(gateway) = self.nested_tmux_gateways.get_mut(&key) {
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
        let mut outcome = self
            .view_stack
            .tmux_connection_mut(connection_id)
            .map(|view| view.process_output(pane_id, bytes, is_active_pane))
            .transpose()?
            .flatten();
        // Current tmux asks a control client to supply default-colour reports.
        // Route only the OSC 10/11 replies through the dedicated report API;
        // every other shadow reply remains tmux-owned and is discarded.
        if let Some(outcome) = &mut outcome {
            let replies = std::mem::take(&mut outcome.update.pty_replies);
            self.queue_tmux_pane_report_replies(connection_id, pane_id, &replies);
        }
        let bells = outcome.as_ref().map_or(0, |outcome| outcome.bells);
        let output_screen = outcome.as_ref().map(|outcome| {
            (
                outcome.update.screen_after,
                outcome.update.screen_before != outcome.update.screen_after,
            )
        });
        let adaptive_quiet_trainable = outcome
            .as_ref()
            .is_some_and(|outcome| adaptive_quiet_is_trainable(&outcome.update));
        let presented_bells = if bells > 0 {
            self.present_tmux_bell(sr, connection_id, pane_id, is_visible, term_out)?
        } else {
            0
        };
        if is_visible && let Some(outcome) = outcome {
            if let Some(batch) = &mut self.pending_presentation_batch {
                batch.push_pane(connection_id, pane_id, outcome.update, presented_bells);
            } else {
                self.render_tmux_pane_update(term_out, pane_id, presented_bells, &outcome.update)?;
            }
            if is_active_pane {
                let now_ms = self.clock.now_ms();
                let view_id = self.view_stack.active_mut().model().view_id();
                let (screen, screen_context_changed) =
                    output_screen.expect("visible output retained its screen identity");
                self.note_pty_update(
                    AccessibilityContext { view_id, screen },
                    now_ms,
                    screen_context_changed,
                    adaptive_quiet_trainable,
                );
            }
        }
        Ok(())
    }

    fn queue_tmux_pane_report_replies(
        &mut self,
        connection_id: u64,
        pane_id: crate::tmux_model::PaneId,
        replies: &[u8],
    ) {
        for command in crate::tmux_input::refresh_client_report_commands(pane_id, replies) {
            self.pending_tmux_commands.push_back(PendingTmuxCommand {
                connection_id,
                bytes: command,
                expected_replies: vec![ExpectedTmuxReply::Ignored],
                kind: PendingTmuxCommandKind::ColorReport(pane_id),
            });
        }
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
        self.discard_deferred_tmux_connections(&removed);
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
            self.sync_tmux_panes(connection_id)?;
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
        let now_ms = self.clock.now_ms();
        let active_gateway_path = self.live_tmux_gateway_carriers();
        let topology = {
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
            connection.topology.clone()
        };
        let attached_window = topology
            .attached_session()
            .and_then(|session_id| topology.session(session_id))
            .and_then(|session| session.active_window);
        let connection_is_presented = self
            .view_stack
            .presented_tmux_connection_mut()
            .is_some_and(|view| view.connection_id() == connection_id && !view.is_showing_portal());
        let pane_is_presented = |pane_id| {
            (connection_is_presented
                && topology
                    .pane(pane_id)
                    .is_some_and(|pane| Some(pane.window_id) == attached_window))
                || active_gateway_path.contains(&(connection_id, pane_id))
        };
        let pane_resume_requests = self
            .tmux_connections
            .iter_mut()
            .find(|connection| connection.id == connection_id)
            .into_iter()
            .flat_map(|connection| connection.pane_flow.iter_mut())
            .filter_map(|(pane_id, flow)| {
                (pane_is_presented(*pane_id)
                    && (flow.is_paused || flow.pause_requested)
                    && !flow.final_resync_requested
                    && !flow.resume_requested)
                    .then(|| {
                        flow.resume_requested = true;
                        *pane_id
                    })
            })
            .collect::<Vec<_>>();

        let obsolete_deferred = self
            .pending_tmux_background_output
            .keys()
            .filter(|(source_connection_id, pane_id)| {
                *source_connection_id == connection_id
                    && (topology.pane(*pane_id).is_none() || pane_is_presented(*pane_id))
            })
            .copied()
            .collect::<Vec<_>>();
        for key @ (_, pane_id) in obsolete_deferred {
            let dropped = self.discard_deferred_tmux_pane_output(key);
            if dropped == 0 || topology.pane(pane_id).is_none() {
                continue;
            }
            if let Some(flow) = self
                .tmux_connections
                .iter_mut()
                .find(|connection| connection.id == connection_id)
                .and_then(|connection| connection.pane_flow.get_mut(&pane_id))
            {
                flow.status = TmuxFlowStatus::Resynchronizing;
                if flow.resync_in_flight {
                    Self::schedule_tmux_pane_recapture(flow, now_ms);
                } else if !flow.resync_requested {
                    flow.resync_after_ms = None;
                    flow.recapture_hard_deadline_ms = None;
                }
                flow.skipped_incremental_bytes =
                    flow.skipped_incremental_bytes.saturating_add(dropped);
                flow.limitations.extend([
                    TmuxResyncLimitation::KittyImages,
                    TmuxResyncLimitation::SemanticMetadata,
                ]);
            }
        }
        self.recent_tmux_bells
            .retain(|(source_connection, pane_id), _| {
                *source_connection != connection_id || topology.pane(*pane_id).is_some()
            });
        self.tmux_background_bell_windows
            .retain(|(source_connection, window_id)| {
                *source_connection != connection_id || topology.window(*window_id).is_some()
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
        let bootstrap_panes = requests
            .iter()
            .map(|request| request.pane_id)
            .collect::<BTreeSet<_>>();
        let pane_resync_requests = self
            .tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id)
            .into_iter()
            .flat_map(|connection| connection.pane_flow.iter())
            .filter_map(|(pane_id, flow)| {
                (pane_is_presented(*pane_id)
                    && matches!(
                        flow.status,
                        TmuxFlowStatus::Resynchronizing | TmuxFlowStatus::ResyncFailed
                    )
                    && (!flow.is_paused || flow.final_resync_requested)
                    && !flow.pause_requested
                    && !flow.resync_requested
                    && flow
                        .resync_after_ms
                        .is_none_or(|deadline| deadline <= now_ms)
                    && !bootstrap_panes.contains(pane_id))
                .then_some(*pane_id)
            })
            .collect::<Vec<_>>();
        for pane_id in pane_resume_requests {
            self.pending_tmux_commands.push_back(PendingTmuxCommand {
                connection_id,
                bytes: crate::tmux_input::continue_pane_command(pane_id),
                expected_replies: vec![ExpectedTmuxReply::PaneContinue(pane_id)],
                kind: PendingTmuxCommandKind::Ordinary,
            });
        }
        let capture_line_flags = self
            .tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id)
            .is_none_or(|connection| connection.capture_line_flags_supported != Some(false));
        for mut request in requests {
            if let Some(flow) = self
                .tmux_connections
                .iter_mut()
                .find(|connection| connection.id == connection_id)
                .and_then(|connection| connection.pane_flow.get_mut(&request.pane_id))
                && flow.status == TmuxFlowStatus::Resynchronizing
            {
                flow.resync_requested = true;
            }
            if !capture_line_flags && let Some(pane) = topology.pane(request.pane_id) {
                request.command = crate::tmux_panes::portable_capture_command(pane);
            }
            self.pending_tmux_commands.push_back(PendingTmuxCommand {
                connection_id,
                bytes: request.command,
                expected_replies: vec![ExpectedTmuxReply::Bootstrap {
                    pane_id: request.pane_id,
                    line_flags: capture_line_flags,
                }],
                kind: PendingTmuxCommandKind::Ordinary,
            });
        }
        for pane_id in pane_resync_requests {
            self.queue_tmux_pane_resync(connection_id, pane_id)?;
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
        let now_ms = self.clock.now_ms();
        let pane_exists = self
            .tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id)
            .is_some_and(|connection| connection.topology.pane(pane_id).is_some());
        if !pane_exists {
            anyhow::bail!("tmux pane resync target is unavailable");
        }
        let mut pause_for_final_capture = false;
        if let Some(flow) = self
            .tmux_connections
            .iter_mut()
            .find(|connection| connection.id == connection_id)
            .and_then(|connection| connection.pane_flow.get_mut(&pane_id))
        {
            let final_round = flow.final_resync_requested
                || flow
                    .recapture_hard_deadline_ms
                    .is_some_and(|deadline| deadline <= now_ms);
            flow.status = TmuxFlowStatus::Resynchronizing;
            flow.resync_in_flight = false;
            flow.final_resync_requested = final_round;
            flow.resync_after_ms = None;
            if final_round {
                flow.recapture_hard_deadline_ms = None;
            }
            if final_round && !flow.is_paused {
                flow.pause_requested = true;
                flow.resync_requested = false;
                pause_for_final_capture = true;
            } else {
                flow.resync_requested = true;
            }
        }
        if pause_for_final_capture {
            self.pending_tmux_commands.push_back(PendingTmuxCommand {
                connection_id,
                bytes: crate::tmux_input::pause_pane_command(pane_id),
                expected_replies: vec![ExpectedTmuxReply::PanePause(pane_id)],
                kind: PendingTmuxCommandKind::Ordinary,
            });
            return Ok(());
        }
        self.pending_tmux_commands.push_back(PendingTmuxCommand {
            connection_id,
            bytes: crate::tmux_model::pane_capture_metadata_command(pane_id),
            expected_replies: vec![ExpectedTmuxReply::PaneResyncProbe(pane_id)],
            kind: PendingTmuxCommandKind::Ordinary,
        });
        Ok(())
    }

    pub(super) fn queue_tmux_inventory(&mut self, connection_id: u64) {
        let Some(connection) = self
            .tmux_connections
            .iter_mut()
            .find(|connection| connection.id == connection_id)
        else {
            return;
        };
        if !connection.begin_inventory_attempt() {
            // A request made while tmux is already producing a generation
            // cannot be satisfied by assuming that generation is new enough.
            // Invalidate it; completion will queue exactly one successor.
            if connection.inventory_phase == TmuxInventoryPhase::InFlight {
                connection.inventory_invalidated = true;
            }
            return;
        }
        for command in crate::tmux_model::INVENTORY_COMMANDS {
            self.pending_tmux_commands.push_back(PendingTmuxCommand {
                connection_id,
                bytes: command.as_bytes().to_vec(),
                expected_replies: vec![ExpectedTmuxReply::Inventory],
                kind: PendingTmuxCommandKind::Ordinary,
            });
        }
    }

    pub(super) fn queue_tmux_resize(
        &mut self,
        connection_id: u64,
        geometry: crate::terminal::TerminalGeometry,
    ) {
        if let Some(command) = self.pending_tmux_commands.iter_mut().find(|command| {
            command.connection_id == connection_id && command.kind == PendingTmuxCommandKind::Resize
        }) {
            command.bytes = crate::tmux_input::refresh_client_command(geometry);
            return;
        }
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
        match self.classify_tmux_session_change(connection_id, command) {
            TmuxSessionChange::Target(session_id) => {
                anyhow::ensure!(
                    self.queue_tmux_session_switch(connection_id, session_id, Vec::new()),
                    "tmux session switch is already in progress"
                );
                return Ok(());
            }
            TmuxSessionChange::Detach | TmuxSessionChange::Unsafe => {
                anyhow::bail!(
                    "tmux session-changing command must use Lector's nested-safe session or detach path"
                );
            }
            TmuxSessionChange::NotApplicable => {}
        }
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
        self.advance_graceful_tmux_teardown(sr, term_out)?;
        self.queue_due_tmux_pane_resyncs()?;
        self.flush_deferred_kitty_releases(pty_out)?;
        self.drain_direct_gateway_input(pty_out)?;
        if let Some(connection_id) = self.tmux_gateway.active_connection() {
            self.drain_tmux_commands_for(connection_id, pty_out)?;
        }
        self.expire_tmux_force_abandon(sr, term_out)?;
        let released_probe_input = self
            .startup_probe_broker
            .as_mut()
            .map(|broker| broker.finish_if_timed_out(self.clock.now_ms()))
            .unwrap_or_default();
        self.refresh_probed_profile();
        if !released_probe_input.is_empty() {
            self.handle_filtered_terminal_input(sr, &released_probe_input, pty_out, term_out)?;
        }
        self.flush_application_replies(pty_out)?;
        self.flush_pending_input(sr, pty_out, term_out)?;
        let tick_action = self.view_stack.active_mut().tick(sr, pty_out)?;
        self.handle_view_action(sr, tick_action, term_out)?;
        self.drain_tmux_background_output(sr, term_out)?;
        if self.pending_view_announcement && self.accessibility_announcement_ready() {
            self.announce_view_change(sr)?;
        } else if let Some(connection_id) = self.pending_tmux_location_announcement
            && self.accessibility_announcement_ready()
        {
            self.announce_tmux_location_change(sr, connection_id)?;
        }
        if let Some(target) = self.pending_active_view_read {
            let logical = self.view_stack.logical_active_view_id();
            if target != logical {
                self.pending_active_view_read = None;
            } else if self.presented_accessibility_view == Some(target)
                && self.logical_accessibility_view_is_presented()
            {
                self.pending_active_view_read = None;
                self.read_active_view_changes(sr)?;
            }
        }
        self.tmux_hidden_output_bytes_this_turn = 0;
        self.flush_pending_clipboard_writes(sr, term_out)
    }

    fn expire_tmux_force_abandon(
        &mut self,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let now_ms = self.clock.now_ms();
        let Some(pending) = self
            .pending_force_abandon
            .as_ref()
            .filter(|pending| now_ms >= pending.deadline_ms)
        else {
            return Ok(());
        };
        let connection_id = pending.connection_id;
        self.pending_force_abandon = None;
        let Some(origin) = self.tmux_hierarchy.origin(connection_id) else {
            return Ok(());
        };

        self.log_event(&format!(
            "tmux transport did not exit after Control-backslash; exposing raw channel for connection {connection_id}"
        ));
        match origin {
            GatewayOrigin::Direct => {
                self.tmux_gateway = TmuxGatewayRouter::new();
                self.tmux_termination_deadline_ms = None;
            }
            GatewayOrigin::Pane {
                parent_connection_id,
                pane_id,
                ..
            } => {
                self.nested_tmux_gateways.insert(
                    (parent_connection_id, pane_id),
                    NestedTmuxGatewayState::new(),
                );
            }
        }

        // Only the parser and its presentation state are released. The root
        // PTY or owning parent pane stays alive so the user can type raw tmux
        // commands, SSH escapes, or otherwise recover the transport manually.
        self.view_stack.clear_overlays();
        self.end_tmux_connection(sr, connection_id, term_out)?;
        match origin {
            GatewayOrigin::Direct => sr.speak(
                "tmux control abandoned; raw transport exposed in terminal",
                true,
            )?,
            GatewayOrigin::Pane {
                parent_connection_id,
                pane_id,
                ..
            } => sr.speak(
                &format!(
                    "tmux control abandoned; raw transport exposed in connection {parent_connection_id}, pane percent {pane_id}"
                ),
                true,
            )?,
        };
        Ok(())
    }

    fn queue_due_tmux_pane_resyncs(&mut self) -> Result<()> {
        let active_gateway_path = self.live_tmux_gateway_carriers();
        let presented_connection = self
            .view_stack
            .presented_tmux_connection_mut()
            .filter(|view| !view.is_showing_portal())
            .map(|view| view.connection_id());
        let now_ms = self.clock.now_ms();
        let presented_panes = self
            .tmux_connections
            .iter()
            .flat_map(|connection| {
                let active_gateway_path = &active_gateway_path;
                let attached_window = connection
                    .topology
                    .attached_session()
                    .and_then(|session_id| connection.topology.session(session_id))
                    .and_then(|session| session.active_window);
                connection.pane_flow.keys().filter_map(move |pane_id| {
                    (presented_connection == Some(connection.id)
                        && connection
                            .topology
                            .pane(*pane_id)
                            .is_some_and(|pane| Some(pane.window_id) == attached_window)
                        || active_gateway_path.contains(&(connection.id, *pane_id)))
                    .then_some((connection.id, *pane_id))
                })
            })
            .collect::<BTreeSet<_>>();
        let mut resumes = Vec::new();
        let mut resyncs = Vec::new();
        for connection in &mut self.tmux_connections {
            for (pane_id, flow) in &mut connection.pane_flow {
                if !matches!(
                    flow.status,
                    TmuxFlowStatus::Resynchronizing | TmuxFlowStatus::ResyncFailed
                ) || !flow
                    .resync_after_ms
                    .is_some_and(|deadline| deadline <= now_ms)
                {
                    continue;
                }
                // A hidden pane has no reason to keep a physical retry timer
                // alive. Its stale state itself triggers recovery on reveal.
                if !presented_panes.contains(&(connection.id, *pane_id)) {
                    flow.final_resync_requested = false;
                    flow.resync_after_ms = None;
                    flow.recapture_hard_deadline_ms = None;
                    continue;
                }
                // An overdue recapture marker must survive until the capture
                // it raced completes. It is excluded from poll deadlines
                // while that request is outstanding, so retaining it cannot
                // create a zero-timeout spin.
                if flow.resync_requested {
                    continue;
                }
                if flow.is_paused && !flow.final_resync_requested {
                    flow.final_resync_requested = false;
                    flow.resync_after_ms = None;
                    flow.recapture_hard_deadline_ms = None;
                    if !flow.resume_requested {
                        flow.resume_requested = true;
                        resumes.push((connection.id, *pane_id));
                    }
                } else {
                    resyncs.push((connection.id, *pane_id));
                }
            }
        }
        for (connection_id, pane_id) in resumes {
            self.pending_tmux_commands.push_back(PendingTmuxCommand {
                connection_id,
                bytes: crate::tmux_input::continue_pane_command(pane_id),
                expected_replies: vec![ExpectedTmuxReply::PaneContinue(pane_id)],
                kind: PendingTmuxCommandKind::Ordinary,
            });
        }
        for (connection_id, pane_id) in resyncs {
            self.queue_tmux_pane_resync(connection_id, pane_id)?;
        }
        Ok(())
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
        let color_probe_pending = self
            .startup_probe_broker
            .as_ref()
            .is_some_and(|broker| broker.color_wait_pending(self.clock.now_ms()));
        let mut color_blocked_transports = BTreeSet::new();
        for _ in 0..queued {
            let Some(mut command) = self.pending_tmux_commands.pop_front() else {
                break;
            };
            let direct_transport = self.direct_transport_for(command.connection_id);
            if direct_transport != Some(connection_id) {
                self.pending_tmux_commands.push_back(command);
                continue;
            }
            if color_blocked_transports.contains(&direct_transport) {
                self.pending_tmux_commands.push_back(command);
                continue;
            }
            if color_probe_pending && matches!(command.kind, PendingTmuxCommandKind::ColorReport(_))
            {
                color_blocked_transports.insert(direct_transport);
                self.pending_tmux_commands.push_back(command);
                continue;
            }
            if matches!(command.kind, PendingTmuxCommandKind::ColorReport(_))
                && let Some(colors) = self.physical_profile.virtual_terminal_colors()
            {
                command.bytes =
                    crate::terminal_protocol::rewrite_virtual_color_replies(&command.bytes, colors);
            }
            let rejected_connection_id = command.connection_id;
            let rejected_replies = command.expected_replies.clone();
            let Some((root_connection_id, encoded)) = self.route_tmux_command(command)? else {
                self.recover_discarded_tmux_flow_command(rejected_connection_id, &rejected_replies);
                continue;
            };
            self.mark_tmux_flow_commands_in_flight(rejected_connection_id, &rejected_replies);
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

    fn mark_tmux_flow_commands_in_flight(
        &mut self,
        connection_id: u64,
        expected_replies: &[ExpectedTmuxReply],
    ) {
        let Some(connection) = self
            .tmux_connections
            .iter_mut()
            .find(|connection| connection.id == connection_id)
        else {
            return;
        };
        for expected in expected_replies {
            if matches!(expected, ExpectedTmuxReply::Inventory) {
                connection.inventory_phase = TmuxInventoryPhase::InFlight;
            }
            let pane_id = match expected {
                ExpectedTmuxReply::PaneResyncProbe(pane_id)
                | ExpectedTmuxReply::PaneResyncCapture(pane_id)
                | ExpectedTmuxReply::PaneResyncPendingEscape(pane_id)
                | ExpectedTmuxReply::PaneResyncVerify(pane_id) => pane_id,
                _ => continue,
            };
            if let Some(flow) = connection.pane_flow.get_mut(pane_id) {
                flow.resync_in_flight = true;
            }
        }
    }

    fn recover_discarded_tmux_flow_command(
        &mut self,
        connection_id: u64,
        expected_replies: &[ExpectedTmuxReply],
    ) {
        let now_ms = self.clock.now_ms();
        let Some(connection) = self
            .tmux_connections
            .iter_mut()
            .find(|connection| connection.id == connection_id)
        else {
            return;
        };
        let mut actions = BTreeMap::<_, (bool, bool, bool)>::new();
        for expected in expected_replies {
            let (pane_id, pause, resume, resync) = match expected {
                ExpectedTmuxReply::PanePause(pane_id) => (*pane_id, true, false, false),
                ExpectedTmuxReply::PaneContinue(pane_id) => (*pane_id, false, true, false),
                ExpectedTmuxReply::PaneResyncContinue(pane_id) => (*pane_id, false, true, true),
                ExpectedTmuxReply::PaneResyncProbe(pane_id)
                | ExpectedTmuxReply::PaneResyncCapture(pane_id)
                | ExpectedTmuxReply::PaneResyncPendingEscape(pane_id)
                | ExpectedTmuxReply::PaneResyncVerify(pane_id) => (*pane_id, false, false, true),
                _ => continue,
            };
            let action = actions.entry(pane_id).or_default();
            action.0 |= pause;
            action.1 |= resume;
            action.2 |= resync;
        }
        for (pane_id, (pause, resume, resync)) in actions {
            if resync {
                connection.pending_pane_captures.remove(&pane_id);
            }
            let Some(flow) = connection.pane_flow.get_mut(&pane_id) else {
                continue;
            };
            if pause {
                flow.is_paused = false;
                flow.pause_requested = false;
            }
            if resume {
                flow.resume_requested = false;
            }
            if resync {
                flow.resync_requested = false;
                flow.resync_in_flight = false;
                flow.final_resync_requested = false;
            }
            if flow.status != TmuxFlowStatus::Running {
                flow.recapture_hard_deadline_ms = None;
                flow.consecutive_resync_failures =
                    flow.consecutive_resync_failures.saturating_add(1);
                flow.resync_failures = flow.resync_failures.saturating_add(1);
                let retry_delay = Self::tmux_flow_retry_delay_ms(flow.consecutive_resync_failures);
                flow.resync_after_ms = Some(now_ms.saturating_add(retry_delay));
            }
        }
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
            PendingTmuxCommandKind::Ordinary
            | PendingTmuxCommandKind::Resize
            | PendingTmuxCommandKind::ColorReport(_) => {
                vec![(command.bytes, command.expected_replies)]
            }
        };
        let route_is_bounded = |commands: &[(Vec<u8>, Vec<ExpectedTmuxReply>)]| {
            commands.len() <= TMUX_ROUTED_COMMAND_LIMIT_COUNT
                && commands
                    .iter()
                    .try_fold(0usize, |total, (bytes, _)| total.checked_add(bytes.len()))
                    .is_some_and(|total| total <= TMUX_ROUTED_COMMAND_LIMIT_BYTES)
        };
        if !route_is_bounded(&encoded) {
            self.log_event(&format!(
                "discarded oversized tmux command route connection={connection_id}"
            ));
            return Ok(None);
        }
        let mut reply_additions = Vec::new();
        for _ in 0..=64 {
            let Some(connection_index) = self
                .tmux_connections
                .iter()
                .position(|connection| connection.id == connection_id)
            else {
                return Ok(None);
            };
            reply_additions.push((
                connection_index,
                encoded
                    .iter()
                    .flat_map(|(_, expected_replies)| expected_replies.iter().cloned())
                    .collect::<Vec<_>>(),
            ));
            match self.tmux_hierarchy.origin(connection_id) {
                Some(GatewayOrigin::Direct) => {
                    let reply_backlog_exceeded = reply_additions.iter().any(|(index, _)| {
                        let added = reply_additions
                            .iter()
                            .filter(|(candidate, _)| candidate == index)
                            .map(|(_, replies)| replies.len())
                            .sum::<usize>();
                        self.tmux_connections[*index]
                            .expected_replies
                            .len()
                            .saturating_add(added)
                            > TMUX_EXPECTED_REPLY_LIMIT
                    });
                    if reply_backlog_exceeded {
                        self.log_event(&format!(
                            "discarded tmux command because reply backlog is saturated connection={}",
                            command.connection_id
                        ));
                        return Ok(None);
                    }
                    for (index, expected_replies) in reply_additions {
                        self.tmux_connections[index]
                            .expected_replies
                            .extend(expected_replies);
                    }
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
                    if !route_is_bounded(&encoded) {
                        self.log_event(&format!(
                            "discarded oversized nested tmux command route connection={} parent={parent_connection_id}",
                            command.connection_id
                        ));
                        return Ok(None);
                    }
                    connection_id = parent_connection_id;
                }
                None => return Ok(None),
            }
        }
        anyhow::bail!("tmux connection routing exceeds its nesting-depth bound")
    }

    pub fn maybe_finalize_changes(&mut self, sr: &mut ScreenReader) -> Result<bool> {
        let now_ms = self.clock.now_ms();
        let presentation_tracking = self.output_scheduler.is_some();
        let presented_update = if presentation_tracking {
            self.active_presented_update_status()
        } else {
            PresentedUpdateStatus::default()
        };
        if presentation_tracking && !presented_update.finalization_pending {
            return Ok(false);
        }
        let tmux_base_active = self
            .view_stack
            .active_tmux_connection_mut()
            .is_some_and(|view| view.is_ready() && !view.is_showing_portal());
        let overlay_active = !tmux_base_active && self.view_stack.has_overlay();
        let (accessibility_blocked, update_status) = if presentation_tracking {
            (
                self.application_transaction_blocks_stabilization(
                    presented_update.application_transaction_open,
                ),
                presented_update,
            )
        } else {
            let view = if tmux_base_active {
                self.view_stack.active_mut().model()
            } else {
                self.view_stack.root_mut().model()
            };
            let update = view.accessibility_update_summary();
            let parser_continuation = update.parser_continuation;
            let adaptive_quiet_trainable = adaptive_quiet_is_trainable(update);
            let explicitly_stable = update.synchronized_output_closed
                || (update.semantic_input_boundary
                    && update.screen_after == ScreenIdentity::Primary);
            let context = AccessibilityContext {
                view_id: view.view_id(),
                screen: view.screen().screen,
            };
            (
                view.application_transaction_open(),
                PresentedUpdateStatus {
                    context: Some(context),
                    application_transaction_open: view.application_transaction_open(),
                    explicitly_stable,
                    completes_linear_output_record: view
                        .accessibility_completes_linear_output_record(),
                    prompt_transaction_open: view.accessibility_prompt_transaction_open(),
                    parser_continuation,
                    adaptive_quiet_trainable,
                    ..PresentedUpdateStatus::default()
                },
            )
        };
        let context = update_status
            .context
            .expect("an active accessibility update has a stabilization context");
        let history_focus_presentation = if sr.has_pending_history_navigation() {
            let view = if presentation_tracking {
                self.presented_accessibility_model_mut()
            } else if tmux_base_active {
                self.view_stack.active_mut().model()
            } else {
                self.view_stack.root_mut().model()
            };
            sr.visual_focus_response_presentation_ready(view)
        } else {
            None
        };
        if sr.has_pending_history_navigation() && history_focus_presentation.is_none() {
            sr.clear_pending_history_navigation();
        }
        let Some(burst) = self.stabilization_burst(context) else {
            return Ok(false);
        };
        if !overlay_active
            && !accessibility_blocked
            && !update_status.prompt_transaction_open
            && !update_status.parser_continuation
        {
            let (announced, revision) = {
                let view = if presentation_tracking {
                    self.presented_accessibility_model_mut()
                } else if tmux_base_active {
                    self.view_stack.active_mut().model()
                } else {
                    self.view_stack.root_mut().model()
                };
                let policy = view.application_accessibility_policy();
                let announced = if policy.suppress_cursor_tracking {
                    sr.clear_pending_delete();
                    false
                } else {
                    sr.resolve_confirmed_pending_delete(view)?
                };
                (announced, view.accessibility_revision())
            };
            if announced {
                self.log_latency_stage("delete-announced", || {
                    format!("view_id={} revision={revision:?}", context.view_id.0)
                });
            }
        }
        // Application-declared boundaries are carried by the exact presented
        // frame. The shared decision table therefore sees them only after the
        // scheduler's physical receipt and cannot commit a draw in progress.
        let recent_input = stabilization_input_is_recent(now_ms, self.last_stdin_update);
        let decision = stabilization_decision(now_ms, burst, update_status, accessibility_blocked);
        if let StabilizationDecision::Commit(commit_reason) = decision {
            self.log_latency_stage("accessibility-finalization-start", || {
                format!(
                    "reason={} parser_continuation={} prompt_transaction_open={} diff_delay_ms={}",
                    commit_reason.as_str(),
                    update_status.parser_continuation,
                    update_status.prompt_transaction_open,
                    burst.delay_ms,
                )
            });
            let view = if presentation_tracking {
                self.presented_accessibility_model_mut()
            } else if tmux_base_active {
                self.view_stack.active_mut().model()
            } else {
                self.view_stack.root_mut().model()
            };
            let application_policy = consume_application_accessibility(sr, view, !overlay_active)?;
            if application_policy.suppress_auto_read {
                sr.clear_pending_history_navigation();
            }
            let presented_screen_identity_changed =
                view.prev_screen().screen != view.screen().screen;
            if !overlay_active && presented_screen_identity_changed {
                sr.retain_pending_key_echo_for_screen(view.screen().screen);
                prepare_review_cursor_for_active_context(sr, view)?;
            }
            let mut screen_transition_observed = false;
            view.with_live_screen(|view| -> Result<()> {
                let screen_identity_changed = view.prev_screen().screen != view.screen().screen;
                let screen_transition =
                    screen_identity_changed || view.accessibility_screen_transition_pending();
                screen_transition_observed = screen_transition;
                let screen_transition_stable =
                    screen_transition && view.screen().has_visible_non_whitespace_content();
                if !overlay_active && screen_transition {
                    // A screen identity handoff is a new reading context, not
                    // a whole-screen diff. Input echo acknowledgements cannot
                    // cross that context boundary. Read a settled alternate
                    // screen in full, then only the current line when its
                    // already-heard primary context is restored.
                    if !application_policy.suppress_auto_read {
                        announce_screen_transition(sr, view)?;
                    }
                } else if !overlay_active {
                    let mut read_text = if application_policy.suppress_cursor_tracking {
                        false
                    } else {
                        sr.resolve_pending_delete(view)?
                    };
                    // A shell's OSC 133 B marker can remain attached to the
                    // primary grid while a temporary interface owns that same
                    // screen. First honor an exact, key-caused visual focus
                    // transfer; only then interpret Up/Down as Readline history.
                    let history_waits_for_presentation = history_focus_presentation == Some(false);
                    let visual_focus_read = sr.has_pending_history_navigation()
                        && history_focus_presentation == Some(true)
                        && sr.auto_read_enabled()
                        && !application_policy.suppress_auto_read
                        && recent_input
                        && sr.read_visual_focus_transfer(view)?;
                    let navigation_read = if visual_focus_read {
                        sr.clear_pending_history_navigation();
                        true
                    } else if !application_policy.suppress_auto_read
                        && !history_waits_for_presentation
                        && sr.take_pending_history_navigation()
                    {
                        if let Some(input) = view.active_semantic_input() {
                            sr.speak(if input.is_empty() { "blank" } else { &input }, false)?;
                            true
                        } else if sr.auto_read_enabled() && recent_input {
                            sr.read_history_navigation_logical_line_repaint(view)?
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if navigation_read {
                        read_text = true;
                    } else {
                        if sr.highlight_tracking_enabled() && !application_policy.suppress_auto_read
                        {
                            sr.track_highlighting(view)?;
                        }
                        let auto_read_text =
                            if sr.auto_read_enabled() && !application_policy.suppress_auto_read {
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
                    if recent_input && !read_text && !application_policy.suppress_cursor_tracking {
                        sr.track_cursor(view)?;
                    }
                }

                synchronize_pending_review_cursor(sr, view)?;
                if screen_transition_stable {
                    view.complete_accessibility_screen_transition();
                } else if screen_transition {
                    view.defer_accessibility_screen_transition();
                }
                sr.hook_on_screen_update(view, overlay_active)?;
                view.finalize_changes(now_ms);
                Ok(())
            })?;
            // A completed receipt may still trail a newer parser revision.
            // Give that newer revision its own stabilization window. The
            // scheduler deadline includes this window once its render receipt
            // completes, so no further PTY edge is needed to wake auto-read.
            let parser_is_newer = presentation_tracking
                && self
                    .presented_accessibility_model_mut()
                    .accessibility_awaiting_presentation();
            if parser_is_newer {
                self.rebase_stabilization_burst_deadline(context, now_ms);
            } else {
                self.finish_stabilization_burst(
                    context,
                    now_ms,
                    burst.last_output_ms,
                    commit_reason.trains_adaptive_quiet(update_status),
                    screen_transition_observed,
                );
            }
            self.log_latency_stage("accessibility-finalized", || {
                format!("reason={}", commit_reason.as_str())
            });
            return Ok(true);
        }
        Ok(false)
    }
}

fn join_tmux_command_output(lines: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if index != 0 {
            bytes.push(b'\n');
        }
        bytes.extend_from_slice(line);
    }
    bytes
}

/// Joins complete one-line tmux commands into a single parsed command
/// sequence. Unlike several newline-delimited commands in one write, this
/// cannot be split across control-client read callbacks between commands.
fn join_tmux_command_sequence(commands: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (index, command) in commands.iter().enumerate() {
        let command = command.strip_suffix(b"\n").unwrap_or(command);
        debug_assert!(!command.contains(&b'\n'));
        if index != 0 {
            bytes.extend_from_slice(b" ; ");
        }
        bytes.extend_from_slice(command);
    }
    bytes.push(b'\n');
    bytes
}

#[cfg(test)]
mod tests {
    use super::{App, join_tmux_command_sequence};

    #[test]
    fn tmux_flow_retry_backoff_is_exponential_and_capped() {
        assert_eq!(App::tmux_flow_retry_delay_ms(0), 100);
        assert_eq!(App::tmux_flow_retry_delay_ms(1), 100);
        assert_eq!(App::tmux_flow_retry_delay_ms(2), 200);
        assert_eq!(App::tmux_flow_retry_delay_ms(3), 400);
        assert_eq!(App::tmux_flow_retry_delay_ms(5), 1_600);
        assert_eq!(App::tmux_flow_retry_delay_ms(6), 2_000);
        assert_eq!(App::tmux_flow_retry_delay_ms(u32::MAX), 2_000);
    }

    #[test]
    fn tmux_command_sequence_has_one_control_input_boundary() {
        assert_eq!(
            join_tmux_command_sequence(&[b"one\n".to_vec(), b"two 2\n".to_vec()]),
            b"one ; two 2\n"
        );
    }
}
