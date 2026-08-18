use super::{CommandResult, Result, review};
use crate::{
    keymap::InputMode,
    screen_reader::ScreenReader,
    table::{self, TableState, TabstopChange},
    view::View,
};

pub(super) fn toggle_mode(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if matches!(sr.input_mode(), InputMode::Table) {
        return exit_mode(sr);
    }

    let row = view.review_cursor_position().0;
    let Some(model) = table::detect(view, row) else {
        sr.speak("no table found", false)?;
        return Ok(CommandResult::Handled);
    };
    enter_mode_with_model(sr, view, model)?;
    Ok(CommandResult::Handled)
}

pub(super) fn start_setup(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if matches!(sr.input_mode(), InputMode::TableSetup) {
        sr.speak("table setup already on", false)?;
        return Ok(CommandResult::Handled);
    }
    if matches!(sr.input_mode(), InputMode::Table) {
        sr.speak("exit table mode first", false)?;
        return Ok(CommandResult::Handled);
    }

    let row = view.review_cursor_position().0;
    if view.line(row).trim().is_empty() {
        sr.speak("header row is blank", false)?;
        return Ok(CommandResult::Handled);
    }

    let old_mode = sr.table_session_mut().enter_setup(row);
    sr.hook_on_mode_change(old_mode, sr.input_mode())?;
    sr.speak("table setup on", false)?;
    Ok(CommandResult::Handled)
}

pub(super) fn cancel_setup(sr: &mut ScreenReader) -> Result<CommandResult> {
    if !matches!(sr.input_mode(), InputMode::TableSetup) {
        return Ok(CommandResult::Handled);
    }

    let old_mode = sr.table_session_mut().exit();
    sr.hook_on_mode_change(old_mode, sr.input_mode())?;
    sr.speak("table setup off", false)?;
    Ok(CommandResult::Handled)
}

pub(super) fn toggle_setup_tabstop(sr: &mut ScreenReader, view: &View) -> Result<CommandResult> {
    if !matches!(sr.input_mode(), InputMode::TableSetup) {
        return Ok(CommandResult::Handled);
    }
    let Some(setup) = sr.table_session_mut().setup_mut() else {
        sr.speak("table setup not active", false)?;
        return Ok(CommandResult::Handled);
    };

    let col = view.review_cursor_position().1;
    if col == 0 {
        sr.speak("cannot set tabstop at first column", false)?;
        return Ok(CommandResult::Handled);
    }

    let message = match setup.toggle_tabstop(col) {
        TabstopChange::Added => "tabstop added",
        TabstopChange::Removed => "tabstop removed",
    };
    sr.speak(message, false)?;
    Ok(CommandResult::Handled)
}

pub(super) fn commit_setup(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if !matches!(sr.input_mode(), InputMode::TableSetup) {
        return Ok(CommandResult::Handled);
    }

    let Some(setup) = sr.table_session().setup().cloned() else {
        sr.speak("table setup not active", false)?;
        return Ok(CommandResult::Handled);
    };
    let Some(model) = table::detect_manual_from_header(view, setup.header_row(), setup.tabstops())
    else {
        sr.speak("manual table setup invalid", false)?;
        return Ok(CommandResult::Handled);
    };

    enter_mode_with_model(sr, view, model)?;
    Ok(CommandResult::Handled)
}

fn enter_mode_with_model(
    sr: &mut ScreenReader,
    view: &mut View,
    model: table::TableModel,
) -> Result<()> {
    let old_position = view.review_cursor_position();
    let anchor_row = old_position.0;
    let entry_row = model
        .nearest_data_row(view, anchor_row)
        .unwrap_or(anchor_row);
    let preferred_column = model.column_for_col(view.review_cursor_position().1);
    let current_col = model.nearest_non_empty_col(view, entry_row, preferred_column);
    let state = TableState::new(model, current_col);
    let column = state
        .current_column()
        .expect("detected tables always contain a current column");
    view.set_review_cursor_position((entry_row, column.start()));

    let old_mode = sr.table_session_mut().enter_table(state.clone());
    sr.hook_on_mode_change(old_mode, sr.input_mode())?;
    sr.hook_on_table_mode_enter(&state)?;
    review::report_move(sr, view, old_position)?;
    sr.speak("table mode on", false)?;
    cell_read(sr, view)?;
    Ok(())
}

pub(super) fn exit_mode(sr: &mut ScreenReader) -> Result<CommandResult> {
    let old_mode = sr.table_session_mut().exit();
    sr.hook_on_mode_change(old_mode, sr.input_mode())?;
    sr.hook_on_table_mode_exit()?;
    sr.speak("table mode off", false)?;
    Ok(CommandResult::Handled)
}

#[derive(Copy, Clone)]
pub(super) enum RowMove {
    Previous,
    Next,
    First,
    Last,
}

pub(super) fn row_move(
    sr: &mut ScreenReader,
    view: &mut View,
    movement: RowMove,
) -> Result<CommandResult> {
    if !ensure_state(sr, view) {
        sr.speak("no table found", false)?;
        return Ok(CommandResult::Handled);
    }

    let state = sr.table_session().navigation().unwrap().clone();
    let current_row = view.review_cursor_position().0;
    let (new_row, boundary) = match movement {
        RowMove::Previous => (state.model().prev_data_row(view, current_row), "top"),
        RowMove::Next => (state.model().next_data_row(view, current_row), "bottom"),
        RowMove::First => (
            state.model().nearest_data_row(view, state.model().top()),
            "top",
        ),
        RowMove::Last => (
            state.model().nearest_data_row(view, state.model().bottom()),
            "bottom",
        ),
    };
    let Some(new_row) = new_row else {
        sr.speak(boundary, false)?;
        return Ok(CommandResult::Handled);
    };

    let old_position = view.review_cursor_position();
    move_to_cell(view, &state, new_row);
    review::report_move(sr, view, old_position)?;
    speak_cell(sr, view, &state, false)?;
    Ok(CommandResult::Handled)
}

#[derive(Copy, Clone)]
pub(super) enum ColumnMove {
    Previous,
    Next,
    First,
    Last,
}

pub(super) fn column_move(
    sr: &mut ScreenReader,
    view: &mut View,
    movement: ColumnMove,
) -> Result<CommandResult> {
    if !ensure_state(sr, view) {
        sr.speak("no table found", false)?;
        return Ok(CommandResult::Handled);
    }

    let state = sr.table_session().navigation().unwrap();
    let current_col = state.current_col();
    let last_column = state.model().column_count().saturating_sub(1);
    let (target, boundary) = match movement {
        ColumnMove::Previous => (current_col.checked_sub(1), "left"),
        ColumnMove::Next => (
            (current_col < last_column).then_some(current_col + 1),
            "right",
        ),
        ColumnMove::First => ((current_col != 0).then_some(0), "left"),
        ColumnMove::Last => ((current_col != last_column).then_some(last_column), "right"),
    };
    let Some(target) = target else {
        sr.speak(boundary, false)?;
        return Ok(CommandResult::Handled);
    };

    let old_position = view.review_cursor_position();
    let row = old_position.0;
    let state_snapshot = {
        let state = sr.table_session_mut().navigation_mut().unwrap();
        state.set_current_col(target);
        move_to_cell(view, state, row);
        state.clone()
    };
    review::report_move(sr, view, old_position)?;
    speak_cell(sr, view, &state_snapshot, sr.table_header_auto())?;
    Ok(CommandResult::Handled)
}

pub(super) fn cell_read(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if !ensure_state(sr, view) {
        sr.speak("no table found", false)?;
        return Ok(CommandResult::Handled);
    }
    let state = sr.table_session().navigation().unwrap().clone();
    speak_cell(sr, view, &state, false)?;
    Ok(CommandResult::Handled)
}

pub(super) fn header_read(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if !ensure_state(sr, view) {
        sr.speak("no table found", false)?;
        return Ok(CommandResult::Handled);
    }
    let state = sr.table_session().navigation().unwrap().clone();
    if let Some(text) = state.model().header_text(view, state.current_col()) {
        sr.speak(&text, false)?;
    } else {
        sr.speak("no header", false)?;
    }
    Ok(CommandResult::Handled)
}

pub(super) fn toggle_header_read(sr: &mut ScreenReader) -> Result<CommandResult> {
    let status = if sr.toggle_table_header_auto() {
        "on"
    } else {
        "off"
    };
    sr.speak(&format!("table headers {status}"), false)?;
    Ok(CommandResult::Handled)
}

pub(super) fn word_previous(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if !ensure_state(sr, view) {
        sr.speak("no table found", false)?;
        return Ok(CommandResult::Handled);
    }
    let Some((start, _)) = current_cell_text_bounds(sr, view) else {
        return Ok(CommandResult::Handled);
    };
    let old_position = view.review_cursor_position();
    let (row, col) = old_position;
    if col <= start {
        sr.speak("left", false)?;
        word_read(sr, view)?;
        return Ok(CommandResult::Handled);
    }

    let mut index = col.saturating_sub(1);
    while index > start && is_cell_whitespace(view, row, index) {
        index = index.saturating_sub(1);
    }
    while index > start && !is_cell_whitespace(view, row, index.saturating_sub(1)) {
        index = index.saturating_sub(1);
    }
    view.set_review_cursor_col(index);
    review::report_move(sr, view, old_position)?;
    word_read(sr, view)?;
    Ok(CommandResult::Handled)
}

pub(super) fn word_next(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if !ensure_state(sr, view) {
        sr.speak("no table found", false)?;
        return Ok(CommandResult::Handled);
    }
    let Some((start, end)) = current_cell_text_bounds(sr, view) else {
        return Ok(CommandResult::Handled);
    };
    let old_position = view.review_cursor_position();
    let (row, mut index) = old_position;
    let old_word_end = word_end_from_or_left(view, row, old_position.1, start, end);
    if index < start {
        index = start;
    }
    while index < end && !is_cell_whitespace(view, row, index) {
        index += 1;
    }
    while index <= end && is_cell_whitespace(view, row, index) {
        index += 1;
    }
    if index > end || old_word_end.is_some_and(|word_end| index <= word_end) {
        sr.speak("right", false)?;
        word_read(sr, view)?;
        return Ok(CommandResult::Handled);
    }
    view.set_review_cursor_col(index);
    review::report_move(sr, view, old_position)?;
    word_read(sr, view)?;
    Ok(CommandResult::Handled)
}

pub(super) fn word_read(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if !ensure_state(sr, view) {
        sr.speak("no table found", false)?;
        return Ok(CommandResult::Handled);
    }
    let Some((start, end)) = current_cell_text_bounds(sr, view) else {
        return Ok(CommandResult::Handled);
    };
    let (row, col) = view.review_cursor_position();
    let Some((word_start, word_end)) = word_bounds_at(view, row, col, start, end) else {
        return Ok(CommandResult::Handled);
    };
    let text = view
        .screen()
        .contents_between(row, word_start, row, word_end + 1);
    let spoken = text.trim();
    if !spoken.is_empty() {
        sr.speak(spoken, false)?;
    }
    Ok(CommandResult::Handled)
}

fn word_bounds_at(view: &View, row: u16, col: u16, start: u16, end: u16) -> Option<(u16, u16)> {
    let mut index = col.clamp(start, end);
    if is_cell_whitespace(view, row, index) {
        let mut right = index;
        while right <= end && is_cell_whitespace(view, row, right) {
            right += 1;
        }
        index = if right <= end {
            right
        } else {
            let mut left = index;
            while left > start && is_cell_whitespace(view, row, left) {
                left -= 1;
            }
            if is_cell_whitespace(view, row, left) {
                return None;
            }
            left
        };
    }

    let mut word_start = index;
    while word_start > start && !is_cell_whitespace(view, row, word_start - 1) {
        word_start -= 1;
    }
    let mut word_end = index;
    while word_end < end && !is_cell_whitespace(view, row, word_end + 1) {
        word_end += 1;
    }
    Some((word_start, word_end))
}

fn word_end_from_or_left(view: &View, row: u16, col: u16, start: u16, end: u16) -> Option<u16> {
    let mut index = col.clamp(start, end);
    while index > start && is_cell_whitespace(view, row, index) {
        index -= 1;
    }
    if is_cell_whitespace(view, row, index) {
        return None;
    }
    while index < end && !is_cell_whitespace(view, row, index + 1) {
        index += 1;
    }
    Some(index)
}

pub(super) fn character_previous(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if !ensure_state(sr, view) {
        sr.speak("no table found", false)?;
        return Ok(CommandResult::Handled);
    }
    let Some((start, _)) = current_cell_text_bounds(sr, view) else {
        return Ok(CommandResult::Handled);
    };
    let old_position = view.review_cursor_position();
    if old_position.1 <= start {
        sr.speak("left", false)?;
        character_read(sr, view)?;
        return Ok(CommandResult::Handled);
    }
    view.set_review_cursor_col(old_position.1.saturating_sub(1));
    review::report_move(sr, view, old_position)?;
    character_read(sr, view)?;
    Ok(CommandResult::Handled)
}

pub(super) fn character_next(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if !ensure_state(sr, view) {
        sr.speak("no table found", false)?;
        return Ok(CommandResult::Handled);
    }
    let Some((_, end)) = current_cell_text_bounds(sr, view) else {
        return Ok(CommandResult::Handled);
    };
    let old_position = view.review_cursor_position();
    if old_position.1 >= end {
        sr.speak("right", false)?;
        character_read(sr, view)?;
        return Ok(CommandResult::Handled);
    }
    view.set_review_cursor_col(old_position.1 + 1);
    review::report_move(sr, view, old_position)?;
    character_read(sr, view)?;
    Ok(CommandResult::Handled)
}

pub(super) fn character_read(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if !ensure_state(sr, view) {
        sr.speak("no table found", false)?;
        return Ok(CommandResult::Handled);
    }
    let Some((start, end)) = current_cell_text_bounds(sr, view) else {
        return Ok(CommandResult::Handled);
    };
    let (row, col) = view.review_cursor_position();
    let col = col.clamp(start, end);
    let character = view.screen().contents_between(row, col, row, col + 1);
    if !character.trim().is_empty() {
        sr.speak(&character, false)?;
    }
    Ok(CommandResult::Handled)
}

fn ensure_state(sr: &mut ScreenReader, view: &mut View) -> bool {
    let row = view.review_cursor_position().0;
    let needs_refresh = match sr.table_session().navigation() {
        Some(state) => row < state.model().top() || row > state.model().bottom(),
        None => true,
    };
    if needs_refresh {
        let Some(model) = table::detect(view, row) else {
            sr.table_session_mut().set_navigation(None);
            return false;
        };
        let current_col = model.column_for_col(view.review_cursor_position().1);
        sr.table_session_mut()
            .set_navigation(Some(TableState::new(model, current_col)));
    }

    if let Some(state) = sr.table_session_mut().navigation_mut() {
        if state.model().is_skippable_row(view, row)
            && let Some(target_row) = state.model().nearest_data_row(view, row)
        {
            move_to_cell(view, state, target_row);
        }
        let current_col = state
            .model()
            .column_for_col(view.review_cursor_position().1);
        state.set_current_col(if current_col < state.model().column_count() {
            current_col
        } else {
            0
        });
    }
    true
}

fn move_to_cell(view: &mut View, state: &TableState, row: u16) {
    let row = state.model().clamp_row(row);
    if let Some(column) = state.current_column() {
        let target_col =
            first_text_col(view, row, column.start(), column.end()).unwrap_or(column.start());
        view.set_review_cursor_position((row, target_col));
    }
}

fn current_cell_text_bounds(sr: &ScreenReader, view: &View) -> Option<(u16, u16)> {
    let state = sr.table_session().navigation()?;
    let row = view.review_cursor_position().0;
    let column = state.current_column()?;
    let start = first_text_col(view, row, column.start(), column.end())?;
    let end = last_text_col(view, row, column.start(), column.end())?;
    Some((start, end))
}

fn first_text_col(view: &View, row: u16, start: u16, end: u16) -> Option<u16> {
    (start..=end).find(|&col| !is_cell_whitespace(view, row, col))
}

fn last_text_col(view: &View, row: u16, start: u16, end: u16) -> Option<u16> {
    (start..=end)
        .rev()
        .find(|&col| !is_cell_whitespace(view, row, col))
}

fn is_cell_whitespace(view: &View, row: u16, col: u16) -> bool {
    view.screen()
        .cell(row, col)
        .map(|cell| !cell.is_wide_continuation() && cell.contents().trim().is_empty())
        .unwrap_or(true)
}

fn speak_cell(
    sr: &mut ScreenReader,
    view: &View,
    state: &TableState,
    include_header: bool,
) -> Result<()> {
    let row = view.review_cursor_position().0;
    if include_header
        && let Some(header_row) = state.model().header_row()
        && header_row != row
        && let Some(text) = state.model().header_text(view, state.current_col())
    {
        sr.speak(&text, false)?;
    }
    let text = state.model().cell_text(view, row, state.current_col());
    if !text.trim().is_empty() {
        sr.speak(&text, false)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        RowMove, character_next, character_previous, character_read, first_text_col, last_text_col,
        row_move, toggle_mode, word_bounds_at, word_end_from_or_left, word_next, word_previous,
        word_read,
    };
    use crate::{screen_reader::ScreenReader, speech, view::View};
    use std::{cell::RefCell, rc::Rc};

    struct RecordingDriver(Rc<RefCell<Vec<String>>>);

    impl speech::Driver for RecordingDriver {
        fn speak(&mut self, text: &str, _interrupt: bool) -> anyhow::Result<()> {
            self.0.borrow_mut().push(text.to_owned());
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

    fn screen_reader() -> (ScreenReader, Rc<RefCell<Vec<String>>>) {
        let output = Rc::new(RefCell::new(Vec::new()));
        let speech = speech::Speech::new(Box::new(RecordingDriver(Rc::clone(&output))));
        (ScreenReader::new(speech), output)
    }

    fn word_view() -> View {
        let mut view = View::new(1, 12);
        view.process_changes(b"foo  bar");
        view
    }

    #[test]
    fn word_bounds_choose_the_current_or_nearest_word() {
        let view = word_view();
        assert_eq!(word_bounds_at(&view, 0, 1, 0, 9), Some((0, 2)));
        assert_eq!(word_bounds_at(&view, 0, 3, 0, 9), Some((5, 7)));
        assert_eq!(word_bounds_at(&view, 0, 9, 0, 9), Some((5, 7)));
    }

    #[test]
    fn word_end_uses_the_word_at_or_left_of_whitespace() {
        let view = word_view();
        assert_eq!(word_end_from_or_left(&view, 0, 4, 0, 9), Some(2));
        assert_eq!(word_end_from_or_left(&view, 0, 6, 0, 9), Some(7));
    }

    #[test]
    fn text_bounds_ignore_padding() {
        let view = word_view();
        assert_eq!(first_text_col(&view, 0, 0, 11), Some(0));
        assert_eq!(last_text_col(&view, 0, 0, 11), Some(7));
    }

    #[test]
    fn table_word_and_character_navigation_cover_both_boundaries() {
        let (mut sr, output) = screen_reader();
        let mut view = View::new(3, 20);
        view.process_changes(b"A   VALUE\r\nx   data baz");
        view.set_review_cursor_position((1, 4));
        toggle_mode(&mut sr, &mut view).unwrap();

        word_previous(&mut sr, &mut view).unwrap();
        word_next(&mut sr, &mut view).unwrap();
        word_next(&mut sr, &mut view).unwrap();
        word_previous(&mut sr, &mut view).unwrap();
        word_read(&mut sr, &mut view).unwrap();
        character_previous(&mut sr, &mut view).unwrap();
        character_next(&mut sr, &mut view).unwrap();
        character_read(&mut sr, &mut view).unwrap();
        view.set_review_cursor_col(8);
        character_read(&mut sr, &mut view).unwrap();

        assert_eq!(view.review_cursor_position(), (1, 8));
        assert_eq!(
            output.borrow().as_slice(),
            [
                "table mode on",
                "data baz",
                "left",
                "data",
                "baz",
                "right",
                "baz",
                "data",
                "data",
                "left",
                "d",
                "a",
                "a",
            ]
        );
    }

    #[test]
    fn blank_table_cells_are_silent_for_navigation_and_explicit_reads() {
        let (mut sr, output) = screen_reader();
        let mut view = View::new(3, 20);
        view.process_changes(b"A   VALUE   END\r\nx   data    yes\r\ny           no");
        view.set_review_cursor_position((1, 4));
        toggle_mode(&mut sr, &mut view).unwrap();
        output.borrow_mut().clear();

        row_move(&mut sr, &mut view, RowMove::Next).unwrap();
        word_read(&mut sr, &mut view).unwrap();
        character_read(&mut sr, &mut view).unwrap();

        assert_eq!(view.review_cursor_position().0, 2);
        assert!(output.borrow().is_empty());
    }
}
