use super::{
    ext::ScreenExt,
    terminal::{
        GhosttyEngine, GhosttyReviewMark, HistoryPosition, SemanticKind as Osc133Kind,
        SemanticMark as Osc133Mark, TerminalEngine, TerminalGeometry, TerminalSnapshot,
        UpdateSummary, Viewport,
    },
};
use std::cmp::min;

/// A bounded history avoids unbounded memory growth while retaining enough
/// output for extended review and semantic-prompt navigation.
pub const SCROLLBACK_LINES: usize = 10_000;

pub struct View {
    engine: GhosttyEngine,
    pending_update: UpdateSummary,
    prev_screen: TerminalSnapshot,
    prev_screen_time: u128,
    review_cursor_position: (u16, u16),
    review_scrollback: usize,
    retained_history_len: usize,
    review_mark: Option<GhosttyReviewMark>,
    review_cursor_indent_level: u16,
    application_cursor_indent_level: u16,
    cached_full: String,
    cached_prev_full: String,
    cached_full_valid: bool,
    cached_prev_full_valid: bool,
    cached_full_row_hashes: Vec<u64>,
    cached_prev_full_row_hashes: Vec<u64>,
}

impl View {
    pub fn new(rows: u16, cols: u16) -> Self {
        let engine = GhosttyEngine::new_with_scrollback(rows.max(1), cols.max(1), SCROLLBACK_LINES)
            .unwrap_or_else(|error| panic!("could not create Ghostty terminal engine: {error}"));
        let cursor_position = engine.snapshot().cursor_position();
        let prev_screen = engine.snapshot().clone();
        View {
            engine,
            pending_update: UpdateSummary::default(),
            prev_screen,
            prev_screen_time: 0,
            review_cursor_position: cursor_position,
            review_scrollback: 0,
            retained_history_len: 0,
            review_mark: None,
            review_cursor_indent_level: 0,
            application_cursor_indent_level: 0,
            cached_full: String::new(),
            cached_prev_full: String::new(),
            cached_full_valid: false,
            cached_prev_full_valid: false,
            cached_full_row_hashes: Vec::new(),
            cached_prev_full_row_hashes: Vec::new(),
        }
    }

    /// Processes new changes, updating the internal screen representation
    pub fn process_changes(&mut self, buf: &[u8]) {
        let old_history_len = self.retained_history_len;
        let old_review_scrollback = self.review_scrollback;

        // Output is always interpreted against the live drawing screen. The
        // selected review viewport is restored afterward.
        self.engine.select_viewport(Viewport::Live);
        let update = TerminalEngine::advance(&mut self.engine, buf);
        self.pending_update.merge(update);
        let history_len = self.engine.scrollback_extent();
        self.retained_history_len = history_len;
        self.review_scrollback = if old_review_scrollback == 0 {
            0
        } else {
            old_review_scrollback
                .saturating_add(history_len.saturating_sub(old_history_len))
                .min(history_len)
        };
        self.engine
            .select_viewport(Viewport::Scrollback(self.review_scrollback));
        self.cached_full_valid = false;
        self.cached_full_row_hashes.clear();
        // If the screen's size changed, the cursor may now be out of bounds.
        let review_cursor_position = self.review_cursor_position;
        let (rows, cols) = self.size();
        let max_row = rows.saturating_sub(1);
        let max_col = cols.saturating_sub(1);
        self.review_cursor_position = (
            min(review_cursor_position.0, max_row),
            min(review_cursor_position.1, max_col),
        );

        // If the review cursor moved,
        // it's because the screen was resized.
        // Clear the mark, because it's probably not where you'd expect it.
        if review_cursor_position != self.review_cursor_position {
            self.clear_review_mark();
        }
    }

    /// Advances the previous screen to match the current one,
    /// and sets its update time to now
    pub fn finalize_changes(&mut self, now_ms: u128) {
        let visible_offset = self.review_scrollback;
        self.engine.select_viewport(Viewport::Live);
        self.prev_screen = self.engine.snapshot().clone();
        self.engine
            .select_viewport(Viewport::Scrollback(visible_offset));
        self.prev_screen_time = now_ms;
        self.pending_update = UpdateSummary::default();
        if self.cached_full_valid {
            self.cached_prev_full.clone_from(&self.cached_full);
            self.cached_prev_full_valid = true;
            self.cached_prev_full_row_hashes
                .clone_from(&self.cached_full_row_hashes);
        } else {
            self.cached_prev_full_valid = false;
            self.cached_prev_full_row_hashes.clear();
        }
    }

    /// Gets the current screen backing this view
    pub fn screen(&self) -> &TerminalSnapshot {
        self.engine.snapshot()
    }

    pub fn snapshot_with_history(&mut self) -> TerminalSnapshot {
        self.engine.snapshot_with_history()
    }

    pub(crate) fn presentation_contents(&self) -> Vec<u8> {
        self.engine.presentation_contents()
    }

    pub(crate) fn presentation_cursor(&self) -> Vec<u8> {
        self.engine.presentation_cursor()
    }

    pub(crate) fn presentation_attributes(&self) -> Vec<u8> {
        self.engine.presentation_attributes()
    }

    pub(crate) fn presentation_input_modes(&self) -> Vec<u8> {
        self.engine.presentation_input_modes()
    }

    /// Runs work against the live drawing screen, then returns to the review
    /// viewport. Screen diffing and application-cursor tracking must not read
    /// whichever historical page the review cursor happens to be on.
    pub(crate) fn with_live_screen<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.engine.select_viewport(Viewport::Live);
        let result = f(self);
        let history_len = self.engine.scrollback_extent();
        self.retained_history_len = history_len;
        self.review_scrollback = self.review_scrollback.min(history_len);
        self.engine
            .select_viewport(Viewport::Scrollback(self.review_scrollback));
        result
    }

    pub fn scrollback(&self) -> usize {
        self.review_scrollback
    }

    pub fn scrollback_len(&self) -> usize {
        self.retained_history_len
    }

    pub fn review_cursor_position(&self) -> (u16, u16) {
        self.review_cursor_position
    }

    pub(crate) fn set_review_cursor_position(&mut self, position: (u16, u16)) {
        self.review_cursor_position = position;
    }

    pub(crate) fn follow_application_cursor(&mut self) {
        self.review_scrollback = 0;
        self.engine.select_viewport(Viewport::Live);
        self.review_cursor_position = self.engine.snapshot().cursor_position();
    }

    pub(crate) fn set_review_cursor_row(&mut self, row: u16) {
        self.review_cursor_position.0 = row;
    }

    pub(crate) fn set_review_cursor_col(&mut self, col: u16) {
        self.review_cursor_position.1 = col;
    }

    pub fn review_history_position(&self) -> HistoryPosition {
        HistoryPosition {
            row: self
                .retained_history_len
                .saturating_sub(self.review_scrollback)
                .saturating_add(usize::from(self.review_cursor_position.0)),
            col: self.review_cursor_position.1,
        }
    }

    fn current_history_position(&self) -> HistoryPosition {
        self.review_history_position()
    }

    pub(crate) fn set_review_history_position(&mut self, position: HistoryPosition) {
        let history_len = self.retained_history_len;
        let last_row = usize::from(self.size().0.saturating_sub(1));
        let max_history_row = history_len.saturating_add(last_row);
        let target_row = position.row.min(max_history_row);
        let current_start = history_len.saturating_sub(self.review_scrollback);
        let current_end = current_start.saturating_add(last_row);

        let visible_start = if target_row < current_start {
            target_row
        } else if target_row > current_end {
            target_row.saturating_sub(last_row)
        } else {
            current_start
        };
        self.review_scrollback = history_len.saturating_sub(visible_start);
        self.engine
            .select_viewport(Viewport::Scrollback(self.review_scrollback));
        self.review_cursor_position = (
            target_row.saturating_sub(visible_start) as u16,
            position.col.min(self.size().1.saturating_sub(1)),
        );
    }

    pub(crate) fn set_review_mark(&mut self) {
        let position = self.review_history_position();
        self.review_mark = Some(
            self.engine
                .track_review_mark(position)
                .unwrap_or_else(|error| panic!("could not track Ghostty review mark: {error}")),
        );
    }

    pub(crate) fn review_mark_position(&self) -> Option<HistoryPosition> {
        self.review_mark.as_ref().and_then(|mark| {
            self.engine
                .review_mark_position(mark)
                .unwrap_or_else(|error| panic!("could not resolve Ghostty review mark: {error}"))
        })
    }

    pub(crate) fn update_summary(&self) -> &UpdateSummary {
        &self.pending_update
    }

    /// Pane-scoped terminal side effects observed since the last finalized
    /// screen update. Borrowed callback data has already been copied into
    /// owned, normalized values before reaching this model.
    pub fn terminal_events(&self) -> &[crate::terminal::TerminalEvent] {
        &self.pending_update.effects.events
    }

    pub(crate) fn clear_update_summary(&mut self) {
        self.pending_update = UpdateSummary::default();
    }

    pub(crate) fn set_previous_screen_time(&mut self, time: u128) {
        self.prev_screen_time = time;
    }

    /// Gets the previous screen backing this view
    pub fn prev_screen(&self) -> &TerminalSnapshot {
        &self.prev_screen
    }
    /// Gets the size of this view
    pub fn size(&self) -> (u16, u16) {
        self.screen().size()
    }

    /// Resizes this view
    pub fn set_size(&mut self, rows: u16, cols: u16) {
        self.set_size_with_geometry(TerminalGeometry::from_cells(rows, cols));
    }

    /// Resizes this view while preserving per-cell pixel geometry for
    /// size-query and image consumers.
    pub fn set_size_with_geometry(&mut self, geometry: TerminalGeometry) {
        let geometry = TerminalGeometry::new(
            geometry.rows,
            geometry.cols,
            geometry.cell_width_px,
            geometry.cell_height_px,
        );
        TerminalEngine::resize_with_geometry(&mut self.engine, geometry);
        let history_len = self.engine.scrollback_extent();
        self.retained_history_len = history_len;
        self.review_scrollback = self.review_scrollback.min(history_len);
        self.engine
            .select_viewport(Viewport::Scrollback(self.review_scrollback));
        self.cached_full_valid = false;
        self.cached_full_row_hashes.clear();
        // If the screen's size changed, the cursor may now be out of bounds.
        let review_cursor_position = self.review_cursor_position;
        let max_row = geometry.rows.saturating_sub(1);
        let max_col = geometry.cols.saturating_sub(1);
        self.review_cursor_position = (
            min(self.review_cursor_position.0, max_row),
            min(self.review_cursor_position.1, max_col),
        );
        if review_cursor_position != self.review_cursor_position {
            self.clear_review_mark();
        }
    }

    fn clear_review_mark(&mut self) {
        self.review_mark = None;
    }

    /// Gets the indentation level of the line under the review cursor,
    /// and whether it's changed since the last time this method was called.
    pub fn review_cursor_indentation_level(&mut self) -> (u16, bool) {
        let indent_level = self
            .screen()
            .find_cell(
                |c| !c.contents().is_empty() && !c.contents().chars().all(char::is_whitespace),
                self.review_cursor_position.0,
                0,
                self.review_cursor_position.0,
                self.size().1 - 1,
            )
            .map_or(self.review_cursor_indent_level, |(_, col)| col);

        let changed = indent_level != self.review_cursor_indent_level;
        self.review_cursor_indent_level = indent_level;
        (indent_level, changed)
    }

    /// Gets the indentation level of the line under the application cursor,
    /// and whether it's changed since the last time this method was called.
    pub fn application_cursor_indentation_level(&mut self) -> (u16, bool) {
        let indent_level = self
            .screen()
            .find_cell(
                |c| !c.contents().is_empty() && !c.contents().chars().all(char::is_whitespace),
                self.screen().cursor_position().0,
                0,
                self.screen().cursor_position().0,
                self.size().1 - 1,
            )
            .map_or(self.application_cursor_indent_level, |(_, col)| col);

        let changed = indent_level != self.application_cursor_indent_level;
        self.application_cursor_indent_level = indent_level;
        (indent_level, changed)
    }

    /// Moves the review cursor up a line.
    /// If skip_blank_lines is true,
    /// the review cursor will move up to the previous non blank line,
    /// or remain in place if this is the first non blank line.
    /// This method will return true only if the cursor moved.
    pub fn review_cursor_up(&mut self, skip_blank_lines: bool) -> bool {
        if !skip_blank_lines {
            if self.review_cursor_position.0 > 0 {
                self.review_cursor_position.0 -= 1;
                return true;
            }
            let history_len = self.retained_history_len;
            if self.review_scrollback >= history_len {
                return false;
            }
            self.review_scrollback += 1;
            self.engine
                .select_viewport(Viewport::Scrollback(self.review_scrollback));
            return true;
        }
        let original = self.current_history_position();
        while self.review_cursor_up(false) {
            if !self.line(self.review_cursor_position.0).trim().is_empty() {
                return true;
            }
        }
        self.set_review_history_position(original);
        false
    }

    /// Moves the review cursor down a line.
    /// If skip_blank_lines is true,
    /// the review cursor will move down to the next non blank line,
    /// or remain in place if this is the last non blank line.
    /// This method will return true only if the cursor moved.
    pub fn review_cursor_down(&mut self, skip_blank_lines: bool) -> bool {
        let last_row = self.size().0 - 1;
        if !skip_blank_lines {
            if self.review_cursor_position.0 < last_row {
                self.review_cursor_position.0 += 1;
                return true;
            }
            if self.review_scrollback == 0 {
                return false;
            }
            self.review_scrollback -= 1;
            self.engine
                .select_viewport(Viewport::Scrollback(self.review_scrollback));
            return true;
        }
        let original = self.current_history_position();
        while self.review_cursor_down(false) {
            if !self.line(self.review_cursor_position.0).trim().is_empty() {
                return true;
            }
        }
        self.set_review_history_position(original);
        false
    }

    pub fn osc133_marks(&self) -> &[Osc133Mark] {
        &self.screen().semantic_marks
    }

    /// Returns the most recently submitted command line delimited by OSC 133
    /// B/C, excluding the prompt. This describes submitted input, not a
    /// transient Readline history selection that has not been executed.
    pub fn last_submitted_input(&mut self) -> Option<String> {
        let marks = self.osc133_marks();
        let alternate_screen = self.screen().alternate_screen();
        let command_index = marks.iter().rposition(|mark| {
            mark.alternate_screen == alternate_screen
                && matches!(mark.kind, Osc133Kind::CommandStart)
        })?;
        let command_start = marks[command_index].position;
        let prompt_index = marks[..command_index].iter().rposition(|mark| {
            mark.alternate_screen == alternate_screen
                && matches!(mark.kind, Osc133Kind::PromptStart)
        });
        let input_start = marks[prompt_index.map_or(0, |index| index + 1)..command_index]
            .iter()
            .rfind(|mark| {
                mark.alternate_screen == alternate_screen
                    && matches!(mark.kind, Osc133Kind::InputStart)
            })?
            .position;
        let mut input = self.contents_between_history(input_start, command_start)?;
        while input.ends_with(['\r', '\n']) {
            input.pop();
        }
        Some(input)
    }

    /// Returns the currently displayed editable input when the latest OSC 133
    /// phase is B (input). Bash does not emit another B for each Readline
    /// history selection, but the original marker remains a reliable prompt
    /// boundary while Up/Down redraw the text after it.
    pub fn active_semantic_input(&mut self) -> Option<String> {
        let alternate_screen = self.screen().alternate_screen();
        let current_marks: Vec<_> = self
            .osc133_marks()
            .iter()
            .filter(|mark| mark.alternate_screen == alternate_screen)
            .collect();
        let input_start = current_marks
            .iter()
            .rev()
            .find_map(|mark| match mark.kind {
                Osc133Kind::InputStart => Some(mark.position),
                Osc133Kind::PromptStart
                | Osc133Kind::CommandStart
                | Osc133Kind::CommandFinished { .. } => None,
            })?;
        let latest_phase = current_marks.last()?.kind;
        if !matches!(latest_phase, Osc133Kind::InputStart) {
            return None;
        }
        let (cursor_row, cursor_col) = self.screen().cursor_position();
        let last_col = self.size().1.saturating_sub(1);
        let input_end_col = self
            .screen()
            .rfind_cell(
                |cell| !cell.contents().is_empty(),
                cursor_row,
                cursor_col,
                cursor_row,
                last_col,
            )
            .map_or(cursor_col, |(_, col)| col.saturating_add(1));
        let cursor = HistoryPosition {
            row: self.retained_history_len + usize::from(cursor_row),
            col: input_end_col,
        };
        let mut input = self.contents_between_history(input_start, cursor)?;
        while matches!(input.chars().last(), Some('\r' | '\n')) {
            input.pop();
        }
        Some(input)
    }

    fn contents_between_history(
        &mut self,
        start: HistoryPosition,
        end: HistoryPosition,
    ) -> Option<String> {
        if start > end {
            return None;
        }
        let saved_offset = self.review_scrollback;
        let cols = self.size().1;
        let mut contents = String::new();

        for absolute_row in start.row..=end.row {
            let (offset, visible_row) = if absolute_row < self.retained_history_len {
                (self.retained_history_len - absolute_row, 0)
            } else {
                let row = absolute_row - self.retained_history_len;
                if row >= usize::from(self.size().0) {
                    self.engine
                        .select_viewport(Viewport::Scrollback(saved_offset));
                    return None;
                }
                (0, row as u16)
            };
            self.engine.select_viewport(Viewport::Scrollback(offset));
            let start_col = if absolute_row == start.row {
                start.col
            } else {
                0
            };
            let end_col = if absolute_row == end.row {
                end.col
            } else {
                cols
            };
            for col in start_col..end_col.min(cols) {
                contents.push_str(
                    self.screen()
                        .cell(visible_row, col)
                        .map_or("", crate::terminal::Cell::contents),
                );
            }
            if absolute_row != end.row && !self.screen().row_wrapped(visible_row) {
                contents.push('\n');
            }
        }

        self.engine
            .select_viewport(Viewport::Scrollback(saved_offset));
        Some(contents)
    }

    pub(crate) fn copy_review_selection(&mut self, mark: HistoryPosition) -> Option<String> {
        let cursor = self.current_history_position();
        if mark > cursor {
            return None;
        }
        self.contents_between_history(
            mark,
            HistoryPosition {
                row: cursor.row,
                col: cursor.col.saturating_add(1).min(self.size().1),
            },
        )
    }

    /// Moves the cursor to the start of the previous word,
    /// or the beginning of the line if the cursor is in or before the first word.
    /// This method will return true only if the cursor moved to a different word.
    pub fn review_cursor_prev_word(&mut self) -> bool {
        let (row, col) = self.review_cursor_position;
        // First, find the beginning of this word.
        let col = self.screen().find_word_start(row, col);
        if col == 0 {
            // The current word was the first.
            // Just move to the beginning of the line.
            self.review_cursor_position.1 = 0;
            return false;
        }

        // Now, find the start of the previous word and move to it.
        let col = self.screen().find_word_start(row, col - 1);
        self.review_cursor_position.1 = col;
        true
    }

    /// Moves the cursor to the start of the next word,
    /// or the end of the line if the cursor is in or past the last word.
    /// This method will return true only if the cursor moved to a different word.
    pub fn review_cursor_next_word(&mut self) -> bool {
        let last = self.size().1 - 1;
        let (row, col) = self.review_cursor_position;
        // First, find the end of this word.
        let col = self.screen().find_word_end(row, col);
        if col >= last {
            // The current word was the last.
            return false;
        }

        self.review_cursor_position.1 = col + 1;
        true
    }

    /// Moves the review cursor left a column.
    /// If the next cell continues a wide character, it will be skipped.
    /// This method will return true only if the cursor moved.
    pub fn review_cursor_left(&mut self) -> bool {
        if self.review_cursor_position.1 == 0 {
            return false;
        }
        if let Some((row, col)) = self.screen().rfind_cell(
            |c| !c.is_wide_continuation(),
            self.review_cursor_position.0,
            0,
            self.review_cursor_position.0,
            self.review_cursor_position.1 - 1,
        ) {
            self.review_cursor_position = (row, col);
            true
        } else {
            false
        }
    }

    /// Moves the review cursor right a column.
    /// If the next cell continues a wide character, it will be skipped.
    /// This method will return true only if the cursor moved.
    pub fn review_cursor_right(&mut self) -> bool {
        if self.review_cursor_position.1 >= self.size().1 - 1 {
            return false;
        }

        if let Some((row, col)) = self.screen().find_cell(
            |c| !c.is_wide_continuation(),
            self.review_cursor_position.0,
            self.review_cursor_position.1 + 1,
            self.review_cursor_position.0,
            self.size().1 - 1,
        ) {
            self.review_cursor_position = (row, col);
            true
        } else {
            false
        }
    }

    /// Returns the entire line at the specified row.
    pub fn line(&self, row: u16) -> String {
        self.screen().contents_between(row, 0, row, self.size().1)
    }

    /// Returns the word at the specified coordinates.
    pub fn word(&self, row: u16, col: u16) -> String {
        let start = self.screen().find_word_start(row, col);
        let end = self.screen().find_word_end(row, col);
        self.screen().contents_between(row, start, row, end + 1)
    }

    /// Returns the character at the specified coordinates.
    pub fn character(&self, row: u16, col: u16) -> String {
        self.screen().contents_between(row, col, row, col + 1)
    }

    /// Returns the contents of the full screen, including blank lines.
    pub fn contents_full(&self) -> String {
        self.screen().contents_full()
    }

    /// Writes the contents of the full screen, including blank lines, into `out`.
    pub fn contents_full_into(&self, out: &mut String) {
        self.screen().contents_full_into(out);
    }

    pub fn full_contents_cached(&mut self) -> (&str, &str, &[u64], &[u64]) {
        self.ensure_cached_full();
        self.ensure_cached_prev_full();
        (
            &self.cached_prev_full,
            &self.cached_full,
            &self.cached_prev_full_row_hashes,
            &self.cached_full_row_hashes,
        )
    }

    fn ensure_cached_full(&mut self) {
        if self.cached_full_valid {
            return;
        }
        let mut cached_full = std::mem::take(&mut self.cached_full);
        self.screen().contents_full_into(&mut cached_full);
        compute_row_hashes(&cached_full, &mut self.cached_full_row_hashes);
        self.cached_full = cached_full;
        self.cached_full_valid = true;
    }

    fn ensure_cached_prev_full(&mut self) {
        if self.cached_prev_full_valid {
            return;
        }
        let mut cached_prev_full = std::mem::take(&mut self.cached_prev_full);
        self.prev_screen.contents_full_into(&mut cached_prev_full);
        compute_row_hashes(&cached_prev_full, &mut self.cached_prev_full_row_hashes);
        self.cached_prev_full = cached_prev_full;
        self.cached_prev_full_valid = true;
    }
}

fn compute_row_hashes(source: &str, out: &mut Vec<u64>) {
    out.clear();
    for line in source.split_terminator('\n') {
        out.push(fnv1a_64(line.as_bytes()));
    }
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::{View, compute_row_hashes, fnv1a_64};
    use crate::terminal::{HistoryPosition, SemanticKind as Osc133Kind};

    #[test]
    fn resize_clamps_review_cursor_and_clears_displaced_mark() {
        let mut view = View::new(4, 8);
        view.set_review_cursor_position((3, 7));
        view.set_review_mark();

        view.set_size(2, 5);

        assert_eq!(view.review_cursor_position(), (1, 4));
        assert_eq!(view.review_mark_position(), None);
    }

    #[test]
    fn resize_preserves_mark_when_review_cursor_remains_valid() {
        let mut view = View::new(4, 8);
        view.set_review_cursor_position((1, 2));
        view.set_review_mark();

        view.set_size(3, 6);

        assert_eq!(view.review_cursor_position(), (1, 2));
        assert_eq!(
            view.review_mark_position(),
            Some(HistoryPosition { row: 1, col: 2 })
        );
    }

    #[test]
    fn finalize_advances_screen_and_clears_pending_update() {
        let mut view = View::new(2, 8);
        view.process_changes(b"hello");
        assert_eq!(view.update_summary().printed_text(), "hello");
        assert_ne!(view.screen().contents(), view.prev_screen().contents());

        view.finalize_changes(42);

        assert_eq!(view.update_summary().batch_count, 0);
        assert_eq!(view.screen().contents(), view.prev_screen().contents());
        assert_eq!(view.prev_screen_time, 42);
    }

    #[test]
    fn cached_contents_follow_process_and_finalize_lifecycle() {
        let mut view = View::new(2, 8);
        view.process_changes(b"old");
        view.finalize_changes(1);
        view.process_changes(b"\rnew");

        let (previous, current, previous_hashes, current_hashes) = view.full_contents_cached();
        assert!(previous.starts_with("old"));
        assert!(current.starts_with("new"));
        assert_ne!(previous_hashes, current_hashes);

        view.finalize_changes(2);
        let (previous, current, previous_hashes, current_hashes) = view.full_contents_cached();
        assert_eq!(previous, current);
        assert_eq!(previous_hashes, current_hashes);
    }

    #[test]
    fn vertical_navigation_skips_blank_lines_and_stops_at_content_boundaries() {
        let mut view = View::new(5, 10);
        view.process_changes(b"top\r\n\r\nmiddle\r\n\r\nbottom");

        assert!(!view.review_cursor_up(true));
        assert!(view.review_cursor_down(true));
        assert_eq!(view.review_cursor_position(), (2, 0));
        assert!(view.review_cursor_down(true));
        assert_eq!(view.review_cursor_position(), (4, 0));
        assert!(!view.review_cursor_down(true));
        assert!(view.review_cursor_up(true));
        assert_eq!(view.review_cursor_position(), (2, 0));
        assert!(view.review_cursor_up(false));
        assert_eq!(view.review_cursor_position(), (1, 0));
        assert!(view.review_cursor_down(false));
        assert_eq!(view.review_cursor_position(), (2, 0));
    }

    #[test]
    fn word_navigation_handles_first_last_and_inter_word_whitespace() {
        let mut view = View::new(1, 10);
        view.process_changes(b"one  two");

        assert!(!view.review_cursor_prev_word());
        assert!(view.review_cursor_next_word());
        assert_eq!(view.review_cursor_position(), (0, 5));
        assert_eq!(view.word(0, 5), "two");
        assert!(!view.review_cursor_next_word());
        view.set_review_cursor_col(7);
        assert!(view.review_cursor_prev_word());
        assert_eq!(view.review_cursor_position(), (0, 0));
        assert_eq!(view.word(0, 4), "one  ");
    }

    #[test]
    fn horizontal_navigation_skips_wide_continuations_and_obeys_edges() {
        let mut view = View::new(1, 6);
        view.process_changes("a界b".as_bytes());

        assert!(!view.review_cursor_left());
        assert!(view.review_cursor_right());
        assert_eq!(view.review_cursor_position(), (0, 1));
        assert_eq!(view.character(0, 1), "界");
        assert!(view.review_cursor_right());
        assert_eq!(view.review_cursor_position(), (0, 3));
        assert!(view.review_cursor_left());
        assert_eq!(view.review_cursor_position(), (0, 1));
        assert!(view.review_cursor_left());
        assert_eq!(view.review_cursor_position(), (0, 0));

        view.set_review_cursor_col(5);
        assert!(!view.review_cursor_right());
    }

    #[test]
    fn indentation_and_full_content_accessors_report_changes_without_blank_resets() {
        let mut view = View::new(3, 12);
        view.process_changes(b"  alpha\r\n    beta\x1B[1;1H");

        assert_eq!(view.review_cursor_indentation_level(), (2, true));
        assert_eq!(view.review_cursor_indentation_level(), (2, false));
        assert_eq!(view.application_cursor_indentation_level(), (2, true));
        assert_eq!(view.application_cursor_indentation_level(), (2, false));

        view.set_review_cursor_row(1);
        assert_eq!(view.review_cursor_indentation_level(), (4, true));
        view.set_review_cursor_row(2);
        assert_eq!(view.review_cursor_indentation_level(), (4, false));

        let mut contents = String::from("stale");
        view.contents_full_into(&mut contents);
        assert_eq!(contents, "  alpha\n    beta\n\n");
        assert_eq!(view.line(1), "    beta");
    }

    #[test]
    fn row_hashes_are_stable_and_reuse_the_destination() {
        assert_eq!(fnv1a_64(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a_64(b"a"), 0xaf63dc4c8601ec8c);

        let mut hashes = vec![0, 1, 2];
        compute_row_hashes("a\n\n", &mut hashes);
        assert_eq!(hashes, [fnv1a_64(b"a"), fnv1a_64(b"")]);
    }

    #[test]
    fn line_navigation_crosses_the_live_viewport_into_scrollback() {
        let mut view = View::new(3, 12);
        view.process_changes(b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        assert_eq!(view.scrollback_len(), 2);
        assert_eq!(view.line(0), "three");

        assert!(view.review_cursor_up(false));
        assert_eq!(view.scrollback(), 1);
        assert_eq!(view.line(0), "two");
        assert!(view.review_cursor_up(false));
        assert_eq!(view.scrollback(), 2);
        assert_eq!(view.line(0), "one");
        assert!(!view.review_cursor_up(false));
    }

    #[test]
    fn osc133_marks_and_submitted_input_survive_scrolling() {
        let mut view = View::new(3, 20);
        view.process_changes(
            b"\x1B]133;A\x07$ \x1B]133;B\x07echo one\r\n\x1B]133;C\x07out\r\n\x1B]133;D;0\x07\r\n\x1B]133;A\x07$ \x1B]133;B\x07echo two\r\n\x1B]133;C\x07done\r\n\x1B]133;D;1\x07",
        );
        assert!(view.scrollback_len() >= 3);
        assert_eq!(
            view.osc133_marks()
                .iter()
                .filter(|mark| matches!(mark.kind, Osc133Kind::PromptStart))
                .count(),
            2
        );
        assert_eq!(view.last_submitted_input().as_deref(), Some("echo two"));
    }

    #[test]
    fn review_copy_can_span_retained_history_and_the_live_screen() {
        let mut view = View::new(2, 8);
        view.process_changes(b"one\r\ntwo\r\nthree");
        view.set_review_cursor_position((0, 0));
        assert!(view.review_cursor_up(false));
        view.set_review_mark();
        view.follow_application_cursor();
        view.set_review_cursor_col(4);

        assert_eq!(
            view.copy_review_selection(view.review_mark_position().unwrap()),
            Some("one\ntwo\nthree".into())
        );
    }

    #[test]
    fn active_semantic_input_uses_the_existing_b_marker_after_readline_redraws() {
        let mut view = View::new(3, 20);
        view.process_changes(b"\x1B]133;A\x07$ \x1B]133;B\x07old");
        assert_eq!(view.active_semantic_input().as_deref(), Some("old"));

        // Readline redraws prompt + recalled text, but emits no new OSC 133 B.
        view.process_changes(b"\r\x1B[K$ recalled");
        assert_eq!(view.active_semantic_input().as_deref(), Some("recalled"));
        view.process_changes(b"\x1B[4D");
        assert_eq!(view.active_semantic_input().as_deref(), Some("recalled"));

        view.process_changes(b"\r\n\x1B]133;C\x07");
        assert_eq!(view.active_semantic_input(), None);
    }
}
