use terminput::{Event, KeyCode, KeyEvent, KeyModifiers};

pub(super) const FOCUS_IN_EVENT: &[u8] = b"\x1B[I";
pub(super) const FOCUS_OUT_EVENT: &[u8] = b"\x1B[O";
const FOCUS_EVENTS_ENABLE: &[u8] = b"\x1B[?1004h";
const FOCUS_EVENTS_DISABLE: &[u8] = b"\x1B[?1004l";
const MODIFY_OTHER_KEYS_PREFIX: &[u8] = b"\x1B[27;";
const OSC_START: u8 = b']';
const ST_ESCAPE: u8 = b'\\';

pub(super) enum SequenceStatus<T> {
    None,
    Incomplete,
    Complete(T),
}

#[derive(Debug, PartialEq)]
pub(super) enum ModifyOtherKeysStatus {
    None,
    Incomplete,
    Event(usize, Event),
    Raw(usize),
}

#[derive(Default)]
pub(super) struct FocusModeFilter {
    pending: Vec<u8>,
    enabled: bool,
}

impl FocusModeFilter {
    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(super) fn filter_into(
        &mut self,
        input: &[u8],
        output: &mut Vec<u8>,
        focus_changes: &mut Vec<bool>,
    ) -> bool {
        output.clear();
        focus_changes.clear();
        if self.pending.is_empty() && !input.contains(&b'\x1B') {
            return false;
        }

        self.pending.extend_from_slice(input);
        output.reserve(self.pending.len());
        let mut consumed = 0;

        while consumed < self.pending.len() {
            let remaining = &self.pending[consumed..];
            if remaining.starts_with(FOCUS_EVENTS_ENABLE) {
                self.enabled = true;
                focus_changes.push(true);
                consumed += FOCUS_EVENTS_ENABLE.len();
            } else if remaining.starts_with(FOCUS_EVENTS_DISABLE) {
                self.enabled = false;
                focus_changes.push(false);
                consumed += FOCUS_EVENTS_DISABLE.len();
            } else if FOCUS_EVENTS_ENABLE.starts_with(remaining)
                || FOCUS_EVENTS_DISABLE.starts_with(remaining)
            {
                break;
            } else {
                output.push(self.pending[consumed]);
                consumed += 1;
            }
        }

        if consumed == self.pending.len() {
            self.pending.clear();
        } else if consumed > 0 {
            self.pending.drain(..consumed);
        }
        true
    }
}

pub(super) fn osc_status(input: &[u8]) -> SequenceStatus<usize> {
    if input.len() < 2 || input[0] != b'\x1B' || input[1] != OSC_START {
        return SequenceStatus::None;
    }

    let mut index = 2;
    while index < input.len() {
        match input[index] {
            0x07 => return SequenceStatus::Complete(index + 1),
            0x1B if input.get(index + 1) == Some(&ST_ESCAPE) => {
                return SequenceStatus::Complete(index + 2);
            }
            _ => index += 1,
        }
    }
    SequenceStatus::Incomplete
}

pub(super) fn focus_event_status(input: &[u8]) -> SequenceStatus<bool> {
    if input.starts_with(FOCUS_IN_EVENT) {
        SequenceStatus::Complete(true)
    } else if FOCUS_IN_EVENT.starts_with(input) && !input.is_empty() {
        SequenceStatus::Incomplete
    } else if input.starts_with(FOCUS_OUT_EVENT) {
        SequenceStatus::Complete(false)
    } else if FOCUS_OUT_EVENT.starts_with(input) && !input.is_empty() {
        SequenceStatus::Incomplete
    } else {
        SequenceStatus::None
    }
}

pub(super) fn modify_other_keys_status(input: &[u8]) -> ModifyOtherKeysStatus {
    if input.is_empty() {
        return ModifyOtherKeysStatus::None;
    }
    if MODIFY_OTHER_KEYS_PREFIX.starts_with(input) && input.len() < MODIFY_OTHER_KEYS_PREFIX.len() {
        return ModifyOtherKeysStatus::Incomplete;
    }
    if !input.starts_with(MODIFY_OTHER_KEYS_PREFIX) {
        return ModifyOtherKeysStatus::None;
    }

    let mut end = MODIFY_OTHER_KEYS_PREFIX.len();
    while end < input.len() {
        match input[end] {
            b'0'..=b'9' | b';' => end += 1,
            b'~' => {
                let raw = &input[..=end];
                return modify_other_keys_event(raw)
                    .map(|event| ModifyOtherKeysStatus::Event(raw.len(), event))
                    .unwrap_or(ModifyOtherKeysStatus::Raw(raw.len()));
            }
            _ => return ModifyOtherKeysStatus::None,
        }
    }

    ModifyOtherKeysStatus::Incomplete
}

pub(super) fn modify_other_keys_event(raw: &[u8]) -> Option<Event> {
    let body = std::str::from_utf8(raw.get(2..raw.len().checked_sub(1)?)?).ok()?;
    let mut parts = body.split(';');
    let prefix = parts.next()?;
    let modifiers = parts.next()?;
    let keycode = parts.next()?;
    if prefix != "27" || parts.next().is_some() {
        return None;
    }

    let translated = format!("\x1B[{keycode};{modifiers}u");
    Event::parse_from(translated.as_bytes()).ok().flatten()
}

pub(super) fn is_invalid_ss3_prefix(input: &[u8]) -> bool {
    input.len() >= 3
        && input[0] == b'\x1B'
        && input[1] == b'O'
        && !matches!(
            input[2],
            b'D' | b'C' | b'A' | b'B' | b'H' | b'F' | b'P'..=b'S'
        )
}

pub(super) fn timed_out_event(raw: &[u8]) -> Option<Event> {
    let key = match raw {
        b"\x1B" => KeyCode::Esc.into(),
        b"\x1B[" => KeyEvent::new(KeyCode::Char('[')).modifiers(KeyModifiers::ALT),
        b"\x1B]" => KeyEvent::new(KeyCode::Char(']')).modifiers(KeyModifiers::ALT),
        b"\x1BO" => KeyEvent::new(KeyCode::Char('O')).modifiers(KeyModifiers::ALT),
        _ => return None,
    };
    Some(Event::Key(key))
}

#[cfg(test)]
mod tests {
    use super::{
        FocusModeFilter, ModifyOtherKeysStatus, SequenceStatus, focus_event_status,
        is_invalid_ss3_prefix, modify_other_keys_event, modify_other_keys_status, osc_status,
        timed_out_event,
    };
    use terminput::{Event, KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn recognizes_complete_and_partial_control_sequences() {
        assert!(matches!(
            osc_status(b"\x1B]title"),
            SequenceStatus::Incomplete
        ));
        assert!(matches!(
            osc_status(b"\x1B]title\x07rest"),
            SequenceStatus::Complete(8)
        ));
        assert!(matches!(
            focus_event_status(b"\x1B["),
            SequenceStatus::Incomplete
        ));
        assert!(matches!(
            focus_event_status(b"\x1B[I"),
            SequenceStatus::Complete(true)
        ));
    }

    #[test]
    fn focus_filter_preserves_split_sequences_and_regular_bytes() {
        let mut filter = FocusModeFilter::default();
        let mut output = Vec::new();
        let mut changes = Vec::new();
        assert!(!filter.filter_into(b"plain", &mut output, &mut changes));
        assert!(output.is_empty());
        assert!(filter.filter_into(b"x\x1B[?10", &mut output, &mut changes));
        assert_eq!(output, b"x");
        assert!(filter.filter_into(b"04hy", &mut output, &mut changes));
        assert_eq!(output, b"y");
        assert_eq!(changes, [true]);
        assert!(filter.enabled());
        assert!(filter.filter_into(b"z\x1B[?1004l", &mut output, &mut changes));
        assert_eq!(output, b"z");
        assert_eq!(changes, [false]);
        assert!(!filter.enabled());
    }

    #[test]
    fn parses_modify_other_keys_events() {
        assert_eq!(
            modify_other_keys_event(b"\x1B[27;2;13~"),
            Some(Event::Key(
                KeyEvent::new(KeyCode::Enter).modifiers(KeyModifiers::SHIFT)
            ))
        );
        assert_eq!(
            modify_other_keys_event(b"\x1B[27;5;100~"),
            Some(Event::Key(
                KeyEvent::new(KeyCode::Char('d')).modifiers(KeyModifiers::CTRL)
            ))
        );
        assert_eq!(
            modify_other_keys_status(b"\x1B[27;2;13"),
            ModifyOtherKeysStatus::Incomplete
        );
    }

    #[test]
    fn osc_status_distinguishes_non_sequences_partial_and_both_terminators() {
        for input in [b"".as_slice(), b"x", b"\x1B[", b"]title"] {
            assert!(matches!(osc_status(input), SequenceStatus::None));
        }
        assert!(matches!(osc_status(b"\x1B]"), SequenceStatus::Incomplete));
        assert!(matches!(
            osc_status(b"\x1B]x\x1B"),
            SequenceStatus::Incomplete
        ));
        assert!(matches!(
            osc_status(b"\x1B]x\x07tail"),
            SequenceStatus::Complete(4)
        ));
        assert!(matches!(
            osc_status(b"\x1B]x\x1B\\tail"),
            SequenceStatus::Complete(5)
        ));
    }

    #[test]
    fn focus_event_status_covers_in_out_partial_and_unrelated_input() {
        assert!(matches!(focus_event_status(b""), SequenceStatus::None));
        assert!(matches!(
            focus_event_status(b"\x1B"),
            SequenceStatus::Incomplete
        ));
        assert!(matches!(
            focus_event_status(b"\x1B[Otail"),
            SequenceStatus::Complete(false)
        ));
        assert!(matches!(
            focus_event_status(b"\x1B[X"),
            SequenceStatus::None
        ));
    }

    #[test]
    fn modify_other_keys_rejects_malformed_input_and_preserves_unknown_sequences() {
        assert_eq!(modify_other_keys_status(b""), ModifyOtherKeysStatus::None);
        assert_eq!(
            modify_other_keys_status(b"\x1B[27"),
            ModifyOtherKeysStatus::Incomplete
        );
        assert_eq!(
            modify_other_keys_status(b"\x1B[28;2;13~"),
            ModifyOtherKeysStatus::None
        );
        assert_eq!(
            modify_other_keys_status(b"\x1B[27;x"),
            ModifyOtherKeysStatus::None
        );
        assert_eq!(
            modify_other_keys_status(b"\x1B[27;;~tail"),
            ModifyOtherKeysStatus::Raw(7)
        );

        for raw in [
            b"".as_slice(),
            b"x",
            b"\x1B[26;2;13~",
            b"\x1B[27;2~",
            b"\x1B[27;2;13;4~",
            b"\x1B[27;\xFF;13~",
        ] {
            assert_eq!(modify_other_keys_event(raw), None, "raw={raw:?}");
        }
    }

    #[test]
    fn ss3_validation_and_timeout_translation_cover_all_supported_prefixes() {
        assert!(!is_invalid_ss3_prefix(b"\x1BO"));
        for final_byte in [b'A', b'D', b'H', b'P', b'S'] {
            assert!(!is_invalid_ss3_prefix(&[b'\x1B', b'O', final_byte]));
        }
        assert!(is_invalid_ss3_prefix(b"\x1BOx"));

        assert_eq!(
            timed_out_event(b"\x1B"),
            Some(Event::Key(KeyEvent::new(KeyCode::Esc)))
        );
        for (raw, ch) in [
            (b"\x1B[".as_slice(), '['),
            (b"\x1B]".as_slice(), ']'),
            (b"\x1BO".as_slice(), 'O'),
        ] {
            assert_eq!(
                timed_out_event(raw),
                Some(Event::Key(
                    KeyEvent::new(KeyCode::Char(ch)).modifiers(KeyModifiers::ALT)
                ))
            );
        }
        assert_eq!(timed_out_event(b"\x1B[x"), None);
    }

    #[test]
    fn focus_filter_handles_multiple_modes_and_abandoned_partial_sequences() {
        let mut filter = FocusModeFilter::default();
        let mut output = vec![1];
        let mut changes = vec![true];

        assert!(!filter.filter_into(b"plain", &mut output, &mut changes));
        assert!(output.is_empty());
        assert!(changes.is_empty());

        assert!(filter.filter_into(b"a\x1B[?1004hb\x1B[?1004lc", &mut output, &mut changes));
        assert_eq!(output, b"abc");
        assert_eq!(changes, [true, false]);
        assert!(!filter.enabled());

        assert!(filter.filter_into(b"\x1B[?10", &mut output, &mut changes));
        assert!(output.is_empty());
        assert!(changes.is_empty());
        assert!(filter.filter_into(b"05x", &mut output, &mut changes));
        assert_eq!(output, b"\x1B[?1005x");
        assert!(changes.is_empty());
    }
}
