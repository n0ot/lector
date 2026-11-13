use std::{
    sync::{Mutex, MutexGuard, mpsc},
    time::{Duration, Instant},
};

static REAL_TMUX_TEST_LOCK: Mutex<()> = Mutex::new(());
// PTY reads have no protocol-level boundary. Bound each fixture phase by
// elapsed time so arbitrary stream fragmentation cannot consume its budget.
const REAL_TMUX_PHASE_TIMEOUT: Duration = Duration::from_secs(10);
const REAL_TMUX_PROGRESS_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, Eq, PartialEq)]
enum RealTmuxPhaseError {
    Deadline,
    Step(mpsc::RecvTimeoutError),
}

// Each step feeds optional PTY bytes, advances timers, drains commands, then
// observes its predicate. A receive wakeup is not the phase or idle deadline.
fn drive_real_tmux_phase(
    receiver: &mpsc::Receiver<Vec<u8>>,
    idle_timeout: Option<Duration>,
    step: impl FnMut(Option<Vec<u8>>) -> Result<bool, mpsc::RecvTimeoutError>,
) -> Result<(), RealTmuxPhaseError> {
    drive_real_tmux_phase_with_clock(
        REAL_TMUX_PHASE_TIMEOUT,
        idle_timeout,
        Instant::now,
        |wait| receiver.recv_timeout(wait),
        step,
    )
}

fn drive_real_tmux_phase_with_clock(
    timeout: Duration,
    idle_timeout: Option<Duration>,
    mut now: impl FnMut() -> Instant,
    mut receive: impl FnMut(Duration) -> Result<Vec<u8>, mpsc::RecvTimeoutError>,
    mut step: impl FnMut(Option<Vec<u8>>) -> Result<bool, mpsc::RecvTimeoutError>,
) -> Result<(), RealTmuxPhaseError> {
    let start = now();
    let deadline = start + timeout;
    let mut idle_deadline = idle_timeout.map(|timeout| start + timeout);
    // Drive pending commands and timers even when the peer has nothing to send.
    if step(None).map_err(RealTmuxPhaseError::Step)? {
        return Ok(());
    }
    loop {
        let current = now();
        if current >= deadline {
            return Err(RealTmuxPhaseError::Deadline);
        }
        let mut wait = deadline
            .duration_since(current)
            .min(REAL_TMUX_PROGRESS_INTERVAL);
        if let Some(idle_deadline) = idle_deadline {
            if current >= idle_deadline {
                return Err(RealTmuxPhaseError::Step(mpsc::RecvTimeoutError::Timeout));
            }
            wait = wait.min(idle_deadline.duration_since(current));
        }
        let chunk = match receive(wait) {
            Ok(chunk) => {
                idle_deadline = idle_timeout.map(|timeout| now() + timeout);
                Some(chunk)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(error) => return Err(RealTmuxPhaseError::Step(error)),
        };
        if now() >= deadline {
            return Err(RealTmuxPhaseError::Deadline);
        }
        if step(chunk).map_err(RealTmuxPhaseError::Step)? {
            return Ok(());
        }
    }
}

// Fixture mutations use a separate tmux client. Their replies must never
// enter the App-owned control stream, whose replies are correlated FIFO.
fn run_real_tmux_fixture_command(socket: &std::path::Path, args: &[&str]) -> String {
    use std::process::{Command, Stdio};
    let mut child = Command::new("tmux")
        .arg("-S")
        .arg(socket)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start fixture tmux client");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
            status => {
                let _ = child.kill();
                let output = child.wait_with_output();
                panic!(
                    "fixture tmux client failed or exceeded its deadline: {args:?}; status={status:?}; output={output:?}"
                );
            }
        }
    }
    let output = child.wait_with_output().expect("reap fixture tmux client");
    assert!(
        output.status.success(),
        "fixture tmux command {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("fixture tmux output is UTF-8")
}

fn real_tmux_fixture_client(socket: &std::path::Path) -> String {
    let output = run_real_tmux_fixture_command(socket, &["list-clients", "-F", "#{client_name}"]);
    let clients = output.lines().collect::<Vec<_>>();
    assert_eq!(
        clients.len(),
        1,
        "fixture requires one explicit control client"
    );
    clients[0].to_owned()
}

fn serialize_real_tmux_test() -> MutexGuard<'static, ()> {
    assert_eq!(
        std::env::var_os("LECTOR_REAL_TMUX_CONTAINER").as_deref(),
        Some(std::ffi::OsStr::new("1")),
        "real tmux tests must run through scripts/test-real-tmux-docker"
    );
    REAL_TMUX_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
fn tmux_phase_drives_pending_work_without_incoming_bytes() {
    let start = Instant::now();
    let elapsed = std::cell::Cell::new(Duration::ZERO);
    let ticks = std::cell::Cell::new(0);
    let receives = std::cell::Cell::new(0);
    let result = drive_real_tmux_phase_with_clock(
        Duration::from_secs(1),
        None,
        || start + elapsed.get(),
        |wait| {
            // An idle peer does not wake us. Only the driver's receive budget
            // lets a pending timer expire and its command become observable.
            receives.set(receives.get() + 1);
            elapsed.set(elapsed.get() + wait);
            Err(mpsc::RecvTimeoutError::Timeout)
        },
        |chunk| {
            assert_eq!(chunk, None);
            ticks.set(ticks.get() + 1);
            Ok(elapsed.get() >= Duration::from_millis(30))
        },
    );
    assert_eq!(result, Ok(()));
    assert_eq!(receives.get(), 3);
    assert_eq!(ticks.get(), 4);
}

#[test]
fn tmux_phase_budget_depends_on_time_not_stream_fragment_count() {
    let start = Instant::now();
    let mut fragments = 0;
    let result = drive_real_tmux_phase_with_clock(
        Duration::from_secs(1),
        None,
        || start,
        |_| Ok(vec![0]),
        |chunk| {
            if chunk.is_some() {
                fragments += 1;
            }
            Ok(fragments == 10_000)
        },
    );
    assert_eq!(result, Ok(()));
    assert_eq!(fragments, 10_000);
}

#[test]
fn tmux_phase_keeps_a_hard_elapsed_time_bound_when_idle_or_receiving() {
    for incoming in [false, true] {
        let start = Instant::now();
        let elapsed = std::cell::Cell::new(Duration::ZERO);
        let mut steps = 0;
        let result = drive_real_tmux_phase_with_clock(
            Duration::from_millis(25),
            None,
            || start + elapsed.get(),
            |wait| {
                elapsed.set(elapsed.get() + wait);
                if incoming {
                    Ok(vec![0])
                } else {
                    Err(mpsc::RecvTimeoutError::Timeout)
                }
            },
            |_| {
                steps += 1;
                Ok(false)
            },
        );
        assert_eq!(result, Err(RealTmuxPhaseError::Deadline));
        assert_eq!(elapsed.get(), Duration::from_millis(25));
        assert_eq!(steps, 3);
    }
}

#[test]
fn tmux_phase_preserves_receive_shutdown_and_step_errors() {
    let start = Instant::now();
    let mut steps = 0;
    let result = drive_real_tmux_phase_with_clock(
        Duration::from_secs(1),
        None,
        || start,
        |_| Err(mpsc::RecvTimeoutError::Disconnected),
        |_| {
            steps += 1;
            Ok(false)
        },
    );
    assert_eq!(
        result,
        Err(RealTmuxPhaseError::Step(
            mpsc::RecvTimeoutError::Disconnected
        ))
    );
    assert_eq!(steps, 1);

    let result = drive_real_tmux_phase_with_clock(
        Duration::from_secs(1),
        None,
        || start,
        |_| panic!("a failed step must stop before receiving"),
        |_| Err(mpsc::RecvTimeoutError::Timeout),
    );
    assert_eq!(
        result,
        Err(RealTmuxPhaseError::Step(mpsc::RecvTimeoutError::Timeout))
    );
}

#[test]
fn tmux_phase_idle_deadline_is_bounded_and_reset_only_by_bytes() {
    let start = Instant::now();
    let elapsed = std::cell::Cell::new(Duration::ZERO);
    let mut waits = Vec::new();
    let mut receives = 0;
    let result = drive_real_tmux_phase_with_clock(
        Duration::from_secs(1),
        Some(Duration::from_millis(15)),
        || start + elapsed.get(),
        |wait| {
            waits.push(wait);
            elapsed.set(elapsed.get() + wait);
            receives += 1;
            if receives == 1 {
                Ok(vec![0])
            } else {
                Err(mpsc::RecvTimeoutError::Timeout)
            }
        },
        |_| Ok(false),
    );
    assert_eq!(
        result,
        Err(RealTmuxPhaseError::Step(mpsc::RecvTimeoutError::Timeout))
    );
    assert_eq!(elapsed.get(), Duration::from_millis(25));
    assert_eq!(
        waits,
        [
            Duration::from_millis(10),
            Duration::from_millis(10),
            Duration::from_millis(5)
        ]
    );
}

#[path = "tmux_bells.rs"]
mod tmux_bells;
#[path = "tmux_completion.rs"]
mod tmux_completion;
#[path = "tmux_connections.rs"]
mod tmux_connections;
#[path = "tmux_control_fuzz.rs"]
mod tmux_control_fuzz;
#[path = "tmux_control_parser.rs"]
mod tmux_control_parser;
#[path = "tmux_gateway.rs"]
mod tmux_gateway;
#[path = "tmux_input.rs"]
mod tmux_input;
#[path = "tmux_interaction.rs"]
mod tmux_interaction;
#[path = "tmux_lifecycle.rs"]
mod tmux_lifecycle;
#[path = "tmux_nested.rs"]
mod tmux_nested;
#[path = "tmux_panes.rs"]
mod tmux_panes;
#[path = "tmux_prefix.rs"]
mod tmux_prefix;
#[path = "tmux_recovery.rs"]
mod tmux_recovery;
#[path = "tmux_topology.rs"]
mod tmux_topology;
