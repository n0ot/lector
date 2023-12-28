use super::{
    attributes,
    ext::{CellExt, ScreenExt},
    screen_reader::{CursorTrackingMode, ScreenReader},
    view::View,
};
use anyhow::{anyhow, Result};
use std::io::Write;

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

pub fn handle_action(
    screen_reader: &mut ScreenReader,
    view: &mut View,
    pty_stream: &mut ptyprocess::stream::Stream,
    action: Action,
) -> Result<bool> {
    if let Action::ToggleHelp = action {
        return action_toggle_help(screen_reader);
    }
    if screen_reader.help_mode {
        screen_reader.speech.speak(&action.help_text(), false)?;
        return Ok(false);
    }

    match action {
        Action::ToggleAutoRead => action_toggle_auto_read(screen_reader),
        Action::ToggleReviewCursorFollowsScreenCursor => {
            action_toggle_review_cursor_follows_screen_cursor(screen_reader, view)
        }
        Action::ToggleSymbolLevel => action_toggle_symbol_level(screen_reader),
        Action::PassNextKey => action_pass_next_key(screen_reader),
        Action::StopSpeaking => action_stop(screen_reader),
        Action::RevLinePrev => action_review_line_prev(screen_reader, view, false),
        Action::RevLineNext => action_review_line_next(screen_reader, view, false),
        Action::RevLinePrevNonBlank => action_review_line_prev(screen_reader, view, true),
        Action::RevLineNextNonBlank => action_review_line_next(screen_reader, view, true),
        Action::RevLineRead => action_review_line_read(screen_reader, view),
        Action::RevWordPrev => action_review_word_prev(screen_reader, view),
        Action::RevWordNext => action_review_word_next(screen_reader, view),
        Action::RevWordRead => action_review_word_read(screen_reader, view),
        Action::RevCharPrev => action_review_char_prev(screen_reader, view),
        Action::RevCharNext => action_review_char_next(screen_reader, view),
        Action::RevCharRead => action_review_char_read(screen_reader, view),
        Action::RevCharReadPhonetic => action_review_char_read_phonetic(screen_reader, view),
        Action::RevTop => action_review_top(screen_reader, view),
        Action::RevBottom => action_review_bottom(screen_reader, view),
        Action::RevFirst => action_review_first(screen_reader, view),
        Action::RevLast => action_review_last(screen_reader, view),
        Action::RevReadAttributes => action_review_read_attributes(screen_reader, view),
        Action::Backspace => action_backspace(screen_reader, view),
        Action::Delete => action_delete(screen_reader, view),
        Action::SayTime => action_say_time(screen_reader),
        Action::SetMark => action_set_mark(screen_reader, view),
        Action::Copy => action_copy(screen_reader, view),
        Action::Paste => action_paste(screen_reader, view, pty_stream),
        Action::SayClipboard => action_clipboard_say(screen_reader),
        Action::PreviousClipboard => action_clipboard_prev(screen_reader),
        Action::NextClipboard => action_clipboard_next(screen_reader),
        _ => {
            screen_reader.speech.speak("not implemented", false)?;
            Ok(false)
        }
    }
}

// Actions
fn action_stop(screen_reader: &mut ScreenReader) -> Result<bool> {
    screen_reader.speech.stop()?;
    Ok(false)
}

fn action_toggle_auto_read(screen_reader: &mut ScreenReader) -> Result<bool> {
    if screen_reader.auto_read {
        screen_reader.auto_read = false;
        screen_reader.speech.speak("auto read disabled", false)?;
    } else {
        screen_reader.auto_read = true;
        screen_reader.speech.speak("auto read enabled", false)?;
    }

    Ok(false)
}

fn action_toggle_review_cursor_follows_screen_cursor(
    screen_reader: &mut ScreenReader,
    view: &mut View,
) -> Result<bool> {
    screen_reader.review_follows_screen_cursor = !screen_reader.review_follows_screen_cursor;
    match screen_reader.review_follows_screen_cursor {
        true => {
            view.review_cursor_position = view.screen().cursor_position();
            screen_reader
                .speech
                .speak("review cursor following screen cursor", false)?;
        }
        false => screen_reader
            .speech
            .speak("review cursor not following screen cursor", false)?,
    };
    Ok(false)
}

fn action_pass_next_key(screen_reader: &mut ScreenReader) -> Result<bool> {
    screen_reader.pass_through = true;
    screen_reader
        .speech
        .speak("forward next key press", false)?;
    Ok(false)
}

fn action_toggle_help(screen_reader: &mut ScreenReader) -> Result<bool> {
    if screen_reader.help_mode {
        screen_reader.help_mode = false;
        screen_reader.speech.speak("exiting help", false)?;
    } else {
        screen_reader.help_mode = true;
        screen_reader
            .speech
            .speak("entering help. Press this key again to exit", false)?;
    }
    Ok(false)
}

fn action_review_line_prev(
    screen_reader: &mut ScreenReader,
    view: &mut View,
    skip_blank_lines: bool,
) -> Result<bool> {
    if !view.review_cursor_up(skip_blank_lines) {
        screen_reader.speech.speak("top", false)?;
    }
    action_review_line_read(screen_reader, view)?;
    Ok(false)
}

fn action_review_line_next(
    screen_reader: &mut ScreenReader,
    view: &mut View,
    skip_blank_lines: bool,
) -> Result<bool> {
    if !view.review_cursor_down(skip_blank_lines) {
        screen_reader.speech.speak("bottom", false)?;
    }
    action_review_line_read(screen_reader, view)?;
    Ok(false)
}

fn action_review_line_read(screen_reader: &mut ScreenReader, view: &mut View) -> Result<bool> {
    let row = view.review_cursor_position.0;
    screen_reader.report_review_cursor_indentation_changes(view)?;
    let line = view.screen().contents_between(row, 0, row, view.size().1);
    if line.is_empty() {
        screen_reader.speech.speak("blank", false)?;
    } else {
        screen_reader.speech.speak(&line, false)?;
    }
    Ok(false)
}

fn action_review_word_prev(screen_reader: &mut ScreenReader, view: &mut View) -> Result<bool> {
    if !view.review_cursor_prev_word() {
        screen_reader.speech.speak("left", false)?;
    }
    action_review_word_read(screen_reader, view)?;
    Ok(false)
}

fn action_review_word_next(screen_reader: &mut ScreenReader, view: &mut View) -> Result<bool> {
    if !view.review_cursor_next_word() {
        screen_reader.speech.speak("right", false)?;
    }
    action_review_word_read(screen_reader, view)?;
    Ok(false)
}

fn action_review_word_read(screen_reader: &mut ScreenReader, view: &View) -> Result<bool> {
    let (row, col) = view.review_cursor_position;
    let start = view.screen().find_word_start(row, col);
    let end = view.screen().find_word_end(row, col);

    let word = view.screen().contents_between(row, start, row, end + 1);
    screen_reader.speech.speak(&word, false)?;
    Ok(false)
}

fn action_review_char_prev(screen_reader: &mut ScreenReader, view: &mut View) -> Result<bool> {
    if !view.review_cursor_left() {
        screen_reader.speech.speak("left", false)?;
    }
    action_review_char_read(screen_reader, view)?;
    Ok(false)
}

fn action_review_char_next(screen_reader: &mut ScreenReader, view: &mut View) -> Result<bool> {
    if !view.review_cursor_right() {
        screen_reader.speech.speak("right", false)?;
    }
    action_review_char_read(screen_reader, view)?;
    Ok(false)
}

fn action_review_char_read(screen_reader: &mut ScreenReader, view: &View) -> Result<bool> {
    let (row, col) = view.review_cursor_position;
    let char = view
        .screen()
        .cell(row, col)
        .ok_or_else(|| anyhow!("cannot get cell at row {}, column {}", row, col))?
        .contents();
    if char.is_empty() {
        screen_reader.speech.speak("blank", false)?;
    } else {
        screen_reader.speech.speak(&char, false)?;
    }
    Ok(false)
}

fn action_review_char_read_phonetic(screen_reader: &mut ScreenReader, view: &View) -> Result<bool> {
    let (row, col) = view.review_cursor_position;
    let char = view
        .screen()
        .cell(row, col)
        .ok_or_else(|| anyhow!("cannot get cell at row {}, column {}", row, col))?
        .contents();
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
    screen_reader.speech.speak(char, false)?;
    Ok(false)
}

fn action_review_top(screen_reader: &mut ScreenReader, view: &mut View) -> Result<bool> {
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
    action_review_line_read(screen_reader, view)?;
    Ok(false)
}

fn action_review_bottom(screen_reader: &mut ScreenReader, view: &mut View) -> Result<bool> {
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
    action_review_line_read(screen_reader, view)?;
    Ok(false)
}

fn action_review_first(screen_reader: &mut ScreenReader, view: &mut View) -> Result<bool> {
    let (row, col) = view.review_cursor_position;
    let last = view.size().1 - 1;
    view.review_cursor_position.1 = match col {
        0 => view
            .screen()
            .find_cell(CellExt::is_in_word, row, 0, row, last)
            .map_or(0, |(_, col)| col),
        _ => 0,
    };
    action_review_char_read(screen_reader, view)?;
    Ok(false)
}

fn action_review_last(screen_reader: &mut ScreenReader, view: &mut View) -> Result<bool> {
    let (row, col) = view.review_cursor_position;
    let last = view.size().1 - 1;
    view.review_cursor_position.1 = if col == last {
        view.screen()
            .rfind_cell(CellExt::is_in_word, row, 0, row, last)
            .map_or(last, |(_, col)| col)
    } else {
        last
    };
    action_review_char_read(screen_reader, view)?;
    Ok(false)
}

fn action_review_read_attributes(screen_reader: &mut ScreenReader, view: &View) -> Result<bool> {
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

    screen_reader.speech.speak(&attrs, false)?;
    Ok(false)
}

fn action_backspace(screen_reader: &mut ScreenReader, view: &View) -> Result<bool> {
    let (row, col) = view.screen().cursor_position();
    if col > 0 {
        let char = view
            .screen()
            .cell(row, col - 1)
            .ok_or_else(|| anyhow!("cannot get cell at row {}, column {}", row, col))?
            .contents();
        screen_reader.speech.speak(&char, false)?;
    }
    // When backspacing, the cursor will end up moving to the left, but we don't want to hear
    // that.
    screen_reader.cursor_tracking_mode = match screen_reader.cursor_tracking_mode {
        CursorTrackingMode::Off => CursorTrackingMode::Off,
        _ => CursorTrackingMode::OffOnce,
    };
    Ok(true)
}

fn action_delete(screen_reader: &mut ScreenReader, view: &View) -> Result<bool> {
    let (row, col) = view.screen().cursor_position();
    let char = view
        .screen()
        .cell(row, col)
        .ok_or_else(|| anyhow!("cannot get cell at row {}, column {}", row, col))?
        .contents();
    screen_reader.speech.speak(&char, false)?;
    Ok(true)
}

fn action_say_time(screen_reader: &mut ScreenReader) -> Result<bool> {
    let date = chrono::Local::now();
    screen_reader
        .speech
        .speak(&format!("{}", date.format("%H:%M")), false)?;
    Ok(false)
}
fn action_set_mark(screen_reader: &mut ScreenReader, view: &View) -> Result<bool> {
    screen_reader.clipboard.mark = Some(view.review_cursor_position);
    screen_reader.speech.speak("mark set", false)?;
    Ok(false)
}

fn action_copy(screen_reader: &mut ScreenReader, view: &View) -> Result<bool> {
    match screen_reader.clipboard.mark {
        Some((mark_row, mark_col)) => {
            let (cur_row, cur_col) = view.review_cursor_position;
            if mark_row > cur_row || (mark_row == cur_row && mark_col > cur_col) {
                screen_reader
                    .speech
                    .speak("mark is after the review cursor", false)?;
                return Ok(false);
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
            screen_reader.clipboard.mark = None;
            screen_reader.clipboard.put(contents);
            screen_reader.speech.speak("copied", false)?;
        }
        None => screen_reader.speech.speak("no mark set", false)?,
    }
    Ok(false)
}

fn action_paste(
    screen_reader: &mut ScreenReader,
    view: &View,
    stream: &mut ptyprocess::stream::Stream,
) -> Result<bool> {
    match screen_reader.clipboard.get() {
        Some(contents) => {
            if view.screen().bracketed_paste() {
                write!(stream, "\x1B[200~{}\x1B[201~", contents)?;
            } else {
                write!(stream, "{}", contents)?;
            }
            screen_reader.speech.speak("pasted", false)?;
        }
        None => screen_reader.speech.speak("no clipboard", false)?,
    }
    Ok(false)
}

fn action_clipboard_prev(screen_reader: &mut ScreenReader) -> Result<bool> {
    if screen_reader.clipboard.size() == 0 {
        screen_reader.speech.speak("no clipboard", false)?;
    } else if screen_reader.clipboard.prev() {
        action_clipboard_say(screen_reader)?;
    } else {
        screen_reader.speech.speak("first clipboard", false)?;
    }
    Ok(false)
}

fn action_clipboard_next(screen_reader: &mut ScreenReader) -> Result<bool> {
    if screen_reader.clipboard.size() == 0 {
        screen_reader.speech.speak("no clipboard", false)?;
    } else if screen_reader.clipboard.next() {
        action_clipboard_say(screen_reader)?;
    } else {
        screen_reader.speech.speak("last clipboard", false)?;
    }
    Ok(false)
}

fn action_clipboard_say(screen_reader: &mut ScreenReader) -> Result<bool> {
    match screen_reader.clipboard.get() {
        Some(contents) => screen_reader.speech.speak(contents, false)?,
        None => screen_reader.speech.speak("no clipboard", false)?,
    }
    Ok(false)
}

fn action_toggle_symbol_level(screen_reader: &mut ScreenReader) -> Result<bool> {
    use super::speech::symbols::Level;

    screen_reader.speech.symbol_level = match screen_reader.speech.symbol_level {
        Level::None => {
            screen_reader.speech.speak("some", false)?;
            Level::Some
        }
        Level::Some => {
            screen_reader.speech.speak("most", false)?;
            Level::Most
        }
        Level::Most => {
            screen_reader.speech.speak("all", false)?;
            Level::All
        }
        Level::All | Level::Character => {
            screen_reader.speech.speak("none", false)?;
            Level::None
        }
    };

    Ok(false)
}
