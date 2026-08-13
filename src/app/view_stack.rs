use super::*;

impl App {
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
                    if !self.view_stack.has_overlay() {
                        self.restore_root_screen_selection(term_out)?;
                    }
                    self.render_active_view(term_out)?;
                    self.announce_view_change(sr)?;
                    if !self.view_stack.has_overlay() {
                        // Hidden PTY changes have now been rendered and finalized.
                        // Do not let their old stabilization deadline run again.
                        self.first_pty_update = None;
                        self.last_pty_update = None;
                        self.reporter.reset();
                    }
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

    fn restore_root_screen_selection(&mut self, term_out: &mut dyn Write) -> Result<()> {
        let alternate_screen = self
            .view_stack
            .root_mut()
            .model()
            .screen()
            .alternate_screen();
        if alternate_screen == self.displayed_alternate_screen {
            return Ok(());
        }

        term_out
            .write_all(if alternate_screen {
                b"\x1B[?1049h"
            } else {
                b"\x1B[?1049l"
            })
            .context("restore terminal screen selection")?;
        self.displayed_alternate_screen = alternate_screen;
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

    pub(super) fn render_active_view(&mut self, term_out: &mut dyn Write) -> Result<()> {
        let view = self.view_stack.active_mut().model();
        view.with_live_screen(|view| -> Result<()> {
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
                .write_all(&view.screen().attributes_formatted())
                .context("restore drawing attributes")?;
            term_out
                .write_all(&view.screen().input_mode_formatted())
                .context("render input modes")?;
            term_out.flush().context("flush view render")?;
            Ok(())
        })
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
}
