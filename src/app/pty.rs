use super::*;

impl App {
    pub fn handle_pty(
        &mut self,
        sr: &mut ScreenReader,
        buf: &[u8],
        term_out: &mut dyn Write,
    ) -> Result<()> {
        self.process_pty_output(sr, buf, term_out)
    }

    fn process_pty_output(
        &mut self,
        sr: &mut ScreenReader,
        buf: &[u8],
        term_out: &mut dyn Write,
    ) -> Result<()> {
        self.log_bytes("pty output from app", buf);
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

    pub fn handle_tick(
        &mut self,
        sr: &mut ScreenReader,
        pty_out: &mut dyn Write,
        term_out: &mut dyn Write,
    ) -> Result<()> {
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

    pub fn maybe_finalize_changes(&mut self, sr: &mut ScreenReader) -> Result<bool> {
        let Some(lpu) = self.last_pty_update else {
            return Ok(false);
        };
        let first_pty_update = self.first_pty_update.unwrap_or(lpu);
        let now_ms = self.clock.now_ms();
        let overlay_active = self.view_stack.has_overlay();
        if self
            .view_stack
            .root_mut()
            .model()
            .update_summary()
            .synchronized_output
        {
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
