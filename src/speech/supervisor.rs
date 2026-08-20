//! Deferred process-speech lifecycle and crash supervision.
//!
//! [`Supervisor`] is deliberately synchronous. It is intended to live below
//! [`super::worker::BoundedAsyncDriver`], which keeps every process spawn and
//! RPC call on the speech worker while the terminal event loop interacts only
//! with [`SupervisorHandle`].

use super::{Driver, SpeechServerSpec, proc_driver};
use anyhow::{Context, Result as DriverResult, anyhow};
use mio::Waker;
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

const CRASH_WINDOW: Duration = Duration::from_secs(30);
const MAX_EVENTS: usize = 32;
const MAX_PENDING_SPEECH_ITEMS: usize = 32;
const MAX_PENDING_SPEECH_BYTES: usize = 256 * 1024;
const MAX_PENDING_SPEECH_ITEM_BYTES: usize = 64 * 1024;
const TRUNCATION_SUFFIX: &str = " … speech truncated";

/// A lifecycle transition for the main event loop to consume.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SupervisorEvent {
    /// Speech cannot safely continue and Lector should leave its event loop.
    Fatal(String),
    /// A runtime server replacement committed successfully.
    Reconfigured(SpeechServerSpec),
    /// A runtime replacement failed and the previous server remains active.
    ReconfigureFailed(String),
}

type Notifier = Arc<dyn Fn() + Send + Sync + 'static>;
type Terminator = Arc<dyn Fn() + Send + Sync + 'static>;

#[derive(Default)]
struct HandleState {
    events: VecDeque<SupervisorEvent>,
    owned_terminators: BTreeMap<u64, Terminator>,
    notifier: Option<Notifier>,
    shutting_down: bool,
}

struct HandleInner {
    state: Mutex<HandleState>,
}

/// Cloneable, nonblocking control plane for a speech [`Supervisor`].
///
/// This handle never performs speech RPC. `terminate` only sends the current
/// child a kill request so a worker blocked in pipe I/O can unwind promptly.
#[derive(Clone)]
pub struct SupervisorHandle {
    inner: Arc<HandleInner>,
}

impl SupervisorHandle {
    /// Attach the event-loop waker used for lifecycle notifications.
    ///
    /// If events were queued before attachment, the newly attached waker is
    /// fired once immediately. Replacing a waker is safe and does not discard
    /// queued events.
    pub fn set_waker(&self, waker: Arc<Waker>) {
        self.set_notifier(Arc::new(move || {
            let _ = waker.wake();
        }));
    }

    /// Attach an event callback instead of a mio waker.
    ///
    /// The callback can run on the speech worker and must return promptly. A
    /// panic is contained so a notification cannot kill that worker.
    pub fn set_notifier(&self, notifier: Notifier) {
        let has_events = {
            let mut state = self.lock();
            state.notifier = Some(Arc::clone(&notifier));
            !state.events.is_empty()
        };
        if has_events {
            notify(&notifier);
        }
    }

    /// Drain all currently queued lifecycle events in emission order.
    #[must_use]
    pub fn take_events(&self) -> Vec<SupervisorEvent> {
        self.lock().events.drain(..).collect()
    }

    /// Terminate the currently active direct child, if any.
    ///
    /// Reaping remains the speech worker's responsibility.
    pub fn terminate(&self) {
        let terminators: Vec<_> = {
            let mut state = self.lock();
            state.shutting_down = true;
            state.owned_terminators.values().cloned().collect()
        };
        for terminator in terminators {
            terminator();
        }
    }

    fn new() -> Self {
        Self {
            inner: Arc::new(HandleInner {
                state: Mutex::new(HandleState::default()),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HandleState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn register_child(&self, generation: u64, terminator: Terminator) {
        let terminate_immediately = {
            let mut state = self.lock();
            if state.shutting_down {
                true
            } else {
                state
                    .owned_terminators
                    .insert(generation, terminator.clone());
                false
            }
        };
        if terminate_immediately {
            terminator();
        }
    }

    fn unregister_child(&self, generation: u64) {
        self.lock().owned_terminators.remove(&generation);
    }

    fn is_shutting_down(&self) -> bool {
        self.lock().shutting_down
    }

    fn push_event(&self, event: SupervisorEvent) {
        let notifier = {
            let mut state = self.lock();
            if state.events.len() == MAX_EVENTS {
                let _ = state.events.pop_front();
            }
            state.events.push_back(event);
            state.notifier.clone()
        };
        if let Some(notifier) = notifier {
            notify(&notifier);
        }
    }
}

fn notify(notifier: &Notifier) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| notifier()));
}

struct ManagedProcess {
    driver: Box<dyn Driver + Send>,
    terminator: Terminator,
    _ownership: ChildOwnership,
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        // ProcDriver's Drop performs the synchronous reap immediately after
        // this kill request. Both happen only on the speech worker.
        (self.terminator)();
    }
}

struct ChildOwnership {
    generation: u64,
    handle: SupervisorHandle,
}

impl ChildOwnership {
    fn register(&self, terminator: Terminator) {
        self.handle.register_child(self.generation, terminator);
    }
}

impl Drop for ChildOwnership {
    fn drop(&mut self) {
        self.handle.unregister_child(self.generation);
    }
}

trait ProcessFactory: Send {
    fn spawn(
        &mut self,
        spec: &SpeechServerSpec,
        ownership: ChildOwnership,
    ) -> DriverResult<ManagedProcess>;
}

struct ProcFactory;

impl ProcessFactory for ProcFactory {
    fn spawn(
        &mut self,
        spec: &SpeechServerSpec,
        ownership: ChildOwnership,
    ) -> DriverResult<ManagedProcess> {
        let command = command_for_spec(spec)?;
        let mut registered_terminator = None;
        let driver = proc_driver::ProcDriver::new_with_args_and_registration(
            &command.program,
            &command.args,
            proc_driver::RpcTimeouts::default(),
            |termination| {
                let terminator: Terminator = Arc::new(move || termination.terminate());
                ownership.register(Arc::clone(&terminator));
                registered_terminator = Some(terminator);
            },
        )
        .with_context(|| format!("start speech server {}", command.program.display()))?;
        let terminator = registered_terminator
            .expect("ProcDriver registers its termination handle before returning");
        Ok(ManagedProcess {
            driver: Box::new(driver),
            terminator,
            _ownership: ownership,
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
struct ServerCommand {
    program: PathBuf,
    args: Vec<String>,
}

fn command_for_spec(spec: &SpeechServerSpec) -> DriverResult<ServerCommand> {
    match spec {
        SpeechServerSpec::Native => Ok(ServerCommand {
            program: std::env::current_exe().context("locate Lector native speech host")?,
            args: vec![
                "--native-speech-server".to_owned(),
                "--native-speech-parent-pid".to_owned(),
                std::process::id().to_string(),
            ],
        }),
        SpeechServerSpec::Process { program, args } => Ok(ServerCommand {
            program: PathBuf::from(program),
            args: args.clone(),
        }),
    }
}

#[derive(Debug)]
struct PendingSpeech {
    text: String,
    interrupt: bool,
}

/// Owns the selected process-backed speech server and its recovery policy.
///
/// Construction is deferred: no process is created until [`Driver::start`].
/// Put this driver underneath [`super::worker::BoundedAsyncDriver`] so all of
/// its methods except [`Self::handle`] execute on the speech worker.
pub struct Supervisor {
    spec: SpeechServerSpec,
    active: Option<ManagedProcess>,
    desired_rate: f32,
    pending_speech: VecDeque<PendingSpeech>,
    pending_speech_bytes: usize,
    started: bool,
    startup_error: Option<String>,
    fatal_error: Option<String>,
    last_crash: Option<Instant>,
    handle: SupervisorHandle,
    next_generation: u64,
    factory: Box<dyn ProcessFactory>,
    now: Box<dyn Fn() -> Instant + Send>,
}

impl Supervisor {
    /// Create a deferred supervisor for `spec` without spawning it.
    #[must_use]
    pub fn new(spec: SpeechServerSpec) -> Self {
        Self::new_inner(spec, Box::new(ProcFactory), Box::new(Instant::now))
    }

    /// Return the nonblocking event-loop/shutdown handle.
    #[must_use]
    pub fn handle(&self) -> SupervisorHandle {
        self.handle.clone()
    }

    /// Return the currently selected server specification.
    #[must_use]
    pub fn server_spec(&self) -> &SpeechServerSpec {
        &self.spec
    }

    fn new_inner(
        spec: SpeechServerSpec,
        factory: Box<dyn ProcessFactory>,
        now: Box<dyn Fn() -> Instant + Send>,
    ) -> Self {
        Self {
            spec,
            active: None,
            desired_rate: 1.0,
            pending_speech: VecDeque::new(),
            pending_speech_bytes: 0,
            started: false,
            startup_error: None,
            fatal_error: None,
            last_crash: None,
            handle: SupervisorHandle::new(),
            next_generation: 1,
            factory,
            now,
        }
    }

    fn spawn_ready(&mut self, spec: &SpeechServerSpec) -> DriverResult<ManagedProcess> {
        if self.handle.is_shutting_down() {
            return Err(anyhow!("speech supervisor is shutting down"));
        }
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        let ownership = ChildOwnership {
            generation,
            handle: self.handle.clone(),
        };
        let mut candidate = self.factory.spawn(spec, ownership)?;
        if self.handle.is_shutting_down() {
            drop(candidate);
            return Err(anyhow!("speech supervisor is shutting down"));
        }
        candidate
            .driver
            .set_rate(self.desired_rate)
            .context("restore speech rate")?;
        Ok(candidate)
    }

    fn install_active(&mut self, process: ManagedProcess) -> Option<ManagedProcess> {
        self.active.replace(process)
    }

    fn clear_active(&mut self) -> Option<ManagedProcess> {
        self.active.take()
    }

    fn startup(&mut self) -> DriverResult<()> {
        if self.started {
            return self.ensure_available();
        }
        if let Some(error) = &self.startup_error {
            return Err(anyhow!(error.clone()));
        }

        let spec = self.spec.clone();
        let first_error = match self.spawn_ready(&spec) {
            Ok(process) => {
                let _ = self.install_active(process);
                self.started = true;
                return self.flush_pending_speech();
            }
            Err(error) => error,
        };
        let second_error = match self.spawn_ready(&spec) {
            Ok(process) => {
                let _ = self.install_active(process);
                self.started = true;
                return self.flush_pending_speech();
            }
            Err(error) => error,
        };
        let message = format!(
            "speech server startup failed twice; first attempt: {first_error:#}; retry: {second_error:#}"
        );
        self.startup_error = Some(message.clone());
        Err(anyhow!(message))
    }

    fn flush_pending_speech(&mut self) -> DriverResult<()> {
        while let Some(pending) = self.pending_speech.pop_front() {
            self.pending_speech_bytes =
                self.pending_speech_bytes.saturating_sub(pending.text.len());
            if let Err(error) = self.call_active("speak", move |driver| {
                driver.speak(&pending.text, pending.interrupt)
            }) {
                if self.fatal_error.is_some() {
                    self.pending_speech.clear();
                    self.pending_speech_bytes = 0;
                    return Err(error);
                }
                crate::diagnostics::event(
                    "speech-supervisor",
                    "buffered-speech-error",
                    &format!("{error:#}"),
                );
            }
        }
        Ok(())
    }

    fn buffer_speech(&mut self, text: &str, interrupt: bool) {
        if interrupt {
            self.pending_speech.clear();
            self.pending_speech_bytes = 0;
        }
        let text = bounded_text(text, MAX_PENDING_SPEECH_ITEM_BYTES);
        while self.pending_speech.len() == MAX_PENDING_SPEECH_ITEMS
            || self.pending_speech_bytes.saturating_add(text.len()) > MAX_PENDING_SPEECH_BYTES
        {
            let Some(stale) = self.pending_speech.pop_front() else {
                break;
            };
            self.pending_speech_bytes = self.pending_speech_bytes.saturating_sub(stale.text.len());
        }
        self.pending_speech_bytes = self.pending_speech_bytes.saturating_add(text.len());
        self.pending_speech
            .push_back(PendingSpeech { text, interrupt });
    }

    fn ensure_available(&self) -> DriverResult<()> {
        if let Some(error) = &self.fatal_error {
            return Err(anyhow!(error.clone()));
        }
        if self.active.is_none() {
            return Err(anyhow!("speech server is not active"));
        }
        Ok(())
    }

    fn call_active(
        &mut self,
        operation: &'static str,
        call: impl FnOnce(&mut dyn Driver) -> DriverResult<()>,
    ) -> DriverResult<()> {
        self.ensure_available()?;
        let result = {
            let process = self.active.as_mut().expect("active checked above");
            call(process.driver.as_mut())
        };
        let Err(error) = result else {
            return Ok(());
        };
        if !is_transport_failure(&error) {
            return Err(error);
        }

        // The request may have reached the failed process, so recovery never
        // calls `call` again. Only a fresh generation and desired rate are
        // established for later requests.
        let failure = format!("speech {operation} transport failure: {error:#}");
        self.recover_after_transport_failure(failure)?;
        Err(error)
    }

    fn recover_after_transport_failure(&mut self, failure: String) -> DriverResult<()> {
        let now = (self.now)();
        let may_restart = restart_allowed(self.last_crash, now);
        let failed = self.clear_active();
        drop(failed);

        if !may_restart {
            let message =
                format!("{failure}; a second speech transport failure occurred within 30 seconds");
            self.enter_fatal(message.clone());
            return Err(anyhow!(message));
        }
        self.last_crash = Some(now);

        let spec = self.spec.clone();
        match self.spawn_ready(&spec) {
            Ok(process) => {
                let _ = self.install_active(process);
                Ok(())
            }
            Err(error) => {
                let message = format!("{failure}; restarting the speech server failed: {error:#}");
                self.enter_fatal(message.clone());
                Err(anyhow!(message))
            }
        }
    }

    fn enter_fatal(&mut self, message: String) {
        if self.fatal_error.is_some() {
            return;
        }
        self.fatal_error = Some(message.clone());
        self.handle.push_event(SupervisorEvent::Fatal(message));
    }

    fn reconfigure(&mut self, spec: SpeechServerSpec) -> DriverResult<()> {
        if !self.started {
            if self.startup_error.is_some() {
                return Err(anyhow!(
                    "speech server startup has already failed; configuration is closed"
                ));
            }
            self.spec = spec;
            return Ok(());
        }
        self.ensure_available()?;

        match self.spawn_ready(&spec) {
            Ok(candidate) => {
                let old = self.install_active(candidate);
                self.spec = spec.clone();
                // An intentional replacement is not itself a crash. Preserve
                // any real crash timestamp so changing servers cannot bypass
                // the rolling 30-second failure policy.
                drop(old);
                self.handle.push_event(SupervisorEvent::Reconfigured(spec));
                Ok(())
            }
            Err(error) => {
                let message = format!("replace speech server: {error:#}");
                self.handle
                    .push_event(SupervisorEvent::ReconfigureFailed(message.clone()));
                Err(anyhow!(message))
            }
        }
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new(SpeechServerSpec::default())
    }
}

impl Driver for Supervisor {
    fn speak(&mut self, text: &str, interrupt: bool) -> DriverResult<()> {
        if text.is_empty() {
            return Ok(());
        }
        if !self.started {
            if let Some(error) = &self.startup_error {
                return Err(anyhow!(error.clone()));
            }
            self.buffer_speech(text, interrupt);
            return Ok(());
        }
        self.call_active("speak", |driver| driver.speak(text, interrupt))
    }

    fn stop(&mut self) -> DriverResult<()> {
        if !self.started {
            self.pending_speech.clear();
            self.pending_speech_bytes = 0;
            return Ok(());
        }
        self.call_active("stop", |driver| driver.stop())
    }

    fn get_rate(&self) -> f32 {
        self.desired_rate
    }

    fn set_rate(&mut self, rate: f32) -> DriverResult<()> {
        if !rate.is_finite() {
            return Err(anyhow!("speech rate must be finite"));
        }
        self.desired_rate = rate;
        if !self.started {
            return Ok(());
        }
        self.call_active("set_rate", |driver| driver.set_rate(rate))
    }

    fn start(&mut self) -> DriverResult<()> {
        self.startup()
    }

    fn configure_server(&mut self, spec: SpeechServerSpec) -> DriverResult<()> {
        self.reconfigure(spec)
    }

    fn shutdown(&mut self) {
        self.handle.terminate();
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        let active = self.clear_active();
        drop(active);
    }
}

fn is_transport_failure(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<proc_driver::Error>()
        .is_some_and(proc_driver::Error::is_transport_failure)
}

fn restart_allowed(last_crash: Option<Instant>, now: Instant) -> bool {
    last_crash.is_none_or(|last_crash| {
        now.checked_duration_since(last_crash)
            .is_some_and(|elapsed| elapsed >= CRASH_WINDOW)
    })
}

fn bounded_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let prefix_limit = limit.saturating_sub(TRUNCATION_SUFFIX.len());
    let mut end = prefix_limit.min(text.len());
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let mut bounded = String::with_capacity(limit);
    bounded.push_str(&text[..end]);
    bounded.push_str(TRUNCATION_SUFFIX);
    bounded
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::{
            Arc, Condvar,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        thread,
    };

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum Call {
        Speak(String, bool),
        Stop,
        SetRate(u32),
        Terminate,
    }

    #[derive(Default)]
    struct FakeState {
        calls: Vec<Call>,
        speak_results: VecDeque<DriverResult<()>>,
        stop_results: VecDeque<DriverResult<()>>,
        rate_results: VecDeque<DriverResult<()>>,
    }

    struct FakeDriver {
        state: Arc<Mutex<FakeState>>,
        rate: f32,
    }

    impl Driver for FakeDriver {
        fn speak(&mut self, text: &str, interrupt: bool) -> DriverResult<()> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(Call::Speak(text.to_owned(), interrupt));
            state.speak_results.pop_front().unwrap_or(Ok(()))
        }

        fn stop(&mut self) -> DriverResult<()> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(Call::Stop);
            state.stop_results.pop_front().unwrap_or(Ok(()))
        }

        fn get_rate(&self) -> f32 {
            self.rate
        }

        fn set_rate(&mut self, rate: f32) -> DriverResult<()> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(Call::SetRate(rate.to_bits()));
            let result = state.rate_results.pop_front().unwrap_or(Ok(()));
            if result.is_ok() {
                self.rate = rate;
            }
            result
        }
    }

    enum SpawnStep {
        Driver(Arc<Mutex<FakeState>>),
        Error(&'static str),
    }

    struct FakeFactory {
        steps: Arc<Mutex<VecDeque<SpawnStep>>>,
        specs: Arc<Mutex<Vec<SpeechServerSpec>>>,
        spawns: Arc<AtomicUsize>,
    }

    impl ProcessFactory for FakeFactory {
        fn spawn(
            &mut self,
            spec: &SpeechServerSpec,
            ownership: ChildOwnership,
        ) -> DriverResult<ManagedProcess> {
            self.spawns.fetch_add(1, Ordering::SeqCst);
            self.specs.lock().unwrap().push(spec.clone());
            let step = self
                .steps
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted spawn step");
            match step {
                SpawnStep::Error(message) => Err(anyhow!(message)),
                SpawnStep::Driver(state) => {
                    let terminated = Arc::clone(&state);
                    let terminator: Terminator = Arc::new(move || {
                        terminated.lock().unwrap().calls.push(Call::Terminate);
                    });
                    ownership.register(Arc::clone(&terminator));
                    Ok(ManagedProcess {
                        driver: Box::new(FakeDriver { state, rate: 1.0 }),
                        terminator,
                        _ownership: ownership,
                    })
                }
            }
        }
    }

    struct BlockingCandidateFactory {
        spawn_count: usize,
        active: Arc<Mutex<FakeState>>,
        candidate_started: mpsc::SyncSender<()>,
        candidate_release: Arc<(Mutex<bool>, Condvar)>,
        candidate_terminations: Arc<AtomicUsize>,
    }

    impl ProcessFactory for BlockingCandidateFactory {
        fn spawn(
            &mut self,
            _spec: &SpeechServerSpec,
            ownership: ChildOwnership,
        ) -> DriverResult<ManagedProcess> {
            self.spawn_count += 1;
            if self.spawn_count == 1 {
                let terminated = Arc::clone(&self.active);
                let terminator: Terminator = Arc::new(move || {
                    terminated.lock().unwrap().calls.push(Call::Terminate);
                });
                ownership.register(Arc::clone(&terminator));
                return Ok(ManagedProcess {
                    driver: Box::new(FakeDriver {
                        state: Arc::clone(&self.active),
                        rate: 1.0,
                    }),
                    terminator,
                    _ownership: ownership,
                });
            }

            let release = Arc::clone(&self.candidate_release);
            let terminations = Arc::clone(&self.candidate_terminations);
            let terminator: Terminator = Arc::new(move || {
                terminations.fetch_add(1, Ordering::SeqCst);
                let (released, available) = &*release;
                *released.lock().unwrap() = true;
                available.notify_all();
            });
            ownership.register(terminator);
            self.candidate_started.send(()).unwrap();
            let (released, available) = &*self.candidate_release;
            let mut released = released.lock().unwrap();
            while !*released {
                released = available.wait(released).unwrap();
            }
            Err(anyhow!("candidate initialization was interrupted"))
        }
    }

    struct Harness {
        supervisor: Supervisor,
        steps: Arc<Mutex<VecDeque<SpawnStep>>>,
        specs: Arc<Mutex<Vec<SpeechServerSpec>>>,
        spawns: Arc<AtomicUsize>,
        now: Arc<Mutex<Instant>>,
    }

    impl Harness {
        fn new() -> Self {
            let steps = Arc::new(Mutex::new(VecDeque::new()));
            let specs = Arc::new(Mutex::new(Vec::new()));
            let spawns = Arc::new(AtomicUsize::new(0));
            let now = Arc::new(Mutex::new(Instant::now()));
            let clock = Arc::clone(&now);
            let supervisor = Supervisor::new_inner(
                SpeechServerSpec::Native,
                Box::new(FakeFactory {
                    steps: Arc::clone(&steps),
                    specs: Arc::clone(&specs),
                    spawns: Arc::clone(&spawns),
                }),
                Box::new(move || *clock.lock().unwrap()),
            );
            Self {
                supervisor,
                steps,
                specs,
                spawns,
                now,
            }
        }

        fn push_driver(&self, state: Arc<Mutex<FakeState>>) {
            self.steps
                .lock()
                .unwrap()
                .push_back(SpawnStep::Driver(state));
        }

        fn push_error(&self, message: &'static str) {
            self.steps
                .lock()
                .unwrap()
                .push_back(SpawnStep::Error(message));
        }

        fn advance(&self, duration: Duration) {
            let mut now = self.now.lock().unwrap();
            *now += duration;
        }
    }

    fn fake_state() -> Arc<Mutex<FakeState>> {
        Arc::new(Mutex::new(FakeState::default()))
    }

    fn transport_failure() -> anyhow::Error {
        proc_driver::Error::Closed.into()
    }

    #[test]
    fn process_command_preserves_program_and_exact_arguments() {
        let spec = SpeechServerSpec::Process {
            program: "/tmp/server with spaces".to_owned(),
            args: vec![
                "two words".to_owned(),
                "'quotes'".to_owned(),
                "$not-expanded".to_owned(),
            ],
        };
        assert_eq!(
            command_for_spec(&spec).unwrap(),
            ServerCommand {
                program: PathBuf::from("/tmp/server with spaces"),
                args: vec![
                    "two words".to_owned(),
                    "'quotes'".to_owned(),
                    "$not-expanded".to_owned(),
                ],
            }
        );
    }

    #[test]
    fn native_command_self_execs_with_parent_pid() {
        let command = command_for_spec(&SpeechServerSpec::Native).unwrap();
        assert_eq!(command.program, std::env::current_exe().unwrap());
        assert_eq!(
            command.args,
            [
                "--native-speech-server",
                "--native-speech-parent-pid",
                &std::process::id().to_string(),
            ]
        );
    }

    #[test]
    fn startup_is_deferred_retries_once_and_restores_rate() {
        let mut harness = Harness::new();
        let active = fake_state();
        harness.push_error("first initialize failed");
        harness.push_driver(Arc::clone(&active));

        harness.supervisor.set_rate(1.75).unwrap();
        assert_eq!(harness.spawns.load(Ordering::SeqCst), 0);
        harness.supervisor.start().unwrap();

        assert_eq!(harness.spawns.load(Ordering::SeqCst), 2);
        assert_eq!(
            active.lock().unwrap().calls,
            [Call::SetRate(1.75f32.to_bits())]
        );
    }

    #[test]
    fn startup_stops_after_two_failures() {
        let mut harness = Harness::new();
        harness.push_error("first");
        harness.push_error("second");
        let error = harness.supervisor.start().unwrap_err();
        assert!(format!("{error:#}").contains("startup failed twice"));
        assert_eq!(harness.spawns.load(Ordering::SeqCst), 2);

        assert!(harness.supervisor.start().is_err());
        assert_eq!(harness.spawns.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn prestart_speech_is_bounded_flushed_and_cleared_by_stop() {
        let mut harness = Harness::new();
        for index in 0..(MAX_PENDING_SPEECH_ITEMS + 5) {
            harness
                .supervisor
                .speak(&format!("message {index}"), false)
                .unwrap();
        }
        assert_eq!(
            harness.supervisor.pending_speech.len(),
            MAX_PENDING_SPEECH_ITEMS
        );
        harness.supervisor.stop().unwrap();
        assert!(harness.supervisor.pending_speech.is_empty());

        harness.supervisor.speak("kept", false).unwrap();
        let active = fake_state();
        harness.push_driver(Arc::clone(&active));
        harness.supervisor.start().unwrap();
        assert_eq!(
            active.lock().unwrap().calls,
            [
                Call::SetRate(1.0f32.to_bits()),
                Call::Speak("kept".to_owned(), false),
            ]
        );
    }

    #[test]
    fn runtime_failure_restarts_without_replaying_uncertain_request() {
        let mut harness = Harness::new();
        let failed = fake_state();
        failed
            .lock()
            .unwrap()
            .speak_results
            .push_back(Err(transport_failure()));
        let restarted = fake_state();
        harness.push_driver(Arc::clone(&failed));
        harness.push_driver(Arc::clone(&restarted));
        harness.supervisor.set_rate(1.5).unwrap();
        harness.supervisor.start().unwrap();

        assert!(harness.supervisor.speak("uncertain", false).is_err());
        harness.supervisor.speak("later", true).unwrap();
        assert_eq!(
            failed.lock().unwrap().calls,
            [
                Call::SetRate(1.5f32.to_bits()),
                Call::Speak("uncertain".to_owned(), false),
                Call::Terminate,
            ]
        );
        assert_eq!(
            restarted.lock().unwrap().calls,
            [
                Call::SetRate(1.5f32.to_bits()),
                Call::Speak("later".to_owned(), true),
            ]
        );
        assert!(harness.supervisor.handle().take_events().is_empty());
    }

    #[test]
    fn rpc_operation_error_does_not_restart_or_emit_fatal() {
        let mut harness = Harness::new();
        let active = fake_state();
        active
            .lock()
            .unwrap()
            .speak_results
            .push_back(Err(proc_driver::Error::Rpc {
                code: -32603,
                message: "backend rejected speech".to_owned(),
                data: String::new(),
            }
            .into()));
        harness.push_driver(Arc::clone(&active));
        harness.supervisor.start().unwrap();

        assert!(harness.supervisor.speak("rejected", false).is_err());
        harness.supervisor.speak("still active", false).unwrap();
        assert_eq!(harness.spawns.load(Ordering::SeqCst), 1);
        assert!(!active.lock().unwrap().calls.contains(&Call::Terminate));
        assert!(harness.supervisor.handle().take_events().is_empty());
    }

    #[test]
    fn second_crash_before_window_is_fatal_without_restart() {
        let mut harness = Harness::new();
        let first = fake_state();
        first
            .lock()
            .unwrap()
            .speak_results
            .push_back(Err(transport_failure()));
        let second = fake_state();
        second
            .lock()
            .unwrap()
            .speak_results
            .push_back(Err(transport_failure()));
        harness.push_driver(first);
        harness.push_driver(second);
        harness.supervisor.start().unwrap();
        assert!(harness.supervisor.speak("one", false).is_err());
        harness.advance(CRASH_WINDOW - Duration::from_nanos(1));
        assert!(harness.supervisor.speak("two", false).is_err());

        assert_eq!(harness.spawns.load(Ordering::SeqCst), 2);
        assert!(matches!(
            harness.supervisor.handle().take_events().as_slice(),
            [SupervisorEvent::Fatal(message)] if message.contains("within 30 seconds")
        ));
    }

    #[test]
    fn crash_window_allows_exact_boundary_and_later() {
        let base = Instant::now();
        assert!(!restart_allowed(
            Some(base),
            base + CRASH_WINDOW - Duration::from_nanos(1)
        ));
        assert!(restart_allowed(Some(base), base + CRASH_WINDOW));
        assert!(restart_allowed(
            Some(base),
            base + CRASH_WINDOW + Duration::from_nanos(1)
        ));
    }

    #[test]
    fn failed_restart_is_fatal() {
        let mut harness = Harness::new();
        let failed = fake_state();
        failed
            .lock()
            .unwrap()
            .speak_results
            .push_back(Err(transport_failure()));
        harness.push_driver(failed);
        harness.push_error("restart initialize failed");
        harness.supervisor.start().unwrap();
        assert!(harness.supervisor.speak("crash", false).is_err());
        assert!(matches!(
            harness.supervisor.handle().take_events().as_slice(),
            [SupervisorEvent::Fatal(message)] if message.contains("restarting")
        ));
    }

    #[test]
    fn reconfiguration_is_transactional_and_preserves_the_crash_window() {
        let mut harness = Harness::new();
        let old = fake_state();
        harness.push_driver(Arc::clone(&old));
        harness.supervisor.start().unwrap();

        harness.push_error("candidate failed");
        let rejected = SpeechServerSpec::Process {
            program: "rejected".to_owned(),
            args: vec![],
        };
        assert!(
            harness
                .supervisor
                .configure_server(rejected.clone())
                .is_err()
        );
        harness.supervisor.speak("still old", false).unwrap();
        assert_eq!(harness.supervisor.server_spec(), &SpeechServerSpec::Native);

        let recorded_crash = *harness.now.lock().unwrap();
        harness.supervisor.last_crash = Some(recorded_crash);
        let replacement = fake_state();
        harness.push_driver(replacement);
        let accepted = SpeechServerSpec::Process {
            program: "accepted".to_owned(),
            args: vec!["exact argument".to_owned()],
        };
        harness
            .supervisor
            .configure_server(accepted.clone())
            .unwrap();
        assert_eq!(harness.supervisor.server_spec(), &accepted);
        assert_eq!(harness.supervisor.last_crash, Some(recorded_crash));
        assert!(old.lock().unwrap().calls.contains(&Call::Terminate));
        assert_eq!(
            harness.supervisor.handle().take_events(),
            [
                SupervisorEvent::ReconfigureFailed(
                    "replace speech server: candidate failed".to_owned()
                ),
                SupervisorEvent::Reconfigured(accepted),
            ]
        );
    }

    #[test]
    fn intentional_reconfiguration_cannot_bypass_a_recent_crash() {
        let mut harness = Harness::new();
        let first = fake_state();
        first
            .lock()
            .unwrap()
            .speak_results
            .push_back(Err(transport_failure()));
        let recovered = fake_state();
        let replacement = fake_state();
        replacement
            .lock()
            .unwrap()
            .speak_results
            .push_back(Err(transport_failure()));
        harness.push_driver(first);
        harness.push_driver(recovered);
        harness.push_driver(replacement);
        harness.supervisor.start().unwrap();

        harness.supervisor.speak("first crash", false).unwrap_err();
        harness
            .supervisor
            .configure_server(SpeechServerSpec::Process {
                program: "intentional replacement".to_owned(),
                args: vec![],
            })
            .unwrap();
        harness.supervisor.speak("second crash", false).unwrap_err();

        assert_eq!(harness.spawns.load(Ordering::SeqCst), 3);
        assert!(matches!(
            harness.supervisor.handle().take_events().as_slice(),
            [SupervisorEvent::Reconfigured(_), SupervisorEvent::Fatal(message)]
                if message.contains("within 30 seconds")
        ));
    }

    #[test]
    fn queued_events_survive_notifier_attachment_and_handle_terminates() {
        let mut harness = Harness::new();
        let active = fake_state();
        harness.push_driver(Arc::clone(&active));
        harness.supervisor.start().unwrap();
        harness.push_error("candidate failed");
        let _ = harness
            .supervisor
            .configure_server(SpeechServerSpec::Native);

        let notifications = Arc::new(AtomicUsize::new(0));
        let notified = Arc::clone(&notifications);
        let handle = harness.supervisor.handle();
        handle.set_notifier(Arc::new(move || {
            notified.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        assert_eq!(handle.take_events().len(), 1);

        handle.terminate();
        assert!(active.lock().unwrap().calls.contains(&Call::Terminate));
    }

    #[test]
    fn shutdown_immediately_terminates_children_registered_after_the_race() {
        let handle = SupervisorHandle::new();
        handle.terminate();
        let terminations = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&terminations);

        handle.register_child(
            7,
            Arc::new(move || {
                observed.fetch_add(1, Ordering::SeqCst);
            }),
        );

        assert_eq!(terminations.load(Ordering::SeqCst), 1);
        assert!(handle.lock().owned_terminators.is_empty());
    }

    #[test]
    fn terminate_interrupts_active_and_initializing_candidate() {
        let active = fake_state();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let candidate_release = Arc::new((Mutex::new(false), Condvar::new()));
        let candidate_terminations = Arc::new(AtomicUsize::new(0));
        let mut supervisor = Supervisor::new_inner(
            SpeechServerSpec::Native,
            Box::new(BlockingCandidateFactory {
                spawn_count: 0,
                active: Arc::clone(&active),
                candidate_started: started_tx,
                candidate_release,
                candidate_terminations: Arc::clone(&candidate_terminations),
            }),
            Box::new(Instant::now),
        );
        supervisor.start().unwrap();
        let handle = supervisor.handle();
        let worker = thread::spawn(move || {
            let result = supervisor.configure_server(SpeechServerSpec::Process {
                program: "candidate".to_owned(),
                args: vec![],
            });
            (supervisor, result)
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("candidate registered before blocking");

        handle.terminate();
        let (supervisor, result) = worker.join().unwrap();
        assert!(result.is_err());
        assert_eq!(candidate_terminations.load(Ordering::SeqCst), 1);
        assert_eq!(
            active
                .lock()
                .unwrap()
                .calls
                .iter()
                .filter(|call| **call == Call::Terminate)
                .count(),
            1
        );
        drop(supervisor);
    }

    #[test]
    fn configuration_before_start_only_changes_selection() {
        let mut harness = Harness::new();
        let spec = SpeechServerSpec::Process {
            program: "server".to_owned(),
            args: vec!["one".to_owned(), "two words".to_owned()],
        };
        harness.supervisor.configure_server(spec.clone()).unwrap();
        assert_eq!(harness.spawns.load(Ordering::SeqCst), 0);
        let active = fake_state();
        harness.push_driver(active);
        harness.supervisor.start().unwrap();
        assert_eq!(harness.specs.lock().unwrap().as_slice(), [spec]);
        assert!(harness.supervisor.handle().take_events().is_empty());
    }
}
