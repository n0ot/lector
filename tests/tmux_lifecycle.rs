use lector::{
    app::App,
    screen_reader::ScreenReader,
    speech,
    tmux_lifecycle::{ConnectionHierarchy, GatewayOrigin},
    views,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    cell::RefCell,
    io::{Read, Write},
    path::PathBuf,
    rc::Rc,
    sync::mpsc,
    thread,
    time::{Duration, SystemTime},
};

const SPLIT: &str = "b25f,80x24,0,0{40x24,0,0,20,39x24,41,0,23}";
const PANE_21: &str = "b25f,80x24,0,0,21";
const PANE_22: &str = "b25f,80x24,0,0,22";

fn multi_inventory() -> Vec<Vec<Vec<u8>>> {
    vec![
        vec![b"S\t$1\twork".to_vec(), b"S\t$2\tother".to_vec()],
        vec![
            format!("W\t$1\t@10\t1\t1\t{SPLIT}\t{SPLIT}\t*\teditor").into_bytes(),
            format!("W\t$1\t@11\t2\t0\t{PANE_21}\t{PANE_21}\t-\tlogs").into_bytes(),
            format!("W\t$2\t@12\t1\t1\t{PANE_22}\t{PANE_22}\t*\tremote").into_bytes(),
        ],
        vec![
            b"P\t@10\t%20\t1\t1\t0\t0\t40\t24\t0\t0\t0\t1\t0\t0\t0\t0\tleft".to_vec(),
            b"P\t@10\t%23\t2\t0\t41\t0\t39\t24\t0\t0\t0\t1\t0\t0\t0\t0\tright".to_vec(),
            b"P\t@11\t%21\t1\t1\t0\t0\t80\t24\t0\t0\t0\t1\t0\t0\t0\t0\tlog-pane".to_vec(),
            b"P\t@12\t%22\t1\t1\t0\t0\t80\t24\t0\t0\t0\t1\t0\t0\t0\t0\tremote-pane".to_vec(),
        ],
        vec![b"A\t$1".to_vec()],
        vec![b"O\tbase-index\t1".to_vec()],
        vec![b"O\tpane-base-index\t1".to_vec()],
        vec![b"C\tclient_name\t/dev/ttys-life".to_vec()],
        vec![b"O\tprefix\tC-a".to_vec()],
        vec![b"O\tprefix2\tNone".to_vec()],
        vec![b"O\tkey-table\troot".to_vec()],
        vec![b"O\trepeat-time\t500".to_vec()],
        vec![
            b"B\tc\t0\tnew-window".to_vec(),
            b"B\td\t0\tdetach-client".to_vec(),
            b"B\tx\t0\tconfirm-before -p \"kill pane? (y/n)\" kill-pane".to_vec(),
            b"B\t&\t0\tconfirm-before -p \"kill window? (y/n)\" kill-window".to_vec(),
        ],
    ]
}

fn single_inventory(window_id: u64, pane_id: u64, name: &str) -> Vec<Vec<Vec<u8>>> {
    let layout = format!("b25f,80x24,0,0,{pane_id}");
    vec![
        vec![b"S\t$1\tonly-session".to_vec()],
        vec![format!("W\t$1\t@{window_id}\t1\t1\t{layout}\t{layout}\t*\t{name}").into_bytes()],
        vec![
            format!(
                "P\t@{window_id}\t%{pane_id}\t1\t1\t0\t0\t80\t24\t0\t0\t0\t1\t0\t0\t0\t0\tonly-pane"
            )
            .into_bytes(),
        ],
        vec![b"A\t$1".to_vec()],
        vec![b"O\tbase-index\t1".to_vec()],
        vec![b"O\tpane-base-index\t1".to_vec()],
        vec![b"C\tclient_name\t/dev/ttys-only".to_vec()],
        vec![b"O\tprefix\tC-a".to_vec()],
        vec![b"O\tprefix2\tNone".to_vec()],
        vec![b"O\tkey-table\troot".to_vec()],
        vec![b"O\trepeat-time\t500".to_vec()],
        vec![
            b"B\tc\t0\tnew-window".to_vec(),
            b"B\td\t0\tdetach-client".to_vec(),
            b"B\tx\t0\tconfirm-before -p \"kill pane? (y/n)\" kill-pane".to_vec(),
            b"B\t&\t0\tconfirm-before -p \"kill window? (y/n)\" kill-window".to_vec(),
        ],
    ]
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

fn ready_app(groups: Vec<Vec<Vec<u8>>>) -> (App, ScreenReader, Recorder, Vec<u8>) {
    let recorder = Recorder::default();
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(24, 80)));
    let mut app = App::new(stack).unwrap();
    let mut sr = ScreenReader::new(speech::Speech::new(Box::new(recorder.clone())));
    let mut physical = Vec::new();
    app.handle_pty(
        &mut sr,
        b"gateway$ tmux -CC\r\n\x1bP1000p%begin 1 1 0\n%end 1 1 0\n",
        &mut physical,
    )
    .unwrap();
    let mut commands = Vec::new();
    app.handle_tick(&mut sr, &mut commands, &mut physical)
        .unwrap();
    assert_eq!(
        commands,
        [
            lector::app::TMUX_FLOW_CONTROL_COMMAND,
            lector::app::TMUX_FLOW_CONTROL_VERIFY_COMMAND,
            b"refresh-client -C 80x24\n",
            lector::tmux_model::INVENTORY_COMMAND.as_bytes(),
        ]
        .concat()
    );
    app.handle_pty(&mut sr, &reply(2, &[], true), &mut physical)
        .unwrap();
    app.handle_pty(
        &mut sr,
        &reply(3, &[b"attached,control-mode,pause-after=1".to_vec()], true),
        &mut physical,
    )
    .unwrap();
    app.handle_pty(&mut sr, &reply(4, &[], true), &mut physical)
        .unwrap();
    assert_eq!(groups.len(), lector::tmux_model::INVENTORY_REPLY_COUNT);
    for (index, group) in groups.iter().enumerate() {
        app.handle_pty(&mut sr, &reply(index + 5, group, true), &mut physical)
            .unwrap();
    }
    commands.clear();
    app.handle_tick(&mut sr, &mut commands, &mut physical)
        .unwrap();
    let captures = String::from_utf8(commands).unwrap();
    let pane_ids = groups[2]
        .iter()
        .map(|record| {
            let fields = record.split(|byte| *byte == b'\t').collect::<Vec<_>>();
            String::from_utf8(fields[2].to_vec()).unwrap()
        })
        .collect::<Vec<_>>();
    for pane_id in &pane_ids {
        assert!(captures.contains(&format!("-t {pane_id}\n")));
    }
    for (index, pane_id) in pane_ids.iter().enumerate() {
        app.handle_pty(
            &mut sr,
            &reply(30 + index, &[format!("ready-{pane_id}").into_bytes()], true),
            &mut physical,
        )
        .unwrap();
    }
    (app, sr, recorder, physical)
}

fn input(app: &mut App, sr: &mut ScreenReader, physical: &mut Vec<u8>, bytes: &[u8]) -> Vec<u8> {
    let mut pty = Vec::new();
    app.handle_stdin(sr, bytes, &mut pty, physical).unwrap();
    pty
}

fn tick(app: &mut App, sr: &mut ScreenReader, physical: &mut Vec<u8>) -> Vec<u8> {
    let mut commands = Vec::new();
    app.handle_tick(sr, &mut commands, physical).unwrap();
    commands
}

#[test]
fn prefix_detach_uses_the_invoking_control_channel() {
    let (mut app, mut sr, _recorder, mut physical) = ready_app(multi_inventory());
    input(&mut app, &mut sr, &mut physical, b"\x01d");
    assert_eq!(tick(&mut app, &mut sr, &mut physical), b"detach-client\n");
}

#[test]
fn kill_confirmations_capture_and_report_the_original_stable_target() {
    let (mut app, mut sr, _recorder, mut physical) = ready_app(multi_inventory());

    input(&mut app, &mut sr, &mut physical, b"\x01x");
    let pane_prompt = app.debug_active_view_contents();
    assert!(pane_prompt.contains("pane %20"), "{pane_prompt:?}");
    assert!(pane_prompt.contains("left"), "{pane_prompt:?}");
    assert!(pane_prompt.contains("window @10"), "{pane_prompt:?}");
    app.handle_pty(&mut sr, b"%window-pane-changed @10 %23\n", &mut physical)
        .unwrap();
    input(&mut app, &mut sr, &mut physical, b"\r");
    assert_eq!(
        tick(&mut app, &mut sr, &mut physical),
        b"kill-pane -t %20\n"
    );

    let (mut app, mut sr, _recorder, mut physical) = ready_app(multi_inventory());
    input(&mut app, &mut sr, &mut physical, b"\x01&");
    let window_prompt = app.debug_active_view_contents();
    assert!(window_prompt.contains("window @10"), "{window_prompt:?}");
    assert!(window_prompt.contains("editor"), "{window_prompt:?}");
    assert!(
        window_prompt.contains("session $1 work"),
        "{window_prompt:?}"
    );
    app.handle_pty(&mut sr, b"%session-window-changed $1 @11\n", &mut physical)
        .unwrap();
    input(&mut app, &mut sr, &mut physical, b"\r");
    assert_eq!(
        tick(&mut app, &mut sr, &mut physical),
        b"kill-window -t @10\n"
    );
}

#[test]
fn external_destruction_rejects_a_stale_confirmation_and_server_death_clears_it() {
    let (mut app, mut sr, recorder, mut physical) = ready_app(multi_inventory());
    input(&mut app, &mut sr, &mut physical, b"\x01x");
    app.handle_pty(&mut sr, b"%pane-exited %20\n", &mut physical)
        .unwrap();
    input(&mut app, &mut sr, &mut physical, b"\r");
    assert!(tick(&mut app, &mut sr, &mut physical).is_empty());
    assert!(
        recorder
            .0
            .borrow()
            .iter()
            .any(|message| message == "tmux target disappeared")
    );

    let (mut app, mut sr, _recorder, mut physical) = ready_app(multi_inventory());
    input(&mut app, &mut sr, &mut physical, b"\x01&");
    app.handle_pty(
        &mut sr,
        b"%exit server terminated during confirmation\n\x1b\\gateway returned\r\n",
        &mut physical,
    )
    .unwrap();
    assert_eq!(app.tmux_connection_count(), 0);
    assert!(!app.has_overlay());
    assert!(
        app.debug_active_view_contents()
            .contains("gateway returned")
    );
    let root_input = input(&mut app, &mut sr, &mut physical, b"\r");
    assert_eq!(root_input, b"\r");
    assert!(tick(&mut app, &mut sr, &mut physical).is_empty());
}

#[test]
fn simultaneous_target_and_server_destruction_cleans_up_exactly_once() {
    let (mut app, mut sr, _recorder, mut physical) =
        ready_app(single_inventory(10, 20, "only-window"));
    input(&mut app, &mut sr, &mut physical, b"\x01&");
    app.handle_pty(
        &mut sr,
        b"%window-close @10\n%exit server stopped\n\x1b\\gateway resumed\r\n",
        &mut physical,
    )
    .unwrap();

    assert_eq!(app.tmux_connection_count(), 0);
    assert!(!app.has_overlay());
    assert!(app.debug_active_view_contents().contains("gateway resumed"));
    assert_eq!(input(&mut app, &mut sr, &mut physical, b"\r"), b"\r");
    assert!(tick(&mut app, &mut sr, &mut physical).is_empty());
}

#[test]
fn only_window_destruction_renders_an_understandable_waiting_scene_and_can_recover() {
    let (mut app, mut sr, _recorder, mut physical) =
        ready_app(single_inventory(10, 20, "only-window"));
    app.handle_pty(&mut sr, b"%window-close @10\n", &mut physical)
        .unwrap();
    assert!(
        app.debug_active_view_contents().contains("ready-%20"),
        "ambiguous close mutated the last valid scene before inventory"
    );
    assert_eq!(
        tick(&mut app, &mut sr, &mut physical),
        lector::tmux_model::INVENTORY_COMMAND.as_bytes()
    );
    let mut destroyed = single_inventory(10, 20, "only-window");
    destroyed[1].clear();
    destroyed[2].clear();
    for (index, group) in destroyed.iter().enumerate() {
        app.handle_pty(&mut sr, &reply(80 + index, group, true), &mut physical)
            .unwrap();
    }
    let waiting = app.debug_active_view_contents();
    assert!(waiting.contains("session $1 only-session"), "{waiting:?}");
    assert!(waiting.contains("no active window"), "{waiting:?}");
    let scene = app.composed_scene().unwrap();
    assert!(scene.images.is_empty());

    app.handle_pty(&mut sr, b"%sessions-changed\n", &mut physical)
        .unwrap();
    assert_eq!(
        tick(&mut app, &mut sr, &mut physical),
        lector::tmux_model::INVENTORY_COMMAND.as_bytes()
    );
    let replacement = single_inventory(30, 40, "replacement");
    for (index, group) in replacement.iter().enumerate() {
        app.handle_pty(&mut sr, &reply(100 + index, group, true), &mut physical)
            .unwrap();
    }
    assert_eq!(
        tick(&mut app, &mut sr, &mut physical),
        b"capture-pane -p -e -F -J -S - -t %40\n"
    );
    app.handle_pty(
        &mut sr,
        &reply(120, &[b"replacement-ready".to_vec()], true),
        &mut physical,
    )
    .unwrap();
    assert!(
        app.debug_active_view_contents()
            .contains("replacement-ready")
    );
}

#[test]
fn window_close_during_inventory_discards_the_mixed_snapshot_and_refreshes_again() {
    let (mut app, mut sr, _recorder, mut physical) =
        ready_app(single_inventory(10, 20, "only-window"));
    app.handle_pty(&mut sr, b"%sessions-changed\n", &mut physical)
        .unwrap();
    assert_eq!(
        tick(&mut app, &mut sr, &mut physical),
        lector::tmux_model::INVENTORY_COMMAND.as_bytes()
    );

    // This generation began before the close and still contains the old
    // window. Interleave the notification with its replies exactly as tmux may
    // do when a process exits during the multi-command inventory transaction.
    let stale = single_inventory(10, 20, "only-window");
    for (index, group) in stale.iter().enumerate() {
        if index == 4 {
            app.handle_pty(&mut sr, b"%window-close @10\n", &mut physical)
                .unwrap();
        }
        app.handle_pty(&mut sr, &reply(200 + index, group, true), &mut physical)
            .unwrap();
    }

    assert!(
        app.debug_active_view_contents().contains("ready-%20"),
        "an invalidated inventory replaced the last valid scene"
    );
    assert_eq!(
        tick(&mut app, &mut sr, &mut physical),
        lector::tmux_model::INVENTORY_COMMAND.as_bytes(),
        "the invalidated generation did not force a fresh inventory"
    );

    let mut authoritative = single_inventory(10, 20, "only-window");
    authoritative[1].clear();
    authoritative[2].clear();
    for (index, group) in authoritative.iter().enumerate() {
        app.handle_pty(&mut sr, &reply(300 + index, group, true), &mut physical)
            .unwrap();
    }
    let waiting = app.debug_active_view_contents();
    assert!(waiting.contains("session $1 only-session"), "{waiting:?}");
    assert!(waiting.contains("no active window"), "{waiting:?}");
}

#[test]
fn applied_pane_exit_during_inventory_cannot_be_overwritten_by_the_old_snapshot() {
    let (mut app, mut sr, _recorder, mut physical) = ready_app(multi_inventory());
    app.handle_pty(&mut sr, b"%sessions-changed\n", &mut physical)
        .unwrap();
    assert_eq!(
        tick(&mut app, &mut sr, &mut physical),
        lector::tmux_model::INVENTORY_COMMAND.as_bytes()
    );

    let stale = multi_inventory();
    for (index, group) in stale.iter().enumerate() {
        if index == 4 {
            app.handle_pty(&mut sr, b"%pane-exited %20\n", &mut physical)
                .unwrap();
            assert!(app.debug_tmux_pane_contents(1, 20).is_none());
        }
        app.handle_pty(&mut sr, &reply(400 + index, group, true), &mut physical)
            .unwrap();
    }

    assert!(
        app.debug_tmux_pane_contents(1, 20).is_none(),
        "the stale inventory resurrected an exited pane"
    );
    assert_eq!(
        tick(&mut app, &mut sr, &mut physical),
        lector::tmux_model::INVENTORY_COMMAND.as_bytes(),
        "the applied notification did not invalidate the older inventory"
    );
}

#[test]
fn failed_create_is_reviewable_and_portal_never_mirrors_live_child_output() {
    let (mut app, mut sr, _recorder, mut physical) = ready_app(multi_inventory());
    input(&mut app, &mut sr, &mut physical, b"\x01c");
    assert_eq!(tick(&mut app, &mut sr, &mut physical), b"new-window\n");
    app.handle_pty(
        &mut sr,
        &reply(110, &[b"create rejected".to_vec()], false),
        &mut physical,
    )
    .unwrap();
    assert!(app.debug_active_view_contents().contains("create rejected"));
    input(&mut app, &mut sr, &mut physical, b"\r");

    assert!(app.show_tmux_gateway(1, &mut sr, &mut physical).unwrap());
    let portal = app.debug_active_view_contents();
    assert!(portal.contains("tmux control mode is running"));
    app.handle_pty(
        &mut sr,
        b"%output %20 child-updated-behind-portal\n",
        &mut physical,
    )
    .unwrap();
    assert_eq!(app.debug_active_view_contents(), portal);
    assert!(input(&mut app, &mut sr, &mut physical, b"r").is_empty());
    assert!(tick(&mut app, &mut sr, &mut physical).is_empty());
    input(&mut app, &mut sr, &mut physical, b"\r");
    assert!(
        app.debug_active_view_contents()
            .contains("child-updated-behind-portal")
    );
}

#[test]
fn gateway_hierarchy_recursively_resolves_destroyed_parent_panes_and_windows_once() {
    let mut hierarchy = ConnectionHierarchy::new();
    hierarchy.insert(1, GatewayOrigin::Direct).unwrap();
    hierarchy.insert(2, GatewayOrigin::Direct).unwrap();
    hierarchy
        .insert(
            3,
            GatewayOrigin::Pane {
                parent_connection_id: 1,
                session_id: 1,
                window_id: 10,
                pane_id: 20,
            },
        )
        .unwrap();
    hierarchy
        .insert(
            4,
            GatewayOrigin::Pane {
                parent_connection_id: 3,
                session_id: 1,
                window_id: 30,
                pane_id: 40,
            },
        )
        .unwrap();

    assert_eq!(hierarchy.remove_gateway_pane(1, 20), vec![4, 3]);
    assert!(hierarchy.contains(1));
    assert!(hierarchy.contains(2));
    assert!(!hierarchy.contains(3));
    assert!(!hierarchy.contains(4));
    assert!(hierarchy.remove_gateway_pane(1, 20).is_empty());

    hierarchy
        .insert(
            5,
            GatewayOrigin::Pane {
                parent_connection_id: 1,
                session_id: 1,
                window_id: 11,
                pane_id: 21,
            },
        )
        .unwrap();
    assert_eq!(hierarchy.remove_gateway_window(1, 11), vec![5]);
    assert_eq!(hierarchy.len(), 2);
}

fn write_real_commands(
    app: &mut App,
    sr: &mut ScreenReader,
    writer: &mut dyn Write,
    physical: &mut Vec<u8>,
) -> Vec<u8> {
    let commands = tick(app, sr, physical);
    if !commands.is_empty() {
        writer.write_all(&commands).unwrap_or_else(|error| {
            panic!(
                "write live tmux commands failed: {error}; connections={}; commands={commands:?}",
                app.tmux_connection_count()
            )
        });
        writer.flush().unwrap_or_else(|error| {
            panic!(
                "flush live tmux commands failed: {error}; connections={}; commands={commands:?}",
                app.tmux_connection_count()
            )
        });
    }
    commands
}

fn drive_real_tmux(
    case: &str,
    app: &mut App,
    sr: &mut ScreenReader,
    receiver: &mpsc::Receiver<Vec<u8>>,
    writer: &mut dyn Write,
    physical: &mut Vec<u8>,
    mut done: impl FnMut(&mut App) -> bool,
) {
    for _ in 0..1000 {
        if done(app) {
            return;
        }
        let chunk = receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|error| {
                panic!(
                    "timed out in {case}: {error}; contents={:?}; topology={:?}",
                    app.debug_active_view_contents(),
                    app.debug_tmux_topology(1)
                )
            });
        app.handle_pty(sr, &chunk, physical).unwrap();
        write_real_commands(app, sr, writer, physical);
    }
    panic!("real tmux lifecycle fixture exceeded its bounded event count in {case}");
}

struct DisposableTmuxServer {
    socket: PathBuf,
}

impl Drop for DisposableTmuxServer {
    fn drop(&mut self) {
        let _ = std::process::Command::new("tmux")
            .args(["-S", self.socket.to_str().unwrap(), "kill-server"])
            .output();
        let _ = std::fs::remove_file(&self.socket);
    }
}

#[test]
fn real_tmux_create_split_kill_window_and_detach_use_only_a_disposable_server() {
    let _serial = super::serialize_real_tmux_test();
    let tmux = std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .expect("tmux integration tests require tmux on PATH");
    assert!(tmux.status.success(), "tmux -V failed");
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let socket_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-tmux");
    std::fs::create_dir_all(&socket_dir).unwrap();
    let socket = socket_dir.join(format!("life-{}-{unique}.sock", std::process::id()));
    let server_guard = DisposableTmuxServer {
        socket: socket.clone(),
    };
    let session = format!("lector_life_{}_{unique}", std::process::id());
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
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/tmux-lifecycle.conf"
        ),
        "-CC",
        "new-session",
        "-s",
        &session,
        "/bin/sh -c 'printf PRIMARY; exec /usr/bin/tail -f /dev/null'",
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
                Err(error) => panic!("read real tmux lifecycle PTY: {error}"),
            }
        }
    });

    let stack = views::ViewStack::new(Box::new(views::PtyView::new(24, 80)));
    let mut app = App::new(stack).unwrap();
    let mut sr = ScreenReader::new(speech::Speech::new(Box::new(Recorder::default())));
    let mut physical = Vec::new();
    drive_real_tmux(
        "lifecycle bootstrap",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| app.debug_active_view_contents().contains("PRIMARY"),
    );

    input(&mut app, &mut sr, &mut physical, b"\x01c");
    assert!(
        String::from_utf8_lossy(&write_real_commands(
            &mut app,
            &mut sr,
            writer.as_mut(),
            &mut physical
        ))
        .starts_with("new-window -n created")
    );
    drive_real_tmux(
        "create window",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| app.debug_active_view_contents().contains("CREATED"),
    );

    input(&mut app, &mut sr, &mut physical, b"\x01%");
    assert!(
        String::from_utf8_lossy(&write_real_commands(
            &mut app,
            &mut sr,
            writer.as_mut(),
            &mut physical
        ))
        .starts_with("split-window -h")
    );
    drive_real_tmux(
        "split window",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| app.debug_active_view_contents().contains("SPLIT"),
    );

    input(&mut app, &mut sr, &mut physical, b"\x01x");
    assert!(app.debug_active_view_contents().contains("pane %"));
    input(&mut app, &mut sr, &mut physical, b"\r");
    assert!(
        String::from_utf8_lossy(&write_real_commands(
            &mut app,
            &mut sr,
            writer.as_mut(),
            &mut physical
        ))
        .starts_with("kill-pane -t %")
    );
    drive_real_tmux(
        "kill pane",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| {
            app.debug_tmux_topology(1)
                .is_some_and(|dump| dump.matches("pane %").count() == 2)
        },
    );

    input(&mut app, &mut sr, &mut physical, b"\x01&");
    assert!(app.debug_active_view_contents().contains("window @"));
    input(&mut app, &mut sr, &mut physical, b"\r");
    assert!(
        String::from_utf8_lossy(&write_real_commands(
            &mut app,
            &mut sr,
            writer.as_mut(),
            &mut physical
        ))
        .starts_with("kill-window -t @")
    );
    drive_real_tmux(
        "kill window",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| app.debug_active_view_contents().contains("PRIMARY"),
    );

    input(&mut app, &mut sr, &mut physical, b"\x01d");
    assert_eq!(
        write_real_commands(&mut app, &mut sr, writer.as_mut(), &mut physical),
        b"detach-client\n"
    );
    drive_real_tmux(
        "detach",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| app.tmux_connection_count() == 0,
    );

    assert!(
        std::process::Command::new("tmux")
            .args(["-S", socket.to_str().unwrap(), "kill-server"])
            .status()
            .unwrap()
            .success(),
        "failed to stop disposable tmux server"
    );
    let _ = child.wait().unwrap();
    read_thread.join().unwrap();
    drop(server_guard);
}
