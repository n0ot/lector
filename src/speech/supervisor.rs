//! Deferred process-speech lifecycle and crash supervision.
//!
//! [`Supervisor`] is deliberately synchronous. It is intended to live below
//! [`super::worker::BoundedAsyncDriver`], which keeps every process spawn and
//! RPC call on the speech worker while the terminal event loop interacts only
//! with [`SupervisorHandle`].

use super::{
    Driver, SpeechServerSpec, UtteranceBoundary,
    manager::{Host, SpeechManager},
    proc_driver,
    protocol::UtteranceId,
};
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
    host: Box<dyn Host + Send>,
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
        let host = proc_driver::ProcDriver::new_with_args_and_registration(
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
            host: Box::new(host),
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
                "tts".to_owned(),
                "--parent-pid".to_owned(),
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
    id: UtteranceId,
    text: String,
    interrupt: bool,
    boundary: UtteranceBoundary,
}

impl PendingSpeech {
    fn queue_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.id.as_str().len())
            .saturating_add(self.text.len())
    }
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
    pending_speech_paused: bool,
    manager: SpeechManager,
    next_compatibility_id: u64,
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
            pending_speech_paused: false,
            manager: SpeechManager::default(),
            next_compatibility_id: 1,
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
        if candidate.host.has_legacy_queue()
            || candidate.host.capabilities().settings.rate.can_write()
        {
            candidate
                .host
                .set_rate(self.desired_rate)
                .context("restore speech rate")?;
        }
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
                return self.flush_pending_speech_if_playing();
            }
            Err(error) => error,
        };
        let second_error = match self.spawn_ready(&spec) {
            Ok(process) => {
                let _ = self.install_active(process);
                self.started = true;
                return self.flush_pending_speech_if_playing();
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
            self.pending_speech_bytes = self
                .pending_speech_bytes
                .saturating_sub(pending.queue_bytes());
            if let Err(error) = self.call_managed("speak", move |manager, host| {
                manager.submit_with_boundary(
                    host,
                    pending.id,
                    pending.text,
                    pending.interrupt,
                    pending.boundary,
                )
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

    fn flush_pending_speech_if_playing(&mut self) -> DriverResult<()> {
        if !self.started || self.pending_speech_paused {
            Ok(())
        } else {
            self.flush_pending_speech()
        }
    }

    fn buffer_speech(
        &mut self,
        id: UtteranceId,
        text: &str,
        interrupt: bool,
        boundary: UtteranceBoundary,
    ) {
        if interrupt {
            self.pending_speech.clear();
            self.pending_speech_bytes = 0;
            self.pending_speech_paused = false;
        }
        let text = bounded_text(text, MAX_PENDING_SPEECH_ITEM_BYTES);
        let pending = PendingSpeech {
            id,
            text,
            interrupt,
            boundary,
        };
        let pending_bytes = pending.queue_bytes();
        while self.pending_speech_bytes.saturating_add(pending_bytes) > MAX_PENDING_SPEECH_BYTES {
            let Some(stale) = self.pending_speech.pop_front() else {
                break;
            };
            self.pending_speech_bytes = self
                .pending_speech_bytes
                .saturating_sub(stale.queue_bytes());
        }
        self.pending_speech_bytes = self.pending_speech_bytes.saturating_add(pending_bytes);
        self.pending_speech.push_back(pending);
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

    fn call_host(
        &mut self,
        operation: &'static str,
        call: impl FnOnce(&mut dyn Host) -> DriverResult<()>,
    ) -> DriverResult<()> {
        self.ensure_available()?;
        let result = {
            let process = self.active.as_mut().expect("active checked above");
            call(process.host.as_mut())
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

    fn call_managed(
        &mut self,
        operation: &'static str,
        call: impl FnOnce(&mut SpeechManager, &mut dyn Host) -> DriverResult<()>,
    ) -> DriverResult<()> {
        self.ensure_available()?;
        let result = {
            let manager = &mut self.manager;
            let process = self.active.as_mut().expect("active checked above");
            call(manager, process.host.as_mut())
        };
        let Err(error) = result else {
            return Ok(());
        };
        if !is_transport_failure(&error) {
            return Err(error);
        }
        let failure = format!("speech {operation} transport failure: {error:#}");
        self.recover_after_transport_failure(failure)?;
        Err(error)
    }

    fn recover_after_transport_failure(&mut self, failure: String) -> DriverResult<()> {
        let now = (self.now)();
        let may_restart = restart_allowed(self.last_crash, now);
        self.manager.host_lost();
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
            Ok(mut process) => {
                if let Err(error) = self.manager.host_ready(process.host.as_mut()) {
                    let message = format!(
                        "{failure}; resume pending speech on restarted server failed: {error:#}"
                    );
                    self.enter_fatal(message.clone());
                    return Err(anyhow!(message));
                }
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
            Ok(mut candidate) => {
                // Prepare the candidate from a clone so failure leaves both
                // the old process and its evidence-backed manager state
                // untouched. Active speech is never replayed; only work that
                // had not yet reached the old host may cross generations.
                let mut candidate_manager = self.manager.clone();
                candidate_manager.host_lost();
                if let Err(error) = candidate_manager.host_ready(candidate.host.as_mut()) {
                    let message = format!(
                        "replace speech server: resume pending speech on candidate: {error:#}"
                    );
                    self.handle
                        .push_event(SupervisorEvent::ReconfigureFailed(message.clone()));
                    return Err(anyhow!(message));
                }
                let old = self.install_active(candidate);
                self.manager = candidate_manager;
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
        let id = UtteranceId::new(format!("compat-{}", self.next_compatibility_id));
        self.next_compatibility_id = self.next_compatibility_id.wrapping_add(1);
        self.speak_utterance(&id, text, interrupt)
    }

    fn speak_utterance(
        &mut self,
        id: &UtteranceId,
        text: &str,
        interrupt: bool,
    ) -> DriverResult<()> {
        self.speak_utterance_with_boundary(id, text, interrupt, UtteranceBoundary::Immediate)
    }

    fn speak_utterance_with_boundary(
        &mut self,
        id: &UtteranceId,
        text: &str,
        interrupt: bool,
        boundary: UtteranceBoundary,
    ) -> DriverResult<()> {
        if text.is_empty() {
            return Ok(());
        }
        if !self.started {
            if let Some(error) = &self.startup_error {
                return Err(anyhow!(error.clone()));
            }
            if self.pending_speech_paused {
                self.pending_speech.clear();
                self.pending_speech_bytes = 0;
                self.pending_speech_paused = false;
            }
            self.buffer_speech(id.clone(), text, interrupt, boundary);
            return Ok(());
        }
        if self.pending_speech_paused {
            self.pending_speech.clear();
            self.pending_speech_bytes = 0;
            self.pending_speech_paused = false;
        }
        self.call_managed("speak", |manager, host| {
            manager.submit_with_boundary(host, id.clone(), text.to_owned(), interrupt, boundary)
        })
    }

    fn stop(&mut self) -> DriverResult<()> {
        self.pending_speech.clear();
        self.pending_speech_bytes = 0;
        self.pending_speech_paused = false;
        if !self.started {
            return Ok(());
        }
        self.call_managed("cancel", SpeechManager::cancel)
    }

    fn pause(&mut self) -> DriverResult<()> {
        if self.pending_speech_paused {
            return Ok(());
        }
        if !self.started {
            self.pending_speech_paused = !self.pending_speech.is_empty();
            return Ok(());
        }
        self.call_managed("pause", SpeechManager::pause)
    }

    fn resume(&mut self) -> DriverResult<()> {
        if self.pending_speech_paused {
            self.pending_speech_paused = false;
            return self.flush_pending_speech_if_playing();
        }
        if !self.started {
            return Ok(());
        }
        self.call_managed("resume", SpeechManager::resume)
    }

    fn toggle(&mut self) -> DriverResult<()> {
        if self.pending_speech_paused {
            return self.resume();
        }
        if !self.started {
            self.pending_speech_paused = !self.pending_speech.is_empty();
            return Ok(());
        }
        self.call_managed("toggle", SpeechManager::toggle)
    }

    fn supports_ordered_utterances(&self) -> bool {
        self.active.as_ref().is_some_and(|process| {
            process
                .host
                .capabilities()
                .lifecycle
                .terminal
                .delivery
                .is_reliable()
        })
    }

    fn poll(&mut self) -> DriverResult<()> {
        if !self.started || self.active.is_none() {
            return Ok(());
        }
        self.call_managed("event", SpeechManager::poll)
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
        if self.active.as_ref().is_some_and(|process| {
            !process.host.has_legacy_queue()
                && !process.host.capabilities().settings.rate.can_write()
        }) {
            return Ok(());
        }
        self.call_host("set_rate", |host| host.set_rate(rate).map(|_| ()))
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

    struct FakeState {
        calls: Vec<Call>,
        speak_results: VecDeque<DriverResult<()>>,
        stop_results: VecDeque<DriverResult<()>>,
        rate_results: VecDeque<DriverResult<()>>,
        legacy: bool,
    }

    impl Default for FakeState {
        fn default() -> Self {
            Self {
                calls: Vec::new(),
                speak_results: VecDeque::new(),
                stop_results: VecDeque::new(),
                rate_results: VecDeque::new(),
                legacy: true,
            }
        }
    }

    struct FakeDriver {
        state: Arc<Mutex<FakeState>>,
        rate: f32,
        capabilities: crate::speech::protocol::SpeechCapabilities,
    }

    impl Host for FakeDriver {
        fn capabilities(&self) -> &crate::speech::protocol::SpeechCapabilities {
            &self.capabilities
        }

        fn has_legacy_queue(&self) -> bool {
            self.state.lock().unwrap().legacy
        }

        fn speak(&mut self, _id: &UtteranceId, text: &str, interrupt: bool) -> DriverResult<()> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(Call::Speak(text.to_owned(), interrupt));
            state.speak_results.pop_front().unwrap_or(Ok(()))
        }

        fn stop(&mut self, _id: &UtteranceId) -> DriverResult<()> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(Call::Stop);
            state.stop_results.pop_front().unwrap_or(Ok(()))
        }

        fn pause(
            &mut self,
            _id: &UtteranceId,
        ) -> DriverResult<crate::speech::protocol::PauseResult> {
            Ok(crate::speech::protocol::PauseResult {
                paused: false,
                position: None,
            })
        }

        fn resume(&mut self, _id: &UtteranceId) -> DriverResult<()> {
            Ok(())
        }

        fn set_rate(&mut self, rate: f32) -> DriverResult<f32> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(Call::SetRate(rate.to_bits()));
            let result = state.rate_results.pop_front().unwrap_or(Ok(()));
            if result.is_ok() {
                self.rate = rate;
            }
            result.map(|()| self.rate)
        }

        fn take_events(
            &mut self,
        ) -> DriverResult<Vec<crate::speech::protocol::SpeechEventNotification>> {
            Ok(Vec::new())
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
                        host: Box::new(FakeDriver {
                            state,
                            rate: 1.0,
                            capabilities: Default::default(),
                        }),
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
                    host: Box::new(FakeDriver {
                        state: Arc::clone(&self.active),
                        rate: 1.0,
                        capabilities: Default::default(),
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
            ["tts", "--parent-pid", &std::process::id().to_string(),]
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
    fn optional_rate_and_terminal_capabilities_degrade_without_blocking_startup() {
        let mut harness = Harness::new();
        let active = fake_state();
        active.lock().unwrap().legacy = false;
        harness.push_driver(Arc::clone(&active));

        harness.supervisor.set_rate(1.75).unwrap();
        harness.supervisor.start().unwrap();

        assert!(active.lock().unwrap().calls.is_empty());
        assert!(!harness.supervisor.supports_ordered_utterances());
        harness.supervisor.speak("one announcement", false).unwrap();
        assert_eq!(
            active.lock().unwrap().calls,
            [Call::Speak("one announcement".to_owned(), false)]
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
    fn prestart_speech_preserves_more_than_thirty_two_items_and_is_cleared_by_cancel() {
        let mut harness = Harness::new();
        for index in 0..64 {
            harness
                .supervisor
                .speak(&format!("message {index}"), false)
                .unwrap();
        }
        assert_eq!(harness.supervisor.pending_speech.len(), 64);
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
    fn prestart_pause_holds_buffered_speech_until_resume() {
        let mut harness = Harness::new();
        harness.supervisor.speak("held", false).unwrap();
        harness.supervisor.pause().unwrap();
        let active = fake_state();
        harness.push_driver(Arc::clone(&active));

        harness.supervisor.start().unwrap();
        assert_eq!(
            active.lock().unwrap().calls,
            [Call::SetRate(1.0f32.to_bits())]
        );

        harness.supervisor.resume().unwrap();
        assert_eq!(
            active.lock().unwrap().calls,
            [
                Call::SetRate(1.0f32.to_bits()),
                Call::Speak("held".to_owned(), false),
            ]
        );
    }

    #[test]
    fn noninterrupting_prestart_speech_replaces_a_suspended_buffer() {
        let mut harness = Harness::new();
        harness.supervisor.speak("discarded", false).unwrap();
        harness.supervisor.pause().unwrap();
        harness.supervisor.speak("replacement", false).unwrap();
        let active = fake_state();
        harness.push_driver(Arc::clone(&active));

        harness.supervisor.start().unwrap();

        assert_eq!(
            active.lock().unwrap().calls,
            [
                Call::SetRate(1.0f32.to_bits()),
                Call::Speak("replacement".to_owned(), false),
            ]
        );
    }

    #[test]
    fn legacy_queue_is_not_paragraph_ordering_evidence() {
        let mut harness = Harness::new();
        let active = fake_state();
        harness.push_driver(active);

        harness.supervisor.start().unwrap();

        assert!(!harness.supervisor.supports_ordered_utterances());
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
                Call::Speak("later".to_owned(), false),
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
