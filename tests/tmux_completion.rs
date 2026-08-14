use lector::{
    app::{App, TMUX_FLOW_CONTROL_COMMAND, TmuxFlowStatus, TmuxResyncLimitation},
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
    cell::RefCell,
    collections::BTreeSet,
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
        vec![b"O\tmode-keys\tvi".to_vec()],
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
        b"O\tmode-keys\tvi".to_vec(),
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

fn ready_app(scheduled: bool) -> (App, ScreenReader, Recorder, Vec<u8>) {
    let recorder = Recorder::default();
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(4, 10)));
    let mut app = App::new(stack).unwrap();
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
        INVENTORY_COMMAND.as_bytes(),
    ]
    .concat();
    assert_eq!(commands, expected);
    app.handle_pty(&mut sr, &reply(2, &[]), &mut physical)
        .unwrap();
    let groups = inventory();
    assert_eq!(groups.len(), INVENTORY_REPLY_COUNT);
    for (index, group) in groups.iter().enumerate() {
        app.handle_pty(&mut sr, &reply(index + 3, group), &mut physical)
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

fn drain(app: &mut App, sr: &mut ScreenReader, physical: &mut Vec<u8>) -> Vec<u8> {
    let mut commands = Vec::new();
    app.handle_tick(sr, &mut commands, physical).unwrap();
    commands
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
        [TMUX_FLOW_CONTROL_COMMAND, INVENTORY_COMMAND.as_bytes()].concat()
    );

    app.handle_pty(
        &mut sr,
        &error_reply(2, &[b"unknown flag: pause-after".to_vec()]),
        &mut physical,
    )
    .unwrap();
    for (index, group) in inventory().iter().enumerate() {
        app.handle_pty(&mut sr, &reply(index + 3, group), &mut physical)
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
        TmuxFlowStatus::Paused
    );
    assert_eq!(
        drain(&mut app, &mut sr, &mut physical),
        b"refresh-client -A %20:continue\n",
        "duplicate pause queued duplicate resume commands"
    );
    app.handle_pty(&mut sr, &reply(40, &[]), &mut physical)
        .unwrap();
    app.handle_pty(&mut sr, b"%continue %20\n", &mut physical)
        .unwrap();
    assert_eq!(
        app.debug_tmux_pane_flow_state(1, 20).unwrap().status,
        TmuxFlowStatus::Running
    );
}

#[test]
fn stale_incremental_output_is_skipped_coalesced_and_rebuilt_from_capture() {
    let (mut app, mut sr, recorder, mut physical) = ready_app(false);
    recorder.0.borrow_mut().clear();
    app.handle_pty(
        &mut sr,
        b"%extended-output %20 60000 : skipped-one\n%output %20 skipped-two\n",
        &mut physical,
    )
    .unwrap();
    assert!(
        !app.debug_tmux_pane_contents(1, 20)
            .unwrap()
            .contains("skipped")
    );
    let pending = app.debug_tmux_pane_flow_state(1, 20).unwrap();
    assert_eq!(pending.status, TmuxFlowStatus::Resynchronizing);
    assert!(pending.skipped_incremental_bytes >= b"skipped-oneskipped-two".len());
    assert_eq!(
        drain(&mut app, &mut sr, &mut physical),
        b"capture-pane -p -e -J -S - -t %20\n",
        "one lost interval must coalesce to one authoritative capture"
    );

    app.handle_pty(
        &mut sr,
        &reply(
            41,
            &[
                b"authoritative history".to_vec(),
                b"authoritative screen".to_vec(),
            ],
        ),
        &mut physical,
    )
    .unwrap();
    let rebuilt = app.debug_tmux_pane_contents(1, 20).unwrap();
    assert!(
        rebuilt
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
            .contains("authoritativescreen"),
        "{rebuilt:?}"
    );
    assert!(!rebuilt.contains("skipped"), "{rebuilt:?}");
    let recovered = app.debug_tmux_pane_flow_state(1, 20).unwrap();
    assert_eq!(recovered.status, TmuxFlowStatus::Running);
    assert_eq!(recovered.resync_count, 1);
    assert_eq!(
        recovered.limitations,
        BTreeSet::from([
            TmuxResyncLimitation::KittyImages,
            TmuxResyncLimitation::ParserContinuation,
            TmuxResyncLimitation::SemanticMetadata,
        ])
    );
    assert!(
        recorder
            .0
            .borrow()
            .iter()
            .any(|message| message.contains("pane 20 resynchronized")
                && message.contains("images")),
        "speech={:?}",
        recorder.0.borrow()
    );

    app.handle_pty(&mut sr, b"%output %20 live-after\n", &mut physical)
        .unwrap();
    let live = app.debug_tmux_pane_contents(1, 20).unwrap();
    assert!(
        live.chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
            .contains("live-after"),
        "{live:?}"
    );
}

#[test]
fn failed_pane_resync_is_explicit_and_fresh_output_can_continue() {
    let (mut app, mut sr, recorder, mut physical) = ready_app(false);
    recorder.0.borrow_mut().clear();
    app.handle_pty(
        &mut sr,
        b"%extended-output %20 60000 : stale\n",
        &mut physical,
    )
    .unwrap();
    assert_eq!(
        drain(&mut app, &mut sr, &mut physical),
        b"capture-pane -p -e -J -S - -t %20\n"
    );

    app.handle_pty(
        &mut sr,
        &error_reply(42, &[b"capture failed".to_vec()]),
        &mut physical,
    )
    .unwrap();
    let failed = app.debug_tmux_pane_flow_state(1, 20).unwrap();
    assert_eq!(failed.status, TmuxFlowStatus::ResyncFailed);
    assert_eq!(failed.resync_count, 0);
    assert_eq!(failed.resync_failures, 1);
    assert!(
        recorder
            .0
            .borrow()
            .iter()
            .any(|message| message.contains("pane 20 resynchronization failed")),
        "speech={:?}",
        recorder.0.borrow()
    );

    app.handle_pty(&mut sr, b"%output %20 fresh-after-failure\n", &mut physical)
        .unwrap();
    let contents = app.debug_tmux_pane_contents(1, 20).unwrap();
    assert!(
        contents
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
            .contains("fresh-after-failure"),
        "{contents:?}"
    );
}

#[test]
fn pane_exit_while_resync_reply_is_in_flight_discards_the_stale_capture() {
    let (mut app, mut sr, recorder, mut physical) = ready_app(false);
    recorder.0.borrow_mut().clear();
    app.handle_pty(
        &mut sr,
        b"%extended-output %20 60000 : stale\n",
        &mut physical,
    )
    .unwrap();
    assert_eq!(
        drain(&mut app, &mut sr, &mut physical),
        b"capture-pane -p -e -J -S - -t %20\n"
    );

    app.handle_pty(&mut sr, b"%pane-exited %20\n", &mut physical)
        .unwrap();
    assert!(app.debug_tmux_pane_contents(1, 20).is_none());
    assert!(app.debug_tmux_pane_flow_state(1, 20).is_none());

    app.handle_pty(
        &mut sr,
        &reply(43, &[b"capture for vanished pane".to_vec()]),
        &mut physical,
    )
    .expect("a late capture reply for a removed pane must be harmless");
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

    app.handle_pty(
        &mut sr,
        b"%extended-output %20 60000 : stale\n",
        &mut physical,
    )
    .unwrap();
    assert_eq!(
        drain(&mut app, &mut sr, &mut physical),
        b"capture-pane -p -e -J -S - -t %20\n"
    );
    app.handle_pty(
        &mut sr,
        &reply(42, &[b"text-only recovery".to_vec()]),
        &mut physical,
    )
    .unwrap();
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
        b"refresh-client -A %20:continue\n"
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
        let bells_before = panes
            .pending_update(pane_id)
            .map_or(0, |pending| pending.effects.bells);
        let update = panes
            .process_output(pane_id, payload.as_bytes())
            .unwrap()
            .unwrap();
        bells = bells.saturating_add(update.effects.bells.saturating_sub(bells_before));

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
