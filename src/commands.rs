use super::{
    attributes,
    ext::{CellExt, ScreenExt},
    keymap::InputMode,
    screen_reader::{CursorTrackingMode, ScreenReader, TableSetupState},
    table::{self, TableState},
    view::View,
};
use anyhow::{Result, anyhow};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Action {
    ToggleHelp,
    ToggleAutoRead,
    ToggleReviewCursorFollowsScreenCursor,
    ToggleSymbolLevel,
    OpenLuaRepl,
    PassNextKey,
    StopSpeaking,
    RevLinePrev,
    RevLineNext,
    RevLinePrevNonBlank,
    RevLineNextNonBlank,
    RevLineRead,
    RevCharPrev,
    RevCharNext,
    RevCharRead,
    RevCharReadPhonetic,
    RevWordPrev,
    RevWordNext,
    RevWordRead,
    RevTop,
    RevBottom,
    RevFirst,
    RevLast,
    RevReadAttributes,
    Backspace,
    Delete,
    SayTime,
    SetMark,
    Copy,
    Paste,
    SayClipboard,
    PreviousClipboard,
    NextClipboard,
    ToggleTableMode,
    ToggleStopSpeechOnFocusLoss,
    StartTableSetupMode,
    CancelTableSetupMode,
    CommitTableSetupMode,
    ToggleTableSetupTabstop,
    ExitTableMode,
    TableRowPrev,
    TableRowNext,
    TableColPrev,
    TableColNext,
    TableCellRead,
    TableHeaderRead,
    ToggleTableHeaderRead,
}

pub enum CommandResult {
    Handled,
    ForwardInput,
    Paste(String),
}

const ACTION_TABLE: &[(Action, &str, &str)] = &[
    (Action::ToggleHelp, "toggle help", "toggle_help"),
    (
        Action::ToggleAutoRead,
        "toggle auto read",
        "toggle_auto_read",
    ),
    (
        Action::ToggleReviewCursorFollowsScreenCursor,
        "toggle whether review cursor follows screen cursor",
        "toggle_review_cursor_follows_screen_cursor",
    ),
    (
        Action::ToggleSymbolLevel,
        "toggle symbol level",
        "toggle_symbol_level",
    ),
    (Action::OpenLuaRepl, "open Lua REPL", "open_lua_repl"),
    (
        Action::PassNextKey,
        "forward next key press",
        "pass_next_key",
    ),
    (Action::StopSpeaking, "stop speaking", "stop_speaking"),
    (Action::RevLinePrev, "previous line", "review_line_prev"),
    (Action::RevLineNext, "next line", "review_line_next"),
    (
        Action::RevLinePrevNonBlank,
        "previous non blank line",
        "review_line_prev_non_blank",
    ),
    (
        Action::RevLineNextNonBlank,
        "next non blank line",
        "review_line_next_non_blank",
    ),
    (Action::RevLineRead, "current line", "review_line_read"),
    (
        Action::RevCharPrev,
        "previous character",
        "review_char_prev",
    ),
    (Action::RevCharNext, "next character", "review_char_next"),
    (Action::RevCharRead, "current character", "review_char_read"),
    (
        Action::RevCharReadPhonetic,
        "current character phonetically",
        "review_char_read_phonetic",
    ),
    (Action::RevWordPrev, "previous word", "review_word_prev"),
    (Action::RevWordNext, "next word", "review_word_next"),
    (Action::RevWordRead, "current word", "review_word_read"),
    (Action::RevTop, "top", "review_top"),
    (Action::RevBottom, "bottom", "review_bottom"),
    (Action::RevFirst, "beginning of line", "review_first"),
    (Action::RevLast, "end of line", "review_last"),
    (
        Action::RevReadAttributes,
        "read attributes",
        "review_read_attributes",
    ),
    (Action::Backspace, "backspace", "backspace"),
    (Action::Delete, "delete", "delete"),
    (Action::SayTime, "say the time", "say_time"),
    (Action::SetMark, "set mark", "set_mark"),
    (Action::Copy, "copy", "copy"),
    (Action::Paste, "paste", "paste"),
    (Action::SayClipboard, "say clipboard", "say_clipboard"),
    (
        Action::PreviousClipboard,
        "previous clipboard",
        "previous_clipboard",
    ),
    (Action::NextClipboard, "next clipboard", "next_clipboard"),
    (
        Action::ToggleTableMode,
        "toggle table mode",
        "toggle_table_mode",
    ),
    (
        Action::ToggleStopSpeechOnFocusLoss,
        "toggle stop speech on focus loss",
        "toggle_stop_speech_on_focus_loss",
    ),
    (
        Action::StartTableSetupMode,
        "start table setup mode",
        "start_table_setup_mode",
    ),
    (
        Action::CancelTableSetupMode,
        "cancel table setup mode",
        "cancel_table_setup_mode",
    ),
    (
        Action::CommitTableSetupMode,
        "commit table setup mode",
        "commit_table_setup_mode",
    ),
    (
        Action::ToggleTableSetupTabstop,
        "toggle tabstop at review cursor",
        "toggle_table_setup_tabstop",
    ),
    (Action::ExitTableMode, "exit table mode", "exit_table_mode"),
    (Action::TableRowPrev, "previous table row", "table_row_prev"),
    (Action::TableRowNext, "next table row", "table_row_next"),
    (
        Action::TableColPrev,
        "previous table column",
        "table_col_prev",
    ),
    (Action::TableColNext, "next table column", "table_col_next"),
    (
        Action::TableCellRead,
        "current table cell",
        "table_cell_read",
    ),
    (
        Action::TableHeaderRead,
        "current table header",
        "table_header_read",
    ),
    (
        Action::ToggleTableHeaderRead,
        "toggle table header reading",
        "toggle_table_header_read",
    ),
];

impl Action {
    pub fn help_text(&self) -> String {
        ACTION_TABLE
            .iter()
            .find(|(action, _, _)| action == self)
            .map(|(_, help, _)| (*help).to_string())
            .unwrap_or_default()
    }
}

pub fn builtin_action_name(action: Action) -> &'static str {
    ACTION_TABLE
        .iter()
        .find(|(entry, _, _)| *entry == action)
        .map(|(_, _, builtin)| *builtin)
        .unwrap_or("")
}

pub fn builtin_action_from_name(name: &str) -> Option<Action> {
    ACTION_TABLE
        .iter()
        .find(|(_, _, builtin)| *builtin == name)
        .map(|(action, _, _)| *action)
}

pub fn handle(sr: &mut ScreenReader, view: &mut View, action: Action) -> Result<CommandResult> {
    if let Action::ToggleHelp = action {
        return action_toggle_help(sr);
    }
    if sr.help_mode {
        sr.speak(&action.help_text(), false)?;
        return Ok(CommandResult::Handled);
    }

    match action {
        Action::ToggleAutoRead => action_toggle_auto_read(sr),
        Action::ToggleReviewCursorFollowsScreenCursor => {
            action_toggle_review_cursor_follows_screen_cursor(sr, view)
        }
        Action::ToggleSymbolLevel => action_toggle_symbol_level(sr),
        Action::PassNextKey => action_pass_next_key(sr),
        Action::StopSpeaking => action_stop(sr),
        Action::RevLinePrev => action_review_line_prev(sr, view, false),
        Action::RevLineNext => action_review_line_next(sr, view, false),
        Action::RevLinePrevNonBlank => action_review_line_prev(sr, view, true),
        Action::RevLineNextNonBlank => action_review_line_next(sr, view, true),
        Action::RevLineRead => action_review_line_read(sr, view),
        Action::RevWordPrev => action_review_word_prev(sr, view),
        Action::RevWordNext => action_review_word_next(sr, view),
        Action::RevWordRead => action_review_word_read(sr, view),
        Action::RevCharPrev => action_review_char_prev(sr, view),
        Action::RevCharNext => action_review_char_next(sr, view),
        Action::RevCharRead => action_review_char_read(sr, view),
        Action::RevCharReadPhonetic => action_review_char_read_phonetic(sr, view),
        Action::RevTop => action_review_top(sr, view),
        Action::RevBottom => action_review_bottom(sr, view),
        Action::RevFirst => action_review_first(sr, view),
        Action::RevLast => action_review_last(sr, view),
        Action::RevReadAttributes => action_review_read_attributes(sr, view),
        Action::Backspace => action_backspace(sr, view),
        Action::Delete => action_delete(sr, view),
        Action::SayTime => action_say_time(sr),
        Action::SetMark => action_set_mark(sr, view),
        Action::Copy => action_copy(sr, view),
        Action::Paste => action_paste(sr),
        Action::SayClipboard => action_clipboard_say(sr),
        Action::PreviousClipboard => action_clipboard_prev(sr),
        Action::NextClipboard => action_clipboard_next(sr),
        Action::ToggleTableMode => action_toggle_table_mode(sr, view),
        Action::ToggleStopSpeechOnFocusLoss => action_toggle_stop_speech_on_focus_loss(sr),
        Action::StartTableSetupMode => action_start_table_setup_mode(sr, view),
        Action::CancelTableSetupMode => action_cancel_table_setup_mode(sr),
        Action::CommitTableSetupMode => action_commit_table_setup_mode(sr, view),
        Action::ToggleTableSetupTabstop => action_toggle_table_setup_tabstop(sr, view),
        Action::ExitTableMode => action_exit_table_mode(sr),
        Action::TableRowPrev => action_table_row_prev(sr, view),
        Action::TableRowNext => action_table_row_next(sr, view),
        Action::TableColPrev => action_table_col_prev(sr, view),
        Action::TableColNext => action_table_col_next(sr, view),
        Action::TableCellRead => action_table_cell_read(sr, view),
        Action::TableHeaderRead => action_table_header_read(sr, view),
        Action::ToggleTableHeaderRead => action_toggle_table_header_read(sr),
        _ => {
            sr.speak("not implemented", false)?;
            Ok(CommandResult::Handled)
        }
    }
}

// Actions
fn action_stop(sr: &mut ScreenReader) -> Result<CommandResult> {
    sr.speech.stop()?;
    Ok(CommandResult::Handled)
}

fn action_toggle_auto_read(sr: &mut ScreenReader) -> Result<CommandResult> {
    if sr.auto_read {
        sr.auto_read = false;
        sr.speak("auto read disabled", false)?;
    } else {
        sr.auto_read = true;
        sr.speak("auto read enabled", false)?;
    }

    Ok(CommandResult::Handled)
}

fn action_toggle_stop_speech_on_focus_loss(sr: &mut ScreenReader) -> Result<CommandResult> {
    sr.stop_speech_on_focus_loss = !sr.stop_speech_on_focus_loss;
    let status = if sr.stop_speech_on_focus_loss {
        "enabled"
    } else {
        "disabled"
    };
    sr.speak(&format!("stop on focus loss {}", status), false)?;
    Ok(CommandResult::Handled)
}

fn action_toggle_review_cursor_follows_screen_cursor(
    sr: &mut ScreenReader,
    view: &mut View,
) -> Result<CommandResult> {
    sr.review_follows_screen_cursor = !sr.review_follows_screen_cursor;
    match sr.review_follows_screen_cursor {
        true => {
            let old = view.review_cursor_position;
            view.review_cursor_position = view.screen().cursor_position();
            sr.hook_on_review_cursor_move(old, view.review_cursor_position)?;
            sr.speak("review cursor following screen cursor", false)?;
        }
        false => sr.speak("review cursor not following screen cursor", false)?,
    };
    Ok(CommandResult::Handled)
}

fn action_pass_next_key(sr: &mut ScreenReader) -> Result<CommandResult> {
    sr.pass_through = true;
    sr.speak("forward next key press", false)?;
    Ok(CommandResult::Handled)
}

fn action_toggle_help(sr: &mut ScreenReader) -> Result<CommandResult> {
    if sr.help_mode {
        sr.help_mode = false;
        sr.speak("exiting help", false)?;
    } else {
        sr.help_mode = true;
        sr.speak("entering help. Press this key again to exit", false)?;
    }
    Ok(CommandResult::Handled)
}

fn report_review_cursor_move(
    sr: &mut ScreenReader,
    view: &View,
    old_pos: (u16, u16),
) -> Result<()> {
    sr.hook_on_review_cursor_move(old_pos, view.review_cursor_position)
}

fn action_review_line_prev(
    sr: &mut ScreenReader,
    view: &mut View,
    skip_blank_lines: bool,
) -> Result<CommandResult> {
    let old_pos = view.review_cursor_position;
    if !view.review_cursor_up(skip_blank_lines) {
        sr.speak("top", false)?;
    }
    report_review_cursor_move(sr, view, old_pos)?;
    action_review_line_read(sr, view)?;
    Ok(CommandResult::Handled)
}

fn action_review_line_next(
    sr: &mut ScreenReader,
    view: &mut View,
    skip_blank_lines: bool,
) -> Result<CommandResult> {
    let old_pos = view.review_cursor_position;
    if !view.review_cursor_down(skip_blank_lines) {
        sr.speak("bottom", false)?;
    }
    report_review_cursor_move(sr, view, old_pos)?;
    action_review_line_read(sr, view)?;
    Ok(CommandResult::Handled)
}

fn action_review_line_read(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let row = view.review_cursor_position.0;
    sr.report_review_cursor_indentation_changes(view)?;
    let line = view.line(row);
    if line.is_empty() {
        sr.speak("blank", false)?;
    } else {
        sr.speak(&line, false)?;
    }
    Ok(CommandResult::Handled)
}

fn action_review_word_prev(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let old_pos = view.review_cursor_position;
    if !view.review_cursor_prev_word() {
        sr.speak("left", false)?;
    }
    report_review_cursor_move(sr, view, old_pos)?;
    action_review_word_read(sr, view)?;
    Ok(CommandResult::Handled)
}

fn action_review_word_next(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let old_pos = view.review_cursor_position;
    if !view.review_cursor_next_word() {
        sr.speak("right", false)?;
    }
    report_review_cursor_move(sr, view, old_pos)?;
    action_review_word_read(sr, view)?;
    Ok(CommandResult::Handled)
}

fn action_review_word_read(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let (row, col) = view.review_cursor_position;
    let word = view.word(row, col);
    sr.speak(&word, false)?;
    Ok(CommandResult::Handled)
}

fn action_review_char_prev(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let old_pos = view.review_cursor_position;
    if !view.review_cursor_left() {
        sr.speak("left", false)?;
    }
    report_review_cursor_move(sr, view, old_pos)?;
    action_review_char_read(sr, view)?;
    Ok(CommandResult::Handled)
}

fn action_review_char_next(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let old_pos = view.review_cursor_position;
    if !view.review_cursor_right() {
        sr.speak("right", false)?;
    }
    report_review_cursor_move(sr, view, old_pos)?;
    action_review_char_read(sr, view)?;
    Ok(CommandResult::Handled)
}

fn action_review_char_read(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let (row, col) = view.review_cursor_position;
    let char = view.character(row, col);
    if char.is_empty() {
        sr.speak("blank", false)?;
    } else {
        sr.speak(&char, false)?;
    }
    Ok(CommandResult::Handled)
}

fn action_review_char_read_phonetic(
    sr: &mut ScreenReader,
    view: &mut View,
) -> Result<CommandResult> {
    let (row, col) = view.review_cursor_position;
    let char = view.character(row, col);
    let char = match char.to_lowercase().as_str() {
        "a" => "Alpha",
        "b" => "Bravo",
        "c" => "Charlie",
        "d" => "Delta",
        "e" => "Echo",
        "f" => "Foxtrot",
        "g" => "Golf",
        "h" => "Hotel",
        "i" => "India",
        "j" => "Juliett",
        "k" => "Kilo",
        "l" => "Lima",
        "m" => "Mike",
        "n" => "November",
        "o" => "Oscar",
        "p" => "Papa",
        "q" => "Quebec",
        "r" => "Romeo",
        "s" => "Sierra",
        "t" => "Tango",
        "u" => "Uniform",
        "v" => "Victor",
        "w" => "Whiskey",
        "x" => "X-ray",
        "y" => "Yankee",
        "z" => "Zulu",
        _ => &char,
    };
    sr.speak(char, false)?;
    Ok(CommandResult::Handled)
}

fn action_review_top(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let old_pos = view.review_cursor_position;
    let row = view.review_cursor_position.0;
    let last_row = view.size().0 - 1;
    let last_col = view.size().1 - 1;
    view.review_cursor_position.0 = match row {
        0 => view
            .screen()
            .find_cell(CellExt::is_in_word, 0, 0, last_row, last_col)
            .map_or(0, |(row, _)| row),
        _ => 0,
    };
    report_review_cursor_move(sr, view, old_pos)?;
    action_review_line_read(sr, view)?;
    Ok(CommandResult::Handled)
}

fn action_review_bottom(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let old_pos = view.review_cursor_position;
    let row = view.review_cursor_position.0;
    let last_row = view.size().0 - 1;
    let last_col = view.size().1 - 1;
    view.review_cursor_position.0 = if row == last_row {
        view.screen()
            .rfind_cell(CellExt::is_in_word, 0, 0, last_row, last_col)
            .map_or(last_row, |(row, _)| row)
    } else {
        last_row
    };
    report_review_cursor_move(sr, view, old_pos)?;
    action_review_line_read(sr, view)?;
    Ok(CommandResult::Handled)
}

fn action_review_first(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let old_pos = view.review_cursor_position;
    let (row, col) = view.review_cursor_position;
    let last = view.size().1 - 1;
    view.review_cursor_position.1 = match col {
        0 => view
            .screen()
            .find_cell(CellExt::is_in_word, row, 0, row, last)
            .map_or(0, |(_, col)| col),
        _ => 0,
    };
    report_review_cursor_move(sr, view, old_pos)?;
    action_review_char_read(sr, view)?;
    Ok(CommandResult::Handled)
}

fn action_review_last(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let old_pos = view.review_cursor_position;
    let (row, col) = view.review_cursor_position;
    let last = view.size().1 - 1;
    view.review_cursor_position.1 = if col == last {
        view.screen()
            .rfind_cell(CellExt::is_in_word, row, 0, row, last)
            .map_or(last, |(_, col)| col)
    } else {
        last
    };
    report_review_cursor_move(sr, view, old_pos)?;
    action_review_char_read(sr, view)?;
    Ok(CommandResult::Handled)
}

fn action_review_read_attributes(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let (row, col) = view.review_cursor_position;
    let cell = view
        .screen()
        .cell(row, col)
        .ok_or_else(|| anyhow!("cannot get cell at row {}, column {}", row, col))?;

    let mut attrs = String::new();
    attrs.push_str(&format!("Row {} col {} ", row + 1, col + 1));
    attrs.push_str(&format!(
        "{} {}",
        attributes::describe_color(cell.fgcolor()),
        if let vt100::Color::Default = cell.bgcolor() {
            "".into()
        } else {
            format!("on {}", attributes::describe_color(cell.bgcolor()))
        }
    ));
    attrs.push_str(&format!(
        "{}{}{}{}{}",
        if cell.bold() { "bold " } else { "" },
        if cell.italic() { "italic " } else { "" },
        if cell.underline() { "underline " } else { "" },
        if cell.inverse() { "inverse " } else { "" },
        if cell.is_wide() { "wide " } else { "" },
    ));

    sr.speak(&attrs, false)?;
    Ok(CommandResult::Handled)
}

fn action_backspace(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let (row, col) = view.screen().cursor_position();
    if col > 0 {
        let char = view
            .screen()
            .cell(row, col - 1)
            .ok_or_else(|| anyhow!("cannot get cell at row {}, column {}", row, col))?
            .contents();
        sr.speak(&char, false)?;
    }
    // When backspacing, the cursor will end up moving to the left, but we don't want to hear
    // that.
    sr.cursor_tracking_mode = match sr.cursor_tracking_mode {
        CursorTrackingMode::Off => CursorTrackingMode::Off,
        _ => CursorTrackingMode::OffOnce,
    };
    Ok(CommandResult::ForwardInput)
}

fn action_delete(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let (row, col) = view.screen().cursor_position();
    let char = view
        .screen()
        .cell(row, col)
        .ok_or_else(|| anyhow!("cannot get cell at row {}, column {}", row, col))?
        .contents();
    sr.speak(&char, false)?;
    Ok(CommandResult::ForwardInput)
}

fn action_say_time(sr: &mut ScreenReader) -> Result<CommandResult> {
    let date = chrono::Local::now();
    sr.speak(&format!("{}", date.format("%H:%M")), false)?;
    Ok(CommandResult::Handled)
}

fn action_set_mark(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    view.review_mark_position = Some(view.review_cursor_position);
    sr.speak("mark set", false)?;
    Ok(CommandResult::Handled)
}

fn action_copy(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    match view.review_mark_position {
        Some((mark_row, mark_col)) => {
            let (cur_row, cur_col) = view.review_cursor_position;
            if mark_row > cur_row || (mark_row == cur_row && mark_col > cur_col) {
                sr.speak("mark is after the review cursor", false)?;
                return Ok(CommandResult::Handled);
            }

            let mut contents = String::new();
            for row in mark_row..=cur_row {
                let start = if row == mark_row { mark_col } else { 0 };
                // end is not inclusive, so that a blank row can be achieved with start == end.
                let end = if row == cur_row {
                    cur_col + 1
                } else {
                    view.size().1
                };
                // Don't add trailing blank/whitespace cells
                let end = view
                    .screen()
                    .rfind_cell(
                        |c| !c.contents().trim().is_empty(),
                        row,
                        start,
                        row,
                        end - 1,
                    )
                    .map_or(end, |(_, col)| col + 1);
                for col in start..end {
                    contents.push_str(
                        &view
                            .screen()
                            .cell(row, col)
                            .map_or("".into(), vt100::Cell::contents),
                    );
                }
                if row != cur_row {
                    contents.push('\n');
                }
            }
            sr.clipboard.put(contents);
            let entry = sr.clipboard.get().map(|value| value.to_string());
            sr.hook_on_clipboard_change("push", entry.as_deref())?;
            sr.speak("copied", false)?;
        }
        None => sr.speak("no mark set", false)?,
    }
    Ok(CommandResult::Handled)
}

fn action_paste(sr: &mut ScreenReader) -> Result<CommandResult> {
    match sr.clipboard.get() {
        Some(contents) => {
            return Ok(CommandResult::Paste(contents.to_string()));
        }
        None => sr.speak("no clipboard", false)?,
    }
    Ok(CommandResult::Handled)
}

fn action_clipboard_prev(sr: &mut ScreenReader) -> Result<CommandResult> {
    if sr.clipboard.size() == 0 {
        sr.speak("no clipboard", false)?;
    } else if sr.clipboard.prev() {
        let entry = sr.clipboard.get().map(|value| value.to_string());
        sr.hook_on_clipboard_change("prev", entry.as_deref())?;
        action_clipboard_say(sr)?;
    } else {
        sr.speak("first clipboard", false)?;
    }
    Ok(CommandResult::Handled)
}

fn action_clipboard_next(sr: &mut ScreenReader) -> Result<CommandResult> {
    if sr.clipboard.size() == 0 {
        sr.speak("no clipboard", false)?;
    } else if sr.clipboard.next() {
        let entry = sr.clipboard.get().map(|value| value.to_string());
        sr.hook_on_clipboard_change("next", entry.as_deref())?;
        action_clipboard_say(sr)?;
    } else {
        sr.speak("last clipboard", false)?;
    }
    Ok(CommandResult::Handled)
}

fn action_clipboard_say(sr: &mut ScreenReader) -> Result<CommandResult> {
    let contents = sr.clipboard.get().map(|value| value.to_string());
    match contents {
        Some(contents) => sr.speak(&contents, false)?,
        None => sr.speak("no clipboard", false)?,
    }
    Ok(CommandResult::Handled)
}

fn action_toggle_table_mode(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if matches!(sr.input_mode, InputMode::Table) {
        return action_exit_table_mode(sr);
    }

    let row = view.review_cursor_position.0;
    let Some(model) = table::detect(view, row) else {
        sr.speak("no table found", false)?;
        return Ok(CommandResult::Handled);
    };
    enter_table_mode_with_model(sr, view, model)?;
    Ok(CommandResult::Handled)
}

fn action_start_table_setup_mode(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if matches!(sr.input_mode, InputMode::TableSetup) {
        sr.speak("table setup already on", false)?;
        return Ok(CommandResult::Handled);
    }
    if matches!(sr.input_mode, InputMode::Table) {
        sr.speak("exit table mode first", false)?;
        return Ok(CommandResult::Handled);
    }

    let row = view.review_cursor_position.0;
    if view.line(row).trim().is_empty() {
        sr.speak("header row is blank", false)?;
        return Ok(CommandResult::Handled);
    }

    sr.table_setup_state = Some(TableSetupState {
        header_row: row,
        tabstops: Vec::new(),
    });
    sr.table_state = None;
    let old_mode = sr.input_mode;
    sr.input_mode = InputMode::TableSetup;
    sr.hook_on_mode_change(old_mode, sr.input_mode)?;
    sr.speak("table setup on", false)?;
    Ok(CommandResult::Handled)
}

fn action_cancel_table_setup_mode(sr: &mut ScreenReader) -> Result<CommandResult> {
    if !matches!(sr.input_mode, InputMode::TableSetup) {
        return Ok(CommandResult::Handled);
    }

    sr.table_setup_state = None;
    let old_mode = sr.input_mode;
    sr.input_mode = InputMode::Normal;
    sr.hook_on_mode_change(old_mode, sr.input_mode)?;
    sr.speak("table setup off", false)?;
    Ok(CommandResult::Handled)
}

fn action_toggle_table_setup_tabstop(
    sr: &mut ScreenReader,
    view: &mut View,
) -> Result<CommandResult> {
    if !matches!(sr.input_mode, InputMode::TableSetup) {
        return Ok(CommandResult::Handled);
    }
    let Some(setup) = sr.table_setup_state.as_mut() else {
        sr.speak("table setup not active", false)?;
        return Ok(CommandResult::Handled);
    };

    let col = view.review_cursor_position.1;
    if col == 0 {
        sr.speak("cannot set tabstop at first column", false)?;
        return Ok(CommandResult::Handled);
    }

    if let Some(idx) = setup.tabstops.iter().position(|stop| *stop == col) {
        setup.tabstops.remove(idx);
        sr.speak("tabstop removed", false)?;
    } else {
        setup.tabstops.push(col);
        setup.tabstops.sort_unstable();
        setup.tabstops.dedup();
        sr.speak("tabstop added", false)?;
    }
    Ok(CommandResult::Handled)
}

fn action_commit_table_setup_mode(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if !matches!(sr.input_mode, InputMode::TableSetup) {
        return Ok(CommandResult::Handled);
    }

    let Some(setup) = sr.table_setup_state.clone() else {
        sr.speak("table setup not active", false)?;
        return Ok(CommandResult::Handled);
    };

    let Some(model) = table::detect_manual_from_header(view, setup.header_row, &setup.tabstops)
    else {
        sr.speak("manual table setup invalid", false)?;
        return Ok(CommandResult::Handled);
    };

    sr.table_setup_state = None;
    enter_table_mode_with_model(sr, view, model)?;
    Ok(CommandResult::Handled)
}

fn enter_table_mode_with_model(
    sr: &mut ScreenReader,
    view: &mut View,
    model: table::TableModel,
) -> Result<()> {
    let old_pos = view.review_cursor_position;
    let anchor_row = old_pos.0;
    let entry_row = model
        .nearest_data_row(view, anchor_row)
        .unwrap_or(anchor_row);
    let mut col_idx = model.column_for_col(view.review_cursor_position.1);
    col_idx = model.nearest_non_empty_col(view, entry_row, col_idx);
    sr.table_state = Some(TableState {
        model,
        current_col: col_idx,
    });
    if let Some(state) = sr.table_state.as_ref() {
        let column = &state.model.columns[state.current_col];
        view.review_cursor_position = (entry_row, column.start);
    }
    let old_mode = sr.input_mode;
    sr.input_mode = InputMode::Table;
    sr.hook_on_mode_change(old_mode, sr.input_mode)?;
    if let Some(state) = sr.table_state.clone() {
        sr.hook_on_table_mode_enter(&state)?;
    }
    report_review_cursor_move(sr, view, old_pos)?;
    sr.speak("table mode on", false)?;
    action_table_cell_read(sr, view)?;
    Ok(())
}

fn action_exit_table_mode(sr: &mut ScreenReader) -> Result<CommandResult> {
    let old_mode = sr.input_mode;
    sr.input_mode = InputMode::Normal;
    sr.table_state = None;
    sr.table_setup_state = None;
    sr.hook_on_mode_change(old_mode, sr.input_mode)?;
    sr.hook_on_table_mode_exit()?;
    sr.speak("table mode off", false)?;
    Ok(CommandResult::Handled)
}

fn action_table_row_prev(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if !ensure_table_state(sr, view) {
        sr.speak("no table found", false)?;
        return Ok(CommandResult::Handled);
    }
    let state_snapshot = sr.table_state.as_ref().unwrap().clone();
    let Some(new_row) = state_snapshot
        .model
        .prev_data_row(view, view.review_cursor_position.0)
    else {
        sr.speak("top", false)?;
        return Ok(CommandResult::Handled);
    };
    let old_pos = view.review_cursor_position;
    move_review_to_table_cell(view, &state_snapshot, new_row);
    report_review_cursor_move(sr, view, old_pos)?;
    speak_table_cell(sr, view, &state_snapshot, false)?;
    Ok(CommandResult::Handled)
}

fn action_table_row_next(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if !ensure_table_state(sr, view) {
        sr.speak("no table found", false)?;
        return Ok(CommandResult::Handled);
    }
    let state_snapshot = sr.table_state.as_ref().unwrap().clone();
    let Some(new_row) = state_snapshot
        .model
        .next_data_row(view, view.review_cursor_position.0)
    else {
        sr.speak("bottom", false)?;
        return Ok(CommandResult::Handled);
    };
    let old_pos = view.review_cursor_position;
    move_review_to_table_cell(view, &state_snapshot, new_row);
    report_review_cursor_move(sr, view, old_pos)?;
    speak_table_cell(sr, view, &state_snapshot, false)?;
    Ok(CommandResult::Handled)
}

fn action_table_col_prev(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if !ensure_table_state(sr, view) {
        sr.speak("no table found", false)?;
        return Ok(CommandResult::Handled);
    }
    let old_pos = view.review_cursor_position;
    let state_snapshot = {
        let state = sr.table_state.as_mut().unwrap();
        if state.current_col == 0 {
            sr.speak("left", false)?;
            return Ok(CommandResult::Handled);
        }
        state.current_col -= 1;
        move_review_to_table_cell(view, state, view.review_cursor_position.0);
        state.clone()
    };
    report_review_cursor_move(sr, view, old_pos)?;
    speak_table_cell(sr, view, &state_snapshot, sr.table_header_auto)?;
    Ok(CommandResult::Handled)
}

fn action_table_col_next(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if !ensure_table_state(sr, view) {
        sr.speak("no table found", false)?;
        return Ok(CommandResult::Handled);
    }
    let old_pos = view.review_cursor_position;
    let state_snapshot = {
        let state = sr.table_state.as_mut().unwrap();
        if state.current_col + 1 >= state.model.columns.len() {
            sr.speak("right", false)?;
            return Ok(CommandResult::Handled);
        }
        state.current_col += 1;
        move_review_to_table_cell(view, state, view.review_cursor_position.0);
        state.clone()
    };
    report_review_cursor_move(sr, view, old_pos)?;
    speak_table_cell(sr, view, &state_snapshot, sr.table_header_auto)?;
    Ok(CommandResult::Handled)
}

fn action_table_cell_read(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if !ensure_table_state(sr, view) {
        sr.speak("no table found", false)?;
        return Ok(CommandResult::Handled);
    }
    let state = sr.table_state.as_ref().unwrap().clone();
    speak_table_cell(sr, view, &state, false)?;
    Ok(CommandResult::Handled)
}

fn action_table_header_read(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if !ensure_table_state(sr, view) {
        sr.speak("no table found", false)?;
        return Ok(CommandResult::Handled);
    }
    let state = sr.table_state.as_ref().unwrap().clone();
    if let Some(text) = state.model.header_text(view, state.current_col) {
        sr.speak(&text, false)?;
    } else {
        sr.speak("no header", false)?;
    }
    Ok(CommandResult::Handled)
}

fn action_toggle_table_header_read(sr: &mut ScreenReader) -> Result<CommandResult> {
    sr.table_header_auto = !sr.table_header_auto;
    let status = if sr.table_header_auto { "on" } else { "off" };
    sr.speak(&format!("table headers {}", status), false)?;
    Ok(CommandResult::Handled)
}

fn ensure_table_state(sr: &mut ScreenReader, view: &mut View) -> bool {
    let row = view.review_cursor_position.0;
    let needs_refresh = match &sr.table_state {
        Some(state) => row < state.model.top || row > state.model.bottom,
        None => true,
    };
    if needs_refresh {
        if let Some(model) = table::detect(view, row) {
            let col_idx = model.column_for_col(view.review_cursor_position.1);
            sr.table_state = Some(TableState {
                model,
                current_col: col_idx,
            });
        } else {
            sr.table_state = None;
            return false;
        }
    }
    if let Some(state) = sr.table_state.as_mut() {
        if state.model.is_skippable_row(view, row) {
            if let Some(target_row) = state.model.nearest_data_row(view, row) {
                move_review_to_table_cell(view, state, target_row);
            }
        }
        state.current_col = state.model.column_for_col(view.review_cursor_position.1);
        if state.current_col >= state.model.columns.len() {
            state.current_col = 0;
        }
    }
    true
}

fn move_review_to_table_cell(view: &mut View, state: &TableState, row: u16) {
    let row = state.model.clamp_row(row);
    if let Some(column) = state.model.columns.get(state.current_col) {
        view.review_cursor_position = (row, column.start);
    }
}

fn speak_table_cell(
    sr: &mut ScreenReader,
    view: &View,
    state: &TableState,
    include_header: bool,
) -> Result<()> {
    let row = view.review_cursor_position.0;
    if include_header {
        if let Some(header_row) = state.model.header_row {
            if header_row != row {
                if let Some(text) = state.model.header_text(view, state.current_col) {
                    sr.speak(&text, false)?;
                }
            }
        }
    }
    let text = state.model.cell_text(view, row, state.current_col);
    if text.is_empty() {
        sr.speak("blank", false)?;
    } else {
        sr.speak(&text, false)?;
    }
    Ok(())
}

fn action_toggle_symbol_level(sr: &mut ScreenReader) -> Result<CommandResult> {
    use super::speech::symbols::Level;

    sr.speech.symbol_level = match sr.speech.symbol_level {
        Level::None => Level::Some,
        Level::Some => Level::Most,
        Level::Most => Level::All,
        Level::All | Level::Character => Level::None,
    };

    sr.speak(&format!("{}", sr.speech.symbol_level), false)?;

    Ok(CommandResult::Handled)
}
