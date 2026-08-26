//! Generic, application-authored terminal accessibility messages.
//!
//! The transport is a versioned APC payload. Applications may suppress
//! Lector's automatic presentation heuristics and provide a semantic spoken
//! replacement without Lector needing application-specific knowledge.

const PREFIX: &[u8] = b"Lector;A11y;1;";
pub(crate) const MAX_SPEECH_BYTES: usize = 2_000;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ApplicationAccessibilityPolicy {
    pub(crate) suppress_auto_read: bool,
    pub(crate) suppress_cursor_tracking: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ApplicationAccessibilityCommand {
    Set(ApplicationAccessibilityPolicy),
    Speak(ApplicationAccessibilitySpeech),
    End,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ApplicationAccessibilitySpeech {
    pub(crate) text: String,
    /// Application-provided indentation for semantic line content. Lector
    /// applies the user's indentation-reporting preference and tracks changes;
    /// the application does not decide whether it is spoken.
    pub(crate) indentation: Option<u16>,
}

/// Parses one complete APC body. Unknown messages remain ordinary unknown
/// terminal sequences; only an exact, non-truncated protocol message is
/// claimed by this parser.
pub(crate) fn parse(content: &[u8], truncated: bool) -> Option<ApplicationAccessibilityCommand> {
    if truncated {
        return None;
    }
    let body = content.strip_prefix(PREFIX)?;
    if body == b"end" {
        return Some(ApplicationAccessibilityCommand::End);
    }
    if let Some(settings) = body.strip_prefix(b"set;") {
        return parse_settings(settings).map(ApplicationAccessibilityCommand::Set);
    }
    if let Some(encoded) = body.strip_prefix(b"say;") {
        return decode_speech(encoded).map(|text| {
            ApplicationAccessibilityCommand::Speak(ApplicationAccessibilitySpeech {
                text,
                indentation: None,
            })
        });
    }
    let semantic_line = body.strip_prefix(b"line;indent=")?;
    let separator = semantic_line.iter().position(|byte| *byte == b';')?;
    let (indentation, encoded) = semantic_line.split_at(separator);
    let encoded = encoded.get(1..)?;
    if indentation.is_empty() || !indentation.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let indentation = std::str::from_utf8(indentation).ok()?.parse().ok()?;
    decode_speech(encoded).map(|text| {
        ApplicationAccessibilityCommand::Speak(ApplicationAccessibilitySpeech {
            text,
            indentation: Some(indentation),
        })
    })
}

fn parse_settings(settings: &[u8]) -> Option<ApplicationAccessibilityPolicy> {
    let mut policy = ApplicationAccessibilityPolicy::default();
    let mut saw_auto = false;
    let mut saw_cursor = false;
    for setting in settings.split(|byte| *byte == b';') {
        match setting {
            b"auto=0" if !saw_auto => {
                policy.suppress_auto_read = true;
                saw_auto = true;
            }
            b"auto=1" if !saw_auto => saw_auto = true,
            b"cursor=0" if !saw_cursor => {
                policy.suppress_cursor_tracking = true;
                saw_cursor = true;
            }
            b"cursor=1" if !saw_cursor => saw_cursor = true,
            _ => return None,
        }
    }
    (saw_auto && saw_cursor).then_some(policy)
}

fn decode_speech(encoded: &[u8]) -> Option<String> {
    if encoded.is_empty()
        || !encoded.len().is_multiple_of(2)
        || encoded.len() > MAX_SPEECH_BYTES * 2
    {
        return None;
    }
    let mut decoded = Vec::with_capacity(encoded.len() / 2);
    for pair in encoded.chunks_exact(2) {
        decoded.push(hex_digit(pair[0])?.checked_mul(16)? + hex_digit(pair[1])?);
    }
    let text = String::from_utf8(decoded).ok()?;
    (!text.is_empty() && !text.chars().any(char::is_control)).then_some(text)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ApplicationAccessibilityCommand, ApplicationAccessibilityPolicy,
        ApplicationAccessibilitySpeech, MAX_SPEECH_BYTES, parse,
    };

    #[test]
    fn parses_policy_speech_and_end() {
        assert_eq!(
            parse(b"Lector;A11y;1;set;auto=0;cursor=0", false),
            Some(ApplicationAccessibilityCommand::Set(
                ApplicationAccessibilityPolicy {
                    suppress_auto_read: true,
                    suppress_cursor_tracking: true,
                }
            ))
        );
        assert_eq!(
            parse(b"Lector;A11y;1;say;68c3a96c6c6f", false),
            Some(ApplicationAccessibilityCommand::Speak(
                ApplicationAccessibilitySpeech {
                    text: "h\u{e9}llo".into(),
                    indentation: None,
                }
            ))
        );
        assert_eq!(
            parse(b"Lector;A11y;1;line;indent=12;696e64656e746564", false),
            Some(ApplicationAccessibilityCommand::Speak(
                ApplicationAccessibilitySpeech {
                    text: "indented".into(),
                    indentation: Some(12),
                }
            ))
        );
        assert_eq!(
            parse(b"Lector;A11y;1;end", false),
            Some(ApplicationAccessibilityCommand::End)
        );
    }

    #[test]
    fn rejects_ambiguous_or_unsafe_messages() {
        for content in [
            b"lector;A11y;1;end".as_slice(),
            b"Lector;A11y;2;end",
            b"Lector;A11y;1;set;auto=0",
            b"Lector;A11y;1;set;auto=0;auto=1;cursor=0",
            b"Lector;A11y;1;say;0",
            b"Lector;A11y;1;say;gg",
            b"Lector;A11y;1;say;0a",
            b"Lector;A11y;1;line;indent=;74657874",
            b"Lector;A11y;1;line;indent=-1;74657874",
            b"Lector;A11y;1;line;indent=+1;74657874",
            b"Lector;A11y;1;line;indent=65536;74657874",
            b"Lector;A11y;1;line;indent=1;0a",
        ] {
            assert_eq!(parse(content, false), None, "accepted {content:?}");
        }
        assert_eq!(parse(b"Lector;A11y;1;end", true), None);
        let oversized = format!("Lector;A11y;1;say;{}", "61".repeat(MAX_SPEECH_BYTES + 1));
        assert_eq!(parse(oversized.as_bytes(), false), None);
    }
}
