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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramStatus {
    state: State,
    app: Option<String>,
}

impl ProgramStatus {
    pub(crate) fn from_fields(state: &[u8], app: &[u8]) -> Option<Self> {
        Some(Self {
            state: State::parse(state)?,
            app: parse_app(app).map(str::to_owned),
        })
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

fn valid_text(value: &[u8], encoded_limit: usize, decoded_limit: usize) -> bool {
    if value.len() > encoded_limit {
        return false;
    }
    let end = value
        .iter()
        .rposition(|byte| *byte != b'=')
        .map_or(0, |index| index + 1);
    let unpadded = &value[..end];
    let padding = value.len() - unpadded.len();
    if padding > 0 && (padding > 2 || !value.len().is_multiple_of(4)) {
        return false;
    }
    crate::base64::decode_unpadded(unpadded).is_some_and(|text| {
        text.len() <= decoded_limit
            && std::str::from_utf8(&text).is_ok_and(|text| !text.chars().any(char::is_control))
    })
}

/// The outer option represents a well-formed report and the inner one allows clear.
fn parse_report(body: &[u8]) -> Option<Option<ProgramStatus>> {
    let mut state = b"".as_slice();
    let mut app = b"".as_slice();
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
            b"title" if !valid_text(value, 256, 192) => return None,
            b"msg" if !valid_text(value, 2732, 2048) => return None,
            _ => {}
        }
    }
    if state == b"clear" {
        Some(None)
    } else {
        ProgramStatus::from_fields(state, app).map(Some)
    }
}

#[derive(Default)]
pub(crate) struct ProgramStatusTracker {
    current: Option<ProgramStatus>,
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
        self.current = current;
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
                    return None;
                }
            };
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
        tracker.restore(ProgramStatus::from_fields(b"blocked", b"Example"));
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
    fn invalid_reports_and_non_root_records_do_not_replace_the_root() {
        let mut tracker = ProgramStatusTracker::default();
        tracker.restore(ProgramStatus::from_fields(b"done", b"Example"));
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
