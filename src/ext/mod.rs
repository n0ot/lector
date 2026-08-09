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

    /// Get the contents of the screen, including blank lines.
    /// Trailing whitespace will be removed from each line.
    fn contents_full(&self) -> String;

    /// Write the contents of the screen into an existing buffer.
    /// Trailing whitespace will be removed from each line.
    fn contents_full_into(&self, out: &mut String);
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
                                let end = if cell.is_highlighted() { col + 1 } else { col };
                                highlights.push(self.contents_between(row, start, row, end));
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

    fn contents_full(&self) -> String {
        self.rows(0, self.size().1)
            .map(|row| format!("{}\n", row.trim_end()))
            .collect::<String>()
    }

    fn contents_full_into(&self, out: &mut String) {
        out.clear();
        let (rows, cols) = self.size();
        out.reserve((rows as usize) * (cols as usize + 1));
        for row in self.rows(0, cols) {
            out.push_str(row.trim_end());
            out.push('\n');
        }
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
        self.has_contents() && !self.contents().chars().any(char::is_whitespace)
    }

    fn is_highlighted(&self) -> bool {
        self.bgcolor() == Color::Idx(11) && self.fgcolor() == Color::Idx(0)
    }
}

#[cfg(test)]
mod tests {
    use super::{CellExt, ScreenExt};

    fn parser(rows: u16, cols: u16, contents: &[u8]) -> vt100::Parser {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(contents);
        parser
    }

    #[test]
    fn searches_forward_and_backward_across_rows_and_clamps_end_bounds() {
        let parser = parser(3, 5, b"a1\r\n b2\r\n  c3");
        let screen = parser.screen();
        let is_digit = |cell: &vt100::Cell| {
            cell.contents()
                .chars()
                .next()
                .is_some_and(|ch| ch.is_ascii_digit())
        };

        assert_eq!(screen.find_cell(is_digit, 0, 0, 99, 99), Some((0, 1)));
        assert_eq!(screen.find_cell(is_digit, 1, 2, 99, 99), Some((1, 2)));
        assert_eq!(screen.rfind_cell(is_digit, 0, 0, 99, 99), Some((2, 3)));
        assert_eq!(screen.find_cell(is_digit, 2, 4, 2, 4), None);
        assert_eq!(screen.rfind_cell(is_digit, 2, 4, 2, 4), None);
    }

    #[test]
    fn word_boundaries_cover_words_whitespace_and_screen_edges() {
        let parser = parser(1, 10, b"one  two");
        let screen = parser.screen();

        assert_eq!(screen.find_word_start(0, 0), 0);
        assert_eq!(screen.find_word_start(0, 4), 0);
        assert_eq!(screen.find_word_start(0, 7), 5);
        assert_eq!(screen.find_word_end(0, 0), 4);
        assert_eq!(screen.find_word_end(0, 5), 9);
        assert_eq!(screen.find_word_end(0, 9), 9);
    }

    #[test]
    fn extracts_only_black_on_bright_yellow_highlight_runs() {
        let parser = parser(2, 8, b"\x1B[30;103mhot\x1B[0m x\r\nabc  \x1B[30;103mend");

        assert_eq!(parser.screen().get_highlights(), ["hot", "end"]);
        assert!(parser.screen().cell(0, 0).unwrap().is_highlighted());
        assert!(!parser.screen().cell(0, 3).unwrap().is_highlighted());
    }

    #[test]
    fn full_contents_preserve_blank_rows_and_reuse_the_output_buffer() {
        let parser = parser(3, 5, b"one\r\n\r\ntwo  ");
        let screen = parser.screen();
        let mut output = String::from("stale contents");

        screen.contents_full_into(&mut output);

        assert_eq!(screen.contents_full(), "one\n\ntwo\n");
        assert_eq!(output, "one\n\ntwo\n");
    }

    #[test]
    fn cell_word_detection_distinguishes_content_whitespace_and_blanks() {
        let parser = parser(1, 4, "é x".as_bytes());
        let screen = parser.screen();

        assert!(screen.cell(0, 0).unwrap().is_in_word());
        assert!(!screen.cell(0, 1).unwrap().is_in_word());
        assert!(screen.cell(0, 2).unwrap().is_in_word());
        assert!(!screen.cell(0, 3).unwrap().is_in_word());
    }
}
