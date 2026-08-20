use anyhow::{Context, Result, anyhow};
use clap::Parser;
use lector::{
    app, diagnostics, lua,
    presentation::PhysicalTerminalLifecycle,
    pty,
    screen_reader::ScreenReader,
    speech,
    terminal_protocol::{
        CapabilityOverrides, PhysicalTerminalProfile, ShutdownFenceBroker, TerminfoCapabilities,
    },
    views,
};
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::sys::termios;
use signal_hook::consts::signal::*;
use signal_hook_mio::v1_0::Signals;
use std::{
    io::{ErrorKind, Read, Write},
    os::fd::{AsFd, AsRawFd, RawFd},
    sync::Arc,
    thread, time,
};

const FOCUS_EVENTS_QUERY: &[u8] = b"\x1B[?1004$p";
const STDOUT_WRITABLE_RETRY_INTERVAL: time::Duration = time::Duration::from_millis(10);
const SHUTDOWN_FENCE_TIMEOUT: time::Duration = time::Duration::from_millis(1_000);
const SHUTDOWN_INPUT_SETTLE_TIME: time::Duration = time::Duration::from_millis(50);

const STARTUP_SIGNAL_TOKEN: mio::Token = mio::Token(0);
const STARTUP_SIGNAL_CANCEL_TOKEN: mio::Token = mio::Token(1);

struct StartupSignalMonitor {
    cancel: Arc<mio::Waker>,
    worker: thread::JoinHandle<Result<StartupSignalObservation>>,
}

struct StartupSignalObservation {
    signals: Signals,
    termination_signal: Option<i32>,
}

impl StartupSignalMonitor {
    fn start(
        mut signals: Signals,
        speech_supervisor: speech::supervisor::SupervisorHandle,
    ) -> Result<Self> {
        let mut poll = mio::Poll::new().context("create startup-signal poll")?;
        poll.registry()
            .register(&mut signals, STARTUP_SIGNAL_TOKEN, mio::Interest::READABLE)
            .context("register startup signals")?;
        let cancel = Arc::new(
            mio::Waker::new(poll.registry(), STARTUP_SIGNAL_CANCEL_TOKEN)
                .context("create startup-signal cancellation waker")?,
        );
        let worker = thread::Builder::new()
            .name("lector-startup-signals".to_owned())
            .spawn(move || {
                let mut events = mio::Events::with_capacity(4);
                loop {
                    poll.poll(&mut events, None)
                        .or_else(|error| {
                            if error.kind() == ErrorKind::Interrupted {
                                events.clear();
                                Ok(())
                            } else {
                                Err(error)
                            }
                        })
                        .context("poll startup signals")?;

                    // Signal delivery wins a race with cancellation. This keeps
                    // a SIGTERM received just as the handshake completes from
                    // being silently converted into ordinary startup.
                    if let Some(signal) = signals.pending().find(|signal| {
                        terminal_signal_action(*signal).is_some_and(|action| {
                            matches!(action, TerminalSignalAction::Shutdown(_))
                        })
                    }) {
                        speech_supervisor.terminate();
                        poll.registry()
                            .deregister(&mut signals)
                            .context("deregister startup signals after termination")?;
                        return Ok(StartupSignalObservation {
                            signals,
                            termination_signal: Some(signal),
                        });
                    }
                    if events
                        .iter()
                        .any(|event| event.token() == STARTUP_SIGNAL_CANCEL_TOKEN)
                    {
                        poll.registry()
                            .deregister(&mut signals)
                            .context("deregister startup signals after handshake")?;
                        return Ok(StartupSignalObservation {
                            signals,
                            termination_signal: None,
                        });
                    }
                }
            })
            .context("start startup-signal monitor")?;
        Ok(Self { cancel, worker })
    }

    fn finish(self) -> Result<StartupSignalObservation> {
        let wake_result = self.cancel.wake().context("wake startup-signal monitor");
        let observation = self
            .worker
            .join()
            .map_err(|_| anyhow!("startup-signal monitor panicked"))??;
        wake_result?;
        Ok(observation)
    }
}

enum StartupSpeechOutcome {
    Ready(Signals),
    Terminated(i32),
}

fn start_configured_speech(
    sr: &mut ScreenReader,
    spec: speech::SpeechServerSpec,
    speech_supervisor: &speech::supervisor::SupervisorHandle,
) -> Result<StartupSpeechOutcome> {
    // Install the termination handlers before the worker can spawn or block in
    // an initialize RPC. The monitor waits on signal-pipe readiness rather than
    // introducing a polling cadence into either startup or the terminal loop.
    let signals = Signals::new([SIGHUP, SIGINT, SIGQUIT, SIGTERM])
        .context("install startup termination signals")?;
    let monitor = StartupSignalMonitor::start(signals, speech_supervisor.clone())?;
    let startup_result = (|| -> Result<()> {
        sr.configure_speech_server(spec)?;
        sr.start_speech()
            .context("start configured speech server")?;
        Ok(())
    })();
    let mut observation = monitor.finish()?;

    // Cover a signal which reached the pipe after the monitor observed its
    // cancellation but before its Poll was dropped.
    if observation.termination_signal.is_none()
        && let Some(signal) = observation.signals.pending().find(|signal| {
            terminal_signal_action(*signal)
                .is_some_and(|action| matches!(action, TerminalSignalAction::Shutdown(_)))
        })
    {
        speech_supervisor.terminate();
        observation.termination_signal = Some(signal);
    }
    if let Some(signal) = observation.termination_signal {
        return Ok(StartupSpeechOutcome::Terminated(signal));
    }

    startup_result?;
    for signal in [SIGWINCH, SIGTSTP, SIGCONT] {
        observation
            .signals
            .add_signal(signal)
            .with_context(|| format!("install terminal signal {signal}"))?;
    }
    Ok(StartupSpeechOutcome::Ready(observation.signals))
}

fn stdout_retry_poll_timeout(
    current: Option<time::Duration>,
    stdout_registered: bool,
) -> Option<time::Duration> {
    if !stdout_registered {
        return current;
    }
    Some(current.map_or(STDOUT_WRITABLE_RETRY_INTERVAL, |timeout| {
        timeout.min(STDOUT_WRITABLE_RETRY_INTERVAL)
    }))
}

/// Add the one-shot silent-child deadline to the current wait. Once it has
/// elapsed, consume it exactly once: an unblocked loop gets one immediate turn
/// to run `on_startup`, while stdout backpressure retains its bounded retry
/// interval instead of spinning on a permanently expired zero timeout.
fn startup_deadline_poll_timeout(
    current: Option<time::Duration>,
    deadline: &mut Option<time::Instant>,
    now: time::Instant,
    stdout_backpressured: bool,
) -> Option<time::Duration> {
    let Some(at) = *deadline else {
        return current;
    };
    let Some(remaining) = at
        .checked_duration_since(now)
        .filter(|wait| !wait.is_zero())
    else {
        *deadline = None;
        return if stdout_backpressured {
            current
        } else {
            Some(time::Duration::ZERO)
        };
    };
    Some(current.map_or(remaining, |timeout| timeout.min(remaining)))
}

struct NonblockingFdGuard {
    fd: RawFd,
    original: OFlag,
    restored: bool,
}

impl NonblockingFdGuard {
    fn enable(fd: RawFd) -> Result<Self> {
        let original = OFlag::from_bits_truncate(
            fcntl(fd, FcntlArg::F_GETFL).context("read descriptor flags")?,
        );
        fcntl(fd, FcntlArg::F_SETFL(original | OFlag::O_NONBLOCK))
            .context("make descriptor nonblocking")?;
        Ok(Self {
            fd,
            original,
            restored: false,
        })
    }

    fn restore(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        fcntl(self.fd, FcntlArg::F_SETFL(self.original)).context("restore descriptor flags")?;
        self.restored = true;
        Ok(())
    }

    fn enable_again(&mut self) -> Result<()> {
        if !self.restored {
            return Ok(());
        }
        fcntl(
            self.fd,
            FcntlArg::F_SETFL(self.original | OFlag::O_NONBLOCK),
        )
        .context("make descriptor nonblocking after resume")?;
        self.restored = false;
        Ok(())
    }
}

impl Drop for NonblockingFdGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

struct EmergencyTerminalGuard {
    initial_term_attrs: termios::Termios,
    focus_was_enabled: Option<bool>,
    armed: bool,
}

impl EmergencyTerminalGuard {
    fn new(initial_term_attrs: &termios::Termios) -> Self {
        Self {
            initial_term_attrs: initial_term_attrs.clone(),
            focus_was_enabled: None,
            armed: false,
        }
    }

    fn arm(&mut self) {
        self.armed = true;
    }

    fn set_prior_focus_mode(&mut self, focus_was_enabled: Option<bool>) {
        self.focus_was_enabled = focus_was_enabled;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for EmergencyTerminalGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(&emergency_terminal_cleanup_bytes(self.focus_was_enabled));
        let _ = stdout.flush();
        let _ = termios::tcsetattr(
            std::io::stdin().as_fd(),
            termios::SetArg::TCSADRAIN,
            &self.initial_term_attrs,
        );
    }
}

fn emergency_terminal_cleanup_bytes(focus_was_enabled: Option<bool>) -> Vec<u8> {
    let mut lifecycle = PhysicalTerminalLifecycle::new(focus_was_enabled);
    let _ = lifecycle.activate();
    lifecycle.shutdown().bytes
}

#[derive(Debug, Default, Eq, PartialEq)]
struct FocusModeQueryResult {
    was_enabled: Option<bool>,
    pending_input: Vec<u8>,
}

fn query_focus_mode<R: Read + AsRawFd>(
    stdin: &mut R,
    stdout: &mut dyn Write,
) -> FocusModeQueryResult {
    if stdout.write_all(FOCUS_EVENTS_QUERY).is_err() || stdout.flush().is_err() {
        return FocusModeQueryResult::default();
    }

    let Ok(mut poll) = mio::Poll::new() else {
        return FocusModeQueryResult::default();
    };
    let stdin_fd = stdin.as_raw_fd();
    let mut source = mio::unix::SourceFd(&stdin_fd);
    if poll
        .registry()
        .register(&mut source, mio::Token(0), mio::Interest::READABLE)
        .is_err()
    {
        return FocusModeQueryResult::default();
    }

    let mut events = mio::Events::with_capacity(8);
    let mut response = Vec::new();
    for timeout_ms in [20, 10, 10] {
        if poll
            .poll(&mut events, Some(time::Duration::from_millis(timeout_ms)))
            .is_err()
        {
            return FocusModeQueryResult {
                was_enabled: None,
                pending_input: response,
            };
        }
        if events.is_empty() {
            continue;
        }
        let mut buf = [0u8; 256];
        match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                response.extend_from_slice(&buf[..n]);
                if parse_focus_mode_report(&response).is_some() {
                    return separate_focus_mode_report_input(&response);
                }
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => continue,
            Err(_) => {
                return FocusModeQueryResult {
                    was_enabled: None,
                    pending_input: response,
                };
            }
        }
    }
    separate_focus_mode_report_input(&response)
}

fn parse_focus_mode_report(buf: &[u8]) -> Option<bool> {
    locate_focus_mode_report(buf).map(|(_, _, enabled)| enabled)
}

fn locate_focus_mode_report(buf: &[u8]) -> Option<(usize, usize, bool)> {
    let prefix = b"\x1B[?1004;";
    let mut i = 0usize;
    while i < buf.len() {
        if !buf[i..].starts_with(prefix) {
            i += 1;
            continue;
        }
        let start = i + prefix.len();
        let mut end = start;
        while end < buf.len() && buf[end].is_ascii_digit() {
            end += 1;
        }
        if end <= start || end + 1 >= buf.len() || buf[end] != b'$' || buf[end + 1] != b'y' {
            i += 1;
            continue;
        }
        let Some(code) = std::str::from_utf8(&buf[start..end])
            .ok()
            .and_then(|digits| digits.parse::<u8>().ok())
        else {
            i += 1;
            continue;
        };
        let enabled = match code {
            1 | 3 => true,
            2 | 4 => false,
            _ => {
                i += 1;
                continue;
            }
        };
        return Some((i, end + 2, enabled));
    }
    None
}

/// Remove every complete focus-mode response while preserving all user input
/// which the ownership query happened to read in the same chunks. The first
/// valid report defines the inherited mode; later duplicate replies are still
/// protocol traffic and are not forwarded to the child.
fn separate_focus_mode_report_input(buf: &[u8]) -> FocusModeQueryResult {
    let mut remaining = buf;
    let mut pending_input = Vec::with_capacity(buf.len());
    let mut was_enabled = None;
    while let Some((start, end, enabled)) = locate_focus_mode_report(remaining) {
        pending_input.extend_from_slice(&remaining[..start]);
        was_enabled.get_or_insert(enabled);
        remaining = &remaining[end..];
    }
    pending_input.extend_from_slice(remaining);
    FocusModeQueryResult {
        was_enabled,
        pending_input,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShutdownFenceOutcome {
    Reply,
    InputEof,
    TimedOut,
}

/// Drain the bytes covered by one readable notification through a shutdown
/// fence reply. Reads are deliberately one byte wide: after the matching final
/// byte, any subsequent user input remains queued for the shell which regains
/// the terminal.
fn drain_shutdown_fence_input<R: Read>(
    stdin: &mut R,
    broker: &mut ShutdownFenceBroker,
) -> Result<Option<ShutdownFenceOutcome>> {
    loop {
        let mut byte = [0_u8; 1];
        match stdin.read(&mut byte) {
            Ok(0) => return Ok(Some(ShutdownFenceOutcome::InputEof)),
            Ok(_) if broker.ingest_byte(byte[0]) => {
                return Ok(Some(ShutdownFenceOutcome::Reply));
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error).context("read terminal input for shutdown fence"),
        }
    }
}

/// Retain raw-input ownership briefly after the fence response. PTYs may make
/// bytes from one terminal write readable in separate scheduling turns, so a
/// Kitty release queued alongside the DA1 reply can otherwise arrive just
/// after the final input flush and be echoed by the restored shell.
fn drain_shutdown_handoff_input<R: Read>(
    stdin: &mut R,
    poll: &mut mio::Poll,
    events: &mut mio::Events,
    settle_time: time::Duration,
) -> Result<()> {
    let deadline = time::Instant::now() + settle_time;
    loop {
        let Some(remaining) = deadline.checked_duration_since(time::Instant::now()) else {
            return Ok(());
        };
        events.clear();
        match poll.poll(events, Some(remaining)) {
            Ok(()) if events.is_empty() => return Ok(()),
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("poll terminal input during shutdown handoff"),
        }

        let mut input = [0_u8; 256];
        loop {
            match stdin.read(&mut input) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => {
                    return Err(error).context("drain terminal input during shutdown handoff");
                }
            }
        }
    }
}

/// Consume terminal input through the response to a freshly emitted DA1
/// request. The descriptor is temporarily nonblocking so one readiness edge
/// can be drained completely. Unsupported terminals are bounded by `timeout`.
fn wait_for_shutdown_fence<R: Read + AsRawFd>(
    stdin: &mut R,
    replies_to_ignore: usize,
    timeout: time::Duration,
) -> Result<ShutdownFenceOutcome> {
    let mut broker = ShutdownFenceBroker::new(replies_to_ignore);
    let mut poll = mio::Poll::new().context("create shutdown-fence poll")?;
    let stdin_fd = stdin.as_raw_fd();
    let _nonblocking_input = NonblockingFdGuard::enable(stdin_fd)
        .context("make terminal input nonblocking for shutdown fence")?;
    let mut source = mio::unix::SourceFd(&stdin_fd);
    poll.registry()
        .register(&mut source, mio::Token(0), mio::Interest::READABLE)
        .context("register terminal input for shutdown fence")?;
    let mut events = mio::Events::with_capacity(8);
    let deadline = time::Instant::now() + timeout;

    loop {
        let Some(remaining) = deadline.checked_duration_since(time::Instant::now()) else {
            return Ok(ShutdownFenceOutcome::TimedOut);
        };
        events.clear();
        match poll.poll(&mut events, Some(remaining)) {
            Ok(()) if events.is_empty() => return Ok(ShutdownFenceOutcome::TimedOut),
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("poll terminal input for shutdown fence"),
        }

        if let Some(outcome) = drain_shutdown_fence_input(stdin, &mut broker)? {
            if outcome == ShutdownFenceOutcome::Reply {
                drain_shutdown_handoff_input(
                    stdin,
                    &mut poll,
                    &mut events,
                    SHUTDOWN_INPUT_SETTLE_TIME,
                )?;
            }
            return Ok(outcome);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputDrainState {
    Open { drain_again: bool },
    Eof,
}

impl InputDrainState {
    const fn is_eof(self) -> bool {
        matches!(self, Self::Eof)
    }

    const fn drain_again(self) -> bool {
        matches!(self, Self::Open { drain_again: true })
    }
}

const INPUT_READ_BUFFER_BYTES: usize = 8 * 1024;
const INPUT_DRAIN_BUDGET_BYTES: usize = 32 * 1024;
const INPUT_DRAIN_BUDGET_TIME: time::Duration = time::Duration::from_millis(4);

/// Drain one bounded turn of terminal input after an edge-triggered readiness
/// notification. The immediate continuation flag is essential: stopping at a
/// fairness budget does not guarantee that the poller will emit another edge.
fn drain_available_input<R, F>(reader: &mut R, mut consume: F) -> Result<InputDrainState>
where
    R: Read + ?Sized,
    F: FnMut(&[u8]) -> Result<()>,
{
    let mut drained_bytes = 0_usize;
    let started = time::Instant::now();
    let mut buffer = [0_u8; INPUT_READ_BUFFER_BYTES];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(InputDrainState::Eof),
            Ok(count) => {
                consume(&buffer[..count])?;
                drained_bytes = drained_bytes.saturating_add(count);
                if drained_bytes >= INPUT_DRAIN_BUDGET_BYTES
                    || started.elapsed() >= INPUT_DRAIN_BUDGET_TIME
                {
                    return Ok(InputDrainState::Open { drain_again: true });
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                return Ok(InputDrainState::Open { drain_again: false });
            }
            Err(error) => return Err(error).context("read terminal input"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PtyDrainState {
    Open {
        received_output: bool,
        drain_again: bool,
    },
    Eof {
        received_output: bool,
    },
}

impl PtyDrainState {
    const fn is_eof(self) -> bool {
        matches!(self, Self::Eof { .. })
    }

    const fn drain_again(self) -> bool {
        matches!(
            self,
            Self::Open {
                drain_again: true,
                ..
            }
        )
    }

    const fn received_output(self) -> bool {
        match self {
            Self::Open {
                received_output, ..
            }
            | Self::Eof { received_output } => received_output,
        }
    }
}

const PTY_READ_BUFFER_BYTES: usize = 8 * 1024;
const PTY_DRAIN_BUDGET_BYTES: usize = 32 * 1024;
const PTY_DRAIN_BUDGET_TIME: time::Duration = time::Duration::from_millis(4);
const STARTUP_PTY_DRAIN_MAX_TURNS: usize = 4;
const STARTUP_PTY_DRAIN_MAX_TIME: time::Duration = time::Duration::from_millis(8);
const STARTUP_SILENT_CHILD_TIMEOUT: time::Duration = time::Duration::from_millis(50);

/// Consume a bounded turn of currently available nonblocking PTY data. The
/// callback receives the stream after each read so protocol replies can be
/// returned before parsing a later, potentially expensive chunk. Reaching the
/// byte or elapsed-time budget requests an immediate later turn. Accessibility
/// reads use the last physically presented snapshot while any newer render is
/// pending, so an open frame never receives a larger budget at the expense of
/// user input fairness.
fn drain_available_pty<R, F>(reader: &mut R, mut consume: F) -> Result<PtyDrainState>
where
    R: Read + ?Sized,
    F: FnMut(&[u8], &mut R) -> Result<()>,
{
    let mut received_output = false;
    let mut drained_bytes = 0_usize;
    let started = time::Instant::now();
    let mut buffer = [0_u8; PTY_READ_BUFFER_BYTES];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(PtyDrainState::Eof { received_output }),
            Ok(count) => {
                received_output = true;
                consume(&buffer[..count], reader)?;
                drained_bytes = drained_bytes.saturating_add(count);
                if drained_bytes >= PTY_DRAIN_BUDGET_BYTES
                    || started.elapsed() >= PTY_DRAIN_BUDGET_TIME
                {
                    return Ok(PtyDrainState::Open {
                        received_output,
                        drain_again: true,
                    });
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                return Ok(PtyDrainState::Open {
                    received_output,
                    drain_again: false,
                });
            }
            // BSD PTY masters report EIO instead of a zero-length read after
            // the final slave descriptor closes.
            Err(error) if error.raw_os_error() == Some(nix::errno::Errno::EIO as i32) => {
                return Ok(PtyDrainState::Eof { received_output });
            }
            Err(error) => return Err(error).context("read child PTY output"),
        }
    }
}

/// Drain one fair PTY turn while coalescing visible tmux pane damage into one
/// presentation. Parsing, model mutation, and protocol replies remain ordered
/// per read; only the expensive scene composition is deferred to the boundary.
fn drain_application_pty<R>(
    app: &mut app::App,
    sr: &mut ScreenReader,
    reader: &mut R,
    term_out: &mut dyn Write,
) -> Result<PtyDrainState>
where
    R: Read + Write,
{
    app.begin_pty_presentation_batch();
    let result = drain_available_pty(reader, |bytes, stream| {
        app.handle_pty(sr, bytes, term_out)?;
        app.flush_application_replies(stream)?;
        Ok(())
    });
    match result {
        Ok(state) => {
            app.finish_pty_presentation_batch(sr, term_out)?;
            Ok(state)
        }
        Err(error) => {
            app.cancel_pty_presentation_batch();
            Err(error)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalSignalAction {
    Resize,
    Suspend,
    Resume,
    Shutdown(i32),
}

fn terminal_signal_action(signal: i32) -> Option<TerminalSignalAction> {
    match signal {
        SIGWINCH => Some(TerminalSignalAction::Resize),
        SIGTSTP => Some(TerminalSignalAction::Suspend),
        SIGCONT => Some(TerminalSignalAction::Resume),
        SIGHUP | SIGINT | SIGQUIT | SIGTERM => Some(TerminalSignalAction::Shutdown(signal)),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SetupFailureAction {
    ShowConfigurationError,
    ReturnRuntimeError,
}

const fn setup_failure_action(event_loop_started: bool) -> SetupFailureAction {
    if event_loop_started {
        SetupFailureAction::ReturnRuntimeError
    } else {
        SetupFailureAction::ShowConfigurationError
    }
}

fn requested_config_path(
    cli_config: Option<std::path::PathBuf>,
    no_config: bool,
    environment_config: Option<std::ffi::OsString>,
) -> Option<std::path::PathBuf> {
    cli_config.or_else(|| {
        (!no_config)
            .then_some(environment_config)
            .flatten()
            .map(std::path::PathBuf::from)
    })
}

#[cfg(test)]
mod tests {
    use super::{
        Cli, INPUT_DRAIN_BUDGET_BYTES, INPUT_READ_BUFFER_BYTES, InputDrainState,
        NonblockingFdGuard, PTY_DRAIN_BUDGET_BYTES, PTY_READ_BUFFER_BYTES, PtyDrainState,
        SetupFailureAction, ShutdownFenceBroker, ShutdownFenceOutcome, TerminalSignalAction,
        drain_available_input, drain_available_pty, drain_shutdown_fence_input,
        emergency_terminal_cleanup_bytes, parse_focus_mode_report, requested_config_path,
        separate_focus_mode_report_input, setup_failure_action, startup_deadline_poll_timeout,
        stdout_retry_poll_timeout, terminal_signal_action, wait_for_shutdown_fence,
    };
    use clap::{CommandFactory, Parser};
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    use signal_hook::consts::signal::*;
    use std::{
        collections::VecDeque,
        io::{self, Read, Write},
        os::fd::AsRawFd,
        thread,
        time::{Duration, Instant},
    };

    #[test]
    fn registered_stdout_gets_a_bounded_retry_timeout() {
        assert_eq!(stdout_retry_poll_timeout(None, false), None);
        assert_eq!(
            stdout_retry_poll_timeout(Some(Duration::from_millis(30)), true),
            Some(Duration::from_millis(10))
        );
        assert_eq!(
            stdout_retry_poll_timeout(Some(Duration::from_millis(4)), true),
            Some(Duration::from_millis(4))
        );
    }

    #[test]
    fn expired_startup_deadline_does_not_spin_while_stdout_is_backpressured() {
        let now = Instant::now();
        let mut deadline = Some(now - Duration::from_millis(1));
        let timeout = startup_deadline_poll_timeout(None, &mut deadline, now, true);

        assert_eq!(deadline, None);
        assert_eq!(timeout, None);
        assert_eq!(
            stdout_retry_poll_timeout(timeout, true),
            Some(Duration::from_millis(10))
        );
    }

    #[test]
    fn future_startup_deadline_is_a_one_shot_poll_wait() {
        let now = Instant::now();
        let mut deadline = Some(now + Duration::from_millis(50));

        assert_eq!(
            startup_deadline_poll_timeout(None, &mut deadline, now, false),
            Some(Duration::from_millis(50))
        );
        assert!(deadline.is_some());
    }

    enum ReadStep {
        Bytes(Vec<u8>),
        Error(io::ErrorKind),
        Eof,
    }

    struct ScriptedPtyReader {
        steps: VecDeque<ReadStep>,
    }

    impl ScriptedPtyReader {
        fn new(steps: impl IntoIterator<Item = ReadStep>) -> Self {
            Self {
                steps: steps.into_iter().collect(),
            }
        }
    }

    impl Read for ScriptedPtyReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.steps.pop_front().expect("scripted PTY read step") {
                ReadStep::Bytes(bytes) => {
                    assert!(bytes.len() <= buf.len());
                    buf[..bytes.len()].copy_from_slice(&bytes);
                    Ok(bytes.len())
                }
                ReadStep::Error(kind) => Err(io::Error::from(kind)),
                ReadStep::Eof => Ok(0),
            }
        }
    }

    #[test]
    fn stdin_drain_consumes_every_ready_chunk_before_waiting_again() {
        let mut reader = ScriptedPtyReader::new([
            ReadStep::Bytes(b"first".to_vec()),
            ReadStep::Error(io::ErrorKind::Interrupted),
            ReadStep::Bytes(b"second".to_vec()),
            ReadStep::Error(io::ErrorKind::WouldBlock),
        ]);
        let mut input = Vec::new();

        let state = drain_available_input(&mut reader, |bytes| {
            input.extend_from_slice(bytes);
            Ok(())
        })
        .expect("drain available terminal input");

        assert_eq!(state, InputDrainState::Open { drain_again: false });
        assert_eq!(input, b"firstsecond");
    }

    #[test]
    fn stdin_drain_yields_at_a_bounded_budget_and_resumes_without_a_new_edge() {
        let mut steps = (0..INPUT_DRAIN_BUDGET_BYTES / INPUT_READ_BUFFER_BYTES + 1)
            .map(|_| ReadStep::Bytes(vec![b'x'; INPUT_READ_BUFFER_BYTES]))
            .collect::<Vec<_>>();
        steps.push(ReadStep::Error(io::ErrorKind::WouldBlock));
        let mut reader = ScriptedPtyReader::new(steps);
        let mut input = Vec::new();

        let first = drain_available_input(&mut reader, |bytes| {
            input.extend_from_slice(bytes);
            Ok(())
        })
        .expect("drain one fair terminal-input turn");
        assert_eq!(first, InputDrainState::Open { drain_again: true });
        assert_eq!(input.len(), INPUT_DRAIN_BUDGET_BYTES);

        let second = drain_available_input(&mut reader, |bytes| {
            input.extend_from_slice(bytes);
            Ok(())
        })
        .expect("resume terminal-input drain without another readiness edge");
        assert_eq!(second, InputDrainState::Open { drain_again: false });
        assert_eq!(
            input.len(),
            INPUT_DRAIN_BUDGET_BYTES + INPUT_READ_BUFFER_BYTES
        );
    }

    #[test]
    fn pty_drain_consumes_every_ready_chunk_before_waiting_again() {
        let mut reader = ScriptedPtyReader::new([
            ReadStep::Bytes(b"first".to_vec()),
            ReadStep::Error(io::ErrorKind::Interrupted),
            ReadStep::Bytes(b"second".to_vec()),
            ReadStep::Error(io::ErrorKind::WouldBlock),
        ]);
        let mut output = Vec::new();

        let state = drain_available_pty(&mut reader, |bytes, _reader| {
            output.extend_from_slice(bytes);
            Ok(())
        })
        .expect("drain available PTY output");

        assert_eq!(
            state,
            PtyDrainState::Open {
                received_output: true,
                drain_again: false,
            }
        );
        assert_eq!(output, b"firstsecond");
    }

    #[test]
    fn pty_drain_yields_at_a_bounded_budget_and_resumes_without_a_new_edge() {
        let mut steps = (0..PTY_DRAIN_BUDGET_BYTES / PTY_READ_BUFFER_BYTES + 1)
            .map(|_| ReadStep::Bytes(vec![b'x'; PTY_READ_BUFFER_BYTES]))
            .collect::<Vec<_>>();
        steps.push(ReadStep::Error(io::ErrorKind::WouldBlock));
        let mut reader = ScriptedPtyReader::new(steps);
        let mut output = Vec::new();

        let first = drain_available_pty(&mut reader, |bytes, _reader| {
            output.extend_from_slice(bytes);
            Ok(())
        })
        .expect("drain one fair PTY turn");
        assert_eq!(
            first,
            PtyDrainState::Open {
                received_output: true,
                drain_again: true,
            }
        );
        assert_eq!(output.len(), PTY_DRAIN_BUDGET_BYTES);

        let second = drain_available_pty(&mut reader, |bytes, _reader| {
            output.extend_from_slice(bytes);
            Ok(())
        })
        .expect("resume PTY drain without another readiness edge");
        assert_eq!(
            second,
            PtyDrainState::Open {
                received_output: true,
                drain_again: false,
            }
        );
        assert_eq!(output.len(), PTY_DRAIN_BUDGET_BYTES + PTY_READ_BUFFER_BYTES);
    }

    #[test]
    fn pty_drain_yields_when_processing_is_expensive_before_the_byte_budget() {
        let mut reader = ScriptedPtyReader::new([
            ReadStep::Bytes(b"x".to_vec()),
            ReadStep::Bytes(b"later".to_vec()),
            ReadStep::Error(io::ErrorKind::WouldBlock),
        ]);
        let mut output = Vec::new();
        let state = drain_available_pty(&mut reader, |bytes, _reader| {
            output.extend_from_slice(bytes);
            thread::sleep(Duration::from_millis(6));
            Ok(())
        })
        .expect("yield an expensive PTY turn");

        assert_eq!(
            state,
            PtyDrainState::Open {
                received_output: true,
                drain_again: true,
            }
        );
        assert_eq!(output, b"x");
    }

    #[test]
    fn pty_drain_does_not_expand_its_budget_for_synchronized_output() {
        let mut reader = ScriptedPtyReader::new([
            ReadStep::Bytes(b"x".to_vec()),
            ReadStep::Bytes(b"frame-end".to_vec()),
            ReadStep::Error(io::ErrorKind::WouldBlock),
        ]);
        let mut output = Vec::new();
        let mut chunks = 0;
        let state = drain_available_pty(&mut reader, |bytes, _reader| {
            output.extend_from_slice(bytes);
            chunks += 1;
            if chunks == 1 {
                thread::sleep(Duration::from_millis(6));
            }
            Ok(())
        })
        .expect("yield during synchronized PTY output");

        assert_eq!(
            state,
            PtyDrainState::Open {
                received_output: true,
                drain_again: true,
            }
        );
        assert_eq!(output, b"x");
    }

    #[test]
    fn pty_drain_reports_eof_only_after_delivering_preceding_output() {
        let mut reader =
            ScriptedPtyReader::new([ReadStep::Bytes(b"final-frame\x07".to_vec()), ReadStep::Eof]);
        let mut output = Vec::new();

        let state = drain_available_pty(&mut reader, |bytes, _reader| {
            output.extend_from_slice(bytes);
            Ok(())
        })
        .expect("drain final PTY output");

        assert_eq!(
            state,
            PtyDrainState::Eof {
                received_output: true
            }
        );
        assert_eq!(output, b"final-frame\x07");
    }

    #[test]
    fn compositor_is_mandatory_and_has_no_runtime_mode_flag() {
        Cli::try_parse_from(["lector", "--shell", "/bin/sh"]).expect("parse default CLI");
        assert!(Cli::try_parse_from(["lector", "--shell", "/bin/sh", "--full-renderer"]).is_err());
        let help = Cli::command().render_long_help().to_string();
        assert!(!help.contains("full-renderer"), "{help}");
        assert!(!help.contains("LECTOR_FULL_RENDERER"), "{help}");
    }

    #[test]
    fn internal_native_speech_host_does_not_require_a_terminal_shell() {
        let cli = Cli::try_parse_from(["lector", "--native-speech-server"])
            .expect("parse internal native speech host");
        assert!(cli.native_speech_server);
        assert!(
            !Cli::command()
                .render_long_help()
                .to_string()
                .contains("native-speech-server")
        );
    }

    #[test]
    fn lua_config_flags_are_explicit_and_mutually_exclusive() {
        let configured = Cli::try_parse_from([
            "lector",
            "--shell",
            "/bin/sh",
            "--config",
            "/tmp/lector.lua",
        ])
        .expect("parse explicit Lua config");
        assert_eq!(
            configured.config.as_deref(),
            Some(std::path::Path::new("/tmp/lector.lua"))
        );
        assert!(!configured.no_config);

        let defaults = Cli::try_parse_from(["lector", "--shell", "/bin/sh", "--no-config"])
            .expect("parse no-config mode");
        assert!(defaults.no_config);
        assert!(defaults.config.is_none());

        assert!(
            Cli::try_parse_from([
                "lector",
                "--shell",
                "/bin/sh",
                "--config",
                "/tmp/lector.lua",
                "--no-config",
            ])
            .is_err()
        );
    }

    #[test]
    fn no_config_overrides_the_environment_fallback_but_not_cli_validation() {
        let environment = Some(std::ffi::OsString::from("/broken/from-environment.lua"));
        assert_eq!(
            requested_config_path(None, false, environment.clone()),
            Some(std::path::PathBuf::from("/broken/from-environment.lua"))
        );
        assert_eq!(requested_config_path(None, true, environment), None);
        assert_eq!(
            requested_config_path(
                Some(std::path::PathBuf::from("/explicit.lua")),
                false,
                Some(std::ffi::OsString::from("/environment.lua")),
            ),
            Some(std::path::PathBuf::from("/explicit.lua"))
        );
    }

    #[test]
    fn obsolete_public_speech_process_flags_are_rejected() {
        for arguments in [
            ["lector", "--shell", "/bin/sh", "--speech-driver", "proc"],
            [
                "lector",
                "--shell",
                "/bin/sh",
                "--speech-server",
                "/tmp/server",
            ],
        ] {
            assert!(Cli::try_parse_from(arguments).is_err());
        }
        let help = Cli::command().render_long_help().to_string();
        assert!(!help.contains("speech-driver"), "{help}");
        assert!(!help.contains("speech-server"), "{help}");
    }

    #[test]
    fn parse_focus_mode_report_enabled_code_1() {
        assert_eq!(parse_focus_mode_report(b"\x1B[?1004;1$y"), Some(true));
    }

    #[test]
    fn parse_focus_mode_report_enabled_code_3() {
        assert_eq!(parse_focus_mode_report(b"xx\x1B[?1004;3$yyy"), Some(true));
    }

    #[test]
    fn parse_focus_mode_report_disabled_code_2() {
        assert_eq!(parse_focus_mode_report(b"\x1B[?1004;2$y"), Some(false));
    }

    #[test]
    fn parse_focus_mode_report_disabled_code_4() {
        assert_eq!(parse_focus_mode_report(b"\x1B[?1004;4$y"), Some(false));
    }

    #[test]
    fn parse_focus_mode_report_none_for_invalid_or_missing() {
        assert_eq!(parse_focus_mode_report(b""), None);
        assert_eq!(parse_focus_mode_report(b"\x1B[?1004$p"), None);
        assert_eq!(parse_focus_mode_report(b"\x1B[?1004;9$y"), None);
    }

    #[test]
    fn focus_query_preserves_input_around_and_between_terminal_reports() {
        let result =
            separate_focus_mode_report_input(b"before\x1B[?1004;1$ymiddle\x1B[?1004;3$yafter");

        assert_eq!(result.was_enabled, Some(true));
        assert_eq!(result.pending_input, b"beforemiddleafter");
    }

    #[test]
    fn focus_query_preserves_incomplete_or_invalid_report_bytes() {
        for input in [
            b"key\x1B[?1004;1$".as_slice(),
            b"key\x1B[?1004;9$y".as_slice(),
        ] {
            let result = separate_focus_mode_report_input(input);
            assert_eq!(result.was_enabled, None);
            assert_eq!(result.pending_input, input);
        }
    }

    #[test]
    fn shutdown_fence_drains_late_focus_and_stale_da1_before_fresh_reply() {
        let (read, write) = nix::unistd::pipe().expect("create shutdown-fence pipe");
        let writer = thread::spawn(move || {
            let mut write = std::fs::File::from(write);
            write
                .write_all(b"\x1b[I\x1b[?6cnoise\x1b[?62;22;52c")
                .expect("write terminal replies");
        });
        let mut read = std::fs::File::from(read);

        assert_eq!(
            wait_for_shutdown_fence(&mut read, 1, Duration::from_millis(100))
                .expect("wait for shutdown fence"),
            ShutdownFenceOutcome::Reply
        );
        writer.join().expect("join terminal writer");
    }

    #[test]
    fn shutdown_handoff_drains_a_kitty_release_delivered_after_the_fence_reply() {
        let (read, write) = nix::unistd::pipe().expect("create shutdown-handoff pipe");
        let writer = thread::spawn(move || {
            let mut write = std::fs::File::from(write);
            write
                .write_all(b"\x1b[?62;22;52c")
                .expect("write shutdown fence reply");
            write.flush().expect("flush shutdown fence reply");
            thread::sleep(Duration::from_millis(5));
            write
                .write_all(b"\x1b[99;5:3u")
                .expect("write delayed Kitty release");
        });
        let mut read = std::fs::File::from(read);

        assert_eq!(
            wait_for_shutdown_fence(&mut read, 0, Duration::from_millis(100))
                .expect("wait through shutdown handoff"),
            ShutdownFenceOutcome::Reply
        );
        writer.join().expect("join terminal writer");
        let mut remaining = Vec::new();
        read.read_to_end(&mut remaining)
            .expect("read bytes after shutdown handoff");
        assert!(remaining.is_empty(), "leaked terminal input: {remaining:?}");
    }

    #[test]
    fn shutdown_fence_drains_one_readiness_edge_without_eating_following_input() {
        let mut steps = b"\x1b[?62;22;52c"
            .iter()
            .map(|byte| ReadStep::Bytes(vec![*byte]))
            .collect::<Vec<_>>();
        steps.push(ReadStep::Bytes(b"x".to_vec()));
        let mut reader = ScriptedPtyReader::new(steps);
        let mut broker = ShutdownFenceBroker::new(0);

        assert_eq!(
            drain_shutdown_fence_input(&mut reader, &mut broker)
                .expect("drain one shutdown-fence readiness edge"),
            Some(ShutdownFenceOutcome::Reply)
        );
        assert_eq!(reader.steps.len(), 1, "post-fence input must remain unread");
    }

    #[test]
    fn shutdown_fence_times_out_when_terminal_does_not_support_da1() {
        let (read, _write) = nix::unistd::pipe().expect("create shutdown-fence pipe");
        let mut read = std::fs::File::from(read);

        assert_eq!(
            wait_for_shutdown_fence(&mut read, 0, Duration::from_millis(10))
                .expect("wait for bounded shutdown fence"),
            ShutdownFenceOutcome::TimedOut
        );
    }

    #[test]
    fn nonblocking_output_guard_sets_and_restores_the_original_descriptor_flags() {
        let (_read, write) = nix::unistd::pipe().expect("create pipe");
        let original = OFlag::from_bits_truncate(
            fcntl(write.as_raw_fd(), FcntlArg::F_GETFL).expect("read original flags"),
        );
        {
            let _guard =
                NonblockingFdGuard::enable(write.as_raw_fd()).expect("enable nonblocking output");
            let active = OFlag::from_bits_truncate(
                fcntl(write.as_raw_fd(), FcntlArg::F_GETFL).expect("read active flags"),
            );
            assert!(active.contains(OFlag::O_NONBLOCK));
        }
        let restored = OFlag::from_bits_truncate(
            fcntl(write.as_raw_fd(), FcntlArg::F_GETFL).expect("read restored flags"),
        );
        assert_eq!(restored, original);
    }

    #[test]
    fn nonblocking_output_guard_can_be_reenabled_after_a_suspend_boundary() {
        let (_read, write) = nix::unistd::pipe().expect("create pipe");
        let original = OFlag::from_bits_truncate(
            fcntl(write.as_raw_fd(), FcntlArg::F_GETFL).expect("read original flags"),
        );
        let mut guard =
            NonblockingFdGuard::enable(write.as_raw_fd()).expect("enable nonblocking output");
        guard.restore().expect("restore for suspend");
        assert_eq!(
            OFlag::from_bits_truncate(
                fcntl(write.as_raw_fd(), FcntlArg::F_GETFL).expect("read suspended flags")
            ),
            original
        );

        guard.enable_again().expect("reenable after resume");
        let resumed = OFlag::from_bits_truncate(
            fcntl(write.as_raw_fd(), FcntlArg::F_GETFL).expect("read resumed flags"),
        );
        assert!(resumed.contains(OFlag::O_NONBLOCK));

        guard.restore().expect("restore after resumed run");
        assert_eq!(
            OFlag::from_bits_truncate(
                fcntl(write.as_raw_fd(), FcntlArg::F_GETFL).expect("read final flags")
            ),
            original
        );
    }

    #[test]
    fn terminal_signals_have_explicit_lifecycle_actions() {
        assert_eq!(
            terminal_signal_action(SIGWINCH),
            Some(TerminalSignalAction::Resize)
        );
        assert_eq!(
            terminal_signal_action(SIGTSTP),
            Some(TerminalSignalAction::Suspend)
        );
        assert_eq!(
            terminal_signal_action(SIGCONT),
            Some(TerminalSignalAction::Resume)
        );
        for signal in [SIGHUP, SIGINT, SIGQUIT, SIGTERM] {
            assert_eq!(
                terminal_signal_action(signal),
                Some(TerminalSignalAction::Shutdown(signal))
            );
        }
        assert_eq!(terminal_signal_action(SIGUSR1), None);
    }

    #[test]
    fn panic_fallback_cleanup_resets_every_owned_terminal_mode() {
        let cleanup = emergency_terminal_cleanup_bytes(None);
        for reset in [
            b"\x1b[?2026l".as_slice(),
            b"\x1b[0m",
            b"\x1b]8;;\x1b\\",
            b"\x1b>",
            b"\x1b[?1l",
            b"\x1b[?2004l",
            b"\x1b[?1000l",
            b"\x1b[?1002l",
            b"\x1b[?1003l",
            b"\x1b[?1005l",
            b"\x1b[?1006l",
            b"\x1b[=0u",
            b"\x1b[?25h",
            b"\x1b[?1004l",
        ] {
            assert!(
                cleanup.windows(reset.len()).any(|window| window == reset),
                "missing emergency reset {reset:?}"
            );
        }
        assert!(cleanup.ends_with(b"\x1b[?1049l"));

        let cleanup_preserving_focus = emergency_terminal_cleanup_bytes(Some(true));
        assert!(
            !cleanup_preserving_focus
                .windows(b"\x1b[?1004l".len())
                .any(|window| window == b"\x1b[?1004l")
        );
        assert!(cleanup_preserving_focus.ends_with(b"\x1b[?1049l"));
    }

    #[test]
    fn only_configuration_failures_start_the_error_overlay_event_loop() {
        assert_eq!(
            setup_failure_action(false),
            SetupFailureAction::ShowConfigurationError
        );
        assert_eq!(
            setup_failure_action(true),
            SetupFailureAction::ReturnRuntimeError
        );
    }
}

#[derive(Parser)]
#[clap(author, version, about)]
struct Cli {
    /// Lector will spawn this shell when it starts
    #[clap(
        long,
        short = 's',
        env,
        required_unless_present = "native_speech_server"
    )]
    shell: Option<std::path::PathBuf>,
    /// Load Lua configuration from this file
    #[clap(long, conflicts_with = "no_config")]
    config: Option<std::path::PathBuf>,
    /// Start with defaults instead of loading Lua configuration
    #[clap(long, conflicts_with = "config")]
    no_config: bool,
    /// Enable debug logging
    #[clap(long, env)]
    log: bool,
    /// Write structured debug logging to this file (also enables --log)
    #[clap(long, env = "LECTOR_LOG_FILE")]
    log_file: Option<std::path::PathBuf>,
    /// Run Lector's internal native speech host
    #[clap(long, hide = true)]
    native_speech_server: bool,
    /// Expected parent process for the internal native speech host
    #[clap(long, hide = true, requires = "native_speech_server")]
    native_speech_parent_pid: Option<u32>,
}

struct DiagnosticsShutdownGuard;

impl Drop for DiagnosticsShutdownGuard {
    fn drop(&mut self) {
        diagnostics::shutdown(time::Duration::from_millis(250));
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.native_speech_server {
        return lector::native_tts_server::run(cli.native_speech_parent_pid);
    }
    let shell = cli
        .shell
        .as_ref()
        .ok_or_else(|| anyhow!("--shell is required"))?;
    // Resolve the environment fallback ourselves so an explicit --no-config
    // can recover even when LECTOR_CONFIG points at a broken file. Clap would
    // otherwise treat an env-provided value as conflicting CLI input.
    let configured_path = requested_config_path(
        cli.config.clone(),
        cli.no_config,
        std::env::var_os("LECTOR_CONFIG"),
    );
    let required_config = configured_path.is_some();
    let conf_file = match configured_path {
        Some(path) => path,
        None if cli.no_config => std::path::PathBuf::new(),
        None => {
            let mut path =
                dirs::config_dir().ok_or_else(|| anyhow!("cannot get config directory"))?;
            path.push("lector");
            path.push("init.lua");
            path
        }
    };
    if required_config && !conf_file.is_file() {
        anyhow::bail!("configuration file does not exist: {}", conf_file.display());
    }
    let load_config = !cli.no_config;
    let logging_enabled = cli.log || cli.log_file.is_some();
    let terminal_geometry =
        pty::terminal_geometry(std::io::stdin().as_raw_fd()).context("cannot get terminal size")?;

    // Keep PTY creation ahead of the speech host and anything else that may
    // start threads. Unix PTY launchers still require a small post-fork setup
    // window.
    let init_term_attrs =
        termios::tcgetattr(std::io::stdin().as_fd()).context("read terminal settings")?;
    let virtual_environment = pty::compatible_terminal_environment();
    let mut process = pty::Process::spawn_with_geometry_and_environment(
        shell,
        std::iter::empty::<&str>(),
        terminal_geometry,
        &init_term_attrs,
        Some(&virtual_environment),
    )
    .context("spawn child process")?;

    // portable-pty performs Unix child setup after fork. Do not start even a
    // diagnostics worker until that fork boundary has completed.
    if logging_enabled {
        diagnostics::initialize(cli.log_file.as_deref())?;
        diagnostics::event("main", "startup", &format!("shell={}", shell.display()));
    }
    let diagnostics_shutdown = DiagnosticsShutdownGuard;

    let mut physical_profile = PhysicalTerminalProfile::conservative(terminal_geometry);
    if let Some(term) = std::env::var_os("TERM")
        && let Some(terminfo) = TerminfoCapabilities::detect(&term)
    {
        physical_profile.apply_terminfo(&terminfo);
    }
    if std::env::var("COLORTERM")
        .ok()
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "truecolor" | "24bit"))
    {
        physical_profile.true_color = true;
    }
    let overrides = CapabilityOverrides::from_environment().map_err(anyhow::Error::msg)?;
    physical_profile.apply_overrides(&overrides);

    let supervisor = speech::supervisor::Supervisor::default();
    let speech_supervisor = supervisor.handle();
    let shutdown_supervisor = speech_supervisor.clone();
    let speech_driver: Box<dyn speech::Driver> = Box::new(
        speech::worker::BoundedAsyncDriver::new_with_shutdown(supervisor, move || {
            shutdown_supervisor.terminate();
        })
        .context("start bounded speech worker")?,
    );
    let speech = speech::Speech::new(speech_driver);
    let mut screen_reader = ScreenReader::new(speech);
    let view_stack = views::ViewStack::new(Box::new(views::PtyView::new_with_geometry(
        terminal_geometry,
    )));
    let mut app = app::App::new(view_stack)?;
    app.set_physical_profile(physical_profile);
    app.set_logging(logging_enabled);
    app.enable_output_scheduler(Default::default());

    let mut event_loop_started = false;
    let mut termination_signal = None;
    let setup_result = lua::setup(
        conf_file.clone(),
        load_config,
        &mut screen_reader,
        |screen_reader| {
            event_loop_started = true;
            let spec = screen_reader.speech_server_spec().clone();
            match start_configured_speech(screen_reader, spec, &speech_supervisor)? {
                StartupSpeechOutcome::Ready(signals) => {
                    termination_signal = do_events(
                        screen_reader,
                        &mut app,
                        &mut process,
                        EventStartup {
                            initial_message: None,
                            config_path: Some(&conf_file),
                            signals,
                        },
                        &speech_supervisor,
                        &init_term_attrs,
                    )?;
                }
                StartupSpeechOutcome::Terminated(signal) => {
                    termination_signal = Some(signal);
                }
            }
            Ok(())
        },
    );
    let result = match setup_result {
        Ok(()) => Ok(()),
        Err(err)
            if setup_failure_action(event_loop_started)
                == SetupFailureAction::ShowConfigurationError =>
        {
            // A broken init.lua must still leave the error UI accessible. Drop
            // any partially queued startup announcements, then start the
            // built-in backend using default process selection.
            screen_reader.stop_speaking()?;
            match start_configured_speech(
                &mut screen_reader,
                speech::SpeechServerSpec::Native,
                &speech_supervisor,
            )
            .context("start native speech server for configuration error")?
            {
                StartupSpeechOutcome::Ready(signals) => {
                    screen_reader.commit_speech_reconfiguration(speech::SpeechServerSpec::Native);
                    termination_signal = do_events(
                        &mut screen_reader,
                        &mut app,
                        &mut process,
                        EventStartup {
                            initial_message: Some(format!(
                                "Error loading config file: {}\n\n{}",
                                conf_file.display(),
                                err
                            )),
                            config_path: None,
                            signals,
                        },
                        &speech_supervisor,
                        &init_term_attrs,
                    )?;
                }
                StartupSpeechOutcome::Terminated(signal) => {
                    termination_signal = Some(signal);
                }
            }
            Ok(())
        }
        Err(err) => Err(anyhow!("{err}")),
    };
    // Clean up before returning the above result.
    if let Err(err) = termios::tcsetattr(
        std::io::stdin().as_fd(),
        termios::SetArg::TCSADRAIN,
        &init_term_attrs,
    ) {
        eprintln!("failed to restore terminal settings: {err}");
    }
    screen_reader.shutdown_speech();
    process.terminate();
    let result = result.map_err(|e| anyhow!("{}", e));
    if let Some(signal) = termination_signal {
        // Signal handlers deliberately return here first so owned subprocesses
        // and diagnostics are shut down before restoring the signal's default
        // disposition. A hard SIGKILL cannot run cleanup, which is why the
        // built-in speech helper also has an independent parent watchdog.
        drop(screen_reader);
        drop(process);
        drop(diagnostics_shutdown);
        signal_hook::low_level::emulate_default_handler(signal)
            .with_context(|| format!("terminate lector with signal {signal}"))?;
    }
    result
}

struct EventStartup<'a> {
    initial_message: Option<String>,
    config_path: Option<&'a std::path::Path>,
    signals: Signals,
}

fn do_events(
    sr: &mut ScreenReader,
    app: &mut app::App,
    process: &mut pty::Process,
    startup: EventStartup<'_>,
    speech_supervisor: &speech::supervisor::SupervisorHandle,
    initial_term_attrs: &termios::Termios,
) -> Result<Option<i32>> {
    let EventStartup {
        initial_message,
        config_path: startup_config_path,
        mut signals,
    } = startup;
    // This fallback is deliberately declared before the nonblocking
    // descriptor guards below. Rust drops locals in reverse order, so an
    // unwind restores the descriptor flags before this writes its final reset.
    let mut emergency_terminal = EmergencyTerminalGuard::new(initial_term_attrs);
    let mut pty_stream = process.stream().context("get PTY stream")?;
    let _nonblocking_pty =
        NonblockingFdGuard::enable(pty_stream.as_raw_fd()).context("make child PTY nonblocking")?;
    // Set stdin to raw, so that input is read character by character,
    // and so that signals like SIGINT aren't sent when pressing keys like ^C.
    pty::set_raw(std::io::stdin().as_raw_fd()).context("set STDIN to raw")?;
    emergency_terminal.arm();

    // Set up a mio poll, to select between reading from stdin, and the PTY.
    const STDIN_TOKEN: mio::Token = mio::Token(0);
    const PTY_TOKEN: mio::Token = mio::Token(1);
    const SIGNALS_TOKEN: mio::Token = mio::Token(2);
    const STDOUT_TOKEN: mio::Token = mio::Token(3);
    const SPEECH_TOKEN: mio::Token = mio::Token(4);
    let mut startup_hook_path = if sr.has_on_startup_hook() {
        startup_config_path
            .map(|path| {
                path.to_str()
                    .ok_or_else(|| anyhow!("configuration path is not valid UTF-8"))
                    .map(str::to_owned)
            })
            .transpose()?
    } else {
        None
    };
    let mut poll = mio::Poll::new()?;
    let speech_waker = Arc::new(mio::Waker::new(poll.registry(), SPEECH_TOKEN)?);
    speech_supervisor.set_waker(Arc::clone(&speech_waker));
    let stdin_fd = std::io::stdin().as_raw_fd();
    let mut stdin_source = mio::unix::SourceFd(&stdin_fd);
    let mut stdin_registered = startup_hook_path.is_none();
    if stdin_registered {
        poll.registry()
            .register(&mut stdin_source, STDIN_TOKEN, mio::Interest::READABLE)?;
    }
    let pty_fd = pty_stream.as_raw_fd();
    let mut pty_source = mio::unix::SourceFd(&pty_fd);
    poll.registry()
        .register(&mut pty_source, PTY_TOKEN, mio::Interest::READABLE)?;
    poll.registry()
        .register(&mut signals, SIGNALS_TOKEN, mio::Interest::READABLE)?;

    // Main event loop
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let FocusModeQueryResult {
        was_enabled: focus_was_enabled,
        pending_input: mut startup_buffered_input,
    } = query_focus_mode(&mut stdin, &mut stdout);
    emergency_terminal.set_prior_focus_mode(focus_was_enabled);
    let mut nonblocking_input =
        NonblockingFdGuard::enable(stdin.as_raw_fd()).context("make terminal input nonblocking")?;
    let stdout_fd = stdout.as_raw_fd();
    let mut nonblocking_output =
        NonblockingFdGuard::enable(stdout_fd).context("make terminal output nonblocking")?;
    let mut stdout_source = mio::unix::SourceFd(&stdout_fd);
    let mut stdout_registered = false;
    let mut pty_writable_registered = false;
    let mut events = mio::Events::with_capacity(1024);
    app.configure_physical_terminal(focus_was_enabled);
    app.activate_physical_terminal(&mut stdout)?;
    app.flush_pending_clipboard_writes(sr, &mut stdout)?;
    let mut termination_signal = None;

    let event_result = (|| -> Result<()> {
        app.start_capability_probes(&mut stdout)?;
        let startup_output = app.drain_scheduled_output(&mut stdout, true)?;
        if startup_output.blocked {
            poll.registry()
                .register(&mut stdout_source, STDOUT_TOKEN, mio::Interest::WRITABLE)?;
            stdout_registered = true;
        }
        if let Some(message) = initial_message {
            app.show_message(sr, "Lector Error", &message, &mut stdout)?;
        }
        // Capture the child output which is already available when Lector
        // takes ownership of the physical terminal. Each drain is bounded on
        // its own, and this outer gate prevents a continuously-writing child
        // from postponing the ready hook forever.
        let startup_drain_started = time::Instant::now();
        let mut startup_drain_turns = 1_usize;
        let mut startup_pty = drain_application_pty(app, sr, &mut pty_stream, &mut stdout)?;
        let mut startup_received_pty_output = startup_pty.received_output();
        while startup_pty.drain_again()
            && startup_drain_turns < STARTUP_PTY_DRAIN_MAX_TURNS
            && startup_drain_started.elapsed() < STARTUP_PTY_DRAIN_MAX_TIME
        {
            startup_pty = drain_application_pty(app, sr, &mut pty_stream, &mut stdout)?;
            startup_received_pty_output |= startup_pty.received_output();
            startup_drain_turns = startup_drain_turns.saturating_add(1);
        }
        if startup_pty.is_eof() {
            app.handle_pty_eof(sr, &mut stdout)?;
            drain_scheduled_output_to_boundary(app, &mut stdout)?;
            return Ok(());
        }
        let mut pty_drain_pending = startup_pty.drain_again();
        let mut stdin_drain_pending = !startup_buffered_input.is_empty();
        // A nonblocking read immediately after the terminal handoff commonly
        // races the child's first write. Start the full grace window only
        // after focus negotiation, activation, and that first empty drain have
        // completed. This is a one-shot startup deadline, not a poll cadence.
        let mut startup_silent_child_deadline = startup_hook_path
            .as_ref()
            .filter(|_| !startup_received_pty_output)
            .map(|_| time::Instant::now() + STARTUP_SILENT_CHILD_TIMEOUT);

        // A blocked lifecycle write remains owned by the registered stdout
        // readiness path. Otherwise, attempt the initial compositor flush
        // now. Never force this drain: force would pierce an application's
        // open DEC 2026 transaction and expose a working Neovim frame.
        let startup_frame_output = if stdout_registered {
            None
        } else {
            let report = app.drain_scheduled_output(&mut stdout, false)?;
            if report.blocked {
                poll.registry().register(
                    &mut stdout_source,
                    STDOUT_TOKEN,
                    mio::Interest::WRITABLE,
                )?;
                stdout_registered = true;
            }
            Some(report)
        };
        let startup_child_ready = startup_received_pty_output
            || startup_silent_child_deadline
                .is_none_or(|deadline| time::Instant::now() >= deadline);
        if startup_child_ready && !startup_received_pty_output {
            startup_silent_child_deadline = None;
        }
        if startup_hook_path.is_some()
            && startup_child_ready
            && startup_frame_output.as_ref().is_some_and(|report| {
                !report.blocked
                    && !report.write_budget_exhausted
                    && !app.application_synchronization_holds_output()
            })
        {
            run_pending_startup_hook(
                sr,
                &mut startup_hook_path,
                &poll,
                &mut stdin_source,
                STDIN_TOKEN,
                &mut stdin_registered,
                &mut stdin_drain_pending,
                speech_supervisor,
            )?;
        }
        service_speech_supervisor(sr, speech_supervisor)?;
        loop {
            service_speech_supervisor(sr, speech_supervisor)?;
            let mut effective_poll_timeout: Option<time::Duration> = None;
            if let Some(output_timeout) = app.scheduled_output_timeout() {
                effective_poll_timeout = Some(
                    effective_poll_timeout
                        .map_or(output_timeout, |current| current.min(output_timeout)),
                );
            }
            if startup_hook_path.is_some()
                && !startup_received_pty_output
                && startup_silent_child_deadline.is_some()
            {
                effective_poll_timeout = startup_deadline_poll_timeout(
                    effective_poll_timeout,
                    &mut startup_silent_child_deadline,
                    time::Instant::now(),
                    stdout_registered,
                );
            }
            if app.wants_tick()
                || pty_drain_pending
                || (startup_hook_path.is_none() && stdin_drain_pending)
            {
                effective_poll_timeout = Some(time::Duration::from_millis(0));
            }
            effective_poll_timeout =
                stdout_retry_poll_timeout(effective_poll_timeout, stdout_registered);
            poll.poll(&mut events, effective_poll_timeout)
                .or_else(|e| {
                    if e.kind() == ErrorKind::Interrupted {
                        events.clear();
                        Ok(())
                    } else {
                        Err(e)
                    }
                })?;

            let mut pty_ready = pty_drain_pending;
            pty_drain_pending = false;
            let mut stdin_ready = stdin_drain_pending;
            stdin_drain_pending = false;
            let mut speech_ready = false;
            for event in events.iter() {
                match event.token() {
                    STDIN_TOKEN => stdin_ready = true,
                    PTY_TOKEN => {
                        if event.is_readable() {
                            pty_ready = true;
                        }
                        if event.is_writable() {
                            let report = pty_stream
                                .drain_pending_writes()
                                .context("drain buffered child PTY input")?;
                            diagnostics::event(
                                "event-loop",
                                "pty-write-ready",
                                &format!(
                                    "written={} blocked={} pending={}",
                                    report.bytes_written,
                                    report.blocked,
                                    pty_stream.pending_write_bytes()
                                ),
                            );
                        }
                    }
                    SIGNALS_TOKEN => {
                        for signal in signals.pending() {
                            match terminal_signal_action(signal) {
                                Some(TerminalSignalAction::Resize) => {
                                    let geometry =
                                        pty::terminal_geometry(std::io::stdin().as_raw_fd())?;
                                    process.resize_with_geometry(geometry)?;
                                    app.on_resize_with_geometry(geometry, &mut stdout)?;
                                }
                                Some(TerminalSignalAction::Suspend) => {
                                    app.suspend_physical_terminal(&mut stdout)?;
                                    nonblocking_output.restore()?;
                                    nonblocking_input.restore()?;
                                    drain_scheduled_output_to_boundary(app, &mut stdout)?;
                                    termios::tcsetattr(
                                        std::io::stdin().as_fd(),
                                        termios::SetArg::TCSADRAIN,
                                        initial_term_attrs,
                                    )
                                    .context("restore terminal settings before suspend")?;
                                    signal_hook::low_level::emulate_default_handler(SIGTSTP)
                                        .context("suspend lector")?;

                                    // SIGCONT is necessarily what lets execution return here.
                                    // Reclaim immediately so no input or output is processed
                                    // while the terminal remains in its restored state. The
                                    // queued SIGCONT action below is deliberately idempotent.
                                    pty::set_raw(std::io::stdin().as_raw_fd())
                                        .context("set STDIN to raw after resume")?;
                                    nonblocking_input.enable_again()?;
                                    nonblocking_output.enable_again()?;
                                    app.resume_physical_terminal(&mut stdout)?;
                                }
                                Some(TerminalSignalAction::Resume) => {
                                    pty::set_raw(std::io::stdin().as_raw_fd())
                                        .context("set STDIN to raw after SIGCONT")?;
                                    nonblocking_input.enable_again()?;
                                    nonblocking_output.enable_again()?;
                                    app.resume_physical_terminal(&mut stdout)?;
                                }
                                Some(TerminalSignalAction::Shutdown(signal)) => {
                                    diagnostics::event(
                                        "event-loop",
                                        "shutdown-signal",
                                        &signal.to_string(),
                                    );
                                    termination_signal = Some(signal);
                                    return Ok(());
                                }
                                None => {}
                            }
                        }
                    }
                    STDOUT_TOKEN => app.notify_scheduled_output_writable(),
                    SPEECH_TOKEN => speech_ready = true,
                    _ => unreachable!("encountered unknown event"),
                }
            }

            if speech_ready {
                service_speech_supervisor(sr, speech_supervisor)?;
            }

            if pty_ready {
                let pty = drain_application_pty(app, sr, &mut pty_stream, &mut stdout)?;
                startup_received_pty_output |= pty.received_output();
                pty_drain_pending = pty.drain_again();
                if pty.is_eof() {
                    app.handle_pty_eof(sr, &mut stdout)?;
                    // Present the last complete child frame before lifecycle cleanup
                    // discards obsolete queued work. This keeps a BEL in that work
                    // associated with the child frame instead of shutdown.
                    drain_scheduled_output_to_boundary(app, &mut stdout)?;
                    return Ok(());
                }
            }

            // Apply ready child output before screen-derived input commands.
            // A poll turn may report both descriptors; reviewing the screen
            // first would otherwise observe the state just before that output.
            if stdin_ready && startup_hook_path.is_some() {
                // Physical input remains in the kernel until the initial
                // child frame has crossed its compositor flush boundary. A
                // remembered readiness is serviced immediately after the hook
                // rather than being lost to edge-trigger details.
                stdin_drain_pending = true;
            } else if stdin_ready {
                if !startup_buffered_input.is_empty() {
                    let buffered_input = std::mem::take(&mut startup_buffered_input);
                    app.handle_stdin(sr, &buffered_input, &mut pty_stream, &mut stdout)?;
                }
                let input = drain_available_input(&mut stdin, |bytes| {
                    app.handle_stdin(sr, bytes, &mut pty_stream, &mut stdout)
                })?;
                stdin_drain_pending = input.drain_again();
                if input.is_eof() {
                    return Ok(());
                }
                // Lua input handlers may request a transactional server swap.
                // Enqueue it now; spawning and handshaking remain entirely on
                // the bounded speech worker.
                service_speech_supervisor(sr, speech_supervisor)?;
            }

            if stdout_registered {
                // A nested macOS PTY can accept a partial write, return EAGAIN,
                // and then omit a later kqueue writable edge as its master is
                // drained. Retrying only while writable interest is registered
                // prevents that partial transaction from permanently blocking
                // every newer frame behind it.
                app.notify_scheduled_output_writable();
            }
            app.handle_tick(sr, &mut pty_stream, &mut stdout)?;
            let dropped_pty_bytes = pty_stream.take_dropped_write_bytes();
            if dropped_pty_bytes != 0 {
                diagnostics::event(
                    "event-loop",
                    "pty-write-overflow",
                    &format!(
                        "dropped={dropped_pty_bytes} pending={}",
                        pty_stream.pending_write_bytes()
                    ),
                );
                process.terminate();
            }
            let wants_pty_writable = pty_stream.has_pending_writes();
            if wants_pty_writable != pty_writable_registered {
                let interest = if wants_pty_writable {
                    mio::Interest::READABLE.add(mio::Interest::WRITABLE)
                } else {
                    mio::Interest::READABLE
                };
                poll.registry()
                    .reregister(&mut pty_source, PTY_TOKEN, interest)?;
                pty_writable_registered = wants_pty_writable;
            }
            let output = app.drain_scheduled_output(&mut stdout, false)?;
            if output.blocked || output.write_budget_exhausted {
                diagnostics::event(
                    "event-loop",
                    "physical-output-backpressure",
                    &format!(
                        "written={} blocked={} budget_exhausted={}",
                        output.bytes_written, output.blocked, output.write_budget_exhausted
                    ),
                );
            }
            if output.blocked && !stdout_registered {
                poll.registry().register(
                    &mut stdout_source,
                    STDOUT_TOKEN,
                    mio::Interest::WRITABLE,
                )?;
                stdout_registered = true;
            } else if !output.blocked && stdout_registered {
                poll.registry().deregister(&mut stdout_source)?;
                stdout_registered = false;
            }

            let startup_child_ready = startup_received_pty_output
                || startup_silent_child_deadline
                    .is_none_or(|deadline| time::Instant::now() >= deadline);
            if startup_child_ready && !startup_received_pty_output {
                startup_silent_child_deadline = None;
            }
            if startup_hook_path.is_some()
                && startup_child_ready
                && !output.blocked
                && !output.write_budget_exhausted
                && !app.application_synchronization_holds_output()
            {
                run_pending_startup_hook(
                    sr,
                    &mut startup_hook_path,
                    &poll,
                    &mut stdin_source,
                    STDIN_TOKEN,
                    &mut stdin_registered,
                    &mut stdin_drain_pending,
                    speech_supervisor,
                )?;
            }

            // The App owns stabilization and maximum-delay deadlines. Keeping
            // them with the exact presented revision avoids both a lost wakeup
            // behind a newer parser frame and a permanent 30 ms poll loop
            // after receiving output for a hidden tmux pane.
            app.maybe_finalize_changes(sr)?;
        }
    })();

    let output_restore_result = nonblocking_output.restore();
    let input_restore_result = nonblocking_input.restore();
    let cleanup_result = (|| -> Result<()> {
        let stale_da1_replies = app.outstanding_startup_primary_device_attributes_replies();
        app.begin_physical_terminal_shutdown_fence(&mut stdout)?;
        drain_scheduled_output_to_boundary(app, &mut stdout)?;
        let fence_result =
            wait_for_shutdown_fence(&mut stdin, stale_da1_replies, SHUTDOWN_FENCE_TIMEOUT);
        app.finish_physical_terminal_shutdown_fence(&mut stdout)?;
        drain_scheduled_output_to_boundary(app, &mut stdout)?;
        termios::tcsetattr(
            std::io::stdin().as_fd(),
            // Input generated while Lector still owned Kitty key-release
            // reporting belongs to Lector, even if it raced the final DA1
            // fence. Do not hand that encoded release to the resumed shell.
            termios::SetArg::TCSAFLUSH,
            initial_term_attrs,
        )
        .context("restore terminal settings after event loop")?;
        // A polling or input error must be reported only after the terminal is
        // out of the alternate screen and its termios settings are restored.
        let _ = fence_result?;
        Ok(())
    })();

    if let Err(error) = event_result {
        if let Err(restore_error) = &output_restore_result {
            eprintln!("failed to restore terminal output flags: {restore_error:#}");
        }
        if let Err(restore_error) = &input_restore_result {
            eprintln!("failed to restore terminal input flags: {restore_error:#}");
        }
        if let Err(cleanup_error) = &cleanup_result {
            eprintln!("failed to clean up physical terminal: {cleanup_error:#}");
        }
        if output_restore_result.is_ok() && input_restore_result.is_ok() && cleanup_result.is_ok() {
            emergency_terminal.disarm();
        }
        return Err(error);
    }
    if termination_signal.is_some() {
        // Cleanup failures must not turn a handled termination signal into an
        // ordinary error return. Report them, retain the emergency fallback
        // when needed, and let main tear down owned children before re-raising
        // the original signal.
        if let Err(restore_error) = &output_restore_result {
            eprintln!("failed to restore terminal output flags: {restore_error:#}");
        }
        if let Err(restore_error) = &input_restore_result {
            eprintln!("failed to restore terminal input flags: {restore_error:#}");
        }
        if let Err(cleanup_error) = &cleanup_result {
            eprintln!("failed to clean up physical terminal: {cleanup_error:#}");
        }
        if output_restore_result.is_ok() && input_restore_result.is_ok() && cleanup_result.is_ok() {
            emergency_terminal.disarm();
        }
        return Ok(termination_signal);
    }
    output_restore_result?;
    input_restore_result?;
    cleanup_result?;
    emergency_terminal.disarm();
    Ok(termination_signal)
}

#[allow(clippy::too_many_arguments)]
fn run_pending_startup_hook(
    sr: &mut ScreenReader,
    startup_hook_path: &mut Option<String>,
    poll: &mio::Poll,
    stdin_source: &mut mio::unix::SourceFd<'_>,
    stdin_token: mio::Token,
    stdin_registered: &mut bool,
    stdin_drain_pending: &mut bool,
    speech_supervisor: &speech::supervisor::SupervisorHandle,
) -> Result<()> {
    let Some(config_path) = startup_hook_path.take() else {
        return Ok(());
    };
    sr.hook_on_startup(&config_path)?;
    service_speech_supervisor(sr, speech_supervisor)?;

    // Do not let physical input race the ready hook. Registering a readable
    // descriptor after bytes have arrived is level-safe on mio's supported
    // pollers, and the explicit pending turn also covers an implementation
    // which does not synthesize an immediate edge at registration.
    if !*stdin_registered {
        poll.registry()
            .register(stdin_source, stdin_token, mio::Interest::READABLE)?;
        *stdin_registered = true;
        *stdin_drain_pending = true;
    }
    Ok(())
}

fn service_speech_supervisor(
    sr: &mut ScreenReader,
    supervisor: &speech::supervisor::SupervisorHandle,
) -> Result<()> {
    for event in supervisor.take_events() {
        match event {
            speech::supervisor::SupervisorEvent::Fatal(message) => {
                anyhow::bail!("speech server failed: {message}");
            }
            speech::supervisor::SupervisorEvent::Reconfigured(spec) => {
                sr.commit_speech_reconfiguration(spec);
            }
            speech::supervisor::SupervisorEvent::ReconfigureFailed(message) => {
                sr.hook_on_error(&message, "speech-reconfigure")?;
            }
        }
    }

    if let Some(spec) = sr.take_speech_reconfiguration() {
        sr.configure_speech_server(spec)?;
    }
    Ok(())
}

fn drain_scheduled_output_to_boundary(app: &mut app::App, term_out: &mut dyn Write) -> Result<()> {
    loop {
        let report = app.drain_scheduled_output(term_out, true)?;
        if !report.blocked && !report.write_budget_exhausted {
            return Ok(());
        }
        app.notify_scheduled_output_writable();
    }
}
