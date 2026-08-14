use lector::{
    app::App,
    screen_reader::ScreenReader,
    speech,
    tmux_gateway::{GatewayEvent, TmuxGatewayRouter},
    tmux_model::{
        INVENTORY_COMMAND, INVENTORY_REPLY_COUNT, PaneId, ReconcileOutcome, SessionId,
        TmuxTopology, WindowId,
    },
    views,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::mpsc,
    thread,
    time::Duration,
};

fn complete_inventory() -> Vec<Vec<u8>> {
    [
        "S\t$1\twork",
        "S\t$2\twork",
        "W\t$1\t@10\t1\t1\tb25f-layout\tb25f-visible\t*\teditor",
        "W\t$1\t@11\t2\t0\tcafe-layout\tcafe-visible\t-\tshell",
        "W\t$2\t@10\t3\t1\tb25f-layout\tb25f-visible\t*\teditor",
        "P\t@10\t%20\t1\t1\t0\t0\t80\t24\t0\tvim",
        "P\t@10\t%21\t2\t0\t80\t0\t80\t24\t0\ttests",
        "P\t@11\t%22\t1\t1\t0\t0\t160\t24\t0\tbash",
        "A\t$1",
        "O\tbase-index\t1",
        "O\tpane-base-index\t1",
        "C\tclient_name\t/dev/ttys001",
        "O\tprefix\tC-a",
        "O\tprefix2\tNone",
        "O\tmode-keys\tvi",
        "O\trepeat-time\t500",
        "B\tn\t0\tnext-window",
    ]
    .into_iter()
    .map(|line| line.as_bytes().to_vec())
    .collect()
}

#[test]
fn inventory_preserves_stable_ids_duplicate_names_indexes_and_links() {
    let mut topology = TmuxTopology::new(7);
    topology.replace_inventory(&complete_inventory()).unwrap();

    assert_eq!(topology.connection_id(), 7);
    assert_eq!(topology.attached_session(), Some(SessionId(1)));
    assert_eq!(topology.sessions().len(), 2);
    assert_eq!(topology.session(SessionId(1)).unwrap().name, "work");
    assert_eq!(topology.session(SessionId(2)).unwrap().name, "work");
    assert_eq!(
        topology.session(SessionId(1)).unwrap().windows.get(&1),
        Some(&WindowId(10))
    );
    assert_eq!(
        topology.session(SessionId(2)).unwrap().windows.get(&3),
        Some(&WindowId(10)),
        "linked window lost its per-session index"
    );
    assert_eq!(topology.window(WindowId(10)).unwrap().links.len(), 2);
    assert_eq!(
        topology.window(WindowId(10)).unwrap().active_pane,
        Some(PaneId(20))
    );
    assert_eq!(topology.pane(PaneId(21)).unwrap().index, 2);
    assert_eq!(topology.option("base-index"), Some("1"));
    assert_eq!(topology.client_info("client_name"), Some("/dev/ttys001"));
    assert!(!topology.needs_resync());
}

#[test]
fn linked_window_inventory_deduplicates_identical_pane_records() {
    let mut inventory = complete_inventory();
    inventory.insert(8, inventory[5].clone());
    let mut topology = TmuxTopology::new(7);
    topology.replace_inventory(&inventory).unwrap();
    assert_eq!(topology.window(WindowId(10)).unwrap().links.len(), 2);
    assert_eq!(topology.pane(PaneId(20)).unwrap().title, "vim");

    inventory[8] = b"P\t@10\t%20\t1\t1\t0\t0\t80\t24\t0\tcontradiction".to_vec();
    assert!(topology.replace_inventory(&inventory).is_err());
}

#[test]
fn notifications_reconcile_renames_focus_close_and_out_of_order_ids() {
    let mut topology = TmuxTopology::new(1);
    topology.replace_inventory(&complete_inventory()).unwrap();

    assert_eq!(
        topology
            .apply_notification(b"window-renamed", b"@10 editor renamed")
            .unwrap(),
        ReconcileOutcome::Applied
    );
    topology
        .apply_notification(b"session-changed", b"$2 work")
        .unwrap();
    topology
        .apply_notification(b"session-renamed", b"duplicate still allowed")
        .unwrap();
    topology
        .apply_notification(b"session-window-changed", b"$2 @10")
        .unwrap();
    topology
        .apply_notification(b"window-pane-changed", b"@10 %21")
        .unwrap();

    assert_eq!(topology.attached_session(), Some(SessionId(2)));
    assert_eq!(
        topology.session(SessionId(2)).unwrap().name,
        "duplicate still allowed"
    );
    assert_eq!(
        topology.window(WindowId(10)).unwrap().name,
        "editor renamed"
    );
    assert_eq!(
        topology.window(WindowId(10)).unwrap().active_pane,
        Some(PaneId(21))
    );

    assert_eq!(
        topology
            .apply_notification(b"window-pane-changed", b"@999 %888")
            .unwrap(),
        ReconcileOutcome::ResyncRequired
    );
    assert!(topology.window(WindowId(999)).is_some());
    assert!(topology.pane(PaneId(888)).is_some());
    assert!(topology.needs_resync());

    topology
        .apply_notification(b"window-close", b"@10")
        .unwrap();
    assert!(
        !topology
            .session(SessionId(2))
            .unwrap()
            .windows
            .contains_key(&3)
    );
    assert!(
        topology.window(WindowId(10)).is_some(),
        "window linked into another session was destroyed"
    );
}

#[test]
fn full_resync_is_transactional_idempotent_and_clears_contradictions() {
    let inventory = complete_inventory();
    let mut topology = TmuxTopology::new(4);
    topology.set_label("production").unwrap();
    topology.replace_inventory(&inventory).unwrap();
    let expected = topology.clone();

    topology
        .apply_notification(b"sessions-changed", b"")
        .unwrap();
    assert!(topology.needs_resync());
    topology.replace_inventory(&inventory).unwrap();
    assert_eq!(topology, expected);
    topology.replace_inventory(&inventory).unwrap();
    assert_eq!(topology, expected, "identical resync was not idempotent");

    let before_bad_snapshot = topology.clone();
    let mut malformed = inventory.clone();
    malformed.push(b"P\t@10\t%not-an-id\t1\t1\t0\t0\t1\t1\t0\tbad".to_vec());
    assert!(topology.replace_inventory(&malformed).is_err());
    assert_eq!(
        topology, before_bad_snapshot,
        "failed resync partially mutated state"
    );
}

#[test]
fn generated_and_user_connection_labels_are_stable_and_bounded() {
    let mut topology = TmuxTopology::new(42);
    assert_eq!(topology.label(), "tmux 42");
    topology.set_label("remote work").unwrap();
    assert_eq!(topology.label(), "remote work");
    assert!(topology.set_label("").is_err());
    assert!(topology.set_label("   \t").is_err());
    assert!(topology.set_label("line\nbreak").is_err());
    assert!(topology.set_label("escape\u{1b}").is_err());
    assert!(topology.set_label(&"x".repeat(257)).is_err());
    assert_eq!(topology.label(), "remote work");

    topology.set_label("  trimmed label  ").unwrap();
    assert_eq!(topology.label(), "trimmed label");
}

#[test]
fn inventory_accepts_empty_optional_values_and_tabs_in_final_names() {
    let lines = [
        b"S\t$1\tname with\ttab".to_vec(),
        b"W\t$1\t@1\t1\t1\tlayout\tvisible\t\twindow with\ttab".to_vec(),
        b"P\t@1\t%1\t1\t1\t0\t0\t80\t24\t0\t".to_vec(),
        b"A\t$1".to_vec(),
        b"C\tclient_name\t".to_vec(),
    ];
    let mut topology = TmuxTopology::new(1);
    topology.replace_inventory(&lines).unwrap();

    assert_eq!(
        topology.session(SessionId(1)).unwrap().name,
        "name with\ttab"
    );
    assert_eq!(
        topology.window(WindowId(1)).unwrap().name,
        "window with\ttab"
    );
    assert_eq!(topology.window(WindowId(1)).unwrap().flags, "");
    assert_eq!(topology.pane(PaneId(1)).unwrap().title, "");
    assert_eq!(topology.client_info("client_name"), Some(""));
}

#[test]
fn layout_pane_exit_unknown_notifications_and_server_restart_are_coherent() {
    let mut topology = TmuxTopology::new(1);
    topology.replace_inventory(&complete_inventory()).unwrap();
    let unchanged = topology.clone();
    assert_eq!(
        topology
            .apply_notification(b"future-event", b"anything")
            .unwrap(),
        ReconcileOutcome::Ignored
    );
    assert_eq!(topology, unchanged);

    topology
        .apply_notification(b"layout-change", b"@10 new-layout new-visible Z")
        .unwrap();
    assert_eq!(topology.window(WindowId(10)).unwrap().layout, "new-layout");
    topology.apply_notification(b"pane-exited", b"%20").unwrap();
    assert!(topology.pane(PaneId(20)).is_none());
    assert_eq!(topology.window(WindowId(10)).unwrap().active_pane, None);

    let restarted = [
        b"S\t$50\trestarted".to_vec(),
        b"W\t$50\t@60\t1\t1\tnew\tnew\t*\tmain".to_vec(),
        b"P\t@60\t%70\t1\t1\t0\t0\t90\t30\t0\tshell".to_vec(),
        b"A\t$50".to_vec(),
    ];
    topology.replace_inventory(&restarted).unwrap();
    assert!(topology.session(SessionId(1)).is_none());
    assert!(topology.window(WindowId(10)).is_none());
    assert!(topology.pane(PaneId(20)).is_none());
    assert_eq!(topology.attached_session(), Some(SessionId(50)));
}

#[test]
fn inventory_command_uses_explicit_machine_formats_for_every_model_layer() {
    for command in [
        "list-sessions -F",
        "list-windows -a -F",
        "list-panes -a -F",
        "#{session_id}",
        "#{window_id}",
        "#{pane_id}",
        "#{cursor_x}",
        "#{cursor_y}",
        "#{cursor_flag}",
        "#{cursor_shape}",
        "#{alternate_on}",
        "#{pane_in_mode}",
        "#{history_size}",
        "#{window_layout}",
        "#{window_visible_layout}",
        "#{client_name}",
        "#{base-index}",
        "#{pane-base-index}",
        "#{prefix}",
        "#{prefix2}",
        "#{mode-keys}",
        "#{repeat-time}",
        "list-keys -T prefix -F",
        "#{key_string}",
        "#{key_repeat}",
        "#{key_command}",
    ] {
        assert!(INVENTORY_COMMAND.contains(command), "missing {command:?}");
    }
    assert!(INVENTORY_COMMAND.ends_with('\n'));
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

#[test]
fn app_queues_inventory_after_handshake_and_exposes_accessible_debug_dump() {
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(8, 80)));
    let mut app = App::new(stack).unwrap();
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
            INVENTORY_COMMAND.as_bytes(),
        ]
        .concat()
    );
    app.handle_pty(&mut sr, b"%begin 2 2 0\n%end 2 2 0\n", &mut physical)
        .unwrap();

    let inventory = complete_inventory();
    let groups = [
        &inventory[0..2],
        &inventory[2..5],
        &inventory[5..8],
        &inventory[8..9],
        &inventory[9..10],
        &inventory[10..11],
        &inventory[11..12],
        &inventory[12..13],
        &inventory[13..14],
        &inventory[14..15],
        &inventory[15..16],
        &inventory[16..17],
    ];
    for (index, group) in groups.into_iter().enumerate() {
        let output = group
            .iter()
            .map(|line| String::from_utf8(line.clone()).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let response = format!(
            "%begin {} {} 0\n{output}\n%end {} {} 0\n",
            index + 3,
            index + 3,
            index + 3,
            index + 3
        );
        app.handle_pty(&mut sr, response.as_bytes(), &mut physical)
            .unwrap();
        if index + 1 < INVENTORY_REPLY_COUNT {
            assert_eq!(
                app.debug_tmux_topology(1).unwrap(),
                "connection 1: tmux 1\n",
                "partial inventory became visible before the batch was complete"
            );
        }
    }
    app.handle_pty(&mut sr, b"%window-renamed @10 edited\n", &mut physical)
        .unwrap();

    let dump = app
        .debug_tmux_topology(1)
        .expect("connection topology dump");
    assert!(dump.contains("connection 1: tmux 1"));
    assert!(dump.contains("session $1 [attached]: work"));
    assert!(dump.contains("window @10 index 1: edited"));
    assert!(dump.contains("pane %20 index 1"));
    assert!(!dump.contains("%begin"));

    control_input.clear();
    app.handle_tick(&mut sr, &mut control_input, &mut physical)
        .unwrap();
    for pane_id in [20, 21, 22] {
        assert!(
            control_input
                .windows(format!("-t %{pane_id}\n").len())
                .any(|bytes| bytes == format!("-t %{pane_id}\n").as_bytes())
        );
    }
    for number in 10..13 {
        let response = format!("%begin {number} {number} 0\n%end {number} {number} 0\n");
        app.handle_pty(&mut sr, response.as_bytes(), &mut physical)
            .unwrap();
    }

    control_input.clear();
    app.handle_pty(
        &mut sr,
        b"%sessions-changed\n%sessions-changed\n",
        &mut physical,
    )
    .unwrap();
    app.handle_tick(&mut sr, &mut control_input, &mut physical)
        .unwrap();
    assert_eq!(
        control_input,
        INVENTORY_COMMAND.as_bytes(),
        "duplicate invalidations queued duplicate full resyncs"
    );

    let mut externally_changed = complete_inventory();
    externally_changed[0] = b"S\t$1\texternally renamed".to_vec();
    let groups = [
        &externally_changed[0..2],
        &externally_changed[2..5],
        &externally_changed[5..8],
        &externally_changed[8..9],
        &externally_changed[9..10],
        &externally_changed[10..11],
        &externally_changed[11..12],
        &externally_changed[12..13],
        &externally_changed[13..14],
        &externally_changed[14..15],
        &externally_changed[15..16],
        &externally_changed[16..17],
    ];
    for (index, group) in groups.into_iter().enumerate() {
        let output = group
            .iter()
            .map(|line| String::from_utf8(line.clone()).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let response = format!(
            "%begin {} {} 0\n{output}\n%end {} {} 0\n",
            index + 20,
            index + 20,
            index + 20,
            index + 20
        );
        app.handle_pty(&mut sr, response.as_bytes(), &mut physical)
            .unwrap();
    }
    assert!(
        app.debug_tmux_topology(1)
            .unwrap()
            .contains("session $1 [attached]: externally renamed"),
        "full resync did not recover a deliberately dropped rename notification"
    );
}

#[test]
fn failed_inventory_batch_retries_once_without_publishing_partial_topology() {
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(8, 80)));
    let mut app = App::new(stack).unwrap();
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
            INVENTORY_COMMAND.as_bytes(),
        ]
        .concat()
    );
    control_input.clear();
    app.handle_pty(&mut sr, b"%begin 2 2 0\n%end 2 2 0\n", &mut physical)
        .unwrap();

    for number in 3..(3 + INVENTORY_REPLY_COUNT) {
        let response = format!("%begin {number} {number} 0\nfailed\n%error {number} {number} 0\n");
        app.handle_pty(&mut sr, response.as_bytes(), &mut physical)
            .unwrap();
    }
    assert_eq!(
        app.debug_tmux_topology(1).unwrap(),
        "connection 1: tmux 1\n",
        "failed inventory published partial topology"
    );
    app.handle_tick(&mut sr, &mut control_input, &mut physical)
        .unwrap();
    assert_eq!(control_input, INVENTORY_COMMAND.as_bytes());

    control_input.clear();
    for number in 20..(20 + INVENTORY_REPLY_COUNT) {
        let response = format!("%begin {number} {number} 0\nfailed\n%error {number} {number} 0\n");
        app.handle_pty(&mut sr, response.as_bytes(), &mut physical)
            .unwrap();
    }
    app.handle_tick(&mut sr, &mut control_input, &mut physical)
        .unwrap();
    assert!(
        control_input.is_empty(),
        "persistent inventory error retried forever"
    );
}

#[test]
fn real_tmux_inventory_formats_parse_without_human_output_assumptions() {
    let tmux = std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .expect("Stop 3 integration tests require tmux on PATH");
    assert!(tmux.status.success(), "tmux -V failed");

    let socket_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-tmux");
    std::fs::create_dir_all(&socket_dir).unwrap();
    let socket = socket_dir.join(format!("topology-{}.sock", std::process::id()));
    let session = format!("lector_topology_{}", std::process::id());
    let linked_session = format!("lector_linked_{}", std::process::id());
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 12,
            cols: 90,
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
        "cat",
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
                Ok(count) => sender.send(buffer[..count].to_vec()).unwrap(),
                Err(error) if error.raw_os_error() == Some(5) => break,
                Err(error) => panic!("read real tmux topology PTY: {error}"),
            }
        }
    });

    let mut router = TmuxGatewayRouter::new();
    let mut initial_reply_seen = false;
    let mut setup_replies = 0;
    let mut inventory_sent = false;
    let mut inventory = Vec::new();
    let mut inventory_replies = 0;
    let mut kill_sent = false;
    while inventory_replies < INVENTORY_REPLY_COUNT || router.active_connection().is_some() {
        let chunk = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("timed out reading real tmux topology fixture");
        for event in router.push(&chunk).unwrap() {
            if let GatewayEvent::Control {
                event: lector::tmux_control::ControlEvent::Command { status, output, .. },
                ..
            } = event
            {
                if !initial_reply_seen {
                    initial_reply_seen = true;
                    let setup = format!(
                        "new-session -d -s {linked_session} ; link-window -s {session}:0 -t {linked_session}:1\n"
                    );
                    writer.write_all(setup.as_bytes()).unwrap();
                    writer.flush().unwrap();
                } else if setup_replies < 2 {
                    assert_eq!(status, lector::tmux_control::CommandStatus::Success);
                    setup_replies += 1;
                    if setup_replies == 2 {
                        writer.write_all(INVENTORY_COMMAND.as_bytes()).unwrap();
                        writer.flush().unwrap();
                        inventory_sent = true;
                    }
                } else if inventory_replies < INVENTORY_REPLY_COUNT {
                    assert_eq!(status, lector::tmux_control::CommandStatus::Success);
                    inventory.extend(output);
                    inventory_replies += 1;
                    if inventory_replies == INVENTORY_REPLY_COUNT {
                        writer.write_all(b"kill-server\n").unwrap();
                        writer.flush().unwrap();
                        kill_sent = true;
                    }
                }
            }
        }
        if inventory_replies == INVENTORY_REPLY_COUNT
            && kill_sent
            && router.active_connection().is_none()
        {
            break;
        }
    }

    let _ = child.wait().unwrap();
    read_thread.join().unwrap();
    let _ = std::fs::remove_file(&socket);
    assert!(inventory_sent && kill_sent);
    let mut topology = TmuxTopology::new(99);
    topology
        .replace_inventory(&inventory)
        .unwrap_or_else(|error| {
            panic!(
                "real tmux inventory failed: {error}; records={:?}",
                inventory
                    .iter()
                    .map(|line| String::from_utf8_lossy(line))
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(topology.sessions().len(), 2);
    assert!(
        topology
            .sessions()
            .values()
            .flat_map(|session| session.windows.values())
            .any(|window_id| topology.window(*window_id).unwrap().links.len() == 2),
        "real linked window did not retain both session links"
    );
    assert!(topology.attached_session().is_some());
    assert_eq!(topology.option("base-index"), Some("0"));
    assert_eq!(topology.option("pane-base-index"), Some("0"));
    assert!(
        topology
            .panes()
            .values()
            .all(|pane| pane.cursor_shape == "default"),
        "real tmux cursor metadata did not use its named machine format"
    );
}
