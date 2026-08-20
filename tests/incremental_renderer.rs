use lector::{
    app::App,
    harness::Harness,
    presentation::{
        CursorOwner, FullSceneVtRenderer, GridPoint, GridRect, IncrementalVtRenderer,
        PresentedScene, RenderCapabilities, RenderOracle, RenderStrategy, RendererBackend, Scene,
        SceneDamage, SceneImagePlacement, SceneOverlay, SceneSurface, SurfaceId,
    },
    screen_reader::ScreenReader,
    speech,
    terminal::{GhosttyEngine, TerminalDamage, TerminalGeometry, UpdateSummary},
    views,
};
use std::io::{self, Write};

const ROOT: SurfaceId = SurfaceId(1);
const OVERLAY: SurfaceId = SurfaceId(2);

fn scene_for(engine: &GhosttyEngine) -> Scene {
    let snapshot = engine.normalized_snapshot();
    let mut scene = Scene::new(snapshot.geometry);
    scene.effects.title.clone_from(&snapshot.title);
    scene
        .effects
        .working_directory
        .clone_from(&snapshot.working_directory);
    scene
        .panes
        .push(SceneSurface::new(ROOT, GridPoint::new(0, 0), snapshot));
    scene.cursor_owner = CursorOwner::Pane(ROOT);
    scene
}

fn byte_len(batch: &lector::presentation::RenderBatch) -> usize {
    batch
        .transactions
        .iter()
        .map(|transaction| transaction.bytes.len())
        .sum()
}

fn full_byte_len(scene: &Scene, previous: &PresentedScene) -> usize {
    let mut full = FullSceneVtRenderer::new(RenderCapabilities::default());
    byte_len(
        &full
            .render(scene, &SceneDamage::Full, previous)
            .expect("render full comparison"),
    )
}

struct IncrementalSession {
    source: GhosttyEngine,
    renderer: IncrementalVtRenderer,
    oracle: RenderOracle,
    presented: PresentedScene,
}

impl IncrementalSession {
    fn new(geometry: TerminalGeometry, initial: &[u8]) -> Self {
        let mut source = GhosttyEngine::new(geometry.rows, geometry.cols).expect("source engine");
        source
            .resize_with_geometry(geometry)
            .expect("set source geometry");
        source.advance(initial).expect("prime source state");
        let scene = scene_for(&source);
        let intended = PresentedScene::compose(&scene).expect("compose initial scene");
        let mut renderer = IncrementalVtRenderer::new(RenderCapabilities::default());
        let blank = PresentedScene::blank(geometry);
        let batch = renderer
            .render(&scene, &SceneDamage::Full, &blank)
            .expect("initial full fallback");
        assert_eq!(renderer.last_strategy(), RenderStrategy::FullFallback);
        let mut oracle = RenderOracle::new(geometry).expect("render oracle");
        oracle
            .verify("incremental-initial", &intended, &batch)
            .expect("verify initial presentation");
        renderer.confirm(&batch.predicted);
        Self {
            source,
            renderer,
            oracle,
            presented: batch.predicted,
        }
    }

    fn update(&mut self, name: &str, bytes: &[u8]) -> (usize, usize) {
        let update = self.source.advance(bytes).expect("advance source");
        let scene = scene_for(&self.source);
        let damage =
            SceneDamage::from_terminal_damage(&scene.panes[0], &update.damage, scene.geometry);
        let full_bytes = full_byte_len(&scene, &self.presented);
        let batch = self
            .renderer
            .render(&scene, &damage, &self.presented)
            .expect("incremental render");
        let intended = PresentedScene::compose(&scene).expect("compose updated scene");
        self.oracle
            .verify(name, &intended, &batch)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let incremental_bytes = byte_len(&batch);
        self.renderer.confirm(&batch.predicted);
        self.presented = batch.predicted;
        (incremental_bytes, full_bytes)
    }

    fn update_with_operations(&mut self, name: &str, bytes: &[u8]) -> (usize, usize) {
        let update = self.source.advance(bytes).expect("advance source");
        let scene = scene_for(&self.source);
        let damage = SceneDamage::from_terminal_update(&scene.panes[0], &update, scene.geometry);
        let full_bytes = full_byte_len(&scene, &self.presented);
        let batch = self
            .renderer
            .render(&scene, &damage, &self.presented)
            .expect("incremental render");
        let intended = PresentedScene::compose(&scene).expect("compose updated scene");
        self.oracle
            .verify(name, &intended, &batch)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let incremental_bytes = byte_len(&batch);
        self.renderer.confirm(&batch.predicted);
        self.presented = batch.predicted;
        (incremental_bytes, full_bytes)
    }
}

#[test]
fn pane_local_row_damage_maps_to_clipped_scene_coordinates() {
    let engine = GhosttyEngine::new(4, 8).expect("create pane engine");
    let snapshot = engine.normalized_snapshot();
    let damage = TerminalDamage::Rows(std::iter::once(1..=3).collect());

    let offset = SceneSurface::new(ROOT, GridPoint::new(2, 5), snapshot.clone());
    assert_eq!(
        SceneDamage::from_terminal_damage(&offset, &damage, TerminalGeometry::from_cells(5, 10),),
        SceneDamage::regions([GridRect::new(GridPoint::new(3, 5), 2, 5)])
    );

    let clipped = SceneSurface::new(ROOT, GridPoint::new(-2, -3), snapshot);
    assert_eq!(
        SceneDamage::from_terminal_damage(
            &clipped,
            &TerminalDamage::Rows(std::iter::once(0..=3).collect()),
            TerminalGeometry::from_cells(5, 10),
        ),
        SceneDamage::regions([GridRect::new(GridPoint::new(0, 0), 2, 5)])
    );
}

#[test]
fn multi_pane_damage_preserves_each_surface_region() {
    let geometry = TerminalGeometry::from_cells(2, 8);
    let left_engine = GhosttyEngine::new(2, 4).expect("create left pane");
    let right_engine = GhosttyEngine::new(2, 4).expect("create right pane");
    let left = SceneSurface::new(
        ROOT,
        GridPoint::new(0, 0),
        left_engine.normalized_snapshot(),
    );
    let right = SceneSurface::new(
        OVERLAY,
        GridPoint::new(0, 4),
        right_engine.normalized_snapshot(),
    );
    let left_update = UpdateSummary {
        damage: TerminalDamage::Rows(std::iter::once(0..=0).collect()),
        ..UpdateSummary::default()
    };
    let right_update = UpdateSummary {
        damage: TerminalDamage::Rows(std::iter::once(1..=1).collect()),
        ..UpdateSummary::default()
    };

    assert_eq!(
        SceneDamage::from_terminal_updates(
            [(&left, &left_update), (&right, &right_update)],
            geometry,
        ),
        SceneDamage::regions([
            GridRect::new(GridPoint::new(0, 0), 1, 4),
            GridRect::new(GridPoint::new(1, 4), 1, 4),
        ])
    );
}

#[test]
fn ghostty_dirty_state_distinguishes_partial_rows_from_full_transitions() {
    let mut engine = GhosttyEngine::new(4, 20).expect("create engine");
    let partial = engine.advance(b"small").expect("write one row");
    assert_eq!(
        partial.damage,
        TerminalDamage::Rows(std::iter::once(0..=0).collect())
    );

    let cursor = engine.advance(b"\x1b[3;4H").expect("move cursor");
    assert!(matches!(cursor.damage, TerminalDamage::Rows(_)));
    assert!(cursor.changed_rows.iter().all(|range| *range.end() < 4));

    let full = engine
        .advance(b"\x1b[?1049h")
        .expect("switch terminal screen");
    assert_eq!(full.damage, TerminalDamage::Full);
    assert_eq!(
        full.changed_rows,
        std::iter::once(0..=3).collect::<Vec<_>>()
    );
}

#[test]
fn ghostty_dirty_flags_are_acknowledged_after_each_adapter_snapshot() {
    let mut engine = GhosttyEngine::new(5, 20).expect("create engine");
    let first = engine.advance(b"row zero").expect("dirty first row");
    assert_eq!(
        first.changed_rows,
        std::iter::once(0..=0).collect::<Vec<_>>()
    );

    let clean = engine.advance(b"").expect("observe clean frame");
    assert_eq!(clean.damage, TerminalDamage::None);
    assert!(clean.changed_rows.is_empty());

    let later = engine
        .advance(b"\x1b[4;1Hrow three")
        .expect("dirty a later row");
    assert_eq!(later.changed_rows, vec![0..=0, 3..=3]);

    let same_row = engine.advance(b"!").expect("continue on later row");
    assert_eq!(
        same_row.changed_rows,
        std::iter::once(3..=3).collect::<Vec<_>>()
    );
}

#[test]
fn small_edits_cursor_moves_and_line_replacements_have_bounded_output_and_work() {
    let geometry = TerminalGeometry::from_cells(10, 80);
    let mut session = IncrementalSession::new(
        geometry,
        b"prompt$ command\r\nbody\x1b[10;1Hstatus: idle\x1b[2;5H",
    );

    let (edit_bytes, edit_full) = session.update("incremental-small-edit", b"X");
    assert_eq!(
        session.renderer.last_strategy(),
        RenderStrategy::Incremental
    );
    assert!(edit_bytes < 96, "small edit emitted {edit_bytes} bytes");
    assert!(edit_bytes * 4 < edit_full);
    let stats = session.renderer.last_stats();
    assert!(stats.rows_considered <= 2, "{stats:?}");
    assert!(stats.cells_compared <= 160, "{stats:?}");
    assert!(stats.cells_emitted <= 4, "{stats:?}");

    let (cursor_bytes, cursor_full) = session.update("incremental-cursor-only", b"\x1b[4;17H");
    assert!(
        cursor_bytes < 48,
        "cursor move emitted {cursor_bytes} bytes"
    );
    assert!(cursor_bytes * 8 < cursor_full);
    assert_eq!(session.renderer.last_stats().cells_emitted, 0);

    let (line_bytes, line_full) = session.update(
        "incremental-status-line",
        b"\x1b[10;1Hstatus: running 42%\x1b[K\x1b[4;17H",
    );
    assert!(
        line_bytes < 180,
        "line replacement emitted {line_bytes} bytes"
    );
    assert!(line_bytes * 3 < line_full);
    assert!(session.renderer.last_stats().rows_considered <= 2);
}

#[test]
fn scrolling_and_overlay_damage_match_the_full_redraw_oracle() {
    let geometry = TerminalGeometry::from_cells(6, 24);
    let mut session =
        IncrementalSession::new(geometry, b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix");
    let (scroll_bytes, scroll_full) =
        session.update("incremental-scrolling-output", b"\r\nseven\r\neight");
    assert!(
        scroll_bytes <= scroll_full,
        "{scroll_bytes} > {scroll_full}"
    );

    let mut overlay_engine = GhosttyEngine::new(2, 10).expect("overlay engine");
    overlay_engine
        .advance(b"NOTICE\r\nready\x1b[2;6H")
        .expect("build overlay");
    let overlay_surface = SceneSurface::new(
        OVERLAY,
        GridPoint::new(2, 7),
        overlay_engine.normalized_snapshot(),
    );
    let mut overlay_scene = scene_for(&session.source);
    overlay_scene
        .overlays
        .push(SceneOverlay::new(overlay_surface, 10));
    overlay_scene.cursor_owner = CursorOwner::Overlay(OVERLAY);
    let overlay_damage = SceneDamage::regions([GridRect::new(GridPoint::new(2, 7), 2, 10)]);
    let overlay_batch = session
        .renderer
        .render(&overlay_scene, &overlay_damage, &session.presented)
        .expect("render overlay damage");
    let overlay_intended = PresentedScene::compose(&overlay_scene).expect("compose overlay");
    session
        .oracle
        .verify(
            "incremental-overlay-open",
            &overlay_intended,
            &overlay_batch,
        )
        .expect("verify overlay open");
    assert_eq!(
        session.renderer.last_strategy(),
        RenderStrategy::Incremental
    );
    assert!(session.renderer.last_stats().rows_considered <= 2);
    session.renderer.confirm(&overlay_batch.predicted);

    let root_scene = scene_for(&session.source);
    let close_batch = session
        .renderer
        .render(&root_scene, &overlay_damage, &overlay_batch.predicted)
        .expect("render exposed root region");
    let root_intended = PresentedScene::compose(&root_scene).expect("compose exposed root");
    session
        .oracle
        .verify("incremental-overlay-close", &root_intended, &close_batch)
        .expect("verify overlay close");
    assert!(byte_len(&close_batch) < full_byte_len(&root_scene, &overlay_batch.predicted));
}

#[test]
fn scrolling_preserves_wrapped_rows_without_a_full_reconstruction() {
    let geometry = TerminalGeometry::from_cells(6, 24);
    let mut session = IncrementalSession::new(
        geometry,
        b"header\r\na line long enough to wrap across the viewport\r\nthree\r\nfour\r\nfive",
    );

    let (scroll_bytes, scroll_full) =
        session.update_with_operations("wrapped-scrolling-output", b"\r\nsix\r\n");

    assert_eq!(
        session.renderer.last_strategy(),
        RenderStrategy::SemanticFastPath
    );
    assert!(
        scroll_bytes * 4 < scroll_full,
        "wrapped scroll emitted {scroll_bytes} bytes versus {scroll_full} for a full reconstruction"
    );
    assert!(
        session.renderer.last_stats().cells_compared
            < usize::from(geometry.rows) * usize::from(geometry.cols),
        "wrapped scroll validation must remain narrower than the full viewport"
    );
}

#[test]
fn wide_and_combining_damage_expands_to_safe_grapheme_boundaries() {
    let geometry = TerminalGeometry::from_cells(3, 16);
    let mut session = IncrementalSession::new(geometry, "A界e\u{301}Z".as_bytes());

    let (bytes, full) = session.update(
        "incremental-wide-combining-replacement",
        "\x1b[1;2HxyQ\u{308}".as_bytes(),
    );
    assert!(bytes < full);
    let stats = session.renderer.last_stats();
    assert!(stats.cells_emitted >= 3, "{stats:?}");
    assert!(stats.cells_emitted <= 6, "{stats:?}");

    let snapshot = session.presented.clone().into_terminal_snapshot();
    assert_eq!(snapshot.rows[0].cells[1].grapheme, "x");
    assert_eq!(snapshot.rows[0].cells[2].grapheme, "y");
    assert!(snapshot.rows[0].cells[3].grapheme.contains('\u{308}'));
}

#[test]
fn uncertainty_resize_and_inconsistent_confirmation_force_full_fallbacks() {
    let geometry = TerminalGeometry::from_cells(4, 20);
    let mut session = IncrementalSession::new(geometry, b"known state");
    session.renderer.invalidate();
    let (invalidated, _) = session.update("incremental-invalidated-fallback", b"!");
    assert_eq!(
        session.renderer.last_strategy(),
        RenderStrategy::FullFallback
    );
    assert!(invalidated > 100);

    let wrong_shadow = PresentedScene::blank(geometry);
    let scene = scene_for(&session.source);
    let batch = session
        .renderer
        .render(
            &scene,
            &SceneDamage::regions([GridRect::new(GridPoint::new(0, 0), 1, 20)]),
            &wrong_shadow,
        )
        .expect("fallback from inconsistent confirmation");
    assert_eq!(
        session.renderer.last_strategy(),
        RenderStrategy::FullFallback
    );
    assert!(byte_len(&batch) > 100);

    let next = TerminalGeometry::from_cells(5, 25);
    let mut resized = scene;
    resized.geometry = next;
    resized.panes[0].snapshot.geometry = next;
    let resized_batch = session
        .renderer
        .render(
            &resized,
            &SceneDamage::Resize {
                previous: geometry,
                next,
            },
            &session.presented,
        )
        .expect("resize full fallback");
    assert_eq!(
        session.renderer.last_strategy(),
        RenderStrategy::FullFallback
    );
    assert_eq!(resized_batch.transactions[0].resize, Some(next));
}

#[test]
fn deterministic_update_property_matches_incremental_and_full_renderers() {
    let geometry = TerminalGeometry::from_cells(8, 32);
    let mut session = IncrementalSession::new(geometry, b"property baseline");
    let mut state = 0x5eed_cafe_u64;
    for step in 0..96 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let row = (state % u64::from(geometry.rows)) as u16 + 1;
        let col = ((state >> 8) % u64::from(geometry.cols - 4)) as u16 + 1;
        let letter = b'a' + ((state >> 16) % 26) as u8;
        let update = format!("\x1b[{row};{col}H{}{}", char::from(letter), step % 10);
        let (incremental, full) =
            session.update(&format!("incremental-property-{step}"), update.as_bytes());
        assert!(incremental < full, "step {step}: {incremental} >= {full}");
        assert!(session.renderer.last_stats().cells_compared <= 64);
    }
}

#[test]
fn full_scene_application_path_uses_incremental_damage_after_initial_confirmation() {
    let mut harness = Harness::new(8, 40).expect("compositor harness");
    harness
        .handle_pty_output(b"initial screen\x1b[3;4H")
        .expect("initial full presentation");
    let initial_len = harness.terminal_output().len();
    assert!(initial_len > 300);

    harness
        .handle_pty_output(b"X")
        .expect("incremental application update");
    let update = &harness.terminal_output()[initial_len..];
    assert!(
        update.len() < 96,
        "live update emitted {} bytes",
        update.len()
    );
    assert!(!update.windows(4).any(|window| window == b"\x1b[2J"));
    assert!(
        !update
            .windows(b"\x1b_Ga=d,d=A\x1b\\".len())
            .any(|window| window == b"\x1b_Ga=d,d=A\x1b\\")
    );
}

struct SilentDriver;

impl speech::Driver for SilentDriver {
    fn speak(&mut self, _text: &str, _interrupt: bool) -> anyhow::Result<()> {
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

struct PartialWriter {
    remaining: usize,
}

impl Write for PartialWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "injected failure",
            ));
        }
        let written = bytes.len().min(self.remaining);
        self.remaining -= written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn partial_physical_write_and_renderer_exception_invalidate_incremental_state() {
    let speech = speech::Speech::new(Box::new(SilentDriver));
    let mut reader = ScreenReader::new(speech);
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(6, 30)));
    let mut app = App::new(stack).expect("create application");
    let mut initial = Vec::new();
    app.handle_pty(&mut reader, b"initial\x1b[2;3H", &mut initial)
        .expect("confirm initial physical state");

    let mut failing = PartialWriter { remaining: 8 };
    assert!(app.handle_pty(&mut reader, b"X", &mut failing).is_err());
    let mut recovered = Vec::new();
    app.handle_pty(&mut reader, b"Y", &mut recovered)
        .expect("recover with full redraw");
    assert!(recovered.windows(4).any(|window| window == b"\x1b[2J"));
    assert!(recovered.len() > 200);

    let geometry = TerminalGeometry::from_cells(3, 12);
    let mut source = GhosttyEngine::new(geometry.rows, geometry.cols).expect("source engine");
    source.advance(b"normal").expect("source state");
    let scene = scene_for(&source);
    let presented = PresentedScene::compose(&scene).expect("compose source");
    let mut renderer = IncrementalVtRenderer::new(RenderCapabilities {
        kitty_graphics: true,
        ..RenderCapabilities::default()
    });
    renderer.confirm(&presented);
    let mut image_scene = scene.clone();
    let mut missing_upload = SceneImagePlacement::default();
    missing_upload.image.image_id = 1;
    missing_upload.image.visible = true;
    missing_upload.image.grid_rect = GridRect::new(GridPoint::new(0, 0), 1, 1);
    image_scene.images.push(missing_upload);
    assert!(
        renderer
            .render(&image_scene, &SceneDamage::Full, &presented)
            .is_err()
    );
    let recovered = renderer
        .render(
            &scene,
            &SceneDamage::regions([GridRect::new(GridPoint::new(0, 0), 1, 12)]),
            &presented,
        )
        .expect("fallback after renderer exception");
    assert_eq!(renderer.last_strategy(), RenderStrategy::FullFallback);
    assert!(byte_len(&recovered) > 100);
}

#[test]
fn incremental_transactions_track_sgr_hyperlinks_modes_effects_and_sync_wrapper() {
    let geometry = TerminalGeometry::from_cells(4, 24);
    let mut source = GhosttyEngine::new(geometry.rows, geometry.cols).expect("source engine");
    source.advance(b"base\x1b[2;2H").expect("initial state");
    let initial_scene = scene_for(&source);
    let initial_intended = PresentedScene::compose(&initial_scene).expect("compose initial");
    let capabilities = RenderCapabilities {
        synchronized_output: true,
        ..RenderCapabilities::default()
    };
    let mut renderer = IncrementalVtRenderer::new(capabilities);
    let blank = PresentedScene::blank(geometry);
    let initial_batch = renderer
        .render(&initial_scene, &SceneDamage::Full, &blank)
        .expect("initial full render");
    let mut oracle = RenderOracle::new(geometry).expect("render oracle");
    oracle
        .verify(
            "incremental-state-initial",
            &initial_intended,
            &initial_batch,
        )
        .expect("verify initial");
    renderer.confirm(&initial_batch.predicted);

    let update = source
        .advance(
            b"\x1b]2;incremental title\x07\x1b]7;file://localhost/tmp\x1b\\\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[=5u\x1b[?25l\x1b[6 q\x1b[2;2H\x1b]8;;https://example.test\x1b\\\x1b[1;31mLINK\x1b]8;;\x1b\\\x1b[0m\x07",
        )
        .expect("styled update");
    let mut scene = scene_for(&source);
    scene.effects.bell_count = update.effects.bells;
    let intended = PresentedScene::compose(&scene).expect("compose styled state");
    let batch = renderer
        .render(
            &scene,
            &SceneDamage::regions([GridRect::new(GridPoint::new(0, 0), 4, 24)]),
            &initial_batch.predicted,
        )
        .expect("incremental state transaction");
    assert_eq!(renderer.last_strategy(), RenderStrategy::Incremental);
    let bytes = &batch.transactions[0].bytes;
    assert!(bytes.starts_with(b"\x1b[?2026h"));
    assert!(bytes.ends_with(b"\x1b[?2026l"));
    assert!(bytes.windows(5).any(|window| window == b"\x1b[0;1"));
    assert!(
        bytes
            .windows(b"https://example.test".len())
            .any(|window| window == b"https://example.test")
    );
    assert!(bytes.windows(8).any(|window| window == b"\x1b[?2004h"));
    assert!(bytes.windows(5).any(|window| window == b"\x1b[=5u"));
    assert!(bytes.contains(&b'\x07'));
    oracle
        .verify("incremental-state-update", &intended, &batch)
        .expect("verify styled incremental state");
    renderer.confirm(&batch.predicted);

    scene.effects.bell_count = 0;
    let noop = renderer
        .render(
            &scene,
            &SceneDamage::regions([GridRect::new(GridPoint::new(0, 0), 1, 24)]),
            &batch.predicted,
        )
        .expect("render unchanged damage");
    assert_eq!(renderer.last_strategy(), RenderStrategy::Noop);
    assert!(noop.transactions.is_empty());
}
