use lector::{
    app::App, screen_reader::ScreenReader, speech, tmux_gateway::TmuxGatewayRouter,
    tmux_model::INVENTORY_REPLY_COUNT, views,
};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    cell::RefCell,
    io::{Read, Write},
    path::PathBuf,
    rc::Rc,
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime},
};

const LAYOUT: &str = "b25f,80x24,0,0,20";

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

fn inventory(
    session_name: &str,
    window_name: &str,
    pane_title: &str,
    client: &str,
) -> Vec<Vec<Vec<u8>>> {
    vec![
        vec![format!("S\t$1\t{session_name}").into_bytes()],
        vec![format!("W\t$1\t@10\t1\t1\t{LAYOUT}\t{LAYOUT}\t*\t{window_name}").into_bytes()],
        vec![
            format!("P\t@10\t%20\t1\t1\t0\t0\t80\t24\t0\t0\t0\t1\t0\t0\t0\t0\t{pane_title}")
                .into_bytes(),
        ],
        vec![b"A\t$1".to_vec()],
        vec![b"O\tbase-index\t1".to_vec()],
        vec![b"O\tpane-base-index\t1".to_vec()],
        vec![format!("C\tclient_name\t{client}").into_bytes()],
        vec![b"O\tprefix\tC-a".to_vec()],
        vec![b"O\tprefix2\tNone".to_vec()],
        vec![b"O\tmode-keys\tvi".to_vec()],
        vec![b"O\trepeat-time\t500".to_vec()],
        vec![
            b"B\t1\t0\tselect-window -t :=1".to_vec(),
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

fn feed(
    app: &mut App,
    sr: &mut ScreenReader,
    router: &mut TmuxGatewayRouter,
    bytes: &[u8],
    physical: &mut Vec<u8>,
) {
    for event in router.push(bytes).unwrap() {
        app.handle_tmux_gateway_event(sr, event, physical).unwrap();
    }
}

fn add_ready_connection(
    app: &mut App,
    sr: &mut ScreenReader,
    physical: &mut Vec<u8>,
    connection_id: u64,
    groups: Vec<Vec<Vec<u8>>>,
    initial_text: &str,
) -> TmuxGatewayRouter {
    let mut router = TmuxGatewayRouter::with_first_connection_id(connection_id);
    feed(
        app,
        sr,
        &mut router,
        b"\x1bP1000p%begin 1 1 0\n%end 1 1 0\n",
        physical,
    );
    let mut commands = Vec::new();
    app.drain_tmux_commands_for(connection_id, &mut commands)
        .unwrap();
    assert_eq!(
        commands,
        [
            lector::app::TMUX_FLOW_CONTROL_COMMAND,
            lector::tmux_model::INVENTORY_COMMAND.as_bytes(),
        ]
        .concat()
    );
    feed(app, sr, &mut router, &reply(2, &[], true), physical);
    assert_eq!(groups.len(), INVENTORY_REPLY_COUNT);
    for (index, group) in groups.iter().enumerate() {
        feed(
            app,
            sr,
            &mut router,
            &reply(index + 3, group, true),
            physical,
        );
    }
    commands.clear();
    app.drain_tmux_commands_for(connection_id, &mut commands)
        .unwrap();
    assert_eq!(commands, b"capture-pane -p -e -J -S - -t %20\n");
    feed(
        app,
        sr,
        &mut router,
        &reply(30, &[initial_text.as_bytes().to_vec()], true),
        physical,
    );
    router
}

fn input(app: &mut App, sr: &mut ScreenReader, physical: &mut Vec<u8>, bytes: &[u8]) -> Vec<u8> {
    let mut root = Vec::new();
    app.handle_stdin(sr, bytes, &mut root, physical).unwrap();
    root
}

fn drain(app: &mut App, connection_id: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    app.drain_tmux_commands_for(connection_id, &mut bytes)
        .unwrap();
    bytes
}

fn app() -> (App, ScreenReader, Recorder, Vec<u8>) {
    let recorder = Recorder::default();
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(24, 80)));
    let app = App::new(stack).unwrap();
    let sr = ScreenReader::new(speech::Speech::new(Box::new(recorder.clone())));
    (app, sr, recorder, Vec::new())
}

#[test]
fn identical_tmux_ids_remain_isolated_across_connections_input_replies_and_speech() {
    let (mut app, mut sr, recorder, mut physical) = app();
    let mut first = add_ready_connection(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        inventory("same", "same", "same", "/dev/ttys-first"),
        "FIRST",
    );
    let mut second = add_ready_connection(
        &mut app,
        &mut sr,
        &mut physical,
        2,
        inventory("same", "same", "same", "/dev/ttys-second"),
        "SECOND",
    );
    assert_eq!(app.tmux_connection_count(), 2);
    assert_eq!(app.active_tmux_connection(), Some(2));
    assert!(app.debug_active_view_contents().contains("SECOND"));

    feed(
        &mut app,
        &mut sr,
        &mut first,
        b"%output %20 hidden-first\n",
        &mut physical,
    );
    assert!(!app.debug_active_view_contents().contains("hidden-first"));
    assert!(
        app.activate_tmux_connection(1, &mut sr, &mut physical)
            .unwrap()
    );
    assert!(app.debug_active_view_contents().contains("hidden-first"));

    input(&mut app, &mut sr, &mut physical, b"A");
    assert!(String::from_utf8_lossy(&drain(&mut app, 1)).contains("-t %20 41"));
    assert!(drain(&mut app, 2).is_empty());

    input(&mut app, &mut sr, &mut physical, b"\x01n");
    assert_eq!(drain(&mut app, 1), b"next-window\n");
    assert!(drain(&mut app, 2).is_empty());

    assert!(
        app.activate_tmux_connection(2, &mut sr, &mut physical)
            .unwrap()
    );
    input(
        &mut app,
        &mut sr,
        &mut physical,
        b"\x01:display-message second\r",
    );
    assert_eq!(drain(&mut app, 2), b"display-message second\n");
    assert!(drain(&mut app, 1).is_empty());
    feed(
        &mut app,
        &mut sr,
        &mut first,
        &reply(90, &[b"wrong connection".to_vec()], false),
        &mut physical,
    );
    assert!(
        !app.debug_active_view_contents()
            .contains("wrong connection")
    );
    assert!(!app.has_overlay());
    feed(
        &mut app,
        &mut sr,
        &mut second,
        &reply(91, &[b"right connection".to_vec()], false),
        &mut physical,
    );
    assert!(app.has_overlay());
    assert!(
        app.debug_active_view_contents()
            .contains("right connection")
    );
    input(&mut app, &mut sr, &mut physical, b"\r");

    recorder.0.borrow_mut().clear();
    input(&mut app, &mut sr, &mut physical, b"\x1bw");
    assert!(
        recorder
            .0
            .borrow()
            .iter()
            .any(|message| message == "tmux, tmux 2, same"),
        "speech={:?}",
        recorder.0.borrow()
    );
}

#[test]
fn rapid_connection_switches_never_cross_pane_state_or_input_routes() {
    let (mut app, mut sr, _recorder, mut physical) = app();
    let mut first = add_ready_connection(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        inventory("one", "one", "one", "/dev/ttys-one"),
        "FIRST",
    );
    let mut second = add_ready_connection(
        &mut app,
        &mut sr,
        &mut physical,
        2,
        inventory("two", "two", "two", "/dev/ttys-two"),
        "SECOND",
    );

    let mut switch_latencies = Vec::with_capacity(100);
    for round in 0..50 {
        let switch_started = Instant::now();
        assert!(
            app.activate_tmux_connection(1, &mut sr, &mut physical)
                .unwrap()
        );
        switch_latencies.push(switch_started.elapsed());
        feed(
            &mut app,
            &mut sr,
            &mut first,
            format!("%output %20 first-{round}\n").as_bytes(),
            &mut physical,
        );
        input(&mut app, &mut sr, &mut physical, b"1");
        assert!(String::from_utf8_lossy(&drain(&mut app, 1)).contains("-t %20 31"));
        assert!(drain(&mut app, 2).is_empty());

        let switch_started = Instant::now();
        assert!(
            app.activate_tmux_connection(2, &mut sr, &mut physical)
                .unwrap()
        );
        switch_latencies.push(switch_started.elapsed());
        feed(
            &mut app,
            &mut sr,
            &mut second,
            format!("%output %20 second-{round}\n").as_bytes(),
            &mut physical,
        );
        input(&mut app, &mut sr, &mut physical, b"2");
        assert!(String::from_utf8_lossy(&drain(&mut app, 2)).contains("-t %20 32"));
        assert!(drain(&mut app, 1).is_empty());
    }

    app.activate_tmux_connection(1, &mut sr, &mut physical)
        .unwrap();
    let first_contents = app.debug_active_view_contents();
    assert!(first_contents.contains("first-49"), "{first_contents:?}");
    assert!(!first_contents.contains("second-49"), "{first_contents:?}");
    app.activate_tmux_connection(2, &mut sr, &mut physical)
        .unwrap();
    let second_contents = app.debug_active_view_contents();
    assert!(second_contents.contains("second-49"), "{second_contents:?}");
    assert!(!second_contents.contains("first-49"), "{second_contents:?}");
    switch_latencies.sort_unstable();
    let p95 = switch_latencies[switch_latencies.len() * 95 / 100];
    assert!(
        p95 < Duration::from_secs(1),
        "connection-switch p95 was {p95:?}"
    );
    eprintln!("tmux connection-switch p95: {p95:?}");
}

#[test]
fn connection_chooser_switches_terminal_and_connections_and_survives_selected_removal() {
    let (mut app, mut sr, _recorder, mut physical) = app();
    let mut first = TmuxGatewayRouter::with_first_connection_id(1);
    feed(
        &mut app,
        &mut sr,
        &mut first,
        b"gateway prompt\r\n\x1bP1000p%begin 1 1 0\n%end 1 1 0\n",
        &mut physical,
    );
    let mut ignored = Vec::new();
    app.drain_tmux_commands_for(1, &mut ignored).unwrap();
    assert_eq!(
        ignored,
        [
            lector::app::TMUX_FLOW_CONTROL_COMMAND,
            lector::tmux_model::INVENTORY_COMMAND.as_bytes(),
        ]
        .concat()
    );
    feed(
        &mut app,
        &mut sr,
        &mut first,
        &reply(2, &[], true),
        &mut physical,
    );
    for (index, group) in inventory("one", "one-window", "one-pane", "/dev/ttys-one")
        .iter()
        .enumerate()
    {
        feed(
            &mut app,
            &mut sr,
            &mut first,
            &reply(index + 3, group, true),
            &mut physical,
        );
    }
    ignored.clear();
    app.drain_tmux_commands_for(1, &mut ignored).unwrap();
    feed(
        &mut app,
        &mut sr,
        &mut first,
        &reply(30, &[b"ONE".to_vec()], true),
        &mut physical,
    );
    let mut second = add_ready_connection(
        &mut app,
        &mut sr,
        &mut physical,
        2,
        inventory("two", "two-window", "two-pane", "/dev/ttys-two"),
        "TWO",
    );

    assert!(
        app.show_tmux_connection_chooser(&mut sr, &mut physical)
            .unwrap()
    );
    let chooser = app.debug_active_view_contents();
    assert!(chooser.contains("terminal"), "{chooser:?}");
    assert!(chooser.contains("connection 1 tmux 1"), "{chooser:?}");
    assert!(chooser.contains("connection 2 tmux 2"), "{chooser:?}");
    input(&mut app, &mut sr, &mut physical, b"\x1b[A\r");
    assert_eq!(app.active_tmux_connection(), Some(1));
    assert!(app.debug_active_view_contents().contains("ONE"));

    assert!(
        app.show_tmux_connection_chooser(&mut sr, &mut physical)
            .unwrap()
    );
    input(&mut app, &mut sr, &mut physical, b"\x1b[A\r");
    assert_eq!(app.active_tmux_connection(), None);
    assert!(app.debug_active_view_contents().contains("gateway prompt"));

    assert!(
        app.activate_tmux_connection(2, &mut sr, &mut physical)
            .unwrap()
    );
    assert!(
        app.show_tmux_connection_chooser(&mut sr, &mut physical)
            .unwrap()
    );
    feed(
        &mut app,
        &mut sr,
        &mut second,
        b"%exit removed while selected\n\x1b\\",
        &mut physical,
    );
    assert!(app.has_overlay(), "the chooser should remain reachable");
    let chooser = app.debug_active_view_contents();
    assert!(!chooser.contains("connection 2"), "{chooser:?}");
    assert!(chooser.contains("connection 1"), "{chooser:?}");
    input(&mut app, &mut sr, &mut physical, b"\r");
    assert_eq!(app.active_tmux_connection(), Some(1));
}

#[test]
fn prefix_partial_state_command_history_and_detach_are_scoped_to_the_announced_connection() {
    let (mut app, mut sr, _recorder, mut physical) = app();
    let _first = add_ready_connection(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        inventory("one", "one", "one", "/dev/ttys-one"),
        "ONE",
    );
    let _second = add_ready_connection(
        &mut app,
        &mut sr,
        &mut physical,
        2,
        inventory("two", "two", "two", "/dev/ttys-two"),
        "TWO",
    );

    assert!(
        app.activate_tmux_connection(1, &mut sr, &mut physical)
            .unwrap()
    );
    input(&mut app, &mut sr, &mut physical, b"\x01");
    assert!(
        app.activate_tmux_connection(2, &mut sr, &mut physical)
            .unwrap()
    );
    input(&mut app, &mut sr, &mut physical, b"\x01");
    assert!(
        app.activate_tmux_connection(1, &mut sr, &mut physical)
            .unwrap()
    );
    input(&mut app, &mut sr, &mut physical, b"n");
    assert_eq!(drain(&mut app, 1), b"next-window\n");
    assert!(drain(&mut app, 2).is_empty());
    assert!(
        app.activate_tmux_connection(2, &mut sr, &mut physical)
            .unwrap()
    );
    input(&mut app, &mut sr, &mut physical, b"n");
    assert_eq!(drain(&mut app, 2), b"next-window\n");
    assert!(drain(&mut app, 1).is_empty());

    input(&mut app, &mut sr, &mut physical, b"\x01d");
    assert_eq!(drain(&mut app, 2), b"detach-client -t =/dev/ttys-two\n");
    assert!(drain(&mut app, 1).is_empty());

    assert!(
        app.show_tmux_command_prompt(&mut sr, &mut physical)
            .unwrap()
    );
    input(&mut app, &mut sr, &mut physical, b"second-history\r");
    assert_eq!(drain(&mut app, 2), b"second-history\n");
    assert!(
        app.activate_tmux_connection(1, &mut sr, &mut physical)
            .unwrap()
    );
    assert!(
        app.show_tmux_command_prompt(&mut sr, &mut physical)
            .unwrap()
    );
    input(&mut app, &mut sr, &mut physical, b"\x1b[A");
    assert!(!app.debug_active_view_contents().contains("second-history"));
}

#[test]
fn connection_labels_are_stable_distinguish_duplicates_and_do_not_leak_to_new_identity() {
    let (mut app, mut sr, _recorder, mut physical) = app();
    let mut first = add_ready_connection(
        &mut app,
        &mut sr,
        &mut physical,
        1,
        inventory("one", "one", "one", "/dev/ttys-one"),
        "ONE",
    );
    let mut second = add_ready_connection(
        &mut app,
        &mut sr,
        &mut physical,
        2,
        inventory("two", "two", "two", "/dev/ttys-two"),
        "TWO",
    );

    assert!(
        app.activate_tmux_connection(1, &mut sr, &mut physical)
            .unwrap()
    );
    assert!(
        app.show_tmux_connection_rename(&mut sr, &mut physical)
            .unwrap()
    );
    input(&mut app, &mut sr, &mut physical, b"shared label\r");
    assert!(
        app.debug_tmux_topology(1)
            .unwrap()
            .starts_with("connection 1: shared label")
    );
    assert!(
        app.show_tmux_connection_rename(&mut sr, &mut physical)
            .unwrap()
    );
    input(&mut app, &mut sr, &mut physical, b"   \r");
    assert!(app.has_overlay());
    assert!(
        app.debug_active_view_contents()
            .contains("connection label must contain 1 to 256 bytes")
    );
    input(&mut app, &mut sr, &mut physical, b"\r");
    assert!(
        app.debug_tmux_topology(1)
            .unwrap()
            .starts_with("connection 1: shared label")
    );

    assert!(
        app.activate_tmux_connection(2, &mut sr, &mut physical)
            .unwrap()
    );
    assert!(
        app.show_tmux_connection_rename(&mut sr, &mut physical)
            .unwrap()
    );
    input(&mut app, &mut sr, &mut physical, b"shared label\r");
    assert!(
        app.show_tmux_connection_chooser(&mut sr, &mut physical)
            .unwrap()
    );
    let duplicate = app.debug_active_view_contents();
    assert!(
        duplicate.contains("connection 1 shared label"),
        "{duplicate:?}"
    );
    assert!(
        duplicate.contains("connection 2 shared label"),
        "{duplicate:?}"
    );
    input(&mut app, &mut sr, &mut physical, b"\x1b");

    feed(
        &mut app,
        &mut sr,
        &mut first,
        b"%sessions-changed\n",
        &mut physical,
    );
    assert_eq!(
        drain(&mut app, 1),
        lector::tmux_model::INVENTORY_COMMAND.as_bytes()
    );
    for (index, group) in inventory("one", "one", "one", "/dev/ttys-one")
        .iter()
        .enumerate()
    {
        feed(
            &mut app,
            &mut sr,
            &mut first,
            &reply(100 + index, group, true),
            &mut physical,
        );
    }
    assert!(
        app.debug_tmux_topology(1)
            .unwrap()
            .starts_with("connection 1: shared label")
    );

    feed(
        &mut app,
        &mut sr,
        &mut second,
        b"%exit old identity\n\x1b\\",
        &mut physical,
    );
    let _third = add_ready_connection(
        &mut app,
        &mut sr,
        &mut physical,
        3,
        inventory("three", "three", "three", "/dev/ttys-three"),
        "THREE",
    );
    assert!(
        app.debug_tmux_topology(3)
            .unwrap()
            .starts_with("connection 3: tmux 3")
    );
}

struct DisposableServer {
    socket: PathBuf,
}

impl Drop for DisposableServer {
    fn drop(&mut self) {
        let _ = std::process::Command::new("tmux")
            .args(["-S", self.socket.to_str().unwrap(), "kill-server"])
            .output();
        let _ = std::fs::remove_file(&self.socket);
    }
}

struct RealTransport {
    connection_id: u64,
    router: TmuxGatewayRouter,
    receiver: mpsc::Receiver<Vec<u8>>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    read_thread: Option<thread::JoinHandle<()>>,
    _master: Box<dyn MasterPty + Send>,
    server: DisposableServer,
}

impl RealTransport {
    fn spawn(connection_id: u64, name: &str, text: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let socket_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-tmux");
        std::fs::create_dir_all(&socket_dir).unwrap();
        let socket = socket_dir.join(format!(
            "multi-{connection_id}-{}-{unique}.sock",
            std::process::id()
        ));
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new("tmux");
        let child_program = format!(
            "/usr/bin/perl -e '$|=1; system q(stty raw -echo); print q({text}); \
             while (sysread(STDIN,$c,1)) {{ print $c; }}'"
        );
        command.args([
            "-S",
            socket.to_str().unwrap(),
            "-f",
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/tmux-prefix.conf"
            ),
            "-CC",
            "new-session",
            "-s",
            name,
            &child_program,
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
                    Err(error) => panic!("read multi-connection tmux PTY: {error}"),
                }
            }
        });
        Self {
            connection_id,
            router: TmuxGatewayRouter::with_first_connection_id(connection_id),
            receiver,
            writer,
            child,
            read_thread: Some(read_thread),
            _master: pair.master,
            server: DisposableServer { socket },
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
                    let clients = std::process::Command::new("tmux")
                        .args([
                            "-S",
                            self.server.socket.to_str().unwrap(),
                            "list-clients",
                            "-F",
                            "#{client_name} #{client_control_mode}",
                        ])
                        .output();
                    let pane = std::process::Command::new("tmux")
                        .args([
                            "-S",
                            self.server.socket.to_str().unwrap(),
                            "capture-pane",
                            "-p",
                            "-t",
                            ":",
                        ])
                        .output();
                    panic!(
                        "{case}: {error}; child={child_status:?}; clients={clients:?}; pane={pane:?}"
                    )
                });
            feed(app, sr, &mut self.router, &chunk, physical);
            app.drain_tmux_commands_for(self.connection_id, self.writer.as_mut())
                .unwrap();
            self.writer.flush().unwrap();
        }
        panic!("{case} exceeded its bounded event count");
    }

    fn assert_server_pane_contains(&self, case: &str, expected: &str) {
        let mut last = Vec::new();
        for _ in 0..32 {
            let output = std::process::Command::new("tmux")
                .args([
                    "-S",
                    self.server.socket.to_str().unwrap(),
                    "capture-pane",
                    "-p",
                    "-t",
                    ":",
                ])
                .output()
                .unwrap_or_else(|error| panic!("{case}: capture live pane: {error}"));
            assert!(output.status.success(), "{case}: {output:?}");
            last = output.stdout;
            if String::from_utf8_lossy(&last).contains(expected) {
                return;
            }
        }
        panic!(
            "{case}: live pane never contained {expected:?}; last={:?}",
            String::from_utf8_lossy(&last)
        );
    }

    fn shutdown(mut self) {
        let _ = std::process::Command::new("tmux")
            .args(["-S", self.server.socket.to_str().unwrap(), "kill-server"])
            .status();
        let _ = self.child.wait();
        if let Some(thread) = self.read_thread.take() {
            thread.join().unwrap();
        }
    }
}

#[test]
fn two_real_disposable_tmux_servers_keep_input_output_and_selection_independent() {
    assert!(
        std::process::Command::new("tmux")
            .arg("-V")
            .status()
            .unwrap()
            .success()
    );
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mut one = RealTransport::spawn(1, &format!("lector_multi_one_{unique}"), "REAL_ONE");
    let mut two = RealTransport::spawn(2, &format!("lector_multi_two_{unique}"), "REAL_TWO");
    let (mut app, mut sr, _recorder, mut physical) = app();
    one.drive(
        "first server bootstrap",
        &mut app,
        &mut sr,
        &mut physical,
        |app| app.debug_active_view_contents().contains("REAL_ONE"),
    );
    two.drive(
        "second server bootstrap",
        &mut app,
        &mut sr,
        &mut physical,
        |app| app.debug_active_view_contents().contains("REAL_TWO"),
    );
    assert_eq!(app.tmux_connection_count(), 2);

    assert!(
        app.activate_tmux_connection(1, &mut sr, &mut physical)
            .unwrap()
    );
    input(&mut app, &mut sr, &mut physical, b"1");
    app.drain_tmux_commands_for(1, one.writer.as_mut()).unwrap();
    assert!(drain(&mut app, 2).is_empty());
    one.writer.flush().unwrap();
    one.assert_server_pane_contains("first server input", "REAL_ONE1");

    assert!(
        app.activate_tmux_connection(2, &mut sr, &mut physical)
            .unwrap()
    );
    input(&mut app, &mut sr, &mut physical, b"2");
    app.drain_tmux_commands_for(2, two.writer.as_mut()).unwrap();
    assert!(drain(&mut app, 1).is_empty());
    two.writer.flush().unwrap();
    two.assert_server_pane_contains("second server input", "REAL_TWO2");

    assert!(
        app.show_tmux_connection_chooser(&mut sr, &mut physical)
            .unwrap()
    );
    input(&mut app, &mut sr, &mut physical, b"\x1b[A");
    input(&mut app, &mut sr, &mut physical, b"\r");
    assert_eq!(app.active_tmux_connection(), Some(1));
    assert!(app.debug_active_view_contents().contains("REAL_ONE"));

    one.shutdown();
    two.shutdown();
}
