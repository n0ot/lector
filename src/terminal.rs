//! Engine-neutral terminal state and Lector's Ghostty terminal adapter.

use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, ops::RangeInclusive, sync::Arc};

pub use lector_ghostty::{
    CellSnapshot as Cell, ColorSnapshot as Color, RowSnapshot as Row, StyleSnapshot as Style,
    UnderlineSnapshot as UnderlineStyle,
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorShape {
    Bar,
    #[default]
    Block,
    Underline,
    BlockHollow,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
    pub visible: bool,
    pub shape: CursorShape,
}

/// Cell and pixel geometry for one terminal grid.
///
/// Pixel dimensions are expressed per cell because that is the geometry
/// libghostty-vt consumes. The total grid dimensions are derived separately
/// for PTY `winsize` propagation and terminal size reports.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct TerminalGeometry {
    pub rows: u16,
    pub cols: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
}

impl TerminalGeometry {
    pub const fn new(rows: u16, cols: u16, cell_width_px: u32, cell_height_px: u32) -> Self {
        Self {
            rows: if rows == 0 { 1 } else { rows },
            cols: if cols == 0 { 1 } else { cols },
            cell_width_px,
            cell_height_px,
        }
    }

    pub const fn from_cells(rows: u16, cols: u16) -> Self {
        Self::new(rows, cols, 0, 0)
    }

    pub fn from_grid_pixels(rows: u16, cols: u16, width_px: u32, height_px: u32) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Self::new(
            rows,
            cols,
            width_px / u32::from(cols),
            height_px / u32::from(rows),
        )
    }

    pub const fn width_px(self) -> u32 {
        self.cell_width_px.saturating_mul(self.cols as u32)
    }

    pub const fn height_px(self) -> u32 {
        self.cell_height_px.saturating_mul(self.rows as u32)
    }
}

impl Default for TerminalGeometry {
    fn default() -> Self {
        Self::from_cells(1, 1)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenIdentity {
    #[default]
    Primary,
    Alternate,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseProtocol {
    #[default]
    None,
    Press,
    PressRelease,
    ButtonMotion,
    AnyMotion,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseEncoding {
    #[default]
    Default,
    Utf8,
    Sgr,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct TerminalModes {
    pub application_keypad: bool,
    pub application_cursor: bool,
    pub bracketed_paste: bool,
    pub synchronized_output: bool,
    pub focus_reporting: bool,
    pub kitty_keyboard_flags: u8,
    pub mouse_protocol: MouseProtocol,
    pub mouse_encoding: MouseEncoding,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct HistoryPosition {
    pub row: usize,
    pub col: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticKind {
    PromptStart,
    InputStart,
    CommandStart,
    CommandFinished { exit_code: Option<i32> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticMark {
    pub kind: SemanticKind,
    pub position: HistoryPosition,
    pub alternate_screen: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TerminalEffects {
    pub bells: usize,
    pub title_changed: bool,
    pub events: Vec<TerminalEvent>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardLocation {
    Standard,
    Selection,
    Primary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardContent {
    pub mime: String,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgressState {
    Remove,
    Set,
    Error,
    Indeterminate,
    Pause,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalQuery {
    Enquiry,
    XtVersion,
    Size,
    ColorScheme,
    DeviceAttributes,
    Clipboard,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalEvent {
    Bell,
    TitleChanged(String),
    WorkingDirectoryChanged(String),
    ClipboardWrite {
        location: ClipboardLocation,
        contents: Vec<ClipboardContent>,
    },
    DesktopNotification {
        title: String,
        body: String,
    },
    ProgressReport {
        state: ProgressState,
        progress: Option<u8>,
    },
    Query(TerminalQuery),
    PtyReply(Vec<u8>),
    UnknownSequence {
        content: Vec<u8>,
        truncated: bool,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum TerminalDamage {
    #[default]
    None,
    Rows(Vec<RangeInclusive<u16>>),
    Full,
}

impl TerminalDamage {
    pub fn is_dirty(&self) -> bool {
        !matches!(self, Self::None)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PrintBoundary {
    #[default]
    Continue,
    LineFeed,
    CarriageReturn,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PrintedRun {
    pub text: String,
    pub boundary: PrintBoundary,
}

/// A renderer optimization hint recorded alongside Ghostty mutation. Ghostty's
/// resulting snapshot remains authoritative; consumers must validate an
/// operation before translating it to physical-terminal bytes. Engine-produced
/// `WriteRun` text is always ASCII, so its byte length is also its
/// terminal-column count; the observer discards operation hints for non-ASCII
/// output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalOperation {
    ScrollUp { top: u16, bottom: u16, count: u16 },
    ScrollDown { top: u16, bottom: u16, count: u16 },
    InsertLines { row: u16, bottom: u16, count: u16 },
    DeleteLines { row: u16, bottom: u16, count: u16 },
    InsertChars { row: u16, col: u16, count: u16 },
    DeleteChars { row: u16, col: u16, count: u16 },
    EraseChars { row: u16, col: u16, count: u16 },
    WriteRun { row: u16, col: u16, text: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateSummary {
    pub effects: TerminalEffects,
    pub damage: TerminalDamage,
    pub pty_replies: Vec<u8>,
    pub printed_runs: Vec<PrintedRun>,
    /// At least one update since the last accessibility commit used
    /// structural terminal output, so `printed_runs` must not be treated as a
    /// completed linear record.
    pub output_report_structural: bool,
    /// Whether the newest engine update retained an incomplete authoritative
    /// parser continuation.
    pub parser_continuation: bool,
    pub operations: Vec<TerminalOperation>,
    pub cursor_operations: usize,
    pub scroll_operations: usize,
    pub history_changed: bool,
    pub changed_rows: Vec<RangeInclusive<u16>>,
    pub cursor_before: Cursor,
    pub cursor_after: Cursor,
    pub screen_before: ScreenIdentity,
    pub screen_after: ScreenIdentity,
    pub synchronized_output: bool,
    /// This batch crossed an actual false-to-true synchronized-output
    /// boundary, including a close/reopen sequence whose final mode is still
    /// enabled.
    pub synchronized_output_opened: bool,
    /// This batch ended exactly at a real synchronized-output close. It is a
    /// stabilization boundary only after its exact render has flushed.
    pub synchronized_output_closed: bool,
    /// This batch ended at an OSC 133 input-start boundary with no subsequent
    /// visible, structural, semantic, or incomplete parser output.
    pub semantic_input_boundary: bool,
    pub cursor_visibility_restored: bool,
    pub batch_count: usize,
}

impl Default for UpdateSummary {
    fn default() -> Self {
        Self {
            effects: TerminalEffects::default(),
            damage: TerminalDamage::None,
            pty_replies: Vec::new(),
            printed_runs: Vec::new(),
            output_report_structural: false,
            parser_continuation: false,
            operations: Vec::new(),
            cursor_operations: 0,
            scroll_operations: 0,
            history_changed: false,
            changed_rows: Vec::new(),
            cursor_before: Cursor::default(),
            cursor_after: Cursor::default(),
            screen_before: ScreenIdentity::Primary,
            screen_after: ScreenIdentity::Primary,
            synchronized_output: false,
            synchronized_output_opened: false,
            synchronized_output_closed: false,
            semantic_input_boundary: false,
            cursor_visibility_restored: false,
            batch_count: 0,
        }
    }
}

impl UpdateSummary {
    pub fn merge(&mut self, mut next: Self) {
        if next.batch_count == 0 {
            return;
        }
        if self.batch_count == 0 {
            self.cursor_before = next.cursor_before;
            self.screen_before = next.screen_before;
        }
        self.cursor_after = next.cursor_after;
        self.screen_after = next.screen_after;
        self.synchronized_output = next.synchronized_output;
        self.synchronized_output_opened |= next.synchronized_output_opened;
        self.synchronized_output_closed = next.synchronized_output_closed;
        self.semantic_input_boundary = next.semantic_input_boundary;
        self.cursor_visibility_restored = next.cursor_visibility_restored;
        self.batch_count = self.batch_count.saturating_add(next.batch_count);
        self.effects.bells = self.effects.bells.saturating_add(next.effects.bells);
        self.effects.title_changed |= next.effects.title_changed;
        self.effects.events.append(&mut next.effects.events);
        self.pty_replies.append(&mut next.pty_replies);
        self.output_report_structural |= next.output_report_structural;
        self.parser_continuation = next.parser_continuation;
        append_printed_runs(&mut self.printed_runs, next.printed_runs);
        append_terminal_operations(&mut self.operations, next.operations);
        self.cursor_operations = self
            .cursor_operations
            .saturating_add(next.cursor_operations);
        self.scroll_operations = self
            .scroll_operations
            .saturating_add(next.scroll_operations);
        self.history_changed |= next.history_changed;
        merge_row_ranges(&mut self.changed_rows, next.changed_rows);
        self.damage = merge_damage(
            std::mem::take(&mut self.damage),
            std::mem::take(&mut next.damage),
        );
    }

    pub fn printed_text(&self) -> String {
        let mut text = String::new();
        self.printed_text_into(&mut text);
        text
    }

    pub fn printed_text_into(&self, text: &mut String) {
        text.clear();
        for run in &self.printed_runs {
            match run.boundary {
                PrintBoundary::Continue => {}
                PrintBoundary::LineFeed => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                }
                PrintBoundary::CarriageReturn => {
                    let line_start = text.rfind('\n').map_or(0, |index| index + 1);
                    text.truncate(line_start);
                }
            }
            text.push_str(&run.text);
        }
    }

    /// Whether this cumulative update is safe to finalize as a completed
    /// line-oriented output record without waiting for screen quiet.
    pub fn completes_linear_output_record(&self) -> bool {
        self.has_linear_output_report()
            && self.residual_carriage_returns_are_record_prefixes()
            && self
                .printed_runs
                .last()
                .is_some_and(|run| run.boundary == PrintBoundary::LineFeed && run.text.is_empty())
    }

    /// Whether the parallel print observer describes ordinary primary-screen
    /// output rather than a structural redraw. Ambiguous reports must fall
    /// back to the authoritative screen diff even after the quiet timer.
    pub fn has_linear_output_report(&self) -> bool {
        self.batch_count > 0
            && !self.output_report_structural
            && !self.parser_continuation
            && self.screen_before == ScreenIdentity::Primary
            && self.screen_after == ScreenIdentity::Primary
    }

    fn residual_carriage_returns_are_record_prefixes(&self) -> bool {
        let mut content_since_line_feed = self.cursor_before.col != 0;
        for run in &self.printed_runs {
            match run.boundary {
                PrintBoundary::Continue => {}
                PrintBoundary::LineFeed => content_since_line_feed = false,
                PrintBoundary::CarriageReturn if content_since_line_feed => return false,
                PrintBoundary::CarriageReturn => {}
            }
            if !run.text.is_empty() {
                content_since_line_feed = true;
            }
        }
        true
    }
}

fn append_printed_runs(target: &mut Vec<PrintedRun>, source: Vec<PrintedRun>) {
    for run in source {
        if run.boundary == PrintBoundary::LineFeed
            && run.text.is_empty()
            && let Some(previous) = target.last_mut()
            && previous.boundary == PrintBoundary::CarriageReturn
            && previous.text.is_empty()
        {
            previous.boundary = PrintBoundary::LineFeed;
            continue;
        }
        if run.boundary == PrintBoundary::Continue
            && let Some(previous) = target.last_mut()
        {
            previous.text.push_str(&run.text);
        } else {
            target.push(run);
        }
    }
}

// A public `UpdateSummary` may contain manually constructed Unicode WriteRuns,
// so their byte length cannot be cached or treated as a column count. Split
// coalesced runs on fixed absolute-column boundaries instead: every adjacency
// rescan is then bounded while merge grouping and Unicode column semantics stay
// deterministic. UTF-8 uses at most four bytes per scalar value.
const MAX_COALESCED_WRITE_RUN_COLUMNS: usize = 256;
const MAX_COALESCED_WRITE_RUN_BYTES: usize = MAX_COALESCED_WRITE_RUN_COLUMNS * 4;

fn append_terminal_operations(target: &mut Vec<TerminalOperation>, source: Vec<TerminalOperation>) {
    for operation in source {
        match operation {
            TerminalOperation::WriteRun { row, col, text } => {
                append_terminal_write_run(target, row, col, text);
            }
            operation => target.push(operation),
        }
    }
}

fn append_terminal_write_run(
    target: &mut Vec<TerminalOperation>,
    row: u16,
    col: u16,
    text: String,
) {
    if text.is_empty() || col == u16::MAX {
        append_terminal_write_run_chunk(target, row, col, text);
        return;
    }

    let mut chunk_start = 0usize;
    let mut chunk_col = col;
    let mut chunk_columns = 0usize;
    let mut chunk_limit =
        MAX_COALESCED_WRITE_RUN_COLUMNS - usize::from(chunk_col) % MAX_COALESCED_WRITE_RUN_COLUMNS;
    let mut split = false;
    for (byte_index, _) in text.char_indices() {
        if chunk_columns == chunk_limit {
            append_terminal_write_run_chunk(
                target,
                row,
                chunk_col,
                text[chunk_start..byte_index].to_owned(),
            );
            split = true;
            chunk_col = chunk_col.saturating_add(chunk_columns.try_into().unwrap_or(u16::MAX));
            chunk_start = byte_index;
            chunk_columns = 0;
            if chunk_col == u16::MAX {
                append_terminal_write_run_chunk(
                    target,
                    row,
                    chunk_col,
                    text[chunk_start..].to_owned(),
                );
                return;
            }
            chunk_limit = MAX_COALESCED_WRITE_RUN_COLUMNS
                - usize::from(chunk_col) % MAX_COALESCED_WRITE_RUN_COLUMNS;
        }
        chunk_columns = chunk_columns.saturating_add(1);
    }

    if split {
        append_terminal_write_run_chunk(target, row, chunk_col, text[chunk_start..].to_owned());
    } else {
        append_terminal_write_run_chunk(target, row, chunk_col, text);
    }
}

fn append_terminal_write_run_chunk(
    target: &mut Vec<TerminalOperation>,
    row: u16,
    col: u16,
    text: String,
) {
    if let Some(TerminalOperation::WriteRun {
        row: previous_row,
        col: previous_col,
        text: previous_text,
    }) = target.last_mut()
        && *previous_row == row
        && usize::from(*previous_col) / MAX_COALESCED_WRITE_RUN_COLUMNS
            == usize::from(col) / MAX_COALESCED_WRITE_RUN_COLUMNS
        && previous_text
            .len()
            .checked_add(text.len())
            .is_some_and(|len| len <= MAX_COALESCED_WRITE_RUN_BYTES)
        && write_run_end_col(*previous_col, previous_text) == col
    {
        previous_text.push_str(&text);
    } else {
        target.push(TerminalOperation::WriteRun { row, col, text });
    }
}

fn write_run_end_col(col: u16, text: &str) -> u16 {
    // The caller bounds this scan to one fixed-column chunk. Engine-produced
    // write hints are ASCII, while manually constructed Unicode hints retain
    // their public character-column semantics.
    let columns = if text.is_ascii() {
        text.len()
    } else {
        text.chars().count()
    };
    col.saturating_add(columns.try_into().unwrap_or(u16::MAX))
}

fn merge_damage(left: TerminalDamage, right: TerminalDamage) -> TerminalDamage {
    match (left, right) {
        (TerminalDamage::Full, _) | (_, TerminalDamage::Full) => TerminalDamage::Full,
        (TerminalDamage::None, damage) | (damage, TerminalDamage::None) => damage,
        (TerminalDamage::Rows(mut left), TerminalDamage::Rows(right)) => {
            merge_row_ranges(&mut left, right);
            TerminalDamage::Rows(left)
        }
    }
}

fn normalize_row_ranges(ranges: &mut Vec<RangeInclusive<u16>>) {
    ranges.sort_unstable_by_key(|range| *range.start());
    coalesce_sorted_row_ranges(ranges);
}

fn merge_row_ranges(target: &mut Vec<RangeInclusive<u16>>, mut source: Vec<RangeInclusive<u16>>) {
    if !row_ranges_are_normalized(target) {
        normalize_row_ranges(target);
    }
    if !row_ranges_are_normalized(&source) {
        normalize_row_ranges(&mut source);
    }
    if source.is_empty() {
        return;
    }
    if target.is_empty() {
        *target = source;
        return;
    }

    if source
        .first()
        .is_some_and(|first| first.start() >= target.last().expect("target is nonempty").start())
    {
        for range in source {
            append_sorted_row_range(target, range);
        }
        return;
    }

    // Both inputs are sorted and disjoint internally. Grow the target once,
    // then perform the ordinary backwards merge with `source` as the separate
    // right-hand storage. This reuses the target allocation and avoids sorting
    // or rebuilding all previously accumulated ranges.
    let left_len = target.len();
    let right_len = source.len();
    target.resize(left_len.saturating_add(right_len), 0..=0);
    let mut left = left_len;
    let mut right = right_len;
    let mut write = left_len.saturating_add(right_len);
    while left > 0 || right > 0 {
        write -= 1;
        let take_left =
            right == 0 || (left > 0 && target[left - 1].start() > source[right - 1].start());
        if take_left {
            left -= 1;
            if write != left {
                target[write] = target[left].clone();
            }
        } else {
            right -= 1;
            target[write] = source[right].clone();
        }
    }
    coalesce_sorted_row_ranges(target);
}

fn row_ranges_are_normalized(ranges: &[RangeInclusive<u16>]) -> bool {
    ranges
        .windows(2)
        .all(|pair| pair[1].start() > &pair[0].end().saturating_add(1))
}

fn append_sorted_row_range(target: &mut Vec<RangeInclusive<u16>>, range: RangeInclusive<u16>) {
    if let Some(previous) = target.last_mut()
        && range.start() <= &previous.end().saturating_add(1)
    {
        let start = *previous.start();
        let end = (*previous.end()).max(*range.end());
        *previous = start..=end;
    } else {
        target.push(range);
    }
}

fn coalesce_sorted_row_ranges(ranges: &mut Vec<RangeInclusive<u16>>) {
    let mut write = 0usize;
    for read in 0..ranges.len() {
        if write > 0 && ranges[read].start() <= &ranges[write - 1].end().saturating_add(1) {
            let start = *ranges[write - 1].start();
            let end = (*ranges[write - 1].end()).max(*ranges[read].end());
            ranges[write - 1] = start..=end;
        } else {
            if write != read {
                ranges[write] = ranges[read].clone();
            }
            write += 1;
        }
    }
    ranges.truncate(write);
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Viewport {
    #[default]
    Live,
    Scrollback(usize),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TerminalSnapshot {
    pub rows: Arc<Vec<Row>>,
    pub scrollback: VecDeque<Row>,
    pub cursor: Cursor,
    pub geometry: TerminalGeometry,
    pub screen: ScreenIdentity,
    pub modes: TerminalModes,
    pub title: Option<String>,
    pub working_directory: Option<String>,
    pub semantic_marks: Vec<SemanticMark>,
    /// Monotonic lineage coordinate of the bounded primary-history window.
    /// This is stable across Ghostty's internal whole-page coordinate rebases.
    pub history_origin: usize,
    pub scrollback_extent: usize,
    pub viewport: Viewport,
}

impl TerminalSnapshot {
    pub fn size(&self) -> (u16, u16) {
        (
            self.rows.len().try_into().unwrap_or(u16::MAX),
            self.rows
                .first()
                .map_or(0, |row| row.cells.len().try_into().unwrap_or(u16::MAX)),
        )
    }

    pub fn cell(&self, row: u16, col: u16) -> Option<&Cell> {
        self.rows.get(usize::from(row))?.cells.get(usize::from(col))
    }

    pub fn row_wrapped(&self, row: u16) -> bool {
        self.rows
            .get(usize::from(row))
            .is_some_and(|row| row.wrapped)
    }

    pub fn cursor_position(&self) -> (u16, u16) {
        (self.cursor.row, self.cursor.col)
    }

    pub fn alternate_screen(&self) -> bool {
        self.screen == ScreenIdentity::Alternate
    }

    pub fn application_keypad(&self) -> bool {
        self.modes.application_keypad
    }

    pub fn application_cursor(&self) -> bool {
        self.modes.application_cursor
    }

    pub fn hide_cursor(&self) -> bool {
        !self.cursor.visible
    }

    pub fn bracketed_paste(&self) -> bool {
        self.modes.bracketed_paste
    }

    pub fn focus_reporting(&self) -> bool {
        self.modes.focus_reporting
    }

    pub fn kitty_keyboard_flags(&self) -> u8 {
        self.modes.kitty_keyboard_flags
    }

    pub fn mouse_protocol_mode(&self) -> MouseProtocol {
        self.modes.mouse_protocol
    }

    pub fn mouse_protocol_encoding(&self) -> MouseEncoding {
        self.modes.mouse_encoding
    }

    pub fn contents(&self) -> String {
        let mut contents = String::new();
        let mut wrapping = false;
        for row in self.rows.iter() {
            let row_start = contents.len();
            row.append_contents_to(&mut contents);
            if contents.len() == row_start && wrapping {
                contents.push('\n');
            }
            if !row.wrapped {
                contents.push('\n');
            }
            wrapping = row.wrapped;
        }
        while contents.ends_with('\n') {
            contents.pop();
        }
        contents
    }

    pub fn rows(&self, start: u16, width: u16) -> impl Iterator<Item = String> + '_ {
        self.rows
            .iter()
            .map(move |row| row_contents(row, start, width))
    }

    pub fn contents_between(
        &self,
        start_row: u16,
        start_col: u16,
        end_row: u16,
        end_col: u16,
    ) -> String {
        match start_row.cmp(&end_row) {
            std::cmp::Ordering::Less => {
                let (_, cols) = self.size();
                let mut contents = String::new();
                for row_index in start_row..=end_row {
                    let Some(row) = self.rows.get(usize::from(row_index)) else {
                        break;
                    };
                    if row_index == start_row {
                        append_row_contents(
                            row,
                            start_col,
                            cols.saturating_sub(start_col),
                            &mut contents,
                        );
                    } else if row_index == end_row {
                        append_row_contents(row, 0, end_col, &mut contents);
                    } else {
                        append_row_contents(row, 0, cols, &mut contents);
                    }
                    if row_index != end_row && !row.wrapped {
                        contents.push('\n');
                    }
                }
                contents
            }
            std::cmp::Ordering::Equal if start_col < end_col => self
                .rows
                .get(usize::from(start_row))
                .map_or_else(String::new, |row| {
                    let mut contents = String::new();
                    append_row_contents(row, start_col, end_col - start_col, &mut contents);
                    contents
                }),
            _ => String::new(),
        }
    }

    pub fn contents_full(&self) -> String {
        let mut out = String::new();
        self.contents_full_into(&mut out);
        out
    }

    pub fn contents_full_into(&self, out: &mut String) {
        out.clear();
        let (_, cols) = self.size();
        for row in self.rows.iter() {
            let row_start = out.len();
            append_row_contents(row, 0, cols, out);
            let trimmed_row_len = out[row_start..].trim_end().len();
            out.truncate(row_start + trimmed_row_len);
            out.push('\n');
        }
    }

    /// Returns whether any visible cell contains a non-whitespace character.
    ///
    /// This is equivalent to `!self.contents().trim().is_empty()` without
    /// constructing the whole-screen string.
    pub fn has_visible_non_whitespace_content(&self) -> bool {
        self.rows.iter().any(|row| {
            row.cells.iter().any(|cell| {
                !cell.continuation
                    && cell
                        .grapheme
                        .chars()
                        .any(|character| !character.is_whitespace())
            })
        })
    }
}

fn row_contents(row: &Row, start: u16, width: u16) -> String {
    let mut output = String::new();
    append_row_contents(row, start, width, &mut output);
    output
}

fn append_row_contents(row: &Row, start: u16, width: u16, output: &mut String) {
    row.append_contents_range_to(output, usize::from(start), usize::from(width));
}

pub trait TerminalEngine {
    fn advance(&mut self, bytes: &[u8]) -> UpdateSummary;
    fn resize(&mut self, rows: u16, cols: u16) -> UpdateSummary {
        self.resize_with_geometry(TerminalGeometry::from_cells(rows, cols))
    }
    fn resize_with_geometry(&mut self, geometry: TerminalGeometry) -> UpdateSummary;
    fn reset(&mut self) -> UpdateSummary;
    fn select_viewport(&mut self, viewport: Viewport);
    fn viewport(&self) -> Viewport;
    fn snapshot(&self) -> &TerminalSnapshot;
    fn snapshot_with_history(&mut self) -> TerminalSnapshot;
    fn scrollback_extent(&self) -> usize;
}

fn full_row_range(rows: u16) -> Vec<RangeInclusive<u16>> {
    (rows > 0).then(|| 0..=rows - 1).into_iter().collect()
}

pub use lector_ghostty::{
    ClipboardContentSnapshot as GhosttyClipboardContent,
    ClipboardLocationSnapshot as GhosttyClipboardLocation, CursorSnapshot as GhosttyCursor,
    EffectSnapshot as GhosttyEffect, ModesSnapshot as GhosttyModes,
    OperationSnapshot as GhosttyOperation, PrintBoundarySnapshot as GhosttyPrintBoundary,
    ProgressStateSnapshot as GhosttyProgressState, QuerySnapshot as GhosttyQuery,
    RenderDamageSnapshot as GhosttyDamage, SemanticKindSnapshot as GhosttySemanticKind,
    TerminalSnapshot as GhosttySnapshot, UpdateSnapshot as GhosttyUpdate,
};

/// Lector's sole authoritative terminal engine.
///
/// This adapter is intentionally not `Send` or `Sync`: the owned Ghostty
/// handles stay on the thread where the engine was constructed.
pub struct GhosttyEngine {
    terminal: lector_ghostty::Terminal,
    snapshot: TerminalSnapshot,
    viewport: Viewport,
    synchronized_output_open_snapshot: Option<TerminalSnapshot>,
    #[cfg(test)]
    snapshot_refresh_count: usize,
    #[cfg(test)]
    history_snapshot_refresh_count: usize,
}

/// A Ghostty-owned review anchor. The reference follows its cell through
/// scrolling and reflow and resolves to `None` once that cell leaves Lector's
/// logically retained history window.
pub struct GhosttyReviewMark {
    reference: lector_ghostty::TrackedGridRef,
    alternate_screen: bool,
}

const _: () = {
    struct Check<T: ?Sized>(std::marker::PhantomData<T>);
    trait AmbiguousIfSend<A> {
        fn marker() {}
    }
    impl<T: ?Sized> AmbiguousIfSend<()> for Check<T> {}
    impl<T: ?Sized + Send> AmbiguousIfSend<u8> for Check<T> {}

    trait AmbiguousIfSync<A> {
        fn marker() {}
    }
    impl<T: ?Sized> AmbiguousIfSync<()> for Check<T> {}
    impl<T: ?Sized + Sync> AmbiguousIfSync<u8> for Check<T> {}

    let _ = <Check<GhosttyEngine> as AmbiguousIfSend<_>>::marker;
    let _ = <Check<GhosttyEngine> as AmbiguousIfSync<_>>::marker;
};

impl GhosttyEngine {
    pub fn new(rows: u16, cols: u16) -> Result<Self, lector_ghostty::Error> {
        Self::new_with_scrollback(rows, cols, 10_000)
    }

    pub fn new_with_scrollback(
        rows: u16,
        cols: u16,
        scrollback_capacity: usize,
    ) -> Result<Self, lector_ghostty::Error> {
        let profile = crate::terminal_protocol::VirtualTerminalProfile::lector(
            TerminalGeometry::from_cells(rows, cols),
            crate::terminal_protocol::ColorScheme::Dark,
        );
        Self::new_with_scrollback_and_profile(rows, cols, scrollback_capacity, profile)
    }

    pub fn new_with_profile(
        rows: u16,
        cols: u16,
        profile: crate::terminal_protocol::VirtualTerminalProfile,
    ) -> Result<Self, lector_ghostty::Error> {
        Self::new_with_scrollback_and_profile(rows, cols, 10_000, profile)
    }

    fn new_with_scrollback_and_profile(
        rows: u16,
        cols: u16,
        scrollback_capacity: usize,
        profile: crate::terminal_protocol::VirtualTerminalProfile,
    ) -> Result<Self, lector_ghostty::Error> {
        let terminal = lector_ghostty::Terminal::new_with_profile(
            rows,
            cols,
            scrollback_capacity,
            lector_ghostty::TerminalProfile {
                rows: profile.geometry.rows,
                columns: profile.geometry.cols,
                cell_width: profile.geometry.cell_width_px,
                cell_height: profile.geometry.cell_height_px,
                color_scheme: match profile.color_scheme {
                    crate::terminal_protocol::ColorScheme::Light => {
                        lector_ghostty::TerminalColorScheme::Light
                    }
                    crate::terminal_protocol::ColorScheme::Dark => {
                        lector_ghostty::TerminalColorScheme::Dark
                    }
                },
                enquiry: profile.enquiry,
                version: profile.version,
                da_conformance: profile.da_conformance,
                da_features: profile.da_features,
                da_device_type: profile.da_device_type,
                da_firmware_version: profile.da_firmware_version,
                da_unit_id: profile.da_unit_id,
                clipboard_read: profile.clipboard_read,
            },
        )?;
        let snapshot = normalize_ghostty_snapshot(terminal.snapshot());
        Ok(Self {
            terminal,
            snapshot,
            viewport: Viewport::Live,
            synchronized_output_open_snapshot: None,
            #[cfg(test)]
            snapshot_refresh_count: 0,
            #[cfg(test)]
            history_snapshot_refresh_count: 0,
        })
    }

    pub fn try_advance(&mut self, bytes: &[u8]) -> Result<UpdateSummary, lector_ghostty::Error> {
        let mut update = self.terminal.advance(bytes)?;
        let synchronized_output_opened = update.synchronized_output_open_snapshot.is_some();
        self.synchronized_output_open_snapshot = update
            .synchronized_output_open_snapshot
            .take()
            .map(|snapshot| normalize_ghostty_snapshot(&snapshot));
        self.refresh_snapshot_after_update(&update.damage)?;
        let mut update = normalize_ghostty_update(update);
        update.synchronized_output_opened = synchronized_output_opened;
        Ok(update)
    }

    pub fn take_synchronized_output_open_snapshot(&mut self) -> Option<TerminalSnapshot> {
        self.synchronized_output_open_snapshot.take()
    }

    /// Fallible direct adapter API. Production callers use `TerminalEngine`;
    /// diagnostics and adapter tests can retain the underlying error.
    pub fn advance(&mut self, bytes: &[u8]) -> Result<UpdateSummary, lector_ghostty::Error> {
        self.try_advance(bytes)
    }

    pub fn try_resize(&mut self, rows: u16, cols: u16) -> Result<(), lector_ghostty::Error> {
        self.terminal.resize(rows, cols)?;
        self.refresh_snapshot()
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<(), lector_ghostty::Error> {
        self.try_resize(rows, cols)
    }

    pub fn try_resize_with_geometry(
        &mut self,
        geometry: TerminalGeometry,
    ) -> Result<(), lector_ghostty::Error> {
        self.terminal.resize_with_geometry(
            geometry.rows,
            geometry.cols,
            geometry.cell_width_px,
            geometry.cell_height_px,
        )?;
        self.refresh_snapshot()
    }

    pub fn resize_with_geometry(
        &mut self,
        geometry: TerminalGeometry,
    ) -> Result<(), lector_ghostty::Error> {
        self.try_resize_with_geometry(geometry)
    }

    pub fn diagnostic_snapshot(
        &self,
    ) -> Result<lector_ghostty::DiagnosticSnapshot, lector_ghostty::Error> {
        self.terminal.diagnostic_snapshot()
    }

    pub fn restore_diagnostic_snapshot(
        snapshot: lector_ghostty::DiagnosticSnapshot,
    ) -> Result<Self, lector_ghostty::Error> {
        let terminal = lector_ghostty::Terminal::restore_diagnostic_snapshot(snapshot)?;
        let snapshot = normalize_ghostty_snapshot(terminal.snapshot());
        Ok(Self {
            terminal,
            snapshot,
            viewport: Viewport::Live,
            synchronized_output_open_snapshot: None,
            #[cfg(test)]
            snapshot_refresh_count: 0,
            #[cfg(test)]
            history_snapshot_refresh_count: 0,
        })
    }

    pub fn try_reset(&mut self) -> Result<(), lector_ghostty::Error> {
        self.terminal.reset()?;
        self.viewport = Viewport::Live;
        self.refresh_snapshot()
    }

    pub fn reset(&mut self) -> Result<(), lector_ghostty::Error> {
        self.try_reset()
    }

    pub fn ghostty_snapshot(&self) -> &GhosttySnapshot {
        self.terminal.snapshot()
    }

    pub fn normalized_snapshot(&self) -> TerminalSnapshot {
        normalize_ghostty_snapshot(self.terminal.snapshot())
    }

    pub fn normalized_snapshot_with_history(
        &self,
    ) -> Result<TerminalSnapshot, lector_ghostty::Error> {
        self.terminal
            .snapshot_with_history()
            .map(|snapshot| normalize_ghostty_snapshot(&snapshot))
    }

    pub fn normalized_history_rows_from(
        &self,
        logical_start: usize,
    ) -> Result<Vec<Row>, lector_ghostty::Error> {
        self.terminal.history_rows_from(logical_start)
    }

    pub fn ghostty_snapshot_with_history(&self) -> Result<GhosttySnapshot, lector_ghostty::Error> {
        self.terminal.snapshot_with_history()
    }

    pub fn kitty_image_placements(
        &self,
    ) -> Result<Vec<lector_ghostty::KittyImagePlacementSnapshot>, lector_ghostty::Error> {
        self.terminal.kitty_image_placements()
    }

    pub fn scrollback_extent(&self) -> usize {
        self.terminal.scrollback_extent()
    }

    pub fn track_review_mark(
        &self,
        position: HistoryPosition,
    ) -> Result<GhosttyReviewMark, lector_ghostty::Error> {
        let snapshot = self.terminal.snapshot();
        let logical_rows = snapshot
            .scrollback_extent
            .saturating_add(snapshot.rows.len());
        if position.row >= logical_rows {
            return Err(lector_ghostty::Error::InvalidValue);
        }
        let physical_row = self
            .terminal
            .physical_history_origin()?
            .saturating_add(position.row);
        Ok(GhosttyReviewMark {
            reference: self
                .terminal
                .track_screen_position(physical_row, position.col)?,
            alternate_screen: snapshot.alternate_screen,
        })
    }

    pub fn review_mark_position(
        &self,
        mark: &GhosttyReviewMark,
    ) -> Result<Option<HistoryPosition>, lector_ghostty::Error> {
        if mark.alternate_screen != self.terminal.snapshot().alternate_screen {
            return Ok(None);
        }
        let Some((physical_row, col)) = mark.reference.screen_position()? else {
            return Ok(None);
        };
        let origin = self.terminal.physical_history_origin()?;
        if physical_row < origin {
            return Ok(None);
        }
        let row = physical_row - origin;
        let logical_rows = self
            .terminal
            .scrollback_extent()
            .saturating_add(self.terminal.snapshot().rows.len());
        Ok((row < logical_rows).then_some(HistoryPosition { row, col }))
    }

    /// Replies are drained into `UpdateSummary` and routed by the owning
    /// application's capability broker. This accessor is always empty.
    pub fn pty_replies(&self) -> &[u8] {
        &[]
    }

    fn refresh_snapshot(&mut self) -> Result<(), lector_ghostty::Error> {
        #[cfg(test)]
        {
            self.snapshot_refresh_count = self.snapshot_refresh_count.saturating_add(1);
        }
        let live = normalize_ghostty_snapshot(self.terminal.snapshot());
        let Viewport::Scrollback(requested_offset) = self.viewport else {
            self.snapshot = live;
            return Ok(());
        };
        debug_assert_ne!(requested_offset, 0, "zero scrollback is the live viewport");
        #[cfg(test)]
        {
            self.history_snapshot_refresh_count =
                self.history_snapshot_refresh_count.saturating_add(1);
        }
        let full = self
            .terminal
            .snapshot_with_history()
            .map(|snapshot| normalize_ghostty_snapshot(&snapshot))?;
        let offset = requested_offset.min(full.scrollback.len());
        if offset == 0 {
            self.viewport = Viewport::Live;
            self.snapshot = live;
            return Ok(());
        }
        let visible_rows = live.rows.len();
        let history_rows = full.scrollback.len();
        let mut all_rows = full.scrollback.into_iter().collect::<Vec<_>>();
        all_rows.extend(full.rows.iter().cloned());
        let start = history_rows.saturating_sub(offset);
        let end = start.saturating_add(visible_rows).min(all_rows.len());
        self.snapshot = live;
        self.snapshot.rows = Arc::new(all_rows[start..end].to_vec());
        while self.snapshot.rows.len() < visible_rows {
            Arc::make_mut(&mut self.snapshot.rows).push(Row {
                cells: Arc::new(vec![
                    Cell::default();
                    usize::from(self.snapshot.geometry.cols)
                ]),
                wrapped: false,
            });
        }
        self.snapshot.viewport = Viewport::Scrollback(offset);
        self.viewport = self.snapshot.viewport;
        Ok(())
    }

    fn refresh_snapshot_after_update(
        &mut self,
        _damage: &GhosttyDamage,
    ) -> Result<(), lector_ghostty::Error> {
        if self.viewport != Viewport::Live {
            return self.refresh_snapshot();
        }
        #[cfg(test)]
        {
            self.snapshot_refresh_count = self.snapshot_refresh_count.saturating_add(1);
        }

        let source = self.terminal.snapshot();
        self.snapshot.rows = Arc::clone(&source.rows);
        self.snapshot.scrollback.clear();
        refresh_normalized_ghostty_metadata(&mut self.snapshot, source);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn reset_snapshot_refresh_counts(&mut self) {
        self.snapshot_refresh_count = 0;
        self.history_snapshot_refresh_count = 0;
    }

    #[cfg(test)]
    pub(crate) const fn snapshot_refresh_counts(&self) -> (usize, usize) {
        (
            self.snapshot_refresh_count,
            self.history_snapshot_refresh_count,
        )
    }
}

impl TerminalEngine for GhosttyEngine {
    fn advance(&mut self, bytes: &[u8]) -> UpdateSummary {
        self.try_advance(bytes)
            .unwrap_or_else(|error| panic!("Ghostty terminal advance failed: {error}"))
    }

    fn resize_with_geometry(&mut self, geometry: TerminalGeometry) -> UpdateSummary {
        let before = self.snapshot.clone();
        self.try_resize_with_geometry(geometry)
            .unwrap_or_else(|error| panic!("Ghostty terminal resize failed: {error}"));
        UpdateSummary {
            damage: TerminalDamage::Full,
            changed_rows: full_row_range(self.snapshot.geometry.rows),
            cursor_before: before.cursor,
            cursor_after: self.snapshot.cursor,
            screen_before: before.screen,
            screen_after: self.snapshot.screen,
            synchronized_output: self.snapshot.modes.synchronized_output,
            synchronized_output_opened: false,
            synchronized_output_closed: false,
            batch_count: 1,
            ..UpdateSummary::default()
        }
    }

    fn reset(&mut self) -> UpdateSummary {
        let before = self.snapshot.clone();
        self.try_reset()
            .unwrap_or_else(|error| panic!("Ghostty terminal reset failed: {error}"));
        UpdateSummary {
            damage: TerminalDamage::Full,
            changed_rows: full_row_range(self.snapshot.geometry.rows),
            cursor_before: before.cursor,
            cursor_after: self.snapshot.cursor,
            screen_before: before.screen,
            screen_after: self.snapshot.screen,
            synchronized_output: self.snapshot.modes.synchronized_output,
            synchronized_output_opened: false,
            synchronized_output_closed: false,
            batch_count: 1,
            ..UpdateSummary::default()
        }
    }

    fn select_viewport(&mut self, viewport: Viewport) {
        let viewport = match viewport {
            Viewport::Scrollback(0) => Viewport::Live,
            viewport => viewport,
        };
        if self.viewport == viewport {
            return;
        }
        self.viewport = viewport;
        self.refresh_snapshot()
            .unwrap_or_else(|error| panic!("Ghostty viewport selection failed: {error}"));
    }

    fn viewport(&self) -> Viewport {
        self.viewport
    }

    fn snapshot(&self) -> &TerminalSnapshot {
        &self.snapshot
    }

    fn snapshot_with_history(&mut self) -> TerminalSnapshot {
        self.normalized_snapshot_with_history()
            .unwrap_or_else(|error| panic!("Ghostty history snapshot failed: {error}"))
    }

    fn scrollback_extent(&self) -> usize {
        self.terminal.scrollback_extent()
    }
}

fn normalize_ghostty_snapshot(snapshot: &GhosttySnapshot) -> TerminalSnapshot {
    let mut normalized = TerminalSnapshot {
        rows: Arc::clone(&snapshot.rows),
        scrollback: snapshot.scrollback.iter().cloned().collect(),
        ..TerminalSnapshot::default()
    };
    refresh_normalized_ghostty_metadata(&mut normalized, snapshot);
    normalized
}

fn refresh_normalized_ghostty_metadata(
    normalized: &mut TerminalSnapshot,
    snapshot: &GhosttySnapshot,
) {
    let (rows, cols) = snapshot.size();
    normalized.cursor = normalize_ghostty_cursor(snapshot.cursor);
    normalized.geometry =
        TerminalGeometry::from_grid_pixels(rows, cols, snapshot.width_px, snapshot.height_px);
    normalized.screen = ghostty_screen_identity(snapshot.alternate_screen);
    normalized.modes = TerminalModes {
        application_keypad: snapshot.modes.application_keypad,
        application_cursor: snapshot.modes.application_cursor,
        bracketed_paste: snapshot.modes.bracketed_paste,
        synchronized_output: snapshot.modes.synchronized_output,
        focus_reporting: snapshot.modes.focus_reporting,
        kitty_keyboard_flags: snapshot.modes.kitty_keyboard_flags,
        mouse_protocol: normalize_ghostty_mouse_protocol(snapshot.modes.mouse_protocol),
        mouse_encoding: normalize_ghostty_mouse_encoding(snapshot.modes.mouse_encoding),
    };
    normalized.title.clone_from(&snapshot.title);
    normalized
        .working_directory
        .clone_from(&snapshot.working_directory);
    normalized.semantic_marks.clear();
    normalized
        .semantic_marks
        .extend(snapshot.semantic_marks.iter().map(|mark| SemanticMark {
            kind: match mark.kind {
                GhosttySemanticKind::PromptStart => SemanticKind::PromptStart,
                GhosttySemanticKind::InputStart => SemanticKind::InputStart,
                GhosttySemanticKind::CommandStart => SemanticKind::CommandStart,
                GhosttySemanticKind::CommandFinished { exit_code } => {
                    SemanticKind::CommandFinished { exit_code }
                }
            },
            position: HistoryPosition {
                row: mark.row,
                col: mark.col,
            },
            alternate_screen: mark.alternate_screen,
        }));
    normalized.history_origin = snapshot.history_origin;
    normalized.scrollback_extent = snapshot.scrollback_extent;
    normalized.viewport = Viewport::Live;
}

fn normalize_ghostty_update(update: GhosttyUpdate) -> UpdateSummary {
    let damage = match update.damage {
        GhosttyDamage::None => TerminalDamage::None,
        GhosttyDamage::Rows(rows) => TerminalDamage::Rows(rows),
        GhosttyDamage::Full => TerminalDamage::Full,
    };
    let changed_rows = update.changed_rows;
    let effects = update
        .effects
        .into_iter()
        .map(normalize_ghostty_effect)
        .filter(|event| !matches!(event, TerminalEvent::PtyReply(_)))
        .collect::<Vec<_>>();
    UpdateSummary {
        effects: TerminalEffects {
            bells: effects
                .iter()
                .filter(|event| matches!(event, TerminalEvent::Bell))
                .count(),
            title_changed: effects
                .iter()
                .any(|event| matches!(event, TerminalEvent::TitleChanged(_))),
            events: effects,
        },
        pty_replies: update.pty_replies,
        damage,
        printed_runs: update
            .printed_runs
            .into_iter()
            .map(|run| PrintedRun {
                text: run.text,
                boundary: match run.boundary {
                    GhosttyPrintBoundary::Continue => PrintBoundary::Continue,
                    GhosttyPrintBoundary::LineFeed => PrintBoundary::LineFeed,
                    GhosttyPrintBoundary::CarriageReturn => PrintBoundary::CarriageReturn,
                },
            })
            .collect(),
        output_report_structural: update.output_report_structural,
        parser_continuation: update.parser_continuation,
        operations: update
            .operations
            .into_iter()
            .map(normalize_ghostty_operation)
            .collect(),
        cursor_operations: update.cursor_operations,
        scroll_operations: update.scroll_operations,
        history_changed: update.history_changed,
        changed_rows,
        cursor_before: normalize_ghostty_cursor(update.cursor_before),
        cursor_after: normalize_ghostty_cursor(update.cursor_after),
        screen_before: ghostty_screen_identity(update.alternate_screen_before),
        screen_after: ghostty_screen_identity(update.alternate_screen_after),
        synchronized_output: update.synchronized_output,
        synchronized_output_opened: false,
        synchronized_output_closed: update.synchronized_output_closed,
        semantic_input_boundary: update.semantic_input_boundary,
        cursor_visibility_restored: update.cursor_visibility_restored,
        batch_count: 1,
    }
}

fn normalize_ghostty_operation(operation: GhosttyOperation) -> TerminalOperation {
    match operation {
        GhosttyOperation::ScrollUp { top, bottom, count } => {
            TerminalOperation::ScrollUp { top, bottom, count }
        }
        GhosttyOperation::ScrollDown { top, bottom, count } => {
            TerminalOperation::ScrollDown { top, bottom, count }
        }
        GhosttyOperation::InsertLines { row, bottom, count } => {
            TerminalOperation::InsertLines { row, bottom, count }
        }
        GhosttyOperation::DeleteLines { row, bottom, count } => {
            TerminalOperation::DeleteLines { row, bottom, count }
        }
        GhosttyOperation::InsertChars { row, col, count } => {
            TerminalOperation::InsertChars { row, col, count }
        }
        GhosttyOperation::DeleteChars { row, col, count } => {
            TerminalOperation::DeleteChars { row, col, count }
        }
        GhosttyOperation::EraseChars { row, col, count } => {
            TerminalOperation::EraseChars { row, col, count }
        }
        GhosttyOperation::WriteRun { row, col, text } => {
            TerminalOperation::WriteRun { row, col, text }
        }
    }
}

fn normalize_ghostty_effect(effect: GhosttyEffect) -> TerminalEvent {
    match effect {
        GhosttyEffect::Bell => TerminalEvent::Bell,
        GhosttyEffect::TitleChanged(title) => TerminalEvent::TitleChanged(title),
        GhosttyEffect::WorkingDirectoryChanged(path) => {
            TerminalEvent::WorkingDirectoryChanged(path)
        }
        GhosttyEffect::ClipboardWrite { location, contents } => TerminalEvent::ClipboardWrite {
            location: match location {
                GhosttyClipboardLocation::Standard => ClipboardLocation::Standard,
                GhosttyClipboardLocation::Selection => ClipboardLocation::Selection,
                GhosttyClipboardLocation::Primary => ClipboardLocation::Primary,
            },
            contents: contents
                .into_iter()
                .map(|content: GhosttyClipboardContent| ClipboardContent {
                    mime: content.mime,
                    data: content.data,
                })
                .collect(),
        },
        GhosttyEffect::DesktopNotification { title, body } => {
            TerminalEvent::DesktopNotification { title, body }
        }
        GhosttyEffect::ProgressReport { state, progress } => TerminalEvent::ProgressReport {
            state: match state {
                GhosttyProgressState::Remove => ProgressState::Remove,
                GhosttyProgressState::Set => ProgressState::Set,
                GhosttyProgressState::Error => ProgressState::Error,
                GhosttyProgressState::Indeterminate => ProgressState::Indeterminate,
                GhosttyProgressState::Pause => ProgressState::Pause,
            },
            progress,
        },
        GhosttyEffect::Query(query) => TerminalEvent::Query(match query {
            GhosttyQuery::Enquiry => TerminalQuery::Enquiry,
            GhosttyQuery::XtVersion => TerminalQuery::XtVersion,
            GhosttyQuery::Size => TerminalQuery::Size,
            GhosttyQuery::ColorScheme => TerminalQuery::ColorScheme,
            GhosttyQuery::DeviceAttributes => TerminalQuery::DeviceAttributes,
            GhosttyQuery::Clipboard => TerminalQuery::Clipboard,
        }),
        GhosttyEffect::PtyReply(bytes) => TerminalEvent::PtyReply(bytes),
        GhosttyEffect::UnknownSequence { content, truncated } => {
            TerminalEvent::UnknownSequence { content, truncated }
        }
    }
}

fn normalize_ghostty_cursor(cursor: GhosttyCursor) -> Cursor {
    Cursor {
        row: cursor.row,
        col: cursor.col,
        visible: cursor.visible,
        shape: match cursor.shape {
            lector_ghostty::CursorShapeSnapshot::Bar => CursorShape::Bar,
            lector_ghostty::CursorShapeSnapshot::Block => CursorShape::Block,
            lector_ghostty::CursorShapeSnapshot::Underline => CursorShape::Underline,
            lector_ghostty::CursorShapeSnapshot::BlockHollow => CursorShape::BlockHollow,
        },
    }
}

fn ghostty_screen_identity(alternate: bool) -> ScreenIdentity {
    if alternate {
        ScreenIdentity::Alternate
    } else {
        ScreenIdentity::Primary
    }
}

fn normalize_ghostty_mouse_protocol(value: lector_ghostty::MouseProtocol) -> MouseProtocol {
    match value {
        lector_ghostty::MouseProtocol::None => MouseProtocol::None,
        lector_ghostty::MouseProtocol::Press => MouseProtocol::Press,
        lector_ghostty::MouseProtocol::PressRelease => MouseProtocol::PressRelease,
        lector_ghostty::MouseProtocol::ButtonMotion => MouseProtocol::ButtonMotion,
        lector_ghostty::MouseProtocol::AnyMotion => MouseProtocol::AnyMotion,
    }
}

fn normalize_ghostty_mouse_encoding(value: lector_ghostty::MouseEncoding) -> MouseEncoding {
    match value {
        lector_ghostty::MouseEncoding::Default => MouseEncoding::Default,
        lector_ghostty::MouseEncoding::Utf8 => MouseEncoding::Utf8,
        lector_ghostty::MouseEncoding::Sgr => MouseEncoding::Sgr,
    }
}

#[cfg(test)]
mod tests {
    use super::{GhosttyEngine, TerminalEngine, Viewport};

    #[test]
    fn viewport_selection_canonicalizes_zero_and_skips_unchanged_snapshots() {
        let mut engine =
            GhosttyEngine::new_with_scrollback(2, 8, 32).expect("create Ghostty engine");
        engine
            .advance(b"one\r\ntwo\r\nthree")
            .expect("seed retained history");
        assert_eq!(engine.scrollback_extent(), 1);

        let live = engine.snapshot().clone();
        engine.reset_snapshot_refresh_counts();
        engine.select_viewport(Viewport::Scrollback(0));

        assert_eq!(engine.viewport(), Viewport::Live);
        assert_eq!(engine.snapshot(), &live);
        assert_eq!(
            engine.snapshot_refresh_counts(),
            (0, 0),
            "zero scrollback must not refresh the live grid or copy history"
        );

        engine.select_viewport(Viewport::Scrollback(1));
        assert_eq!(engine.viewport(), Viewport::Scrollback(1));
        assert_eq!(engine.snapshot_refresh_counts(), (1, 1));

        engine.select_viewport(Viewport::Scrollback(1));
        assert_eq!(
            engine.snapshot_refresh_counts(),
            (1, 1),
            "reselecting the current historical viewport must be a no-op"
        );

        engine.select_viewport(Viewport::Live);
        assert_eq!(engine.snapshot_refresh_counts(), (2, 1));
        engine.select_viewport(Viewport::Live);
        assert_eq!(
            engine.snapshot_refresh_counts(),
            (2, 1),
            "reselecting the live viewport must be a no-op"
        );
    }
}
