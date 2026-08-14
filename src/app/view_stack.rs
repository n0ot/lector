use super::*;

impl App {
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
                self.capture_lua_repl_history();
                if self.view_stack.pop() {
                    self.render_active_view(term_out)?;
                    self.announce_view_change(sr)?;
                    if !self.view_stack.has_overlay() {
                        // Hidden PTY changes have now been rendered and finalized.
                        // Do not let their old stabilization deadline run again.
                        self.first_pty_update = None;
                        self.last_pty_update = None;
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
            views::ViewAction::ActivateTerminal => {
                self.activate_terminal_mode(sr, term_out)?;
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
            views::ViewAction::None => {}
        }
        Ok(())
    }

    pub(super) fn capture_lua_repl_history(&mut self) {
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

    pub(super) fn render_active_view(&mut self, term_out: &mut dyn Write) -> Result<()> {
        self.render_full_scene(term_out, 0)
    }

    pub(super) fn render_full_scene(
        &mut self,
        term_out: &mut dyn Write,
        bell_count: usize,
    ) -> Result<()> {
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
        let damage = terminal_update.map_or(SceneDamage::Full, |update| {
            SceneDamage::from_terminal_update(&scene.panes[0], update, scene.geometry)
        });
        self.render_prepared_scene(term_out, bell_count, terminal_update, scene, damage)
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
        let damage = SceneDamage::from_terminal_update(surface, update, scene.geometry);
        self.render_prepared_scene(term_out, bell_count, Some(update), scene, damage)
    }

    pub(super) fn render_tmux_topology_update(&mut self, term_out: &mut dyn Write) -> Result<()> {
        let scene = self.composed_scene_with_bells(0)?;
        let damage = SceneDamage::regions([crate::presentation::GridRect::new(
            GridPoint::new(0, 0),
            scene.geometry.rows,
            scene.geometry.cols,
        )]);
        self.render_prepared_scene(term_out, 0, None, scene, damage)
    }

    fn render_prepared_scene(
        &mut self,
        term_out: &mut dyn Write,
        bell_count: usize,
        terminal_update: Option<&UpdateSummary>,
        mut scene: Scene,
        mut damage: SceneDamage,
    ) -> Result<()> {
        let scheduled = self.output_scheduler.is_some();
        scene.effects.bell_count = if scheduled { 0 } else { bell_count };
        if scheduled
            && self
                .output_scheduler
                .as_ref()
                .is_some_and(crate::output_scheduler::OutputScheduler::has_render_work)
        {
            // A newer authoritative scene supersedes an unstarted render. If
            // an older render has begun, this full reconstruction follows it
            // without depending on its predicted shadow.
            self.scene_renderer.invalidate();
            damage = SceneDamage::Full;
        }
        let title_effect = (scheduled
            && scene.effects.title.as_deref() != self.presented_scene.title())
        .then(|| {
            crate::terminal::TerminalEvent::TitleChanged(
                scene.effects.title.clone().unwrap_or_default(),
            )
        });
        let working_directory_effect = (scheduled
            && scene.effects.working_directory.as_deref()
                != self.presented_scene.working_directory())
        .then(|| {
            crate::terminal::TerminalEvent::WorkingDirectoryChanged(
                scene.effects.working_directory.clone().unwrap_or_default(),
            )
        });
        let batch = self
            .scene_renderer
            .render(&scene, &damage, &self.presented_scene)?;
        if let Some(scheduler) = &mut self.output_scheduler {
            if let Some(effect) = title_effect {
                scheduler.enqueue_terminal_effect(ROOT_SOURCE, effect, self.clock.now_ms());
            }
            if let Some(effect) = working_directory_effect {
                scheduler.enqueue_terminal_effect(ROOT_SOURCE, effect, self.clock.now_ms());
            }
            if let Some(update) = terminal_update {
                scheduler
                    .set_application_synchronized(update.synchronized_output, self.clock.now_ms());
            } else {
                // Application synchronization batches application damage; it
                // must not freeze a compositor-owned overlay, resize, or
                // other authoritative Lector scene.
                scheduler.set_application_synchronized(false, self.clock.now_ms());
            }
            scheduler.enqueue_render(batch, self.clock.now_ms());
            scheduler.enqueue_bell(bell_count, self.clock.now_ms());
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
        Ok(())
    }

    /// Builds the current logical scene without mutating renderer state.
    pub fn composed_scene(&mut self) -> Result<Scene> {
        self.composed_scene_with_bells(0)
    }

    fn composed_scene_with_bells(&mut self, bell_count: usize) -> Result<Scene> {
        let scheduled = self.output_scheduler.is_some();
        let has_overlay = self.view_stack.has_overlay();
        let root_geometry = self.view_stack.root_mut().model().screen().geometry;
        if let Some(connection) = self.view_stack.presented_tmux_connection_mut()
            && connection.is_ready()
            && !connection.is_showing_portal()
        {
            let mut scene = connection.composed_scene(root_geometry)?;
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
        let mut snapshots = self.view_stack.live_snapshots().into_iter();
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
        self.view_stack.append_live_media(&mut scene)?;
        Ok(scene)
    }

    pub(super) fn announce_view_change(&mut self, sr: &mut ScreenReader) -> Result<()> {
        let title = self.view_stack.active_mut().title().to_string();
        let view = self.view_stack.active_mut().model();
        view.with_live_screen(|view| -> Result<()> {
            if sr.review_follows_screen_cursor()
                && view.review_cursor_position() != view.screen().cursor_position()
            {
                let old = view.review_cursor_position();
                view.follow_application_cursor();
                sr.hook_on_review_cursor_move(old, view.review_cursor_position())?;
            }
            sr.speak(&title, false)?;
            let contents = view.contents_full();
            if contents.trim().is_empty() {
                sr.speak("blank screen", false)?;
            } else {
                sr.speak(&contents, false)?;
            }
            view.finalize_changes(self.clock.now_ms());
            Ok(())
        })
    }

    fn read_active_view_changes(&mut self, sr: &mut ScreenReader) -> Result<()> {
        let now_ms = self.clock.now_ms();
        let overlay_active = self.view_stack.has_overlay();
        let recent_input = self
            .last_stdin_update
            .is_some_and(|lsu| now_ms.saturating_sub(lsu) <= MAX_DIFF_DELAY as u128);
        let view = self.view_stack.active_mut().model();
        view.with_live_screen(|view| -> Result<()> {
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
        })
    }

    pub fn debug_active_view_contents(&mut self) -> String {
        self.view_stack.active_mut().model().contents_full()
    }

    pub fn debug_root_terminal_geometry(&mut self) -> crate::terminal::TerminalGeometry {
        self.view_stack.root_mut().model().screen().geometry
    }
}
