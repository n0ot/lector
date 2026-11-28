//! Root program records from OSC 7501 and tmux's pane state.

use crate::presentation::ProgramActivity;
use lector_ghostty::ProgramStatusUpdate;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Idle,
    Working,
    Blocked,
    Done,
    Error,
}

impl State {
    fn parse(state: &[u8]) -> Option<Self> {
        Some(match state {
            b"idle" => Self::Idle,
            b"working" => Self::Working,
            b"blocked" => Self::Blocked,
            b"done" => Self::Done,
            b"error" => Self::Error,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockReason {
    Approval,
    Question,
    Authentication,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum PendingKind {
    Attention,
    Done,
    Bell,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramStatus {
    state: State,
    app: Option<String>,
    // Recovery exposes no category. A live report can explicitly omit it.
    reason: Option<Option<BlockReason>>,
    message: Option<String>,
}

impl ProgramStatus {
    pub(crate) fn from_fields(state: &[u8], app: &[u8], message: &[u8]) -> Option<Self> {
        Some(Self {
            state: State::parse(state)?,
            app: parse_app(app).map(str::to_owned),
            reason: None,
            message: clean_text(message, 2048),
        })
    }

    fn pending_kind(&self) -> Option<PendingKind> {
        match self.state {
            State::Blocked | State::Error => Some(PendingKind::Attention),
            State::Done => Some(PendingKind::Done),
            State::Idle | State::Working => None,
        }
    }

    pub(crate) fn app(&self) -> Option<&str> {
        self.app.as_deref()
    }

    pub(crate) fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    pub(crate) fn reason(&self) -> &'static str {
        match self.state {
            State::Blocked => match self.reason.flatten() {
                Some(BlockReason::Approval) => "approval",
                Some(BlockReason::Question) => "question",
                Some(BlockReason::Authentication) => "authentication",
                None => "needs attention",
            },
            State::Error => "failed",
            State::Done => "done",
            State::Idle | State::Working => unreachable!("nonpending status"),
        }
    }

    fn matches_acknowledged(&self, acknowledged: &Self, recovering: bool) -> bool {
        self.state == acknowledged.state
            && (self.app == acknowledged.app || (recovering && self.app.is_none()))
            && (self.message == acknowledged.message
                || (recovering && self.message.is_none())
                || self
                    .message
                    .as_ref()
                    .zip(acknowledged.message.as_ref())
                    .is_some_and(|(current, previous)| {
                        // tmux sanitizes this pair in user-facing recovered names.
                        // The lost character cannot establish a new report.
                        if recovering && previous.contains("#(") {
                            previous.replace("#(", "_(") == *current
                        } else if acknowledged.reason.is_none() && current.contains("#(") {
                            current.replace("#(", "_(") == *previous
                        } else {
                            false
                        }
                    }))
            && self
                .reason
                .zip(acknowledged.reason)
                .is_none_or(|(a, b)| a == b)
    }
}

fn parse_app(app: &[u8]) -> Option<&str> {
    (!app.is_empty()
        && app.len() <= 32
        && app
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.+-".contains(byte)))
    .then(|| std::str::from_utf8(app).expect("ASCII program name"))
}

fn clean_text(bytes: &[u8], limit: usize) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    (bytes.len() <= limit && !text.is_empty() && !text.chars().any(char::is_control))
        .then(|| text.to_owned())
}

fn decode_text(value: &[u8], encoded_limit: usize, decoded_limit: usize) -> Option<String> {
    if value.len() > encoded_limit {
        return None;
    }
    let end = value
        .iter()
        .rposition(|byte| *byte != b'=')
        .map_or(0, |index| index + 1);
    let unpadded = &value[..end];
    let padding = value.len() - unpadded.len();
    if padding > 0 && (padding > 2 || !value.len().is_multiple_of(4)) {
        return None;
    }
    let decoded = crate::base64::decode_unpadded(unpadded)?;
    if decoded.is_empty() {
        return Some(String::new());
    }
    clean_text(&decoded, decoded_limit)
}

/// The outer option represents a well-formed report and the inner one allows clear.
fn parse_report(body: &[u8]) -> Option<Option<ProgramStatus>> {
    let mut state = b"".as_slice();
    let mut app = b"".as_slice();
    let mut reason = None;
    let mut message = None;
    for pair in body.split(|byte| *byte == b':') {
        let Some(separator) = pair.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        let key = pair[..separator].trim_ascii();
        let value = pair[separator + 1..].trim_ascii();
        if key.len() > 16 {
            return None;
        }
        // tmux exposes only the root record and cannot restore named records.
        // Ignore any explicit ID so a child report cannot replace the root.
        if key == b"id" {
            return None;
        }
        if !value
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.,+/=-".contains(byte))
        {
            continue;
        }
        match key {
            b"state" => state = value,
            b"app" if value.len() <= 32 => app = value,
            b"app" => return None,
            b"kind" => {
                reason = match value {
                    b"permission" => Some(BlockReason::Approval),
                    b"question" => Some(BlockReason::Question),
                    b"auth" => Some(BlockReason::Authentication),
                    _ => None,
                };
            }
            b"title" => {
                decode_text(value, 256, 192)?;
            }
            b"msg" => {
                let decoded = decode_text(value, 2732, 2048)?;
                message = (!decoded.is_empty()).then_some(decoded);
            }
            _ => {}
        }
    }
    if state == b"clear" {
        Some(None)
    } else {
        let state = State::parse(state)?;
        Some(Some(ProgramStatus {
            state,
            app: parse_app(app).map(str::to_owned),
            reason: Some(reason.filter(|_| state == State::Blocked)),
            message,
        }))
    }
}

#[derive(Default)]
pub(crate) struct ProgramStatusTracker {
    current: Option<ProgramStatus>,
    acknowledged: Option<ProgramStatus>,
}

impl ProgramStatusTracker {
    pub(crate) fn activity(&self) -> Option<ProgramActivity> {
        match self.current.as_ref()?.state {
            State::Working => Some(ProgramActivity::Working),
            State::Blocked => Some(ProgramActivity::Blocked),
            State::Idle | State::Done | State::Error => None,
        }
    }

    pub(crate) fn restore(&mut self, current: Option<ProgramStatus>) {
        // No root status in a snapshot can also mean tmux does not expose
        // it. Keep the acknowledgment until there is evidence of a change.
        if let Some(current) = &current {
            self.reconcile_acknowledgment(current, true);
        }
        self.current = current;
    }

    pub(crate) fn pending(&self) -> Option<(PendingKind, &ProgramStatus)> {
        let current = self.current.as_ref()?;
        self.acknowledged
            .is_none()
            .then_some((current.pending_kind()?, current))
    }

    pub(crate) fn acknowledge(&mut self) {
        if let Some((_, current)) = self.pending() {
            self.acknowledged = Some(current.clone());
        }
    }

    fn reconcile_acknowledgment(&mut self, current: &ProgramStatus, recovering: bool) {
        if let Some(acknowledged) = &self.acknowledged {
            if !current.matches_acknowledged(acknowledged, recovering) {
                self.acknowledged = None;
            } else if !recovering && acknowledged.reason.is_none() {
                self.acknowledged = Some(current.clone());
            }
        }
    }

    pub(crate) fn observe(&mut self, update: ProgramStatusUpdate) -> Option<String> {
        let next =
            match update {
                ProgramStatusUpdate::Report(body) => parse_report(&body)?,
                ProgramStatusUpdate::Reset => None,
                ProgramStatusUpdate::PromptStart => {
                    if self.current.as_ref().is_some_and(|status| {
                        matches!(status.state, State::Working | State::Blocked)
                    }) {
                        self.current = None;
                    }
                    if self
                        .acknowledged
                        .as_ref()
                        .is_some_and(|status| status.state == State::Blocked)
                    {
                        self.acknowledged = None;
                    }
                    return None;
                }
            };
        if let Some(next) = &next {
            self.reconcile_acknowledgment(next, false);
        } else {
            self.acknowledged = None;
        }
        let previous = std::mem::replace(&mut self.current, next);
        let next = self.current.as_ref()?;
        if previous.as_ref().is_some_and(|previous| {
            previous.state == next.state
                && previous
                    .app
                    .as_ref()
                    .zip(next.app.as_ref())
                    .is_none_or(|(a, b)| a == b)
        }) {
            return None;
        }
        let predicate = match next.state {
            State::Blocked => "needs attention",
            State::Done => "is done",
            _ => return None,
        };
        let subject = next.app.as_deref().unwrap_or("Program");
        Some(format!("{subject} {predicate}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(tracker: &mut ProgramStatusTracker, body: &str) -> Option<String> {
        tracker.observe(ProgramStatusUpdate::Report(body.as_bytes().to_vec()))
    }

    #[test]
    fn recovery_and_details_are_silent_and_a_new_work_cycle_rearms_completion() {
        let mut tracker = ProgramStatusTracker::default();
        tracker.restore(ProgramStatus::from_fields(b"blocked", b"Example", b""));
        assert_eq!(report(&mut tracker, "state=blocked:app=Example"), None);
        assert_eq!(
            report(
                &mut tracker,
                "state=blocked:app=Example:msg=bmV3IHJlYXNvbg=="
            ),
            None
        );
        assert_eq!(
            report(&mut tracker, "state=done:app=Example").as_deref(),
            Some("Example is done")
        );
        assert_eq!(tracker.observe(ProgramStatusUpdate::PromptStart), None);
        assert_eq!(report(&mut tracker, "state=done:app=Example"), None);
        assert_eq!(report(&mut tracker, "state=working:app=Example"), None);
        assert_eq!(
            report(&mut tracker, "state=done:app=Example").as_deref(),
            Some("Example is done")
        );
        assert_eq!(report(&mut tracker, "state=clear"), None);
        assert_eq!(
            report(&mut tracker, "state=blocked").as_deref(),
            Some("Program needs attention")
        );
    }

    #[test]
    fn pending_acknowledgment_survives_partial_recovery_and_rearms_on_new_evidence() {
        let mut tracker = ProgramStatusTracker::default();
        let approval = "state=blocked:kind=permission:app=Sample:msg=Q29udGludWU/";
        assert_eq!(
            report(&mut tracker, approval).as_deref(),
            Some("Sample needs attention")
        );
        let (kind, status) = tracker.pending().unwrap();
        assert_eq!(kind, PendingKind::Attention);
        assert_eq!(status.reason(), "approval");
        assert_eq!(status.message(), Some("Continue?"));
        tracker.acknowledge();
        tracker.restore(ProgramStatus::from_fields(
            b"blocked",
            b"Sample",
            b"Continue?",
        ));
        assert!(tracker.pending().is_none());
        assert!(report(&mut tracker, approval).is_none());
        assert!(tracker.pending().is_none());

        let question = "state=blocked:kind=question:app=Sample:msg=Q29udGludWU/";
        assert!(report(&mut tracker, question).is_none());
        assert_eq!(tracker.pending().unwrap().1.reason(), "question");
        tracker.acknowledge();
        tracker.restore(ProgramStatus::from_fields(
            b"blocked",
            b"Sample",
            b"Different question",
        ));
        let (_, recovered) = tracker.pending().unwrap();
        assert_eq!(recovered.reason(), "needs attention");
        assert_eq!(recovered.message(), Some("Different question"));
        tracker.acknowledge();
        report(&mut tracker, "state=working:app=Sample");
        assert!(tracker.pending().is_none());
        report(&mut tracker, approval);
        assert_eq!(tracker.pending().unwrap().1.reason(), "approval");
        tracker.acknowledge();
        tracker.restore(None);
        assert!(tracker.observe(ProgramStatusUpdate::PromptStart).is_none());
        report(&mut tracker, approval);
        assert!(tracker.pending().is_some());
    }

    #[test]
    fn invalid_reports_and_non_root_records_do_not_replace_the_root() {
        let mut tracker = ProgramStatusTracker::default();
        tracker.restore(ProgramStatus::from_fields(b"done", b"Example", b""));
        for body in [
            "state=working:msg=Cg==",
            "state=working:title=/w==",
            "state=working:id=child",
            "state=unknown",
        ] {
            assert_eq!(report(&mut tracker, body), None);
            assert_eq!(report(&mut tracker, "state=done:app=Example"), None);
        }
    }
}
