use super::{
    attributes,
    ext::{CellExt, ScreenExt},
    screen_reader::{CursorTrackingMode, ScreenReader},
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

pub fn handle(
    sr: &mut ScreenReader,
    pty_stream: &mut ptyprocess::stream::Stream,
    action: Action,
) -> Result<bool> {
    if let Action::ToggleHelp = action {
        return action_toggle_help(sr);
    }
    if sr.help_mode {
        sr.speech.speak(&action.help_text(), false)?;
        return Ok(false);
    }

    match action {
        Action::ToggleAutoRead => action_toggle_auto_read(sr),
        Action::ToggleReviewCursorFollowsScreenCursor => {
            action_toggle_review_cursor_follows_screen_cursor(sr)
        }
        Action::ToggleSymbolLevel => action_toggle_symbol_level(sr),
        Action::PassNextKey => action_pass_next_key(sr),
        Action::StopSpeaking => action_stop(sr),
        Action::RevLinePrev => action_review_line_prev(sr, false),
        Action::RevLineNext => action_review_line_next(sr, false),
        Action::RevLinePrevNonBlank => action_review_line_prev(sr, true),
        Action::RevLineNextNonBlank => action_review_line_next(sr, true),
        Action::RevLineRead => action_review_line_read(sr),
        Action::RevWordPrev => action_review_word_prev(sr),
        Action::RevWordNext => action_review_word_next(sr),
        Action::RevWordRead => action_review_word_read(sr),
        Action::RevCharPrev => action_review_char_prev(sr),
        Action::RevCharNext => action_review_char_next(sr),
        Action::RevCharRead => action_review_char_read(sr),
        Action::RevCharReadPhonetic => action_review_char_read_phonetic(sr),
        Action::RevTop => action_review_top(sr),
        Action::RevBottom => action_review_bottom(sr),
        Action::RevFirst => action_review_first(sr),
        Action::RevLast => action_review_last(sr),
        Action::RevReadAttributes => action_review_read_attributes(sr),
        Action::Backspace => action_backspace(sr),
        Action::Delete => action_delete(sr),
        Action::SayTime => action_say_time(sr),
        Action::SetMark => action_set_mark(sr),
        Action::Copy => action_copy(sr),
        Action::Paste => action_paste(sr, pty_stream),
        Action::SayClipboard => action_clipboard_say(sr),
        Action::PreviousClipboard => action_clipboard_prev(sr),
        Action::NextClipboard => action_clipboard_next(sr),
        _ => {
            sr.speech.speak("not implemented", false)?;
            Ok(false)
        }
    }
}

// Actions
fn action_stop(sr: &mut ScreenReader) -> Result<bool> {
    sr.speech.stop()?;
    Ok(false)
}

fn action_toggle_auto_read(sr: &mut ScreenReader) -> Result<bool> {
    if sr.auto_read {
        sr.auto_read = false;
        sr.speech.speak("auto read disabled", false)?;
    } else {
        sr.auto_read = true;
        sr.speech.speak("auto read enabled", false)?;
    }

    Ok(false)
}

fn action_toggle_review_cursor_follows_screen_cursor(sr: &mut ScreenReader) -> Result<bool> {
    sr.review_follows_screen_cursor = !sr.review_follows_screen_cursor;
    match sr.review_follows_screen_cursor {
        true => {
            sr.view.review_cursor_position = sr.view.screen().cursor_position();
            sr.speech
                .speak("review cursor following screen cursor", false)?;
        }
        false => sr
            .speech
            .speak("review cursor not following screen cursor", false)?,
    };
    Ok(false)
}

fn action_pass_next_key(sr: &mut ScreenReader) -> Result<bool> {
    sr.pass_through = true;
    sr.speech.speak("forward next key press", false)?;
    Ok(false)
}

fn action_toggle_help(sr: &mut ScreenReader) -> Result<bool> {
    if sr.help_mode {
        sr.help_mode = false;
        sr.speech.speak("exiting help", false)?;
    } else {
        sr.help_mode = true;
        sr.speech
            .speak("entering help. Press this key again to exit", false)?;
    }
    Ok(false)
}

fn action_review_line_prev(sr: &mut ScreenReader, skip_blank_lines: bool) -> Result<bool> {
    if !sr.view.review_cursor_up(skip_blank_lines) {
        sr.speech.speak("top", false)?;
    }
    action_review_line_read(sr)?;
    Ok(false)
}

fn action_review_line_next(sr: &mut ScreenReader, skip_blank_lines: bool) -> Result<bool> {
    if !sr.view.review_cursor_down(skip_blank_lines) {
        sr.speech.speak("bottom", false)?;
    }
    action_review_line_read(sr)?;
    Ok(false)
}

fn action_review_line_read(sr: &mut ScreenReader) -> Result<bool> {
    let row = sr.view.review_cursor_position.0;
    sr.report_review_cursor_indentation_changes()?;
    let line = sr
        .view
        .screen()
        .contents_between(row, 0, row, sr.view.size().1);
    if line.is_empty() {
        sr.speech.speak("blank", false)?;
    } else {
        sr.speech.speak(&line, false)?;
    }
    Ok(false)
}

fn action_review_word_prev(sr: &mut ScreenReader) -> Result<bool> {
    if !sr.view.review_cursor_prev_word() {
        sr.speech.speak("left", false)?;
    }
    action_review_word_read(sr)?;
    Ok(false)
}

fn action_review_word_next(sr: &mut ScreenReader) -> Result<bool> {
    if !sr.view.review_cursor_next_word() {
        sr.speech.speak("right", false)?;
    }
    action_review_word_read(sr)?;
    Ok(false)
}

fn action_review_word_read(sr: &mut ScreenReader) -> Result<bool> {
    let (row, col) = sr.view.review_cursor_position;
    let start = sr.view.screen().find_word_start(row, col);
    let end = sr.view.screen().find_word_end(row, col);

    let word = sr.view.screen().contents_between(row, start, row, end + 1);
    sr.speech.speak(&word, false)?;
    Ok(false)
}

fn action_review_char_prev(sr: &mut ScreenReader) -> Result<bool> {
    if !sr.view.review_cursor_left() {
        sr.speech.speak("left", false)?;
    }
    action_review_char_read(sr)?;
    Ok(false)
}

fn action_review_char_next(sr: &mut ScreenReader) -> Result<bool> {
    if !sr.view.review_cursor_right() {
        sr.speech.speak("right", false)?;
    }
    action_review_char_read(sr)?;
    Ok(false)
}

fn action_review_char_read(sr: &mut ScreenReader) -> Result<bool> {
    let (row, col) = sr.view.review_cursor_position;
    let char = sr
        .view
        .screen()
        .cell(row, col)
        .ok_or_else(|| anyhow!("cannot get cell at row {}, column {}", row, col))?
        .contents();
    if char.is_empty() {
        sr.speech.speak("blank", false)?;
    } else {
        sr.speech.speak(&char, false)?;
    }
    Ok(false)
}

fn action_review_char_read_phonetic(sr: &mut ScreenReader) -> Result<bool> {
    let (row, col) = sr.view.review_cursor_position;
    let char = sr
        .view
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
    sr.speech.speak(char, false)?;
    Ok(false)
}

fn action_review_top(sr: &mut ScreenReader) -> Result<bool> {
    let row = sr.view.review_cursor_position.0;
    let last_row = sr.view.size().0 - 1;
    let last_col = sr.view.size().1 - 1;
    sr.view.review_cursor_position.0 = match row {
        0 => sr
            .view
            .screen()
            .find_cell(CellExt::is_in_word, 0, 0, last_row, last_col)
            .map_or(0, |(row, _)| row),
        _ => 0,
    };
    action_review_line_read(sr)?;
    Ok(false)
}

fn action_review_bottom(sr: &mut ScreenReader) -> Result<bool> {
    let row = sr.view.review_cursor_position.0;
    let last_row = sr.view.size().0 - 1;
    let last_col = sr.view.size().1 - 1;
    sr.view.review_cursor_position.0 = if row == last_row {
        sr.view
            .screen()
            .rfind_cell(CellExt::is_in_word, 0, 0, last_row, last_col)
            .map_or(last_row, |(row, _)| row)
    } else {
        last_row
    };
    action_review_line_read(sr)?;
    Ok(false)
}

fn action_review_first(sr: &mut ScreenReader) -> Result<bool> {
    let (row, col) = sr.view.review_cursor_position;
    let last = sr.view.size().1 - 1;
    sr.view.review_cursor_position.1 = match col {
        0 => sr
            .view
            .screen()
            .find_cell(CellExt::is_in_word, row, 0, row, last)
            .map_or(0, |(_, col)| col),
        _ => 0,
    };
    action_review_char_read(sr)?;
    Ok(false)
}

fn action_review_last(sr: &mut ScreenReader) -> Result<bool> {
    let (row, col) = sr.view.review_cursor_position;
    let last = sr.view.size().1 - 1;
    sr.view.review_cursor_position.1 = if col == last {
        sr.view
            .screen()
            .rfind_cell(CellExt::is_in_word, row, 0, row, last)
            .map_or(last, |(_, col)| col)
    } else {
        last
    };
    action_review_char_read(sr)?;
    Ok(false)
}

fn action_review_read_attributes(sr: &mut ScreenReader) -> Result<bool> {
    let (row, col) = sr.view.review_cursor_position;
    let cell = sr
        .view
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
    Ok(false)
}

fn action_backspace(sr: &mut ScreenReader) -> Result<bool> {
    let (row, col) = sr.view.screen().cursor_position();
    if col > 0 {
        let char = sr
            .view
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
    Ok(true)
}

fn action_delete(sr: &mut ScreenReader) -> Result<bool> {
    let (row, col) = sr.view.screen().cursor_position();
    let char = sr
        .view
        .screen()
        .cell(row, col)
        .ok_or_else(|| anyhow!("cannot get cell at row {}, column {}", row, col))?
        .contents();
    sr.speech.speak(&char, false)?;
    Ok(true)
}

fn action_say_time(sr: &mut ScreenReader) -> Result<bool> {
    let date = chrono::Local::now();
    sr.speech
        .speak(&format!("{}", date.format("%H:%M")), false)?;
    Ok(false)
}

fn action_set_mark(sr: &mut ScreenReader) -> Result<bool> {
    sr.view.review_mark_position = Some(sr.view.review_cursor_position);
    sr.speech.speak("mark set", false)?;
    Ok(false)
}

fn action_copy(sr: &mut ScreenReader) -> Result<bool> {
    match sr.view.review_mark_position {
        Some((mark_row, mark_col)) => {
            let (cur_row, cur_col) = sr.view.review_cursor_position;
            if mark_row > cur_row || (mark_row == cur_row && mark_col > cur_col) {
                sr.speech.speak("mark is after the review cursor", false)?;
                return Ok(false);
            }

            let mut contents = String::new();
            for row in mark_row..=cur_row {
                let start = if row == mark_row { mark_col } else { 0 };
                // end is not inclusive, so that a blank row can be achieved with start == end.
                let end = if row == cur_row {
                    cur_col + 1
                } else {
                    sr.view.size().1
                };
                // Don't add trailing blank/whitespace cells
                let end = sr
                    .view
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
                        &sr.view
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
    Ok(false)
}

fn action_paste(sr: &mut ScreenReader, stream: &mut ptyprocess::stream::Stream) -> Result<bool> {
    match sr.clipboard.get() {
        Some(contents) => {
            if sr.view.screen().bracketed_paste() {
                write!(stream, "\x1B[200~{}\x1B[201~", contents)?;
            } else {
                write!(stream, "{}", contents)?;
            }
            sr.speech.speak("pasted", false)?;
        }
        None => sr.speech.speak("no clipboard", false)?,
    }
    Ok(false)
}

fn action_clipboard_prev(sr: &mut ScreenReader) -> Result<bool> {
    if sr.clipboard.size() == 0 {
        sr.speech.speak("no clipboard", false)?;
    } else if sr.clipboard.prev() {
        action_clipboard_say(sr)?;
    } else {
        sr.speech.speak("first clipboard", false)?;
    }
    Ok(false)
}

fn action_clipboard_next(sr: &mut ScreenReader) -> Result<bool> {
    if sr.clipboard.size() == 0 {
        sr.speech.speak("no clipboard", false)?;
    } else if sr.clipboard.next() {
        action_clipboard_say(sr)?;
    } else {
        sr.speech.speak("last clipboard", false)?;
    }
    Ok(false)
}

fn action_clipboard_say(sr: &mut ScreenReader) -> Result<bool> {
    match sr.clipboard.get() {
        Some(contents) => sr.speech.speak(contents, false)?,
        None => sr.speech.speak("no clipboard", false)?,
    }
    Ok(false)
}

fn action_toggle_symbol_level(sr: &mut ScreenReader) -> Result<bool> {
    use super::speech::symbols::Level;

    sr.speech.symbol_level = match sr.speech.symbol_level {
        Level::None => {
            sr.speech.speak("some", false)?;
            Level::Some
        }
        Level::Some => {
            sr.speech.speak("most", false)?;
            Level::Most
        }
        Level::Most => {
            sr.speech.speak("all", false)?;
            Level::All
        }
        Level::All | Level::Character => {
            sr.speech.speak("none", false)?;
            Level::None
        }
    };

    Ok(false)
}
