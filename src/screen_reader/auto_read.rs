use super::{Result, ScreenReader};
use crate::view::View;
use similar::{Algorithm, ChangeTag, TextDiff};

#[derive(Default)]
pub(super) struct AutoReadBuffers {
    diff_text: String,
    graphemes: String,
    live_text: String,
    cursor_line: String,
    interface_state: String,
    interface_state_candidate: String,
    interface_region: String,
    transcript_growth: String,
    transcript_growth_candidate: String,
    lcs: Vec<usize>,
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum DiffState {
    NoChanges,
    OneDeletion,
    Single,
    Multi,
}

impl ScreenReader {
    pub fn auto_read(&mut self, view: &mut View) -> Result<bool> {
        self.auto_read_impl(view, false)
    }

    pub(crate) fn auto_read_after_input(&mut self, view: &mut View) -> Result<bool> {
        self.auto_read_impl(view, true)
    }

    fn auto_read_impl(&mut self, view: &mut View, prefer_cursor: bool) -> Result<bool> {
        // Geometry changes, screen handoffs, and terminal resets introduce a
        // new visible context. No visible row identity survives, so read the
        // new grid in full rather than guessing at an incremental mapping.
        if view.accessibility_requires_screen_reintroduction() {
            return self.speak_application_screen(view);
        }

        // Text entry can move an editor's application cursor between wrapped
        // rows or onto a command line. Classify the pending echo before
        // reporting that physical layout change as indentation. The queued
        // text can still be awaiting its receipt when Enter is the newest key.
        if !(prefer_cursor && (self.has_pending_key_echo() || self.key_echo_stream_active)) {
            self.report_application_cursor_indentation_changes(view)?;
        }
        let update = view.accessibility_update_summary();
        let cursor_moves = update.cursor_operations;
        let line_feed_boundaries = update.line_feed_boundaries;
        let cursor_operations_after_last_line_feed = update.cursor_operations_after_last_line_feed;
        let scrolled = update.scroll_operations > 0;
        let structural_repaint = update.output_report_structural && !scrolled;
        // A history-only lineage gap does not invalidate fixed visible-grid
        // coordinates. It merely makes the retained-document comparison
        // unavailable, so fall back to an explicit visible-grid diff.
        let compare_document =
            view.accessibility_document_changed() && view.accessibility_document_is_continuous();
        if compare_document {
            view.prepare_document_contents_cache();
        }
        let prev_cursor = view.prev_screen().cursor_position();
        let cursor = view.screen().cursor_position();
        let linear_output_report = view
            .accessibility_update_summary()
            .has_linear_output_report();

        if prefer_cursor && self.read_visual_focus_transfer(view)? {
            return Ok(true);
        }

        let mut live_text = std::mem::take(&mut self.auto_read_buffers.live_text);
        view.accessibility_update_summary()
            .printed_text_into(&mut live_text);
        let validated_structural_line_report = {
            let update = view.accessibility_update_summary();
            let report = live_text.trim();
            update.output_report_structural
                && !update.parser_continuation
                && update.screen_before == crate::terminal::ScreenIdentity::Primary
                && update.screen_after == crate::terminal::ScreenIdentity::Primary
                && cursor_moves == 0
                && !live_text.contains('\n')
                && !report.is_empty()
                && cursor_logical_line_matches(
                    view,
                    report,
                    &mut self.auto_read_buffers.cursor_line,
                )
        };
        let readable_print_report = linear_output_report || validated_structural_line_report;
        let completed_linear_record = view.accessibility_completes_linear_output_record();

        // A completed blank record is still an authoritative presentation
        // boundary. Consume any matching echo and report it as handled so a
        // recent Enter does not fall through to cursor tracking or a
        // whitespace-only screen diff.
        if completed_linear_record && live_text.trim().is_empty() {
            self.should_suppress_key_echo(&live_text);
            self.auto_read_buffers.live_text = live_text;
            return Ok(true);
        }

        // Printing a separator into an already-blank cell can advance the
        // application cursor without changing normalized screen contents. It
        // is still an echo acknowledgement and must consume the corresponding
        // queued input character before a later word arrives.
        if !completed_linear_record {
            let presented_contents_unchanged = if compare_document {
                let (previous, current, previous_hashes, current_hashes) =
                    view.document_contents_cached();
                previous_hashes == current_hashes && previous == current
            } else {
                // Populate View's reusable full-screen buffers here instead
                // of allocating throwaway normalized strings. A validated LF
                // record does not need a screen diff at all, so it bypasses
                // this whole-screen scan.
                let (previous, current, previous_hashes, current_hashes) =
                    view.full_contents_cached();
                previous_hashes == current_hashes && previous == current
            };
            if presented_contents_unchanged {
                let suppressed_echo = self.should_suppress_key_echo(&live_text);
                self.auto_read_buffers.live_text = live_text;
                return Ok(suppressed_echo
                    || (prefer_cursor && self.key_echo_stream_active && cursor == prev_cursor));
            }
        }

        let mut live_read_result = None;
        {
            let text = live_text.trim();
            if readable_print_report && (cursor_moves == 0 || scrolled) {
                let mut spoken = false;
                // Match against the verbatim print stream. Trimming is a
                // speech concern; dropping spaces here desynchronizes the
                // input acknowledgement queue at every word boundary.
                let suppress_echo = self.should_suppress_key_echo(&live_text);
                if !suppress_echo
                    && !text.is_empty()
                    && let Some(text) = self.hook_on_live_read(text, cursor_moves, scrolled)?
                    && !text.is_empty()
                {
                    if crate::diagnostics::enabled() {
                        crate::diagnostics::event(
                            "screen-reader",
                            "auto-read-progress",
                            &format!("speaking live text bytes={}", text.len()),
                        );
                    }
                    self.speak(&text, false)?;
                    crate::diagnostics::event(
                        "screen-reader",
                        "auto-read-progress",
                        "finished speaking live text",
                    );
                    spoken = true;
                }
                if suppress_echo || !text.is_empty() {
                    live_read_result = Some(suppress_echo || spoken || !text.is_empty());
                }
            }
        }

        if let Some(result) = live_read_result {
            self.auto_read_buffers.live_text = live_text;
            return Ok(result);
        }
        self.auto_read_buffers.live_text = live_text;

        let mut diff_text = std::mem::take(&mut self.auto_read_buffers.diff_text);
        diff_text.clear();
        let cursor_shape_changed = view.prev_screen().cursor.shape != view.screen().cursor.shape;
        let columns = usize::from(view.screen().size().1.max(1));
        let cursor_changed = cursor != prev_cursor;
        let new_interface_region = if structural_repaint {
            let mut region = std::mem::take(&mut self.auto_read_buffers.interface_region);
            let has_region = collect_new_interface_region(view, &mut region);
            self.auto_read_buffers.interface_region = region;
            has_region
        } else {
            false
        };
        view.prepare_full_contents_cache();
        let (old_text, new_text, prev_hashes, curr_hashes) = view.full_contents_from_cache();

        if !compare_document
            && prev_hashes.len() == curr_hashes.len()
            && prev_hashes == curr_hashes
            && old_text == new_text
        {
            self.auto_read_buffers.diff_text = diff_text;
            // Full-screen applications often close an acknowledged echo with
            // a second synchronized transaction which only restores modes or
            // cursor shape. Treat that receipt as handled while the validated
            // echo stream is active; falling through to cursor tracking would
            // announce a spurious indentation/layout change.
            return Ok(prefer_cursor && self.key_echo_stream_active && cursor == prev_cursor);
        }

        let cursor_row = usize::from(cursor.0);
        let cursor_row_changed = prev_hashes
            .get(cursor_row)
            .zip(curr_hashes.get(cursor_row))
            .is_some_and(|(prev, curr)| prev != curr);
        let prev_cursor_row = usize::from(prev_cursor.0);
        let prev_cursor_row_changed = prev_hashes
            .get(prev_cursor_row)
            .zip(curr_hashes.get(prev_cursor_row))
            .is_some_and(|(prev, curr)| prev != curr);
        let (single_changed_row, changed_row_count) = if prev_hashes.len() == curr_hashes.len() {
            // Presentation backpressure can deliberately discard renderer
            // damage hints while retaining exact before/after snapshots. The
            // snapshots are authoritative for accessibility: restricting this
            // count to `changed_rows` made a full-screen redraw look like zero
            // changed rows whenever those optional hints were unavailable.
            let mut changed_rows = prev_hashes
                .iter()
                .zip(curr_hashes)
                .enumerate()
                .filter_map(|(row, (prev, curr))| (prev != curr).then_some(row as u16));
            let first = changed_rows.next();
            let count = first.is_some() as usize + changed_rows.count();
            match (first, count) {
                (Some(row), 1) => (Some(row), 1),
                _ => (None, count),
            }
        } else {
            (None, usize::MAX)
        };
        let multiple_changed_rows = changed_row_count > 1;
        let interface_repaint = structural_repaint
            && ((line_feed_boundaries == 0
                && (prefer_cursor || multiple_changed_rows || cursor_shape_changed))
                || (line_feed_boundaries > 0 && cursor_operations_after_last_line_feed > 0));
        let transcript_growth_repaint = interface_repaint
            && has_inserted_transcript_lines(old_text, new_text, prev_cursor_row.min(cursor_row));
        let pending_key_echo = self.has_pending_key_echo();
        let pending_key_echo_count = self.pending_key_echo.len();
        let cursor_advanced =
            cursor.0 > prev_cursor.0 || (cursor.0 == prev_cursor.0 && cursor.1 > prev_cursor.1);
        let previous_cursor_offset = usize::from(prev_cursor.0)
            .saturating_mul(columns)
            .saturating_add(usize::from(prev_cursor.1));
        let cursor_offset = usize::from(cursor.0)
            .saturating_mul(columns)
            .saturating_add(usize::from(cursor.1));
        let cursor_matches_initial_echo = cursor_offset >= previous_cursor_offset
            && cursor_offset - previous_cursor_offset <= pending_key_echo_count;
        let input_echo_row = if prefer_cursor
            && pending_key_echo
            && !scrolled
            && multiple_changed_rows
            && (self.key_echo_stream_active || cursor_matches_initial_echo)
        {
            if cursor_row_changed {
                Some(cursor.0)
            } else if cursor.0 > prev_cursor.0 && prev_cursor_row_changed {
                // Autowrap leaves the application cursor on the following
                // blank row. The printable cell was committed on the row the
                // cursor just left, so compare that row for exact echo.
                Some(prev_cursor.0)
            } else {
                None
            }
        } else {
            None
        };
        let input_echo_cursor_row = input_echo_row.is_some();
        let unpainted_echo_candidate = prefer_cursor
            && pending_key_echo
            && self.key_echo_stream_active
            && !scrolled
            && cursor_advanced
            && !cursor_row_changed
            && !prev_cursor_row_changed;
        if unpainted_echo_candidate {
            // A trailing whitespace cell is visually indistinguishable from
            // the blank cell it replaced, although a full-screen editor may
            // repaint its ruler. Cursor advancement alone is ambiguous, so
            // suppress this update only when the terminal's actual print
            // report exactly acknowledges pending input.
            let live_text = std::mem::take(&mut self.auto_read_buffers.live_text);
            let suppress_echo = self.should_suppress_key_echo(&live_text);
            self.auto_read_buffers.live_text = live_text;
            if suppress_echo {
                self.auto_read_buffers.diff_text = diff_text;
                return Ok(true);
            }
        }
        let cursor_row_inline_edit = prefer_cursor
            && cursor_changed
            && !scrolled
            && cursor.0 == prev_cursor.0
            && cursor.1 > prev_cursor.1
            && cursor_row_changed
            && multiple_changed_rows;
        if interface_repaint
            && cursor_shape_changed
            && !cursor_row_changed
            && !prev_cursor_row_changed
        {
            // Cursor shape is application-controlled terminal state. When it
            // changes alongside status-only painting, the compact changed
            // label describes a real interface-mode transition rather than a
            // typed character at the application cursor. Keep any pending
            // echo intact for a later cursor-row receipt.
            let mut state = std::mem::take(&mut self.auto_read_buffers.interface_state);
            let mut candidate =
                std::mem::take(&mut self.auto_read_buffers.interface_state_candidate);
            let has_state = collect_compact_interface_state_change(
                old_text,
                new_text,
                prev_cursor_row,
                cursor_row,
                &mut state,
                &mut candidate,
                &mut self.auto_read_buffers.lcs,
            );
            let state_to_speak = if has_state {
                self.hook_on_live_read(&state, cursor_moves, scrolled)?
            } else {
                None
            };
            self.auto_read_buffers.interface_state = state;
            self.auto_read_buffers.interface_state_candidate = candidate;
            if has_state {
                self.auto_read_buffers.diff_text = diff_text;
                if pending_key_echo {
                    // The application has visibly acknowledged a mode change
                    // without painting at its cursor. Treat that receipt as
                    // the start of a text-entry echo stream, but retain every
                    // queued character until exact cursor-row evidence
                    // acknowledges it.
                    self.key_echo_stream_active = true;
                }
                if let Some(text) = state_to_speak
                    && !text.is_empty()
                {
                    self.speak(&text, false)?;
                }
                return Ok(true);
            }
        }
        if interface_repaint && !input_echo_cursor_row && !cursor_row_inline_edit {
            let mut growth = std::mem::take(&mut self.auto_read_buffers.transcript_growth);
            let mut candidate =
                std::mem::take(&mut self.auto_read_buffers.transcript_growth_candidate);
            let has_growth = collect_extended_transcript_lines(
                old_text,
                new_text,
                prev_cursor_row.min(cursor_row),
                &mut growth,
                &mut candidate,
                &mut self.auto_read_buffers.lcs,
            );
            let suppress_echo = has_growth && self.should_suppress_key_echo(&growth);
            let growth_to_speak = if has_growth && !suppress_echo {
                self.hook_on_live_read(&growth, cursor_moves, scrolled)?
            } else {
                None
            };
            self.auto_read_buffers.transcript_growth = growth;
            self.auto_read_buffers.transcript_growth_candidate = candidate;
            if has_growth {
                self.auto_read_buffers.diff_text = diff_text;
                if let Some(text) = growth_to_speak
                    && !text.is_empty()
                {
                    self.speak(&text, false)?;
                }
                return Ok(true);
            }
        }
        if interface_repaint
            && !transcript_growth_repaint
            && !input_echo_cursor_row
            && !cursor_row_inline_edit
            && !(changed_row_count == 1 && (cursor_row_changed || prev_cursor_row_changed))
            && new_interface_region
        {
            // Cursor-addressed painting without line boundaries describes an
            // interface frame, not line-oriented command output. A bounded
            // group of rows newly populated from blank space is evidence that
            // a new interface or modal opened, so introduce that region in
            // full. Stable replacement-style redraws are otherwise ordinary
            // diffs; recent input and a stationary cursor cannot prove that
            // their changed text is incidental.
            self.auto_read_buffers.diff_text = diff_text;
            self.read_new_interface_region_repaint()?;
            return Ok(true);
        }
        // Full-screen applications commonly redraw a ruler or status line along with an
        // inline edit. Keep the fine-grained insertion diff anchored to the cursor row in
        // that case; otherwise the secondary row makes the update look like unrelated
        // multi-line output and the whole edited line is announced.
        let prefer_inline_cursor_row = input_echo_cursor_row || cursor_row_inline_edit;
        let inline_row = usize::from(input_echo_row.unwrap_or(cursor.0));
        let (diff_old_text, diff_new_text) = if prefer_inline_cursor_row {
            (
                old_text
                    .split_terminator('\n')
                    .nth(inline_row)
                    .unwrap_or(""),
                new_text
                    .split_terminator('\n')
                    .nth(inline_row)
                    .unwrap_or(""),
            )
        } else if compare_document {
            let (previous, current, _, _) = view.document_contents_cached();
            (previous, current)
        } else {
            (old_text, new_text)
        };

        let diff_state = collect_auto_read_diff(
            diff_old_text,
            diff_new_text,
            &mut diff_text,
            &mut self.auto_read_buffers.graphemes,
            &mut self.auto_read_buffers.lcs,
        );

        let cursor_crossed_changed_row = single_changed_row.is_some_and(|row| {
            if cursor.0 > prev_cursor.0 {
                row >= prev_cursor.0 && row < cursor.0
            } else if cursor.0 < prev_cursor.0 {
                row <= prev_cursor.0 && row > cursor.0
            } else {
                false
            }
        });
        let cursor_on_changed_row = prefer_inline_cursor_row
            || single_changed_row.is_some_and(|row| row == prev_cursor.0 || row == cursor.0);
        if prefer_cursor
            && diff_state == DiffState::Single
            && cursor_moves > 0
            && !scrolled
            && cursor_changed
            && !cursor_on_changed_row
            && !cursor_crossed_changed_row
        {
            diff_text.clear();
            self.auto_read_buffers.diff_text = diff_text;
            return Ok(false);
        }

        let suppress_echo = if input_echo_cursor_row {
            self.should_suppress_cursor_row_key_echo(&diff_text)
        } else {
            self.should_suppress_key_echo(&diff_text)
        };
        if suppress_echo {
            self.auto_read_buffers.diff_text = diff_text;
            return Ok(true);
        }
        if input_echo_cursor_row {
            // Selecting the cursor row protects exact editor echoes from a
            // parallel ruler repaint. If the candidate does not match queued
            // input, it was not proven to be an echo: restore the complete
            // stable-frame diff so unrelated output cannot disappear.
            collect_auto_read_diff(
                old_text,
                new_text,
                &mut diff_text,
                &mut self.auto_read_buffers.graphemes,
                &mut self.auto_read_buffers.lcs,
            );
            if self.should_suppress_key_echo(&diff_text) {
                self.auto_read_buffers.diff_text = diff_text;
                return Ok(true);
            }
        }

        let original_nonempty = !diff_text.is_empty();
        if let Some(text) = self.hook_on_live_read(&diff_text, cursor_moves, scrolled)?
            && !text.is_empty()
        {
            self.speak(&text, false)?;
        }
        self.auto_read_buffers.diff_text = diff_text;
        Ok(original_nonempty)
    }

    /// Speak the application cursor's complete soft-wrapped logical line.
    ///
    /// Cursor-addressed interfaces repaint physical rows, but a terminal
    /// autowrap still represents one logical line. Keep this separate from the
    /// ordinary cursor tracker: only a causally matched history response should
    /// widen a physical cursor row into its wrap-connected run.
    pub(crate) fn read_history_navigation_logical_line_repaint(
        &mut self,
        view: &View,
    ) -> Result<bool> {
        // Unlike generic auto-read, an explicit history selection is not a
        // printable key echo. A still-pending typed suffix must not suppress
        // the recalled command which replaced it.
        self.read_application_cursor_logical_line_repaint(view)
    }

    fn read_new_interface_region_repaint(&mut self) -> Result<()> {
        let region = std::mem::take(&mut self.auto_read_buffers.interface_region);
        if !self.should_suppress_key_echo(&region) {
            self.speak(&region, false)?;
        }
        self.auto_read_buffers.interface_region = region;
        Ok(())
    }

    fn read_application_cursor_logical_line_repaint(&mut self, view: &View) -> Result<bool> {
        let update = view.accessibility_update_summary();
        if !update.output_report_structural
            || update.parser_continuation
            || update.scroll_operations > 0
            || update.screen_before != crate::terminal::ScreenIdentity::Primary
            || update.screen_after != crate::terminal::ScreenIdentity::Primary
        {
            return Ok(false);
        }
        let mut line = std::mem::take(&mut self.auto_read_buffers.cursor_line);
        let has_logical_line = collect_application_cursor_logical_line(view, &mut line);
        let handled = if has_logical_line {
            self.speak(&line, false)?;
            true
        } else {
            false
        };
        self.auto_read_buffers.cursor_line = line;
        Ok(handled)
    }
}

const MAX_COMPACT_INTERFACE_STATE_CHARS: usize = 32;
const MIN_NEW_INTERFACE_POPULATED_ROWS: usize = 2;
const MAX_NEW_INTERFACE_REGION_ROWS: usize = 64;
const MAX_NEW_INTERFACE_REGION_CELLS: usize = 8_192;
const MAX_APPLICATION_CURSOR_LOGICAL_LINE_ROWS: usize = 64;
const MAX_APPLICATION_CURSOR_LOGICAL_LINE_CELLS: usize = 8_192;

fn transcript_lines_before_cursor(text: &str, cursor_row: usize) -> impl Iterator<Item = &str> {
    text.split_terminator('\n')
        .take(cursor_row)
        .map(str::trim)
        .filter(|line| !line.is_empty())
}

/// Full-screen chat clients commonly redraw the whole transcript with the
/// hardware cursor left at a prompt below it. Preserved lines followed by new
/// lines above that cursor distinguish transcript growth from a menu whose
/// labels were replaced in place.
fn has_inserted_transcript_lines(old_text: &str, new_text: &str, cursor_row: usize) -> bool {
    let old_count = transcript_lines_before_cursor(old_text, cursor_row).count();
    let new_count = transcript_lines_before_cursor(new_text, cursor_row).count();
    if old_count == 0 || new_count <= old_count {
        return false;
    }

    let mut old_lines = transcript_lines_before_cursor(old_text, cursor_row).peekable();
    let mut target = old_lines.next();
    let mut matched = 0usize;
    let mut skipped_old_line = false;
    for new_line in transcript_lines_before_cursor(new_text, cursor_row) {
        if target.is_some_and(|old_line| old_line == new_line) {
            matched = matched.saturating_add(1);
            target = old_lines.next();
        } else if !skipped_old_line
            && old_lines
                .peek()
                .is_some_and(|old_line| *old_line == new_line)
        {
            // Permit one transient spinner/status line to disappear while
            // stable earlier context remains in order.
            skipped_old_line = true;
            matched = matched.saturating_add(1);
            old_lines.next();
            target = old_lines.next();
        }
    }
    matched > 0 && old_count.saturating_sub(matched) <= 1
}

/// Collect only prefix-preserving line extensions above the prompt cursor.
/// This lets streaming response text bypass cursor-only TUI tracking without
/// also announcing a separately repainted status or ruler row.
fn collect_extended_transcript_lines(
    old_text: &str,
    new_text: &str,
    cursor_row: usize,
    out: &mut String,
    candidate: &mut String,
    lcs: &mut Vec<usize>,
) -> bool {
    out.clear();
    candidate.clear();
    for (old_line, new_line) in old_text
        .split_terminator('\n')
        .zip(new_text.split_terminator('\n'))
        .take(cursor_row)
    {
        let old_line = old_line.trim_end();
        let new_line = new_line.trim_end();
        let extends_by_field = !old_line.trim().is_empty()
            && new_line.starts_with(old_line)
            && new_line.len() > old_line.len()
            && new_line.split_whitespace().count() > old_line.split_whitespace().count();
        if !extends_by_field {
            continue;
        }
        candidate.clear();
        if collect_inserted_fields(old_line, new_line, candidate, lcs) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(candidate);
        }
    }
    !out.is_empty()
}

fn collect_new_interface_region(view: &View, out: &mut String) -> bool {
    out.clear();
    let previous = view.prev_screen();
    let current = view.screen();
    if previous.screen != crate::terminal::ScreenIdentity::Primary
        || current.screen != crate::terminal::ScreenIdentity::Primary
        // A visible application cursor remains the strongest generic anchor
        // for editable/search interfaces. Hidden-cursor frames instead need
        // their newly opened region introduced explicitly.
        || current.cursor.visible
        || previous.geometry != current.geometry
        || previous.scrollback.len() != current.scrollback.len()
        || previous.rows.len() != current.rows.len()
    {
        return false;
    }

    let mut first_changed = None;
    let mut last_changed = 0usize;
    let mut changed_text_rows = 0usize;
    let mut newly_populated_rows = 0usize;
    for (row, (old_row, new_row)) in previous.rows.iter().zip(current.rows.iter()).enumerate() {
        if rows_have_same_text(old_row, new_row) {
            continue;
        }
        first_changed.get_or_insert(row);
        last_changed = row;
        changed_text_rows = changed_text_rows.saturating_add(1);
        if !row_has_visible_non_whitespace(old_row) && row_has_visible_non_whitespace(new_row) {
            newly_populated_rows = newly_populated_rows.saturating_add(1);
        }
    }

    let Some(first_changed) = first_changed else {
        return false;
    };
    let row_count = last_changed.saturating_sub(first_changed).saturating_add(1);
    let cells = row_count.saturating_mul(usize::from(current.size().1));
    if changed_text_rows < 2
        || newly_populated_rows < MIN_NEW_INTERFACE_POPULATED_ROWS
        || row_count > MAX_NEW_INTERFACE_REGION_ROWS
        || cells > MAX_NEW_INTERFACE_REGION_CELLS
    {
        return false;
    }

    out.push_str(&current.contents_between(
        first_changed as u16,
        0,
        last_changed as u16,
        current.size().1,
    ));
    let trimmed = out.trim();
    let leading = trimmed.as_ptr() as usize - out.as_ptr() as usize;
    let trailing = leading.saturating_add(trimmed.len());
    if leading > 0 {
        out.drain(..leading);
    }
    out.truncate(trailing.saturating_sub(leading));
    !out.is_empty()
}

fn row_has_visible_non_whitespace(row: &crate::terminal::Row) -> bool {
    row.cells.iter().any(|cell| {
        !cell.is_wide_continuation()
            && cell
                .contents()
                .chars()
                .any(|character| !character.is_whitespace())
    })
}

fn collect_application_cursor_logical_line(view: &View, out: &mut String) -> bool {
    out.clear();
    let previous = view.prev_screen();
    let current = view.screen();
    if !current.cursor.visible
        || previous.screen != current.screen
        || previous.geometry != current.geometry
        || previous.scrollback.len() != current.scrollback.len()
        || previous.rows.len() != current.rows.len()
    {
        return false;
    }

    let Some((previous_start, previous_end)) = cursor_soft_wrapped_span(previous) else {
        return false;
    };
    let Some((current_start, current_end)) = cursor_soft_wrapped_span(current) else {
        return false;
    };
    let history_len = current.scrollback.len();
    if current_start < history_len
        || previous_start < history_len
        || current_start != previous_start
        || current_end == current_start
    {
        return false;
    }

    let span_end = previous_end.max(current_end);
    let span_start_row = current_start - history_len;
    let span_end_row = span_end - history_len;
    let mut changed_text_rows = 0usize;
    for (row, (old_row, new_row)) in previous.rows.iter().zip(current.rows.iter()).enumerate() {
        if rows_have_same_text(old_row, new_row) {
            continue;
        }
        if row < span_start_row || row > span_end_row {
            return false;
        }
        changed_text_rows = changed_text_rows.saturating_add(1);
    }

    let row_count = current_end.saturating_sub(current_start).saturating_add(1);
    let cells = row_count.saturating_mul(usize::from(current.size().1));
    if row_count < 2
        || changed_text_rows < 2
        || row_count > MAX_APPLICATION_CURSOR_LOGICAL_LINE_ROWS
        || cells > MAX_APPLICATION_CURSOR_LOGICAL_LINE_CELLS
    {
        return false;
    }

    for row in span_start_row..=current_end - history_len {
        current.rows[row].append_contents_to(out);
    }
    out.truncate(out.trim_end().len());
    !out.is_empty()
}

fn cursor_soft_wrapped_span(
    snapshot: &crate::terminal::TerminalSnapshot,
) -> Option<(usize, usize)> {
    let history_len = snapshot.scrollback.len();
    let cursor_index = history_len.saturating_add(usize::from(snapshot.cursor.row));
    let total_rows = history_len.saturating_add(snapshot.rows.len());
    if cursor_index >= total_rows {
        return None;
    }
    let row_at = |index: usize| {
        if index < history_len {
            &snapshot.scrollback[index]
        } else {
            &snapshot.rows[index - history_len]
        }
    };

    let mut start = cursor_index;
    while start > 0 && row_at(start - 1).wrapped {
        start -= 1;
    }
    let mut end = cursor_index;
    while end.saturating_add(1) < total_rows && row_at(end).wrapped {
        end += 1;
    }
    (cursor_index == end).then_some((start, end))
}

fn rows_have_same_text(previous: &crate::terminal::Row, current: &crate::terminal::Row) -> bool {
    previous.cells.len() == current.cells.len()
        && previous
            .cells
            .iter()
            .zip(current.cells.iter())
            .all(|(old, new)| old.contents() == new.contents())
}

fn collect_compact_interface_state_change(
    old_text: &str,
    new_text: &str,
    old_cursor_row: usize,
    new_cursor_row: usize,
    out: &mut String,
    candidate: &mut String,
    lcs: &mut Vec<usize>,
) -> bool {
    out.clear();
    candidate.clear();

    let old_rows = old_text.split_terminator('\n');
    let new_rows = new_text.split_terminator('\n');
    for (row, (old_row, new_row)) in old_rows.zip(new_rows).enumerate() {
        if old_row == new_row || row == old_cursor_row || row == new_cursor_row {
            continue;
        }

        candidate.clear();
        if !collect_inserted_fields(old_row, new_row, candidate, lcs) {
            continue;
        }
        let compact = candidate.trim();
        let compact_chars = compact.chars().count();
        if compact_chars == 0 || compact_chars > MAX_COMPACT_INTERFACE_STATE_CHARS {
            continue;
        }
        if out.is_empty() || compact_chars < out.chars().count() {
            out.clear();
            out.push_str(compact);
        }
    }
    !out.is_empty()
}

fn cursor_logical_line_matches(view: &View, report: &str, line: &mut String) -> bool {
    let snapshot = view.screen();
    let cursor_row = usize::from(snapshot.cursor.row);
    if cursor_row >= snapshot.rows.len() {
        return false;
    }

    let history_len = snapshot.scrollback.len();
    let cursor_index = history_len.saturating_add(cursor_row);
    let row_at = |index: usize| {
        if index < history_len {
            &snapshot.scrollback[index]
        } else {
            &snapshot.rows[index - history_len]
        }
    };
    let mut start = cursor_index;
    let columns = usize::from(snapshot.size().1.max(1));
    // Unicode byte length overestimates occupied cells, and the margin covers
    // a nonzero starting column plus the cursor row. If the logical line is
    // older than this, it cannot equal the report and there is no reason to
    // scan or retain an arbitrarily long soft-wrapped history line.
    let maximum_rows = report.len().div_ceil(columns).saturating_add(2);
    let mut rows = 1usize;
    while start > 0 && row_at(start - 1).wrapped {
        if rows >= maximum_rows {
            return false;
        }
        start -= 1;
        rows = rows.saturating_add(1);
    }

    line.clear();
    for index in start..=cursor_index {
        row_at(index).append_contents_to(line);
    }
    line.trim() == report
}

fn next_line_diff_state(state: DiffState, tag: ChangeTag) -> DiffState {
    match state {
        DiffState::NoChanges => match tag {
            ChangeTag::Delete => DiffState::OneDeletion,
            ChangeTag::Equal => DiffState::NoChanges,
            ChangeTag::Insert => DiffState::Multi,
        },
        DiffState::OneDeletion => match tag {
            ChangeTag::Delete => DiffState::Multi,
            ChangeTag::Equal => DiffState::OneDeletion,
            ChangeTag::Insert => DiffState::Single,
        },
        DiffState::Single => match tag {
            ChangeTag::Equal => DiffState::Single,
            _ => DiffState::Multi,
        },
        DiffState::Multi => DiffState::Multi,
    }
}

fn collect_auto_read_diff(
    old_text: &str,
    new_text: &str,
    diff_text: &mut String,
    graphemes: &mut String,
    lcs: &mut Vec<usize>,
) -> DiffState {
    diff_text.clear();
    let line_changes = TextDiff::configure()
        .algorithm(Algorithm::Patience)
        .diff_lines(old_text, new_text);

    let mut diff_state = DiffState::NoChanges;
    for change in line_changes.iter_all_changes() {
        diff_state = next_line_diff_state(diff_state, change.tag());
        if change.tag() == ChangeTag::Insert
            && let Some(change_str) = change.as_str()
        {
            diff_text.push_str(change_str);
            diff_text.push('\n');
        }
    }

    if diff_state != DiffState::Single {
        return diff_state;
    }

    graphemes.clear();
    diff_state = DiffState::NoChanges;
    let mut previous_tag = None;
    for change in TextDiff::configure()
        .algorithm(Algorithm::Patience)
        .diff_graphemes(old_text, new_text)
        .iter_all_changes()
    {
        diff_state = next_grapheme_diff_state(diff_state, previous_tag, change.tag());
        previous_tag = Some(change.tag());
        if diff_state != DiffState::Multi
            && change.tag() == ChangeTag::Insert
            && let Some(change_str) = change.as_str()
        {
            graphemes.push_str(change_str);
        }
    }

    if diff_state == DiffState::Multi {
        graphemes.clear();
        if collect_inserted_fields(old_text, new_text, graphemes, lcs) {
            std::mem::swap(diff_text, graphemes);
        }
    } else {
        std::mem::swap(diff_text, graphemes);
    }
    diff_state
}

fn next_grapheme_diff_state(
    state: DiffState,
    previous: Option<ChangeTag>,
    tag: ChangeTag,
) -> DiffState {
    match state {
        DiffState::NoChanges => match tag {
            ChangeTag::Delete => DiffState::OneDeletion,
            ChangeTag::Equal => DiffState::NoChanges,
            ChangeTag::Insert => DiffState::Single,
        },
        DiffState::OneDeletion => match tag {
            ChangeTag::Delete if previous == Some(ChangeTag::Delete) => DiffState::OneDeletion,
            ChangeTag::Equal => DiffState::OneDeletion,
            ChangeTag::Insert if previous == Some(ChangeTag::Delete) => DiffState::Single,
            _ => DiffState::Multi,
        },
        DiffState::Single => match tag {
            ChangeTag::Equal => DiffState::Single,
            ChangeTag::Insert
                if previous == Some(ChangeTag::Insert) || previous == Some(ChangeTag::Delete) =>
            {
                DiffState::Single
            }
            _ => DiffState::Multi,
        },
        DiffState::Multi => DiffState::Multi,
    }
}

fn collect_inserted_fields(
    old_text: &str,
    new_text: &str,
    out: &mut String,
    lcs: &mut Vec<usize>,
) -> bool {
    let old_fields: Vec<_> = old_text.split_whitespace().collect();
    let new_fields: Vec<_> = new_text.split_whitespace().collect();
    let old_len = old_fields.len();
    let new_len = new_fields.len();
    if new_len == 0 {
        return false;
    }

    lcs.clear();
    lcs.resize((old_len + 1) * (new_len + 1), 0);
    for old_idx in (0..old_len).rev() {
        for new_idx in (0..new_len).rev() {
            let idx = old_idx * (new_len + 1) + new_idx;
            lcs[idx] = if old_fields[old_idx] == new_fields[new_idx] {
                lcs[(old_idx + 1) * (new_len + 1) + new_idx + 1] + 1
            } else {
                lcs[(old_idx + 1) * (new_len + 1) + new_idx]
                    .max(lcs[old_idx * (new_len + 1) + new_idx + 1])
            };
        }
    }

    let mut old_idx = 0;
    let mut new_idx = 0;
    let mut deleted_hunk = Vec::new();
    let mut inserted_hunk = Vec::new();
    let mut last_spoken_hunk = String::new();
    let mut spoke = false;
    while old_idx < old_len || new_idx < new_len {
        if old_idx < old_len && new_idx < new_len && old_fields[old_idx] == new_fields[new_idx] {
            flush_inserted_field_hunk(
                &deleted_hunk,
                &inserted_hunk,
                out,
                &mut last_spoken_hunk,
                &mut spoke,
            );
            deleted_hunk.clear();
            inserted_hunk.clear();
            old_idx += 1;
            new_idx += 1;
        } else if new_idx < new_len
            && (old_idx == old_len
                || lcs[old_idx * (new_len + 1) + new_idx + 1]
                    >= lcs[(old_idx + 1) * (new_len + 1) + new_idx])
        {
            inserted_hunk.push(new_fields[new_idx]);
            new_idx += 1;
        } else {
            deleted_hunk.push(old_fields[old_idx]);
            old_idx += 1;
        }
    }
    flush_inserted_field_hunk(
        &deleted_hunk,
        &inserted_hunk,
        out,
        &mut last_spoken_hunk,
        &mut spoke,
    );

    spoke
}

fn flush_inserted_field_hunk(
    deleted: &[&str],
    inserted: &[&str],
    out: &mut String,
    last_spoken_hunk: &mut String,
    spoke: &mut bool,
) {
    if inserted.is_empty() {
        return;
    }

    let mut hunk = String::new();
    if deleted.len() == inserted.len() {
        for (old_field, new_field) in deleted.iter().zip(inserted) {
            append_inserted_field(field_replacement(old_field, new_field), &mut hunk);
        }
    } else {
        for field in inserted {
            append_inserted_field(field, &mut hunk);
        }
    }

    if hunk.is_empty() || hunk == *last_spoken_hunk {
        return;
    }

    if *spoke {
        out.push(' ');
    }
    out.push_str(&hunk);
    last_spoken_hunk.clone_from(&hunk);
    *spoke = true;
}

fn append_inserted_field(field: &str, out: &mut String) {
    if field.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push(' ');
    }
    out.push_str(field);
}

fn field_replacement<'a>(old_field: &str, new_field: &'a str) -> &'a str {
    let mut prefix_len = 0;
    for ((old_idx, old_ch), (new_idx, new_ch)) in
        old_field.char_indices().zip(new_field.char_indices())
    {
        if old_ch != new_ch {
            break;
        }
        prefix_len = new_idx + new_ch.len_utf8();
        debug_assert_eq!(prefix_len, old_idx + old_ch.len_utf8());
    }

    let old_suffix_source = &old_field[prefix_len..];
    let new_suffix_source = &new_field[prefix_len..];
    let mut suffix_len = 0;
    for (old_ch, new_ch) in old_suffix_source
        .chars()
        .rev()
        .zip(new_suffix_source.chars().rev())
    {
        if old_ch != new_ch {
            break;
        }
        suffix_len += new_ch.len_utf8();
    }

    let mut start = prefix_len;
    let mut end = new_field.len() - suffix_len;
    while start > 0 {
        let Some((previous_index, previous_character)) =
            new_field[..start].char_indices().next_back()
        else {
            break;
        };
        if !is_word_char(previous_character) {
            break;
        }
        start = previous_index;
    }
    while end < new_field.len() {
        let Some(next_character) = new_field[end..].chars().next() else {
            break;
        };
        if !is_word_char(next_character) {
            break;
        }
        end += next_character.len_utf8();
    }
    &new_field[start..end]
}

fn is_word_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

#[cfg(test)]
mod tests {
    use super::{collect_extended_transcript_lines, collect_inserted_fields, field_replacement};

    #[test]
    fn extended_transcript_lines_exclude_parallel_status_replacements() {
        let old = "You: explain repainting\nClaude:\nThe response starts here.\nIt continues on this row\n\nWorking 1s\n> \n";
        let new = "You: explain repainting\nClaude:\nThe response starts here.\nIt continues on this row with more detail.\n\nWorking 2s\n> \n";
        let mut output = String::new();
        let mut candidate = String::new();
        let mut lcs = Vec::new();

        assert!(collect_extended_transcript_lines(
            old,
            new,
            6,
            &mut output,
            &mut candidate,
            &mut lcs,
        ));
        assert_eq!(output, "with more detail.");
    }

    #[test]
    fn replacement_expands_to_word_boundaries() {
        assert_eq!(
            field_replacement("status=ready", "status=running"),
            "running"
        );
        assert_eq!(field_replacement("x42", "x900"), "x900");
    }

    #[test]
    fn inserted_fields_preserve_separate_non_duplicate_hunks() {
        let mut output = String::new();
        let mut lcs = Vec::new();
        assert!(collect_inserted_fields(
            "a old b old c",
            "a first b second c",
            &mut output,
            &mut lcs,
        ));
        assert_eq!(output, "first second");
    }

    #[test]
    fn inserted_fields_collapse_adjacent_duplicate_hunks() {
        let mut output = String::new();
        let mut lcs = Vec::new();
        assert!(collect_inserted_fields(
            "a old b old c",
            "a new b new c",
            &mut output,
            &mut lcs,
        ));
        assert_eq!(output, "new");
    }
}
