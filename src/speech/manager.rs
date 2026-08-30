//! Lector-owned speech sequencing and resumable-pause state.
//!
//! A host accepts at most one active utterance from this manager. Hosts never
//! own Lector's queue. All transitions are driven by accepted commands and
//! correlated host events; stale or malformed events cannot advance speech.

use super::UtteranceBoundary;
use super::protocol::{
    KnownEventKind, MAX_JSON_SAFE_INTEGER, MAX_UTTERANCE_TEXT_BYTES, PauseResult,
    SpeechCapabilities, SpeechEventNotification, TextPosition, UtteranceId,
};
use anyhow::{Result, anyhow};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

const MAX_PENDING_UTTERANCES: usize = 32;
const MAX_PENDING_BYTES: usize = 256 * 1024;
pub(crate) const PARAGRAPH_PAUSE: Duration = Duration::from_millis(200);

pub trait Host {
    fn capabilities(&self) -> &SpeechCapabilities;

    /// Version 1 hosts retain their historical backend-owned queue. This is a
    /// compatibility exception and is never enabled by a version 2 capability.
    fn has_legacy_queue(&self) -> bool {
        false
    }

    fn speak(&mut self, id: &UtteranceId, text: &str, legacy_interrupt: bool) -> Result<()>;
    fn stop(&mut self, id: &UtteranceId) -> Result<()>;
    fn pause(&mut self, id: &UtteranceId) -> Result<PauseResult>;
    fn resume(&mut self, id: &UtteranceId) -> Result<()>;
    fn set_rate(&mut self, rate: f32) -> Result<f32>;
    fn take_events(&mut self) -> Result<Vec<SpeechEventNotification>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Utterance {
    id: UtteranceId,
    text: String,
    boundary: UtteranceBoundary,
}

impl Utterance {
    fn new(id: UtteranceId, text: String, boundary: UtteranceBoundary) -> Self {
        Self { id, text, boundary }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlaybackState {
    Speaking,
    Paused,
    StoppingForRestart,
    RestartAfterStop,
}

#[derive(Clone, Debug)]
struct ActiveUtterance {
    utterance: Utterance,
    state: PlaybackState,
    last_sequence: Option<u64>,
    last_position: Option<TextPosition>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeferredStart {
    ParagraphDelay { ready_at: Instant },
    PausedBetweenParagraphs,
}

/// The host-independent state machine for one speech-server generation.
#[derive(Clone, Default)]
pub struct SpeechManager {
    active: Option<ActiveUtterance>,
    pending: VecDeque<Utterance>,
    pending_bytes: usize,
    paused_restart: Option<Utterance>,
    deferred_start: Option<DeferredStart>,
    next_restart_id: u64,
}

impl SpeechManager {
    pub fn submit(
        &mut self,
        host: &mut dyn Host,
        id: UtteranceId,
        text: String,
        interrupt: bool,
    ) -> Result<()> {
        self.submit_with_boundary(host, id, text, interrupt, UtteranceBoundary::Immediate)
    }

    pub fn submit_with_boundary(
        &mut self,
        host: &mut dyn Host,
        id: UtteranceId,
        text: String,
        interrupt: bool,
        boundary: UtteranceBoundary,
    ) -> Result<()> {
        self.drain_events(host)?;
        if !id.is_valid() || text.is_empty() || text.len() > MAX_UTTERANCE_TEXT_BYTES {
            return Err(anyhow!(
                "speech utterance must have a valid ID and 1 through {MAX_UTTERANCE_TEXT_BYTES} UTF-8 bytes"
            ));
        }
        let utterance = Utterance::new(id, text, boundary);

        if interrupt {
            self.clear_waiting();
            if let Some(active) = self.active.take() {
                host.stop(&active.utterance.id)?;
            }
            // Interruption is implemented once here: clear Lector's queue,
            // stop the active host utterance, then submit normally. Passing a
            // second interrupt flag would make legacy hosts stop twice.
            return self.start(host, utterance, false);
        }

        if self.active.is_none() && self.paused_restart.is_none() && self.deferred_start.is_none() {
            return self.start(host, utterance, false);
        }
        if self.paused_restart.is_some() || self.deferred_start.is_some() {
            if !self.can_sequence(host) {
                return Err(anyhow!(
                    "speech host cannot queue behind paused speech without reliable terminal events"
                ));
            }
            self.enqueue(utterance);
            return Ok(());
        }
        if host.has_legacy_queue() {
            host.speak(&utterance.id, &utterance.text, false)?;
            return Ok(());
        }
        if !self.can_sequence(host) {
            return Err(anyhow!(
                "speech host cannot sequence another utterance without reliable terminal events"
            ));
        }
        self.enqueue(utterance);
        Ok(())
    }

    pub fn stop(&mut self, host: &mut dyn Host) -> Result<()> {
        self.clear_waiting();
        let Some(active) = self.active.take() else {
            return Ok(());
        };
        host.stop(&active.utterance.id)
    }

    /// Pause or resume without discarding queued speech. A host with word
    /// positions resumes the same logical utterance; other hosts stop it and
    /// restart the complete utterance under a fresh protocol ID.
    pub fn toggle_pause(&mut self, host: &mut dyn Host) -> Result<()> {
        self.drain_events(host)?;

        if let Some(deferred) = self.deferred_start {
            match deferred {
                DeferredStart::ParagraphDelay { .. } => {
                    self.deferred_start = Some(DeferredStart::PausedBetweenParagraphs);
                    return Ok(());
                }
                DeferredStart::PausedBetweenParagraphs => {
                    self.deferred_start = None;
                    return self.start_next_now(host);
                }
            }
        }

        if let Some(utterance) = self.paused_restart.take() {
            let utterance = self.restarted(utterance)?;
            self.start(host, utterance, false)?;
            return self.flush_legacy_pending(host);
        }

        let Some(mut active) = self.active.take() else {
            return Ok(());
        };

        match active.state {
            PlaybackState::Paused => {
                if let Err(resume_error) = host.resume(&active.utterance.id) {
                    return match self.pause_by_restart(host, active) {
                        Ok(()) => Err(resume_error),
                        Err(stop_error) => Err(stop_error.context(format!(
                            "speech resume failed ({resume_error:#}) and stopping it for restart also failed"
                        ))),
                    };
                }
                active.state = PlaybackState::Speaking;
                self.active = Some(active);
                return Ok(());
            }
            PlaybackState::StoppingForRestart => {
                active.state = PlaybackState::RestartAfterStop;
                self.active = Some(active);
                return Ok(());
            }
            PlaybackState::RestartAfterStop => {
                active.state = PlaybackState::StoppingForRestart;
                self.active = Some(active);
                return Ok(());
            }
            PlaybackState::Speaking => {}
        }

        if !host.capabilities().supports_resumable_pause() {
            return self.pause_by_restart(host, active);
        }

        let PauseResult { paused, position } = match host.pause(&active.utterance.id) {
            Ok(result) => result,
            Err(pause_error) => {
                return match self.pause_by_restart(host, active) {
                    Ok(()) => Err(pause_error),
                    Err(stop_error) => Err(stop_error.context(format!(
                        "speech pause failed ({pause_error:#}) and stopping it for restart also failed"
                    ))),
                };
            }
        };
        if !paused {
            return self.pause_by_restart(host, active);
        }
        let Some(position) = position.filter(|position| position.valid_for(&active.utterance.text))
        else {
            let error = anyhow!("speech host paused without a valid UTF-8 resume position");
            return match self.pause_by_restart(host, active) {
                Ok(()) => Err(error),
                Err(stop_error) => {
                    Err(stop_error
                        .context(format!("{error:#}; stopping it for restart also failed")))
                }
            };
        };
        active.state = PlaybackState::Paused;
        active.last_position = Some(position);
        self.active = Some(active);
        Ok(())
    }

    pub fn poll(&mut self, host: &mut dyn Host) -> Result<()> {
        self.poll_at(host, Instant::now())
    }

    /// Abandon only work whose delivery is now uncertain. Pending utterances
    /// were never sent and can be started on a replacement host.
    pub fn host_lost(&mut self) {
        self.active = None;
    }

    pub fn host_ready(&mut self, host: &mut dyn Host) -> Result<()> {
        self.start_due(host, Instant::now())
    }

    fn drain_events(&mut self, host: &mut dyn Host) -> Result<()> {
        self.poll_at(host, Instant::now())
    }

    fn poll_at(&mut self, host: &mut dyn Host, now: Instant) -> Result<()> {
        for event in host.take_events()? {
            self.handle_event(host, event, now)?;
        }
        self.start_due(host, now)
    }

    fn handle_event(
        &mut self,
        host: &mut dyn Host,
        event: SpeechEventNotification,
        now: Instant,
    ) -> Result<()> {
        let Some(active) = self.active.as_mut() else {
            return Ok(());
        };
        if active.utterance.id != event.utterance_id
            || event.sequence > MAX_JSON_SAFE_INTEGER
            || active
                .last_sequence
                .is_some_and(|sequence| event.sequence <= sequence)
        {
            return Ok(());
        }
        active.last_sequence = Some(event.sequence);
        let Some(kind) = event.event.known_kind() else {
            return Ok(());
        };

        match kind {
            KnownEventKind::Progress | KnownEventKind::Paused => {
                if let Some(position) = event
                    .event
                    .position
                    .filter(|position| position.valid_for(&active.utterance.text))
                {
                    active.last_position = Some(position);
                }
            }
            KnownEventKind::Started | KnownEventKind::Resumed => {}
            KnownEventKind::Ended => {
                let ended = self
                    .active
                    .take()
                    .expect("active utterance was validated above");
                match ended.state {
                    PlaybackState::StoppingForRestart => {
                        self.paused_restart = Some(ended.utterance);
                    }
                    PlaybackState::RestartAfterStop => {
                        let restarted = self.restarted(ended.utterance)?;
                        self.start(host, restarted, false)?;
                    }
                    PlaybackState::Speaking | PlaybackState::Paused => {
                        self.start_due(host, now)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn start(
        &mut self,
        host: &mut dyn Host,
        utterance: Utterance,
        legacy_interrupt: bool,
    ) -> Result<()> {
        host.speak(&utterance.id, &utterance.text, legacy_interrupt)?;
        self.active = Some(ActiveUtterance {
            utterance,
            state: PlaybackState::Speaking,
            last_sequence: None,
            last_position: None,
        });
        Ok(())
    }

    fn pause_by_restart(&mut self, host: &mut dyn Host, mut active: ActiveUtterance) -> Result<()> {
        if let Err(error) = host.stop(&active.utterance.id) {
            self.active = Some(active);
            return Err(error);
        }
        active.utterance.boundary = UtteranceBoundary::Immediate;
        if host.capabilities().controls.stop == super::protocol::StopSupport::Confirmed {
            self.paused_restart = Some(active.utterance);
        } else {
            // A best-effort stop response cannot prove that old audio is gone.
            // Reliable terminal evidence makes the transition restart-safe;
            // without it this state deliberately remains held.
            active.state = PlaybackState::StoppingForRestart;
            self.active = Some(active);
        }
        Ok(())
    }

    fn restarted(&mut self, mut utterance: Utterance) -> Result<Utterance> {
        self.next_restart_id = self
            .next_restart_id
            .checked_add(1)
            .ok_or_else(|| anyhow!("speech restart identifier space exhausted"))?;
        utterance.id = UtteranceId::new(format!("lector-resume:{}", self.next_restart_id));
        utterance.boundary = UtteranceBoundary::Immediate;
        Ok(utterance)
    }

    fn start_due(&mut self, host: &mut dyn Host, now: Instant) -> Result<()> {
        if self.active.is_some() || self.paused_restart.is_some() {
            return Ok(());
        }
        match self.deferred_start {
            Some(DeferredStart::PausedBetweenParagraphs) => return Ok(()),
            Some(DeferredStart::ParagraphDelay { ready_at }) if now < ready_at => return Ok(()),
            Some(DeferredStart::ParagraphDelay { .. }) => {
                self.deferred_start = None;
                return self.start_next_now(host);
            }
            None => {}
        }

        let Some(next) = self.pending.front() else {
            return Ok(());
        };
        if next.boundary == UtteranceBoundary::Paragraph {
            self.deferred_start = Some(DeferredStart::ParagraphDelay {
                ready_at: now.checked_add(PARAGRAPH_PAUSE).unwrap_or(now),
            });
            return Ok(());
        }
        self.start_next_now(host)
    }

    fn start_next_now(&mut self, host: &mut dyn Host) -> Result<()> {
        let Some(next) = self.pop_pending() else {
            return Ok(());
        };
        self.start(host, next, false)?;
        self.flush_legacy_pending(host)
    }

    fn flush_legacy_pending(&mut self, host: &mut dyn Host) -> Result<()> {
        if !host.has_legacy_queue() || self.active.is_none() {
            return Ok(());
        }
        while let Some(next) = self.pop_pending() {
            host.speak(&next.id, &next.text, false)?;
        }
        Ok(())
    }

    fn can_sequence(&self, host: &dyn Host) -> bool {
        host.has_legacy_queue()
            || host
                .capabilities()
                .lifecycle
                .terminal
                .delivery
                .is_reliable()
    }

    fn enqueue(&mut self, utterance: Utterance) {
        while self.pending.len() == MAX_PENDING_UTTERANCES
            || self.pending_bytes.saturating_add(utterance.text.len()) > MAX_PENDING_BYTES
        {
            let Some(stale) = self.pending.pop_front() else {
                break;
            };
            self.pending_bytes = self.pending_bytes.saturating_sub(stale.text.len());
        }
        self.pending_bytes = self.pending_bytes.saturating_add(utterance.text.len());
        self.pending.push_back(utterance);
    }

    fn pop_pending(&mut self) -> Option<Utterance> {
        let next = self.pending.pop_front()?;
        self.pending_bytes = self.pending_bytes.saturating_sub(next.text.len());
        Some(next)
    }

    fn clear_pending(&mut self) {
        self.pending.clear();
        self.pending_bytes = 0;
    }

    fn clear_waiting(&mut self) {
        self.clear_pending();
        self.paused_restart = None;
        self.deferred_start = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speech::protocol::{
        ControlCapabilities, DeliveryGuarantee, EventCapability, LifecycleCapabilities,
        PauseResumeSupport, ProgressCapabilities, ProgressMode, SpeechEventPayload, StopSupport,
        TerminalCapability,
    };
    use std::collections::BTreeMap;

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum Call {
        Speak(UtteranceId, String),
        Stop(UtteranceId),
        Pause(UtteranceId),
        Resume(UtteranceId),
    }

    struct FakeHost {
        capabilities: SpeechCapabilities,
        calls: Vec<Call>,
        events: Vec<SpeechEventNotification>,
        pause_result: PauseResult,
        fail_pause: bool,
        fail_resume: bool,
        legacy: bool,
    }

    impl FakeHost {
        fn full() -> Self {
            Self {
                capabilities: SpeechCapabilities {
                    lifecycle: LifecycleCapabilities {
                        started: EventCapability {
                            delivery: DeliveryGuarantee::Reliable,
                            ..Default::default()
                        },
                        terminal: TerminalCapability {
                            delivery: DeliveryGuarantee::Reliable,
                            distinguishes: vec![
                                "completed".to_owned(),
                                "cancelled".to_owned(),
                                "failed".to_owned(),
                            ],
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    progress: ProgressCapabilities {
                        modes: vec![ProgressMode {
                            kind: "utf8ByteOffset".to_owned(),
                            granularity: vec!["word".to_owned()],
                            extensions: BTreeMap::new(),
                        }],
                        ..Default::default()
                    },
                    controls: ControlCapabilities {
                        stop: StopSupport::Confirmed,
                        pause_resume: PauseResumeSupport::RestartFromWord,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                calls: Vec::new(),
                events: Vec::new(),
                pause_result: PauseResult {
                    paused: true,
                    position: Some(TextPosition::Utf8ByteOffset { offset: 6 }),
                },
                fail_pause: false,
                fail_resume: false,
                legacy: false,
            }
        }

        fn ended(id: &UtteranceId, sequence: u64) -> SpeechEventNotification {
            SpeechEventNotification {
                utterance_id: id.clone(),
                sequence,
                event: SpeechEventPayload {
                    kind: "ended".to_owned(),
                    position: None,
                    reason: Some("completed".to_owned()),
                    message: None,
                    extensions: BTreeMap::new(),
                },
            }
        }
    }

    impl Host for FakeHost {
        fn capabilities(&self) -> &SpeechCapabilities {
            &self.capabilities
        }

        fn has_legacy_queue(&self) -> bool {
            self.legacy
        }

        fn speak(&mut self, id: &UtteranceId, text: &str, _: bool) -> Result<()> {
            self.calls.push(Call::Speak(id.clone(), text.to_owned()));
            Ok(())
        }

        fn stop(&mut self, id: &UtteranceId) -> Result<()> {
            self.calls.push(Call::Stop(id.clone()));
            Ok(())
        }

        fn pause(&mut self, id: &UtteranceId) -> Result<PauseResult> {
            self.calls.push(Call::Pause(id.clone()));
            if self.fail_pause {
                Err(anyhow!("pause failed"))
            } else {
                Ok(self.pause_result.clone())
            }
        }

        fn resume(&mut self, id: &UtteranceId) -> Result<()> {
            self.calls.push(Call::Resume(id.clone()));
            if self.fail_resume {
                Err(anyhow!("resume failed"))
            } else {
                Ok(())
            }
        }

        fn set_rate(&mut self, rate: f32) -> Result<f32> {
            Ok(rate)
        }

        fn take_events(&mut self) -> Result<Vec<SpeechEventNotification>> {
            Ok(std::mem::take(&mut self.events))
        }
    }

    #[test]
    fn reliable_terminal_event_advances_exactly_one_queued_utterance() {
        let mut manager = SpeechManager::default();
        let mut host = FakeHost::full();
        let one = UtteranceId::new("one");
        let two = UtteranceId::new("two");
        manager
            .submit(&mut host, one.clone(), "first".to_owned(), false)
            .unwrap();
        manager
            .submit(&mut host, two.clone(), "second".to_owned(), false)
            .unwrap();
        assert_eq!(host.calls, [Call::Speak(one.clone(), "first".to_owned())]);

        host.events.push(FakeHost::ended(&one, 1));
        manager.poll(&mut host).unwrap();
        assert_eq!(
            host.calls,
            [
                Call::Speak(one, "first".to_owned()),
                Call::Speak(two, "second".to_owned())
            ]
        );
    }

    #[test]
    fn stale_and_duplicate_terminal_events_cannot_advance_the_queue() {
        let mut manager = SpeechManager::default();
        let mut host = FakeHost::full();
        let one = UtteranceId::new("one");
        let two = UtteranceId::new("two");
        let three = UtteranceId::new("three");
        manager
            .submit(&mut host, one.clone(), "one".to_owned(), false)
            .unwrap();
        manager
            .submit(&mut host, two.clone(), "two".to_owned(), false)
            .unwrap();
        manager
            .submit(&mut host, three, "three".to_owned(), false)
            .unwrap();

        host.events.push(FakeHost::ended(&one, 4));
        host.events.push(FakeHost::ended(&one, 4));
        manager.poll(&mut host).unwrap();
        assert_eq!(host.calls.len(), 2);
        assert_eq!(host.calls[1], Call::Speak(two, "two".to_owned()));
    }

    #[test]
    fn pause_then_resume_retains_the_utterance_and_uses_word_position() {
        let mut manager = SpeechManager::default();
        let mut host = FakeHost::full();
        let id = UtteranceId::new("u");
        manager
            .submit(&mut host, id.clone(), "hello world again".to_owned(), false)
            .unwrap();
        manager.toggle_pause(&mut host).unwrap();
        manager.toggle_pause(&mut host).unwrap();

        assert_eq!(
            host.calls,
            [
                Call::Speak(id.clone(), "hello world again".to_owned()),
                Call::Pause(id.clone()),
                Call::Resume(id)
            ]
        );
    }

    #[test]
    fn unsupported_pause_restarts_the_whole_utterance_with_a_fresh_id() {
        let mut manager = SpeechManager::default();
        let mut host = FakeHost::full();
        host.capabilities.controls.pause_resume = PauseResumeSupport::Unsupported;
        let id = UtteranceId::new("u");
        manager
            .submit(&mut host, id.clone(), "hello".to_owned(), false)
            .unwrap();
        manager.toggle_pause(&mut host).unwrap();
        manager.toggle_pause(&mut host).unwrap();
        manager.toggle_pause(&mut host).unwrap();
        manager.toggle_pause(&mut host).unwrap();

        assert_eq!(
            host.calls,
            [
                Call::Speak(id.clone(), "hello".to_owned()),
                Call::Stop(id),
                Call::Speak(UtteranceId::new("lector-resume:1"), "hello".to_owned(),),
                Call::Stop(UtteranceId::new("lector-resume:1")),
                Call::Speak(UtteranceId::new("lector-resume:2"), "hello".to_owned(),),
            ]
        );
    }

    #[test]
    fn whole_utterance_restart_preserves_later_paragraphs() {
        let mut manager = SpeechManager::default();
        let mut host = FakeHost::full();
        host.capabilities.controls.pause_resume = PauseResumeSupport::Unsupported;
        let first = UtteranceId::new("first");
        let second = UtteranceId::new("second");
        let now = Instant::now();
        manager
            .submit(&mut host, first.clone(), "first words".to_owned(), false)
            .unwrap();
        manager
            .submit_with_boundary(
                &mut host,
                second.clone(),
                "second words".to_owned(),
                false,
                UtteranceBoundary::Paragraph,
            )
            .unwrap();

        manager.toggle_pause(&mut host).unwrap();
        manager.toggle_pause(&mut host).unwrap();
        let restarted = UtteranceId::new("lector-resume:1");
        host.events.push(FakeHost::ended(&restarted, 1));
        manager.poll_at(&mut host, now).unwrap();
        manager.poll_at(&mut host, now + PARAGRAPH_PAUSE).unwrap();

        assert_eq!(
            host.calls,
            [
                Call::Speak(first.clone(), "first words".to_owned()),
                Call::Stop(first),
                Call::Speak(restarted, "first words".to_owned()),
                Call::Speak(second, "second words".to_owned()),
            ]
        );
    }

    #[test]
    fn best_effort_stop_waits_for_terminal_evidence_before_restart() {
        let mut manager = SpeechManager::default();
        let mut host = FakeHost::full();
        host.capabilities.controls.pause_resume = PauseResumeSupport::Unsupported;
        host.capabilities.controls.stop = StopSupport::BestEffort;
        let id = UtteranceId::new("u");
        manager
            .submit(&mut host, id.clone(), "hello".to_owned(), false)
            .unwrap();

        manager.toggle_pause(&mut host).unwrap();
        manager.toggle_pause(&mut host).unwrap();
        assert_eq!(
            host.calls,
            [
                Call::Speak(id.clone(), "hello".to_owned()),
                Call::Stop(id.clone())
            ]
        );

        host.events.push(FakeHost::ended(&id, 1));
        manager.poll(&mut host).unwrap();
        assert_eq!(
            host.calls,
            [
                Call::Speak(id.clone(), "hello".to_owned()),
                Call::Stop(id),
                Call::Speak(UtteranceId::new("lector-resume:1"), "hello".to_owned(),),
            ]
        );
    }

    #[test]
    fn stop_without_confirmation_or_terminal_evidence_never_guesses_restart_safety() {
        let mut manager = SpeechManager::default();
        let mut host = FakeHost::full();
        host.capabilities.controls.pause_resume = PauseResumeSupport::Unsupported;
        host.capabilities.controls.stop = StopSupport::BestEffort;
        host.capabilities.lifecycle.terminal.delivery = DeliveryGuarantee::Unsupported;
        let id = UtteranceId::new("u");
        manager
            .submit(&mut host, id.clone(), "hello".to_owned(), false)
            .unwrap();

        manager.toggle_pause(&mut host).unwrap();
        manager.toggle_pause(&mut host).unwrap();
        manager.poll(&mut host).unwrap();

        assert_eq!(
            host.calls,
            [Call::Speak(id.clone(), "hello".to_owned()), Call::Stop(id)]
        );
    }

    #[test]
    fn failed_or_declined_pause_falls_back_to_a_whole_utterance_restart() {
        for fail_pause in [false, true] {
            let mut manager = SpeechManager::default();
            let mut host = FakeHost::full();
            host.fail_pause = fail_pause;
            if !fail_pause {
                host.pause_result = PauseResult {
                    paused: false,
                    position: None,
                };
            }
            let id = UtteranceId::new(if fail_pause { "failed" } else { "declined" });
            manager
                .submit(&mut host, id.clone(), "hello world".to_owned(), false)
                .unwrap();

            let result = manager.toggle_pause(&mut host);
            assert_eq!(result.is_err(), fail_pause);
            manager.toggle_pause(&mut host).unwrap();
            assert_eq!(
                host.calls,
                [
                    Call::Speak(id.clone(), "hello world".to_owned()),
                    Call::Pause(id.clone()),
                    Call::Stop(id),
                    Call::Speak(
                        UtteranceId::new("lector-resume:1"),
                        "hello world".to_owned(),
                    ),
                ]
            );
        }
    }

    #[test]
    fn failed_resume_falls_back_to_a_whole_utterance_restart() {
        let mut manager = SpeechManager::default();
        let mut host = FakeHost::full();
        let id = UtteranceId::new("u");
        manager
            .submit(&mut host, id.clone(), "hello world".to_owned(), false)
            .unwrap();
        manager.toggle_pause(&mut host).unwrap();
        host.fail_resume = true;

        assert!(manager.toggle_pause(&mut host).is_err());
        manager.toggle_pause(&mut host).unwrap();
        assert_eq!(
            host.calls,
            [
                Call::Speak(id.clone(), "hello world".to_owned()),
                Call::Pause(id.clone()),
                Call::Resume(id.clone()),
                Call::Stop(id),
                Call::Speak(
                    UtteranceId::new("lector-resume:1"),
                    "hello world".to_owned(),
                ),
            ]
        );
    }

    #[test]
    fn typing_stop_discards_paused_resume_state_and_pending_speech() {
        let mut manager = SpeechManager::default();
        let mut host = FakeHost::full();
        let id = UtteranceId::new("u");
        manager
            .submit(&mut host, id.clone(), "hello world".to_owned(), false)
            .unwrap();
        manager
            .submit(
                &mut host,
                UtteranceId::new("queued"),
                "later".to_owned(),
                false,
            )
            .unwrap();
        manager.toggle_pause(&mut host).unwrap();
        manager.stop(&mut host).unwrap();
        manager.toggle_pause(&mut host).unwrap();

        assert_eq!(
            host.calls,
            [
                Call::Speak(id.clone(), "hello world".to_owned()),
                Call::Pause(id.clone()),
                Call::Stop(id)
            ]
        );
    }

    #[test]
    fn cancellation_discards_whole_utterance_restart_state() {
        let mut manager = SpeechManager::default();
        let mut host = FakeHost::full();
        host.capabilities.controls.pause_resume = PauseResumeSupport::Unsupported;
        let id = UtteranceId::new("u");
        manager
            .submit(&mut host, id.clone(), "hello world".to_owned(), false)
            .unwrap();

        manager.toggle_pause(&mut host).unwrap();
        manager.stop(&mut host).unwrap();
        manager.toggle_pause(&mut host).unwrap();

        assert_eq!(
            host.calls,
            [
                Call::Speak(id.clone(), "hello world".to_owned()),
                Call::Stop(id),
            ]
        );
    }

    #[test]
    fn interrupting_speech_discards_paused_resume_state() {
        let mut manager = SpeechManager::default();
        let mut host = FakeHost::full();
        let old = UtteranceId::new("old");
        let new = UtteranceId::new("new");
        manager
            .submit(&mut host, old.clone(), "old words".to_owned(), false)
            .unwrap();
        manager.toggle_pause(&mut host).unwrap();
        manager
            .submit(&mut host, new.clone(), "new words".to_owned(), true)
            .unwrap();
        manager.toggle_pause(&mut host).unwrap();

        assert_eq!(
            host.calls,
            [
                Call::Speak(old.clone(), "old words".to_owned()),
                Call::Pause(old.clone()),
                Call::Stop(old),
                Call::Speak(new.clone(), "new words".to_owned()),
                Call::Pause(new)
            ]
        );
    }

    #[test]
    fn new_hosts_without_reliable_completion_reject_ambiguous_queueing() {
        let mut manager = SpeechManager::default();
        let mut host = FakeHost::full();
        host.capabilities.lifecycle.terminal.delivery = DeliveryGuarantee::Unsupported;
        manager
            .submit(&mut host, UtteranceId::new("one"), "one".to_owned(), false)
            .unwrap();
        let error = manager
            .submit(&mut host, UtteranceId::new("two"), "two".to_owned(), false)
            .unwrap_err();
        assert!(error.to_string().contains("reliable terminal events"));
    }

    #[test]
    fn invalid_utf8_pause_position_is_stopped_instead_of_guessed() {
        let mut manager = SpeechManager::default();
        let mut host = FakeHost::full();
        host.pause_result.position = Some(TextPosition::Utf8ByteOffset { offset: 2 });
        let id = UtteranceId::new("u");
        manager
            .submit(&mut host, id.clone(), "aé word".to_owned(), false)
            .unwrap();

        let error = manager.toggle_pause(&mut host).unwrap_err();
        assert!(error.to_string().contains("valid UTF-8 resume position"));
        manager.toggle_pause(&mut host).unwrap();
        assert_eq!(
            host.calls,
            [
                Call::Speak(id.clone(), "aé word".to_owned()),
                Call::Pause(id.clone()),
                Call::Stop(id),
                Call::Speak(UtteranceId::new("lector-resume:1"), "aé word".to_owned(),),
            ]
        );
    }

    #[test]
    fn paragraph_boundary_waits_for_a_nonblocking_deadline() {
        let mut manager = SpeechManager::default();
        let mut host = FakeHost::full();
        let first = UtteranceId::new("first");
        let second = UtteranceId::new("second");
        let now = Instant::now();
        manager
            .submit(&mut host, first.clone(), "first".to_owned(), false)
            .unwrap();
        manager
            .submit_with_boundary(
                &mut host,
                second.clone(),
                "second".to_owned(),
                false,
                UtteranceBoundary::Paragraph,
            )
            .unwrap();

        host.events.push(FakeHost::ended(&first, 1));
        manager.poll_at(&mut host, now).unwrap();
        manager
            .poll_at(&mut host, now + Duration::from_millis(199))
            .unwrap();
        assert_eq!(host.calls, [Call::Speak(first, "first".to_owned())]);

        manager.poll_at(&mut host, now + PARAGRAPH_PAUSE).unwrap();
        assert_eq!(
            host.calls,
            [
                Call::Speak(UtteranceId::new("first"), "first".to_owned()),
                Call::Speak(second, "second".to_owned()),
            ]
        );
    }

    #[test]
    fn pause_during_paragraph_gap_holds_then_starts_the_next_paragraph() {
        let mut manager = SpeechManager::default();
        let mut host = FakeHost::full();
        let first = UtteranceId::new("first");
        let second = UtteranceId::new("second");
        let now = Instant::now() + Duration::from_secs(60);
        manager
            .submit(&mut host, first.clone(), "first".to_owned(), false)
            .unwrap();
        manager
            .submit_with_boundary(
                &mut host,
                second.clone(),
                "second".to_owned(),
                false,
                UtteranceBoundary::Paragraph,
            )
            .unwrap();
        host.events.push(FakeHost::ended(&first, 1));
        manager.poll_at(&mut host, now).unwrap();

        manager.toggle_pause(&mut host).unwrap();
        manager
            .poll_at(&mut host, now + Duration::from_secs(120))
            .unwrap();
        assert_eq!(host.calls, [Call::Speak(first, "first".to_owned())]);

        manager.toggle_pause(&mut host).unwrap();
        assert_eq!(
            host.calls,
            [
                Call::Speak(UtteranceId::new("first"), "first".to_owned()),
                Call::Speak(second, "second".to_owned()),
            ]
        );
    }

    #[test]
    fn cancellation_during_paragraph_gap_discards_remaining_paragraphs() {
        let mut manager = SpeechManager::default();
        let mut host = FakeHost::full();
        let first = UtteranceId::new("first");
        let now = Instant::now() + Duration::from_secs(60);
        manager
            .submit(&mut host, first.clone(), "first".to_owned(), false)
            .unwrap();
        manager
            .submit_with_boundary(
                &mut host,
                UtteranceId::new("second"),
                "second".to_owned(),
                false,
                UtteranceBoundary::Paragraph,
            )
            .unwrap();
        host.events.push(FakeHost::ended(&first, 1));
        manager.poll_at(&mut host, now).unwrap();

        manager.stop(&mut host).unwrap();
        manager.toggle_pause(&mut host).unwrap();
        manager
            .poll_at(&mut host, now + Duration::from_secs(120))
            .unwrap();
        assert_eq!(host.calls, [Call::Speak(first, "first".to_owned())]);
    }

    #[test]
    fn out_of_range_event_sequence_cannot_advance_the_queue() {
        let mut manager = SpeechManager::default();
        let mut host = FakeHost::full();
        let one = UtteranceId::new("one");
        manager
            .submit(&mut host, one.clone(), "one".to_owned(), false)
            .unwrap();
        manager
            .submit(&mut host, UtteranceId::new("two"), "two".to_owned(), false)
            .unwrap();
        host.events
            .push(FakeHost::ended(&one, MAX_JSON_SAFE_INTEGER + 1));

        manager.poll(&mut host).unwrap();
        assert_eq!(host.calls, [Call::Speak(one, "one".to_owned())]);
    }
}
