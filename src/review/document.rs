use crate::{
    perform::{HistoryPosition, Osc133Kind, Osc133Mark},
    view::View,
};
use regex::RegexBuilder;
use std::ops::Range;

#[derive(Clone, Debug)]
struct Cell {
    text: String,
    wide_continuation: bool,
}

#[derive(Clone, Debug)]
struct Row {
    cells: Vec<Cell>,
    wrapped: bool,
    end: u16,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum WordStyle {
    Word,
    BigWord,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum WordMove {
    ForwardStart,
    BackwardStart,
    ForwardEnd,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum SearchDirection {
    Forward,
    Backward,
}

impl SearchDirection {
    pub(crate) fn reverse(self) -> Self {
        match self {
            Self::Forward => Self::Backward,
            Self::Backward => Self::Forward,
        }
    }
}

/// A frozen, addressable copy of a terminal view and all of its retained
/// scrollback. The source `View` and its parser are never consulted again.
pub(crate) struct ReviewDocument {
    screen: vt100::Screen,
    rows: Vec<Row>,
    marks: Vec<Osc133Mark>,
    history_len: usize,
    capture_cols: u16,
    alternate_screen: bool,
    flat_text: String,
    flat_positions: Vec<HistoryPosition>,
    positions: Vec<HistoryPosition>,
}

impl ReviewDocument {
    pub(crate) fn capture(view: &mut View) -> (Self, HistoryPosition, usize) {
        let history_len = view.scrollback_len();
        let (capture_rows, capture_cols) = view.size();
        let initial = view.review_history_position();
        let initial_top = history_len.saturating_sub(view.scrollback());
        let alternate_screen = view.screen().alternate_screen();
        let marks = view.osc133_marks().to_vec();
        let mut screen = view.screen().clone();
        let mut rows = Vec::with_capacity(history_len + usize::from(capture_rows));

        for absolute_row in 0..history_len + usize::from(capture_rows) {
            let (offset, visible_row) = if absolute_row < history_len {
                (history_len - absolute_row, 0)
            } else {
                (0, (absolute_row - history_len) as u16)
            };
            screen.set_scrollback(offset);
            let mut cells = Vec::with_capacity(usize::from(capture_cols));
            let mut end = 0;
            for col in 0..capture_cols {
                let cell = screen.cell(visible_row, col);
                let text = cell.map_or("", vt100::Cell::contents).to_string();
                let wide_continuation = cell.is_some_and(vt100::Cell::is_wide_continuation);
                if !text.is_empty() || wide_continuation {
                    end = col.saturating_add(1);
                }
                cells.push(Cell {
                    text,
                    wide_continuation,
                });
            }
            let wrapped = screen.row_wrapped(visible_row);
            if wrapped {
                end = capture_cols;
            }
            rows.push(Row {
                cells,
                wrapped,
                end,
            });
        }
        screen.set_scrollback(0);

        let mut document = Self {
            screen,
            rows,
            marks,
            history_len,
            capture_cols,
            alternate_screen,
            flat_text: String::new(),
            flat_positions: Vec::new(),
            positions: Vec::new(),
        };
        document.build_search_text();
        let initial = document.clamp(initial);
        let max_top = document.max_viewport_top(usize::from(capture_rows));
        (document, initial, initial_top.min(max_top))
    }

    #[cfg(test)]
    pub(crate) fn from_text(rows: u16, cols: u16, text: &[u8]) -> Self {
        let mut view = View::new(rows, cols);
        view.process_changes(text);
        Self::capture(&mut view).0
    }

    pub(crate) fn row_count(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn capture_cols(&self) -> u16 {
        self.capture_cols
    }

    pub(crate) fn max_viewport_top(&self, height: usize) -> usize {
        self.row_count().saturating_sub(height.max(1))
    }

    pub(crate) fn clamp(&self, mut position: HistoryPosition) -> HistoryPosition {
        position.row = position.row.min(self.row_count().saturating_sub(1));
        position.col = self.clamp_col(position.row, position.col);
        position
    }

    fn clamp_col(&self, row: usize, col: u16) -> u16 {
        let last = self.line_last_col(row);
        let mut col = col.min(last);
        while col > 0
            && self
                .cell(row, col)
                .is_some_and(|cell| cell.wide_continuation)
        {
            col -= 1;
        }
        col
    }

    pub(crate) fn line_last_col(&self, row: usize) -> u16 {
        self.rows
            .get(row)
            .map_or(0, |row| row.end.saturating_sub(1))
    }

    pub(crate) fn line_first_nonblank(&self, row: usize) -> u16 {
        let Some(row) = self.rows.get(row) else {
            return 0;
        };
        row.cells
            .iter()
            .take(usize::from(row.end))
            .position(|cell| !cell.text.chars().all(char::is_whitespace) && !cell.text.is_empty())
            .unwrap_or(0) as u16
    }

    pub(crate) fn line_text(&self, row: usize) -> String {
        let Some(row) = self.rows.get(row) else {
            return String::new();
        };
        let mut text = String::new();
        for cell in row.cells.iter().take(usize::from(row.end)) {
            if cell.wide_continuation {
                continue;
            }
            if cell.text.is_empty() {
                text.push(' ');
            } else {
                text.push_str(&cell.text);
            }
        }
        text
    }

    pub(crate) fn cell_text(&self, position: HistoryPosition) -> &str {
        self.cell(position.row, position.col)
            .map_or("", |cell| cell.text.as_str())
    }

    fn cell(&self, row: usize, col: u16) -> Option<&Cell> {
        self.rows.get(row)?.cells.get(usize::from(col))
    }

    pub(crate) fn move_horizontal(
        &self,
        mut position: HistoryPosition,
        forward: bool,
        count: usize,
    ) -> Option<HistoryPosition> {
        let original = position;
        for _ in 0..count {
            if forward {
                let last = self.line_last_col(position.row);
                if position.col >= last {
                    break;
                }
                position.col += 1;
                while position.col < last
                    && self
                        .cell(position.row, position.col)
                        .is_some_and(|cell| cell.wide_continuation)
                {
                    position.col += 1;
                }
            } else {
                if position.col == 0 {
                    break;
                }
                position.col -= 1;
                while position.col > 0
                    && self
                        .cell(position.row, position.col)
                        .is_some_and(|cell| cell.wide_continuation)
                {
                    position.col -= 1;
                }
            }
        }
        (position != original).then_some(position)
    }

    pub(crate) fn move_vertical(
        &self,
        position: HistoryPosition,
        down: bool,
        count: usize,
    ) -> Option<HistoryPosition> {
        let row = if down {
            position
                .row
                .saturating_add(count)
                .min(self.row_count().saturating_sub(1))
        } else {
            position.row.saturating_sub(count)
        };
        (row != position.row).then(|| HistoryPosition {
            row,
            col: self.clamp_col(row, position.col),
        })
    }

    pub(crate) fn move_word(
        &self,
        mut position: HistoryPosition,
        movement: WordMove,
        style: WordStyle,
        count: usize,
    ) -> Option<HistoryPosition> {
        let original = position;
        for _ in 0..count {
            position = match movement {
                WordMove::ForwardStart => self.next_word_start(position, style)?,
                WordMove::BackwardStart => self.previous_word_start(position, style)?,
                WordMove::ForwardEnd => self.next_word_end(position, style)?,
            };
        }
        (position != original).then_some(position)
    }

    fn next_word_start(
        &self,
        position: HistoryPosition,
        style: WordStyle,
    ) -> Option<HistoryPosition> {
        let cells = &self.positions;
        let index = cells.iter().position(|value| *value == position)?;
        let mut i = index + 1;
        while i < cells.len() && self.same_word(cells[i - 1], cells[i], style) {
            i += 1;
        }
        while i < cells.len() && self.word_class(cells[i], style) == 0 {
            i += 1;
        }
        cells.get(i).copied()
    }

    fn previous_word_start(
        &self,
        position: HistoryPosition,
        style: WordStyle,
    ) -> Option<HistoryPosition> {
        let cells = &self.positions;
        let index = cells.iter().position(|value| *value == position)?;
        let mut i = index.checked_sub(1)?;
        while i > 0 && self.word_class(cells[i], style) == 0 {
            i -= 1;
        }
        let class = self.word_class(cells[i], style);
        while i > 0
            && self.word_class(cells[i - 1], style) == class
            && self.same_logical_line(cells[i - 1], cells[i])
        {
            i -= 1;
        }
        Some(cells[i])
    }

    fn next_word_end(
        &self,
        position: HistoryPosition,
        style: WordStyle,
    ) -> Option<HistoryPosition> {
        let cells = &self.positions;
        let index = cells.iter().position(|value| *value == position)?;
        let mut i = index + 1;
        while i < cells.len() && self.word_class(cells[i], style) == 0 {
            i += 1;
        }
        let class = self.word_class(*cells.get(i)?, style);
        while i + 1 < cells.len()
            && self.word_class(cells[i + 1], style) == class
            && self.same_logical_line(cells[i], cells[i + 1])
        {
            i += 1;
        }
        cells.get(i).copied()
    }

    fn word_class(&self, position: HistoryPosition, style: WordStyle) -> u8 {
        let text = self.cell_text(position);
        let Some(ch) = text.chars().next() else {
            return 0;
        };
        if ch.is_whitespace() {
            0
        } else if style == WordStyle::BigWord || ch.is_alphanumeric() || ch == '_' {
            1
        } else {
            2
        }
    }

    fn same_word(&self, left: HistoryPosition, right: HistoryPosition, style: WordStyle) -> bool {
        self.same_logical_line(left, right)
            && self.word_class(left, style) == self.word_class(right, style)
    }

    fn same_logical_line(&self, left: HistoryPosition, right: HistoryPosition) -> bool {
        left.row == right.row
            || (left.row.saturating_add(1) == right.row
                && self.rows.get(left.row).is_some_and(|row| row.wrapped))
    }

    pub(crate) fn find_character(
        &self,
        position: HistoryPosition,
        target: char,
        forward: bool,
        till: bool,
        count: usize,
    ) -> Option<HistoryPosition> {
        let range = self.logical_line_range(position.row);
        let mut matches = Vec::new();
        for row in range {
            let end = self.rows[row].end;
            for col in 0..end {
                let candidate = HistoryPosition { row, col };
                if self.cell_text(candidate).starts_with(target)
                    && if forward {
                        candidate > position
                    } else {
                        candidate < position
                    }
                {
                    matches.push(candidate);
                }
            }
        }
        if !forward {
            matches.reverse();
        }
        let mut found = *matches.get(count.saturating_sub(1))?;
        if till {
            found = self.adjacent(found, !forward)?;
        }
        (found != position).then_some(found)
    }

    pub(crate) fn matching_brace(&self, position: HistoryPosition) -> Option<HistoryPosition> {
        let line = self.logical_line_range(position.row);
        let mut start = None;
        'outer: for row in line {
            let first_col = if row == position.row { position.col } else { 0 };
            for col in first_col..self.rows[row].end {
                let candidate = HistoryPosition { row, col };
                if matches!(
                    self.cell_char(candidate),
                    Some('(' | ')' | '[' | ']' | '{' | '}')
                ) {
                    start = Some(candidate);
                    break 'outer;
                }
            }
        }
        let start = start?;
        let ch = self.cell_char(start)?;
        let (mate, forward) = match ch {
            '(' => (')', true),
            '[' => (']', true),
            '{' => ('}', true),
            ')' => ('(', false),
            ']' => ('[', false),
            '}' => ('{', false),
            _ => return None,
        };
        let positions = &self.positions;
        let start_index = positions.iter().position(|value| *value == start)?;
        let mut depth = 0usize;
        if forward {
            for &candidate in positions.iter().skip(start_index + 1) {
                match self.cell_char(candidate) {
                    Some(value) if value == ch => depth += 1,
                    Some(value) if value == mate && depth == 0 => return Some(candidate),
                    Some(value) if value == mate => depth -= 1,
                    _ => {}
                }
            }
        } else {
            for &candidate in positions.iter().take(start_index).rev() {
                match self.cell_char(candidate) {
                    Some(value) if value == ch => depth += 1,
                    Some(value) if value == mate && depth == 0 => return Some(candidate),
                    Some(value) if value == mate => depth -= 1,
                    _ => {}
                }
            }
        }
        None
    }

    fn cell_char(&self, position: HistoryPosition) -> Option<char> {
        self.cell_text(position).chars().next()
    }

    fn logical_line_range(&self, row: usize) -> Range<usize> {
        let mut start = row.min(self.row_count().saturating_sub(1));
        while start > 0 && self.rows[start - 1].wrapped {
            start -= 1;
        }
        let mut end = row.saturating_add(1).min(self.row_count());
        while end < self.row_count() && self.rows[end - 1].wrapped {
            end += 1;
        }
        start..end
    }

    pub(crate) fn adjacent(
        &self,
        position: HistoryPosition,
        forward: bool,
    ) -> Option<HistoryPosition> {
        let positions = &self.positions;
        let index = positions.iter().position(|value| *value == position)?;
        if forward {
            positions.get(index + 1).copied()
        } else {
            index.checked_sub(1).and_then(|i| positions.get(i).copied())
        }
    }

    pub(crate) fn prompt(
        &self,
        position: HistoryPosition,
        forward: bool,
        count: usize,
    ) -> Option<HistoryPosition> {
        let mut positions = self
            .marks
            .iter()
            .filter(|mark| {
                mark.alternate_screen == self.alternate_screen
                    && matches!(mark.kind, Osc133Kind::PromptStart)
            })
            .map(|mark| mark.position)
            .filter(|candidate| {
                if forward {
                    *candidate > position
                } else {
                    *candidate < position
                }
            })
            .collect::<Vec<_>>();
        if !forward {
            positions.reverse();
        }
        positions
            .get(count.saturating_sub(1))
            .copied()
            .map(|p| self.clamp(p))
    }

    pub(crate) fn search(
        &self,
        query: &str,
        position: HistoryPosition,
        direction: SearchDirection,
        count: usize,
    ) -> Option<HistoryPosition> {
        let regex = RegexBuilder::new(query).multi_line(true).build().ok()?;
        let matches = regex
            .find_iter(&self.flat_text)
            .filter_map(|found| self.position_for_offset(found.start()))
            .collect::<Vec<_>>();
        if matches.is_empty() {
            return None;
        }
        let ordered = match direction {
            SearchDirection::Forward => matches
                .iter()
                .copied()
                .filter(|candidate| *candidate > position)
                .chain(
                    matches
                        .iter()
                        .copied()
                        .filter(|candidate| *candidate <= position),
                )
                .collect::<Vec<_>>(),
            SearchDirection::Backward => matches
                .iter()
                .copied()
                .rev()
                .filter(|candidate| *candidate < position)
                .chain(
                    matches
                        .iter()
                        .copied()
                        .rev()
                        .filter(|candidate| *candidate >= position),
                )
                .collect::<Vec<_>>(),
        };
        let target = *ordered.get(count.saturating_sub(1) % ordered.len())?;
        (target != position).then_some(target)
    }

    fn build_search_text(&mut self) {
        self.flat_text.clear();
        self.flat_positions.clear();
        self.positions.clear();
        for (row_index, row) in self.rows.iter_mut().enumerate() {
            let addressable_end = row.end.max(1);
            for col in 0..addressable_end {
                if !row.cells[usize::from(col)].wide_continuation {
                    self.positions.push(HistoryPosition {
                        row: row_index,
                        col,
                    });
                }
            }
            for col in 0..row.end {
                let cell = &row.cells[usize::from(col)];
                if cell.wide_continuation {
                    continue;
                }
                let text = if cell.text.is_empty() {
                    " "
                } else {
                    cell.text.as_str()
                };
                self.flat_text.push_str(text);
                self.flat_positions.extend(std::iter::repeat_n(
                    HistoryPosition {
                        row: row_index,
                        col,
                    },
                    text.len(),
                ));
            }
            if !row.wrapped {
                self.flat_text.push('\n');
                self.flat_positions.push(HistoryPosition {
                    row: row_index,
                    col: 0,
                });
            }
        }
    }

    fn position_for_offset(&self, offset: usize) -> Option<HistoryPosition> {
        self.flat_positions.get(offset).copied().or_else(|| {
            self.rows.last().map(|_| {
                self.clamp(HistoryPosition {
                    row: self.row_count().saturating_sub(1),
                    col: 0,
                })
            })
        })
    }

    pub(crate) fn yank_range(
        &self,
        first: HistoryPosition,
        last: HistoryPosition,
        linewise: bool,
    ) -> Option<String> {
        let (mut first, mut last) = if first <= last {
            (first, last)
        } else {
            (last, first)
        };
        if linewise {
            first.col = 0;
            last.col = self.line_last_col(last.row);
        }
        let mut text = String::new();
        for row_index in first.row..=last.row {
            let start = if row_index == first.row { first.col } else { 0 };
            let end = if row_index == last.row {
                last.col.saturating_add(1)
            } else {
                self.rows[row_index].end
            };
            for col in start..end.min(self.rows[row_index].end) {
                let cell = &self.rows[row_index].cells[usize::from(col)];
                if cell.wide_continuation {
                    continue;
                }
                if cell.text.is_empty() {
                    text.push(' ');
                } else {
                    text.push_str(&cell.text);
                }
            }
            while text.ends_with(' ') && (linewise || row_index != last.row) {
                text.pop();
            }
            if row_index != last.row && !self.rows[row_index].wrapped {
                text.push('\n');
            }
        }
        if linewise {
            text.push('\n');
        }
        (!text.is_empty()).then_some(text)
    }

    pub(crate) fn inner_word_range(
        &self,
        position: HistoryPosition,
        style: WordStyle,
        around: bool,
        count: usize,
    ) -> Option<(HistoryPosition, HistoryPosition)> {
        let positions = &self.positions;
        let mut index = positions
            .iter()
            .position(|candidate| *candidate == position)?;
        if self.word_class(positions[index], style) == 0 {
            while index < positions.len() && self.word_class(positions[index], style) == 0 {
                index += 1;
            }
        }
        let class = self.word_class(*positions.get(index)?, style);
        if class == 0 {
            return None;
        }
        let mut start = index;
        while start > 0
            && self.word_class(positions[start - 1], style) == class
            && self.same_logical_line(positions[start - 1], positions[start])
        {
            start -= 1;
        }
        let mut end = index;
        for word_index in 0..count {
            while end + 1 < positions.len()
                && self.word_class(positions[end + 1], style) == class
                && self.same_logical_line(positions[end], positions[end + 1])
            {
                end += 1;
            }
            if word_index + 1 < count {
                let mut next = end + 1;
                while next < positions.len() && self.word_class(positions[next], style) == 0 {
                    next += 1;
                }
                if next >= positions.len() {
                    break;
                }
                end = next;
            }
        }
        if around {
            let mut trailing = end + 1;
            while trailing < positions.len() && self.word_class(positions[trailing], style) == 0 {
                end = trailing;
                trailing += 1;
            }
            if end == index {
                while start > 0 && self.word_class(positions[start - 1], style) == 0 {
                    start -= 1;
                }
            }
        }
        Some((positions[start], positions[end]))
    }

    pub(crate) fn previous_position(&self, position: HistoryPosition) -> Option<HistoryPosition> {
        let index = self
            .positions
            .iter()
            .position(|candidate| *candidate == position)?;
        index
            .checked_sub(1)
            .and_then(|index| self.positions.get(index).copied())
    }

    pub(crate) fn formatted_row(&mut self, absolute_row: usize, width: u16) -> Vec<u8> {
        if absolute_row >= self.row_count() {
            return Vec::new();
        }
        let (offset, visible_row) = if absolute_row < self.history_len {
            (self.history_len - absolute_row, 0usize)
        } else {
            (0, absolute_row - self.history_len)
        };
        self.screen.set_scrollback(offset);
        self.screen
            .rows_formatted(0, width.min(self.capture_cols))
            .nth(visible_row)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::{ReviewDocument, SearchDirection, WordMove, WordStyle};
    use crate::{perform::HistoryPosition, view::View};

    fn pos(row: usize, col: u16) -> HistoryPosition {
        HistoryPosition { row, col }
    }

    #[test]
    fn snapshot_retains_scrollback_and_does_not_change_with_source() {
        let mut source = View::new(2, 20);
        source.process_changes(b"one\r\ntwo\r\nthree");
        let (document, _, _) = ReviewDocument::capture(&mut source);
        source.process_changes(b"\r\nfour");

        assert_eq!(document.row_count(), 3);
        assert_eq!(document.line_text(0), "one");
        assert_eq!(document.line_text(2), "three");
    }

    #[test]
    fn word_motions_distinguish_words_and_big_words() {
        let document = ReviewDocument::from_text(2, 30, b"one.two  three");
        assert_eq!(
            document.move_word(pos(0, 0), WordMove::ForwardStart, WordStyle::Word, 1),
            Some(pos(0, 3))
        );
        assert_eq!(
            document.move_word(pos(0, 0), WordMove::ForwardStart, WordStyle::BigWord, 1),
            Some(pos(0, 9))
        );
        assert_eq!(
            document.move_word(pos(0, 9), WordMove::BackwardStart, WordStyle::Word, 1),
            Some(pos(0, 4))
        );
        assert_eq!(
            document.move_word(pos(0, 0), WordMove::ForwardEnd, WordStyle::Word, 1),
            Some(pos(0, 2))
        );
    }

    #[test]
    fn matches_nested_braces_and_reports_missing_mates() {
        let document = ReviewDocument::from_text(2, 30, b"x {(a[b])} y {broken");
        assert_eq!(document.matching_brace(pos(0, 2)), Some(pos(0, 9)));
        assert_eq!(document.matching_brace(pos(0, 4)), Some(pos(0, 7)));
        assert_eq!(document.matching_brace(pos(0, 14)), None);
    }

    #[test]
    fn character_find_is_bounded_by_the_logical_line() {
        let document = ReviewDocument::from_text(3, 20, b"azbz\r\nz");
        assert_eq!(
            document.find_character(pos(0, 0), 'z', true, false, 2),
            Some(pos(0, 3))
        );
        assert_eq!(
            document.find_character(pos(0, 3), 'z', true, false, 1),
            None
        );
    }

    #[test]
    fn regex_search_wraps_in_both_directions() {
        let document = ReviewDocument::from_text(3, 20, b"alpha\r\nbeta alpha\r\ngamma");
        assert_eq!(
            document.search("alpha", pos(0, 0), SearchDirection::Forward, 1),
            Some(pos(1, 5))
        );
        assert_eq!(
            document.search("alpha", pos(0, 0), SearchDirection::Backward, 1),
            Some(pos(1, 5))
        );
        assert_eq!(
            document.search("[", pos(0, 0), SearchDirection::Forward, 1),
            None
        );
        assert_eq!(
            document.search("absent", pos(0, 0), SearchDirection::Forward, 1),
            None
        );
    }

    #[test]
    fn yank_respects_hard_and_soft_line_boundaries() {
        let document = ReviewDocument::from_text(3, 5, b"abcdef\r\nxy");
        assert_eq!(
            document.yank_range(pos(0, 0), pos(1, 0), false),
            Some("abcdef".into())
        );
        assert_eq!(
            document.yank_range(pos(1, 0), pos(2, 1), false),
            Some("f\nxy".into())
        );
    }
}
