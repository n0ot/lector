use lector::{
    app::App,
    screen_reader::ScreenReader,
    speech,
    tmux_lifecycle::{ConnectionHierarchy, GatewayOrigin, LifecycleError},
    tmux_model::INVENTORY_REPLY_COUNT,
    views,
};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, SystemTime},
};

const SPLIT_LAYOUT: &str = "abcd,80x24,0,0{40x24,0,0,20,39x24,41,0,21}";
const SINGLE_LAYOUT: &str = "b25f,80x24,0,0,20";

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

fn app() -> (App, ScreenReader, Vec<u8>) {
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(24, 80)));
    let app = App::new(stack).unwrap();
    let sr = ScreenReader::new(speech::Speech::new(Box::<SilentDriver>::default()));
    (app, sr, Vec::new())
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
        vec![b"O\tmode-keys\tvi".to_vec()],
        vec![b"O\trepeat-time\t500".to_vec()],
        vec![
            b"B\tn\t0\tnext-window".to_vec(),
            b"B\td\t0\tdetach-client".to_vec(),
            b"B\t:\t0\tcommand-prompt".to_vec(),
        ],
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
            lector::tmux_model::INVENTORY_COMMAND.as_bytes(),
        ]
        .concat()
    );
    app.handle_pty(sr, &reply(2, &[], true), physical).unwrap();
    let groups = inventory(true, "outer", "/dev/ttys-outer");
    assert_eq!(groups.len(), INVENTORY_REPLY_COUNT);
    for (index, group) in groups.iter().enumerate() {
        app.handle_pty(sr, &reply(index + 3, group, true), physical)
            .unwrap();
    }
    assert_eq!(
        drain_root(app),
        b"capture-pane -p -e -J -S - -t %20\ncapture-pane -p -e -J -S - -t %21\n"
    );
    app.handle_pty(sr, &reply(30, &[b"PARENT".to_vec()], true), physical)
        .unwrap();
    app.handle_pty(sr, &reply(31, &[b"SIBLING".to_vec()], true), physical)
        .unwrap();
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
            lector::tmux_model::INVENTORY_COMMAND.as_bytes(),
        ]
        .concat()
    );

    let groups = inventory(false, name, client);
    feed_at_depth(app, sr, physical, depth, &reply(49, &[], true));
    for (index, group) in groups.iter().enumerate() {
        feed_at_depth(app, sr, physical, depth, &reply(50 + index, group, true));
    }
    let routed_capture = drain_root(app);
    let mut decoded = routed_capture;
    for _ in 0..depth {
        decoded = decode_send_keys(&decoded, 20);
    }
    assert_eq!(decoded, b"capture-pane -p -e -J -S - -t %20\n");
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
fn hierarchy_rejects_nesting_beyond_the_explicit_depth_limit() {
    let mut hierarchy = ConnectionHierarchy::new();
    hierarchy.insert(1, GatewayOrigin::Direct).unwrap();
    for connection_id in 2..=64 {
        hierarchy
            .insert(
                connection_id,
                GatewayOrigin::Pane {
                    parent_connection_id: connection_id - 1,
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
            Some(cycle as u64 + 2)
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
                    panic!(
                        "{case}: {error}; child={child_status:?}; outer={outer_pane:?}; inner={inner_pane:?}"
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

    input(&mut app, &mut sr, &mut physical, b"X");
    app.drain_tmux_commands_for(1, harness.writer.as_mut())
        .unwrap();
    harness.writer.flush().unwrap();
    harness.drive(
        "nested tmux child echo",
        &mut app,
        &mut sr,
        &mut physical,
        |app| app.debug_active_view_contents().contains("INNER-READYX"),
    );
}
