#![cfg(unix)]

use nix::{
    errno::Errno,
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(5);
// This remains below one initialize deadline, so it still proves the signal
// interrupted the stalled RPC rather than waiting for normal timeout recovery.
const SIGNAL_EXIT_TIMEOUT: Duration = Duration::from_secs(2);

struct LiveProcesses {
    lector: Box<dyn Child + Send + Sync>,
    speech_pid: Option<Pid>,
    app_pid: Option<Pid>,
    _master: Box<dyn MasterPty + Send>,
}

impl Drop for LiveProcesses {
    fn drop(&mut self) {
        let _ = self.lector.kill();
        let _ = self.lector.wait();
        if let Some(pid) = self.speech_pid {
            let _ = kill(pid, Signal::SIGKILL);
        }
        if let Some(pid) = self.app_pid {
            let _ = kill(pid, Signal::SIGKILL);
        }
    }
}

fn wait_for_pid(path: &Path, timeout: Duration) -> Pid {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(value) = fs::read_to_string(path)
            && let Ok(pid) = value.trim().parse::<i32>()
        {
            return Pid::from_raw(pid);
        }
        assert!(
            Instant::now() < deadline,
            "startup fixture did not publish its PID at {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_exit(
    child: &mut (dyn Child + Send + Sync),
    timeout: Duration,
) -> Option<portable_pty::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("query Lector status") {
            return Some(status);
        }
        let _remaining = deadline.checked_duration_since(Instant::now())?;
        thread::sleep(Duration::from_millis(5));
    }
}

fn process_exists(pid: Pid) -> bool {
    match kill(pid, None) {
        Ok(()) | Err(Errno::EPERM) => true,
        Err(Errno::ESRCH) => false,
        Err(error) => panic!("query child process {pid}: {error}"),
    }
}

#[test]
fn sigterm_during_stalled_speech_initialize_cleans_up_and_is_reraised() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after Unix epoch")
        .as_nanos();
    let artifact_dir = std::env::temp_dir().join(format!(
        "lector-startup-signal-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir(&artifact_dir).expect("create startup-signal artifact directory");
    let speech_pid_file = artifact_dir.join("speech.pid");
    let app_pid_file = artifact_dir.join("app.pid");

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 1280,
            pixel_height: 816,
        })
        .expect("open physical PTY");
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_lector"));
    command.args([
        "--shell",
        env!("CARGO_BIN_EXE_tmux-control-adversary"),
        "--config",
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/pty/startup-signal.lua")
            .to_str()
            .expect("UTF-8 config fixture"),
    ]);
    command.env("TERM", "xterm-256color");
    command.env(
        "LECTOR_TEST_STARTUP_SIGNAL_SERVER",
        env!("CARGO_BIN_EXE_proc_stub_server"),
    );
    command.env("LECTOR_TEST_STARTUP_SIGNAL_PID_FILE", &speech_pid_file);
    command.env("LECTOR_TEST_BLOCKING_APP_PID_FILE", &app_pid_file);
    let lector = pair
        .slave
        .spawn_command(command)
        .expect("spawn Lector with stalled startup speech server");
    drop(pair.slave);
    let mut live = LiveProcesses {
        lector,
        speech_pid: None,
        app_pid: None,
        _master: pair.master,
    };

    let app_pid = wait_for_pid(&app_pid_file, SERVER_READY_TIMEOUT);
    live.app_pid = Some(app_pid);
    let speech_pid = wait_for_pid(&speech_pid_file, SERVER_READY_TIMEOUT);
    live.speech_pid = Some(speech_pid);
    let lector_pid = Pid::from_raw(
        live.lector
            .process_id()
            .and_then(|pid| i32::try_from(pid).ok())
            .expect("Lector PID fits i32"),
    );
    let signaled_at = Instant::now();
    kill(lector_pid, Signal::SIGTERM).expect("send SIGTERM during initialize");

    let status = wait_for_exit(&mut *live.lector, SIGNAL_EXIT_TIMEOUT)
        .expect("Lector did not exit promptly after startup SIGTERM");
    assert!(
        !status.success(),
        "SIGTERM became a successful exit: {status:?}"
    );
    assert!(
        status.signal().is_some(),
        "Lector did not re-raise SIGTERM after cleanup: {status:?}"
    );
    assert!(
        signaled_at.elapsed() < SIGNAL_EXIT_TIMEOUT,
        "startup SIGTERM cleanup took {:?}",
        signaled_at.elapsed()
    );

    let cleanup_deadline = Instant::now() + Duration::from_secs(1);
    while (process_exists(speech_pid) || process_exists(app_pid))
        && Instant::now() < cleanup_deadline
    {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !process_exists(speech_pid),
        "direct speech child {speech_pid} survived Lector's handled SIGTERM"
    );
    assert!(
        !process_exists(app_pid),
        "direct app child {app_pid} survived Lector's handled SIGTERM"
    );
    live.speech_pid = None;
    live.app_pid = None;
    fs::remove_dir_all(&artifact_dir).expect("remove startup-signal artifacts");
}
