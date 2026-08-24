use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ApplicationPresentationModel {
    Live,
    Committed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct RenderSynchronization {
    opened: bool,
    activity: bool,
    compositor_transition: Option<views::CompositorTransitionToken>,
}

impl App {
    pub(super) fn open_review(
        &mut self,
        sr: &mut ScreenReader,
        page_up: bool,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        if self.view_stack.active_mut().kind() == views::ViewKind::Review {
            sr.speak("Review already open", false)?;
            return Ok(());
        }
        let review = if page_up {
            views::ReviewView::new_page_up(self.presented_accessibility_model_mut())
        } else {
            views::ReviewView::new(self.presented_accessibility_model_mut())
        };
        self.handle_view_action(sr, views::ViewAction::Push(Box::new(review)), term_out)
    }

    pub(super) fn sync_table_setup_layer(
        &mut self,
        mode_before: crate::keymap::InputMode,
        sr: &ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let mode_after = sr.input_mode();
        if mode_before != crate::keymap::InputMode::TableSetup
            && mode_after == crate::keymap::InputMode::TableSetup
        {
            let title = self.view_stack.active_mut().title().to_string();
            let frozen = {
                let active = self.view_stack.active_mut();
                views::ReviewView::new_table_setup(active.model(), title)
            };
            self.view_stack.push(Box::new(frozen));
            self.render_active_view(term_out)?;
        } else if mode_before == crate::keymap::InputMode::TableSetup
            && mode_after != crate::keymap::InputMode::TableSetup
            && self.view_stack.active_mut().kind() == views::ViewKind::TableSetup
        {
            let review_cursor = self
                .view_stack
                .active_mut()
                .model()
                .review_cursor_position();
            self.view_stack.pop();
            self.view_stack
                .active_mut()
                .model()
                .set_review_cursor_position(review_cursor);
            self.render_active_view(term_out)?;
        }
        Ok(())
    }

    pub(super) fn dispatch_to_view(
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
        self.log_latency_stage("input-dispatched", || format!("bytes={}", input.len()));
        self.handle_view_action(sr, action, term_out)
    }

    pub(super) fn handle_view_action(
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
                self.emit_physical_bells(term_out, 1)?;
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
                    if !self.view_stack.has_overlay() {
                        // `announce_view_change` cannot finalize an open
                        // synchronized-output frame. Preserve its stabilization
                        // deadline so the real close still drives one atomic
                        // auto-read/finalization pass.
                        if !self.view_stack.presented_holds_synchronized_output() {
                            self.cancel_stabilization_bursts();
                        }
                    }
                }
            }
            views::ViewAction::PopupResponse(response) => {
                let gateway_confirmation = self.pending_gateway_confirmation.take();
                if let Some(confirmation) = gateway_confirmation {
                    let confirmed = response == views::PopupResponse::Confirmed;
                    self.handle_view_action(sr, views::ViewAction::Pop, term_out)?;
                    if confirmed && !self.accept_gateway_confirmation(confirmation)? {
                        self.show_popup_error(
                            sr,
                            "tmux gateway disappeared",
                            "the confirmed tmux gateway no longer exists; no bytes were sent",
                            term_out,
                        )?;
                    }
                    return Ok(());
                }
                let tmux_confirmation = self.pending_tmux_confirmation.take();
                if let Some(confirmation) = tmux_confirmation {
                    let confirmed = response == views::PopupResponse::Confirmed;
                    let target_exists = self.tmux_confirmation_target_exists(&confirmation);
                    self.handle_view_action(sr, views::ViewAction::Pop, term_out)?;
                    if confirmed && target_exists {
                        self.queue_tmux_user_command(
                            confirmation.connection_id,
                            &confirmation.command,
                        )?;
                    } else if confirmed {
                        self.show_popup_error(
                            sr,
                            "tmux target disappeared",
                            "the confirmed tmux target no longer exists; no command was sent",
                            term_out,
                        )?;
                    }
                } else {
                    self.popup_responses.push_back(response);
                    self.handle_view_action(sr, views::ViewAction::Pop, term_out)?;
                }
            }
            views::ViewAction::ActivateTmuxConnection(connection_id) => {
                if !self.activate_tmux_connection(connection_id, sr, term_out)? {
                    self.emit_physical_bells(term_out, 1)?;
                }
            }
            views::ViewAction::TmuxConnectionControl {
                connection_id,
                action,
            } => {
                if !self.request_tmux_gateway_action_for(connection_id, sr, action, term_out)? {
                    self.emit_physical_bells(term_out, 1)?;
                } else if !action.requires_confirmation() {
                    let description = match action {
                        crate::tmux_lifecycle::GatewayControlAction::GracefulDetach => {
                            "graceful teardown requested"
                        }
                        crate::tmux_lifecycle::GatewayControlAction::ForceAbandon => {
                            unreachable!("confirmation-gated action")
                        }
                    };
                    sr.speak(
                        &format!("{description} for tmux connection {connection_id}"),
                        false,
                    )?;
                }
            }
            views::ViewAction::TmuxConnectionRename {
                connection_id,
                label,
            } => {
                self.handle_tmux_connection_rename(sr, connection_id, label, term_out)?;
            }
            views::ViewAction::TmuxChooserSelect {
                connection_id,
                target,
            } => {
                self.handle_tmux_chooser_selection(sr, connection_id, target, term_out)?;
            }
            views::ViewAction::TmuxCommandSubmit {
                connection_id,
                command,
            } => {
                self.handle_tmux_command_submit(sr, connection_id, command, term_out)?;
            }
            views::ViewAction::TmuxInput {
                connection_id,
                pane_id,
                bytes,
            } => {
                self.queue_tmux_input(connection_id, pane_id, &bytes)?;
                self.last_stdin_update = Some(self.clock.now_ms());
            }
            views::ViewAction::Redraw => {
                self.render_active_view(term_out)?;
                self.read_active_view_changes(sr)?;
            }
            views::ViewAction::RedrawSilently => {
                self.render_active_view(term_out)?;
                let auto_read_enabled = sr.auto_read_enabled();
                sr.set_auto_read_enabled(false);
                sr.suppress_cursor_tracking_once();
                let result = self.read_active_view_changes(sr);
                sr.set_auto_read_enabled(auto_read_enabled);
                result?;
            }
            views::ViewAction::None => {}
        }
        Ok(())
    }

    pub(super) fn render_active_view(&mut self, term_out: &mut dyn Write) -> Result<()> {
        self.render_full_scene(term_out, 0)
    }

    pub(super) fn render_full_scene(
        &mut self,
        term_out: &mut dyn Write,
        bell_count: usize,
    ) -> Result<()> {
        if let Some(batch) = &mut self.pending_presentation_batch {
            batch.require_authoritative_scene(bell_count);
            return Ok(());
        }
        self.render_scene_with_update(term_out, bell_count, None)
    }

    pub(super) fn render_terminal_update(
        &mut self,
        term_out: &mut dyn Write,
        bell_count: usize,
        update: &UpdateSummary,
    ) -> Result<()> {
        self.render_scene_with_update(term_out, bell_count, Some(update))
    }

    fn render_scene_with_update(
        &mut self,
        term_out: &mut dyn Write,
        bell_count: usize,
        terminal_update: Option<&UpdateSummary>,
    ) -> Result<()> {
        let scene = self.composed_scene_with_bells(bell_count)?;
        let compositor_transition = self.view_stack.compositor_transition();
        let damage = if compositor_transition.is_some() {
            SceneDamage::Full
        } else {
            terminal_update.map_or(SceneDamage::Full, |update| {
                SceneDamage::from_terminal_update(&scene.panes[0], update, scene.geometry)
            })
        };
        self.render_prepared_scene(
            term_out,
            bell_count,
            RenderSynchronization {
                opened: terminal_update.is_some_and(|update| update.synchronized_output_opened),
                activity: terminal_update.is_some_and(|update| update.synchronized_output),
                compositor_transition,
            },
            scene,
            damage,
        )
    }

    pub(super) fn render_tmux_pane_update(
        &mut self,
        term_out: &mut dyn Write,
        pane_id: crate::tmux_model::PaneId,
        bell_count: usize,
        update: &UpdateSummary,
    ) -> Result<()> {
        let Some(surface_id) = self
            .view_stack
            .active_tmux_connection_mut()
            .and_then(|view| view.surface_id(pane_id))
        else {
            return Ok(());
        };
        let scene = self.composed_scene_with_bells(bell_count)?;
        let Some(surface) = scene.panes.iter().find(|surface| surface.id == surface_id) else {
            return Ok(());
        };
        let compositor_transition = self.view_stack.compositor_transition();
        let damage = if compositor_transition.is_some() {
            SceneDamage::Full
        } else {
            SceneDamage::from_terminal_update(surface, update, scene.geometry)
        };
        self.render_prepared_scene(
            term_out,
            bell_count,
            RenderSynchronization {
                opened: update.synchronized_output_opened,
                activity: update.synchronized_output,
                compositor_transition,
            },
            scene,
            damage,
        )
    }

    pub(super) fn render_tmux_batched_updates(
        &mut self,
        term_out: &mut dyn Write,
        bell_count: usize,
        updates: impl IntoIterator<Item = PendingPanePresentation>,
    ) -> Result<()> {
        let mut first_surface_update = None;
        let mut additional_surface_updates = Vec::new();
        if let Some(view) = self.view_stack.active_tmux_connection_mut() {
            for pending in updates {
                if pending.connection_id != view.connection_id()
                    || !view.is_pane_visible(pending.pane_id)
                {
                    continue;
                }
                let Some(surface_id) = view.surface_id(pending.pane_id) else {
                    continue;
                };
                let surface_update = (surface_id, pending.update);
                if first_surface_update.is_some() {
                    additional_surface_updates.push(surface_update);
                } else {
                    first_surface_update = Some(surface_update);
                }
            }
        }
        let Some(first_surface_update) = first_surface_update else {
            return Ok(());
        };
        let surface_updates =
            std::iter::once(&first_surface_update).chain(additional_surface_updates.iter());
        let synchronized_output_opened = surface_updates
            .clone()
            .any(|(_, update)| update.synchronized_output_opened);
        let synchronized_output_activity = surface_updates
            .clone()
            .any(|(_, update)| update.synchronized_output);
        let scene = self.composed_scene_with_bells(bell_count)?;
        let compositor_transition = self.view_stack.compositor_transition();
        let damage = if compositor_transition.is_some() {
            SceneDamage::Full
        } else {
            SceneDamage::from_terminal_updates(
                surface_updates.filter_map(|(surface_id, update)| {
                    scene
                        .panes
                        .iter()
                        .find(|surface| surface.id == *surface_id)
                        .map(|surface| (surface, update))
                }),
                scene.geometry,
            )
        };
        self.render_prepared_scene(
            term_out,
            bell_count,
            RenderSynchronization {
                opened: synchronized_output_opened,
                activity: synchronized_output_activity,
                compositor_transition,
            },
            scene,
            damage,
        )
    }

    pub(super) fn render_tmux_topology_update(&mut self, term_out: &mut dyn Write) -> Result<()> {
        if let Some(batch) = &mut self.pending_presentation_batch {
            batch.require_authoritative_scene(0);
            return Ok(());
        }
        let scene = self.composed_scene_with_bells(0)?;
        let compositor_transition = self.view_stack.compositor_transition();
        let damage = if compositor_transition.is_some() {
            SceneDamage::Full
        } else {
            SceneDamage::regions([crate::presentation::GridRect::new(
                GridPoint::new(0, 0),
                scene.geometry.rows,
                scene.geometry.cols,
            )])
        };
        self.render_prepared_scene(
            term_out,
            0,
            RenderSynchronization {
                compositor_transition,
                ..RenderSynchronization::default()
            },
            scene,
            damage,
        )
    }

    fn render_prepared_scene(
        &mut self,
        term_out: &mut dyn Write,
        bell_count: usize,
        synchronization: RenderSynchronization,
        mut scene: Scene,
        mut damage: SceneDamage,
    ) -> Result<()> {
        let scheduled = self.output_scheduler.is_some();
        scene.effects.bell_count = if scheduled { 0 } else { bell_count };
        if scheduled
            && self
                .output_scheduler
                .as_ref()
                .is_some_and(crate::output_scheduler::OutputScheduler::has_started_render_work)
        {
            // If an older render has begun, this full reconstruction follows
            // it without depending on its predicted shadow. An unstarted
            // render is replaceable and was itself computed from the still
            // confirmed scene, so it does not invalidate incremental damage.
            self.scene_renderer.invalidate();
            damage = SceneDamage::Full;
        }
        let presented_application_synchronized =
            self.view_stack.presented_holds_synchronized_output();
        let compositor_overlay_visible = self.view_stack.has_overlay();
        let compositor_bypass_requested =
            compositor_overlay_visible || synchronization.compositor_transition.is_some();
        let compensate_compositor_effects = scheduled
            && (synchronization.compositor_transition.is_some()
                || (compositor_overlay_visible && presented_application_synchronized));
        let title_effect = (scheduled
            && (compensate_compositor_effects
                || scene.effects.title.as_deref() != self.presented_scene.title()))
        .then(|| {
            crate::terminal::TerminalEvent::TitleChanged(
                scene.effects.title.clone().unwrap_or_default(),
            )
        });
        let working_directory_effect = (scheduled
            && (compensate_compositor_effects
                || scene.effects.working_directory.as_deref()
                    != self.presented_scene.working_directory()))
        .then(|| {
            crate::terminal::TerminalEvent::WorkingDirectoryChanged(
                scene.effects.working_directory.clone().unwrap_or_default(),
            )
        });
        let batch = self
            .scene_renderer
            .render(&scene, &damage, &self.presented_scene)?;
        if let Some(scheduler) = &mut self.output_scheduler {
            let application_synchronization_timed_out =
                scheduler.application_synchronization_is_ignored();
            let accessibility = self.view_stack.capture_presentation_bundle(
                !compositor_overlay_visible
                    && (synchronization.compositor_transition.is_none()
                        || !presented_application_synchronized
                        || application_synchronization_timed_out),
            );
            if let Some(effect) = title_effect {
                scheduler.enqueue_terminal_effect(ROOT_SOURCE, effect, self.clock.now_ms());
            }
            if let Some(effect) = working_directory_effect {
                scheduler.enqueue_terminal_effect(ROOT_SOURCE, effect, self.clock.now_ms());
            }
            // Aggregate every currently presented terminal surface. One tmux
            // pane's ordinary update must not release another visible pane's
            // open transaction. An opaque Lector overlay may bypass the hold,
            // but it does not erase the underlying epoch or its timeout.
            scheduler.observe_application_synchronization(
                presented_application_synchronized,
                synchronization.opened,
                synchronization.activity,
                self.clock.now_ms(),
            );
            scheduler.set_application_synchronization_bypassed(compositor_bypass_requested);
            let render_outcome = scheduler.enqueue_render_with_accessibility(
                batch,
                accessibility,
                self.clock.now_ms(),
            );
            let accepted_bypass_generation = (render_outcome
                != crate::output_scheduler::EnqueueOutcome::DroppedForCapacity
                && compositor_bypass_requested
                && presented_application_synchronized)
                .then(|| scheduler.application_synchronization_bypass_generation())
                .flatten();
            let capacity_will_drain = scheduler.pending_bytes() != 0 || scheduler.has_render_work();
            if render_outcome != crate::output_scheduler::EnqueueOutcome::DroppedForCapacity {
                scheduler.enqueue_bell(bell_count, self.clock.now_ms());
            }
            self.prune_retired_accessibility_views();
            if render_outcome == crate::output_scheduler::EnqueueOutcome::DroppedForCapacity {
                self.scene_renderer.invalidate();
                if capacity_will_drain
                    && synchronization.compositor_transition
                        == self.view_stack.compositor_transition()
                    && synchronization.compositor_transition
                        != self.compositor_transition_retry_attempt
                {
                    self.compositor_transition_retry = synchronization.compositor_transition;
                }
                self.log_event("discarded physical render which exceeded the scheduler budget");
            } else {
                if compositor_bypass_requested {
                    self.compositor_transition_bypass_owner =
                        accepted_bypass_generation.zip(synchronization.compositor_transition);
                }
                if let Some(transition) = synchronization.compositor_transition {
                    if self.compositor_transition_retry == Some(transition) {
                        self.compositor_transition_retry = None;
                    }
                    if !presented_application_synchronized {
                        // With no application transaction to hold a
                        // replacement, every subsequent live scene is also
                        // committed. The accepted render therefore completes
                        // the compositor handoff logically even though its
                        // physical flush is asynchronous.
                        self.view_stack.complete_compositor_transition(transition);
                    }
                }
            }
            return Ok(());
        }
        for transaction in &batch.transactions {
            if let Err(error) = term_out.write_all(&transaction.bytes) {
                self.scene_renderer.invalidate();
                return Err(error).context("write terminal scene");
            }
        }
        if let Err(error) = term_out.flush() {
            self.scene_renderer.invalidate();
            return Err(error).context("flush terminal scene");
        }
        self.scene_renderer.confirm(&batch.predicted);
        self.presented_scene = batch.predicted;
        if let Some(transition) = synchronization.compositor_transition {
            self.view_stack.complete_compositor_transition(transition);
        }
        Ok(())
    }

    /// Builds the current logical scene without mutating renderer state.
    pub fn composed_scene(&mut self) -> Result<Scene> {
        self.composed_scene_with_bells(0)
    }

    fn composed_scene_with_bells(&mut self, bell_count: usize) -> Result<Scene> {
        let parser_synchronization_open = self.view_stack.presented_holds_synchronized_output();
        let application_synchronization_held = parser_synchronization_open
            && self
                .output_scheduler
                .as_ref()
                .is_none_or(|scheduler| !scheduler.application_synchronization_is_ignored());
        let application_model = if self.view_stack.has_overlay()
            || (self.view_stack.compositor_transition_pending() && application_synchronization_held)
        {
            ApplicationPresentationModel::Committed
        } else {
            ApplicationPresentationModel::Live
        };
        self.composed_scene_with_bells_and_model(bell_count, application_model)
    }

    fn composed_scene_with_bells_and_model(
        &mut self,
        bell_count: usize,
        application_model: ApplicationPresentationModel,
    ) -> Result<Scene> {
        let scheduled = self.output_scheduler.is_some();
        let has_overlay = self.view_stack.has_overlay();
        let root_geometry = self.view_stack.root_mut().model().live_screen().geometry;
        if let Some(connection) = self.view_stack.presented_tmux_connection_mut()
            && connection.is_ready()
            && !connection.is_showing_portal()
        {
            let mut scene = match application_model {
                ApplicationPresentationModel::Live => connection.composed_scene(root_geometry)?,
                ApplicationPresentationModel::Committed => {
                    connection.composed_committed_scene(root_geometry)?
                }
            };
            if scheduled {
                for pane in &mut scene.panes {
                    pane.snapshot.modes.synchronized_output = false;
                }
            }
            scene.effects.bell_count = bell_count;
            if !has_overlay {
                return Ok(scene);
            }

            let presentation_screen = self.presented_scene.screen();
            let overlay_snapshots = self.view_stack.overlay_snapshots();
            for (index, mut overlay) in overlay_snapshots.into_iter().enumerate() {
                overlay.screen = presentation_screen;
                if scheduled {
                    overlay.modes.synchronized_output = false;
                }
                let id = SurfaceId(u64::try_from(index).unwrap_or(u64::MAX).saturating_add(2));
                scene.overlays.push(SceneOverlay::new(
                    SceneSurface::new(id, GridPoint::new(0, 0), overlay),
                    i32::try_from(index).unwrap_or(i32::MAX),
                ));
                scene.cursor_owner = CursorOwner::Overlay(id);
            }
            if let Some(active) = scene.overlays.last_mut() {
                active.surface.snapshot.modes.focus_reporting = true;
            }
            self.view_stack.append_overlay_media(&mut scene)?;
            return Ok(scene);
        }
        let mut snapshots = match application_model {
            ApplicationPresentationModel::Live => self.view_stack.live_snapshots(),
            ApplicationPresentationModel::Committed => {
                self.view_stack.committed_presentation_snapshots()
            }
        }
        .into_iter();
        let mut root = snapshots
            .next()
            .expect("view stack always contains a root surface");
        let geometry = root.geometry;
        let root_screen = root.screen;
        let presentation_screen = if self.view_stack.has_overlay() {
            self.presented_scene.screen()
        } else {
            root_screen
        };
        let title = root.title.clone();
        let working_directory = root.working_directory.clone();
        if scheduled {
            root.modes.synchronized_output = false;
        }
        let root_id = SurfaceId(1);
        let mut scene = Scene::new(geometry);
        scene
            .panes
            .push(SceneSurface::new(root_id, GridPoint::new(0, 0), root));

        let mut active_id = root_id;
        for (index, mut overlay) in snapshots.enumerate() {
            // An overlay stays on the physical screen that was active when it
            // became visible. A hidden application's primary/alternate-screen
            // transition is presented only after the overlay is dismissed.
            overlay.screen = presentation_screen;
            if scheduled {
                overlay.modes.synchronized_output = false;
            }
            let id = SurfaceId(u64::try_from(index).unwrap_or(u64::MAX).saturating_add(2));
            scene.overlays.push(SceneOverlay::new(
                SceneSurface::new(id, GridPoint::new(0, 0), overlay),
                i32::try_from(index).unwrap_or(i32::MAX),
            ));
            active_id = id;
        }

        // Lector owns outer focus reporting. The application's pane-local
        // mode remains in its source engine and input broker.
        if let Some(active) = if scene.overlays.is_empty() {
            scene.panes.first_mut()
        } else {
            scene
                .overlays
                .last_mut()
                .map(|overlay| &mut overlay.surface)
        } {
            active.snapshot.modes.focus_reporting = true;
        }
        scene.cursor_owner = if scene.overlays.is_empty() {
            CursorOwner::Pane(active_id)
        } else {
            CursorOwner::Overlay(active_id)
        };
        scene.effects.title = title;
        scene.effects.working_directory = working_directory;
        scene.effects.bell_count = bell_count;
        match application_model {
            ApplicationPresentationModel::Live => self.view_stack.append_live_media(&mut scene)?,
            ApplicationPresentationModel::Committed => self
                .view_stack
                .append_committed_presentation_media(&mut scene)?,
        }
        Ok(scene)
    }

    pub(super) fn announce_view_change(&mut self, sr: &mut ScreenReader) -> Result<()> {
        if !self.accessibility_announcement_ready() {
            self.pending_view_announcement = true;
            return Ok(());
        }
        self.pending_view_announcement = false;
        self.announce_active_view(sr, true)
    }

    pub(super) fn announce_view_contents(&mut self, sr: &mut ScreenReader) -> Result<()> {
        if !self.accessibility_announcement_ready() {
            self.pending_view_announcement = true;
            return Ok(());
        }
        self.announce_active_view(sr, false)
    }

    pub(super) fn logical_accessibility_view_is_presented(&mut self) -> bool {
        if self.output_scheduler.is_none() {
            return true;
        }
        let Some(presented) = self.presented_accessibility_view else {
            return false;
        };
        if presented != self.view_stack.logical_active_view_id() {
            return false;
        }
        self.view_stack
            .model_by_id_mut(presented)
            .is_some_and(|view| !view.accessibility_awaiting_presentation())
    }

    pub(super) fn accessibility_announcement_ready(&mut self) -> bool {
        self.pending_presentation_batch.is_none()
            && self.logical_accessibility_view_is_presented()
            && self
                .output_scheduler
                .as_ref()
                .is_none_or(|scheduler| !scheduler.has_render_work())
    }

    fn announce_active_view(&mut self, sr: &mut ScreenReader, announce_title: bool) -> Result<()> {
        self.pending_active_view_read = None;
        let title = if !announce_title {
            None
        } else if self.output_scheduler.is_some() {
            self.presented_accessibility_label.clone()
        } else {
            Some(self.view_stack.active_mut().title().to_string())
        };
        let now_ms = self.clock.now_ms();
        let view = self.view_stack.active_mut().model();
        prepare_review_cursor_for_active_context(sr, view)?;
        view.with_live_screen(|view| -> Result<()> {
            if let Some(title) = &title {
                sr.speak(title, false)?;
            }
            speak_application_cursor_line(sr, view)?;
            view.complete_accessibility_screen_transition();
            view.finalize_changes(now_ms);
            Ok(())
        })
    }

    pub(super) fn read_active_view_changes(&mut self, sr: &mut ScreenReader) -> Result<()> {
        let logical_view = self.view_stack.logical_active_view_id();
        if !self.logical_accessibility_view_is_presented() {
            self.pending_active_view_read = Some(logical_view);
            return Ok(());
        }
        self.pending_active_view_read = None;
        let now_ms = self.clock.now_ms();
        let overlay_active = self.view_stack.has_overlay();
        let recent_input = self
            .last_stdin_update
            .is_some_and(|lsu| now_ms.saturating_sub(lsu) <= MAX_DIFF_DELAY as u128);
        let view = if self.output_scheduler.is_some() {
            self.presented_accessibility_model_mut()
        } else {
            self.view_stack.active_mut().model()
        };
        let presented_screen_identity_changed = view.prev_screen().screen != view.screen().screen;
        if presented_screen_identity_changed {
            sr.retain_pending_key_echo_for_screen(view.screen().screen);
            prepare_review_cursor_for_active_context(sr, view)?;
        }
        view.with_live_screen(|view| -> Result<()> {
            let screen_identity_changed = view.prev_screen().screen != view.screen().screen;
            let screen_transition =
                screen_identity_changed || view.accessibility_screen_transition_pending();
            let screen_transition_stable =
                screen_transition && view.screen().has_visible_non_whitespace_content();
            if screen_transition {
                // Primary and alternate screens are separate accessibility
                // contexts. Do not let an acknowledgement from one context
                // suppress text in the next. A settled alternate screen is a
                // new reading context, while the restored primary screen has
                // already been heard and only needs its current line.
                announce_screen_transition(sr, view)?;
            } else {
                let mut read_text = sr.resolve_pending_delete(view)?;
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
                if recent_input && !read_text {
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
        })
    }

    pub fn debug_active_view_contents(&mut self) -> String {
        self.view_stack.active_mut().model().contents_full()
    }

    pub fn debug_root_terminal_geometry(&mut self) -> crate::terminal::TerminalGeometry {
        self.view_stack.root_mut().model().live_screen().geometry
    }

    pub fn debug_last_render_strategy(&self) -> crate::presentation::RenderStrategy {
        self.scene_renderer.last_strategy()
    }
}
