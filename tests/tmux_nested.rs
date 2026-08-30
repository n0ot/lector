use lector::{
    app::App,
    screen_reader::ScreenReader,
    speech,
    tmux_lifecycle::{ConnectionHierarchy, GatewayControlAction, GatewayOrigin, LifecycleError},
    tmux_model::INVENTORY_REPLY_COUNT,
    views,
};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    cell::{Cell, RefCell},
    io::{Read, Write},
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc,
    thread,
    time::{Duration, SystemTime},
};

const SPLIT_LAYOUT: &str = "abcd,80x24,0,0{40x24,0,0,20,39x24,41,0,21}";
const SINGLE_LAYOUT: &str = "b25f,80x24,0,0,20";
const REMEMBERED_SPLIT_LAYOUT: &str = "abcd,80x24,0,0{40x24,0,0,22,39x24,41,0,23}";

#[derive(Default)]
struct SilentDriver;

#[derive(Clone, Default)]
struct TestClock(Rc<Cell<u128>>);

impl TestClock {
    fn set(&self, now_ms: u128) {
        self.0.set(now_ms);
    }
}

impl lector::app::Clock for TestClock {
    fn now_ms(&self) -> u128 {
        self.0.get()
    }
}

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

fn app() -> (App, ScreenReader, Vec<u8>) {
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(24, 80)));
    let app = App::new(stack).unwrap();
    let sr = ScreenReader::new(speech::Speech::new(Box::<SilentDriver>::default()));
    (app, sr, Vec::new())
}

fn app_with_clock() -> (App, ScreenReader, TestClock, Vec<u8>) {
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(24, 80)));
    let clock = TestClock::default();
    let app = App::new_with_clock(stack, Box::new(clock.clone())).unwrap();
    let sr = ScreenReader::new(speech::Speech::new(Box::<SilentDriver>::default()));
    (app, sr, clock, Vec::new())
}

fn recording_app() -> (App, ScreenReader, Recorder, Vec<u8>) {
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(24, 80)));
    let app = App::new(stack).unwrap();
    let recorder = Recorder::default();
    let sr = ScreenReader::new(speech::Speech::new(Box::new(recorder.clone())));
    (app, sr, recorder, Vec::new())
}

fn inventory(split: bool, name: &str, client: &str) -> Vec<Vec<Vec<u8>>> {
    let layout = if split { SPLIT_LAYOUT } else { SINGLE_LAYOUT };
    let panes = if split {
        vec![
            b"P\t@10\t%20\t1\t1\t0\t0\t40\t24\t0\t6\t0\t1\t0\t0\t0\t0\tgateway".to_vec(),
            b"P\t@10\t%21\t2\t0\t41\t0\t39\t24\t0\t7\t0\t1\t0\t0\t0\t0\tsibling".to_vec(),
        ]
    } else {
        vec![b"P\t@10\t%20\t1\t1\t0\t0\t80\t24\t0\t0\t0\t1\t0\t0\t0\t0\tgateway".to_vec()]
    };
    vec![
        vec![format!("S\t$1\t{name}").into_bytes()],
        vec![format!("W\t$1\t@10\t1\t1\t{layout}\t{layout}\t*\t{name}").into_bytes()],
        panes,
        vec![b"A\t$1".to_vec()],
        vec![b"O\tbase-index\t1".to_vec()],
        vec![b"O\tpane-base-index\t1".to_vec()],
        vec![format!("C\tclient_name\t{client}").into_bytes()],
        vec![b"O\tprefix\tC-a".to_vec()],
        vec![b"O\tprefix2\tNone".to_vec()],
        vec![b"O\tkey-table\troot".to_vec()],
        vec![b"O\trepeat-time\t500".to_vec()],
        vec![
            b"B\tn\t0\tnext-window".to_vec(),
            b"B\td\t0\tdetach-client".to_vec(),
            b"B\t:\t0\tcommand-prompt".to_vec(),
        ],
    ]
}

fn inventory_with_remembered_session() -> Vec<Vec<Vec<u8>>> {
    vec![
        vec![b"S\t$1\tbar".to_vec(), b"S\t$2\tfoo".to_vec()],
        vec![
            format!("W\t$1\t@10\t1\t1\t{SINGLE_LAYOUT}\t{SINGLE_LAYOUT}\t*\tbar").into_bytes(),
            format!(
                "W\t$2\t@11\t1\t1\t{REMEMBERED_SPLIT_LAYOUT}\t{REMEMBERED_SPLIT_LAYOUT}\t*\tfoo"
            )
            .into_bytes(),
            b"W\t$2\t@12\t2\t0\tb25f,80x24,0,0,24\tb25f,80x24,0,0,24\t-\tother".to_vec(),
        ],
        vec![
            b"P\t@10\t%20\t1\t1\t0\t0\t80\t24\t0\t0\t0\t1\t0\t0\t0\t0\tgateway".to_vec(),
            b"P\t@11\t%22\t1\t0\t0\t0\t40\t24\t0\t0\t0\t1\t0\t0\t0\t0\tleft".to_vec(),
            b"P\t@11\t%23\t2\t1\t41\t0\t39\t24\t0\t0\t0\t1\t0\t0\t0\t0\tremembered".to_vec(),
            b"P\t@12\t%24\t1\t1\t0\t0\t80\t24\t0\t0\t0\t1\t0\t0\t0\t0\tother".to_vec(),
        ],
        vec![b"A\t$1".to_vec()],
        vec![b"O\tbase-index\t1".to_vec()],
        vec![b"O\tpane-base-index\t1".to_vec()],
        vec![b"C\tclient_name\t/dev/ttys-outer".to_vec()],
        vec![b"O\tprefix\tC-a".to_vec()],
        vec![b"O\tprefix2\tNone".to_vec()],
        vec![b"O\tkey-table\troot".to_vec()],
        vec![b"O\trepeat-time\t500".to_vec()],
        vec![b"B\td\t0\tdetach-client".to_vec()],
    ]
}

fn inventory_with_renumbered_carrier() -> Vec<Vec<Vec<u8>>> {
    vec![
        vec![b"S\t$1\tbar".to_vec(), b"S\t$2\tfoo".to_vec()],
        vec![
            format!("W\t$1\t@10\t1\t1\t{SINGLE_LAYOUT}\t{SINGLE_LAYOUT}\t*\tbar").into_bytes(),
            format!(
                "W\t$2\t@11\t1\t1\t{REMEMBERED_SPLIT_LAYOUT}\t{REMEMBERED_SPLIT_LAYOUT}\t*\tfoo"
            )
            .into_bytes(),
            b"W\t$2\t@12\t2\t0\tb25f,80x24,0,0,24\tb25f,80x24,0,0,24\t-\tother".to_vec(),
            format!("W\t$2\t@10\t3\t0\t{SINGLE_LAYOUT}\t{SINGLE_LAYOUT}\t-\tbar").into_bytes(),
        ],
        vec![
            b"P\t@10\t%20\t1\t1\t0\t0\t80\t24\t0\t0\t0\t1\t0\t0\t0\t0\tgateway".to_vec(),
            b"P\t@11\t%22\t1\t0\t0\t0\t40\t24\t0\t0\t0\t1\t0\t0\t0\t0\tleft".to_vec(),
            b"P\t@11\t%23\t2\t1\t41\t0\t39\t24\t0\t0\t0\t1\t0\t0\t0\t0\tremembered".to_vec(),
            b"P\t@12\t%24\t1\t1\t0\t0\t80\t24\t0\t0\t0\t1\t0\t0\t0\t0\tother".to_vec(),
        ],
        vec![b"A\t$2".to_vec()],
        vec![b"O\tbase-index\t1".to_vec()],
        vec![b"O\tpane-base-index\t1".to_vec()],
        vec![b"C\tclient_name\t/dev/ttys-outer".to_vec()],
        vec![b"O\tprefix\tC-a".to_vec()],
        vec![b"O\tprefix2\tNone".to_vec()],
        vec![b"O\tkey-table\troot".to_vec()],
        vec![b"O\trepeat-time\t500".to_vec()],
        vec![b"B\td\t0\tdetach-client".to_vec()],
    ]
}

fn reply(serial: usize, lines: &[Vec<u8>], success: bool) -> Vec<u8> {
    let mut bytes = format!("%begin {serial} {serial} 0\n").into_bytes();
    for line in lines {
        bytes.extend_from_slice(line);
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(
        format!(
            "%{} {serial} {serial} 0\n",
            if success { "end" } else { "error" }
        )
        .as_bytes(),
    );
    bytes
}

fn octal(bytes: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::new();
    for &byte in bytes {
        if (0x20..=0x7e).contains(&byte) && byte != b'\\' {
            encoded.push(byte);
        } else {
            encoded.extend_from_slice(format!("\\{byte:03o}").as_bytes());
        }
    }
    encoded
}

fn pane_output(pane_id: u64, bytes: &[u8]) -> Vec<u8> {
    let mut record = format!("%output %{pane_id} ").into_bytes();
    record.extend_from_slice(&octal(bytes));
    record.push(b'\n');
    record
}

fn wrap_at_depth(mut bytes: Vec<u8>, depth: usize) -> Vec<u8> {
    for _ in 0..depth {
        bytes = pane_output(20, &bytes);
    }
    bytes
}

fn feed_at_depth(
    app: &mut App,
    sr: &mut ScreenReader,
    physical: &mut Vec<u8>,
    depth: usize,
    bytes: &[u8],
) {
    app.handle_pty(sr, &wrap_at_depth(bytes.to_vec(), depth), physical)
        .unwrap();
}

fn drain_root(app: &mut App) -> Vec<u8> {
    let mut bytes = Vec::new();
    app.drain_tmux_commands_for(1, &mut bytes).unwrap();
    bytes
}

fn acknowledge_root_replies(
    app: &mut App,
    sr: &mut ScreenReader,
    physical: &mut Vec<u8>,
    first_serial: usize,
) {
    let count = app.debug_tmux_expected_reply_count(1).unwrap_or_default();
    for serial in first_serial..first_serial + count {
        app.handle_pty(sr, &reply(serial, &[], true), physical)
            .unwrap();
    }
}

fn decode_send_keys(stream: &[u8], pane_id: u64) -> Vec<u8> {
    let prefix = format!("send-keys -H -t %{pane_id} ");
    let mut decoded = Vec::new();
    for command in stream.split_inclusive(|byte| *byte == b'\n') {
        let text = std::str::from_utf8(command).unwrap();
        let payload = text
            .strip_prefix(&prefix)
            .and_then(|text| text.strip_suffix('\n'))
            .unwrap_or_else(|| panic!("unexpected routed command: {text:?}"));
        for byte in payload.split_ascii_whitespace() {
            decoded.push(u8::from_str_radix(byte, 16).unwrap());
        }
    }
    decoded
}

fn split_first_command(stream: &[u8]) -> (&[u8], &[u8]) {
    let end = stream
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|index| index + 1)
        .expect("routed command must end with a newline");
    stream.split_at(end)
}

fn ready_root(app: &mut App, sr: &mut ScreenReader, physical: &mut Vec<u8>) {
    app.handle_pty(
        sr,
        b"root shell\r\n\x1bP1000p%begin 1 1 0\n%end 1 1 0\n",
        physical,
    )
    .unwrap();
    assert_eq!(
        drain_root(app),
        [
            lector::app::TMUX_FLOW_CONTROL_COMMAND,
            lector::app::TMUX_FLOW_CONTROL_VERIFY_COMMAND,
            b"refresh-client -C 80x24\n",
            lector::tmux_model::INVENTORY_COMMAND.as_bytes(),
        ]
        .concat()
    );
    app.handle_pty(sr, &reply(2, &[], true), physical).unwrap();
    app.handle_pty(
        sr,
        &reply(3, &[b"attached,control-mode,pause-after=1".to_vec()], true),
        physical,
    )
    .unwrap();
    app.handle_pty(sr, &reply(4, &[], true), physical).unwrap();
    let groups = inventory(true, "outer", "/dev/ttys-outer");
    assert_eq!(groups.len(), INVENTORY_REPLY_COUNT);
    for (index, group) in groups.iter().enumerate() {
        app.handle_pty(sr, &reply(index + 5, group, true), physical)
            .unwrap();
    }
    assert_eq!(
        drain_root(app),
        b"capture-pane -p -e -F -J -S - -t %20\ncapture-pane -p -e -F -J -S - -t %21\n"
    );
    app.handle_pty(sr, &reply(30, &[b"PARENT".to_vec()], true), physical)
        .unwrap();
    app.handle_pty(sr, &reply(31, &[b"SIBLING".to_vec()], true), physical)
        .unwrap();
}

fn ready_root_with_remembered_session(
    app: &mut App,
    sr: &mut ScreenReader,
    physical: &mut Vec<u8>,
) {
    app.handle_pty(
        sr,
        b"root shell\r\n\x1bP1000p%begin 1 1 0\n%end 1 1 0\n",
        physical,
    )
    .unwrap();
    let _ = drain_root(app);
    app.handle_pty(sr, &reply(2, &[], true), physical).unwrap();
    app.handle_pty(
        sr,
        &reply(3, &[b"attached,control-mode,pause-after=1".to_vec()], true),
        physical,
    )
    .unwrap();
    app.handle_pty(sr, &reply(4, &[], true), physical).unwrap();
    let groups = inventory_with_remembered_session();
    assert_eq!(groups.len(), INVENTORY_REPLY_COUNT);
    for (index, group) in groups.iter().enumerate() {
        app.handle_pty(sr, &reply(index + 5, group, true), physical)
            .unwrap();
    }
    assert_eq!(
        drain_root(app),
        b"capture-pane -p -e -F -J -S - -t %20\ncapture-pane -p -e -F -J -S - -t %22\ncapture-pane -p -e -F -J -S - -t %23\ncapture-pane -p -e -F -J -S - -t %24\n"
    );
    for (serial, contents) in ["BAR", "LEFT", "REMEMBERED", "OTHER"]
        .into_iter()
        .enumerate()
    {
        app.handle_pty(
            sr,
            &reply(serial + 30, &[contents.as_bytes().to_vec()], true),
            physical,
        )
        .unwrap();
    }
}

fn start_nested(app: &mut App, sr: &mut ScreenReader, physical: &mut Vec<u8>, depth: usize) {
    let start = b"\x1bP1000p%begin 40 40 0\n%end 40 40 0\n";
    for byte in start {
        feed_at_depth(app, sr, physical, depth, std::slice::from_ref(byte));
    }
}

fn ready_nested(
    app: &mut App,
    sr: &mut ScreenReader,
    physical: &mut Vec<u8>,
    depth: usize,
    name: &str,
    client: &str,
) {
    let routed_inventory = drain_root(app);
    let mut decoded = routed_inventory;
    for _ in 0..depth {
        decoded = decode_send_keys(&decoded, 20);
    }
    assert_eq!(
        decoded,
        [
            lector::app::TMUX_FLOW_CONTROL_COMMAND,
            lector::app::TMUX_FLOW_CONTROL_VERIFY_COMMAND,
            b"refresh-client -C 80x24\n",
            lector::tmux_model::INVENTORY_COMMAND.as_bytes(),
        ]
        .concat()
    );

    let groups = inventory(false, name, client);
    feed_at_depth(app, sr, physical, depth, &reply(49, &[], true));
    feed_at_depth(
        app,
        sr,
        physical,
        depth,
        &reply(50, &[b"attached,control-mode,pause-after=1".to_vec()], true),
    );
    feed_at_depth(app, sr, physical, depth, &reply(51, &[], true));
    for (index, group) in groups.iter().enumerate() {
        feed_at_depth(app, sr, physical, depth, &reply(52 + index, group, true));
    }
    let routed_capture = drain_root(app);
    let mut decoded = routed_capture;
    for _ in 0..depth {
        decoded = decode_send_keys(&decoded, 20);
    }
    assert_eq!(decoded, b"capture-pane -p -e -F -J -S - -t %20\n");
    feed_at_depth(
        app,
        sr,
        physical,
        depth,
        &reply(80, &[format!("{name}-READY").into_bytes()], true),
    );
}

fn input(app: &mut App, sr: &mut ScreenReader, physical: &mut Vec<u8>, bytes: &[u8]) {
    let mut root = Vec::new();
    app.handle_stdin(sr, bytes, &mut root, physical).unwrap();
    assert!(root.is_empty());
}

#[test]
fn fragmented_child_start_failure_consumes_only_decoded_parent_pane_bytes() {
    let (mut app, mut sr, mut physical) = app();
    ready_root(&mut app, &mut sr, &mut physical);
    let mut child = b"before\x1bP1000p%begin 90 90 0\n%end 90 90 0\n".to_vec();
    child.extend_from_slice(&pane_output(20, &[0, 0xff, b'X']));
    child.extend_from_slice(b"%exit startup failed\n\x1b\\after");

    for byte in child {
        feed_at_depth(
            &mut app,
            &mut sr,
            &mut physical,
            1,
            std::slice::from_ref(&byte),
        );
    }

    assert_eq!(app.tmux_connection_count(), 1);
    assert_eq!(app.active_tmux_connection(), Some(1));
    assert_eq!(app.debug_tmux_pane_portal_target(1, 20), None);
    let parent = app.debug_tmux_pane_contents(1, 20).unwrap();
    assert!(parent.contains("PARENTbeforeafter"), "{parent:?}");
    assert!(
        !parent.contains("1000p") && !parent.contains("%begin"),
        "{parent:?}"
    );
}

#[test]
fn nested_connection_routes_commands_through_its_parent_portal_and_preserves_siblings() {
    let (mut app, mut sr, mut physical) = app();
    ready_root(&mut app, &mut sr, &mut physical);
    start_nested(&mut app, &mut sr, &mut physical, 1);

    assert_eq!(app.tmux_connection_count(), 2);
    assert_eq!(app.active_tmux_connection(), Some(2));
    assert_eq!(
        app.debug_tmux_gateway_origin(2),
        Some(GatewayOrigin::Pane {
            parent_connection_id: 1,
            session_id: 1,
            window_id: 10,
            pane_id: 20,
        })
    );
    assert_eq!(app.debug_tmux_pane_portal_target(1, 20), Some(2));
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        "inner",
        "/dev/ttys-inner",
    );
    assert!(
        app.debug_tmux_pane_contents(2, 20)
            .unwrap()
            .contains("inner-READY")
    );
    assert!(
        app.debug_tmux_pane_contents(1, 21)
            .unwrap()
            .contains("SIBLING")
    );
    assert!(
        app.debug_tmux_pane_contents(1, 20)
            .unwrap()
            .contains("PARENT")
    );

    assert!(
        app.activate_tmux_connection(1, &mut sr, &mut physical)
            .unwrap()
    );
    assert!(
        app.debug_active_view_contents()
            .contains("tmux control mode is running")
    );
    input(&mut app, &mut sr, &mut physical, b"\x01n");
    assert_eq!(
        drain_root(&mut app),
        b"next-window\n",
        "the active pane portal disabled its parent tmux key table"
    );
    app.handle_pty(&mut sr, &reply(90, &[], true), &mut physical)
        .unwrap();
    app.handle_pty(&mut sr, b"%window-pane-changed @10 %21\n", &mut physical)
        .unwrap();
    assert!(app.debug_active_view_contents().contains("SIBLING"));
    input(&mut app, &mut sr, &mut physical, b"Q");
    assert_eq!(decode_send_keys(&drain_root(&mut app), 21), b"Q");
    app.handle_pty(&mut sr, b"%window-pane-changed @10 %20\n", &mut physical)
        .unwrap();
    assert!(
        app.debug_active_view_contents()
            .contains("tmux control mode is running")
    );
    input(&mut app, &mut sr, &mut physical, b"\r");
    assert_eq!(app.active_tmux_connection(), Some(2));
    assert_eq!(
        drain_root(&mut app),
        b"",
        "activating a running carrier must not perturb its stream"
    );

    input(&mut app, &mut sr, &mut physical, b"Z");
    let child_command = decode_send_keys(&drain_root(&mut app), 20);
    assert_eq!(child_command, b"send-keys -H -t %20 5a\n");

    input(&mut app, &mut sr, &mut physical, b"\x01n");
    let child_prefix = decode_send_keys(&drain_root(&mut app), 20);
    assert_eq!(child_prefix, b"next-window\n");

    feed_at_depth(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        &pane_output(20, b"CHILD-LIVE"),
    );
    assert!(
        app.debug_tmux_pane_contents(2, 20)
            .unwrap()
            .contains("CHILD-LIVE")
    );
    assert!(
        !app.debug_tmux_pane_contents(1, 20)
            .unwrap()
            .contains("CHILD-LIVE")
    );

    feed_at_depth(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        b"%exit inner done\n\x1b\\AFTER",
    );
    assert_eq!(app.tmux_connection_count(), 1);
    assert_eq!(app.active_tmux_connection(), Some(1));
    assert_eq!(app.debug_tmux_pane_portal_target(1, 20), None);
    assert!(
        app.debug_tmux_pane_contents(1, 20)
            .unwrap()
            .contains("PARENTAFTER")
    );
}

#[test]
fn parent_controls_render_and_announce_when_focus_returns_to_a_nested_pane_portal() {
    let (mut app, mut sr, recorder, mut physical) = recording_app();
    ready_root(&mut app, &mut sr, &mut physical);
    start_nested(&mut app, &mut sr, &mut physical, 1);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        "inner",
        "/dev/ttys-inner",
    );
    assert!(
        app.activate_tmux_connection(1, &mut sr, &mut physical)
            .unwrap()
    );

    assert!(
        app.show_tmux_session_chooser(&mut sr, &mut physical)
            .unwrap(),
        "the parent session chooser was disabled by its active pane portal"
    );
    assert!(app.debug_active_view_contents().contains("outer"));
    input(&mut app, &mut sr, &mut physical, b"\x1b[27;1u");

    app.handle_pty(&mut sr, b"%window-pane-changed @10 %21\n", &mut physical)
        .unwrap();
    physical.clear();
    recorder.0.borrow_mut().clear();
    app.handle_pty(&mut sr, b"%window-pane-changed @10 %20\n", &mut physical)
        .unwrap();

    assert!(
        !physical.is_empty(),
        "switching back to the portal produced no physical update"
    );
    assert!(
        app.debug_active_view_contents()
            .contains("tmux control mode is running")
    );
    assert!(
        recorder.0.borrow().is_empty(),
        "an unchanged returning pane portal must not replay its cursor line"
    );
}

#[test]
fn carrier_pause_drops_the_nested_protocol_instead_of_resuming_after_loss() {
    let (mut app, mut sr, mut physical) = app();
    ready_root(&mut app, &mut sr, &mut physical);
    start_nested(&mut app, &mut sr, &mut physical, 1);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        "inner",
        "/dev/ttys-inner",
    );
    assert!(
        app.activate_tmux_connection(1, &mut sr, &mut physical)
            .unwrap()
    );
    let _ = drain_root(&mut app);

    assert!(
        app.show_tmux_connection_chooser(&mut sr, &mut physical)
            .unwrap()
    );
    input(&mut app, &mut sr, &mut physical, b"\x1b[B\r");
    assert_eq!(app.active_tmux_connection(), Some(2));
    assert_eq!(
        drain_root(&mut app),
        b"",
        "manager activation perturbed a carrier that had never paused"
    );

    app.handle_pty(&mut sr, b"%pause %20\n", &mut physical)
        .unwrap();
    assert_eq!(app.tmux_connection_count(), 1);
    assert_eq!(app.active_tmux_connection(), Some(1));
    assert!(
        !drain_root(&mut app)
            .windows(b"%20:continue".len())
            .any(|window| window == b"%20:continue"),
        "Lector tried to continue a carrier after tmux confirmed byte loss"
    );
}

#[test]
fn an_inactive_nested_carrier_is_drained_instead_of_flow_paused() {
    let (mut app, mut sr, mut physical) = app();
    ready_root(&mut app, &mut sr, &mut physical);
    start_nested(&mut app, &mut sr, &mut physical, 1);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        "inner",
        "/dev/ttys-inner",
    );
    assert_eq!(app.active_tmux_connection(), Some(2));
    let _ = drain_root(&mut app);

    // The parent is not presented while its child is selected. Enough direct
    // output to cross the ordinary hidden-pane pause threshold must still be
    // consumed immediately because pane %20 carries the child's control
    // protocol as well.
    let direct_carrier_output = vec![b'x'; 20 * 1024];
    app.handle_pty(
        &mut sr,
        &pane_output(20, &direct_carrier_output),
        &mut physical,
    )
    .unwrap();

    let flow = app.debug_tmux_pane_flow_state(1, 20).unwrap();
    assert!(!flow.pause_requested);
    assert!(!flow.is_paused);
    assert_eq!(app.debug_tmux_background_pending_bytes(), 0);
    assert!(drain_root(&mut app).is_empty());
}

#[test]
fn five_hours_of_silence_and_a_covered_ui_do_not_drop_a_nested_connection() {
    let (mut app, mut sr, clock, mut physical) = app_with_clock();
    ready_root(&mut app, &mut sr, &mut physical);
    start_nested(&mut app, &mut sr, &mut physical, 1);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        "inner",
        "/dev/ttys-inner",
    );
    app.activate_tmux_connection(1, &mut sr, &mut physical)
        .unwrap();
    app.show_tmux_connection_chooser(&mut sr, &mut physical)
        .unwrap();

    clock.set(5 * 60 * 60 * 1_000);
    let mut transport = Vec::new();
    app.handle_tick(&mut sr, &mut transport, &mut physical)
        .unwrap();
    assert_eq!(app.tmux_connection_count(), 2);

    feed_at_depth(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        &pane_output(20, b"AFTER-FIVE-HOURS"),
    );
    assert_eq!(app.tmux_connection_count(), 2);
    assert!(
        app.debug_tmux_pane_contents(2, 20)
            .unwrap()
            .contains("AFTER-FIVE-HOURS")
    );
}

#[test]
fn parent_session_switch_links_the_nested_carrier_before_switching() {
    let (mut app, mut sr, mut physical) = app();
    ready_root_with_remembered_session(&mut app, &mut sr, &mut physical);
    start_nested(&mut app, &mut sr, &mut physical, 1);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        "inner",
        "/dev/ttys-inner",
    );
    acknowledge_root_replies(&mut app, &mut sr, &mut physical, 1_000);

    assert!(
        app.activate_tmux_connection(1, &mut sr, &mut physical)
            .unwrap()
    );
    assert!(
        app.show_tmux_session_chooser(&mut sr, &mut physical)
            .unwrap()
    );
    input(&mut app, &mut sr, &mut physical, b"\x1b[B\r");
    assert_eq!(
        drain_root(&mut app),
        b"link-window -d -s @10 -t $2:1000000\n",
        "the client switched before creating a carrier in the destination"
    );
    assert_eq!(app.debug_tmux_expected_reply_count(1), Some(1));
    app.handle_pty(&mut sr, &reply(90, &[], true), &mut physical)
        .unwrap();
    assert_eq!(
        drain_root(&mut app),
        b"list-windows -t $2 -F 'L\t#{window_id}\t#{window_index}'\n"
    );
    app.handle_pty(
        &mut sr,
        &reply(91, &[b"L\t@10\t1000000".to_vec()], true),
        &mut physical,
    )
    .unwrap();
    assert_eq!(drain_root(&mut app), b"switch-client -t $2\n");
    app.handle_pty(&mut sr, b"%session-changed $2 foo\n", &mut physical)
        .unwrap();
    app.handle_pty(&mut sr, &reply(92, &[], true), &mut physical)
        .unwrap();
    let recovery = drain_root(&mut app);
    assert!(
        !recovery
            .windows(b"unlink-window".len())
            .any(|window| window == b"unlink-window"),
        "the active carrier alias was removed immediately: {recovery:?}"
    );
    assert!(
        app.show_tmux_window_chooser(&mut sr, &mut physical)
            .unwrap()
    );
    let chooser = app.debug_active_view_contents();
    assert!(chooser.contains("@11 1 foo"), "{chooser:?}");
    assert!(chooser.contains("@12 2 other"), "{chooser:?}");
    assert!(
        !chooser.contains("1000000") && !chooser.contains("@10"),
        "Lector's carrier alias leaked into its window chooser: {chooser:?}"
    );
    input(&mut app, &mut sr, &mut physical, b"\x1b[27;1u");

    // Activating the child no longer switches its parent back to the original
    // carrier session. The linked window is the same @window and pane stream,
    // so resuming it in foo preserves the exact control-mode byte offset.
    assert!(
        app.activate_tmux_connection(2, &mut sr, &mut physical)
            .unwrap()
    );
    let routed = drain_root(&mut app);
    assert!(
        !routed
            .windows(b"switch-client".len())
            .any(|window| window == b"switch-client"),
        "child activation changed the parent's user session: {routed:?}"
    );
    assert!(
        routed.is_empty(),
        "activating a healthy linked carrier changed its stream: {routed:?}"
    );
}

#[test]
fn carrier_link_collision_retries_inside_the_reserved_high_index_band() {
    let (mut app, mut sr, mut physical) = app();
    ready_root_with_remembered_session(&mut app, &mut sr, &mut physical);
    start_nested(&mut app, &mut sr, &mut physical, 1);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        "inner",
        "/dev/ttys-inner",
    );
    acknowledge_root_replies(&mut app, &mut sr, &mut physical, 1_100);
    app.activate_tmux_connection(1, &mut sr, &mut physical)
        .unwrap();
    app.show_tmux_session_chooser(&mut sr, &mut physical)
        .unwrap();
    input(&mut app, &mut sr, &mut physical, b"\x1b[B\r");
    assert_eq!(
        drain_root(&mut app),
        b"link-window -d -s @10 -t $2:1000000\n"
    );

    app.handle_pty(
        &mut sr,
        &reply(1_200, &[b"index in use".to_vec()], false),
        &mut physical,
    )
    .unwrap();
    assert_eq!(
        drain_root(&mut app),
        b"link-window -d -s @10 -t $2:1000001\n"
    );
    app.handle_pty(&mut sr, &reply(1_201, &[], true), &mut physical)
        .unwrap();
    assert_eq!(
        drain_root(&mut app),
        b"list-windows -t $2 -F 'L\t#{window_id}\t#{window_index}'\n"
    );
    app.handle_pty(
        &mut sr,
        &reply(1_202, &[b"L\t@10\t1000001".to_vec()], true),
        &mut physical,
    )
    .unwrap();
    assert_eq!(drain_root(&mut app), b"switch-client -t $2\n");
}

#[test]
fn failed_carrier_verification_aborts_the_switch_and_queues_guarded_cleanup() {
    let (mut app, mut sr, mut physical) = app();
    ready_root_with_remembered_session(&mut app, &mut sr, &mut physical);
    start_nested(&mut app, &mut sr, &mut physical, 1);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        "inner",
        "/dev/ttys-inner",
    );
    acknowledge_root_replies(&mut app, &mut sr, &mut physical, 1_300);
    app.activate_tmux_connection(1, &mut sr, &mut physical)
        .unwrap();
    app.show_tmux_session_chooser(&mut sr, &mut physical)
        .unwrap();
    input(&mut app, &mut sr, &mut physical, b"\x1b[B\r");
    let _ = drain_root(&mut app);
    app.handle_pty(&mut sr, &reply(1_400, &[], true), &mut physical)
        .unwrap();
    let _ = drain_root(&mut app);
    app.handle_pty(
        &mut sr,
        &reply(1_401, &[b"L\t@99\t1000000".to_vec()], true),
        &mut physical,
    )
    .unwrap();

    let cleanup = drain_root(&mut app);
    assert!(
        cleanup.starts_with(
            b"if-shell -F -t '$2:1000000' '#{==:#{window_id},@10}' 'unlink-window -t $2:1000000'\n"
        ),
        "missing ownership-guarded carrier cleanup: {cleanup:?}"
    );
    assert!(
        !cleanup
            .windows(b"switch-client".len())
            .any(|window| window == b"switch-client"),
        "verification failure still switched sessions: {cleanup:?}"
    );
}

#[test]
fn command_prompt_session_changes_use_the_same_carrier_gate() {
    let (mut app, mut sr, mut physical) = app();
    ready_root_with_remembered_session(&mut app, &mut sr, &mut physical);
    start_nested(&mut app, &mut sr, &mut physical, 1);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        "inner",
        "/dev/ttys-inner",
    );
    acknowledge_root_replies(&mut app, &mut sr, &mut physical, 1_450);
    app.activate_tmux_connection(1, &mut sr, &mut physical)
        .unwrap();
    app.show_tmux_command_prompt(&mut sr, &mut physical)
        .unwrap();
    input(&mut app, &mut sr, &mut physical, b"switch-client -t $2\r");
    assert_eq!(
        drain_root(&mut app),
        b"link-window -d -s @10 -t $2:1000000\n",
        "the command prompt bypassed Lector's carrier transaction"
    );
}

#[test]
fn ambiguous_and_compound_session_commands_cannot_bypass_the_carrier_gate() {
    let (mut app, mut sr, mut physical) = app();
    ready_root_with_remembered_session(&mut app, &mut sr, &mut physical);
    start_nested(&mut app, &mut sr, &mut physical, 1);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        "inner",
        "/dev/ttys-inner",
    );
    acknowledge_root_replies(&mut app, &mut sr, &mut physical, 1_470);
    app.activate_tmux_connection(1, &mut sr, &mut physical)
        .unwrap();

    for command in [
        "new -A -s surprise",
        "display-message safe ; switchc -t $2",
        "switchc -O activity -n",
    ] {
        app.show_tmux_command_prompt(&mut sr, &mut physical)
            .unwrap();
        let mut submission = command.as_bytes().to_vec();
        submission.push(b'\r');
        input(&mut app, &mut sr, &mut physical, &submission);
        assert!(
            drain_root(&mut app).is_empty(),
            "unsafe session command reached tmux: {command}"
        );
        assert!(
            app.has_overlay(),
            "unsafe session command was rejected without an explanation: {command}"
        );
        input(&mut app, &mut sr, &mut physical, b"\x1b[27;1u");
    }
}

#[test]
fn switching_back_to_the_original_carrier_session_unlinks_only_the_owned_alias() {
    let (mut app, mut sr, mut physical) = app();
    ready_root_with_remembered_session(&mut app, &mut sr, &mut physical);
    start_nested(&mut app, &mut sr, &mut physical, 1);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        "inner",
        "/dev/ttys-inner",
    );
    acknowledge_root_replies(&mut app, &mut sr, &mut physical, 1_500);
    app.activate_tmux_connection(1, &mut sr, &mut physical)
        .unwrap();
    app.show_tmux_session_chooser(&mut sr, &mut physical)
        .unwrap();
    input(&mut app, &mut sr, &mut physical, b"\x1b[B\r");
    let _ = drain_root(&mut app);
    app.handle_pty(&mut sr, &reply(1_600, &[], true), &mut physical)
        .unwrap();
    let _ = drain_root(&mut app);
    app.handle_pty(
        &mut sr,
        &reply(1_601, &[b"L\t@10\t1000000".to_vec()], true),
        &mut physical,
    )
    .unwrap();
    assert_eq!(drain_root(&mut app), b"switch-client -t $2\n");
    app.handle_pty(&mut sr, b"%session-changed $2 foo\n", &mut physical)
        .unwrap();
    app.handle_pty(&mut sr, &reply(1_602, &[], true), &mut physical)
        .unwrap();
    let _ = drain_root(&mut app);
    acknowledge_root_replies(&mut app, &mut sr, &mut physical, 1_700);

    app.show_tmux_session_chooser(&mut sr, &mut physical)
        .unwrap();
    input(&mut app, &mut sr, &mut physical, b"\x1b[A\r");
    assert_eq!(
        drain_root(&mut app),
        b"switch-client -t $1\n",
        "the original user winlink should need no second carrier alias"
    );
    app.handle_pty(&mut sr, b"%session-changed $1 bar\n", &mut physical)
        .unwrap();
    app.handle_pty(&mut sr, &reply(1_800, &[], true), &mut physical)
        .unwrap();
    let cleanup = drain_root(&mut app);
    let (unlink, _) = split_first_command(&cleanup);
    assert_eq!(
        unlink,
        b"if-shell -F -t '$2:1000000' '#{==:#{window_id},@10}' 'unlink-window -t $2:1000000'\n"
    );

    // tmux 3.7b reports the removal of this winlink as `%window-close
    // @10`, even though @10 remains linked in the newly attached $1 session.
    // The notification does not identify the session which lost its link, so
    // it must not destroy the stable window, its carrier pane, or the child.
    app.handle_pty(&mut sr, &reply(1_801, &[], true), &mut physical)
        .unwrap();
    app.handle_pty(&mut sr, b"%window-close @10\n", &mut physical)
        .unwrap();
    assert_eq!(app.tmux_connection_count(), 2);
    assert_eq!(app.debug_tmux_pane_portal_target(1, 20), Some(2));
    assert!(
        app.debug_tmux_pane_contents(2, 20)
            .is_some_and(|contents| contents.contains("inner-READY"))
    );
    assert_eq!(
        drain_root(&mut app),
        lector::tmux_model::INVENTORY_COMMAND.as_bytes()
    );
}

#[test]
fn detaching_a_nested_connection_unlinks_its_carrier_after_the_child_exits() {
    let (mut app, mut sr, mut physical) = app();
    ready_root_with_remembered_session(&mut app, &mut sr, &mut physical);
    start_nested(&mut app, &mut sr, &mut physical, 1);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        "inner",
        "/dev/ttys-inner",
    );
    acknowledge_root_replies(&mut app, &mut sr, &mut physical, 1_820);
    app.activate_tmux_connection(1, &mut sr, &mut physical)
        .unwrap();
    app.show_tmux_session_chooser(&mut sr, &mut physical)
        .unwrap();
    input(&mut app, &mut sr, &mut physical, b"\x1b[B\r");
    let _ = drain_root(&mut app);
    app.handle_pty(&mut sr, &reply(1_830, &[], true), &mut physical)
        .unwrap();
    let _ = drain_root(&mut app);
    app.handle_pty(
        &mut sr,
        &reply(1_831, &[b"L\t@10\t1000000".to_vec()], true),
        &mut physical,
    )
    .unwrap();
    let _ = drain_root(&mut app);
    app.handle_pty(&mut sr, b"%session-changed $2 foo\n", &mut physical)
        .unwrap();
    app.handle_pty(&mut sr, &reply(1_832, &[], true), &mut physical)
        .unwrap();
    let _ = drain_root(&mut app);
    acknowledge_root_replies(&mut app, &mut sr, &mut physical, 1_840);

    app.activate_tmux_connection(2, &mut sr, &mut physical)
        .unwrap();
    let _ = drain_root(&mut app);
    input(&mut app, &mut sr, &mut physical, b"\x01d");
    let routed = drain_root(&mut app);
    let detach_route = decode_send_keys(&routed, 20);
    assert_eq!(detach_route, b"detach-client\n");
    feed_at_depth(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        b"%exit inner detached\n\x1b\\",
    );
    let cleanup = drain_root(&mut app);
    let (unlink, _) = split_first_command(&cleanup);
    assert_eq!(
        unlink,
        b"if-shell -F -t '$2:1000000' '#{==:#{window_id},@10}' 'unlink-window -t $2:1000000'\n"
    );
    assert_eq!(app.tmux_connection_count(), 1);
}

#[test]
fn renumbered_carrier_is_reidentified_by_window_id_and_moved_back_to_the_high_band() {
    let (mut app, mut sr, mut physical) = app();
    ready_root_with_remembered_session(&mut app, &mut sr, &mut physical);
    start_nested(&mut app, &mut sr, &mut physical, 1);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        "inner",
        "/dev/ttys-inner",
    );
    acknowledge_root_replies(&mut app, &mut sr, &mut physical, 1_900);
    app.activate_tmux_connection(1, &mut sr, &mut physical)
        .unwrap();
    app.show_tmux_session_chooser(&mut sr, &mut physical)
        .unwrap();
    input(&mut app, &mut sr, &mut physical, b"\x1b[B\r");
    let _ = drain_root(&mut app);
    app.handle_pty(&mut sr, &reply(2_000, &[], true), &mut physical)
        .unwrap();
    let _ = drain_root(&mut app);
    app.handle_pty(
        &mut sr,
        &reply(2_001, &[b"L\t@10\t1000000".to_vec()], true),
        &mut physical,
    )
    .unwrap();
    let _ = drain_root(&mut app);
    app.handle_pty(&mut sr, b"%session-changed $2 foo\n", &mut physical)
        .unwrap();
    app.handle_pty(&mut sr, &reply(2_002, &[], true), &mut physical)
        .unwrap();
    let _ = drain_root(&mut app);
    acknowledge_root_replies(&mut app, &mut sr, &mut physical, 2_100);

    // This inventory is the result tmux produces after `renumber-windows on`
    // collapses the owned @10 winlink from 1000000 to 3.
    app.handle_pty(&mut sr, b"%sessions-changed\n", &mut physical)
        .unwrap();
    assert_eq!(
        drain_root(&mut app),
        lector::tmux_model::INVENTORY_COMMAND.as_bytes()
    );
    for (offset, group) in inventory_with_renumbered_carrier().iter().enumerate() {
        app.handle_pty(&mut sr, &reply(2_200 + offset, group, true), &mut physical)
            .unwrap();
    }
    let relocation = drain_root(&mut app);
    let (move_command, pane_recovery) = split_first_command(&relocation);
    assert_eq!(
        move_command,
        b"if-shell -F -t '$2:3' '#{==:#{window_id},@10}' 'move-window -d -s $2:3 -t $2:1000000'\n"
    );
    assert!(
        pane_recovery
            .windows(b"display-message -p -t %22".len())
            .any(|window| window == b"display-message -p -t %22")
    );
    app.handle_pty(&mut sr, &reply(2_300, &[], true), &mut physical)
        .unwrap();
    assert_eq!(
        drain_root(&mut app),
        b"list-windows -t $2 -F 'L\t#{window_id}\t#{window_index}'\n"
    );
    app.handle_pty(&mut sr, &reply(2_301, &[], true), &mut physical)
        .unwrap();
    app.handle_pty(&mut sr, &reply(2_302, &[], true), &mut physical)
        .unwrap();
    app.handle_pty(
        &mut sr,
        &reply(2_303, &[b"L\t@10\t1000000".to_vec()], true),
        &mut physical,
    )
    .unwrap();
    let topology = app.debug_tmux_topology(1).unwrap();
    assert!(
        topology.contains("window @10 index 1000000: bar"),
        "{topology}"
    );
    assert!(!topology.contains("window @10 index 3: bar"), "{topology}");
}

#[test]
fn two_nested_levels_keep_identical_ids_separate_and_route_recursively() {
    let (mut app, mut sr, mut physical) = app();
    ready_root(&mut app, &mut sr, &mut physical);
    start_nested(&mut app, &mut sr, &mut physical, 1);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        "inner",
        "/dev/ttys-inner",
    );
    start_nested(&mut app, &mut sr, &mut physical, 2);

    assert_eq!(app.tmux_connection_count(), 3);
    assert_eq!(app.active_tmux_connection(), Some(3));
    assert_eq!(
        app.debug_tmux_gateway_origin(3),
        Some(GatewayOrigin::Pane {
            parent_connection_id: 2,
            session_id: 1,
            window_id: 10,
            pane_id: 20,
        })
    );
    let outer_routed = drain_root(&mut app);
    let inner_routed = decode_send_keys(&outer_routed, 20);
    let grandchild_command = decode_send_keys(&inner_routed, 20);
    assert_eq!(
        grandchild_command,
        [
            lector::app::TMUX_FLOW_CONTROL_COMMAND,
            lector::app::TMUX_FLOW_CONTROL_VERIFY_COMMAND,
            b"refresh-client -C 80x24\n",
            lector::tmux_model::INVENTORY_COMMAND.as_bytes(),
        ]
        .concat()
    );
    assert!(app.debug_tmux_topology(1).unwrap().contains("session $1"));
    assert!(app.debug_tmux_topology(2).unwrap().contains("session $1"));
    assert_eq!(app.debug_tmux_pane_portal_target(1, 20), Some(2));
    assert_eq!(app.debug_tmux_pane_portal_target(2, 20), Some(3));

    feed_at_depth(
        &mut app,
        &mut sr,
        &mut physical,
        2,
        b"%exit grandchild done\n\x1b\\",
    );
    assert_eq!(app.tmux_connection_count(), 2);
    assert_eq!(app.active_tmux_connection(), Some(2));
    assert_eq!(app.debug_tmux_pane_portal_target(2, 20), None);
    assert_eq!(app.debug_tmux_pane_portal_target(1, 20), Some(2));

    feed_at_depth(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        b"%exit child done\n\x1b\\",
    );
    assert_eq!(app.tmux_connection_count(), 1);
    assert_eq!(app.active_tmux_connection(), Some(1));
}

#[test]
fn a_linked_outer_carrier_preserves_a_byte_fragmented_level_three_stream() {
    let (mut app, mut sr, mut physical) = app();
    ready_root_with_remembered_session(&mut app, &mut sr, &mut physical);
    start_nested(&mut app, &mut sr, &mut physical, 1);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        "inner",
        "/dev/ttys-inner",
    );
    start_nested(&mut app, &mut sr, &mut physical, 2);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        2,
        "grandchild",
        "/dev/ttys-grandchild",
    );
    acknowledge_root_replies(&mut app, &mut sr, &mut physical, 3_000);

    app.activate_tmux_connection(1, &mut sr, &mut physical)
        .unwrap();
    app.show_tmux_session_chooser(&mut sr, &mut physical)
        .unwrap();
    input(&mut app, &mut sr, &mut physical, b"\x1b[B\r");
    assert_eq!(
        drain_root(&mut app),
        b"link-window -d -s @10 -t $2:1000000\n"
    );
    app.handle_pty(&mut sr, &reply(3_100, &[], true), &mut physical)
        .unwrap();
    let _ = drain_root(&mut app);
    app.handle_pty(
        &mut sr,
        &reply(3_101, &[b"L\t@10\t1000000".to_vec()], true),
        &mut physical,
    )
    .unwrap();
    let _ = drain_root(&mut app);
    app.handle_pty(&mut sr, b"%session-changed $2 foo\n", &mut physical)
        .unwrap();
    app.handle_pty(&mut sr, &reply(3_102, &[], true), &mut physical)
        .unwrap();

    let payload = b"SEQ:000001|SEQ:000002|SEQ:000003";
    let routed = wrap_at_depth(pane_output(20, payload), 2);
    for byte in routed {
        app.handle_pty(&mut sr, std::slice::from_ref(&byte), &mut physical)
            .unwrap();
    }
    let grandchild = app.debug_tmux_pane_contents(3, 20).unwrap();
    assert!(
        grandchild.contains(std::str::from_utf8(payload).unwrap()),
        "fragmented level-three output had a gap or duplicate: {grandchild:?}"
    );
    assert!(
        app.debug_tmux_topology(1)
            .is_some_and(|dump| dump.contains("[attached]: foo")),
        "level-three routing switched the outer client back"
    );
}

#[test]
fn graceful_root_teardown_waits_for_each_descendant_deepest_first() {
    let (mut app, mut sr, mut physical) = app();
    ready_root(&mut app, &mut sr, &mut physical);
    start_nested(&mut app, &mut sr, &mut physical, 1);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        "inner",
        "/dev/ttys-inner",
    );
    start_nested(&mut app, &mut sr, &mut physical, 2);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        2,
        "grandchild",
        "/dev/ttys-grandchild",
    );
    assert!(
        app.activate_tmux_connection(1, &mut sr, &mut physical)
            .unwrap()
    );

    assert!(
        app.request_tmux_gateway_action(
            &mut sr,
            GatewayControlAction::GracefulDetach,
            &mut physical,
        )
        .unwrap()
    );
    let routed = drain_root(&mut app);
    let routed = decode_send_keys(&routed, 20);
    let routed = decode_send_keys(&routed, 20);
    assert_eq!(
        routed, b"detach-client\n",
        "the deepest connection must detach first"
    );
    assert_eq!(app.tmux_connection_count(), 3);

    feed_at_depth(
        &mut app,
        &mut sr,
        &mut physical,
        2,
        b"%exit grandchild detached\n\x1b\\",
    );
    let routed = drain_root(&mut app);
    let routed = decode_send_keys(&routed, 20);
    assert_eq!(routed, b"detach-client\n");
    assert_eq!(app.tmux_connection_count(), 2);

    feed_at_depth(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        b"%exit inner detached\n\x1b\\",
    );
    assert_eq!(drain_root(&mut app), b"detach-client\n");
    assert_eq!(app.tmux_connection_count(), 1);

    app.handle_pty(&mut sr, b"%exit outer detached\n\x1b\\", &mut physical)
        .unwrap();
    assert_eq!(app.tmux_connection_count(), 0);
    assert_eq!(app.active_tmux_connection(), None);
    assert!(!app.has_overlay());
}

#[test]
fn prefix_detach_cascades_through_the_current_connection_subtree() {
    let (mut app, mut sr, mut physical) = app();
    ready_root(&mut app, &mut sr, &mut physical);
    start_nested(&mut app, &mut sr, &mut physical, 1);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        "inner",
        "/dev/ttys-inner",
    );
    start_nested(&mut app, &mut sr, &mut physical, 2);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        2,
        "grandchild",
        "/dev/ttys-grandchild",
    );
    app.activate_tmux_connection(1, &mut sr, &mut physical)
        .unwrap();

    input(&mut app, &mut sr, &mut physical, b"\x01d");
    let routed = drain_root(&mut app);
    let routed = decode_send_keys(&routed, 20);
    assert_eq!(decode_send_keys(&routed, 20), b"detach-client\n");

    feed_at_depth(
        &mut app,
        &mut sr,
        &mut physical,
        2,
        b"%exit grandchild detached\n\x1b\\",
    );
    let routed = drain_root(&mut app);
    assert_eq!(decode_send_keys(&routed, 20), b"detach-client\n");

    feed_at_depth(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        b"%exit inner detached\n\x1b\\",
    );
    assert_eq!(drain_root(&mut app), b"detach-client\n");
}

#[test]
fn prefix_detach_from_an_inner_connection_leaves_its_parent_attached() {
    let (mut app, mut sr, mut physical) = app();
    ready_root(&mut app, &mut sr, &mut physical);
    start_nested(&mut app, &mut sr, &mut physical, 1);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        "inner",
        "/dev/ttys-inner",
    );
    start_nested(&mut app, &mut sr, &mut physical, 2);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        2,
        "grandchild",
        "/dev/ttys-grandchild",
    );
    app.activate_tmux_connection(2, &mut sr, &mut physical)
        .unwrap();
    let _ = drain_root(&mut app);

    input(&mut app, &mut sr, &mut physical, b"\x01d");
    let routed = drain_root(&mut app);
    let routed = decode_send_keys(&routed, 20);
    assert_eq!(decode_send_keys(&routed, 20), b"detach-client\n");

    feed_at_depth(
        &mut app,
        &mut sr,
        &mut physical,
        2,
        b"%exit grandchild detached\n\x1b\\",
    );
    let routed = drain_root(&mut app);
    assert_eq!(decode_send_keys(&routed, 20), b"detach-client\n");

    feed_at_depth(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        b"%exit inner detached\n\x1b\\",
    );
    assert_eq!(app.tmux_connection_count(), 1);
    assert_eq!(app.active_tmux_connection(), Some(1));
    assert!(drain_root(&mut app).is_empty());
}

#[test]
fn shutdown_skips_each_stuck_connection_after_two_hundred_milliseconds() {
    let (mut app, mut sr, clock, mut physical) = app_with_clock();
    ready_root(&mut app, &mut sr, &mut physical);
    start_nested(&mut app, &mut sr, &mut physical, 1);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        "inner",
        "/dev/ttys-inner",
    );
    start_nested(&mut app, &mut sr, &mut physical, 2);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        2,
        "grandchild",
        "/dev/ttys-grandchild",
    );

    app.begin_tmux_shutdown(&mut sr, &mut physical).unwrap();
    assert!(app.tmux_shutdown_pending());
    assert_eq!(app.tmux_shutdown_timeout(), None);
    let mut transport = Vec::new();
    app.handle_tmux_shutdown_tick(&mut sr, &mut transport, &mut physical)
        .unwrap();
    assert_eq!(
        app.tmux_shutdown_timeout(),
        Some(Duration::from_millis(200))
    );
    let routed = &transport;
    let routed = decode_send_keys(routed, 20);
    assert_eq!(decode_send_keys(&routed, 20), b"detach-client\n");

    clock.set(199);
    transport.clear();
    app.handle_tmux_shutdown_tick(&mut sr, &mut transport, &mut physical)
        .unwrap();
    assert!(transport.is_empty());

    clock.set(200);
    app.handle_tmux_shutdown_tick(&mut sr, &mut transport, &mut physical)
        .unwrap();
    assert_eq!(decode_send_keys(&transport, 20), b"detach-client\n");

    transport.clear();
    clock.set(400);
    app.handle_tmux_shutdown_tick(&mut sr, &mut transport, &mut physical)
        .unwrap();
    assert_eq!(transport, b"detach-client\n");

    transport.clear();
    clock.set(599);
    app.handle_tmux_shutdown_tick(&mut sr, &mut transport, &mut physical)
        .unwrap();
    assert!(app.tmux_shutdown_pending());
    clock.set(600);
    app.handle_tmux_shutdown_tick(&mut sr, &mut transport, &mut physical)
        .unwrap();
    assert!(!app.tmux_shutdown_pending());
    assert_eq!(app.tmux_connection_count(), 3);
}

#[test]
fn force_abandon_during_a_cascade_targets_the_descendant_that_is_stuck() {
    let (mut app, mut sr, mut physical) = app();
    ready_root(&mut app, &mut sr, &mut physical);
    start_nested(&mut app, &mut sr, &mut physical, 1);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        "inner",
        "/dev/ttys-inner",
    );
    start_nested(&mut app, &mut sr, &mut physical, 2);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        2,
        "grandchild",
        "/dev/ttys-grandchild",
    );
    app.activate_tmux_connection(1, &mut sr, &mut physical)
        .unwrap();
    app.request_tmux_gateway_action(&mut sr, GatewayControlAction::GracefulDetach, &mut physical)
        .unwrap();
    let _detach = drain_root(&mut app);

    app.request_tmux_gateway_action(&mut sr, GatewayControlAction::ForceAbandon, &mut physical)
        .unwrap();
    let mut ignored = Vec::new();
    app.handle_stdin(&mut sr, b"\r", &mut ignored, &mut physical)
        .unwrap();
    let mut routed = drain_root(&mut app);
    routed = decode_send_keys(&routed, 20);
    routed = decode_send_keys(&routed, 20);
    assert_eq!(routed, b"\x1c");
}

#[test]
fn hierarchy_rejects_nesting_beyond_the_explicit_depth_limit() {
    let mut hierarchy = ConnectionHierarchy::new();
    hierarchy.insert(1, GatewayOrigin::Direct).unwrap();
    for connection_id in 2..=64 {
        hierarchy
            .insert(
                connection_id,
                GatewayOrigin::Pane {
                    parent_connection_id: connection_id - 1,
                    session_id: 1,
                    window_id: 10,
                    pane_id: 20,
                },
            )
            .unwrap();
    }
    assert_eq!(
        hierarchy.insert(
            65,
            GatewayOrigin::Pane {
                parent_connection_id: 64,
                session_id: 1,
                window_id: 10,
                pane_id: 20,
            }
        ),
        Err(LifecycleError::TooDeep)
    );
}

#[test]
fn repeated_nested_attach_bootstrap_and_detach_releases_every_child_resource() {
    let (mut app, mut sr, mut physical) = app();
    ready_root(&mut app, &mut sr, &mut physical);
    for cycle in 0..32 {
        start_nested(&mut app, &mut sr, &mut physical, 1);
        ready_nested(
            &mut app,
            &mut sr,
            &mut physical,
            1,
            &format!("child-{cycle}"),
            &format!("/dev/ttys-child-{cycle}"),
        );
        assert_eq!(app.tmux_connection_count(), 2, "attach cycle {cycle}");
        assert_eq!(
            app.debug_tmux_pane_portal_target(1, 20),
            Some(2),
            "connection number was not reused on attach cycle {cycle}"
        );
        feed_at_depth(
            &mut app,
            &mut sr,
            &mut physical,
            1,
            b"%exit child detached\n\x1b\\",
        );
        assert_eq!(app.tmux_connection_count(), 1, "detach cycle {cycle}");
        assert_eq!(app.active_tmux_connection(), Some(1));
        assert_eq!(app.debug_tmux_pane_portal_target(1, 20), None);
    }
    assert_eq!(app.debug_nested_tmux_gateway_count(), 1);
}

#[test]
fn nested_command_bytes_are_binary_safe_at_both_transport_layers() {
    let (mut app, mut sr, mut physical) = app();
    ready_root(&mut app, &mut sr, &mut physical);
    start_nested(&mut app, &mut sr, &mut physical, 1);
    ready_nested(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        "inner",
        "/dev/ttys-inner",
    );

    let bytes = [0, b'\n', b'\r', b'\\', 0x80, 0xff];
    input(&mut app, &mut sr, &mut physical, &bytes);
    let inner_command = decode_send_keys(&drain_root(&mut app), 20);
    assert_eq!(
        decode_send_keys(&inner_command, 20),
        bytes,
        "one hexadecimal send-keys layer must be removed at each gateway"
    );
}

#[test]
fn destroyed_ordinary_parent_pane_releases_its_gateway_detector() {
    let (mut app, mut sr, mut physical) = app();
    ready_root(&mut app, &mut sr, &mut physical);
    feed_at_depth(&mut app, &mut sr, &mut physical, 1, b"ordinary output");
    assert_eq!(app.debug_nested_tmux_gateway_count(), 1);

    app.handle_pty(&mut sr, b"%pane-exited %20\n", &mut physical)
        .unwrap();
    assert_eq!(app.debug_nested_tmux_gateway_count(), 0);
}

struct RealNestedHarness {
    receiver: mpsc::Receiver<Vec<u8>>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    read_thread: Option<thread::JoinHandle<()>>,
    _master: Box<dyn MasterPty + Send>,
    outer_socket: PathBuf,
    inner_socket: PathBuf,
}

impl RealNestedHarness {
    fn spawn() -> Self {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let socket_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-tmux");
        std::fs::create_dir_all(&socket_dir).unwrap();
        let suffix = format!("{}-{unique}", std::process::id());
        let outer_socket = socket_dir.join(format!("no-{suffix}.sock"));
        let inner_socket = socket_dir.join(format!("ni-{suffix}.sock"));
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
            outer_socket.to_str().unwrap(),
            "-f",
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/tmux-prefix.conf"
            ),
            "-CC",
            "new-session",
            "-s",
            "nested-outer",
            "/bin/sh",
        ]);
        command.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
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
                    Err(error) => panic!("read nested tmux PTY: {error}"),
                }
            }
        });
        Self {
            receiver,
            writer,
            child,
            read_thread: Some(read_thread),
            _master: pair.master,
            outer_socket,
            inner_socket,
        }
    }

    fn drive(
        &mut self,
        case: &str,
        app: &mut App,
        sr: &mut ScreenReader,
        physical: &mut Vec<u8>,
        mut done: impl FnMut(&mut App) -> bool,
    ) {
        for _ in 0..1_000 {
            if done(app) {
                return;
            }
            let chunk = self
                .receiver
                .recv_timeout(Duration::from_secs(5))
                .unwrap_or_else(|error| {
                    let child_status = self.child.try_wait();
                    let outer_pane = capture_pane(&self.outer_socket);
                    let inner_pane = capture_pane(&self.inner_socket);
                    let active_connection = app.active_tmux_connection();
                    let connection_count = app.tmux_connection_count();
                    let active_contents = app.debug_active_view_contents();
                    let outer_topology = app.debug_tmux_topology(1);
                    panic!(
                        "{case}: {error}; child={child_status:?}; outer={outer_pane:?}; inner={inner_pane:?}; active={active_connection:?}; count={connection_count}; contents={active_contents:?}; topology={outer_topology:?}"
                    )
                });
            app.handle_pty(sr, &chunk, physical).unwrap();
            app.drain_tmux_commands_for(1, self.writer.as_mut())
                .unwrap();
            self.writer.flush().unwrap();
        }
        panic!("{case} exceeded its bounded event count");
    }

    fn launch_inner_command(&self) -> Vec<u8> {
        format!(
            "tmux -S {} -f /dev/null -CC new-session -s nested-inner \"/usr/bin/perl -e '\\$|=1; system q(stty raw -echo); print q(INNER-READY); while (sysread(STDIN,\\$c,1)) {{ print \\$c; }}'\"\r",
            self.inner_socket.display()
        )
        .into_bytes()
    }
}

impl Drop for RealNestedHarness {
    fn drop(&mut self) {
        for socket in [&self.inner_socket, &self.outer_socket] {
            let _ = std::process::Command::new("tmux")
                .args(["-S", socket.to_str().unwrap(), "kill-server"])
                .output();
            let _ = std::fs::remove_file(socket);
        }
        let _ = self.child.wait();
        if let Some(read_thread) = self.read_thread.take() {
            let _ = read_thread.join();
        }
    }
}

fn capture_pane(socket: &Path) -> Option<String> {
    let output = std::process::Command::new("tmux")
        .args([
            "-S",
            socket.to_str().unwrap(),
            "capture-pane",
            "-p",
            "-t",
            ":",
        ])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[test]
fn real_tmux_nested_loopback_routes_control_and_child_input() {
    let _serial = super::serialize_real_tmux_test();
    let (mut app, mut sr, mut physical) = app();
    let mut harness = RealNestedHarness::spawn();

    harness.drive(
        "outer tmux readiness",
        &mut app,
        &mut sr,
        &mut physical,
        |app| {
            let contents = app.debug_active_view_contents();
            app.tmux_connection_count() == 1
                && !contents.contains("tmux connection is active")
                && !contents.trim().is_empty()
        },
    );
    input(
        &mut app,
        &mut sr,
        &mut physical,
        &harness.launch_inner_command(),
    );
    app.drain_tmux_commands_for(1, harness.writer.as_mut())
        .unwrap();
    harness.writer.flush().unwrap();

    harness.drive(
        "nested tmux readiness",
        &mut app,
        &mut sr,
        &mut physical,
        |app| {
            app.tmux_connection_count() == 2
                && app.active_tmux_connection() == Some(2)
                && app.debug_active_view_contents().contains("INNER-READY")
        },
    );
    let GatewayOrigin::Pane {
        parent_connection_id,
        pane_id,
        ..
    } = app.debug_tmux_gateway_origin(2).unwrap()
    else {
        panic!("real inner tmux was not discovered through its parent pane");
    };
    assert_eq!(parent_connection_id, 1);
    assert_eq!(app.debug_tmux_pane_portal_target(1, pane_id), Some(2));

    assert!(
        app.activate_tmux_connection(1, &mut sr, &mut physical)
            .unwrap()
    );
    harness
        .writer
        .write_all(b"new-session -d -s nested-away /bin/sh\n")
        .unwrap();
    harness.writer.flush().unwrap();
    harness.drive(
        "outer alternate-session discovery",
        &mut app,
        &mut sr,
        &mut physical,
        |app| {
            app.debug_tmux_topology(1)
                .is_some_and(|dump| dump.contains("nested-away"))
                && app.debug_tmux_expected_reply_count(1) == Some(0)
        },
    );
    assert!(
        app.show_tmux_session_chooser(&mut sr, &mut physical)
            .unwrap()
    );
    input(&mut app, &mut sr, &mut physical, b"\x1b[B\r");
    app.drain_tmux_commands_for(1, harness.writer.as_mut())
        .unwrap();
    harness.writer.flush().unwrap();
    harness.drive(
        "link carrier and switch the outer client",
        &mut app,
        &mut sr,
        &mut physical,
        |app| {
            app.debug_tmux_topology(1)
                .is_some_and(|dump| dump.contains("[attached]: nested-away"))
                && app.debug_tmux_expected_reply_count(1) == Some(0)
        },
    );

    // Stand in for five hours of inactivity without sleeping or locking the
    // test machine. The child produces bytes while Lector presents the parent
    // in another session; those bytes must still reach the nested parser.
    let produced = std::process::Command::new("tmux")
        .args([
            "-S",
            harness.inner_socket.to_str().unwrap(),
            "send-keys",
            "-t",
            ":",
            "-l",
            "FIVE-HOURS-LATER",
        ])
        .output()
        .unwrap();
    assert!(produced.status.success(), "{produced:?}");
    harness.drive(
        "nested output while the parent remains in another session",
        &mut app,
        &mut sr,
        &mut physical,
        |app| {
            app.debug_tmux_pane_contents(2, 0)
                .is_some_and(|contents| contents.contains("FIVE-HOURS-LATER"))
        },
    );

    assert!(
        app.show_tmux_connection_chooser(&mut sr, &mut physical)
            .unwrap()
    );
    input(&mut app, &mut sr, &mut physical, b"\x1b[B\r");
    assert_eq!(app.active_tmux_connection(), Some(2));
    app.drain_tmux_commands_for(1, harness.writer.as_mut())
        .unwrap();
    harness.writer.flush().unwrap();
    assert!(
        app.debug_tmux_topology(1)
            .is_some_and(|dump| dump.contains("[attached]: nested-away")),
        "activating the child changed the parent's session"
    );

    input(&mut app, &mut sr, &mut physical, b"X");
    app.drain_tmux_commands_for(1, harness.writer.as_mut())
        .unwrap();
    harness.writer.flush().unwrap();
    harness.drive(
        "nested tmux child echo",
        &mut app,
        &mut sr,
        &mut physical,
        |app| {
            app.debug_active_view_contents()
                .contains("FIVE-HOURS-LATERX")
        },
    );

    assert!(
        app.show_tmux_connection_chooser(&mut sr, &mut physical)
            .unwrap()
    );
    input(&mut app, &mut sr, &mut physical, b"\x1b[A\r");
    assert_eq!(app.active_tmux_connection(), Some(1));
    assert!(
        app.debug_tmux_topology(1)
            .is_some_and(|dump| dump.contains("[attached]: nested-away")),
        "returning to the parent did not preserve its user session"
    );
}
