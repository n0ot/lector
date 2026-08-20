use lector::{
    harness::Harness,
    presentation::{
        CursorOwner, FullSceneVtRenderer, GridPoint, GridRect, IncrementalVtRenderer, MediaLimits,
        OutputTransaction, PaneMediaStore, PresentedScene, RenderBatch, RenderCapabilities,
        RenderOracle, RenderStrategy, RendererBackend, Scene, SceneDamage, SceneOverlay,
        SceneSurface, SurfaceId,
    },
    terminal::{GhosttyEngine, TerminalGeometry},
    terminal_protocol::PhysicalTerminalProfile,
};
use std::sync::Arc;

const ROOT: SurfaceId = SurfaceId(1);
const OVERLAY: SurfaceId = SurfaceId(2);
const ONE_PIXEL_RGBA: &[u8] = b"\x1b_Ga=T,f=32,s=1,v=1,i=7,p=9,c=1,r=1,q=2;/wAA/w==\x1b\\";

fn image_engine(geometry: TerminalGeometry, bytes: &[u8]) -> GhosttyEngine {
    let mut engine = GhosttyEngine::new(geometry.rows, geometry.cols).expect("image engine");
    engine
        .resize_with_geometry(geometry)
        .expect("image geometry");
    engine.advance(bytes).expect("image stream");
    engine
}

fn scene_with_media(
    owner: SurfaceId,
    origin: GridPoint,
    geometry: TerminalGeometry,
    engine: &GhosttyEngine,
    store: &mut PaneMediaStore,
) -> Scene {
    let placements = engine
        .kitty_image_placements()
        .expect("copy Ghostty media state");
    store
        .synchronize(&placements)
        .expect("synchronize pane media");
    let mut scene = Scene::new(geometry);
    scene.panes.push(SceneSurface::new(
        owner,
        origin,
        engine.normalized_snapshot(),
    ));
    scene.cursor_owner = CursorOwner::Pane(owner);
    store
        .append_to_scene(
            owner,
            origin,
            GridRect::new(origin, geometry.rows, geometry.cols),
            &mut scene,
        )
        .expect("append pane media");
    scene
}

#[test]
fn ghostty_media_lifetime_handles_chunking_delete_scroll_resize_and_malformed_payloads() {
    let geometry = TerminalGeometry::new(5, 8, 10, 20);
    let mut engine = image_engine(geometry, b"");
    let first = b"\x1b_Ga=T,f=32,s=2,v=1,i=3,p=4,c=2,r=1,z=-3,m=1;/wAA/w==\x1b\\";
    let last = b"\x1b_Gm=0;AP8A/w==\x1b\\";
    engine.advance(first).expect("first image chunk");
    assert!(
        engine
            .kitty_image_placements()
            .expect("incomplete placements")
            .is_empty(),
        "an incomplete upload must not become a placement"
    );
    engine.advance(last).expect("last image chunk");
    let complete = engine.kitty_image_placements().expect("complete placement");
    assert_eq!(complete.len(), 1);
    assert_eq!(complete[0].data.len(), 8);
    assert_eq!(complete[0].z_index, -3);

    engine
        .advance(b"\r\n\r\n\r\n\r\n\r\n\r\n")
        .expect("scroll placement");
    let scrolled = engine
        .kitty_image_placements()
        .expect("scrolled placements");
    assert_eq!(scrolled.len(), 1, "history retains the anchored placement");
    assert!(
        !scrolled[0].visible && scrolled[0].viewport_row < 0,
        "a placement outside the live viewport must be retained but hidden: {scrolled:?}"
    );

    engine.advance(ONE_PIXEL_RGBA).expect("replacement image");
    engine
        .resize_with_geometry(TerminalGeometry::new(3, 4, 12, 24))
        .expect("resize with media");
    let resized = engine.kitty_image_placements().expect("resized placement");
    let visible: Vec<_> = resized
        .iter()
        .filter(|placement| placement.visible)
        .collect();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].rendered_pixel_width, 12);
    assert_eq!(visible[0].rendered_pixel_height, 24);

    engine
        .advance(b"\x1b_Ga=d,d=A\x1b\\")
        .expect("delete media");
    assert!(
        engine
            .kitty_image_placements()
            .expect("deleted placements")
            .is_empty()
    );
    engine
        .advance(b"\x1b_Ga=T,f=32,s=999999999,v=999999999,i=8;not-base64\x1b\\")
        .expect("malformed media is contained");
    assert!(
        engine
            .kitty_image_placements()
            .expect("malformed placements")
            .is_empty()
    );
}

#[test]
fn pane_media_store_separates_uploads_from_placements_and_enforces_limits() {
    let geometry = TerminalGeometry::new(3, 8, 10, 20);
    let mut engine = image_engine(geometry, ONE_PIXEL_RGBA);
    engine
        .advance(b"\x1b[2;2H\x1b_Ga=p,i=7,p=10,c=1,r=1,C=1;\x1b\\")
        .expect("second placement");
    let placements = engine.kitty_image_placements().expect("source placements");
    assert_eq!(placements.len(), 2);

    let mut store = PaneMediaStore::new(MediaLimits::default());
    let report = store.synchronize(&placements).expect("media sync");
    assert_eq!(
        store.upload_count(),
        1,
        "pixel data is pane-scoped and deduplicated"
    );
    assert_eq!(store.placement_count(), 2);
    assert_eq!(store.total_bytes(), 4);
    assert_eq!(report.uploads_added, 1);
    assert_eq!(report.placements_added, 2);
    assert!(!PaneMediaStore::animation_exposed_by_engine());

    let limits = MediaLimits {
        maximum_image_bytes: 3,
        maximum_pane_bytes: 3,
        maximum_scene_bytes: 3,
        maximum_placements: 1,
    };
    let mut bounded = PaneMediaStore::new(limits);
    let error = bounded
        .synchronize(&placements)
        .expect_err("oversized decoded image must be rejected before retention");
    assert!(error.to_string().contains("media limit"));
    assert_eq!(bounded.total_bytes(), 0);
    assert_eq!(bounded.upload_count(), 0);
    assert_eq!(bounded.placement_count(), 0);
}

#[test]
fn pane_upload_cache_is_shared_across_scene_recomposition_without_copying_pixels() {
    let geometry = TerminalGeometry::new(3, 8, 10, 20);
    let engine = image_engine(geometry, ONE_PIXEL_RGBA);
    let mut store = PaneMediaStore::new(MediaLimits::default());
    let source = engine.kitty_image_placements().expect("source media");
    store.synchronize(&source).expect("cache source media");

    let first = scene_with_media(ROOT, GridPoint::new(0, 0), geometry, &engine, &mut store);
    let second = scene_with_media(ROOT, GridPoint::new(0, 0), geometry, &engine, &mut store);

    assert_eq!(first.image_uploads.len(), 1);
    assert_eq!(second.image_uploads.len(), 1);
    assert!(Arc::ptr_eq(&source[0].data, &first.image_uploads[0].data));
    assert!(Arc::ptr_eq(
        &first.image_uploads[0].data,
        &second.image_uploads[0].data
    ));
}

#[test]
fn ghostty_media_payloads_are_shared_until_the_image_bytes_change() {
    let geometry = TerminalGeometry::new(3, 8, 10, 20);
    let mut engine = image_engine(geometry, ONE_PIXEL_RGBA);
    let first = engine
        .kitty_image_placements()
        .expect("first media snapshot");

    engine.advance(b"text").expect("unrelated text update");
    let unchanged = engine
        .kitty_image_placements()
        .expect("unchanged media snapshot");
    assert!(Arc::ptr_eq(&first[0].data, &unchanged[0].data));
    assert_eq!(first[0].data_digest, unchanged[0].data_digest);

    let replacement =
        b"\x1b_Ga=d,d=A\x1b\\\x1b_Ga=T,f=32,s=1,v=1,i=7,p=9,c=1,r=1,q=2;AAD//w==\x1b\\";
    engine.advance(replacement).expect("replace image bytes");
    let changed = engine
        .kitty_image_placements()
        .expect("changed media snapshot");
    assert_eq!(changed.len(), 1);
    assert!(!Arc::ptr_eq(&first[0].data, &changed[0].data));
    assert_ne!(first[0].data_digest, changed[0].data_digest);
    assert_eq!(changed[0].data.as_ref(), [0, 0, 255, 255]);
}

#[test]
fn kitty_backend_reconstructs_text_upload_and_placement_at_every_apc_boundary() {
    let geometry = TerminalGeometry::new(3, 12, 10, 20);
    let source = [b"linked ".as_slice(), ONE_PIXEL_RGBA].concat();
    let engine = image_engine(geometry, &source);
    let mut store = PaneMediaStore::new(MediaLimits::default());
    let scene = scene_with_media(ROOT, GridPoint::new(0, 0), geometry, &engine, &mut store);
    let logical = PresentedScene::compose(&scene).expect("logical media scene");
    assert_eq!(logical.images().len(), 1);

    let mut renderer = FullSceneVtRenderer::new(RenderCapabilities {
        kitty_graphics: true,
        hyperlinks: true,
        ..RenderCapabilities::default()
    });
    let batch = renderer
        .render(&scene, &SceneDamage::Full, &PresentedScene::blank(geometry))
        .expect("render image scene");
    assert_eq!(batch.predicted.images().len(), 1);
    assert_ne!(batch.predicted.images()[0].image_id, 0);
    assert_ne!(batch.predicted.images()[0].placement_id, 0);
    let emitted: Vec<u8> = batch
        .transactions
        .iter()
        .flat_map(|transaction| transaction.bytes.iter().copied())
        .collect();
    assert!(emitted.windows(4).any(|window| window == b"\x1b_Ga"));

    let mut oracle = RenderOracle::new(geometry).expect("image oracle");
    oracle
        .verify("media-full-render", &batch.predicted, &batch)
        .expect("outer terminal matches text and media");

    for split in 0..=emitted.len() {
        let mut fragmented = RenderOracle::new(geometry).expect("fragmented image oracle");
        let split_batch = lector::presentation::RenderBatch::new(
            vec![
                lector::presentation::OutputTransaction::new(&emitted[..split]),
                lector::presentation::OutputTransaction::new(&emitted[split..]),
            ],
            batch.predicted.clone(),
        );
        fragmented
            .verify(
                &format!("media-full-render-split-{split}"),
                &batch.predicted,
                &split_batch,
            )
            .unwrap_or_else(|error| panic!("split {split}: {error}"));
    }
}

#[test]
fn kitty_backend_chunks_large_uploads_and_text_fallback_never_leaks_graphics() {
    let geometry = TerminalGeometry::new(4, 20, 10, 20);
    let pixels = vec![0x7f; 40 * 40 * 4];
    let encoded = base64(&pixels);
    let command = format!("\x1b_Ga=T,f=32,s=40,v=40,i=31,p=41,c=4,r=4,q=2;{encoded}\x1b\\");
    let engine = image_engine(geometry, command.as_bytes());
    let mut store = PaneMediaStore::new(MediaLimits::default());
    let scene = scene_with_media(ROOT, GridPoint::new(0, 0), geometry, &engine, &mut store);

    let mut kitty = FullSceneVtRenderer::new(RenderCapabilities {
        kitty_graphics: true,
        ..RenderCapabilities::default()
    });
    let batch = kitty
        .render(&scene, &SceneDamage::Full, &PresentedScene::blank(geometry))
        .expect("chunked render");
    let bytes = &batch.transactions[0].bytes;
    assert!(bytes.windows(4).any(|window| window == b"m=1;"));
    assert!(bytes.windows(4).any(|window| window == b"m=0;"));
    assert!(batch.transactions[0].bytes.len() < pixels.len() * 2);

    let mut text_only = FullSceneVtRenderer::new(RenderCapabilities::default());
    let fallback = text_only
        .render(&scene, &SceneDamage::Full, &PresentedScene::blank(geometry))
        .expect("text fallback");
    assert!(fallback.predicted.images().is_empty());
    assert!(
        !fallback.transactions[0]
            .bytes
            .windows(3)
            .any(|window| window == b"_Ga")
    );
}

#[test]
fn pane_ids_are_namespaced_and_overlay_occlusion_releases_then_restores_placements() {
    let geometry = TerminalGeometry::new(3, 12, 10, 20);
    let engine = image_engine(geometry, ONE_PIXEL_RGBA);
    let mut left_store = PaneMediaStore::new(MediaLimits::default());
    let mut right_store = PaneMediaStore::new(MediaLimits::default());
    let mut scene = scene_with_media(
        ROOT,
        GridPoint::new(0, 0),
        geometry,
        &engine,
        &mut left_store,
    );
    right_store
        .synchronize(
            &engine
                .kitty_image_placements()
                .expect("second pane placement"),
        )
        .expect("second pane media");
    right_store
        .append_to_scene(
            SurfaceId(99),
            GridPoint::new(1, 2),
            GridRect::new(GridPoint::new(1, 2), 2, 6),
            &mut scene,
        )
        .expect("append second pane media");
    let visible = PresentedScene::compose(&scene).expect("two-pane media");
    assert_eq!(visible.images().len(), 2);
    assert_ne!(visible.images()[0].image_id, visible.images()[1].image_id);
    assert_ne!(
        visible.images()[0].placement_id,
        visible.images()[1].placement_id
    );
    assert!(visible.images()[1].grid_rect.cols <= 6);

    let mut overlay = GhosttyEngine::new(geometry.rows, geometry.cols).expect("overlay");
    overlay.advance(b"overlay").expect("draw overlay");
    scene.overlays.push(SceneOverlay::new(
        SceneSurface::new(OVERLAY, GridPoint::new(0, 0), overlay.normalized_snapshot()),
        i32::MAX,
    ));
    scene.cursor_owner = CursorOwner::Overlay(OVERLAY);
    let occluded = PresentedScene::compose(&scene).expect("occluded media");
    assert!(occluded.images().is_empty());

    scene.overlays.clear();
    scene.cursor_owner = CursorOwner::Pane(ROOT);
    let restored = PresentedScene::compose(&scene).expect("restored media");
    assert_eq!(restored.images(), visible.images());
}

#[test]
fn a_partial_overlay_splits_and_crops_the_visible_image_without_reuploading_pixels() {
    let geometry = TerminalGeometry::new(2, 6, 10, 20);
    let pixels = [
        255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
    ];
    let command = format!(
        "\x1b_Ga=T,f=32,s=4,v=1,i=17,p=23,c=4,r=1,q=2;{}\x1b\\",
        base64(&pixels)
    );
    let engine = image_engine(geometry, command.as_bytes());
    let mut store = PaneMediaStore::new(MediaLimits::default());
    let mut scene = scene_with_media(ROOT, GridPoint::new(0, 0), geometry, &engine, &mut store);
    let mut overlay = GhosttyEngine::new(1, 2).expect("partial overlay");
    overlay.advance(b"XX").expect("draw partial overlay");
    scene.overlays.push(SceneOverlay::new(
        SceneSurface::new(OVERLAY, GridPoint::new(0, 1), overlay.normalized_snapshot()),
        i32::MAX,
    ));

    let composed = PresentedScene::compose(&scene).expect("partially occluded image");
    assert_eq!(composed.images().len(), 2);
    assert_eq!(composed.images()[0].grid_rect.cols, 1);
    assert_eq!(composed.images()[1].grid_rect.cols, 1);
    let mut fragments = composed
        .images()
        .iter()
        .map(|image| {
            (
                image.grid_rect.origin.col,
                image.source_x,
                image.source_width,
            )
        })
        .collect::<Vec<_>>();
    fragments.sort_unstable();
    assert_eq!(fragments, vec![(0, 0, 1), (3, 3, 1)]);
    assert_eq!(
        composed
            .images()
            .iter()
            .map(|image| image.image_id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1,
        "both cropped placements must share one retained upload"
    );

    let mut renderer = FullSceneVtRenderer::new(RenderCapabilities {
        kitty_graphics: true,
        ..RenderCapabilities::default()
    });
    let batch = renderer
        .render(&scene, &SceneDamage::Full, &PresentedScene::blank(geometry))
        .expect("render partially occluded image");
    let mut oracle = RenderOracle::new(geometry).expect("partial image oracle");
    oracle
        .verify("partial-image-occlusion", &batch.predicted, &batch)
        .expect("cropped placements match outer terminal");
}

#[test]
fn hyperlink_lifetime_survives_incremental_transactions_and_has_an_explicit_fallback() {
    let geometry = TerminalGeometry::from_cells(3, 24);
    let mut source = image_engine(geometry, b"\x1b]8;;https://example.test/open\x1b\\ABC");
    let scene = text_scene(ROOT, geometry, &source);
    let mut renderer = IncrementalVtRenderer::new(RenderCapabilities {
        hyperlinks: true,
        ..RenderCapabilities::default()
    });
    let mut oracle = RenderOracle::new(geometry).expect("hyperlink oracle");
    let initial = renderer
        .render(&scene, &SceneDamage::Full, &PresentedScene::blank(geometry))
        .expect("initial linked render");
    assert!(
        initial.transactions[0]
            .bytes
            .windows(b"https://example.test/open".len())
            .any(|window| window == b"https://example.test/open")
    );
    assert!(
        initial.transactions[0]
            .bytes
            .windows(b"\x1b]8;;\x1b\\".len())
            .any(|window| window == b"\x1b]8;;\x1b\\"),
        "an open source hyperlink must still close at the outer transaction boundary"
    );
    oracle
        .verify("hyperlink-open-initial", &initial.predicted, &initial)
        .expect("initial hyperlink state");
    renderer.confirm(&initial.predicted);

    let update = source.advance(b"D").expect("extend open source hyperlink");
    let next_scene = text_scene(ROOT, geometry, &source);
    let damage = SceneDamage::from_terminal_update(&next_scene.panes[0], &update, geometry);
    let incremental = renderer
        .render(&next_scene, &damage, &initial.predicted)
        .expect("incremental linked render");
    assert!(
        incremental.transactions[0]
            .bytes
            .windows(b"https://example.test/open".len())
            .any(|window| window == b"https://example.test/open")
    );
    assert!(
        incremental.transactions[0]
            .bytes
            .ends_with(b"\x1b]8;;\x1b\\\x1b[0m")
            || incremental.transactions[0]
                .bytes
                .windows(b"\x1b]8;;\x1b\\".len())
                .any(|window| window == b"\x1b]8;;\x1b\\")
    );
    oracle
        .verify(
            "hyperlink-open-incremental",
            &incremental.predicted,
            &incremental,
        )
        .expect("incremental hyperlink state");

    let mut text_only = FullSceneVtRenderer::new(RenderCapabilities {
        hyperlinks: false,
        ..RenderCapabilities::default()
    });
    let fallback = text_only
        .render(
            &next_scene,
            &SceneDamage::Full,
            &PresentedScene::blank(geometry),
        )
        .expect("non-hyperlink fallback");
    assert!(
        fallback
            .predicted
            .clone()
            .into_terminal_snapshot()
            .rows
            .iter()
            .flat_map(|row| row.cells.iter())
            .all(|cell| cell.hyperlink.is_none())
    );
    assert!(
        !fallback.transactions[0]
            .bytes
            .windows(b"https://example.test/open".len())
            .any(|window| window == b"https://example.test/open")
    );
}

#[test]
fn text_damage_with_unchanged_images_is_incremental_and_never_reuploads_pixels() {
    let geometry = TerminalGeometry::new(3, 12, 10, 20);
    let mut source = image_engine(geometry, ONE_PIXEL_RGBA);
    let mut store = PaneMediaStore::new(MediaLimits::default());
    let first_scene = scene_with_media(ROOT, GridPoint::new(0, 0), geometry, &source, &mut store);
    let mut renderer = IncrementalVtRenderer::new(RenderCapabilities {
        kitty_graphics: true,
        ..RenderCapabilities::default()
    });
    let first = renderer
        .render(
            &first_scene,
            &SceneDamage::Full,
            &PresentedScene::blank(geometry),
        )
        .expect("initial image render");
    renderer.confirm(&first.predicted);
    let mut oracle = RenderOracle::new(geometry).expect("incremental media oracle");
    oracle
        .verify("incremental-media-initial", &first.predicted, &first)
        .expect("initial media state");

    let update = source
        .advance(b"\x1b[2;1Hchanged")
        .expect("text update beside retained image");
    let next_scene = scene_with_media(ROOT, GridPoint::new(0, 0), geometry, &source, &mut store);
    let damage = SceneDamage::from_terminal_update(&next_scene.panes[0], &update, geometry);
    let next = renderer
        .render(&next_scene, &damage, &first.predicted)
        .expect("increment text around image");

    assert!(matches!(
        renderer.last_strategy(),
        RenderStrategy::Incremental | RenderStrategy::SemanticFastPath
    ));
    assert!(
        next.transactions.iter().all(|transaction| !transaction
            .bytes
            .windows(3)
            .any(|window| window == b"\x1b_G")),
        "unchanged image uploads and placements must remain untouched"
    );
    oracle
        .verify("incremental-media-text", &next.predicted, &next)
        .expect("text-only update preserves physical media");
}

#[test]
fn replacing_an_upload_releases_stale_outer_data_before_installing_the_new_pixels() {
    let geometry = TerminalGeometry::new(3, 12, 10, 20);
    let red = format!(
        "\x1b_Ga=T,f=32,s=1,v=1,i=7,p=9,c=1,r=1,q=2;{}\x1b\\",
        base64(&[255, 0, 0, 255])
    );
    let blue = format!(
        "\x1b_Ga=T,f=32,s=1,v=1,i=7,p=9,c=1,r=1,q=2;{}\x1b\\",
        base64(&[0, 0, 255, 255])
    );
    let first_source = image_engine(geometry, red.as_bytes());
    let second_source = image_engine(geometry, blue.as_bytes());
    let mut store = PaneMediaStore::new(MediaLimits::default());
    let first_scene = scene_with_media(
        ROOT,
        GridPoint::new(0, 0),
        geometry,
        &first_source,
        &mut store,
    );
    let mut renderer = IncrementalVtRenderer::new(RenderCapabilities {
        kitty_graphics: true,
        ..RenderCapabilities::default()
    });
    let first = renderer
        .render(
            &first_scene,
            &SceneDamage::Full,
            &PresentedScene::blank(geometry),
        )
        .expect("render first upload");
    renderer.confirm(&first.predicted);
    let old_outer_id = first.predicted.images()[0].image_id;
    let mut oracle = RenderOracle::new(geometry).expect("replacement oracle");
    oracle
        .verify("media-replacement-initial", &first.predicted, &first)
        .expect("initial upload state");

    let second_scene = scene_with_media(
        ROOT,
        GridPoint::new(0, 0),
        geometry,
        &second_source,
        &mut store,
    );
    let second = renderer
        .render(&second_scene, &SceneDamage::Full, &first.predicted)
        .expect("replace upload");
    let emitted = second
        .transactions
        .iter()
        .flat_map(|transaction| transaction.bytes.iter().copied())
        .collect::<Vec<_>>();
    let stale_delete = format!("\x1b_Ga=d,d=I,i={old_outer_id}\x1b\\");

    assert!(
        emitted
            .windows(stale_delete.len())
            .any(|window| window == stale_delete.as_bytes())
    );
    assert!(emitted.windows(6).any(|window| window == b"\x1b_Ga=t"));
    assert_ne!(second.predicted.images()[0].image_id, old_outer_id);
    oracle
        .verify("media-replacement-final", &second.predicted, &second)
        .expect("stale upload deleted and replacement rendered");
}

#[test]
fn image_store_churn_is_bounded_and_cleans_up_without_retaining_stale_payloads() {
    let geometry = TerminalGeometry::new(3, 12, 10, 20);
    let mut store = PaneMediaStore::new(MediaLimits {
        maximum_image_bytes: 1024,
        maximum_pane_bytes: 4096,
        maximum_scene_bytes: 4096,
        maximum_placements: 16,
    });
    for image_id in 1..=500u32 {
        let byte = u8::try_from(image_id % 255).expect("bounded byte");
        let payload = base64(&[byte, 0, 0, 255]);
        let command =
            format!("\x1b_Ga=T,f=32,s=1,v=1,i={image_id},p={image_id},c=1,r=1,q=2;{payload}\x1b\\");
        let mut engine = image_engine(geometry, command.as_bytes());
        let placements = engine.kitty_image_placements().expect("churn placement");
        store.synchronize(&placements).expect("bounded churn sync");
        assert!(store.total_bytes() <= 4096);
        assert_eq!(store.upload_count(), 1);
        assert_eq!(store.placement_count(), 1);
        engine
            .advance(b"\x1b_Ga=d,d=A\x1b\\")
            .expect("churn delete");
        store
            .synchronize(
                &engine
                    .kitty_image_placements()
                    .expect("empty churn placements"),
            )
            .expect("cleanup churn sync");
        assert_eq!(store.total_bytes(), 0);
    }
}

#[test]
fn application_harness_routes_media_hyperlinks_effects_and_overlay_cleanup_through_one_writer() {
    let geometry = TerminalGeometry::new(4, 20, 10, 20);
    let mut harness = Harness::new_scheduled(4, 20).expect("application harness");
    let mut profile = PhysicalTerminalProfile::conservative(geometry);
    profile.hyperlinks = true;
    profile.kitty_graphics = true;
    harness.set_physical_profile(profile);
    harness
        .resize_with_geometry(geometry)
        .expect("set harness pixel geometry");
    let source = [
        b"\x1b]2;media harness\x1b\\\x1b]7;file://localhost/tmp/media\x1b\\".as_slice(),
        b"\x1b]8;;https://example.test/media\x1b\\link\x1b]8;;\x1b\\".as_slice(),
        b"\x1b]9;4;1;42\x1b\\\x1b]52;c;Y29weQ==\x1b\\".as_slice(),
        ONE_PIXEL_RGBA,
    ]
    .concat();
    harness
        .handle_pty_output(&source)
        .expect("queue source text, effects, and media");
    harness.tick(4).expect("reach scheduler boundary");
    let mut output_cursor = 0usize;
    let mut oracle = RenderOracle::new(geometry).expect("application media oracle");
    let visible_render = verify_harness_render(
        &mut harness,
        &mut oracle,
        &mut output_cursor,
        "application-media-visible",
        1,
    );
    assert!(
        visible_render
            .windows(6)
            .any(|window| window == b"\x1b_Ga=t")
    );
    assert_eq!(harness.clipboard_text(), Some("copy"));
    let visible_output = &harness.terminal_output()[..output_cursor];
    assert!(visible_output.windows(3).any(|window| window == b"_Ga"));
    assert!(
        visible_output
            .windows(b"https://example.test/media".len())
            .any(|window| window == b"https://example.test/media")
    );
    assert!(
        visible_output
            .windows(b"\x1b]9;4;1;42\x1b\\".len())
            .any(|window| window == b"\x1b]9;4;1;42\x1b\\")
    );
    assert!(
        !visible_output
            .windows(b"Y29weQ==".len())
            .any(|window| window == b"Y29weQ==")
    );

    harness
        .handle_terminal_input(b"\x1br")
        .expect("open Review over image source");
    harness.tick(4).expect("reach overlay boundary");
    let hidden_render = verify_harness_render(
        &mut harness,
        &mut oracle,
        &mut output_cursor,
        "application-media-occluded",
        0,
    );
    assert!(
        hidden_render
            .windows(10)
            .any(|window| window == b"\x1b_Ga=d,d=a")
    );
    assert!(
        !hidden_render
            .windows(6)
            .any(|window| window == b"\x1b_Ga=t")
    );

    harness
        .handle_terminal_input(b"q")
        .expect("close Review overlay");
    harness.tick(4).expect("reach restore boundary");
    let restored_render = verify_harness_render(
        &mut harness,
        &mut oracle,
        &mut output_cursor,
        "application-media-restored",
        1,
    );
    assert!(
        restored_render
            .windows(6)
            .any(|window| window == b"\x1b_Ga=p")
    );
    assert!(
        !restored_render
            .windows(6)
            .any(|window| window == b"\x1b_Ga=t"),
        "the retained outer upload must be reused after an overlay closes"
    );
    assert!(harness.application_input().is_empty());
}

#[test]
fn media_created_behind_review_is_uploaded_once_and_placed_only_after_dismissal() {
    let geometry = TerminalGeometry::new(4, 20, 10, 20);
    let mut harness = Harness::new_scheduled(4, 20).expect("application harness");
    let mut profile = PhysicalTerminalProfile::conservative(geometry);
    profile.kitty_graphics = true;
    harness.set_physical_profile(profile);
    harness
        .resize_with_geometry(geometry)
        .expect("set harness pixel geometry");
    let mut oracle = RenderOracle::new(geometry).expect("hidden media oracle");
    let mut output_cursor = 0usize;

    harness
        .handle_terminal_input(b"\x1br")
        .expect("open Review before media exists");
    harness.tick(4).expect("reach initial overlay boundary");
    verify_harness_render(
        &mut harness,
        &mut oracle,
        &mut output_cursor,
        "hidden-media-overlay-initial",
        0,
    );

    harness
        .handle_pty_output(ONE_PIXEL_RGBA)
        .expect("create image behind Review");
    harness.tick(4).expect("reach hidden media boundary");
    let hidden = verify_harness_render(
        &mut harness,
        &mut oracle,
        &mut output_cursor,
        "hidden-media-uploaded",
        0,
    );
    assert!(hidden.windows(6).any(|window| window == b"\x1b_Ga=t"));
    assert!(!hidden.windows(6).any(|window| window == b"\x1b_Ga=p"));

    harness
        .handle_terminal_input(b"q")
        .expect("dismiss Review after hidden upload");
    harness
        .tick(4)
        .expect("reach hidden media restore boundary");
    let restored = verify_harness_render(
        &mut harness,
        &mut oracle,
        &mut output_cursor,
        "hidden-media-restored",
        1,
    );
    assert!(restored.windows(6).any(|window| window == b"\x1b_Ga=p"));
    assert!(!restored.windows(6).any(|window| window == b"\x1b_Ga=t"));
    assert!(harness.application_input().is_empty());
}

fn verify_harness_render(
    harness: &mut Harness,
    oracle: &mut RenderOracle,
    output_cursor: &mut usize,
    name: &str,
    expected_images: usize,
) -> Vec<u8> {
    let report = harness
        .drain_scheduled_output(false)
        .expect("drain application render");
    assert_eq!(report.completed_renders.len(), 1, "{name}");
    let predicted = report.completed_renders[0].predicted.clone();
    assert_eq!(predicted.images().len(), expected_images, "{name}");
    let output = harness.terminal_output();
    let bytes = &output[*output_cursor..];
    let batch = RenderBatch::new(vec![OutputTransaction::new(bytes)], predicted.clone());
    oracle
        .verify(name, &predicted, &batch)
        .unwrap_or_else(|error| panic!("{name}: {error}"));
    *output_cursor = output.len();
    bytes.to_vec()
}

fn text_scene(owner: SurfaceId, geometry: TerminalGeometry, engine: &GhosttyEngine) -> Scene {
    let mut scene = Scene::new(geometry);
    let snapshot = engine.normalized_snapshot();
    scene.effects.title.clone_from(&snapshot.title);
    scene
        .effects
        .working_directory
        .clone_from(&snapshot.working_directory);
    scene
        .panes
        .push(SceneSurface::new(owner, GridPoint::new(0, 0), snapshot));
    scene.cursor_owner = CursorOwner::Pane(owner);
    scene
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        encoded.push(TABLE[usize::from(a >> 2)] as char);
        encoded.push(TABLE[usize::from((a & 0x03) << 4 | b >> 4)] as char);
        encoded.push(if chunk.len() > 1 {
            TABLE[usize::from((b & 0x0f) << 2 | c >> 6)] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            TABLE[usize::from(c & 0x3f)] as char
        } else {
            '='
        });
    }
    encoded
}
