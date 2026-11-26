//! Admission and generation ownership for one NVDA speech backend.

use std::collections::VecDeque;

use parking_lot::{Condvar, Mutex};

use crate::{Error, protocol::SoundCueAt};

/// Serializes controller calls without making a waiting job depend on the
/// current owner returning. The monitor lock never spans a native call.
pub(super) struct CallGate {
    state: Mutex<GateState>,
    changed: Condvar,
}

struct GateState {
    owned: bool,
    poisoned: bool,
}

pub(super) enum AdmissionError {
    Cancelled,
    Poisoned,
}

pub(super) struct CallGuard<'a>(&'a CallGate);

impl CallGate {
    pub(super) const fn new() -> Self {
        Self {
            state: Mutex::new(GateState {
                owned: false,
                poisoned: false,
            }),
            changed: Condvar::new(),
        }
    }

    pub(super) fn acquire(
        &self,
        still_current: impl Fn() -> bool,
    ) -> Result<CallGuard<'_>, AdmissionError> {
        let mut state = self.state.lock();
        loop {
            if state.poisoned {
                return Err(AdmissionError::Poisoned);
            }
            if !still_current() {
                return Err(AdmissionError::Cancelled);
            }
            if !state.owned {
                state.owned = true;
                return Ok(CallGuard(self));
            }
            self.changed.wait(&mut state);
        }
    }

    pub(super) fn is_poisoned(&self) -> bool {
        self.state.lock().poisoned
    }

    pub(super) fn poison(&self) {
        self.state.lock().poisoned = true;
        self.changed.notify_all();
    }

    pub(super) fn notify_cancellation(&self) {
        // Acquire after invalidating the local generation and dropping its
        // lock. This pairs with acquire's predicate check and atomic wait.
        let _state = self.state.lock();
        self.changed.notify_all();
    }
}

impl Drop for CallGuard<'_> {
    fn drop(&mut self) {
        self.0.state.lock().owned = false;
        self.0.changed.notify_all();
    }
}

const MAX_PENDING: usize = 32;

pub(super) struct Job {
    pub(super) id: u64,
    pub(super) text: String,
    pub(super) cues: Vec<SoundCueAt>,
    generation: u64,
}

impl std::fmt::Debug for Job {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Job")
            .field("id", &self.id)
            .field("generation", &self.generation)
            .field("text_bytes", &self.text.len())
            .field("cue_count", &self.cues.len())
            .finish_non_exhaustive()
    }
}

impl Job {
    pub(super) fn label_fallback(&self, text: String) -> Self {
        Self {
            id: self.id,
            text,
            cues: Vec::new(),
            generation: self.generation,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct Schedule {
    generation: u64,
    queue: VecDeque<Job>,
    active: Option<Active>,
    unavailable: bool,
    cancel_in_flight: bool,
}

#[derive(Debug)]
struct Active {
    id: u64,
    generation: u64,
    speaking: bool,
}

impl Schedule {
    pub(super) fn submit(&mut self, id: u64, text: &str, cues: &[SoundCueAt]) -> Result<(), Error> {
        if self.unavailable {
            return Err(Error::BackendUnavailable(
                "NVDA did not return from cancellation",
            ));
        }
        if self.queue.len() >= MAX_PENDING {
            return Err(Error::OperationFailed("queue NVDA speech"));
        }
        self.queue.push_back(Job {
            id,
            generation: self.generation,
            text: text.to_owned(),
            cues: cues.to_vec(),
        });
        Ok(())
    }

    pub(super) fn undo_submit(&mut self) {
        self.queue.pop_back();
    }

    pub(super) fn next(&mut self) -> Option<Job> {
        if self.cancel_in_flight {
            return None;
        }
        self.queue.pop_front()
    }

    pub(super) fn can_start(&self, job: &Job) -> bool {
        !self.unavailable && !self.cancel_in_flight && self.generation == job.generation
    }

    pub(super) fn start(&mut self, job: &Job) -> bool {
        if !self.can_start(job) {
            return false;
        }
        self.active = Some(Active {
            id: job.id,
            generation: job.generation,
            speaking: false,
        });
        true
    }

    pub(super) fn is_current(&self, id: u64) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.id == id && active.generation == self.generation)
    }

    pub(super) fn begin_native(&mut self, id: u64) -> bool {
        if !self.is_current(id) {
            return false;
        }
        self.active
            .as_mut()
            .expect("matching active attempt")
            .speaking = true;
        true
    }

    pub(super) fn end_native(&mut self, id: u64) {
        if let Some(active) = &mut self.active
            && active.id == id
        {
            active.speaking = false;
        }
    }

    /// Returns current-generation status, or none if the failed RPC's
    /// terminal event was already delivered.
    pub(super) fn complete(&mut self, id: u64) -> Option<bool> {
        let active = self.active.as_ref()?;
        if active.id != id {
            return None;
        }
        self.active
            .take()
            .map(|active| active.generation == self.generation)
    }

    pub(super) fn cancel(&mut self) -> VecDeque<Job> {
        self.generation = self.generation.wrapping_add(1);
        std::mem::take(&mut self.queue)
    }

    pub(super) fn cancelled_native(&self) -> Option<u64> {
        self.active
            .as_ref()
            .filter(|active| active.speaking && active.generation != self.generation)
            .map(|active| active.id)
    }

    pub(super) fn reserve_cancel(&mut self) -> Option<u64> {
        if self.cancel_in_flight {
            return None;
        }
        let id = self.cancelled_native()?;
        self.cancel_in_flight = true;
        Some(id)
    }

    pub(super) fn cancel_in_flight(&self) -> bool {
        self.cancel_in_flight
    }

    pub(super) fn finish_cancel(&mut self) {
        self.cancel_in_flight = false;
    }

    pub(super) fn poison(&mut self) -> VecDeque<Job> {
        self.unavailable = true;
        self.finish_cancel();
        self.active = None;
        std::mem::take(&mut self.queue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_instance_can_report_terminal_while_first_keeps_native_ownership() {
        use std::{sync::mpsc::channel, thread, time::Duration};

        for poison in [false, true] {
            let gate = CallGate::new();
            let first = gate
                .acquire(|| true)
                .ok()
                .expect("first instance owns native calls");
            let second = Mutex::new(Schedule::default());
            second.lock().submit(2, "second", &[]).unwrap();
            let waiting_job = second.lock().next().unwrap();
            let (observed, waiting) = channel();
            let (finished, terminal) = channel();
            thread::scope(|threads| {
                threads.spawn(|| {
                    let result = gate.acquire(|| {
                        let _ = observed.send(());
                        second.lock().can_start(&waiting_job)
                    });
                    let error = result
                        .err()
                        .expect("second instance must not enter native call");
                    finished.send((waiting_job.id, error)).unwrap();
                });
                if let Err(error) = waiting.recv_timeout(Duration::from_secs(2)) {
                    drop(first);
                    panic!("second instance did not reach admission: {error}");
                }
                if poison {
                    gate.poison();
                    assert!(gate.is_poisoned());
                } else {
                    assert!(second.lock().cancel().is_empty());
                    gate.notify_cancellation();
                }
                let result = terminal.recv_timeout(Duration::from_secs(2));
                drop(first);
                let (id, error) = result.unwrap();
                assert_eq!(id, 2);
                if poison {
                    assert!(matches!(error, AdmissionError::Poisoned));
                } else {
                    assert!(matches!(error, AdmissionError::Cancelled));
                }
                assert_eq!(second.lock().complete(id), None);
            });
        }
    }

    #[test]
    fn failed_wake_undoes_admission_without_losing_prior_work() {
        let mut schedule = Schedule::default();
        let cue = SoundCueAt {
            cue: crate::protocol::SoundCue::Link,
            offset: 0,
        };
        schedule.submit(1, "linked", &[cue]).unwrap();
        schedule.submit(2, "lost sender", &[]).unwrap();
        schedule.undo_submit();
        let job = schedule.next().unwrap();
        assert_eq!(job.id, 1);
        assert_eq!(job.text, "linked");
        assert_eq!(job.cues, [cue]);
        assert!(schedule.next().is_none());
        let fallback = job.label_fallback("link".to_owned());
        assert_eq!(fallback.id, job.id);
        assert_eq!(fallback.text, "link");
        assert_eq!(fallback.cues, []);
        assert!(schedule.start(&job));
        assert!(schedule.begin_native(fallback.id));
    }

    #[test]
    fn cancel_discards_pending_and_never_retries_against_the_replacement() {
        let mut schedule = Schedule::default();
        schedule.submit(1, "active", &[]).unwrap();
        schedule.submit(2, "old pending", &[]).unwrap();
        let active = schedule.next().unwrap();
        assert!(schedule.start(&active));
        assert!(schedule.begin_native(active.id));
        assert_eq!(schedule.cancel().front().unwrap().id, 2);
        assert!(!schedule.is_current(active.id));
        assert_eq!(schedule.cancelled_native(), Some(1));
        schedule.submit(3, "replacement", &[]).unwrap();
        assert_eq!(schedule.complete(1), Some(false));
        let next = schedule.next().unwrap();
        assert!(schedule.start(&next));
        assert!(schedule.begin_native(next.id));
        assert_eq!(schedule.cancelled_native(), None);
        assert_eq!(schedule.complete(1), None);
        assert_eq!(schedule.complete(3), Some(true));
    }

    #[test]
    fn cancel_never_targets_nvda_for_standalone_or_drained_speech() {
        let mut schedule = Schedule::default();
        schedule.submit(1, "standalone", &[]).unwrap();
        let standalone = schedule.next().unwrap();
        assert!(schedule.start(&standalone));
        assert!(schedule.cancel().is_empty());
        assert_eq!(schedule.cancelled_native(), None);
        assert!(!schedule.begin_native(standalone.id));
        assert_eq!(schedule.complete(1), Some(false));

        schedule.submit(2, "draining", &[]).unwrap();
        let draining = schedule.next().unwrap();
        assert!(schedule.start(&draining));
        assert!(schedule.begin_native(draining.id));
        schedule.end_native(draining.id);
        schedule.cancel();
        assert_eq!(schedule.cancelled_native(), None);
    }

    #[test]
    fn replacement_waits_for_cancellation_reply_even_after_speech_returns() {
        let mut schedule = Schedule::default();
        schedule.submit(1, "speaking", &[]).unwrap();
        let active = schedule.next().unwrap();
        assert!(schedule.start(&active));
        assert!(schedule.begin_native(active.id));
        schedule.cancel();
        assert_eq!(schedule.reserve_cancel(), Some(active.id));
        assert!(schedule.cancel_in_flight());
        assert_eq!(schedule.complete(active.id), Some(false));
        schedule.submit(2, "next", &[]).unwrap();
        assert!(schedule.next().is_none());
        schedule.finish_cancel();
        assert_eq!(schedule.next().unwrap().id, 2);
    }

    #[test]
    fn cancellation_timeout_still_fails_replacement_if_old_speech_already_ended() {
        let mut schedule = Schedule::default();
        schedule.submit(1, "speaking", &[]).unwrap();
        let active = schedule.next().unwrap();
        assert!(schedule.start(&active));
        assert!(schedule.begin_native(active.id));
        schedule.cancel();
        assert_eq!(schedule.reserve_cancel(), Some(active.id));
        assert_eq!(schedule.complete(active.id), Some(false));
        schedule.submit(2, "replacement", &[]).unwrap();
        assert!(schedule.next().is_none());
        assert_eq!(schedule.poison().front().unwrap().id, 2);
        assert!(!schedule.cancel_in_flight());
        assert_eq!(schedule.complete(active.id), None);
        assert!(schedule.submit(3, "future", &[]).is_err());
    }

    #[test]
    fn poisoned_rpc_rejects_new_work_and_releases_queued_replacement() {
        let mut schedule = Schedule::default();
        schedule.submit(1, "blocked", &[]).unwrap();
        let blocked = schedule.next().unwrap();
        assert!(schedule.start(&blocked));
        assert!(schedule.begin_native(blocked.id));
        schedule.cancel();
        assert_eq!(schedule.reserve_cancel(), Some(blocked.id));
        schedule.submit(2, "replacement", &[]).unwrap();
        assert!(schedule.next().is_none());
        assert_eq!(schedule.poison().front().unwrap().id, 2);
        assert!(schedule.submit(3, "future", &[]).is_err());
        assert_eq!(schedule.complete(blocked.id), None);
    }
}
