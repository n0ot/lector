use super::{
    attributes,
    ext::{CellExt, ScreenExt},
    screen_reader::{CursorTrackingMode, ScreenReader},
    view::View,
};
use anyhow::{Result, anyhow};

#[derive(Copy, Clone)]
pub enum Action {
    ToggleHelp,
    ToggleAutoRead,
    ToggleReviewCursorFollowsScreenCursor,
    ToggleSymbolLevel,
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
}

pub enum CommandResult {
    Handled,
    ForwardInput,
    Paste(String),
}

impl Action {
    fn help_text(&self) -> String {
        match self {
            Action::ToggleHelp => "toggle help".into(),
            Action::ToggleAutoRead => "toggle auto read".into(),
            Action::ToggleReviewCursorFollowsScreenCursor => {
                "toggle whether review cursor follows screen cursor".into()
            }
            Action::ToggleSymbolLevel => "toggle symbol level".into(),
            Action::PassNextKey => "forward next key press".into(),
            Action::StopSpeaking => "stop speaking".into(),
            Action::RevLinePrev => "previous line".into(),
            Action::RevLineNext => "next line".into(),
            Action::RevLinePrevNonBlank => "previous non blank line".into(),
            Action::RevLineNextNonBlank => "next non blank line".into(),
            Action::RevLineRead => "current line".into(),
            Action::RevCharPrev => "previous character".into(),
            Action::RevCharNext => "next character".into(),
            Action::RevCharRead => "current character".into(),
            Action::RevCharReadPhonetic => "current character phonetically".into(),
            Action::RevWordPrev => "previous word".into(),
            Action::RevWordNext => "next word".into(),
            Action::RevWordRead => "current word".into(),
            Action::RevTop => "top".into(),
            Action::RevBottom => "botom".into(),
            Action::RevFirst => "beginning of line".into(),
            Action::RevLast => "end of line".into(),
            Action::RevReadAttributes => "read attributes".into(),
            Action::Backspace => "backspace".into(),
            Action::Delete => "delete".into(),
            Action::SayTime => "say the time".into(),
            Action::SetMark => "set mark".into(),
            Action::Copy => "copy".into(),
            Action::Paste => "paste".into(),
            Action::SayClipboard => "say clipboard".into(),
            Action::PreviousClipboard => "previous clipboard".into(),
            Action::NextClipboard => "next clipboard".into(),
        }
    }
}

pub fn handle(
    sr: &mut ScreenReader,
    view: &mut View,
    action: Action,
) -> Result<CommandResult> {
    if let Action::ToggleHelp = action {
        return action_toggle_help(sr);
    }
    if sr.help_mode {
        sr.speech.speak(&action.help_text(), false)?;
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
        _ => {
            sr.speech.speak("not implemented", false)?;
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
        sr.speech.speak("auto read disabled", false)?;
    } else {
        sr.auto_read = true;
        sr.speech.speak("auto read enabled", false)?;
    }

    Ok(CommandResult::Handled)
}

fn action_toggle_review_cursor_follows_screen_cursor(
    sr: &mut ScreenReader,
    view: &mut View,
) -> Result<CommandResult> {
    sr.review_follows_screen_cursor = !sr.review_follows_screen_cursor;
    match sr.review_follows_screen_cursor {
        true => {
            view.review_cursor_position = view.screen().cursor_position();
            sr.speech
                .speak("review cursor following screen cursor", false)?;
        }
        false => sr
            .speech
            .speak("review cursor not following screen cursor", false)?,
    };
    Ok(CommandResult::Handled)
}

fn action_pass_next_key(sr: &mut ScreenReader) -> Result<CommandResult> {
    sr.pass_through = true;
    sr.speech.speak("forward next key press", false)?;
    Ok(CommandResult::Handled)
}

fn action_toggle_help(sr: &mut ScreenReader) -> Result<CommandResult> {
    if sr.help_mode {
        sr.help_mode = false;
        sr.speech.speak("exiting help", false)?;
    } else {
        sr.help_mode = true;
        sr.speech
            .speak("entering help. Press this key again to exit", false)?;
    }
    Ok(CommandResult::Handled)
}

fn action_review_line_prev(
    sr: &mut ScreenReader,
    view: &mut View,
    skip_blank_lines: bool,
) -> Result<CommandResult> {
    if !view.review_cursor_up(skip_blank_lines) {
        sr.speech.speak("top", false)?;
    }
    action_review_line_read(sr, view)?;
    Ok(CommandResult::Handled)
}

fn action_review_line_next(
    sr: &mut ScreenReader,
    view: &mut View,
    skip_blank_lines: bool,
) -> Result<CommandResult> {
    if !view.review_cursor_down(skip_blank_lines) {
        sr.speech.speak("bottom", false)?;
    }
    action_review_line_read(sr, view)?;
    Ok(CommandResult::Handled)
}

fn action_review_line_read(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let row = view.review_cursor_position.0;
    sr.report_review_cursor_indentation_changes(view)?;
    let line = view.line(row);
    if line.is_empty() {
        sr.speech.speak("blank", false)?;
    } else {
        sr.speech.speak(&line, false)?;
    }
    Ok(CommandResult::Handled)
}

fn action_review_word_prev(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if !view.review_cursor_prev_word() {
        sr.speech.speak("left", false)?;
    }
    action_review_word_read(sr, view)?;
    Ok(CommandResult::Handled)
}

fn action_review_word_next(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if !view.review_cursor_next_word() {
        sr.speech.speak("right", false)?;
    }
    action_review_word_read(sr, view)?;
    Ok(CommandResult::Handled)
}

fn action_review_word_read(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let (row, col) = view.review_cursor_position;
    let word = view.word(row, col);
    sr.speech.speak(&word, false)?;
    Ok(CommandResult::Handled)
}

fn action_review_char_prev(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if !view.review_cursor_left() {
        sr.speech.speak("left", false)?;
    }
    action_review_char_read(sr, view)?;
    Ok(CommandResult::Handled)
}

fn action_review_char_next(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    if !view.review_cursor_right() {
        sr.speech.speak("right", false)?;
    }
    action_review_char_read(sr, view)?;
    Ok(CommandResult::Handled)
}

fn action_review_char_read(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let (row, col) = view.review_cursor_position;
    let char = view.character(row, col);
    if char.is_empty() {
        sr.speech.speak("blank", false)?;
    } else {
        sr.speech.speak(&char, false)?;
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
    sr.speech.speak(char, false)?;
    Ok(CommandResult::Handled)
}

fn action_review_top(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
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
    action_review_line_read(sr, view)?;
    Ok(CommandResult::Handled)
}

fn action_review_bottom(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
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
    action_review_line_read(sr, view)?;
    Ok(CommandResult::Handled)
}

fn action_review_first(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let (row, col) = view.review_cursor_position;
    let last = view.size().1 - 1;
    view.review_cursor_position.1 = match col {
        0 => view
            .screen()
            .find_cell(CellExt::is_in_word, row, 0, row, last)
            .map_or(0, |(_, col)| col),
        _ => 0,
    };
    action_review_char_read(sr, view)?;
    Ok(CommandResult::Handled)
}

fn action_review_last(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let (row, col) = view.review_cursor_position;
    let last = view.size().1 - 1;
    view.review_cursor_position.1 = if col == last {
        view.screen()
            .rfind_cell(CellExt::is_in_word, row, 0, row, last)
            .map_or(last, |(_, col)| col)
    } else {
        last
    };
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

    sr.speech.speak(&attrs, false)?;
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
        sr.speech.speak(&char, false)?;
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
    sr.speech.speak(&char, false)?;
    Ok(CommandResult::ForwardInput)
}

fn action_say_time(sr: &mut ScreenReader) -> Result<CommandResult> {
    let date = chrono::Local::now();
    sr.speech
        .speak(&format!("{}", date.format("%H:%M")), false)?;
    Ok(CommandResult::Handled)
}

fn action_set_mark(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    view.review_mark_position = Some(view.review_cursor_position);
    sr.speech.speak("mark set", false)?;
    Ok(CommandResult::Handled)
}

fn action_copy(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    match view.review_mark_position {
        Some((mark_row, mark_col)) => {
            let (cur_row, cur_col) = view.review_cursor_position;
            if mark_row > cur_row || (mark_row == cur_row && mark_col > cur_col) {
                sr.speech.speak("mark is after the review cursor", false)?;
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
            sr.speech.speak("copied", false)?;
        }
        None => sr.speech.speak("no mark set", false)?,
    }
    Ok(CommandResult::Handled)
}

fn action_paste(sr: &mut ScreenReader) -> Result<CommandResult> {
    match sr.clipboard.get() {
        Some(contents) => {
            return Ok(CommandResult::Paste(contents.to_string()));
        }
        None => sr.speech.speak("no clipboard", false)?,
    }
    Ok(CommandResult::Handled)
}

fn action_clipboard_prev(sr: &mut ScreenReader) -> Result<CommandResult> {
    if sr.clipboard.size() == 0 {
        sr.speech.speak("no clipboard", false)?;
    } else if sr.clipboard.prev() {
        action_clipboard_say(sr)?;
    } else {
        sr.speech.speak("first clipboard", false)?;
    }
    Ok(CommandResult::Handled)
}

fn action_clipboard_next(sr: &mut ScreenReader) -> Result<CommandResult> {
    if sr.clipboard.size() == 0 {
        sr.speech.speak("no clipboard", false)?;
    } else if sr.clipboard.next() {
        action_clipboard_say(sr)?;
    } else {
        sr.speech.speak("last clipboard", false)?;
    }
    Ok(CommandResult::Handled)
}

fn action_clipboard_say(sr: &mut ScreenReader) -> Result<CommandResult> {
    match sr.clipboard.get() {
        Some(contents) => sr.speech.speak(contents, false)?,
        None => sr.speech.speak("no clipboard", false)?,
    }
    Ok(CommandResult::Handled)
}

fn action_toggle_symbol_level(sr: &mut ScreenReader) -> Result<CommandResult> {
    use super::speech::symbols::Level;

    sr.speech.symbol_level = match sr.speech.symbol_level {
        Level::None => Level::Some,
        Level::Some => Level::Most,
        Level::Most => Level::All,
        Level::All | Level::Character => Level::None,
    };

    sr.speech
        .speak(&format!("{}", sr.speech.symbol_level), false)?;

    Ok(CommandResult::Handled)
}
