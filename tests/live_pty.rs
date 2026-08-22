#![cfg(target_os = "macos")]

use lector_ghostty::{Terminal, TerminalColorScheme, TerminalProfile};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    ffi::OsString,
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
const COMPATIBLE_ENVIRONMENT: &[u8] = b"ENV:xterm-256color:sync";
const SPOKEN_READY: &str = "LECTOR dash- BELL dash- READY";
const SPOKEN_READY_UPDATE: &str = "BELL dash- READY";
const SPOKEN_OUTER_READY: &str = "OUTER dash- SHELL dash- READY";
const HOST_SCREEN: &[u8] = b"HOST-SCREEN-BEFORE-LECTOR";
const PARENT_AFTER: &[u8] = b"LECTOR-PARENT-AFTER:0";
const PARENT_INPUT_LEAK: &[u8] = b"LECTOR-PARENT-INPUT-LEAK";
const TMUX_FOREGROUND_READY: &[u8] = b"LECTOR-TMUX-FOREGROUND-READY";
const TMUX_FOREGROUND_ACK: &[u8] = b"LECTOR-TMUX-FOREGROUND-ACK";
const TMUX_SYNC_READY: &str = "LECTOR-TMUX-SYNC-READY";
const LATENCY_READY: &str = "LECTOR-LATENCY-READY";
const STARTUP_HOOK_MARKER: &[u8] = b"LECTOR-STARTUP-HOOK-RAN";
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

#[derive(Clone, Debug)]
enum StartupConfiguration {
    ProcessFixture,
    Native,
    Explicit {
        config: PathBuf,
        atomic_startup: bool,
    },
    FatalSpeech {
        config: PathBuf,
        state: PathBuf,
    },
    NoConfig {
        config: PathBuf,
        marker: PathBuf,
    },
    ResolvedConfig {
        cli_config: Option<PathBuf>,
        environment_config: Option<PathBuf>,
        xdg_config_home: Option<OsString>,
        home: PathBuf,
        marker: PathBuf,
    },
}

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
    input_before_first_terminal_reply: Option<Vec<u8>>,
    input_after_shutdown_fence_reply: Option<(usize, Vec<u8>)>,
    outer_speech_log: PathBuf,
    inner_speech_log: PathBuf,
}

impl LiveLector {
    fn spawn(shell: &Path, nested: bool) -> Self {
        Self::spawn_at_size(shell, nested, STANDARD_TERMINAL)
    }

    fn spawn_at_size(shell: &Path, nested: bool, size: TestTerminalSize) -> Self {
        Self::spawn_at_size_under_parent(
            shell,
            nested,
            size,
            false,
            StartupConfiguration::ProcessFixture,
        )
    }

    fn spawn_with_native_speech(shell: &Path) -> Self {
        Self::spawn_at_size_under_parent(
            shell,
            false,
            STANDARD_TERMINAL,
            false,
            StartupConfiguration::Native,
        )
    }

    fn spawn_with_parent_shell(shell: &Path) -> Self {
        Self::spawn_at_size_under_parent(
            shell,
            false,
            STANDARD_TERMINAL,
            true,
            StartupConfiguration::ProcessFixture,
        )
    }

    fn spawn_with_config(shell: &Path, config: &Path) -> Self {
        Self::spawn_at_size_under_parent(
            shell,
            false,
            STANDARD_TERMINAL,
            false,
            StartupConfiguration::Explicit {
                config: config.to_path_buf(),
                atomic_startup: false,
            },
        )
    }

    fn spawn_with_atomic_config(shell: &Path, config: &Path) -> Self {
        Self::spawn_at_size_under_parent(
            shell,
            false,
            STANDARD_TERMINAL,
            false,
            StartupConfiguration::Explicit {
                config: config.to_path_buf(),
                atomic_startup: true,
            },
        )
    }

    fn spawn_without_config(shell: &Path, config: &Path, marker: &Path) -> Self {
        Self::spawn_at_size_under_parent(
            shell,
            false,
            STANDARD_TERMINAL,
            false,
            StartupConfiguration::NoConfig {
                config: config.to_path_buf(),
                marker: marker.to_path_buf(),
            },
        )
    }

    fn spawn_with_resolved_config(
        shell: &Path,
        cli_config: Option<PathBuf>,
        environment_config: Option<PathBuf>,
        xdg_config_home: Option<OsString>,
        home: PathBuf,
        marker: PathBuf,
    ) -> Self {
        Self::spawn_at_size_under_parent(
            shell,
            false,
            STANDARD_TERMINAL,
            false,
            StartupConfiguration::ResolvedConfig {
                cli_config,
                environment_config,
                xdg_config_home,
                home,
                marker,
            },
        )
    }

    fn spawn_with_fatal_speech(shell: &Path, config: &Path, state: &Path) -> Self {
        Self::spawn_at_size_under_parent(
            shell,
            false,
            STANDARD_TERMINAL,
            false,
            StartupConfiguration::FatalSpeech {
                config: config.to_path_buf(),
                state: state.to_path_buf(),
            },
        )
    }

    fn spawn_at_size_under_parent(
        shell: &Path,
        nested: bool,
        size: TestTerminalSize,
        parent_shell: bool,
        startup_configuration: StartupConfiguration,
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
            command.args(["--shell", shell.to_str().expect("UTF-8 fixture path")]);
            match &startup_configuration {
                StartupConfiguration::ProcessFixture => {
                    command.args([
                        "--config",
                        fixture("tests/fixtures/pty/proc-speech.lua")
                            .to_str()
                            .expect("UTF-8 fixture path"),
                    ]);
                }
                StartupConfiguration::Native => {}
                StartupConfiguration::Explicit { config, .. }
                | StartupConfiguration::FatalSpeech { config, .. } => {
                    command.args(["--config", config.to_str().expect("UTF-8 fixture path")]);
                }
                StartupConfiguration::NoConfig { .. } => {
                    command.arg("--no-config");
                }
                StartupConfiguration::ResolvedConfig { cli_config, .. } => {
                    if let Some(config) = cli_config {
                        command.args(["--config", config.to_str().expect("UTF-8 fixture path")]);
                    }
                }
            }
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
        let native_speech = matches!(
            &startup_configuration,
            StartupConfiguration::Native | StartupConfiguration::NoConfig { .. }
        );
        if native_speech {
            command.env("LECTOR_SPEECH_TEST_MUTE", "1");
            command.env("LECTOR_SPEECH_EVENT_LOG", &outer_speech_log);
            command.env("LECTOR_SPEECH_RPC_LOG", &inner_speech_log);
        } else {
            command.env("LECTOR_PROC_STUB_LOG", &outer_speech_log);
            command.env(
                "LECTOR_TEST_SPEECH_SERVER",
                env!("CARGO_BIN_EXE_proc_stub_server"),
            );
            command.env(
                "LECTOR_TEST_SPEECH_CONFIG",
                fixture("tests/fixtures/pty/proc-speech.lua"),
            );
        }
        match &startup_configuration {
            StartupConfiguration::Explicit {
                config,
                atomic_startup,
            } => {
                command.env("LECTOR_TEST_STARTUP_SPEECH_SERVER", "/bin/bash");
                command.env(
                    "LECTOR_TEST_STARTUP_SPEECH_SCRIPT",
                    fixture("tests/fixtures/pty/startup-speech-server"),
                );
                command.env("LECTOR_TEST_STARTUP_CONFIG", config);
                command.env("LECTOR_TEST_STARTUP_ORDER_LOG", &inner_speech_log);
                if *atomic_startup {
                    command.env("LECTOR_TEST_ATOMIC_STARTUP", "1");
                }
            }
            StartupConfiguration::FatalSpeech { state, .. } => {
                command.env("LECTOR_TEST_FATAL_SPEECH_STATE", state);
                command.env("LECTOR_TEST_FATAL_SPEECH_RPC_LOG", &inner_speech_log);
            }
            StartupConfiguration::NoConfig { config, marker } => {
                command.env("LECTOR_CONFIG", config);
                command.env("LECTOR_NO_CONFIG_MARKER", marker);
            }
            StartupConfiguration::ResolvedConfig {
                environment_config,
                xdg_config_home,
                home,
                marker,
                ..
            } => {
                command.env_remove("LECTOR_CONFIG");
                command.env_remove("XDG_CONFIG_HOME");
                command.env("HOME", home);
                command.env("LECTOR_CONFIG_TEST_MARKER", marker);
                command.env("LECTOR_SPEECH_TEST_MUTE", "1");
                if let Some(config) = environment_config {
                    command.env("LECTOR_CONFIG", config);
                }
                if let Some(config_home) = xdg_config_home {
                    command.env("XDG_CONFIG_HOME", config_home);
                }
            }
            StartupConfiguration::ProcessFixture | StartupConfiguration::Native => {}
        }
        if parent_shell {
            command.env("LECTOR_TEST_BINARY", env!("CARGO_BIN_EXE_lector"));
            command.env("LECTOR_TEST_CHILD_SHELL", shell);
        }
        if nested {
            command.env("LECTOR_TEST_BINARY", env!("CARGO_BIN_EXE_lector"));
            command.env(
                "LECTOR_TEST_CHILD_SHELL",
                fixture("tests/fixtures/pty/bell-shell"),
            );
            command.env("LECTOR_TEST_INNER_SPEECH_LOG", &inner_speech_log);
            command.env("SHELL", fixture("tests/fixtures/pty/interactive-shell"));
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
            input_before_first_terminal_reply: None,
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
            if let Some(input) = self.input_before_first_terminal_reply.take() {
                self.writer
                    .write_all(&input)
                    .expect("write input immediately before terminal reply");
            }
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

    fn inject_input_before_first_terminal_reply(&mut self, input: &[u8]) {
        self.input_before_first_terminal_reply = Some(input.to_vec());
    }

    fn inject_input_after_shutdown_fence_reply(&mut self, input: &[u8]) {
        self.input_after_shutdown_fence_reply = Some((self.output.len(), input.to_vec()));
    }

    fn pump_output(&mut self, timeout: Duration) {
        if let Ok(chunk) = self.receiver.recv_timeout(timeout) {
            self.accept_output(&chunk);
        }
    }

    fn pump_for(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            self.pump_output(remaining.min(Duration::from_millis(5)));
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

    fn wait_for_exit(&mut self, timeout: Duration) -> Option<portable_pty::ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll live Lector") {
                return Some(status);
            }
            let remaining = deadline.checked_duration_since(Instant::now())?;
            self.pump_output(remaining.min(Duration::from_millis(10)));
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

fn write_config_resolution_fixture(path: &Path, identity: &str) {
    fs::create_dir_all(path.parent().expect("config fixture parent"))
        .expect("create config fixture directory");
    fs::write(
        path,
        format!(
            r#"
                local marker = assert(io.open(os.getenv("LECTOR_CONFIG_TEST_MARKER"), "w"))
                marker:write({identity:?})
                marker:close()
                lector.o.speech = {{
                    program = assert(os.getenv("LECTOR_TEST_SPEECH_SERVER")),
                    args = {{}},
                }}
            "#
        ),
    )
    .expect("write config resolution fixture");
}

fn assert_resolved_config(
    artifact_dir: &Path,
    case: &str,
    cli_config: Option<PathBuf>,
    environment_config: Option<PathBuf>,
    xdg_config_home: Option<OsString>,
    home: PathBuf,
    expected: &str,
) {
    let marker = artifact_dir.join(format!("{case}.loaded"));
    let mut lector = LiveLector::spawn_with_resolved_config(
        &fixture("tests/fixtures/pty/latency-child"),
        cli_config,
        environment_config,
        xdg_config_home,
        home,
        marker.clone(),
    );
    assert!(
        lector.wait_for_physical_terminal(Duration::from_secs(5), |terminal| {
            physical_screen_contains(terminal, LATENCY_READY)
        }),
        "{case}: Lector did not reach the child terminal; output={:?}",
        String::from_utf8_lossy(&lector.output)
    );
    assert_eq!(
        fs::read_to_string(&marker).expect("selected config wrote its marker"),
        expected,
        "{case}: wrong init.lua was loaded"
    );
    lector.send(b"q");
    assert!(
        lector.finish(Duration::from_secs(3)),
        "{case}: Lector did not exit"
    );
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
        "{case}: child did not receive the public xterm identity with synchronized output advertised; output={:?}",
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

fn physical_screen_contains(terminal: &Terminal, expected: &str) -> bool {
    terminal
        .snapshot()
        .rows
        .iter()
        .any(|row| row.text().contains(expected))
}

fn assert_live_key_to_pixel_latency(shell: &Path, case: &str, control_mode: bool) {
    let mut lector = LiveLector::spawn(shell, false);
    assert!(
        lector.wait_for_physical_terminal(Duration::from_secs(5), |terminal| {
            physical_screen_contains(terminal, LATENCY_READY)
        }),
        "{case}: child did not become physically visible; output={:?}",
        String::from_utf8_lossy(&lector.output)
    );

    let mut samples = Vec::new();
    for sample in 1..=20 {
        let expected = format!("LECTOR-LATENCY-ACK-{sample:02}");
        let sent_at = Instant::now();
        lector.send_now(b"p");
        assert!(
            lector.wait_for_physical_terminal(Duration::from_secs(2), |terminal| {
                physical_screen_contains(terminal, &expected)
            }),
            "{case}: response {sample} did not reach the physical terminal"
        );
        samples.push(sent_at.elapsed());
    }
    samples.sort_unstable();
    let median = samples[samples.len() / 2];
    let p95 = samples[(samples.len() * 95).div_ceil(100) - 1];
    eprintln!("{case}: key-to-pixel median={median:?} p95={p95:?}");
    assert!(
        median < Duration::from_millis(25),
        "{case}: median key-to-pixel latency {median:?} exceeded 25 ms; samples={samples:?}"
    );
    assert!(
        p95 < Duration::from_millis(100),
        "{case}: p95 key-to-pixel latency {p95:?} exceeded 100 ms; samples={samples:?}"
    );

    lector.send_now(b"q");
    if control_mode && !lector.finish(Duration::from_secs(2)) {
        lector.send_now(b"\x1bC");
        lector.send_now(b"d");
    }
    assert!(
        lector.finish(Duration::from_secs(5)),
        "{case}: Lector did not exit after the latency probe"
    );
}

#[test]
fn direct_terminal_key_to_pixel_latency_stays_interactive() {
    let _serial = serialize_live_pty_test();
    assert_live_key_to_pixel_latency(
        &fixture("tests/fixtures/pty/latency-child"),
        "direct terminal",
        false,
    );
}

#[test]
fn direct_native_speech_continues_after_startup_and_never_blocks_input() {
    let _serial = serialize_live_pty_test();
    let mut lector =
        LiveLector::spawn_with_native_speech(&fixture("tests/fixtures/pty/latency-child"));
    assert!(
        lector.wait_for(Duration::from_secs(3), |output| output
            .windows(LATENCY_READY.len())
            .any(|window| window == LATENCY_READY.as_bytes())),
        "native-speech Lector did not present its child"
    );
    let event_count = |speech: &str| {
        speech
            .lines()
            .filter(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .is_ok_and(|record| record["event"] == "begin")
            })
            .count()
    };
    let event_log = lector.outer_speech_log.clone();
    assert!(
        lector.wait_for_speech(
            Duration::from_secs(5),
            |speech| event_count(speech) >= 1,
            &event_log,
        ),
        "native speech never began its startup utterance"
    );
    // Let any already-queued startup announcements begin before measuring the
    // post-input utterance. M-i interrupts speech and immediately reads the
    // current line, matching the ordering used by ordinary key interaction.
    thread::sleep(Duration::from_millis(100));
    let before = event_count(&lector.outer_speech());
    let sent_at = Instant::now();
    lector.send(b"\x1bi");
    assert!(
        lector.wait_for_speech(
            Duration::from_secs(5),
            |speech| event_count(speech) > before,
            &event_log,
        ),
        "native speech did not begin another utterance after startup; events={:?}; rpc={:?}; output={:?}",
        lector.outer_speech(),
        fs::read_to_string(&lector.inner_speech_log).unwrap_or_default(),
        String::from_utf8_lossy(&lector.output)
    );
    assert!(
        sent_at.elapsed() < Duration::from_millis(250),
        "replacement native speech took {:?} to begin",
        sent_at.elapsed()
    );

    lector.send(b"s");
    let rpc_log = lector.inner_speech_log.clone();
    assert!(
        lector.wait_for_speech(
            Duration::from_secs(5),
            |records| {
                records.lines().any(|line| {
                    serde_json::from_str::<serde_json::Value>(line).is_ok_and(|record| {
                        record["method"] == "speak"
                            && record["params"]["text"]
                                .as_str()
                                .is_some_and(|text| text.len() > 1_000)
                    })
                })
            },
            &rpc_log,
        ),
        "native host never received the deliberately long utterance"
    );
    let input_at = Instant::now();
    lector.send_now(b"p");
    assert!(
        lector.wait_for_physical_terminal(Duration::from_secs(2), |terminal| {
            physical_screen_contains(terminal, "LECTOR-LATENCY-ACK-01")
        }),
        "terminal input was blocked behind native speech"
    );
    assert!(
        input_at.elapsed() < Duration::from_millis(100),
        "native speech delayed terminal input/rendering for {:?}",
        input_at.elapsed()
    );
    lector.send(b"q");
    assert!(
        lector.finish(Duration::from_secs(3)),
        "native-speech Lector did not exit"
    );
}

#[test]
fn init_lua_process_argv_and_deferred_speech_cross_the_ready_boundary_in_order() {
    let _serial = serialize_live_pty_test();
    let config = fixture("tests/fixtures/pty/startup-speech.lua");
    let mut lector =
        LiveLector::spawn_with_config(&fixture("tests/fixtures/pty/latency-child"), &config);

    assert!(
        lector.wait_for_outer_speech(Duration::from_secs(3), "LECTOR-TOP-LEVEL-SPEAK"),
        "top-level init.lua speech was not buffered through the configured server handshake; speech={:?}; output={:?}",
        lector.outer_speech(),
        String::from_utf8_lossy(&lector.output)
    );
    assert!(
        lector.wait_for(Duration::from_secs(3), |output| output
            .windows(STARTUP_HOOK_MARKER.len())
            .any(|window| window == STARTUP_HOOK_MARKER)),
        "on_startup could not speak through the configured server; speech={:?}; output={:?}",
        lector.outer_speech(),
        String::from_utf8_lossy(&lector.output)
    );

    let initial_frame = lector
        .output
        .windows(LATENCY_READY.len())
        .position(|window| window == LATENCY_READY.as_bytes())
        .expect("initial child frame reached the physical terminal");
    let startup_hook = lector
        .output
        .windows(STARTUP_HOOK_MARKER.len())
        .position(|window| window == STARTUP_HOOK_MARKER)
        .expect("startup hook emitted its synchronous marker");
    assert!(
        initial_frame < startup_hook,
        "on_startup ran before the initial child frame was presented; output={:?}",
        String::from_utf8_lossy(&lector.output)
    );

    let spoken = lector
        .outer_speech()
        .lines()
        .filter_map(|line| serde_json::from_str::<String>(line).ok())
        .collect::<Vec<_>>();
    assert_eq!(
        spoken,
        ["LECTOR-TOP-LEVEL-SPEAK", "LECTOR-STARTUP-HOOK-SPEAK"],
        "startup speech must remain ordered across handshake and ready-hook boundaries"
    );
    assert!(
        !lector
            .output
            .windows(b"LECTOR-SPEECH-ARGV-ERROR".len())
            .any(|window| window == b"LECTOR-SPEECH-ARGV-ERROR"),
        "Lua process arguments were not passed as exact argv entries"
    );

    lector.send(b"q");
    assert!(
        lector.finish(Duration::from_secs(3)),
        "custom-speech Lector did not exit"
    );
}

#[test]
fn startup_hook_waits_for_the_committed_dec_2026_frame() {
    let _serial = serialize_live_pty_test();
    let config = fixture("tests/fixtures/pty/startup-speech.lua");
    let mut lector =
        LiveLector::spawn_with_atomic_config(&fixture("tests/fixtures/pty/latency-child"), &config);

    // Place physical input immediately before the focus-mode response. The
    // ownership query necessarily reads both before terminal activation, so
    // this key exercises its preservation buffer rather than the later mio
    // stdin path.
    lector.inject_input_before_first_terminal_reply(b"p");
    assert!(
        lector.wait_for(Duration::from_secs(2), |output| output
            .windows(b"\x1b[?1049h".len())
            .any(|window| window == b"\x1b[?1049h")),
        "Lector did not acquire the physical terminal before the atomic draw"
    );
    // The early key must remain pending while DEC 2026 is open, then run only
    // after the committed frame is physical and on_startup has completed.
    assert!(
        lector.wait_for(Duration::from_secs(3), |output| output
            .windows(STARTUP_HOOK_MARKER.len())
            .any(|window| window == STARTUP_HOOK_MARKER)),
        "on_startup did not run after the atomic child draw; output={:?}",
        String::from_utf8_lossy(&lector.output)
    );
    lector
        .output
        .windows(STARTUP_HOOK_MARKER.len())
        .position(|window| window == STARTUP_HOOK_MARKER)
        .expect("startup hook emitted its synchronous marker");
    assert!(
        physical_screen_contains(&lector.physical_terminal, "LECTOR-ATOMIC-FINAL"),
        "on_startup ran before the committed DEC 2026 frame was physical; screen={:?}; output={:?}",
        lector
            .physical_terminal
            .snapshot()
            .rows
            .first()
            .map(lector_ghostty::RowSnapshot::text),
        String::from_utf8_lossy(&lector.output)
    );
    assert!(
        lector.wait_for_physical_terminal(Duration::from_secs(2), |terminal| {
            physical_screen_contains(terminal, "LECTOR-LATENCY-ACK-01")
        }),
        "input retained during startup was not delivered after on_startup"
    );
    assert_eq!(
        fs::read_to_string(&lector.inner_speech_log)
            .expect("read startup hook/input order log")
            .lines()
            .collect::<Vec<_>>(),
        ["hook", "input"],
        "physical input was consumed before on_startup"
    );
    assert!(
        !lector
            .output
            .windows(b"LECTOR-ATOMIC-TRANSIENT".len())
            .any(|window| window == b"LECTOR-ATOMIC-TRANSIENT"),
        "the compositor exposed a transient DEC 2026 startup frame; output={:?}",
        String::from_utf8_lossy(&lector.output)
    );
    assert!(
        lector.wait_for_outer_speech(Duration::from_secs(2), "LECTOR-STARTUP-HOOK-SPEAK"),
        "on_startup could not speak after the committed atomic frame"
    );
    assert!(
        !lector.outer_speech().contains("ATOMIC-TRANSIENT"),
        "speech observed transient DEC 2026 pixels: {:?}",
        lector.outer_speech()
    );

    lector.send(b"q");
    assert!(
        lector.finish(Duration::from_secs(3)),
        "atomic-startup Lector did not exit"
    );
}

#[test]
fn a_second_speech_crash_wakes_and_terminates_the_live_main_loop() {
    let _serial = serialize_live_pty_test();
    let artifact_dir = fixture("target/test-artifacts/live-pty");
    fs::create_dir_all(&artifact_dir).expect("create live PTY artifact directory");
    let unique = format!(
        "fatal-speech-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system clock after Unix epoch")
            .as_nanos()
    );
    let state = artifact_dir.join(format!("{unique}.state"));
    let config = fixture("tests/fixtures/pty/fatal-speech.lua");
    let started = Instant::now();
    let mut lector = LiveLector::spawn_with_fatal_speech(
        &fixture("tests/fixtures/pty/latency-child"),
        &config,
        &state,
    );

    let status = lector
        .wait_for_exit(Duration::from_secs(3))
        .unwrap_or_else(|| {
            panic!(
                "fatal speech event did not wake the main loop; output={:?}; rpc={:?}",
                String::from_utf8_lossy(&lector.output),
                fs::read_to_string(&lector.inner_speech_log).unwrap_or_default()
            )
        });
    assert!(
        !status.success(),
        "Lector exited successfully after a fatal second speech crash: {status:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "fatal speech shutdown was not prompt: {:?}",
        started.elapsed()
    );
    assert_eq!(
        fs::read_to_string(&state)
            .expect("read speech lifecycle generation")
            .trim(),
        "2",
        "the fatal path must not start a third server generation"
    );
    let crash_records = fs::read_to_string(&lector.inner_speech_log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|record| record["method"] == "speak")
        .map(|record| record["generation"].as_u64())
        .collect::<Vec<_>>();
    assert_eq!(
        crash_records,
        [Some(1), Some(2)],
        "both crashing requests must reach their distinct server generations"
    );

    drop(lector);
    fs::remove_file(&state).expect("remove speech lifecycle state");
}

#[test]
fn startup_config_resolution_honors_cli_environment_xdg_and_macos_legacy_order() {
    let _serial = serialize_live_pty_test();
    let unique = format!(
        "config-resolution-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system clock after Unix epoch")
            .as_nanos()
    );
    let artifact_dir = fixture("target/test-artifacts/live-pty").join(&unique);
    fs::create_dir_all(&artifact_dir).expect("create config resolution artifact directory");

    let cli_home = artifact_dir.join("cli/home");
    let cli_xdg = artifact_dir.join("cli/xdg");
    let cli_config = artifact_dir.join("cli/explicit.lua");
    let environment_config = artifact_dir.join("cli/environment.lua");
    write_config_resolution_fixture(&cli_home.join(".config/lector/init.lua"), "home-default");
    write_config_resolution_fixture(&cli_xdg.join("lector/init.lua"), "xdg");
    write_config_resolution_fixture(&environment_config, "environment");
    write_config_resolution_fixture(&cli_config, "cli");
    assert_resolved_config(
        &artifact_dir,
        "cli-precedence",
        Some(cli_config),
        Some(environment_config),
        Some(cli_xdg.into_os_string()),
        cli_home,
        "cli",
    );

    let environment_home = artifact_dir.join("environment/home");
    let environment_xdg = artifact_dir.join("environment/xdg");
    let environment_config = artifact_dir.join("environment/selected.lua");
    write_config_resolution_fixture(
        &environment_home.join(".config/lector/init.lua"),
        "home-default",
    );
    write_config_resolution_fixture(&environment_xdg.join("lector/init.lua"), "xdg");
    write_config_resolution_fixture(&environment_config, "environment");
    assert_resolved_config(
        &artifact_dir,
        "environment-precedence",
        None,
        Some(environment_config),
        Some(environment_xdg.into_os_string()),
        environment_home,
        "environment",
    );

    let xdg_home = artifact_dir.join("xdg/home");
    let explicit_xdg = artifact_dir.join("xdg/selected");
    write_config_resolution_fixture(&xdg_home.join(".config/lector/init.lua"), "home-default");
    write_config_resolution_fixture(
        &xdg_home.join("Library/Application Support/lector/init.lua"),
        "legacy",
    );
    write_config_resolution_fixture(&explicit_xdg.join("lector/init.lua"), "xdg");
    assert_resolved_config(
        &artifact_dir,
        "absolute-xdg",
        None,
        None,
        Some(explicit_xdg.into_os_string()),
        xdg_home,
        "xdg",
    );

    for (case, xdg_config_home) in [("unset-xdg", None), ("empty-xdg", Some(OsString::new()))] {
        let home = artifact_dir.join(case).join("home");
        write_config_resolution_fixture(&home.join(".config/lector/init.lua"), "home-default");
        assert_resolved_config(
            &artifact_dir,
            case,
            None,
            None,
            xdg_config_home,
            home,
            "home-default",
        );
    }

    let relative_home = artifact_dir.join("relative-xdg/home");
    let relative_xdg = artifact_dir.join("relative-xdg/candidate");
    let relative_xdg_value = relative_xdg
        .strip_prefix(fixture(""))
        .expect("artifact below repository root")
        .as_os_str()
        .to_owned();
    write_config_resolution_fixture(
        &relative_home.join(".config/lector/init.lua"),
        "home-default",
    );
    write_config_resolution_fixture(&relative_xdg.join("lector/init.lua"), "relative-xdg");
    assert_resolved_config(
        &artifact_dir,
        "relative-xdg",
        None,
        None,
        Some(relative_xdg_value),
        relative_home,
        "home-default",
    );

    let legacy_home = artifact_dir.join("legacy/home");
    write_config_resolution_fixture(
        &legacy_home.join("Library/Application Support/lector/init.lua"),
        "legacy",
    );
    assert_resolved_config(
        &artifact_dir,
        "legacy-fallback",
        None,
        None,
        None,
        legacy_home,
        "legacy",
    );

    let preferred_home = artifact_dir.join("preferred/home");
    write_config_resolution_fixture(
        &preferred_home.join(".config/lector/init.lua"),
        "home-default",
    );
    write_config_resolution_fixture(
        &preferred_home.join("Library/Application Support/lector/init.lua"),
        "legacy",
    );
    assert_resolved_config(
        &artifact_dir,
        "preferred-before-legacy",
        None,
        None,
        None,
        preferred_home,
        "home-default",
    );

    fs::remove_dir_all(&artifact_dir).expect("remove config resolution artifacts");
}

#[test]
fn no_config_skips_the_default_init_lua() {
    let _serial = serialize_live_pty_test();
    let artifact_dir = fixture("target/test-artifacts/live-pty");
    fs::create_dir_all(&artifact_dir).expect("create live PTY artifact directory");
    let unique = format!(
        "no-config-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system clock after Unix epoch")
            .as_nanos()
    );
    let config = artifact_dir.join(format!("{unique}.lua"));
    let marker = artifact_dir.join(format!("{unique}.loaded"));
    fs::write(
        &config,
        r#"
            local marker = assert(io.open(os.getenv("LECTOR_NO_CONFIG_MARKER"), "w"))
            marker:write("loaded")
            marker:close()
            error("--no-config unexpectedly loaded the default init.lua")
        "#,
    )
    .expect("write default init.lua sentinel");

    {
        let mut lector = LiveLector::spawn_without_config(
            &fixture("tests/fixtures/pty/latency-child"),
            &config,
            &marker,
        );
        assert!(
            lector.wait_for_physical_terminal(Duration::from_secs(5), |terminal| {
                physical_screen_contains(terminal, LATENCY_READY)
            }),
            "--no-config did not reach the child terminal; output={:?}",
            String::from_utf8_lossy(&lector.output)
        );
        assert!(
            !marker.exists(),
            "--no-config loaded the init.lua selected by LECTOR_CONFIG"
        );
        lector.send(b"q");
        assert!(
            lector.finish(Duration::from_secs(3)),
            "--no-config Lector did not exit"
        );
    }

    fs::remove_file(&config).expect("remove sentinel init.lua");
}

#[test]
fn ordinary_tmux_key_to_pixel_latency_stays_interactive() {
    let _serial = serialize_live_pty_test();
    assert_live_key_to_pixel_latency(
        &fixture("tests/fixtures/pty/latency-tmux"),
        "ordinary tmux client",
        false,
    );
}

#[test]
fn fresh_ordinary_tmux_client_loads_lectors_sync_capability() {
    let _serial = serialize_live_pty_test();
    let mut lector = LiveLector::spawn(&fixture("tests/fixtures/pty/tmux-sync-info"), false);
    assert!(
        lector.wait_for_physical_terminal(Duration::from_secs(5), |terminal| {
            physical_screen_contains(terminal, TMUX_SYNC_READY)
        }),
        "fresh ordinary tmux client did not report Lector's Sync capability; screen={:?}; output={:?}",
        lector
            .physical_terminal
            .snapshot()
            .rows
            .iter()
            .map(|row| row.text())
            .collect::<Vec<_>>(),
        String::from_utf8_lossy(&lector.output)
    );

    lector.send(b"q");
    assert!(
        lector.finish(Duration::from_secs(5)),
        "fresh ordinary tmux Sync fixture did not exit"
    );
}

#[test]
fn tmux_control_mode_key_to_pixel_latency_stays_interactive() {
    let _serial = serialize_live_pty_test();
    assert_live_key_to_pixel_latency(
        &fixture("tests/fixtures/pty/latency-tmux-control"),
        "tmux control mode",
        true,
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
#[ignore = "manual stress test requiring the maintainer's Bash, fzf, and Neovim setup"]
fn actual_bash_fzf_and_neovim_behave_under_repeated_interactive_input() {
    let _serial = serialize_live_pty_test();
    let mut lector = LiveLector::spawn_with_config(
        Path::new("/bin/bash"),
        &fixture("tests/fixtures/pty/proc-speech-suppress.lua"),
    );
    lector.pump_for(Duration::from_secs(2));

    let mut unexpected_fzf_speech = Vec::new();
    for iteration in 0..100 {
        lector.send(b"\x15");
        lector.pump_for(Duration::from_millis(10));
        lector.clear_speech_logs();
        lector.send(b"\x12");
        lector.pump_for(Duration::from_millis(if iteration == 0 {
            500
        } else {
            80 + iteration % 41
        }));
        let speech = lector
            .outer_speech()
            .lines()
            .filter_map(|line| serde_json::from_str::<String>(line).ok())
            .collect::<Vec<_>>();
        if speech.as_slice() != [" greater "] {
            unexpected_fzf_speech.push((iteration, speech));
        }
        lector.clear_speech_logs();
        lector.send(b"\x1b");
        lector.pump_for(Duration::from_millis(150));
        lector.clear_speech_logs();
    }
    eprintln!("unexpected fzf speech samples: {unexpected_fzf_speech:#?}");

    drop(lector);
    let mut lector = LiveLector::spawn_with_config(
        Path::new("/bin/bash"),
        &fixture("tests/fixtures/pty/proc-speech-suppress.lua"),
    );
    lector.pump_for(Duration::from_secs(2));

    let mut alternate_screen_launch_speech = Vec::new();
    let mut alternate_screen_return_speech = Vec::new();
    for iteration in 0..20 {
        lector.send(b"\x15");
        lector.clear_speech_logs();
        lector.send(b"nvim ~/.bashrc\r");
        assert!(
            lector.wait_for_physical_terminal(Duration::from_secs(5), |terminal| {
                physical_screen_contains(terminal, "shellcheck shell=bash")
            }),
            "Neovim did not show .bashrc on iteration {iteration}"
        );
        lector.pump_for(Duration::from_millis(300));
        alternate_screen_launch_speech.push(lector.outer_speech());

        lector.send(b"\x1b");
        lector.pump_for(Duration::from_millis(100));
        lector.clear_speech_logs();
        lector.send(b":q!\r");
        lector.pump_for(Duration::from_millis(300));
        alternate_screen_return_speech.push(lector.outer_speech());
        lector.clear_speech_logs();
    }
    eprintln!("Neovim .bashrc launch speech: {alternate_screen_launch_speech:#?}");
    eprintln!("Neovim .bashrc return speech: {alternate_screen_return_speech:#?}");
    let expected_launch_speech = alternate_screen_launch_speech
        .first()
        .expect("at least one alternate-screen launch");
    assert!(
        expected_launch_speech.contains("shellcheck shell equals bash"),
        "Neovim did not read the .bashrc cursor line"
    );
    assert!(
        alternate_screen_launch_speech
            .iter()
            .all(|speech| speech == expected_launch_speech),
        "Neovim alternate-screen launches produced inconsistent speech"
    );
    assert!(
        alternate_screen_return_speech.iter().all(String::is_empty),
        "Neovim alternate-screen returns spoke restored or typed content"
    );

    lector.send(b"\x15");
    for byte in b"nvim --clean" {
        lector.send(&[*byte]);
    }
    lector.send(b"\r");
    assert!(
        lector.wait_for_physical_terminal(Duration::from_secs(5), |terminal| {
            physical_screen_contains(terminal, "[No Name]")
        }),
        "Neovim did not become visible; screen={:?}",
        lector
            .physical_terminal
            .snapshot()
            .rows
            .iter()
            .map(|row| row.text())
            .collect::<Vec<_>>()
    );
    lector.pump_for(Duration::from_secs(1));
    lector.clear_speech_logs();
    lector.send_now(b"a");
    // Keep the first inserted character in the same unsettled input/output
    // window as the append command. This is the intermittent case that used
    // to announce the initial `T` while the alternate-screen redraw arrived.
    lector.send_now(b"T");
    const WORDS: &[u8] = b"his is a test alpha beta gamma delta ";
    for iteration in 0..3_200 {
        let byte = WORDS[iteration % WORDS.len()];
        lector.send_now(&[byte]);
        if iteration < 1_200 {
            lector.pump_for(Duration::from_millis(3 + (iteration % 7) as u64));
        } else if iteration % 13 == 0 {
            lector.pump_for(Duration::from_millis((iteration % 5) as u64));
        }
    }
    lector.pump_for(Duration::from_secs(2));
    let neovim_speech = lector.outer_speech();
    eprintln!("Neovim speech: {neovim_speech:?}");

    lector.send(b"\x1b");
    lector.pump_for(Duration::from_millis(100));
    lector.send(b":q!\r");
    lector.pump_for(Duration::from_secs(1));
    lector.send(b"exit\r");
    assert!(lector.finish(Duration::from_secs(5)), "Lector did not exit");

    assert!(
        unexpected_fzf_speech.is_empty(),
        "fzf did not consistently read only its application-cursor character"
    );
    assert!(
        neovim_speech.is_empty(),
        "Neovim spoke while suppressing typed-key echo"
    );
}

#[test]
#[ignore = "manual characterization requiring the maintainer's Neovim setup"]
fn actual_neovim_separates_insert_mode_status_from_first_echo() {
    let _serial = serialize_live_pty_test();
    let mut lector = LiveLector::spawn_with_config(
        Path::new("/bin/bash"),
        &fixture("tests/fixtures/pty/proc-speech-suppress.lua"),
    );
    lector.pump_for(Duration::from_secs(2));
    lector.clear_speech_logs();
    lector.send(b"nvim\r");
    assert!(
        lector.wait_for_physical_terminal(Duration::from_secs(5), |terminal| {
            physical_screen_contains(terminal, "[No Name]")
        }),
        "Neovim did not become visible"
    );
    lector.pump_for(Duration::from_secs(1));
    for iteration in 0..40 {
        lector.clear_speech_logs();
        lector.send(b"a");
        assert!(
            lector.wait_for_physical_terminal(Duration::from_secs(1), |terminal| {
                physical_screen_contains(terminal, "i [No Name]")
            }),
            "insert indicator did not become visible on iteration {iteration}"
        );
        lector.pump_for(Duration::from_millis(60 + (iteration % 7) * 17));
        lector.send(b"T");
        lector.pump_for(Duration::from_millis(250));
        let speech = lector
            .outer_speech()
            .lines()
            .filter_map(|line| serde_json::from_str::<String>(line).ok())
            .collect::<Vec<_>>();
        assert_eq!(speech.as_slice(), ["i"], "iteration {iteration} speech");

        lector.send(b"\x1b");
        lector.pump_for(Duration::from_millis(100));
        lector.send(b"u");
        lector.pump_for(Duration::from_millis(100));
    }

    lector.send(b":q!\r");
    lector.pump_for(Duration::from_millis(300));
    lector.send(b"exit\r");
    assert!(lector.finish(Duration::from_secs(5)), "Lector did not exit");
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
