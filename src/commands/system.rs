use super::{CommandResult, Result};
use crate::{screen_reader::ScreenReader, view::View};

pub(super) fn stop(sr: &mut ScreenReader) -> Result<CommandResult> {
    sr.stop_speaking()?;
    Ok(CommandResult::Handled)
}

pub(super) fn toggle_auto_read(sr: &mut ScreenReader) -> Result<CommandResult> {
    let enabled = sr.toggle_auto_read();
    sr.speak(
        if enabled {
            "auto read enabled"
        } else {
            "auto read disabled"
        },
        false,
    )?;
    Ok(CommandResult::Handled)
}

pub(super) fn toggle_stop_speech_on_focus_loss(sr: &mut ScreenReader) -> Result<CommandResult> {
    let status = if sr.toggle_stop_speech_on_focus_loss() {
        "enabled"
    } else {
        "disabled"
    };
    sr.speak(&format!("stop on focus loss {status}"), false)?;
    Ok(CommandResult::Handled)
}

pub(super) fn say_overlay(sr: &mut ScreenReader, title: &str) -> Result<CommandResult> {
    sr.speak(title, false)?;
    Ok(CommandResult::Handled)
}

pub(super) fn toggle_review_follows_screen_cursor(
    sr: &mut ScreenReader,
    view: &mut View,
) -> Result<CommandResult> {
    if sr.toggle_review_follows_screen_cursor() {
        let old_position = view.review_cursor_position();
        view.set_review_cursor_position(view.screen().cursor_position());
        sr.hook_on_review_cursor_move(old_position, view.review_cursor_position())?;
        sr.speak("review cursor following screen cursor", false)?;
    } else {
        sr.speak("review cursor not following screen cursor", false)?;
    }
    Ok(CommandResult::Handled)
}

pub(super) fn pass_next_key(sr: &mut ScreenReader) -> Result<CommandResult> {
    sr.request_pass_through();
    sr.speak("forward next key press", false)?;
    Ok(CommandResult::Handled)
}

pub(super) fn toggle_help(sr: &mut ScreenReader) -> Result<CommandResult> {
    let enabled = sr.toggle_help_mode();
    sr.speak(
        if enabled {
            "entering help. Press this key again to exit"
        } else {
            "exiting help"
        },
        false,
    )?;
    Ok(CommandResult::Handled)
}

pub(super) fn backspace(sr: &mut ScreenReader, view: &View) -> Result<CommandResult> {
    sr.defer_backspace(view);
    sr.suppress_cursor_tracking_once();
    Ok(CommandResult::ForwardInput)
}

pub(super) fn delete(sr: &mut ScreenReader, view: &View) -> Result<CommandResult> {
    sr.defer_delete(view);
    Ok(CommandResult::ForwardInput)
}

pub(super) fn say_time(sr: &mut ScreenReader) -> Result<CommandResult> {
    let date = chrono::Local::now();
    sr.speak(&date.format("%H:%M").to_string(), false)?;
    Ok(CommandResult::Handled)
}

pub(super) fn toggle_symbol_level(sr: &mut ScreenReader) -> Result<CommandResult> {
    let level = sr.speech_mut().cycle_symbol_level();
    sr.speak(&level.to_string(), false)?;
    Ok(CommandResult::Handled)
}

#[cfg(test)]
mod tests {
    use super::{
        backspace, delete, pass_next_key, say_overlay, say_time, stop, toggle_auto_read,
        toggle_help, toggle_review_follows_screen_cursor, toggle_stop_speech_on_focus_loss,
        toggle_symbol_level,
    };
    use crate::{
        commands::CommandResult,
        screen_reader::ScreenReader,
        speech::{self, symbols::Level},
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
    fn boolean_toggles_round_trip_with_stable_announcements() {
        let (mut sr, output) = screen_reader();

        toggle_auto_read(&mut sr).unwrap();
        toggle_auto_read(&mut sr).unwrap();
        toggle_help(&mut sr).unwrap();
        toggle_help(&mut sr).unwrap();

        assert!(sr.auto_read_enabled());
        assert!(!sr.help_mode());
        assert_eq!(
            output.borrow().as_slice(),
            [
                "auto read disabled",
                "auto read enabled",
                "entering help. Press this key again to exit",
                "exiting help",
            ]
        );
    }

    #[test]
    fn symbol_level_cycles_through_every_public_level() {
        let (mut sr, output) = screen_reader();

        for _ in 0..4 {
            toggle_symbol_level(&mut sr).unwrap();
        }

        assert!(sr.speech().symbol_level() == Level::Some);
        assert_eq!(output.borrow().as_slice(), ["most", "all", "none", "some"]);
    }

    #[test]
    fn system_commands_update_flags_delegate_stop_and_announce_state() {
        let (mut sr, output) = screen_reader();

        assert!(matches!(stop(&mut sr).unwrap(), CommandResult::Handled));
        assert!(matches!(
            say_overlay(&mut sr, "Terminal").unwrap(),
            CommandResult::Handled
        ));
        assert!(matches!(
            pass_next_key(&mut sr).unwrap(),
            CommandResult::Handled
        ));
        assert!(sr.take_pass_through());
        assert!(!sr.take_pass_through());

        toggle_stop_speech_on_focus_loss(&mut sr).unwrap();
        assert!(!sr.stop_speech_on_focus_loss());
        toggle_stop_speech_on_focus_loss(&mut sr).unwrap();
        assert!(sr.stop_speech_on_focus_loss());

        assert_eq!(
            output.borrow().as_slice(),
            [
                "Terminal",
                "forward next key press",
                "stop on focus loss disabled",
                "stop on focus loss enabled",
            ]
        );
    }

    #[test]
    fn review_follow_toggle_snaps_to_application_cursor_only_when_enabled() {
        let (mut sr, output) = screen_reader();
        let mut view = View::new(3, 8);
        view.process_changes(b"\x1B[2;4H");
        sr.set_review_follows_screen_cursor(false);

        toggle_review_follows_screen_cursor(&mut sr, &mut view).unwrap();
        assert!(sr.review_follows_screen_cursor());
        assert_eq!(view.review_cursor_position(), (1, 3));

        view.set_review_cursor_position((0, 0));
        toggle_review_follows_screen_cursor(&mut sr, &mut view).unwrap();
        assert!(!sr.review_follows_screen_cursor());
        assert_eq!(view.review_cursor_position(), (0, 0));
        assert_eq!(
            output.borrow().as_slice(),
            [
                "review cursor following screen cursor",
                "review cursor not following screen cursor",
            ]
        );
    }

    #[test]
    fn editing_commands_forward_input_and_time_has_a_stable_shape() {
        let (mut sr, output) = screen_reader();
        let view = View::new(2, 4);

        assert!(matches!(
            backspace(&mut sr, &view).unwrap(),
            CommandResult::ForwardInput
        ));
        assert!(matches!(
            delete(&mut sr, &view).unwrap(),
            CommandResult::ForwardInput
        ));
        say_time(&mut sr).unwrap();

        let spoken = output.borrow();
        let time = spoken.last().unwrap();
        assert_eq!(time.len(), 5);
        assert_eq!(time.as_bytes()[2], b':');
        assert!(
            time.bytes()
                .enumerate()
                .all(|(idx, byte)| idx == 2 || byte.is_ascii_digit())
        );
    }
}
