use lector::{
    harness::Harness,
    presentation::{
        CursorOwner, GridPoint, OutputTransaction, PresentedScene, RenderBatch, RenderOracle,
        Scene, SceneSurface, SurfaceId,
    },
    terminal::{GhosttyEngine, TerminalGeometry},
};

#[test]
fn ghostty_only_workflow_preserves_compositor_presentation_and_accessibility_harnesses() {
    let chunks: &[&[u8]] = &[
        b"\x1b]133;A\x07dev@host$ \x1b]133;B\x07cargo test",
        b"\x1b]133;C\x07\r\n\x1b[31merror\x1b[0m: expected `;`\r\n",
        b"\x1b[?1049h\x1b[Heditor\x1b[?1049l",
        b"\x1b]133;D;1\x07",
    ];
    let source = chunks.concat();
    let mut harness = Harness::new(8, 48).expect("create Ghostty workflow harness");
    for chunk in chunks {
        harness
            .handle_pty_output(chunk)
            .expect("process Ghostty-owned PTY output");
    }
    assert_ne!(harness.terminal_output(), source);

    let geometry = TerminalGeometry::from_cells(8, 48);
    let mut engine = GhosttyEngine::new(geometry.rows, geometry.cols).expect("source terminal");
    engine.advance(&source).expect("advance source terminal");
    let mut snapshot = engine.normalized_snapshot();
    snapshot.modes.focus_reporting = true;
    let mut scene = Scene::new(geometry);
    scene.effects.title.clone_from(&snapshot.title);
    scene
        .effects
        .working_directory
        .clone_from(&snapshot.working_directory);
    scene.panes.push(SceneSurface::new(
        SurfaceId(1),
        GridPoint::new(0, 0),
        snapshot,
    ));
    scene.cursor_owner = CursorOwner::Pane(SurfaceId(1));
    let intended = PresentedScene::compose(&scene).expect("compose intended scene");
    let batch = RenderBatch::new(
        vec![OutputTransaction::new(harness.terminal_output())],
        intended.clone(),
    );
    RenderOracle::new(geometry)
        .expect("physical oracle")
        .verify("ghostty-authoritative-workflow", &intended, &batch)
        .expect("compositor output matches Ghostty-owned state");

    for script in [
        include_str!("scripts/semantic_history.txt"),
        include_str!("scripts/auto_read.txt"),
        include_str!("scripts/terminal_resize.txt"),
        include_str!("scripts/terminal_effects_and_modes.txt"),
    ] {
        let mut harness = Harness::new(24, 80).expect("create accessibility harness");
        harness
            .run_script(script)
            .expect("run Ghostty-only workflow");
    }
}
