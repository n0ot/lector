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
            b"P\t@10\t%20\t1\t1\t0\t0\t40\t24\t0\t0\t0\t1\t0\t0\t0\t0\tactive".to_vec(),
            b"P\t@10\t%23\t2\t0\t41\t0\t39\t24\t0\t0\t0\t1\t0\t0\t0\t0\tinactive".to_vec(),
            b"P\t@11\t%21\t1\t1\t0\t0\t80\t24\t0\t0\t0\t1\t0\t0\t0\t0\thidden".to_vec(),
            b"P\t@12\t%22\t1\t1\t0\t0\t80\t24\t0\t0\t0\t1\t0\t0\t0\t0\tunattached".to_vec(),
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
    for (index, pane_id) in [20, 21, 22, 23].into_iter().enumerate() {
        feed(
            app,
            sr,
            &mut router,
            &reply(30 + index, &[format!("ready-{pane_id}").into_bytes()]),
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
    let result = super::drive_real_tmux_phase(|remaining| {
        let chunk = receiver.recv_timeout(remaining)?;
        app.handle_pty(sr, &chunk, physical).unwrap();
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
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new("tmux");
    command.args([
        "-S",
        socket.to_str().unwrap(),
        "-f",
        "/dev/null",
        "-CC",
        "new-session",
        "-s",
        &session,
        "/bin/sh -c 'printf ACTIVE_READY; exec /bin/sh'",
    ]);
    command.env("TERM", "xterm-256color");
    command.env_remove("TMUX");
    command.env_remove("TMUX_PANE");
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let (sender, receiver) = mpsc::channel();
    let read_thread = thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if sender.send(buffer[..count].to_vec()).is_err() {
                        break;
                    }
                }
                Err(error) if error.raw_os_error() == Some(5) => break,
                Err(error) => panic!("read real tmux bell PTY: {error}"),
            }
        }
    });

    let recorder = Recorder::default();
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(24, 80)));
    let mut app = App::new(stack).unwrap();
    let mut sr = ScreenReader::new(speech::Speech::new(Box::new(recorder.clone())));
    let mut physical = Vec::new();
    drive_real_tmux(
        "initial activity pane bootstrap",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| app.debug_active_view_contents().contains("ACTIVE_READY"),
    );
    let topology = app.debug_tmux_topology(1).unwrap();
    let background_pane = pane_id_at_index(&topology, 0).expect("background pane id");

    writer
        .write_all(
            format!(
                "new-window -t {session} -n foreground \"/bin/sh -c 'printf FOREGROUND_READY; exec /bin/sh'\"\n"
            )
            .as_bytes(),
        )
        .unwrap();
    writer.flush().unwrap();
    drive_real_tmux(
        "foreground window bootstrap",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| {
            app.debug_active_view_contents()
                .contains("FOREGROUND_READY")
        },
    );

    sr.set_tmux_bell_mode(TmuxBellMode::Spoken);
    recorder.clear();
    writer
        .write_all(
            format!("send-keys -t %{background_pane} \"printf BACKGROUND_DONE\" Enter\n")
                .as_bytes(),
        )
        .unwrap();
    writer.flush().unwrap();
    drive_real_tmux(
        "background window activity",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| {
            app.debug_tmux_pane_contents(1, background_pane)
                .is_some_and(|contents| contents.contains("BACKGROUND_DONE"))
        },
    );
    assert!(
        recorder.messages().is_empty(),
        "ordinary background activity was announced: {:?}",
        recorder.messages()
    );
    assert!(app.last_tmux_bell_source().is_none());

    writer
        .write_all(
            format!("send-keys -t %{background_pane} \"printf '\\\\007'\" Enter\n").as_bytes(),
        )
        .unwrap();
    writer.flush().unwrap();
    drive_real_tmux(
        "background pane bell",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |_| !recorder.messages().is_empty(),
    );
    let source = app.last_tmux_bell_source().expect("real pane bell source");
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

    writer.write_all(b"kill-server\n").unwrap();
    writer.flush().unwrap();
    let _ = child.wait().unwrap();
    read_thread.join().unwrap();
    let _ = std::fs::remove_file(&socket);
}
