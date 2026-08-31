//! Bounded asynchronous isolation for speech backends which may block.
//!
//! The terminal event loop owns all screen and tmux state.  A speech backend
//! is an external side effect and must never be allowed to stall that owner.

use super::protocol::UtteranceId;
use super::{CapabilityStatus, Driver, OptionState, SetOptionOutcome, UtteranceBoundary};
use anyhow::{Result as DriverResult, anyhow};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use super::SpeechServerSpec;

const MAX_PENDING_SPEECH_BYTES: usize = 256 * 1024;
const MAX_SPEECH_ITEM_BYTES: usize = 64 * 1024;
const TRUNCATION_SUFFIX: &str = " … speech truncated";
const WORKER_SHUTDOWN_GRACE: Duration = Duration::from_millis(100);
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(10);

enum Request {
    Speak {
        id: UtteranceId,
        text: String,
        interrupt: bool,
        boundary: UtteranceBoundary,
    },
    Cancel,
    Pause,
    Resume,
    Toggle,
    SetRate(f32),
    SetPitch(f32),
    SetVolume(f32),
    SetVoice(String),
    ConfigureServer(SpeechServerSpec),
    Start(mpsc::SyncSender<std::result::Result<(), String>>),
}

impl Request {
    fn speech_bytes(&self) -> usize {
        match self {
            Self::Speak { id, text, .. } => std::mem::size_of::<Self>()
                .saturating_add(id.as_str().len())
                .saturating_add(text.len()),
            Self::Cancel
            | Self::Pause
            | Self::Resume
            | Self::Toggle
            | Self::SetRate(_)
            | Self::SetPitch(_)
            | Self::SetVolume(_)
            | Self::SetVoice(_)
            | Self::ConfigureServer(_)
            | Self::Start(_) => 0,
        }
    }

    fn is_speech(&self) -> bool {
        matches!(self, Self::Speak { .. })
    }
}

#[derive(Default)]
struct MailboxState {
    requests: VecDeque<Request>,
    speech_items: usize,
    speech_bytes: usize,
    dropped_speech_items: u64,
    shutdown: bool,
}

struct Mailbox {
    state: Mutex<MailboxState>,
    available: Condvar,
}

impl Mailbox {
    fn new() -> Self {
        Self {
            state: Mutex::new(MailboxState::default()),
            available: Condvar::new(),
        }
    }

    fn lock(&self) -> DriverResult<MutexGuard<'_, MailboxState>> {
        self.state
            .lock()
            .map_err(|_| anyhow!("speech worker mailbox is poisoned"))
    }

    fn remove_request(state: &mut MailboxState, index: usize) -> Option<Request> {
        let request = state.requests.remove(index)?;
        if request.is_speech() {
            state.speech_items = state.speech_items.saturating_sub(1);
            state.speech_bytes = state.speech_bytes.saturating_sub(request.speech_bytes());
        }
        Some(request)
    }

    fn discard_speech(state: &mut MailboxState) -> usize {
        let mut discarded = 0usize;
        let mut index = 0;
        while index < state.requests.len() {
            if state.requests[index].is_speech() {
                let _ = Self::remove_request(state, index);
                discarded = discarded.saturating_add(1);
            } else {
                index += 1;
            }
        }
        discarded
    }

    fn enqueue_speech(&self, id: &UtteranceId, text: &str, interrupt: bool) -> DriverResult<()> {
        self.enqueue_speech_with_boundary(id, text, interrupt, UtteranceBoundary::Immediate)
    }

    fn enqueue_speech_with_boundary(
        &self,
        id: &UtteranceId,
        text: &str,
        interrupt: bool,
        boundary: UtteranceBoundary,
    ) -> DriverResult<()> {
        let text = bounded_text(text, MAX_SPEECH_ITEM_BYTES);
        let request = Request::Speak {
            id: id.clone(),
            text,
            interrupt,
            boundary,
        };
        let speech_bytes = request.speech_bytes();
        let mut state = self.lock()?;
        if state.shutdown {
            return Ok(());
        }

        if interrupt {
            let _ = Self::discard_speech(&mut state);
            state.requests.retain(|request| {
                !matches!(
                    request,
                    Request::Cancel | Request::Pause | Request::Resume | Request::Toggle
                )
            });
        }
        let mut dropped = 0usize;
        while state.speech_bytes.saturating_add(speech_bytes) > MAX_PENDING_SPEECH_BYTES {
            let Some(index) = state.requests.iter().position(Request::is_speech) else {
                break;
            };
            let _ = Self::remove_request(&mut state, index);
            state.dropped_speech_items = state.dropped_speech_items.saturating_add(1);
            dropped = dropped.saturating_add(1);
        }

        state.speech_items = state.speech_items.saturating_add(1);
        state.speech_bytes = state.speech_bytes.saturating_add(speech_bytes);
        state.requests.push_back(request);
        let dropped_total = state.dropped_speech_items;
        drop(state);
        self.available.notify_one();

        if dropped != 0 && dropped_total.is_power_of_two() {
            crate::diagnostics::event(
                "speech-worker",
                "queue-saturated",
                &format!("dropped_total={dropped_total}"),
            );
        }
        Ok(())
    }

    fn enqueue_cancel(&self) -> DriverResult<()> {
        let mut state = self.lock()?;
        if state.shutdown {
            return Ok(());
        }
        let _ = Self::discard_speech(&mut state);
        state.requests.retain(|request| {
            !matches!(
                request,
                Request::Cancel | Request::Pause | Request::Resume | Request::Toggle
            )
        });
        state.requests.push_front(Request::Cancel);
        drop(state);
        self.available.notify_one();
        Ok(())
    }

    fn enqueue_pause(&self) -> DriverResult<()> {
        let mut state = self.lock()?;
        if state.shutdown {
            return Ok(());
        }
        if state
            .requests
            .iter()
            .any(|request| matches!(request, Request::Cancel))
        {
            // A preceding cancellation makes this pause inert even if the
            // worker has not observed the cancellation yet.
            return Ok(());
        }
        if !matches!(state.requests.back(), Some(Request::Pause)) {
            state.requests.push_back(Request::Pause);
        }
        drop(state);
        self.available.notify_one();
        Ok(())
    }

    fn enqueue_resume(&self) -> DriverResult<()> {
        let mut state = self.lock()?;
        if state.shutdown {
            return Ok(());
        }
        if state
            .requests
            .iter()
            .any(|request| matches!(request, Request::Cancel))
        {
            return Ok(());
        }
        if !matches!(state.requests.back(), Some(Request::Resume)) {
            state.requests.push_back(Request::Resume);
        }
        drop(state);
        self.available.notify_one();
        Ok(())
    }

    fn enqueue_toggle(&self) -> DriverResult<()> {
        let mut state = self.lock()?;
        if state.shutdown {
            return Ok(());
        }
        if state
            .requests
            .iter()
            .any(|request| matches!(request, Request::Cancel))
        {
            return Ok(());
        }
        // Preserve request order so a toggle submitted just after speech sees
        // that speech rather than an apparently idle worker.
        state.requests.push_back(Request::Toggle);
        drop(state);
        self.available.notify_one();
        Ok(())
    }

    fn enqueue_rate(&self, rate: f32) -> DriverResult<()> {
        let mut state = self.lock()?;
        if state.shutdown {
            return Ok(());
        }
        state
            .requests
            .retain(|request| !matches!(request, Request::SetRate(_)));
        state.requests.push_front(Request::SetRate(rate));
        drop(state);
        self.available.notify_one();
        Ok(())
    }

    fn enqueue_pitch(&self, pitch: f32) -> DriverResult<()> {
        let mut state = self.lock()?;
        if state.shutdown {
            return Ok(());
        }
        state
            .requests
            .retain(|request| !matches!(request, Request::SetPitch(_)));
        state.requests.push_front(Request::SetPitch(pitch));
        drop(state);
        self.available.notify_one();
        Ok(())
    }

    fn enqueue_volume(&self, volume: f32) -> DriverResult<()> {
        let mut state = self.lock()?;
        if state.shutdown {
            return Ok(());
        }
        state
            .requests
            .retain(|request| !matches!(request, Request::SetVolume(_)));
        state.requests.push_front(Request::SetVolume(volume));
        drop(state);
        self.available.notify_one();
        Ok(())
    }

    fn enqueue_voice(&self, voice_id: String) -> DriverResult<()> {
        let mut state = self.lock()?;
        if state.shutdown {
            return Ok(());
        }
        state
            .requests
            .retain(|request| !matches!(request, Request::SetVoice(_)));
        state.requests.push_front(Request::SetVoice(voice_id));
        drop(state);
        self.available.notify_one();
        Ok(())
    }

    fn enqueue_server(&self, spec: SpeechServerSpec) -> DriverResult<()> {
        let mut state = self.lock()?;
        if state.shutdown {
            return Ok(());
        }
        state
            .requests
            .retain(|request| !matches!(request, Request::ConfigureServer(_)));
        state.requests.push_front(Request::ConfigureServer(spec));
        drop(state);
        self.available.notify_one();
        Ok(())
    }

    fn enqueue_start(
        &self,
        completed: mpsc::SyncSender<std::result::Result<(), String>>,
    ) -> DriverResult<()> {
        let mut state = self.lock()?;
        if state.shutdown {
            return Err(anyhow!("speech worker has shut down"));
        }
        // Configuration and speech emitted while init.lua was loading must be
        // observed before this activation fence.
        state.requests.push_back(Request::Start(completed));
        drop(state);
        self.available.notify_one();
        Ok(())
    }

    fn next_request(&self, timeout: Duration) -> NextRequest {
        let Ok(mut state) = self.state.lock() else {
            return NextRequest::Shutdown;
        };
        if state.requests.is_empty() && !state.shutdown {
            let Ok((next, _)) = self.available.wait_timeout(state, timeout) else {
                return NextRequest::Shutdown;
            };
            state = next;
        }
        if state.shutdown {
            return NextRequest::Shutdown;
        }
        let Some(request) = state.requests.pop_front() else {
            return NextRequest::Idle;
        };
        if request.is_speech() {
            state.speech_items = state.speech_items.saturating_sub(1);
            state.speech_bytes = state.speech_bytes.saturating_sub(request.speech_bytes());
        }
        NextRequest::Request(request)
    }

    fn shut_down(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.shutdown = true;
            state.requests.clear();
            state.speech_items = 0;
            state.speech_bytes = 0;
        }
        self.available.notify_all();
    }

    #[cfg(test)]
    fn usage(&self) -> (usize, usize, u64) {
        let state = self.state.lock().expect("speech mailbox lock");
        (
            state.speech_items,
            state.speech_bytes,
            state.dropped_speech_items,
        )
    }
}

enum NextRequest {
    Request(Request),
    Idle,
    Shutdown,
}

/// Moves a potentially blocking speech driver off the terminal event loop.
///
/// The worker owns the backend. The foreground owns only a bounded mailbox and
/// never waits for backend I/O. Speech is lossy under overload by design:
/// controls are coalesced and the newest announcements replace stale queued
/// announcements.
pub struct BoundedAsyncDriver {
    mailbox: Arc<Mailbox>,
    ordered_utterances: Arc<AtomicBool>,
    option_state: Arc<Mutex<OptionState>>,
    rate: f32,
    worker: Option<thread::JoinHandle<()>>,
    shutdown_backend: Option<Box<dyn FnOnce() + Send>>,
    next_compatibility_id: u64,
}

impl BoundedAsyncDriver {
    pub fn new<D>(driver: D) -> std::io::Result<Self>
    where
        D: Driver + Send + 'static,
    {
        Self::new_inner(driver, None)
    }

    pub fn new_with_shutdown<D, F>(driver: D, shutdown_backend: F) -> std::io::Result<Self>
    where
        D: Driver + Send + 'static,
        F: FnOnce() + Send + 'static,
    {
        Self::new_inner(driver, Some(Box::new(shutdown_backend)))
    }

    fn new_inner<D>(
        driver: D,
        shutdown_backend: Option<Box<dyn FnOnce() + Send>>,
    ) -> std::io::Result<Self>
    where
        D: Driver + Send + 'static,
    {
        let rate = driver.get_rate();
        let initial_option_state = driver.option_state();
        let ordered_utterances = Arc::new(AtomicBool::new(driver.supports_ordered_utterances()));
        let option_state = Arc::new(Mutex::new(initial_option_state));
        let mailbox = Arc::new(Mailbox::new());
        let worker_mailbox = Arc::clone(&mailbox);
        let worker_ordered_utterances = Arc::clone(&ordered_utterances);
        let worker_option_state = Arc::clone(&option_state);
        let worker = thread::Builder::new()
            .name("lector-speech-driver".to_owned())
            .spawn(move || {
                run_worker(
                    driver,
                    &worker_mailbox,
                    &worker_ordered_utterances,
                    &worker_option_state,
                );
            })?;
        Ok(Self {
            mailbox,
            ordered_utterances,
            option_state,
            rate,
            worker: Some(worker),
            shutdown_backend,
            next_compatibility_id: 1,
        })
    }
}

impl Driver for BoundedAsyncDriver {
    fn speak(&mut self, text: &str, interrupt: bool) -> DriverResult<()> {
        let id = UtteranceId::new(format!("compat-{}", self.next_compatibility_id));
        self.next_compatibility_id = self.next_compatibility_id.wrapping_add(1);
        self.mailbox.enqueue_speech(&id, text, interrupt)
    }

    fn speak_utterance(
        &mut self,
        id: &UtteranceId,
        text: &str,
        interrupt: bool,
    ) -> DriverResult<()> {
        self.mailbox.enqueue_speech(id, text, interrupt)
    }

    fn speak_utterance_with_boundary(
        &mut self,
        id: &UtteranceId,
        text: &str,
        interrupt: bool,
        boundary: UtteranceBoundary,
    ) -> DriverResult<()> {
        self.mailbox
            .enqueue_speech_with_boundary(id, text, interrupt, boundary)
    }

    fn stop(&mut self) -> DriverResult<()> {
        self.mailbox.enqueue_cancel()
    }

    fn pause(&mut self) -> DriverResult<()> {
        self.mailbox.enqueue_pause()
    }

    fn resume(&mut self) -> DriverResult<()> {
        self.mailbox.enqueue_resume()
    }

    fn toggle(&mut self) -> DriverResult<()> {
        self.mailbox.enqueue_toggle()
    }

    fn supports_ordered_utterances(&self) -> bool {
        self.ordered_utterances.load(Ordering::Acquire)
    }

    fn get_rate(&self) -> f32 {
        self.rate
    }

    fn set_rate(&mut self, rate: f32) -> DriverResult<()> {
        if !rate.is_finite() {
            return Err(anyhow!("speech rate must be finite"));
        }
        self.mailbox.enqueue_rate(rate)?;
        self.rate = rate;
        Ok(())
    }

    fn option_state(&self) -> OptionState {
        self.option_state
            .lock()
            .map(|state| state.clone())
            .unwrap_or_default()
    }

    fn set_rate_option(&mut self, rate: f32) -> DriverResult<SetOptionOutcome> {
        if !rate.is_finite() {
            return Err(anyhow!("speech rate must be finite"));
        }
        let status = self
            .option_state
            .lock()
            .map(|state| state.rate_status)
            .unwrap_or(CapabilityStatus::Unknown);
        if status == CapabilityStatus::Unsupported {
            return Ok(SetOptionOutcome::Unsupported);
        }
        self.mailbox.enqueue_rate(rate)?;
        self.rate = rate;
        if status == CapabilityStatus::Supported
            && let Ok(mut state) = self.option_state.lock()
        {
            state.rate = Some(rate);
        }
        Ok(SetOptionOutcome::Accepted)
    }

    fn set_pitch_option(&mut self, pitch: f32) -> DriverResult<SetOptionOutcome> {
        if !pitch.is_finite() {
            return Err(anyhow!("speech pitch must be finite"));
        }
        let status = self
            .option_state
            .lock()
            .map(|state| state.pitch_status)
            .unwrap_or(CapabilityStatus::Unknown);
        if status == CapabilityStatus::Unsupported {
            return Ok(SetOptionOutcome::Unsupported);
        }
        self.mailbox.enqueue_pitch(pitch)?;
        if status == CapabilityStatus::Supported
            && let Ok(mut state) = self.option_state.lock()
        {
            state.pitch = Some(pitch);
        }
        Ok(SetOptionOutcome::Accepted)
    }

    fn set_volume_option(&mut self, volume: f32) -> DriverResult<SetOptionOutcome> {
        if !volume.is_finite() {
            return Err(anyhow!("speech volume must be finite"));
        }
        let status = self
            .option_state
            .lock()
            .map(|state| state.volume_status)
            .unwrap_or(CapabilityStatus::Unknown);
        if status == CapabilityStatus::Unsupported {
            return Ok(SetOptionOutcome::Unsupported);
        }
        self.mailbox.enqueue_volume(volume)?;
        if status == CapabilityStatus::Supported
            && let Ok(mut state) = self.option_state.lock()
        {
            state.volume = Some(volume);
        }
        Ok(SetOptionOutcome::Accepted)
    }

    fn set_voice_option(&mut self, voice_id: &str) -> DriverResult<SetOptionOutcome> {
        if voice_id.is_empty() {
            return Err(anyhow!("speech voice ID must not be empty"));
        }
        let status = self
            .option_state
            .lock()
            .map(|state| state.voice_selection_status)
            .unwrap_or(CapabilityStatus::Unknown);
        if status == CapabilityStatus::Unsupported {
            return Ok(SetOptionOutcome::Unsupported);
        }
        self.mailbox.enqueue_voice(voice_id.to_owned())?;
        Ok(SetOptionOutcome::Accepted)
    }

    fn start(&mut self) -> DriverResult<()> {
        let (completed_tx, completed_rx) = mpsc::sync_channel(1);
        self.mailbox.enqueue_start(completed_tx)?;
        completed_rx
            .recv_timeout(Duration::from_secs(15))
            .map_err(|error| anyhow!("wait for speech backend startup: {error}"))?
            .map_err(anyhow::Error::msg)
    }

    fn configure_server(&mut self, spec: SpeechServerSpec) -> DriverResult<()> {
        // Until the candidate commits, use the conservative one-utterance
        // presentation path. This prevents foreground speech from being split
        // according to capabilities that belonged to the old generation.
        self.ordered_utterances.store(false, Ordering::Release);
        self.mailbox.enqueue_server(spec)
    }

    fn shutdown(&mut self) {
        self.shutdown_inner();
    }
}

impl Drop for BoundedAsyncDriver {
    fn drop(&mut self) {
        self.shutdown_inner();
    }
}

impl BoundedAsyncDriver {
    fn shutdown_inner(&mut self) {
        self.mailbox.shut_down();
        if let Some(shutdown_backend) = self.shutdown_backend.take() {
            shutdown_backend();
        }
        let Some(worker) = self.worker.take() else {
            return;
        };
        // A backend can be permanently stuck in foreign code or pipe I/O.
        // Never move that failure back onto the event loop during teardown.
        // Give a killed process backend a small, bounded window to unwind and
        // reap its child. A backend stuck in arbitrary foreign code is still
        // detached after the deadline, so shutdown cannot hang Lector.
        let deadline = std::time::Instant::now() + WORKER_SHUTDOWN_GRACE;
        while !worker.is_finished() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        if worker.is_finished() {
            let _ = worker.join();
        }
    }
}

fn run_worker(
    mut driver: impl Driver,
    mailbox: &Mailbox,
    ordered_utterances: &AtomicBool,
    option_state: &Mutex<OptionState>,
) {
    let mut failures = 0u64;
    loop {
        let result = match mailbox.next_request(EVENT_POLL_INTERVAL) {
            NextRequest::Request(Request::Speak {
                id,
                text,
                interrupt,
                boundary,
            }) => driver.speak_utterance_with_boundary(&id, &text, interrupt, boundary),
            NextRequest::Request(Request::Cancel) => driver.stop(),
            NextRequest::Request(Request::Pause) => driver.pause(),
            NextRequest::Request(Request::Resume) => driver.resume(),
            NextRequest::Request(Request::Toggle) => driver.toggle(),
            NextRequest::Request(Request::SetRate(rate)) => driver.set_rate(rate),
            NextRequest::Request(Request::SetPitch(pitch)) => {
                driver.set_pitch_option(pitch).map(|_| ())
            }
            NextRequest::Request(Request::SetVolume(volume)) => {
                driver.set_volume_option(volume).map(|_| ())
            }
            NextRequest::Request(Request::SetVoice(voice_id)) => {
                driver.set_voice_option(&voice_id).map(|_| ())
            }
            NextRequest::Request(Request::ConfigureServer(spec)) => driver.configure_server(spec),
            NextRequest::Request(Request::Start(completed)) => {
                let result = driver.start();
                ordered_utterances.store(driver.supports_ordered_utterances(), Ordering::Release);
                if let Ok(mut published) = option_state.lock() {
                    *published = driver.option_state();
                }
                let report = match &result {
                    Ok(()) => Ok(()),
                    Err(error) => Err(format!("{error:#}")),
                };
                let _ = completed.send(report);
                result
            }
            NextRequest::Idle => driver.poll(),
            NextRequest::Shutdown => break,
        };
        ordered_utterances.store(driver.supports_ordered_utterances(), Ordering::Release);
        if let Ok(mut published) = option_state.lock() {
            *published = driver.option_state();
        }
        if let Err(error) = result {
            failures = failures.saturating_add(1);
            if failures.is_power_of_two() {
                crate::diagnostics::event(
                    "speech-worker",
                    "backend-error",
                    &format!("failures={failures} error={error:#}"),
                );
            }
        }
    }
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
    use super::{
        BoundedAsyncDriver, CapabilityStatus, Driver, MAX_PENDING_SPEECH_BYTES,
        MAX_SPEECH_ITEM_BYTES, Mailbox, NextRequest, OptionState, Request, UtteranceId,
    };
    use std::{
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    struct BlockingDriver {
        started: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
    }

    impl Driver for BlockingDriver {
        fn speak(&mut self, _text: &str, _interrupt: bool) -> anyhow::Result<()> {
            let _ = self.started.send(());
            let _ = self.release.recv();
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

    #[test]
    fn blocked_backend_cannot_block_the_caller_or_grow_the_mailbox() {
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (shutdown_tx, shutdown_rx) = mpsc::sync_channel(1);
        let mut driver = BoundedAsyncDriver::new_with_shutdown(
            BlockingDriver {
                started: started_tx,
                release: release_rx,
            },
            move || {
                let _ = release_tx.send(());
                let _ = shutdown_tx.send(());
            },
        )
        .unwrap();
        driver.speak("block", false).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let start = Instant::now();
        for _ in 0..1_000 {
            driver
                .speak(&"x".repeat(MAX_SPEECH_ITEM_BYTES * 2), false)
                .unwrap();
        }
        assert!(start.elapsed() < Duration::from_secs(1));
        let (_, bytes, dropped) = driver.mailbox.usage();
        assert!(bytes <= MAX_PENDING_SPEECH_BYTES);
        assert!(dropped > 0);

        let drop_started = Instant::now();
        drop(driver);
        assert!(drop_started.elapsed() < Duration::from_millis(100));
        shutdown_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn blocked_backend_preserves_more_than_thirty_two_small_announcements() {
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let mut driver = BoundedAsyncDriver::new(BlockingDriver {
            started: started_tx,
            release: release_rx,
        })
        .unwrap();
        driver.speak("block", false).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        for index in 0..64 {
            driver.speak(&format!("message {index}"), false).unwrap();
        }
        let (items, bytes, dropped) = driver.mailbox.usage();
        assert_eq!(items, 64);
        assert!(bytes < MAX_PENDING_SPEECH_BYTES);
        assert_eq!(dropped, 0);

        release_tx.send(()).unwrap();
    }

    #[test]
    fn interrupt_and_stop_discard_stale_speech_without_dropping_controls() {
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let mut driver = BoundedAsyncDriver::new(BlockingDriver {
            started: started_tx,
            release: release_rx,
        })
        .unwrap();
        driver.speak("block", false).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        driver.speak("stale one", false).unwrap();
        driver.speak("stale two", false).unwrap();
        driver.set_rate(1.5).unwrap();
        driver.speak("new", true).unwrap();
        assert_eq!(driver.mailbox.usage().0, 1);
        driver.stop().unwrap();
        assert_eq!(driver.mailbox.usage().0, 0);

        release_tx.send(()).unwrap();
        // Give the worker time to observe the queued controls before its owner
        // is dropped. This is not needed for correctness, only clean test exit.
        thread::sleep(Duration::from_millis(10));
    }

    #[test]
    fn truncation_preserves_utf8_and_numeric_setting_validation_is_local() {
        let mailbox = Mailbox::new();
        mailbox
            .enqueue_speech(
                &UtteranceId::new("long"),
                &"é".repeat(MAX_SPEECH_ITEM_BYTES),
                false,
            )
            .unwrap();
        let NextRequest::Request(Request::Speak { text, .. }) =
            mailbox.next_request(Duration::ZERO)
        else {
            panic!("bounded speech request was not queued");
        };
        assert!(text.len() <= MAX_SPEECH_ITEM_BYTES);
        assert!(text.is_char_boundary(text.len()));

        let (started_tx, _started_rx) = mpsc::sync_channel(1);
        let (_release_tx, release_rx) = mpsc::sync_channel(1);
        let mut driver = BoundedAsyncDriver::new(BlockingDriver {
            started: started_tx,
            release: release_rx,
        })
        .unwrap();
        assert!(driver.set_rate(f32::NAN).is_err());
        assert!(driver.set_pitch_option(f32::INFINITY).is_err());
        assert!(driver.set_volume_option(f32::NEG_INFINITY).is_err());
        assert_eq!(driver.get_rate(), 1.0);
    }

    #[test]
    fn ordered_utterance_capability_is_published_after_backend_start() {
        struct CapabilityDriver {
            started: bool,
        }

        impl Driver for CapabilityDriver {
            fn speak(&mut self, _text: &str, _interrupt: bool) -> anyhow::Result<()> {
                Ok(())
            }

            fn stop(&mut self) -> anyhow::Result<()> {
                Ok(())
            }

            fn supports_ordered_utterances(&self) -> bool {
                self.started
            }

            fn get_rate(&self) -> f32 {
                1.0
            }

            fn set_rate(&mut self, _rate: f32) -> anyhow::Result<()> {
                Ok(())
            }

            fn option_state(&self) -> OptionState {
                if self.started {
                    OptionState {
                        rate_status: CapabilityStatus::Unsupported,
                        voice_status: CapabilityStatus::Unsupported,
                        voice_selection_status: CapabilityStatus::Unsupported,
                        ..OptionState::default()
                    }
                } else {
                    OptionState::default()
                }
            }

            fn start(&mut self) -> anyhow::Result<()> {
                self.started = true;
                Ok(())
            }
        }

        let mut driver = BoundedAsyncDriver::new(CapabilityDriver { started: false }).unwrap();
        assert!(!driver.supports_ordered_utterances());
        assert_eq!(driver.option_state().rate_status, CapabilityStatus::Unknown);
        driver.start().unwrap();
        assert!(driver.supports_ordered_utterances());
        assert_eq!(
            driver.option_state().rate_status,
            CapabilityStatus::Unsupported
        );
    }

    #[test]
    fn cancellation_barriers_cannot_be_overtaken_by_pause_toggles() {
        let mailbox = Mailbox::new();
        mailbox.enqueue_cancel().unwrap();
        mailbox.enqueue_toggle().unwrap();
        assert!(matches!(
            mailbox.next_request(Duration::ZERO),
            NextRequest::Request(Request::Cancel)
        ));
        assert!(matches!(
            mailbox.next_request(Duration::ZERO),
            NextRequest::Idle
        ));

        let id = UtteranceId::new("new");
        mailbox.enqueue_speech(&id, "new words", true).unwrap();
        mailbox.enqueue_toggle().unwrap();
        assert!(matches!(
            mailbox.next_request(Duration::ZERO),
            NextRequest::Request(Request::Speak {
                interrupt: true,
                ..
            })
        ));
        assert!(matches!(
            mailbox.next_request(Duration::ZERO),
            NextRequest::Request(Request::Toggle)
        ));
    }

    #[test]
    fn playback_controls_observe_preceding_noninterrupting_speech() {
        let mailbox = Mailbox::new();
        let id = UtteranceId::new("new");
        mailbox.enqueue_speech(&id, "new words", false).unwrap();
        mailbox.enqueue_pause().unwrap();
        mailbox.enqueue_resume().unwrap();
        mailbox.enqueue_toggle().unwrap();

        assert!(matches!(
            mailbox.next_request(Duration::ZERO),
            NextRequest::Request(Request::Speak {
                interrupt: false,
                ..
            })
        ));
        assert!(matches!(
            mailbox.next_request(Duration::ZERO),
            NextRequest::Request(Request::Pause)
        ));
        assert!(matches!(
            mailbox.next_request(Duration::ZERO),
            NextRequest::Request(Request::Resume)
        ));
        assert!(matches!(
            mailbox.next_request(Duration::ZERO),
            NextRequest::Request(Request::Toggle)
        ));
    }
}
