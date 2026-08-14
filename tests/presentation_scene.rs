use lector::{
    harness::Harness,
    presentation::{
        CursorOwner, GridPoint, GridRect, OutputTransaction, PhysicalTerminalLifecycle,
        PresentationError, PresentedScene, RenderBatch, RenderOracle, RendererBackend, Scene,
        SceneDamage, SceneImagePlacement, SceneOverlay, SceneSurface, SurfaceId,
    },
    terminal::{GhosttyEngine, TerminalGeometry},
};

struct ContractRenderer {
    observed_damage: Option<SceneDamage>,
}

impl RendererBackend for ContractRenderer {
    fn render(
        &mut self,
        scene: &Scene,
        damage: &SceneDamage,
        _presented: &PresentedScene,
    ) -> Result<RenderBatch, PresentationError> {
        self.observed_damage = Some(damage.clone());
        let predicted = PresentedScene::compose(scene)?;
        Ok(RenderBatch::new(Vec::new(), predicted))
    }
}

fn presented(geometry: TerminalGeometry, bytes: &[u8]) -> PresentedScene {
    let mut engine = GhosttyEngine::new(geometry.rows, geometry.cols).expect("create scene engine");
    engine
        .resize_with_geometry(geometry)
        .expect("set scene geometry");
    engine.advance(bytes).expect("construct scene state");
    PresentedScene::from_engine(&engine).expect("capture presented state")
}

#[test]
fn empty_one_pane_and_z_ordered_overlay_scenes_compose_without_engine_identity() {
    let geometry = TerminalGeometry::new(3, 8, 9, 18);
    let empty = Scene::new(geometry);
    let blank = PresentedScene::compose(&empty).expect("compose empty scene");
    assert_eq!(blank.geometry(), geometry);
    assert_eq!(blank.row_text(0), "");
    assert_eq!(blank.cursor_owner(), CursorOwner::Hidden);

    let pane_id = SurfaceId(1);
    let low_id = SurfaceId(2);
    let high_id = SurfaceId(3);
    let pane = presented(geometry, b"abcdefgh\r\nijklmnop");
    let low = presented(TerminalGeometry::from_cells(1, 4), b"low!");
    let high = presented(TerminalGeometry::from_cells(1, 2), b"HI");

    let mut scene = Scene::new(geometry);
    scene.panes.push(SceneSurface::new(
        pane_id,
        GridPoint::new(0, 0),
        pane.into_terminal_snapshot(),
    ));
    scene.overlays.push(SceneOverlay::new(
        SceneSurface::new(low_id, GridPoint::new(1, 2), low.into_terminal_snapshot()),
        10,
    ));
    scene.overlays.push(SceneOverlay::new(
        SceneSurface::new(high_id, GridPoint::new(1, 3), high.into_terminal_snapshot()),
        20,
    ));
    scene.cursor_owner = CursorOwner::Overlay(high_id);
    scene.effects.title = Some("active overlay".into());
    let mut image = lector::presentation::PresentedImage {
        grid_rect: GridRect::new(GridPoint::new(0, 0), 1, 1),
        visible: true,
        ..Default::default()
    };
    image.pixel_width = 1;
    image.pixel_height = 1;
    scene.images.push(SceneImagePlacement {
        owner: pane_id,
        image,
    });

    let composed = PresentedScene::compose(&scene).expect("compose layered scene");
    assert_eq!(composed.row_text(0), "abcdefgh");
    assert_eq!(composed.row_text(1), "ijlHI!op");
    assert_eq!(composed.cursor_owner(), CursorOwner::Overlay(high_id));
    assert_eq!(composed.title(), Some("active overlay"));
    assert_eq!(composed.images().len(), 1);
}

#[test]
fn renderer_backend_receives_scene_damage_and_predicts_independent_state() {
    let scene = Scene::new(TerminalGeometry::from_cells(2, 4));
    let presented = PresentedScene::blank(scene.geometry);
    let mut renderer = ContractRenderer {
        observed_damage: None,
    };

    let result = renderer
        .render(&scene, &SceneDamage::Cursor, &presented)
        .expect("render through backend contract");

    assert_eq!(renderer.observed_damage, Some(SceneDamage::Cursor));
    assert_eq!(result.predicted, PresentedScene::compose(&scene).unwrap());
}

#[test]
fn oracle_tracks_full_cursor_only_and_resize_transactions() {
    let initial_geometry = TerminalGeometry::new(2, 8, 9, 18);
    let initial_bytes = b"\x1b[2J\x1b[Hhello\x1b]2;editor\x07\x1b[?1h\x1b[?2004h";
    let initial = presented(initial_geometry, initial_bytes);
    let mut oracle = RenderOracle::new(initial_geometry).expect("create render oracle");
    oracle
        .verify(
            "initial-full-scene",
            &initial,
            &RenderBatch::new(vec![OutputTransaction::new(initial_bytes)], initial.clone()),
        )
        .expect("verify initial scene");

    let cursor_bytes = b"\x1b[2;7H\x1b[?25l";
    let final_cursor = presented(
        initial_geometry,
        &[initial_bytes.as_slice(), cursor_bytes.as_slice()].concat(),
    );
    oracle
        .verify(
            "cursor-only",
            &final_cursor,
            &RenderBatch::new(
                vec![OutputTransaction::new(cursor_bytes)],
                final_cursor.clone(),
            ),
        )
        .expect("verify cursor-only scene");

    let resized_geometry = TerminalGeometry::new(3, 10, 10, 20);
    let resized_bytes = b"\x1b[?1l\x1b[?2004l\x1b[?25h\x1b]2;resized\x07\x1b[2J\x1b[Hresized";
    let resized = presented(resized_geometry, resized_bytes);
    oracle
        .verify(
            "resized-full-scene",
            &resized,
            &RenderBatch::new(
                vec![OutputTransaction::with_resize(
                    resized_geometry,
                    resized_bytes,
                )],
                resized.clone(),
            ),
        )
        .expect("verify resized scene");
}

#[test]
fn oracle_compares_supported_kitty_image_placements() {
    let geometry = TerminalGeometry::new(2, 4, 10, 20);
    let image = b"\x1b_Ga=T,f=32,s=1,v=1,i=7,p=9,c=1,r=1,q=2;/wAA/w==\x1b\\";
    let intended = presented(geometry, image);
    assert_eq!(intended.images().len(), 1);
    assert_eq!(intended.images()[0].image_id, 7);
    assert_eq!(intended.images()[0].placement_id, 9);
    assert_eq!(intended.images()[0].data_len, 4);
    assert_eq!(intended.images()[0].pixel_width, 1);
    assert_eq!(intended.images()[0].pixel_height, 1);
    assert_eq!(intended.images()[0].rendered_pixel_width, 10);
    assert_eq!(intended.images()[0].rendered_pixel_height, 20);
    assert_eq!(intended.images()[0].grid_rect.rows, 1);
    assert_eq!(intended.images()[0].grid_rect.cols, 1);

    let mut oracle = RenderOracle::new(geometry).expect("create image oracle");
    oracle
        .verify(
            "kitty-image",
            &intended,
            &RenderBatch::new(vec![OutputTransaction::new(image)], intended.clone()),
        )
        .expect("verify image placement");

    for split in 0..=image.len() {
        let mut fragmented = RenderOracle::new(geometry).expect("create fragmented image oracle");
        fragmented
            .verify(
                &format!("kitty-image-split-{split}"),
                &intended,
                &RenderBatch::new(
                    vec![
                        OutputTransaction::new(&image[..split]),
                        OutputTransaction::new(&image[split..]),
                    ],
                    intended.clone(),
                ),
            )
            .unwrap_or_else(|error| panic!("image split {split} failed: {error}"));
    }
}

#[test]
fn oracle_observes_terminal_wide_bell_effects() {
    let geometry = TerminalGeometry::from_cells(2, 4);
    let pane_id = SurfaceId(11);
    let pane = presented(geometry, b"");
    let mut scene = Scene::new(geometry);
    scene.panes.push(SceneSurface::new(
        pane_id,
        GridPoint::new(0, 0),
        pane.into_terminal_snapshot(),
    ));
    scene.cursor_owner = CursorOwner::Pane(pane_id);
    scene.effects.bell_count = 1;
    let intended = PresentedScene::compose(&scene).expect("compose bell scene");

    let mut oracle = RenderOracle::new(geometry).expect("create bell oracle");
    oracle
        .verify(
            "terminal-wide-bell",
            &intended,
            &RenderBatch::new(vec![OutputTransaction::new(b"\x07")], intended.clone()),
        )
        .expect("verify bell effect");
}

#[test]
fn corrupted_renderer_output_always_preserves_a_reproducible_artifact() {
    let geometry = TerminalGeometry::from_cells(2, 8);
    let intended = presented(geometry, b"expected");
    let batch = RenderBatch::new(vec![OutputTransaction::new(b"corrupt")], intended.clone());
    let mut oracle = RenderOracle::new(geometry).expect("create render oracle");
    let failure = oracle
        .verify("deliberately-corrupted", &intended, &batch)
        .expect_err("corrupt output must fail the oracle");

    let artifact: serde_json::Value = serde_json::from_slice(
        &std::fs::read(failure.artifact_path()).expect("oracle failure artifact must exist"),
    )
    .expect("parse oracle artifact");
    for key in [
        "initial_outer_state",
        "intended_scene",
        "emitted_transactions",
        "predicted_scene",
        "resulting_outer_state",
    ] {
        assert!(artifact.get(key).is_some(), "artifact omitted {key}");
    }
    assert_eq!(
        artifact["emitted_transactions"][0]["bytes"],
        serde_json::json!(b"corrupt")
    );
    assert_ne!(
        artifact["intended_scene"]["rows"],
        artifact["resulting_outer_state"]["rows"]
    );
}

#[test]
fn physical_terminal_suspend_resume_and_shutdown_are_explicit_and_idempotent() {
    let mut lifecycle = PhysicalTerminalLifecycle::new(Some(false));
    let activation = lifecycle.activate();
    assert!(
        activation
            .bytes
            .windows(8)
            .any(|part| part == b"\x1b[?1004h")
    );

    let suspended = lifecycle.suspend();
    assert_eq!(suspended.damage, SceneDamage::Full);
    assert!(
        suspended
            .bytes
            .windows(8)
            .any(|part| part == b"\x1b[?1004l")
    );
    assert!(suspended.bytes.windows(6).any(|part| part == b"\x1b[?25h"));

    let resumed = lifecycle.resume();
    assert_eq!(resumed.damage, SceneDamage::Full);
    assert!(resumed.bytes.windows(8).any(|part| part == b"\x1b[?1004h"));

    let shutdown = lifecycle.shutdown();
    assert_eq!(shutdown.damage, SceneDamage::None);
    assert!(shutdown.bytes.windows(8).any(|part| part == b"\x1b[?1004l"));
    assert!(lifecycle.shutdown().bytes.is_empty());
    assert!(lifecycle.resume().bytes.is_empty());
}

#[test]
fn overlay_lifecycle_runs_through_the_real_application_harness() {
    let mut harness = Harness::new(24, 80).expect("create presentation harness");
    harness
        .run_script(include_str!("scripts/presentation_lifecycle.txt"))
        .expect("run overlay presentation lifecycle");
}
