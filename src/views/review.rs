use super::{Result, ViewAction, ViewController, ViewKind};
use crate::{
    line_editor::{EditorAction, LineEditor},
    review::{
        document::{ReviewDocument, SearchDirection},
        parser::{
            Command, FindDirection, Key, Motion, Parser, TextObject, ViewportPlacement, VisualKind,
        },
        table::{CellAddress, CellMove, MarkerChange, ReviewTable, TableSetup},
    },
    screen_reader::ScreenReader,
    terminal::HistoryPosition,
    terminal_input::KeyInput,
    view::View,
};
use std::{any::Any, io::Write};
use terminput::{KeyCode, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Copy, Clone, Debug)]
struct LastFind {
    direction: FindDirection,
    till: bool,
    target: char,
    matched: HistoryPosition,
}

#[derive(Clone, Debug)]
struct LastSearch {
    query: String,
    direction: SearchDirection,
}

#[derive(Clone, Debug)]
struct SearchPrompt {
    query: String,
    direction: SearchDirection,
}

struct NamePrompt {
    tabstop: u16,
    logical_column: usize,
    editor: LineEditor,
}

pub struct ReviewView {
    view: View,
    title: String,
    kind: ViewKind,
    document: ReviewDocument,
    parser: Parser,
    cursor: HistoryPosition,
    viewport_top: usize,
    viewport_left: u16,
    rows: u16,
    cols: u16,
    visual_anchor: Option<HistoryPosition>,
    visual_kind: VisualKind,
    last_find: Option<LastFind>,
    last_search: Option<LastSearch>,
    search_prompt: Option<SearchPrompt>,
    table: Option<ReviewTable>,
    table_setup: Option<TableSetup>,
    name_prompt: Option<NamePrompt>,
}

impl ReviewView {
    pub fn new(source: &mut View) -> Self {
        Self::new_with_identity(source, "Review", ViewKind::Review)
    }

    pub fn new_page_up(source: &mut View) -> Self {
        let mut review = Self::new(source);
        let _ = review.scroll_page(false, 1);
        review
    }

    pub(crate) fn new_table_setup(source: &mut View, title: impl Into<String>) -> Self {
        Self::new_with_identity(source, title, ViewKind::TableSetup)
    }

    fn new_with_identity(source: &mut View, title: impl Into<String>, kind: ViewKind) -> Self {
        // Review opens at the source's independent review cursor, not at the
        // source application's cursor. render() exposes this as the overlay's
        // application cursor so normal cursor tracking starts from that point.
        let (document, cursor, viewport_top) = ReviewDocument::capture(source);
        let (rows, cols) = source.size();
        let mut review = Self {
            view: View::new(rows, cols),
            title: title.into(),
            kind,
            document,
            parser: Parser::default(),
            cursor,
            viewport_top,
            viewport_left: 0,
            rows,
            cols,
            visual_anchor: None,
            visual_kind: VisualKind::Character,
            last_find: None,
            last_search: None,
            search_prompt: None,
            table: None,
            table_setup: None,
            name_prompt: None,
        };
        review.ensure_cursor_visible();
        review.render();
        review
    }

    fn document_height(&self) -> usize {
        usize::from(self.rows).saturating_sub(usize::from(
            self.search_prompt.is_some() || self.name_prompt.is_some(),
        ))
    }

    fn ensure_cursor_visible(&mut self) {
        let height = self.document_height().max(1);
        if self.cursor.row < self.viewport_top {
            self.viewport_top = self.cursor.row;
        } else if self.cursor.row >= self.viewport_top.saturating_add(height) {
            self.viewport_top = self.cursor.row.saturating_add(1).saturating_sub(height);
        }
        self.viewport_top = self
            .viewport_top
            .min(self.document.row_count().saturating_sub(1));

        let width = self.cols.max(1);
        if self.cursor.col < self.viewport_left {
            self.viewport_left = self.cursor.col;
        } else if self.cursor.col >= self.viewport_left.saturating_add(width) {
            self.viewport_left = self.cursor.col.saturating_add(1).saturating_sub(width);
        }
        self.viewport_left = self
            .viewport_left
            .min(self.document.max_viewport_left(width));
    }

    fn render(&mut self) {
        self.ensure_cursor_visible();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x1B[2J\x1B[H");
        let height = self.document_height();
        for screen_row in 0..height {
            let absolute_row = self.viewport_top.saturating_add(screen_row);
            if absolute_row >= self.document.row_count() {
                break;
            }
            bytes.extend_from_slice(format!("\x1B[{};1H", screen_row.saturating_add(1)).as_bytes());
            bytes.extend_from_slice(&self.document.formatted_row(
                absolute_row,
                self.viewport_left,
                self.cols,
            ));
            bytes.extend_from_slice(b"\x1B[0m");
        }

        if let Some(prompt) = &self.search_prompt {
            let marker = match prompt.direction {
                SearchDirection::Forward => '/',
                SearchDirection::Backward => '?',
            };
            let row = self.rows.max(1);
            bytes.extend_from_slice(format!("\x1B[{row};1H\x1B[2K{marker}").as_bytes());
            bytes.extend_from_slice(prompt.query.as_bytes());
            let col = 2usize
                .saturating_add(prompt.query.graphemes(true).count())
                .min(usize::from(self.cols).max(1));
            bytes.extend_from_slice(format!("\x1B[{row};{col}H\x1B[?25h").as_bytes());
        } else if let Some(prompt) = &self.name_prompt {
            let row = self.rows.max(1);
            let prefix = format!("Column {} name: ", prompt.logical_column + 1);
            bytes.extend_from_slice(format!("\x1B[{row};1H\x1B[2K{prefix}").as_bytes());
            bytes.extend_from_slice(prompt.editor.input().as_bytes());
            let before_cursor = prompt
                .editor
                .input()
                .graphemes(true)
                .take(prompt.editor.cursor())
                .collect::<String>();
            let col = 1usize
                .saturating_add(prefix.width())
                .saturating_add(before_cursor.width())
                .min(usize::from(self.cols).max(1));
            bytes.extend_from_slice(format!("\x1B[{row};{col}H\x1B[?25h").as_bytes());
        } else {
            let row = self
                .cursor
                .row
                .saturating_sub(self.viewport_top)
                .saturating_add(1)
                .min(usize::from(self.rows).max(1));
            let col = usize::from(self.cursor.col.saturating_sub(self.viewport_left))
                .saturating_add(1)
                .min(usize::from(self.cols).max(1));
            bytes.extend_from_slice(format!("\x1B[{row};{col}H\x1B[?25h").as_bytes());
        }

        // Keep one View alive for the lifetime of the overlay. Its application
        // cursor is the visible terminal cursor, and preserving prev_screen lets
        // the shared cursor tracker report review motions just like PTY motions.
        self.view.clear_update_summary();
        self.view.process_changes(&bytes);
        self.view.clear_update_summary();
    }

    fn handle_search_key(&mut self, sr: &mut ScreenReader, key: Key) -> Result<ViewAction> {
        match key {
            Key::Escape => {
                self.search_prompt = None;
                self.render();
                Ok(ViewAction::Redraw)
            }
            Key::Backspace => {
                let prompt = self.search_prompt.as_mut().expect("search prompt");
                let Some((index, _)) = prompt.query.grapheme_indices(true).next_back() else {
                    return Ok(ViewAction::Bell);
                };
                prompt.query.truncate(index);
                self.render();
                Ok(ViewAction::Redraw)
            }
            Key::Ctrl('u') => {
                let prompt = self.search_prompt.as_mut().expect("search prompt");
                if prompt.query.is_empty() {
                    return Ok(ViewAction::Bell);
                }
                prompt.query.clear();
                self.render();
                Ok(ViewAction::Redraw)
            }
            Key::Enter => self.finish_search(sr),
            Key::Char(ch) if !ch.is_control() => {
                self.search_prompt
                    .as_mut()
                    .expect("search prompt")
                    .query
                    .push(ch);
                self.render();
                Ok(ViewAction::Redraw)
            }
            _ => Ok(ViewAction::Bell),
        }
    }

    fn finish_search(&mut self, sr: &mut ScreenReader) -> Result<ViewAction> {
        let prompt = self.search_prompt.as_ref().expect("search prompt");
        let query = if prompt.query.is_empty() {
            let Some(last) = &self.last_search else {
                return Ok(ViewAction::Bell);
            };
            last.query.clone()
        } else {
            prompt.query.clone()
        };
        let direction = prompt.direction;
        let Some(target) = self.document.search(&query, self.cursor, direction, 1) else {
            return Ok(ViewAction::Bell);
        };
        self.last_search = Some(LastSearch { query, direction });
        self.search_prompt = None;
        self.move_with_table_context(sr, target)
    }

    fn handle_command(&mut self, sr: &mut ScreenReader, command: Command) -> Result<ViewAction> {
        match command {
            Command::None => Ok(ViewAction::None),
            Command::Bell => Ok(ViewAction::Bell),
            Command::Exit => Ok(ViewAction::Pop),
            Command::StartVisual(kind) => {
                self.visual_anchor = Some(self.cursor);
                self.visual_kind = kind;
                sr.speak(
                    match kind {
                        VisualKind::Character => "visual",
                        VisualKind::Line => "visual line",
                    },
                    false,
                )?;
                Ok(ViewAction::None)
            }
            Command::CancelVisual => {
                self.visual_anchor = None;
                sr.speak("visual cancelled", false)?;
                Ok(ViewAction::None)
            }
            Command::Move(motion, count) | Command::MoveVisual(motion, count) => {
                let Some(target) = self.motion_target(motion, count) else {
                    return Ok(ViewAction::Bell);
                };
                self.move_with_table_context(sr, target)
            }
            Command::ScrollPage { forward, count } => {
                let old = self.current_table_cell();
                let action = self.scroll_page(forward, count);
                self.announce_table_transition(sr, old)?;
                Ok(action)
            }
            Command::RepositionViewport {
                placement,
                line,
                first_nonblank,
            } => {
                let old = self.current_table_cell();
                let action = self.reposition_viewport(placement, line, first_nonblank);
                self.announce_table_transition(sr, old)?;
                Ok(action)
            }
            Command::YankMotion(motion, count, register) => {
                let Some(target) = self.motion_target(motion, count) else {
                    return Ok(ViewAction::Bell);
                };
                let Some(text) = self.yank_motion_text(motion, target) else {
                    return Ok(ViewAction::Bell);
                };
                self.yank(sr, register, text)
            }
            Command::YankLine(count, register) => {
                let last_row = self
                    .cursor
                    .row
                    .saturating_add(count.saturating_sub(1))
                    .min(self.document.row_count().saturating_sub(1));
                let Some(text) = self.document.yank_range(
                    self.cursor,
                    HistoryPosition {
                        row: last_row,
                        col: self.document.line_last_col(last_row),
                    },
                    true,
                ) else {
                    return Ok(ViewAction::Bell);
                };
                self.yank(sr, register, text)
            }
            Command::YankTextObject(TextObject::Word { style, around }, count, register) => {
                let Some((first, last)) =
                    self.document
                        .inner_word_range(self.cursor, style, around, count)
                else {
                    return Ok(ViewAction::Bell);
                };
                let Some(text) = self.document.yank_range(first, last, false) else {
                    return Ok(ViewAction::Bell);
                };
                self.yank(sr, register, text)
            }
            Command::YankVisual(register) => {
                let Some(anchor) = self.visual_anchor.take() else {
                    return Ok(ViewAction::Bell);
                };
                let Some(text) = self.document.yank_range(
                    anchor,
                    self.cursor,
                    self.visual_kind == VisualKind::Line,
                ) else {
                    return Ok(ViewAction::Bell);
                };
                self.yank(sr, register, text)
            }
            Command::StartSearch(direction) => {
                self.search_prompt = Some(SearchPrompt {
                    query: String::new(),
                    direction,
                });
                self.render();
                Ok(ViewAction::Redraw)
            }
            Command::RepeatSearch { reverse, count } => {
                let Some(last) = self.last_search.clone() else {
                    return Ok(ViewAction::Bell);
                };
                let direction = if reverse {
                    last.direction.reverse()
                } else {
                    last.direction
                };
                let Some(target) = self
                    .document
                    .search(&last.query, self.cursor, direction, count)
                else {
                    return Ok(ViewAction::Bell);
                };
                self.move_with_table_context(sr, target)
            }
            Command::DetectTable => self.detect_table(sr),
            Command::StartTableSetup => self.start_table_setup(sr),
            Command::MarkTableBottom => self.mark_table_bottom(sr),
            Command::MarkTableRight => self.mark_table_right(sr),
            Command::ToggleTableRowHeader => self.toggle_table_row_header(sr),
            Command::MoveTableCell(movement, count) => self.move_table_cell(sr, movement, count),
        }
    }

    fn detect_table(&mut self, sr: &mut ScreenReader) -> Result<ViewAction> {
        self.table = None;
        self.table_setup = None;
        self.name_prompt = None;
        let Some(table) = ReviewTable::detect(&self.document, self.cursor) else {
            sr.speak("no table found", false)?;
            return Ok(ViewAction::None);
        };
        let Some(address) = table
            .cell_at(self.cursor)
            .or_else(|| table.nearest_cell(self.cursor))
        else {
            sr.speak("no table found", false)?;
            return Ok(ViewAction::None);
        };
        let target = table.position_for_cell(&self.document, address);
        let moved = target != self.cursor;
        self.table = Some(table);
        if moved {
            self.cursor = target;
            self.render();
        }
        let table = self.table.as_ref().expect("installed table");
        let (rows, columns) = table.dimensions();
        sr.speak("table", false)?;
        sr.speak(&format!("{rows} rows"), false)?;
        sr.speak(&format!("{columns} columns"), false)?;
        Self::speak_full_cell(sr, table, address, true)?;
        Ok(if moved {
            ViewAction::RedrawSilently
        } else {
            ViewAction::None
        })
    }

    fn start_table_setup(&mut self, sr: &mut ScreenReader) -> Result<ViewAction> {
        self.parser = Parser::default();
        self.table = None;
        self.name_prompt = None;
        self.table_setup = Some(TableSetup::new(self.cursor.row));
        sr.speak("table setup", false)?;
        sr.speak("headers from first row", false)?;
        Ok(ViewAction::None)
    }

    fn mark_table_bottom(&mut self, sr: &mut ScreenReader) -> Result<ViewAction> {
        let Some(setup) = self.table_setup.as_mut() else {
            return Ok(ViewAction::Bell);
        };
        if self.cursor.row < setup.top_row() {
            sr.speak("bottom row cannot be above the first row", false)?;
            return Ok(ViewAction::None);
        }
        let row_number = self
            .cursor
            .row
            .saturating_sub(setup.top_row())
            .saturating_add(1)
            .saturating_sub(usize::from(
                setup.header_mode() == crate::review::table::HeaderMode::FirstRow,
            ));
        match setup.toggle_bottom(self.cursor.row) {
            MarkerChange::Set => {
                sr.speak("bottom row set", false)?;
                if row_number > 0 {
                    sr.speak(&format!("row {row_number}"), false)?;
                }
            }
            MarkerChange::Cleared => {
                sr.speak("bottom row automatic", false)?;
            }
        }
        Ok(ViewAction::None)
    }

    fn mark_table_right(&mut self, sr: &mut ScreenReader) -> Result<ViewAction> {
        let Some(setup) = self.table_setup.as_mut() else {
            return Ok(ViewAction::Bell);
        };
        if setup
            .tabstops()
            .last()
            .is_some_and(|last| self.cursor.col < *last)
        {
            sr.speak("right edge cannot be before the last tabstop", false)?;
            return Ok(ViewAction::None);
        }
        match setup.toggle_right_edge(self.cursor.col) {
            MarkerChange::Set => {
                sr.speak("right edge set", false)?;
                sr.speak(
                    &format!("display column {}", self.cursor.col.saturating_add(1)),
                    false,
                )?;
            }
            MarkerChange::Cleared => {
                sr.speak("right edge cleared", false)?;
            }
        }
        Ok(ViewAction::None)
    }

    fn toggle_table_row_header(&mut self, sr: &mut ScreenReader) -> Result<ViewAction> {
        let Some(table) = self.table.as_mut() else {
            sr.speak("no active table", false)?;
            return Ok(ViewAction::None);
        };
        let Some(address) = table.cell_at(self.cursor) else {
            sr.speak("outside table", false)?;
            return Ok(ViewAction::None);
        };
        let label = table.label(address).to_owned();
        if table.toggle_row_header(address.column) {
            sr.speak("row headers from", false)?;
            sr.speak(&label, false)?;
        } else {
            sr.speak("row headers off", false)?;
        }
        Ok(ViewAction::None)
    }

    fn move_table_cell(
        &mut self,
        sr: &mut ScreenReader,
        movement: CellMove,
        count: usize,
    ) -> Result<ViewAction> {
        let Some(table) = self.table.as_ref() else {
            sr.speak("no active table", false)?;
            return Ok(ViewAction::None);
        };
        let (target, include_row) = if let Some(current) = table.cell_at(self.cursor) {
            let Some(target) = table.move_cell(current, movement, count) else {
                let (edge, row) = table.boundary_announcement(movement, current);
                sr.speak(edge, false)?;
                if let Some(row) = row {
                    sr.speak(&format!("row {row}"), false)?;
                }
                return Ok(ViewAction::None);
            };
            (target, current.row != target.row)
        } else {
            let Some(entry) = table.reentry_cell(self.cursor, movement) else {
                sr.speak(table.edge_announcement(movement), false)?;
                return Ok(ViewAction::None);
            };
            let target = count
                .checked_sub(1)
                .filter(|remaining| *remaining > 0)
                .and_then(|remaining| table.move_cell(entry, movement, remaining))
                .unwrap_or(entry);
            (target, true)
        };
        self.cursor = table.position_for_cell(&self.document, target);
        self.render();
        let table = self.table.as_ref().expect("active table");
        Self::speak_full_cell(sr, table, target, include_row)?;
        Ok(ViewAction::RedrawSilently)
    }

    fn speak_full_cell(
        sr: &mut ScreenReader,
        table: &ReviewTable,
        address: CellAddress,
        include_row: bool,
    ) -> Result<()> {
        if include_row {
            Self::speak_row_identity(sr, table, address)?;
        }
        sr.speak(table.label(address), false)?;
        let text = table.text(address).trim();
        sr.speak(if text.is_empty() { "blank" } else { text }, false)?;
        Ok(())
    }

    fn speak_row_identity(
        sr: &mut ScreenReader,
        table: &ReviewTable,
        address: CellAddress,
    ) -> Result<()> {
        if table.is_header(address) {
            sr.speak("header row", false)?;
            return Ok(());
        }
        if let Some(row) = table.row_number(address) {
            sr.speak(&format!("row {row}"), false)?;
        }
        if table
            .row_header_column()
            .is_some_and(|column| column != address.column)
            && let Some(text) = table.row_header_text(address)
        {
            let text = text.trim();
            sr.speak(if text.is_empty() { "blank" } else { text }, false)?;
        }
        Ok(())
    }

    fn move_with_table_context(
        &mut self,
        sr: &mut ScreenReader,
        target: HistoryPosition,
    ) -> Result<ViewAction> {
        let old = self.current_table_cell();
        let action = self.move_to(target);
        self.announce_table_transition(sr, old)?;
        Ok(action)
    }

    fn current_table_cell(&self) -> Option<CellAddress> {
        self.table
            .as_ref()
            .and_then(|table| table.cell_at(self.cursor))
    }

    fn announce_table_transition(
        &self,
        sr: &mut ScreenReader,
        old: Option<CellAddress>,
    ) -> Result<()> {
        if self.table_setup.is_some() {
            return Ok(());
        }
        let Some(table) = self.table.as_ref() else {
            return Ok(());
        };
        let new = table.cell_at(self.cursor);
        if old == new {
            return Ok(());
        }
        match (old, new) {
            (None, Some(address)) => {
                sr.speak("table", false)?;
                Self::speak_row_identity(sr, table, address)?;
                sr.speak(table.label(address), false)?;
            }
            (Some(_), None) => {
                sr.speak(
                    if table.is_structural_row(self.cursor.row) {
                        "table separator"
                    } else {
                        "out of table"
                    },
                    false,
                )?;
            }
            (Some(previous), Some(address)) => {
                if previous.row != address.row {
                    Self::speak_row_identity(sr, table, address)?;
                }
                if previous.column != address.column {
                    sr.speak(table.label(address), false)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn motion_target(&mut self, motion: Motion, count: usize) -> Option<HistoryPosition> {
        let mut position = self.cursor;
        match motion {
            Motion::Left => self.document.move_horizontal(position, false, count),
            Motion::Right => self.document.move_horizontal(position, true, count),
            Motion::Up => self.document.move_vertical(position, false, count),
            Motion::Down => self.document.move_vertical(position, true, count),
            Motion::LineStart => {
                position.col = 0;
                (position != self.cursor).then_some(position)
            }
            Motion::FirstNonblank => {
                position.col = self.document.line_first_nonblank(position.row);
                (position != self.cursor).then_some(position)
            }
            Motion::LineEnd => {
                position.col = self.document.line_last_col(position.row);
                (position != self.cursor).then_some(position)
            }
            Motion::Word(movement, style) => {
                self.document.move_word(position, movement, style, count)
            }
            Motion::DocumentStart => {
                position.row = count
                    .saturating_sub(1)
                    .min(self.document.row_count().saturating_sub(1));
                position.col = self.document.line_first_nonblank(position.row);
                (position != self.cursor).then_some(position)
            }
            Motion::DocumentEnd => {
                position.row = if count == 1 {
                    self.document.row_count().saturating_sub(1)
                } else {
                    count
                        .saturating_sub(1)
                        .min(self.document.row_count().saturating_sub(1))
                };
                position.col = self.document.line_first_nonblank(position.row);
                (position != self.cursor).then_some(position)
            }
            Motion::MatchingBrace => {
                for _ in 0..count {
                    position = self.document.matching_brace(position)?;
                }
                (position != self.cursor).then_some(position)
            }
            Motion::Find {
                direction,
                till,
                target,
            } => {
                let forward = direction == FindDirection::Forward;
                let matched = self
                    .document
                    .find_character(position, target, forward, false, count)?;
                let destination = if till {
                    self.document.adjacent(matched, !forward)?
                } else {
                    matched
                };
                if destination == self.cursor {
                    return None;
                }
                self.last_find = Some(LastFind {
                    direction,
                    till,
                    target,
                    matched,
                });
                Some(destination)
            }
            Motion::RepeatFind { reverse } => {
                let mut find = self.last_find?;
                let forward = (find.direction == FindDirection::Forward) != reverse;
                let start = if reverse { position } else { find.matched };
                let matched =
                    self.document
                        .find_character(start, find.target, forward, false, count)?;
                let destination = if find.till {
                    self.document.adjacent(matched, !forward)?
                } else {
                    matched
                };
                if destination == self.cursor {
                    return None;
                }
                find.matched = matched;
                self.last_find = Some(find);
                Some(destination)
            }
            Motion::Prompt { forward } => self.document.prompt(position, forward, count),
        }
    }

    fn scroll_page(&mut self, forward: bool, count: usize) -> ViewAction {
        let distance = self.document_height().max(1).saturating_mul(count);
        let max_top = self.document.max_viewport_top(self.document_height());
        let target_top = if forward {
            self.viewport_top.saturating_add(distance).min(max_top)
        } else {
            self.viewport_top.saturating_sub(distance)
        };
        let target_cursor = self.document.move_vertical(self.cursor, forward, distance);
        if target_top == self.viewport_top && target_cursor.is_none() {
            return ViewAction::Bell;
        }
        self.viewport_top = target_top;
        if let Some(target) = target_cursor {
            self.cursor = self.document.clamp(target);
        }
        self.render();
        ViewAction::Redraw
    }

    fn reposition_viewport(
        &mut self,
        placement: ViewportPlacement,
        line: Option<usize>,
        first_nonblank: bool,
    ) -> ViewAction {
        if let Some(line) = line {
            self.cursor.row = line
                .saturating_sub(1)
                .min(self.document.row_count().saturating_sub(1));
            self.cursor = self.document.clamp(self.cursor);
        }
        if first_nonblank {
            self.cursor.col = self.document.line_first_nonblank(self.cursor.row);
        }

        let height = self.document_height().max(1);
        self.viewport_top = match placement {
            ViewportPlacement::Top => self.cursor.row,
            ViewportPlacement::Center => self.cursor.row.saturating_sub(height / 2),
            ViewportPlacement::Bottom => self.cursor.row.saturating_sub(height.saturating_sub(1)),
        };
        self.render();
        ViewAction::Redraw
    }

    fn yank_motion_text(&self, motion: Motion, target: HistoryPosition) -> Option<String> {
        if matches!(
            motion,
            Motion::Up | Motion::Down | Motion::DocumentStart | Motion::DocumentEnd
        ) {
            return self.document.yank_range(self.cursor, target, true);
        }

        let exclusive = matches!(
            motion,
            Motion::Left
                | Motion::Right
                | Motion::LineStart
                | Motion::FirstNonblank
                | Motion::Word(crate::review::document::WordMove::ForwardStart, _)
                | Motion::Word(crate::review::document::WordMove::BackwardStart, _)
                | Motion::Prompt { .. }
        );
        if !exclusive {
            return self.document.yank_range(self.cursor, target, false);
        }

        let (first, last) = if target > self.cursor {
            (self.cursor, self.document.previous_position(target)?)
        } else {
            (target, self.document.previous_position(self.cursor)?)
        };
        self.document.yank_range(first, last, false)
    }

    fn move_to(&mut self, target: HistoryPosition) -> ViewAction {
        self.cursor = self.document.clamp(target);
        self.render();
        ViewAction::Redraw
    }

    fn yank(
        &mut self,
        sr: &mut ScreenReader,
        register: Option<crate::clipboard::ClipboardRegister>,
        text: String,
    ) -> Result<ViewAction> {
        let register = register.unwrap_or_else(|| sr.clipboard_default_register());
        if let Err(error) = sr.write_clipboard(register, text) {
            sr.speak(&error.to_string(), false)?;
            return Ok(ViewAction::Bell);
        }
        sr.speak("copied", false)?;
        self.visual_anchor = None;
        Ok(ViewAction::None)
    }

    fn handle_setup_key(&mut self, sr: &mut ScreenReader, key: Key) -> Result<ViewAction> {
        match key {
            Key::Escape => {
                self.parser = Parser::default();
                self.table_setup = None;
                sr.speak("table setup cancelled", false)?;
                Ok(ViewAction::None)
            }
            Key::Enter => self.commit_table_setup(sr),
            Key::Char(' ') => self.toggle_setup_tabstop(sr),
            Key::Char('H') => {
                let mode = self
                    .table_setup
                    .as_mut()
                    .expect("active table setup")
                    .toggle_header_mode();
                sr.speak(mode.announcement(), false)?;
                Ok(ViewAction::None)
            }
            Key::Char('c') => self.start_name_prompt(sr),
            _ => {
                let command = self.parser.feed(key);
                self.handle_command(sr, command)
            }
        }
    }

    fn toggle_setup_tabstop(&mut self, sr: &mut ScreenReader) -> Result<ViewAction> {
        let setup = self.table_setup.as_mut().expect("active table setup");
        if setup
            .right_edge()
            .is_some_and(|right| self.cursor.col > right)
        {
            sr.speak("tabstop cannot be after the right edge", false)?;
            return Ok(ViewAction::None);
        }
        match setup.toggle_tabstop(self.cursor.col) {
            MarkerChange::Set => sr.speak("tabstop set", false)?,
            MarkerChange::Cleared => sr.speak("tabstop cleared", false)?,
        };
        sr.speak(
            &format!("display column {}", self.cursor.col.saturating_add(1)),
            false,
        )?;
        Ok(ViewAction::None)
    }

    fn start_name_prompt(&mut self, sr: &mut ScreenReader) -> Result<ViewAction> {
        let setup = self.table_setup.as_ref().expect("active table setup");
        let Some((logical_column, tabstop)) = setup.tabstop_at_or_before(self.cursor.col) else {
            sr.speak("no tabstop at or before cursor", false)?;
            return Ok(ViewAction::None);
        };
        let mut editor = LineEditor::new();
        if let Some(name) = setup.name(tabstop) {
            editor.handle_text(name);
        }
        self.name_prompt = Some(NamePrompt {
            tabstop,
            logical_column,
            editor,
        });
        self.render();
        Ok(ViewAction::Redraw)
    }

    fn commit_table_setup(&mut self, sr: &mut ScreenReader) -> Result<ViewAction> {
        let setup = self.table_setup.as_ref().expect("active table setup");
        let table = match ReviewTable::from_setup(&self.document, setup) {
            Ok(table) => table,
            Err(message) => {
                sr.speak(message, false)?;
                return Ok(ViewAction::None);
            }
        };
        let address = table
            .cell_at(self.cursor)
            .or_else(|| table.nearest_cell(self.cursor))
            .expect("valid table has a cell");
        self.cursor = table.position_for_cell(&self.document, address);
        self.table = Some(table);
        self.table_setup = None;
        self.parser = Parser::default();
        self.render();
        sr.speak("table setup saved", false)?;
        let table = self.table.as_ref().expect("installed table");
        Self::speak_full_cell(sr, table, address, true)?;
        Ok(ViewAction::RedrawSilently)
    }

    fn cancel_name_prompt(&mut self, sr: &mut ScreenReader) -> Result<ViewAction> {
        self.name_prompt = None;
        self.render();
        sr.speak("name edit cancelled", false)?;
        Ok(ViewAction::Redraw)
    }

    fn clear_name_prompt(&mut self) -> ViewAction {
        let prompt = self.name_prompt.as_mut().expect("active name prompt");
        if prompt.editor.input().is_empty() {
            return ViewAction::Bell;
        }
        prompt.editor.clear();
        self.render();
        ViewAction::Redraw
    }

    fn finish_name_prompt(&mut self, sr: &mut ScreenReader) -> Result<ViewAction> {
        let prompt = self.name_prompt.take().expect("active name prompt");
        let value = prompt.editor.input().to_owned();
        let logical_column = prompt.logical_column;
        self.table_setup
            .as_mut()
            .expect("name prompt belongs to setup")
            .set_name(prompt.tabstop, value.clone());
        self.render();
        let value = value.trim();
        if value.is_empty() {
            sr.speak(
                &format!("column {} uses column number", logical_column + 1),
                false,
            )?;
        } else {
            sr.speak("column name saved", false)?;
            sr.speak(value, false)?;
        }
        Ok(ViewAction::Redraw)
    }

    fn handle_name_editor_action(
        &mut self,
        sr: &mut ScreenReader,
        action: EditorAction,
    ) -> Result<ViewAction> {
        match action {
            EditorAction::None => Ok(ViewAction::None),
            EditorAction::Changed => {
                self.render();
                Ok(ViewAction::Redraw)
            }
            EditorAction::Submit => self.finish_name_prompt(sr),
            EditorAction::Bell => Ok(ViewAction::Bell),
        }
    }

    fn handle_name_key(&mut self, sr: &mut ScreenReader, key: Key) -> Result<ViewAction> {
        match key {
            Key::Escape => self.cancel_name_prompt(sr),
            Key::Ctrl('u') => Ok(self.clear_name_prompt()),
            Key::Enter => self.finish_name_prompt(sr),
            Key::Backspace => {
                let action = self
                    .name_prompt
                    .as_mut()
                    .expect("active name prompt")
                    .editor
                    .handle_bytes(b"\x7f");
                self.handle_name_editor_action(sr, action)
            }
            Key::Left => {
                let action = self
                    .name_prompt
                    .as_mut()
                    .expect("active name prompt")
                    .editor
                    .handle_bytes(b"\x1b[D");
                self.handle_name_editor_action(sr, action)
            }
            Key::Right => {
                let action = self
                    .name_prompt
                    .as_mut()
                    .expect("active name prompt")
                    .editor
                    .handle_bytes(b"\x1b[C");
                self.handle_name_editor_action(sr, action)
            }
            Key::Char(ch) if !ch.is_control() => {
                let action = self
                    .name_prompt
                    .as_mut()
                    .expect("active name prompt")
                    .editor
                    .handle_text(&ch.to_string());
                self.handle_name_editor_action(sr, action)
            }
            _ => Ok(ViewAction::Bell),
        }
    }

    fn handle_review_key(&mut self, sr: &mut ScreenReader, key: Key) -> Result<ViewAction> {
        if self.name_prompt.is_some() {
            self.handle_name_key(sr, key)
        } else if self.search_prompt.is_some() {
            self.handle_search_key(sr, key)
        } else if self.table_setup.is_some() {
            self.handle_setup_key(sr, key)
        } else {
            let command = self.parser.feed(key);
            self.handle_command(sr, command)
        }
    }

    fn handle_keys(
        &mut self,
        sr: &mut ScreenReader,
        keys: impl IntoIterator<Item = Key>,
    ) -> Result<ViewAction> {
        let mut result = ViewAction::None;
        for key in keys {
            let action = self.handle_review_key(sr, key)?;
            match action {
                ViewAction::Pop | ViewAction::Bell => return Ok(action),
                ViewAction::Redraw => result = ViewAction::Redraw,
                ViewAction::RedrawSilently => result = ViewAction::RedrawSilently,
                ViewAction::None => {}
                ViewAction::PtyInput
                | ViewAction::Push(_)
                | ViewAction::PopupResponse(_)
                | ViewAction::ActivateTmuxConnection(_)
                | ViewAction::TmuxConnectionControl { .. }
                | ViewAction::TmuxConnectionRename { .. }
                | ViewAction::TmuxChooserSelect { .. }
                | ViewAction::TmuxCommandSubmit { .. }
                | ViewAction::TmuxInput { .. } => {
                    unreachable!()
                }
            }
        }
        Ok(result)
    }
}

impl ViewController for ReviewView {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn model(&mut self) -> &mut View {
        &mut self.view
    }

    fn title(&self) -> &str {
        &self.title
    }

    fn kind(&self) -> ViewKind {
        self.kind
    }

    fn place_application_cursor_at_review_cursor(&mut self) -> Option<ViewAction> {
        let (row, col) = self.view.review_cursor_position();
        let target = HistoryPosition {
            row: self.viewport_top.saturating_add(usize::from(row)),
            col: self.viewport_left.saturating_add(col),
        };
        Some(self.move_to(target))
    }

    fn handle_input(
        &mut self,
        sr: &mut ScreenReader,
        input: &[u8],
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        if self.name_prompt.is_some() {
            if input == b"\x1b" {
                return self.cancel_name_prompt(sr);
            }
            if input == b"\x15" {
                return Ok(self.clear_name_prompt());
            }
            let action = self
                .name_prompt
                .as_mut()
                .expect("active name prompt")
                .editor
                .handle_bytes(input);
            return self.handle_name_editor_action(sr, action);
        }
        let keys = input.iter().copied().map(raw_key);
        self.handle_keys(sr, keys)
    }

    fn handle_key_input(
        &mut self,
        sr: &mut ScreenReader,
        key: &KeyInput,
        _raw: &[u8],
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        if key.is_release() {
            return Ok(ViewAction::None);
        }
        if self.name_prompt.is_some() {
            let semantic = semantic_key(key);
            if semantic == Key::Escape {
                return self.cancel_name_prompt(sr);
            }
            if semantic == Key::Ctrl('u') {
                return Ok(self.clear_name_prompt());
            }
            let action = self
                .name_prompt
                .as_mut()
                .expect("active name prompt")
                .editor
                .handle_key_input(key);
            return self.handle_name_editor_action(sr, action);
        }
        let semantic = semantic_key(key);
        if semantic != Key::Unknown {
            return self.handle_review_key(sr, semantic);
        }
        if let Some(text) = key.text() {
            return self.handle_keys(
                sr,
                text.chars().filter(|ch| !ch.is_control()).map(Key::Char),
            );
        }
        self.handle_review_key(sr, Key::Unknown)
    }

    fn handle_paste(
        &mut self,
        sr: &mut ScreenReader,
        contents: &str,
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        if let Some(prompt) = self.name_prompt.as_mut() {
            let action = prompt.editor.handle_text(contents);
            return self.handle_name_editor_action(sr, action);
        }
        if self.search_prompt.is_none() {
            return Ok(ViewAction::Bell);
        }
        let chars = contents
            .chars()
            .filter(|ch| !ch.is_control())
            .map(Key::Char);
        self.handle_keys(sr, chars)
    }

    fn on_resize(&mut self, rows: u16, cols: u16) {
        let cursor_screen_row = self.cursor.row.saturating_sub(self.viewport_top);
        let cursor_screen_col = self.cursor.col.saturating_sub(self.viewport_left);
        self.rows = rows;
        self.cols = cols;
        self.view.set_size(rows, cols);

        let height = self.document_height().max(1);
        let desired_screen_row = cursor_screen_row.min(height.saturating_sub(1));
        self.viewport_top = self
            .cursor
            .row
            .saturating_sub(desired_screen_row)
            .min(self.document.max_viewport_top(height));

        let width = self.cols.max(1);
        let desired_screen_col = cursor_screen_col.min(width.saturating_sub(1));
        self.viewport_left = self
            .cursor
            .col
            .saturating_sub(desired_screen_col)
            .min(self.document.max_viewport_left(width));
        self.ensure_cursor_visible();
        self.render();
    }
}

fn semantic_key(key: &KeyInput) -> Key {
    let event = key.normalized_event();
    if let Some(code) = key.control_code() {
        return match code {
            0x1B => Key::Escape,
            0x02 => Key::Ctrl('b'),
            0x04 => Key::Ctrl('d'),
            0x06 => Key::Ctrl('f'),
            0x15 => Key::Ctrl('u'),
            _ => Key::Unknown,
        };
    }
    if event.modifiers.intersects(
        KeyModifiers::CTRL
            | KeyModifiers::ALT
            | KeyModifiers::META
            | KeyModifiers::SUPER
            | KeyModifiers::HYPER,
    ) {
        return Key::Unknown;
    }
    match event.code {
        KeyCode::Char(ch) => Key::Char(ch),
        KeyCode::Esc => Key::Escape,
        KeyCode::Enter => Key::Enter,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Left => Key::Left,
        KeyCode::Down => Key::Down,
        KeyCode::Up => Key::Up,
        KeyCode::Right => Key::Right,
        _ => Key::Unknown,
    }
}

fn raw_key(byte: u8) -> Key {
    match byte {
        0x1B => Key::Escape,
        b'\r' | b'\n' => Key::Enter,
        0x08 | 0x7F => Key::Backspace,
        0x02 => Key::Ctrl('b'),
        0x04 => Key::Ctrl('d'),
        0x06 => Key::Ctrl('f'),
        0x15 => Key::Ctrl('u'),
        value if value.is_ascii() && !value.is_ascii_control() => Key::Char(value as char),
        _ => Key::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::ReviewView;
    use crate::{
        clipboard::{ClipboardRegister, SystemClipboardProvider},
        screen_reader::ScreenReader,
        speech,
        terminal::HistoryPosition,
        terminal_input::KeyInput,
        view::View,
        views::{ViewAction, ViewController, ViewKind},
    };
    use std::{cell::RefCell, rc::Rc};
    use terminput::{KeyCode, KeyEvent, KeyModifiers};

    struct RecordingDriver(Rc<RefCell<Vec<String>>>);

    impl speech::Driver for RecordingDriver {
        fn speak(&mut self, text: &str, _interrupt: bool) -> anyhow::Result<()> {
            self.0.borrow_mut().push(text.to_string());
            Ok(())
        }
        fn stop(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn get_rate(&self) -> f32 {
            1.0
        }
        fn set_rate(&mut self, _rate: f32) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn setup(text: &[u8]) -> (ReviewView, ScreenReader, Rc<RefCell<Vec<String>>>) {
        let mut source = View::new(3, 30);
        source.process_changes(text);
        source.set_review_history_position(crate::terminal::HistoryPosition { row: 0, col: 0 });
        let output = Rc::new(RefCell::new(Vec::new()));
        let sr = ScreenReader::new(speech::Speech::new(Box::new(RecordingDriver(
            output.clone(),
        ))));
        (ReviewView::new(&mut source), sr, output)
    }

    fn input(view: &mut ReviewView, sr: &mut ScreenReader, bytes: &[u8]) -> ViewAction {
        view.handle_input(sr, bytes, &mut Vec::new()).unwrap()
    }

    #[test]
    fn review_is_read_only_and_only_q_exits() {
        let (mut view, mut sr, _) = setup(b"abc");
        assert_eq!(view.kind(), ViewKind::Review);
        assert!(matches!(input(&mut view, &mut sr, b"x"), ViewAction::Bell));
        assert!(matches!(
            input(&mut view, &mut sr, b"\x1b"),
            ViewAction::Bell
        ));
        assert!(matches!(input(&mut view, &mut sr, b"q"), ViewAction::Pop));
    }

    #[test]
    fn review_application_cursor_starts_at_the_source_review_cursor() {
        let mut source = View::new(3, 20);
        source.process_changes(b"zero\r\none\r\ntwo\r\nthree\r\nfour\r\nfive");
        source.set_review_history_position(HistoryPosition { row: 1, col: 2 });
        let expected = source.review_history_position();

        let review = ReviewView::new(&mut source);

        assert_eq!(review.cursor, expected);
        assert_eq!(
            review.view.screen().cursor_position(),
            (
                expected.row.saturating_sub(review.viewport_top) as u16,
                expected.col
            )
        );
    }

    #[test]
    fn escape_cancels_pending_command_without_closing() {
        let (mut view, mut sr, _) = setup(b"abc");
        assert!(matches!(input(&mut view, &mut sr, b"f"), ViewAction::None));
        assert!(matches!(
            input(&mut view, &mut sr, b"\x1b"),
            ViewAction::None
        ));
        assert!(matches!(
            input(&mut view, &mut sr, b"\x1b"),
            ViewAction::Bell
        ));
    }

    #[test]
    fn failed_motion_and_find_bell() {
        let (mut view, mut sr, _) = setup(b"abc");
        assert!(matches!(input(&mut view, &mut sr, b"h"), ViewAction::Bell));
        assert!(matches!(input(&mut view, &mut sr, b"fz"), ViewAction::Bell));
    }

    #[test]
    fn vi_motions_do_not_use_or_move_the_independent_review_cursor() {
        let (mut view, mut sr, _) = setup(b"abcd");
        view.model().set_review_cursor_col(2);

        assert!(matches!(
            input(&mut view, &mut sr, b"l"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 1);
        assert_eq!(view.model().review_cursor_position(), (0, 2));
    }

    #[test]
    fn custom_table_setup_and_cell_jumps_use_separate_utterances() {
        let mut source = View::new(4, 40);
        source.process_changes(b"NAME      AGE\r\nAlice     37\r\nBob       42");
        source.set_review_history_position(HistoryPosition { row: 0, col: 0 });
        let spoken = Rc::new(RefCell::new(Vec::new()));
        let mut sr = ScreenReader::new(speech::Speech::new(Box::new(RecordingDriver(
            spoken.clone(),
        ))));
        let mut view = ReviewView::new(&mut source);

        assert!(matches!(input(&mut view, &mut sr, b"gT"), ViewAction::None));
        assert!(matches!(input(&mut view, &mut sr, b" "), ViewAction::None));
        assert!(matches!(
            input(&mut view, &mut sr, b"10l "),
            ViewAction::Redraw
        ));
        assert!(matches!(
            input(&mut view, &mut sr, b"2jgB"),
            ViewAction::Redraw
        ));
        assert!(matches!(
            input(&mut view, &mut sr, b"k\r"),
            ViewAction::RedrawSilently
        ));

        assert!(spoken.borrow().ends_with(&[
            "table setup saved".to_owned(),
            "row 1".to_owned(),
            "AGE".to_owned(),
            "37".to_owned(),
        ]));

        spoken.borrow_mut().clear();
        assert!(matches!(
            input(&mut view, &mut sr, b"}|"),
            ViewAction::RedrawSilently
        ));
        assert_eq!(spoken.borrow().as_slice(), ["row 2", "AGE", "42"]);

        spoken.borrow_mut().clear();
        assert!(matches!(input(&mut view, &mut sr, b"}|"), ViewAction::None));
        assert_eq!(spoken.borrow().as_slice(), ["bottom of table", "row 2"]);

        spoken.borrow_mut().clear();
        assert!(matches!(
            input(&mut view, &mut sr, b"k"),
            ViewAction::Redraw
        ));
        assert_eq!(spoken.borrow().as_slice(), ["row 1"]);

        spoken.borrow_mut().clear();
        assert!(matches!(
            input(&mut view, &mut sr, b"10h"),
            ViewAction::Redraw
        ));
        assert_eq!(spoken.borrow().as_slice(), ["NAME"]);

        spoken.borrow_mut().clear();
        assert!(matches!(input(&mut view, &mut sr, b"gH"), ViewAction::None));
        assert_eq!(spoken.borrow().as_slice(), ["row headers from", "NAME"]);

        input(&mut view, &mut sr, b"10l");
        spoken.borrow_mut().clear();
        assert!(matches!(
            input(&mut view, &mut sr, b"}|"),
            ViewAction::RedrawSilently
        ));
        assert_eq!(spoken.borrow().as_slice(), ["row 2", "Bob", "AGE", "42"]);

        spoken.borrow_mut().clear();
        input(&mut view, &mut sr, b"10h");
        assert!(matches!(input(&mut view, &mut sr, b"gH"), ViewAction::None));
        assert_eq!(
            spoken.borrow().last().map(String::as_str),
            Some("row headers off")
        );
    }

    #[test]
    fn cell_motions_directionally_reenter_the_active_table() {
        let mut source = View::new(8, 40);
        source.process_changes(
            b"| NAME | AGE |\r\n| ---- | --- |\r\n| Alice | 37 |\r\n| Bob | 42 |\r\n\r\nafter\r\nlater",
        );
        source.set_review_history_position(HistoryPosition { row: 0, col: 2 });
        let spoken = Rc::new(RefCell::new(Vec::new()));
        let mut sr = ScreenReader::new(speech::Speech::new(Box::new(RecordingDriver(
            spoken.clone(),
        ))));
        let mut view = ReviewView::new(&mut source);

        input(&mut view, &mut sr, b"gt");
        input(&mut view, &mut sr, b"j");
        spoken.borrow_mut().clear();
        assert!(matches!(
            input(&mut view, &mut sr, b"}|"),
            ViewAction::RedrawSilently
        ));
        assert_eq!(spoken.borrow().as_slice(), ["row 1", "NAME", "Alice"]);

        input(&mut view, &mut sr, b"G");
        spoken.borrow_mut().clear();
        assert!(matches!(
            input(&mut view, &mut sr, b"{|"),
            ViewAction::RedrawSilently
        ));
        assert_eq!(spoken.borrow().as_slice(), ["row 2", "NAME", "Bob"]);
    }

    #[test]
    fn starting_detection_or_setup_discards_the_active_table() {
        let mut source = View::new(8, 40);
        source.process_changes(
            b"| NAME | AGE |\r\n| ---- | --- |\r\n| Alice | 37 |\r\n\r\nplain\r\ntext\r\nhere",
        );
        source.set_review_history_position(HistoryPosition { row: 0, col: 2 });
        let spoken = Rc::new(RefCell::new(Vec::new()));
        let mut sr = ScreenReader::new(speech::Speech::new(Box::new(RecordingDriver(
            spoken.clone(),
        ))));
        let mut view = ReviewView::new(&mut source);

        input(&mut view, &mut sr, b"gt");
        assert!(view.table.is_some());
        input(&mut view, &mut sr, b"Ggt");
        assert!(view.table.is_none());
        assert_eq!(
            spoken.borrow().last().map(String::as_str),
            Some("no table found")
        );

        input(&mut view, &mut sr, b"gggt");
        assert!(view.table.is_some());
        input(&mut view, &mut sr, b"gT");
        assert!(view.table.is_none());
        assert!(view.table_setup.is_some());
        input(&mut view, &mut sr, b"\x1b");
        assert!(view.table.is_none());
        assert!(view.table_setup.is_none());
    }

    #[test]
    fn name_edit_escape_is_transactional_and_control_u_clears() {
        let mut source = View::new(3, 40);
        source.process_changes(b"Alice     37\r\nBob       42");
        source.set_review_history_position(HistoryPosition { row: 0, col: 0 });
        let spoken = Rc::new(RefCell::new(Vec::new()));
        let mut sr = ScreenReader::new(speech::Speech::new(Box::new(RecordingDriver(spoken))));
        let mut view = ReviewView::new(&mut source);

        input(&mut view, &mut sr, b"gTH c");
        input(&mut view, &mut sr, b"name\r");
        input(&mut view, &mut sr, b"cs\x1b");
        assert_eq!(view.table_setup.as_ref().unwrap().name(0), Some("name"));

        input(&mut view, &mut sr, b"c\x15\r");
        assert_eq!(view.table_setup.as_ref().unwrap().name(0), None);

        input(&mut view, &mut sr, b"\x1b");
        assert!(view.table_setup.is_none());
        input(&mut view, &mut sr, b"gTH ");
        assert_eq!(view.table_setup.as_ref().unwrap().name(0), None);
    }

    #[test]
    fn automatic_table_detection_uses_offscreen_scrollback_cursor() {
        let mut source = View::new(3, 40);
        source.process_changes(
            b"| NAME | AGE |\r\n| ---- | --- |\r\n| Alice | 37 |\r\n\r\nlater\r\nlatest",
        );
        source.set_review_history_position(HistoryPosition { row: 0, col: 2 });
        let spoken = Rc::new(RefCell::new(Vec::new()));
        let mut sr = ScreenReader::new(speech::Speech::new(Box::new(RecordingDriver(
            spoken.clone(),
        ))));
        let mut view = ReviewView::new(&mut source);

        assert_eq!(view.cursor.row, 0);
        assert!(view.document.row_count() > 3);
        assert!(matches!(
            input(&mut view, &mut sr, b"gt"),
            ViewAction::None | ViewAction::RedrawSilently
        ));
        assert!(view.table.is_some());
        assert!(spoken.borrow().iter().any(|utterance| utterance == "table"));
    }

    #[test]
    fn z_commands_place_the_cursor_line_in_the_viewport() {
        let mut source = View::new(5, 20);
        source.process_changes(
            b"zero\r\none\r\ntwo\r\nthree\r\nfour\r\nfive\r\n  six\r\nseven\r\neight",
        );
        source.set_review_history_position(HistoryPosition { row: 6, col: 4 });
        let output = Rc::new(RefCell::new(Vec::new()));
        let mut sr = ScreenReader::new(speech::Speech::new(Box::new(RecordingDriver(output))));
        let mut view = ReviewView::new(&mut source);

        assert!(matches!(
            input(&mut view, &mut sr, b"zt"),
            ViewAction::Redraw
        ));
        assert_eq!(view.viewport_top, 6);
        assert_eq!(view.model().screen().cursor_position(), (0, 4));

        assert!(matches!(
            input(&mut view, &mut sr, b"zz"),
            ViewAction::Redraw
        ));
        assert_eq!(view.viewport_top, 4);
        assert_eq!(view.model().screen().cursor_position(), (2, 4));

        assert!(matches!(
            input(&mut view, &mut sr, b"zb"),
            ViewAction::Redraw
        ));
        assert_eq!(view.viewport_top, 2);
        assert_eq!(view.model().screen().cursor_position(), (4, 4));

        assert!(matches!(
            input(&mut view, &mut sr, b"z\r"),
            ViewAction::Redraw
        ));
        assert_eq!(view.viewport_top, 6);
        assert_eq!(view.cursor.col, 2);

        assert!(matches!(
            input(&mut view, &mut sr, b"3z."),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor, HistoryPosition { row: 2, col: 0 });
        assert_eq!(view.viewport_top, 0);
        assert_eq!(view.model().screen().cursor_position(), (2, 0));
    }

    #[test]
    fn find_and_till_repeats_track_the_matched_character() {
        let (mut view, mut sr, _) = setup(b"a1x2x");
        assert!(matches!(
            input(&mut view, &mut sr, b"tx"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 1);
        assert!(matches!(
            input(&mut view, &mut sr, b";"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 3);

        let (mut view, mut sr, _) = setup(b"a1x2x");
        assert!(matches!(
            input(&mut view, &mut sr, b"fx;"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 4);
        assert!(matches!(
            input(&mut view, &mut sr, b","),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 2);

        assert!(matches!(input(&mut view, &mut sr, b"fz"), ViewAction::Bell));
        assert!(matches!(
            input(&mut view, &mut sr, b";"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 4);
    }

    #[test]
    fn count_motion_and_percent_move_the_cursor() {
        let (mut view, mut sr, _) = setup(b"one two {x}");
        assert!(matches!(
            input(&mut view, &mut sr, b"2w"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 8);
        assert!(matches!(
            input(&mut view, &mut sr, b"%"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 10);
    }

    #[test]
    fn inner_word_yank_uses_the_internal_clipboard() {
        let (mut view, mut sr, spoken) = setup(b"alpha beta");
        assert!(matches!(
            input(&mut view, &mut sr, b"yiw"),
            ViewAction::None
        ));
        assert_eq!(sr.clipboard_text(), Some("alpha"));
        assert!(spoken.borrow().iter().any(|text| text == "copied"));
    }

    #[test]
    fn explicit_and_default_system_registers_write_through_osc52() {
        let (mut view, mut sr, _) = setup(b"alpha beta");
        sr.set_system_clipboard_provider(SystemClipboardProvider::Osc52);

        assert!(matches!(
            input(&mut view, &mut sr, b"\"+yiw"),
            ViewAction::None
        ));
        assert_eq!(sr.clipboard_text(), None);
        assert_eq!(
            sr.take_terminal_clipboard_writes(),
            [b"\x1b]52;c;YWxwaGE=\x1b\\".to_vec()]
        );

        sr.set_clipboard_default_register(ClipboardRegister::System);
        assert!(matches!(
            input(&mut view, &mut sr, b"wyiw"),
            ViewAction::Redraw
        ));
        assert_eq!(
            sr.take_terminal_clipboard_writes(),
            [b"\x1b]52;c;YmV0YQ==\x1b\\".to_vec()]
        );
    }

    #[test]
    fn yank_motions_counts_lines_and_visual_ranges_use_vi_boundaries() {
        let (mut view, mut sr, _) = setup(b"alpha beta\r\ngamma");
        assert!(matches!(
            input(&mut view, &mut sr, b"y2l"),
            ViewAction::None
        ));
        assert_eq!(sr.clipboard_text(), Some("al"));

        assert!(matches!(
            input(&mut view, &mut sr, b"2yiw"),
            ViewAction::None
        ));
        assert_eq!(sr.clipboard_text(), Some("alpha beta"));

        assert!(matches!(
            input(&mut view, &mut sr, b"vey"),
            ViewAction::Redraw
        ));
        assert_eq!(sr.clipboard_text(), Some("alpha"));

        assert!(matches!(
            input(&mut view, &mut sr, b"2yy"),
            ViewAction::None
        ));
        assert_eq!(sr.clipboard_text(), Some("alpha beta\ngamma\n"));
    }

    #[test]
    fn page_and_edge_line_motions_reveal_frozen_scrollback() {
        let mut source = View::new(3, 20);
        source.process_changes(b"zero\r\none\r\ntwo\r\nthree\r\nfour\r\nfive");
        source.follow_application_cursor();
        let output = Rc::new(RefCell::new(Vec::new()));
        let mut sr = ScreenReader::new(speech::Speech::new(Box::new(RecordingDriver(output))));
        let mut view = ReviewView::new(&mut source);
        assert_eq!(view.viewport_top, source.scrollback_len());

        assert!(matches!(
            input(&mut view, &mut sr, b"\x02"),
            ViewAction::Redraw
        ));
        assert_eq!(view.viewport_top, 0);
        assert_eq!(view.cursor.row, 2);
        assert!(view.model().contents_full().contains("two"));

        assert!(matches!(
            input(&mut view, &mut sr, b"\x06"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.row, view.document.row_count() - 1);
        assert_eq!(view.viewport_top, source.scrollback_len());

        source.set_review_history_position(HistoryPosition {
            row: source.scrollback_len(),
            col: 0,
        });
        let mut view = ReviewView::new(&mut source);
        let old_top = view.viewport_top;
        assert!(matches!(
            input(&mut view, &mut sr, b"k"),
            ViewAction::Redraw
        ));
        assert_eq!(view.viewport_top + 1, old_top);
    }

    #[test]
    fn forward_backward_and_repeat_search_work() {
        let (mut view, mut sr, _) = setup(b"alpha beta alpha");
        assert!(matches!(
            input(&mut view, &mut sr, b"/alpha\r"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 11);
        assert!(matches!(
            input(&mut view, &mut sr, b"n"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 0);
        assert!(matches!(
            input(&mut view, &mut sr, b"N"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 11);

        let (mut view, mut sr, _) = setup(b"alpha beta alpha");
        assert!(matches!(
            input(&mut view, &mut sr, b"?alpha\r"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 11);
    }

    #[test]
    fn search_errors_bell_and_escape_only_cancels_the_prompt() {
        let (mut view, mut sr, _) = setup(b"alpha");
        assert!(matches!(input(&mut view, &mut sr, b"n"), ViewAction::Bell));
        assert!(matches!(
            input(&mut view, &mut sr, b"/missing\r"),
            ViewAction::Bell
        ));
        assert!(view.search_prompt.is_some());
        assert!(matches!(
            input(&mut view, &mut sr, b"\x1b"),
            ViewAction::Redraw
        ));
        assert!(view.search_prompt.is_none());

        assert!(matches!(
            input(&mut view, &mut sr, b"/[\r"),
            ViewAction::Bell
        ));
    }

    #[test]
    fn missing_prompt_and_unmatched_brace_are_bell_errors() {
        let (mut view, mut sr, _) = setup(b"{broken");
        assert!(matches!(input(&mut view, &mut sr, b"]p"), ViewAction::Bell));
        assert!(matches!(input(&mut view, &mut sr, b"%"), ViewAction::Bell));
    }

    #[test]
    fn screen_snapshot_stays_frozen() {
        let mut source = View::new(2, 20);
        source.process_changes(b"before");
        let mut review = ReviewView::new(&mut source);
        source.process_changes(b" after");
        assert!(review.model().contents_full().contains("before"));
        assert!(!review.model().contents_full().contains("after"));
    }

    #[test]
    fn resize_keeps_the_logical_cursor_and_pans_the_frozen_snapshot() {
        let (mut view, _sr, _) = setup(b"abcdefghij\r\nsecond");
        view.cursor.col = 9;

        view.on_resize(2, 5);

        assert_eq!(view.model().size(), (2, 5));
        assert_eq!(view.cursor.col, 9);
        assert_eq!(view.viewport_left, 5);
        assert_eq!(view.model().screen().cursor_position(), (0, 4));
        assert!(view.model().contents_full().contains("fghij"));

        view.on_resize(2, 8);
        assert_eq!(view.cursor.col, 9);
        assert_eq!(view.viewport_left, 2);
        assert_eq!(view.model().screen().cursor_position(), (0, 7));
        assert!(view.model().contents_full().contains("cdefghij"));
    }

    #[test]
    fn resize_changes_page_distance_and_clamps_the_viewport() {
        let mut source = View::new(3, 20);
        source.process_changes(b"zero\r\none\r\ntwo\r\nthree\r\nfour\r\nfive");
        source.follow_application_cursor();
        let output = Rc::new(RefCell::new(Vec::new()));
        let mut sr = ScreenReader::new(speech::Speech::new(Box::new(RecordingDriver(output))));
        let mut view = ReviewView::new(&mut source);

        view.on_resize(2, 20);
        let old_top = view.viewport_top;
        assert!(matches!(
            input(&mut view, &mut sr, b"\x02"),
            ViewAction::Redraw
        ));
        assert_eq!(view.viewport_top, old_top.saturating_sub(2));

        view.on_resize(6, 20);
        assert_eq!(view.viewport_top, 0);
        assert_eq!(view.model().size(), (6, 20));
    }

    #[test]
    fn resize_preserves_the_cursor_screen_row_until_a_boundary_requires_more_context() {
        let mut source = View::new(4, 20);
        source.process_changes(b"zero\r\none\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\nseven");
        let mut view = ReviewView::new(&mut source);

        view.cursor = HistoryPosition { row: 4, col: 0 };
        view.viewport_top = 2;
        view.render();
        view.on_resize(3, 20);
        assert_eq!(view.viewport_top, 2);
        assert_eq!(view.model().screen().cursor_position().0, 2);

        view.on_resize(5, 20);
        assert_eq!(view.viewport_top, 2);
        assert_eq!(view.model().screen().cursor_position().0, 2);

        view.cursor.row = view.document.row_count() - 1;
        view.viewport_top = view.document.max_viewport_top(5);
        view.render();
        view.on_resize(7, 20);
        assert_eq!(view.viewport_top, view.document.max_viewport_top(7));
        assert_eq!(view.model().screen().cursor_position().0, 6);

        view.on_resize(3, 20);
        assert_eq!(view.viewport_top, view.document.max_viewport_top(3));
        assert_eq!(view.model().screen().cursor_position().0, 2);
    }

    #[test]
    fn extended_ctrl_page_keys_take_the_semantic_control_path() {
        let mut source = View::new(3, 20);
        source.process_changes(b"zero\r\none\r\ntwo\r\nthree\r\nfour\r\nfive");
        source.follow_application_cursor();
        let output = Rc::new(RefCell::new(Vec::new()));
        let mut sr = ScreenReader::new(speech::Speech::new(Box::new(RecordingDriver(output))));
        let mut view = ReviewView::new(&mut source);
        let key = KeyInput::new(
            KeyEvent::new(KeyCode::Char('b')).modifiers(KeyModifiers::CTRL),
            b"\x1b[98;5;2u",
        );

        assert!(matches!(
            view.handle_key_input(&mut sr, &key, b"", &mut Vec::new())
                .unwrap(),
            ViewAction::Redraw
        ));
        assert_eq!(view.viewport_top, 0);
    }
}
