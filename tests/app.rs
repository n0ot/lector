use lector::{
    app::{App, Clock, DIFF_DELAY},
    screen_reader::ScreenReader,
    speech, views,
};
use std::{cell::Cell, cell::RefCell, rc::Rc};

#[derive(Default)]
struct RecorderState {
    speaks: Vec<(String, bool)>,
    stops: usize,
    rate: f32,
}

#[derive(Clone, Default)]
struct Recorder {
    inner: Rc<RefCell<RecorderState>>,
}

struct FakeDriver {
    recorder: Recorder,
}

impl speech::Driver for FakeDriver {
    fn speak(&mut self, text: &str, interrupt: bool) -> anyhow::Result<()> {
        self.recorder
            .inner
            .borrow_mut()
            .speaks
            .push((text.to_string(), interrupt));
        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        self.recorder.inner.borrow_mut().stops += 1;
        Ok(())
    }

    fn get_rate(&self) -> f32 {
        self.recorder.inner.borrow().rate
    }

    fn set_rate(&mut self, rate: f32) -> anyhow::Result<()> {
        self.recorder.inner.borrow_mut().rate = rate;
        Ok(())
    }
}

#[derive(Clone, Default)]
struct FakeClock {
    now: Rc<Cell<u128>>,
}

impl FakeClock {
    fn advance_ms(&self, delta: u128) {
        self.now.set(self.now.get().saturating_add(delta));
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u128 {
        self.now.get()
    }
}

fn make_app() -> (App, ScreenReader, Recorder, FakeClock) {
    let recorder = Recorder::default();
    let driver = FakeDriver {
        recorder: recorder.clone(),
    };
    let speech = speech::Speech::new(Box::new(driver));
    let screen_reader = ScreenReader::new(speech);
    let view_stack = views::ViewStack::new(Box::new(views::PtyView::new(24, 80)));
    let clock = FakeClock::default();
    let app = App::new_with_clock(view_stack, Box::new(clock.clone())).expect("create app");
    (app, screen_reader, recorder, clock)
}

#[test]
fn stdin_unmapped_forwards_to_pty() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"a", &mut pty_out, &mut term_out)
        .expect("handle stdin");

    assert_eq!(pty_out, b"a");
    assert!(term_out.is_empty());
    assert_eq!(sr.last_key, b"a");
    assert_eq!(recorder.inner.borrow().stops, 1);
}

#[test]
fn paste_writes_to_pty_and_speaks() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    sr.clipboard.put("hello".to_string());
    app.handle_stdin(&mut sr, b"\x1B[18~", &mut pty_out, &mut term_out)
        .expect("handle stdin");

    assert_eq!(pty_out, b"hello");
    let speaks = &recorder.inner.borrow().speaks;
    assert!(speaks.iter().any(|(text, _)| text == "pasted"));
}

#[test]
fn click_actions_write_mouse_events_at_review_cursor() {
    let (mut app, mut sr, _recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"\x1B[?1000h\x1B[?1006h\x1B[5;8H", &mut term_out)
        .expect("enable mouse and position cursor");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).expect("finalize"));

    app.handle_stdin(&mut sr, b"\x1B{", &mut pty_out, &mut term_out)
        .expect("left click");
    app.handle_stdin(&mut sr, b"\x1B}", &mut pty_out, &mut term_out)
        .expect("right click");

    assert_eq!(pty_out, b"\x1B[<0;8;5M\x1B[<0;8;5m\x1B[<2;8;5M\x1B[<2;8;5m");
}

#[test]
fn click_without_mouse_reporting_is_not_forwarded() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1B{", &mut pty_out, &mut term_out)
        .expect("click without mouse reporting");

    assert!(pty_out.is_empty());
    assert!(
        recorder
            .inner
            .borrow()
            .speaks
            .iter()
            .any(|(text, _)| text == "mouse input unavailable")
    );
}

#[test]
fn pty_output_writes_terminal_and_autoreads() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"hello\r\n", &mut term_out)
        .expect("handle pty");
    assert_eq!(term_out, b"hello\r\n");

    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    let _ = app.maybe_finalize_changes(&mut sr).expect("finalize");

    let speaks = &recorder.inner.borrow().speaks;
    assert!(speaks.iter().any(|(text, _)| text.contains("hello")));
}

#[test]
fn screen_stabilization_uses_first_and_last_update_timers() {
    let (mut app, mut sr, _recorder, clock) = make_app();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"a", &mut term_out)
        .expect("first update");
    clock.advance_ms(20);
    app.handle_pty(&mut sr, b"b", &mut term_out)
        .expect("second update");

    clock.advance_ms(29);
    assert!(!app.maybe_finalize_changes(&mut sr).expect("still changing"));

    clock.advance_ms(1);
    assert!(app.maybe_finalize_changes(&mut sr).expect("stable"));
}

#[test]
fn idle_time_before_a_new_batch_does_not_trigger_the_maximum_delay() {
    let (mut app, mut sr, _recorder, clock) = make_app();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"a", &mut term_out)
        .expect("initial update");
    clock.advance_ms(u128::from(DIFF_DELAY));
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize initial update")
    );

    clock.advance_ms(1_000);
    app.handle_pty(&mut sr, b"b", &mut term_out)
        .expect("new update after idle");
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("new batch should not finalize immediately")
    );

    clock.advance_ms(u128::from(DIFF_DELAY));
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("new batch becomes stable")
    );
}

#[test]
fn maximum_delay_finalizes_continuously_changing_output() {
    let (mut app, mut sr, _recorder, clock) = make_app();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"a", &mut term_out)
        .expect("first update");
    for update in 1..=15 {
        clock.advance_ms(20);
        app.handle_pty(&mut sr, b"b", &mut term_out)
            .expect("continuous update");
        let finalized = app
            .maybe_finalize_changes(&mut sr)
            .expect("check maximum delay");
        assert_eq!(finalized, update == 15);
    }
}

#[test]
fn cursor_tracking_wins_over_single_line_ruler_change() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(
        &mut sr,
        b"\x1B[2J\x1B[1;1Hfirst line\x1B[2;1Hsecond line\x1B[4;1Hfile 1,1 All\x1B[1;1H",
        &mut term_out,
    )
    .expect("draw initial screen");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    app.maybe_finalize_changes(&mut sr)
        .expect("finalize initial screen");
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"j", &mut pty_out, &mut term_out)
        .expect("move cursor down");
    app.handle_pty(
        &mut sr,
        b"\x1B[2J\x1B[1;1Hfirst line\x1B[2;1Hsecond line\x1B[4;1Hfile 2,1 All\x1B[2;1H",
        &mut term_out,
    )
    .expect("draw updated screen");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    app.maybe_finalize_changes(&mut sr)
        .expect("finalize cursor move");

    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        [("second line".into(), false)]
    );
}

#[test]
fn cursor_tracking_stays_silent_on_blank_line_during_ruler_change() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(
        &mut sr,
        b"\x1B[2J\x1B[1;1Hfirst line\x1B[2;1H \x1B[4;1Hfile 1,1 All\x1B[1;1H",
        &mut term_out,
    )
    .expect("draw initial screen");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    app.maybe_finalize_changes(&mut sr)
        .expect("finalize initial screen");
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"j", &mut pty_out, &mut term_out)
        .expect("move cursor down");
    app.handle_pty(
        &mut sr,
        b"\x1B[2J\x1B[1;1Hfirst line\x1B[2;1H \x1B[4;1Hfile 2,1 All\x1B[2;1H",
        &mut term_out,
    )
    .expect("draw updated screen");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    app.maybe_finalize_changes(&mut sr)
        .expect("finalize cursor move");

    assert!(recorder.inner.borrow().speaks.is_empty());
}

#[test]
fn printed_line_wins_when_cursor_moves_past_it_to_a_blank_line() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"\x1B[1;1Hworking...\x1B[1;1H", &mut term_out)
        .expect("draw initial screen");
    clock.advance_ms(u128::from(DIFF_DELAY));
    app.maybe_finalize_changes(&mut sr)
        .expect("finalize initial screen");
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\r", &mut pty_out, &mut term_out)
        .expect("submit input");
    app.handle_pty(
        &mut sr,
        b"\x1B[1;1H\x1B[2Kprinted line\x1B[2;1H",
        &mut term_out,
    )
    .expect("print line and move cursor below it");
    clock.advance_ms(u128::from(DIFF_DELAY));
    app.maybe_finalize_changes(&mut sr)
        .expect("finalize printed output");

    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        [("printed line".into(), false)]
    );
}

#[test]
fn split_alt_sequence_maps_to_action() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1B", &mut pty_out, &mut term_out)
        .expect("handle stdin");
    assert!(pty_out.is_empty());

    app.handle_stdin(&mut sr, b"l", &mut pty_out, &mut term_out)
        .expect("handle stdin");

    assert!(pty_out.is_empty());
    assert_eq!(sr.last_key, b"\x1Bl");
    assert!(!recorder.inner.borrow().speaks.is_empty());
}

#[test]
fn kitty_meta_key_interrupts_speech() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1B[117;3u", &mut pty_out, &mut term_out)
        .expect("handle Kitty Meta-u");

    assert_eq!(recorder.inner.borrow().stops, 1);
    assert_eq!(sr.last_key, b"\x1B[117;3u");
}

#[test]
fn kitty_control_key_interrupts_speech() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1B[108;5u", &mut pty_out, &mut term_out)
        .expect("handle Kitty Control-l");

    assert_eq!(recorder.inner.borrow().stops, 1);
    assert_eq!(sr.last_key, b"\x1B[108;5u");
}

#[test]
fn alt_bracket_maps_after_timeout() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1B[", &mut pty_out, &mut term_out)
        .expect("handle stdin");
    assert!(pty_out.is_empty());

    clock.advance_ms(100);
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .expect("handle tick");

    assert!(pty_out.is_empty());
    assert_eq!(sr.last_key, b"\x1B[");
    let speaks = &recorder.inner.borrow().speaks;
    assert!(speaks.iter().any(|(text, _)| text == "no clipboard"));
}

#[test]
fn alt_close_bracket_maps_after_timeout() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1B]", &mut pty_out, &mut term_out)
        .expect("handle stdin");
    assert!(pty_out.is_empty());

    clock.advance_ms(100);
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .expect("handle tick");

    assert!(pty_out.is_empty());
    assert_eq!(sr.last_key, b"\x1B]");
    let speaks = &recorder.inner.borrow().speaks;
    assert!(speaks.iter().any(|(text, _)| text == "no clipboard"));
}

#[test]
fn osc_sequence_forwards_to_pty() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    let osc = b"\x1B]0;lector test\x07";
    app.handle_stdin(&mut sr, osc, &mut pty_out, &mut term_out)
        .expect("handle stdin");

    assert_eq!(pty_out, osc);
    assert!(term_out.is_empty());
    assert_eq!(sr.last_key, osc);
    assert_eq!(recorder.inner.borrow().stops, 1);
}

#[test]
fn osc_sequence_with_st_terminator_forwards_to_pty() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    let osc = b"\x1B]0;lector test\x1B\\";
    app.handle_stdin(&mut sr, osc, &mut pty_out, &mut term_out)
        .expect("handle stdin");

    assert_eq!(pty_out, osc);
    assert!(term_out.is_empty());
    assert_eq!(sr.last_key, osc);
    assert_eq!(recorder.inner.borrow().stops, 1);
}

#[test]
fn help_mode_can_toggle_off() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    sr.help_mode = true;
    app.handle_stdin(&mut sr, b"\x1BOP", &mut pty_out, &mut term_out)
        .expect("handle stdin");

    assert!(!sr.help_mode);
}

#[test]
fn focus_events_not_forwarded_without_app_request() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1B[O", &mut pty_out, &mut term_out)
        .expect("handle stdin");

    assert!(pty_out.is_empty());
    assert!(!sr.terminal_focused);
    assert_eq!(recorder.inner.borrow().stops, 1);
}

#[test]
fn focus_events_forwarded_after_app_enables_them() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"\x1B[?1004h", &mut term_out)
        .expect("handle pty");
    assert!(term_out.is_empty());

    app.handle_stdin(&mut sr, b"\x1B[I", &mut pty_out, &mut term_out)
        .expect("handle stdin");

    assert_eq!(pty_out, b"\x1B[I");
    assert!(sr.terminal_focused);
}

#[test]
fn focus_mode_sequences_are_filtered_from_terminal_output() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut term_out = Vec::new();
    let mut pty_out = Vec::new();

    app.handle_pty(&mut sr, b"x\x1B[?10", &mut term_out)
        .expect("handle pty");
    assert_eq!(term_out, b"x");

    app.handle_pty(&mut sr, b"04hy", &mut term_out)
        .expect("handle pty");
    assert_eq!(term_out, b"xy");

    app.handle_stdin(&mut sr, b"\x1B[I", &mut pty_out, &mut term_out)
        .expect("handle stdin");
    assert_eq!(pty_out, b"\x1B[I");

    app.handle_pty(&mut sr, b"\x1B[?1004l", &mut term_out)
        .expect("handle pty");
    assert_eq!(term_out, b"xy");

    app.handle_stdin(&mut sr, b"\x1B[O", &mut pty_out, &mut term_out)
        .expect("handle stdin");
    assert_eq!(pty_out, b"\x1B[I");
}

#[test]
fn auto_read_does_not_speak_when_terminal_unfocused() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1B[O", &mut pty_out, &mut term_out)
        .expect("handle stdin");
    app.handle_pty(&mut sr, b"hello\r\n", &mut term_out)
        .expect("handle pty");

    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    let _ = app.maybe_finalize_changes(&mut sr).expect("finalize");

    assert!(recorder.inner.borrow().speaks.is_empty());
}

#[test]
fn focus_out_does_not_stop_when_option_disabled() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    sr.stop_speech_on_focus_loss = false;
    app.handle_stdin(&mut sr, b"\x1B[O", &mut pty_out, &mut term_out)
        .expect("handle stdin");

    assert!(!sr.terminal_focused);
    assert_eq!(recorder.inner.borrow().stops, 0);
}

#[test]
fn toggle_stop_on_focus_loss_hotkey_disables_stopping() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1Bg", &mut pty_out, &mut term_out)
        .expect("handle stdin");
    assert!(!sr.stop_speech_on_focus_loss);

    app.handle_stdin(&mut sr, b"\x1B[O", &mut pty_out, &mut term_out)
        .expect("handle stdin");

    let state = recorder.inner.borrow();
    assert!(
        state
            .speaks
            .iter()
            .any(|(text, _)| text == "stop on focus loss disabled")
    );
    assert_eq!(state.stops, 1);
}

#[test]
fn say_overlay_hotkey_speaks_terminal_title() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1Br", &mut pty_out, &mut term_out)
        .expect("handle stdin");

    let speaks = &recorder.inner.borrow().speaks;
    assert!(speaks.iter().any(|(text, _)| text == "Terminal"));
}

#[test]
fn lua_repl_history_persists_after_close() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1BL", &mut pty_out, &mut term_out)
        .expect("open repl");
    app.handle_stdin(&mut sr, b"print(1)\r", &mut pty_out, &mut term_out)
        .expect("submit command");
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .expect("finish eval");

    app.handle_stdin(&mut sr, b"\x1B", &mut pty_out, &mut term_out)
        .expect("queue escape");
    clock.advance_ms(100);
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .expect("close repl");

    app.handle_stdin(&mut sr, b"\x1BL", &mut pty_out, &mut term_out)
        .expect("reopen repl");
    app.handle_stdin(&mut sr, b"\x10", &mut pty_out, &mut term_out)
        .expect("history up");

    assert!(pty_out.is_empty());
    let rendered = String::from_utf8_lossy(&term_out);
    assert!(rendered.contains("> print(1)"));
    let speaks = &recorder.inner.borrow().speaks;
    assert!(speaks.iter().any(|(text, _)| text == "Lua REPL"));
}

#[test]
fn lua_repl_ctrl_l_clears_through_app_input_path() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1BL", &mut pty_out, &mut term_out)
        .expect("open repl");
    app.handle_stdin(&mut sr, b"alpha\r", &mut pty_out, &mut term_out)
        .expect("submit alpha");
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .expect("finish eval");

    let before_clear = String::from_utf8_lossy(&term_out).into_owned();
    assert!(before_clear.contains("alpha"));

    app.handle_stdin(&mut sr, b"\x0C", &mut pty_out, &mut term_out)
        .expect("ctrl-l");

    let after_clear = app.debug_active_view_contents();
    assert!(after_clear.contains("Esc to close"));
    assert!(!after_clear.contains("alpha"));
}

#[test]
fn lua_repl_ctrl_l_from_modify_other_keys_clears_output() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1BL", &mut pty_out, &mut term_out)
        .expect("open repl");
    app.handle_stdin(&mut sr, b"alpha\r", &mut pty_out, &mut term_out)
        .expect("submit alpha");
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .expect("finish eval");

    term_out.clear();
    app.handle_stdin(&mut sr, b"\x1B[27;5;108~", &mut pty_out, &mut term_out)
        .expect("ctrl-l modifyOtherKeys");

    let after_clear = app.debug_active_view_contents();
    assert!(after_clear.contains("Esc to close"));
    assert!(!after_clear.contains("alpha"));
}

#[test]
fn backspace_waits_for_cursor_movement_before_speaking() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"$ ", &mut term_out)
        .expect("handle pty");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    let _ = app.maybe_finalize_changes(&mut sr).expect("finalize");
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\x7F", &mut pty_out, &mut term_out)
        .expect("handle stdin");
    assert!(recorder.inner.borrow().speaks.is_empty());

    app.handle_pty(&mut sr, b"", &mut term_out)
        .expect("handle pty");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    let _ = app.maybe_finalize_changes(&mut sr).expect("finalize");

    assert!(recorder.inner.borrow().speaks.is_empty());
}

#[test]
fn delete_speaks_after_screen_change_with_auto_read_off() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    sr.auto_read = false;
    app.handle_pty(&mut sr, b"abc\x1B[D\x1B[D", &mut term_out)
        .expect("handle pty");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    let _ = app.maybe_finalize_changes(&mut sr).expect("finalize");
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\x1B[3~", &mut pty_out, &mut term_out)
        .expect("handle stdin");
    assert!(recorder.inner.borrow().speaks.is_empty());

    app.handle_pty(&mut sr, b"\x1B[P", &mut term_out)
        .expect("handle pty");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    let _ = app.maybe_finalize_changes(&mut sr).expect("finalize");

    let speaks = &recorder.inner.borrow().speaks;
    assert!(speaks.iter().any(|(text, _)| text == "b"));
}
