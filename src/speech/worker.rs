//! Bounded asynchronous isolation for speech backends which may block.
//!
//! The terminal event loop owns all screen and tmux state.  A speech backend
//! is an external side effect and must never be allowed to stall that owner.

use super::Driver;
use anyhow::{Result as DriverResult, anyhow};
use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex, MutexGuard},
    thread,
};

const MAX_PENDING_SPEECH_ITEMS: usize = 32;
const MAX_PENDING_SPEECH_BYTES: usize = 256 * 1024;
const MAX_SPEECH_ITEM_BYTES: usize = 64 * 1024;
const TRUNCATION_SUFFIX: &str = " … speech truncated";

enum Request {
    Speak { text: String, interrupt: bool },
    Stop,
    SetRate(f32),
}

impl Request {
    fn speech_bytes(&self) -> usize {
        match self {
            Self::Speak { text, .. } => text.len(),
            Self::Stop | Self::SetRate(_) => 0,
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

    fn enqueue_speech(&self, text: &str, interrupt: bool) -> DriverResult<()> {
        let text = bounded_text(text, MAX_SPEECH_ITEM_BYTES);
        let text_bytes = text.len();
        let mut state = self.lock()?;
        if state.shutdown {
            return Ok(());
        }

        if interrupt {
            let _ = Self::discard_speech(&mut state);
        }
        let mut dropped = 0usize;
        while state.speech_items == MAX_PENDING_SPEECH_ITEMS
            || state.speech_bytes.saturating_add(text_bytes) > MAX_PENDING_SPEECH_BYTES
        {
            let Some(index) = state.requests.iter().position(Request::is_speech) else {
                break;
            };
            let _ = Self::remove_request(&mut state, index);
            state.dropped_speech_items = state.dropped_speech_items.saturating_add(1);
            dropped = dropped.saturating_add(1);
        }

        state.speech_items = state.speech_items.saturating_add(1);
        state.speech_bytes = state.speech_bytes.saturating_add(text_bytes);
        state.requests.push_back(Request::Speak { text, interrupt });
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

    fn enqueue_stop(&self) -> DriverResult<()> {
        let mut state = self.lock()?;
        if state.shutdown {
            return Ok(());
        }
        let _ = Self::discard_speech(&mut state);
        state
            .requests
            .retain(|request| !matches!(request, Request::Stop));
        state.requests.push_front(Request::Stop);
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

    fn next_request(&self) -> Option<Request> {
        let mut state = self.state.lock().ok()?;
        while state.requests.is_empty() && !state.shutdown {
            state = self.available.wait(state).ok()?;
        }
        if state.shutdown {
            return None;
        }
        let request = state.requests.pop_front()?;
        if request.is_speech() {
            state.speech_items = state.speech_items.saturating_sub(1);
            state.speech_bytes = state.speech_bytes.saturating_sub(request.speech_bytes());
        }
        Some(request)
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

/// Moves a potentially blocking speech driver off the terminal event loop.
///
/// The worker owns the backend. The foreground owns only a bounded mailbox and
/// never waits for backend I/O. Speech is lossy under overload by design:
/// controls are coalesced and the newest announcements replace stale queued
/// announcements.
pub struct BoundedAsyncDriver {
    mailbox: Arc<Mailbox>,
    rate: f32,
    worker: Option<thread::JoinHandle<()>>,
    shutdown_backend: Option<Box<dyn FnOnce() + Send>>,
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
        let mailbox = Arc::new(Mailbox::new());
        let worker_mailbox = Arc::clone(&mailbox);
        let worker = thread::Builder::new()
            .name("lector-speech-driver".to_owned())
            .spawn(move || run_worker(driver, &worker_mailbox))?;
        Ok(Self {
            mailbox,
            rate,
            worker: Some(worker),
            shutdown_backend,
        })
    }
}

impl Driver for BoundedAsyncDriver {
    fn speak(&mut self, text: &str, interrupt: bool) -> DriverResult<()> {
        self.mailbox.enqueue_speech(text, interrupt)
    }

    fn stop(&mut self) -> DriverResult<()> {
        self.mailbox.enqueue_stop()
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
}

impl Drop for BoundedAsyncDriver {
    fn drop(&mut self) {
        self.mailbox.shut_down();
        if let Some(shutdown_backend) = self.shutdown_backend.take() {
            shutdown_backend();
        }
        let Some(worker) = self.worker.take() else {
            return;
        };
        // A backend can be permanently stuck in foreign code or pipe I/O.
        // Never move that failure back onto the event loop during teardown.
        // If it has already stopped, reap it; otherwise detaching is bounded
        // and process exit will reclaim the one process-lifetime worker.
        if worker.is_finished() {
            let _ = worker.join();
        }
    }
}

fn run_worker(mut driver: impl Driver, mailbox: &Mailbox) {
    let mut failures = 0u64;
    while let Some(request) = mailbox.next_request() {
        let result = match request {
            Request::Speak { text, interrupt } => driver.speak(&text, interrupt),
            Request::Stop => driver.stop(),
            Request::SetRate(rate) => driver.set_rate(rate),
        };
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
        BoundedAsyncDriver, Driver, MAX_PENDING_SPEECH_BYTES, MAX_PENDING_SPEECH_ITEMS,
        MAX_SPEECH_ITEM_BYTES,
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
        let (items, bytes, dropped) = driver.mailbox.usage();
        assert!(items <= MAX_PENDING_SPEECH_ITEMS);
        assert!(bytes <= MAX_PENDING_SPEECH_BYTES);
        assert!(dropped > 0);

        let drop_started = Instant::now();
        drop(driver);
        assert!(drop_started.elapsed() < Duration::from_millis(100));
        shutdown_rx.recv_timeout(Duration::from_secs(1)).unwrap();
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
    fn truncation_preserves_utf8_and_rate_validation_is_local() {
        let (started_tx, _started_rx) = mpsc::sync_channel(1);
        let (_release_tx, release_rx) = mpsc::sync_channel(1);
        let mut driver = BoundedAsyncDriver::new(BlockingDriver {
            started: started_tx,
            release: release_rx,
        })
        .unwrap();
        driver
            .speak(&"é".repeat(MAX_SPEECH_ITEM_BYTES), false)
            .unwrap();
        assert!(driver.mailbox.usage().1 <= MAX_SPEECH_ITEM_BYTES);
        assert!(driver.set_rate(f32::NAN).is_err());
        assert_eq!(driver.get_rate(), 1.0);
    }
}
