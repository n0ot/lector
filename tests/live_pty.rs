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

const READY: &[u8] = b"LECTOR-BELL-READY";
const OUTER_READY: &[u8] = b"LECTOR-OUTER-SHELL-READY";
const OUTER_PROMPT: &[u8] = b"LECTOR-OUTER-PROMPT>";
const INNER_PROMPT: &[u8] = b"LECTOR-INNER-PROMPT>";
const COMPATIBLE_ENVIRONMENT: &[u8] = b"ENV:xterm-256color:unset";
const SPOKEN_READY: &str = "LECTOR dash- BELL dash- READY";
const SPOKEN_READY_UPDATE: &str = "BELL dash- READY";
const SPOKEN_OUTER_READY: &str = "OUTER dash- SHELL dash- READY";
const HOST_SCREEN: &[u8] = b"HOST-SCREEN-BEFORE-LECTOR";
const PARENT_AFTER: &[u8] = b"LECTOR-PARENT-AFTER:0";
const PARENT_INPUT_LEAK: &[u8] = b"LECTOR-PARENT-INPUT-LEAK";
const TMUX_FOREGROUND_READY: &[u8] = b"LECTOR-TMUX-FOREGROUND-READY";
const TMUX_FOREGROUND_ACK: &[u8] = b"LECTOR-TMUX-FOREGROUND-ACK";
static LIVE_PTY_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug)]
struct TestTerminalSize {
    columns: u16,
    rows: u16,
}

impl TestTerminalSize {
    const fn new(columns: u16, rows: u16) -> Self {
        Self { columns, rows }
    }

    const fn pty_size(self) -> PtySize {
        PtySize {
            rows: self.rows,
            cols: self.columns,
            // Match the 16x34 cell geometry required by this nested-terminal
            // regression.
            pixel_width: self.columns * 16,
            pixel_height: self.rows * 34,
        }
    }

    fn ghostty_profile(self) -> TerminalProfile {
        TerminalProfile {
            rows: self.rows,
            columns: self.columns,
            cell_width: 16,
            cell_height: 34,
            color_scheme: TerminalColorScheme::Dark,
            enquiry: b"Ghostty".to_vec(),
            version: "ghostty 1.2.3".to_owned(),
            da_conformance: 62,
            da_features: vec![22, 52],
            da_device_type: 1,
            da_firmware_version: 10,
            ..TerminalProfile::default()
        }
    }
}

const STANDARD_TERMINAL: TestTerminalSize = TestTerminalSize::new(80, 24);

fn serialize_live_pty_test() -> std::sync::MutexGuard<'static, ()> {
    LIVE_PTY_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct LiveLector {
    receiver: mpsc::Receiver<Vec<u8>>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    read_thread: Option<thread::JoinHandle<()>>,
    _master: Box<dyn MasterPty + Send>,
    physical_terminal: Terminal,
    output: Vec<u8>,
    input_after_shutdown_fence_reply: Option<(usize, Vec<u8>)>,
    outer_speech_log: PathBuf,
    inner_speech_log: PathBuf,
}

impl LiveLector {
    fn spawn(shell: &Path, nested: bool) -> Self {
        Self::spawn_at_size(shell, nested, STANDARD_TERMINAL)
    }

    fn spawn_at_size(shell: &Path, nested: bool, size: TestTerminalSize) -> Self {
        Self::spawn_at_size_under_parent(shell, nested, size, false)
    }

    fn spawn_with_parent_shell(shell: &Path) -> Self {
        Self::spawn_at_size_under_parent(shell, false, STANDARD_TERMINAL, true)
    }

    fn spawn_at_size_under_parent(
        shell: &Path,
        nested: bool,
        size: TestTerminalSize,
        parent_shell: bool,
    ) -> Self {
        let artifact_dir = fixture("target/test-artifacts/live-pty");
        fs::create_dir_all(&artifact_dir).expect("create live PTY artifact directory");
        let unique = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("system clock after Unix epoch")
                .as_nanos()
        );
        let outer_speech_log = artifact_dir.join(format!("outer-{unique}.jsonl"));
        let inner_speech_log = artifact_dir.join(format!("inner-{unique}.jsonl"));
        let pair = native_pty_system()
            .openpty(size.pty_size())
            .expect("open physical PTY");
        let mut command = if parent_shell {
            CommandBuilder::new(fixture("tests/fixtures/pty/parent-shell"))
        } else {
            let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_lector"));
            command.args([
                "--shell",
                shell.to_str().expect("UTF-8 fixture path"),
                "--speech-driver",
                "proc",
                "--speech-server",
                env!("CARGO_BIN_EXE_proc_stub_server"),
            ]);
            command
        };
        command.env("TERM", "xterm-ghostty");
        command.env("COLORTERM", "truecolor");
        command.env("LECTOR_OUTER_COLORS", "256");
        command.env("LECTOR_OUTER_TRUE_COLOR", "true");
        command.env("LECTOR_OUTER_HYPERLINKS", "true");
        command.env("LECTOR_OUTER_SYNC", "true");
        command.env("LECTOR_OUTER_KITTY_KEYBOARD", "true");
        command.env("LECTOR_OUTER_KITTY_GRAPHICS", "true");
        command.env("LECTOR_OUTER_FOCUS", "true");
        command.env("LECTOR_TEST_PRELOAD_OUTPUT", "1");
        command.env("LECTOR_PROC_STUB_LOG", &outer_speech_log);
        if parent_shell {
            command.env("LECTOR_TEST_BINARY", env!("CARGO_BIN_EXE_lector"));
            command.env("LECTOR_TEST_CHILD_SHELL", shell);
            command.env(
                "LECTOR_TEST_SPEECH_SERVER",
                env!("CARGO_BIN_EXE_proc_stub_server"),
            );
        }
        if nested {
            command.env("LECTOR_TEST_BINARY", env!("CARGO_BIN_EXE_lector"));
            command.env(
                "LECTOR_TEST_CHILD_SHELL",
                fixture("tests/fixtures/pty/bell-shell"),
            );
            command.env(
                "LECTOR_TEST_SPEECH_SERVER",
                env!("CARGO_BIN_EXE_proc_stub_server"),
            );
            command.env("LECTOR_TEST_INNER_SPEECH_LOG", &inner_speech_log);
            command.env("SHELL", fixture("tests/fixtures/pty/interactive-shell"));
            command.env("SPEECH_DRIVER", "proc");
            command.env("SPEECH_SERVER", env!("CARGO_BIN_EXE_proc_stub_server"));
        }
        let child = pair
            .slave
            .spawn_command(command)
            .expect("spawn live Lector");
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
        let writer = pair.master.take_writer().expect("take PTY writer");
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
                    Err(error) => panic!("read live Lector PTY: {error}"),
                }
            }
        });
        let mut physical_terminal =
            Terminal::new_with_profile(size.rows, size.columns, 10_000, size.ghostty_profile())
                .expect("create physical Ghostty terminal");
        physical_terminal
            .advance(HOST_SCREEN)
            .expect("prime the host screen beneath Lector");
        Self {
            receiver,
            writer,
            child,
            read_thread: Some(read_thread),
            _master: pair.master,
            physical_terminal,
            output: Vec::new(),
            input_after_shutdown_fence_reply: None,
            outer_speech_log,
            inner_speech_log,
        }
    }

    fn wait_for(&mut self, timeout: Duration, predicate: impl Fn(&[u8]) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while !predicate(&self.output) {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            match self.receiver.recv_timeout(remaining) {
                Ok(chunk) => self.accept_output(&chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => return false,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        predicate(&self.output)
    }

    fn wait_for_physical_terminal(
        &mut self,
        timeout: Duration,
        predicate: impl Fn(&Terminal) -> bool,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        while !predicate(&self.physical_terminal) {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            match self.receiver.recv_timeout(remaining) {
                Ok(chunk) => self.accept_output(&chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => return false,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        predicate(&self.physical_terminal)
    }

    fn accept_output(&mut self, chunk: &[u8]) {
        self.output.extend_from_slice(chunk);
        let update = self
            .physical_terminal
            .advance(chunk)
            .expect("parse physical output with Ghostty");
        if !update.pty_replies.is_empty() {
            self.writer
                .write_all(&update.pty_replies)
                .expect("write Ghostty terminal reply");
            let followup = self
                .input_after_shutdown_fence_reply
                .as_ref()
                .is_some_and(|(offset, _)| {
                    let scan_start = offset
                        .saturating_sub(lector::terminal_protocol::SHUTDOWN_FENCE_QUERY.len() - 1);
                    self.output[scan_start..]
                        .windows(lector::terminal_protocol::SHUTDOWN_FENCE_QUERY.len())
                        .any(|window| window == lector::terminal_protocol::SHUTDOWN_FENCE_QUERY)
                })
                .then(|| {
                    self.input_after_shutdown_fence_reply
                        .take()
                        .expect("armed shutdown-fence followup")
                        .1
                });
            if let Some(followup) = followup {
                self.writer
                    .write_all(&followup)
                    .expect("write input racing shutdown fence");
            }
            self.writer.flush().expect("flush Ghostty terminal reply");
        }
    }

    fn inject_input_after_shutdown_fence_reply(&mut self, input: &[u8]) {
        self.input_after_shutdown_fence_reply = Some((self.output.len(), input.to_vec()));
    }

    fn pump_output(&mut self, timeout: Duration) {
        if let Ok(chunk) = self.receiver.recv_timeout(timeout) {
            self.accept_output(&chunk);
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        // A real terminal consumes output and sends protocol replies
        // independently of the user's next keypress. Keep this single-threaded
        // libghostty host caught up before injecting physical input so a query
        // reply cannot arrive after Lector's probe window and leak to its child.
        while let Ok(chunk) = self.receiver.recv_timeout(Duration::from_millis(5)) {
            self.accept_output(&chunk);
        }
        self.writer.write_all(bytes).expect("write physical input");
        self.writer.flush().expect("flush physical input");
    }

    fn send_now(&mut self, bytes: &[u8]) {
        // Flood tests deliberately inject input without waiting for the PTY
        // output stream to become quiet; an infinite producer never does.
        self.writer.write_all(bytes).expect("write physical input");
        self.writer.flush().expect("flush physical input");
    }

    fn clear_speech_logs(&self) {
        fs::write(&self.outer_speech_log, []).expect("clear outer speech log");
        fs::write(&self.inner_speech_log, []).expect("clear inner speech log");
    }

    fn wait_for_outer_speech(&mut self, timeout: Duration, expected: &str) -> bool {
        let path = self.outer_speech_log.clone();
        self.wait_for_speech(timeout, |speech| speech.contains(expected), &path)
    }

    fn wait_for_inner_speech(&mut self, timeout: Duration, expected: &str) -> bool {
        let path = self.inner_speech_log.clone();
        self.wait_for_speech(timeout, |speech| speech.contains(expected), &path)
    }

    fn wait_for_exact_outer_speech(&mut self, timeout: Duration, expected: &str) -> bool {
        let path = self.outer_speech_log.clone();
        self.wait_for_speech(
            timeout,
            |speech| {
                speech.lines().any(|line| {
                    serde_json::from_str::<String>(line).is_ok_and(|entry| entry == expected)
                })
            },
            &path,
        )
    }

    fn wait_for_speech(
        &mut self,
        timeout: Duration,
        predicate: impl Fn(&str) -> bool,
        path: &Path,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if predicate(&fs::read_to_string(path).unwrap_or_default()) {
                return true;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            self.pump_output(remaining.min(Duration::from_millis(5)));
        }
    }

    fn outer_speech(&self) -> String {
        fs::read_to_string(&self.outer_speech_log).unwrap_or_default()
    }

    fn finish(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut exited = false;
        let mut disconnected = false;
        loop {
            if !exited {
                exited = self.child.try_wait().expect("poll live Lector").is_some();
            }
            if exited && disconnected {
                return true;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            if disconnected {
                thread::sleep(remaining.min(Duration::from_millis(1)));
                continue;
            }
            match self
                .receiver
                .recv_timeout(remaining.min(Duration::from_millis(10)))
            {
                Ok(chunk) => self.accept_output(&chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => disconnected = true,
            }
        }
    }
}

impl Drop for LiveLector {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(read_thread) = self.read_thread.take() {
            let _ = read_thread.join();
        }
        let _ = fs::remove_file(&self.outer_speech_log);
        let _ = fs::remove_file(&self.inner_speech_log);
    }
}

fn fixture(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn assert_bell_lifecycle(mut lector: LiveLector, case: &str) {
    assert!(
        lector.wait_for(Duration::from_secs(2), |output| output
            .windows(READY.len())
            .any(|window| window == READY)),
        "{case}: child output was stranded until unrelated physical input; output={:?}",
        String::from_utf8_lossy(&lector.output)
    );
    assert!(
        lector.physical_terminal.snapshot().alternate_screen,
        "{case}: Lector did not own the physical alternate screen"
    );
    assert!(
        lector.wait_for(Duration::from_secs(2), |output| output
            .windows(COMPATIBLE_ENVIRONMENT.len())
            .any(|window| window == COMPATIBLE_ENVIRONMENT)),
        "{case}: child did not receive the public compatibility TERM with private TERMINFO removed; output={:?}",
        String::from_utf8_lossy(&lector.output)
    );

    let before_bell = lector.output.len();
    lector.send(b"\x7f");
    assert!(
        lector.wait_for(Duration::from_secs(2), |output| output[before_bell..]
            .contains(&b'\x07')),
        "{case}: application BEL did not reach the physical terminal promptly"
    );
    assert_eq!(
        lector.output[before_bell..]
            .iter()
            .filter(|byte| **byte == b'\x07')
            .count(),
        1,
        "{case}: one application BEL must produce one physical BEL"
    );

    let before_exit = lector.output.len();
    lector.inject_input_after_shutdown_fence_reply(b"\x1b[99;5:3u");
    lector.send(b"q");
    assert!(
        lector.finish(Duration::from_secs(3)),
        "{case}: did not exit"
    );
    assert!(
        !lector.output[before_exit..].contains(&b'\x07'),
        "{case}: a delayed BEL escaped during shutdown"
    );
    assert!(
        lector.output[before_exit..]
            .windows(b"\x1b[?1049l".len())
            .any(|window| window == b"\x1b[?1049l"),
        "{case}: shutdown did not release the physical alternate screen"
    );
    let restored = lector.physical_terminal.snapshot();
    assert!(
        !restored.alternate_screen,
        "{case}: shutdown left the physical alternate screen active"
    );
    assert_eq!(
        restored.rows[0].text(),
        String::from_utf8_lossy(HOST_SCREEN),
        "{case}: shutdown did not restore the host screen"
    );
}

#[test]
fn live_pty_drains_output_that_was_ready_before_poll_registration_and_delivers_bells_promptly() {
    let _serial = serialize_live_pty_test();
    let shell = fixture("tests/fixtures/pty/bell-shell");
    assert_bell_lifecycle(LiveLector::spawn(&shell, false), "direct Lector");
}

#[test]
fn hidden_tmux_output_flood_does_not_starve_foreground_input() {
    let _serial = serialize_live_pty_test();
    let shell = fixture("tests/fixtures/pty/tmux-hidden-flood");
    let mut lector = LiveLector::spawn(&shell, false);
    assert!(
        lector.wait_for(Duration::from_secs(5), |output| output
            .windows(TMUX_FOREGROUND_READY.len())
            .any(|window| window == TMUX_FOREGROUND_READY)),
        "foreground pane did not become visible during hidden-pane output; output={:?}",
        String::from_utf8_lossy(&lector.output)
    );

    let sent_at = Instant::now();
    lector.send(b"p");
    assert!(
        lector.wait_for(Duration::from_secs(2), |output| output
            .windows(TMUX_FOREGROUND_ACK.len())
            .any(|window| window == TMUX_FOREGROUND_ACK)),
        "foreground input was starved for {:?} by hidden-pane output; output={:?}",
        sent_at.elapsed(),
        String::from_utf8_lossy(&lector.output)
    );

    lector.send(b"\x1bC");
    lector.send(b"d");
    assert!(
        lector.finish(Duration::from_secs(5)),
        "Lector's connection-manager detach did not close the tmux control client"
    );
}

#[test]
fn direct_output_flood_does_not_starve_auto_read_toggle() {
    let _serial = serialize_live_pty_test();
    let mut lector = LiveLector::spawn(Path::new("/usr/bin/yes"), false);
    assert!(
        lector.wait_for(Duration::from_secs(3), |output| output.contains(&b'y')),
        "Lector did not render output from yes"
    );
    lector.clear_speech_logs();

    let sent_at = Instant::now();
    lector.send_now(b"\x1b'");
    assert!(
        lector.wait_for_outer_speech(Duration::from_secs(1), "auto read disabled"),
        "auto-read toggle was starved for {:?} by direct PTY output; speech={:?}",
        sent_at.elapsed(),
        lector.outer_speech()
    );

    lector.send_now(b"\x03");
    assert!(
        lector.finish(Duration::from_secs(3)),
        "Lector did not exit after interrupting yes"
    );
}

#[test]
fn live_shutdown_fence_consumes_terminal_replies_before_parent_shell_resumes() {
    let _serial = serialize_live_pty_test();
    let shell = fixture("tests/fixtures/pty/bell-shell");
    let mut lector = LiveLector::spawn_with_parent_shell(&shell);
    assert!(
        lector.wait_for(Duration::from_secs(2), |output| output
            .windows(READY.len())
            .any(|window| window == READY)),
        "Lector child did not become ready"
    );

    let before_exit = lector.output.len();
    lector.send(b"q");
    assert!(
        lector.wait_for(Duration::from_secs(3), |output| output
            .windows(PARENT_AFTER.len())
            .any(|window| window == PARENT_AFTER)),
        "parent shell did not regain control after Lector exited; output={:?}",
        String::from_utf8_lossy(&lector.output)
    );
    assert!(
        lector.finish(Duration::from_secs(3)),
        "parent shell did not exit"
    );

    let shutdown = &lector.output[before_exit..];
    assert!(
        !shutdown.contains(&b'\x07'),
        "a terminal reply escaped to the parent shell and rang its bell"
    );
    assert!(
        !shutdown
            .windows(PARENT_INPUT_LEAK.len())
            .any(|window| window == PARENT_INPUT_LEAK),
        "the parent shell observed bytes that belonged to Lector"
    );
    let fence = shutdown
        .windows(b"\x1b[c".len())
        .position(|window| window == b"\x1b[c")
        .expect("shutdown emitted DA1 fence");
    let release = shutdown
        .windows(b"\x1b[?1049l".len())
        .position(|window| window == b"\x1b[?1049l")
        .expect("shutdown released alternate screen");
    assert!(
        fence < release,
        "DA1 fence must precede alternate-screen release"
    );
}

#[test]
fn nested_lector_output_and_bells_are_visible_to_the_outer_lector() {
    let _serial = serialize_live_pty_test();
    let shell = fixture("tests/fixtures/pty/nested-lector-shell");
    let mut lector = LiveLector::spawn(&shell, true);
    assert!(
        lector.wait_for(Duration::from_secs(2), |output| output
            .windows(OUTER_READY.len())
            .any(|window| window == OUTER_READY)),
        "outer shell did not become ready before launching nested Lector"
    );
    assert!(
        lector.wait_for_outer_speech(Duration::from_secs(2), SPOKEN_OUTER_READY),
        "outer shell screen did not settle before nested launch; speech={:?}",
        lector.outer_speech()
    );
    lector.clear_speech_logs();
    // Move the outer review cursor onto the blank row before the nested full
    // repaint. Blank review lines are intentionally silent, so use a complete
    // Kitty event and the nested READY output below as the synchronization
    // barrier rather than waiting for obsolete "blank" speech.
    lector.send(b"\x1b[111;3u");
    lector.send(b"n");
    assert!(
        lector.wait_for(Duration::from_secs(2), |output| output
            .windows(READY.len())
            .any(|window| window == READY)),
        "nested Lector did not present child output"
    );
    assert!(
        lector.wait_for_outer_speech(Duration::from_secs(2), SPOKEN_READY_UPDATE),
        "outer Lector did not settle and auto-read the nested screen; speech={:?}",
        lector.outer_speech()
    );

    lector.clear_speech_logs();
    lector.send(b"\x1bi");
    assert!(
        lector.wait_for_outer_speech(Duration::from_secs(2), SPOKEN_READY),
        "the outer Lector review cursor could not read the inner Lector screen; speech={:?}",
        lector.outer_speech()
    );

    lector.clear_speech_logs();
    lector.send(b"\x1br");
    assert!(
        lector.wait_for_outer_speech(Duration::from_secs(2), SPOKEN_READY),
        "the outer Lector Review overlay captured a blank inner Lector screen; speech={:?}",
        lector.outer_speech()
    );
    lector.clear_speech_logs();
    lector.send(b"q");
    assert!(
        lector.wait_for_outer_speech(Duration::from_secs(2), SPOKEN_READY),
        "the outer Lector did not return from Review to the nested terminal; speech={:?}",
        lector.outer_speech()
    );

    assert_bell_lifecycle(lector, "nested Lector");
}

fn assert_outer_review_reads_inner_prompt(size: TestTerminalSize) {
    let shell = fixture("tests/fixtures/pty/interactive-shell");
    let mut lector = LiveLector::spawn_at_size(&shell, true, size);
    assert!(
        lector.wait_for(Duration::from_secs(2), |output| output
            .windows(OUTER_PROMPT.len())
            .any(|window| window == OUTER_PROMPT)),
        "outer Lector did not display its shell prompt"
    );
    assert!(
        lector.wait_for_outer_speech(Duration::from_secs(2), "OUTER dash- PROMPT"),
        "outer shell prompt did not settle before nested launch; speech={:?}",
        lector.outer_speech()
    );
    lector.clear_speech_logs();
    lector.send(b"\x1b'");
    assert!(
        lector.wait_for_outer_speech(Duration::from_secs(2), "auto read disabled"),
        "test setup could not disable outer auto-read; speech={:?}",
        lector.outer_speech()
    );
    lector.clear_speech_logs();

    lector.send(b"lector\r");
    assert!(
        lector.wait_for_inner_speech(Duration::from_secs(3), "INNER dash- PROMPT"),
        "{}x{}: inner Lector did not read its own default-shell prompt; outer speech={:?}; inner speech={:?}",
        size.columns,
        size.rows,
        lector.outer_speech(),
        fs::read_to_string(&lector.inner_speech_log).unwrap_or_default()
    );
    assert!(
        lector.wait_for_physical_terminal(Duration::from_secs(3), |terminal| {
            let snapshot = terminal.snapshot();
            snapshot
                .rows
                .first()
                .is_some_and(|row| row.text().trim_end().as_bytes() == INNER_PROMPT)
                && snapshot.cursor.row == 0
                && usize::from(snapshot.cursor.col) == INNER_PROMPT.len()
        }),
        "{}x{}: inner Lector spoke its prompt, but its complete frame and final cursor remained stranded before reaching the physical Ghostty host; output={:?}; physical_row={:?}; cursor={:?}",
        size.columns,
        size.rows,
        String::from_utf8_lossy(&lector.output),
        lector
            .physical_terminal
            .snapshot()
            .rows
            .first()
            .map(lector_ghostty::RowSnapshot::text),
        lector.physical_terminal.snapshot().cursor
    );
    // Auto-read is disabled above and this log is cleared only after the inner
    // process has spoken, so any following outer speech was caused by M-i.
    lector.clear_speech_logs();
    lector.send(b"\x1bi");
    assert!(
        lector.wait_for_exact_outer_speech(
            Duration::from_secs(2),
            "LECTOR dash- INNER dash- PROMPT greater "
        ),
        "{}x{}: outer M-i did not read the nested Lector's default-shell prompt; outer speech={:?}; inner speech={:?}",
        size.columns,
        size.rows,
        lector.outer_speech(),
        fs::read_to_string(&lector.inner_speech_log).unwrap_or_default()
    );
    assert!(
        lector
            .output
            .windows(INNER_PROMPT.len())
            .any(|window| window == INNER_PROMPT),
        "{}x{}: inner prompt never reached the physical Ghostty host",
        size.columns,
        size.rows
    );
}

#[test]
fn outer_review_reads_inner_prompt_at_80_by_24() {
    let _serial = serialize_live_pty_test();
    assert_outer_review_reads_inner_prompt(TestTerminalSize::new(80, 24));
}

#[test]
fn outer_review_reads_inner_prompt_at_160_by_24() {
    let _serial = serialize_live_pty_test();
    assert_outer_review_reads_inner_prompt(TestTerminalSize::new(160, 24));
}

#[test]
fn outer_review_reads_inner_prompt_at_160_by_50() {
    let _serial = serialize_live_pty_test();
    assert_outer_review_reads_inner_prompt(TestTerminalSize::new(160, 50));
}
