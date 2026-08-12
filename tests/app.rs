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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalKeyboardSupport {
    LegacyOnly,
    Kitty,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UnderlyingAppKeyboardSupport {
    Legacy,
    Kitty,
}

#[derive(Clone, Copy)]
enum ReplInputProtocol {
    Legacy,
    Kitty,
}

struct ReplLifecycleInput {
    open_repl: &'static [&'static [u8]],
    say_overlay: &'static [&'static [u8]],
    underscore: &'static [&'static [u8]],
    beginning_of_line: &'static [&'static [u8]],
    text_x: &'static [&'static [u8]],
    enter: &'static [&'static [u8]],
    expression: &'static [&'static [u8]],
    close_repl: &'static [&'static [u8]],
    resumed_app_input: &'static [&'static [u8]],
}

const LEGACY_REPL_LIFECYCLE_INPUT: ReplLifecycleInput = ReplLifecycleInput {
    open_repl: &[b"\x1BL"],
    say_overlay: &[b"\x1Bw"],
    underscore: &[b"_"],
    beginning_of_line: &[b"\x01"],
    text_x: &[b"x"],
    enter: &[b"\r"],
    expression: &[b"1+1"],
    close_repl: &[b"\x1B"],
    resumed_app_input: &[b"_\x01"],
};

// This represents a child application which requested every progressive Kitty
// keyboard feature, including alternate keys, event types, all-keys reporting,
// and associated text. Presses and releases are separate byte strings so the
// test exercises Lector's real incremental input parser.
const KITTY_REPL_LIFECYCLE_INPUT: ReplLifecycleInput = ReplLifecycleInput {
    open_repl: &[b"\x1B[108:76;4:1;76u", b"\x1B[108:76;4:3u"],
    say_overlay: &[b"\x1B[119;3:1;119u", b"\x1B[119;3:3u"],
    underscore: &[b"\x1B[45:95;2:1;95u", b"\x1B[45:95;2:3u"],
    beginning_of_line: &[b"\x1B[97;5:1u", b"\x1B[97;5:3u"],
    text_x: &[b"\x1B[120;1:1;120u", b"\x1B[120;1:3u"],
    enter: &[b"\x1B[13;1:1u", b"\x1B[13;1:3u"],
    expression: &[
        b"\x1B[49;1:1;49u",
        b"\x1B[49;1:3u",
        b"\x1B[61:43;2:1;43u",
        b"\x1B[61:43;2:3u",
        b"\x1B[49;1:1;49u",
        b"\x1B[49;1:3u",
    ],
    close_repl: &[b"\x1B[27;1:1u", b"\x1B[27;1:3u"],
    resumed_app_input: &[
        b"\x1B[45:95;2:1;95u",
        b"\x1B[45:95;2:3u",
        b"\x1B[97;5:1u",
        b"\x1B[97;5:3u",
    ],
};

fn send_input_events(
    app: &mut App,
    sr: &mut ScreenReader,
    events: &[&[u8]],
    pty_out: &mut Vec<u8>,
    term_out: &mut Vec<u8>,
    context: &str,
) {
    for event in events {
        app.handle_stdin(sr, event, pty_out, term_out)
            .unwrap_or_else(|error| panic!("{context}: {error}"));
    }
}

fn assert_repl_lifecycle(
    terminal: TerminalKeyboardSupport,
    underlying_app: UnderlyingAppKeyboardSupport,
) {
    let (protocol, input) = match (terminal, underlying_app) {
        (TerminalKeyboardSupport::LegacyOnly, UnderlyingAppKeyboardSupport::Legacy)
        | (TerminalKeyboardSupport::Kitty, UnderlyingAppKeyboardSupport::Legacy) => {
            (ReplInputProtocol::Legacy, &LEGACY_REPL_LIFECYCLE_INPUT)
        }
        (TerminalKeyboardSupport::Kitty, UnderlyingAppKeyboardSupport::Kitty) => {
            (ReplInputProtocol::Kitty, &KITTY_REPL_LIFECYCLE_INPUT)
        }
        (TerminalKeyboardSupport::LegacyOnly, UnderlyingAppKeyboardSupport::Kitty) => {
            panic!("a legacy-only terminal cannot enable the Kitty keyboard protocol")
        }
    };
    let scenario = format!("terminal={terminal:?}, underlying_app={underlying_app:?}");
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    if underlying_app == UnderlyingAppKeyboardSupport::Kitty {
        const ENABLE_ALL_KITTY_KEYBOARD_FEATURES: &[u8] = b"\x1B[>31u";
        app.handle_pty(&mut sr, ENABLE_ALL_KITTY_KEYBOARD_FEATURES, &mut term_out)
            .expect("forward child Kitty keyboard-mode request");
        assert_eq!(
            term_out, ENABLE_ALL_KITTY_KEYBOARD_FEATURES,
            "child keyboard-mode control must pass through: {scenario}"
        );
        term_out.clear();
    }

    send_input_events(
        &mut app,
        &mut sr,
        input.open_repl,
        &mut pty_out,
        &mut term_out,
        "open REPL",
    );
    assert!(app.has_overlay(), "REPL did not open: {scenario}");
    let contents = app.debug_active_view_contents();
    assert!(
        contents.contains("Lua REPL ready.") && contents.contains("Esc to close"),
        "REPL was not rendered: {scenario}, contents={contents:?}"
    );
    assert!(
        pty_out.is_empty(),
        "REPL shortcut leaked to child: {scenario}"
    );

    recorder.inner.borrow_mut().speaks.clear();
    send_input_events(
        &mut app,
        &mut sr,
        input.say_overlay,
        &mut pty_out,
        &mut term_out,
        "say overlay title",
    );
    assert!(
        recorder
            .inner
            .borrow()
            .speaks
            .iter()
            .any(|(text, _)| text == "Lua REPL"),
        "Lector command did not run inside REPL: {scenario}"
    );
    assert!(app.has_overlay(), "Lector command closed REPL: {scenario}");

    recorder.inner.borrow_mut().speaks.clear();
    send_input_events(
        &mut app,
        &mut sr,
        input.open_repl,
        &mut pty_out,
        &mut term_out,
        "open REPL while already open",
    );
    assert!(
        recorder
            .inner
            .borrow()
            .speaks
            .iter()
            .any(|(text, _)| text == "Lua REPL already open"),
        "second Lector command did not run inside REPL: {scenario}"
    );
    assert!(
        app.has_overlay(),
        "second REPL shortcut nested views: {scenario}"
    );

    for (events, context) in [
        (input.underscore, "type underscore"),
        (input.beginning_of_line, "move to beginning of line"),
        (input.text_x, "insert at beginning of line"),
    ] {
        send_input_events(
            &mut app,
            &mut sr,
            events,
            &mut pty_out,
            &mut term_out,
            context,
        );
    }
    let contents = app.debug_active_view_contents();
    assert!(
        contents.contains("> x_"),
        "text entry or C-a editing failed: {scenario}, contents={contents:?}"
    );

    send_input_events(
        &mut app,
        &mut sr,
        input.enter,
        &mut pty_out,
        &mut term_out,
        "submit edited identifier",
    );
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .expect("finish first REPL evaluation");
    send_input_events(
        &mut app,
        &mut sr,
        input.expression,
        &mut pty_out,
        &mut term_out,
        "type expression",
    );
    send_input_events(
        &mut app,
        &mut sr,
        input.enter,
        &mut pty_out,
        &mut term_out,
        "submit expression",
    );
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .expect("finish expression evaluation");
    let contents = app.debug_active_view_contents();
    assert!(
        contents.lines().any(|line| line.trim() == "2"),
        "REPL evaluation failed: {scenario}, contents={contents:?}"
    );

    term_out.clear();
    app.handle_pty(&mut sr, b"underlying app output\r\n", &mut term_out)
        .expect("receive child output while REPL is open");
    assert!(
        term_out.is_empty(),
        "child output overwrote active REPL: {scenario}"
    );

    send_input_events(
        &mut app,
        &mut sr,
        input.close_repl,
        &mut pty_out,
        &mut term_out,
        "close REPL",
    );
    if matches!(protocol, ReplInputProtocol::Legacy) {
        clock.advance_ms(50);
        app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
            .expect("resolve legacy Escape");
    }
    assert!(!app.has_overlay(), "REPL did not close: {scenario}");
    assert!(
        app.debug_active_view_contents()
            .contains("underlying app output"),
        "root application was not restored: {scenario}"
    );
    assert!(
        String::from_utf8_lossy(&term_out).contains("underlying app output"),
        "restored root application was not rendered: {scenario}"
    );
    assert!(
        pty_out.is_empty(),
        "REPL input or closing release leaked to child: {scenario}"
    );

    send_input_events(
        &mut app,
        &mut sr,
        input.resumed_app_input,
        &mut pty_out,
        &mut term_out,
        "forward input after REPL exit",
    );
    let expected: Vec<u8> = input
        .resumed_app_input
        .iter()
        .flat_map(|event| event.iter().copied())
        .collect();
    assert_eq!(
        pty_out, expected,
        "child input did not resume verbatim: {scenario}"
    );
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
    assert_eq!(sr.last_key(), b"a");
    assert_eq!(recorder.inner.borrow().stops, 1);
}

#[test]
fn semantic_and_scrollback_shortcuts_forward_when_the_feature_is_unavailable() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    for input in [
        b"\x1B[1;3A".as_slice(),
        b"\x1B[1;3B",
        b"\x1B[5;3~",
        b"\x1B[6;3~",
    ] {
        app.handle_stdin(&mut sr, input, &mut pty_out, &mut term_out)
            .expect("forward unavailable review shortcut");
    }

    assert_eq!(pty_out, b"\x1B[1;3A\x1B[1;3B\x1B[5;3~\x1B[6;3~");
}

#[test]
fn legacy_alt_arrows_are_forwarded_even_when_osc133_prompts_exist() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    app.handle_pty(
        &mut sr,
        b"\x1B]133;A\x07$ first\r\n\x1B]133;C\x07one\r\n\x1B]133;D;0\x07\x1B]133;A\x07$ second",
        &mut term_out,
    )
    .expect("render semantic prompts");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).expect("finalize"));
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\x1B[1;3A", &mut pty_out, &mut term_out)
        .expect("previous prompt");
    app.handle_stdin(&mut sr, b"\x1B[1;3A", &mut pty_out, &mut term_out)
        .expect("previous prompt again");
    app.handle_stdin(&mut sr, b"\x1B[1;3B", &mut pty_out, &mut term_out)
        .expect("next prompt");

    assert_eq!(pty_out, b"\x1B[1;3A\x1B[1;3A\x1B[1;3B");
    let spoken: Vec<_> = recorder
        .inner
        .borrow()
        .speaks
        .iter()
        .map(|(text, _)| text.clone())
        .collect();
    assert!(spoken.is_empty());
}

#[test]
fn legacy_alt_page_keys_are_forwarded_even_when_scrollback_exists() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let output = (0..30)
        .map(|line| format!("line {line}\r\n"))
        .collect::<String>();
    app.handle_pty(&mut sr, output.as_bytes(), &mut term_out)
        .expect("render enough output to scroll");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).expect("finalize"));
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\x1B[5;3~", &mut pty_out, &mut term_out)
        .expect("page up");
    app.handle_stdin(&mut sr, b"\x1B[6;3~", &mut pty_out, &mut term_out)
        .expect("page down");

    assert_eq!(pty_out, b"\x1B[5;3~\x1B[6;3~");
    assert!(recorder.inner.borrow().speaks.is_empty());
}

#[test]
fn legacy_review_overlay_freezes_output_bells_on_errors_and_restores_the_root() {
    let (mut app, mut sr, _recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    app.handle_pty(&mut sr, b"before\x1B[H", &mut term_out)
        .expect("draw source");

    term_out.clear();
    app.handle_stdin(&mut sr, b"\x1Br", &mut pty_out, &mut term_out)
        .expect("open review");
    assert!(app.has_overlay());
    assert!(app.debug_active_view_contents().contains("before"));
    assert!(pty_out.is_empty());

    term_out.clear();
    app.handle_pty(&mut sr, b"\x1B[Hafter", &mut term_out)
        .expect("update root behind review");
    assert!(term_out.is_empty());
    assert!(app.debug_active_view_contents().contains("before"));
    assert!(!app.debug_active_view_contents().contains("after"));

    app.handle_stdin(&mut sr, b"fz", &mut pty_out, &mut term_out)
        .expect("failed find");
    assert_eq!(term_out, b"\x07");
    assert!(app.has_overlay());

    term_out.clear();
    app.handle_stdin(&mut sr, b"f\x1B", &mut pty_out, &mut term_out)
        .expect("queue cancellation escape");
    clock.advance_ms(100);
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .expect("cancel pending find");
    assert!(term_out.is_empty());
    assert!(app.has_overlay());

    app.handle_stdin(&mut sr, b"\x1B", &mut pty_out, &mut term_out)
        .expect("queue idle escape");
    clock.advance_ms(100);
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .expect("bell on idle escape");
    assert_eq!(term_out, b"\x07");
    assert!(app.has_overlay());

    term_out.clear();
    app.handle_stdin(&mut sr, b"q", &mut pty_out, &mut term_out)
        .expect("close review");
    assert!(!app.has_overlay());
    assert!(app.debug_active_view_contents().contains("after"));
    assert!(String::from_utf8_lossy(&term_out).contains("after"));
    assert!(pty_out.is_empty());
}

#[test]
fn kitty_meta_r_opens_review_without_toggling_or_leaking() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    app.handle_pty(&mut sr, b"snapshot", &mut term_out)
        .expect("draw source");
    let meta_r = b"\x1B[114;3:1u\x1B[114;3:3u";

    app.handle_stdin(&mut sr, meta_r, &mut pty_out, &mut term_out)
        .expect("open Kitty review");
    assert!(app.has_overlay());
    assert!(app.debug_active_view_contents().contains("snapshot"));

    recorder.inner.borrow_mut().speaks.clear();
    app.handle_stdin(&mut sr, meta_r, &mut pty_out, &mut term_out)
        .expect("invoke open-only action inside review");
    assert!(app.has_overlay());
    assert!(
        recorder
            .inner
            .borrow()
            .speaks
            .iter()
            .any(|(text, _)| text == "Review already open")
    );
    app.handle_stdin(&mut sr, b"q", &mut pty_out, &mut term_out)
        .expect("close review with q");
    assert!(!app.has_overlay());
    assert!(pty_out.is_empty());
}

#[test]
fn review_over_lua_returns_to_the_lua_overlay_normally() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    app.handle_stdin(&mut sr, b"\x1BL", &mut pty_out, &mut term_out)
        .expect("open Lua");
    assert!(app.debug_active_view_contents().contains("Lua REPL ready"));

    app.handle_stdin(&mut sr, b"\x1Br", &mut pty_out, &mut term_out)
        .expect("review Lua");
    assert!(app.debug_active_view_contents().contains("Lua REPL ready"));
    app.handle_stdin(&mut sr, b"q", &mut pty_out, &mut term_out)
        .expect("leave review");
    app.handle_stdin(&mut sr, b"x", &mut pty_out, &mut term_out)
        .expect("resume Lua input");

    assert!(app.has_overlay());
    assert!(app.debug_active_view_contents().contains("> x"));
    assert!(pty_out.is_empty());
}

#[test]
fn review_search_repeat_and_inner_word_yank_feed_f7_clipboard_paste() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    app.handle_pty(&mut sr, b"one two one three\x1B[H", &mut term_out)
        .expect("draw searchable source");
    app.handle_stdin(&mut sr, b"\x1Br", &mut pty_out, &mut term_out)
        .expect("open review");

    app.handle_stdin(&mut sr, b"/one\rNnwyiw", &mut pty_out, &mut term_out)
        .expect("search, repeat, move and yank");
    app.handle_stdin(&mut sr, b"q", &mut pty_out, &mut term_out)
        .expect("close review");
    app.handle_stdin(&mut sr, b"\x1B[18~", &mut pty_out, &mut term_out)
        .expect("paste with F7");

    assert_eq!(pty_out, b"three");
}

#[test]
fn review_backward_search_and_reverse_repeat_are_motion_commands() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    app.handle_pty(&mut sr, b"one two one three\x1B[H", &mut term_out)
        .expect("draw searchable source");
    app.handle_stdin(&mut sr, b"\x1Br", &mut pty_out, &mut term_out)
        .expect("open review");
    app.handle_stdin(&mut sr, b"?one\rNwyiwq", &mut pty_out, &mut term_out)
        .expect("backward search, reverse repeat, move, yank and close");
    app.handle_stdin(&mut sr, b"\x1B[18~", &mut pty_out, &mut term_out)
        .expect("paste yank");

    assert_eq!(pty_out, b"two");
}

#[test]
fn review_prompt_jumps_use_osc133_markers() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    app.handle_pty(
        &mut sr,
        b"\x1B]133;A\x07$ first\r\nresult\r\n\x1B]133;A\x07$ second\x1B[H",
        &mut term_out,
    )
    .expect("draw semantic prompts");
    app.handle_stdin(&mut sr, b"\x1Br]pwyiwq", &mut pty_out, &mut term_out)
        .expect("jump to next prompt and yank its first word");
    app.handle_stdin(&mut sr, b"\x1B[18~", &mut pty_out, &mut term_out)
        .expect("paste prompt word");

    assert_eq!(pty_out, b"second");
}

#[test]
fn readline_history_arrows_speak_the_recalled_input_without_the_prompt() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    app.handle_pty(
        &mut sr,
        b"\x1B]133;A\x07user@host$ \x1B]133;B\x07old",
        &mut term_out,
    )
    .expect("render editable prompt");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize prompt")
    );
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\x1B[A", &mut pty_out, &mut term_out)
        .expect("forward history up");
    app.handle_pty(
        &mut sr,
        b"\r\x1B[Kuser@host$ recalled command",
        &mut term_out,
    )
    .expect("render Readline history selection");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize history redraw")
    );

    assert_eq!(pty_out, b"\x1B[A");
    let spoken: Vec<_> = recorder
        .inner
        .borrow()
        .speaks
        .iter()
        .map(|(text, _)| text.clone())
        .collect();
    assert_eq!(spoken, ["recalled command"]);
}

#[test]
fn paste_writes_to_pty_and_speaks() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    sr.push_clipboard("hello".to_string()).unwrap();
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
fn table_navigation_preserves_direction_and_boundary_behavior() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(
        &mut sr,
        b"| A | B | C |\r\n|---|---|---|\r\n| 1 | 2 | 3 |\r\n| 4 | 5 | 6 |\x1B[1;1H",
        &mut term_out,
    )
    .expect("draw table");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).expect("finalize table"));
    app.handle_stdin(&mut sr, b"\x1Bt", &mut pty_out, &mut term_out)
        .expect("enter table mode");
    recorder.inner.borrow_mut().speaks.clear();

    for key in [
        b"h".as_slice(),
        b"l",
        b"$",
        b"$",
        b"^",
        b"j",
        b"G",
        b"k",
        b"g",
        b"k",
    ] {
        app.handle_stdin(&mut sr, key, &mut pty_out, &mut term_out)
            .expect("navigate table");
    }

    let spoken: Vec<String> = recorder
        .inner
        .borrow()
        .speaks
        .iter()
        .map(|(text, _)| text.clone())
        .collect();
    assert_eq!(
        spoken,
        ["left", "B", "C", "right", "A", "1", "4", "1", "A", "top"]
    );
    assert!(pty_out.is_empty());
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
fn key_echo_suppression_handles_typeahead_before_slow_terminal_echo() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    sr.set_suppress_key_echo(true);
    app.handle_stdin(&mut sr, b"abcdefg", &mut pty_out, &mut term_out)
        .expect("type ahead");
    assert_eq!(pty_out, b"abcdefg");
    assert_eq!(sr.last_key(), b"g");

    for byte in b"abcdefg" {
        app.handle_pty(&mut sr, &[*byte], &mut term_out)
            .expect("receive delayed echo");
        clock.advance_ms(u128::from(DIFF_DELAY) + 1);
        assert!(
            app.maybe_finalize_changes(&mut sr)
                .expect("finalize delayed echo")
        );
    }

    assert!(recorder.inner.borrow().speaks.is_empty());
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
fn diff_wins_when_cursor_movement_redraws_the_cursor_line() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"cat -frob", &mut term_out)
        .expect("draw initial command");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    app.maybe_finalize_changes(&mut sr)
        .expect("finalize initial command");
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\t", &mut pty_out, &mut term_out)
        .expect("request completion");
    app.handle_pty(&mut sr, b"\x1B[9D\x1B[Kcat -frobnicate-mode", &mut term_out)
        .expect("redraw completed command");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    app.maybe_finalize_changes(&mut sr)
        .expect("finalize completion");

    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        [("nicate-mode".into(), false)]
    );
}

#[test]
fn inline_input_ignores_coupled_ruler_redraws() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(
        &mut sr,
        b"\x1B[Hhe\x1B[23;1H[No Name] [+]                                                 1,3            All\x1B[1;3H",
        &mut term_out,
    )
    .expect("draw initial editor screen");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    app.maybe_finalize_changes(&mut sr)
        .expect("finalize initial editor screen");
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"l", &mut pty_out, &mut term_out)
        .expect("append first character");
    app.handle_pty(
        &mut sr,
        b"\x1B[?25ll\x1B[23;65H4\x1B[1;4H\x1B[34h\x1B[?25h",
        &mut term_out,
    )
    .expect("redraw first character and ruler");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    app.maybe_finalize_changes(&mut sr)
        .expect("finalize first character");

    app.handle_stdin(&mut sr, b"p", &mut pty_out, &mut term_out)
        .expect("append second character");
    app.handle_pty(
        &mut sr,
        b"\x1B[?25lp\x1B[23;65H5\x1B[1;5H\x1B[34h\x1B[?25h",
        &mut term_out,
    )
    .expect("redraw second character and ruler");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    app.maybe_finalize_changes(&mut sr)
        .expect("finalize second character");

    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        [("l".into(), false), ("p".into(), false)]
    );
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
    assert_eq!(sr.last_key(), b"\x1Bl");
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
    assert_eq!(sr.last_key(), b"\x1B[117;3u");
}

#[test]
fn kitty_control_key_interrupts_speech() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1B[108;5u", &mut pty_out, &mut term_out)
        .expect("handle Kitty Control-l");

    assert_eq!(recorder.inner.borrow().stops, 1);
    assert_eq!(sr.last_key(), b"\x1B[108;5u");
}

#[test]
fn kitty_release_does_not_repeat_lector_binding() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(
        &mut sr,
        b"\x1B[39;3:1u\x1B[39;3:3u",
        &mut pty_out,
        &mut term_out,
    )
    .expect("handle Kitty Meta-apostrophe press and release");

    assert!(!sr.auto_read_enabled());
    assert!(pty_out.is_empty());
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        [("auto read disabled".into(), false)]
    );
    assert_eq!(recorder.inner.borrow().stops, 1);
}

#[test]
fn kitty_repeat_repeats_lector_binding_but_release_does_not() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(
        &mut sr,
        b"\x1B[39;3:1u\x1B[39;3:2u\x1B[39;3:3u",
        &mut pty_out,
        &mut term_out,
    )
    .expect("handle Kitty Meta-apostrophe press, repeat, and release");

    assert!(sr.auto_read_enabled());
    assert!(pty_out.is_empty());
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        [
            ("auto read disabled".into(), false),
            ("auto read enabled".into(), false),
        ]
    );
    assert_eq!(recorder.inner.borrow().stops, 2);
}

#[test]
fn kitty_unbound_press_and_release_are_forwarded() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let input = b"\x1B[97;1:1u\x1B[97;1:3u";

    app.handle_stdin(&mut sr, input, &mut pty_out, &mut term_out)
        .expect("handle Kitty a press and release");

    assert_eq!(pty_out, input);
}

#[test]
fn kitty_passed_through_binding_forwards_press_and_release() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1Bn", &mut pty_out, &mut term_out)
        .expect("enable pass-through");
    let input = b"\x1B[39;3:1u\x1B[39;3:3u";
    app.handle_stdin(&mut sr, input, &mut pty_out, &mut term_out)
        .expect("pass through Kitty Meta-apostrophe press and release");

    assert_eq!(pty_out, input);
    assert!(sr.auto_read_enabled());
}

#[test]
fn kitty_forwarding_binding_forwards_press_and_release() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let input = b"\x1B[127;1:1u\x1B[127;1:3u";

    app.handle_stdin(&mut sr, input, &mut pty_out, &mut term_out)
        .expect("handle Kitty Backspace press and release");

    assert_eq!(pty_out, input);
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
    assert_eq!(sr.last_key(), b"\x1B[");
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
    assert_eq!(sr.last_key(), b"\x1B]");
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
    assert_eq!(sr.last_key(), osc);
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
    assert_eq!(sr.last_key(), osc);
    assert_eq!(recorder.inner.borrow().stops, 1);
}

#[test]
fn help_mode_can_toggle_off() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    sr.set_help_mode(true);
    app.handle_stdin(&mut sr, b"\x1BOP", &mut pty_out, &mut term_out)
        .expect("handle stdin");

    assert!(!sr.help_mode());
}

#[test]
fn focus_events_not_forwarded_without_app_request() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1B[O", &mut pty_out, &mut term_out)
        .expect("handle stdin");

    assert!(pty_out.is_empty());
    assert!(!sr.terminal_focused());
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
    assert!(sr.terminal_focused());
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

    sr.set_stop_speech_on_focus_loss(false);
    app.handle_stdin(&mut sr, b"\x1B[O", &mut pty_out, &mut term_out)
        .expect("handle stdin");

    assert!(!sr.terminal_focused());
    assert_eq!(recorder.inner.borrow().stops, 0);
}

#[test]
fn toggle_stop_on_focus_loss_hotkey_disables_stopping() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1Bg", &mut pty_out, &mut term_out)
        .expect("handle stdin");
    assert!(!sr.stop_speech_on_focus_loss());

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

    app.handle_stdin(&mut sr, b"\x1Bw", &mut pty_out, &mut term_out)
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
fn lua_repl_accepts_legacy_and_kitty_encoded_shift_punctuation() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1BL", &mut pty_out, &mut term_out)
        .expect("open repl");
    app.handle_stdin(&mut sr, b"!", &mut pty_out, &mut term_out)
        .expect("legacy shift punctuation");
    app.handle_stdin(
        &mut sr,
        b"\x1B[95;2u\x1B[43;2u\x1B[123;2u",
        &mut pty_out,
        &mut term_out,
    )
    .expect("Kitty shift punctuation");

    assert!(pty_out.is_empty());
    assert!(app.debug_active_view_contents().contains("> !_+{"));
}

#[test]
fn lua_repl_accepts_modify_other_keys_shift_punctuation() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1BL", &mut pty_out, &mut term_out)
        .expect("open repl");
    app.handle_stdin(&mut sr, b"\x1B[27;2;95~", &mut pty_out, &mut term_out)
        .expect("modifyOtherKeys underscore");

    assert!(pty_out.is_empty());
    assert!(app.debug_active_view_contents().contains("> _"));
}

#[test]
fn lua_repl_uses_kitty_associated_text_and_legacy_unicode() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1BL", &mut pty_out, &mut term_out)
        .expect("open repl");
    app.handle_stdin(&mut sr, "é".as_bytes(), &mut pty_out, &mut term_out)
        .expect("legacy UTF-8 text");
    app.handle_stdin(&mut sr, b"\x1B[45;2;95u", &mut pty_out, &mut term_out)
        .expect("Kitty associated underscore text");
    app.handle_stdin(&mut sr, b"\x1B[0;1;229u", &mut pty_out, &mut term_out)
        .expect("Kitty pure text event");

    assert!(pty_out.is_empty());
    assert!(app.debug_active_view_contents().contains("> é_å"));
}

#[test]
fn lua_repl_accepts_multiline_bracketed_paste_without_executing_controls() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1BL", &mut pty_out, &mut term_out)
        .expect("open repl");
    app.handle_stdin(
        &mut sr,
        b"\x1B[200~1 +\n1\x1B[201~",
        &mut pty_out,
        &mut term_out,
    )
    .expect("paste multiline expression");
    assert!(app.debug_active_view_contents().contains("> 1 +↵1"));

    app.handle_stdin(&mut sr, b"\r", &mut pty_out, &mut term_out)
        .expect("submit pasted expression");
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .expect("finish eval");

    assert!(app.debug_active_view_contents().contains("2"));
    assert!(pty_out.is_empty());
}

#[test]
fn lua_repl_semantic_commands_work_with_kitty_modifiers() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1BL", &mut pty_out, &mut term_out)
        .expect("open repl");
    app.handle_stdin(&mut sr, b"abc def", &mut pty_out, &mut term_out)
        .expect("initial text");
    app.handle_stdin(&mut sr, b"\x1B[98;3uX", &mut pty_out, &mut term_out)
        .expect("Kitty Alt-b and insertion");
    app.handle_stdin(&mut sr, b"\x1B[97;6uY", &mut pty_out, &mut term_out)
        .expect("Kitty Ctrl-Shift-a and insertion");
    app.handle_stdin(&mut sr, b"\x1B[101;5u", &mut pty_out, &mut term_out)
        .expect("Kitty Ctrl-e");
    app.handle_stdin(&mut sr, b"\x1B[104;5u", &mut pty_out, &mut term_out)
        .expect("Kitty Ctrl-h through forwarding binding");
    app.handle_stdin(&mut sr, b"\x1B[127;3u", &mut pty_out, &mut term_out)
        .expect("Kitty Alt-Backspace");

    assert!(pty_out.is_empty());
    let contents = app.debug_active_view_contents();
    assert!(contents.contains("> Yabc\n"), "contents={contents:?}");
}

#[test]
fn lua_repl_editing_is_protocol_invariant() {
    fn run(commands: &[&[u8]]) -> String {
        let (mut app, mut sr, _recorder, _clock) = make_app();
        let mut pty_out = Vec::new();
        let mut term_out = Vec::new();
        app.handle_stdin(&mut sr, b"\x1BLabc def", &mut pty_out, &mut term_out)
            .expect("open repl and enter initial text");
        for command in commands {
            app.handle_stdin(&mut sr, command, &mut pty_out, &mut term_out)
                .expect("apply editing command");
        }
        assert!(pty_out.is_empty());
        app.debug_active_view_contents()
    }

    let legacy = run(&[
        b"\x1Bb", b"X", b"\x01", b"Y", b"\x05", b"\x7F", b"\x1B[D", b"\x1B[3~",
    ]);
    let kitty = run(&[
        b"\x1B[98;3u",
        b"X",
        b"\x1B[97;5u",
        b"Y",
        b"\x1B[101;5u",
        b"\x1B[127;1u",
        b"\x1B[1;1D",
        b"\x1B[3;1~",
    ]);
    let modify_other_keys = run(&[
        b"\x1B[27;3;98~",
        b"X",
        b"\x1B[27;5;97~",
        b"Y",
        b"\x1B[27;5;101~",
        b"\x1B[27;1;127~",
        b"\x1B[D",
        b"\x1B[3~",
    ]);

    assert!(legacy.contains("> Yabc Xd"), "legacy={legacy:?}");
    assert_eq!(kitty, legacy);
    assert_eq!(modify_other_keys, legacy);
}

#[test]
fn lua_repl_handles_kitty_special_keys_and_ignores_releases() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1BLabc", &mut pty_out, &mut term_out)
        .expect("open repl and type");
    app.handle_stdin(
        &mut sr,
        b"\x1B[1;1:1D\x1B[1;1:3DX",
        &mut pty_out,
        &mut term_out,
    )
    .expect("Kitty left press, release, and insertion");
    app.handle_stdin(&mut sr, b"\x1B[3;1~", &mut pty_out, &mut term_out)
        .expect("Kitty Delete");
    app.handle_stdin(&mut sr, b"\x1B[127;1u", &mut pty_out, &mut term_out)
        .expect("Kitty Backspace");

    assert!(pty_out.is_empty());
    assert!(app.debug_active_view_contents().contains("> ab"));
}

#[test]
fn lua_repl_submits_and_closes_with_report_all_keys_encodings() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1BL1+1", &mut pty_out, &mut term_out)
        .expect("open repl and type expression");
    app.handle_stdin(&mut sr, b"\x1B[13;1u", &mut pty_out, &mut term_out)
        .expect("Kitty Enter");
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .expect("finish eval");
    assert!(app.debug_active_view_contents().contains("2"));

    app.handle_stdin(&mut sr, b"\x1B[27;1u", &mut pty_out, &mut term_out)
        .expect("Kitty Escape");
    assert!(!app.has_overlay());
    assert!(pty_out.is_empty());
}

#[test]
fn repl_lifecycle_on_non_kitty_terminal_with_legacy_app() {
    assert_repl_lifecycle(
        TerminalKeyboardSupport::LegacyOnly,
        UnderlyingAppKeyboardSupport::Legacy,
    );
}

#[test]
fn repl_lifecycle_on_kitty_terminal_with_legacy_app() {
    assert_repl_lifecycle(
        TerminalKeyboardSupport::Kitty,
        UnderlyingAppKeyboardSupport::Legacy,
    );
}

#[test]
fn repl_lifecycle_on_kitty_terminal_with_kitty_app() {
    assert_repl_lifecycle(
        TerminalKeyboardSupport::Kitty,
        UnderlyingAppKeyboardSupport::Kitty,
    );
}

#[test]
fn kitty_associated_text_is_forwarded_verbatim_outside_overlays() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let input = b"\x1B[45;2;95u";

    app.handle_stdin(&mut sr, input, &mut pty_out, &mut term_out)
        .expect("forward Kitty associated-text event");

    assert_eq!(pty_out, input);
}

#[test]
fn legacy_console_input_is_forwarded_verbatim_outside_overlays() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let input = b"_\x01\x1B[D\x7F";

    app.handle_stdin(&mut sr, input, &mut pty_out, &mut term_out)
        .expect("forward legacy console input");

    assert_eq!(pty_out, input);
}

#[test]
fn kitty_report_all_enter_and_release_close_message_once() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    app.show_message(&mut sr, "Notice", "body", &mut term_out)
        .expect("show message");

    app.handle_stdin(
        &mut sr,
        b"\x1B[13;1:1u\x1B[13;1:3u",
        &mut pty_out,
        &mut term_out,
    )
    .expect("Kitty Enter press and release");

    assert!(!app.has_overlay());
    assert!(pty_out.is_empty());
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

    sr.set_auto_read_enabled(false);
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

#[test]
fn no_pty_update_never_finalizes() {
    let (mut app, mut sr, _recorder, clock) = make_app();

    clock.advance_ms(10_000);

    assert!(!app.maybe_finalize_changes(&mut sr).unwrap());
}

#[test]
fn lone_escape_flushes_at_the_exact_timeout_boundary() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1B", &mut pty_out, &mut term_out)
        .unwrap();
    clock.advance_ms(49);
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .unwrap();
    assert!(pty_out.is_empty());

    clock.advance_ms(1);
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .unwrap();

    assert_eq!(pty_out, b"\x1B");
    assert_eq!(sr.last_key(), b"\x1B");
    assert_eq!(recorder.inner.borrow().stops, 1);
}

#[test]
fn unterminated_control_sequence_flushes_as_raw_input_after_timeout() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let input = b"\x1B]unterminated";

    app.handle_stdin(&mut sr, input, &mut pty_out, &mut term_out)
        .unwrap();
    assert!(pty_out.is_empty());
    clock.advance_ms(50);
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .unwrap();

    assert_eq!(pty_out, input);
    assert_eq!(sr.last_key(), input);
    assert_eq!(recorder.inner.borrow().stops, 1);
}

#[test]
fn unknown_modify_other_keys_sequence_is_forwarded_verbatim() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let input = b"\x1B[27;;~";

    app.handle_stdin(&mut sr, input, &mut pty_out, &mut term_out)
        .unwrap();

    assert_eq!(pty_out, input);
    assert_eq!(sr.last_key(), input);
}

#[test]
fn message_overlay_renders_resizes_and_closes_without_pty_input() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    assert!(!app.has_overlay());
    app.show_message(&mut sr, "Notice", "first\nsecond", &mut term_out)
        .unwrap();

    assert!(app.has_overlay());
    assert!(app.debug_active_view_contents().contains("first\nsecond"));
    assert!(String::from_utf8_lossy(&term_out).contains("first"));
    assert!(
        recorder
            .inner
            .borrow()
            .speaks
            .iter()
            .any(|(text, _)| text == "Notice")
    );

    term_out.clear();
    app.on_resize(12, 40, &mut term_out).unwrap();
    assert!(String::from_utf8_lossy(&term_out).contains("second"));

    app.handle_stdin(&mut sr, b"\r", &mut pty_out, &mut term_out)
        .unwrap();
    assert!(!app.has_overlay());
    assert!(pty_out.is_empty());
}

#[test]
fn pty_output_updates_root_while_overlay_remains_visible() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.show_message(&mut sr, "Notice", "foreground", &mut term_out)
        .unwrap();
    term_out.clear();
    app.handle_pty(&mut sr, b"background", &mut term_out)
        .unwrap();
    assert!(term_out.is_empty());
    assert!(app.debug_active_view_contents().contains("foreground"));

    app.handle_stdin(&mut sr, b"\n", &mut pty_out, &mut term_out)
        .unwrap();

    assert!(!app.has_overlay());
    assert!(app.debug_active_view_contents().contains("background"));
    assert!(String::from_utf8_lossy(&term_out).contains("background"));
    assert!(pty_out.is_empty());
}

#[test]
fn standard_clock_constructor_exposes_idle_terminal_state() {
    let view_stack = views::ViewStack::new(Box::new(views::PtyView::new(2, 3)));
    let mut app = App::new(view_stack).unwrap();

    assert!(!app.has_overlay());
    assert!(!app.wants_tick());
    assert_eq!(app.debug_active_view_contents(), "\n\n");
}
