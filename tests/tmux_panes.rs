use lector::{
    app::{App, Clock},
    presentation::{
        CursorOwner, IncrementalVtRenderer, OutputTransaction, PresentedScene, RenderBatch,
        RenderCapabilities, RenderOracle, RenderStrategy, RendererBackend, SceneDamage,
    },
    screen_reader::ScreenReader,
    speech,
    terminal::{Color, ScreenIdentity, TerminalDamage, TerminalGeometry, UpdateSummary},
    tmux_control::CommandStatus,
    tmux_model::{PaneId, SessionId, TmuxTopology, WindowId},
    tmux_panes::{TmuxLayout, TmuxPaneSet},
    views,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    cell::Cell,
    io::{Read, Write},
    path::PathBuf,
    rc::Rc,
    sync::mpsc,
    thread,
};

const LEFT_RIGHT: &str = "abcd,80x24,0,0{40x24,0,0,20,39x24,41,0,21}";
const TOP_BOTTOM: &str = "abcd,80x24,0,0[80x12,0,0,20,80x11,0,13,21]";
const NESTED: &str = concat!(
    "abcd,80x24,0,0{40x24,0,0,20,",
    "39x24,41,0[39x12,41,0,21,39x11,41,13,22]}"
);

fn topology_with_layout(layout: &str, visible_layout: &str) -> TmuxTopology {
    let lines = [
        b"S\t$1\twork".to_vec(),
        format!("W\t$1\t@10\t1\t1\t{layout}\t{visible_layout}\t*\teditor").into_bytes(),
        b"P\t@10\t%20\t1\t1\t0\t0\t40\t24\t0\t2\t1\t1\t0\t0\t0\t2\tleft".to_vec(),
        b"P\t@10\t%21\t2\t0\t41\t0\t39\t24\t0\t3\t2\t1\t2\t0\t0\t0\tright".to_vec(),
        b"A\t$1".to_vec(),
        b"O\tbase-index\t1".to_vec(),
        b"O\tpane-base-index\t1".to_vec(),
    ];
    let mut topology = TmuxTopology::new(1);
    topology.replace_inventory(&lines).unwrap();
    topology
}

fn bootstrap_all(panes: &mut TmuxPaneSet, requests: &[lector::tmux_panes::BootstrapRequest]) {
    for request in requests {
        let text = match request.pane_id {
            PaneId(20) => b"left pane".as_slice(),
            PaneId(21) => b"right pane".as_slice(),
            PaneId(22) => b"bottom pane".as_slice(),
            _ => b"pane".as_slice(),
        };
        panes
            .apply_bootstrap(
                request.pane_id,
                CommandStatus::Success,
                &[text.to_vec()],
                100,
            )
            .unwrap();
    }
}

#[test]
fn active_pane_bootstraps_first_and_background_sessions_do_not_delay_readiness() {
    let background_layout = "b25f,80x24,0,0,20";
    let active_layout = "b260,80x24,0,0,99";
    let lines = [
        b"S\t$1\tbackground".to_vec(),
        b"S\t$2\tactive".to_vec(),
        format!("W\t$1\t@10\t1\t1\t{background_layout}\t{background_layout}\t*\tbackground")
            .into_bytes(),
        format!("W\t$2\t@11\t1\t1\t{active_layout}\t{active_layout}\t*\tactive").into_bytes(),
        b"P\t@10\t%20\t1\t1\t0\t0\t80\t24\t0\t0\t0\t1\t0\t0\t0\t0\tbackground".to_vec(),
        b"P\t@11\t%99\t1\t1\t0\t0\t80\t24\t0\t0\t0\t1\t0\t0\t0\t0\tactive".to_vec(),
        b"A\t$2".to_vec(),
    ];
    let mut topology = TmuxTopology::new(1);
    topology.replace_inventory(&lines).unwrap();
    let mut view = views::TmuxConnectionView::new(24, 80, 1);

    let requests = view.sync_topology(&topology).unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.pane_id)
            .collect::<Vec<_>>(),
        vec![PaneId(99), PaneId(20)]
    );
    assert!(!view.is_ready());

    view.apply_bootstrap(
        PaneId(99),
        CommandStatus::Success,
        &[b"ncarpenter:~$".to_vec()],
        100,
    )
    .unwrap();
    assert!(view.is_ready());
    let contents = views::ViewController::model(&mut view).contents_full();
    assert!(contents.contains("ncarpenter:~$"));
    assert!(!contents.contains("background"));
}

#[test]
fn tmux_pane_engines_discard_shadow_terminal_replies_before_and_after_bootstrap() {
    let topology = topology_with_layout(LEFT_RIGHT, LEFT_RIGHT);
    let mut panes = TmuxPaneSet::new(1);
    panes.reconcile(&topology).unwrap();

    assert!(
        panes
            .process_output(PaneId(20), b"\x1b[c\x1b[>q")
            .unwrap()
            .is_none()
    );
    panes
        .apply_bootstrap(PaneId(20), CommandStatus::Error, &[], 100)
        .unwrap();
    panes
        .apply_bootstrap(
            PaneId(21),
            CommandStatus::Success,
            &[b"right pane".to_vec()],
            100,
        )
        .unwrap();
    assert!(
        panes
            .pending_update(PaneId(20))
            .unwrap()
            .pty_replies
            .is_empty(),
        "prebootstrap shadow replies remained pending"
    );

    let update = panes
        .process_output(PaneId(20), b"\x1b[c\x1b[>c\x1b[>q")
        .unwrap()
        .unwrap();
    assert!(
        !update.pty_replies.is_empty(),
        "the controller needs this batch's replies to extract OSC 10/11 reports"
    );
    assert!(
        panes
            .pending_update(PaneId(20))
            .unwrap()
            .pty_replies
            .is_empty(),
        "a tmux pane shadow retained duplicate replies after bootstrap"
    );
}

#[test]
fn parses_one_split_nested_and_zoom_layouts_with_internal_borders() {
    let one = TmuxLayout::parse("b25f,80x24,0,0,20").unwrap();
    assert_eq!(one.panes().len(), 1);
    assert_eq!(one.panes()[0].pane_id, PaneId(20));
    assert_eq!(one.panes()[0].rows, 24);
    assert_eq!(one.panes()[0].cols, 80);

    let left_right = TmuxLayout::parse(LEFT_RIGHT).unwrap();
    assert_eq!(left_right.panes().len(), 2);
    assert_eq!(left_right.pane(PaneId(21)).unwrap().origin.col, 41);
    let vertical_border = left_right.border_snapshot(TerminalGeometry::from_cells(24, 80));
    assert_eq!(vertical_border.cell(4, 40).unwrap().contents(), "│");

    let top_bottom = TmuxLayout::parse(TOP_BOTTOM).unwrap();
    assert_eq!(top_bottom.pane(PaneId(21)).unwrap().origin.row, 13);
    let horizontal_border = top_bottom.border_snapshot(TerminalGeometry::from_cells(24, 80));
    assert_eq!(horizontal_border.cell(12, 17).unwrap().contents(), "─");

    let nested = TmuxLayout::parse(NESTED).unwrap();
    assert_eq!(nested.panes().len(), 3);
    let border = nested.border_snapshot(TerminalGeometry::from_cells(24, 80));
    assert_eq!(border.cell(12, 40).unwrap().contents(), "├");
    assert_eq!(border.cell(12, 55).unwrap().contents(), "─");

    let zoom = TmuxLayout::parse("beef,80x24,0,0,21").unwrap();
    assert_eq!(zoom.panes()[0].pane_id, PaneId(21));
    assert!(TmuxLayout::parse("bad layout").is_err());
    assert!(TmuxLayout::parse("beef,0x24,0,0,21").is_err());
    assert!(
        TmuxLayout::parse("beef,10x5,0,0{5x5,0,0,1,5x5,0,0,2}").is_err(),
        "overlapping split children were accepted"
    );
    assert!(TmuxLayout::parse(&format!("beef,1x1,0,0{}", "[".repeat(200))).is_err());
}

#[test]
fn parses_floating_panes_and_orders_them_bottom_to_top() {
    let layout = TmuxLayout::parse(
        "511d,100x30,0,0[100x30,0,0,20,40x10,5,4,21,20x5,20,2,22]<40x10,5,4,21,20x5,20,2,22>",
    )
    .unwrap();
    assert_eq!(
        layout
            .panes()
            .iter()
            .map(|pane| pane.pane_id)
            .collect::<Vec<_>>(),
        vec![PaneId(20), PaneId(22), PaneId(21)],
        "the `<...>` suffix is top-to-bottom while scene composition is bottom-to-top"
    );
    assert_eq!(layout.pane(PaneId(21)).unwrap().origin.col, 5);
    assert_eq!(layout.pane(PaneId(22)).unwrap().origin.row, 2);
    let borders = layout.border_snapshot(TerminalGeometry::from_cells(30, 100));
    assert!(
        borders
            .rows
            .iter()
            .all(|row| { row.cells.iter().all(|cell| cell.contents().is_empty()) })
    );
}

#[test]
fn pane_engines_keep_partial_sequences_owned_across_active_pane_switches() {
    let mut topology = topology_with_layout(LEFT_RIGHT, LEFT_RIGHT);
    let mut panes = TmuxPaneSet::new(1);
    let requests = panes.reconcile(&topology).unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].command.ends_with(b"-t %20\n"));
    assert!(requests[1].command.ends_with(b"-t %21\n"));
    bootstrap_all(&mut panes, &requests);

    panes.process_output(PaneId(20), b"\x1b[31").unwrap();
    topology
        .apply_notification(b"window-pane-changed", b"@10 %21")
        .unwrap();
    panes.process_output(PaneId(21), b" only-right").unwrap();
    panes.process_output(PaneId(20), b"mRED").unwrap();

    let left = panes.pane_view(PaneId(20)).unwrap().screen();
    assert_eq!(left.cell(1, 2).unwrap().contents(), "R");
    assert_eq!(left.cell(1, 2).unwrap().fgcolor(), Color::Indexed(1));
    assert!(!left.contents_full().contains("only-right"));
    assert!(
        panes
            .pane_view(PaneId(21))
            .unwrap()
            .screen()
            .contents_full()
            .contains("only-right")
    );

    let scene = panes
        .compose(&topology, TerminalGeometry::from_cells(24, 80))
        .unwrap();
    assert_eq!(scene.panes.len(), 3, "border plus two pane surfaces");
    assert_eq!(
        scene.cursor_owner,
        CursorOwner::Pane(panes.surface_id(PaneId(21)).unwrap())
    );
}

#[test]
fn preinventory_output_is_bounded_across_unknown_pane_ids() {
    let mut panes = TmuxPaneSet::new(1);
    let mut rejected = false;
    for pane_id in 0..10_000 {
        if panes.process_output(PaneId(pane_id), b"x").is_err() {
            rejected = true;
            break;
        }
    }
    assert!(
        rejected,
        "unknown pane IDs retained unbounded bootstrap state"
    );
}

#[test]
fn bootstrap_seeds_history_cursor_screen_and_starts_accessibility_quiet() {
    let layout = "cafe,10x3,0,0,20";
    let lines = [
        b"S\t$1\twork".to_vec(),
        format!("W\t$1\t@10\t1\t1\t{layout}\t{layout}\tZ\tzoom").into_bytes(),
        b"P\t@10\t%20\t1\t1\t0\t0\t10\t3\t0\t4\t1\t0\t2\t1\t0\t2\talt".to_vec(),
        b"A\t$1".to_vec(),
    ];
    let mut topology = TmuxTopology::new(1);
    topology.replace_inventory(&lines).unwrap();
    let mut panes = TmuxPaneSet::new(1);
    let requests = panes.reconcile(&topology).unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].command, b"capture-pane -p -e -F -J -t %20\n",
        "the default grid is the displayed alternate screen; -a selects tmux's saved grid"
    );

    panes
        .process_output(PaneId(20), b"pre-capture duplicate")
        .unwrap();
    panes
        .apply_bootstrap(
            PaneId(20),
            CommandStatus::Success,
            &[b"one".to_vec(), b"two".to_vec(), b"three".to_vec()],
            200,
        )
        .unwrap();
    let view = panes.pane_view(PaneId(20)).unwrap();
    assert_eq!(view.screen().screen, ScreenIdentity::Alternate);
    assert_eq!(view.screen().cursor_position(), (1, 4));
    assert!(!view.screen().cursor.visible);
    assert!(
        !view
            .screen()
            .contents_full()
            .contains("pre-capture duplicate")
    );
    assert_eq!(
        panes.pending_update(PaneId(20)),
        Some(&UpdateSummary::default())
    );

    let primary_layout = "cafe,10x3,0,0,20";
    let primary_lines = [
        b"S\t$1\twork".to_vec(),
        format!("W\t$1\t@10\t1\t1\t{primary_layout}\t{primary_layout}\t*\tmain").into_bytes(),
        b"P\t@10\t%20\t1\t1\t0\t0\t10\t3\t0\t0\t2\t1\t0\t0\t0\t2\tmain".to_vec(),
        b"A\t$1".to_vec(),
    ];
    let mut primary = TmuxTopology::new(2);
    primary.replace_inventory(&primary_lines).unwrap();
    let mut primary_panes = TmuxPaneSet::new(2);
    let request = primary_panes.reconcile(&primary).unwrap().remove(0);
    primary_panes
        .apply_bootstrap(
            request.pane_id,
            CommandStatus::Success,
            &[
                b"history one".to_vec(),
                b"history two".to_vec(),
                b"visible one".to_vec(),
                b"visible two".to_vec(),
                b"visible three".to_vec(),
            ],
            200,
        )
        .unwrap();
    assert!(
        primary_panes
            .pane_view(PaneId(20))
            .unwrap()
            .scrollback_len()
            >= 2
    );
}

#[test]
fn empty_bootstrap_capture_preserves_output_that_arrived_during_bootstrap() {
    let layout = "cafe,20x4,0,0,20";
    let lines = [
        b"S\t$1\tdev".to_vec(),
        format!("W\t$1\t@10\t1\t1\t{layout}\t{layout}\t*\tbash").into_bytes(),
        b"P\t@10\t%20\t1\t1\t0\t0\t20\t4\t0\t0\t0\t1\t0\t0\t0\t0\tbash".to_vec(),
        b"A\t$1".to_vec(),
    ];
    let mut topology = TmuxTopology::new(1);
    topology.replace_inventory(&lines).unwrap();
    let mut panes = TmuxPaneSet::new(1);
    let request = panes.reconcile(&topology).unwrap().remove(0);

    panes
        .process_output(request.pane_id, b"ncarpenter:~$ ")
        .unwrap();
    panes
        .apply_bootstrap(request.pane_id, CommandStatus::Success, &[], 200)
        .unwrap();

    assert!(
        panes
            .pane_view(request.pane_id)
            .unwrap()
            .contents_full()
            .contains("ncarpenter:~$")
    );
}

#[test]
fn hidden_window_state_survives_output_partial_sequences_and_switching() {
    let first = "aaaa,20x5,0,0,20";
    let second = "bbbb,20x5,0,0,30";
    let lines = [
        b"S\t$1\twork".to_vec(),
        format!("W\t$1\t@10\t1\t1\t{first}\t{first}\t*\tone").into_bytes(),
        format!("W\t$1\t@11\t2\t0\t{second}\t{second}\t-\ttwo").into_bytes(),
        b"P\t@10\t%20\t1\t1\t0\t0\t20\t5\t0\t0\t0\t1\t0\t0\t0\t0\tone".to_vec(),
        b"P\t@11\t%30\t1\t1\t0\t0\t20\t5\t0\t0\t0\t1\t0\t0\t0\t0\ttwo".to_vec(),
        b"A\t$1".to_vec(),
    ];
    let mut topology = TmuxTopology::new(1);
    topology.replace_inventory(&lines).unwrap();
    let mut panes = TmuxPaneSet::new(1);
    let requests = panes.reconcile(&topology).unwrap();
    bootstrap_all(&mut panes, &requests);
    panes.process_output(PaneId(30), b"hidden \x1b[3").unwrap();
    panes.process_output(PaneId(20), b"visible").unwrap();
    assert_eq!(
        panes
            .compose(&topology, TerminalGeometry::from_cells(5, 20))
            .unwrap()
            .cursor_owner,
        CursorOwner::Pane(panes.surface_id(PaneId(20)).unwrap())
    );

    topology
        .apply_notification(b"session-window-changed", b"$1 @11")
        .unwrap();
    panes.process_output(PaneId(30), b"1mRED").unwrap();
    let scene = panes
        .compose(&topology, TerminalGeometry::from_cells(5, 20))
        .unwrap();
    assert_eq!(
        scene.cursor_owner,
        CursorOwner::Pane(panes.surface_id(PaneId(30)).unwrap())
    );
    let intended = PresentedScene::compose(&scene).unwrap();
    assert!(intended.row_text(0).contains("hidden"));

    let replacement = topology_with_layout(LEFT_RIGHT, LEFT_RIGHT);
    panes.reconcile(&replacement).unwrap();
    assert!(
        panes.pane_view(PaneId(30)).is_none(),
        "closed pane engine leaked"
    );
}

#[test]
fn closing_a_pane_drops_its_terminal_engine_and_media_without_touching_survivors() {
    let mut topology = topology_with_layout(LEFT_RIGHT, LEFT_RIGHT);
    let mut panes = TmuxPaneSet::new(1);
    let requests = panes.reconcile(&topology).unwrap();
    bootstrap_all(&mut panes, &requests);
    let image = b"\x1b_Ga=T,f=32,s=1,v=1,i=7,p=9,c=1,r=1,q=2;/wAA/w==\x1b\\";
    panes.process_output(PaneId(20), image).unwrap();
    assert_eq!(
        panes
            .compose(&topology, TerminalGeometry::from_cells(24, 80))
            .unwrap()
            .images
            .len(),
        1
    );

    let layout = "beef,80x24,0,0,21";
    topology
        .replace_inventory(&[
            b"S\t$1\twork".to_vec(),
            format!("W\t$1\t@10\t1\t1\t{layout}\t{layout}\t*\teditor").into_bytes(),
            b"P\t@10\t%21\t1\t1\t0\t0\t80\t24\t0\t3\t2\t1\t2\t0\t0\t0\tright".to_vec(),
            b"A\t$1".to_vec(),
        ])
        .unwrap();
    assert!(panes.reconcile(&topology).unwrap().is_empty());

    assert!(panes.pane_view(PaneId(20)).is_none());
    assert!(panes.pane_view(PaneId(21)).is_some());
    assert!(
        panes
            .compose(&topology, TerminalGeometry::from_cells(24, 80))
            .unwrap()
            .images
            .is_empty()
    );
}

#[test]
fn split_scene_and_incremental_pane_updates_match_the_ghostty_render_oracle() {
    let topology = topology_with_layout(LEFT_RIGHT, LEFT_RIGHT);
    let mut panes = TmuxPaneSet::new(1);
    let requests = panes.reconcile(&topology).unwrap();
    bootstrap_all(&mut panes, &requests);
    let geometry = TerminalGeometry::from_cells(24, 80);
    let initial = panes.compose(&topology, geometry).unwrap();
    let initial_intended = PresentedScene::compose(&initial).unwrap();
    assert!(!initial_intended.row_text(23).contains("status"));

    let mut renderer = IncrementalVtRenderer::new(RenderCapabilities::default());
    let blank = PresentedScene::blank(geometry);
    let first = renderer
        .render(&initial, &SceneDamage::Full, &blank)
        .unwrap();
    let mut oracle = RenderOracle::new(geometry).unwrap();
    oracle
        .verify("tmux-split-initial", &initial_intended, &first)
        .unwrap();
    renderer.confirm(&first.predicted);

    let update = panes
        .process_output(PaneId(21), b"\x1b[5;6Hupdated")
        .unwrap()
        .expect("bootstrapped pane update");
    assert!(matches!(update.damage, TerminalDamage::Rows(_)));
    let next = panes.compose(&topology, geometry).unwrap();
    let surface = next
        .panes
        .iter()
        .find(|surface| surface.id == panes.surface_id(PaneId(21)).unwrap())
        .unwrap();
    let damage = SceneDamage::from_terminal_update(surface, &update, geometry);
    let batch = renderer.render(&next, &damage, &first.predicted).unwrap();
    assert_ne!(renderer.last_strategy(), RenderStrategy::FullFallback);
    oracle
        .verify(
            "tmux-split-pane-update",
            &PresentedScene::compose(&next).unwrap(),
            &batch,
        )
        .unwrap();
}

#[test]
fn model_selects_attached_session_active_window_and_visible_zoom_layout() {
    let mut topology = topology_with_layout(LEFT_RIGHT, "beef,80x24,0,0,21");
    let mut panes = TmuxPaneSet::new(1);
    let requests = panes.reconcile(&topology).unwrap();
    bootstrap_all(&mut panes, &requests);
    let scene = panes
        .compose(&topology, TerminalGeometry::from_cells(24, 80))
        .unwrap();
    assert_eq!(scene.panes.len(), 2, "border plus only the zoomed pane");
    assert_eq!(
        scene.cursor_owner,
        CursorOwner::Pane(panes.surface_id(PaneId(21)).unwrap())
    );

    assert_eq!(
        topology
            .apply_notification(
                b"layout-change",
                b"@10 abcd,80x24,0,0{40x24,0,0,20,39x24,41,0,21} beef,80x24,0,0,20 Z",
            )
            .unwrap(),
        lector::tmux_model::ReconcileOutcome::ResyncRequired
    );
    let changed = panes
        .compose(&topology, TerminalGeometry::from_cells(24, 80))
        .unwrap();
    assert_eq!(
        changed.cursor_owner,
        CursorOwner::Pane(panes.surface_id(PaneId(20)).unwrap())
    );
    assert_eq!(topology.attached_session(), Some(SessionId(1)));
    assert_eq!(
        topology.session(SessionId(1)).unwrap().active_window,
        Some(WindowId(10))
    );
}

#[derive(Default)]
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

#[test]
fn application_harness_bootstraps_and_incrementally_renders_real_control_records() {
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(24, 80)));
    let clock = TestClock::default();
    let mut app = App::new_with_clock(stack, Box::new(clock.clone())).unwrap();
    let mut sr = ScreenReader::new(speech::Speech::new(Box::<SilentDriver>::default()));
    let mut physical = Vec::new();
    let mut control_input = Vec::new();

    app.handle_pty(
        &mut sr,
        b"\x1bP1000p%begin 1 1 0\n%end 1 1 0\n",
        &mut physical,
    )
    .unwrap();
    app.handle_tick(&mut sr, &mut control_input, &mut physical)
        .unwrap();
    assert_eq!(
        control_input,
        [
            lector::app::TMUX_FLOW_CONTROL_COMMAND,
            lector::app::TMUX_FLOW_CONTROL_VERIFY_COMMAND,
            b"refresh-client -C 80x24\n",
            lector::tmux_model::INVENTORY_COMMAND.as_bytes(),
        ]
        .concat()
    );
    app.handle_pty(&mut sr, b"%begin 2 2 0\n%end 2 2 0\n", &mut physical)
        .unwrap();
    app.handle_pty(
        &mut sr,
        b"%begin 3 3 0\nattached,control-mode,pause-after=1\n%end 3 3 0\n",
        &mut physical,
    )
    .unwrap();
    app.handle_pty(&mut sr, b"%begin 4 4 0\n%end 4 4 0\n", &mut physical)
        .unwrap();

    let inventory_groups = [
        vec![b"S\t$1\twork".to_vec()],
        vec![format!("W\t$1\t@10\t1\t1\t{LEFT_RIGHT}\t{LEFT_RIGHT}\t*\teditor").into_bytes()],
        vec![
            b"P\t@10\t%20\t1\t1\t0\t0\t40\t24\t0\t0\t0\t1\t0\t0\t0\t0\tleft".to_vec(),
            b"P\t@10\t%21\t2\t0\t41\t0\t39\t24\t0\t0\t0\t1\t0\t0\t0\t0\tright".to_vec(),
        ],
        vec![b"A\t$1".to_vec()],
        vec![b"O\tbase-index\t1".to_vec()],
        vec![b"O\tpane-base-index\t1".to_vec()],
        vec![b"C\tclient_name\t/dev/ttys001".to_vec()],
        vec![b"O\tprefix\tC-a".to_vec()],
        vec![b"O\tprefix2\tNone".to_vec()],
        vec![b"O\tkey-table\troot".to_vec()],
        vec![b"O\trepeat-time\t500".to_vec()],
        vec![b"B\tn\t0\tnext-window".to_vec()],
    ];
    for (index, group) in inventory_groups.iter().enumerate() {
        let output = group
            .iter()
            .map(|line| String::from_utf8(line.clone()).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let response = format!(
            "%begin {} {} 0\n{output}\n%end {} {} 0\n",
            index + 5,
            index + 5,
            index + 5,
            index + 5
        );
        app.handle_pty(&mut sr, response.as_bytes(), &mut physical)
            .unwrap();
    }

    control_input.clear();
    app.handle_tick(&mut sr, &mut control_input, &mut physical)
        .unwrap();
    assert_eq!(
        String::from_utf8(control_input.clone()).unwrap(),
        "capture-pane -p -e -F -J -S - -t %20\n\
         capture-pane -p -e -F -J -S - -t %21\n"
    );

    app.handle_pty(
        &mut sr,
        format!("%layout-change @10 {LEFT_RIGHT} {LEFT_RIGHT} *\n").as_bytes(),
        &mut physical,
    )
    .unwrap();

    app.handle_pty(
        &mut sr,
        b"%begin 20 20 0\nleft bootstrap\n%end 20 20 0\n",
        &mut physical,
    )
    .unwrap();
    assert!(
        app.debug_active_view_contents()
            .contains("tmux connection is active")
    );
    app.handle_pty(
        &mut sr,
        b"%begin 21 21 0\nright bootstrap\n%end 21 21 0\n",
        &mut physical,
    )
    .unwrap();
    control_input.clear();
    app.handle_tick(&mut sr, &mut control_input, &mut physical)
        .unwrap();
    assert_eq!(
        control_input,
        lector::tmux_model::INVENTORY_COMMAND.as_bytes(),
        "a resync queued behind bootstrap replies lost command correlation"
    );
    for (index, group) in inventory_groups.iter().enumerate() {
        let output = group
            .iter()
            .map(|line| String::from_utf8(line.clone()).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let response = format!(
            "%begin {} {} 0\n{output}\n%end {} {} 0\n",
            index + 30,
            index + 30,
            index + 30,
            index + 30
        );
        app.handle_pty(&mut sr, response.as_bytes(), &mut physical)
            .unwrap();
    }
    assert!(app.debug_active_view_contents().contains("left bootstrap"));

    let scene = app.composed_scene().unwrap();
    let intended = PresentedScene::compose(&scene).unwrap();
    let batch = RenderBatch::new(vec![OutputTransaction::new(&physical)], intended.clone());
    let mut oracle = RenderOracle::new(scene.geometry).unwrap();
    oracle
        .verify("tmux-app-bootstrap", &intended, &batch)
        .unwrap();

    let before = physical.len();
    app.handle_pty(
        &mut sr,
        b"%output %21 \\033[5;6Hlive-right\n",
        &mut physical,
    )
    .unwrap();
    let updated_scene = app.composed_scene().unwrap();
    let updated = PresentedScene::compose(&updated_scene).unwrap();
    assert!(updated.row_text(4).contains("live-right"));
    let update_batch = RenderBatch::new(
        vec![OutputTransaction::new(&physical[before..])],
        updated.clone(),
    );
    oracle
        .verify("tmux-app-live-output", &updated, &update_batch)
        .unwrap();
    assert!(
        !physical[before..].windows(4).any(|bytes| bytes == b"%out"),
        "control record leaked to the physical terminal"
    );

    app.handle_pty(&mut sr, b"%output %20 active-read\n", &mut physical)
        .unwrap();
    clock.advance(u128::from(lector::app::DIFF_DELAY));
    assert!(
        app.maybe_finalize_changes(&mut sr).unwrap(),
        "active tmux pane changes never reached the accessibility finalizer"
    );

    app.handle_stdin(
        &mut sr,
        b"\x1b[114;3:1u\x1b[114;3:3u",
        &mut control_input,
        &mut physical,
    )
    .unwrap();
    assert!(app.debug_active_view_contents().contains("active-read"));
    app.handle_pty(&mut sr, b"%output %20 review-hidden\n", &mut physical)
        .unwrap();
    assert!(
        !app.debug_active_view_contents().contains("review-hidden"),
        "frozen review changed while pane output continued"
    );
    app.handle_stdin(&mut sr, b"q", &mut control_input, &mut physical)
        .unwrap();
    assert!(app.debug_active_view_contents().contains("review-hidden"));

    app.show_tmux_gateway(1, &mut sr, &mut physical).unwrap();
    app.handle_pty(&mut sr, b"%output %20 hidden-update\n", &mut physical)
        .unwrap();
    app.handle_stdin(&mut sr, b"\r", &mut control_input, &mut physical)
        .unwrap();
    assert!(app.debug_active_view_contents().contains("hidden-update"));
}

fn drive_real_tmux_until(
    case: &str,
    app: &mut App,
    sr: &mut ScreenReader,
    receiver: &mpsc::Receiver<Vec<u8>>,
    writer: &mut dyn Write,
    physical: &mut Vec<u8>,
    mut ready: impl FnMut(&mut App) -> bool,
) {
    if ready(app) {
        return;
    }
    let result = super::drive_real_tmux_phase(|remaining| {
        let chunk = receiver.recv_timeout(remaining)?;
        app.handle_pty(sr, &chunk, physical).unwrap();
        let mut commands = Vec::new();
        app.handle_tick(sr, &mut commands, physical).unwrap();
        if !commands.is_empty() {
            match writer.write_all(&commands) {
                Ok(()) => {
                    if let Err(error) = writer.flush()
                        && error.kind() != std::io::ErrorKind::BrokenPipe
                        && error.raw_os_error() != Some(5)
                    {
                        panic!("flush real tmux pane PTY in {case}: {error}");
                    }
                }
                Err(error)
                    if error.kind() != std::io::ErrorKind::BrokenPipe
                        && error.raw_os_error() != Some(5) =>
                {
                    panic!("write real tmux pane PTY in {case}: {error}");
                }
                Err(_) => {}
            }
        }
        Ok::<_, mpsc::RecvTimeoutError>(ready(app))
    });
    if let Err(error) = result {
        panic!(
            "failed to reach {case}: {error:?}; active={:?}; topology={:?}",
            app.debug_active_view_contents(),
            app.debug_tmux_topology(1)
        );
    }
}

fn verify_app_scene(case: &str, app: &mut App, physical: &[u8]) {
    let scene = app.composed_scene().unwrap();
    let intended = PresentedScene::compose(&scene).unwrap();
    let batch = RenderBatch::new(vec![OutputTransaction::new(physical)], intended.clone());
    RenderOracle::new(scene.geometry)
        .unwrap()
        .verify(case, &intended, &batch)
        .unwrap_or_else(|error| panic!("{case}: {error}"));
}

#[test]
fn real_tmux_split_resize_zoom_close_and_bootstrap_match_the_render_oracle() {
    let _serial = super::serialize_real_tmux_test();
    let tmux = std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .expect("tmux integration tests require tmux on PATH");
    assert!(tmux.status.success(), "tmux -V failed");

    let socket_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-tmux");
    std::fs::create_dir_all(&socket_dir).unwrap();
    let socket = socket_dir.join(format!("panes-{}.sock", std::process::id()));
    let session = format!("lector_panes_{}", std::process::id());
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new("tmux");
    command.args([
        "-S",
        socket.to_str().unwrap(),
        "-f",
        "/dev/null",
        "-CC",
        "new-session",
        "-s",
        &session,
        "printf 'first-pane\\n'; exec cat",
    ]);
    command.env("TERM", "xterm-256color");
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let (sender, receiver) = mpsc::channel();
    let read_thread = thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if sender.send(buffer[..count].to_vec()).is_err() {
                        break;
                    }
                }
                Err(error) if error.raw_os_error() == Some(5) => break,
                Err(error) => panic!("read real tmux pane PTY: {error}"),
            }
        }
    });

    let stack = views::ViewStack::new(Box::new(views::PtyView::new(24, 80)));
    let mut app = App::new(stack).unwrap();
    let mut sr = ScreenReader::new(speech::Speech::new(Box::<SilentDriver>::default()));
    let mut physical = Vec::new();
    drive_real_tmux_until(
        "initial bootstrap",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| app.debug_active_view_contents().contains("first-pane"),
    );
    assert_eq!(app.composed_scene().unwrap().panes.len(), 2);
    verify_app_scene("real-tmux-one-pane", &mut app, &physical);

    let before_split = physical.len();
    writer
        .write_all(b"split-window -h -d \"printf 'second-pane\\n'; exec cat\"\n")
        .unwrap();
    writer.flush().unwrap();
    drive_real_tmux_until(
        "split",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| {
            let Ok(scene) = app.composed_scene() else {
                return false;
            };
            let presented = PresentedScene::compose(&scene).unwrap();
            scene.panes.len() == 3
                && (0..scene.geometry.rows)
                    .any(|row| presented.row_text(row).contains("second-pane"))
        },
    );
    assert!(
        !physical[before_split..]
            .windows(b"\x1b[2J".len())
            .any(|bytes| bytes == b"\x1b[2J"),
        "split used a flicker-prone full-terminal clear"
    );
    verify_app_scene("real-tmux-split", &mut app, &physical);

    let split_widths = app
        .composed_scene()
        .unwrap()
        .panes
        .iter()
        .skip(1)
        .map(|surface| surface.snapshot.geometry.cols)
        .collect::<Vec<_>>();
    let before_resize = physical.len();
    writer.write_all(b"resize-pane -t %0 -R 5\n").unwrap();
    writer.flush().unwrap();
    drive_real_tmux_until(
        "resize",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| {
            app.composed_scene().is_ok_and(|scene| {
                let widths = scene
                    .panes
                    .iter()
                    .skip(1)
                    .map(|surface| surface.snapshot.geometry.cols)
                    .collect::<Vec<_>>();
                scene.panes.len() == 3 && widths != split_widths
            })
        },
    );
    assert!(
        !physical[before_resize..]
            .windows(b"\x1b[2J".len())
            .any(|bytes| bytes == b"\x1b[2J"),
        "pane resize used a flicker-prone full-terminal clear"
    );
    verify_app_scene("real-tmux-resize", &mut app, &physical);

    let before_zoom = physical.len();
    writer.write_all(b"resize-pane -Z -t %1\n").unwrap();
    writer.flush().unwrap();
    drive_real_tmux_until(
        "zoom",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| {
            app.composed_scene()
                .is_ok_and(|scene| scene.panes.len() == 2)
        },
    );
    assert!(
        !physical[before_zoom..]
            .windows(b"\x1b[2J".len())
            .any(|bytes| bytes == b"\x1b[2J"),
        "zoom used a flicker-prone full-terminal clear"
    );
    verify_app_scene("real-tmux-zoom", &mut app, &physical);

    let before_close = physical.len();
    writer.write_all(b"resize-pane -Z -t %1\n").unwrap();
    writer.write_all(b"kill-pane -t %1\n").unwrap();
    writer.flush().unwrap();
    drive_real_tmux_until(
        "close",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| {
            app.composed_scene()
                .is_ok_and(|scene| scene.panes.len() == 2)
                && app.debug_active_view_contents().contains("first-pane")
                && app
                    .debug_tmux_topology(1)
                    .is_some_and(|dump| !dump.contains("pane %1"))
        },
    );
    assert!(
        !physical[before_close..]
            .windows(b"\x1b[2J".len())
            .any(|bytes| bytes == b"\x1b[2J"),
        "pane close used a flicker-prone full-terminal clear"
    );
    verify_app_scene("real-tmux-close", &mut app, &physical);

    writer.write_all(b"kill-server\n").unwrap();
    writer.flush().unwrap();
    drive_real_tmux_until(
        "exit",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| app.tmux_connection_count() == 0,
    );
    let _ = child.wait().unwrap();
    read_thread.join().unwrap();
    let _ = std::fs::remove_file(&socket);
}
