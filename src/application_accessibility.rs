//! Generic, application-authored terminal accessibility messages.
//!
//! The transport is a versioned APC payload. Applications may suppress
//! Lector's automatic presentation heuristics and provide a semantic spoken
//! replacement without Lector needing application-specific knowledge.

use std::ops::Range;
const CAPABILITIES_PREFIX: &[u8] = b"\x1b_Lector;A11y;1;capabilities;";
const ST: &[u8] = b"\x1b\\";
const CAPABILITIES: &[u8] = b";cue=blank;link=1";
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
    Query(String),
    End,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ApplicationAccessibilitySpeech {
    pub(crate) text: String,
    pub(crate) links: Vec<Range<usize>>,
    pub(crate) cue: Option<crate::speech::protocol::SoundCue>,
    /// Application-provided indentation for semantic line content. Lector
    /// applies the user's indentation-reporting preference and tracks changes;
    /// the application does not decide whether it is spoken.
    pub(crate) indentation: Option<u16>,
}

impl ApplicationAccessibilitySpeech {
    pub(crate) fn heap_bytes(&self) -> usize {
        self.text.len().saturating_add(
            self.links
                .capacity()
                .saturating_mul(std::mem::size_of::<Range<usize>>()),
        )
    }
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
    if let Some(id) = body.strip_prefix(b"query;") {
        return valid_query_id(id).then(|| {
            ApplicationAccessibilityCommand::Query(
                std::str::from_utf8(id)
                    .expect("query ID contains only ASCII digits")
                    .into(),
            )
        });
    }
    if let Some(encoded) = body.strip_prefix(b"say-link;") {
        return decode_speech(encoded).map(|text| {
            let link = 0..text.len();
            ApplicationAccessibilityCommand::Speak(ApplicationAccessibilitySpeech {
                text,
                links: std::iter::once(link).collect(),
                ..Default::default()
            })
        });
    }
    if let Some(body) = body.strip_prefix(b"cue;blank;") {
        let (indentation, encoded) = if let Some(line) = body.strip_prefix(b"indent=") {
            let (indentation, encoded) = parse_line(line)?;
            (Some(indentation), encoded)
        } else {
            (None, body)
        };
        let text = if encoded.is_empty() {
            String::new()
        } else {
            decode_speech(encoded)?
        };
        return Some(ApplicationAccessibilityCommand::Speak(
            ApplicationAccessibilitySpeech {
                text,
                indentation,
                cue: Some(crate::speech::protocol::SoundCue::Blank),
                ..Default::default()
            },
        ));
    }
    if let Some(encoded) = body.strip_prefix(b"say;") {
        return decode_speech(encoded).map(|text| {
            ApplicationAccessibilityCommand::Speak(ApplicationAccessibilitySpeech {
                text,
                indentation: None,
                ..Default::default()
            })
        });
    }
    let (indentation, encoded) = parse_line(body.strip_prefix(b"line;indent=")?)?;
    decode_speech(encoded).map(|text| {
        ApplicationAccessibilityCommand::Speak(ApplicationAccessibilitySpeech {
            text,
            indentation: Some(indentation),
            ..Default::default()
        })
    })
}

fn parse_line(body: &[u8]) -> Option<(u16, &[u8])> {
    let separator = body.iter().position(|byte| *byte == b';')?;
    let (indentation, encoded) = body.split_at(separator);
    if indentation.is_empty() || !indentation.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some((
        std::str::from_utf8(indentation).ok()?.parse().ok()?,
        &encoded[1..],
    ))
}

fn valid_query_id(id: &[u8]) -> bool {
    !id.is_empty() && id.len() <= 32 && id.iter().all(u8::is_ascii_digit)
}

pub(crate) fn append_capabilities(reply: &mut Vec<u8>, id: &str) {
    reply.extend_from_slice(CAPABILITIES_PREFIX);
    reply.extend_from_slice(id.as_bytes());
    reply.extend_from_slice(CAPABILITIES);
    reply.extend_from_slice(ST);
}

/// Extract our own replies from the pane's terminal replies. Tmux owns all
/// other reports except the color replies handled by its dedicated API.
pub(crate) fn capability_replies(replies: &[u8]) -> impl Iterator<Item = &[u8]> {
    replies
        .split_inclusive(|byte| *byte == b'\\')
        .filter_map(|part| {
            let start = part
                .windows(CAPABILITIES_PREFIX.len())
                .rposition(|window| window == CAPABILITIES_PREFIX)?;
            let reply = &part[start..];
            let id = reply
                .strip_prefix(CAPABILITIES_PREFIX)?
                .strip_suffix(ST)?
                .strip_suffix(CAPABILITIES)?;
            valid_query_id(id).then_some(reply)
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
    let (pairs, remainder) = encoded.as_chunks::<2>();
    if pairs.is_empty() || !remainder.is_empty() || pairs.len() > MAX_SPEECH_BYTES {
        return None;
    }
    let mut decoded = Vec::with_capacity(pairs.len());
    for pair in pairs {
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
                    ..Default::default()
                }
            ))
        );
        assert_eq!(
            parse(b"Lector;A11y;1;line;indent=12;696e64656e746564", false),
            Some(ApplicationAccessibilityCommand::Speak(
                ApplicationAccessibilitySpeech {
                    text: "indented".into(),
                    indentation: Some(12),
                    ..Default::default()
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
