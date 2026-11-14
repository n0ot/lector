use crate::{
    terminal::{
        Color, HistoryPosition, SemanticKind as Osc133Kind, SemanticMark as Osc133Mark, Style,
        TerminalSnapshot,
    },
    view::View,
};
use regex::RegexBuilder;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SearchMatch {
    pub(crate) position: HistoryPosition,
    pub(crate) ordinal: usize,
    pub(crate) total: usize,
    pub(crate) wrapped: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SearchMode {
    Regex,
    Word { whole_keyword: bool },
}

pub(crate) struct WordSearch {
    pub query: String,
    pub origin: HistoryPosition,
    pub mode: SearchMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SearchFailure {
    InvalidPattern,
    NotFound,
}

#[derive(Clone, Debug)]
struct Cell {
    text: String,
    wide_continuation: bool,
    style: Style,
    hyperlink: Option<String>,
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

fn is_keyword_character(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
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
    rows: Vec<Row>,
    marks: Vec<Osc133Mark>,
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
        let snapshot = view.snapshot_with_history();
        let document = Self::from_snapshot(snapshot, capture_cols);
        let initial = document.clamp(initial);
        let max_top = document.max_viewport_top(usize::from(capture_rows));
        (document, initial, initial_top.min(max_top))
    }

    pub(crate) fn from_snapshot(snapshot: TerminalSnapshot, capture_cols: u16) -> Self {
        let alternate_screen = snapshot.alternate_screen();
        let marks = snapshot.semantic_marks.clone();
        let mut rows = Vec::with_capacity(snapshot.scrollback.len() + snapshot.rows.len());
        for source_row in snapshot.scrollback.iter().chain(snapshot.rows.iter()) {
            let mut cells = Vec::with_capacity(usize::from(capture_cols));
            let mut end = 0;
            for col in 0..capture_cols {
                let cell = source_row.cells.get(usize::from(col));
                let text = cell.map_or("", |cell| cell.contents()).to_string();
                let wide_continuation = cell.is_some_and(|cell| cell.is_wide_continuation());
                if !text.is_empty() || wide_continuation {
                    end = col.saturating_add(1);
                }
                cells.push(Cell {
                    text,
                    wide_continuation,
                    hyperlink: cell.and_then(|cell| cell.hyperlink.clone()),
                    style: cell.map_or_else(Style::default, |cell| cell.style.clone()),
                });
            }
            let wrapped = source_row.wrapped;
            if wrapped {
                end = capture_cols;
            }
            rows.push(Row {
                cells,
                wrapped,
                end,
            });
        }
        let mut document = Self {
            rows,
            marks,
            capture_cols,
            alternate_screen,
            flat_text: String::new(),
            flat_positions: Vec::new(),
            positions: Vec::new(),
        };
        document.build_search_text();
        document
    }

    /// Build generated help as inert text rather than feeding user-supplied
    /// descriptions into a terminal parser. Return each entry's physical rows
    /// so refreshed documents can preserve a stable entry and offset.
    pub(crate) fn from_lines(lines: &[String], cols: u16) -> (Self, Vec<Range<usize>>) {
        let cols = cols.max(2);
        let mut rows = Vec::new();
        let mut spans = Vec::new();
        for line in lines {
            let start = rows.len();
            let mut row = Row {
                cells: Vec::new(),
                wrapped: false,
                end: 0,
            };
            let text: String = line
                .chars()
                .flat_map(|ch| {
                    if ch.is_control() {
                        ch.escape_default().collect::<Vec<_>>()
                    } else {
                        vec![ch]
                    }
                })
                .collect();
            for grapheme in text.graphemes(true) {
                let width = grapheme.width().clamp(1, 2) as u16;
                if u32::from(row.end) + u32::from(width) > u32::from(cols) {
                    // A wide grapheme can leave an unused tail cell. Padding
                    // is inert and keeps wrapped row coordinates rectangular.
                    while row.end < cols {
                        row.cells.push(Cell {
                            text: " ".into(),
                            wide_continuation: false,
                            style: Style::default(),
                            hyperlink: None,
                        });
                        row.end += 1;
                    }
                    row.wrapped = true;
                    rows.push(row);
                    row = Row {
                        cells: Vec::new(),
                        wrapped: false,
                        end: 0,
                    };
                }
                row.cells.push(Cell {
                    text: grapheme.to_owned(),
                    wide_continuation: false,
                    style: Style::default(),
                    hyperlink: None,
                });
                if width == 2 {
                    row.cells.push(Cell {
                        text: String::new(),
                        wide_continuation: true,
                        style: Style::default(),
                        hyperlink: None,
                    });
                }
                row.end += width;
            }
            rows.push(row);
            spans.push(start..rows.len());
        }
        if rows.is_empty() {
            rows.push(Row {
                cells: Vec::new(),
                wrapped: false,
                end: 0,
            });
        }
        for row in &mut rows {
            row.cells.resize_with(usize::from(cols), || Cell {
                text: " ".into(),
                wide_continuation: false,
                style: Style::default(),
                hyperlink: None,
            });
        }
        let mut document = Self {
            rows,
            marks: Vec::new(),
            capture_cols: cols,
            alternate_screen: false,
            flat_text: String::new(),
            flat_positions: Vec::new(),
            positions: Vec::new(),
        };
        document.build_search_text();
        (document, spans)
    }

    #[cfg(test)]
    pub(crate) fn from_text(rows: u16, cols: u16, text: &[u8]) -> Self {
        let mut view = View::new(rows, cols);
        view.process_changes(text);
        Self::capture(&mut view).0
    }

    /// Resolve only the exact cursor cell, using frozen OSC 8 metadata before
    /// conservative plain-text detection. Search text already joins soft wraps
    /// and inserts a newline at every hard row boundary.
    pub(crate) fn link_at(&self, position: HistoryPosition) -> Option<String> {
        let cell = self.cell(position.row, position.col)?;
        if let Some(uri) = &cell.hyperlink {
            return Some(uri.clone());
        }
        let position = if cell.wide_continuation {
            HistoryPosition {
                row: position.row,
                col: self.clamp_col(position.row, position.col),
            }
        } else {
            position
        };
        if let Some(uri) = &self.cell(position.row, position.col)?.hyperlink {
            return Some(uri.clone());
        }
        let offset = self.flat_positions.iter().position(|p| *p == position)?;
        super::links::plain_url_at(&self.flat_text, offset).map(str::to_owned)
    }

    pub(crate) fn row_count(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn max_viewport_top(&self, height: usize) -> usize {
        self.row_count().saturating_sub(height.max(1))
    }

    pub(crate) fn max_viewport_left(&self, width: u16) -> u16 {
        self.rows
            .iter()
            .map(|row| row.end)
            .max()
            .unwrap_or(0)
            .max(1)
            .saturating_sub(width.max(1))
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

    pub(crate) fn capture_cols(&self) -> u16 {
        self.capture_cols
    }

    pub(crate) fn line_end(&self, row: usize) -> u16 {
        self.rows.get(row).map_or(0, |row| row.end)
    }

    pub(crate) fn is_wrapped_row(&self, row: usize) -> bool {
        self.rows.get(row).is_some_and(|row| row.wrapped)
    }

    pub(crate) fn text_between(&self, row: usize, start: u16, end: u16) -> String {
        let Some(source) = self.rows.get(row) else {
            return String::new();
        };
        let mut text = String::new();
        for cell in source
            .cells
            .iter()
            .skip(usize::from(start.min(self.capture_cols)))
            .take(usize::from(
                end.min(self.capture_cols)
                    .saturating_sub(start.min(self.capture_cols)),
            ))
        {
            if cell.wide_continuation {
                continue;
            }
            if cell.text.is_empty() {
                text.push(' ');
            } else {
                text.push_str(&cell.text);
            }
        }
        text.trim().to_owned()
    }

    pub(crate) fn first_text_col(&self, row: usize, start: u16, end: u16) -> Option<u16> {
        let source = self.rows.get(row)?;
        (start.min(self.capture_cols)..end.min(self.capture_cols)).find(|col| {
            source.cells.get(usize::from(*col)).is_some_and(|cell| {
                !cell.wide_continuation
                    && !cell.text.is_empty()
                    && !cell.text.chars().all(char::is_whitespace)
            })
        })
    }

    pub(crate) fn cell_text(&self, position: HistoryPosition) -> &str {
        self.cell(position.row, position.col)
            .map_or("", |cell| cell.text.as_str())
    }

    /// Describe the destination from document coordinates, independently of
    /// scrolling, clipping, or the physical width of a Unicode grapheme.
    pub(crate) fn cursor_feedback(
        &self,
        previous: Option<HistoryPosition>,
        current: HistoryPosition,
    ) -> crate::application_accessibility::ApplicationAccessibilitySpeech {
        use crate::application_accessibility::ApplicationAccessibilitySpeech;
        if previous.is_none_or(|previous| previous.row != current.row) {
            let line = self.line_text(current.row);
            return ApplicationAccessibilitySpeech {
                text: if line.trim().is_empty() {
                    "blank".into()
                } else {
                    line
                },
                indentation: Some(self.line_first_nonblank(current.row)),
            };
        }
        let previous = previous.unwrap();
        let distance = self.rows.get(current.row).map_or(0, |row| {
            row.cells
                .iter()
                .skip(usize::from(previous.col.min(current.col)))
                .take(usize::from(previous.col.abs_diff(current.col)))
                .filter(|cell| !cell.wide_continuation)
                .count()
        });
        let word = self.keyword_bounds(current);
        let text = if distance > 1
            && let Some((start, end)) = word
            && Some(start) != self.keyword_bounds(previous).map(|bounds| bounds.0)
        {
            self.text_between(current.row, start, end)
        } else {
            match self.cell_text(current) {
                "" => "blank".into(),
                " " => "space".into(),
                "\t" => "tab".into(),
                text if text.chars().all(char::is_whitespace) => "blank".into(),
                text => text.into(),
            }
        };
        ApplicationAccessibilitySpeech {
            text,
            indentation: None,
        }
    }

    pub(crate) fn search_feedback(
        &self,
        previous: HistoryPosition,
        current: HistoryPosition,
    ) -> crate::application_accessibility::ApplicationAccessibilitySpeech {
        if previous.row != current.row {
            return self.cursor_feedback(None, current);
        }
        let mut feedback = self.cursor_feedback(Some(current), current);
        if let Some((start, end)) = self.keyword_bounds(current) {
            feedback.text = self.text_between(current.row, start, end);
        }
        feedback
    }

    fn keyword_bounds(&self, position: HistoryPosition) -> Option<(u16, u16)> {
        if self.word_class(position, WordStyle::Word) != 1 {
            return None;
        }
        let row = self.rows.get(position.row)?;
        let mut start = position.col;
        while start > 0 {
            let col = start - 1;
            if !row.cells[usize::from(col)].wide_continuation
                && self.word_class(
                    HistoryPosition {
                        row: position.row,
                        col,
                    },
                    WordStyle::Word,
                ) != 1
            {
                break;
            }
            start = col;
        }
        let mut end = position.col + 1;
        while end < row.end {
            if !row.cells[usize::from(end)].wide_continuation
                && self.word_class(
                    HistoryPosition {
                        row: position.row,
                        col: end,
                    },
                    WordStyle::Word,
                ) != 1
            {
                break;
            }
            end += 1;
        }
        Some((start, end))
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
                let mut next = position.col + 1;
                while next <= last
                    && self
                        .cell(position.row, next)
                        .is_some_and(|cell| cell.wide_continuation)
                {
                    next += 1;
                }
                if next > last {
                    break;
                }
                position.col = next;
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
        } else if style == WordStyle::BigWord || is_keyword_character(ch) {
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

    /// Select a keyword under/after the cursor on this logical line, falling
    /// back to the nonblank word under/after it when no keyword remains.
    pub(crate) fn word_search(&self, position: HistoryPosition, whole: bool) -> Option<WordSearch> {
        let position = self.clamp(position);
        let line = self.logical_line_range(position.row);
        let cells: Vec<_> = self
            .positions
            .iter()
            .copied()
            .filter(|p| line.contains(&p.row))
            .collect();
        let current = cells.iter().position(|p| *p >= position)?;
        let keyword =
            (current..cells.len()).find(|&i| self.word_class(cells[i], WordStyle::Word) == 1);
        let (index, style) = if let Some(index) = keyword {
            (index, WordStyle::Word)
        } else {
            (
                (current..cells.len())
                    .find(|&i| self.word_class(cells[i], WordStyle::BigWord) != 0)?,
                WordStyle::BigWord,
            )
        };
        let mut first = index;
        let mut last = index;
        while first > 0 && self.word_class(cells[first - 1], style) == 1 {
            first -= 1;
        }
        while last + 1 < cells.len() && self.word_class(cells[last + 1], style) == 1 {
            last += 1;
        }
        let text: String = cells[first..=last]
            .iter()
            .map(|p| self.cell_text(*p))
            .collect();
        Some(WordSearch {
            query: regex::escape(&text),
            origin: cells[first],
            mode: SearchMode::Word {
                whole_keyword: whole && keyword.is_some(),
            },
        })
    }

    #[cfg(test)]
    pub(crate) fn search(
        &self,
        query: &str,
        position: HistoryPosition,
        direction: SearchDirection,
        count: usize,
    ) -> Result<SearchMatch, SearchFailure> {
        self.search_with_mode(query, position, direction, count, SearchMode::Regex)
    }

    pub(crate) fn search_with_mode(
        &self,
        query: &str,
        position: HistoryPosition,
        direction: SearchDirection,
        count: usize,
        mode: SearchMode,
    ) -> Result<SearchMatch, SearchFailure> {
        let regex = RegexBuilder::new(query)
            .multi_line(true)
            .build()
            .map_err(|_| SearchFailure::InvalidPattern)?;
        // Literal word matches cannot split a displayed grapheme. Whole-word
        // boundaries use the same alphanumeric/underscore classification as
        // Review movement, rather than the regex engine's broader Unicode \w.
        let mut boundaries = Vec::new();
        if matches!(mode, SearchMode::Word { .. }) {
            boundaries.extend(self.flat_text.grapheme_indices(true).map(|(offset, text)| {
                (
                    offset,
                    text.chars().next().is_some_and(is_keyword_character),
                )
            }));
            boundaries.push((self.flat_text.len(), false));
        }
        let candidates: Box<dyn Iterator<Item = regex::Match<'_>> + '_> = match mode {
            SearchMode::Regex => Box::new(regex.find_iter(&self.flat_text)),
            SearchMode::Word { .. } => {
                let mut offset = Some(0);
                let regex = &regex;
                let boundaries = &boundaries;
                Box::new(std::iter::from_fn(move || {
                    let found = regex.find_at(&self.flat_text, offset?)?;
                    // A rejected partial-grapheme match must not hide a later
                    // overlapping match that begins at a complete grapheme.
                    let next = boundaries.partition_point(|(start, _)| *start <= found.start());
                    offset = boundaries.get(next).map(|(start, _)| *start);
                    Some(found)
                }))
            }
        };
        let matches = candidates
            .filter(|found| {
                let SearchMode::Word { whole_keyword } = mode else {
                    return true;
                };
                let Ok(start) =
                    boundaries.binary_search_by_key(&found.start(), |(offset, _)| *offset)
                else {
                    return false;
                };
                let Ok(end) = boundaries.binary_search_by_key(&found.end(), |(offset, _)| *offset)
                else {
                    return false;
                };
                !whole_keyword || ((start == 0 || !boundaries[start - 1].1) && !boundaries[end].1)
            })
            .filter_map(|found| self.position_for_offset(found.start()))
            .collect::<Vec<_>>();
        if matches.is_empty() {
            return Err(SearchFailure::NotFound);
        }
        let steps = count.max(1) - 1;
        let total = matches.len();
        // Newlines map to the start of their row, so regex offset order is
        // not necessarily cursor-position order. Partition by direction
        // without binary-searching the mapped positions.
        let mut ahead = Vec::new();
        let mut behind = Vec::new();
        let indices: Box<dyn Iterator<Item = usize>> = match direction {
            SearchDirection::Forward => Box::new(0..total),
            SearchDirection::Backward => Box::new((0..total).rev()),
        };
        for index in indices {
            let is_ahead = match direction {
                SearchDirection::Forward => matches[index] > position,
                SearchDirection::Backward => matches[index] < position,
            };
            if is_ahead {
                ahead.push(index);
            } else {
                behind.push(index);
            }
        }
        let wrapped = steps >= ahead.len();
        let index = ahead
            .into_iter()
            .chain(behind)
            .nth(steps % total)
            .expect("nonempty search matches");
        Ok(SearchMatch {
            position: matches[index],
            ordinal: index + 1,
            total,
            wrapped,
        })
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
        for (offset, row) in self.rows[first.row..=last.row].iter().enumerate() {
            let row_index = first.row + offset;
            let start = if row_index == first.row { first.col } else { 0 };
            let end = if row_index == last.row {
                last.col.saturating_add(1)
            } else {
                row.end
            };
            for col in start..end.min(row.end) {
                let cell = &row.cells[usize::from(col)];
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
            if row_index != last.row && !row.wrapped {
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

    pub(crate) fn formatted_row(&self, absolute_row: usize, first_col: u16, width: u16) -> Vec<u8> {
        let Some(row) = self.rows.get(absolute_row) else {
            return Vec::new();
        };
        let mut bytes = Vec::new();
        let mut style = Style::default();
        for (index, cell) in row
            .cells
            .iter()
            .enumerate()
            .skip(usize::from(first_col.min(self.capture_cols)))
            .take(usize::from(
                width.min(self.capture_cols.saturating_sub(first_col)),
            ))
        {
            if cell.wide_continuation {
                // If a horizontal viewport begins on the trailing half of a
                // wide cell, keep the remaining columns aligned.
                if index == usize::from(first_col) {
                    bytes.push(b' ');
                }
                continue;
            }
            if cell.style != style {
                write_style(&mut bytes, &cell.style);
                style = cell.style.clone();
            }
            if cell.text.is_empty() {
                bytes.push(b' ');
            } else {
                bytes.extend_from_slice(cell.text.as_bytes());
            }
        }
        bytes
    }
}

fn write_style(bytes: &mut Vec<u8>, style: &Style) {
    bytes.extend_from_slice(b"\x1B[0");
    if style.bold {
        bytes.extend_from_slice(b";1");
    }
    if style.dim {
        bytes.extend_from_slice(b";2");
    }
    if style.italic {
        bytes.extend_from_slice(b";3");
    }
    if style.underline != crate::terminal::UnderlineStyle::None {
        bytes.extend_from_slice(b";4");
    }
    if style.inverse {
        bytes.extend_from_slice(b";7");
    }
    write_color(bytes, style.foreground, true);
    write_color(bytes, style.background, false);
    bytes.push(b'm');
}

fn write_color(bytes: &mut Vec<u8>, color: Color, foreground: bool) {
    match color {
        Color::Default => {}
        Color::Indexed(index) => bytes.extend_from_slice(
            format!(";{};5;{index}", if foreground { 38 } else { 48 }).as_bytes(),
        ),
        Color::Rgb(red, green, blue) => bytes.extend_from_slice(
            format!(
                ";{};2;{red};{green};{blue}",
                if foreground { 38 } else { 48 }
            )
            .as_bytes(),
        ),
    }
}

#[cfg(test)]
mod link_tests {
    use super::*;

    #[test]
    fn frozen_osc8_links_override_label_text_and_remain_copyable() {
        let mut source = View::new(3, 40);
        source.process_changes(b"\x1b]8;;custom:destination\x07https://example.test\x1b]8;;\x07");
        let document = ReviewDocument::capture(&mut source).0;
        source.process_changes(b"\r\x1b[2Kreplacement");
        assert_eq!(
            document
                .link_at(HistoryPosition { row: 0, col: 4 })
                .as_deref(),
            Some("custom:destination")
        );
        assert_eq!(document.link_at(HistoryPosition { row: 0, col: 25 }), None);
    }

    #[test]
    fn plain_urls_join_soft_wraps_but_not_hard_newlines() {
        let document = ReviewDocument::from_text(4, 12, b"https://example.test/path");
        assert_eq!(
            document
                .link_at(HistoryPosition { row: 1, col: 3 })
                .as_deref(),
            Some("https://example.test/path")
        );
        let document = ReviewDocument::from_text(4, 40, b"https://example.test\r\n/path");
        assert_eq!(
            document
                .link_at(HistoryPosition { row: 0, col: 4 })
                .as_deref(),
            Some("https://example.test")
        );
        assert_eq!(document.link_at(HistoryPosition { row: 1, col: 2 }), None);
    }

    #[test]
    fn wide_link_labels_resolve_at_both_columns_and_in_history() {
        let document = ReviewDocument::from_text(
            2,
            20,
            "\x1b]8;;https://example.test\x07界\x1b]8;;\x07\r\nx\r\ny".as_bytes(),
        );
        for col in [0, 1] {
            assert_eq!(
                document.link_at(HistoryPosition { row: 0, col }).as_deref(),
                Some("https://example.test")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ReviewDocument, SearchDirection, WordMove, WordStyle};
    use crate::{terminal::HistoryPosition, view::View};

    fn pos(row: usize, col: u16) -> HistoryPosition {
        HistoryPosition { row, col }
    }

    #[test]
    fn word_search_selects_keyword_then_nonblank_fallback_on_logical_line() {
        let (document, _) =
            ReviewDocument::from_lines(&["  -- alpha.beta  [.*]".into(), "next".into()], 8);
        let selected = document.word_search(pos(0, 2), true).unwrap();
        assert_eq!(selected.query, "alpha");
        assert_eq!(selected.origin, pos(0, 5));
        assert_eq!(
            selected.mode,
            super::SearchMode::Word {
                whole_keyword: true
            }
        );
        let selected = document.word_search(pos(1, 2), true).unwrap();
        assert_eq!(selected.query, "beta");
        let selected = document.word_search(pos(2, 1), true).unwrap();
        assert_eq!(selected.query, regex::escape("[.*]"));
        assert_eq!(
            selected.mode,
            super::SearchMode::Word {
                whole_keyword: false
            }
        );
        let (blank, _) = ReviewDocument::from_lines(&["   ".into(), "later".into()], 10);
        assert!(blank.word_search(pos(0, 0), true).is_none());
    }

    #[test]
    fn word_search_boundaries_counts_and_literal_punctuation_are_preserved() {
        let (document, _) = ReviewDocument::from_lines(&["cat scatter cat cat".into()], 40);
        let word = document.word_search(pos(0, 1), true).unwrap();
        let find = |direction, count| {
            document
                .search_with_mode(&word.query, word.origin, direction, count, word.mode)
                .unwrap()
        };
        assert_eq!(find(SearchDirection::Forward, 1).position, pos(0, 12));
        assert_eq!(find(SearchDirection::Forward, 2).position, pos(0, 16));
        assert_eq!(find(SearchDirection::Backward, 1).position, pos(0, 16));
        let partial = document.word_search(pos(0, 0), false).unwrap();
        assert_eq!(
            document
                .search_with_mode(
                    &partial.query,
                    partial.origin,
                    SearchDirection::Forward,
                    1,
                    partial.mode
                )
                .unwrap()
                .position,
            pos(0, 5)
        );
        let (punctuation, _) = ReviewDocument::from_lines(&["[.*] [xx] [.*]".into()], 40);
        // At the final token no keyword remains to the right: fallback must
        // search punctuation literally, not interpret it as a regex.
        let word = punctuation.word_search(pos(0, 11), true).unwrap();
        assert_eq!(word.query, regex::escape("[.*]"));
        let found = punctuation
            .search_with_mode(
                &word.query,
                word.origin,
                SearchDirection::Forward,
                1,
                word.mode,
            )
            .unwrap();
        assert_eq!(found.position, pos(0, 0));
        assert_eq!(found.total, 2);
    }

    #[test]
    fn word_search_respects_unicode_graphemes_and_review_keyword_class() {
        let (document, _) = ReviewDocument::from_lines(&["界é 界éx 界é".into()], 5);
        let word = document.word_search(pos(0, 0), true).unwrap();
        let found = document
            .search_with_mode(
                &word.query,
                word.origin,
                SearchDirection::Forward,
                1,
                word.mode,
            )
            .unwrap();
        assert_eq!(found.total, 2);
        let (document, _) = ReviewDocument::from_lines(&["e e\u{301} e".into()], 40);
        let word = document.word_search(pos(0, 0), false).unwrap();
        let found = document
            .search_with_mode(
                &word.query,
                word.origin,
                SearchDirection::Forward,
                1,
                word.mode,
            )
            .unwrap();
        assert_eq!(found.position, pos(0, 4));
        assert_eq!(found.total, 2); // literal searches never split e + accent
        let (overlap, _) = ReviewDocument::from_lines(&["a\u{301}a a\u{301}a\u{301}a".into()], 40);
        let word = overlap.word_search(pos(0, 0), false).unwrap();
        let found = overlap
            .search_with_mode(
                &word.query,
                word.origin,
                SearchDirection::Forward,
                1,
                word.mode,
            )
            .unwrap();
        assert_eq!(found.position, pos(0, 4));
        assert_eq!(found.total, 2);
        let (document, _) = ReviewDocument::from_lines(&["foo e\u{301}foo foo".into()], 40);
        let word = document.word_search(pos(0, 0), true).unwrap();
        assert_eq!(
            document
                .search_with_mode(
                    &word.query,
                    word.origin,
                    SearchDirection::Forward,
                    1,
                    word.mode
                )
                .unwrap()
                .total,
            2
        );
    }

    #[test]
    fn generated_lines_are_inert_wrapped_and_allow_empty_rows() {
        let (document, spans) =
            ReviewDocument::from_lines(&["".into(), "ab界c".into(), "x\x1b[31my".into()], 4);
        assert_eq!(spans[0], 0..1);
        assert_eq!(spans[1], 1..3);
        assert_eq!(document.cell_text(HistoryPosition { row: 1, col: 2 }), "界");
        assert!(document.flat_text.contains("ab界c"));
        assert!(document.flat_text.contains(r"x\u{1b}[31my"));
        assert!(!document.flat_text.contains('\x1b'));
        let (empty, _) = ReviewDocument::from_lines(&[], 0);
        assert_eq!(empty.row_count(), 1);
    }

    #[test]
    fn cursor_feedback_uses_graphemes_words_and_explicit_whitespace() {
        let mut source = View::new(4, 40);
        source.process_changes("界a one two\r\n  indented\r\n\r\nlast".as_bytes());
        let (document, _, _) = ReviewDocument::capture(&mut source);
        // Crossing a double-width grapheme is a one-character move.
        assert_eq!(
            document.cursor_feedback(Some(pos(0, 0)), pos(0, 2)).text,
            "a"
        );
        assert_eq!(
            document.cursor_feedback(Some(pos(0, 2)), pos(0, 3)).text,
            "space"
        );
        assert_eq!(
            document.cursor_feedback(Some(pos(0, 2)), pos(0, 8)).text,
            "two"
        );
        let line = document.cursor_feedback(Some(pos(0, 8)), pos(1, 2));
        assert_eq!(line.text, "  indented");
        assert_eq!(line.indentation, Some(2));
        assert_eq!(
            document.cursor_feedback(Some(pos(1, 2)), pos(2, 0)).text,
            "blank"
        );
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
    fn search_feedback_reads_a_word_on_the_same_row_and_a_line_on_another() {
        let document = ReviewDocument::from_text(3, 30, b"a to z\r\nnext destination");
        assert_eq!(document.search_feedback(pos(0, 1), pos(0, 2)).text, "to");
        assert_eq!(document.search_feedback(pos(0, 2), pos(0, 2)).text, "to");
        assert_eq!(
            document.search_feedback(pos(0, 2), pos(1, 5)).text,
            "next destination"
        );
    }

    #[test]
    fn search_reports_position_wrap_and_failure_without_requiring_movement() {
        use super::SearchFailure;
        let document = ReviewDocument::from_text(2, 30, b"one two one");
        let found = document
            .search("one", pos(0, 0), SearchDirection::Forward, 1)
            .unwrap();
        assert_eq!(
            (found.position, found.ordinal, found.total, found.wrapped),
            (pos(0, 8), 2, 2, false)
        );
        let wrapped = document
            .search("one", found.position, SearchDirection::Forward, 1)
            .unwrap();
        assert_eq!(
            (wrapped.position, wrapped.ordinal, wrapped.wrapped),
            (pos(0, 0), 1, true)
        );
        let same = document
            .search("two", pos(0, 4), SearchDirection::Backward, 1)
            .unwrap();
        assert_eq!(
            (same.position, same.total, same.wrapped),
            (pos(0, 4), 1, true)
        );
        assert!(
            document
                .search("one", pos(0, 0), SearchDirection::Forward, 3)
                .unwrap()
                .wrapped
        );
        assert_eq!(
            document.search("[", pos(0, 0), SearchDirection::Forward, 1),
            Err(SearchFailure::InvalidPattern)
        );
        assert_eq!(
            document.search("absent", pos(0, 0), SearchDirection::Forward, 1),
            Err(SearchFailure::NotFound)
        );
    }

    #[test]
    fn regex_search_preserves_direction_with_line_boundary_matches() {
        let document = ReviewDocument::from_text(2, 20, b"a ab\r\na");
        for pattern in ["a|$", "a|\\n"] {
            let forward = document
                .search(pattern, pos(0, 0), SearchDirection::Forward, 1)
                .unwrap();
            assert_eq!(forward.position, pos(0, 2));
            assert_eq!(forward.ordinal, 2);
            assert!(!forward.wrapped);
            let backward = document
                .search(pattern, pos(0, 2), SearchDirection::Backward, 1)
                .unwrap();
            assert_eq!(backward.position, pos(0, 0));
            assert_eq!(backward.ordinal, 3);
            assert!(!backward.wrapped);
            for direction in [SearchDirection::Forward, SearchDirection::Backward] {
                let first = document.search(pattern, pos(0, 0), direction, 1).unwrap();
                let repeated = document
                    .search(pattern, pos(0, 0), direction, first.total + 1)
                    .unwrap();
                assert_eq!(repeated.position, first.position);
                assert_eq!(repeated.ordinal, first.ordinal);
                assert!(repeated.wrapped);
            }
        }
    }

    #[test]
    fn regex_search_wraps_in_both_directions() {
        let document = ReviewDocument::from_text(3, 20, b"alpha\r\nbeta alpha\r\ngamma");
        assert_eq!(
            document
                .search("alpha", pos(0, 0), SearchDirection::Forward, 1)
                .ok()
                .map(|found| found.position),
            Some(pos(1, 5))
        );
        assert_eq!(
            document
                .search("alpha", pos(0, 0), SearchDirection::Backward, 1)
                .ok()
                .map(|found| found.position),
            Some(pos(1, 5))
        );
        assert_eq!(
            document
                .search("[", pos(0, 0), SearchDirection::Forward, 1)
                .ok(),
            None
        );
        assert_eq!(
            document
                .search("absent", pos(0, 0), SearchDirection::Forward, 1)
                .ok(),
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

    #[test]
    fn horizontal_motion_never_lands_on_a_double_width_tail() {
        let document = ReviewDocument::from_text(1, 8, "a界b".as_bytes());
        assert_eq!(
            document.move_horizontal(pos(0, 0), true, 1),
            Some(pos(0, 1))
        );
        assert_eq!(
            document.move_horizontal(pos(0, 1), true, 1),
            Some(pos(0, 3))
        );
        assert_eq!(
            document.move_horizontal(pos(0, 3), false, 1),
            Some(pos(0, 1))
        );

        let document = ReviewDocument::from_text(1, 3, "a界".as_bytes());
        assert_eq!(document.move_horizontal(pos(0, 1), true, 1), None);
    }

    #[test]
    fn ghostty_backed_document_preserves_history_unicode_styles_and_copying() {
        use crate::terminal::GhosttyEngine;

        let mut engine =
            GhosttyEngine::new_with_scrollback(3, 12, 20).expect("create Ghostty engine");
        engine
            .advance("old\r\n\x1b[31mwide 界\x1b[0m\r\nlive e\u{301}\r\nfinal".as_bytes())
            .expect("advance review fixture");
        let snapshot = engine
            .normalized_snapshot_with_history()
            .expect("capture Ghostty history");
        let document = ReviewDocument::from_snapshot(snapshot, 12);

        assert_eq!(document.line_text(0), "old");
        assert_eq!(document.line_text(1), "wide 界");
        assert_eq!(document.line_text(2), "live e\u{301}");
        assert_eq!(document.line_text(3), "final");
        assert_eq!(document.move_horizontal(pos(1, 5), true, 1), None);
        assert_eq!(
            document.move_word(pos(0, 0), WordMove::ForwardStart, WordStyle::Word, 1),
            Some(pos(1, 0))
        );
        assert_eq!(
            document
                .search("final", pos(0, 0), SearchDirection::Forward, 1)
                .ok()
                .map(|found| found.position),
            Some(pos(3, 0))
        );
        assert_eq!(
            document.yank_range(pos(0, 0), pos(3, 4), false),
            Some("old\nwide 界\nlive e\u{301}\nfinal".into())
        );
        assert!(
            document
                .formatted_row(1, 0, 12)
                .starts_with(b"\x1b[0;38;5;1mwide")
        );

        engine.advance(b"\r\nnew output").unwrap();
        assert_eq!(document.line_text(0), "old");
        assert_eq!(document.row_count(), 4);
    }

    #[test]
    fn ghostty_backed_document_navigates_exact_semantic_prompts() {
        use crate::terminal::GhosttyEngine;

        let mut engine = GhosttyEngine::new_with_scrollback(3, 16, 20).unwrap();
        engine
            .advance(
                b"\x1b]133;A\x07$ \x1b]133;B\x07one\x1b]133;C\x07\r\nout\x1b]133;D;0\x07\r\n\x1b]133;A\x07$ ",
            )
            .unwrap();
        let document =
            ReviewDocument::from_snapshot(engine.normalized_snapshot_with_history().unwrap(), 16);

        assert_eq!(document.prompt(pos(0, 1), true, 1), Some(pos(2, 0)));
        assert_eq!(document.prompt(pos(2, 0), false, 1), Some(pos(0, 0)));
    }
}
