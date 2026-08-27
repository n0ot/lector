// This is an original Lector implementation over ReviewDocument. Keep this
// module free of copied or translated GPL-licensed table-plugin code.
use super::document::ReviewDocument;
use crate::terminal::HistoryPosition;
use std::collections::BTreeMap;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum CellMove {
    Previous,
    Next,
    Up,
    Down,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum HeaderMode {
    FirstRow,
    NoHeader,
}

impl HeaderMode {
    pub(crate) fn toggle(self) -> Self {
        match self {
            Self::FirstRow => Self::NoHeader,
            Self::NoHeader => Self::FirstRow,
        }
    }

    pub(crate) fn announcement(self) -> &'static str {
        match self {
            Self::FirstRow => "headers from first row",
            Self::NoHeader => "no header row; use custom names or column numbers",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TableSetup {
    top_row: usize,
    bottom_row: Option<usize>,
    right_edge: Option<u16>,
    header_mode: HeaderMode,
    tabstops: Vec<u16>,
    names: BTreeMap<u16, String>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum MarkerChange {
    Set,
    Cleared,
}

impl TableSetup {
    pub(crate) fn new(top_row: usize) -> Self {
        Self {
            top_row,
            bottom_row: None,
            right_edge: None,
            header_mode: HeaderMode::FirstRow,
            tabstops: Vec::new(),
            names: BTreeMap::new(),
        }
    }

    pub(crate) fn top_row(&self) -> usize {
        self.top_row
    }

    pub(crate) fn right_edge(&self) -> Option<u16> {
        self.right_edge
    }

    pub(crate) fn header_mode(&self) -> HeaderMode {
        self.header_mode
    }

    pub(crate) fn toggle_header_mode(&mut self) -> HeaderMode {
        self.header_mode = self.header_mode.toggle();
        self.header_mode
    }

    pub(crate) fn tabstops(&self) -> &[u16] {
        &self.tabstops
    }

    pub(crate) fn toggle_tabstop(&mut self, col: u16) -> MarkerChange {
        match self.tabstops.binary_search(&col) {
            Ok(index) => {
                self.tabstops.remove(index);
                MarkerChange::Cleared
            }
            Err(index) => {
                self.tabstops.insert(index, col);
                MarkerChange::Set
            }
        }
    }

    pub(crate) fn tabstop_at_or_before(&self, col: u16) -> Option<(usize, u16)> {
        let index = self.tabstops.partition_point(|candidate| *candidate <= col);
        index
            .checked_sub(1)
            .map(|index| (index, self.tabstops[index]))
    }

    pub(crate) fn name(&self, tabstop: u16) -> Option<&str> {
        self.names.get(&tabstop).map(String::as_str)
    }

    pub(crate) fn set_name(&mut self, tabstop: u16, name: String) {
        let name = name.trim();
        if name.is_empty() {
            self.names.remove(&tabstop);
        } else {
            self.names.insert(tabstop, name.to_owned());
        }
    }

    pub(crate) fn toggle_bottom(&mut self, row: usize) -> MarkerChange {
        if self.bottom_row == Some(row) {
            self.bottom_row = None;
            MarkerChange::Cleared
        } else {
            self.bottom_row = Some(row);
            MarkerChange::Set
        }
    }

    pub(crate) fn toggle_right_edge(&mut self, col: u16) -> MarkerChange {
        if self.right_edge == Some(col) {
            self.right_edge = None;
            MarkerChange::Cleared
        } else {
            self.right_edge = Some(col);
            MarkerChange::Set
        }
    }
}

#[derive(Clone, Debug)]
struct CellSpan {
    start: u16,
    end: u16,
    text: String,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum RowKind {
    Header,
    Data(usize),
}

#[derive(Clone, Debug)]
struct TableRow {
    document_row: usize,
    left: u16,
    right: u16,
    kind: RowKind,
    cells: Vec<CellSpan>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct CellAddress {
    pub(crate) row: usize,
    pub(crate) column: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct ReviewTable {
    top: usize,
    bottom: usize,
    rows: Vec<TableRow>,
    row_lookup: Vec<Option<usize>>,
    labels: Vec<String>,
    row_header: Option<usize>,
}

impl ReviewTable {
    fn new(top: usize, bottom: usize, rows: Vec<TableRow>, labels: Vec<String>) -> Self {
        let mut row_lookup = vec![None; bottom.saturating_sub(top).saturating_add(1)];
        for (index, row) in rows.iter().enumerate() {
            if let Some(slot) = row_lookup.get_mut(row.document_row.saturating_sub(top)) {
                *slot = Some(index);
            }
        }
        Self {
            top,
            bottom,
            rows,
            row_lookup,
            labels,
            row_header: None,
        }
    }

    pub(crate) fn detect(document: &ReviewDocument, anchor: HistoryPosition) -> Option<Self> {
        detect_pipe(document, anchor).or_else(|| detect_fixed_width(document, anchor))
    }

    pub(crate) fn from_setup(
        document: &ReviewDocument,
        setup: &TableSetup,
    ) -> Result<Self, &'static str> {
        if setup.tabstops.len() < 2 {
            return Err("set at least two tabstops");
        }
        if setup
            .right_edge
            .is_some_and(|right| setup.tabstops.last().is_some_and(|last| right < *last))
        {
            return Err("right edge cannot be before the last tabstop");
        }
        let bottom = match setup.bottom_row {
            Some(bottom) if bottom < setup.top_row => {
                return Err("bottom row cannot be above the first row");
            }
            Some(bottom) => bottom.min(document.row_count().saturating_sub(1)),
            None => infer_bottom(document, setup.top_row),
        };
        if (setup.top_row..=bottom).any(|row| row_is_soft_wrapped_part(document, row)) {
            return Err("wrapped table rows are not supported");
        }
        let right = setup
            .right_edge
            .map(|edge| edge.saturating_add(1).min(document.capture_cols()))
            .unwrap_or(document.capture_cols());
        let mut rows = Vec::new();
        let mut header_cells = None;
        let mut data_number = 0usize;
        for row in setup.top_row..=bottom {
            let cells = fixed_cells(document, row, &setup.tabstops, right);
            if row == setup.top_row && setup.header_mode == HeaderMode::FirstRow {
                header_cells = Some(cells.clone());
                rows.push(TableRow {
                    document_row: row,
                    left: setup.tabstops[0],
                    right,
                    kind: RowKind::Header,
                    cells,
                });
            } else if is_separator_text(&document.line_text(row)) {
                continue;
            } else {
                data_number += 1;
                rows.push(TableRow {
                    document_row: row,
                    left: setup.tabstops[0],
                    right,
                    kind: RowKind::Data(data_number),
                    cells,
                });
            }
        }
        if !rows.iter().any(|row| matches!(row.kind, RowKind::Data(_))) {
            return Err("table must contain at least one data row");
        }
        let labels = (0..setup.tabstops.len())
            .map(|column| match setup.header_mode {
                HeaderMode::FirstRow => header_cells
                    .as_ref()
                    .and_then(|cells| cells.get(column))
                    .map(|cell| cell.text.trim())
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| fallback_label(column)),
                HeaderMode::NoHeader => setup
                    .name(setup.tabstops[column])
                    .map(str::to_owned)
                    .unwrap_or_else(|| fallback_label(column)),
            })
            .collect();
        Ok(Self::new(setup.top_row, bottom, rows, labels))
    }

    pub(crate) fn dimensions(&self) -> (usize, usize) {
        (
            self.rows
                .iter()
                .filter(|row| matches!(row.kind, RowKind::Data(_)))
                .count(),
            self.labels.len(),
        )
    }

    pub(crate) fn cell_at(&self, position: HistoryPosition) -> Option<CellAddress> {
        let row = *self
            .row_lookup
            .get(position.row.checked_sub(self.top)?)?
            .as_ref()?;
        let source = &self.rows[row];
        if position.col < source.left || position.col >= source.right {
            return None;
        }
        let column = source.cells.iter().enumerate().find_map(|(index, cell)| {
            let next_start = source
                .cells
                .get(index + 1)
                .map_or(source.right, |next| next.start);
            (position.col >= cell.start && position.col < next_start).then_some(index)
        })?;
        Some(CellAddress { row, column })
    }

    pub(crate) fn nearest_cell(&self, position: HistoryPosition) -> Option<CellAddress> {
        if let Some(address) = self.cell_at(position) {
            return Some(address);
        }
        let next = self
            .rows
            .partition_point(|row| row.document_row < position.row);
        let row = match (next.checked_sub(1), self.rows.get(next)) {
            (Some(previous), Some(following))
                if self.rows[previous].document_row.abs_diff(position.row)
                    <= following.document_row.abs_diff(position.row) =>
            {
                previous
            }
            (_, Some(_)) => next,
            (Some(previous), None) => previous,
            (None, None) => return None,
        };
        let source = &self.rows[row];
        let column = source
            .cells
            .partition_point(|cell| cell.start <= position.col)
            .saturating_sub(1)
            .min(source.cells.len().saturating_sub(1));
        Some(CellAddress { row, column })
    }

    pub(crate) fn reentry_cell(
        &self,
        position: HistoryPosition,
        movement: CellMove,
    ) -> Option<CellAddress> {
        match movement {
            CellMove::Previous => self
                .rows
                .iter()
                .enumerate()
                .rev()
                .find_map(|(row, source)| {
                    if source.document_row < position.row {
                        return source
                            .cells
                            .len()
                            .checked_sub(1)
                            .map(|column| CellAddress { row, column });
                    }
                    if source.document_row != position.row {
                        return None;
                    }
                    source
                        .cells
                        .iter()
                        .rposition(|cell| cell.start < position.col)
                        .map(|column| CellAddress { row, column })
                }),
            CellMove::Next => self.rows.iter().enumerate().find_map(|(row, source)| {
                if source.document_row > position.row {
                    return (!source.cells.is_empty()).then_some(CellAddress { row, column: 0 });
                }
                if source.document_row != position.row {
                    return None;
                }
                source
                    .cells
                    .iter()
                    .position(|cell| cell.start > position.col)
                    .map(|column| CellAddress { row, column })
            }),
            CellMove::Up => {
                let row = self
                    .rows
                    .iter()
                    .rposition(|source| source.document_row < position.row)?;
                Some(CellAddress {
                    row,
                    column: self.column_at_or_before(row, position.col),
                })
            }
            CellMove::Down => {
                let row = self
                    .rows
                    .iter()
                    .position(|source| source.document_row > position.row)?;
                Some(CellAddress {
                    row,
                    column: self.column_at_or_before(row, position.col),
                })
            }
        }
    }

    fn column_at_or_before(&self, row: usize, col: u16) -> usize {
        self.rows[row]
            .cells
            .partition_point(|cell| cell.start <= col)
            .saturating_sub(1)
            .min(self.rows[row].cells.len().saturating_sub(1))
    }

    pub(crate) fn move_cell(
        &self,
        mut address: CellAddress,
        movement: CellMove,
        count: usize,
    ) -> Option<CellAddress> {
        let original = address;
        for _ in 0..count {
            address = match movement {
                CellMove::Previous if address.column > 0 => CellAddress {
                    row: address.row,
                    column: address.column - 1,
                },
                CellMove::Previous if address.row > 0 => CellAddress {
                    row: address.row - 1,
                    column: self.rows[address.row - 1].cells.len().saturating_sub(1),
                },
                CellMove::Next if address.column + 1 < self.rows[address.row].cells.len() => {
                    CellAddress {
                        row: address.row,
                        column: address.column + 1,
                    }
                }
                CellMove::Next if address.row + 1 < self.rows.len() => CellAddress {
                    row: address.row + 1,
                    column: 0,
                },
                CellMove::Up if address.row > 0 => CellAddress {
                    row: address.row - 1,
                    column: address
                        .column
                        .min(self.rows[address.row - 1].cells.len().saturating_sub(1)),
                },
                CellMove::Down if address.row + 1 < self.rows.len() => CellAddress {
                    row: address.row + 1,
                    column: address
                        .column
                        .min(self.rows[address.row + 1].cells.len().saturating_sub(1)),
                },
                _ => break,
            };
        }
        (address != original).then_some(address)
    }

    pub(crate) fn position_for_cell(
        &self,
        document: &ReviewDocument,
        address: CellAddress,
    ) -> HistoryPosition {
        let row = &self.rows[address.row];
        let cell = &row.cells[address.column];
        let col = document
            .first_text_col(row.document_row, cell.start, cell.end)
            .unwrap_or(cell.start);
        HistoryPosition {
            row: row.document_row,
            col,
        }
    }

    pub(crate) fn row_number(&self, address: CellAddress) -> Option<usize> {
        match self.rows[address.row].kind {
            RowKind::Header => None,
            RowKind::Data(number) => Some(number),
        }
    }

    pub(crate) fn is_header(&self, address: CellAddress) -> bool {
        self.rows[address.row].kind == RowKind::Header
    }

    pub(crate) fn label(&self, address: CellAddress) -> &str {
        self.labels
            .get(address.column)
            .map(String::as_str)
            .unwrap_or("column")
    }

    pub(crate) fn text(&self, address: CellAddress) -> &str {
        self.rows[address.row]
            .cells
            .get(address.column)
            .map(|cell| cell.text.as_str())
            .unwrap_or("")
    }

    pub(crate) fn toggle_row_header(&mut self, column: usize) -> bool {
        if self.row_header == Some(column) {
            self.row_header = None;
            false
        } else {
            self.row_header = Some(column);
            true
        }
    }

    pub(crate) fn row_header_column(&self) -> Option<usize> {
        self.row_header
    }

    pub(crate) fn row_header_text(&self, address: CellAddress) -> Option<&str> {
        let column = self.row_header?;
        self.rows[address.row]
            .cells
            .get(column)
            .map(|cell| cell.text.as_str())
    }

    pub(crate) fn contains_row(&self, row: usize) -> bool {
        row >= self.top && row <= self.bottom
    }

    pub(crate) fn is_structural_row(&self, row: usize) -> bool {
        self.contains_row(row)
            && self
                .row_lookup
                .get(row.saturating_sub(self.top))
                .is_some_and(Option::is_none)
    }

    pub(crate) fn boundary_announcement(
        &self,
        movement: CellMove,
        address: CellAddress,
    ) -> (&'static str, Option<usize>) {
        (self.edge_announcement(movement), self.row_number(address))
    }

    pub(crate) fn edge_announcement(&self, movement: CellMove) -> &'static str {
        match movement {
            CellMove::Previous | CellMove::Up => "top of table",
            CellMove::Next | CellMove::Down => "bottom of table",
        }
    }
}

fn fallback_label(column: usize) -> String {
    format!("column {}", column + 1)
}

fn infer_bottom(document: &ReviewDocument, top: usize) -> usize {
    let mut bottom = top.min(document.row_count().saturating_sub(1));
    while bottom + 1 < document.row_count() {
        if document.line_text(bottom + 1).trim().is_empty() {
            break;
        }
        bottom += 1;
    }
    bottom
}

fn fixed_cells(document: &ReviewDocument, row: usize, starts: &[u16], right: u16) -> Vec<CellSpan> {
    starts
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, start)| *start < right)
        .map(|(index, start)| {
            let end = starts.get(index + 1).copied().unwrap_or(right).min(right);
            CellSpan {
                start,
                end,
                text: document.text_between(row, start, end),
            }
        })
        .collect()
}

fn detect_pipe(document: &ReviewDocument, anchor: HistoryPosition) -> Option<ReviewTable> {
    let delimiters = ['|', '│'];
    for delimiter in delimiters {
        let Some(anchor_row) = nearest_row(document, anchor.row, 2, |row| {
            !row_is_soft_wrapped_part(document, row)
                && pipe_cells(document, row, delimiter).is_some_and(|cells| cells.len() >= 2)
        }) else {
            continue;
        };
        let expected = pipe_cells(document, anchor_row, delimiter)?.len();
        let compatible = |row| {
            !row_is_soft_wrapped_part(document, row)
                && pipe_cells(document, row, delimiter).is_some_and(|cells| cells.len() == expected)
        };
        let mut top = anchor_row;
        while top > 0 && compatible(top - 1) {
            top -= 1;
        }
        let mut bottom = anchor_row;
        while bottom + 1 < document.row_count() && compatible(bottom + 1) {
            bottom += 1;
        }
        let separator = (top..=bottom).find(|row| {
            pipe_cells(document, *row, delimiter)
                .is_some_and(|cells| cells.iter().all(|cell| is_separator_text(&cell.text)))
        });
        let header = separator.and_then(|row| (row > top).then_some(row - 1));
        let mut data_number = 0usize;
        let mut rows = Vec::new();
        let mut header_cells = None;
        for row in top..=bottom {
            let cells = pipe_cells(document, row, delimiter)?;
            if Some(row) == separator || cells.iter().all(|cell| is_separator_text(&cell.text)) {
                continue;
            }
            let left = cells.first()?.start.saturating_sub(1);
            let right = document.line_end(row).max(cells.last()?.end);
            let kind = if Some(row) == header {
                header_cells = Some(cells.clone());
                RowKind::Header
            } else {
                data_number += 1;
                RowKind::Data(data_number)
            };
            rows.push(TableRow {
                document_row: row,
                left,
                right,
                kind,
                cells,
            });
        }
        let data_rows = rows
            .iter()
            .filter(|row| matches!(row.kind, RowKind::Data(_)))
            .count();
        if data_rows == 0 || (header.is_none() && data_rows < 2) {
            continue;
        }
        let labels = (0..expected)
            .map(|column| {
                header_cells
                    .as_ref()
                    .and_then(|cells| cells.get(column))
                    .map(|cell| cell.text.trim())
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| fallback_label(column))
            })
            .collect();
        return Some(ReviewTable::new(top, bottom, rows, labels));
    }
    None
}

fn pipe_cells(document: &ReviewDocument, row: usize, delimiter: char) -> Option<Vec<CellSpan>> {
    let end = document.line_end(row);
    let mut positions = Vec::new();
    for col in 0..end {
        if document
            .cell_text(HistoryPosition { row, col })
            .starts_with(delimiter)
            && !is_escaped_delimiter(document, row, col)
        {
            positions.push(col);
        }
    }
    if positions.is_empty() {
        return None;
    }
    let mut cells = Vec::new();
    let mut start = 0u16;
    for delimiter_col in positions {
        if delimiter_col > start {
            cells.push(CellSpan {
                start,
                end: delimiter_col,
                text: document.text_between(row, start, delimiter_col),
            });
        }
        start = delimiter_col.saturating_add(1);
    }
    if start < end {
        cells.push(CellSpan {
            start,
            end,
            text: document.text_between(row, start, end),
        });
    }
    (cells.len() >= 2).then_some(cells)
}

fn is_escaped_delimiter(document: &ReviewDocument, row: usize, col: u16) -> bool {
    let mut preceding = 0usize;
    let mut cursor = col;
    while cursor > 0 {
        cursor -= 1;
        if document.cell_text(HistoryPosition { row, col: cursor }) == "\\" {
            preceding += 1;
        } else {
            break;
        }
    }
    preceding % 2 == 1
}

fn detect_fixed_width(document: &ReviewDocument, anchor: HistoryPosition) -> Option<ReviewTable> {
    let anchor_row = nearest_row(document, anchor.row, 2, |row| {
        !row_is_soft_wrapped_part(document, row) && fixed_width_starts(document, row).len() >= 2
    })?;
    let mut region_top = anchor_row;
    while region_top > 0 && !document.line_text(region_top - 1).trim().is_empty() {
        region_top -= 1;
    }
    let mut region_bottom = anchor_row;
    while region_bottom + 1 < document.row_count()
        && !document.line_text(region_bottom + 1).trim().is_empty()
    {
        region_bottom += 1;
    }
    let candidate_rows = region_top..=region_bottom.min(region_top.saturating_add(4));
    let starts = candidate_rows
        .map(|row| fixed_width_starts(document, row))
        .max_by_key(Vec::len)?;
    if starts.len() < 2 {
        return None;
    }
    let right = document.capture_cols();
    let compatible = |row| {
        if row_is_soft_wrapped_part(document, row) {
            return false;
        }
        if is_separator_text(&document.line_text(row)) {
            return true;
        }
        let cells = fixed_cells(document, row, &starts, right);
        cells.iter().filter(|cell| !cell.text.is_empty()).count() >= 2
    };
    let mut top = anchor_row;
    while top > region_top && compatible(top - 1) {
        top -= 1;
    }
    let mut bottom = anchor_row;
    while bottom < region_bottom && compatible(bottom + 1) {
        bottom += 1;
    }
    let first_cells = fixed_cells(document, top, &starts, right);
    let separator_after = top < bottom && is_separator_text(&document.line_text(top + 1));
    let uppercase_header = first_cells.iter().all(|cell| {
        let letters = cell.text.chars().filter(|ch| ch.is_alphabetic());
        let mut saw_letter = false;
        for ch in letters {
            saw_letter = true;
            if ch.is_lowercase() {
                return false;
            }
        }
        saw_letter
    });
    let header = (separator_after || uppercase_header).then_some(top);
    let mut data_number = 0usize;
    let mut rows = Vec::new();
    let mut header_cells = None;
    for row in top..=bottom {
        if is_separator_text(&document.line_text(row)) {
            continue;
        }
        let cells = fixed_cells(document, row, &starts, right);
        let kind = if Some(row) == header {
            header_cells = Some(cells.clone());
            RowKind::Header
        } else {
            data_number += 1;
            RowKind::Data(data_number)
        };
        rows.push(TableRow {
            document_row: row,
            left: starts[0],
            right,
            kind,
            cells,
        });
    }
    if data_number == 0 || (header.is_none() && data_number < 2) {
        return None;
    }
    let labels = (0..starts.len())
        .map(|column| {
            header_cells
                .as_ref()
                .and_then(|cells| cells.get(column))
                .map(|cell| cell.text.trim())
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| fallback_label(column))
        })
        .collect();
    Some(ReviewTable::new(top, bottom, rows, labels))
}

fn fixed_width_starts(document: &ReviewDocument, row: usize) -> Vec<u16> {
    let end = document.line_end(row);
    let mut starts = Vec::new();
    let mut col = 0u16;
    while col < end
        && document
            .cell_text(HistoryPosition { row, col })
            .trim()
            .is_empty()
    {
        col += 1;
    }
    if col >= end {
        return starts;
    }
    starts.push(col);
    while col < end {
        if !document
            .cell_text(HistoryPosition { row, col })
            .trim()
            .is_empty()
        {
            col += 1;
            continue;
        }
        let gap_start = col;
        while col < end
            && document
                .cell_text(HistoryPosition { row, col })
                .trim()
                .is_empty()
        {
            col += 1;
        }
        if col < end && col.saturating_sub(gap_start) >= 2 {
            starts.push(col);
        }
    }
    starts
}

fn nearest_row(
    document: &ReviewDocument,
    anchor: usize,
    radius: usize,
    predicate: impl Fn(usize) -> bool,
) -> Option<usize> {
    if anchor < document.row_count() && predicate(anchor) {
        return Some(anchor);
    }
    for offset in 1..=radius {
        if let Some(up) = anchor.checked_sub(offset)
            && predicate(up)
        {
            return Some(up);
        }
        let down = anchor.saturating_add(offset);
        if down < document.row_count() && predicate(down) {
            return Some(down);
        }
    }
    None
}

fn row_is_soft_wrapped_part(document: &ReviewDocument, row: usize) -> bool {
    document.is_wrapped_row(row)
        || row
            .checked_sub(1)
            .is_some_and(|previous| document.is_wrapped_row(previous))
}

fn is_separator_text(text: &str) -> bool {
    let mut structural = 0usize;
    for ch in text.trim().chars() {
        if ch.is_whitespace() {
            continue;
        }
        if matches!(
            ch,
            '-' | '='
                | '+'
                | ':'
                | '|'
                | '│'
                | '─'
                | '━'
                | '┼'
                | '╋'
                | '┬'
                | '┴'
                | '├'
                | '┤'
                | '┌'
                | '┐'
                | '└'
                | '┘'
        ) {
            structural += 1;
        } else {
            return false;
        }
    }
    structural >= 3
}

#[cfg(test)]
mod tests {
    use super::{CellMove, HeaderMode, MarkerChange, ReviewTable, TableSetup};
    use crate::{review::document::ReviewDocument, terminal::HistoryPosition};

    fn document(text: &[u8]) -> ReviewDocument {
        ReviewDocument::from_text(12, 80, text)
    }

    #[test]
    fn manual_setup_uses_explicit_starts_names_and_bounds() {
        let document = document(
            b"junk NAME      AGE       note ignored\r\njunk Alice     37        ok   ignored",
        );
        let mut setup = TableSetup::new(0);
        setup.toggle_header_mode();
        assert_eq!(setup.header_mode(), HeaderMode::NoHeader);
        assert_eq!(setup.toggle_tabstop(5), MarkerChange::Set);
        setup.toggle_tabstop(15);
        setup.toggle_tabstop(25);
        setup.set_name(5, "name".to_owned());
        setup.set_name(15, "age".to_owned());
        setup.toggle_bottom(1);
        setup.toggle_right_edge(29);

        let table = ReviewTable::from_setup(&document, &setup).expect("manual table");
        assert_eq!(table.dimensions(), (2, 3));
        let first = table
            .nearest_cell(HistoryPosition { row: 0, col: 5 })
            .unwrap();
        assert_eq!(table.label(first), "name");
        let third = table
            .move_cell(first, CellMove::Next, 2)
            .expect("third cell");
        assert_eq!(table.label(third), "column 3");
        assert_eq!(table.text(third), "note");
    }

    #[test]
    fn pipe_detection_skips_separator_and_numbers_data_rows() {
        let document =
            document(b"| NAME | AGE |\r\n| ---- | --- |\r\n| Alice | 37 |\r\n| Bob | 42 |");
        let table =
            ReviewTable::detect(&document, HistoryPosition { row: 2, col: 3 }).expect("pipe table");
        assert_eq!(table.dimensions(), (2, 2));
        let alice = table
            .nearest_cell(HistoryPosition { row: 2, col: 3 })
            .unwrap();
        assert_eq!(table.row_number(alice), Some(1));
        assert_eq!(table.label(alice), "NAME");
        let age = table.move_cell(alice, CellMove::Next, 1).unwrap();
        assert_eq!(table.label(age), "AGE");
        assert_eq!(table.text(age), "37");
    }

    #[test]
    fn removing_and_readding_tabstop_preserves_draft_name() {
        let mut setup = TableSetup::new(0);
        setup.toggle_tabstop(4);
        setup.set_name(4, "name".to_owned());
        setup.toggle_tabstop(4);
        setup.toggle_tabstop(4);
        assert_eq!(setup.name(4), Some("name"));
    }

    #[test]
    fn manual_right_edge_is_inclusive_and_ignores_following_text() {
        let document = document(b"aa  bbTAIL\r\ncc  ddTAIL");
        let mut setup = TableSetup::new(0);
        setup.toggle_header_mode();
        setup.toggle_tabstop(0);
        setup.toggle_tabstop(4);
        setup.toggle_bottom(1);
        setup.toggle_right_edge(5);

        let table = ReviewTable::from_setup(&document, &setup).expect("manual table");
        let first = table
            .cell_at(HistoryPosition { row: 0, col: 0 })
            .expect("first cell");
        let second = table.move_cell(first, CellMove::Next, 1).unwrap();
        assert_eq!(table.text(second), "bb");
        assert!(table.cell_at(HistoryPosition { row: 0, col: 6 }).is_none());
    }

    #[test]
    fn row_header_toggles_between_one_column_and_off() {
        let document = document(b"NAME  AGE\r\nAlice 37");
        let table = ReviewTable::detect(&document, HistoryPosition { row: 1, col: 0 })
            .expect("fixed-width table");
        let mut table = table;
        let name = table
            .cell_at(HistoryPosition { row: 1, col: 0 })
            .expect("name cell");
        let age = table.move_cell(name, CellMove::Next, 1).unwrap();

        assert!(table.toggle_row_header(name.column));
        assert_eq!(table.row_header_column(), Some(name.column));
        assert_eq!(table.row_header_text(age), Some("Alice"));
        assert!(table.toggle_row_header(age.column));
        assert_eq!(table.row_header_column(), Some(age.column));
        assert!(!table.toggle_row_header(age.column));
        assert_eq!(table.row_header_column(), None);
    }

    #[test]
    fn directional_reentry_skips_structural_rows_and_respects_edges() {
        let document = document(b"  A  1\r\n-------\r\n  B  2");
        let mut setup = TableSetup::new(0);
        setup.toggle_header_mode();
        setup.toggle_tabstop(2);
        setup.toggle_tabstop(5);
        setup.toggle_bottom(2);
        let table = ReviewTable::from_setup(&document, &setup).expect("manual table");

        let from_left = table
            .reentry_cell(HistoryPosition { row: 2, col: 0 }, CellMove::Next)
            .unwrap();
        assert_eq!(table.text(from_left), "B");
        let from_right = table
            .reentry_cell(HistoryPosition { row: 0, col: 79 }, CellMove::Previous)
            .unwrap();
        assert_eq!(table.text(from_right), "1");
        let up = table
            .reentry_cell(HistoryPosition { row: 1, col: 5 }, CellMove::Up)
            .unwrap();
        assert_eq!(table.text(up), "1");
        let down = table
            .reentry_cell(HistoryPosition { row: 1, col: 2 }, CellMove::Down)
            .unwrap();
        assert_eq!(table.text(down), "B");
    }
}
