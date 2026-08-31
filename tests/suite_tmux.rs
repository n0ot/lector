use std::{
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant},
};

static REAL_TMUX_TEST_LOCK: Mutex<()> = Mutex::new(());
// PTY reads have no protocol-level boundary. Bound each fixture phase by
// elapsed time so arbitrary stream fragmentation cannot consume its budget.
const REAL_TMUX_PHASE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Eq, PartialEq)]
enum RealTmuxPhaseError<E> {
    Deadline,
    Step(E),
}

fn drive_real_tmux_phase<E>(
    step: impl FnMut(Duration) -> Result<bool, E>,
) -> Result<(), RealTmuxPhaseError<E>> {
    drive_real_tmux_phase_with_clock(REAL_TMUX_PHASE_TIMEOUT, Instant::now, step)
}

fn drive_real_tmux_phase_with_clock<E>(
    timeout: Duration,
    mut now: impl FnMut() -> Instant,
    mut step: impl FnMut(Duration) -> Result<bool, E>,
) -> Result<(), RealTmuxPhaseError<E>> {
    let deadline = now() + timeout;
    loop {
        let current = now();
        if current >= deadline {
            return Err(RealTmuxPhaseError::Deadline);
        }
        let remaining = deadline.duration_since(current);
        if step(remaining).map_err(RealTmuxPhaseError::Step)? {
            return Ok(());
        }
    }
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
fn tmux_phase_budget_depends_on_time_not_stream_fragment_count() {
    let start = Instant::now();
    let fragments = std::cell::Cell::new(0_usize);
    let result = drive_real_tmux_phase_with_clock(
        Duration::from_secs(1),
        || start,
        |_| {
            fragments.set(fragments.get() + 1);
            Ok::<_, ()>(fragments.get() == 10_000)
        },
    );

    assert_eq!(result, Ok(()));
    assert_eq!(fragments.get(), 10_000);
}

#[test]
fn tmux_phase_keeps_a_hard_elapsed_time_bound() {
    let start = Instant::now();
    let clock_reads = std::cell::Cell::new(0_usize);
    let steps = std::cell::Cell::new(0_usize);
    let result = drive_real_tmux_phase_with_clock(
        Duration::from_secs(1),
        || {
            let elapsed = clock_reads.replace(clock_reads.get() + 1);
            start + Duration::from_secs(elapsed as u64)
        },
        |_| {
            steps.set(steps.get() + 1);
            Ok::<_, ()>(false)
        },
    );

    assert_eq!(result, Err(RealTmuxPhaseError::Deadline));
    assert_eq!(steps.get(), 0);
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
