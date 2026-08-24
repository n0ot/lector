use lector::{
    app::{App, Clock, DIFF_DELAY, MAX_DIFF_DELAY, MAX_PENDING_TERMINAL_INPUT_BYTES},
    output_scheduler::OutputSchedulerConfig,
    presentation::{PresentedScene, RenderStrategy},
    screen_reader::ScreenReader,
    speech,
    terminal::{Color, GhosttyEngine, TerminalEngine},
    terminal_protocol::PhysicalTerminalProfile,
    view::View,
    views,
};
use std::{
    cell::{Cell, RefCell},
    io::{self, Write},
    rc::Rc,
};

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

#[derive(Default)]
struct FlushGateWriter {
    bytes: Vec<u8>,
    block_writes: bool,
    block_flush: bool,
}

#[derive(Default)]
struct PresentationOutput {
    bytes: Vec<u8>,
    flushes: usize,
}

impl Write for PresentationOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flushes = self.flushes.saturating_add(1);
        Ok(())
    }
}

impl Write for FlushGateWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.block_writes {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.block_flush {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        } else {
            Ok(())
        }
    }
}

#[derive(Default)]
struct SingleRenderByteWriteGate {
    bytes: Vec<u8>,
    accepted_render_byte: bool,
    blocked: bool,
}

impl SingleRenderByteWriteGate {
    fn release(&mut self) {
        self.blocked = false;
    }
}

impl Write for SingleRenderByteWriteGate {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.blocked {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        // Model effects are separate OSC transactions ahead of the render.
        // Let them complete, then make the visual render nonreplaceable by
        // accepting exactly one of its bytes before applying backpressure.
        if bytes.starts_with(b"\x1b]") {
            self.bytes.extend_from_slice(bytes);
            return Ok(bytes.len());
        }
        if !self.accepted_render_byte {
            self.accepted_render_byte = true;
            self.blocked = true;
            self.bytes.extend_from_slice(&bytes[..1]);
            return Ok(1);
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
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
fn indentation_reporting_option_covers_both_cursors_and_auto_read() {
    let (_app, mut sr, recorder, _clock) = make_app();
    sr.set_indentation_reporting_enabled(false);

    let mut application_view = View::new(1, 16);
    application_view.process_changes(b"  application");
    sr.report_application_cursor_indentation_changes(&mut application_view)
        .expect("suppress application cursor indentation");

    let mut review_view = View::new(1, 16);
    review_view.process_changes(b"    review");
    sr.report_review_cursor_indentation_changes(&mut review_view)
        .expect("suppress review cursor indentation");

    let mut auto_read_view = View::new(1, 16);
    auto_read_view.process_changes(b"      auto read");
    sr.auto_read(&mut auto_read_view)
        .expect("auto-read without indentation reporting");

    assert!(
        recorder
            .inner
            .borrow()
            .speaks
            .iter()
            .all(|(text, _)| !text.starts_with("indent "))
    );
}

#[test]
fn direct_pty_presentation_batch_models_every_read_and_renders_once() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let before = app.presented_scene().clone();
    let mut physical = PresentationOutput::default();

    app.begin_pty_presentation_batch();
    app.handle_pty(&mut sr, b"\x1b[2J\x1b[Hfirst", &mut physical)
        .expect("model first direct read");
    assert!(app.debug_active_view_contents().contains("first"));
    app.handle_pty(&mut sr, b"\r\x1b[2Kfinal\x1b[6n", &mut physical)
        .expect("model second direct read");
    assert!(app.debug_active_view_contents().contains("final"));

    let mut replies = Vec::new();
    app.flush_application_replies(&mut replies)
        .expect("publish protocol reply before presentation finishes");
    assert!(!replies.is_empty());
    assert_eq!(app.presented_scene(), &before);
    assert!(physical.bytes.is_empty());
    assert_eq!(physical.flushes, 0);

    app.finish_pty_presentation_batch(&mut physical)
        .expect("present final direct state");
    assert_eq!(physical.flushes, 1);
    assert!(
        app.presented_scene()
            .clone()
            .into_terminal_snapshot()
            .contents_full()
            .contains("final")
    );
}

#[test]
fn canceling_direct_presentation_keeps_model_but_drops_its_orphan_bell() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let before = app.presented_scene().clone();
    let mut physical = PresentationOutput::default();

    app.begin_pty_presentation_batch();
    app.handle_pty(&mut sr, b"retained\x07", &mut physical)
        .expect("model canceled direct output");
    assert!(app.debug_active_view_contents().contains("retained"));
    assert!(physical.bytes.is_empty());
    assert_eq!(physical.flushes, 0);

    app.cancel_pty_presentation_batch();
    app.finish_pty_presentation_batch(&mut physical)
        .expect("finish canceled batch harmlessly");
    assert_eq!(app.presented_scene(), &before);
    assert!(physical.bytes.is_empty());
    assert_eq!(physical.flushes, 0);

    app.begin_pty_presentation_batch();
    app.handle_pty(&mut sr, b" next", &mut physical)
        .expect("model next direct output");
    app.finish_pty_presentation_batch(&mut physical)
        .expect("recover canceled presentation");
    let presented = app
        .presented_scene()
        .clone()
        .into_terminal_snapshot()
        .contents_full();
    assert!(
        presented.contains("retained next"),
        "presented={presented:?}"
    );
    assert_eq!(
        app.debug_last_render_strategy(),
        RenderStrategy::FullFallback
    );
    assert!(
        !physical.bytes.contains(&b'\x07'),
        "the canceled update's bell escaped behind a later scene"
    );
}

#[test]
fn accepted_direct_presentation_writes_its_bell_after_the_visual_transaction() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut physical = PresentationOutput::default();

    app.begin_pty_presentation_batch();
    app.handle_pty(&mut sr, b"visible-before-bell\x07", &mut physical)
        .expect("model direct output and bell");
    assert!(physical.bytes.is_empty());

    app.finish_pty_presentation_batch(&mut physical)
        .expect("present direct output and bell");
    assert_eq!(physical.bytes.last(), Some(&b'\x07'));
    assert_eq!(physical.flushes, 1);
    assert!(
        app.presented_scene()
            .clone()
            .into_terminal_snapshot()
            .contents_full()
            .contains("visible-before-bell")
    );
}

#[test]
fn scheduler_capacity_drop_does_not_emit_a_bell_without_its_scene() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        maximum_pending_bytes: 1,
        ..OutputSchedulerConfig::default()
    });
    let mut physical = PresentationOutput::default();

    app.begin_pty_presentation_batch();
    app.handle_pty(&mut sr, b"render-too-large\x07", &mut physical)
        .expect("model over-capacity output and bell");
    app.finish_pty_presentation_batch(&mut physical)
        .expect("reject over-capacity render");
    app.drain_scheduled_output(&mut physical, true)
        .expect("drain accepted scheduler work");

    assert!(physical.bytes.is_empty());
    assert_eq!(physical.flushes, 0);
}

#[test]
fn capacity_dropped_overlay_pop_retries_after_the_started_overlay_drains() {
    const UNDERLAY: &[u8] = b"stable-underlay";
    const WORKING: &[u8] = b"\x1b[?2026h\rworking-frame";
    const OVERLAY_BODY: &str = "capacity-blocked-overlay";

    // Measure each independently replaceable full scene first. The real
    // scheduler cap admits either scene by itself but not the replacement
    // plus a partially written predecessor.
    let (mut probe, mut probe_sr, _recorder, _clock) = make_app();
    let mut probe_term = Vec::new();
    let mut probe_pty = Vec::new();
    probe
        .handle_pty(&mut probe_sr, UNDERLAY, &mut probe_term)
        .expect("establish probe underlay");
    probe.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        maximum_pending_bytes: 1024 * 1024,
        ..OutputSchedulerConfig::default()
    });
    probe
        .handle_pty(&mut probe_sr, WORKING, &mut probe_term)
        .expect("open probe synchronized transaction");
    probe
        .show_message(&mut probe_sr, "Notice", OVERLAY_BODY, &mut probe_term)
        .expect("queue probe overlay");
    let overlay_bytes = probe.debug_scheduled_output_pending_bytes();
    probe
        .handle_stdin(&mut probe_sr, b"\n", &mut probe_pty, &mut probe_term)
        .expect("queue probe overlay pop");
    let restored_bytes = probe.debug_scheduled_output_pending_bytes();
    let capacity = overlay_bytes.max(restored_bytes);
    assert!(capacity > 1);

    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut baseline = Vec::new();
    let mut pty_out = Vec::new();
    app.handle_pty(&mut sr, UNDERLAY, &mut baseline)
        .expect("establish physical underlay");
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        write_budget_bytes: usize::MAX,
        maximum_pending_bytes: capacity,
        ..OutputSchedulerConfig::default()
    });
    app.handle_pty(&mut sr, WORKING, &mut baseline)
        .expect("open synchronized transaction");
    app.show_message(&mut sr, "Notice", OVERLAY_BODY, &mut baseline)
        .expect("queue bounded overlay");
    assert_eq!(app.debug_scheduled_output_pending_bytes(), overlay_bytes);

    let mut physical_output = SingleRenderByteWriteGate::default();
    let partial = app
        .drain_scheduled_output(&mut physical_output, false)
        .expect("start overlay render");
    assert!(partial.blocked);
    let retained_overlay_bytes = app.debug_scheduled_output_pending_bytes();
    assert!(physical_output.accepted_render_byte);
    assert!(retained_overlay_bytes > 0);
    assert!(retained_overlay_bytes < overlay_bytes);

    app.handle_stdin(&mut sr, b"\n", &mut pty_out, &mut physical_output)
        .expect("pop overlay while its render owns capacity");
    assert!(!app.has_overlay());
    assert!(
        app.debug_compositor_transition_retry_pending(),
        "the rejected replacement must retain an owned retry"
    );
    assert!(app.debug_scheduled_output_pending_bytes() >= retained_overlay_bytes);
    assert!(app.debug_scheduled_output_pending_bytes() <= capacity);

    physical_output.release();
    app.notify_scheduled_output_writable();
    assert!(
        app.wants_tick(),
        "writable notification must resume the drain"
    );
    for _ in 0..4 {
        app.drain_scheduled_output(&mut physical_output, false)
            .expect("drain retired overlay work toward the retry boundary");
        if !app.debug_compositor_transition_retry_pending() {
            break;
        }
    }
    assert!(
        app.debug_scheduled_output_pending_bytes() > 0,
        "capacity drain should enqueue the authoritative underlay exactly once"
    );
    assert!(!app.debug_compositor_transition_retry_pending());
    assert!(app.wants_tick(), "the accepted retry must stay scheduled");
    app.drain_scheduled_output(&mut physical_output, false)
        .expect("flush the retried underlay");
    assert_eq!(app.debug_scheduled_output_pending_bytes(), 0);

    let mut physical = GhosttyEngine::new(24, 80).expect("physical oracle");
    physical.advance(&baseline).expect("parse baseline output");
    physical
        .advance(&physical_output.bytes)
        .expect("parse overlay followed by retried underlay");
    let contents = physical.normalized_snapshot().contents();
    assert!(
        contents.contains("stable-underlay"),
        "contents={contents:?}"
    );
    assert!(!contents.contains(OVERLAY_BODY), "contents={contents:?}");
}

#[test]
fn overlay_transition_supersedes_direct_incremental_damage_with_one_final_scene() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut physical = PresentationOutput::default();

    app.begin_pty_presentation_batch();
    app.handle_pty(&mut sr, b"stale incremental", &mut physical)
        .expect("model underlying output");
    app.show_message(
        &mut sr,
        "transition",
        "authoritative overlay",
        &mut physical,
    )
    .expect("switch to overlay within presentation batch");
    assert!(physical.bytes.is_empty());
    assert_eq!(physical.flushes, 0);

    app.finish_pty_presentation_batch(&mut physical)
        .expect("present authoritative overlay");
    assert_eq!(physical.flushes, 1);
    let presented = app
        .presented_scene()
        .clone()
        .into_terminal_snapshot()
        .contents_full();
    assert!(presented.contains("authoritative overlay"));
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
            .expect("model child Kitty keyboard-mode request");
        assert!(
            !term_out
                .windows(ENABLE_ALL_KITTY_KEYBOARD_FEATURES.len())
                .any(|window| window == ENABLE_ALL_KITTY_KEYBOARD_FEATURES),
            "child keyboard-mode control reached the outer terminal: {scenario}"
        );
        let mut physical = GhosttyEngine::new(24, 80).expect("Kitty mode oracle");
        physical
            .advance(&term_out)
            .expect("parse compositor output");
        assert_eq!(
            physical.normalized_snapshot().modes.kitty_keyboard_flags,
            31
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
        b"\x1B]133;A\x07$ first\r\n\x1B]133;C\x07one\r\n\x1B]133;D;0\x07\x1B]133;A\x07$ second\x1B]133;B\x07",
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
fn review_previous_line_stops_at_visible_top_when_scrollback_exists() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let output = (0..30)
        .map(|line| format!("history-{line:02}"))
        .collect::<Vec<_>>()
        .join("\r\n");
    app.handle_pty(&mut sr, output.as_bytes(), &mut term_out)
        .expect("render enough output to create scrollback");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).expect("finalize"));

    app.handle_stdin(&mut sr, b"\x1by", &mut pty_out, &mut term_out)
        .expect("move review cursor to visible top");
    recorder.inner.borrow_mut().speaks.clear();
    app.handle_stdin(&mut sr, b"\x1bu", &mut pty_out, &mut term_out)
        .expect("try to move above visible top");

    let spoken = recorder
        .inner
        .borrow()
        .speaks
        .iter()
        .map(|(text, _)| text.clone())
        .collect::<Vec<_>>();
    assert_eq!(spoken, ["top", "history-06"]);
    assert!(pty_out.is_empty());
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
fn popping_review_resets_hidden_updates_and_later_clear_does_not_restore_snapshot() {
    let (mut app, mut sr, _recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let mut terminal = GhosttyEngine::new_with_scrollback(24, 80, 0).expect("create render oracle");

    app.handle_pty(&mut sr, b"snapshot\x1B[H", &mut term_out)
        .expect("draw initial state");
    terminal.advance(&term_out).expect("advance render oracle");
    term_out.clear();
    app.handle_stdin(&mut sr, b"\x1Br", &mut pty_out, &mut term_out)
        .expect("open review");
    terminal.advance(&term_out).expect("advance render oracle");
    term_out.clear();

    app.handle_pty(&mut sr, b"\r\x1B[2Krunning one", &mut term_out)
        .expect("first hidden update");
    app.handle_pty(&mut sr, b"\r\x1B[2Krunning two", &mut term_out)
        .expect("second hidden update");
    assert!(term_out.is_empty());
    app.handle_stdin(&mut sr, b"\x0C", &mut pty_out, &mut term_out)
        .expect("ignore ctrl-l in review");
    assert!(pty_out.is_empty());
    terminal.advance(&term_out).expect("advance render oracle");
    term_out.clear();

    app.handle_stdin(&mut sr, b"q", &mut pty_out, &mut term_out)
        .expect("close review");
    terminal.advance(&term_out).expect("advance render oracle");
    term_out.clear();
    assert!(terminal.snapshot().contents().contains("running two"));
    assert!(!terminal.snapshot().contents().contains("snapshot"));

    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(!app.maybe_finalize_changes(&mut sr).expect("no stale batch"));

    app.handle_stdin(&mut sr, b"\x0C", &mut pty_out, &mut term_out)
        .expect("forward ctrl-l after review");
    assert_eq!(pty_out, b"\x0C");
    app.handle_pty(&mut sr, b"\x1B[2J\x1B[Hprompt", &mut term_out)
        .expect("application clears the screen");
    terminal.advance(&term_out).expect("advance render oracle");
    term_out.clear();
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).expect("finalize clear"));

    assert_eq!(terminal.snapshot().contents().trim(), "prompt");
    assert_eq!(app.debug_active_view_contents().trim(), "prompt");
    assert!(term_out.is_empty());
}

#[test]
fn terminal_update_remains_synchronized_when_review_opens_between_fragments() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"old screen\x1B[", &mut term_out)
        .expect("receive first terminal fragment");
    app.handle_stdin(&mut sr, b"\x1Br", &mut pty_out, &mut term_out)
        .expect("open review between fragments");
    recorder.inner.borrow_mut().speaks.clear();
    term_out.clear();

    app.handle_pty(&mut sr, b"2J\x1B[Hbackground", &mut term_out)
        .expect("complete hidden terminal update");
    assert!(term_out.is_empty());
    app.handle_stdin(&mut sr, b"q", &mut pty_out, &mut term_out)
        .expect("close review");

    assert!(!app.has_overlay());
    assert_eq!(app.debug_active_view_contents().trim(), "background");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("hidden update was finalized on restore")
    );
}

#[test]
fn popping_review_restores_hidden_alternate_screen_transitions() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let mut terminal = GhosttyEngine::new_with_scrollback(24, 80, 0).expect("create render oracle");
    terminal
        .advance(b"\x1b[?1049h")
        .expect("enter Lector-owned alternate screen");

    app.handle_pty(&mut sr, b"primary\x1B[H", &mut term_out)
        .expect("draw primary screen");
    terminal.advance(&term_out).expect("advance render oracle");
    term_out.clear();
    app.handle_stdin(&mut sr, b"\x1Br", &mut pty_out, &mut term_out)
        .expect("open review");
    terminal.advance(&term_out).expect("advance render oracle");
    term_out.clear();

    app.handle_pty(&mut sr, b"\x1B[?1049hfullscreen", &mut term_out)
        .expect("enter alternate screen behind review");
    assert!(term_out.is_empty());
    app.handle_stdin(&mut sr, b"q", &mut pty_out, &mut term_out)
        .expect("close review onto alternate screen");
    terminal.advance(&term_out).expect("advance render oracle");
    term_out.clear();
    assert!(terminal.snapshot().alternate_screen());
    assert!(terminal.snapshot().contents().contains("fullscreen"));

    app.handle_stdin(&mut sr, b"\x1Br", &mut pty_out, &mut term_out)
        .expect("review alternate screen");
    terminal.advance(&term_out).expect("advance render oracle");
    term_out.clear();
    app.handle_pty(&mut sr, b"\x1B[?1049l\x1B[2J\x1B[Hshell", &mut term_out)
        .expect("leave alternate screen behind review");
    assert!(term_out.is_empty());
    app.handle_stdin(&mut sr, b"q", &mut pty_out, &mut term_out)
        .expect("close review onto primary screen");
    terminal.advance(&term_out).expect("advance render oracle");

    assert!(terminal.snapshot().alternate_screen());
    assert_eq!(terminal.snapshot().contents().trim(), "shell");
    assert!(pty_out.is_empty());
}

#[test]
fn popping_review_restores_the_authoritative_pen_style_before_compositor_output_resumes() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let mut oracle = GhosttyEngine::new_with_scrollback(24, 80, 0).expect("create render oracle");
    let mut profile = PhysicalTerminalProfile::conservative(
        lector::terminal::TerminalGeometry::from_cells(24, 80),
    );
    profile.hyperlinks = true;
    app.set_physical_profile(profile);

    app.handle_pty(
        &mut sr,
        b"\x1B[1;31m\x1B]8;;https://example.test/active\x1B\\A\x1B[?1h\x1B[?2004h",
        &mut term_out,
    )
    .expect("draw styled source");
    oracle.advance(&term_out).expect("advance render oracle");
    term_out.clear();

    app.handle_stdin(&mut sr, b"\x1Br", &mut pty_out, &mut term_out)
        .expect("open review");
    oracle.advance(&term_out).expect("render review");
    term_out.clear();
    app.handle_stdin(&mut sr, b"q", &mut pty_out, &mut term_out)
        .expect("close review");
    oracle.advance(&term_out).expect("restore source view");
    term_out.clear();

    app.handle_pty(&mut sr, b"B", &mut term_out)
        .expect("resume styled compositor output");
    let intended = PresentedScene::compose(&app.composed_scene().expect("compose source scene"))
        .expect("present source scene")
        .into_terminal_snapshot();
    assert_eq!(
        intended.cell(0, 1).unwrap().hyperlink.as_deref(),
        Some("https://example.test/active"),
        "the authoritative source model must retain the active OSC 8 link"
    );
    oracle.advance(&term_out).expect("advance resumed output");

    let actual = oracle.normalized_snapshot();
    assert_eq!(actual.cell(0, 1).unwrap().grapheme, "B");
    assert_eq!(
        actual.cell(0, 1).unwrap().style.foreground,
        Color::Indexed(1)
    );
    assert!(actual.cell(0, 1).unwrap().style.bold);
    assert_eq!(
        actual.cell(0, 1).unwrap().hyperlink.as_deref(),
        Some("https://example.test/active")
    );
    assert_eq!(actual.cursor_position(), (0, 2));
    assert!(actual.modes.application_cursor);
    assert!(actual.modes.bracketed_paste);
    assert!(pty_out.is_empty());
}

#[test]
fn phase_two_baseline_preserves_full_overlay_redraw_title_cursor_and_modes() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let source = b"\x1b]2;editor title\x07\x1b[2;4Hroot\x1b[?25l\x1b[?1h\x1b[?2004h";
    let mut expected = GhosttyEngine::new_with_scrollback(24, 80, 0).expect("create expected root");
    expected.advance(source).expect("construct expected root");
    let expected = expected.normalized_snapshot();
    let mut physical = GhosttyEngine::new_with_scrollback(24, 80, 0).expect("create outer oracle");

    app.handle_pty(&mut sr, source, &mut term_out)
        .expect("draw root");
    physical.advance(&term_out).expect("advance root output");
    term_out.clear();

    app.show_message(&mut sr, "Notice", "foreground", &mut term_out)
        .expect("push message overlay");
    physical.advance(&term_out).expect("advance overlay redraw");
    assert!(physical.snapshot().contents().contains("foreground"));
    assert_eq!(physical.snapshot().title.as_deref(), Some("editor title"));
    assert!(
        recorder
            .inner
            .borrow()
            .speaks
            .iter()
            .any(|(text, _)| text == "Notice")
    );

    term_out.clear();
    app.handle_stdin(&mut sr, b"\r", &mut pty_out, &mut term_out)
        .expect("pop message overlay");
    physical.advance(&term_out).expect("restore root redraw");
    let actual = physical.normalized_snapshot();
    let mut expected = expected;
    expected.modes.focus_reporting = true;
    assert_eq!(actual.contents_full(), expected.contents_full());
    assert_eq!(actual.cursor, expected.cursor);
    assert_eq!(actual.modes, expected.modes);
    assert_eq!(actual.title, expected.title);
    assert!(pty_out.is_empty());
}

#[test]
fn review_overlay_uses_normal_cursor_tracking_for_horizontal_motions() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    app.handle_pty(&mut sr, b"abcd\x1B[H", &mut term_out)
        .expect("draw source");
    app.handle_stdin(&mut sr, b"\x1Br", &mut pty_out, &mut term_out)
        .expect("open review");
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"llh", &mut pty_out, &mut term_out)
        .expect("move the visible review cursor");

    let spoken = recorder
        .inner
        .borrow()
        .speaks
        .iter()
        .map(|(text, _)| text.clone())
        .collect::<Vec<_>>();
    assert_eq!(spoken, ["b", "c", "b"]);
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
fn overlay_redraws_keep_the_physical_terminal_in_sync_with_the_composed_scene() {
    fn assert_physical_scene(app: &mut App, physical: &GhosttyEngine, context: &str) {
        let intended = PresentedScene::compose(&app.composed_scene().expect("compose scene"))
            .expect("present scene")
            .into_terminal_snapshot();
        let actual = physical.normalized_snapshot();
        assert_eq!(
            actual.contents_full(),
            intended.contents_full(),
            "{context}"
        );
        assert_eq!(actual.cursor, intended.cursor, "{context}");
        assert_eq!(actual.modes, intended.modes, "{context}");
    }

    fn present(
        app: &mut App,
        clock: &FakeClock,
        output: &mut Vec<u8>,
        physical: &mut GhosttyEngine,
        context: &str,
    ) {
        clock.advance_ms(4);
        let report = app
            .drain_scheduled_output(output, false)
            .unwrap_or_else(|error| panic!("{context}: {error}"));
        assert!(!report.blocked, "{context}");
        physical
            .advance(output)
            .unwrap_or_else(|error| panic!("{context}: {error}"));
        output.clear();
    }

    let (mut app, mut sr, _recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig::default());
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let mut physical = GhosttyEngine::new(24, 80).expect("create physical oracle");

    app.handle_pty(
        &mut sr,
        b"python first\r\nother line\r\npython last\x1b[24;1H",
        &mut term_out,
    )
    .expect("draw source with the application cursor at the bottom");
    present(
        &mut app,
        &clock,
        &mut term_out,
        &mut physical,
        "present source",
    );

    app.handle_stdin(&mut sr, b"\x1br", &mut pty_out, &mut term_out)
        .expect("open review");
    present(
        &mut app,
        &clock,
        &mut term_out,
        &mut physical,
        "present review",
    );

    app.handle_stdin(&mut sr, b"?python\r", &mut pty_out, &mut term_out)
        .expect("search backward in review");
    present(
        &mut app,
        &clock,
        &mut term_out,
        &mut physical,
        "present search result",
    );
    assert_physical_scene(&mut app, &physical, "completed review search");
    assert!(
        !physical
            .normalized_snapshot()
            .contents_full()
            .contains("?python"),
        "the completed prompt remained visible"
    );

    app.handle_stdin(&mut sr, b"q", &mut pty_out, &mut term_out)
        .expect("close review");
    present(
        &mut app,
        &clock,
        &mut term_out,
        &mut physical,
        "restore source",
    );
    assert_physical_scene(&mut app, &physical, "review dismissal");

    app.handle_stdin(&mut sr, b"\x1bL3 + 3\r", &mut pty_out, &mut term_out)
        .expect("open the Lua REPL and submit an expression");
    present(
        &mut app,
        &clock,
        &mut term_out,
        &mut physical,
        "present submitted expression",
    );
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .expect("finish Lua evaluation");
    present(
        &mut app,
        &clock,
        &mut term_out,
        &mut physical,
        "present Lua result",
    );
    assert_physical_scene(&mut app, &physical, "Lua evaluation result");
    assert!(
        physical
            .normalized_snapshot()
            .contents_full()
            .lines()
            .any(|line| line.trim() == "6"),
        "the Lua result was modeled but not visible"
    );
    assert!(pty_out.is_empty());
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
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        ..OutputSchedulerConfig::default()
    });
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    app.handle_pty(
        &mut sr,
        b"\x1B[2;1H\x1B[2m\xe2\x96\x8c\x1B[0m\x1B[1;1H\x1B]133;A\x07user@host$ \x1B]133;B\x07old",
        &mut term_out,
    )
    .expect("render editable prompt");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present editable prompt");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize prompt")
    );
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_pty(
        &mut sr,
        b"\x1B7\x1B[2;1H\x1B[2;4m\xe2\x96\x8c\x1B[0m\x1B8",
        &mut term_out,
    )
    .expect("queue a pre-input prompt repaint");
    let mut writer = FlushGateWriter {
        block_flush: true,
        ..FlushGateWriter::default()
    };
    app.drain_scheduled_output(&mut writer, false)
        .expect("write the pre-input repaint up to its flush fence");

    app.handle_stdin(&mut sr, b"\x1B[A", &mut pty_out, &mut term_out)
        .expect("forward history up");
    app.handle_pty(
        &mut sr,
        b"\r\x1B[Kuser@host$ recalled command",
        &mut term_out,
    )
    .expect("render Readline history selection");

    writer.block_flush = false;
    writer.block_writes = true;
    app.drain_scheduled_output(&mut writer, true)
        .expect("flush only the pre-input repaint");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize the pre-input receipt")
    );
    assert!(recorder.inner.borrow().speaks.is_empty());

    writer.block_writes = false;
    app.drain_scheduled_output(&mut writer, true)
        .expect("present the Readline redraw");
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
fn readline_history_repaint_reads_the_complete_soft_wrapped_cursor_line() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    app.on_resize(8, 20, &mut term_out)
        .expect("resize the terminal");
    app.handle_pty(&mut sr, b"P> ", &mut term_out)
        .expect("render a prompt without semantic markers");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize prompt")
    );
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\x1B[A", &mut pty_out, &mut term_out)
        .expect("recall the newer wrapped entry");
    app.handle_pty(
        &mut sr,
        b"bravo red blue green white black orange purple gray",
        &mut term_out,
    )
    .expect("render the newer wrapped entry");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize the newer entry")
    );
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\x1B[A", &mut pty_out, &mut term_out)
        .expect("recall the older wrapped entry");
    // Byte-for-byte Readline 8.3 redraw at 20 columns: it edits all three
    // physical rows, then leaves the hardware cursor on the final one.
    app.handle_pty(
        &mut sr,
        b"\x1B[A\x1B[A\r\x1B[C\x1B[C\x1B[Calpha one two th\r\n\r\x1B[C\x1B[C four five six seve\x1B[1Pn eight nine",
        &mut term_out,
    )
    .expect("render the older wrapped entry");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize the older entry")
    );

    assert_eq!(pty_out, b"\x1B[A\x1B[A");
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[(
            "P greater  alpha one two three four five six seven eight nine".into(),
            false,
        )]
    );

    recorder.inner.borrow_mut().speaks.clear();
    app.handle_stdin(&mut sr, b"\x1B[B", &mut pty_out, &mut term_out)
        .expect("return to the newer wrapped entry");
    app.handle_pty(
        &mut sr,
        b"\x1B[A\x1B[A\r\x1B[C\x1B[C\x1B[Cbravo red blue g\r\n\r\x1B[C\x1B[Cn white black orange purple gray",
        &mut term_out,
    )
    .expect("redraw the newer wrapped entry");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize the newer entry again")
    );

    assert_eq!(pty_out, b"\x1B[A\x1B[A\x1B[B");
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[(
            "P greater  bravo red blue green white black orange purple gray".into(),
            false,
        )]
    );
}

#[test]
fn unwrapped_multirow_interface_repaint_reads_its_stable_diff() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(
        &mut sr,
        b"\x1B[HMenu old\x1B[2;1HPanel old\x1B[3;1HStatus wait",
        &mut term_out,
    )
    .expect("render an unwrapped interface");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize the initial interface")
    );
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\x1B[A", &mut pty_out, &mut term_out)
        .expect("forward interface navigation");
    app.handle_pty(
        &mut sr,
        b"\x1B[HMenu new\x1B[2;1HPanel new\x1B[3;1HStatus done",
        &mut term_out,
    )
    .expect("repaint the unwrapped interface");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize the interface repaint")
    );

    assert_eq!(pty_out, b"\x1B[A");
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[("Menu new\n\nPanel new\n\nStatus done".into(), false)]
    );
}

#[test]
fn cursor_addressed_transcript_growth_reads_the_inserted_response() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(
        &mut sr,
        b"\x1B[?2026h\x1B[2J\x1B[HYou: explain repainting\x1B[4;1HWorking 1s\x1B[5;1H> \x1B[5;3H\x1B[?2026l",
        &mut term_out,
    )
    .expect("render a cursor-addressed conversation");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize the conversation")
    );
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\r", &mut pty_out, &mut term_out)
        .expect("submit the prompt");
    app.handle_pty(
        &mut sr,
        b"\x1B[?2026h\x1B[2J\x1B[HYou: explain repainting\x1B[2;1HClaude:\x1B[3;1HThe response starts here.\x1B[4;1HIt continues on this row\x1B[6;1HWorking 1s\x1B[7;1H> \x1B[7;3H\x1B[?2026l",
        &mut term_out,
    )
    .expect("paint assistant output above the stable prompt cursor");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize the assistant response")
    );

    assert_eq!(pty_out, b"\r");
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[(
            "Claude:\n\nThe response starts here.\n\nIt continues on this row".into(),
            false,
        )]
    );

    recorder.inner.borrow_mut().speaks.clear();
    app.handle_pty(
        &mut sr,
        b"\x1B[?2026h\x1B[4;1HIt continues on this row with more detail.\x1B[6;1HWorking 2s\x1B[7;3H\x1B[?2026l",
        &mut term_out,
    )
    .expect("stream more response text alongside a status repaint");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize the streamed response")
    );
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[("with more detail.".into(), false)]
    );
}

#[test]
fn newly_opened_primary_screen_interface_reads_its_bounded_region() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(
        &mut sr,
        b"\x1B[10;1HAsk for input\x1B[12;1Hcurrent status\x1B[10;1H",
        &mut term_out,
    )
    .expect("render the underlying interface");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize the underlying interface")
    );
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\r", &mut pty_out, &mut term_out)
        .expect("confirm opening the modal");
    app.handle_pty(
        &mut sr,
        b"\x1B[?2026h\x1B[10;1H\x1B[JSelect model\x1B[11;1HChoose one\x1B[13;1HFirst choice\x1B[14;1HSecond choice\x1B[16;1HPress enter to confirm\x1B[?25l\x1B[?2026l",
        &mut term_out,
    )
    .expect("open a bounded multi-row modal in the primary screen");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize the modal")
    );

    assert_eq!(pty_out, b"\r");
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[(
            "Select model\nChoose one\n\nFirst choice\nSecond choice\n\nPress enter to confirm"
                .into(),
            false,
        )]
    );
}

#[test]
fn wrapped_history_selection_is_not_suppressed_by_stale_printable_echo() {
    let (mut app, mut sr, recorder, clock) = make_app();
    sr.set_suppress_key_echo(true);
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    app.on_resize(8, 20, &mut term_out)
        .expect("resize the terminal");
    app.handle_pty(&mut sr, b"P> ", &mut term_out)
        .expect("render a prompt without semantic markers");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize prompt")
    );
    recorder.inner.borrow_mut().speaks.clear();

    // The typed x has reached the model but not an accessibility commit when
    // Up replaces it. Since the recalled command also ends in x, generic echo
    // suffix matching must not consume the history announcement.
    app.handle_stdin(&mut sr, b"x", &mut pty_out, &mut term_out)
        .expect("type a draft suffix");
    app.handle_pty(&mut sr, b"x", &mut term_out)
        .expect("echo the uncommitted draft suffix");
    app.handle_stdin(&mut sr, b"\x1B[A", &mut pty_out, &mut term_out)
        .expect("recall wrapped history");
    app.handle_pty(
        &mut sr,
        b"\r\x1B[2KP> alpha one two three four five six seven eight nine x",
        &mut term_out,
    )
    .expect("replace the draft with wrapped history");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize wrapped history")
    );

    assert_eq!(pty_out, b"x\x1B[A");
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[(
            "P greater  alpha one two three four five six seven eight nine x".into(),
            false,
        )]
    );
}

#[test]
fn visual_focus_transfer_precedes_a_stale_shell_input_marker() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    app.handle_pty(&mut sr, b"\x1B]133;A\x07$ \x1B]133;B\x07old", &mut term_out)
        .expect("render editable prompt");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize prompt")
    );

    app.handle_stdin(&mut sr, b"\x12", &mut pty_out, &mut term_out)
        .expect("forward history search key");
    app.handle_pty(
        &mut sr,
        b"\x1B[2J\x1B[H\x1B[1m\xe2\x96\x8c\x1B[0m \x1B[1;7mAlpha\x1B[0m\x1B[2;1H\x1B[2m\xe2\x96\x8c\x1B[0m Bravo\x1B[3;1H\x1B[2m\xe2\x96\x8c\x1B[0m Delta\x1B[4;1H\x1B[2m\xe2\x96\x8c\x1B[0m Gamma\x1B[6;3H\x1B[?25h",
        &mut term_out,
    )
    .expect("render temporary history interface");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize history interface")
    );
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\x1B[A", &mut pty_out, &mut term_out)
        .expect("forward interface navigation");
    app.handle_pty(
        &mut sr,
        b"\x1B[H\x1B[2m\xe2\x96\x8c\x1B[0m Alpha\x1B[2;1H\x1B[1m\xe2\x96\x8c\x1B[0m \x1B[1;7mBravo\x1B[0m\x1B[6;3H",
        &mut term_out,
    )
    .expect("move visual focus");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize visual focus move")
    );

    assert_eq!(pty_out, b"\x12\x1B[A");
    let spoken: Vec<_> = recorder
        .inner
        .borrow()
        .speaks
        .iter()
        .map(|(text, _)| text.clone())
        .collect();
    assert_eq!(spoken, ["Bravo"]);
}

#[test]
fn stale_shell_input_waits_for_the_visual_focus_keys_presented_frame() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        ..OutputSchedulerConfig::default()
    });
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"\x1B]133;A\x07$ \x1B]133;B\x07old", &mut term_out)
        .expect("queue editable prompt");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present editable prompt");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize prompt")
    );

    app.handle_stdin(&mut sr, b"\x12", &mut pty_out, &mut term_out)
        .expect("forward history search key");
    app.handle_pty(
        &mut sr,
        b"\x1B[2J\x1B[H\x1B[1m\xe2\x96\x8c\x1B[0m \x1B[1;7mAlpha\x1B[0m\x1B[2;1H\x1B[2m\xe2\x96\x8c\x1B[0m Bravo\x1B[3;1H\x1B[2m\xe2\x96\x8c\x1B[0m Delta\x1B[4;1H\x1B[2m\xe2\x96\x8c\x1B[0m Gamma\x1B[6;3H\x1B[?25h",
        &mut term_out,
    )
    .expect("queue temporary history interface");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present temporary history interface");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize history interface")
    );
    recorder.inner.borrow_mut().speaks.clear();

    // This harmless style repaint was modeled before Up and cannot prove
    // how that key should be interpreted, even though its receipt comes later.
    app.handle_pty(
        &mut sr,
        b"\x1B7\x1B[3;1H\x1B[2;4m\xe2\x96\x8c\x1B[0m\x1B8",
        &mut term_out,
    )
    .expect("queue a pre-input repaint");
    let mut writer = FlushGateWriter {
        block_flush: true,
        ..FlushGateWriter::default()
    };
    app.drain_scheduled_output(&mut writer, false)
        .expect("write the pre-input repaint up to its flush fence");

    app.handle_stdin(&mut sr, b"\x1B[A", &mut pty_out, &mut term_out)
        .expect("forward interface navigation");
    app.handle_pty(
        &mut sr,
        b"\x1B[H\x1B[2m\xe2\x96\x8c\x1B[0m Alpha\x1B[2;1H\x1B[1m\xe2\x96\x8c\x1B[0m \x1B[1;7mBravo\x1B[0m\x1B[6;3H",
        &mut term_out,
    )
    .expect("queue the causally later visual focus frame");

    writer.block_flush = false;
    writer.block_writes = true;
    app.drain_scheduled_output(&mut writer, true)
        .expect("flush only the pre-input repaint");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize the pre-input receipt")
    );
    assert!(recorder.inner.borrow().speaks.is_empty());

    recorder.inner.borrow_mut().speaks.clear();
    writer.block_writes = false;
    app.drain_scheduled_output(&mut writer, true)
        .expect("present the visual focus frame");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize the visual focus receipt")
    );

    assert_eq!(pty_out, b"\x12\x1B[A");
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[("Bravo".into(), false)]
    );
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
fn coordinate_clicks_use_the_live_application_protocol_during_a_held_frame() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    app.enable_output_scheduler(OutputSchedulerConfig::default());

    app.handle_pty(
        &mut sr,
        b"\x1B[?2026h\x1B[?1000h\x1B[?1006h\x1B[5;8Hpartial",
        &mut term_out,
    )
    .expect("open a working frame which enables mouse reporting");
    app.handle_stdin(&mut sr, b"\x1B{", &mut pty_out, &mut term_out)
        .expect("send a coordinate-dependent click");

    assert_eq!(pty_out, b"\x1B[<0;1;1M\x1B[<0;1;1m");
}

#[test]
fn left_click_action_places_the_review_overlay_application_cursor_locally() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    app.handle_pty(&mut sr, b"abcdef\x1B[H", &mut term_out)
        .expect("draw source");
    app.handle_stdin(&mut sr, b"\x1Br", &mut pty_out, &mut term_out)
        .expect("open review");

    app.handle_stdin(&mut sr, b"\x1B.", &mut pty_out, &mut term_out)
        .expect("move review cursor right");
    app.handle_stdin(&mut sr, b"\x1B.", &mut pty_out, &mut term_out)
        .expect("move review cursor right again");
    recorder.inner.borrow_mut().speaks.clear();
    app.handle_stdin(&mut sr, b"\x1B{", &mut pty_out, &mut term_out)
        .expect("place application cursor with left click action");
    app.handle_stdin(&mut sr, b"l", &mut pty_out, &mut term_out)
        .expect("continue vi navigation from placed cursor");

    let spoken = recorder
        .inner
        .borrow()
        .speaks
        .iter()
        .map(|(text, _)| text.clone())
        .collect::<Vec<_>>();
    assert_eq!(spoken, ["c", "d"]);
    assert!(pty_out.is_empty());
}

#[test]
fn right_click_action_keeps_its_existing_behavior_in_review() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    app.handle_pty(&mut sr, b"abcdef\x1B[H", &mut term_out)
        .expect("draw source");
    app.handle_stdin(&mut sr, b"\x1Br", &mut pty_out, &mut term_out)
        .expect("open review");
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\x1B}", &mut pty_out, &mut term_out)
        .expect("invoke right click action");

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
    let mut physical = GhosttyEngine::new(24, 80).expect("physical oracle");
    physical
        .advance(&term_out)
        .expect("parse compositor output");
    assert!(physical.normalized_snapshot().contents().contains("hello"));

    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    let _ = app.maybe_finalize_changes(&mut sr).expect("finalize");

    let speaks = &recorder.inner.borrow().speaks;
    assert!(speaks.iter().any(|(text, _)| text.contains("hello")));
}

#[test]
fn complete_linear_output_reads_immediately_without_waiting_for_quiet() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"delayed", &mut term_out)
        .expect("receive line text");
    clock.advance_ms(25);
    app.handle_pty(&mut sr, b" line\r", &mut term_out)
        .expect("receive split carriage return");
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("an unterminated record remains pending")
    );

    clock.advance_ms(25);
    app.handle_pty(&mut sr, b"\n", &mut term_out)
        .expect("receive split line feed");
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("the complete record finalizes immediately")
    );
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        [("delayed line".into(), false)]
    );
}

#[test]
fn readline_wrapped_linear_output_reads_immediately() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut term_out = Vec::new();

    for line in ["first edbrowse line", "second edbrowse line"] {
        let update = format!("\x1b[?2004h\r\n\x1b[?2004l\r{line}\r\n\x1b[?2004h");
        app.handle_pty(&mut sr, update.as_bytes(), &mut term_out)
            .expect("receive readline-wrapped line output");
        assert!(
            app.maybe_finalize_changes(&mut sr)
                .expect("readline wrapper does not delay the completed record")
        );
    }

    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        [
            ("first edbrowse line".into(), false),
            ("second edbrowse line".into(), false),
        ]
    );
}

#[test]
fn trailing_partial_and_structural_output_keep_the_stabilization_fallback() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"complete\r\npartial", &mut term_out)
        .expect("receive a complete record plus a partial suffix");
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("the partial suffix prevents fast finalization")
    );
    clock.advance_ms(u128::from(DIFF_DELAY));
    assert!(app.maybe_finalize_changes(&mut sr).expect("quiet fallback"));
    assert!(
        recorder
            .inner
            .borrow()
            .speaks
            .iter()
            .any(|(text, _)| text.contains("complete") && text.contains("partial"))
    );

    recorder.inner.borrow_mut().speaks.clear();
    app.handle_pty(&mut sr, b"\x1b[2J\x1b[Hscreen row\r\n", &mut term_out)
        .expect("receive structural redraw");
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("structural output remains timer-driven")
    );
}

#[test]
fn complete_linear_output_waits_for_its_physical_render_receipt() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        ..OutputSchedulerConfig::default()
    });
    let mut term_out = Vec::new();

    let line = format!("{}end", "presented-line-".repeat(8));
    let output = format!("{line}\r\n");
    app.handle_pty(&mut sr, output.as_bytes(), &mut term_out)
        .expect("queue complete line");
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("unpresented output is inaccessible")
    );

    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the line");
    assert_eq!(
        app.scheduled_output_timeout(),
        Some(std::time::Duration::ZERO)
    );
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("presented complete line finalizes immediately")
    );
    assert_eq!(recorder.inner.borrow().speaks.as_slice(), [(line, false)]);
}

#[test]
fn parser_continuation_uses_only_the_hard_stabilization_deadline() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        ..OutputSchedulerConfig::default()
    });
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"visible\x1b[", &mut term_out)
        .expect("queue text followed by an incomplete CSI");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the text preceding the parser continuation");

    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("an ordinary quiet interval must not publish a parser continuation")
    );
    assert!(recorder.inner.borrow().speaks.is_empty());

    clock.advance_ms(u128::from(MAX_DIFF_DELAY).saturating_sub(clock.now_ms()));
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("the hard streaming deadline releases an abandoned continuation")
    );
    assert!(
        recorder
            .inner
            .borrow()
            .speaks
            .iter()
            .any(|(text, _)| text.contains("visible"))
    );
}

#[test]
fn structural_and_title_only_bursts_do_not_train_the_adaptive_quiet_window() {
    let mut shortened_profiles = Vec::new();
    for title_only in [false, true] {
        let (mut app, mut sr, recorder, clock) = make_app();
        app.enable_output_scheduler(OutputSchedulerConfig {
            latency_budget_ms: 0,
            ..OutputSchedulerConfig::default()
        });
        let mut term_out = Vec::new();

        // Establish a trainable ordinary finalization immediately before the
        // first non-training update. A title or structural record in the late
        // continuation window must not raise the learned delay merely because
        // its timestamp is close to that prior output.
        app.handle_pty(&mut sr, b"x", &mut term_out)
            .expect("queue the ordinary seed burst");
        app.drain_scheduled_output(&mut term_out, false)
            .expect("present the ordinary seed burst");
        clock.advance_ms(u128::from(DIFF_DELAY) + 1);
        assert!(
            app.maybe_finalize_changes(&mut sr)
                .expect("finalize the ordinary seed burst")
        );
        clock.advance_ms(1);

        for index in 0..36 {
            let output = if title_only {
                format!("\x1b]2;title-{index}\x1b\\")
            } else {
                format!("\r\x1b[2Kstructural-{index}")
            };
            app.handle_pty(&mut sr, output.as_bytes(), &mut term_out)
                .expect("queue a non-training burst");
            app.drain_scheduled_output(&mut term_out, false)
                .expect("present a non-training burst");
            clock.advance_ms(u128::from(DIFF_DELAY) + 1);
            assert!(
                app.maybe_finalize_changes(&mut sr)
                    .expect("finalize a non-training burst"),
                "burst {index} did not finalize (title_only={title_only})"
            );
        }

        recorder.inner.borrow_mut().speaks.clear();
        app.handle_pty(&mut sr, b"x", &mut term_out)
            .expect("queue an ordinary partial record after non-training bursts");
        app.drain_scheduled_output(&mut term_out, false)
            .expect("present the ordinary partial record");
        clock.advance_ms(u128::from(DIFF_DELAY - 1));
        if app
            .maybe_finalize_changes(&mut sr)
            .expect("retain the initial quiet window")
        {
            shortened_profiles.push(if title_only {
                "title-only"
            } else {
                "structural"
            });
            continue;
        }
        clock.advance_ms(1);
        assert!(
            app.maybe_finalize_changes(&mut sr)
                .expect("finalize at the unchanged quiet deadline")
        );
    }
    assert!(
        shortened_profiles.is_empty(),
        "non-training bursts shortened these quiet-window profiles: {shortened_profiles:?}"
    );
}

#[test]
fn repeated_identical_complete_lines_use_print_provenance() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut term_out = Vec::new();

    for _ in 0..24 {
        app.handle_pty(&mut sr, b"same\r\n", &mut term_out)
            .expect("print repeated line");
        assert!(
            app.maybe_finalize_changes(&mut sr)
                .expect("complete repeated line finalizes")
        );
    }

    assert_eq!(
        recorder
            .inner
            .borrow()
            .speaks
            .iter()
            .filter(|(text, _)| text == "same")
            .count(),
        24
    );
}

#[test]
fn blank_complete_record_finalizes_without_speech() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"\r\n", &mut term_out)
        .expect("print blank record");
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("blank record finalizes immediately")
    );
    assert!(recorder.inner.borrow().speaks.is_empty());
}

#[test]
fn readline_wrapped_blank_record_is_handled_silently_after_input() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"preceding line\r\n", &mut term_out)
        .expect("draw preceding line");
    assert!(app.maybe_finalize_changes(&mut sr).expect("finalize line"));
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\r", &mut pty_out, &mut term_out)
        .expect("submit blank readline input");
    app.handle_pty(
        &mut sr,
        b"\x1b[?2004h\r\n\x1b[?2004l\r\r\n\x1b[?2004h",
        &mut term_out,
    )
    .expect("receive blank application record");
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("blank record finalizes immediately")
    );
    assert!(recorder.inner.borrow().speaks.is_empty());
}

#[test]
fn completed_record_prefix_does_not_validate_against_stale_line_suffix() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"foobar\r", &mut term_out)
        .expect("draw baseline with cursor reset");
    clock.advance_ms(u128::from(DIFF_DELAY));
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize baseline")
    );
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_pty(&mut sr, b"\rfoo\r\n", &mut term_out)
        .expect("write only a prefix of the existing line");
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("stale suffix rejects the completed-record fast path")
    );
    assert!(recorder.inner.borrow().speaks.is_empty());
}

#[test]
fn overlapping_stale_suffix_cannot_validate_a_completed_record() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"abab\r", &mut term_out)
        .expect("draw overlapping baseline with cursor reset");
    clock.advance_ms(u128::from(DIFF_DELAY));
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize baseline")
    );
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_pty(&mut sr, b"ab\r\n", &mut term_out)
        .expect("write a prefix with an overlapping stale occurrence");
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("overlapping stale text rejects the fast path")
    );
    assert!(recorder.inner.borrow().speaks.is_empty());
}

#[test]
fn stale_post_linefeed_cursor_row_cannot_validate_a_completed_record() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"abab\r\nab\x1b[H", &mut term_out)
        .expect("draw two rows and home the cursor");
    clock.advance_ms(u128::from(DIFF_DELAY));
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize baseline")
    );
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_pty(&mut sr, b"ab\r\n", &mut term_out)
        .expect("write a stale prefix before landing on an identical row");
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("the post-LF cursor row cannot validate the prior record")
    );
    assert!(recorder.inner.borrow().speaks.is_empty());
}

#[test]
fn spaces_only_record_does_not_bypass_stale_suffix_validation() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"foobar\r", &mut term_out)
        .expect("draw baseline with cursor reset");
    clock.advance_ms(u128::from(DIFF_DELAY));
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize baseline")
    );
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_pty(&mut sr, b"   \r\n", &mut term_out)
        .expect("overwrite only a whitespace prefix of the existing line");
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("spaces still require physical tail validation")
    );
    assert!(recorder.inner.borrow().speaks.is_empty());
}

#[test]
fn structural_taint_survives_a_later_plain_line_fragment() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"old", &mut term_out)
        .expect("draw baseline");
    clock.advance_ms(u128::from(DIFF_DELAY));
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize baseline")
    );
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_pty(&mut sr, b"\x1b[2J\x1b[H", &mut term_out)
        .expect("start structural redraw");
    clock.advance_ms(25);
    app.handle_pty(&mut sr, b"final row\r\n", &mut term_out)
        .expect("finish redraw with a line-like fragment");
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("earlier structural activity taints the full burst")
    );
    assert!(recorder.inner.borrow().speaks.is_empty());

    clock.advance_ms(u128::from(MAX_DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).expect("diff fallback"));
    assert!(
        recorder
            .inner
            .borrow()
            .speaks
            .iter()
            .any(|(text, _)| text.contains("final row"))
    );
}

#[test]
fn structural_redraw_discards_transient_print_report_at_fallback() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"transient\x1b[2Kfinal\r\n", &mut term_out)
        .expect("receive a redraw containing overwritten print provenance");
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("structural redraw remains timer-driven")
    );

    clock.advance_ms(u128::from(MAX_DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).expect("diff fallback"));
    let speaks = &recorder.inner.borrow().speaks;
    assert!(speaks.iter().any(|(text, _)| text.contains("final")));
    assert!(speaks.iter().all(|(text, _)| !text.contains("transient")));
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
fn fragmented_cursor_update_survives_auto_read_toggle_between_fragments() {
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
    sr.set_auto_read_enabled(false);
    app.handle_pty(&mut sr, b"\x1B[", &mut term_out)
        .expect("receive partial cursor sequence");
    sr.set_auto_read_enabled(true);
    app.handle_pty(&mut sr, b"9D\x1B[Kcat -frobnicate-mode", &mut term_out)
        .expect("complete cursor sequence and redraw");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    app.maybe_finalize_changes(&mut sr)
        .expect("finalize fragmented redraw");

    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        [("nicate-mode".into(), false)]
    );
}

#[test]
fn stationary_application_repaint_preserves_a_manually_moved_review_cursor() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(
        &mut sr,
        b"review target\r\nactive prompt\x1B[2;14H",
        &mut term_out,
    )
    .expect("draw output above an active prompt");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize initial prompt")
    );
    app.handle_stdin(&mut sr, b"\x1Bu", &mut pty_out, &mut term_out)
        .expect("move review cursor above the application cursor");
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_pty(
        &mut sr,
        b"\x1B[2;1Hactive prompt .\x1B[K\x1B[2;14H",
        &mut term_out,
    )
    .expect("redraw activity in place without moving the application cursor");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize stationary prompt repaint")
    );
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\x1Bi", &mut pty_out, &mut term_out)
        .expect("read manually selected line after stationary repaint");
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[("review target".into(), false)]
    );
}

#[test]
fn alternate_screen_transition_realigns_review_cursor_even_at_the_same_position() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"outer screen\x1B[1;43H", &mut term_out)
        .expect("draw outer screen");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize outer screen")
    );
    app.handle_stdin(&mut sr, b"\x1Bo", &mut pty_out, &mut term_out)
        .expect("move review cursor to a blank row");
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_pty(
        &mut sr,
        b"\x1B[?1049h\x1B[2J\x1B[Hnested screen\x1B[1;43H",
        &mut term_out,
    )
    .expect("enter an alternate screen without changing the final cursor position");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize alternate-screen transition")
    );
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[("nested screen".into(), false)]
    );
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\x1Bi", &mut pty_out, &mut term_out)
        .expect("read the new alternate screen");
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[("nested screen".into(), false)]
    );
}

#[test]
fn alternate_screen_entry_reads_the_settled_view_and_primary_restore_reads_its_cursor_line() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        ..OutputSchedulerConfig::default()
    });
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"primary line", &mut term_out)
        .expect("draw primary screen");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the primary screen");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).unwrap());
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_pty(&mut sr, b"\x1b[?1049h\x1b[2J\x1b[H", &mut term_out)
        .expect("enter a still-blank alternate screen");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the blank alternate screen");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).unwrap());
    assert!(recorder.inner.borrow().speaks.is_empty());

    app.handle_pty(
        &mut sr,
        b"not the cursor line\r\ncursor line",
        &mut term_out,
    )
    .expect("draw the stabilized alternate screen");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the populated alternate screen");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).unwrap());
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[("not the cursor line\ncursor line".into(), false)]
    );

    recorder.inner.borrow_mut().speaks.clear();
    app.handle_pty(&mut sr, b"\x1b[?1049l", &mut term_out)
        .expect("leave the alternate screen");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the restored primary screen");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).unwrap());
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[("primary line".into(), false)]
    );
}

#[test]
fn cursor_restore_does_not_expose_a_transient_alternate_screen_frame() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        ..OutputSchedulerConfig::default()
    });
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"primary", &mut term_out)
        .expect("queue primary screen");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present primary screen");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).unwrap());
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\x12", &mut pty_out, &mut term_out)
        .expect("launch a full-screen application");
    app.handle_pty(
        &mut sr,
        b"\x1b[?1049h\x1b[?25l\x1b[2J\x1b[Htransient cursor row\x1b[?25h",
        &mut term_out,
    )
    .expect("queue transient alternate frame");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present transient alternate frame");
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("hold cursor-bracketed alternate redraw")
    );
    assert!(recorder.inner.borrow().speaks.is_empty());

    clock.advance_ms(5);
    app.handle_pty(
        &mut sr,
        b"\x1b[?25l\x1b[H\x1b[2Kfinal cursor row\x1b[?25h",
        &mut term_out,
    )
    .expect("queue completed alternate frame");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present completed alternate frame");
    assert!(!app.maybe_finalize_changes(&mut sr).unwrap());

    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).unwrap());
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[("final cursor row".into(), false)]
    );
}

#[test]
fn a_new_screen_context_does_not_inherit_the_previous_bursts_hard_deadline() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        ..OutputSchedulerConfig::default()
    });
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"old primary burst", &mut term_out)
        .expect("queue an unfinished primary-screen burst");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the unfinished primary-screen burst");
    clock.advance_ms(u128::from(MAX_DIFF_DELAY) - 1);

    app.handle_pty(
        &mut sr,
        b"\x1b[?1049h\x1b[2J\x1b[Halternate cursor line",
        &mut term_out,
    )
    .expect("switch to a new screen context");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the new screen context");

    clock.advance_ms(1);
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("the previous context's hard deadline must not publish the new context")
    );
    assert!(recorder.inner.borrow().speaks.is_empty());

    clock.advance_ms(u128::from(DIFF_DELAY));
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize the new context after its own quiet interval")
    );
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[("alternate cursor line".into(), false)]
    );
}

#[test]
fn pending_repaint_realigns_a_following_review_cursor_before_review_input() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"outer prompt\r\n", &mut term_out)
        .expect("draw and leave the outer prompt");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize outer prompt")
    );
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_pty(
        &mut sr,
        b"\x1B[2J\x1B[Hnested prompt\r\nreview target\x1B[H",
        &mut term_out,
    )
    .expect("render nested prompt without waiting for stabilization");
    app.handle_stdin(&mut sr, b"\x1Bi", &mut pty_out, &mut term_out)
        .expect("read nested prompt immediately");

    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("nested prompt".into(), false))
    );

    recorder.inner.borrow_mut().speaks.clear();
    app.handle_stdin(&mut sr, b"\x1Bo", &mut pty_out, &mut term_out)
        .expect("move away after consuming pending follow");
    app.handle_stdin(&mut sr, b"\x1Bi", &mut pty_out, &mut term_out)
        .expect("read the manually selected line without snapping again");
    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("review target".into(), false))
    );
}

#[test]
fn synchronized_output_waits_for_the_complete_screen_update_before_speaking() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"\x1B[?2026hpartial", &mut term_out)
        .expect("begin synchronized output");
    clock.advance_ms(u128::from(MAX_DIFF_DELAY) + 1);
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("defer synchronized update")
    );
    assert!(recorder.inner.borrow().speaks.is_empty());

    app.handle_pty(&mut sr, b" complete\x1B[?2026l", &mut term_out)
        .expect("end synchronized output");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize synchronized update")
    );
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        [("partial complete".into(), false)]
    );
}

#[test]
fn synchronized_output_auto_read_never_speaks_overwritten_transaction_text() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"before", &mut term_out)
        .expect("draw the previous committed frame");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize the previous frame")
    );
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_pty(
        &mut sr,
        b"\x1b[?2026h\r\x1b[2Ktransient\r\x1b[2Kfinal\x1b[?2026l",
        &mut term_out,
    )
    .expect("draw and commit an atomic replacement");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize the committed replacement")
    );

    let spoken = recorder
        .inner
        .borrow()
        .speaks
        .iter()
        .map(|(text, _)| text.clone())
        .collect::<Vec<_>>();
    assert!(
        spoken.iter().any(|text| text.contains("final")),
        "{spoken:?}"
    );
    assert!(
        spoken.iter().all(|text| !text.contains("transient")),
        "{spoken:?}"
    );
}

#[test]
fn review_reads_the_committed_screen_while_synchronized_output_is_open() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"outer prompt", &mut term_out)
        .expect("draw committed prompt");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize prompt")
    );
    recorder.inner.borrow_mut().speaks.clear();
    app.enable_output_scheduler(OutputSchedulerConfig::default());
    term_out.clear();

    app.handle_pty(
        &mut sr,
        b"\x1b[?2026h\x1b[2J\x1b[10;1Hpartial",
        &mut term_out,
    )
    .expect("open a partial synchronized frame");
    app.handle_stdin(&mut sr, b"\x1bi", &mut pty_out, &mut term_out)
        .expect("read from the committed frame");

    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("outer prompt".into(), false))
    );
    assert!(pty_out.is_empty());
    clock.advance_ms(100);
    let timed_out = app
        .drain_scheduled_output(&mut term_out, false)
        .expect("release the physical partial frame after its idle timeout");
    assert!(timed_out.synchronization_timed_out);
    app.handle_stdin(&mut sr, b"\x1bi", &mut pty_out, &mut term_out)
        .expect("read the partial frame after its timeout render flushed");
    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("partial".into(), false))
    );

    app.handle_stdin(&mut sr, b"x", &mut pty_out, &mut term_out)
        .expect("forward raw input while the frame remains open");
    assert_eq!(pty_out, b"x");
    app.handle_stdin(&mut sr, b"\x1bi", &mut pty_out, &mut term_out)
        .expect("read the same physically presented partial frame after raw input");
    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("partial".into(), false))
    );

    app.handle_pty(
        &mut sr,
        b"\x1b[2J\x1b[Hinner prompt\x1b[?2026l",
        &mut term_out,
    )
    .expect("commit the complete frame");
    app.handle_stdin(&mut sr, b"\x1bi", &mut pty_out, &mut term_out)
        .expect("keep reading the old physical frame until the close render flushes");
    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("partial".into(), false))
    );

    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("flush the closed application frame");
    app.handle_stdin(&mut sr, b"\x1bi", &mut pty_out, &mut term_out)
        .expect("read from the physically presented closed frame");
    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("inner prompt".into(), false))
    );
}

#[test]
fn synchronized_close_becomes_readable_only_after_its_render_flushes() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig::default());
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"old", &mut term_out)
        .expect("queue the original frame");
    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the original frame");
    term_out.clear();
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_pty(
        &mut sr,
        b"\x1b[?2026h\r\x1b[2Kfinal\x1b[?2026l",
        &mut term_out,
    )
    .expect("queue a closed synchronized frame");
    clock.advance_ms(4);
    let mut writer = FlushGateWriter {
        block_flush: true,
        ..FlushGateWriter::default()
    };
    let blocked = app
        .drain_scheduled_output(&mut writer, false)
        .expect("write the render up to its blocked flush fence");
    assert!(blocked.blocked);
    assert!(blocked.completed_renders.is_empty());

    app.handle_stdin(&mut sr, b"\x1bi", &mut pty_out, &mut term_out)
        .expect("read while the close render is not yet presented");
    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("old".into(), false))
    );

    writer.block_flush = false;
    let completed = app
        .drain_scheduled_output(&mut writer, true)
        .expect("complete the physical flush fence");
    assert_eq!(completed.completed_renders.len(), 1);
    app.handle_stdin(&mut sr, b"\x1bi", &mut pty_out, &mut term_out)
        .expect("read the newly presented frame");
    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("final".into(), false))
    );

    let mut oracle = GhosttyEngine::new(24, 80).expect("physical oracle");
    oracle
        .advance(&writer.bytes)
        .expect("parse the completed physical frame");
    assert!(oracle.normalized_snapshot().contents().starts_with("final"));
}

#[test]
fn osc133_prompt_waits_for_b_then_commits_without_the_diff_delay() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig::default());
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"old", &mut term_out)
        .expect("queue baseline");
    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present baseline");
    clock.advance_ms(u128::from(DIFF_DELAY));
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize baseline")
    );
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_pty(&mut sr, b"\r\x1b[2K\x1b]133;A\x07$ ", &mut term_out)
        .expect("queue partial prompt");
    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present partial prompt");
    clock.advance_ms(u128::from(DIFF_DELAY));
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("hold partial prompt")
    );
    assert!(recorder.inner.borrow().speaks.is_empty());

    app.handle_pty(&mut sr, b"ready\x1b]133;B\x07", &mut term_out)
        .expect("queue completed prompt");
    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present completed prompt");
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("commit prompt at semantic boundary")
    );
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        [("$ ready".into(), false)]
    );
}

#[test]
fn osc133_input_boundary_does_not_commit_trailing_partial_text() {
    for fragmented in [false, true] {
        let (mut app, mut sr, recorder, clock) = make_app();
        app.enable_output_scheduler(OutputSchedulerConfig {
            latency_budget_ms: 0,
            ..OutputSchedulerConfig::default()
        });
        let mut term_out = Vec::new();

        if fragmented {
            app.begin_pty_presentation_batch();
            for chunk in [
                b"\x1b]133;A\x07$ \x1b]133;".as_slice(),
                b"B\x07pa".as_slice(),
                b"rtial".as_slice(),
            ] {
                app.handle_pty(&mut sr, chunk, &mut term_out)
                    .expect("model fragmented prompt and partial input");
            }
            app.finish_pty_presentation_batch(&mut term_out)
                .expect("present one fragmented drain");
        } else {
            app.handle_pty(
                &mut sr,
                b"\x1b]133;A\x07$ \x1b]133;B\x07partial",
                &mut term_out,
            )
            .expect("model prompt and partial input in one read");
        }
        app.drain_scheduled_output(&mut term_out, false)
            .expect("publish the exact frame");

        assert!(
            !app.maybe_finalize_changes(&mut sr)
                .expect("trailing text keeps ordinary stabilization"),
            "OSC 133 B committed a partial frame (fragmented={fragmented})"
        );
        assert!(recorder.inner.borrow().speaks.is_empty());

        clock.advance_ms(u128::from(DIFF_DELAY));
        assert!(
            app.maybe_finalize_changes(&mut sr)
                .expect("ordinary quiet window completes the partial frame")
        );
    }
}

#[test]
fn osc133_input_boundary_does_not_commit_an_alternate_screen_redraw() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        ..OutputSchedulerConfig::default()
    });
    let mut term_out = Vec::new();

    app.handle_pty(
        &mut sr,
        b"\x1b[?1049h\x1b]133;A\x07full-screen\x1b]133;B\x07",
        &mut term_out,
    )
    .expect("model alternate-screen semantic markers");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present alternate-screen frame");

    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("alternate screen keeps ordinary stabilization")
    );
    assert!(recorder.inner.borrow().speaks.is_empty());

    clock.advance_ms(u128::from(DIFF_DELAY));
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("quiet alternate-screen frame finalizes")
    );
}

#[test]
fn abandoned_osc133_prompt_boundary_falls_back_at_the_maximum_delay() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"\x1b]133;A\x07$ partial", &mut term_out)
        .expect("draw prompt without its input boundary");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("hold partial prompt")
    );

    clock.advance_ms(u128::from(MAX_DIFF_DELAY - DIFF_DELAY));
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("release abandoned semantic transaction")
    );
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        [("$ partial".into(), false)]
    );

    app.handle_pty(&mut sr, b" later", &mut term_out)
        .expect("continue after abandoned marker was baselined");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("use ordinary stabilization after semantic timeout")
    );
}

#[test]
fn alternate_screen_restores_the_primary_review_cursor() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    sr.set_review_follows_screen_cursor(false);

    app.handle_pty(
        &mut sr,
        b"saved review target\r\nprimary application line",
        &mut term_out,
    )
    .expect("draw primary screen");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).unwrap());
    recorder.inner.borrow_mut().speaks.clear();
    app.handle_stdin(&mut sr, b"\x1bi", &mut pty_out, &mut term_out)
        .expect("verify primary review cursor before transition");
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[("saved review target".into(), false)]
    );
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_pty(
        &mut sr,
        b"\x1b[?1049h\x1b[2J\x1b[Halternate first\r\nalternate cursor line",
        &mut term_out,
    )
    .expect("enter alternate screen");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).unwrap());
    recorder.inner.borrow_mut().speaks.clear();
    app.handle_stdin(&mut sr, b"\x1bi", &mut pty_out, &mut term_out)
        .expect("read alternate review cursor");
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[("alternate cursor line".into(), false)]
    );

    recorder.inner.borrow_mut().speaks.clear();
    app.handle_pty(&mut sr, b"\x1b[?1049l", &mut term_out)
        .expect("restore primary screen");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).unwrap());
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[("primary application line".into(), false)]
    );

    recorder.inner.borrow_mut().speaks.clear();
    app.handle_stdin(&mut sr, b"\x1bi", &mut pty_out, &mut term_out)
        .expect("read restored primary review cursor");
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[("saved review target".into(), false)]
    );
}

#[test]
fn alternate_screen_entry_does_not_read_coalesced_insert_echo() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    sr.set_suppress_key_echo(true);

    app.handle_pty(&mut sr, b"primary", &mut term_out)
        .expect("draw primary screen");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).unwrap());
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_pty(&mut sr, b"\x1b[?1049h\x1b[2J\x1b[H", &mut term_out)
        .expect("begin alternate-screen entry");
    app.handle_stdin(&mut sr, b"aT", &mut pty_out, &mut term_out)
        .expect("type before the entry presentation settles");
    app.handle_pty(
        &mut sr,
        b"\x1b[?2026h\x1b[HT\x1b[2;1H~\x1b[3;1H-- INSERT --\x1b[1;2H\x1b[?2026l",
        &mut term_out,
    )
    .expect("present first inserted character with editor repaint");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).unwrap());

    assert!(recorder.inner.borrow().speaks.is_empty());
}

#[test]
fn returning_from_an_overlay_preserves_the_underlying_review_cursor() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    sr.set_review_follows_screen_cursor(false);

    app.handle_pty(
        &mut sr,
        b"overlay return target\r\napplication line",
        &mut term_out,
    )
    .expect("draw source");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).unwrap());

    app.show_message(&mut sr, "Notice", "overlay", &mut term_out)
        .expect("open overlay");
    app.handle_stdin(&mut sr, b"\r", &mut pty_out, &mut term_out)
        .expect("close overlay");
    recorder.inner.borrow_mut().speaks.clear();
    app.handle_stdin(&mut sr, b"\x1bi", &mut pty_out, &mut term_out)
        .expect("read restored source review cursor");

    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[("overlay return target".into(), false)]
    );
}

#[test]
fn cursor_restore_after_input_waits_for_screen_quiet() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig::default());
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"old", &mut term_out)
        .expect("queue baseline");
    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present baseline");
    clock.advance_ms(u128::from(DIFF_DELAY));
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize baseline")
    );
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"j", &mut pty_out, &mut term_out)
        .expect("forward navigation input");
    app.handle_pty(
        &mut sr,
        b"\x1b[?25l\r\x1b[2Knew line\x1b[?25h",
        &mut term_out,
    )
    .expect("queue legacy cursor-bracketed redraw");
    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present legacy redraw");

    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("cursor restoration is only a painting hint")
    );
    assert!(recorder.inner.borrow().speaks.is_empty());

    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("commit after the redraw becomes quiet")
    );
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        [("new line".into(), false)]
    );
}

#[test]
fn cursor_restore_does_not_expose_a_transient_fzf_height_frame() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        ..OutputSchedulerConfig::default()
    });
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"$ ", &mut term_out)
        .expect("queue shell prompt");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present shell prompt");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).unwrap());
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\x12", &mut pty_out, &mut term_out)
        .expect("launch reverse history search");
    app.handle_pty(&mut sr, b"\x1b[?25l\x1b[2;1H$ \x1b[?25h", &mut term_out)
        .expect("queue transient popup frame");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present transient popup frame");
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("hold cursor-bracketed partial redraw")
    );
    assert!(recorder.inner.borrow().speaks.is_empty());

    clock.advance_ms(5);
    app.handle_pty(
        &mut sr,
        b"\x1b[?25l\x1b[2;1H\x1b[2Kfinal history choice\x1b[?25h",
        &mut term_out,
    )
    .expect("queue completed popup frame");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present completed popup frame");
    assert!(!app.maybe_finalize_changes(&mut sr).unwrap());

    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).unwrap());
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[("final history choice".into(), false)]
    );
}

#[test]
fn reverse_search_interface_reads_its_settled_contents() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"$ ", &mut term_out)
        .expect("draw shell prompt");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).unwrap());
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\x12", &mut pty_out, &mut term_out)
        .expect("launch reverse history search");
    app.handle_pty(
        &mut sr,
        b"\x1b[2J\x1b[Hhistory item\x1b[2;1H----------------\x1b[3;1H>\x1b[4;1H1/100\x1b[3;1H",
        &mut term_out,
    )
    .expect("draw cursor-addressed history interface");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).unwrap());

    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[(
            "history item\n\n----------------\n\n greater \n\n1 slash 100".into(),
            false,
        )]
    );
}

#[test]
fn reverse_search_discards_a_transient_semantic_input_macro() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"\x1b]133;A\x07$ \x1b]133;B\x07", &mut term_out)
        .expect("draw semantic shell prompt");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).unwrap());
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\x12", &mut pty_out, &mut term_out)
        .expect("launch reverse history search");
    app.handle_pty(&mut sr, b"\r\x1b[2K$ `__fzf_history__`", &mut term_out)
        .expect("draw transient Readline macro");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(!app.maybe_finalize_changes(&mut sr).unwrap());
    assert!(recorder.inner.borrow().speaks.is_empty());

    app.handle_pty(
        &mut sr,
        b"\x1b[2J\x1b[Hhistory one\r\nhistory two\r\n\x1b[4;1H>\x1b[4;1H",
        &mut term_out,
    )
    .expect("draw final cursor-addressed history interface");
    clock.advance_ms(u128::from(MAX_DIFF_DELAY));
    assert!(app.maybe_finalize_changes(&mut sr).unwrap());

    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[("history one\n\nhistory two\n\n greater ".into(), false)]
    );
    assert!(
        recorder
            .inner
            .borrow()
            .speaks
            .iter()
            .all(|(text, _)| !text.contains("__fzf_history__"))
    );
}

#[test]
fn control_j_and_enter_have_identical_output_reading_semantics() {
    fn spoken_after_submit(submit: &[u8]) -> Vec<(String, bool)> {
        let (mut app, mut sr, recorder, clock) = make_app();
        let mut pty_out = Vec::new();
        let mut term_out = Vec::new();

        app.handle_pty(&mut sr, b"$ command", &mut term_out)
            .expect("draw command line");
        clock.advance_ms(u128::from(DIFF_DELAY) + 1);
        assert!(app.maybe_finalize_changes(&mut sr).unwrap());
        recorder.inner.borrow_mut().speaks.clear();

        app.handle_stdin(&mut sr, submit, &mut pty_out, &mut term_out)
            .expect("submit command");
        app.handle_pty(
            &mut sr,
            b"\x1b[2J\x1b[Hfirst result\r\nsecond result\r\nthird result\r\nfourth result",
            &mut term_out,
        )
        .expect("draw line-oriented command output");
        clock.advance_ms(u128::from(DIFF_DELAY) + 1);
        assert!(app.maybe_finalize_changes(&mut sr).unwrap());

        recorder.inner.borrow().speaks.clone()
    }

    let enter = spoken_after_submit(b"\r");
    let control_j = spoken_after_submit(b"\n");
    assert!(!enter.is_empty());
    assert_eq!(control_j, enter);
}

#[test]
fn unsolicited_cursor_restore_remains_on_the_ordinary_stability_path() {
    let (mut app, mut sr, _recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig::default());
    let mut term_out = Vec::new();

    app.handle_pty(
        &mut sr,
        b"\x1b[?25lbackground redraw\x1b[?25h",
        &mut term_out,
    )
    .expect("queue unsolicited cursor-bracketed redraw");
    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present unsolicited redraw");

    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("retain ordinary debounce without causal input")
    );
    clock.advance_ms(u128::from(DIFF_DELAY));
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize after ordinary debounce")
    );
}

#[test]
fn replacing_an_unstarted_render_keeps_incremental_rendering_available() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig::default());
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"baseline", &mut term_out)
        .expect("queue baseline");
    app.drain_scheduled_output(&mut term_out, true)
        .expect("confirm baseline");

    app.handle_pty(&mut sr, b"\rfirst", &mut term_out)
        .expect("queue the first incremental render");
    assert_ne!(
        app.debug_last_render_strategy(),
        RenderStrategy::FullFallback
    );
    app.handle_pty(&mut sr, b"\rsecond", &mut term_out)
        .expect("replace it before either render starts");
    assert_ne!(
        app.debug_last_render_strategy(),
        RenderStrategy::FullFallback,
        "an unstarted render still shares the confirmed physical shadow"
    );
}

#[test]
fn neovim_atomic_redraw_is_never_read_mid_draw_and_finalizes_on_its_flush() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig::default());
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"old", &mut term_out)
        .expect("queue the committed baseline");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the committed baseline");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize baseline")
    );
    recorder.inner.borrow_mut().speaks.clear();
    term_out.clear();

    app.handle_pty(
        &mut sr,
        b"\x1b[?2026h\r\x1b[2Kneovim transient one",
        &mut term_out,
    )
    .expect("begin Neovim-style atomic redraw");
    for partial in [
        b"\r\x1b[2Kneovim transient two".as_slice(),
        b"\r\x1b[2Kneovim transient three".as_slice(),
    ] {
        clock.advance_ms(u128::from(DIFF_DELAY) + 1);
        assert!(
            !app.maybe_finalize_changes(&mut sr)
                .expect("do not read an unpresented working frame")
        );
        assert!(recorder.inner.borrow().speaks.is_empty());
        assert!(
            app.drain_scheduled_output(&mut term_out, false)
                .expect("hold the working frame")
                .completed_renders
                .is_empty()
        );
        app.handle_pty(&mut sr, partial, &mut term_out)
            .expect("replace the working frame");
    }
    assert!(
        term_out.is_empty(),
        "working pixels escaped the atomic draw"
    );

    app.handle_pty(&mut sr, b"\r\x1b[2Kneovim final\x1b[?2026l", &mut term_out)
        .expect("commit the final Neovim frame");
    let mut writer = FlushGateWriter {
        block_flush: true,
        ..FlushGateWriter::default()
    };
    let blocked = app
        .drain_scheduled_output(&mut writer, false)
        .expect("write but do not publish the final frame");
    assert!(blocked.completed_renders.is_empty());
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("do not read before the physical flush")
    );
    assert!(recorder.inner.borrow().speaks.is_empty());

    writer.block_flush = false;
    let completed = app
        .drain_scheduled_output(&mut writer, true)
        .expect("flush the exact committed frame");
    assert_eq!(completed.completed_renders.len(), 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("the explicit commit needs no extra debounce")
    );
    let spoken = recorder
        .inner
        .borrow()
        .speaks
        .iter()
        .map(|(text, _)| text.clone())
        .collect::<Vec<_>>();
    assert!(
        spoken.iter().any(|text| text.contains("neovim final")),
        "{spoken:?}"
    );
    assert!(
        spoken.iter().all(|text| !text.contains("transient")),
        "{spoken:?}"
    );
}

#[test]
fn timeout_flush_publishes_its_exact_generation_not_a_newer_parser_frame() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig::default());
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let mut physical = GhosttyEngine::new(24, 80).expect("physical oracle");

    app.handle_pty(&mut sr, b"old", &mut term_out)
        .expect("queue the original frame");
    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the original frame");
    physical
        .advance(&term_out)
        .expect("parse the original physical frame");
    term_out.clear();
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_pty(&mut sr, b"\x1b[?2026h\r\x1b[2Kpartial-a", &mut term_out)
        .expect("open the first partial frame");
    clock.advance_ms(100);
    let mut first_writer = FlushGateWriter {
        block_flush: true,
        ..FlushGateWriter::default()
    };
    let blocked = app
        .drain_scheduled_output(&mut first_writer, false)
        .expect("write the timed-out frame but block its flush");
    assert!(blocked.blocked);
    assert!(blocked.completed_renders.is_empty());

    app.handle_pty(&mut sr, b"\r\x1b[2Kpartial-b", &mut term_out)
        .expect("parse a newer frame while the old render is backpressured");
    app.handle_stdin(&mut sr, b"\x1bi", &mut pty_out, &mut term_out)
        .expect("read before either partial frame is presented");
    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("old".into(), false))
    );

    first_writer.block_flush = false;
    first_writer.block_writes = true;
    let first_completed = app
        .drain_scheduled_output(&mut first_writer, true)
        .expect("flush only the first timed-out generation");
    assert_eq!(first_completed.completed_renders.len(), 1);
    physical
        .advance(&first_writer.bytes)
        .expect("parse the first timed-out physical generation");
    app.handle_stdin(&mut sr, b"\x1bi", &mut pty_out, &mut term_out)
        .expect("read the exact first physical generation");
    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("partial-a".into(), false))
    );
    assert!(
        physical
            .normalized_snapshot()
            .contents()
            .starts_with("partial-a")
    );

    clock.advance_ms(4);
    let mut second_bytes = Vec::new();
    let second_completed = app
        .drain_scheduled_output(&mut second_bytes, true)
        .expect("flush the newer partial generation");
    assert_eq!(second_completed.completed_renders.len(), 1);
    physical
        .advance(&second_bytes)
        .expect("parse the newer physical generation");
    app.handle_stdin(&mut sr, b"\x1bi", &mut pty_out, &mut term_out)
        .expect("read the newer physical generation");
    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("partial-b".into(), false))
    );
    assert!(
        physical
            .normalized_snapshot()
            .contents()
            .starts_with("partial-b")
    );
}

#[test]
fn completed_lf_receipt_stays_immediately_eligible_while_the_parser_is_newer() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        ..OutputSchedulerConfig::default()
    });
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"receipt line\r\n", &mut term_out)
        .expect("queue a completed line frame");
    let mut writer = FlushGateWriter {
        block_flush: true,
        ..FlushGateWriter::default()
    };
    assert!(
        app.drain_scheduled_output(&mut writer, false)
            .expect("write the completed line up to its flush fence")
            .blocked
    );

    app.handle_pty(&mut sr, b"newer parser text", &mut term_out)
        .expect("advance the parser past the completed line frame");
    writer.block_flush = false;
    writer.block_writes = true;
    assert_eq!(
        app.drain_scheduled_output(&mut writer, true)
            .expect("flush only the completed line frame")
            .completed_renders
            .len(),
        1
    );

    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("the exact LF receipt retains its immediate commit boundary"),
        "parser-ahead state discarded the completed receipt's LF boundary"
    );
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[("receipt line".into(), false)]
    );
}

#[test]
fn auto_read_advances_to_a_presented_frame_while_the_parser_is_newer() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        ..OutputSchedulerConfig::default()
    });
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"old", &mut term_out)
        .expect("queue the original frame");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the original frame");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).expect("finalize old"));
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_pty(&mut sr, b"\r\x1b[2Kfirst", &mut term_out)
        .expect("queue the first new frame");
    let mut writer = FlushGateWriter {
        block_flush: true,
        ..FlushGateWriter::default()
    };
    let blocked = app
        .drain_scheduled_output(&mut writer, false)
        .expect("write the first frame up to its flush fence");
    assert!(blocked.blocked);

    app.handle_pty(&mut sr, b"\r\x1b[2Ksecond", &mut term_out)
        .expect("parse a newer frame before the first flush completes");
    writer.block_flush = false;
    writer.block_writes = true;
    let completed = app
        .drain_scheduled_output(&mut writer, true)
        .expect("flush only the first frame");
    assert_eq!(completed.completed_renders.len(), 1);

    clock.advance_ms(u128::from(MAX_DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("auto-read the completed physical frame"),
        "a newer parser frame must not indefinitely defer an older completed presentation"
    );
    assert!(
        recorder
            .inner
            .borrow()
            .speaks
            .iter()
            .any(|(text, _)| text.contains("first")),
        "speaks={:?}",
        recorder.inner.borrow().speaks
    );
    assert!(
        recorder
            .inner
            .borrow()
            .speaks
            .iter()
            .all(|(text, _)| !text.contains("second")),
        "unpresented text leaked into speech: {:?}",
        recorder.inner.borrow().speaks
    );

    writer.block_writes = false;
    app.drain_scheduled_output(&mut writer, true)
        .expect("present the newer frame");
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("preserve the next frame's stabilization window"),
        "continuous output must not bypass the per-presentation debounce"
    );
    assert_eq!(
        app.scheduled_output_timeout(),
        Some(std::time::Duration::from_millis(u64::from(DIFF_DELAY)))
    );
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("auto-read the newer completed frame")
    );
    assert!(
        recorder
            .inner
            .borrow()
            .speaks
            .iter()
            .any(|(text, _)| text.contains("second")),
        "speaks={:?}",
        recorder.inner.borrow().speaks
    );
    assert_eq!(
        app.scheduled_output_timeout(),
        None,
        "finalizing the newest presented revision must disarm its wakeup"
    );
    assert!(
        !app.wants_tick(),
        "finalization left an immediate tick armed"
    );
}

#[test]
fn lagged_neovim_alternate_screen_receipt_reads_its_settled_view() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        ..OutputSchedulerConfig::default()
    });
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"primary", &mut term_out)
        .expect("queue the primary frame");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the primary frame");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).unwrap());
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_pty(
        &mut sr,
        b"\x1b[?1049h\x1b[2J\x1b[Hfile first line\r\ncursor line",
        &mut term_out,
    )
    .expect("queue Neovim's alternate-screen frame");
    let mut writer = FlushGateWriter {
        block_flush: true,
        ..FlushGateWriter::default()
    };
    assert!(
        app.drain_scheduled_output(&mut writer, false)
            .expect("write the alternate frame up to its flush fence")
            .blocked
    );

    app.handle_pty(
        &mut sr,
        b"\x1b[1;1Hnewer parser frame\x1b[2;12H",
        &mut term_out,
    )
    .expect("advance the parser beyond the blocked physical frame");
    writer.block_flush = false;
    writer.block_writes = true;
    assert_eq!(
        app.drain_scheduled_output(&mut writer, true)
            .expect("flush only the first alternate frame")
            .completed_renders
            .len(),
        1
    );

    clock.advance_ms(u128::from(MAX_DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).unwrap());
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        &[("file first line\ncursor line".into(), false)]
    );
}

#[test]
fn lagged_presented_auto_read_uses_live_viewport_and_preserves_review_cursor_row() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        ..OutputSchedulerConfig::default()
    });
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    let initial = (0..30)
        .map(|line| format!("history-{line:02}"))
        .collect::<Vec<_>>()
        .join("\r\n");
    app.handle_pty(&mut sr, initial.as_bytes(), &mut term_out)
        .expect("queue the initial frame and its scrollback");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the initial frame");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize the initial frame")
    );

    // Move to the top of the visible page and explicitly leave cursor
    // following off. Review commands must not select a historical viewport.
    for _ in 0..24 {
        app.handle_stdin(&mut sr, b"\x1bu", &mut pty_out, &mut term_out)
            .expect("move the review cursor to the visible top");
    }
    sr.set_review_follows_screen_cursor(false);
    recorder.inner.borrow_mut().speaks.clear();
    app.handle_stdin(&mut sr, b"\x1bi", &mut pty_out, &mut term_out)
        .expect("read the selected visible row");
    let selected_line = recorder
        .inner
        .borrow()
        .speaks
        .last()
        .expect("selected line was spoken")
        .0
        .clone();
    assert!(selected_line.starts_with("history-"));
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_pty(
        &mut sr,
        b"\x1b[1;1H\x1b[2Kphysical first\x1b[24;1H",
        &mut term_out,
    )
    .expect("queue the first visible replacement");
    let mut writer = FlushGateWriter {
        block_flush: true,
        ..FlushGateWriter::default()
    };
    let blocked = app
        .drain_scheduled_output(&mut writer, false)
        .expect("write the first replacement up to its flush fence");
    assert!(blocked.blocked);

    app.handle_pty(
        &mut sr,
        b"\x1b[1;1H\x1b[2Knewer live frame\x1b[24;1H",
        &mut term_out,
    )
    .expect("parse a newer replacement while the first is backpressured");
    writer.block_flush = false;
    writer.block_writes = true;
    let completed = app
        .drain_scheduled_output(&mut writer, true)
        .expect("flush only the first replacement");
    assert_eq!(completed.completed_renders.len(), 1);

    clock.advance_ms(u128::from(MAX_DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("auto-read the lagged physical frame")
    );
    let auto_read = recorder
        .inner
        .borrow()
        .speaks
        .iter()
        .map(|(text, _)| text.clone())
        .collect::<Vec<_>>();
    assert!(
        auto_read.iter().any(|text| text.contains("physical first")),
        "the physical live viewport was not read: {auto_read:?}"
    );
    assert!(
        auto_read
            .iter()
            .all(|text| !text.contains("newer live frame")),
        "unpresented parser text leaked into speech: {auto_read:?}"
    );

    recorder.inner.borrow_mut().speaks.clear();
    app.handle_stdin(&mut sr, b"\x1bi", &mut pty_out, &mut term_out)
        .expect("read the preserved review cursor row");
    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("physical first".to_owned(), false)),
        "the review cursor left the visible row or exposed unpresented text"
    );
    assert!(pty_out.is_empty());
}

#[test]
fn scheduled_synchronized_frame_auto_reads_only_its_final_pixels() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        ..OutputSchedulerConfig::default()
    });
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"before", &mut term_out)
        .expect("queue the baseline frame");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the baseline frame");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize baseline")
    );
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_pty(
        &mut sr,
        b"\x1b[?2026h\r\x1b[2Ktransient pixels\r\x1b[2Kfinal pixels\x1b[?2026l",
        &mut term_out,
    )
    .expect("queue one synchronized replacement frame");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the synchronized replacement frame");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("auto-read the synchronized replacement")
    );

    let spoken = recorder
        .inner
        .borrow()
        .speaks
        .iter()
        .map(|(text, _)| text.clone())
        .collect::<Vec<_>>();
    assert!(
        spoken.iter().any(|text| text.contains("final pixels")),
        "the committed pixels were not spoken: {spoken:?}"
    );
    assert!(
        spoken.iter().all(|text| !text.contains("transient pixels")),
        "overwritten synchronized text leaked into speech: {spoken:?}"
    );
}

#[test]
fn lagged_presented_cursor_follow_uses_the_presented_cursor() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        ..OutputSchedulerConfig::default()
    });
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"baseline", &mut term_out)
        .expect("queue the baseline frame");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the baseline frame");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize baseline")
    );
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_pty(&mut sr, b"\x1b[2;1Hpresented cursor", &mut term_out)
        .expect("queue a frame whose cursor is on row two");
    let mut writer = FlushGateWriter {
        block_flush: true,
        ..FlushGateWriter::default()
    };
    let blocked = app
        .drain_scheduled_output(&mut writer, false)
        .expect("write the row-two frame up to its flush fence");
    assert!(blocked.blocked);

    app.handle_pty(&mut sr, b"\x1b[3;1Hnewer live cursor", &mut term_out)
        .expect("move the newer parser cursor to row three");
    writer.block_flush = false;
    writer.block_writes = true;
    let completed = app
        .drain_scheduled_output(&mut writer, true)
        .expect("flush only the row-two frame");
    assert_eq!(completed.completed_renders.len(), 1);

    clock.advance_ms(u128::from(MAX_DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize the lagged presented cursor")
    );
    recorder.inner.borrow_mut().speaks.clear();
    app.handle_stdin(&mut sr, b"\x1bi", &mut pty_out, &mut term_out)
        .expect("read at the followed review cursor");

    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("presented cursor".into(), false)),
        "review cursor followed the newer parser cursor instead of the physical cursor"
    );
    assert!(pty_out.is_empty());
}

#[test]
fn backspace_resolves_on_a_presented_frame_while_the_parser_is_newer() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        ..OutputSchedulerConfig::default()
    });
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"abc", &mut term_out)
        .expect("queue the original input line");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the original input line");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).expect("finalize abc"));
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\x7f", &mut pty_out, &mut term_out)
        .expect("forward and defer backspace");
    app.handle_pty(&mut sr, b"\x08\x1b[P", &mut term_out)
        .expect("queue the application echo of the deletion");
    let mut writer = FlushGateWriter {
        block_flush: true,
        ..FlushGateWriter::default()
    };
    app.drain_scheduled_output(&mut writer, false)
        .expect("write the deletion up to its flush fence");

    app.handle_pty(&mut sr, b"\x1b[2;1Hstatus\x1b[1;3H", &mut term_out)
        .expect("parse a newer status redraw");
    writer.block_flush = false;
    writer.block_writes = true;
    app.drain_scheduled_output(&mut writer, true)
        .expect("flush only the deletion frame");

    clock.advance_ms(u128::from(MAX_DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("resolve the deletion against the completed frame")
    );
    assert!(
        recorder
            .inner
            .borrow()
            .speaks
            .iter()
            .any(|(text, _)| text == "c"),
        "speaks={:?}",
        recorder.inner.borrow().speaks
    );
}

#[test]
fn completed_backspace_echo_is_announced_without_finalizing_its_surrounding_update() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        ..OutputSchedulerConfig::default()
    });
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"abc", &mut term_out)
        .expect("queue the original input line");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the original input line");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).expect("finalize abc"));
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\x7f", &mut pty_out, &mut term_out)
        .expect("forward and defer backspace");
    app.handle_pty(&mut sr, b"\x08\x1b[P", &mut term_out)
        .expect("queue the complete application echo");
    assert!(recorder.inner.borrow().speaks.is_empty());
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the complete application echo");

    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("announce the confirmed deletion without a quiet wait")
    );
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        [("c".into(), false)]
    );

    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize the surrounding update at its normal boundary")
    );
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        [("c".into(), false)],
        "ordinary finalization must not announce the deletion twice"
    );
    assert_eq!(pty_out, b"\x7f");
}

#[test]
fn completed_delete_echo_is_announced_without_finalizing_its_surrounding_update() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        ..OutputSchedulerConfig::default()
    });
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"abc\x1b[2D", &mut term_out)
        .expect("queue the original input line");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the original input line");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).expect("finalize abc"));
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\x1b[3~", &mut pty_out, &mut term_out)
        .expect("forward and defer delete");
    app.handle_pty(&mut sr, b"\x1b[P", &mut term_out)
        .expect("queue the complete application echo");
    assert!(recorder.inner.borrow().speaks.is_empty());
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the complete application echo");

    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("announce the confirmed deletion without a quiet wait")
    );
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        [("b".into(), false)]
    );

    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize the surrounding update at its normal boundary")
    );
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        [("b".into(), false)],
        "ordinary finalization must not announce the deletion twice"
    );
    assert_eq!(pty_out, b"\x1b[3~");
}

#[test]
fn split_backspace_echo_waits_for_the_erase_before_immediate_announcement() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        ..OutputSchedulerConfig::default()
    });
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"abc", &mut term_out)
        .expect("queue the original input line");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the original input line");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).expect("finalize abc"));
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"\x7f", &mut pty_out, &mut term_out)
        .expect("forward and defer backspace");
    app.handle_pty(&mut sr, b"\x08", &mut term_out)
        .expect("queue only the cursor-left prefix");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present only the cursor-left prefix");
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("the partial echo must retain stabilization")
    );
    assert!(recorder.inner.borrow().speaks.is_empty());

    app.handle_pty(&mut sr, b"\x1b[P", &mut term_out)
        .expect("queue the character erase");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the completed deletion");
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("the complete deletion can now be announced")
    );
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        [("c".into(), false)]
    );

    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("the surrounding update retains its quiet boundary")
    );
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        [("c".into(), false)]
    );
    assert_eq!(pty_out, b"\x7f");
}

#[test]
fn backspace_inside_synchronized_output_waits_for_the_transaction_close_receipt() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        ..OutputSchedulerConfig::default()
    });
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"abc", &mut term_out)
        .expect("queue the original input line");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the original input line");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).expect("finalize abc"));
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_pty(&mut sr, b"\x1b[?2026h", &mut term_out)
        .expect("open synchronized output");
    app.handle_stdin(&mut sr, b"\x7f", &mut pty_out, &mut term_out)
        .expect("press backspace inside the transaction");
    app.handle_pty(&mut sr, b"\x08\x1b[P", &mut term_out)
        .expect("apply the deletion inside the transaction");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("keep the open transaction held");
    clock.advance_ms(u128::from(MAX_DIFF_DELAY) + 1);
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("an open transaction cannot announce the deletion")
    );
    assert!(recorder.inner.borrow().speaks.is_empty());

    app.handle_pty(&mut sr, b"\x1b[?2026l", &mut term_out)
        .expect("close synchronized output");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the closed transaction");
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("the exact close receipt finalizes the transaction")
    );
    assert_eq!(
        recorder.inner.borrow().speaks.as_slice(),
        [("c".into(), false)]
    );
    assert_eq!(pty_out, b"\x7f");
}

#[test]
fn backspace_survives_a_pre_input_receipt_and_later_typeahead() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        ..OutputSchedulerConfig::default()
    });
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"abc", &mut term_out)
        .expect("queue the original input line");
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the original input line");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).expect("finalize abc"));
    recorder.inner.borrow_mut().speaks.clear();

    // This repaint began before Backspace and therefore cannot confirm it,
    // even though its receipt completes afterward.
    app.handle_pty(&mut sr, b"\x1b7\x1b[2;1Hstatus\x1b8", &mut term_out)
        .expect("queue a pre-input status repaint");
    let mut writer = FlushGateWriter {
        block_flush: true,
        ..FlushGateWriter::default()
    };
    app.drain_scheduled_output(&mut writer, false)
        .expect("write the status repaint up to its flush fence");

    app.handle_stdin(&mut sr, b"\x7f", &mut pty_out, &mut term_out)
        .expect("forward and defer backspace");
    app.handle_pty(&mut sr, b"\x08\x1b[P", &mut term_out)
        .expect("queue the application's deletion echo");
    app.handle_stdin(&mut sr, b"x", &mut pty_out, &mut term_out)
        .expect("type ahead before either receipt completes");

    writer.block_flush = false;
    writer.block_writes = true;
    app.drain_scheduled_output(&mut writer, true)
        .expect("flush only the pre-input repaint");
    clock.advance_ms(u128::from(MAX_DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize the pre-input repaint")
    );
    assert!(
        recorder
            .inner
            .borrow()
            .speaks
            .iter()
            .all(|(text, _)| text != "c"),
        "the pre-input receipt consumed the deletion intent"
    );

    recorder.inner.borrow_mut().speaks.clear();
    writer.block_writes = false;
    app.drain_scheduled_output(&mut writer, true)
        .expect("present the deletion echo");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("resolve deletion on its causally later receipt")
    );
    assert_eq!(
        recorder
            .inner
            .borrow()
            .speaks
            .iter()
            .filter(|(text, _)| text == "c")
            .count(),
        1,
        "speaks={:?}",
        recorder.inner.borrow().speaks
    );
    assert_eq!(pty_out, b"\x7fx");
}

#[test]
fn review_commands_follow_the_physically_presented_view_across_overlay_backpressure() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig::default());
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"base", &mut term_out)
        .expect("queue the base scene");
    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the base scene");
    term_out.clear();

    recorder.inner.borrow_mut().speaks.clear();
    app.show_message(&mut sr, "Notice", "foreground", &mut term_out)
        .expect("queue an overlay without presenting it yet");
    assert!(recorder.inner.borrow().speaks.is_empty());
    app.handle_stdin(&mut sr, b"\x1bi", &mut pty_out, &mut term_out)
        .expect("read while the base remains physical");
    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("base".into(), false))
    );

    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the overlay");
    recorder.inner.borrow_mut().speaks.clear();
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .expect("announce the overlay only after it is physical");
    assert!(
        recorder
            .inner
            .borrow()
            .speaks
            .iter()
            .any(|(text, _)| text.contains("Press Enter"))
    );
    recorder.inner.borrow_mut().speaks.clear();
    app.handle_stdin(&mut sr, b"\x1bi", &mut pty_out, &mut term_out)
        .expect("read the presented overlay");
    assert!(
        recorder
            .inner
            .borrow()
            .speaks
            .last()
            .is_some_and(|(text, _)| {
                text.contains("foreground") || text.contains("Press Enter")
            })
    );

    recorder.inner.borrow_mut().speaks.clear();
    app.handle_stdin(&mut sr, b"\r", &mut pty_out, &mut term_out)
        .expect("logically dismiss the overlay");
    assert!(recorder.inner.borrow().speaks.is_empty());
    app.handle_stdin(&mut sr, b"\x1bi", &mut pty_out, &mut term_out)
        .expect("keep reading the retired overlay until its replacement flushes");
    assert!(
        recorder
            .inner
            .borrow()
            .speaks
            .last()
            .is_some_and(|(text, _)| {
                text.contains("foreground") || text.contains("Press Enter")
            })
    );

    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the revealed base");
    recorder.inner.borrow_mut().speaks.clear();
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .expect("announce the base only after it is physical");
    assert!(
        recorder
            .inner
            .borrow()
            .speaks
            .iter()
            .any(|(text, _)| text.contains("base"))
    );
    recorder.inner.borrow_mut().speaks.clear();
    app.handle_stdin(&mut sr, b"\x1bi", &mut pty_out, &mut term_out)
        .expect("read the physically revealed base");
    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("base".into(), false))
    );
}

#[test]
fn overlay_redraw_auto_read_waits_for_the_matching_physical_frame() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig::default());
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"base", &mut term_out)
        .expect("queue the base scene");
    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the base scene");

    app.handle_stdin(&mut sr, b"\x1BL", &mut pty_out, &mut term_out)
        .expect("open the Lua REPL");
    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the Lua REPL");
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .expect("announce the presented Lua REPL");
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"physically gated", &mut pty_out, &mut term_out)
        .expect("edit the logical REPL before its redraw is presented");
    assert!(
        recorder.inner.borrow().speaks.is_empty(),
        "a logical redraw was auto-read before its physical frame completed"
    );
    assert!(
        app.wants_tick(),
        "the zero-delay render should request an immediate presentation turn"
    );

    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the edited REPL");
    assert!(
        app.wants_tick(),
        "the deferred read should become runnable after presentation"
    );
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .expect("auto-read the physically presented edit");
    assert!(
        recorder
            .inner
            .borrow()
            .speaks
            .iter()
            .any(|(text, _)| text.contains("physically gated")),
        "the matching physical redraw was not auto-read: {:?}",
        recorder.inner.borrow().speaks
    );
}

#[test]
fn terminal_finalization_waits_while_an_unpresented_overlay_is_logically_active() {
    let (mut app, mut sr, _recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig::default());
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"old base", &mut term_out)
        .expect("queue the initial base frame");
    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the initial base frame");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize the initial base frame")
    );

    app.show_message(&mut sr, "Notice", "logical overlay", &mut term_out)
        .expect("queue an overlay without presenting it");
    app.handle_pty(&mut sr, b"\r\nnew hidden base", &mut term_out)
        .expect("update the hidden base");
    clock.advance_ms(u128::from(MAX_DIFF_DELAY) + 1);

    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("defer finalization across the view transition"),
        "hidden base output was finalized through the logical overlay"
    );
}

#[test]
fn dismissing_an_overlay_reveals_the_committed_underlay_during_a_synchronized_frame() {
    let (mut app, mut sr, _recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig::default());
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let mut physical = GhosttyEngine::new(24, 80).expect("create physical oracle");

    app.handle_pty(
        &mut sr,
        b"\x1b]2;committed title\x1b\\committed base",
        &mut term_out,
    )
    .expect("queue committed base");
    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present committed base");
    physical
        .advance(&term_out)
        .expect("apply committed base to oracle");
    term_out.clear();

    app.handle_pty(
        &mut sr,
        b"\x1b[?2026h\x1b]2;working title\x1b\\\x1b[2J\x1b[Hpartial frame",
        &mut term_out,
    )
    .expect("open working frame");
    app.show_message(&mut sr, "Notice", "foreground overlay", &mut term_out)
        .expect("open overlay");
    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present overlay");
    physical
        .advance(&term_out)
        .expect("apply overlay to oracle");
    assert!(
        physical
            .normalized_snapshot()
            .contents()
            .contains("foreground overlay")
    );
    assert_eq!(
        physical.normalized_snapshot().title.as_deref(),
        Some("committed title"),
        "the overlay bypass must not release a working-frame title effect"
    );
    term_out.clear();

    app.handle_stdin(&mut sr, b"\r", &mut pty_out, &mut term_out)
        .expect("dismiss overlay");
    assert!(!app.has_overlay());
    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("reveal committed underlay");
    physical
        .advance(&term_out)
        .expect("apply committed underlay to oracle");
    let revealed = physical.normalized_snapshot().contents();
    assert!(revealed.contains("committed base"), "{revealed:?}");
    assert!(!revealed.contains("partial frame"), "{revealed:?}");
    assert!(!revealed.contains("foreground overlay"), "{revealed:?}");
    assert_eq!(
        physical.normalized_snapshot().title.as_deref(),
        Some("committed title")
    );
    assert_eq!(
        app.scheduled_output_timeout(),
        Some(std::time::Duration::from_millis(22)),
        "the revealed committed frame must wake accessibility before the application timeout"
    );
    clock.advance_ms(22);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize the physically revealed committed frame")
    );
    assert_eq!(
        app.scheduled_output_timeout(),
        Some(std::time::Duration::from_millis(70)),
        "the earlier accessibility wake must preserve the application's original timeout epoch"
    );
    assert!(pty_out.is_empty());

    term_out.clear();
    app.handle_pty(
        &mut sr,
        b"\x1b]2;final title\x1b\\\x1b[2J\x1b[Hcommitted frame\x1b[?2026l",
        &mut term_out,
    )
    .expect("close application frame");
    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present committed frame");
    physical
        .advance(&term_out)
        .expect("apply committed frame to oracle");
    let committed = physical.normalized_snapshot().contents();
    assert!(committed.contains("committed frame"), "{committed:?}");
    assert_eq!(
        physical.normalized_snapshot().title.as_deref(),
        Some("final title")
    );
}

#[test]
fn synchronized_timeout_republishes_live_frame_replaced_by_an_earlier_overlay_pop() {
    let (mut app, mut sr, _recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig::default());
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let mut physical = GhosttyEngine::new(24, 80).expect("create physical oracle");

    app.handle_pty(&mut sr, b"committed base", &mut term_out)
        .expect("queue committed base");
    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present committed base");
    physical
        .advance(&term_out)
        .expect("apply committed base to oracle");
    term_out.clear();

    app.handle_pty(
        &mut sr,
        b"\x1b[?2026h\x1b[2J\x1b[Hworking frame",
        &mut term_out,
    )
    .expect("open working frame");
    app.show_message(&mut sr, "Notice", "foreground overlay", &mut term_out)
        .expect("open overlay");
    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present overlay");
    physical
        .advance(&term_out)
        .expect("apply overlay to oracle");
    term_out.clear();

    app.handle_stdin(&mut sr, b"\r", &mut pty_out, &mut term_out)
        .expect("dismiss overlay before timeout");
    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present committed underlay");
    physical
        .advance(&term_out)
        .expect("apply committed underlay to oracle");
    let committed = physical.normalized_snapshot().contents();
    assert!(committed.contains("committed base"), "{committed:?}");
    assert!(!committed.contains("working frame"), "{committed:?}");
    term_out.clear();

    clock.advance_ms(100);
    let timeout = app
        .drain_scheduled_output(&mut term_out, false)
        .expect("time out the replaced working render");
    assert!(timeout.synchronization_timed_out);
    assert_eq!(
        app.scheduled_output_timeout(),
        Some(std::time::Duration::ZERO),
        "timeout recovery must queue a fresh live scene"
    );
    app.drain_scheduled_output(&mut term_out, false)
        .expect("publish timeout recovery scene");
    physical
        .advance(&term_out)
        .expect("apply timeout recovery scene to oracle");
    let released = physical.normalized_snapshot().contents();
    assert!(released.contains("working frame"), "{released:?}");
    assert!(!released.contains("foreground overlay"), "{released:?}");
    assert!(pty_out.is_empty());
}

#[test]
fn popping_an_overlay_after_synchronized_timeout_reveals_the_live_frame() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig::default());
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let mut physical = GhosttyEngine::new(24, 80).expect("create physical oracle");

    app.handle_pty(&mut sr, b"committed base", &mut term_out)
        .expect("queue committed base");
    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present committed base");
    physical
        .advance(&term_out)
        .expect("apply committed base to oracle");
    term_out.clear();

    app.handle_pty(
        &mut sr,
        b"\x1b[?2026h\x1b[2J\x1b[Hworking frame",
        &mut term_out,
    )
    .expect("open working frame");
    app.show_message(&mut sr, "Notice", "foreground overlay", &mut term_out)
        .expect("open overlay");
    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present overlay");
    physical
        .advance(&term_out)
        .expect("apply overlay to oracle");
    term_out.clear();

    clock.advance_ms(100);
    let timeout = app
        .drain_scheduled_output(&mut term_out, false)
        .expect("time out while overlay remains visible");
    assert!(timeout.synchronization_timed_out);
    assert!(
        physical
            .normalized_snapshot()
            .contents()
            .contains("foreground overlay")
    );

    app.handle_stdin(&mut sr, b"\r", &mut pty_out, &mut term_out)
        .expect("dismiss overlay after timeout");
    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the fail-open live underlay");
    physical
        .advance(&term_out)
        .expect("apply fail-open live underlay to oracle");
    let released = physical.normalized_snapshot().contents();
    assert!(released.contains("working frame"), "{released:?}");
    assert!(!released.contains("committed base"), "{released:?}");
    assert!(!released.contains("foreground overlay"), "{released:?}");
    app.handle_stdin(&mut sr, b"\x1bi", &mut pty_out, &mut term_out)
        .expect("read the physically presented fail-open frame");
    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("working frame".into(), false)),
        "the timeout receipt must publish the same live model as its pixels"
    );
    assert!(pty_out.is_empty());
}

#[test]
fn synchronized_timeout_does_not_auto_read_the_exposed_partial_frame() {
    let (mut app, mut sr, recorder, clock) = make_app();
    app.enable_output_scheduler(OutputSchedulerConfig::default());
    let mut term_out = Vec::new();

    app.handle_pty(
        &mut sr,
        b"\x1b[?2026h\x1b[2J\x1b[Hexposed partial",
        &mut term_out,
    )
    .expect("open working frame");
    clock.advance_ms(100);
    let timeout = app
        .drain_scheduled_output(&mut term_out, false)
        .expect("release the abandoned physical frame");
    assert!(timeout.synchronization_timed_out);
    assert_eq!(
        app.scheduled_output_timeout(),
        Some(std::time::Duration::ZERO),
        "timeout recovery queues one authoritative live scene"
    );
    app.drain_scheduled_output(&mut term_out, false)
        .expect("publish the authoritative timeout recovery scene");
    assert!(!app.maybe_finalize_changes(&mut sr).expect("remain blocked"));
    clock.advance_ms(1_000);
    assert_eq!(
        app.scheduled_output_timeout(),
        None,
        "the blocked accessibility frame must not create a zero-timeout spin"
    );
    assert!(
        !app.maybe_finalize_changes(&mut sr)
            .expect("remain blocked past the hard accessibility deadline")
    );
    assert!(
        recorder
            .inner
            .borrow()
            .speaks
            .iter()
            .all(|(text, _)| !text.contains("exposed partial"))
    );

    app.handle_pty(&mut sr, b"\x1b[?2026l", &mut term_out)
        .expect("close the working frame");
    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the real close");
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize only the real close")
    );
}

#[test]
fn overlay_pop_preserves_the_open_frames_maximum_stabilization_deadline() {
    let (mut app, mut sr, _recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(
        &mut sr,
        b"\x1b[?2026h\x1b[2J\x1b[Hworking frame",
        &mut term_out,
    )
    .expect("open working frame");
    app.show_message(&mut sr, "Notice", "foreground", &mut term_out)
        .expect("open overlay");
    clock.advance_ms(u128::from(MAX_DIFF_DELAY) + 1);

    app.handle_stdin(&mut sr, b"\r", &mut pty_out, &mut term_out)
        .expect("dismiss overlay");
    app.handle_pty(&mut sr, b"\x1b[?2026l", &mut term_out)
        .expect("close working frame");

    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize at the original maximum deadline"),
        "popping the overlay must not restart the open frame's stabilization window"
    );
    assert!(pty_out.is_empty());
}

#[test]
fn overlay_uses_live_geometry_while_accessibility_keeps_the_old_frame_geometry() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"old\x1b[?2026hpartial", &mut term_out)
        .expect("open working frame");
    app.on_resize(30, 100, &mut term_out)
        .expect("resize live terminal");
    app.show_message(&mut sr, "Notice", "full-size overlay", &mut term_out)
        .expect("open overlay at live geometry");

    let scene = app.composed_scene().expect("compose resized overlay");
    assert_eq!((scene.geometry.rows, scene.geometry.cols), (30, 100));
    assert_eq!(
        scene
            .overlays
            .last()
            .expect("message overlay")
            .surface
            .snapshot
            .size(),
        (30, 100)
    );
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
fn neovim_typeahead_suppression_skips_the_unechoed_append_command() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    sr.set_suppress_key_echo(true);
    app.handle_pty(
        &mut sr,
        b"\x1B[2J\x1B[23;1H[No Name]                                                   1,1            All\x1B[1;1H",
        &mut term_out,
    )
    .expect("draw empty editor screen");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    app.maybe_finalize_changes(&mut sr)
        .expect("finalize empty editor screen");
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_stdin(&mut sr, b"a", &mut pty_out, &mut term_out)
        .expect("queue append command");
    app.handle_stdin(&mut sr, b"h", &mut pty_out, &mut term_out)
        .expect("queue insert-mode typeahead");
    app.handle_pty(
        &mut sr,
        b"\x1B[?25lh\x1B[23;65H2\x1B[1;2H\x1B[34h\x1B[?25h",
        &mut term_out,
    )
    .expect("redraw inserted character and ruler");
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(
        app.maybe_finalize_changes(&mut sr)
            .expect("finalize insert-mode redraw")
    );

    assert!(
        recorder.inner.borrow().speaks.is_empty(),
        "speaks={:?}",
        recorder.inner.borrow().speaks
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
fn kitty_unbound_press_is_transcoded_and_release_is_dropped_for_legacy_child() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let input = b"\x1B[97;1:1u\x1B[97;1:3u";

    app.handle_stdin(&mut sr, input, &mut pty_out, &mut term_out)
        .expect("handle Kitty a press and release");

    assert_eq!(pty_out, b"a");
}

#[test]
fn kitty_special_key_event_types_are_transcoded_for_legacy_child() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(
        &mut sr,
        b"\x1B[1;1:1D\x1B[1;1:2D\x1B[1;1:3D",
        &mut pty_out,
        &mut term_out,
    )
    .expect("handle Kitty Left press, repeat, and release");

    assert_eq!(pty_out, b"\x1B[D\x1B[D");
}

#[test]
fn kitty_release_from_an_exited_full_screen_app_does_not_reach_the_resumed_shell() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let press = b"\x1B[99;5:1u";
    let release = b"\x1B[99;5:3u";

    app.handle_pty(&mut sr, b"\x1B[>7u", &mut term_out)
        .expect("enable child Kitty keyboard mode");
    app.handle_stdin(
        &mut sr,
        &[press.as_slice(), release.as_slice()].concat(),
        &mut pty_out,
        &mut term_out,
    )
    .expect("handle one contiguous Ctrl-c press and release read");
    assert_eq!(pty_out, press);

    pty_out.clear();
    app.handle_pty(&mut sr, b"\x1B[<u\r\nshell$ ", &mut term_out)
        .expect("full-screen app exits and restores its shell");
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .expect("reconcile deferred release after child output");
    assert!(
        pty_out.is_empty(),
        "late Kitty release leaked to resumed shell: {pty_out:?}"
    );

    app.handle_stdin(&mut sr, b"x", &mut pty_out, &mut term_out)
        .expect("forward later shell input");
    assert_eq!(pty_out, b"x");
}

#[test]
fn rapid_second_kitty_ctrl_c_cycle_does_not_reach_shell_after_mode_reset() {
    let (mut app, mut sr, _recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let press = b"\x1B[99;5:1u";
    let release = b"\x1B[99;5:3u";
    let cycle = [press.as_slice(), release.as_slice()].concat();

    app.handle_pty(&mut sr, b"\x1B[>7u", &mut term_out)
        .expect("enable child Kitty keyboard mode");
    app.handle_stdin(&mut sr, &cycle, &mut pty_out, &mut term_out)
        .expect("deliver the Ctrl-c cycle which exits the application");
    assert_eq!(pty_out, press);

    pty_out.clear();
    app.handle_pty(&mut sr, b"\x1B[<u\r\nshell$ ", &mut term_out)
        .expect("application exits before the second physical Ctrl-c arrives");
    app.handle_stdin(&mut sr, &cycle, &mut pty_out, &mut term_out)
        .expect("deliver a second cycle encoded under the outgoing mode");
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .expect("reconcile the first cycle's deferred release");
    assert!(
        pty_out.is_empty(),
        "rapid second Kitty Ctrl-c cycle leaked to resumed shell: {pty_out:?}"
    );

    clock.advance_ms(501);
    let later_input = b"\x1B[97;1:1u\x1B[97;1:3u";
    app.handle_stdin(&mut sr, later_input, &mut pty_out, &mut term_out)
        .expect("forward Kitty input after the bounded handoff window");
    assert_eq!(pty_out, b"a");
}

#[test]
fn deferred_kitty_ctrl_c_release_reaches_an_application_that_stays_active() {
    let (mut app, mut sr, _recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let press = b"\x1B[99;5:1u";
    let release = b"\x1B[99;5:3u";

    app.handle_pty(&mut sr, b"\x1B[>7u", &mut term_out)
        .expect("enable child Kitty keyboard mode");
    app.handle_stdin(
        &mut sr,
        &[press.as_slice(), release.as_slice()].concat(),
        &mut pty_out,
        &mut term_out,
    )
    .expect("handle Ctrl-c press and release");
    assert_eq!(pty_out, press);

    clock.advance_ms(1_000);
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .expect("release deferred Ctrl-c key-up");
    assert_eq!(pty_out, [press.as_slice(), release.as_slice()].concat());
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

    assert_eq!(pty_out, b"\x1B'");
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

    assert_eq!(pty_out, b"\x7F");
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
    let mut physical = GhosttyEngine::new(24, 80).expect("physical oracle");
    physical
        .advance(&term_out)
        .expect("parse compositor output");
    assert!(physical.normalized_snapshot().modes.focus_reporting);

    app.handle_stdin(&mut sr, b"\x1B[I", &mut pty_out, &mut term_out)
        .expect("handle stdin");

    assert_eq!(pty_out, b"\x1B[I");
    assert!(sr.terminal_focused());
}

#[test]
fn focus_mode_is_modeled_and_controls_only_application_input_routing() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut term_out = Vec::new();
    let mut pty_out = Vec::new();

    app.handle_pty(&mut sr, b"x\x1B[?10", &mut term_out)
        .expect("handle pty");
    let mut physical = GhosttyEngine::new(24, 80).expect("physical oracle");
    physical
        .advance(&term_out)
        .expect("parse first compositor frame");
    assert_eq!(physical.normalized_snapshot().contents().trim(), "x");
    term_out.clear();

    app.handle_pty(&mut sr, b"04hy", &mut term_out)
        .expect("handle pty");
    physical.advance(&term_out).expect("parse enabled frame");
    assert_eq!(physical.normalized_snapshot().contents().trim(), "xy");
    assert!(physical.normalized_snapshot().modes.focus_reporting);
    term_out.clear();

    app.handle_stdin(&mut sr, b"\x1B[I", &mut pty_out, &mut term_out)
        .expect("handle stdin");
    assert_eq!(pty_out, b"\x1B[I");

    app.handle_pty(&mut sr, b"\x1B[?1004l", &mut term_out)
        .expect("handle pty");
    physical.advance(&term_out).expect("parse disabled frame");
    assert!(physical.normalized_snapshot().modes.focus_reporting);

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
fn say_overlay_hotkey_speaks_terminal_mode_and_current_title() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"\x1b]2;Build status\x1b\\", &mut term_out)
        .expect("model application title");

    app.handle_stdin(&mut sr, b"\x1Bw", &mut pty_out, &mut term_out)
        .expect("handle stdin");

    let speaks = &recorder.inner.borrow().speaks;
    assert!(
        speaks
            .iter()
            .any(|(text, _)| text == "terminal, Build status"),
        "speaks={speaks:?}"
    );
}

#[test]
fn say_overlay_follows_the_completed_overlay_identity_across_backpressure() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"\x1b]2;Base title\x1b\\base", &mut term_out)
        .expect("present the initial terminal");
    app.enable_output_scheduler(OutputSchedulerConfig::default());
    term_out.clear();

    app.show_message(&mut sr, "Notice", "foreground", &mut term_out)
        .expect("queue the overlay");
    recorder.inner.borrow_mut().speaks.clear();
    app.handle_stdin(&mut sr, b"\x1bw", &mut pty_out, &mut term_out)
        .expect("say the still-presented terminal");
    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("terminal, Base title".into(), false))
    );

    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the overlay");
    app.handle_stdin(&mut sr, b"\x1bw", &mut pty_out, &mut term_out)
        .expect("say the presented overlay");
    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("Notice".into(), false))
    );

    app.handle_stdin(&mut sr, b"\r", &mut pty_out, &mut term_out)
        .expect("queue overlay dismissal");
    app.handle_stdin(&mut sr, b"\x1bw", &mut pty_out, &mut term_out)
        .expect("keep saying the physical overlay");
    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("Notice".into(), false))
    );

    clock.advance_ms(4);
    app.drain_scheduled_output(&mut term_out, false)
        .expect("present the revealed terminal");
    app.handle_stdin(&mut sr, b"\x1bw", &mut pty_out, &mut term_out)
        .expect("say the revealed terminal");
    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("terminal, Base title".into(), false))
    );
    assert!(pty_out.is_empty());
}

#[test]
fn say_overlay_tracks_a_flushed_title_effect_before_its_cell_render() {
    const TITLE_EFFECT_BYTES: usize = b"\x1b]2;working\x1b\\".len();

    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"\x1b]2;committed\x1b\\old content", &mut term_out)
        .expect("present the initial terminal");
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        write_budget_bytes: TITLE_EFFECT_BYTES,
        ..OutputSchedulerConfig::default()
    });
    term_out.clear();

    app.handle_pty(
        &mut sr,
        b"\x1b]2;working\x1b\\\r\x1b[2Knew content",
        &mut term_out,
    )
    .expect("queue a title and cell update");
    app.handle_stdin(&mut sr, b"\x1bw", &mut pty_out, &mut term_out)
        .expect("say the title before either transaction flushes");
    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("terminal, committed".into(), false))
    );

    let report = app
        .drain_scheduled_output(&mut term_out, false)
        .expect("flush exactly the title effect");
    assert_eq!(report.completed_effects.len(), 1);
    assert!(report.completed_renders.is_empty());

    app.handle_stdin(&mut sr, b"\x1bw", &mut pty_out, &mut term_out)
        .expect("say the physically applied title");
    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("terminal, working".into(), false))
    );
    app.handle_stdin(&mut sr, b"\x1bi", &mut pty_out, &mut term_out)
        .expect("read the still-presented cells");
    assert_eq!(
        recorder.inner.borrow().speaks.last(),
        Some(&("old content".into(), false))
    );
    assert!(pty_out.is_empty());
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
    let contents = app.debug_active_view_contents();
    assert!(contents.contains("> print(1)"));
    assert!(contents.lines().any(|line| line.trim() == "1"));
    let rendered = String::from_utf8_lossy(&term_out);
    assert!(rendered.contains("> print(1)"));
    let speaks = &recorder.inner.borrow().speaks;
    assert!(speaks.iter().any(|(text, _)| text == "Lua REPL"));
}

#[test]
fn lua_repl_session_persists_transcript_draft_continuation_and_environment_after_close() {
    let (mut app, mut sr, _recorder, clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"\x1BL", &mut pty_out, &mut term_out)
        .expect("open repl");
    app.handle_stdin(&mut sr, b"saved = 17\r", &mut pty_out, &mut term_out)
        .expect("define saved value");
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .expect("finish definition");
    app.handle_stdin(
        &mut sr,
        b"function pending()\rreturn saved",
        &mut pty_out,
        &mut term_out,
    )
    .expect("enter pending function");

    app.handle_stdin(&mut sr, b"\x1B", &mut pty_out, &mut term_out)
        .expect("queue escape");
    clock.advance_ms(100);
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .expect("close repl");
    app.handle_stdin(&mut sr, b"\x1BL", &mut pty_out, &mut term_out)
        .expect("reopen repl");

    let contents = app.debug_active_view_contents();
    assert!(contents.contains("> saved = 17"), "contents={contents:?}");
    assert!(
        contents.contains("> function pending()"),
        "contents={contents:?}"
    );
    assert!(
        contents.contains("... return saved"),
        "contents={contents:?}"
    );

    app.handle_stdin(&mut sr, b"\x03saved\r", &mut pty_out, &mut term_out)
        .expect("abort pending chunk and evaluate saved value");
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .expect("finish saved value evaluation");
    assert!(
        app.debug_active_view_contents()
            .lines()
            .any(|line| line.trim() == "17")
    );
    assert!(pty_out.is_empty());
}

#[test]
fn lua_repl_kitty_ctrl_c_aborts_continuation_and_ctrl_u_clears_current_line() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(
        &mut sr,
        b"\x1BLfunction abandoned()\rdiscard me",
        &mut pty_out,
        &mut term_out,
    )
    .expect("open repl and enter continuation");
    app.handle_stdin(&mut sr, b"\x1B[117;5u", &mut pty_out, &mut term_out)
        .expect("Kitty Ctrl-U");
    let contents = app.debug_active_view_contents();
    assert!(contents.contains("> function abandoned()"));
    assert!(contents.contains("..."));
    assert!(!contents.contains("discard me"));

    app.handle_stdin(&mut sr, b"still discarded", &mut pty_out, &mut term_out)
        .expect("replace current continuation line");
    app.handle_stdin(&mut sr, b"\x1B[99;5u", &mut pty_out, &mut term_out)
        .expect("Kitty Ctrl-C");
    let contents = app.debug_active_view_contents();
    assert!(contents.lines().any(|line| line.trim() == ">"));
    assert!(!contents.contains("function abandoned"));
    assert!(!contents.contains("still discarded"));
    assert!(pty_out.is_empty());
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
fn kitty_associated_text_is_transcoded_for_legacy_child() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let input = b"\x1B[45;2;95u";

    app.handle_stdin(&mut sr, input, &mut pty_out, &mut term_out)
        .expect("forward Kitty associated-text event");

    assert_eq!(pty_out, b"_");
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
fn extended_shift_enter_becomes_enter_for_legacy_child() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    for input in [b"\x1B[27;2;13~".as_slice(), b"\x1B[13;2u"] {
        app.handle_stdin(&mut sr, input, &mut pty_out, &mut term_out)
            .expect("forward Shift-Enter to a legacy child");
        assert_eq!(pty_out, b"\r", "input={input:?}");
        pty_out.clear();
    }
}

#[test]
fn modify_other_keys_shift_enter_is_preserved_for_extended_keyboard_child() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let input = b"\x1B[27;2;13~";
    app.handle_pty(&mut sr, b"\x1B[>1u", &mut term_out)
        .expect("enable child Kitty keyboard mode");

    app.handle_stdin(&mut sr, input, &mut pty_out, &mut term_out)
        .expect("forward Shift-Enter to an extended-keyboard child");

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
    assert_eq!(
        app.scheduled_output_timeout(),
        Some(std::time::Duration::from_millis(50))
    );
    clock.advance_ms(49);
    assert_eq!(
        app.scheduled_output_timeout(),
        Some(std::time::Duration::from_millis(1))
    );
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .unwrap();
    assert!(pty_out.is_empty());

    clock.advance_ms(1);
    app.handle_tick(&mut sr, &mut pty_out, &mut term_out)
        .unwrap();

    assert_eq!(pty_out, b"\x1B");
    assert_eq!(app.scheduled_output_timeout(), None);
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
fn unterminated_terminal_input_sequence_has_a_hard_memory_bound() {
    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let mut input = b"\x1B]".to_vec();
    input.extend(std::iter::repeat_n(
        b'x',
        MAX_PENDING_TERMINAL_INPUT_BYTES * 2,
    ));

    app.handle_stdin(&mut sr, &input, &mut pty_out, &mut term_out)
        .unwrap();

    assert!(app.debug_pending_terminal_input_bytes() < MAX_PENDING_TERMINAL_INPUT_BYTES);
    assert!(pty_out.len() >= MAX_PENDING_TERMINAL_INPUT_BYTES);
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
fn underlying_application_title_changes_stay_silent_in_the_same_overlay() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut term_out = Vec::new();

    app.show_message(&mut sr, "Notice", "foreground", &mut term_out)
        .unwrap();
    recorder.inner.borrow_mut().speaks.clear();

    app.handle_pty(
        &mut sr,
        b"\x1b]2;changed behind the overlay\x1b\\",
        &mut term_out,
    )
    .unwrap();
    clock.advance_ms(u128::from(DIFF_DELAY) + 1);
    assert!(app.maybe_finalize_changes(&mut sr).unwrap());
    assert!(recorder.inner.borrow().speaks.is_empty());
}

#[test]
fn resize_and_alternate_screen_transition_remain_live_behind_overlay() {
    use lector::terminal::TerminalGeometry;

    let (mut app, mut sr, _recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();
    let mut physical = GhosttyEngine::new(24, 80).expect("physical oracle");
    physical
        .advance(b"\x1b[?1049h")
        .expect("enter Lector-owned alternate screen");
    app.show_message(&mut sr, "Notice", "foreground", &mut term_out)
        .unwrap();
    physical.advance(&term_out).expect("parse initial overlay");

    term_out.clear();
    app.handle_pty(&mut sr, b"\x1b[?1049halt-hidden", &mut term_out)
        .unwrap();
    assert!(term_out.is_empty(), "hidden output={term_out:?}");

    let geometry = TerminalGeometry::new(12, 40, 9, 18);
    app.on_resize_with_geometry(geometry, &mut term_out)
        .unwrap();
    physical
        .resize_with_geometry(geometry)
        .expect("resize physical oracle");
    physical.advance(&term_out).expect("parse resized overlay");
    assert_eq!(app.debug_root_terminal_geometry(), geometry);
    assert!(String::from_utf8_lossy(&term_out).contains("foreground"));

    term_out.clear();
    app.handle_stdin(&mut sr, b"\n", &mut pty_out, &mut term_out)
        .unwrap();
    physical
        .advance(&term_out)
        .expect("parse restored alternate screen");
    assert!(!app.has_overlay());
    assert!(physical.normalized_snapshot().alternate_screen());
    assert!(
        physical
            .normalized_snapshot()
            .contents()
            .contains("alt-hidden")
    );
    assert!(app.debug_active_view_contents().contains("alt-hidden"));
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
