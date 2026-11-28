use lector::{
    app::{App, Clock, TmuxBellSource},
    output_scheduler::OutputSchedulerConfig,
    presentation::{OutputTransaction, PresentedScene, RenderBatch, RenderOracle},
    screen_reader::{ScreenReader, TmuxBellMode},
    speech,
    tmux_gateway::TmuxGatewayRouter,
    tmux_model::{INVENTORY_REPLY_COUNT, PaneId, SessionId, WindowId},
    views,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    cell::{Cell, RefCell},
    io::{Read, Write},
    path::PathBuf,
    rc::Rc,
    sync::mpsc,
    thread,
    time::SystemTime,
};

const SPLIT: &str = "b25f,80x24,0,0{40x24,0,0,20,39x24,41,0,23}";
const HIDDEN: &str = "b25f,80x24,0,0,21";
const UNATTACHED: &str = "b25f,80x24,0,0,22";

#[derive(Clone, Default)]
struct TestClock(Rc<Cell<u128>>);

impl TestClock {
    fn advance_ms(&self, delta: u128) {
        self.0.set(self.0.get().saturating_add(delta));
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> u128 {
        self.0.get()
    }
}

#[derive(Clone, Default)]
struct Recorder(Rc<RefCell<Vec<String>>>);

impl Recorder {
    fn clear(&self) {
        self.0.borrow_mut().clear();
    }

    fn messages(&self) -> Vec<String> {
        self.0.borrow().clone()
    }
}

impl speech::Driver for Recorder {
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

fn inventory(connection_id: u64) -> Vec<Vec<Vec<u8>>> {
    vec![
        vec![b"S\t$1\twork".to_vec(), b"S\t$2\tother".to_vec()],
        vec![
            format!("W\t$1\t@10\t1\t1\t{SPLIT}\t{SPLIT}\t*\teditor").into_bytes(),
            format!("W\t$1\t@11\t2\t0\t{HIDDEN}\t{HIDDEN}\t-\tlogs").into_bytes(),
            format!("W\t$2\t@12\t1\t1\t{UNATTACHED}\t{UNATTACHED}\t*\tremote").into_bytes(),
        ],
        vec![
            b"P\tstatus\t@10\t%20\t1\t1\t0\t0\t40\t24\t0\t0\t0\t1\t0\t0\t0\t0\tactive".to_vec(),
            b"P\tstatus\t@10\t%23\t2\t0\t41\t0\t39\t24\t0\t0\t0\t1\t0\t0\t0\t0\tinactive".to_vec(),
            b"P\tstatus\t@11\t%21\t1\t1\t0\t0\t80\t24\t0\t0\t0\t1\t0\t0\t0\t0\thidden".to_vec(),
            b"P\tstatus\t@12\t%22\t1\t1\t0\t0\t80\t24\t0\t0\t0\t1\t0\t0\t0\t0\tunattached".to_vec(),
        ],
        vec![b"A\t$1".to_vec()],
        vec![b"O\tbase-index\t1".to_vec()],
        vec![b"O\tpane-base-index\t1".to_vec()],
        vec![format!("C\tclient_name\t/dev/ttys-bell-{connection_id}").into_bytes()],
        vec![b"O\tprefix\tC-a".to_vec()],
        vec![b"O\tprefix2\tNone".to_vec()],
        vec![b"O\tkey-table\troot".to_vec()],
        vec![b"O\trepeat-time\t500".to_vec()],
        vec![b"B\td\t0\tdetach-client".to_vec()],
    ]
}

fn reply(serial: usize, lines: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = format!("%begin {serial} {serial} 0\n").into_bytes();
    for line in lines {
        bytes.extend_from_slice(line);
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(format!("%end {serial} {serial} 0\n").as_bytes());
    bytes
}

fn feed(
    app: &mut App,
    sr: &mut ScreenReader,
    router: &mut TmuxGatewayRouter,
    bytes: &[u8],
    physical: &mut Vec<u8>,
) {
    for event in router.push(bytes).unwrap() {
        app.handle_tmux_gateway_event(sr, event, physical).unwrap();
    }
}

fn add_ready_connection(
    app: &mut App,
    sr: &mut ScreenReader,
    physical: &mut Vec<u8>,
    connection_id: u64,
) -> TmuxGatewayRouter {
    add_connection_with_bootstrap_sample(app, sr, physical, connection_id, &[], &[], None)
}

struct BootstrapSample<'a> {
    pane: u64,
    state: &'a str,
    name: &'a str,
    message: &'a str,
    pending_escape: &'a [u8],
}

fn add_connection_with_bootstrap_sample(
    app: &mut App,
    sr: &mut ScreenReader,
    physical: &mut Vec<u8>,
    connection_id: u64,
    pending_records: &[Vec<u8>],
    reports_after_sample: &[(u64, &str)],
    captured_status: Option<BootstrapSample<'_>>,
) -> TmuxGatewayRouter {
    let mut router = TmuxGatewayRouter::with_first_connection_id(connection_id);
    feed(
        app,
        sr,
        &mut router,
        b"\x1bP1000p%begin 1 1 0\n%end 1 1 0\n",
        physical,
    );
    let mut commands = Vec::new();
    app.drain_tmux_commands_for(connection_id, &mut commands)
        .unwrap();
    assert_eq!(
        commands,
        [
            lector::app::TMUX_FLOW_CONTROL_COMMAND,
            lector::app::TMUX_FLOW_CONTROL_VERIFY_COMMAND,
            b"refresh-client -C 80x24\n",
            lector::tmux_model::INVENTORY_COMMAND.as_bytes(),
        ]
        .concat()
    );

    feed(app, sr, &mut router, &reply(2, &[]), physical);
    feed(
        app,
        sr,
        &mut router,
        &reply(3, &[b"attached,control-mode,pause-after=1".to_vec()]),
        physical,
    );
    feed(app, sr, &mut router, &reply(4, &[]), physical);

    let groups = inventory(connection_id);
    assert_eq!(groups.len(), INVENTORY_REPLY_COUNT);
    for (index, group) in groups.iter().enumerate() {
        feed(app, sr, &mut router, &reply(index + 5, group), physical);
    }

    commands.clear();
    app.drain_tmux_commands_for(connection_id, &mut commands)
        .unwrap();
    for pane_id in [20, 21, 22, 23] {
        assert!(
            String::from_utf8_lossy(&commands).contains(&format!("-t %{pane_id}\n")),
            "missing pane %{pane_id} capture in {:?}",
            String::from_utf8_lossy(&commands)
        );
    }
    for record in pending_records {
        feed(app, sr, &mut router, record, physical);
    }
    let commands = std::str::from_utf8(&commands).unwrap();
    assert_eq!(
        app.debug_tmux_expected_reply_count(connection_id),
        Some(commands.lines().count()),
        "each bootstrap sequence must occupy one bounded reply slot"
    );
    for (index, command) in commands.lines().enumerate() {
        let (_, target) = command.rsplit_once("-t %").expect("capture target");
        let pane_id: u64 = target.parse().unwrap();
        let pending = captured_status
            .as_ref()
            .filter(|sample| sample.pane == pane_id);
        let sample = pending.map_or_else(
            || format!("T\t%{pane_id}\t\t"),
            |sample| {
                format!(
                    "T\t%{pane_id}\t{}\t{}\t{}",
                    sample.state, sample.name, sample.message
                )
            },
        );
        feed(
            app,
            sr,
            &mut router,
            &reply(30 + index * 3, &[sample.into_bytes()]),
            physical,
        );
        for &(target, report) in reports_after_sample
            .iter()
            .filter(|(target, _)| *target == pane_id)
        {
            feed(app, sr, &mut router, &pane_output(target, report), physical);
        }
        let pending_escape = pending
            .map(|sample| sample.pending_escape.to_vec())
            .into_iter()
            .collect::<Vec<_>>();
        feed(
            app,
            sr,
            &mut router,
            &reply(31 + index * 3, &pending_escape),
            physical,
        );
        feed(
            app,
            sr,
            &mut router,
            &reply(32 + index * 3, &[format!("ready-{pane_id}").into_bytes()]),
            physical,
        );
    }
    router
}

fn make_app(scheduled: bool) -> (App, ScreenReader, Recorder, TestClock, Vec<u8>) {
    let recorder = Recorder::default();
    let clock = TestClock::default();
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(24, 80)));
    let mut app = App::new_with_clock(stack, Box::new(clock.clone())).unwrap();
    if scheduled {
        app.enable_output_scheduler(OutputSchedulerConfig::default());
    }
    let sr = ScreenReader::new(speech::Speech::new(Box::new(recorder.clone())));
    (app, sr, recorder, clock, Vec::new())
}

fn pane_output(pane_id: u64, encoded_payload: &str) -> Vec<u8> {
    format!("%output %{pane_id} {encoded_payload}\n").into_bytes()
}

fn assert_context(message: &str, connection: u64, window: u64, pane: u64, title: &str) {
    for expected in [
        format!("connection {connection}"),
        "session 1 work".to_owned(),
        format!("window {window}"),
        format!("pane {pane} {title}"),
    ] {
        assert!(
            message.contains(&expected),
            "{expected:?} absent from {message:?}"
        );
    }
}

#[test]
fn mode_defaults_audible_and_parses_only_the_documented_values() {
    let (_app, mut sr, _recorder, _clock, _physical) = make_app(false);
    assert_eq!(sr.tmux_bell_mode(), TmuxBellMode::Audible);
    for (text, mode) in [
        ("off", TmuxBellMode::Off),
        ("spoken", TmuxBellMode::Spoken),
        ("audible", TmuxBellMode::Audible),
    ] {
        assert_eq!(text.parse::<TmuxBellMode>().unwrap(), mode);
        assert_eq!(mode.to_string(), text);
        sr.set_tmux_bell_mode(mode);
        assert_eq!(sr.tmux_bell_mode(), mode);
    }
    assert!("beep".parse::<TmuxBellMode>().is_err());
}

#[test]
fn spoken_bells_cover_active_inactive_and_hidden_panes_but_not_unattached_sessions() {
    let (mut app, mut sr, recorder, _clock, mut physical) = make_app(false);
    let mut router = add_ready_connection(&mut app, &mut sr, &mut physical, 1);
    recorder.clear();

    // Explicit off mode remains available even though audible is the default.
    sr.set_tmux_bell_mode(TmuxBellMode::Off);
    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(20, "\\007"),
        &mut physical,
    );
    assert!(recorder.messages().is_empty());
    assert!(app.last_tmux_bell_source().is_none());

    sr.set_tmux_bell_mode(TmuxBellMode::Spoken);
    for (pane, window, title) in [(20, 10, "active"), (23, 10, "inactive"), (21, 11, "hidden")] {
        feed(
            &mut app,
            &mut sr,
            &mut router,
            &pane_output(pane, "\\007"),
            &mut physical,
        );
        let messages = recorder.messages();
        assert_context(messages.last().unwrap(), 1, window, pane, title);
    }
    assert_eq!(recorder.messages().len(), 3);

    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(22, "\\007"),
        &mut physical,
    );
    assert_eq!(
        recorder.messages().len(),
        3,
        "unattached pane was announced"
    );
    assert_eq!(
        app.last_tmux_bell_source(),
        Some(&TmuxBellSource {
            connection_id: 1,
            connection_label: "tmux 1".to_owned(),
            session_id: SessionId(1),
            session_name: "work".to_owned(),
            window_id: WindowId(11),
            window_name: "logs".to_owned(),
            pane_id: PaneId(21),
            pane_title: "hidden".to_owned(),
        })
    );
}

#[test]
fn spoken_bells_survive_overlays_sync_and_floods_and_coalesce_only_rapid_duplicates() {
    let (mut app, mut sr, recorder, clock, mut physical) = make_app(false);
    let mut router = add_ready_connection(&mut app, &mut sr, &mut physical, 1);
    sr.set_tmux_bell_mode(TmuxBellMode::Spoken);
    app.show_message(&mut sr, "overlay", "still visible", &mut physical)
        .unwrap();
    recorder.clear();

    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(21, "\\007"),
        &mut physical,
    );
    assert!(app.has_overlay());
    assert_eq!(recorder.messages().len(), 1);

    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(23, "\\033[?2026hinside\\007\\033[?2026l"),
        &mut physical,
    );
    assert_eq!(recorder.messages().len(), 2, "synchronized BEL was lost");

    let flood = "\\007".repeat(10_000);
    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(20, &flood),
        &mut physical,
    );
    assert_eq!(
        recorder.messages().len(),
        3,
        "one flood became many notices"
    );
    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(20, "\\007"),
        &mut physical,
    );
    assert_eq!(
        recorder.messages().len(),
        3,
        "rapid duplicate was not coalesced"
    );

    // The three different pane sources above were all announced at the same
    // fake-clock instant, so coalescing is source-local rather than global.
    clock.advance_ms(500);
    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(20, "\\007"),
        &mut physical,
    );
    assert_eq!(
        recorder.messages().len(),
        4,
        "later bell was over-coalesced"
    );
}

#[test]
fn audible_bells_follow_complete_visible_transactions_and_hidden_panes_use_the_scheduler() {
    let (mut app, mut sr, _recorder, clock, mut physical) = make_app(true);
    let mut router = add_ready_connection(&mut app, &mut sr, &mut physical, 1);
    sr.set_tmux_bell_mode(TmuxBellMode::Audible);
    app.drain_scheduled_output(&mut physical, true).unwrap();
    let initial_render = physical.clone();
    physical.clear();

    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(20, "visible-text\\007"),
        &mut physical,
    );
    assert!(physical.is_empty(), "audible bell bypassed the scheduler");
    clock.advance_ms(10);
    app.drain_scheduled_output(&mut physical, false).unwrap();
    assert_eq!(physical.iter().filter(|byte| **byte == b'\x07').count(), 1);
    assert_eq!(
        physical.last(),
        Some(&b'\x07'),
        "BEL split a render transaction"
    );
    let mut complete_output = initial_render;
    complete_output.extend_from_slice(&physical);
    let mut scene = app.composed_scene().unwrap();
    scene.effects.bell_count = 1;
    let intended = PresentedScene::compose(&scene).unwrap();
    RenderOracle::new(scene.geometry)
        .unwrap()
        .verify(
            "tmux-visible-bell-scheduler-boundary",
            &intended,
            &RenderBatch::new(
                vec![OutputTransaction::new(&complete_output)],
                intended.clone(),
            ),
        )
        .unwrap();

    physical.clear();
    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(21, "\\007"),
        &mut physical,
    );
    assert!(physical.is_empty());
    clock.advance_ms(10);
    app.drain_scheduled_output(&mut physical, false).unwrap();
    assert_eq!(
        physical, b"\x07",
        "hidden pane BEL did not use sole scheduler"
    );
}

#[test]
fn ordinary_background_window_activity_is_silent() {
    let (mut app, mut sr, recorder, clock, mut physical) = make_app(true);
    let mut router = add_ready_connection(&mut app, &mut sr, &mut physical, 1);
    sr.set_tmux_bell_mode(TmuxBellMode::Audible);
    app.drain_scheduled_output(&mut physical, true).unwrap();
    physical.clear();
    recorder.clear();

    feed(
        &mut app,
        &mut sr,
        &mut router,
        b"%session-window-changed $1 @11\n",
        &mut physical,
    );
    app.drain_scheduled_output(&mut physical, true).unwrap();
    physical.clear();

    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(20, "ordinary-background-output"),
        &mut physical,
    );
    clock.advance_ms(10);
    app.drain_scheduled_output(&mut physical, true).unwrap();
    assert_eq!(
        physical.iter().filter(|byte| **byte == b'\x07').count(),
        0,
        "ordinary output in a background window became a bell"
    );
    assert!(recorder.messages().is_empty());

    sr.set_tmux_bell_mode(TmuxBellMode::Spoken);
    physical.clear();
    recorder.clear();
    clock.advance_ms(500);
    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(20, "more-ordinary-background-output"),
        &mut physical,
    );
    app.drain_scheduled_output(&mut physical, true).unwrap();
    assert!(physical.is_empty());
    assert!(recorder.messages().is_empty());
}

#[test]
fn audible_bells_speak_only_background_tmux_indexes_and_latch_that_window() {
    let (mut app, mut sr, recorder, clock, mut physical) = make_app(true);
    let mut router = add_ready_connection(&mut app, &mut sr, &mut physical, 1);
    sr.set_tmux_bell_mode(TmuxBellMode::Audible);
    app.drain_scheduled_output(&mut physical, true).unwrap();
    physical.clear();
    recorder.clear();

    // An inactive pane in the active split window still belongs to the active
    // window, so its physical bell must not receive a spoken location.
    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(23, "\\007"),
        &mut physical,
    );
    clock.advance_ms(10);
    app.drain_scheduled_output(&mut physical, false).unwrap();
    assert_eq!(physical.iter().filter(|byte| **byte == b'\x07').count(), 1);
    assert!(recorder.messages().is_empty());

    physical.clear();
    clock.advance_ms(500);

    feed(
        &mut app,
        &mut sr,
        &mut router,
        b"%session-window-changed $1 @11\n",
        &mut physical,
    );
    app.drain_scheduled_output(&mut physical, true).unwrap();
    physical.clear();
    recorder.clear();

    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(21, "\\007"),
        &mut physical,
    );
    clock.advance_ms(10);
    app.drain_scheduled_output(&mut physical, false).unwrap();
    assert_eq!(physical.iter().filter(|byte| **byte == b'\x07').count(), 1);
    assert!(recorder.messages().is_empty());

    physical.clear();
    recorder.clear();
    clock.advance_ms(500);
    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(20, "\\007"),
        &mut physical,
    );
    app.drain_scheduled_output(&mut physical, true).unwrap();
    assert_eq!(physical.iter().filter(|byte| **byte == b'\x07').count(), 1);
    assert_eq!(recorder.messages(), ["bell in pane 1.1"]);

    physical.clear();
    recorder.clear();
    clock.advance_ms(4_000);
    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(20, "\\007"),
        &mut physical,
    );
    app.drain_scheduled_output(&mut physical, true).unwrap();
    assert!(physical.is_empty(), "background window replayed its bell");
    assert!(recorder.messages().is_empty());

    feed(
        &mut app,
        &mut sr,
        &mut router,
        b"%session-window-changed $1 @10\n%session-window-changed $1 @11\n",
        &mut physical,
    );
    app.drain_scheduled_output(&mut physical, true).unwrap();
    physical.clear();
    recorder.clear();
    clock.advance_ms(500);
    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(23, "\\007"),
        &mut physical,
    );
    app.drain_scheduled_output(&mut physical, true).unwrap();
    assert_eq!(physical.iter().filter(|byte| **byte == b'\x07').count(), 1);
    assert_eq!(recorder.messages(), ["bell in pane 1.2"]);
}

#[test]
fn session_reentry_discards_all_raw_effects_from_stale_background_panes() {
    let (mut app, mut sr, recorder, _clock, mut physical) = make_app(true);
    let mut router = add_ready_connection(&mut app, &mut sr, &mut physical, 1);
    sr.set_tmux_bell_mode(TmuxBellMode::Audible);
    let before = app.debug_tmux_pane_contents(1, 20).unwrap();

    // Match the live failure: leave Codex in window 1, select window 2, move
    // to another session, then return. Entering the session marks every pane
    // stale until its capture completes, so any intervening pane output takes
    // the skipped-output path.
    feed(
        &mut app,
        &mut sr,
        &mut router,
        b"%session-window-changed $1 @11\n%session-changed $2 other\n%session-changed $1 work\n",
        &mut physical,
    );
    app.drain_scheduled_output(&mut physical, true).unwrap();
    physical.clear();
    recorder.clear();

    // The first record can be a suffix of an OSC whose introducer was emitted
    // while this control client was attached elsewhere. Complete controls are
    // equally unsafe: without the missing prefix there is no trustworthy
    // parser state in which to interpret any raw byte or terminal side effect.
    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(20, "dex\\007"),
        &mut physical,
    );
    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(20, "\\033[2JSTALE"),
        &mut physical,
    );
    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(20, "\\007"),
        &mut physical,
    );
    app.drain_scheduled_output(&mut physical, true).unwrap();
    assert_eq!(app.debug_tmux_pane_contents(1, 20).unwrap(), before);
    assert_eq!(physical.iter().filter(|byte| **byte == b'\x07').count(), 0);
    assert!(recorder.messages().is_empty());
    assert!(app.last_tmux_bell_source().is_none());
    assert!(
        app.debug_tmux_pane_flow_state(1, 20)
            .unwrap()
            .skipped_incremental_bytes
            > 0
    );
}

#[test]
fn bells_are_scoped_by_connection_even_when_the_source_connection_is_not_presented() {
    let (mut app, mut sr, recorder, _clock, mut physical) = make_app(false);
    let mut first = add_ready_connection(&mut app, &mut sr, &mut physical, 1);
    let mut second = add_ready_connection(&mut app, &mut sr, &mut physical, 2);
    assert_eq!(app.active_tmux_connection(), Some(2));
    sr.set_tmux_bell_mode(TmuxBellMode::Spoken);
    recorder.clear();

    feed(
        &mut app,
        &mut sr,
        &mut first,
        &pane_output(21, "\\007"),
        &mut physical,
    );
    assert_eq!(app.active_tmux_connection(), Some(2));
    assert_context(&recorder.messages()[0], 1, 11, 21, "hidden");

    feed(
        &mut app,
        &mut sr,
        &mut second,
        &pane_output(23, "\\007"),
        &mut physical,
    );
    assert_context(&recorder.messages()[1], 2, 10, 23, "inactive");
    assert_eq!(app.last_tmux_bell_source().unwrap().connection_id, 2);

    feed(
        &mut app,
        &mut sr,
        &mut second,
        b"%exit\n\x1b\\",
        &mut physical,
    );
    assert!(
        app.last_tmux_bell_source().is_none(),
        "removed connection left a stale source locator"
    );
}

fn write_real_commands(
    app: &mut App,
    sr: &mut ScreenReader,
    writer: &mut dyn Write,
    physical: &mut Vec<u8>,
) {
    let mut commands = Vec::new();
    app.handle_tick(sr, &mut commands, physical).unwrap();
    if !commands.is_empty() {
        writer.write_all(&commands).unwrap();
        writer.flush().unwrap();
    }
}

fn drive_real_tmux(
    case: &str,
    app: &mut App,
    sr: &mut ScreenReader,
    receiver: &mpsc::Receiver<Vec<u8>>,
    writer: &mut dyn Write,
    physical: &mut Vec<u8>,
    mut done: impl FnMut(&mut App) -> bool,
) {
    if done(app) {
        return;
    }
    let result = super::drive_real_tmux_phase(receiver, None, |chunk| {
        if let Some(chunk) = chunk {
            app.handle_pty(sr, &chunk, physical).unwrap();
        }
        write_real_commands(app, sr, writer, physical);
        Ok::<_, mpsc::RecvTimeoutError>(done(app))
    });
    if let Err(error) = result {
        panic!(
            "failed to reach {case}: {error:?}; contents={:?}; topology={:?}",
            app.debug_active_view_contents(),
            app.debug_tmux_topology(1)
        );
    }
}

fn pane_id_at_index(topology: &str, wanted_index: u64) -> Option<u64> {
    topology.lines().find_map(|line| {
        let pane = line.trim().strip_prefix("pane %")?;
        let (pane_id, rest) = pane.split_once(" index ")?;
        let index = rest.split(':').next()?.parse::<u64>().ok()?;
        (index == wanted_index)
            .then(|| pane_id.parse::<u64>().ok())
            .flatten()
    })
}

#[test]
#[ignore = "run through scripts/test-real-tmux-docker"]
fn real_tmux_ignores_background_activity_and_reports_pane_bells() {
    let _serial = super::serialize_real_tmux_test();
    let tmux = std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .expect("tmux integration tests require tmux on PATH");
    assert!(tmux.status.success(), "tmux -V failed");

    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let socket_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-tmux");
    std::fs::create_dir_all(&socket_dir).unwrap();
    let socket = socket_dir.join(format!("bells-{}-{unique}.sock", std::process::id()));
    let session = format!("lector_bells_{}_{unique}", std::process::id());
    let mut client = RealTmuxClient::start(
        &socket,
        &[
            "-f",
            "/dev/null",
            "-CC",
            "new-session",
            "-s",
            &session,
            "/bin/sh -c 'printf ACTIVE_READY; exec /bin/sh'",
        ],
    );
    client.drive("initial activity pane bootstrap", |app| {
        app.debug_active_view_contents().contains("ACTIVE_READY")
    });
    let topology = client.app.debug_tmux_topology(1).unwrap();
    let background_pane = pane_id_at_index(&topology, 0).expect("background pane id");

    super::run_real_tmux_fixture_command(
        &socket,
        &[
            "new-window",
            "-t",
            &session,
            "-n",
            "foreground",
            "/bin/sh -c 'printf FOREGROUND_READY; exec /bin/sh'",
        ],
    );
    client.drive("foreground window bootstrap", |app| {
        app.debug_active_view_contents()
            .contains("FOREGROUND_READY")
    });

    client.sr.set_tmux_bell_mode(TmuxBellMode::Spoken);
    client.recorder.clear();
    super::run_real_tmux_fixture_command(
        &socket,
        &[
            "send-keys",
            "-t",
            &format!("%{background_pane}"),
            "printf BACKGROUND_DONE",
            "Enter",
        ],
    );
    client.drive("background window activity", |app| {
        app.debug_tmux_pane_contents(1, background_pane)
            .is_some_and(|contents| contents.contains("BACKGROUND_DONE"))
    });
    assert!(
        client.recorder.messages().is_empty(),
        "ordinary background activity was announced: {:?}",
        client.recorder.messages()
    );
    assert!(client.app.last_tmux_bell_source().is_none());

    super::run_real_tmux_fixture_command(
        &socket,
        &[
            "send-keys",
            "-t",
            &format!("%{background_pane}"),
            "printf '\\007'",
            "Enter",
        ],
    );
    let recorder = client.recorder.clone();
    client.drive("background pane bell", |_| !recorder.messages().is_empty());
    let source = client
        .app
        .last_tmux_bell_source()
        .expect("real pane bell source");
    assert_eq!(source.connection_id, 1);
    assert_eq!(source.pane_id, PaneId(background_pane));
    assert_eq!(source.session_name, session);
    assert!(
        recorder.messages()[0].starts_with("bell in tmux connection"),
        "speech={:?}",
        recorder.messages()
    );
    assert!(
        recorder.messages()[0].contains(&format!("pane {background_pane}")),
        "speech={:?}",
        recorder.messages()
    );

    recorder.clear();
    client
        .app
        .show_pending_tmux_panes(&mut client.sr, &mut client.physical)
        .unwrap();
    assert_eq!(recorder.messages(), ["window 0, pane 0, bell"]);
    client
        .app
        .handle_stdin(&mut client.sr, b"\r", &mut Vec::new(), &mut client.physical)
        .unwrap();
    client.drive("visit the pane that rang", |app| {
        !app.has_overlay() && app.debug_active_view_contents().contains("BACKGROUND_DONE")
    });

    super::run_real_tmux_fixture_command(&socket, &["kill-server"]);
    drop(client);
    let _ = std::fs::remove_file(&socket);
}

#[test]
fn tmux_program_status_observes_each_pane_and_respects_accessibility_settings() {
    let (mut app, mut sr, recorder, _clock, mut physical) = make_app(false);
    let mut router = add_ready_connection(&mut app, &mut sr, &mut physical, 1);
    recorder.clear();
    physical.clear();
    assert!(sr.program_status_announcements());
    assert!(sr.auto_read_enabled());
    let contents = app.debug_active_view_contents();

    let blocked = "\\033]7501;state=blocked:app=Fixture:msg=c2FtcGxlIHJlYXNvbg==\\033\\134";
    let working = "\\033]7501;state=working:app=Fixture\\033\\134";
    let done = "\\033]7501;state=done:ignored=x;y:app=Fixture\\033\\134";

    for (pane, report) in [(20, blocked), (21, blocked), (21, blocked)] {
        feed(
            &mut app,
            &mut sr,
            &mut router,
            &pane_output(pane, report),
            &mut physical,
        );
    }
    assert_eq!(recorder.messages(), ["Fixture needs attention"]);
    assert!(physical.is_empty());

    // Reports suppressed by auto-read were still observed. Toggling it
    // off makes a new transition in the current pane eligible.
    sr.set_auto_read_enabled(false);
    let settings = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(settings.path(), "lector.o.tones = false").unwrap();
    lector::lua::setup(settings.path().to_path_buf(), true, &mut sr, |_| Ok(())).unwrap();
    for report in [blocked, done, done, working, done] {
        feed(
            &mut app,
            &mut sr,
            &mut router,
            &pane_output(20, report),
            &mut physical,
        );
    }
    assert_eq!(
        recorder.messages(),
        [
            "Fixture needs attention",
            "Fixture is done",
            "Fixture is done"
        ]
    );
    assert!(physical.is_empty());

    std::fs::write(settings.path(), "lector.o.tones = true").unwrap();
    lector::lua::setup(settings.path().to_path_buf(), true, &mut sr, |_| Ok(())).unwrap();
    sr.set_program_status_announcements(false);
    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(21, done),
        &mut physical,
    );
    sr.set_program_status_announcements(true);
    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(21, done),
        &mut physical,
    );
    assert_eq!(recorder.messages().len(), 3);
    assert!(physical.is_empty());
    sr.set_auto_read_enabled(true);
    app.handle_stdin(&mut sr, b"\x1b[O", &mut Vec::new(), &mut physical)
        .unwrap();
    recorder.clear();
    for pane in [20, 21] {
        for report in [working, blocked, done] {
            feed(
                &mut app,
                &mut sr,
                &mut router,
                &pane_output(pane, report),
                &mut physical,
            );
        }
    }
    app.handle_stdin(&mut sr, b"\x1b[I", &mut Vec::new(), &mut physical)
        .unwrap();
    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(21, done),
        &mut physical,
    );
    assert_eq!(
        recorder.messages(),
        [
            "Fixture needs attention",
            "Fixture is done",
            "Fixture needs attention",
            "Fixture is done"
        ]
    );
    assert_eq!(app.debug_active_view_contents(), contents);
    assert_eq!(app.active_tmux_connection(), Some(1));
}

#[test]
fn pending_tmux_chooser_prioritizes_and_acknowledges_only_visits() {
    let (mut app, mut sr, recorder, _clock, mut physical) = make_app(false);
    let mut router = add_ready_connection(&mut app, &mut sr, &mut physical, 1);
    sr.set_program_status_announcements(false);
    sr.set_tmux_bell_mode(TmuxBellMode::Off);
    let mut stdin = Vec::new();
    let attention =
        "\\033]7501;state=blocked:kind=permission:app=Sample:msg=Q29udGludWU/\\033\\134";
    let done = "\\033]7501;state=done\\033\\134";
    recorder.clear();
    for (pane_id, data) in [
        (22, "\\007"),
        (23, done),
        (21, "\\007"),
        (21, attention),
        (20, done),
    ] {
        feed(
            &mut app,
            &mut sr,
            &mut router,
            &pane_output(pane_id, data),
            &mut physical,
        );
    }
    assert!(recorder.messages().is_empty());

    app.handle_stdin(&mut sr, b"\x1bP", &mut stdin, &mut physical)
        .unwrap();
    assert!(stdin.is_empty());
    assert_eq!(
        recorder.messages(),
        ["window 2, pane 1, Sample, approval. Continue?"]
    );
    let rows = app.debug_active_view_contents();
    let attention_row = rows.find("Sample, approval").unwrap();
    let done_row = rows.find("window 1, pane 2, done").unwrap();
    let bell_row = rows.find("session other, window 1, pane 1, bell").unwrap();
    assert!(attention_row < done_row && done_row < bell_row, "{rows:?}");
    assert_eq!(rows.matches("window 2, pane 1").count(), 1);
    assert!(!rows.contains("window 1, pane 1, done"));
    assert!(!rows.contains("no details"));

    // Keep the done pane selected when another pane changes priority.
    app.handle_stdin(&mut sr, b"\x1b[B", &mut stdin, &mut physical)
        .unwrap();
    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(22, attention),
        &mut physical,
    );
    app.handle_stdin(&mut sr, b"\r", &mut stdin, &mut physical)
        .unwrap();
    let mut wire = Vec::new();
    app.drain_tmux_commands_for(1, &mut wire).unwrap();
    assert_eq!(wire, b"switch-client -E -t '$1:@10.%23'\n");
    assert!(app.has_overlay());
    // The location notification alone is not confirmation of the command.
    // The visible layout can also lag behind the selected pane. Keep the
    // chooser open until the target owns the rendered layout.
    let zoomed = format!("%layout-change @10 {SPLIT} b25f,80x24,0,0,20 *Z\n");
    feed(
        &mut app,
        &mut sr,
        &mut router,
        zoomed.as_bytes(),
        &mut physical,
    );
    feed(
        &mut app,
        &mut sr,
        &mut router,
        b"%window-pane-changed @10 %23\n",
        &mut physical,
    );
    assert!(app.has_overlay());
    feed(
        &mut app,
        &mut sr,
        &mut router,
        &reply(100, &[]),
        &mut physical,
    );
    assert!(app.has_overlay());
    let unzoomed = format!("%layout-change @10 {SPLIT} {SPLIT} *\n");
    feed(
        &mut app,
        &mut sr,
        &mut router,
        unzoomed.as_bytes(),
        &mut physical,
    );
    assert!(app.has_overlay());
    wire.clear();
    app.drain_tmux_commands_for(1, &mut wire).unwrap();
    assert_eq!(wire, lector::tmux_model::INVENTORY_COMMAND.as_bytes());
    let mut groups = inventory(1);
    groups[2][0] =
        b"P\tstatus\t@10\t%20\t1\t0\t0\t0\t40\t24\t0\t0\t0\t1\t0\t0\t0\t0\tactive".to_vec();
    groups[2][1] =
        b"P\tstatus\t@10\t%23\t2\t1\t41\t0\t39\t24\t0\t0\t0\t1\t0\t0\t0\t0\tinactive".to_vec();
    for (index, group) in groups.iter().enumerate() {
        feed(
            &mut app,
            &mut sr,
            &mut router,
            &reply(110 + index, group),
            &mut physical,
        );
    }
    assert!(!app.has_overlay());
    assert!(app.debug_active_view_contents().contains("ready-23"));
    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(23, done),
        &mut physical,
    );
    feed(
        &mut app,
        &mut sr,
        &mut router,
        b"%window-pane-changed @10 %20\n",
        &mut physical,
    );

    app.show_pending_tmux_panes(&mut sr, &mut physical).unwrap();
    app.handle_stdin(&mut sr, b"Continue", &mut stdin, &mut physical)
        .unwrap();
    assert!(
        !app.debug_active_view_contents()
            .contains("window 1, pane 2")
    );
    app.handle_stdin(&mut sr, b"\x1b[27;1u", &mut stdin, &mut physical)
        .unwrap();
    assert!(!app.has_overlay());

    // Ordinary navigation reaches the other pending pane and acknowledges its
    // status and bell together. Subsequent reports repeat the same event.
    feed(
        &mut app,
        &mut sr,
        &mut router,
        b"%session-window-changed $1 @11\n",
        &mut physical,
    );
    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(21, attention),
        &mut physical,
    );
    feed(
        &mut app,
        &mut sr,
        &mut router,
        b"%session-window-changed $1 @10\n",
        &mut physical,
    );
    app.show_pending_tmux_panes(&mut sr, &mut physical).unwrap();
    assert!(
        !app.debug_active_view_contents()
            .contains("window 2, pane 1")
    );
    app.handle_stdin(&mut sr, b"\x1b[27;1u", &mut stdin, &mut physical)
        .unwrap();

    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(21, "\\007"),
        &mut physical,
    );
    app.show_pending_tmux_panes(&mut sr, &mut physical).unwrap();
    assert!(
        app.debug_active_view_contents()
            .contains("window 2, pane 1, bell")
    );
}

#[test]
fn pending_pane_acknowledgment_waits_for_presentation() {
    let (mut app, mut sr, recorder, _clock, mut physical) = make_app(true);
    let mut router = add_ready_connection(&mut app, &mut sr, &mut physical, 1);
    sr.set_program_status_announcements(false);
    app.drain_scheduled_output(&mut physical, true).unwrap();
    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(21, "\\033]7501;state=done\\033\\134"),
        &mut physical,
    );
    // The two location changes coalesce. The briefly selected pane was
    // never presented, so the report is still available.
    for window in [11, 10] {
        let notification = format!("%session-window-changed $1 @{window}\n");
        feed(
            &mut app,
            &mut sr,
            &mut router,
            notification.as_bytes(),
            &mut physical,
        );
    }
    app.drain_scheduled_output(&mut physical, true).unwrap();
    app.show_pending_tmux_panes(&mut sr, &mut physical).unwrap();
    assert!(app.has_overlay());
    app.handle_stdin(&mut sr, b"\x1b[27;1u", &mut Vec::new(), &mut physical)
        .unwrap();
    assert!(!app.has_overlay());

    for window in [11, 10] {
        let notification = format!("%session-window-changed $1 @{window}\n");
        feed(
            &mut app,
            &mut sr,
            &mut router,
            notification.as_bytes(),
            &mut physical,
        );
        app.drain_scheduled_output(&mut physical, true).unwrap();
    }
    recorder.clear();
    app.show_pending_tmux_panes(&mut sr, &mut physical).unwrap();
    assert!(!app.has_overlay());
    assert_eq!(recorder.messages(), ["No pending panes"]);
}

#[test]
fn empty_pending_tmux_chooser_preserves_an_existing_overlay_and_edit_position() {
    let (mut app, mut sr, recorder, _clock, mut physical) = make_app(false);
    let mut router = add_ready_connection(&mut app, &mut sr, &mut physical, 1);
    sr.set_program_status_announcements(false);
    for (pane_id, data) in [(20, "done"), (21, "working"), (23, "idle")] {
        feed(
            &mut app,
            &mut sr,
            &mut router,
            &pane_output(pane_id, &format!("\\033]7501;state={data}\\033\\134")),
            &mut physical,
        );
    }
    assert!(
        app.show_tmux_command_prompt(&mut sr, &mut physical)
            .unwrap()
    );
    let mut stdin = Vec::new();
    app.handle_stdin(&mut sr, b"abcd\x1b[D", &mut stdin, &mut physical)
        .unwrap();
    let before = app.debug_active_view_contents();
    let cursor = app
        .composed_scene()
        .unwrap()
        .overlays
        .last()
        .unwrap()
        .surface
        .snapshot
        .cursor;
    recorder.clear();
    physical.clear();
    app.handle_stdin(&mut sr, b"\x1bP", &mut stdin, &mut physical)
        .unwrap();
    assert_eq!(recorder.messages(), ["No pending panes"]);
    assert!(physical.is_empty());
    assert!(stdin.is_empty());
    assert!(app.has_overlay());
    assert_eq!(app.debug_active_view_contents(), before);
    assert_eq!(
        app.composed_scene()
            .unwrap()
            .overlays
            .last()
            .unwrap()
            .surface
            .snapshot
            .cursor,
        cursor
    );
    app.handle_stdin(&mut sr, b"x", &mut stdin, &mut physical)
        .unwrap();
    assert!(app.debug_active_view_contents().contains("abcxd"));
}

#[test]
fn pending_status_bootstrap_keeps_order_and_corroborated_live_details_silently() {
    let report = "\\033]7501;state=blocked:kind=permission:app=Sample:msg=Q29udGludWU/\\033\\134";
    let before_split = "\\033]7501;state=blocked:kind=ques";
    let after_split = "tion:app=Sample\\033\\134";
    let clear = "\\033]7501;state=clear\\033\\134";
    for (before, sample, after, expected) in [
        (
            report,
            Some(("blocked", "Continue?")),
            "",
            "window 2, pane 1, Sample, approval. Continue?",
        ),
        (
            report,
            None,
            "",
            "window 2, pane 1, Sample, approval. Continue?",
        ),
        (
            report,
            Some(("done", "")),
            "",
            "window 2, pane 1, Sample, done",
        ),
        (
            before_split,
            Some(("working", "")),
            after_split,
            "window 2, pane 1, Sample, question",
        ),
        (report, None, clear, "No pending panes"),
    ] {
        let (mut app, mut sr, recorder, _clock, mut physical) = make_app(false);
        // Send enough live bytes to place this hidden pane's pre-sample
        // report in App's deferred queue rather than the immediate budget.
        let buffered = format!("{}{before}", ".".repeat(4200));
        let reports_after_sample = (!after.is_empty())
            .then_some((21, after))
            .into_iter()
            .collect::<Vec<_>>();
        let _router = add_connection_with_bootstrap_sample(
            &mut app,
            &mut sr,
            &mut physical,
            1,
            &[pane_output(21, &buffered)],
            &reports_after_sample,
            sample.map(|(state, message)| BootstrapSample {
                pane: 21,
                state,
                name: "Sample",
                message,
                pending_escape: &[],
            }),
        );
        assert!(
            !recorder
                .messages()
                .iter()
                .any(|message| message.contains("Sample"))
        );
        recorder.clear();
        app.show_pending_tmux_panes(&mut sr, &mut physical).unwrap();
        assert_eq!(recorder.messages(), [expected]);
        assert_eq!(app.has_overlay(), expected != "No pending panes");
    }
}

#[test]
fn pending_bootstrap_replays_older_output_first_when_a_pane_becomes_visible() {
    for (older, newer, expected) in [
        (
            "\\033]7501;state=blocked:kind=ques",
            "tion:app=Sample\\033\\134",
            "window 2, pane 1, Sample, question",
        ),
        (
            "\\033]7501;state=done:app=Sample\\033\\134",
            "\\033]7501;state=clear\\033\\134",
            "No pending panes",
        ),
    ] {
        let (mut app, mut sr, recorder, _clock, mut physical) = make_app(false);
        let deferred = format!("{}{older}", ".".repeat(4200));
        let records = [
            pane_output(21, &deferred),
            b"%session-window-changed $1 @11\n".to_vec(),
            pane_output(21, newer),
            b"%session-window-changed $1 @10\n".to_vec(),
        ];
        let _router = add_connection_with_bootstrap_sample(
            &mut app,
            &mut sr,
            &mut physical,
            1,
            &records,
            &[],
            None,
        );
        assert!(
            !recorder
                .messages()
                .iter()
                .any(|message| message.contains("Sample"))
        );
        recorder.clear();
        app.show_pending_tmux_panes(&mut sr, &mut physical).unwrap();
        assert_eq!(recorder.messages(), [expected]);
        assert_eq!(app.has_overlay(), expected != "No pending panes");
    }
}

#[test]
fn tmux_bootstrap_uses_the_capture_time_status_after_buffered_transitions() {
    let (mut app, mut sr, recorder, _clock, mut physical) = make_app(false);
    let working = "\\033]7501;state=working:app=Fixture\\033\\134";
    let blocked = "\\033]7501;state=blocked:app=Fixture\\033\\134";
    let mut router = add_connection_with_bootstrap_sample(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        &[pane_output(21, working), pane_output(21, blocked)],
        &[],
        Some(BootstrapSample {
            pane: 21,
            state: "blocked",
            name: "Fixture",
            message: "",
            pending_escape: &[],
        }),
    );
    assert!(
        !recorder
            .messages()
            .iter()
            .any(|text| text == "Fixture needs attention")
    );
    recorder.clear();
    for report in [blocked, working, blocked] {
        feed(
            &mut app,
            &mut sr,
            &mut router,
            &pane_output(21, report),
            &mut physical,
        );
    }
    assert_eq!(recorder.messages(), ["Fixture needs attention"]);
}

#[test]
fn tmux_bootstrap_resumes_a_program_report_split_at_capture() {
    let (mut app, mut sr, recorder, _clock, mut physical) = make_app(false);
    let mut router = add_connection_with_bootstrap_sample(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        &[pane_output(21, "\\033]7501;app=Fixture:state=bloc")],
        &[],
        Some(BootstrapSample {
            pane: 21,
            state: "working",
            name: "Fixture",
            message: "",
            pending_escape: b"\x1b]7501;app=Fixture:state=bloc",
        }),
    );
    recorder.clear();
    feed(
        &mut app,
        &mut sr,
        &mut router,
        &pane_output(21, "ked\\033\\134"),
        &mut physical,
    );
    assert_eq!(recorder.messages(), ["Fixture needs attention"]);
    assert!(!app.debug_tmux_pane_contents(1, 21).unwrap().contains("ked"));
}

struct RealTmuxClient {
    app: App,
    sr: ScreenReader,
    recorder: Recorder,
    physical: Vec<u8>,
    writer: Box<dyn Write + Send>,
    receiver: mpsc::Receiver<Vec<u8>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    reader: Option<thread::JoinHandle<()>>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl RealTmuxClient {
    fn start(socket: &std::path::Path, args: &[&str]) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new("tmux");
        command.args(["-S", socket.to_str().unwrap()]);
        command.args(args);
        command.env("TERM", "xterm-256color");
        command.env_remove("TMUX");
        command.env_remove("TMUX_PANE");
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let (sender, receiver) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut buffer = [0; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) if sender.send(buffer[..count].to_vec()).is_err() => break,
                    Ok(_) => {}
                    Err(error) if error.raw_os_error() == Some(5) => break,
                    Err(error) => panic!("read control PTY: {error}"),
                }
            }
        });
        let recorder = Recorder::default();
        let stack = views::ViewStack::new(Box::new(views::PtyView::new(24, 80)));
        Self {
            app: App::new(stack).unwrap(),
            sr: ScreenReader::new(speech::Speech::new(Box::new(recorder.clone()))),
            recorder,
            physical: Vec::new(),
            writer,
            receiver,
            child,
            reader: Some(reader),
            _master: pair.master,
        }
    }

    fn attach(socket: &std::path::Path, session: &str) -> Self {
        Self::start(socket, &["-CC", "attach", "-t", session])
    }

    fn drive(&mut self, case: &str, done: impl FnMut(&mut App) -> bool) {
        drive_real_tmux(
            case,
            &mut self.app,
            &mut self.sr,
            &self.receiver,
            self.writer.as_mut(),
            &mut self.physical,
            done,
        );
    }

    fn wait_for(&mut self, pane: u64, text: &str) {
        self.drive(text, |app| {
            app.debug_active_view_contents().contains("STATUS READY")
                && app
                    .debug_tmux_pane_contents(1, pane)
                    .is_some_and(|contents| contents.contains(text))
        });
    }
}

impl Drop for RealTmuxClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.reader.take().unwrap().join().unwrap();
    }
}

#[test]
#[ignore = "run through scripts/test-real-tmux-docker with the pinned OSC 7501 preview"]
fn real_tmux_preview_announces_background_program_status_once_across_reattach() {
    let _serial = super::serialize_real_tmux_test();
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("tmux.sock");
    let session = "program-status";
    let fixture = env!("CARGO_BIN_EXE_lector-harness");
    super::run_real_tmux_fixture_command(
        &socket,
        &[
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            session,
            "-e",
            "LECTOR_TEST_PTY_MODE=program-status",
            fixture,
            "pty",
        ],
    );
    super::run_real_tmux_fixture_command(
        &socket,
        &[
            "bind-key",
            "-T",
            "prefix",
            "A",
            "choose-tree",
            "-Zw",
            "-f",
            "#{==:#{pane_program_state},blocked}",
            "-F",
            "#{pane_program_name} #{pane_program_state} #{pane_program_message}",
        ],
    );
    let background = super::run_real_tmux_fixture_command(
        &socket,
        &[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            session,
            "-e",
            "LECTOR_TEST_PTY_MODE=program-status",
            fixture,
            "pty",
        ],
    );
    let target = background.trim();
    let pane = target.strip_prefix('%').unwrap().parse().unwrap();
    {
        let mut client = RealTmuxClient::attach(&socket, session);
        client.wait_for(pane, "STATUS READY");
        client.recorder.clear();
        super::run_real_tmux_fixture_command(&socket, &["send-keys", "-t", target, "p"]);
        client.wait_for(pane, "PROBE ACCEPTED");
        assert_eq!(client.recorder.messages(), ["Fixture needs attention"]);
        assert!(
            !client
                .app
                .debug_active_view_contents()
                .contains("PROBE ACCEPTED")
        );

        assert_eq!(
            super::run_real_tmux_fixture_command(
                &socket,
                &[
                    "display-message",
                    "-p",
                    "-t",
                    target,
                    "#{pane_program_state} #{pane_program_name}",
                ],
            )
            .trim(),
            "blocked Fixture"
        );
        client
            .app
            .handle_stdin(
                &mut client.sr,
                b"\x02A",
                &mut Vec::new(),
                &mut client.physical,
            )
            .unwrap();
        client.drive("filtered program status chooser", |app| {
            app.debug_active_view_contents()
                .contains("Fixture blocked sample reason")
        });
        assert!(client.app.has_overlay());
        client
            .app
            .handle_stdin(
                &mut client.sr,
                b"\x1b[27;1u",
                &mut Vec::new(),
                &mut client.physical,
            )
            .unwrap();
        assert!(!client.app.has_overlay());
        assert!(
            client
                .app
                .debug_active_view_contents()
                .contains("STATUS READY")
        );

        super::run_real_tmux_fixture_command(&socket, &["detach-client", "-s", session]);
    }
    {
        let mut client = RealTmuxClient::attach(&socket, session);
        client.wait_for(pane, "PROBE ACCEPTED");
        super::run_real_tmux_fixture_command(&socket, &["send-keys", "-t", target, "r"]);
        client.wait_for(pane, "STATUS REPEATED");
        assert!(
            !client
                .recorder
                .messages()
                .iter()
                .any(|message| { message.contains("needs attention") })
        );
        assert!(
            !client
                .app
                .debug_active_view_contents()
                .contains("STATUS REPEATED")
        );
        client.recorder.clear();
        client
            .app
            .show_pending_tmux_panes(&mut client.sr, &mut client.physical)
            .unwrap();
        assert_eq!(
            client.recorder.messages(),
            ["window 1, pane 0, Fixture, needs attention. sample reason"]
        );
        assert!(client.app.has_overlay());
        client
            .app
            .handle_stdin(&mut client.sr, b"\r", &mut Vec::new(), &mut client.physical)
            .unwrap();
        client.drive("visit the recovered status pane", |app| {
            !app.has_overlay() && app.debug_active_view_contents().contains("STATUS REPEATED")
        });
        super::run_real_tmux_fixture_command(
            &socket,
            &["select-window", "-t", &format!("{session}:0")],
        );
        client.drive("return to the original window", |app| {
            !app.debug_active_view_contents().contains("STATUS REPEATED")
        });
        client.recorder.clear();
        client
            .app
            .show_pending_tmux_panes(&mut client.sr, &mut client.physical)
            .unwrap();
        assert_eq!(client.recorder.messages(), ["No pending panes"]);
        assert!(!client.app.has_overlay());
        super::run_real_tmux_fixture_command(&socket, &["kill-server"]);
    }
}

#[test]
#[ignore = "run through scripts/test-real-tmux-docker with the pinned OSC 7501 preview"]
fn real_tmux_preview_visits_recovered_status_in_another_session() {
    let _serial = super::serialize_real_tmux_test();
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("tmux.sock");
    let fixture = env!("CARGO_BIN_EXE_lector-harness");
    for session in ["first", "second"] {
        super::run_real_tmux_fixture_command(
            &socket,
            &[
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                session,
                "-e",
                "LECTOR_TEST_PTY_MODE=program-status",
                fixture,
                "pty",
            ],
        );
    }
    let mut prep = RealTmuxClient::attach(&socket, "second");
    let target = super::run_real_tmux_fixture_command(
        &socket,
        &["display-message", "-t", "second:", "-p", "#{pane_id}"],
    );
    let target = target.trim();
    let pane = target.strip_prefix('%').unwrap().parse().unwrap();
    prep.wait_for(pane, "STATUS READY");
    super::run_real_tmux_fixture_command(&socket, &["send-keys", "-t", target, "p"]);
    prep.wait_for(pane, "PROBE ACCEPTED");
    super::run_real_tmux_fixture_command(&socket, &["detach-client", "-s", "second"]);
    drop(prep);

    let mut client = RealTmuxClient::attach(&socket, "first");
    client.wait_for(pane, "PROBE ACCEPTED");
    client.recorder.clear();
    client
        .app
        .show_pending_tmux_panes(&mut client.sr, &mut client.physical)
        .unwrap();
    assert_eq!(
        client.recorder.messages(),
        ["session second, window 0, pane 0, Fixture, needs attention. sample reason"]
    );
    client
        .app
        .handle_stdin(&mut client.sr, b"\r", &mut Vec::new(), &mut client.physical)
        .unwrap();
    client.drive("visit a pending pane in another session", |app| {
        !app.has_overlay() && app.debug_active_view_contents().contains("PROBE ACCEPTED")
    });
    super::run_real_tmux_fixture_command(&socket, &["kill-server"]);
}
