use lector::{
    app::{App, Clock},
    presentation::{
        CursorOwner, GridPoint, OutputTransaction, PresentedScene, RenderBatch, RenderOracle,
        Scene, SceneImagePlacement, SceneOverlay, SceneSurface, SurfaceId,
    },
    screen_reader::ScreenReader,
    speech,
    terminal::{GhosttyEngine, ScreenIdentity},
    views::{self, PopupResponse},
};
use std::{cell::Cell, rc::Rc};

#[derive(Clone, Default)]
struct FakeClock(Rc<Cell<u128>>);

impl Clock for FakeClock {
    fn now_ms(&self) -> u128 {
        self.0.get()
    }
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

fn application(rows: u16, cols: u16) -> (App, ScreenReader) {
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(rows, cols)));
    let app = App::new_with_clock(stack, Box::new(FakeClock::default())).expect("application");
    let reader = ScreenReader::new(speech::Speech::new(Box::new(SilentDriver)));
    (app, reader)
}

#[test]
fn live_compositor_represents_root_message_and_review_as_distinct_scene_layers() {
    let (mut app, mut reader) = application(6, 30);
    let mut pty_input = Vec::new();
    let mut physical = Vec::new();

    app.handle_pty(&mut reader, b"root-before", &mut physical)
        .expect("render root");
    app.show_message(&mut reader, "Notice", "message layer", &mut physical)
        .expect("push message");
    app.handle_stdin(&mut reader, b"\x1br", &mut pty_input, &mut physical)
        .expect("push review over message");
    app.handle_pty(
        &mut reader,
        b"\x1b[?1049h\x1b[2J\x1b[Hroot-after",
        &mut physical,
    )
    .expect("advance hidden root");

    let scene = app.composed_scene().expect("compose layered scene");
    assert_eq!(scene.panes.len(), 1);
    assert_eq!(scene.overlays.len(), 2);
    assert_eq!(scene.panes[0].id, SurfaceId(1));
    assert!(scene.panes[0].snapshot.contents().contains("root-after"));
    assert_eq!(scene.panes[0].snapshot.screen, ScreenIdentity::Alternate);
    assert!(
        scene.overlays[0]
            .surface
            .snapshot
            .contents()
            .contains("message layer")
    );
    assert!(
        scene.overlays[1]
            .surface
            .snapshot
            .contents()
            .contains("message layer")
    );
    assert_eq!(scene.cursor_owner, CursorOwner::Overlay(SurfaceId(3)));
    assert!(
        scene
            .overlays
            .iter()
            .all(|overlay| overlay.surface.snapshot.screen == ScreenIdentity::Primary)
    );
    assert!(pty_input.is_empty());
}

#[test]
fn nested_overlay_push_pop_and_hidden_output_match_the_ghostty_physical_oracle() {
    let (mut app, mut reader) = application(6, 30);
    app.enable_output_scheduler(Default::default());
    let mut pty_input = Vec::new();
    let mut physical = Vec::new();
    let mut oracle = GhosttyEngine::new(6, 30).expect("physical oracle");

    app.handle_pty(&mut reader, b"root-before", &mut physical)
        .expect("queue root");
    drain(&mut app, &mut physical, &mut oracle);
    app.show_message(&mut reader, "Notice", "message layer", &mut physical)
        .expect("queue message");
    drain(&mut app, &mut physical, &mut oracle);
    app.handle_stdin(&mut reader, b"\x1br", &mut pty_input, &mut physical)
        .expect("queue review");
    drain(&mut app, &mut physical, &mut oracle);

    app.handle_pty(&mut reader, b"\x1b[2J\x1b[Hroot-after", &mut physical)
        .expect("update hidden root");
    drain(&mut app, &mut physical, &mut oracle);
    assert!(
        oracle
            .normalized_snapshot()
            .contents()
            .contains("message layer")
    );
    assert!(
        !oracle
            .normalized_snapshot()
            .contents()
            .contains("root-after")
    );

    app.handle_stdin(&mut reader, b"q", &mut pty_input, &mut physical)
        .expect("pop review");
    drain(&mut app, &mut physical, &mut oracle);
    assert!(
        oracle
            .normalized_snapshot()
            .contents()
            .contains("message layer")
    );
    app.handle_stdin(&mut reader, b"\r", &mut pty_input, &mut physical)
        .expect("pop message");
    drain(&mut app, &mut physical, &mut oracle);
    assert!(
        oracle
            .normalized_snapshot()
            .contents()
            .contains("root-after")
    );
    assert_eq!(
        app.composed_scene()
            .expect("compose popped scene")
            .overlays
            .len(),
        0
    );
    assert!(pty_input.is_empty());
}

#[test]
fn review_table_setup_freezes_a_compositor_layer_but_keeps_the_source_engine_live() {
    let (mut app, mut reader) = application(5, 30);
    let mut pty_input = Vec::new();
    let mut physical = Vec::new();

    app.handle_pty(&mut reader, b"Name  Value\r\none   1\x1b[H", &mut physical)
        .expect("draw table");
    app.handle_stdin(&mut reader, b"\x1br", &mut pty_input, &mut physical)
        .expect("enter review");
    app.handle_stdin(&mut reader, b"gT", &mut pty_input, &mut physical)
        .expect("enter review table setup");
    assert_eq!(reader.input_mode().as_str(), "normal");

    app.handle_pty(&mut reader, b"\x1b[2J\x1b[Hnew source state", &mut physical)
        .expect("update source during setup");
    let scene = app.composed_scene().expect("compose table scene");
    assert_eq!(scene.overlays.len(), 1);
    assert!(
        scene.panes[0]
            .snapshot
            .contents()
            .contains("new source state")
    );
    assert!(
        scene.overlays[0]
            .surface
            .snapshot
            .contents()
            .contains("Name  Value")
    );

    app.handle_stdin(&mut reader, b"\x1b[27;1u", &mut pty_input, &mut physical)
        .expect("cancel review table setup");
    assert_eq!(reader.input_mode().as_str(), "normal");
    assert_eq!(
        app.composed_scene()
            .expect("compose review after setup cancellation")
            .overlays
            .len(),
        1
    );
    app.handle_stdin(&mut reader, b"q", &mut pty_input, &mut physical)
        .expect("leave review");
    assert_eq!(
        app.composed_scene()
            .expect("compose live source")
            .overlays
            .len(),
        0
    );
    assert!(
        app.debug_active_view_contents()
            .contains("new source state")
    );
    assert!(pty_input.is_empty());
}

#[test]
fn reviewable_popups_distinguish_dismiss_confirm_and_cancel_responses() {
    let (mut app, mut reader) = application(6, 32);
    let mut pty_input = Vec::new();
    let mut physical = Vec::new();

    app.show_popup_announcement(&mut reader, "tmux message", "window renamed", &mut physical)
        .expect("show announcement");
    assert!(
        app.composed_scene()
            .expect("compose announcement")
            .overlays
            .len()
            == 1
    );
    app.on_resize(7, 40, &mut physical)
        .expect("resize announcement");
    let resized = app.composed_scene().expect("compose resized popup");
    assert_eq!(resized.geometry.rows, 7);
    assert_eq!(resized.overlays[0].surface.snapshot.geometry.rows, 7);
    app.handle_stdin(&mut reader, b"\x1br", &mut pty_input, &mut physical)
        .expect("review popup");
    assert!(app.debug_active_view_contents().contains("window renamed"));
    app.handle_stdin(&mut reader, b"q", &mut pty_input, &mut physical)
        .expect("leave popup review");
    app.handle_stdin(&mut reader, b"\r", &mut pty_input, &mut physical)
        .expect("dismiss announcement");
    assert_eq!(app.take_popup_response(), Some(PopupResponse::Dismissed));

    app.show_popup_error(&mut reader, "tmux error", "command failed", &mut physical)
        .expect("show error");
    app.handle_stdin(&mut reader, b"\x1b[27;1u", &mut pty_input, &mut physical)
        .expect("dismiss error");
    assert_eq!(app.take_popup_response(), Some(PopupResponse::Dismissed));

    app.show_popup_confirmation(&mut reader, "Confirm", "kill pane %7?", &mut physical)
        .expect("show confirmation");
    app.handle_stdin(&mut reader, b"\x1b[27;1u", &mut pty_input, &mut physical)
        .expect("cancel confirmation");
    assert_eq!(app.take_popup_response(), Some(PopupResponse::Cancelled));

    app.show_popup_confirmation(&mut reader, "Confirm", "kill pane %7?", &mut physical)
        .expect("show confirmation again");
    app.handle_stdin(&mut reader, b"\n", &mut pty_input, &mut physical)
        .expect("accept confirmation");
    assert_eq!(app.take_popup_response(), Some(PopupResponse::Confirmed));
    assert!(!app.has_overlay());
    assert!(pty_input.is_empty());
}

#[test]
fn lua_repl_and_help_preserve_their_input_behavior_while_source_output_advances() {
    let (mut app, mut reader) = application(6, 32);
    let mut pty_input = Vec::new();
    let mut physical = Vec::new();

    app.handle_stdin(&mut reader, b"\x1bL1+1\r", &mut pty_input, &mut physical)
        .expect("open and submit to Lua REPL");
    app.handle_tick(&mut reader, &mut pty_input, &mut physical)
        .expect("finish Lua expression");
    assert!(app.debug_active_view_contents().contains('2'));
    app.handle_pty(
        &mut reader,
        b"\x1b[2J\x1b[Hhidden while Lua is open",
        &mut physical,
    )
    .expect("advance source behind Lua");
    let lua_scene = app.composed_scene().expect("compose Lua scene");
    assert_eq!(lua_scene.overlays.len(), 1);
    assert!(
        lua_scene.panes[0]
            .snapshot
            .contents()
            .contains("hidden while Lua is open")
    );
    app.handle_stdin(&mut reader, b"\x1b[27;1u", &mut pty_input, &mut physical)
        .expect("close Lua REPL");
    assert!(
        app.debug_active_view_contents()
            .contains("hidden while Lua is open")
    );

    reader.set_help_mode(true);
    app.handle_pty(
        &mut reader,
        b"\r\nsource remains live in help",
        &mut physical,
    )
    .expect("advance source during help");
    app.handle_stdin(&mut reader, b"\x1bOP", &mut pty_input, &mut physical)
        .expect("leave help with F1");
    assert!(!reader.help_mode());
    assert!(
        app.debug_active_view_contents()
            .contains("source remains live")
    );
    assert!(pty_input.is_empty());
}

#[test]
fn image_state_beneath_an_overlay_remains_logical_but_is_physically_occluded() {
    let geometry = lector::terminal::TerminalGeometry::new(3, 12, 10, 20);
    let image = b"\x1b_Ga=T,f=32,s=1,v=1,i=7,p=9,c=1,r=1,q=2;/wAA/w==\x1b\\";
    let root_bytes = [b"base".as_slice(), image].concat();
    let mut root = GhosttyEngine::new(3, 12).expect("root image engine");
    root.resize_with_geometry(geometry).expect("root geometry");
    root.advance(&root_bytes).expect("place root image");
    let root_presented = PresentedScene::from_engine(&root).expect("capture root image");
    let placed = root_presented.images()[0].clone();
    let origin = placed.grid_rect.origin;

    let mut overlay = GhosttyEngine::new(1, 2).expect("overlay engine");
    overlay.advance(b"X").expect("draw overlay cell");
    let overlay_id = SurfaceId(2);
    let mut scene = Scene::new(geometry);
    scene.panes.push(SceneSurface::new(
        SurfaceId(1),
        GridPoint::new(0, 0),
        root.normalized_snapshot(),
    ));
    scene.overlays.push(SceneOverlay::new(
        SceneSurface::new(overlay_id, origin, overlay.normalized_snapshot()),
        10,
    ));
    scene.cursor_owner = CursorOwner::Overlay(overlay_id);
    scene.images.push(SceneImagePlacement {
        owner: SurfaceId(1),
        image: placed,
    });
    let intended = PresentedScene::compose(&scene).expect("compose image and overlay");
    assert_eq!(
        scene.images.len(),
        1,
        "the source placement remains retained"
    );
    assert!(
        intended.images().is_empty(),
        "the overlay occludes physical media"
    );

    let overlay_bytes = format!("\x1b[{};{}HX", origin.row + 1, origin.col + 1);
    let transaction = [
        root_bytes,
        b"\x1b_Ga=d,d=A\x1b\\".to_vec(),
        overlay_bytes.into_bytes(),
    ]
    .concat();
    let mut oracle = RenderOracle::new(geometry).expect("image overlay oracle");
    oracle
        .verify(
            "compositor-image-beneath-overlay",
            &intended,
            &RenderBatch::new(vec![OutputTransaction::new(transaction)], intended.clone()),
        )
        .expect("overlay occludes retained underlying image state");
}

fn drain(app: &mut App, physical: &mut Vec<u8>, oracle: &mut GhosttyEngine) {
    physical.clear();
    let report = app
        .drain_scheduled_output(physical, true)
        .expect("drain scheduled scene");
    assert_eq!(report.completed_renders.len(), 1);
    oracle.advance(physical).expect("parse physical output");
    let intended = app.presented_scene().clone().into_terminal_snapshot();
    let actual = oracle.normalized_snapshot();
    assert_eq!(actual.contents_full(), intended.contents_full());
    assert_eq!(actual.cursor, intended.cursor);
    assert_eq!(actual.screen, intended.screen);
    assert_eq!(actual.modes, intended.modes);
    assert_eq!(
        actual.title.as_deref().unwrap_or_default(),
        intended.title.as_deref().unwrap_or_default()
    );
}
