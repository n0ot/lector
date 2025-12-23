use crate::view::View;

#[derive(Clone, Debug)]
pub struct Column {
    pub start: u16,
    pub end: u16,
}

#[derive(Clone, Debug)]
pub struct TableModel {
    pub top: u16,
    pub bottom: u16,
    pub columns: Vec<Column>,
    pub header_row: Option<u16>,
}

#[derive(Clone, Debug)]
pub struct TableState {
    pub model: TableModel,
    pub current_col: usize,
}

impl TableModel {
    pub fn column_for_col(&self, col: u16) -> usize {
        for (idx, column) in self.columns.iter().enumerate() {
            if col >= column.start && col <= column.end {
                return idx;
            }
        }
        0
    }

    pub fn clamp_row(&self, row: u16) -> u16 {
        if row < self.top {
            self.top
        } else if row > self.bottom {
            self.bottom
        } else {
            row
        }
    }

    pub fn cell_text(&self, view: &View, row: u16, col_idx: usize) -> String {
        let Some(column) = self.columns.get(col_idx) else {
            return String::new();
        };
        let end = column.end.min(view.size().1.saturating_sub(1));
        let text = view
            .screen()
            .contents_between(row, column.start, row, end + 1);
        text.trim().to_string()
    }

    pub fn header_text(&self, view: &View, col_idx: usize) -> Option<String> {
        let header_row = self.header_row?;
        let text = self.cell_text(view, header_row, col_idx);
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }
}

pub fn detect(view: &View, row: u16) -> Option<TableModel> {
    let (rows, cols) = view.size();
    if rows == 0 || cols == 0 {
        return None;
    }

    let delimiter = detect_delimiter(view, row).or_else(|| detect_delimiter_nearby(view, row));
    let mut top = row;
    while top > 0 && row_is_in_table(view, top - 1, delimiter) {
        top -= 1;
    }

    let mut bottom = row;
    while bottom + 1 < rows && row_is_in_table(view, bottom + 1, delimiter) {
        bottom += 1;
    }

    let columns = if let Some(delim) = delimiter {
        detect_delimited_columns(view, top, bottom, delim)
    } else {
        detect_fixed_width_columns(view, top, bottom)
    }?;

    if columns.len() < 2 {
        return None;
    }

    let header_row = detect_header_row(view, top, bottom, delimiter);

    Some(TableModel {
        top,
        bottom,
        columns,
        header_row,
    })
}

fn detect_delimiter(view: &View, row: u16) -> Option<char> {
    let line = view.line(row);
    if line.contains('|') {
        return Some('|');
    }
    if line.matches(',').count() >= 2 {
        return Some(',');
    }
    if line.contains('\t') {
        return Some('\t');
    }
    None
}

fn detect_delimiter_nearby(view: &View, row: u16) -> Option<char> {
    let rows = view.size().0;
    for offset in 1..=2u16 {
        if row >= offset {
            if let Some(delim) = detect_delimiter(view, row - offset) {
                return Some(delim);
            }
        }
        let down = row + offset;
        if down < rows {
            if let Some(delim) = detect_delimiter(view, down) {
                return Some(delim);
            }
        }
    }
    None
}

fn row_is_in_table(view: &View, row: u16, delimiter: Option<char>) -> bool {
    if row_is_blank(view, row) {
        return false;
    }
    if is_separator_row(view, row) {
        return true;
    }
    match delimiter {
        Some(delim) => row_has_delimiter(view, row, delim),
        None => row_has_fixed_width_columns(view, row),
    }
}

fn row_is_blank(view: &View, row: u16) -> bool {
    let (_, cols) = view.size();
    for col in 0..cols {
        if let Some(cell) = view.screen().cell(row, col) {
            if cell.is_wide_continuation() {
                return false;
            }
            if !cell.contents().trim().is_empty() {
                return false;
            }
        }
    }
    true
}

fn row_has_delimiter(view: &View, row: u16, delim: char) -> bool {
    let (_, cols) = view.size();
    let needle = delim.to_string();
    for col in 0..cols {
        if let Some(cell) = view.screen().cell(row, col) {
            if cell.contents() == needle {
                return true;
            }
        }
    }
    false
}

fn row_has_fixed_width_columns(view: &View, row: u16) -> bool {
    let line = view.line(row);
    fixed_width_column_count(&line) >= 2
}

fn fixed_width_column_count(line: &str) -> usize {
    let mut columns = 0;
    let mut in_word = false;
    let mut space_run = 0;
    for ch in line.chars() {
        if ch.is_whitespace() {
            if in_word {
                space_run += 1;
                if space_run >= 2 {
                    columns += 1;
                    in_word = false;
                }
            }
        } else {
            if !in_word {
                in_word = true;
            }
            space_run = 0;
        }
    }
    if in_word {
        columns += 1;
    }
    columns
}

fn detect_delimited_columns(
    view: &View,
    top: u16,
    bottom: u16,
    delim: char,
) -> Option<Vec<Column>> {
    let mut best_row = None;
    let mut best_count = 0usize;
    for row in top..=bottom {
        if is_separator_row(view, row) {
            continue;
        }
        let count = delimiter_positions(view, row, delim).len();
        if count > best_count {
            best_count = count;
            best_row = Some(row);
        }
    }

    let row = best_row?;
    let positions = delimiter_positions(view, row, delim);
    if positions.is_empty() {
        return None;
    }

    let mut columns = Vec::new();
    let mut start = 0u16;
    let last_col = view.size().1.saturating_sub(1);
    for pos in positions {
        if pos >= start {
            columns.push(Column {
                start,
                end: pos.saturating_sub(1),
            });
        }
        start = pos.saturating_add(1);
    }
    if start <= last_col {
        columns.push(Column {
            start,
            end: last_col,
        });
    }

    columns.retain(|col| column_has_content(view, top, bottom, col.start, col.end));
    Some(columns)
}

fn delimiter_positions(view: &View, row: u16, delim: char) -> Vec<u16> {
    let (_, cols) = view.size();
    let needle = delim.to_string();
    let mut positions = Vec::new();
    for col in 0..cols {
        if let Some(cell) = view.screen().cell(row, col) {
            if cell.contents() == needle {
                positions.push(col);
            }
        }
    }
    positions
}

fn detect_fixed_width_columns(view: &View, top: u16, bottom: u16) -> Option<Vec<Column>> {
    let (_, cols) = view.size();
    let mut rows = Vec::new();
    for row in top..=bottom {
        if !is_separator_row(view, row) {
            rows.push(row);
        }
    }
    if rows.is_empty() {
        return None;
    }

    let mut gap_cols = vec![true; cols as usize];
    for col in 0..cols {
        for row in &rows {
            if let Some(cell) = view.screen().cell(*row, col) {
                if cell.is_wide_continuation() || !cell.contents().trim().is_empty() {
                    gap_cols[col as usize] = false;
                    break;
                }
            }
        }
    }

    let mut columns = Vec::new();
    let mut start: Option<u16> = None;
    for col in 0..cols {
        let is_gap = gap_cols[col as usize];
        match (start, is_gap) {
            (None, false) => start = Some(col),
            (Some(s), true) => {
                columns.push(Column { start: s, end: col - 1 });
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        columns.push(Column {
            start: s,
            end: cols.saturating_sub(1),
        });
    }

    columns.retain(|col| column_has_content(view, top, bottom, col.start, col.end));
    Some(columns)
}

fn column_has_content(view: &View, top: u16, bottom: u16, start: u16, end: u16) -> bool {
    for row in top..=bottom {
        if is_separator_row(view, row) {
            continue;
        }
        for col in start..=end {
            if let Some(cell) = view.screen().cell(row, col) {
                if cell.is_wide_continuation() {
                    return true;
                }
                if !cell.contents().trim().is_empty() {
                    return true;
                }
            }
        }
    }
    false
}

fn detect_header_row(view: &View, top: u16, bottom: u16, _delimiter: Option<char>) -> Option<u16> {
    if top >= bottom {
        return None;
    }
    for row in top..=bottom {
        if is_separator_row(view, row) {
            if row > top {
                return Some(row - 1);
            }
        }
    }
    if row_has_letters(view, top) {
        Some(top)
    } else {
        None
    }
}

fn row_has_letters(view: &View, row: u16) -> bool {
    let line = view.line(row);
    line.chars().any(|c| c.is_alphabetic())
}

fn is_separator_row(view: &View, row: u16) -> bool {
    let line = view.line(row);
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    trimmed.chars().all(|ch| match ch {
        '-' | '=' | '+' | '|' | '_' => true,
        _ if ch.is_whitespace() => true,
        _ => false,
    })
}
