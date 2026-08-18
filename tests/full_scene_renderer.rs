use lector::{
    harness::Harness,
    presentation::{
        CursorOwner, FullSceneVtRenderer, GridPoint, OutputTransaction, PresentedScene,
        RenderBatch, RenderCapabilities, RenderOracle, RendererBackend, Scene, SceneDamage,
        SceneSurface, SurfaceId,
    },
    terminal::{CursorShape, GhosttyEngine, TerminalGeometry, UnderlineStyle},
};
use serde::Deserialize;
use std::{fs, path::Path};

const ROOT: SurfaceId = SurfaceId(1);

fn scene_from_bytes(geometry: TerminalGeometry, bytes: &[u8]) -> (Scene, PresentedScene) {
    let mut engine =
        GhosttyEngine::new(geometry.rows, geometry.cols).expect("create source engine");
    engine
        .resize_with_geometry(geometry)
        .expect("set source geometry");
    let update = engine.advance(bytes).expect("advance source engine");
    let snapshot = engine.normalized_snapshot();
    let mut scene = Scene::new(geometry);
    scene.effects.title.clone_from(&snapshot.title);
    scene
        .effects
        .working_directory
        .clone_from(&snapshot.working_directory);
    scene.effects.bell_count = update.effects.bells;
    scene
        .panes
        .push(SceneSurface::new(ROOT, GridPoint::new(0, 0), snapshot));
    scene.cursor_owner = CursorOwner::Pane(ROOT);
    let intended = PresentedScene::compose(&scene).expect("compose source scene");
    (scene, intended)
}

fn app_presented(mut snapshot: lector::terminal::TerminalSnapshot) -> PresentedScene {
    snapshot.modes.focus_reporting = true;
    let geometry = snapshot.geometry;
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
    PresentedScene::compose(&scene).expect("compose application presentation")
}

fn full_render(scene: &Scene, capabilities: RenderCapabilities) -> (RenderBatch, PresentedScene) {
    let previous = PresentedScene::blank(scene.geometry);
    let mut renderer = FullSceneVtRenderer::new(capabilities);
    let batch = renderer
        .render(scene, &SceneDamage::Full, &previous)
        .expect("render complete scene");
    let intended = PresentedScene::compose(scene).expect("compose intended scene");
    (batch, intended)
}

fn assert_oracle(case: &str, scene: &Scene, capabilities: RenderCapabilities) -> RenderBatch {
    let (batch, intended) = full_render(scene, capabilities);
    let mut oracle = RenderOracle::new(scene.geometry).expect("create render oracle");
    oracle
        .verify(case, &intended, &batch)
        .unwrap_or_else(|error| panic!("{case}: {error}"));
    batch
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
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

#[derive(Deserialize)]
struct CharacterizationFixture {
    name: String,
    rows: u16,
    cols: u16,
    operations: Vec<CharacterizationOperation>,
}

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum CharacterizationOperation {
    Process { hex: String },
    Resize { rows: u16, cols: u16 },
}

fn decode_hex(value: &str) -> Vec<u8> {
    let (pairs, remainder) = value.as_bytes().as_chunks::<2>();
    assert!(remainder.is_empty(), "fixture hex must have an even length");
    pairs
        .iter()
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).expect("ASCII hex"), 16)
                .expect("valid fixture hex")
        })
        .collect()
}

fn replay_recording(recording: &Recording, bytewise: bool) -> GhosttyEngine {
    let mut engine =
        GhosttyEngine::new_with_scrollback(recording.rows, recording.cols, recording.scrollback)
            .expect("create fixture engine");
    for operation in &recording.operations {
        match operation {
            RecordingOperation::Write { chunks } => {
                for chunk in chunks {
                    if bytewise {
                        for byte in chunk.as_bytes() {
                            engine.advance(&[*byte]).expect("advance split text byte");
                        }
                    } else {
                        engine
                            .advance(chunk.as_bytes())
                            .expect("advance text chunk");
                    }
                }
            }
            RecordingOperation::WriteHex { chunks } => {
                for chunk in chunks {
                    let chunk = decode_hex(chunk);
                    if bytewise {
                        for byte in chunk {
                            engine.advance(&[byte]).expect("advance split hex byte");
                        }
                    } else {
                        engine.advance(&chunk).expect("advance hex chunk");
                    }
                }
            }
            RecordingOperation::Resize { rows, cols } => {
                engine.resize(*rows, *cols).expect("resize fixture engine");
            }
            RecordingOperation::Reset => {
                engine.reset().expect("reset fixture engine");
            }
        }
    }
    engine
}

#[test]
fn full_renderer_reconstructs_every_sgr_style_color_underline_and_hyperlink() {
    let geometry = TerminalGeometry::from_cells(3, 12);
    let source = concat!(
        "\x1b[1;2;3;5;7;8;9;53;38;5;123;48;2;1;2;3;58;2;4;5;6mA",
        "\x1b[0;4:1mS\x1b[4:2mD\x1b[4:3mC\x1b[4:4mO\x1b[4:5mH",
        "\x1b[24;38;2;9;8;7;48;5;42mZ",
        "\x1b]8;;https://example.test/a?b=1\x1b\\link\x1b]8;;\x1b\\"
    );
    let (scene, intended) = scene_from_bytes(geometry, source.as_bytes());
    let snapshot = intended.clone().into_terminal_snapshot();
    let first = &snapshot.rows[0].cells[0].style;
    assert!(first.bold);
    assert!(first.dim);
    assert!(first.italic);
    assert!(first.blink);
    assert!(first.inverse);
    assert!(first.invisible);
    assert!(first.strikethrough);
    assert!(first.overline);
    assert_eq!(first.underline_color, lector::terminal::Color::Rgb(4, 5, 6));
    assert_eq!(
        snapshot.rows[0].cells[1].style.underline,
        UnderlineStyle::Single
    );
    assert_eq!(
        snapshot.rows[0].cells[2].style.underline,
        UnderlineStyle::Double
    );
    assert_eq!(
        snapshot.rows[0].cells[3].style.underline,
        UnderlineStyle::Curly
    );
    assert_eq!(
        snapshot.rows[0].cells[4].style.underline,
        UnderlineStyle::Dotted
    );
    assert_eq!(
        snapshot.rows[0].cells[5].style.underline,
        UnderlineStyle::Dashed
    );

    assert_oracle(
        "full-renderer-all-styles",
        &scene,
        RenderCapabilities::default(),
    );
}

#[test]
fn full_renderer_preserves_wide_combining_blank_and_wrap_boundaries() {
    let geometry = TerminalGeometry::from_cells(4, 6);
    let source = "ab界e\u{301}x\x1b[2;1H\x1b[44m  \x1b[0mZ\x1b[3;1H1234567";
    let (scene, intended) = scene_from_bytes(geometry, source.as_bytes());
    let snapshot = intended.clone().into_terminal_snapshot();
    assert!(snapshot.rows[2].wrapped);
    assert_eq!(snapshot.rows[0].cells[2].width, 2);
    assert!(snapshot.rows[0].cells[3].continuation);
    assert!(snapshot.rows[0].cells[4].grapheme.contains('\u{301}'));

    assert_oracle(
        "full-renderer-wide-combining-wrap",
        &scene,
        RenderCapabilities::default(),
    );
}

#[test]
fn full_renderer_preserves_a_wrap_marker_on_the_last_visible_row() {
    let geometry = TerminalGeometry::from_cells(2, 8);
    let (scene, intended) = scene_from_bytes(geometry, b"primary\x1b[?1049halt\r\nscreen\x1b[1;4H");
    assert!(
        intended
            .clone()
            .into_terminal_snapshot()
            .rows
            .last()
            .expect("last row")
            .wrapped
    );
    assert_oracle(
        "full-renderer-last-visible-wrap",
        &scene,
        RenderCapabilities::default(),
    );
}

#[test]
fn full_renderer_reconstructs_cursor_shapes_visibility_and_screen_selection() {
    let geometry = TerminalGeometry::from_cells(3, 8);
    for (name, control, expected) in [
        ("block", "\x1b[2 q", CursorShape::Block),
        ("underline", "\x1b[4 q", CursorShape::Underline),
        ("bar", "\x1b[6 q", CursorShape::Bar),
    ] {
        let bytes = format!("\x1b[?1049h{name}\x1b[2;3H{control}\x1b[?25l");
        let (scene, intended) = scene_from_bytes(geometry, bytes.as_bytes());
        let snapshot = intended.clone().into_terminal_snapshot();
        assert!(snapshot.alternate_screen());
        assert!(!snapshot.cursor.visible);
        assert_eq!(snapshot.cursor.shape, expected);
        assert_oracle(
            &format!("full-renderer-cursor-{name}"),
            &scene,
            RenderCapabilities::default(),
        );
    }
}

#[test]
fn full_renderer_keeps_child_screen_transitions_inside_the_owned_outer_alternate() {
    let geometry = TerminalGeometry::from_cells(3, 12);
    let mut renderer = FullSceneVtRenderer::new(RenderCapabilities::default());
    let mut previous = PresentedScene::blank(geometry);
    let mut physical = GhosttyEngine::new(geometry.rows, geometry.cols).expect("physical engine");
    physical
        .advance(b"\x1b[?1049h")
        .expect("enter Lector-owned alternate screen");

    for (name, source, expected) in [
        ("child-primary", b"shell".as_slice(), "shell"),
        ("child-alternate", b"\x1b[?1049heditor".as_slice(), "editor"),
        (
            "child-primary-restored",
            b"shell-returned".as_slice(),
            "shell-returned",
        ),
    ] {
        let (scene, _) = scene_from_bytes(geometry, source);
        let batch = renderer
            .render(&scene, &SceneDamage::Full, &previous)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let emitted = batch
            .transactions
            .iter()
            .flat_map(|transaction| transaction.bytes.iter().copied())
            .collect::<Vec<_>>();
        assert!(
            !contains(&emitted, b"\x1b[?1049h") && !contains(&emitted, b"\x1b[?1049l"),
            "{name}: renderer changed the physical screen: {emitted:?}"
        );
        physical
            .advance(&emitted)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let snapshot = physical.normalized_snapshot();
        assert!(
            snapshot.alternate_screen(),
            "{name}: renderer escaped Lector's owned alternate screen"
        );
        assert!(snapshot.contents().contains(expected), "{name}");
        previous = batch.predicted;
    }
}

#[test]
fn full_renderer_clears_prior_cells_images_and_unknown_physical_state() {
    let geometry = TerminalGeometry::new(3, 8, 10, 20);
    let image = b"\x1b_Ga=T,f=32,s=1,v=1,i=7,p=9,c=1,r=1,q=2;/wAA/w==\x1b\\";
    let corrupt = [
        b"junk\x1b[31;44m\x1b[?6h\x1b[4h\x1b[?7l\x1b[2;3r\x1b=\x1b[?1h\x1b[?2004h\x1b[?1004h\x1b[?1003h\x1b[?1006h\x1b[=5u\x1b[?2026h".as_slice(),
        image.as_slice(),
    ]
    .concat();
    let corrupt_presented = {
        let mut engine = GhosttyEngine::new(geometry.rows, geometry.cols).expect("corrupt engine");
        engine
            .resize_with_geometry(geometry)
            .expect("set corrupt geometry");
        engine.advance(&corrupt).expect("construct corrupt state");
        PresentedScene::from_engine(&engine).expect("capture corrupt state")
    };
    let mut oracle = RenderOracle::new(geometry).expect("create oracle");
    oracle
        .verify(
            "full-renderer-corrupt-primer",
            &corrupt_presented,
            &RenderBatch::new(
                vec![OutputTransaction::new(&corrupt)],
                corrupt_presented.clone(),
            ),
        )
        .expect("prime corrupt physical state");

    let (scene, intended) = scene_from_bytes(
        geometry,
        b"\x1b]2;clean title\x07\x1b]7;file://localhost/tmp/clean\x1b\\clean",
    );
    let mut renderer = FullSceneVtRenderer::new(RenderCapabilities {
        kitty_graphics: true,
        ..RenderCapabilities::default()
    });
    let batch = renderer
        .render(&scene, &SceneDamage::Full, &corrupt_presented)
        .expect("render over corrupt state");
    let bytes = &batch.transactions[0].bytes;
    for reset in [
        b"\x1b[?6l".as_slice(),
        b"\x1b[?7h".as_slice(),
        b"\x1b[4l".as_slice(),
        b"\x1b[r".as_slice(),
        b"\x1b[?69l".as_slice(),
        b"\x1b[0m".as_slice(),
        b"\x1b]8;;\x1b\\".as_slice(),
        b"\x1b_Ga=d,d=A\x1b\\".as_slice(),
    ] {
        assert!(contains(bytes, reset), "missing reset {reset:?}");
    }
    oracle
        .verify("full-renderer-clears-corruption", &intended, &batch)
        .expect("full renderer must replace all observable state");

    let blank_scene = Scene::new(geometry);
    let cleared = renderer
        .render(&blank_scene, &SceneDamage::Full, &intended)
        .expect("clear terminal-wide strings");
    let blank = PresentedScene::compose(&blank_scene).expect("compose blank scene");
    oracle
        .verify("full-renderer-clears-title-and-pwd", &blank, &cleared)
        .expect("unset strings must replace prior values");
}

#[test]
fn synchronized_output_is_structural_only_when_capability_reports_support() {
    let geometry = TerminalGeometry::from_cells(2, 8);
    let (scene, _) = scene_from_bytes(geometry, b"synchronized");

    let synchronized = assert_oracle(
        "full-renderer-synchronized",
        &scene,
        RenderCapabilities {
            synchronized_output: true,
            ..RenderCapabilities::default()
        },
    );
    let synchronized_bytes = &synchronized.transactions[0].bytes;
    assert!(synchronized_bytes.starts_with(b"\x1b[?2026h"));
    assert!(synchronized_bytes.ends_with(b"\x1b[?2026l"));

    let fallback = assert_oracle(
        "full-renderer-no-synchronized-capability",
        &scene,
        RenderCapabilities {
            synchronized_output: false,
            ..RenderCapabilities::default()
        },
    );
    assert!(!contains(&fallback.transactions[0].bytes, b"\x1b[?2026h"));
}

#[test]
fn full_renderer_leaves_every_mode_in_the_modeled_state() {
    let geometry = TerminalGeometry::from_cells(2, 8);
    let (scene, intended) = scene_from_bytes(
        geometry,
        b"\x1b=\x1b[?1h\x1b[?2004h\x1b[?1004h\x1b[?1003h\x1b[?1006h\x1b[=5u\x1b[?2026hmode",
    );
    let snapshot = intended.clone().into_terminal_snapshot();
    assert!(snapshot.modes.application_keypad);
    assert!(snapshot.modes.application_cursor);
    assert!(snapshot.modes.bracketed_paste);
    assert!(snapshot.modes.focus_reporting);
    assert!(snapshot.modes.synchronized_output);
    assert_eq!(snapshot.modes.kitty_keyboard_flags, 5);
    assert_eq!(
        snapshot.modes.mouse_protocol,
        lector::terminal::MouseProtocol::AnyMotion
    );
    assert_eq!(
        snapshot.modes.mouse_encoding,
        lector::terminal::MouseEncoding::Sgr
    );

    let batch = assert_oracle(
        "full-renderer-all-modes",
        &scene,
        RenderCapabilities {
            synchronized_output: true,
            ..RenderCapabilities::default()
        },
    );
    assert!(batch.transactions[0].bytes.starts_with(b"\x1b[?2026h"));
    assert!(!batch.transactions[0].bytes.ends_with(b"\x1b[?2026l"));
}

#[test]
fn application_harness_renders_only_from_terminal_state() {
    let source = b"visible\x1b]777;raw-source-marker\x1b\\\x1b[2;3Hdone";

    let mut rendered = Harness::new(3, 12).expect("create compositor harness");
    rendered
        .handle_pty_output(source)
        .expect("full-rendered output");
    assert!(!contains(rendered.terminal_output(), b"raw-source-marker"));

    let (_, source_intended) = scene_from_bytes(TerminalGeometry::from_cells(3, 12), source);
    let intended = app_presented(source_intended.into_terminal_snapshot());
    let batch = RenderBatch::new(
        vec![OutputTransaction::new(rendered.terminal_output())],
        intended.clone(),
    );
    let mut oracle =
        RenderOracle::new(TerminalGeometry::from_cells(3, 12)).expect("create harness oracle");
    oracle
        .verify("full-renderer-opt-in-harness", &intended, &batch)
        .expect("harness output matches modeled state");
}

#[test]
fn application_compositor_emits_modeled_bells_once() {
    let geometry = TerminalGeometry::from_cells(2, 8);
    let mut harness = Harness::new(geometry.rows, geometry.cols).expect("create bell harness");
    harness
        .handle_pty_output(b"bell\x07")
        .expect("render bell update");
    assert_eq!(
        harness
            .terminal_output()
            .iter()
            .filter(|byte| **byte == b'\x07')
            .count(),
        1
    );

    let mut source = GhosttyEngine::new(geometry.rows, geometry.cols).expect("bell source");
    source.advance(b"bell\x07").expect("advance bell source");
    let mut snapshot = source.normalized_snapshot();
    snapshot.modes.focus_reporting = true;
    let mut scene = Scene::new(geometry);
    scene
        .panes
        .push(SceneSurface::new(ROOT, GridPoint::new(0, 0), snapshot));
    scene.cursor_owner = CursorOwner::Pane(ROOT);
    scene.effects.bell_count = 1;
    let intended = PresentedScene::compose(&scene).expect("compose bell presentation");
    let batch = RenderBatch::new(
        vec![OutputTransaction::new(harness.terminal_output())],
        intended.clone(),
    );
    let mut oracle = RenderOracle::new(geometry).expect("create bell oracle");
    oracle
        .verify("full-renderer-application-bell", &intended, &batch)
        .expect("application bell rendered once");
}

#[test]
fn application_compositor_is_invariant_at_every_source_byte_boundary() {
    let geometry = TerminalGeometry::from_cells(3, 14);
    let source =
        b"start\xe7\x95\x8ce\xcc\x81\x1b[31;4:3mstyled\x1b[0m\x1b]2;split title\x1b\\\x1b[2;4Hdone";
    let (_, source_intended) = scene_from_bytes(geometry, source);
    let intended = app_presented(source_intended.into_terminal_snapshot());

    for split in 0..=source.len() {
        let mut harness = Harness::new(geometry.rows, geometry.cols).expect("create split harness");
        harness
            .handle_pty_output(&source[..split])
            .unwrap_or_else(|error| panic!("first source fragment {split}: {error}"));
        harness
            .handle_pty_output(&source[split..])
            .unwrap_or_else(|error| panic!("second source fragment {split}: {error}"));
        let batch = RenderBatch::new(
            vec![OutputTransaction::new(harness.terminal_output())],
            intended.clone(),
        );
        let mut oracle = RenderOracle::new(geometry).expect("create split oracle");
        oracle
            .verify(
                &format!("full-renderer-app-split-{split}"),
                &intended,
                &batch,
            )
            .unwrap_or_else(|error| panic!("source split {split}: {error}"));
    }
}

#[test]
fn compositor_harness_restores_hidden_output_across_overlay_and_resize() {
    let script = include_str!("scripts/full_scene_renderer.txt");
    let mut harness = Harness::new(4, 12).expect("create compositor harness");
    harness
        .run_script(script)
        .expect("run full renderer script");

    let initial_geometry = TerminalGeometry::from_cells(4, 12);
    let geometry = TerminalGeometry::new(3, 10, 9, 18);
    let mut source =
        GhosttyEngine::new(initial_geometry.rows, initial_geometry.cols).expect("source engine");
    source.advance(b"before").expect("initial source output");
    source
        .advance(b"\x1b[2;1Hhidden wide \xe7\x95\x8c\x1b[?1h\x1b[?2004h")
        .expect("hidden source output");
    source
        .resize_with_geometry(geometry)
        .expect("resize source model");
    let mut snapshot = source.normalized_snapshot();
    snapshot.modes.focus_reporting = true;
    let mut scene = Scene::new(geometry);
    scene
        .panes
        .push(SceneSurface::new(ROOT, GridPoint::new(0, 0), snapshot));
    scene.cursor_owner = CursorOwner::Pane(ROOT);
    let intended = PresentedScene::compose(&scene).expect("compose resized source");
    let batch = RenderBatch::new(
        vec![OutputTransaction::with_resize(
            geometry,
            harness.terminal_output(),
        )],
        intended.clone(),
    );
    let mut oracle = RenderOracle::new(initial_geometry).expect("create transition oracle");
    oracle
        .verify("full-renderer-overlay-resize", &intended, &batch)
        .expect("overlay lifecycle output restores resized root");
}

#[test]
fn every_recorded_source_fixture_renders_identically_after_every_input_byte_split() {
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
    assert_eq!(
        paths.len(),
        12,
        "the complete source corpus must be rendered"
    );

    for path in paths {
        let recording: Recording = serde_json::from_slice(
            &fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
        )
        .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
        let recorded = replay_recording(&recording, false);
        let split = replay_recording(&recording, true);
        assert_eq!(
            split.normalized_snapshot(),
            recorded.normalized_snapshot(),
            "{} changed when every source sequence was fragmented",
            recording.name
        );

        let snapshot = split.normalized_snapshot();
        let geometry = snapshot.geometry;
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
        assert_oracle(
            &format!("full-renderer-recording-{}", recording.name),
            &scene,
            RenderCapabilities::default(),
        );
    }
}

#[test]
fn every_terminal_characterization_fixture_renders_through_the_oracle() {
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/terminal");
    let mut paths = fs::read_dir(&fixture_dir)
        .expect("read terminal fixtures")
        .map(|entry| entry.expect("read terminal fixture entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    paths.sort();
    assert_eq!(
        paths.len(),
        3,
        "the full characterization corpus must render"
    );

    for path in paths {
        let fixture: CharacterizationFixture = serde_json::from_slice(
            &fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
        )
        .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
        let mut engine = GhosttyEngine::new(fixture.rows, fixture.cols).expect("fixture engine");
        for operation in fixture.operations {
            match operation {
                CharacterizationOperation::Process { hex } => {
                    for byte in decode_hex(&hex) {
                        engine
                            .advance(&[byte])
                            .expect("advance characterization byte");
                    }
                }
                CharacterizationOperation::Resize { rows, cols } => {
                    engine
                        .resize(rows, cols)
                        .expect("resize characterization fixture");
                }
            }
        }
        let snapshot = engine.normalized_snapshot();
        let geometry = snapshot.geometry;
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
        assert_oracle(
            &format!("full-renderer-characterization-{}", fixture.name),
            &scene,
            RenderCapabilities::default(),
        );
    }
}
