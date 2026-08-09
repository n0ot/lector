use super::{CommandResult, Result};
use crate::{
    ext::ScreenExt,
    screen_reader::{ClipboardMove, ScreenReader},
    view::View,
};

pub(super) fn set_mark(sr: &mut ScreenReader, view: &mut View) -> Result<CommandResult> {
    view.set_review_mark();
    sr.speak("mark set", false)?;
    Ok(CommandResult::Handled)
}

pub(super) fn copy(sr: &mut ScreenReader, view: &View) -> Result<CommandResult> {
    let Some(mark) = view.review_mark_position() else {
        sr.speak("no mark set", false)?;
        return Ok(CommandResult::Handled);
    };
    let Some(contents) = copy_selection(view, mark, view.review_cursor_position()) else {
        sr.speak("mark is after the review cursor", false)?;
        return Ok(CommandResult::Handled);
    };

    sr.push_clipboard(contents)?;
    sr.speak("copied", false)?;
    Ok(CommandResult::Handled)
}

fn copy_selection(
    view: &View,
    (mark_row, mark_col): (u16, u16),
    (cursor_row, cursor_col): (u16, u16),
) -> Option<String> {
    if (mark_row, mark_col) > (cursor_row, cursor_col) {
        return None;
    }

    let mut contents = String::new();
    for row in mark_row..=cursor_row {
        let start = if row == mark_row { mark_col } else { 0 };
        let end = if row == cursor_row {
            cursor_col + 1
        } else {
            view.size().1
        };
        let end = view
            .screen()
            .rfind_cell(
                |cell| !cell.contents().trim().is_empty(),
                row,
                start,
                row,
                end - 1,
            )
            .map_or(end, |(_, col)| col + 1);
        for col in start..end {
            contents.push_str(
                view.screen()
                    .cell(row, col)
                    .map_or("", vt100::Cell::contents),
            );
        }
        if row != cursor_row {
            contents.push('\n');
        }
    }
    Some(contents)
}

pub(super) fn paste(sr: &mut ScreenReader) -> Result<CommandResult> {
    match sr.clipboard_text() {
        Some(contents) => Ok(CommandResult::Paste(contents.to_owned())),
        None => {
            sr.speak("no clipboard", false)?;
            Ok(CommandResult::Handled)
        }
    }
}

pub(super) fn previous(sr: &mut ScreenReader) -> Result<CommandResult> {
    match sr.previous_clipboard()? {
        ClipboardMove::Empty => sr.speak("no clipboard", false)?,
        ClipboardMove::Boundary => sr.speak("first clipboard", false)?,
        ClipboardMove::Selected => {
            say(sr)?;
        }
    }
    Ok(CommandResult::Handled)
}

pub(super) fn next(sr: &mut ScreenReader) -> Result<CommandResult> {
    match sr.next_clipboard()? {
        ClipboardMove::Empty => sr.speak("no clipboard", false)?,
        ClipboardMove::Boundary => sr.speak("last clipboard", false)?,
        ClipboardMove::Selected => {
            say(sr)?;
        }
    }
    Ok(CommandResult::Handled)
}

pub(super) fn say(sr: &mut ScreenReader) -> Result<CommandResult> {
    let contents = sr.clipboard_text().map(str::to_owned);
    sr.speak(contents.as_deref().unwrap_or("no clipboard"), false)?;
    Ok(CommandResult::Handled)
}

#[cfg(test)]
mod tests {
    use super::{copy, copy_selection, next, paste, previous, say, set_mark};
    use crate::{
        commands::CommandResult,
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
    fn selection_copy_preserves_lines_and_trims_trailing_cells() {
        let mut view = View::new(3, 12);
        view.process_changes(b"one\r\ntwo");

        assert_eq!(
            copy_selection(&view, (0, 0), (1, 11)),
            Some("one\ntwo".into())
        );
    }

    #[test]
    fn selection_copy_rejects_a_mark_after_the_cursor() {
        let view = View::new(3, 12);
        assert_eq!(copy_selection(&view, (2, 0), (1, 0)), None);
        assert_eq!(copy_selection(&view, (1, 4), (1, 3)), None);
    }

    #[test]
    fn mark_copy_and_paste_round_trip_the_selected_text() {
        let (mut sr, output) = screen_reader();
        let mut view = View::new(2, 8);
        view.process_changes(b"abc");

        set_mark(&mut sr, &mut view).unwrap();
        view.set_review_cursor_col(2);
        copy(&mut sr, &view).unwrap();

        let CommandResult::Paste(contents) = paste(&mut sr).unwrap() else {
            panic!("expected paste contents");
        };
        assert_eq!(contents, "abc");
        assert_eq!(output.borrow().as_slice(), ["mark set", "copied"]);
    }

    #[test]
    fn clipboard_commands_report_empty_selection_and_both_boundaries() {
        let (mut sr, output) = screen_reader();

        assert!(matches!(paste(&mut sr).unwrap(), CommandResult::Handled));
        previous(&mut sr).unwrap();
        next(&mut sr).unwrap();
        say(&mut sr).unwrap();
        assert_eq!(output.borrow().as_slice(), ["no clipboard"; 4]);

        output.borrow_mut().clear();
        sr.push_clipboard("first".into()).unwrap();
        sr.push_clipboard("second".into()).unwrap();
        next(&mut sr).unwrap();
        next(&mut sr).unwrap();
        previous(&mut sr).unwrap();
        previous(&mut sr).unwrap();
        assert_eq!(
            output.borrow().as_slice(),
            ["first", "last clipboard", "second", "first clipboard"]
        );
    }

    #[test]
    fn copy_reports_missing_and_reversed_marks() {
        let (mut sr, output) = screen_reader();
        let mut view = View::new(2, 8);
        view.process_changes(b"abc");

        copy(&mut sr, &view).unwrap();
        view.set_review_cursor_col(2);
        set_mark(&mut sr, &mut view).unwrap();
        view.set_review_cursor_col(0);
        copy(&mut sr, &view).unwrap();

        assert_eq!(
            output.borrow().as_slice(),
            ["no mark set", "mark set", "mark is after the review cursor"]
        );
    }
}
