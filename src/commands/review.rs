use super::{CommandResult, Error, Result};
use crate::{
    attributes,
    ext::{CellExt, ScreenExt},
    screen_reader::ScreenReader,
    view::View,
};

pub(super) fn report_move(
    sr: &mut ScreenReader,
    view: &mut View,
    old_position: (u16, u16),
) -> Result<()> {
    if old_position != view.review_cursor_position() {
        view.cancel_pending_screen_transition_follow();
    }
    sr.hook_on_review_cursor_move(old_position, view.review_cursor_position())?;
    Ok(())
}

pub(super) fn line_previous(
    sr: &mut ScreenReader,
    view: &mut View,
    skip_blank_lines: bool,
) -> Result<CommandResult> {
    let old_position = view.review_cursor_position();
    if !view.review_cursor_up(skip_blank_lines) {
        sr.speak("top", false)?;
    }
    report_move(sr, view, old_position)?;
    line_read(sr, view)?;
    Ok(CommandResult::Handled)
}

pub(super) fn line_next(
    sr: &mut ScreenReader,
    view: &mut View,
    skip_blank_lines: bool,
) -> Result<CommandResult> {
    let old_position = view.review_cursor_position();
    if !view.review_cursor_down(skip_blank_lines) {
        sr.speak("bottom", false)?;
    }
    report_move(sr, view, old_position)?;
    line_read(sr, view)?;
    Ok(CommandResult::Handled)
}

pub(super) fn line_read(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let row = view.review_cursor_position().0;
    sr.report_review_cursor_indentation_changes(view)?;
    let line = view.line(row);
    if !line.trim().is_empty() {
        sr.speak(&line, false)?;
    }
    Ok(CommandResult::Handled)
}

pub(super) fn word_previous(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let old_position = view.review_cursor_position();
    if !view.review_cursor_prev_word() {
        sr.speak("left", false)?;
    }
    report_move(sr, view, old_position)?;
    word_read(sr, view)?;
    Ok(CommandResult::Handled)
}

pub(super) fn word_next(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let old_position = view.review_cursor_position();
    if !view.review_cursor_next_word() {
        sr.speak("right", false)?;
    }
    report_move(sr, view, old_position)?;
    word_read(sr, view)?;
    Ok(CommandResult::Handled)
}

pub(super) fn word_read(sr: &mut ScreenReader, view: &View) -> Result<CommandResult> {
    let (row, col) = view.review_cursor_position();
    sr.speak(&view.word(row, col), false)?;
    Ok(CommandResult::Handled)
}

pub(super) fn character_previous(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let old_position = view.review_cursor_position();
    if !view.review_cursor_left() {
        sr.speak("left", false)?;
    }
    report_move(sr, view, old_position)?;
    character_read(sr, view)?;
    Ok(CommandResult::Handled)
}

pub(super) fn character_next(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let old_position = view.review_cursor_position();
    if !view.review_cursor_right() {
        sr.speak("right", false)?;
    }
    report_move(sr, view, old_position)?;
    character_read(sr, view)?;
    Ok(CommandResult::Handled)
}

pub(super) fn character_read(sr: &mut ScreenReader, view: &View) -> Result<CommandResult> {
    let (row, col) = view.review_cursor_position();
    let character = view.character(row, col);
    if !character.trim().is_empty() {
        sr.speak(&character, false)?;
    }
    Ok(CommandResult::Handled)
}

pub(super) fn character_read_phonetic(sr: &mut ScreenReader, view: &View) -> Result<CommandResult> {
    let (row, col) = view.review_cursor_position();
    let character = view.character(row, col);
    if !character.trim().is_empty() {
        sr.speak(phonetic_name(&character).unwrap_or(&character), false)?;
    }
    Ok(CommandResult::Handled)
}

fn phonetic_name(character: &str) -> Option<&'static str> {
    match character.as_bytes() {
        [b'a' | b'A'] => Some("Alpha"),
        [b'b' | b'B'] => Some("Bravo"),
        [b'c' | b'C'] => Some("Charlie"),
        [b'd' | b'D'] => Some("Delta"),
        [b'e' | b'E'] => Some("Echo"),
        [b'f' | b'F'] => Some("Foxtrot"),
        [b'g' | b'G'] => Some("Golf"),
        [b'h' | b'H'] => Some("Hotel"),
        [b'i' | b'I'] => Some("India"),
        [b'j' | b'J'] => Some("Juliett"),
        [b'k' | b'K'] => Some("Kilo"),
        [b'l' | b'L'] => Some("Lima"),
        [b'm' | b'M'] => Some("Mike"),
        [b'n' | b'N'] => Some("November"),
        [b'o' | b'O'] => Some("Oscar"),
        [b'p' | b'P'] => Some("Papa"),
        [b'q' | b'Q'] => Some("Quebec"),
        [b'r' | b'R'] => Some("Romeo"),
        [b's' | b'S'] => Some("Sierra"),
        [b't' | b'T'] => Some("Tango"),
        [b'u' | b'U'] => Some("Uniform"),
        [b'v' | b'V'] => Some("Victor"),
        [b'w' | b'W'] => Some("Whiskey"),
        [b'x' | b'X'] => Some("X-ray"),
        [b'y' | b'Y'] => Some("Yankee"),
        [b'z' | b'Z'] => Some("Zulu"),
        _ => None,
    }
}

pub(super) fn top(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let old_position = view.review_cursor_position();
    let row = view.review_cursor_position().0;
    let last_row = view.size().0 - 1;
    let last_col = view.size().1 - 1;
    let target_row = match row {
        0 => view
            .screen()
            .find_cell(CellExt::is_in_word, 0, 0, last_row, last_col)
            .map_or(0, |(row, _)| row),
        _ => 0,
    };
    view.set_review_cursor_row(target_row);
    report_move(sr, view, old_position)?;
    line_read(sr, view)?;
    Ok(CommandResult::Handled)
}

pub(super) fn bottom(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let old_position = view.review_cursor_position();
    let row = view.review_cursor_position().0;
    let last_row = view.size().0 - 1;
    let last_col = view.size().1 - 1;
    let target_row = if row == last_row {
        view.screen()
            .rfind_cell(CellExt::is_in_word, 0, 0, last_row, last_col)
            .map_or(last_row, |(row, _)| row)
    } else {
        last_row
    };
    view.set_review_cursor_row(target_row);
    report_move(sr, view, old_position)?;
    line_read(sr, view)?;
    Ok(CommandResult::Handled)
}

pub(super) fn first(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let old_position = view.review_cursor_position();
    let (row, col) = view.review_cursor_position();
    let last = view.size().1 - 1;
    let target_col = match col {
        0 => view
            .screen()
            .find_cell(CellExt::is_in_word, row, 0, row, last)
            .map_or(0, |(_, col)| col),
        _ => 0,
    };
    view.set_review_cursor_col(target_col);
    report_move(sr, view, old_position)?;
    character_read(sr, view)?;
    Ok(CommandResult::Handled)
}

pub(super) fn last(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    let old_position = view.review_cursor_position();
    let (row, col) = view.review_cursor_position();
    let last = view.size().1 - 1;
    let target_col = if col == last {
        view.screen()
            .rfind_cell(CellExt::is_in_word, row, 0, row, last)
            .map_or(last, |(_, col)| col)
    } else {
        last
    };
    view.set_review_cursor_col(target_col);
    report_move(sr, view, old_position)?;
    character_read(sr, view)?;
    Ok(CommandResult::Handled)
}

pub(super) fn read_attributes(sr: &mut ScreenReader, view: &View) -> Result<CommandResult> {
    let (row, col) = view.review_cursor_position();
    let cell = view
        .screen()
        .cell(row, col)
        .ok_or(Error::MissingCell { row, col })?;

    let mut attrs = String::new();
    attrs.push_str(&format!("Row {} col {} ", row + 1, col + 1));
    attrs.push_str(&format!(
        "{} {}",
        attributes::describe_color(cell.fgcolor()),
        if let crate::terminal::Color::Default = cell.bgcolor() {
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

#[cfg(test)]
mod tests {
    use super::{
        bottom, character_next, character_previous, character_read, character_read_phonetic, first,
        last, line_next, line_previous, line_read, phonetic_name, read_attributes, top, word_next,
        word_previous,
    };
    use crate::{
        commands::Error,
        screen_reader::ScreenReader,
        speech::{self},
        view::View,
    };
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

    #[test]
    fn line_read_preserves_renderer_padding_as_silence() {
        let (mut sr, output) = screen_reader();
        let mut view = View::new(1, 8);
        view.process_changes(b"        ");

        line_read(&mut sr, &mut view).unwrap();

        assert!(output.borrow().is_empty());
    }

    #[test]
    fn phonetic_names_cover_ascii_letters_case_insensitively() {
        assert_eq!(phonetic_name("a"), Some("Alpha"));
        assert_eq!(phonetic_name("J"), Some("Juliett"));
        assert_eq!(phonetic_name("z"), Some("Zulu"));
        assert_eq!(phonetic_name("?"), None);
        assert_eq!(phonetic_name("é"), None);
    }

    #[test]
    fn line_navigation_skips_blanks_and_announces_boundaries() {
        let (mut sr, output) = screen_reader();
        let mut view = View::new(4, 10);
        view.process_changes(b"top\r\n\r\nbottom");

        line_previous(&mut sr, &mut view, true).unwrap();
        assert_eq!(view.review_cursor_position(), (0, 0));
        line_next(&mut sr, &mut view, true).unwrap();
        assert_eq!(view.review_cursor_position(), (2, 0));
        line_next(&mut sr, &mut view, true).unwrap();
        assert_eq!(view.review_cursor_position(), (2, 0));

        assert_eq!(
            output.borrow().as_slice(),
            ["top", "top", "bottom", "bottom", "bottom"]
        );

        output.borrow_mut().clear();
        view.set_review_cursor_position((0, 0));
        line_next(&mut sr, &mut view, false).unwrap();
        assert_eq!(view.review_cursor_position(), (1, 0));
        assert!(output.borrow().is_empty());
    }

    #[test]
    fn word_and_character_navigation_cover_edges_and_phonetics() {
        let (mut sr, output) = screen_reader();
        let mut view = View::new(1, 10);
        view.process_changes(b"a  beta");

        word_previous(&mut sr, &mut view).unwrap();
        word_next(&mut sr, &mut view).unwrap();
        word_next(&mut sr, &mut view).unwrap();
        view.set_review_cursor_col(0);
        character_previous(&mut sr, &mut view).unwrap();
        character_next(&mut sr, &mut view).unwrap();
        character_read_phonetic(&mut sr, &view).unwrap();
        view.set_review_cursor_col(9);
        character_read(&mut sr, &view).unwrap();

        assert_eq!(view.review_cursor_position(), (0, 9));
        assert_eq!(
            output.borrow().as_slice(),
            ["left", "a", "beta", "right", "beta", "left", "a"]
        );
    }

    #[test]
    fn absolute_navigation_toggles_between_edges_and_content() {
        let (mut sr, _output) = screen_reader();
        let mut view = View::new(5, 8);
        view.process_changes(b"\r\n  one\r\n\r\n x");

        top(&mut sr, &mut view).unwrap();
        assert_eq!(view.review_cursor_position().0, 1);
        top(&mut sr, &mut view).unwrap();
        assert_eq!(view.review_cursor_position().0, 0);
        bottom(&mut sr, &mut view).unwrap();
        assert_eq!(view.review_cursor_position().0, 4);
        bottom(&mut sr, &mut view).unwrap();
        assert_eq!(view.review_cursor_position().0, 3);

        view.set_review_cursor_position((1, 0));
        first(&mut sr, &mut view).unwrap();
        assert_eq!(view.review_cursor_position().1, 2);
        first(&mut sr, &mut view).unwrap();
        assert_eq!(view.review_cursor_position().1, 0);
        last(&mut sr, &mut view).unwrap();
        assert_eq!(view.review_cursor_position().1, 7);
        last(&mut sr, &mut view).unwrap();
        assert_eq!(view.review_cursor_position().1, 4);
    }

    #[test]
    fn attribute_reading_reports_styles_and_invalid_coordinates() {
        let (mut sr, output) = screen_reader();
        let mut view = View::new(2, 8);
        view.process_changes(b"\x1B[1;3;4;7;38;5;9;48;5;12mA");

        read_attributes(&mut sr, &view).unwrap();
        let spoken = output.borrow();
        let attributes = spoken.last().unwrap();
        assert!(attributes.contains("Row 1 col 1"));
        assert!(attributes.contains("Red on Blue"));
        assert!(attributes.contains("bold"));
        assert!(attributes.contains("italic"));
        assert!(attributes.contains("underline"));
        assert!(attributes.contains("inverse"));
        drop(spoken);

        view.set_review_cursor_position((9, 9));
        let Err(error) = read_attributes(&mut sr, &view) else {
            panic!("expected missing cell error");
        };
        assert!(matches!(error, Error::MissingCell { row: 9, col: 9 }));
    }
}
