use crate::{keymap::InputMode, view::View};

mod detection;

pub(crate) use detection::{detect, detect_manual_from_header};
use detection::{is_separator_row, pipe_delimited_cell_text, row_has_fixed_width_columns};

#[derive(Clone, Debug)]
pub(crate) struct SetupState {
    header_row: u16,
    tabstops: Vec<u16>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum TabstopChange {
    Added,
    Removed,
}

impl SetupState {
    pub(crate) fn header_row(&self) -> u16 {
        self.header_row
    }

    pub(crate) fn tabstops(&self) -> &[u16] {
        &self.tabstops
    }

    pub(crate) fn toggle_tabstop(&mut self, col: u16) -> TabstopChange {
        match self.tabstops.binary_search(&col) {
            Ok(index) => {
                self.tabstops.remove(index);
                TabstopChange::Removed
            }
            Err(index) => {
                self.tabstops.insert(index, col);
                TabstopChange::Added
            }
        }
    }
}

pub(crate) struct Session {
    mode: InputMode,
    // Navigation state is intentionally independent of the input mode. Lua can bind table
    // navigation commands in any mode, and failed detection can temporarily clear this state
    // while table mode remains active.
    navigation: Option<TableState>,
    setup: Option<SetupState>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            mode: InputMode::Normal,
            navigation: None,
            setup: None,
        }
    }
}

impl Session {
    pub(crate) fn mode(&self) -> InputMode {
        self.mode
    }

    pub(crate) fn navigation(&self) -> Option<&TableState> {
        self.navigation.as_ref()
    }

    pub(crate) fn navigation_mut(&mut self) -> Option<&mut TableState> {
        self.navigation.as_mut()
    }

    pub(crate) fn set_navigation(&mut self, state: Option<TableState>) {
        self.navigation = state;
    }

    pub(crate) fn setup(&self) -> Option<&SetupState> {
        self.setup.as_ref()
    }

    pub(crate) fn setup_mut(&mut self) -> Option<&mut SetupState> {
        self.setup.as_mut()
    }

    pub(crate) fn enter_setup(&mut self, header_row: u16) -> InputMode {
        let previous = self.mode;
        self.mode = InputMode::TableSetup;
        self.navigation = None;
        self.setup = Some(SetupState {
            header_row,
            tabstops: Vec::new(),
        });
        previous
    }

    pub(crate) fn enter_table(&mut self, state: TableState) -> InputMode {
        let previous = self.mode;
        self.mode = InputMode::Table;
        self.navigation = Some(state);
        self.setup = None;
        previous
    }

    pub(crate) fn exit(&mut self) -> InputMode {
        let previous = self.mode;
        self.mode = InputMode::Normal;
        self.navigation = None;
        self.setup = None;
        previous
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Column {
    start: u16,
    end: u16,
}

#[derive(Clone, Debug)]
pub(crate) struct TableModel {
    top: u16,
    bottom: u16,
    columns: Vec<Column>,
    header_row: Option<u16>,
    delimiter: Option<char>,
}

#[derive(Clone, Debug)]
pub(crate) struct TableState {
    model: TableModel,
    current_col: usize,
}

impl Column {
    #[cfg(test)]
    pub(crate) fn new(start: u16, end: u16) -> Self {
        Self { start, end }
    }

    pub(crate) fn start(&self) -> u16 {
        self.start
    }

    pub(crate) fn end(&self) -> u16 {
        self.end
    }
}

impl TableState {
    pub(crate) fn new(model: TableModel, current_col: usize) -> Self {
        Self { model, current_col }
    }

    pub(crate) fn model(&self) -> &TableModel {
        &self.model
    }

    pub(crate) fn current_col(&self) -> usize {
        self.current_col
    }

    pub(crate) fn set_current_col(&mut self, current_col: usize) {
        self.current_col = current_col;
    }

    pub(crate) fn current_column(&self) -> Option<&Column> {
        self.model.columns.get(self.current_col)
    }
}

impl TableModel {
    #[cfg(test)]
    pub(crate) fn new(
        top: u16,
        bottom: u16,
        columns: Vec<Column>,
        header_row: Option<u16>,
        delimiter: Option<char>,
    ) -> Self {
        Self {
            top,
            bottom,
            columns,
            header_row,
            delimiter,
        }
    }

    pub(crate) fn top(&self) -> u16 {
        self.top
    }

    pub(crate) fn bottom(&self) -> u16 {
        self.bottom
    }

    pub(crate) fn column_count(&self) -> usize {
        self.columns.len()
    }

    pub(crate) fn header_row(&self) -> Option<u16> {
        self.header_row
    }

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
        if self.delimiter == Some('|')
            && let Some(text) = pipe_delimited_cell_text(&view.line(row), col_idx)
        {
            return text.to_string();
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
        if self.delimiter == Some('|') {
            let line = view.line(row);
            return nearest_matching_column(self.columns.len(), preferred, |col| {
                pipe_delimited_cell_text(&line, col).is_some_and(|text| !text.is_empty())
            });
        }
        nearest_matching_column(self.columns.len(), preferred, |col| {
            !self.cell_text(view, row, col).is_empty()
        })
    }

    pub fn is_skippable_row(&self, view: &View, row: u16) -> bool {
        is_separator_row(view, row) || self.is_banner_row(view, row)
    }

    pub fn is_banner_row(&self, view: &View, row: u16) -> bool {
        if row < self.top || row > self.bottom || is_separator_row(view, row) {
            return false;
        }

        if self.delimiter.is_none() {
            return !row_has_fixed_width_columns(view, row);
        }

        let line = view.line(row);
        let trimmed = line.trim();
        if !(trimmed.starts_with('|') && trimmed.ends_with('|')) {
            return false;
        }

        (0..self.columns.len())
            .filter(|&col| {
                pipe_delimited_cell_text(trimmed, col).is_some_and(|text| !text.is_empty())
            })
            .take(2)
            .count()
            <= 1
    }
}

fn nearest_matching_column(
    column_count: usize,
    preferred: usize,
    mut matches: impl FnMut(usize) -> bool,
) -> usize {
    if column_count == 0 {
        return 0;
    }
    let preferred = preferred.min(column_count - 1);
    if matches(preferred) {
        return preferred;
    }
    for offset in 1..column_count {
        if preferred >= offset && matches(preferred - offset) {
            return preferred - offset;
        }
        let right = preferred + offset;
        if right < column_count && matches(right) {
            return right;
        }
    }
    preferred
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view_with_lines(rows: u16, cols: u16, lines: &[&str]) -> View {
        let mut view = View::new(rows, cols);
        let mut data = String::new();
        for (idx, line) in lines.iter().enumerate() {
            if idx > 0 {
                data.push_str("\r\n");
            }
            data.push_str(line);
        }
        view.process_changes(data.as_bytes());
        view
    }

    #[test]
    fn df_capacity_column_does_not_absorb_next_column_digits() {
        let view = view_with_lines(
            24,
            220,
            &[
                "Filesystem     1024-blocks      Used Available Capacity iused      ifree %iused  Mounted on",
                "/dev/disk3s1s1 1942700360   31903848 853122808     4%  453019 4265614040    0%   /",
                "devfs                 411        411         0   100%     712          0  100%   /dev",
            ],
        );

        let model = detect(&view, 1).expect("detect table");
        assert_eq!(model.header_row, Some(0));
        assert!(model.columns.len() >= 9);

        let capacity = model.cell_text(&view, 1, 4);
        let iused = model.cell_text(&view, 1, 5);
        assert_eq!(capacity, "4%");
        assert_eq!(iused, "453019");
    }

    #[test]
    fn docker_created_column_keeps_ago_out_of_status_column() {
        let view = view_with_lines(
            24,
            220,
            &[
                "CONTAINER ID   IMAGE                                COMMAND                  CREATED         STATUS                             PORTS                       NAMES",
                "ce14b2a58e31   ghcr.io/open-webui/open-webui:main   \"bash start.sh\"          12 months ago   Up 17 seconds (health: starting)   0.0.0.0:3000->8080/tcp      open-webui",
                "9f68d2b92c9c   kindest/node:v1.30.0                 \"/usr/local/bin/entr...\"   12 months ago   Up 17 seconds                                                  kind-worker2",
            ],
        );

        let model = detect(&view, 1).expect("detect table");
        let header_row = model.header_row.expect("header row");

        let mut created_col = None;
        let mut status_col = None;
        for idx in 0..model.columns.len() {
            let header = model.cell_text(&view, header_row, idx);
            if header == "CREATED" {
                created_col = Some(idx);
            } else if header == "STATUS" {
                status_col = Some(idx);
            }
        }

        let created_col = created_col.expect("CREATED column");
        let status_col = status_col.expect("STATUS column");

        let created = model.cell_text(&view, 1, created_col);
        let status = model.cell_text(&view, 1, status_col);
        assert_eq!(created, "12 months ago");
        assert!(status.starts_with("Up 17 seconds"));
        assert!(!status.starts_with("ago"));
    }

    #[test]
    fn session_transitions_keep_mode_state_coherent() {
        let mut session = Session::default();
        assert_eq!(session.mode(), InputMode::Normal);
        assert!(session.navigation().is_none());
        assert!(session.setup().is_none());

        assert_eq!(session.enter_setup(4), InputMode::Normal);
        assert_eq!(session.mode(), InputMode::TableSetup);
        assert_eq!(session.setup().unwrap().header_row(), 4);
        assert_eq!(
            session.setup_mut().unwrap().toggle_tabstop(8),
            TabstopChange::Added
        );
        assert_eq!(
            session.setup_mut().unwrap().toggle_tabstop(3),
            TabstopChange::Added
        );
        assert_eq!(session.setup().unwrap().tabstops(), [3, 8]);

        let state = TableState::new(
            TableModel {
                top: 4,
                bottom: 8,
                columns: vec![Column { start: 0, end: 2 }, Column { start: 3, end: 7 }],
                header_row: Some(4),
                delimiter: None,
            },
            1,
        );
        assert_eq!(session.enter_table(state), InputMode::TableSetup);
        assert_eq!(session.mode(), InputMode::Table);
        assert_eq!(session.navigation().unwrap().current_col(), 1);
        assert!(session.setup().is_none());

        assert_eq!(session.exit(), InputMode::Table);
        assert_eq!(session.mode(), InputMode::Normal);
        assert!(session.navigation().is_none());
        assert!(session.setup().is_none());
    }

    #[test]
    fn navigation_state_is_independent_of_input_mode() {
        let mut session = Session::default();
        let state = TableState::new(
            TableModel {
                top: 0,
                bottom: 2,
                columns: vec![Column { start: 0, end: 2 }, Column { start: 3, end: 5 }],
                header_row: Some(0),
                delimiter: None,
            },
            0,
        );

        session.set_navigation(Some(state));
        assert_eq!(session.mode(), InputMode::Normal);
        assert!(session.navigation().is_some());

        session.set_navigation(None);
        assert_eq!(session.mode(), InputMode::Normal);
        assert!(session.navigation().is_none());
    }

    #[test]
    fn nearest_column_search_prefers_left_at_equal_distance() {
        assert_eq!(nearest_matching_column(5, 2, |col| col == 1 || col == 3), 1);
        assert_eq!(nearest_matching_column(5, 9, |col| col == 2), 2);
        assert_eq!(nearest_matching_column(0, 3, |_| true), 0);
    }

    #[test]
    fn toggling_tabstops_keeps_them_sorted_and_unique() {
        let mut setup = SetupState {
            header_row: 0,
            tabstops: Vec::new(),
        };
        assert_eq!(setup.toggle_tabstop(8), TabstopChange::Added);
        assert_eq!(setup.toggle_tabstop(3), TabstopChange::Added);
        assert_eq!(setup.toggle_tabstop(5), TabstopChange::Added);
        assert_eq!(setup.tabstops(), [3, 5, 8]);
        assert_eq!(setup.toggle_tabstop(5), TabstopChange::Removed);
        assert_eq!(setup.tabstops(), [3, 8]);
    }

    #[test]
    fn pipe_cell_text_handles_optional_outer_delimiters_and_missing_cells() {
        assert_eq!(pipe_delimited_cell_text("A | B | C", 0), Some("A"));
        assert_eq!(pipe_delimited_cell_text("A | B | C", 2), Some("C"));
        assert_eq!(pipe_delimited_cell_text("| A | B |", 0), Some("A"));
        assert_eq!(pipe_delimited_cell_text("| A | B |", 1), Some("B"));
        assert_eq!(pipe_delimited_cell_text("| A | B |", 9), Some(""));
        assert_eq!(pipe_delimited_cell_text("no delimiters", 0), None);
    }

    #[test]
    fn manual_detection_filters_tabstops_columns_and_row_bounds() {
        let view = view_with_lines(5, 12, &["", "ID Name Age", "1  Ada  37", "2  Bob  41", ""]);

        let model =
            detect_manual_from_header(&view, 1, &[8, 3, 3, 0, 12]).expect("detect manual table");
        assert_eq!(model.top, 1);
        assert_eq!(model.bottom, 3);
        assert_eq!(model.header_row, Some(1));
        assert_eq!(model.delimiter, None);
        assert_eq!(model.columns.len(), 3);
        assert_eq!(model.cell_text(&view, 2, 0), "1");
        assert_eq!(model.cell_text(&view, 2, 1), "Ada");
        assert_eq!(model.cell_text(&view, 2, 2), "37");

        assert!(detect_manual_from_header(&view, 5, &[3]).is_none());
        assert!(detect_manual_from_header(&view, 1, &[]).is_none());
    }

    #[test]
    fn numeric_fixed_width_tables_use_blank_gutters_without_a_header() {
        let view = view_with_lines(4, 12, &["1  2  3", "4  5  6", "", ""]);

        let model = detect(&view, 3).expect("find nearby numeric table");

        assert_eq!(model.top, 0);
        assert_eq!(model.bottom, 1);
        assert_eq!(model.header_row, None);
        assert_eq!(model.delimiter, None);
        assert_eq!(model.columns.len(), 3);
        assert_eq!(model.cell_text(&view, 0, 0), "1");
        assert_eq!(model.cell_text(&view, 1, 2), "6");
    }

    #[test]
    fn separator_rows_accept_table_punctuation_but_reject_content() {
        let view = view_with_lines(
            4,
            16,
            &["---+===|___:::", " - = + | _ : ", "", "-- data --"],
        );

        assert!(is_separator_row(&view, 0));
        assert!(is_separator_row(&view, 1));
        assert!(!is_separator_row(&view, 2));
        assert!(!is_separator_row(&view, 3));
    }
}
