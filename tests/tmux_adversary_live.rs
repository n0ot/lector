#![cfg(target_os = "macos")]

use lector_ghostty::{Terminal, TerminalColorScheme, TerminalProfile};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Mutex, mpsc},
    thread,
    time::{Duration, Instant, SystemTime},
};

const READY: &[u8] = b"LECTOR-ADVERSARY-READY";
const NESTED_READY: &[u8] = b"LECTOR-NESTED-READY";
const NESTED_ACK: &[u8] = b"LECTOR-NESTED-INPUT-ACK";
const BAD_RECOVERED: &[u8] = b"LECTOR-ADVERSARY-BAD-RECOVERED";
static LIVE_ADVERSARY_LOCK: Mutex<()> = Mutex::new(());

struct LiveAdversary {
    receiver: mpsc::Receiver<Vec<u8>>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    reader: Option<thread::JoinHandle<()>>,
    _master: Box<dyn MasterPty + Send>,
    terminal: Terminal,
    output: Vec<u8>,
    log: PathBuf,
    _speech_log: PathBuf,
    event_log: PathBuf,
}

impl LiveAdversary {
    fn spawn(scenario: &str) -> Self {
        Self::spawn_with_stalled_speech(scenario, false)
    }

    fn spawn_with_stalled_speech(scenario: &str, stall_speech: bool) -> Self {
        let artifact_dir =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-artifacts/tmux-adversary");
        fs::create_dir_all(&artifact_dir).expect("create adversary artifact directory");
        let unique = format!(
            "{}-{}-{scenario}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        );
        let log = artifact_dir.join(format!("{unique}.jsonl"));
        let speech_log = artifact_dir.join(format!("{unique}-speech.jsonl"));
        let event_log = artifact_dir.join(format!("{unique}-events.log"));
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 1280,
                pixel_height: 816,
            })
            .expect("open physical adversary PTY");
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_lector"));
        command.args([
            "--shell",
            env!("CARGO_BIN_EXE_tmux-control-adversary"),
            "--config",
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/pty/proc-speech.lua")
                .to_str()
                .expect("UTF-8 speech config path"),
            "--log-file",
            log.to_str().expect("UTF-8 diagnostic path"),
        ]);
        command.env("TERM", "xterm-ghostty");
        command.env("COLORTERM", "truecolor");
        command.env("LECTOR_TMUX_ADVERSARY", scenario);
        command.env("LECTOR_TMUX_ADVERSARY_EVENTS", &event_log);
        command.env("LECTOR_PROC_STUB_LOG", &speech_log);
        command.env(
            "LECTOR_TEST_SPEECH_SERVER",
            env!("CARGO_BIN_EXE_proc_stub_server"),
        );
        if stall_speech {
            command.env("LECTOR_PROC_STUB_STALL_SPEECH", "1");
        }
        command.env("LECTOR_OUTER_COLORS", "256");
        command.env("LECTOR_OUTER_TRUE_COLOR", "true");
        command.env("LECTOR_OUTER_HYPERLINKS", "true");
        command.env("LECTOR_OUTER_SYNC", "true");
        command.env("LECTOR_OUTER_KITTY_KEYBOARD", "true");
        command.env("LECTOR_OUTER_KITTY_GRAPHICS", "true");
        command.env("LECTOR_OUTER_FOCUS", "true");
        let child = pair
            .slave
            .spawn_command(command)
            .expect("spawn Lector with adversarial peer");
        drop(pair.slave);
        let mut input = pair.master.try_clone_reader().expect("clone PTY reader");
        let writer = pair.master.take_writer().expect("take PTY writer");
        let (sender, receiver) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut bytes = [0_u8; 8192];
            loop {
                match input.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(count) if sender.send(bytes[..count].to_vec()).is_err() => break,
                    Ok(_) => {}
                    Err(error) if error.raw_os_error() == Some(5) => break,
                    Err(error) => panic!("read live adversary PTY: {error}"),
                }
            }
        });
        let profile = TerminalProfile {
            rows: 24,
            columns: 80,
            cell_width: 16,
            cell_height: 34,
            color_scheme: TerminalColorScheme::Dark,
            enquiry: b"Ghostty".to_vec(),
            version: "ghostty adversary host".to_owned(),
            da_conformance: 62,
            da_features: vec![22, 52],
            da_device_type: 1,
            da_firmware_version: 10,
            ..TerminalProfile::default()
        };
        let terminal = Terminal::new_with_profile(24, 80, 10_000, profile)
            .expect("create adversary host terminal");
        Self {
            receiver,
            writer,
            child,
            reader: Some(reader),
            _master: pair.master,
            terminal,
            output: Vec::new(),
            log,
            _speech_log: speech_log,
            event_log,
        }
    }

    fn accept(&mut self, bytes: &[u8]) {
        self.output.extend_from_slice(bytes);
        let update = self.terminal.advance(bytes).expect("parse Lector output");
        if !update.pty_replies.is_empty() {
            self.writer
                .write_all(&update.pty_replies)
                .expect("return terminal replies");
            self.writer.flush().expect("flush terminal replies");
        }
    }

    fn wait_for_after(&mut self, start: usize, needle: &[u8], timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.output[start..]
                .windows(needle.len())
                .any(|window| window == needle)
            {
                return true;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            match self.receiver.recv_timeout(remaining) {
                Ok(bytes) => self.accept(&bytes),
                Err(mpsc::RecvTimeoutError::Timeout) => return false,
                Err(mpsc::RecvTimeoutError::Disconnected) => return false,
            }
        }
    }

    fn wait_for(&mut self, needle: &[u8], timeout: Duration) -> bool {
        self.wait_for_after(0, needle, timeout)
    }

    fn wait_for_screen(&mut self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let screen = self
                .terminal
                .snapshot()
                .rows
                .iter()
                .map(|row| row.text())
                .collect::<Vec<_>>()
                .join("\n");
            if screen.contains(needle) {
                return true;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            match self.receiver.recv_timeout(remaining) {
                Ok(bytes) => self.accept(&bytes),
                Err(mpsc::RecvTimeoutError::Timeout) => return false,
                Err(mpsc::RecvTimeoutError::Disconnected) => return false,
            }
        }
    }

    fn screen_text(&self) -> String {
        self.terminal
            .snapshot()
            .rows
            .iter()
            .map(|row| row.text())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn pump(&mut self) {
        let deadline = Instant::now() + Duration::from_millis(10);
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match self
                .receiver
                .recv_timeout(remaining.min(Duration::from_millis(2)))
            {
                Ok(bytes) => self.accept(&bytes),
                Err(_) => break,
            }
        }
    }

    fn wait_for_event(&mut self, expected: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if fs::read_to_string(&self.event_log)
                .unwrap_or_default()
                .lines()
                .any(|event| event == expected)
            {
                return true;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            match self
                .receiver
                .recv_timeout(remaining.min(Duration::from_millis(5)))
            {
                Ok(bytes) => self.accept(&bytes),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return false,
            }
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.pump();
        self.writer.write_all(bytes).expect("send physical input");
        self.writer.flush().expect("flush physical input");
    }

    fn force_abandon(&mut self) {
        self.send(b"\x1bC");
        assert!(
            self.wait_for_screen("Up/Down select", Duration::from_secs(1)),
            "connection manager did not render: {:?}",
            self.screen_text()
        );
        self.send(b"D");
        assert!(
            self.wait_for_screen("Control-backslash", Duration::from_secs(1)),
            "force-abandon confirmation did not render: {:?}",
            self.screen_text()
        );
        self.send(b"\r");
    }

    fn wait_for_log(&mut self, expected: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if fs::read_to_string(&self.log)
                .unwrap_or_default()
                .contains(expected)
            {
                return true;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            match self
                .receiver
                .recv_timeout(remaining.min(Duration::from_millis(10)))
            {
                Ok(bytes) => self.accept(&bytes),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return false,
            }
        }
    }

    fn finish(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.child.try_wait().expect("poll Lector").is_some() {
                self.pump();
                return true;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            match self
                .receiver
                .recv_timeout(remaining.min(Duration::from_millis(10)))
            {
                Ok(bytes) => self.accept(&bytes),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {}
            }
        }
    }

    fn assert_diagnostics(&self, scenario: &str) {
        let log = fs::read_to_string(&self.log).expect("read Lector diagnostic log");
        assert!(log.contains("\"kind\":\"log-start\""), "{scenario}: {log}");
        assert!(log.contains("connection-started"), "{scenario}: {log}");
        assert!(!log.contains("\"kind\":\"panic\""), "{scenario}: {log}");
    }
}

impl Drop for LiveAdversary {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        // Keep all three files together. The event trace is often the most
        // useful evidence when Lector remains alive but a control peer stalls.
    }
}

#[test]
fn malformed_control_output_recovers_without_a_lector_crash() {
    let _serial = LIVE_ADVERSARY_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut live = LiveAdversary::spawn("malformed");
    assert!(
        live.wait_for(BAD_RECOVERED, Duration::from_secs(4)),
        "malformed recovery marker was not rendered; output={:?}",
        String::from_utf8_lossy(&live.output)
    );
    assert!(live.finish(Duration::from_secs(4)), "Lector did not exit");
    live.assert_diagnostics("malformed");
}

#[test]
fn silent_and_unread_peers_can_expose_raw_transport_without_killing_lector() {
    let _serial = LIVE_ADVERSARY_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for scenario in ["silent", "no-read"] {
        let mut live = LiveAdversary::spawn(scenario);
        assert!(
            live.wait_for(READY, Duration::from_secs(4)),
            "{scenario}: bootstrap never completed; output={:?}",
            String::from_utf8_lossy(&live.output)
        );
        if scenario == "no-read" {
            // Fill the kernel's small PTY input queue. Lector must retain the
            // remainder without blocking its physical input/event loop.
            let mut paste = b"\x1b[200~".to_vec();
            paste.extend(std::iter::repeat_n(b'z', 16 * 1024));
            paste.extend_from_slice(b"\x1b[201~");
            live.send(&paste);
        } else {
            live.send(b"ordinary-input-with-no-reply");
        }
        assert!(
            live.child.try_wait().expect("poll Lector").is_none(),
            "{scenario}: Lector exited merely because its peer applied backpressure"
        );
        live.force_abandon();
        assert!(live.wait_for_log(
            "exposing raw channel for connection 1",
            Duration::from_secs(2)
        ));
        assert!(
            live.child.try_wait().expect("poll Lector").is_none(),
            "{scenario}: exposing the raw transport killed Lector"
        );
        if scenario == "silent" {
            // Control-backslash was delivered as a raw byte because this peer
            // deliberately disabled ISIG. Terminate that partial line, then
            // use the exposed raw control channel exactly as a user would.
            live.send(b"\ndetach-client\n");
            assert!(
                live.finish(Duration::from_secs(4)),
                "{scenario}: raw detach command did not reclaim the peer"
            );
        } else {
            // This synthetic peer does not read at all, so no in-band command
            // can make it exit. The important property is that Lector remains
            // responsive and forwards recovery input instead of killing its
            // whole PTY child.
            live.send(b"\r~.");
            assert!(
                live.child.try_wait().expect("poll Lector").is_none(),
                "{scenario}: raw recovery input killed Lector"
            );
        }
        live.assert_diagnostics(scenario);
        let log = fs::read_to_string(&live.log).unwrap();
        assert!(log.contains("raw-transport fallback"), "{scenario}: {log}");
        assert!(!log.contains("force-close-child"), "{scenario}: {log}");
    }
}

#[test]
fn stalled_proc_speech_backend_cannot_block_tmux_input_or_shutdown() {
    let _serial = LIVE_ADVERSARY_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut live = LiveAdversary::spawn_with_stalled_speech("normal", true);
    assert!(
        live.wait_for(READY, Duration::from_secs(4)),
        "control peer did not bootstrap while speech was stalled"
    );
    live.send(b"p");
    assert!(
        live.wait_for_event("input-p", Duration::from_secs(2)),
        "stalled speech backend blocked ordinary tmux input"
    );
    live.send(b"\x02d");
    assert!(
        live.wait_for_event("detach", Duration::from_secs(2)),
        "stalled speech backend blocked graceful tmux detach"
    );
    assert!(
        live.finish(Duration::from_secs(5)),
        "stalled speech backend blocked Lector shutdown"
    );
    live.assert_diagnostics("stalled-speech");
}

#[test]
fn active_and_hidden_floods_preserve_foreground_input_and_window_switching() {
    let _serial = LIVE_ADVERSARY_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for scenario in ["flood", "hidden-flood"] {
        let mut live = LiveAdversary::spawn(scenario);
        let ready = if scenario == "flood" {
            live.wait_for_screen("XXXXXXXXXXXXXXXX", Duration::from_secs(5))
        } else {
            live.wait_for(READY, Duration::from_secs(5))
        };
        assert!(
            ready,
            "{scenario} not ready; screen={:?}; output_tail={:?}",
            live.screen_text(),
            String::from_utf8_lossy(&live.output[live.output.len().saturating_sub(4096)..])
        );
        if scenario == "hidden-flood" {
            live.send(b"\x02n");
            assert!(
                live.wait_for_event("window-11", Duration::from_secs(2)),
                "hidden flood prevented the window-switch command from reaching the peer"
            );
        }
        let sent = Instant::now();
        live.send(b"p");
        assert!(
            live.wait_for_event("input-p", Duration::from_secs(2)),
            "{scenario}: foreground input starved for {:?}",
            sent.elapsed()
        );
        if scenario == "hidden-flood" {
            live.send(b"\x02n");
            thread::sleep(Duration::from_millis(50));
        }
        live.send(b"\x02d");
        assert!(
            live.wait_for_event("detach", Duration::from_secs(2)),
            "{scenario}: detach command did not reach the peer; peer events={:?}",
            fs::read_to_string(&live.event_log).unwrap_or_default()
        );
        assert!(
            live.finish(Duration::from_secs(5)),
            "{scenario}: detach hung; peer events={:?}",
            fs::read_to_string(&live.event_log).unwrap_or_default()
        );
        live.assert_diagnostics(scenario);
    }
}

#[test]
fn nested_control_session_accepts_input_and_gracefully_cascades_deepest_first() {
    let _serial = LIVE_ADVERSARY_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut live = LiveAdversary::spawn("nested");
    assert!(
        live.wait_for(NESTED_READY, Duration::from_secs(5)),
        "nested control session did not bootstrap; output={:?}",
        String::from_utf8_lossy(&live.output)
    );

    // Exercise asymmetric child -> parent and parent -> child routing. The
    // parent -> child path must resume the hidden parent carrier so the child
    // stays responsive.
    live.send(b"\x1bC");
    assert!(
        live.wait_for_screen("Up/Down select", Duration::from_secs(1)),
        "nested connection manager did not render before parent switch: {:?}",
        live.screen_text()
    );
    live.send(b"\x1b[A\r");
    assert!(
        live.wait_for_screen(
            "Press Enter to switch to the nested session",
            Duration::from_secs(1)
        ),
        "switching to the parent did not show its nested portal: {:?}",
        live.screen_text()
    );
    live.send(b"\x1bC");
    assert!(
        live.wait_for_screen("Up/Down select", Duration::from_secs(1)),
        "connection manager did not reopen from the parent portal: {:?}",
        live.screen_text()
    );
    live.send(b"\x1b[B\r");
    live.send(b"p");
    assert!(
        live.wait_for_screen(
            std::str::from_utf8(NESTED_ACK).expect("nested marker is UTF-8"),
            Duration::from_secs(2)
        ),
        "nested input did not return after a manager parent/child round trip"
    );
    live.send(b"\x1bC");
    assert!(
        live.wait_for_screen("Up/Down select", Duration::from_secs(1)),
        "nested connection manager did not render: {:?}",
        live.screen_text()
    );
    // The active nested connection is the second row. Select its root and ask
    // for one graceful tree teardown; Lector must wait for the nested %exit
    // before sending the outer detach.
    live.send(b"\x1b[Ad");
    assert!(
        live.wait_for_event("nested-detach", Duration::from_secs(2)),
        "nested connection did not detach first"
    );
    assert!(
        live.wait_for_event("detach", Duration::from_secs(2)),
        "outer connection did not detach after its child"
    );
    assert!(
        live.finish(Duration::from_secs(5)),
        "nested graceful cascade did not finish"
    );
    live.assert_diagnostics("nested");
}
