use lector::{
    harness::Harness,
    presentation::{
        CursorOwner, GridPoint, OutputTransaction, PresentedScene, RenderBatch, RenderOracle,
        Scene, SceneSurface, SurfaceId,
    },
    terminal::{GhosttyEngine, TerminalGeometry},
    terminal_protocol::PhysicalTerminalProfile,
};
use serde::Deserialize;
use std::{fs, path::Path};

const ROOT: SurfaceId = SurfaceId(1);
const DIRECT_WRITER_TRIPWIRE: &[u8] = b"lector-direct-writer-tripwire";
const SOURCE: &[u8] = b"\x1b[2J\x1b[Hstart\x1bPlector-direct-writer-tripwire\x1b\\\r\nfinal";

fn intended_scene(geometry: TerminalGeometry) -> PresentedScene {
    let mut engine = GhosttyEngine::new(geometry.rows, geometry.cols).expect("source engine");
    engine.advance(SOURCE).expect("advance source engine");
    let mut snapshot = engine.normalized_snapshot();
    // Lector owns the physical focus subscription independently of the child.
    snapshot.modes.focus_reporting = true;
    let mut scene = Scene::new(geometry);
    scene.effects.title.clone_from(&snapshot.title);
    scene
        .effects
        .working_directory
        .clone_from(&snapshot.working_directory);
    scene
        .panes
        .push(SceneSurface::new(ROOT, GridPoint::new(0, 0), snapshot));
    scene.cursor_owner = CursorOwner::Pane(ROOT);
    PresentedScene::compose(&scene).expect("compose intended scene")
}

fn verify_final_output(name: &str, geometry: TerminalGeometry, output: &[u8]) {
    let intended = intended_scene(geometry);
    let batch = RenderBatch::new(vec![OutputTransaction::new(output)], intended.clone());
    let mut oracle = RenderOracle::new(geometry).expect("physical Ghostty oracle");
    oracle
        .verify(name, &intended, &batch)
        .unwrap_or_else(|error| panic!("{name}: {error}"));
}

#[test]
fn default_application_path_never_forwards_an_application_pty_transaction() {
    let geometry = TerminalGeometry::from_cells(4, 40);
    let mut harness = Harness::new(geometry.rows, geometry.cols).expect("default harness");
    harness
        .handle_pty_output(SOURCE)
        .expect("present application output");

    let output = harness.terminal_output();
    assert!(
        !output
            .windows(DIRECT_WRITER_TRIPWIRE.len())
            .any(|window| window == DIRECT_WRITER_TRIPWIRE),
        "an opaque application DCS reached the physical writer verbatim"
    );
    verify_final_output("mandatory-compositor-default", geometry, output);
}

#[test]
fn overlay_switches_are_safe_at_every_unfinished_source_escape_boundary() {
    let geometry = TerminalGeometry::from_cells(4, 40);
    for split in 0..=SOURCE.len() {
        let mut harness = Harness::new(geometry.rows, geometry.cols).expect("default harness");
        harness
            .handle_pty_output(&SOURCE[..split])
            .unwrap_or_else(|error| panic!("split {split}: source prefix: {error}"));
        harness
            .handle_terminal_input(b"\x1br")
            .unwrap_or_else(|error| panic!("split {split}: open Review: {error}"));
        harness
            .handle_pty_output(&SOURCE[split..])
            .unwrap_or_else(|error| panic!("split {split}: source suffix: {error}"));
        harness
            .handle_terminal_input(b"q")
            .unwrap_or_else(|error| panic!("split {split}: close Review: {error}"));

        let output = harness.terminal_output();
        assert!(
            !output
                .windows(DIRECT_WRITER_TRIPWIRE.len())
                .any(|window| window == DIRECT_WRITER_TRIPWIRE),
            "split {split}: application DCS reached the physical writer"
        );
        verify_final_output(
            &format!("mandatory-compositor-overlay-split-{split}"),
            geometry,
            output,
        );
        assert!(harness.application_input().is_empty(), "split {split}");
    }
}

#[test]
fn scheduled_lifecycle_restores_modes_and_reconstructs_after_resume() {
    let geometry = TerminalGeometry::from_cells(4, 40);
    let mut harness = Harness::new_scheduled(geometry.rows, geometry.cols).expect("live harness");
    harness.configure_physical_terminal(Some(false));
    harness
        .activate_physical_terminal()
        .expect("activate physical terminal");
    harness
        .handle_pty_output(b"lifecycle screen\x1b[?1h\x1b[?2004h")
        .expect("queue source scene");
    harness.tick(4).expect("reach activation boundary");
    let active = harness
        .drain_scheduled_output(false)
        .expect("drain active scene");
    assert_eq!(active.completed_renders.len(), 1);
    assert!(
        harness
            .terminal_output()
            .windows(b"\x1b[?1004h".len())
            .any(|window| window == b"\x1b[?1004h")
    );

    let before_suspend = harness.terminal_output().len();
    harness
        .suspend_physical_terminal()
        .expect("suspend physical terminal");
    harness.tick(4).expect("reach suspend boundary");
    harness
        .drain_scheduled_output(false)
        .expect("drain suspend cleanup");
    let cleanup = &harness.terminal_output()[before_suspend..];
    for reset in [
        b"\x1b[?2026l".as_slice(),
        b"\x1b]8;;\x1b\\",
        b"\x1b[?2004l",
        b"\x1b[=0u",
        b"\x1b[?25h",
        b"\x1b[?1004l",
    ] {
        assert!(
            cleanup.windows(reset.len()).any(|window| window == reset),
            "missing suspend reset {reset:?}"
        );
    }

    let before_resume = harness.terminal_output().len();
    harness
        .resume_physical_terminal()
        .expect("resume physical terminal");
    harness.tick(4).expect("reach resume boundary");
    let resumed = harness
        .drain_scheduled_output(false)
        .expect("drain resumed reconstruction");
    assert_eq!(resumed.completed_renders.len(), 1);
    let resumed_bytes = &harness.terminal_output()[before_resume..];
    let predicted = resumed.completed_renders[0].predicted.clone();
    let batch = RenderBatch::new(
        vec![OutputTransaction::new(resumed_bytes)],
        predicted.clone(),
    );
    RenderOracle::new(geometry)
        .expect("resume oracle")
        .verify("mandatory-compositor-resume", &predicted, &batch)
        .expect("resume must reconstruct the authoritative scene");

    let before_shutdown = harness.terminal_output().len();
    harness
        .shutdown_physical_terminal()
        .expect("shutdown physical terminal");
    harness.tick(4).expect("reach shutdown boundary");
    harness
        .drain_scheduled_output(false)
        .expect("drain shutdown cleanup");
    assert!(harness.terminal_output().len() > before_shutdown);
    let after_shutdown = harness.terminal_output().len();
    harness
        .shutdown_physical_terminal()
        .expect("repeat shutdown");
    harness.tick(4).expect("reach repeated shutdown boundary");
    harness
        .drain_scheduled_output(false)
        .expect("drain repeated shutdown");
    assert_eq!(harness.terminal_output().len(), after_shutdown);
}

#[test]
fn shutdown_discards_unstarted_render_work_and_cleanup_is_last() {
    let mut harness = Harness::new_scheduled(3, 24).expect("live harness");
    harness.configure_physical_terminal(Some(false));
    harness
        .activate_physical_terminal()
        .expect("queue activation");
    harness
        .handle_pty_output(b"unstarted-secret-frame")
        .expect("queue unstarted frame");
    harness
        .shutdown_physical_terminal()
        .expect("queue shutdown cleanup");
    harness.tick(4).expect("reach shutdown boundary");
    harness
        .drain_scheduled_output(false)
        .expect("drain shutdown");

    let output = harness.terminal_output();
    assert!(
        !output
            .windows(b"unstarted-secret-frame".len())
            .any(|window| window == b"unstarted-secret-frame"),
        "an unstarted render was emitted during shutdown"
    );
    assert!(
        output.ends_with(b"\x1b[?1004l"),
        "terminal cleanup was not the final transaction: {output:?}"
    );
}

#[derive(Deserialize)]
struct Recording {
    name: String,
    rows: u16,
    cols: u16,
    scrollback: usize,
    operations: Vec<RecordingOperation>,
}

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum RecordingOperation {
    Write { chunks: Vec<String> },
    WriteHex { chunks: Vec<String> },
    Resize { rows: u16, cols: u16 },
    Reset,
}

fn decode_hex(value: &str) -> Vec<u8> {
    let compact = value
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    let (pairs, remainder) = compact.as_chunks::<2>();
    assert!(remainder.is_empty(), "fixture hex has an odd length");
    pairs
        .iter()
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).expect("ASCII hex"), 16)
                .expect("valid fixture hex")
        })
        .collect()
}

fn drain_live_boundary(harness: &mut Harness) {
    harness.tick(4).expect("advance scheduler boundary");
    loop {
        let report = harness
            .drain_scheduled_output(true)
            .expect("drain scheduled physical writes");
        if !report.blocked && !report.write_budget_exhausted {
            break;
        }
    }
}

#[test]
fn every_recording_reaches_the_physical_terminal_only_as_oracle_verified_writes() {
    let fixture_dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ghostty-recordings");
    let mut paths = fs::read_dir(&fixture_dir)
        .expect("read recording fixtures")
        .map(|entry| entry.expect("read fixture entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    paths.sort();
    assert_eq!(paths.len(), 12, "the complete recording corpus must run");

    for path in paths {
        let recording: Recording = serde_json::from_slice(
            &fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
        )
        .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
        let mut geometry = TerminalGeometry::from_cells(recording.rows, recording.cols);
        let mut source = GhosttyEngine::new_with_scrollback(
            recording.rows,
            recording.cols,
            recording.scrollback,
        )
        .expect("source terminal");
        let mut harness =
            Harness::new_scheduled(recording.rows, recording.cols).expect("live harness");
        let mut profile = PhysicalTerminalProfile::conservative(geometry);
        profile.hyperlinks = true;
        harness.set_physical_profile(profile);
        harness.configure_physical_terminal(Some(false));
        harness
            .activate_physical_terminal()
            .expect("activate physical terminal");
        drain_live_boundary(&mut harness);

        for operation in recording.operations {
            match operation {
                RecordingOperation::Write { chunks } => {
                    for chunk in chunks {
                        source
                            .advance(chunk.as_bytes())
                            .expect("advance text chunk");
                        harness
                            .handle_pty_output(chunk.as_bytes())
                            .expect("queue text chunk");
                        drain_live_boundary(&mut harness);
                    }
                }
                RecordingOperation::WriteHex { chunks } => {
                    for chunk in chunks {
                        let bytes = decode_hex(&chunk);
                        source.advance(&bytes).expect("advance hex chunk");
                        harness.handle_pty_output(&bytes).expect("queue hex chunk");
                        drain_live_boundary(&mut harness);
                    }
                }
                RecordingOperation::Resize { rows, cols } => {
                    geometry = TerminalGeometry::from_cells(rows, cols);
                    source
                        .resize_with_geometry(geometry)
                        .expect("resize source terminal");
                    harness
                        .resize_with_geometry(geometry)
                        .expect("resize live terminal");
                    drain_live_boundary(&mut harness);
                }
                RecordingOperation::Reset => {
                    source.reset().expect("reset source terminal");
                    harness
                        .handle_pty_output(b"\x1bc")
                        .expect("queue source reset");
                    drain_live_boundary(&mut harness);
                }
            }
        }

        let effects = b"\x1b]2;recording complete\x1b\\\x1b]7;file://host/tmp\x1b\\\x07";
        let bells = source
            .advance(effects)
            .expect("advance recording effects")
            .effects
            .bells;
        harness
            .handle_pty_output(effects)
            .expect("queue recording effects");
        drain_live_boundary(&mut harness);

        let mut snapshot = source.normalized_snapshot();
        snapshot.modes.focus_reporting = true;
        let mut scene = Scene::new(geometry);
        scene.effects.title.clone_from(&snapshot.title);
        scene
            .effects
            .working_directory
            .clone_from(&snapshot.working_directory);
        scene.effects.bell_count = bells;
        scene
            .panes
            .push(SceneSurface::new(ROOT, GridPoint::new(0, 0), snapshot));
        scene.cursor_owner = CursorOwner::Pane(ROOT);
        let intended = PresentedScene::compose(&scene).expect("compose recording scene");
        let transactions = harness
            .physical_writes()
            .iter()
            .map(OutputTransaction::new)
            .collect::<Vec<_>>();
        assert!(
            !transactions.is_empty(),
            "{} emitted no writes",
            recording.name
        );
        let batch = RenderBatch::new(transactions, intended.clone());
        RenderOracle::new(geometry)
            .expect("recording oracle")
            .verify(
                &format!("mandatory-recording-{}", recording.name),
                &intended,
                &batch,
            )
            .unwrap_or_else(|error| panic!("{}: {error}", recording.name));
    }
}

#[test]
fn compositor_scheduler_soak_preserves_state_effects_and_lifecycle_recovery() {
    const ITERATIONS: usize = 2_000;
    let mut geometry = TerminalGeometry::from_cells(8, 48);
    let mut source = GhosttyEngine::new_with_scrollback(geometry.rows, geometry.cols, 256)
        .expect("soak source terminal");
    let mut harness = Harness::new_scheduled(geometry.rows, geometry.cols).expect("soak harness");
    let mut profile = PhysicalTerminalProfile::conservative(geometry);
    profile.hyperlinks = true;
    harness.set_physical_profile(profile);
    harness.configure_physical_terminal(Some(false));
    harness
        .activate_physical_terminal()
        .expect("activate soak terminal");
    drain_live_boundary(&mut harness);

    let mut bells = 0usize;
    for iteration in 0..ITERATIONS {
        if iteration > 0 && iteration % 113 == 0 {
            geometry = if (iteration / 113) % 2 == 0 {
                TerminalGeometry::from_cells(8, 48)
            } else {
                TerminalGeometry::from_cells(6, 36)
            };
            source
                .resize_with_geometry(geometry)
                .expect("resize soak source");
            harness
                .resize_with_geometry(geometry)
                .expect("resize soak harness");
        }

        let row = iteration % usize::from(geometry.rows) + 1;
        let col = iteration % usize::from(geometry.cols.saturating_sub(8).max(1)) + 1;
        let mut update = format!(
            "\x1b[{row};{col}H\x1b[38;5;{}m{:06}\x1b[0m",
            iteration % 256,
            iteration
        )
        .into_bytes();
        if iteration % 97 == 0 {
            update.push(b'\x07');
        }
        if iteration % 131 == 0 {
            update.extend_from_slice(format!("\x1b]2;soak-{iteration}\x1b\\").as_bytes());
        }
        if iteration % 173 == 0 {
            update.extend_from_slice(b"\x1b]8;;https://example.test/soak\x1b\\link\x1b]8;;\x1b\\");
        }
        bells = bells.saturating_add(
            source
                .advance(&update)
                .expect("advance soak update")
                .effects
                .bells,
        );

        // Fragment inside the first CSI on every update. Multiple source
        // chunks are intentionally queued before one scheduler boundary.
        for chunk in [&update[..1], &update[1..3], &update[3..]] {
            harness
                .handle_pty_output(chunk)
                .expect("queue fragmented soak update");
        }

        if iteration % 149 == 0 {
            harness
                .handle_terminal_input(b"\x1br")
                .expect("open Review during soak");
            let hidden = format!("\x1b[1;1Hhidden-{iteration:06}");
            source
                .advance(hidden.as_bytes())
                .expect("advance hidden soak update");
            harness
                .handle_pty_output(hidden.as_bytes())
                .expect("queue hidden soak update");
            drain_live_boundary(&mut harness);
            harness
                .handle_terminal_input(b"q")
                .expect("close Review during soak");
        }

        drain_live_boundary(&mut harness);
        if iteration > 0 && iteration % 211 == 0 {
            harness
                .suspend_physical_terminal()
                .expect("suspend soak terminal");
            drain_live_boundary(&mut harness);
            harness
                .resume_physical_terminal()
                .expect("resume soak terminal");
            drain_live_boundary(&mut harness);
        }
    }

    // End at a stable geometry and force a final authoritative redraw so the
    // fixed-size oracle can replay the complete cross-resize write history.
    geometry = TerminalGeometry::from_cells(8, 48);
    source
        .resize_with_geometry(geometry)
        .expect("finalize soak source geometry");
    harness
        .resize_with_geometry(geometry)
        .expect("finalize soak harness geometry");
    drain_live_boundary(&mut harness);

    let mut snapshot = source.normalized_snapshot();
    snapshot.modes.focus_reporting = true;
    let mut scene = Scene::new(geometry);
    scene.effects.title.clone_from(&snapshot.title);
    scene
        .effects
        .working_directory
        .clone_from(&snapshot.working_directory);
    scene.effects.bell_count = bells;
    scene
        .panes
        .push(SceneSurface::new(ROOT, GridPoint::new(0, 0), snapshot));
    scene.cursor_owner = CursorOwner::Pane(ROOT);
    let intended = PresentedScene::compose(&scene).expect("compose soak scene");
    let batch = RenderBatch::new(
        harness
            .physical_writes()
            .iter()
            .map(OutputTransaction::new)
            .collect(),
        intended.clone(),
    );
    RenderOracle::new(geometry)
        .expect("soak oracle")
        .verify("mandatory-compositor-scheduler-soak", &intended, &batch)
        .expect("soak output must preserve the authoritative scene and effects");
}
