use vt100::Color;

pub trait ScreenExt {
    /// Find the first cell between (row_start, col_start) and (row_end, col_end) where matcher(cell) returns true.
    fn find_cell<F>(
        &self,
        matcher: F,
        row_start: u16,
        col_start: u16,
        row_end: u16,
        col_end: u16,
    ) -> Option<(u16, u16)>
    where
        F: Fn(&vt100::Cell) -> bool;

    /// Find the last cell between (row_start, col_start) and (row_end, col_end) where matcher(cell) returns true.
    fn rfind_cell<F>(
        &self,
        matcher: F,
        row_start: u16,
        col_start: u16,
        row_end: u16,
        col_end: u16,
    ) -> Option<(u16, u16)>
    where
        F: Fn(&vt100::Cell) -> bool;

    /// Find the beginning of the word relative to row, col.
    /// If row, col is not in a word, the starting position of the previous word will be returned,
    /// or 0 (the first column) if there isn't one.
    /// Only the current row will be considered.
    fn find_word_start(&self, row: u16, col: u16) -> u16;

    /// Find the end of the word relative to row, col.
    /// The word ends at the column just before the start of the next word, or the last column, if
    /// there isn't one.
    /// This means the cells in range word_start..=word_end will include trailing non-word
    /// characters.
    /// Only the current row will be considered.
    fn find_word_end(&self, row: u16, col: u16) -> u16;

    /// Get the highlighted text on this screen.
    fn get_highlights(&self) -> Vec<String>;
}

impl ScreenExt for vt100::Screen {
    fn find_cell<F>(
        &self,
        matcher: F,
        row_start: u16,
        col_start: u16,
        row_end: u16,
        col_end: u16,
    ) -> Option<(u16, u16)>
    where
        F: Fn(&vt100::Cell) -> bool,
    {
        // row_end and col_end cannot be off the screen.
        let (row_end, col_end) = (
            std::cmp::min(row_end, self.size().0 - 1),
            std::cmp::min(col_end, self.size().1 - 1),
        );
        for row in row_start..=row_end {
            let col_start = if row == row_start { col_start } else { 0 };
            let col_end = if row == row_end {
                col_end
            } else {
                self.size().1 - 1
            };
            for col in col_start..=col_end {
                match self.cell(row, col) {
                    Some(c) if matcher(c) => return Some((row, col)),
                    _ => continue,
                }
            }
        }
        None
    }

    fn rfind_cell<F>(
        &self,
        matcher: F,
        row_start: u16,
        col_start: u16,
        row_end: u16,
        col_end: u16,
    ) -> Option<(u16, u16)>
    where
        F: Fn(&vt100::Cell) -> bool,
    {
        // row_end and col_end cannot be off the screen.
        let (row_end, col_end) = (
            std::cmp::min(row_end, self.size().0 - 1),
            std::cmp::min(col_end, self.size().1 - 1),
        );
        for row in (row_start..=row_end).rev() {
            let col_start = if row == row_start { col_start } else { 0 };
            let col_end = if row == row_end {
                col_end
            } else {
                self.size().1 - 1
            };
            for col in (col_start..=col_end).rev() {
                match self.cell(row, col) {
                    Some(c) if matcher(c) => return Some((row, col)),
                    _ => continue,
                }
            }
        }
        None
    }

    fn find_word_start(&self, row: u16, col: u16) -> u16 {
        // If col isn't in a word, first move it to the end of the previous word.
        let col = self
            .rfind_cell(CellExt::is_in_word, row, 0, row, col)
            .map_or(0, |(_, col)| col);
        if col == 0 {
            // Either the provided col was 0,
            // the end of the previous word was at position 0,
            // or there isn't a word to the left of col.
            return col;
        }

        // Now that col is in a word, find its beginning.
        self.rfind_cell(|c| !c.is_in_word(), row, 0, row, col)
            .map_or(0, |v| v.1 + 1)
    }

    fn find_word_end(&self, row: u16, col: u16) -> u16 {
        // If col is in an word, first move it to the first non-word cell.
        let last = self.size().1 - 1;
        let col = self
            .find_cell(|c| !c.is_in_word(), row, col, row, last)
            .map_or(last, |(_, col)| col);
        if col == last {
            // Either the provided col was at the right edge of the screen,
            // the first non-word character to the right col col was at the right edge of the
            // screen,
            // or this word ends at the right edge of the screen.
            return col;
        }

        self.find_cell(CellExt::is_in_word, row, col, row, last)
            .map_or(last, |v| v.1 - 1)
    }

    fn get_highlights(&self) -> Vec<String> {
        let mut highlights = Vec::new();
        for row in 0..self.size().0 {
            let mut highlight_start = None;
            for col in 0..self.size().1 {
                if let Some(cell) = self.cell(row, col) {
                    match highlight_start {
                        Some(start) => {
                            if !cell.is_highlighted() || col == self.size().1 - 1 {
                                highlights.push(self.contents_between(row, start, row, col + 1));
                                highlight_start = None;
                            }
                        }
                        None => {
                            if cell.is_highlighted() {
                                if col == self.size().1 - 1 {
                                    highlights.push(self.contents_between(row, col, row, col + 1));
                                } else {
                                    highlight_start = Some(col);
                                }
                            }
                        }
                    }
                }
            }
        }

        highlights
    }
}

pub trait CellExt {
    /// Returns true if this cell is in a word.
    fn is_in_word(&self) -> bool;

    /// Returns true if this cell is highlighted (black on yellow).
    fn is_highlighted(&self) -> bool;
}

impl CellExt for vt100::Cell {
    fn is_in_word(&self) -> bool {
        !self.contents().chars().any(char::is_whitespace)
    }

    fn is_highlighted(&self) -> bool {
        self.bgcolor() == Color::Idx(11) && self.fgcolor() == Color::Idx(0)
    }
}
