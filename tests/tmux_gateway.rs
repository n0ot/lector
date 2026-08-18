use lector::views::{ViewAction, ViewController, ViewKind};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    io::Read,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    thread,
};

static LIVE_TMUX_ID: AtomicU64 = AtomicU64::new(1);
use lector::{
    app::App,
    screen_reader::ScreenReader,
    speech,
    terminal::GhosttyEngine,
    tmux_control::{CommandStatus, ControlEvent},
    tmux_gateway::{GatewayEvent, TmuxGatewayRouter},
    views,
};

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

fn app_harness() -> (App, ScreenReader) {
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(8, 60)));
    let app = App::new(stack).unwrap();
    let sr = ScreenReader::new(speech::Speech::new(Box::<SilentDriver>::default()));
    (app, sr)
}

#[derive(Debug, Eq, PartialEq)]
struct NormalizedRoute {
    direct: Vec<u8>,
    started: Vec<u64>,
    control: Vec<(u64, ControlEvent)>,
    ended: Vec<u64>,
    failed: Vec<u64>,
}

fn route_chunks<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> NormalizedRoute {
    let mut router = TmuxGatewayRouter::new();
    let mut route = NormalizedRoute {
        direct: Vec::new(),
        started: Vec::new(),
        control: Vec::new(),
        ended: Vec::new(),
        failed: Vec::new(),
    };
    for chunk in chunks {
        for event in router.push(chunk).unwrap() {
            match event {
                GatewayEvent::DirectOutput(bytes) => route.direct.extend(bytes),
                GatewayEvent::ConnectionStarted { connection_id } => {
                    route.started.push(connection_id);
                }
                GatewayEvent::Control {
                    connection_id,
                    event,
                } => route.control.push((connection_id, event)),
                GatewayEvent::ConnectionEnded { connection_id } => {
                    route.ended.push(connection_id);
                }
                GatewayEvent::ConnectionFailed { connection_id, .. } => {
                    route.failed.push(connection_id);
                }
            }
        }
    }
    route.direct.extend(router.finish_direct().unwrap());
    route
}

#[test]
fn source_router_owns_the_exact_marker_boundary_under_every_fragmentation() {
    let stream = b"before\x1bP1000p%begin 1 2 0\nfailed\n%error 1 2 0\n%exit done\n\x1b\\after";
    let expected = NormalizedRoute {
        direct: b"beforeafter".to_vec(),
        started: vec![1],
        control: vec![
            (1, ControlEvent::Started),
            (
                1,
                ControlEvent::Command {
                    timestamp: 1,
                    number: 2,
                    flags: 0,
                    status: CommandStatus::Error,
                    output: vec![b"failed".to_vec()],
                },
            ),
            (
                1,
                ControlEvent::Exit {
                    reason: Some(b"done".to_vec()),
                },
            ),
            (1, ControlEvent::Ended),
        ],
        ended: vec![1],
        failed: Vec::new(),
    };

    assert_eq!(route_chunks([stream.as_slice()]), expected);
    for split in 0..=stream.len() {
        assert_eq!(
            route_chunks([&stream[..split], &stream[split..]]),
            expected,
            "gateway route changed at split {split}"
        );
    }
    assert_eq!(
        route_chunks(stream.iter().map(std::slice::from_ref)),
        expected
    );
}

#[test]
fn marker_lookalikes_round_trip_to_the_direct_terminal_without_loss() {
    let bytes = b"a\x1bP1000x b\x1bP10 c\x1b\x1bP1000q";
    assert_eq!(
        route_chunks(bytes.iter().map(std::slice::from_ref)).direct,
        bytes
    );
}

#[test]
fn captured_terminal_escapes_at_the_start_of_command_lines_stay_in_control_mode() {
    let styled = b"\x1b[2mcaptured pane\x1b[0m".to_vec();
    let hyperlink = b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\".to_vec();
    let stream = [
        b"\x1bP1000p%begin 7 9 0\n".as_slice(),
        styled.as_slice(),
        b"\n".as_slice(),
        hyperlink.as_slice(),
        b"\n%end 7 9 0\n%exit\n\x1b\\after".as_slice(),
    ]
    .concat();

    let expected = NormalizedRoute {
        direct: b"after".to_vec(),
        started: vec![1],
        control: vec![
            (1, ControlEvent::Started),
            (
                1,
                ControlEvent::Command {
                    timestamp: 7,
                    number: 9,
                    flags: 0,
                    status: CommandStatus::Success,
                    output: vec![styled, hyperlink],
                },
            ),
            (1, ControlEvent::Exit { reason: None }),
            (1, ControlEvent::Ended),
        ],
        ended: vec![1],
        failed: Vec::new(),
    };

    assert_eq!(route_chunks([stream.as_slice()]), expected);
    assert_eq!(
        route_chunks(stream.iter().map(std::slice::from_ref)),
        expected
    );
}

#[test]
fn application_harness_keeps_control_protocol_out_of_both_terminal_engines() {
    let (mut app, mut sr) = app_harness();
    let mut pty_out = Vec::new();
    let mut physical_bytes = Vec::new();

    app.handle_stdin(&mut sr, b"\x01", &mut pty_out, &mut physical_bytes)
        .unwrap();
    app.handle_stdin(&mut sr, b"tm\r", &mut pty_out, &mut physical_bytes)
        .unwrap();
    assert_eq!(
        pty_out, b"\x01tm\r",
        "ordinary C-a or launcher was consumed"
    );
    pty_out.clear();

    app.handle_pty(&mut sr, b"gateway$ tm\r\n\x1bP10", &mut physical_bytes)
        .unwrap();
    app.handle_pty(
        &mut sr,
        b"00p%begin 7 1 0\nstartup failed\n%error 7 1 0\n",
        &mut physical_bytes,
    )
    .unwrap();

    assert_eq!(app.tmux_connection_count(), 1);
    assert_eq!(app.active_tmux_connection(), Some(1));
    assert!(
        !app.has_overlay(),
        "a selectable tmux connection is a base scene, not an overlay"
    );
    let connection = app.debug_active_view_contents();
    assert!(connection.contains("tmux connection is active"));
    assert!(!connection.contains("gateway$") && !connection.contains("%begin"));

    assert!(
        app.show_tmux_gateway(1, &mut sr, &mut physical_bytes)
            .unwrap()
    );
    let portal = app.debug_active_view_contents();
    assert!(portal.contains("tmux control mode is running"));
    assert!(portal.contains("Enter") && portal.contains("active session"));
    assert!(!portal.contains("gateway$") && !portal.contains("%begin"));

    app.handle_stdin(&mut sr, b"r", &mut pty_out, &mut physical_bytes)
        .unwrap();
    app.handle_stdin(&mut sr, b"\r", &mut pty_out, &mut physical_bytes)
        .unwrap();
    assert!(pty_out.is_empty(), "read-only portal wrote to control PTY");
    assert_eq!(app.active_tmux_connection(), Some(1));
    assert!(
        app.debug_active_view_contents()
            .contains("tmux connection is active"),
        "Enter did not leave the portal for the active connection"
    );

    let mut physical = GhosttyEngine::new(8, 60).unwrap();
    physical.advance(&physical_bytes).unwrap();
    let rendered = physical.normalized_snapshot().contents_full();
    assert!(rendered.contains("tmux connection is active"));
    assert!(!rendered.contains("%begin") && !rendered.contains("%error"));

    app.handle_pty(
        &mut sr,
        b"%exit clean detach\n\x1b\\gateway returned\r\n",
        &mut physical_bytes,
    )
    .unwrap();
    assert_eq!(app.tmux_connection_count(), 0);
    assert_eq!(app.active_tmux_connection(), None);
    assert!(!app.has_overlay());
    let gateway = app.debug_active_view_contents();
    assert!(gateway.contains("gateway$ tm"));
    assert!(gateway.contains("gateway returned"));
    assert!(!gateway.contains("%exit") && !gateway.contains("%error"));
}

#[test]
fn immediate_connection_exit_in_one_read_returns_to_the_gateway() {
    let stream = b"before\x1bP1000p%exit immediate\n\x1b\\after";
    for split in 0..=stream.len() {
        let (mut app, mut sr) = app_harness();
        let mut physical = Vec::new();
        app.handle_pty(&mut sr, &stream[..split], &mut physical)
            .unwrap();
        app.handle_pty(&mut sr, &stream[split..], &mut physical)
            .unwrap();
        assert_eq!(app.tmux_connection_count(), 0, "split {split}");
        assert!(!app.has_overlay(), "split {split}");
        let gateway = app.debug_active_view_contents();
        assert!(
            gateway.contains("beforeafter"),
            "split {split}: {gateway:?}"
        );
        assert!(!gateway.contains("%exit"), "split {split}: {gateway:?}");
    }
}

#[test]
fn portal_is_read_only_and_enter_targets_its_connection() {
    let mut portal = views::TmuxPortalView::new(6, 50, 73);
    let mut sr = ScreenReader::new(speech::Speech::new(Box::<SilentDriver>::default()));
    let mut pty = Vec::new();

    assert_eq!(portal.kind(), ViewKind::TmuxPortal);
    assert!(matches!(
        portal.handle_input(&mut sr, b"r", &mut pty).unwrap(),
        ViewAction::None
    ));
    assert!(matches!(
        portal.handle_input(&mut sr, b"\r", &mut pty).unwrap(),
        ViewAction::ActivateTmuxConnection(73)
    ));
    assert!(pty.is_empty());
    assert!(portal.model().contents_full().contains("active session"));
}

#[test]
fn real_local_tmux_control_client_crosses_the_pty_gateway_harness() {
    let tmux = std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .expect("tmux integration tests require tmux on PATH");
    assert!(tmux.status.success(), "tmux -V failed");

    let unique = LIVE_TMUX_ID.fetch_add(1, Ordering::Relaxed);
    let socket_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-tmux");
    std::fs::create_dir_all(&socket_dir).unwrap();
    let socket = socket_dir.join(format!("gateway-{}-{unique}.sock", std::process::id()));
    let session = format!("lector_gateway_{}_{unique}", std::process::id());

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 8,
            cols: 60,
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
        "printf 'live tmux smoke\\n'; sleep 0.2",
    ]);
    command.env("TERM", "xterm-256color");
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let _writer = pair.master.take_writer().unwrap();
    let read_thread = thread::spawn(move || {
        let mut output = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => output.extend_from_slice(&buffer[..count]),
                Err(error) if error.raw_os_error() == Some(5) => break,
                Err(error) => panic!("read real tmux PTY: {error}"),
            }
        }
        output
    });
    let _ = child.wait().unwrap();
    let control_stream = read_thread.join().unwrap();
    let _ = std::fs::remove_file(&socket);

    assert!(
        control_stream
            .windows(b"\x1bP1000p".len())
            .any(|window| window == b"\x1bP1000p"),
        "real tmux emitted no control marker: {control_stream:?}"
    );
    assert!(control_stream.ends_with(b"\x1b\\"));

    let (mut app, mut sr) = app_harness();
    let mut physical = Vec::new();
    let mut saw_connection = false;
    for byte in &control_stream {
        app.handle_pty(&mut sr, std::slice::from_ref(byte), &mut physical)
            .unwrap();
        if app.tmux_connection_count() == 1 {
            saw_connection = true;
            assert!(
                app.debug_active_view_contents()
                    .contains("tmux connection is active")
            );
        }
    }
    assert!(saw_connection, "real tmux connection was never activated");
    assert_eq!(app.tmux_connection_count(), 0);
    assert!(!app.has_overlay());
    assert!(!app.debug_active_view_contents().contains("%begin"));
}

#[test]
fn scheduled_physical_writer_never_receives_control_records() {
    let (mut app, mut sr) = app_harness();
    app.enable_output_scheduler(Default::default());
    let mut physical_bytes = Vec::new();

    app.handle_pty(
        &mut sr,
        b"shell\r\n\x1bP1000p%begin 1 1 0\n%end 1 1 0\n%output %0 secret\n",
        &mut physical_bytes,
    )
    .unwrap();
    app.drain_scheduled_output(&mut physical_bytes, true)
        .unwrap();
    assert!(!physical_bytes.windows(6).any(|bytes| bytes == b"%begin"));
    assert!(!physical_bytes.windows(7).any(|bytes| bytes == b"%output"));

    let mut physical = GhosttyEngine::new(8, 60).unwrap();
    physical.advance(&physical_bytes).unwrap();
    assert!(
        physical
            .normalized_snapshot()
            .contents_full()
            .contains("tmux connection is active")
    );

    app.handle_pty(&mut sr, b"%exit\n\x1b\\back\r\n", &mut physical_bytes)
        .unwrap();
    app.drain_scheduled_output(&mut physical_bytes, true)
        .unwrap();
    assert_eq!(app.tmux_connection_count(), 0);
}
