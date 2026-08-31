use lector::{
    app::{App, Clock},
    presentation::GridPoint,
    screen_reader::ScreenReader,
    speech,
    terminal::{MouseEncoding, MouseProtocol, TerminalGeometry},
    tmux_input::{
        MAX_SEND_KEYS_COMMAND_BYTES, continue_pane_command, encode_send_keys, pause_pane_command,
        refresh_client_command, refresh_client_report_commands, translate_mouse,
    },
    tmux_model::PaneId,
    tmux_panes::LayoutPane,
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
    time::{Duration, Instant, SystemTime},
};
use terminput::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

const SPLIT: &str = "abcd,80x24,0,0{40x24,0,0,20,39x24,41,0,21}";

fn decode_send_keys(commands: &[Vec<u8>], pane_id: PaneId) -> Vec<u8> {
    let prefix = format!("send-keys -H -t %{} ", pane_id.0);
    let mut decoded = Vec::new();
    for command in commands {
        assert!(command.len() <= MAX_SEND_KEYS_COMMAND_BYTES);
        assert!(command.ends_with(b"\n"));
        assert!(command.iter().all(u8::is_ascii));
        let text = std::str::from_utf8(command).unwrap();
        let payload = text
            .strip_prefix(&prefix)
            .and_then(|text| text.strip_suffix('\n'))
            .unwrap_or_else(|| panic!("unsafe or mistargeted send-keys command: {text:?}"));
        for byte in payload.split_ascii_whitespace() {
            assert_eq!(byte.len(), 2, "hex arguments must encode exactly one byte");
            decoded.push(u8::from_str_radix(byte, 16).unwrap());
        }
    }
    decoded
}

#[test]
fn hexadecimal_send_keys_is_binary_safe_bounded_ordered_and_injection_proof() {
    let mut corpus = (0_u8..=255).collect::<Vec<_>>();
    corpus.extend_from_slice(b"\nrun-shell 'touch /tmp/never' ; display-message #{pane_id}\r\n");
    corpus.extend(std::iter::repeat_n(0x80, MAX_SEND_KEYS_COMMAND_BYTES * 3));

    let commands = encode_send_keys(PaneId(42), &corpus).unwrap();

    assert!(
        commands.len() > 3,
        "large input was not split into bounded commands"
    );
    assert_eq!(decode_send_keys(&commands, PaneId(42)), corpus);
    assert!(
        commands
            .iter()
            .all(|command| !command.windows(9).any(|w| w == b"run-shell"))
    );
    assert!(encode_send_keys(PaneId(42), b"").unwrap().is_empty());
}

#[test]
fn only_complete_osc_10_and_11_replies_become_control_client_reports() {
    let replies = b"\x1b[?64;22c\x1b]10;rgb:ffff/ffff/ffff\x1b\\\
                    ignored\x1b]11;rgb:0000/0000/0000\x07\
                    \x1b]12;rgb:1111/2222/3333\x1b\\\
                    \x1b]10;unsafe'quote\x1b\\\
                    \x1b]11;incomplete";
    assert_eq!(
        refresh_client_report_commands(PaneId(7), replies),
        vec![
            b"refresh-client -r '%7:\x1b]10;rgb:ffff/ffff/ffff\x1b\\'\n".to_vec(),
            b"refresh-client -r '%7:\x1b]11;rgb:0000/0000/0000\x07'\n".to_vec(),
        ]
    );
}

#[test]
fn resize_and_mouse_encoding_preserve_tmux_and_pane_coordinate_authority() {
    assert_eq!(
        refresh_client_command(TerminalGeometry::from_cells(40, 120)),
        b"refresh-client -C 120x40\n"
    );
    assert_eq!(
        continue_pane_command(PaneId(21)),
        b"refresh-client -A '%21:continue'\n"
    );
    assert_eq!(
        pause_pane_command(PaneId(21)),
        b"refresh-client -A '%21:pause'\n"
    );
    let pane = LayoutPane {
        pane_id: PaneId(21),
        origin: GridPoint::new(5, 10),
        rows: 8,
        cols: 20,
    };
    let event = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 14,
        row: 7,
        modifiers: KeyModifiers::CTRL,
    };
    assert_eq!(
        translate_mouse(event, pane, MouseProtocol::PressRelease, MouseEncoding::Sgr).unwrap(),
        b"\x1b[<16;5;3M"
    );
    assert!(
        translate_mouse(
            MouseEvent { column: 9, ..event },
            pane,
            MouseProtocol::PressRelease,
            MouseEncoding::Sgr,
        )
        .is_none(),
        "a click on a border or different pane must not leak to the active pane"
    );
    assert!(
        translate_mouse(event, pane, MouseProtocol::None, MouseEncoding::Sgr).is_none(),
        "mouse input must be gated by the active pane's requested mode"
    );
    assert!(
        translate_mouse(
            MouseEvent {
                kind: MouseEventKind::Moved,
                ..event
            },
            pane,
            MouseProtocol::PressRelease,
            MouseEncoding::Sgr,
        )
        .is_none(),
        "motion must be gated by the active pane's requested protocol"
    );

    assert_eq!(
        translate_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 14,
                row: 7,
                modifiers: KeyModifiers::CTRL,
            },
            pane,
            MouseProtocol::Press,
            MouseEncoding::Default,
        )
        .unwrap(),
        b"\x1b[M0%#"
    );
    let utf8_pane = LayoutPane {
        pane_id: PaneId(21),
        origin: GridPoint::new(0, 0),
        rows: 200,
        cols: 200,
    };
    assert_eq!(
        translate_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Right),
                column: 95,
                row: 95,
                modifiers: KeyModifiers::empty(),
            },
            utf8_pane,
            MouseProtocol::PressRelease,
            MouseEncoding::Utf8,
        )
        .unwrap(),
        b"\x1b[M\"\xc2\x80\xc2\x80"
    );
    assert!(
        translate_mouse(
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                ..event
            },
            pane,
            MouseProtocol::ButtonMotion,
            MouseEncoding::Sgr,
        )
        .is_some()
    );
    assert!(
        translate_mouse(
            MouseEvent {
                kind: MouseEventKind::Moved,
                ..event
            },
            pane,
            MouseProtocol::AnyMotion,
            MouseEncoding::Sgr,
        )
        .is_some()
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

impl Clock for TestClock {
    fn now_ms(&self) -> u128 {
        self.0.get()
    }
}

fn command_reply(serial: usize, lines: &[&str]) -> Vec<u8> {
    format!(
        "%begin {serial} {serial} 0\n{}%end {serial} {serial} 0\n",
        lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>()
    )
    .into_bytes()
}

fn ready_split_app() -> (App, ScreenReader, Vec<u8>) {
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(24, 80)));
    let mut app = App::new_with_clock(stack, Box::new(TestClock::default())).unwrap();
    let mut sr = ScreenReader::new(speech::Speech::new(Box::<SilentDriver>::default()));
    let mut physical = Vec::new();
    let mut control = Vec::new();
    app.handle_pty(
        &mut sr,
        b"\x1bP1000p%begin 1 1 0\n%end 1 1 0\n",
        &mut physical,
    )
    .unwrap();
    app.handle_tick(&mut sr, &mut control, &mut physical)
        .unwrap();
    assert_eq!(
        control,
        [
            lector::app::TMUX_FLOW_CONTROL_COMMAND,
            lector::app::TMUX_FLOW_CONTROL_VERIFY_COMMAND,
            b"refresh-client -C 80x24\n",
            lector::tmux_model::INVENTORY_COMMAND.as_bytes(),
        ]
        .concat()
    );
    app.handle_pty(&mut sr, &command_reply(2, &[]), &mut physical)
        .unwrap();
    app.handle_pty(
        &mut sr,
        &command_reply(3, &["attached,control-mode,pause-after=1"]),
        &mut physical,
    )
    .unwrap();
    app.handle_pty(&mut sr, &command_reply(4, &[]), &mut physical)
        .unwrap();
    let window = format!("W\t$1\t@10\t1\t1\t{SPLIT}\t{SPLIT}\t*\tinput");
    let groups = [
        vec!["S\t$1\twork"],
        vec![window.as_str()],
        vec![
            "P\t@10\t%20\t1\t0\t0\t0\t40\t24\t0\t0\t0\t1\t0\t0\t0\t0\tleft",
            "P\t@10\t%21\t2\t1\t41\t0\t39\t24\t0\t0\t0\t1\t0\t0\t0\t0\tright",
        ],
        vec!["A\t$1"],
        vec!["O\tbase-index\t1"],
        vec!["O\tpane-base-index\t1"],
        vec!["C\tclient_name\ttest"],
        vec!["O\tprefix\tC-a"],
        vec!["O\tprefix2\tNone"],
        vec!["O\tkey-table\troot"],
        vec!["O\trepeat-time\t500"],
        vec!["B\tn\t0\tnext-window"],
    ];
    for (index, lines) in groups.iter().enumerate() {
        app.handle_pty(&mut sr, &command_reply(index + 5, lines), &mut physical)
            .unwrap();
    }
    control.clear();
    app.handle_tick(&mut sr, &mut control, &mut physical)
        .unwrap();
    assert!(String::from_utf8_lossy(&control).contains("capture-pane"));
    app.handle_pty(&mut sr, &command_reply(20, &["left"]), &mut physical)
        .unwrap();
    app.handle_pty(&mut sr, &command_reply(21, &["right"]), &mut physical)
        .unwrap();
    app.handle_pty(
        &mut sr,
        b"%output %21 \\033[?2004h\\033[?1004h\\033[?1000h\\033[?1006h\n",
        &mut physical,
    )
    .unwrap();
    (app, sr, physical)
}

fn tick_commands(app: &mut App, sr: &mut ScreenReader, physical: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut output = Vec::new();
    app.handle_tick(sr, &mut output, physical).unwrap();
    output
        .split_inclusive(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

#[test]
fn application_harness_routes_keyboard_paste_focus_mouse_queries_and_latest_size() {
    let (mut app, mut sr, mut physical) = ready_split_app();
    let protocols: &[(&[u8], &[u8])] = &[
        (b"x", b"x"),
        ("é".as_bytes(), "é".as_bytes()),
        (b"\xff", b"\xff"),
        (b"\x03", b"\x03"),
        (b"\x1bz", b"\x1bz"),
        (b"\x1b[120;5:1u", b"\x18"),
    ];
    let mut expected = Vec::new();
    for (input, legacy) in protocols {
        app.handle_stdin(&mut sr, input, &mut Vec::new(), &mut physical)
            .unwrap();
        expected.extend_from_slice(legacy);
    }
    let commands = tick_commands(&mut app, &mut sr, &mut physical);
    assert_eq!(commands.len(), 1, "adjacent key input was not batched");
    assert_eq!(decode_send_keys(&commands, PaneId(21)), expected);

    app.handle_stdin(
        &mut sr,
        b"\x1b[200~paste\nwith ; '#{}' and utf8 \xc3\xa9\x1b[201~",
        &mut Vec::new(),
        &mut physical,
    )
    .unwrap();
    assert_eq!(
        decode_send_keys(&tick_commands(&mut app, &mut sr, &mut physical), PaneId(21)),
        b"\x1b[200~paste\nwith ; '#{}' and utf8 \xc3\xa9\x1b[201~"
    );

    app.handle_stdin(&mut sr, b"\x1b[O", &mut Vec::new(), &mut physical)
        .unwrap();
    assert_eq!(
        decode_send_keys(&tick_commands(&mut app, &mut sr, &mut physical), PaneId(21)),
        b"\x1b[O"
    );

    app.handle_stdin(&mut sr, b"\x1b[<0;46;3M", &mut Vec::new(), &mut physical)
        .unwrap();
    assert_eq!(
        decode_send_keys(&tick_commands(&mut app, &mut sr, &mut physical), PaneId(21)),
        b"\x1b[<0;5;3M"
    );
    app.handle_stdin(&mut sr, b"\x1b[<0;41;3M", &mut Vec::new(), &mut physical)
        .unwrap();
    assert!(
        tick_commands(&mut app, &mut sr, &mut physical).is_empty(),
        "a mouse event on the split border leaked into the active pane"
    );

    app.handle_pty(&mut sr, b"%output %21 \\033[6n\n", &mut physical)
        .unwrap();
    assert!(
        tick_commands(&mut app, &mut sr, &mut physical).is_empty(),
        "Lector duplicated the tmux server's terminal reply into the active pane"
    );

    app.handle_pty(&mut sr, b"%output %20 \\033[6n\n", &mut physical)
        .unwrap();
    assert!(
        tick_commands(&mut app, &mut sr, &mut physical).is_empty(),
        "Lector duplicated the tmux server's terminal reply into a hidden pane"
    );

    for cols in 81..=180 {
        app.on_resize(30, cols, &mut physical).unwrap();
    }
    let before_tmux_layout = app.composed_scene().unwrap();
    assert_eq!(
        before_tmux_layout.geometry,
        TerminalGeometry::from_cells(30, 180)
    );
    assert!(
        before_tmux_layout
            .panes
            .iter()
            .skip(1)
            .all(|pane| pane.snapshot.geometry.cols <= 40),
        "outer resize directly resized a pane before tmux supplied new layout geometry"
    );
    assert_eq!(
        tick_commands(&mut app, &mut sr, &mut physical),
        [b"refresh-client -C 180x30\n".to_vec()],
        "a resize storm should coalesce to the latest client size"
    );
}

#[test]
fn large_bracketed_paste_is_batched_without_quoting_or_command_size_failures() {
    let (mut app, mut sr, mut physical) = ready_split_app();
    let text = "a;'#{pane_id}\nλ".repeat(4096);
    let mut event = b"\x1b[200~".to_vec();
    event.extend_from_slice(text.as_bytes());
    event.extend_from_slice(b"\x1b[201~");

    app.handle_stdin(&mut sr, &event, &mut Vec::new(), &mut physical)
        .unwrap();
    let commands = tick_commands(&mut app, &mut sr, &mut physical);
    assert!(
        commands.len() > 20,
        "large paste did not use bounded chunks"
    );
    assert_eq!(decode_send_keys(&commands, PaneId(21)), event);
}

#[test]
fn pathological_input_route_amplification_is_discarded_without_wedging_later_input() {
    let (mut app, mut sr, mut physical) = ready_split_app();
    let mut event = b"\x1b[200~".to_vec();
    event.extend(std::iter::repeat_n(b'x', 400 * 1024));
    event.extend_from_slice(b"\x1b[201~");

    app.handle_stdin(&mut sr, &event, &mut Vec::new(), &mut physical)
        .unwrap();
    assert!(
        tick_commands(&mut app, &mut sr, &mut physical).is_empty(),
        "an amplified command larger than the route budget was emitted"
    );
    assert_eq!(app.debug_tmux_pending_command_bytes(), 0);

    app.handle_stdin(&mut sr, b"z", &mut Vec::new(), &mut physical)
        .unwrap();
    assert_eq!(
        decode_send_keys(&tick_commands(&mut app, &mut sr, &mut physical), PaneId(21)),
        b"z",
        "discarding one pathological command poisoned later input"
    );
}

#[test]
fn input_command_replies_do_not_steal_later_inventory_correlation() {
    let (mut app, mut sr, mut physical) = ready_split_app();
    app.handle_stdin(&mut sr, &vec![b'x'; 5000], &mut Vec::new(), &mut physical)
        .unwrap();
    let commands = tick_commands(&mut app, &mut sr, &mut physical);
    assert!(commands.len() > 1);
    for serial in 100..100 + commands.len() {
        app.handle_pty(&mut sr, &command_reply(serial, &[]), &mut physical)
            .unwrap();
    }

    app.handle_pty(
        &mut sr,
        format!("%layout-change @10 {SPLIT} {SPLIT} *\n").as_bytes(),
        &mut physical,
    )
    .unwrap();
    assert_eq!(
        tick_commands(&mut app, &mut sr, &mut physical).concat(),
        lector::tmux_model::INVENTORY_COMMAND.as_bytes(),
        "ignored send-keys replies consumed or displaced the resync transaction"
    );
}

#[test]
fn missing_command_replies_hit_a_hard_backlog_limit_and_recover_when_replies_resume() {
    let (mut app, mut sr, mut physical) = ready_split_app();
    let mut saturated = false;
    for _ in 0..800 {
        app.handle_stdin(&mut sr, b"x", &mut Vec::new(), &mut physical)
            .unwrap();
        if tick_commands(&mut app, &mut sr, &mut physical).is_empty() {
            saturated = true;
            break;
        }
    }
    assert!(saturated, "a silent peer left the reply backlog unbounded");
    let capped = app.debug_tmux_expected_reply_count(1).unwrap();
    assert!(capped > 0);

    for serial in 10_000..10_016 {
        app.handle_pty(&mut sr, &command_reply(serial, &[]), &mut physical)
            .unwrap();
    }
    app.handle_stdin(&mut sr, b"recovered", &mut Vec::new(), &mut physical)
        .unwrap();
    assert!(
        !tick_commands(&mut app, &mut sr, &mut physical).is_empty(),
        "reply flow resumed but the command path stayed wedged"
    );
    assert!(app.debug_tmux_expected_reply_count(1).unwrap() < capped);
}

#[test]
fn resize_reaches_the_tmux_client_while_a_frozen_review_overlay_is_active() {
    let (mut app, mut sr, mut physical) = ready_split_app();
    app.handle_stdin(
        &mut sr,
        b"\x1b[114;3:1u\x1b[114;3:3u",
        &mut Vec::new(),
        &mut physical,
    )
    .unwrap();
    assert!(app.has_overlay());

    app.on_resize(50, 132, &mut physical).unwrap();

    assert_eq!(
        tick_commands(&mut app, &mut sr, &mut physical),
        [b"refresh-client -C 132x50\n".to_vec()]
    );
}

#[test]
fn input_batching_has_a_bounded_interactive_latency_budget() {
    let (mut app, mut sr, mut physical) = ready_split_app();
    let input = vec![b'x'; 10_000];

    let started = Instant::now();
    app.handle_stdin(&mut sr, &input, &mut Vec::new(), &mut physical)
        .unwrap();
    let queued = started.elapsed();
    let commands = tick_commands(&mut app, &mut sr, &mut physical);
    let encoded = started.elapsed();

    assert_eq!(decode_send_keys(&commands, PaneId(21)), input);
    assert!(
        queued < Duration::from_secs(2),
        "queuing 10,000 adjacent bytes took {queued:?}"
    );
    assert!(
        encoded < Duration::from_secs(5),
        "queuing and hex-encoding 10,000 adjacent bytes took {encoded:?}"
    );
    eprintln!("tmux input latency: queued 10,000 bytes in {queued:?}, encoded in {encoded:?}");
}

#[test]
fn input_stays_ordered_during_partial_sequences_pane_switch_and_tmux_flow_control() {
    let (mut app, mut sr, mut physical) = ready_split_app();
    app.handle_stdin(&mut sr, b"\x1b[120;", &mut Vec::new(), &mut physical)
        .unwrap();
    assert!(tick_commands(&mut app, &mut sr, &mut physical).is_empty());
    app.handle_pty(
        &mut sr,
        b"%window-pane-changed @10 %20\n%pause %20\n",
        &mut physical,
    )
    .unwrap();
    app.handle_stdin(&mut sr, b"5:1u", &mut Vec::new(), &mut physical)
        .unwrap();
    app.handle_stdin(&mut sr, b"after-pause", &mut Vec::new(), &mut physical)
        .unwrap();
    let commands = tick_commands(&mut app, &mut sr, &mut physical);
    assert_eq!(commands[0], b"refresh-client -A '%20:continue'\n");
    assert_eq!(
        decode_send_keys(&commands[1..], PaneId(20)),
        b"\x18after-pause"
    );
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
        write_pending_real_commands(app, sr, writer, physical);
        Ok::<_, mpsc::RecvTimeoutError>(ready(app))
    });
    if let Err(error) = result {
        panic!(
            "failed to reach {case}: {error:?}; contents={:?}; topology={:?}",
            app.debug_active_view_contents(),
            app.debug_tmux_topology(1)
        );
    }
}

fn write_pending_real_commands(
    app: &mut App,
    sr: &mut ScreenReader,
    writer: &mut dyn Write,
    physical: &mut Vec<u8>,
) -> Vec<u8> {
    let mut commands = Vec::new();
    app.handle_tick(sr, &mut commands, physical).unwrap();
    if !commands.is_empty() {
        writer.write_all(&commands).unwrap();
        writer.flush().unwrap();
    }
    commands
}

fn send_real_input(
    app: &mut App,
    sr: &mut ScreenReader,
    writer: &mut dyn Write,
    physical: &mut Vec<u8>,
    input: &[u8],
) {
    app.handle_stdin(sr, input, &mut Vec::new(), physical)
        .unwrap();
    let commands = write_pending_real_commands(app, sr, writer, physical);
    assert!(
        String::from_utf8_lossy(&commands).contains("send-keys -H"),
        "real input did not cross the binary-safe command path"
    );
}

#[test]
#[ignore = "run through scripts/test-real-tmux-docker"]
fn real_tmux_byte_echo_paste_mouse_resize_and_output_flood_harness() {
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
    let socket = socket_dir.join(format!("input-{}-{unique}.sock", std::process::id()));
    let session = format!("lector_input_{}_{unique}", std::process::id());
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let child_program = concat!(
        "/usr/bin/perl -e '$|=1; system q(stty raw -echo); print q(READY); ",
        "$m=0; while (sysread(STDIN,$c,1)) { $n=ord($c); ",
        "if ($n==126 && !$m) { $m=1; print qq(\\e[?2004h\\e[?1004h\\e[?1000h\\e[?1006hMODES); } ",
        "elsif ($n==33) { print q(F) x 131072; } ",
        "else { printf qq(%02x), $n; } }'",
    );
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
        child_program,
    ]);
    command.env("TERM", "xterm-256color");
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let (sender, receiver) = mpsc::channel();
    let read_thread = thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if sender.send(buffer[..count].to_vec()).is_err() {
                        break;
                    }
                }
                Err(error) if error.raw_os_error() == Some(5) => break,
                Err(error) => panic!("read real tmux input PTY: {error}"),
            }
        }
    });

    let stack = views::ViewStack::new(Box::new(views::PtyView::new(24, 80)));
    let mut app = App::new(stack).unwrap();
    let mut sr = ScreenReader::new(speech::Speech::new(Box::<SilentDriver>::default()));
    let mut physical = Vec::new();
    drive_real_tmux_until(
        "input bootstrap",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| app.debug_active_view_contents().contains("READY"),
    );

    send_real_input(&mut app, &mut sr, writer.as_mut(), &mut physical, b"~");
    drive_real_tmux_until(
        "enable input modes",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| app.debug_active_view_contents().contains("MODES"),
    );

    let protocols: &[(&[u8], &[u8])] = &[
        (b"\0", b"\0"),
        ("é".as_bytes(), "é".as_bytes()),
        (b"\xff", b"\xff"),
        (b"\x03", b"\x03"),
        (b"\x1bz", b"\x1bz"),
        (b"\x1b[120;5:1u", b"\x18"),
    ];
    let mut protocol_bytes = Vec::new();
    for (protocol, legacy) in protocols {
        app.handle_stdin(&mut sr, protocol, &mut Vec::new(), &mut physical)
            .unwrap();
        protocol_bytes.extend_from_slice(legacy);
    }
    let commands = write_pending_real_commands(&mut app, &mut sr, writer.as_mut(), &mut physical);
    assert_eq!(
        commands.iter().filter(|byte| **byte == b'\n').count(),
        1,
        "key input was not batched"
    );
    let expected_hex = protocol_bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    drive_real_tmux_until(
        "keyboard protocols",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| app.debug_active_view_contents().contains(&expected_hex),
    );

    let paste = "paste λ\nEND";
    let mut paste_event = b"\x1b[200~".to_vec();
    paste_event.extend_from_slice(paste.as_bytes());
    paste_event.extend_from_slice(b"\x1b[201~");
    send_real_input(
        &mut app,
        &mut sr,
        writer.as_mut(),
        &mut physical,
        &paste_event,
    );
    let paste_hex = paste_event
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    drive_real_tmux_until(
        "bracketed paste",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| {
            app.debug_active_view_contents()
                .split_whitespace()
                .collect::<String>()
                .contains(&paste_hex)
        },
    );

    for (case, input) in [
        ("focus", b"\x1b[O".as_slice()),
        ("mouse", b"\x1b[<0;3;2M".as_slice()),
    ] {
        send_real_input(&mut app, &mut sr, writer.as_mut(), &mut physical, input);
        let expected = input
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        drive_real_tmux_until(
            case,
            &mut app,
            &mut sr,
            &receiver,
            writer.as_mut(),
            &mut physical,
            |app| {
                app.debug_active_view_contents()
                    .split_whitespace()
                    .collect::<String>()
                    .contains(&expected)
            },
        );
    }

    for cols in 81..=100 {
        app.on_resize(30, cols, &mut physical).unwrap();
    }
    let resize = write_pending_real_commands(&mut app, &mut sr, writer.as_mut(), &mut physical);
    assert_eq!(resize, b"refresh-client -C 100x30\n");
    drive_real_tmux_until(
        "rapid resize",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| {
            app.composed_scene().is_ok_and(|scene| {
                scene.geometry == TerminalGeometry::from_cells(30, 100)
                    && scene
                        .panes
                        .iter()
                        .skip(1)
                        .any(|pane| pane.snapshot.geometry.cols == 100)
            })
        },
    );

    app.handle_stdin(&mut sr, b"!", &mut Vec::new(), &mut physical)
        .unwrap();
    app.handle_stdin(&mut sr, b"z", &mut Vec::new(), &mut physical)
        .unwrap();
    write_pending_real_commands(&mut app, &mut sr, writer.as_mut(), &mut physical);
    drive_real_tmux_until(
        "output flood and trailing input",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| app.debug_active_view_contents().contains("7a"),
    );

    writer.write_all(b"kill-server\n").unwrap();
    writer.flush().unwrap();
    drive_real_tmux_until(
        "input fixture exit",
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
