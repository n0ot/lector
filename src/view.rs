use super::ext::{CellExt, ScreenExt};
use std::cmp::min;

pub struct View {
    parser: vt100::Parser,
    pub next_bytes: Vec<u8>,
    prev_screen: vt100::Screen,
    pub prev_screen_time: u128,
    pub review_cursor_position: (u16, u16), // (row, col)
    pub(crate) review_mark_position: Option<(u16, u16)>, // (row, col)
    review_cursor_indent_level: u16,
    application_cursor_indent_level: u16,
}

impl View {
    pub fn new(rows: u16, cols: u16) -> Self {
        let parser = vt100::Parser::new(rows, cols, 0);
        let cursor_position = parser.screen().cursor_position();
        let prev_screen = parser.screen().clone();
        View {
            parser,
            next_bytes: Vec::new(),
            prev_screen,
            prev_screen_time: 0,
            review_cursor_position: cursor_position,
            review_mark_position: None,
            review_cursor_indent_level: 0,
            application_cursor_indent_level: 0,
        }
    }

    /// Processes new changes, updating the internal screen representation
    pub fn process_changes(&mut self, buf: &[u8]) {
        self.parser.process(buf);
        self.next_bytes.extend_from_slice(buf);
        // If the screen's size changed, the cursor may now be out of bounds.
        let review_cursor_position = self.review_cursor_position;
        self.review_cursor_position = (
            min(review_cursor_position.0, self.size().0),
            min(review_cursor_position.1, self.size().1),
        );

        // If the review cursor moved,
        // it's because the screen was resized.
        // Clear the mark, because it's probably not where you'd expect it.
        if review_cursor_position != self.review_cursor_position {
            self.review_mark_position = None;
        }
    }

    /// Advances the previous screen to match the current one,
    /// and sets its update time to now
    pub fn finalize_changes(&mut self, now_ms: u128) {
        self.prev_screen = self.screen().clone();
        self.prev_screen_time = now_ms;
        self.next_bytes.clear();
    }

    /// Gets the current screen backing this view
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Gets the previous screen backing this view
    pub fn prev_screen(&self) -> &vt100::Screen {
        &self.prev_screen
    }
    /// Gets the size of this view
    pub fn size(&self) -> (u16, u16) {
        self.screen().size()
    }

    /// Resizes this view
    pub fn set_size(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
        // If the screen's size changed, the cursor may now be out of bounds.
        self.review_cursor_position = (
            min(self.review_cursor_position.0, self.size().0),
            min(self.review_cursor_position.1, self.size().1),
        );
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
        if self.review_cursor_position.0 == 0 {
            return false;
        }
        if !skip_blank_lines {
            self.review_cursor_position.0 -= 1;
            return true;
        }

        let row = self.review_cursor_position.0;
        let last_col = self.size().1 - 1;
        self.review_cursor_position.0 = self
            .screen()
            .rfind_cell(CellExt::is_in_word, 0, 0, row - 1, last_col)
            .map_or(row, |(row, _)| row);

        return self.review_cursor_position.0 != row;
    }

    /// Moves the review cursor down a line.
    /// If skip_blank_lines is true,
    /// the review cursor will move down to the next non blank line,
    /// or remain in place if this is the last non blank line.
    /// This method will return true only if the cursor moved.
    pub fn review_cursor_down(&mut self, skip_blank_lines: bool) -> bool {
        let last_row = self.size().0 - 1;
        let last_col = self.size().1 - 1;
        if self.review_cursor_position.0 == last_row {
            return false;
        }
        if !skip_blank_lines {
            self.review_cursor_position.0 += 1;
            return true;
        }

        let row = self.review_cursor_position.0;
        self.review_cursor_position.0 = self
            .screen()
            .find_cell(CellExt::is_in_word, row + 1, 0, last_row, last_col)
            .map_or(row, |(row, _)| row);

        return self.review_cursor_position.0 != row;
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
}
