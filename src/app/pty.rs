use super::*;

impl App {
    pub fn handle_pty(
        &mut self,
        sr: &mut ScreenReader,
        buf: &[u8],
        term_out: &mut dyn Write,
    ) -> Result<()> {
        if matches!(sr.input_mode(), crate::keymap::InputMode::TableSetup) {
            self.deferred_pty_output.extend_from_slice(buf);
            return Ok(());
        }

        if self.deferred_pty_output.is_empty() {
            self.process_pty_output(sr, buf, term_out)?;
        } else {
            let mut merged = std::mem::take(&mut self.deferred_pty_output);
            merged.extend_from_slice(buf);
            let result = self.process_pty_output(sr, &merged, term_out);
            merged.clear();
            self.deferred_pty_output = merged;
            result?;
        }
        Ok(())
    }

    fn process_pty_output(
        &mut self,
        sr: &mut ScreenReader,
        buf: &[u8],
        term_out: &mut dyn Write,
    ) -> Result<()> {
        self.log_bytes("pty output from app", buf);
        let filtered = self.focus_mode.filter_into(
            buf,
            &mut self.filtered_pty_output,
            &mut self.focus_mode_changes,
        );
        for &enabled in &self.focus_mode_changes {
            self.log_event(if enabled {
                "focus mode: app enabled ?1004 passthrough"
            } else {
                "focus mode: app disabled ?1004 passthrough"
            });
        }
        if filtered && self.filtered_pty_output != buf {
            self.log_bytes(
                "pty output after focus filtering",
                &self.filtered_pty_output,
            );
        }
        let overlay_active = self.view_stack.has_overlay();
        self.view_stack.root_mut().handle_pty_output(buf)?;
        if !overlay_active {
            if filtered {
                term_out
                    .write_all(&self.filtered_pty_output)
                    .context("write PTY output")?;
            } else {
                term_out.write_all(buf).context("write PTY output")?;
            }
            term_out.flush().context("flush output")?;
            if sr.auto_read_enabled() {
                self.vte_parser.advance(&mut self.reporter, buf);
            }
        }
        let now_ms = self.clock.now_ms();
        if self.first_pty_update.is_none() {
            self.first_pty_update = Some(now_ms);
        }
        self.last_pty_update = Some(now_ms);
        Ok(())
    }

    pub(super) fn flush_deferred_pty_output(
        &mut self,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        if self.deferred_pty_output.is_empty() {
            return Ok(());
        }
        if matches!(sr.input_mode(), crate::keymap::InputMode::TableSetup) {
            return Ok(());
        }
        let mut deferred = std::mem::take(&mut self.deferred_pty_output);
        let result = self.process_pty_output(sr, &deferred, term_out);
        deferred.clear();
        self.deferred_pty_output = deferred;
        result
    }

    pub fn handle_tick(
        &mut self,
        sr: &mut ScreenReader,
        pty_out: &mut dyn Write,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        self.flush_pending_input(sr, pty_out, term_out)?;
        let tick_action = self.view_stack.active_mut().tick(sr, pty_out)?;
        self.handle_view_action(sr, tick_action, term_out)
    }

    pub fn maybe_finalize_changes(&mut self, sr: &mut ScreenReader) -> Result<bool> {
        let Some(lpu) = self.last_pty_update else {
            return Ok(false);
        };
        let first_pty_update = self.first_pty_update.unwrap_or(lpu);
        let now_ms = self.clock.now_ms();
        let overlay_active = self.view_stack.has_overlay();
        if now_ms.saturating_sub(lpu) >= DIFF_DELAY as u128
            || now_ms.saturating_sub(first_pty_update) >= MAX_DIFF_DELAY as u128
        {
            self.first_pty_update = None;
            self.last_pty_update = None;
            let recent_input = self
                .last_stdin_update
                .is_some_and(|lsu| now_ms.saturating_sub(lsu) <= MAX_DIFF_DELAY as u128);
            let reporter = &mut self.reporter;
            let view = self.view_stack.root_mut().model();
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
                                sr.auto_read_after_input(view, reporter)?
                            } else {
                                sr.auto_read(view, reporter)?
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
