use lector::{
    presentation::{
        CursorOwner, FullSceneVtRenderer, GridPoint, GridRect, IncrementalVtRenderer,
        PresentedScene, RenderBatch, RenderCapabilities, RenderOracle, RenderStats, RenderStrategy,
        RendererBackend, Scene, SceneDamage, SceneImagePlacement, SceneOperation, SceneOverlay,
        SceneSurface, SurfaceId,
    },
    terminal::{GhosttyEngine, TerminalGeometry, TerminalOperation, TerminalSnapshot},
};
use std::sync::Arc;

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

fn initial_grid(geometry: TerminalGeometry) -> Vec<u8> {
    let mut bytes = Vec::new();
    for row in 1..=geometry.rows {
        bytes.extend_from_slice(format!("\x1b[{row};1Hr{row}-abcdefghij").as_bytes());
    }
    bytes
}

fn byte_len(batch: &RenderBatch) -> usize {
    batch
        .transactions
        .iter()
        .map(|transaction| transaction.bytes.len())
        .sum()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

struct SemanticSession {
    source: GhosttyEngine,
    renderer: IncrementalVtRenderer,
    oracle: RenderOracle,
    presented: PresentedScene,
}

impl SemanticSession {
    fn new(geometry: TerminalGeometry) -> Self {
        let mut source = GhosttyEngine::new(geometry.rows, geometry.cols).expect("source engine");
        source
            .advance(&initial_grid(geometry))
            .expect("initialize source grid");
        let scene = scene_for(&source);
        let intended = PresentedScene::compose(&scene).expect("compose initial scene");
        let mut renderer = IncrementalVtRenderer::new(RenderCapabilities::default());
        let blank = PresentedScene::blank(geometry);
        let batch = renderer
            .render(&scene, &SceneDamage::Full, &blank)
            .expect("initial full render");
        let mut oracle = RenderOracle::new(geometry).expect("render oracle");
        oracle
            .verify("semantic-initial", &intended, &batch)
            .expect("verify initial scene");
        renderer.confirm(&batch.predicted);
        Self {
            source,
            renderer,
            oracle,
            presented: batch.predicted,
        }
    }

    fn update(&mut self, name: &str, bytes: &[u8]) -> (RenderBatch, usize, RenderStats) {
        let update = self.source.advance(bytes).expect("advance source");
        let scene = scene_for(&self.source);
        let damage = SceneDamage::from_terminal_update(&scene.panes[0], &update, scene.geometry);
        let mut full = FullSceneVtRenderer::new(RenderCapabilities::default());
        let full_batch = full
            .render(&scene, &SceneDamage::Full, &self.presented)
            .expect("full comparison");
        let batch = self
            .renderer
            .render(&scene, &damage, &self.presented)
            .expect("semantic render");
        let intended = PresentedScene::compose(&scene).expect("compose intended scene");
        self.oracle
            .verify(name, &intended, &batch)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let stats = self.renderer.last_stats();
        self.renderer.confirm(&batch.predicted);
        self.presented = batch.predicted.clone();
        (batch, byte_len(&full_batch), stats)
    }
}

#[test]
fn ghostty_updates_record_structural_and_write_operations_as_non_authoritative_hints() {
    let mut scroll = GhosttyEngine::new(5, 12).expect("scroll engine");
    let update = scroll.advance(b"\x1b[2S").expect("scroll up");
    assert_eq!(
        update.operations,
        vec![TerminalOperation::ScrollUp {
            top: 0,
            bottom: 4,
            count: 2,
        }]
    );

    let mut partial = GhosttyEngine::new(5, 12).expect("partial engine");
    let update = partial
        .advance(b"\x1b[2;4r\x1b[3;1H\x1b[2L")
        .expect("insert lines");
    assert!(update.operations.contains(&TerminalOperation::InsertLines {
        row: 2,
        bottom: 3,
        count: 2,
    }));

    let mut cells = GhosttyEngine::new(5, 12).expect("cell engine");
    let update = cells
        .advance(b"\x1b[2;3H\x1b[2@\x1b[3P\x1b[4X")
        .expect("edit cells");
    assert!(update.operations.contains(&TerminalOperation::InsertChars {
        row: 1,
        col: 2,
        count: 2,
    }));
    assert!(update.operations.contains(&TerminalOperation::DeleteChars {
        row: 1,
        col: 2,
        count: 3,
    }));
    assert!(update.operations.contains(&TerminalOperation::EraseChars {
        row: 1,
        col: 2,
        count: 4,
    }));

    let mut write = GhosttyEngine::new(5, 12).expect("write engine");
    let update = write
        .advance(b"\x1b[2;2H\x1b[3Cx\x1b[3b")
        .expect("relative repeated write");
    assert!(update.operations.iter().any(|operation| {
        matches!(
            operation,
            TerminalOperation::WriteRun { row: 1, col: 4, text } if text == "xxxx"
        )
    }));
}

#[test]
fn full_and_partial_scrolls_use_structural_vt_and_match_the_oracle() {
    let geometry = TerminalGeometry::from_cells(6, 20);
    let mut full = SemanticSession::new(geometry);
    let (batch, full_bytes, stats) = full.update(
        "semantic-full-scroll",
        b"\x1b[2S\x1b[5;1Hnew-five\x1b[6;1Hnew-six",
    );
    assert_eq!(
        full.renderer.last_strategy(),
        RenderStrategy::SemanticFastPath
    );
    let bytes = &batch.transactions[0].bytes;
    assert!(contains(bytes, b"\x1b[2S"));
    assert!(!contains(bytes, b"\x1b[2J"));
    assert!(byte_len(&batch) * 2 < full_bytes);
    assert!(stats.cells_compared < usize::from(geometry.rows * geometry.cols));

    let mut partial = SemanticSession::new(geometry);
    let (batch, full_bytes, _) = partial.update(
        "semantic-partial-scroll",
        b"\x1b[2;5r\x1b[2S\x1b[4;1Hpartial-four\x1b[5;1Hpartial-five",
    );
    assert_eq!(
        partial.renderer.last_strategy(),
        RenderStrategy::SemanticFastPath
    );
    let bytes = &batch.transactions[0].bytes;
    assert!(contains(bytes, b"\x1b[2;5r"));
    assert!(contains(bytes, b"\x1b[2S"));
    assert!(byte_len(&batch) * 2 < full_bytes);

    let (batch, full_bytes, _) = partial.update(
        "semantic-partial-reverse-scroll",
        b"\x1b[2T\x1b[2;1Hreverse-two\x1b[3;1Hreverse-three",
    );
    assert_eq!(
        partial.renderer.last_strategy(),
        RenderStrategy::SemanticFastPath
    );
    assert!(contains(&batch.transactions[0].bytes, b"\x1b[2T"));
    assert!(byte_len(&batch) * 2 < full_bytes);
}

#[test]
fn line_character_and_erase_operations_are_translated_and_reconciled() {
    let geometry = TerminalGeometry::from_cells(6, 20);
    for (name, source, canonical) in [
        (
            "semantic-insert-lines",
            b"\x1b[2;5r\x1b[3;1H\x1b[2Linserted".as_slice(),
            b"\x1b[2L".as_slice(),
        ),
        (
            "semantic-delete-lines",
            b"\x1b[2;5r\x1b[3;1H\x1b[2Mreplacement".as_slice(),
            b"\x1b[2M".as_slice(),
        ),
        (
            "semantic-insert-chars",
            b"\x1b[2;4H\x1b[3@XYZ".as_slice(),
            b"\x1b[3@".as_slice(),
        ),
        (
            "semantic-delete-chars",
            b"\x1b[2;4H\x1b[3Pxyz".as_slice(),
            b"\x1b[3P".as_slice(),
        ),
        (
            "semantic-erase-chars",
            b"\x1b[2;4H\x1b[5X".as_slice(),
            b"\x1b[5X".as_slice(),
        ),
        (
            "semantic-erase-line",
            b"\x1b[2;4H\x1b[K".as_slice(),
            b"\x1b[17X".as_slice(),
        ),
        (
            "semantic-erase-display",
            b"\x1b[4;4H\x1b[J".as_slice(),
            b"\x1b[17X".as_slice(),
        ),
    ] {
        let mut session = SemanticSession::new(geometry);
        let (batch, full_bytes, _) = session.update(name, source);
        assert_eq!(
            session.renderer.last_strategy(),
            RenderStrategy::SemanticFastPath,
            "{name}"
        );
        assert!(contains(&batch.transactions[0].bytes, canonical), "{name}");
        assert!(byte_len(&batch) < full_bytes, "{name}");
    }
}

#[test]
fn adjacent_repeated_and_cursor_relative_writes_stay_batched() {
    let geometry = TerminalGeometry::from_cells(6, 20);
    let mut session = SemanticSession::new(geometry);
    let (batch, full_bytes, stats) =
        session.update("semantic-write-runs", b"\x1b[2;2H\x1b[3Cx\x1b[7b-more");
    assert_eq!(
        session.renderer.last_strategy(),
        RenderStrategy::SemanticFastPath
    );
    assert!(stats.rows_considered <= 2);
    assert!(stats.cells_emitted <= 13);
    assert!(batch.transactions.len() == 1);
    assert!(byte_len(&batch) * 3 < full_bytes);
}

#[test]
fn scroll_regions_persist_across_updates_and_ambiguous_writes_do_not_emit_hints() {
    let mut engine = GhosttyEngine::new(5, 12).expect("operation engine");
    let region = engine.advance(b"\x1b[2;4r").expect("set persistent region");
    assert!(region.operations.is_empty());
    let scroll = engine.advance(b"\x1b[2S").expect("scroll saved region");
    assert_eq!(
        scroll.operations,
        vec![TerminalOperation::ScrollUp {
            top: 1,
            bottom: 3,
            count: 2,
        }]
    );

    let ambiguous = engine
        .advance(b"\x1b[1;12Hright-margin")
        .expect("right margin write");
    assert!(ambiguous.operations.is_empty());
    let tab = engine.advance(b"\ttext").expect("tabbed write");
    assert!(tab.operations.is_empty());
    let unicode = engine.advance("é".as_bytes()).expect("unicode write");
    assert!(unicode.operations.is_empty());

    let saved_cursor = engine
        .advance(b"\x1b[2;2H\x1b[s\x1b[4;4HX\x1b[u\x1b[2;2H")
        .expect("saved cursor ambiguity");
    assert!(saved_cursor.operations.is_empty());
    let horizontal_margins = engine
        .advance(b"\x1b[?69h\x1b[3;8s\x1b[2;3H\x1b[2@")
        .expect("horizontal margin ambiguity");
    assert!(horizontal_margins.operations.is_empty());
    let soft_reset = engine
        .advance(b"\x1b[!p\x1b[2S")
        .expect("soft reset ambiguity");
    assert!(soft_reset.operations.is_empty());

    let mut cursor_visibility = GhosttyEngine::new(5, 12).expect("visibility engine");
    let cursor_visibility = cursor_visibility
        .advance(b"\x1b[?25l\x1b[2S\x1b[?25h")
        .expect("cursor visibility around structural update");
    assert_eq!(
        cursor_visibility.operations,
        vec![TerminalOperation::ScrollUp {
            top: 0,
            bottom: 4,
            count: 2,
        }]
    );
}

#[test]
fn fragmented_adjacent_write_hints_merge_without_becoming_terminal_truth() {
    let mut engine = GhosttyEngine::new(3, 20).expect("fragmented engine");
    let mut merged = lector::terminal::UpdateSummary::default();
    for chunk in [
        b"adjacent".as_slice(),
        b"-write".as_slice(),
        b"-run".as_slice(),
    ] {
        merged.merge(engine.advance(chunk).expect("advance fragmented write"));
    }
    assert_eq!(
        merged.operations,
        vec![TerminalOperation::WriteRun {
            row: 0,
            col: 0,
            text: "adjacent-write-run".to_owned(),
        }]
    );
    assert_eq!(
        engine.normalized_snapshot().rows[0].contents(),
        "adjacent-write-run"
    );
}

#[test]
fn occlusion_and_clipping_reject_structural_fast_paths_but_remain_correct() {
    let geometry = TerminalGeometry::from_cells(6, 20);
    let mut session = SemanticSession::new(geometry);
    let update = session.source.advance(b"\x1b[2S").expect("scroll source");
    let mut scene = scene_for(&session.source);
    let mut overlay_engine = GhosttyEngine::new(2, 8).expect("overlay engine");
    overlay_engine.advance(b"overlay").expect("draw overlay");
    scene.overlays.push(SceneOverlay::new(
        SceneSurface::new(
            OVERLAY,
            GridPoint::new(2, 4),
            overlay_engine.normalized_snapshot(),
        ),
        10,
    ));
    scene.cursor_owner = CursorOwner::Overlay(OVERLAY);
    let damage = SceneDamage::from_terminal_update(&scene.panes[0], &update, scene.geometry);
    let batch = session
        .renderer
        .render(&scene, &damage, &session.presented)
        .expect("occluded fallback");
    assert_ne!(
        session.renderer.last_strategy(),
        RenderStrategy::SemanticFastPath
    );
    let intended = PresentedScene::compose(&scene).expect("compose occluded scene");
    session
        .oracle
        .verify("semantic-occluded-fallback", &intended, &batch)
        .expect("verify occluded fallback");

    let mut clipped_source = GhosttyEngine::new(6, 20).expect("clipped source");
    clipped_source
        .advance(&initial_grid(geometry))
        .expect("initialize clipped source");
    let snapshot = clipped_source.normalized_snapshot();
    let mut clipped_scene = Scene::new(geometry);
    clipped_scene
        .panes
        .push(SceneSurface::new(ROOT, GridPoint::new(0, 1), snapshot));
    clipped_scene.cursor_owner = CursorOwner::Pane(ROOT);
    let clipped_initial_intended =
        PresentedScene::compose(&clipped_scene).expect("compose clipped initial");
    let mut clipped_renderer = IncrementalVtRenderer::new(RenderCapabilities::default());
    let blank = PresentedScene::blank(geometry);
    let initial = clipped_renderer
        .render(&clipped_scene, &SceneDamage::Full, &blank)
        .expect("render clipped initial");
    clipped_renderer.confirm(&initial.predicted);
    let update = clipped_source
        .advance(b"\x1b[2;4H\x1b[2P")
        .expect("clipped delete");
    clipped_scene.panes[0].snapshot = clipped_source.normalized_snapshot();
    let damage =
        SceneDamage::from_terminal_update(&clipped_scene.panes[0], &update, clipped_scene.geometry);
    let batch = clipped_renderer
        .render(&clipped_scene, &damage, &initial.predicted)
        .expect("clipped fallback");
    assert_ne!(
        clipped_renderer.last_strategy(),
        RenderStrategy::SemanticFastPath
    );
    let mut oracle = RenderOracle::new(geometry).expect("clipped oracle");
    oracle
        .verify(
            "semantic-clipped-initial",
            &clipped_initial_intended,
            &initial,
        )
        .expect("prime clipped oracle");
    let intended = PresentedScene::compose(&clipped_scene).expect("compose clipped intended");
    oracle
        .verify("semantic-clipped-fallback", &intended, &batch)
        .expect("verify clipped fallback");
}

#[test]
fn an_inconsistent_operation_hint_forces_the_full_correctness_fallback() {
    let geometry = TerminalGeometry::from_cells(6, 20);
    let mut session = SemanticSession::new(geometry);
    let update = session
        .source
        .advance(b"\x1b[2;4H\x1b[2P")
        .expect("delete chars");
    let mut scene = scene_for(&session.source);
    let rows = Arc::make_mut(&mut scene.panes[0].snapshot.rows);
    Arc::make_mut(&mut rows[5].cells)[0].grapheme = "!".into();
    let damage = SceneDamage::from_terminal_update(&scene.panes[0], &update, scene.geometry);
    let batch = session
        .renderer
        .render(&scene, &damage, &session.presented)
        .expect("inconsistent operation fallback");
    assert_eq!(
        session.renderer.last_strategy(),
        RenderStrategy::FullFallback
    );
    let intended = PresentedScene::compose(&scene).expect("compose inconsistent intended");
    session
        .oracle
        .verify("semantic-inconsistent-fallback", &intended, &batch)
        .expect("verify full fallback");
}

#[test]
fn every_structural_precondition_rejects_to_a_correct_nonsemantic_path() {
    let geometry = TerminalGeometry::from_cells(6, 20);
    let region = lector::presentation::GridRect::new(GridPoint::new(1, 3), 1, 17);
    let valid_operation = SceneOperation::DeleteChars { region, count: 2 };

    for (name, owner, operation, extra_pane, mismatched_geometry) in [
        (
            "semantic-owner-rejection",
            SurfaceId(99),
            valid_operation.clone(),
            false,
            false,
        ),
        (
            "semantic-multiple-pane-rejection",
            ROOT,
            valid_operation.clone(),
            true,
            false,
        ),
        (
            "semantic-geometry-rejection",
            ROOT,
            valid_operation.clone(),
            false,
            true,
        ),
        (
            "semantic-clipped-region-rejection",
            ROOT,
            SceneOperation::DeleteChars {
                region: lector::presentation::GridRect::new(GridPoint::new(1, 19), 1, 4),
                count: 2,
            },
            false,
            false,
        ),
        (
            "semantic-zero-count-rejection",
            ROOT,
            SceneOperation::DeleteChars { region, count: 0 },
            false,
            false,
        ),
        (
            "semantic-unicode-hint-rejection",
            ROOT,
            SceneOperation::WriteRun {
                origin: GridPoint::new(1, 3),
                text: "é".to_owned(),
            },
            false,
            false,
        ),
    ] {
        let mut session = SemanticSession::new(geometry);
        let mut scene = scene_for(&session.source);
        if extra_pane {
            scene.panes.push(SceneSurface::new(
                SurfaceId(3),
                GridPoint::new(0, 0),
                TerminalSnapshot::default(),
            ));
        }
        if mismatched_geometry {
            scene.panes[0].snapshot.geometry.cols -= 1;
        }
        let intended = PresentedScene::compose(&scene).expect("compose rejection scene");
        let damage = SceneDamage::Operations {
            owner,
            regions: vec![region],
            operations: vec![operation],
        };
        let batch = session
            .renderer
            .render(&scene, &damage, &session.presented)
            .expect("render rejected semantic operation");
        assert_ne!(
            session.renderer.last_strategy(),
            RenderStrategy::SemanticFastPath,
            "{name}"
        );
        session
            .oracle
            .verify(name, &intended, &batch)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
    }

    let mut image_session = SemanticSession::new(geometry);
    image_session.renderer.set_capabilities(RenderCapabilities {
        kitty_graphics: true,
        ..RenderCapabilities::default()
    });
    let mut image_scene = scene_for(&image_session.source);
    let mut missing_upload = SceneImagePlacement::default();
    missing_upload.image.image_id = 1;
    missing_upload.image.visible = true;
    missing_upload.image.grid_rect = GridRect::new(GridPoint::new(0, 0), 1, 1);
    image_scene.images.push(missing_upload);
    let error = image_session
        .renderer
        .render(
            &image_scene,
            &SceneDamage::Operations {
                owner: ROOT,
                regions: vec![region],
                operations: vec![valid_operation],
            },
            &image_session.presented,
        )
        .expect_err("a placement without retained upload data must be rejected");
    assert!(error.to_string().contains("no retained upload"));
    assert_eq!(
        image_session.renderer.last_strategy(),
        RenderStrategy::FullFallback
    );
}

#[test]
fn deterministic_semantic_sequence_matches_pure_diff_and_full_scene_oracles() {
    let geometry = TerminalGeometry::from_cells(8, 24);
    let mut source = GhosttyEngine::new(geometry.rows, geometry.cols).expect("property source");
    source
        .advance(&initial_grid(geometry))
        .expect("initialize property source");
    let initial_scene = scene_for(&source);
    let initial_intended =
        PresentedScene::compose(&initial_scene).expect("compose property initial");
    let blank = PresentedScene::blank(geometry);

    let mut semantic = IncrementalVtRenderer::new(RenderCapabilities::default());
    let semantic_initial = semantic
        .render(&initial_scene, &SceneDamage::Full, &blank)
        .expect("semantic initial");
    let mut semantic_oracle = RenderOracle::new(geometry).expect("semantic property oracle");
    semantic_oracle
        .verify(
            "semantic-property-initial",
            &initial_intended,
            &semantic_initial,
        )
        .expect("verify semantic initial");
    semantic.confirm(&semantic_initial.predicted);
    let mut semantic_presented = semantic_initial.predicted;

    let mut pure_diff = IncrementalVtRenderer::new(RenderCapabilities::default());
    let diff_initial = pure_diff
        .render(&initial_scene, &SceneDamage::Full, &blank)
        .expect("diff initial");
    let mut diff_oracle = RenderOracle::new(geometry).expect("diff property oracle");
    diff_oracle
        .verify(
            "semantic-property-diff-initial",
            &initial_intended,
            &diff_initial,
        )
        .expect("verify diff initial");
    pure_diff.confirm(&diff_initial.predicted);
    let mut diff_presented = diff_initial.predicted;

    let full_region =
        lector::presentation::GridRect::new(GridPoint::new(0, 0), geometry.rows, geometry.cols);
    let mut semantic_bytes = 0usize;
    let mut diff_bytes = 0usize;
    for step in 0..64 {
        let update_bytes = match step % 8 {
            0 => format!("\x1b[r\x1b[S\x1b[8;1Hscroll-{step:02}\x1b[K"),
            1 => format!("\x1b[r\x1b[T\x1b[1;1Hreverse-{step:02}\x1b[K"),
            2 => format!("\x1b[3;5H\x1b[2@I{step:02}"),
            3 => format!("\x1b[4;6H\x1b[2PD{step:02}"),
            4 => format!("\x1b[2;7r\x1b[3;1H\x1b[Lline-{step:02}"),
            5 => format!("\x1b[2;7r\x1b[4;1H\x1b[Mdrop-{step:02}"),
            6 => format!("\x1b[5;4H\x1b[6X{step:02}"),
            _ => format!("\x1b[2;2H\x1b[2Cx\x1b[3b-{step:02}"),
        };
        let update = source
            .advance(update_bytes.as_bytes())
            .expect("advance property operation");
        let scene = scene_for(&source);
        let intended = PresentedScene::compose(&scene).expect("compose property step");
        let semantic_damage =
            SceneDamage::from_terminal_update(&scene.panes[0], &update, scene.geometry);
        let semantic_batch = semantic
            .render(&scene, &semantic_damage, &semantic_presented)
            .expect("semantic property render");
        semantic_oracle
            .verify(
                &format!("semantic-property-step-{step}"),
                &intended,
                &semantic_batch,
            )
            .unwrap_or_else(|error| panic!("semantic step {step}: {error}"));
        semantic_bytes = semantic_bytes.saturating_add(byte_len(&semantic_batch));
        semantic.confirm(&semantic_batch.predicted);
        semantic_presented = semantic_batch.predicted;

        let diff_batch = pure_diff
            .render(
                &scene,
                &SceneDamage::regions([full_region]),
                &diff_presented,
            )
            .expect("pure diff property render");
        diff_oracle
            .verify(
                &format!("semantic-property-diff-step-{step}"),
                &intended,
                &diff_batch,
            )
            .unwrap_or_else(|error| panic!("diff step {step}: {error}"));
        diff_bytes = diff_bytes.saturating_add(byte_len(&diff_batch));
        pure_diff.confirm(&diff_batch.predicted);
        diff_presented = diff_batch.predicted;
    }
    assert!(
        semantic_bytes * 5 < diff_bytes * 3,
        "semantic={semantic_bytes}, pure_diff={diff_bytes}"
    );
}
