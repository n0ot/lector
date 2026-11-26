//! Admission and generation ownership for one NVDA speech backend.

use std::collections::VecDeque;

use crate::{Error, protocol::SoundCueAt};

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

    pub(super) fn start(&mut self, job: &Job) -> bool {
        if self.unavailable || self.cancel_in_flight || self.generation != job.generation {
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
