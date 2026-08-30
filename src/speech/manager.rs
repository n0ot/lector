//! Lector-owned speech sequencing and resumable-pause state.
//!
//! A host accepts at most one active utterance from this manager. Hosts never
//! own Lector's queue. All transitions are driven by accepted commands and
//! correlated host events; stale or malformed events cannot advance speech.

use super::protocol::{
    KnownEventKind, MAX_JSON_SAFE_INTEGER, MAX_UTTERANCE_TEXT_BYTES, PauseResult,
    SpeechCapabilities, SpeechEventNotification, TextPosition, UtteranceId,
};
use anyhow::{Result, anyhow};
use std::collections::VecDeque;

const MAX_PENDING_UTTERANCES: usize = 32;
const MAX_PENDING_BYTES: usize = 256 * 1024;

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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlaybackState {
    Speaking,
    Paused,
}

#[derive(Clone, Debug)]
struct ActiveUtterance {
    utterance: Utterance,
    state: PlaybackState,
    last_sequence: Option<u64>,
    last_position: Option<TextPosition>,
}

/// The host-independent state machine for one speech-server generation.
#[derive(Clone, Default)]
pub struct SpeechManager {
    active: Option<ActiveUtterance>,
    pending: VecDeque<Utterance>,
    pending_bytes: usize,
}

impl SpeechManager {
    pub fn submit(
        &mut self,
        host: &mut dyn Host,
        id: UtteranceId,
        text: String,
        interrupt: bool,
    ) -> Result<()> {
        self.drain_events(host)?;
        if !id.is_valid() || text.is_empty() || text.len() > MAX_UTTERANCE_TEXT_BYTES {
            return Err(anyhow!(
                "speech utterance must have a valid ID and 1 through {MAX_UTTERANCE_TEXT_BYTES} UTF-8 bytes"
            ));
        }
        let utterance = Utterance { id, text };

        if interrupt {
            self.clear_pending();
            if let Some(active) = self.active.take() {
                host.stop(&active.utterance.id)?;
            }
            // Interruption is implemented once here: clear Lector's queue,
            // stop the active host utterance, then submit normally. Passing a
            // second interrupt flag would make legacy hosts stop twice.
            return self.start(host, utterance, false);
        }

        if self.active.is_none() {
            return self.start(host, utterance, false);
        }
        if host.has_legacy_queue() {
            host.speak(&utterance.id, &utterance.text, false)?;
            return Ok(());
        }
        if !host
            .capabilities()
            .lifecycle
            .terminal
            .delivery
            .is_reliable()
        {
            return Err(anyhow!(
                "speech host cannot sequence another utterance without reliable terminal events"
            ));
        }
        self.enqueue(utterance);
        Ok(())
    }

    pub fn stop(&mut self, host: &mut dyn Host) -> Result<()> {
        self.clear_pending();
        let Some(active) = self.active.take() else {
            return Ok(());
        };
        host.stop(&active.utterance.id)
    }

    /// Pause the active utterance, resume an already paused utterance, or use
    /// the one-way stop fallback. A second fallback invocation is inert
    /// because the first one removes the active utterance.
    pub fn toggle_pause(&mut self, host: &mut dyn Host) -> Result<()> {
        self.drain_events(host)?;
        let Some(mut active) = self.active.take() else {
            return Ok(());
        };

        if active.state == PlaybackState::Paused {
            if let Err(resume_error) = host.resume(&active.utterance.id) {
                self.clear_pending();
                return match host.stop(&active.utterance.id) {
                    Ok(()) => Err(resume_error),
                    Err(stop_error) => Err(stop_error.context(format!(
                        "speech resume failed ({resume_error:#}) and cancelling the uncertain utterance also failed"
                    ))),
                };
            }
            active.state = PlaybackState::Speaking;
            self.active = Some(active);
            return Ok(());
        }

        if !host.capabilities().supports_resumable_pause() {
            self.clear_pending();
            return host.stop(&active.utterance.id);
        }

        let PauseResult { paused, position } = match host.pause(&active.utterance.id) {
            Ok(result) => result,
            Err(pause_error) => {
                self.clear_pending();
                return match host.stop(&active.utterance.id) {
                    Ok(()) => Err(pause_error),
                    Err(stop_error) => Err(stop_error.context(format!(
                        "speech pause failed ({pause_error:#}) and cancelling the uncertain utterance also failed"
                    ))),
                };
            }
        };
        if !paused {
            self.clear_pending();
            return host.stop(&active.utterance.id);
        }
        let Some(position) = position.filter(|position| position.valid_for(&active.utterance.text))
        else {
            // A host claiming resumable pause must never leave speech paused
            // at an unusable or non-UTF-8 position.
            let _ = host.stop(&active.utterance.id);
            self.clear_pending();
            return Err(anyhow!(
                "speech host paused without a valid UTF-8 resume position"
            ));
        };
        active.state = PlaybackState::Paused;
        active.last_position = Some(position);
        self.active = Some(active);
        Ok(())
    }

    pub fn poll(&mut self, host: &mut dyn Host) -> Result<()> {
        self.drain_events(host)
    }

    /// Abandon only work whose delivery is now uncertain. Pending utterances
    /// were never sent and can be started on a replacement host.
    pub fn host_lost(&mut self) {
        self.active = None;
    }

    pub fn host_ready(&mut self, host: &mut dyn Host) -> Result<()> {
        if self.active.is_none()
            && let Some(next) = self.pop_pending()
        {
            self.start(host, next, false)?;
        }
        Ok(())
    }

    fn drain_events(&mut self, host: &mut dyn Host) -> Result<()> {
        for event in host.take_events()? {
            self.handle_event(host, event)?;
        }
        Ok(())
    }

    fn handle_event(&mut self, host: &mut dyn Host, event: SpeechEventNotification) -> Result<()> {
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
                self.active = None;
                if let Some(next) = self.pop_pending() {
                    self.start(host, next, false)?;
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
    fn unsupported_pause_stops_once_and_second_toggle_is_inert() {
        let mut manager = SpeechManager::default();
        let mut host = FakeHost::full();
        host.capabilities.controls.pause_resume = PauseResumeSupport::Unsupported;
        let id = UtteranceId::new("u");
        manager
            .submit(&mut host, id.clone(), "hello".to_owned(), false)
            .unwrap();
        manager.toggle_pause(&mut host).unwrap();
        manager.toggle_pause(&mut host).unwrap();

        assert_eq!(
            host.calls,
            [Call::Speak(id.clone(), "hello".to_owned()), Call::Stop(id)]
        );
    }

    #[test]
    fn failed_or_declined_pause_is_cancelled_and_cannot_resume_later() {
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
                ]
            );
        }
    }

    #[test]
    fn failed_resume_cancels_paused_state_and_a_later_toggle_is_inert() {
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
            ]
        );
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
