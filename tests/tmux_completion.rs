use lector::{
    app::{
        App, Clock, TMUX_FLOW_CONTROL_COMMAND, TMUX_FLOW_CONTROL_VERIFY_COMMAND, TmuxFlowStatus,
        TmuxResyncLimitation,
    },
    output_scheduler::OutputSchedulerConfig,
    presentation::{
        FullSceneVtRenderer, GridRect, IncrementalVtRenderer, OutputTransaction, PresentedScene,
        RenderBatch, RenderCapabilities, RenderOracle, RenderStrategy, RendererBackend,
        SceneDamage,
    },
    screen_reader::ScreenReader,
    speech,
    terminal::TerminalGeometry,
    terminal_protocol::PhysicalTerminalProfile,
    tmux_control::CommandStatus,
    tmux_model::{INVENTORY_COMMAND, INVENTORY_REPLY_COUNT, PaneId, TmuxTopology},
    tmux_panes::TmuxPaneSet,
    views,
};
use std::{
    cell::{Cell, RefCell},
    io::{self, Write},
    rc::Rc,
    time::{Duration, Instant},
};

#[cfg(unix)]
fn process_cpu_time() -> Duration {
    let mut usage = std::mem::MaybeUninit::<nix::libc::rusage>::zeroed();
    // SAFETY: `usage` is writable storage for one `rusage`; a successful call
    // initializes it before `assume_init`.
    if unsafe { nix::libc::getrusage(nix::libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return Duration::ZERO;
    }
    // SAFETY: the successful `getrusage` call initialized the value.
    let usage = unsafe { usage.assume_init() };
    let seconds = usage.ru_utime.tv_sec.saturating_add(usage.ru_stime.tv_sec);
    let micros = usage
        .ru_utime
        .tv_usec
        .saturating_add(usage.ru_stime.tv_usec);
    Duration::from_secs(seconds.try_into().unwrap_or(0))
        .saturating_add(Duration::from_micros(micros.try_into().unwrap_or(0)))
}

#[cfg(not(unix))]
fn process_cpu_time() -> Duration {
    Duration::ZERO
}

const SPLIT: &str = "beef,10x4,0,0{5x4,0,0,20,4x4,6,0,21}";
const HIDDEN: &str = "beef,10x4,0,0,22";
const RED_IMAGE: &[u8] = b"\x1b_Ga=T,f=32,s=1,v=1,i=7,p=9,c=3,r=2,q=2;/wAA/w==\x1b\\";
const GREEN_IMAGE: &[u8] = b"\x1b_Ga=T,f=32,s=1,v=1,i=7,p=9,c=3,r=2,q=2;AP8A/w==\x1b\\";

fn inventory() -> Vec<Vec<Vec<u8>>> {
    vec![
        vec![b"S\t$1\twork".to_vec()],
        vec![
            format!("W\t$1\t@10\t1\t1\t{SPLIT}\t{SPLIT}\t*\tactive").into_bytes(),
            format!("W\t$1\t@11\t2\t0\t{HIDDEN}\t{HIDDEN}\t-\thidden").into_bytes(),
        ],
        vec![
            b"P\t@10\t%20\t0\t1\t0\t0\t5\t4\t0\t0\t0\t1\t0\t0\t0\t0\tleft".to_vec(),
            b"P\t@10\t%21\t1\t0\t6\t0\t4\t4\t0\t0\t0\t1\t0\t0\t0\t0\tright".to_vec(),
            b"P\t@11\t%22\t0\t1\t0\t0\t10\t4\t0\t0\t0\t1\t0\t0\t0\t0\toffscreen".to_vec(),
        ],
        vec![b"A\t$1".to_vec()],
        vec![b"O\tbase-index\t1".to_vec()],
        vec![b"O\tpane-base-index\t0".to_vec()],
        vec![b"C\tclient_name\t/dev/ttys-completion".to_vec()],
        vec![b"O\tprefix\tC-a".to_vec()],
        vec![b"O\tprefix2\tNone".to_vec()],
        vec![b"O\tkey-table\troot".to_vec()],
        vec![b"O\trepeat-time\t500".to_vec()],
        vec![b"B\td\t0\tdetach-client".to_vec()],
    ]
}

fn topology() -> TmuxTopology {
    let mut topology = TmuxTopology::new(1);
    topology
        .replace_inventory(&inventory().into_iter().flatten().collect::<Vec<_>>())
        .unwrap();
    topology
}

fn stress_topology(windows: usize) -> TmuxTopology {
    let mut records = vec![b"S\t$1\tstress".to_vec()];
    for window in 0..windows {
        let window_id = 10 + window as u64;
        let left = 100 + window as u64 * 2;
        let right = left + 1;
        let layout = format!("beef,40x8,0,0{{20x8,0,0,{left},19x8,21,0,{right}}}");
        records.push(
            format!(
                "W\t$1\t@{window_id}\t{}\t{}\t{layout}\t{layout}\t{}\twindow-{window}",
                window + 1,
                usize::from(window == 0),
                if window == 0 { "*" } else { "-" },
            )
            .into_bytes(),
        );
    }
    for window in 0..windows {
        let window_id = 10 + window as u64;
        for pane_index in 0..2 {
            let pane_id = 100 + window as u64 * 2 + pane_index;
            let left = if pane_index == 0 { 0 } else { 21 };
            let active = usize::from(pane_index == 0);
            records.push(
                format!(
                    "P\t@{window_id}\t%{pane_id}\t{pane_index}\t{active}\t{left}\t0\t{}\t8\t0\t0\t0\t1\t0\t0\t0\t0\tpane-{pane_id}",
                    if pane_index == 0 { 20 } else { 19 },
                )
                .into_bytes(),
            );
        }
    }
    records.extend([
        b"A\t$1".to_vec(),
        b"O\tbase-index\t1".to_vec(),
        b"O\tpane-base-index\t0".to_vec(),
        b"C\tclient_name\t/dev/ttys-stress".to_vec(),
        b"O\tprefix\tC-a".to_vec(),
        b"O\tprefix2\tNone".to_vec(),
        b"O\tkey-table\troot".to_vec(),
        b"O\trepeat-time\t500".to_vec(),
        b"B\td\t0\tdetach-client".to_vec(),
    ]);
    let mut topology = TmuxTopology::new(99);
    topology.replace_inventory(&records).unwrap();
    topology
}

fn pane_set() -> (TmuxTopology, TmuxPaneSet) {
    let topology = topology();
    let mut panes = TmuxPaneSet::new(1);
    for request in panes.reconcile(&topology).unwrap() {
        panes
            .apply_bootstrap(
                request.pane_id,
                CommandStatus::Success,
                &[format!("pane-{}", request.pane_id.0).into_bytes()],
                0,
            )
            .unwrap();
    }
    (topology, panes)
}

fn within(rect: GridRect, clip: GridRect) -> bool {
    rect.origin.row >= clip.origin.row
        && rect.origin.col >= clip.origin.col
        && rect.origin.row + i32::from(rect.rows) <= clip.origin.row + i32::from(clip.rows)
        && rect.origin.col + i32::from(rect.cols) <= clip.origin.col + i32::from(clip.cols)
}

#[test]
fn tmux_images_are_namespaced_clipped_and_persist_only_with_their_window() {
    let (mut topology, mut panes) = pane_set();
    panes.process_output(PaneId(20), RED_IMAGE).unwrap();
    panes.process_output(PaneId(21), GREEN_IMAGE).unwrap();
    panes.process_output(PaneId(22), RED_IMAGE).unwrap();

    let active = panes
        .compose(
            &topology,
            lector::terminal::TerminalGeometry::from_cells(4, 10),
        )
        .unwrap();
    assert_eq!(active.image_uploads.len(), 2);
    assert_eq!(active.images.len(), 2);
    let logical = PresentedScene::compose(&active).unwrap();
    assert_eq!(logical.images().len(), 2);
    assert_ne!(logical.images()[0].image_id, logical.images()[1].image_id);
    assert_ne!(
        logical.images()[0].placement_id,
        logical.images()[1].placement_id
    );
    for image in &active.images {
        let clip = if image.owner == panes.surface_id(PaneId(20)).unwrap() {
            GridRect::new(lector::presentation::GridPoint::new(0, 0), 4, 5)
        } else {
            GridRect::new(lector::presentation::GridPoint::new(0, 6), 4, 4)
        };
        assert!(
            within(image.image.grid_rect, clip),
            "image={image:?}, clip={clip:?}"
        );
    }

    topology
        .apply_notification(b"session-window-changed", b"$1 @11")
        .unwrap();
    let hidden_window = panes
        .compose(
            &topology,
            lector::terminal::TerminalGeometry::from_cells(4, 10),
        )
        .unwrap();
    assert_eq!(hidden_window.images.len(), 1);
    assert_eq!(
        hidden_window.images[0].owner,
        panes.surface_id(PaneId(22)).unwrap()
    );

    topology
        .apply_notification(b"session-window-changed", b"$1 @10")
        .unwrap();
    topology
        .apply_notification(
            b"layout-change",
            b"@10 cafe,8x4,0,0{4x4,0,0,20,3x4,5,0,21} cafe,8x4,0,0{4x4,0,0,20,3x4,5,0,21} *",
        )
        .unwrap();
    panes.reconcile(&topology).unwrap();
    let resized = panes
        .compose(
            &topology,
            lector::terminal::TerminalGeometry::from_cells(4, 8),
        )
        .unwrap();
    assert_eq!(resized.images.len(), 2);
    for image in &resized.images {
        assert!(
            within(
                image.image.grid_rect,
                GridRect::new(lector::presentation::GridPoint::new(0, 0), 4, 8)
            ),
            "resized image escaped the scene: {image:?}"
        );
    }
}

#[derive(Clone, Default)]
struct Recorder(Rc<RefCell<Vec<String>>>);

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

fn reply(serial: usize, lines: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = format!("%begin {serial} {serial} 0\n").into_bytes();
    for line in lines {
        bytes.extend_from_slice(line);
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(format!("%end {serial} {serial} 0\n").as_bytes());
    bytes
}

fn error_reply(serial: usize, lines: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = format!("%begin {serial} {serial} 0\n").into_bytes();
    for line in lines {
        bytes.extend_from_slice(line);
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(format!("%error {serial} {serial} 0\n").as_bytes());
    bytes
}

#[derive(Clone, Default)]
struct TestClock(Rc<Cell<u128>>);

impl TestClock {
    fn advance(&self, milliseconds: u128) {
        self.0.set(self.0.get().saturating_add(milliseconds));
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> u128 {
        self.0.get()
    }
}

fn start_app_with_bootstraps_in_flight(
    mut app: App,
    recorder: Recorder,
    scheduled: bool,
) -> (App, ScreenReader, Recorder, Vec<u8>) {
    if scheduled {
        app.enable_output_scheduler(OutputSchedulerConfig {
            maximum_pending_bytes: 8 * 1024,
            ..OutputSchedulerConfig::default()
        });
    }
    let mut sr = ScreenReader::new(speech::Speech::new(Box::new(recorder.clone())));
    let mut physical = Vec::new();
    app.handle_pty(
        &mut sr,
        b"\x1bP1000p%begin 1 1 0\n%end 1 1 0\n",
        &mut physical,
    )
    .unwrap();
    let mut commands = Vec::new();
    app.handle_tick(&mut sr, &mut commands, &mut physical)
        .unwrap();
    let expected = [
        b"refresh-client -f pause-after=1\n".as_slice(),
        TMUX_FLOW_CONTROL_VERIFY_COMMAND,
        INVENTORY_COMMAND.as_bytes(),
    ]
    .concat();
    assert_eq!(commands, expected);
    app.handle_pty(&mut sr, &reply(2, &[]), &mut physical)
        .unwrap();
    app.handle_pty(
        &mut sr,
        &reply(3, &[b"attached,control-mode,pause-after=1".to_vec()]),
        &mut physical,
    )
    .unwrap();
    let groups = inventory();
    assert_eq!(groups.len(), INVENTORY_REPLY_COUNT);
    for (index, group) in groups.iter().enumerate() {
        app.handle_pty(&mut sr, &reply(index + 4, group), &mut physical)
            .unwrap();
    }
    commands.clear();
    app.handle_tick(&mut sr, &mut commands, &mut physical)
        .unwrap();
    for pane_id in [20, 21, 22] {
        assert!(
            String::from_utf8_lossy(&commands).contains(&format!("-t %{pane_id}\n")),
            "captures={:?}",
            String::from_utf8_lossy(&commands)
        );
    }
    (app, sr, recorder, physical)
}

fn finish_ready_app(
    app: App,
    recorder: Recorder,
    scheduled: bool,
) -> (App, ScreenReader, Recorder, Vec<u8>) {
    let (mut app, mut sr, recorder, mut physical) =
        start_app_with_bootstraps_in_flight(app, recorder, scheduled);
    for (index, pane_id) in [20, 21, 22].into_iter().enumerate() {
        app.handle_pty(
            &mut sr,
            &reply(30 + index, &[format!("ready-{pane_id}").into_bytes()]),
            &mut physical,
        )
        .unwrap();
    }
    (app, sr, recorder, physical)
}

fn ready_app(scheduled: bool) -> (App, ScreenReader, Recorder, Vec<u8>) {
    let recorder = Recorder::default();
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(4, 10)));
    let app = App::new(stack).unwrap();
    finish_ready_app(app, recorder, scheduled)
}

fn ready_app_with_clock(scheduled: bool) -> (App, ScreenReader, Recorder, TestClock, Vec<u8>) {
    let recorder = Recorder::default();
    let clock = TestClock::default();
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(4, 10)));
    let app = App::new_with_clock(stack, Box::new(clock.clone())).unwrap();
    let (app, sr, recorder, physical) = finish_ready_app(app, recorder, scheduled);
    (app, sr, recorder, clock, physical)
}

fn drain(app: &mut App, sr: &mut ScreenReader, physical: &mut Vec<u8>) -> Vec<u8> {
    let mut commands = Vec::new();
    app.handle_tick(sr, &mut commands, physical).unwrap();
    commands
}

fn pane_metadata_line(pane_id: u64) -> Vec<u8> {
    pane_metadata_line_with(pane_id, false, 0, 2, 1)
}

fn pane_metadata_line_with(
    pane_id: u64,
    alternate_on: bool,
    pane_in_mode: u32,
    cursor_x: u32,
    cursor_y: u32,
) -> Vec<u8> {
    let (left, width) = match pane_id {
        20 => (0, 5),
        21 => (6, 4),
        22 => (0, 10),
        _ => (0, 10),
    };
    format!(
        "R\t%{pane_id}\t{left}\t0\t{width}\t4\t0\t{cursor_x}\t{cursor_y}\t1\t0\t{}\t{pane_in_mode}\t2",
        u8::from(alternate_on)
    )
    .into_bytes()
}

fn capture_pipeline(pane_id: u64) -> Vec<u8> {
    let pane_id = lector::tmux_model::PaneId(pane_id);
    capture_pipeline_from_metadata(&pane_metadata_line(pane_id.0), pane_id)
}

fn final_capture_pipeline(pane_id: u64) -> Vec<u8> {
    let pane_id_value = lector::tmux_model::PaneId(pane_id);
    let commands = [
        lector::tmux_input::continue_pane_command(pane_id_value),
        capture_pipeline(pane_id),
    ];
    let mut sequence = commands.concat();
    sequence.pop();
    sequence = sequence
        .split(|byte| *byte == b'\n')
        .collect::<Vec<_>>()
        .join(b" ; ".as_slice());
    sequence.push(b'\n');
    sequence
}

fn capture_pipeline_from_metadata(line: &[u8], pane_id: lector::tmux_model::PaneId) -> Vec<u8> {
    let metadata = lector::tmux_model::parse_pane_capture_metadata(line, pane_id).unwrap();
    let mut commands = lector::tmux_panes::capture_command_for_metadata(&metadata);
    commands.extend_from_slice(&lector::tmux_panes::pending_escape_capture_command(pane_id));
    commands.extend_from_slice(&lector::tmux_model::pane_capture_metadata_command(pane_id));
    commands
}

fn start_capture_after_probe(
    app: &mut App,
    sr: &mut ScreenReader,
    physical: &mut Vec<u8>,
    serial: usize,
    pane_id: u64,
) {
    let pane_id_value = lector::tmux_model::PaneId(pane_id);
    assert_eq!(
        drain(app, sr, physical),
        lector::tmux_model::pane_capture_metadata_command(pane_id_value)
    );
    app.handle_pty(sr, &reply(serial, &[pane_metadata_line(pane_id)]), physical)
        .unwrap();
    assert_eq!(drain(app, sr, physical), capture_pipeline(pane_id));
}

fn finish_capture(
    app: &mut App,
    sr: &mut ScreenReader,
    physical: &mut Vec<u8>,
    serial: usize,
    pane_id: u64,
    output: &[Vec<u8>],
) {
    app.handle_pty(sr, &reply(serial, output), physical)
        .unwrap();
    app.handle_pty(sr, &reply(serial + 1, &[]), physical)
        .unwrap();
    app.handle_pty(
        sr,
        &reply(serial + 2, &[pane_metadata_line(pane_id)]),
        physical,
    )
    .unwrap();
}

fn start_visible_lossy_capture(
    app: &mut App,
    sr: &mut ScreenReader,
    physical: &mut Vec<u8>,
    serial: usize,
    pane_id: u64,
) {
    app.handle_pty(sr, format!("%pause %{pane_id}\n").as_bytes(), physical)
        .unwrap();
    assert_eq!(
        drain(app, sr, physical),
        lector::tmux_input::continue_pane_command(lector::tmux_model::PaneId(pane_id))
    );
    app.handle_pty(sr, &reply(serial, &[]), physical).unwrap();
    start_capture_after_probe(app, sr, physical, serial + 1, pane_id);
}

#[test]
fn unsupported_flow_policy_reply_does_not_steal_inventory_or_bootstrap_correlation() {
    let recorder = Recorder::default();
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(4, 10)));
    let mut app = App::new(stack).unwrap();
    let mut sr = ScreenReader::new(speech::Speech::new(Box::new(recorder)));
    let mut physical = Vec::new();
    app.handle_pty(
        &mut sr,
        b"\x1bP1000p%begin 1 1 0\n%end 1 1 0\n",
        &mut physical,
    )
    .unwrap();
    let commands = drain(&mut app, &mut sr, &mut physical);
    assert_eq!(
        commands,
        [
            TMUX_FLOW_CONTROL_COMMAND,
            TMUX_FLOW_CONTROL_VERIFY_COMMAND,
            INVENTORY_COMMAND.as_bytes(),
        ]
        .concat()
    );

    app.handle_pty(
        &mut sr,
        &error_reply(2, &[b"unknown flag: pause-after".to_vec()]),
        &mut physical,
    )
    .unwrap();
    app.handle_pty(
        &mut sr,
        &reply(3, &[b"attached,control-mode".to_vec()]),
        &mut physical,
    )
    .unwrap();
    for (index, group) in inventory().iter().enumerate() {
        app.handle_pty(&mut sr, &reply(index + 4, group), &mut physical)
            .unwrap();
    }
    let captures = drain(&mut app, &mut sr, &mut physical);
    assert_eq!(
        captures
            .split(|byte| *byte == b'\n')
            .filter(|line| line.starts_with(b"capture-pane "))
            .count(),
        3,
        "captures={:?}",
        String::from_utf8_lossy(&captures)
    );
    for (index, pane_id) in [20, 21, 22].into_iter().enumerate() {
        app.handle_pty(
            &mut sr,
            &reply(30 + index, &[format!("ready-{pane_id}").into_bytes()]),
            &mut physical,
        )
        .unwrap();
    }
    let contents = app.debug_tmux_pane_contents(1, 20).unwrap();
    assert!(
        contents
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
            .contains("ready-20"),
        "{contents:?}"
    );
    assert_eq!(
        app.debug_tmux_pane_flow_state(1, 20).unwrap().status,
        TmuxFlowStatus::Running
    );
}

fn pane_output_record(pane_id: u64, bytes: &[u8]) -> Vec<u8> {
    let mut record = format!("%output %{pane_id} ").into_bytes();
    for &byte in bytes {
        if (0x20..=0x7e).contains(&byte) && byte != b'\\' {
            record.push(byte);
        } else {
            record.extend_from_slice(format!("\\{byte:03o}").as_bytes());
        }
    }
    record.push(b'\n');
    record
}

#[test]
fn scheduled_active_tmux_pane_finalization_wakes_once_without_spinning() {
    let (mut app, mut sr, recorder, clock, mut physical) = ready_app_with_clock(true);
    app.drain_scheduled_output(&mut physical, true)
        .expect("present the bootstrapped tmux scene");
    let mut control = Vec::new();
    app.handle_tick(&mut sr, &mut control, &mut physical)
        .expect("finish the deferred bootstrap announcement");
    recorder.0.borrow_mut().clear();
    physical.clear();

    app.handle_pty(
        &mut sr,
        &pane_output_record(20, b"\r\x1b[2Kactive deadline"),
        &mut physical,
    )
    .expect("queue active-pane output");
    assert_eq!(
        app.scheduled_output_timeout(),
        Some(Duration::ZERO),
        "the render must wake the loop immediately before accessibility can run"
    );
    assert!(
        !app.maybe_finalize_changes(&mut sr).unwrap(),
        "unpresented pane output reached accessibility"
    );

    let report = app
        .drain_scheduled_output(&mut physical, false)
        .expect("present active-pane output at its render deadline");
    assert_eq!(report.completed_renders.len(), 1);

    let stabilization_ms = u128::from(lector::app::DIFF_DELAY);
    let remaining_ms = stabilization_ms;
    assert_eq!(
        app.scheduled_output_timeout(),
        Some(Duration::from_millis(
            remaining_ms.try_into().expect("test duration fits u64")
        )),
        "the completed active pane must arm its accessibility deadline"
    );
    assert!(
        !app.wants_tick(),
        "a future accessibility deadline must sleep instead of busy-spinning"
    );

    if remaining_ms > 0 {
        clock.advance(remaining_ms.saturating_sub(1));
        assert_eq!(
            app.scheduled_output_timeout(),
            Some(Duration::from_millis(1))
        );
        assert!(!app.maybe_finalize_changes(&mut sr).unwrap());
        assert!(!app.wants_tick());
        clock.advance(1);
    }
    assert_eq!(
        app.scheduled_output_timeout(),
        Some(Duration::ZERO),
        "the event loop must receive one wakeup at the finalization boundary"
    );
    assert!(app.maybe_finalize_changes(&mut sr).unwrap());
    assert!(
        recorder
            .0
            .borrow()
            .iter()
            .any(|text| text.contains("active deadline")),
        "active pane output was not auto-read: {:?}",
        recorder.0.borrow()
    );
    assert_eq!(
        app.scheduled_output_timeout(),
        None,
        "the completed accessibility pass must disarm its deadline"
    );
    assert!(
        !app.wants_tick(),
        "the completed pass left a spin source armed"
    );

    app.handle_pty(
        &mut sr,
        &pane_output_record(22, b"hidden background update"),
        &mut physical,
    )
    .expect("model hidden-pane output");
    assert_eq!(
        app.scheduled_output_timeout(),
        None,
        "ordinary background-window activity must remain silent"
    );
    assert!(!app.wants_tick());
    assert!(!physical.contains(&b'\x07'));
}

#[test]
fn pty_presentation_batch_models_visible_output_before_finishing_render() {
    let (mut app, mut sr, _recorder, _initial_physical) = ready_app(false);
    let before = app
        .presented_scene()
        .clone()
        .into_terminal_snapshot()
        .contents_full();
    let mut physical = PresentationOutput::default();

    app.begin_pty_presentation_batch();
    app.handle_pty(
        &mut sr,
        &pane_output_record(20, b"\x1b[1;1H\x1b[2KMODEL"),
        &mut physical,
    )
    .unwrap();

    assert!(
        app.debug_tmux_pane_contents(1, 20)
            .unwrap()
            .contains("MODEL")
    );
    assert_eq!(
        app.presented_scene()
            .clone()
            .into_terminal_snapshot()
            .contents_full(),
        before,
        "parsing a batch published it before the presentation boundary"
    );
    assert!(physical.bytes.is_empty());
    assert_eq!(physical.flushes, 0);

    app.finish_pty_presentation_batch(&mut sr, &mut physical)
        .unwrap();
    assert!(!physical.bytes.is_empty());
    assert_eq!(physical.flushes, 1);
    assert!(
        app.presented_scene()
            .clone()
            .into_terminal_snapshot()
            .contents_full()
            .contains("MODEL")
    );
}

#[test]
fn pty_presentation_batch_coalesces_same_pane_records_into_one_final_render() {
    let (mut app, mut sr, _recorder, _initial_physical) = ready_app(false);
    let mut physical = PresentationOutput::default();

    app.begin_pty_presentation_batch();
    app.handle_pty(
        &mut sr,
        &pane_output_record(20, b"\x1b[1;1H\x1b[2KFIRST"),
        &mut physical,
    )
    .unwrap();
    assert!(
        app.debug_tmux_pane_contents(1, 20)
            .unwrap()
            .contains("FIRST")
    );
    app.handle_pty(
        &mut sr,
        &pane_output_record(20, b"\x1b[1;1H\x1b[2KFINAL"),
        &mut physical,
    )
    .unwrap();
    assert!(
        app.debug_tmux_pane_contents(1, 20)
            .unwrap()
            .contains("FIRST"),
        "the adjacent tail should wait for the PTY-drain boundary"
    );
    assert!(physical.bytes.is_empty());
    assert_eq!(physical.flushes, 0);

    app.finish_pty_presentation_batch(&mut sr, &mut physical)
        .unwrap();
    assert_eq!(
        physical.flushes, 1,
        "same-pane records rendered more than once"
    );
    let presented = app
        .presented_scene()
        .clone()
        .into_terminal_snapshot()
        .contents_full();
    assert!(presented.contains("FINAL"), "presented={presented:?}");
    assert!(!presented.contains("FIRST"), "presented={presented:?}");
}

#[test]
fn pty_presentation_batch_advances_ghostty_twice_for_a_fragmented_same_pane_burst() {
    const RECORDS: usize = 512;
    let (mut app, mut sr, _recorder, _initial_physical) = ready_app(false);
    let mut physical = PresentationOutput::default();

    app.begin_pty_presentation_batch();
    for index in 0..RECORDS {
        let payload = format!("\r\x1b[2Krecord-{index:03}");
        app.handle_pty(
            &mut sr,
            &pane_output_record(20, payload.as_bytes()),
            &mut physical,
        )
        .unwrap();
    }

    assert_eq!(
        app.debug_tmux_pane_pending_update_batch_count(1, 20),
        Some(1),
        "adjacent tmux records reached Ghostty before the drain boundary"
    );
    app.finish_pty_presentation_batch(&mut sr, &mut physical)
        .unwrap();
    assert_eq!(
        app.debug_tmux_pane_pending_update_batch_count(1, 20),
        Some(2),
        "a fragmented same-pane burst should require only the immediate and coalesced advances"
    );
    let contents = app.debug_tmux_pane_contents(1, 20).unwrap();
    let compact = contents
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    assert!(compact.contains("record-511"), "contents={contents:?}");
    assert_eq!(physical.flushes, 1);
}

#[test]
fn canceling_pty_presentation_batch_cannot_leave_a_stale_deferred_render() {
    let (mut app, mut sr, _recorder, _initial_physical) = ready_app(false);
    let before = app.presented_scene().clone();
    let mut physical = PresentationOutput::default();

    app.begin_pty_presentation_batch();
    app.handle_pty(
        &mut sr,
        &pane_output_record(20, b"\x1b[1;1H\x1b[2KHOLD"),
        &mut physical,
    )
    .unwrap();
    assert!(
        app.debug_tmux_pane_contents(1, 20)
            .unwrap()
            .contains("HOLD")
    );
    app.cancel_pty_presentation_batch();
    app.cancel_pty_presentation_batch();
    app.finish_pty_presentation_batch(&mut sr, &mut physical)
        .unwrap();
    assert!(physical.bytes.is_empty());
    assert_eq!(physical.flushes, 0);
    assert_eq!(app.presented_scene(), &before);

    app.begin_pty_presentation_batch();
    app.handle_pty(
        &mut sr,
        &pane_output_record(20, b"\x1b[1;1H\x1b[2KFRESH"),
        &mut physical,
    )
    .unwrap();
    app.finish_pty_presentation_batch(&mut sr, &mut physical)
        .unwrap();
    assert_eq!(physical.flushes, 1);
    let presented = app
        .presented_scene()
        .clone()
        .into_terminal_snapshot()
        .contents_full();
    assert!(presented.contains("FRESH"), "presented={presented:?}");
    assert!(!presented.contains("HOLD"), "presented={presented:?}");
}

#[test]
fn low_rate_nonactive_updates_stay_summary_bounded_end_to_end() {
    const ITERATIONS: usize = 512;
    let (mut app, mut sr, _recorder, mut physical) = ready_app(false);

    for iteration in 0..ITERATIONS {
        for (pane_id, prefix) in [(21, 'v'), (22, 'h')] {
            let payload = format!("\x1b[1;1H\x1b[2K{prefix}{iteration:03}");
            app.handle_pty(
                &mut sr,
                &pane_output_record(pane_id, payload.as_bytes()),
                &mut physical,
            )
            .unwrap();
            assert_eq!(
                app.debug_tmux_pane_pending_update_batch_count(1, pane_id),
                Some(0),
                "pane {pane_id} retained batch {iteration}"
            );
        }
        // Each small pair is one quiet transport turn, so this workload never
        // reaches byte-backlog flow control and exercises summary ownership
        // rather than overload recovery.
        let _ = drain(&mut app, &mut sr, &mut physical);
        physical.clear();
    }

    assert!(
        app.debug_tmux_pane_contents(1, 21)
            .unwrap()
            .contains("v511")
    );
    assert!(
        app.debug_tmux_pane_contents(1, 22)
            .unwrap()
            .contains("h511")
    );
}

fn verify_scheduled_scene(
    app: &mut App,
    physical: &mut Vec<u8>,
    oracle: &mut RenderOracle,
    case: &str,
) -> PresentedScene {
    physical.clear();
    let report = app.drain_scheduled_output(physical, true).unwrap();
    assert_eq!(report.completed_renders.len(), 1, "{case}");
    let predicted = report.completed_renders[0].predicted.clone();
    let completed_geometry = report.completed_renders[0].geometry;
    oracle
        .verify(
            case,
            &predicted,
            &RenderBatch::new(
                vec![OutputTransaction::with_resize(
                    completed_geometry,
                    physical.as_slice(),
                )],
                predicted.clone(),
            ),
        )
        .unwrap_or_else(|error| panic!("{case}: {error}"));
    predicted
}

#[test]
fn app_tmux_images_survive_splits_overlays_window_switches_and_resizes_in_the_oracle() {
    let (mut app, mut sr, _recorder, mut physical) = ready_app(true);
    let geometry = TerminalGeometry::new(4, 10, 8, 10);
    let mut profile = PhysicalTerminalProfile::conservative(geometry);
    profile.kitty_graphics = true;
    app.set_physical_profile(profile);
    app.on_resize_with_geometry(geometry, &mut physical)
        .unwrap();
    let mut oracle = RenderOracle::new(geometry).unwrap();
    verify_scheduled_scene(&mut app, &mut physical, &mut oracle, "tmux-image-initial");

    app.handle_pty(&mut sr, &pane_output_record(20, RED_IMAGE), &mut physical)
        .unwrap();
    app.handle_pty(&mut sr, &pane_output_record(21, GREEN_IMAGE), &mut physical)
        .unwrap();
    let visible = verify_scheduled_scene(
        &mut app,
        &mut physical,
        &mut oracle,
        "tmux-image-split-visible",
    );
    assert_eq!(visible.images().len(), 2);
    assert_ne!(visible.images()[0].image_id, visible.images()[1].image_id);

    app.show_message(&mut sr, "overlay", "images are occluded", &mut physical)
        .unwrap();
    let occluded =
        verify_scheduled_scene(&mut app, &mut physical, &mut oracle, "tmux-image-overlay");
    assert!(occluded.images().is_empty());
    app.handle_stdin(&mut sr, b"\r", &mut Vec::new(), &mut physical)
        .unwrap();
    let restored = verify_scheduled_scene(
        &mut app,
        &mut physical,
        &mut oracle,
        "tmux-image-overlay-restored",
    );
    assert_eq!(restored.images().len(), 2);

    app.handle_pty(&mut sr, b"%session-window-changed $1 @11\n", &mut physical)
        .unwrap();
    app.handle_pty(&mut sr, &pane_output_record(22, RED_IMAGE), &mut physical)
        .unwrap();
    let hidden_window = verify_scheduled_scene(
        &mut app,
        &mut physical,
        &mut oracle,
        "tmux-image-window-switch",
    );
    assert_eq!(hidden_window.images().len(), 1);

    let resized_geometry = TerminalGeometry::new(6, 12, 8, 10);
    app.on_resize_with_geometry(resized_geometry, &mut physical)
        .unwrap();
    let resized = verify_scheduled_scene(&mut app, &mut physical, &mut oracle, "tmux-image-resize");
    assert_eq!(resized.geometry(), resized_geometry);
    assert_eq!(resized.images().len(), 1);
}

#[test]
fn pause_continue_and_extended_output_have_explicit_per_pane_state() {
    let (mut app, mut sr, _recorder, mut physical) = ready_app(false);
    assert_eq!(
        app.debug_tmux_pane_flow_state(1, 20).unwrap().status,
        TmuxFlowStatus::Running
    );

    app.handle_pty(
        &mut sr,
        b"%extended-output %20 75 future : fresh-extended\n",
        &mut physical,
    )
    .unwrap();
    let fresh = app.debug_tmux_pane_contents(1, 20).unwrap();
    assert!(
        fresh
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
            .contains("fresh-extended"),
        "{fresh:?}"
    );
    assert_eq!(
        app.debug_tmux_pane_flow_state(1, 20)
            .unwrap()
            .last_extended_output_age_ms,
        Some(75)
    );

    app.handle_pty(&mut sr, b"%pause %20\n%pause %20\n", &mut physical)
        .unwrap();
    assert_eq!(
        app.debug_tmux_pane_flow_state(1, 20).unwrap().status,
        TmuxFlowStatus::Resynchronizing,
        "tmux pause discards output, so continue alone cannot make the pane current"
    );
    assert_eq!(
        drain(&mut app, &mut sr, &mut physical),
        b"refresh-client -A '%20:continue'\n",
        "duplicate pause queued duplicate resume commands"
    );
    app.handle_pty(
        &mut sr,
        &reply(40, &[b"%continue %20".to_vec()]),
        &mut physical,
    )
    .unwrap();
    assert_eq!(
        app.debug_tmux_pane_flow_state(1, 20).unwrap().status,
        TmuxFlowStatus::Resynchronizing,
        "resume must not trust pixels that tmux discarded while paused"
    );
    start_capture_after_probe(&mut app, &mut sr, &mut physical, 41, 20);
    finish_capture(
        &mut app,
        &mut sr,
        &mut physical,
        42,
        20,
        &[b"current after pause".to_vec()],
    );
    assert_eq!(
        app.debug_tmux_pane_flow_state(1, 20).unwrap().status,
        TmuxFlowStatus::Running
    );

    app.handle_pty(&mut sr, b"%pause %20\n", &mut physical)
        .unwrap();
    assert_eq!(
        drain(&mut app, &mut sr, &mut physical),
        b"refresh-client -A '%20:continue'\n",
        "a later pause was stranded by the previous resume request"
    );
}

#[test]
fn pane_default_colour_queries_use_the_control_client_report_channel() {
    let (mut app, mut sr, _recorder, mut physical) = ready_app(false);
    app.handle_pty(
        &mut sr,
        &pane_output_record(20, b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\"),
        &mut physical,
    )
    .unwrap();
    assert_eq!(
        drain(&mut app, &mut sr, &mut physical),
        b"refresh-client -r '%20:\x1b]10;rgb:ffff/ffff/ffff\x1b\\'\n\
          refresh-client -r '%20:\x1b]11;rgb:0000/0000/0000\x1b\\'\n"
    );
}

#[test]
fn bounded_foreground_bursts_do_not_pause_and_input_runs_between_turns() {
    let (mut app, mut sr, _recorder, mut physical) = ready_app(false);
    let mut first_turn = vec![b'x'; 24 * 1024];
    first_turn.extend_from_slice(b"\x1b[2J\x1b[HTURN-ONE");

    app.begin_pty_presentation_batch();
    app.handle_pty(&mut sr, &pane_output_record(20, &first_turn), &mut physical)
        .expect("process one bounded foreground-output turn");
    app.finish_pty_presentation_batch(&mut sr, &mut physical)
        .expect("present the final foreground state");

    let contents = app.debug_tmux_pane_contents(1, 20).unwrap();
    let compact = contents
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    assert!(
        compact.contains("TURN-ONE"),
        "the foreground parser dropped the latter part of its bounded PTY turn: {contents:?}"
    );
    let flow = app.debug_tmux_pane_flow_state(1, 20).unwrap();
    assert_eq!(flow.status, TmuxFlowStatus::Running);
    assert!(!flow.is_paused);
    assert_eq!(flow.skipped_incremental_bytes, 0);

    app.handle_stdin(&mut sr, b"z", &mut Vec::new(), &mut physical)
        .expect("accept input between bounded foreground turns");
    assert_eq!(
        drain(&mut app, &mut sr, &mut physical),
        b"send-keys -H -t %20 7a\n",
        "a foreground burst starved input before the next PTY turn"
    );

    let mut second_turn = vec![b'y'; 8 * 1024];
    second_turn.extend_from_slice(b"\x1b[2J\x1b[HTURN-TWO");
    app.begin_pty_presentation_batch();
    app.handle_pty(
        &mut sr,
        &pane_output_record(20, &second_turn),
        &mut physical,
    )
    .expect("process the next bounded foreground-output turn");
    app.finish_pty_presentation_batch(&mut sr, &mut physical)
        .expect("present the second foreground state");
    let contents = app.debug_tmux_pane_contents(1, 20).unwrap();
    let compact = contents
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    assert!(compact.contains("TURN-TWO"), "{contents:?}");
    let flow = app.debug_tmux_pane_flow_state(1, 20).unwrap();
    assert_eq!(flow.status, TmuxFlowStatus::Running);
    assert!(!flow.is_paused);
    assert_eq!(flow.skipped_incremental_bytes, 0);
    assert!(
        drain(&mut app, &mut sr, &mut physical).is_empty(),
        "ordinary foreground output incorrectly triggered tmux flow control"
    );
}

#[test]
fn pause_during_initial_bootstrap_requires_resume_and_a_final_capture() {
    let recorder = Recorder::default();
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(4, 10)));
    let app = App::new(stack).unwrap();
    let (mut app, mut sr, _recorder, mut physical) =
        start_app_with_bootstraps_in_flight(app, recorder, false);

    app.handle_pty(&mut sr, b"%pause %20\n", &mut physical)
        .unwrap();
    assert_eq!(
        drain(&mut app, &mut sr, &mut physical),
        b"refresh-client -A '%20:continue'\n"
    );
    app.handle_pty(
        &mut sr,
        &reply(30, &[b"bootstrap-before-resume".to_vec()]),
        &mut physical,
    )
    .unwrap();
    let paused = app.debug_tmux_pane_flow_state(1, 20).unwrap();
    assert!(paused.is_paused);
    assert_eq!(paused.status, TmuxFlowStatus::Resynchronizing);
    assert_eq!(paused.resync_count, 0);

    app.handle_pty(&mut sr, &reply(31, &[b"ready-21".to_vec()]), &mut physical)
        .unwrap();
    app.handle_pty(&mut sr, &reply(32, &[b"ready-22".to_vec()]), &mut physical)
        .unwrap();
    app.handle_pty(&mut sr, &reply(40, &[]), &mut physical)
        .unwrap();
    start_capture_after_probe(&mut app, &mut sr, &mut physical, 41, 20);
    finish_capture(
        &mut app,
        &mut sr,
        &mut physical,
        42,
        20,
        &[b"final-after-bootstrap-race".to_vec()],
    );
    let recovered = app.debug_tmux_pane_flow_state(1, 20).unwrap();
    assert!(!recovered.is_paused);
    assert_eq!(recovered.status, TmuxFlowStatus::Running);
    assert_eq!(recovered.resync_count, 1);
}

#[test]
fn paused_hidden_window_resumes_then_captures_authoritative_pixels_on_reveal() {
    let (mut app, mut sr, _recorder, mut physical) = ready_app(false);

    app.handle_pty(&mut sr, b"%pause %22\n", &mut physical)
        .unwrap();
    assert_eq!(
        app.debug_tmux_pane_flow_state(1, 22).unwrap().status,
        TmuxFlowStatus::Resynchronizing
    );
    assert!(
        drain(&mut app, &mut sr, &mut physical).is_empty(),
        "a hidden paused pane was resumed into the foreground transport"
    );

    app.handle_pty(&mut sr, b"%session-window-changed $1 @11\n", &mut physical)
        .unwrap();
    assert_eq!(
        drain(&mut app, &mut sr, &mut physical),
        b"refresh-client -A '%22:continue'\n"
    );
    app.handle_pty(&mut sr, &reply(40, &[]), &mut physical)
        .unwrap();
    start_capture_after_probe(&mut app, &mut sr, &mut physical, 41, 22);
    finish_capture(
        &mut app,
        &mut sr,
        &mut physical,
        42,
        22,
        &[b"changed entirely while hidden".to_vec()],
    );
    assert_eq!(
        app.debug_tmux_pane_flow_state(1, 22).unwrap().status,
        TmuxFlowStatus::Running
    );
    assert!(
        app.debug_tmux_pane_contents(1, 22)
            .unwrap()
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
            .contains("changedentirelywhilehidden")
    );
}

#[test]
fn failed_continue_retries_before_capture_and_cannot_leave_a_revealed_pane_paused() {
    let (mut app, mut sr, _recorder, clock, mut physical) = ready_app_with_clock(false);

    app.handle_pty(&mut sr, b"%pause %22\n", &mut physical)
        .unwrap();
    app.handle_pty(&mut sr, b"%session-window-changed $1 @11\n", &mut physical)
        .unwrap();
    assert_eq!(
        drain(&mut app, &mut sr, &mut physical),
        b"refresh-client -A '%22:continue'\n"
    );

    app.handle_pty(
        &mut sr,
        &error_reply(40, &[b"temporary continue failure".to_vec()]),
        &mut physical,
    )
    .unwrap();
    let failed = app.debug_tmux_pane_flow_state(1, 22).unwrap();
    assert!(failed.is_paused);
    assert!(!failed.resume_requested);
    assert_eq!(failed.status, TmuxFlowStatus::Resynchronizing);
    assert_eq!(failed.resync_failures, 1);
    assert_eq!(failed.consecutive_resync_failures, 1);
    assert_eq!(
        app.scheduled_output_timeout(),
        Some(Duration::from_millis(100))
    );
    assert!(
        drain(&mut app, &mut sr, &mut physical).is_empty(),
        "continue retry ignored its backoff"
    );

    clock.advance(99);
    assert!(drain(&mut app, &mut sr, &mut physical).is_empty());
    clock.advance(1);
    assert_eq!(
        drain(&mut app, &mut sr, &mut physical),
        b"refresh-client -A '%22:continue'\n"
    );
    app.handle_pty(&mut sr, &reply(41, &[]), &mut physical)
        .unwrap();
    start_capture_after_probe(&mut app, &mut sr, &mut physical, 42, 22);
    finish_capture(
        &mut app,
        &mut sr,
        &mut physical,
        43,
        22,
        &[b"authoritative after continue retry".to_vec()],
    );
    let recovered = app.debug_tmux_pane_flow_state(1, 22).unwrap();
    assert!(!recovered.is_paused);
    assert_eq!(recovered.status, TmuxFlowStatus::Running);
    assert_eq!(recovered.resync_failures, 1);
    assert_eq!(recovered.consecutive_resync_failures, 0);
}

#[test]
fn proactive_pause_stays_stale_after_its_local_backlog_drains() {
    let (mut app, mut sr, _recorder, mut physical) = ready_app(false);
    let mut output = Vec::new();
    for _ in 0..5 {
        output.extend(pane_output_record(22, &[b'x'; 4 * 1024]));
    }
    app.handle_pty(&mut sr, &output, &mut physical).unwrap();

    let paused = app.debug_tmux_pane_flow_state(1, 22).unwrap();
    assert!(paused.pause_requested);
    assert!(!paused.is_paused);
    assert_eq!(paused.status, TmuxFlowStatus::Resynchronizing);
    assert_eq!(
        drain(&mut app, &mut sr, &mut physical),
        b"refresh-client -A '%22:pause'\n"
    );
    app.handle_pty(&mut sr, &reply(40, &[]), &mut physical)
        .unwrap();
    for _ in 0..8 {
        if app.debug_tmux_background_pending_bytes() == 0 {
            break;
        }
        assert!(drain(&mut app, &mut sr, &mut physical).is_empty());
    }
    assert_eq!(app.debug_tmux_background_pending_bytes(), 0);
    assert_eq!(
        app.debug_tmux_pane_flow_state(1, 22).unwrap().status,
        TmuxFlowStatus::Resynchronizing,
        "draining Lector's local queue cannot restore bytes tmux discards while paused"
    );

    app.handle_pty(&mut sr, b"%session-window-changed $1 @11\n", &mut physical)
        .unwrap();
    assert_eq!(
        drain(&mut app, &mut sr, &mut physical),
        b"refresh-client -A '%22:continue'\n"
    );
    app.handle_pty(&mut sr, &reply(41, &[]), &mut physical)
        .unwrap();
    start_capture_after_probe(&mut app, &mut sr, &mut physical, 42, 22);
    finish_capture(
        &mut app,
        &mut sr,
        &mut physical,
        43,
        22,
        &[b"authoritative after proactive pause".to_vec()],
    );
    assert_eq!(
        app.debug_tmux_pane_flow_state(1, 22).unwrap().status,
        TmuxFlowStatus::Running
    );
}

#[test]
fn hidden_output_backlog_is_bounded_and_resynchronized_when_selected() {
    let (mut app, mut sr, _recorder, mut physical) = ready_app(false);
    let mut flood = Vec::new();
    for _ in 0..700 {
        flood.extend(pane_output_record(22, &[b'x'; 1024]));
    }
    flood.extend(pane_output_record(20, b"\x1b[2J\x1b[HALIVE"));

    app.handle_pty(&mut sr, &flood, &mut physical).unwrap();
    assert!(
        app.debug_tmux_pane_contents(1, 20)
            .unwrap()
            .contains("ALIVE"),
        "hidden output prevented a later foreground update in the same transport turn"
    );
    let hidden_flow = app.debug_tmux_pane_flow_state(1, 22).unwrap();
    assert_eq!(hidden_flow.status, TmuxFlowStatus::Resynchronizing);
    assert!(hidden_flow.skipped_incremental_bytes > 64 * 1024);
    assert!(app.debug_tmux_background_pending_bytes() <= 64 * 1024);
    assert_eq!(
        drain(&mut app, &mut sr, &mut physical),
        b"refresh-client -A '%22:pause'\n",
        "a hidden overloaded pane must pause upstream without an eager capture"
    );

    app.handle_pty(&mut sr, b"%session-window-changed $1 @11\n", &mut physical)
        .unwrap();
    assert_eq!(
        drain(&mut app, &mut sr, &mut physical),
        b"refresh-client -A '%22:continue'\n"
    );
    app.handle_pty(&mut sr, &reply(50, &[]), &mut physical)
        .unwrap();
    app.handle_pty(&mut sr, &reply(51, &[]), &mut physical)
        .unwrap();
    start_capture_after_probe(&mut app, &mut sr, &mut physical, 52, 22);
    finish_capture(
        &mut app,
        &mut sr,
        &mut physical,
        53,
        22,
        &[b"authoritative hidden screen".to_vec()],
    );
    assert_eq!(
        app.debug_tmux_pane_flow_state(1, 22).unwrap().status,
        TmuxFlowStatus::Running
    );
    assert!(
        app.debug_tmux_pane_contents(1, 22)
            .unwrap()
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
            .contains("authoritativehiddenscreen")
    );
}

#[test]
fn deferred_terminal_bytes_do_not_hide_a_nested_tmux_gateway_marker() {
    let (mut app, mut sr, _recorder, mut physical) = ready_app(false);
    app.handle_pty(
        &mut sr,
        &pane_output_record(22, &[b'x'; 4 * 1024]),
        &mut physical,
    )
    .unwrap();
    app.handle_pty(
        &mut sr,
        &pane_output_record(22, &[b'y'; 1024]),
        &mut physical,
    )
    .unwrap();
    assert!(app.debug_tmux_background_pending_bytes() > 0);

    app.handle_pty(
        &mut sr,
        &pane_output_record(22, b"\x1bP1000p%begin 1 1 0\n%end 1 1 0\n"),
        &mut physical,
    )
    .unwrap();
    assert_eq!(
        app.debug_tmux_gateway_origin(2),
        Some(lector::tmux_lifecycle::GatewayOrigin::Pane {
            parent_connection_id: 1,
            session_id: 1,
            window_id: 11,
            pane_id: 22,
        })
    );
}

#[test]
fn old_extended_output_is_delayed_not_lossy() {
    let (mut app, mut sr, _recorder, mut physical) = ready_app(false);
    app.handle_pty(
        &mut sr,
        b"%extended-output %20 60000 : delayed-but-complete\n%output %20 live-after\n",
        &mut physical,
    )
    .unwrap();
    let contents = app.debug_tmux_pane_contents(1, 20).unwrap();
    let compact = contents
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    assert!(compact.contains("complete"), "{contents:?}");
    assert!(compact.contains("live-after"), "{contents:?}");
    let flow = app.debug_tmux_pane_flow_state(1, 20).unwrap();
    assert_eq!(flow.status, TmuxFlowStatus::Running);
    assert_eq!(flow.last_extended_output_age_ms, Some(60_000));
    assert_eq!(flow.skipped_incremental_bytes, 0);
    assert!(drain(&mut app, &mut sr, &mut physical).is_empty());
}

#[test]
fn output_after_capture_is_sent_forces_one_quiet_authoritative_recapture() {
    let (mut app, mut sr, _recorder, clock, mut physical) = ready_app_with_clock(false);
    start_visible_lossy_capture(&mut app, &mut sr, &mut physical, 40, 20);
    assert!(
        app.debug_tmux_pane_flow_state(1, 20)
            .unwrap()
            .resync_in_flight
    );

    app.handle_pty(
        &mut sr,
        b"%output %20 raced-with-first-capture\n",
        &mut physical,
    )
    .unwrap();
    finish_capture(
        &mut app,
        &mut sr,
        &mut physical,
        42,
        20,
        &[b"first snapshot".to_vec()],
    );
    let pending = app.debug_tmux_pane_flow_state(1, 20).unwrap();
    assert_eq!(pending.status, TmuxFlowStatus::Resynchronizing);
    assert!(!pending.resync_in_flight);
    assert_eq!(pending.resync_count, 0);
    assert!(drain(&mut app, &mut sr, &mut physical).is_empty());

    clock.advance(100);
    start_capture_after_probe(&mut app, &mut sr, &mut physical, 45, 20);
    finish_capture(
        &mut app,
        &mut sr,
        &mut physical,
        46,
        20,
        &[b"quiet final snapshot".to_vec()],
    );
    let recovered = app.debug_tmux_pane_flow_state(1, 20).unwrap();
    assert_eq!(recovered.status, TmuxFlowStatus::Running);
    assert_eq!(recovered.resync_count, 1);
}

#[test]
fn screen_mode_change_during_capture_is_detected_and_retried() {
    let (mut app, mut sr, _recorder, clock, mut physical) = ready_app_with_clock(false);
    start_visible_lossy_capture(&mut app, &mut sr, &mut physical, 40, 20);
    app.handle_pty(
        &mut sr,
        &reply(42, &[b"snapshot from primary".to_vec()]),
        &mut physical,
    )
    .unwrap();
    app.handle_pty(&mut sr, &reply(43, &[]), &mut physical)
        .unwrap();
    app.handle_pty(
        &mut sr,
        &reply(44, &[pane_metadata_line_with(20, true, 0, 4, 2)]),
        &mut physical,
    )
    .unwrap();
    let raced = app.debug_tmux_pane_flow_state(1, 20).unwrap();
    assert_eq!(raced.status, TmuxFlowStatus::Resynchronizing);
    assert_eq!(raced.resync_count, 0);
    assert!(
        !app.debug_tmux_pane_contents(1, 20)
            .unwrap()
            .contains("snapshot from primary")
    );
    assert!(drain(&mut app, &mut sr, &mut physical).is_empty());
    clock.advance(100);
    assert_eq!(
        drain(&mut app, &mut sr, &mut physical),
        lector::tmux_model::pane_capture_metadata_command(lector::tmux_model::PaneId(20))
    );
}

#[test]
fn nonstop_output_cannot_postpone_the_final_capture_past_its_hard_deadline() {
    let (mut app, mut sr, _recorder, clock, mut physical) = ready_app_with_clock(false);
    start_visible_lossy_capture(&mut app, &mut sr, &mut physical, 40, 20);

    app.handle_pty(&mut sr, b"%output %20 first-race\n", &mut physical)
        .unwrap();
    finish_capture(
        &mut app,
        &mut sr,
        &mut physical,
        42,
        20,
        &[b"first snapshot".to_vec()],
    );
    assert_eq!(
        app.debug_tmux_pane_flow_state(1, 20).unwrap().status,
        TmuxFlowStatus::Resynchronizing
    );

    for step in 1..=20 {
        clock.advance(50);
        app.handle_pty(
            &mut sr,
            &pane_output_record(20, format!("racing-{step}").as_bytes()),
            &mut physical,
        )
        .unwrap();
        if step == 10 {
            app.handle_stdin(&mut sr, b"z", &mut Vec::new(), &mut physical)
                .expect("accept input during nonstop recovery output");
        }
        let commands = drain(&mut app, &mut sr, &mut physical);
        match step {
            10 => assert_eq!(
                commands, b"send-keys -H -t %20 7a\n",
                "recovery coalescing starved foreground input"
            ),
            20 => assert_eq!(
                commands,
                lector::tmux_input::pause_pane_command(lector::tmux_model::PaneId(20)),
                "continuous output postponed recovery beyond its one-second hard deadline"
            ),
            _ => assert!(
                commands.is_empty(),
                "recovery capture ran before its coalescing deadline: {commands:?}"
            ),
        }
    }

    let final_capture = app.debug_tmux_pane_flow_state(1, 20).unwrap();
    assert!(final_capture.pause_requested);
    assert!(!final_capture.resync_in_flight);
    assert!(final_capture.final_resync_requested);
    assert_eq!(final_capture.resync_after_ms, None);
    assert_eq!(final_capture.recapture_hard_deadline_ms, None);

    // Keep racing the hard-deadline pause request itself. This final round must
    // not silently start a fresh one-second recovery epoch; its later snapshot
    // covers everything tmux processed before the pause became effective.
    for step in 21..=25 {
        clock.advance(50);
        app.handle_pty(
            &mut sr,
            &pane_output_record(20, format!("racing-final-{step}").as_bytes()),
            &mut physical,
        )
        .unwrap();
        assert!(
            drain(&mut app, &mut sr, &mut physical).is_empty(),
            "output racing the final pause queued another recovery command"
        );
    }
    let raced_final = app.debug_tmux_pane_flow_state(1, 20).unwrap();
    assert!(raced_final.final_resync_requested);
    assert_eq!(raced_final.resync_after_ms, None);
    assert_eq!(raced_final.recapture_hard_deadline_ms, None);

    // The input and pause commands share one reply FIFO. After tmux confirms
    // the pause, probe its capture basis while delivery is stopped.
    app.handle_pty(&mut sr, &reply(60, &[]), &mut physical)
        .unwrap();
    app.handle_pty(&mut sr, &reply(61, &[]), &mut physical)
        .unwrap();
    assert_eq!(
        drain(&mut app, &mut sr, &mut physical),
        lector::tmux_model::pane_capture_metadata_command(lector::tmux_model::PaneId(20))
    );
    app.handle_pty(
        &mut sr,
        &reply(62, &[pane_metadata_line(20)]),
        &mut physical,
    )
    .unwrap();
    assert_eq!(
        drain(&mut app, &mut sr, &mut physical),
        final_capture_pipeline(20)
    );
    // Continue and capture are one parsed command sequence. tmux resets
    // this client's stream offset before taking the snapshot, so the capture
    // is the exact boundary between discarded paused output and live delivery.
    app.handle_pty(&mut sr, &reply(63, &[]), &mut physical)
        .unwrap();
    finish_capture(
        &mut app,
        &mut sr,
        &mut physical,
        64,
        20,
        &[b"bounded final snapshot".to_vec()],
    );
    let recovered = app.debug_tmux_pane_flow_state(1, 20).unwrap();
    assert_eq!(recovered.status, TmuxFlowStatus::Running);
    assert_eq!(recovered.resync_count, 1);
    assert!(!recovered.final_resync_requested);

    app.handle_pty(
        &mut sr,
        &pane_output_record(20, b"\x1b[2J\x1b[Hlive-after-final"),
        &mut physical,
    )
    .unwrap();
    let live = app.debug_tmux_pane_contents(1, 20).unwrap();
    assert!(
        live.chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
            .contains("live-after-final"),
        "incremental delivery did not resume after the finite final capture: {live:?}"
    );
    clock.advance(1_000);
    assert!(
        drain(&mut app, &mut sr, &mut physical).is_empty(),
        "post-recovery output incorrectly started another recovery epoch"
    );
}

#[test]
fn failed_pane_resync_retries_without_applying_incremental_output_to_stale_pixels() {
    let (mut app, mut sr, recorder, clock, mut physical) = ready_app_with_clock(false);
    recorder.0.borrow_mut().clear();
    start_visible_lossy_capture(&mut app, &mut sr, &mut physical, 40, 20);

    app.handle_pty(
        &mut sr,
        &error_reply(42, &[b"capture failed".to_vec()]),
        &mut physical,
    )
    .unwrap();
    app.handle_pty(&mut sr, &reply(43, &[]), &mut physical)
        .unwrap();
    app.handle_pty(
        &mut sr,
        &reply(44, &[pane_metadata_line(20)]),
        &mut physical,
    )
    .unwrap();
    let failed = app.debug_tmux_pane_flow_state(1, 20).unwrap();
    assert_eq!(failed.status, TmuxFlowStatus::ResyncFailed);
    assert_eq!(failed.resync_count, 0);
    assert_eq!(failed.resync_failures, 1);
    assert_eq!(failed.consecutive_resync_failures, 1);
    assert_eq!(
        app.scheduled_output_timeout(),
        Some(Duration::from_millis(100))
    );
    assert!(
        !recorder
            .0
            .borrow()
            .iter()
            .any(|message| message.contains("resynchron")),
        "an automatically retried pane capture is diagnostic, not actionable speech: {:?}",
        recorder.0.borrow()
    );

    app.handle_pty(&mut sr, b"%output %20 fresh-after-failure\n", &mut physical)
        .unwrap();
    let contents = app.debug_tmux_pane_contents(1, 20).unwrap();
    assert!(
        !contents.contains("fresh-after-failure"),
        "incremental output cannot repair a screen whose base snapshot is stale: {contents:?}"
    );
    assert!(
        drain(&mut app, &mut sr, &mut physical).is_empty(),
        "capture retry ignored its backoff"
    );

    clock.advance(99);
    assert!(drain(&mut app, &mut sr, &mut physical).is_empty());
    clock.advance(1);
    assert_eq!(
        drain(&mut app, &mut sr, &mut physical),
        lector::tmux_model::pane_capture_metadata_command(lector::tmux_model::PaneId(20))
    );
    app.handle_pty(
        &mut sr,
        &error_reply(45, &[b"metadata probe failed".to_vec()]),
        &mut physical,
    )
    .unwrap();
    let failed_again = app.debug_tmux_pane_flow_state(1, 20).unwrap();
    assert_eq!(failed_again.resync_failures, 2);
    assert_eq!(failed_again.consecutive_resync_failures, 2);
    assert_eq!(
        app.scheduled_output_timeout(),
        Some(Duration::from_millis(200))
    );
    assert!(
        !recorder
            .0
            .borrow()
            .iter()
            .any(|message| message.contains("resynchron")),
        "repeated automatic retries must remain silent: {:?}",
        recorder.0.borrow()
    );

    clock.advance(199);
    assert!(drain(&mut app, &mut sr, &mut physical).is_empty());
    clock.advance(1);
    start_capture_after_probe(&mut app, &mut sr, &mut physical, 46, 20);
    finish_capture(
        &mut app,
        &mut sr,
        &mut physical,
        47,
        20,
        &[b"retry-current".to_vec()],
    );
    let contents = app.debug_tmux_pane_contents(1, 20).unwrap();
    assert!(
        contents
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
            .contains("retry-current"),
        "{contents:?}"
    );
    let recovered = app.debug_tmux_pane_flow_state(1, 20).unwrap();
    assert_eq!(recovered.status, TmuxFlowStatus::Running);
    assert_eq!(recovered.resync_count, 1);
    assert_eq!(recovered.resync_failures, 2);
    assert_eq!(recovered.consecutive_resync_failures, 0);
    assert!(!recovered.resync_failure_announced);
}

#[test]
fn pane_exit_while_resync_reply_is_in_flight_discards_the_stale_capture() {
    let (mut app, mut sr, recorder, mut physical) = ready_app(false);
    recorder.0.borrow_mut().clear();
    start_visible_lossy_capture(&mut app, &mut sr, &mut physical, 40, 20);

    app.handle_pty(&mut sr, b"%pane-exited %20\n", &mut physical)
        .unwrap();
    assert!(app.debug_tmux_pane_contents(1, 20).is_none());
    assert!(app.debug_tmux_pane_flow_state(1, 20).is_none());

    app.handle_pty(
        &mut sr,
        &reply(42, &[b"capture for vanished pane".to_vec()]),
        &mut physical,
    )
    .expect("a late capture reply for a removed pane must be harmless");
    app.handle_pty(&mut sr, &reply(43, &[]), &mut physical)
        .expect("a late parser-continuation reply must be harmless");
    app.handle_pty(
        &mut sr,
        &reply(44, &[pane_metadata_line(20)]),
        &mut physical,
    )
    .expect("a late verification reply must be harmless");
    assert!(app.debug_tmux_pane_contents(1, 20).is_none());
    assert!(app.debug_tmux_pane_flow_state(1, 20).is_none());
    assert!(
        !recorder
            .0
            .borrow()
            .iter()
            .any(|message| message.contains("pane 20 resynchronized")),
        "speech={:?}",
        recorder.0.borrow()
    );
}

#[test]
fn pane_resync_explicitly_drops_unrecoverable_image_state() {
    let (mut app, mut sr, _recorder, mut physical) = ready_app(false);
    let mut record = b"%output %20 ".to_vec();
    for &byte in RED_IMAGE {
        if (0x20..=0x7e).contains(&byte) && byte != b'\\' {
            record.push(byte);
        } else {
            record.extend_from_slice(format!("\\{byte:03o}").as_bytes());
        }
    }
    record.push(b'\n');
    app.handle_pty(&mut sr, &record, &mut physical).unwrap();
    assert_eq!(app.composed_scene().unwrap().images.len(), 1);

    start_visible_lossy_capture(&mut app, &mut sr, &mut physical, 40, 20);
    finish_capture(
        &mut app,
        &mut sr,
        &mut physical,
        42,
        20,
        &[b"text-only recovery".to_vec()],
    );
    assert!(app.composed_scene().unwrap().images.is_empty());
    assert!(
        app.debug_tmux_pane_flow_state(1, 20)
            .unwrap()
            .limitations
            .contains(&TmuxResyncLimitation::KittyImages)
    );
}

#[test]
fn slow_physical_output_and_repeated_flow_events_keep_every_retained_queue_bounded() {
    let (mut app, mut sr, recorder, mut physical) = ready_app(true);
    recorder.0.borrow_mut().clear();
    app.drain_scheduled_output(&mut physical, true).unwrap();
    physical.clear();
    let started = Instant::now();
    for index in 0..200 {
        app.handle_pty(
            &mut sr,
            format!("%output %20 row-{index}\\015\\012\n").as_bytes(),
            &mut physical,
        )
        .unwrap();
    }
    for _ in 0..200 {
        app.handle_pty(&mut sr, b"%pause %20\n", &mut physical)
            .unwrap();
    }
    let elapsed = started.elapsed();
    let usage = app.debug_tmux_resource_usage(1).unwrap();
    assert!(usage.scrollback_rows <= 10_000);
    assert!(usage.image_bytes <= 64 * 1024 * 1024);
    assert!(
        app.debug_scheduled_output_pending_bytes() <= 8 * 1024,
        "scheduler retained {} bytes",
        app.debug_scheduled_output_pending_bytes()
    );
    assert!(app.debug_tmux_pending_command_bytes() <= 4096);
    assert!(recorder.0.borrow().len() <= 1);
    assert!(
        elapsed < Duration::from_secs(20),
        "bounded tmux flood took {elapsed:?}"
    );
    assert_eq!(
        drain(&mut app, &mut sr, &mut physical),
        b"refresh-client -A '%20:continue'\n"
    );
}

#[test]
fn many_pane_window_output_resize_image_and_bell_soak_stays_bounded_and_oracle_correct() {
    const WINDOWS: usize = 8;
    const ITERATIONS: usize = 192;
    let mut topology = stress_topology(WINDOWS);
    let mut panes = TmuxPaneSet::new(99);
    let requests = panes.reconcile(&topology).unwrap();
    assert_eq!(requests.len(), WINDOWS * 2);
    for request in requests {
        panes
            .apply_bootstrap(
                request.pane_id,
                CommandStatus::Success,
                &[format!("bootstrap-{}", request.pane_id.0).into_bytes()],
                0,
            )
            .unwrap();
        panes.process_output(request.pane_id, RED_IMAGE).unwrap();
    }

    let mut geometry = TerminalGeometry::new(8, 40, 8, 10);
    let initial = panes.compose(&topology, geometry).unwrap();
    let capabilities = RenderCapabilities {
        kitty_graphics: true,
        ..RenderCapabilities::default()
    };
    let mut renderer = IncrementalVtRenderer::new(capabilities);
    let first = renderer
        .render(
            &initial,
            &SceneDamage::Full,
            &PresentedScene::blank(geometry),
        )
        .unwrap();
    let mut oracle = RenderOracle::new(geometry).unwrap();
    oracle
        .verify("tmux-completion-soak-initial", &first.predicted, &first)
        .unwrap();
    renderer.confirm(&first.predicted);
    let mut presented = first.predicted;
    let mut incremental_bytes = 0usize;
    let mut full_bytes = 0usize;
    let mut full_fallbacks = 0usize;
    let mut bells = 0usize;
    let mut pty_to_render_latencies = Vec::with_capacity(ITERATIONS);
    let workload_started = Instant::now();
    let cpu_started = process_cpu_time();

    for iteration in 0..ITERATIONS {
        let iteration_started = Instant::now();
        let window = (iteration / 24) % WINDOWS;
        topology
            .apply_notification(
                b"session-window-changed",
                format!("$1 @{}", 10 + window).as_bytes(),
            )
            .unwrap();
        let pane_id = PaneId(100 + window as u64 * 2 + iteration as u64 % 2);
        let payload = format!("\x1b[8;1Hsoak-{iteration:04}\r\n{}", "x".repeat(96));
        let update = panes
            .process_output(pane_id, payload.as_bytes())
            .unwrap()
            .unwrap();
        bells = bells.saturating_add(update.effects.bells);

        let previous_geometry = geometry;
        if iteration % 24 == 23 {
            geometry = if geometry.cols == 40 {
                TerminalGeometry::new(10, 44, 8, 10)
            } else {
                TerminalGeometry::new(8, 40, 8, 10)
            };
        }
        let scene = panes.compose(&topology, geometry).unwrap();
        let damage = if previous_geometry != geometry || iteration % 24 == 0 {
            if previous_geometry != geometry {
                SceneDamage::Resize {
                    previous: previous_geometry,
                    next: geometry,
                }
            } else {
                SceneDamage::Full
            }
        } else {
            let surface = scene
                .panes
                .iter()
                .find(|surface| surface.id == panes.surface_id(pane_id).unwrap())
                .unwrap();
            SceneDamage::from_terminal_update(surface, &update, geometry)
        };
        let batch = renderer.render(&scene, &damage, &presented).unwrap();
        full_fallbacks += usize::from(renderer.last_strategy() == RenderStrategy::FullFallback);
        incremental_bytes = incremental_bytes.saturating_add(
            batch
                .transactions
                .iter()
                .map(|transaction| transaction.bytes.len())
                .sum::<usize>(),
        );
        let mut full = FullSceneVtRenderer::new(capabilities);
        let fallback = full.render(&scene, &SceneDamage::Full, &presented).unwrap();
        assert_eq!(fallback.predicted, batch.predicted);
        full_bytes = full_bytes.saturating_add(
            fallback
                .transactions
                .iter()
                .map(|transaction| transaction.bytes.len())
                .sum::<usize>(),
        );
        oracle
            .verify(
                &format!("tmux-completion-soak-{iteration}"),
                &batch.predicted,
                &batch,
            )
            .unwrap();
        renderer.confirm(&batch.predicted);
        presented = batch.predicted;
        pty_to_render_latencies.push(iteration_started.elapsed());
    }

    let usage = panes.resource_usage().unwrap();
    assert_eq!(usage.pane_count, WINDOWS * 2);
    assert_eq!(usage.image_uploads, WINDOWS * 2);
    assert_eq!(usage.image_bytes, WINDOWS * 2 * 4);
    assert!(usage.retained_text_bytes > WINDOWS * 2);
    assert!(usage.scrollback_rows > WINDOWS * 2);
    assert!(usage.scrollback_rows <= WINDOWS * 2 * 10_000);
    assert_eq!(bells, ITERATIONS);
    assert!(
        full_fallbacks > 0,
        "the mixed workload never exercised its correctness fallback"
    );
    assert!(
        incremental_bytes <= full_bytes.saturating_mul(2),
        "incremental={incremental_bytes}, full={full_bytes}"
    );
    pty_to_render_latencies.sort_unstable();
    let p95 = pty_to_render_latencies[ITERATIONS * 95 / 100];
    assert!(
        p95 < Duration::from_secs(1),
        "PTY-to-render p95 was {p95:?}"
    );
    assert!(workload_started.elapsed() < Duration::from_secs(20));
    let cpu_time = process_cpu_time().saturating_sub(cpu_started);
    assert!(cpu_time < Duration::from_secs(20));
    eprintln!(
        "tmux soak: panes={}, scrollback_rows={}, retained_text_bytes={}, image_bytes={}, PTY-to-render p95={p95:?}, cpu_time={cpu_time:?}, output_bytes={incremental_bytes}, full_fallback_bytes={full_bytes}, full_fallbacks={full_fallbacks}",
        usage.pane_count, usage.scrollback_rows, usage.retained_text_bytes, usage.image_bytes,
    );
}
