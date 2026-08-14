use lector::{
    app::App,
    screen_reader::ScreenReader,
    speech,
    tmux_gateway::{
        GatewayEvent, GatewayFailure, GatewayLifecycleState, TMUX_CONTROL_START_MARKER,
        TmuxGatewayRouter,
    },
    tmux_lifecycle::GatewayControlAction,
    views,
};
use std::{
    cell::{Cell, RefCell},
    io::{Read, Write},
    path::PathBuf,
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
    thread,
};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

static LIVE_RECOVERY_ID: AtomicU64 = AtomicU64::new(1);

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

#[derive(Clone, Default)]
struct TestClock(Rc<Cell<u128>>);

impl TestClock {
    fn advance(&self, milliseconds: u128) {
        self.0.set(self.0.get() + milliseconds);
    }
}

impl lector::app::Clock for TestClock {
    fn now_ms(&self) -> u128 {
        self.0.get()
    }
}

fn failure(events: &[GatewayEvent]) -> Option<(u64, GatewayFailure)> {
    events.iter().find_map(|event| match event {
        GatewayEvent::ConnectionFailed {
            connection_id,
            reason,
        } => Some((*connection_id, reason.clone())),
        _ => None,
    })
}

#[test]
fn router_distinguishes_every_terminal_boundary_and_recovers_direct_output() {
    let mut clean = TmuxGatewayRouter::new();
    let events = clean
        .push(b"\x1bP1000p%exit detached\n\x1b\\shell$ ")
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, GatewayEvent::ConnectionEnded { connection_id: 1 }))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        GatewayEvent::DirectOutput(bytes) if bytes == b"shell$ "
    )));
    assert_eq!(clean.lifecycle_state(), GatewayLifecycleState::Direct);

    let mut no_exit = TmuxGatewayRouter::new();
    let events = no_exit.push(b"\x1bP1000p\x1b\\shell$ ").unwrap();
    assert_eq!(failure(&events), Some((1, GatewayFailure::MissingExit)));
    assert!(events.iter().any(|event| matches!(
        event,
        GatewayEvent::DirectOutput(bytes) if bytes == b"shell$ "
    )));

    let mut no_st = TmuxGatewayRouter::new();
    no_st.push(b"\x1bP1000p%exit detached\n").unwrap();
    assert_eq!(
        no_st.lifecycle_state(),
        GatewayLifecycleState::AwaitingTerminator
    );
    assert_eq!(
        failure(&no_st.expire_termination()),
        Some((1, GatewayFailure::TerminatorTimeout))
    );
    assert_eq!(no_st.lifecycle_state(), GatewayLifecycleState::Direct);

    let mut escaped_shell = TmuxGatewayRouter::new();
    let events = escaped_shell
        .push(b"\x1bP1000p%exit detached\n\x1b[31mparent$\x1b[0m\r\n")
        .unwrap();
    assert_eq!(
        failure(&events),
        Some((1, GatewayFailure::MissingTerminator))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        GatewayEvent::DirectOutput(bytes) if bytes == b"\x1b[31mparent$\x1b[0m\r\n"
    )));

    let mut no_st_at_eof = TmuxGatewayRouter::new();
    no_st_at_eof.push(b"\x1bP1000p%exit detached\n").unwrap();
    assert_eq!(
        failure(&no_st_at_eof.finish_transport()),
        Some((1, GatewayFailure::MissingTerminator))
    );

    let mut eof = TmuxGatewayRouter::new();
    eof.push(b"\x1bP1000p%output %1 partial").unwrap();
    assert_eq!(
        failure(&eof.finish_transport()),
        Some((1, GatewayFailure::TransportEof))
    );
    assert_eq!(eof.lifecycle_state(), GatewayLifecycleState::Direct);
}

#[test]
fn eof_at_every_protocol_byte_is_bounded_idempotent_and_never_poisoned() {
    let stream = b"\x1bP1000p%begin 1 2 0\nreply\n%end 1 2 0\n%exit dead\n\x1b\\";
    for split in 0..stream.len() {
        let mut router = TmuxGatewayRouter::new();
        let prefix = &stream[..split];
        router.push(prefix).unwrap();
        let events = router.finish_transport();
        if split < TMUX_CONTROL_START_MARKER.len() {
            let direct = events
                .iter()
                .filter_map(|event| match event {
                    GatewayEvent::DirectOutput(bytes) => Some(bytes.as_slice()),
                    _ => None,
                })
                .flatten()
                .copied()
                .collect::<Vec<_>>();
            assert_eq!(direct, prefix, "marker split {split}");
        } else {
            assert!(failure(&events).is_some(), "protocol split {split}");
        }
        assert!(router.finish_transport().is_empty(), "split {split}");
        assert_eq!(router.lifecycle_state(), GatewayLifecycleState::Direct);
        let resumed = router.push(b"ordinary shell output\r\n").unwrap();
        assert!(resumed.iter().any(|event| matches!(
            event,
            GatewayEvent::DirectOutput(bytes) if bytes == b"ordinary shell output\r\n"
        )));
    }
}

#[test]
fn malformed_control_replays_the_revealing_shell_line_and_resumes_routing() {
    let mut router = TmuxGatewayRouter::new();
    let events = router
        .push(b"\x1bP1000pConnection to host closed.\r\nshell$ ready\r\n")
        .unwrap();
    assert!(matches!(
        failure(&events),
        Some((1, GatewayFailure::Protocol(_)))
    ));
    let direct = events
        .iter()
        .filter_map(|event| match event {
            GatewayEvent::DirectOutput(bytes) => Some(bytes.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(direct, b"Connection to host closed.\r\nshell$ ready\r\n");
    assert_eq!(router.lifecycle_state(), GatewayLifecycleState::Direct);

    let mut invalid_protocol = TmuxGatewayRouter::new();
    let events = invalid_protocol
        .push(b"\x1bP1000p%output invalid\nshell$ after\r\n")
        .unwrap();
    assert!(matches!(
        failure(&events),
        Some((1, GatewayFailure::Protocol(_)))
    ));
    assert!(!events.iter().any(|event| matches!(
        event,
        GatewayEvent::DirectOutput(bytes) if bytes.starts_with(b"%output")
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        GatewayEvent::DirectOutput(bytes) if bytes == b"shell$ after\r\n"
    )));
}

fn app_with_clock() -> (App, ScreenReader, Recorder, TestClock, Vec<u8>) {
    let recorder = Recorder::default();
    let clock = TestClock::default();
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(24, 80)));
    let app = App::new_with_clock(stack, Box::new(clock.clone())).unwrap();
    let sr = ScreenReader::new(speech::Speech::new(Box::new(recorder.clone())));
    (app, sr, recorder, clock, Vec::new())
}

fn start_connection(app: &mut App, sr: &mut ScreenReader, physical: &mut Vec<u8>) {
    app.handle_pty(
        sr,
        b"gateway$ \r\n\x1bP1000p%begin 1 1 0\n%end 1 1 0\n",
        physical,
    )
    .unwrap();
    assert_eq!(app.tmux_connection_count(), 1);
}

fn reply(serial: usize, lines: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = format!("%begin {serial} {serial} 0\n").into_bytes();
    for line in lines {
        bytes.extend_from_slice(line);
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(format!("%end {serial} {serial} 0\n").as_bytes());
    bytes
}

fn pane_output(pane_id: u64, bytes: &[u8]) -> Vec<u8> {
    let mut output = format!("%output %{pane_id} ").into_bytes();
    for &byte in bytes {
        if (0x20..=0x7e).contains(&byte) && byte != b'\\' {
            output.push(byte);
        } else {
            output.extend_from_slice(format!("\\{byte:03o}").as_bytes());
        }
    }
    output.push(b'\n');
    output
}

fn ready_parent(app: &mut App, sr: &mut ScreenReader, physical: &mut Vec<u8>) {
    start_connection(app, sr, physical);
    let mut transport = Vec::new();
    app.handle_tick(sr, &mut transport, physical).unwrap();
    assert_eq!(
        transport,
        [
            lector::app::TMUX_FLOW_CONTROL_COMMAND,
            lector::tmux_model::INVENTORY_COMMAND.as_bytes(),
        ]
        .concat()
    );
    app.handle_pty(sr, &reply(2, &[]), physical).unwrap();
    let groups = [
        vec![b"S\t$1\tparent".to_vec()],
        vec![b"W\t$1\t@10\t1\t1\tb25f,80x24,0,0,20\tb25f,80x24,0,0,20\t*\tparent".to_vec()],
        vec![b"P\t@10\t%20\t1\t1\t0\t0\t80\t24\t0\t0\t0\t1\t0\t0\t0\t0\tgateway".to_vec()],
        vec![b"A\t$1".to_vec()],
        vec![b"O\tbase-index\t1".to_vec()],
        vec![b"O\tpane-base-index\t1".to_vec()],
        vec![b"C\tclient_name\t/dev/ttys-parent".to_vec()],
        vec![b"O\tprefix\tC-a".to_vec()],
        vec![b"O\tprefix2\tNone".to_vec()],
        vec![b"O\tmode-keys\tvi".to_vec()],
        vec![b"O\trepeat-time\t500".to_vec()],
        vec![b"B\td\t0\tdetach-client".to_vec()],
    ];
    assert_eq!(groups.len(), lector::tmux_model::INVENTORY_REPLY_COUNT);
    for (index, group) in groups.iter().enumerate() {
        app.handle_pty(sr, &reply(index + 3, group), physical)
            .unwrap();
    }
    transport.clear();
    app.handle_tick(sr, &mut transport, physical).unwrap();
    assert_eq!(transport, b"capture-pane -p -e -J -S - -t %20\n");
    app.handle_pty(sr, &reply(30, &[b"PARENT READY".to_vec()]), physical)
        .unwrap();
}

fn start_nested(app: &mut App, sr: &mut ScreenReader, physical: &mut Vec<u8>) {
    app.handle_pty(
        sr,
        &pane_output(20, b"\x1bP1000p%begin 40 40 0\n%end 40 40 0\n"),
        physical,
    )
    .unwrap();
    assert_eq!(app.tmux_connection_count(), 2);
    let child = app
        .active_tmux_connection()
        .expect("nested connection active");
    assert_ne!(child, 1);
    assert_eq!(app.debug_tmux_pane_portal_target(1, 20), Some(child));
}

#[test]
fn nested_ssh_death_reconstructs_parent_pane_and_parent_exit_cleans_once() {
    let (mut app, mut sr, recorder, _clock, mut physical) = app_with_clock();
    ready_parent(&mut app, &mut sr, &mut physical);
    start_nested(&mut app, &mut sr, &mut physical);

    app.handle_pty(
        &mut sr,
        &pane_output(
            20,
            b"Connection to remote.example closed.\r\nparent$ recovered\r\n",
        ),
        &mut physical,
    )
    .unwrap();
    assert_eq!(app.tmux_connection_count(), 1);
    assert_eq!(app.active_tmux_connection(), Some(1));
    assert_eq!(app.debug_tmux_pane_portal_target(1, 20), None);
    let parent = app.debug_tmux_pane_contents(1, 20).unwrap();
    assert!(
        parent.contains("Connection to remote.example closed."),
        "{parent:?}"
    );
    assert!(parent.contains("parent$ recovered"), "{parent:?}");
    let speech = recorder.0.borrow();
    assert!(
        speech.iter().any(|message| {
            message.contains("invalid control protocol")
                && message.contains("parent tmux connection 1, pane  percent 20")
        }),
        "{speech:?}"
    );
    drop(speech);

    start_nested(&mut app, &mut sr, &mut physical);
    app.handle_pty(&mut sr, b"%pane-exited %20\n", &mut physical)
        .unwrap();
    assert_eq!(app.tmux_connection_count(), 1);
    assert_eq!(app.active_tmux_connection(), Some(1));
    assert_eq!(app.debug_nested_tmux_gateway_count(), 0);
    assert!(
        recorder
            .0
            .borrow()
            .iter()
            .any(|message| message.contains("parent pane") && message.contains("connection 1"))
    );
    app.handle_pty(&mut sr, b"%pane-exited %20\n", &mut physical)
        .unwrap();
    assert_eq!(app.tmux_connection_count(), 1);
    assert_eq!(app.debug_nested_tmux_gateway_count(), 0);
}

#[test]
fn nested_gateway_controls_target_only_the_parent_transport_pane() {
    let (mut app, mut sr, _recorder, _clock, mut physical) = app_with_clock();
    ready_parent(&mut app, &mut sr, &mut physical);
    start_nested(&mut app, &mut sr, &mut physical);
    let child = app.active_tmux_connection().unwrap();
    let before = app.debug_active_view_contents();

    assert!(
        app.request_tmux_gateway_action(&mut sr, GatewayControlAction::Interrupt, &mut physical,)
            .unwrap()
    );
    let mut root = Vec::new();
    app.handle_tick(&mut sr, &mut root, &mut physical).unwrap();
    let routed = String::from_utf8(root).unwrap();
    assert!(routed.contains("send-keys -H -t %20 03\n"), "{routed:?}");
    assert_eq!(app.tmux_connection_count(), 2);
    assert_eq!(app.active_tmux_connection(), Some(child));
    assert_eq!(app.debug_active_view_contents(), before);

    assert!(
        app.request_tmux_gateway_action(
            &mut sr,
            GatewayControlAction::SshEscapeDisconnect,
            &mut physical,
        )
        .unwrap()
    );
    assert!(app.has_overlay());
    let mut ignored = Vec::new();
    app.handle_stdin(&mut sr, b"\r", &mut ignored, &mut physical)
        .unwrap();
    root = ignored;
    app.handle_tick(&mut sr, &mut root, &mut physical).unwrap();
    let routed = String::from_utf8(root).unwrap();
    assert!(
        routed.contains("send-keys -H -t %20 0d 7e 2e\n"),
        "{routed:?}"
    );
    assert!(!app.has_overlay());
    assert_eq!(app.tmux_connection_count(), 2);
    assert_eq!(app.active_tmux_connection(), Some(child));
}

#[test]
fn app_timeout_and_eof_return_to_terminal_once_and_announce_why() {
    let (mut app, mut sr, recorder, clock, mut physical) = app_with_clock();
    start_connection(&mut app, &mut sr, &mut physical);
    app.handle_pty(&mut sr, b"%exit ssh died\n", &mut physical)
        .unwrap();
    assert_eq!(app.tmux_connection_count(), 1);
    clock.advance(1_001);
    let mut transport = Vec::new();
    app.handle_tick(&mut sr, &mut transport, &mut physical)
        .unwrap();
    assert_eq!(app.tmux_connection_count(), 0);
    assert_eq!(app.active_tmux_connection(), None);
    assert!(!app.has_overlay());
    assert!(recorder.0.borrow().iter().any(|message| {
        message.contains("tmux connection 1")
            && message.contains("terminator")
            && message.contains("terminal")
    }));

    app.handle_pty_eof(&mut sr, &mut physical).unwrap();
    assert_eq!(app.tmux_connection_count(), 0);

    let (mut app, mut sr, recorder, _clock, mut physical) = app_with_clock();
    start_connection(&mut app, &mut sr, &mut physical);
    app.handle_pty_eof(&mut sr, &mut physical).unwrap();
    app.handle_pty_eof(&mut sr, &mut physical).unwrap();
    assert_eq!(app.tmux_connection_count(), 0);
    assert_eq!(
        recorder
            .0
            .borrow()
            .iter()
            .filter(|message| message.contains("transport ended"))
            .count(),
        1
    );
}

#[test]
fn accessible_gateway_controls_are_scoped_and_dangerous_bytes_are_confirmed() {
    let (mut app, mut sr, _recorder, _clock, mut physical) = app_with_clock();
    start_connection(&mut app, &mut sr, &mut physical);
    let mut transport = Vec::new();

    assert!(
        app.request_tmux_gateway_action(
            &mut sr,
            GatewayControlAction::GracefulDetach,
            &mut physical,
        )
        .unwrap()
    );
    app.handle_tick(&mut sr, &mut transport, &mut physical)
        .unwrap();
    assert!(transport.ends_with(b"detach-client\n"));
    transport.clear();

    assert!(
        app.request_tmux_gateway_action(&mut sr, GatewayControlAction::Interrupt, &mut physical,)
            .unwrap()
    );
    app.handle_tick(&mut sr, &mut transport, &mut physical)
        .unwrap();
    assert_eq!(transport, b"\x03");
    transport.clear();

    for (action, expected) in [
        (GatewayControlAction::ForceClose, b"\x1c".as_slice()),
        (
            GatewayControlAction::SshEscapeDisconnect,
            b"\r~.".as_slice(),
        ),
        (GatewayControlAction::SshEscapeHelp, b"\r~?".as_slice()),
    ] {
        assert!(
            app.request_tmux_gateway_action(&mut sr, action, &mut physical)
                .unwrap()
        );
        assert!(app.has_overlay());
        app.handle_tick(&mut sr, &mut transport, &mut physical)
            .unwrap();
        assert!(transport.is_empty(), "{action:?} escaped confirmation");
        app.handle_stdin(&mut sr, b"\r", &mut transport, &mut physical)
            .unwrap();
        transport.clear();
        app.handle_tick(&mut sr, &mut transport, &mut physical)
            .unwrap();
        assert_eq!(transport, expected, "{action:?}");
        transport.clear();
        assert!(!app.has_overlay());
        assert_eq!(app.tmux_connection_count(), 1);
    }
}

#[test]
fn writes_that_fail_do_not_duplicate_exceptional_gateway_bytes() {
    struct FailOnce {
        failed: bool,
        bytes: Vec<u8>,
    }

    impl Write for FailOnce {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if !self.failed {
                self.failed = true;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "fault injection",
                ));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let (mut app, mut sr, _recorder, _clock, mut physical) = app_with_clock();
    start_connection(&mut app, &mut sr, &mut physical);
    let mut initial_commands = Vec::new();
    app.handle_tick(&mut sr, &mut initial_commands, &mut physical)
        .unwrap();
    app.request_tmux_gateway_action(&mut sr, GatewayControlAction::Interrupt, &mut physical)
        .unwrap();
    let mut writer = FailOnce {
        failed: false,
        bytes: Vec::new(),
    };
    assert!(
        app.handle_tick(&mut sr, &mut writer, &mut physical)
            .is_err()
    );
    app.handle_tick(&mut sr, &mut writer, &mut physical)
        .unwrap();
    assert!(writer.bytes.is_empty(), "exceptional bytes were retried");
}

#[test]
fn real_tmux_server_death_with_a_lost_terminator_recovers_a_nested_parent() {
    assert!(
        std::process::Command::new("tmux")
            .arg("-V")
            .status()
            .unwrap()
            .success()
    );
    let unique = LIVE_RECOVERY_ID.fetch_add(1, Ordering::Relaxed);
    let socket_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-tmux");
    std::fs::create_dir_all(&socket_dir).unwrap();
    let socket = socket_dir.join(format!("recovery-{}-{unique}.sock", std::process::id()));
    struct ServerGuard(PathBuf);
    impl Drop for ServerGuard {
        fn drop(&mut self) {
            let _ = std::process::Command::new("tmux")
                .args(["-S", self.0.to_str().unwrap(), "kill-server"])
                .output();
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _guard = ServerGuard(socket.clone());

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
        &format!("lector_recovery_{}_{unique}", std::process::id()),
        "/bin/sh",
    ]);
    command.env("TERM", "xterm-256color");
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let read_thread = thread::spawn(move || {
        let mut output = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    output.extend_from_slice(&buffer[..count]);
                    if output
                        .windows(TMUX_CONTROL_START_MARKER.len())
                        .any(|window| window == TMUX_CONTROL_START_MARKER)
                    {
                        break;
                    }
                }
                Err(error) if error.raw_os_error() == Some(5) => break,
                Err(error) => panic!("read real recovery tmux PTY: {error}"),
            }
        }
        (output, reader)
    });
    let (mut control_stream, mut reader) = read_thread.join().unwrap();
    assert!(
        control_stream
            .windows(TMUX_CONTROL_START_MARKER.len())
            .any(|window| window == TMUX_CONTROL_START_MARKER),
        "real tmux emitted no marker: {control_stream:?}"
    );
    writer.write_all(b"kill-server\n").unwrap();
    writer.flush().unwrap();
    let tail_thread = thread::spawn(move || {
        let mut tail = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => tail.extend_from_slice(&buffer[..count]),
                Err(error) if error.raw_os_error() == Some(5) => break,
                Err(error) => panic!("read real recovery tmux tail: {error}"),
            }
        }
        tail
    });
    let _ = child.wait().unwrap();
    control_stream.extend(tail_thread.join().unwrap());
    assert!(
        control_stream.windows(5).any(|window| window == b"%exit"),
        "real tmux emitted no exit: {control_stream:?}"
    );
    assert!(control_stream.ends_with(b"\x1b\\"), "{control_stream:?}");

    let marker = control_stream
        .windows(TMUX_CONTROL_START_MARKER.len())
        .position(|window| window == TMUX_CONTROL_START_MARKER)
        .unwrap();
    let faulted = &control_stream[marker..control_stream.len() - 2];
    let (mut app, mut sr, recorder, clock, mut physical) = app_with_clock();
    ready_parent(&mut app, &mut sr, &mut physical);
    for chunk in faulted.chunks(7) {
        app.handle_pty(&mut sr, &pane_output(20, chunk), &mut physical)
            .unwrap();
    }
    assert_eq!(app.tmux_connection_count(), 2);
    clock.advance(1_001);
    let mut root = Vec::new();
    app.handle_tick(&mut sr, &mut root, &mut physical).unwrap();
    assert_eq!(app.tmux_connection_count(), 1);
    assert_eq!(app.active_tmux_connection(), Some(1));
    assert_eq!(app.debug_tmux_pane_portal_target(1, 20), None);
    app.handle_pty(
        &mut sr,
        &pane_output(20, b"parent$ after server death\r\n"),
        &mut physical,
    )
    .unwrap();
    assert!(
        app.debug_tmux_pane_contents(1, 20)
            .unwrap()
            .contains("parent$ after server death")
    );
    assert!(
        recorder
            .0
            .borrow()
            .iter()
            .any(|message| message.contains("timed out") && message.contains("connection 1"))
    );
}
