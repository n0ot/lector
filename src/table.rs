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
    pub delimiter: Option<char>,
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
        if self.delimiter == Some('|') {
            if let Some(text) = pipe_delimited_cell_text(&view.line(row), col_idx) {
                return text;
            }
        }

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
        if text.is_empty() { None } else { Some(text) }
    }

    pub fn prev_data_row(&self, view: &View, row: u16) -> Option<u16> {
        if row <= self.top {
            return None;
        }
        let mut candidate = row - 1;
        loop {
            if !self.is_skippable_row(view, candidate) {
                return Some(candidate);
            }
            if candidate == self.top {
                return None;
            }
            candidate -= 1;
        }
    }

    pub fn next_data_row(&self, view: &View, row: u16) -> Option<u16> {
        if row >= self.bottom {
            return None;
        }
        let mut candidate = row + 1;
        loop {
            if !self.is_skippable_row(view, candidate) {
                return Some(candidate);
            }
            if candidate >= self.bottom {
                return None;
            }
            candidate += 1;
        }
    }

    pub fn nearest_data_row(&self, view: &View, row: u16) -> Option<u16> {
        if row >= self.top && row <= self.bottom && !self.is_skippable_row(view, row) {
            return Some(row);
        }
        let mut offset = 1u16;
        loop {
            let mut progressed = false;
            if row >= self.top.saturating_add(offset) {
                progressed = true;
                let up = row - offset;
                if up >= self.top && !self.is_skippable_row(view, up) {
                    return Some(up);
                }
            }
            let down = row.saturating_add(offset);
            if down <= self.bottom {
                progressed = true;
                if !self.is_skippable_row(view, down) {
                    return Some(down);
                }
            }
            if !progressed {
                break;
            }
            offset = offset.saturating_add(1);
        }
        None
    }

    pub fn nearest_non_empty_col(&self, view: &View, row: u16, preferred: usize) -> usize {
        if self.columns.is_empty() {
            return 0;
        }
        let preferred = preferred.min(self.columns.len() - 1);
        if !self.cell_text(view, row, preferred).is_empty() {
            return preferred;
        }
        for offset in 1..self.columns.len() {
            if preferred >= offset {
                let left = preferred - offset;
                if !self.cell_text(view, row, left).is_empty() {
                    return left;
                }
            }
            let right = preferred + offset;
            if right < self.columns.len() && !self.cell_text(view, row, right).is_empty() {
                return right;
            }
        }
        preferred
    }

    pub fn is_skippable_row(&self, view: &View, row: u16) -> bool {
        is_separator_row(view, row) || self.is_banner_row(view, row)
    }

    pub fn is_banner_row(&self, view: &View, row: u16) -> bool {
        if row < self.top || row > self.bottom || is_separator_row(view, row) {
            return false;
        }

        let mut non_empty_cells = 0usize;
        for idx in 0..self.columns.len() {
            if !self.cell_text(view, row, idx).is_empty() {
                non_empty_cells += 1;
            }
        }

        if self.delimiter.is_none() {
            return !row_has_fixed_width_columns(view, row);
        }

        let line = view.line(row);
        let trimmed = line.trim();
        if !(trimmed.starts_with('|') && trimmed.ends_with('|')) {
            return false;
        }

        non_empty_cells <= 1
    }
}

fn pipe_delimited_cell_text(line: &str, col_idx: usize) -> Option<String> {
    let trimmed = line.trim();
    if !trimmed.contains('|') {
        return None;
    }

    let cells: Vec<&str> = trimmed.split('|').collect();
    if cells.len() < 2 {
        return None;
    }

    let start = if trimmed.starts_with('|') { 1 } else { 0 };
    let end = if trimmed.ends_with('|') {
        cells.len().saturating_sub(1)
    } else {
        cells.len()
    };
    if end <= start {
        return None;
    }

    let idx = start + col_idx;
    if idx >= end {
        return Some(String::new());
    }

    Some(cells[idx].trim().to_string())
}

pub fn detect(view: &View, row: u16) -> Option<TableModel> {
    detect_pipe_table(view, row).or_else(|| detect_fixed_width_table(view, row))
}

pub fn detect_manual_from_header(
    view: &View,
    header_row: u16,
    tabstops: &[u16],
) -> Option<TableModel> {
    let (rows, cols) = view.size();
    if rows == 0 || cols == 0 || header_row >= rows {
        return None;
    }

    let mut starts = vec![0u16];
    for stop in tabstops.iter().copied() {
        if stop > 0 && stop < cols {
            starts.push(stop);
        }
    }
    starts.sort_unstable();
    starts.dedup();

    let columns = columns_from_starts(cols, &starts);
    if columns.len() < 2 {
        return None;
    }

    let mut top = header_row;
    while top > 0 && row_is_manual_table_row(view, top - 1) {
        top -= 1;
    }

    let mut bottom = header_row;
    while bottom + 1 < rows && row_is_manual_table_row(view, bottom + 1) {
        bottom += 1;
    }

    let mut columns = columns;
    columns.retain(|col| column_has_content(view, top, bottom, col.start, col.end));
    if columns.len() < 2 {
        return None;
    }

    Some(TableModel {
        top,
        bottom,
        columns,
        header_row: Some(header_row),
        delimiter: None,
    })
}

fn detect_pipe_table(view: &View, row: u16) -> Option<TableModel> {
    let rows = view.size().0;
    let anchor = nearest_pipe_row(view, row)?;

    let mut top = anchor;
    while top > 0 && row_is_pipe_table_row(view, top - 1) {
        top -= 1;
    }

    let mut bottom = anchor;
    while bottom + 1 < rows && row_is_pipe_table_row(view, bottom + 1) {
        bottom += 1;
    }

    let header_row = find_pipe_header_row_in_bounds(view, top, bottom)?;

    let columns = detect_pipe_columns(view, top, bottom, header_row)?;
    if columns.len() < 2 {
        return None;
    }

    let header_row = detect_header_row(view, top, bottom, &columns, Some('|')).or(Some(header_row));

    Some(TableModel {
        top,
        bottom,
        columns,
        header_row,
        delimiter: Some('|'),
    })
}

fn nearest_pipe_row(view: &View, row: u16) -> Option<u16> {
    let rows = view.size().0;
    if rows == 0 {
        return None;
    }

    if row_is_pipe_table_row(view, row) {
        return Some(row);
    }

    for offset in 1..=6u16 {
        if row >= offset {
            let up = row - offset;
            if row_is_pipe_table_row(view, up) {
                return Some(up);
            }
        }

        let down = row + offset;
        if down < rows && row_is_pipe_table_row(view, down) {
            return Some(down);
        }
    }

    None
}

fn find_pipe_header_row_in_bounds(view: &View, top: u16, bottom: u16) -> Option<u16> {
    for row in top..=bottom {
        if row_looks_like_pipe_header(view, row) {
            return Some(row);
        }
    }
    None
}

fn row_looks_like_pipe_header(view: &View, row: u16) -> bool {
    let line = view.line(row);
    let trimmed = line.trim();
    if trimmed.is_empty() || is_separator_row(view, row) {
        return false;
    }
    if trimmed.matches('|').count() < 1 {
        return false;
    }

    let parts: Vec<&str> = trimmed.split('|').collect();
    let start = if trimmed.starts_with('|') { 1 } else { 0 };
    let end = if trimmed.ends_with('|') {
        parts.len().saturating_sub(1)
    } else {
        parts.len()
    };
    if end <= start {
        return false;
    }

    let cells: Vec<&str> = parts[start..end]
        .iter()
        .map(|cell| cell.trim())
        .filter(|cell| !cell.is_empty())
        .collect();

    cells.len() >= 2
}

fn row_is_pipe_table_row(view: &View, row: u16) -> bool {
    !row_is_blank(view, row) && (is_separator_row(view, row) || view.line(row).contains('|'))
}

fn detect_pipe_columns(view: &View, top: u16, bottom: u16, header_row: u16) -> Option<Vec<Column>> {
    let positions = delimiter_positions(view, header_row, '|');
    if positions.is_empty() {
        return None;
    }

    let mut columns = columns_from_delimiter_positions(view, &positions);
    columns.retain(|col| column_has_content(view, top, bottom, col.start, col.end));

    if columns.len() < 2 {
        return None;
    }

    Some(columns)
}

fn columns_from_delimiter_positions(view: &View, positions: &[u16]) -> Vec<Column> {
    let mut columns = Vec::new();
    let mut start = 0u16;
    let last_col = view.size().1.saturating_sub(1);

    for pos in positions.iter().copied() {
        if pos > start {
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

    columns
}

fn columns_from_starts(cols: u16, starts: &[u16]) -> Vec<Column> {
    let mut columns = Vec::new();
    if cols == 0 || starts.is_empty() {
        return columns;
    }

    for (idx, start) in starts.iter().copied().enumerate() {
        if start >= cols {
            continue;
        }
        let end = if let Some(next_start) = starts.get(idx + 1).copied() {
            next_start.saturating_sub(1)
        } else {
            cols.saturating_sub(1)
        };
        if start <= end {
            columns.push(Column { start, end });
        }
    }

    columns
}

fn detect_fixed_width_table(view: &View, row: u16) -> Option<TableModel> {
    let (rows, cols) = view.size();
    if rows == 0 || cols == 0 {
        return None;
    }

    let anchor = if row_is_fixed_width_candidate(view, row) {
        row
    } else {
        nearest_fixed_width_candidate(view, row)?
    };

    let mut top = anchor;
    while top > 0 && row_is_fixed_width_candidate(view, top - 1) {
        top -= 1;
    }

    let mut bottom = anchor;
    while bottom + 1 < rows && row_is_fixed_width_candidate(view, bottom + 1) {
        bottom += 1;
    }

    let structural_rows: Vec<u16> = (top..=bottom)
        .filter(|candidate| {
            !is_separator_row(view, *candidate) && row_has_fixed_width_columns(view, *candidate)
        })
        .collect();

    if structural_rows.len() < 2 {
        return None;
    }

    let mut columns =
        detect_fixed_width_columns_from_blanks(view, top, bottom, cols, &structural_rows)?;
    columns.retain(|col| column_has_content(view, top, bottom, col.start, col.end));

    if columns.len() < 2 {
        return None;
    }

    let header_row = detect_header_row(view, top, bottom, &columns, None);

    Some(TableModel {
        top,
        bottom,
        columns,
        header_row,
        delimiter: None,
    })
}

fn row_is_fixed_width_candidate(view: &View, row: u16) -> bool {
    if row_is_blank(view, row) {
        return false;
    }

    is_separator_row(view, row)
        || row_has_fixed_width_columns(view, row)
        || row_is_fixed_width_continuation(view, row)
}

fn row_is_manual_table_row(view: &View, row: u16) -> bool {
    if row_is_blank(view, row) {
        return false;
    }
    is_separator_row(view, row)
        || row_has_fixed_width_columns(view, row)
        || row_is_fixed_width_continuation(view, row)
        || view.line(row).contains('|')
}

fn nearest_fixed_width_candidate(view: &View, row: u16) -> Option<u16> {
    let rows = view.size().0;
    if rows == 0 {
        return None;
    }

    for offset in 1..=2u16 {
        if row >= offset {
            let up = row - offset;
            if row_is_fixed_width_candidate(view, up) {
                return Some(up);
            }
        }

        let down = row + offset;
        if down < rows && row_is_fixed_width_candidate(view, down) {
            return Some(down);
        }
    }

    None
}

fn detect_fixed_width_columns_from_blanks(
    view: &View,
    top: u16,
    bottom: u16,
    cols: u16,
    structural_rows: &[u16],
) -> Option<Vec<Column>> {
    if cols == 0 || structural_rows.is_empty() {
        return None;
    }

    let mut blank_everywhere = vec![true; cols as usize];

    for col in 0..cols {
        for row in structural_rows {
            let has_content = view
                .screen()
                .cell(*row, col)
                .map(|cell| cell.is_wide_continuation() || !cell.contents().trim().is_empty())
                .unwrap_or(false);

            if has_content {
                blank_everywhere[col as usize] = false;
                break;
            }
        }
    }

    let mut columns = Vec::new();
    let mut start: Option<u16> = None;

    for col in 0..cols {
        let is_gap = blank_everywhere[col as usize];
        match (start, is_gap) {
            (None, false) => start = Some(col),
            (Some(s), true) => {
                columns.push(Column {
                    start: s,
                    end: col.saturating_sub(1),
                });
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

    if columns.len() < 2 {
        return None;
    }

    Some(columns)
}

fn row_is_blank(view: &View, row: u16) -> bool {
    let (_, cols) = view.size();
    for col in 0..cols {
        if let Some(cell) = view.screen().cell(row, col) {
            if cell.is_wide_continuation() || !cell.contents().trim().is_empty() {
                return false;
            }
        }
    }
    true
}

fn row_has_fixed_width_columns(view: &View, row: u16) -> bool {
    let line = view.line(row);
    fixed_width_column_count(&line) >= 2
}

fn row_is_fixed_width_continuation(view: &View, row: u16) -> bool {
    if row_is_blank(view, row) || row_has_fixed_width_columns(view, row) {
        return false;
    }

    let rows = view.size().0;
    (row > 0 && row_has_fixed_width_columns(view, row - 1))
        || (row + 1 < rows && row_has_fixed_width_columns(view, row + 1))
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

fn detect_header_row(
    view: &View,
    top: u16,
    bottom: u16,
    columns: &[Column],
    delimiter: Option<char>,
) -> Option<u16> {
    if top >= bottom {
        return None;
    }

    let model = TableModel {
        top,
        bottom,
        columns: columns.to_vec(),
        header_row: None,
        delimiter,
    };

    for row in top..=bottom {
        if is_separator_row(view, row) && row > top {
            let mut candidate = row - 1;
            loop {
                if !model.is_skippable_row(view, candidate) {
                    return Some(candidate);
                }
                if candidate == top {
                    break;
                }
                candidate -= 1;
            }
        }
    }

    for row in top..=bottom {
        if !model.is_skippable_row(view, row) && row_has_letters(view, row) {
            return Some(row);
        }
    }

    None
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

fn column_has_content(view: &View, top: u16, bottom: u16, start: u16, end: u16) -> bool {
    for row in top..=bottom {
        if is_separator_row(view, row) {
            continue;
        }
        for col in start..=end {
            if let Some(cell) = view.screen().cell(row, col) {
                if cell.is_wide_continuation() || !cell.contents().trim().is_empty() {
                    return true;
                }
            }
        }
    }
    false
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
        '-' | '=' | '+' | '|' | '_' | ':' => true,
        _ if ch.is_whitespace() => true,
        _ => false,
    })
}
