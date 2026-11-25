//! User cues carried with speech until the active host's capabilities are known.

use super::protocol::{SoundCue, SoundCueAt, TextPosition};
use std::borrow::Cow;

pub(super) use tts::speech_fallback::InsertedLink;
use tts::speech_fallback::{
    MAX_LINK_ANNOUNCEMENT_BYTES as MAX_MARKER_BYTES, append_link_announcement, canonical_offset,
};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SpeechLink {
    pub offset: usize,
    /// No synchronized overlay is possible when normalization leaves no
    /// audible text in the link. Announce those links with the spoken label.
    pub audible: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum SpeechCues {
    #[default]
    None,
    Standalone(SoundCue),
    Links {
        links: Vec<SpeechLink>,
        tones: bool,
    },
}

impl SpeechCues {
    pub(super) fn valid_for(&self, text: &str) -> bool {
        match self {
            Self::None => true,
            Self::Standalone(cue) => *cue != SoundCue::Link,
            Self::Links { links, .. } => {
                links.iter().all(|link| {
                    (!link.audible || link.offset < text.len())
                        && text.is_char_boundary(link.offset)
                }) && links.windows(2).all(|pair| pair[0] < pair[1])
            }
        }
    }

    pub(super) fn queue_bytes(&self) -> usize {
        match self {
            Self::Links { links, .. } => size_of::<SpeechLink>().saturating_mul(links.len()),
            _ => 0,
        }
    }

    pub(super) fn bound_text(&mut self, text: &str, limit: usize) -> String {
        const TRUNCATION_SUFFIX: &str = " … speech truncated";
        let mut rendered_bytes = text.len();
        if let Self::Links { links, .. } = self {
            rendered_bytes =
                rendered_bytes.saturating_add(links.len().saturating_mul(MAX_MARKER_BYTES));
        }
        if rendered_bytes <= limit {
            return text.to_owned();
        }

        let budget = limit.saturating_sub(TRUNCATION_SUFFIX.len());
        let mut end = budget.min(text.len());
        if let Self::Links { links, .. } = self {
            // Each retained link needs room for its spoken fallback even if
            // the current host can overlay the tone.
            for (index, link) in links.iter().enumerate() {
                let with_marker = budget.saturating_sub((index + 1) * MAX_MARKER_BYTES);
                if link.offset >= end {
                    break;
                }
                if link.offset >= with_marker {
                    end = link.offset;
                    break;
                }
                end = with_marker.min(text.len());
            }
        }
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if let Self::Links { links, .. } = self {
            links.retain(|link| link.offset < end);
        }
        format!("{}{TRUNCATION_SUFFIX}", &text[..end])
    }

    pub(super) fn render<'a>(&self, text: &'a str, supported: bool) -> RenderedCues<'a> {
        let mut rendered = RenderedCues {
            text: Cow::Borrowed(text),
            cues: Vec::new(),
            inserted_links: Vec::new(),
        };
        match self {
            Self::None => {}
            Self::Standalone(cue) if supported => {
                rendered.text = Cow::Borrowed("");
                rendered.cues.push(SoundCueAt {
                    cue: *cue,
                    offset: 0,
                });
            }
            Self::Standalone(_) => {}
            Self::Links { links, tones } => {
                let overlay = supported && *tones;
                let mut inserted = 0;
                let mut previous = 0;
                for link in links {
                    let offset = link.offset;
                    if overlay && link.audible {
                        rendered.cues.push(SoundCueAt {
                            cue: SoundCue::Link,
                            offset: offset + inserted,
                        });
                        continue;
                    }

                    if matches!(rendered.text, Cow::Borrowed(_)) {
                        rendered.text = Cow::Owned(String::with_capacity(
                            text.len() + links.len() * MAX_MARKER_BYTES,
                        ));
                    }
                    let result = rendered.text.to_mut();
                    result.push_str(&text[previous..offset]);
                    let announcement = append_link_announcement(result, text, offset);
                    inserted += announcement.inserted_bytes();
                    rendered.inserted_links.push(announcement);
                    previous = offset;
                }
                if let Cow::Owned(result) = &mut rendered.text {
                    result.push_str(&text[previous..]);
                }
            }
        }
        rendered
    }
}

pub(super) struct RenderedCues<'a> {
    pub(super) text: Cow<'a, str>,
    pub(super) cues: Vec<SoundCueAt>,
    pub(super) inserted_links: Vec<InsertedLink>,
}

/// Host byte offsets in the inserted announcement map to the start of the
/// linked text. Offsets after it move back by the added length.
pub(super) fn canonical_position(
    position: TextPosition,
    inserted_links: &[InsertedLink],
) -> TextPosition {
    let TextPosition::Utf8ByteOffset { offset } = position else {
        return position;
    };
    TextPosition::Utf8ByteOffset {
        offset: canonical_offset(offset, inserted_links),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speech::protocol::MAX_UTTERANCE_TEXT_BYTES;
    use tts::server::MAX_RPC_FRAME_BYTES;

    #[test]
    fn dense_cues_leave_room_for_a_pcm_host_or_spoken_fallback() {
        // A control byte has the maximum JSON escape expansion, and every
        // byte starts a distinct link.
        let text = "\0".repeat(MAX_UTTERANCE_TEXT_BYTES);
        let mut cues = SpeechCues::Links {
            links: (0..text.len())
                .map(|offset| SpeechLink {
                    offset,
                    audible: true,
                })
                .collect(),
            tones: true,
        };
        let bounded = cues.bound_text(&text, MAX_UTTERANCE_TEXT_BYTES);
        assert!(cues.valid_for(&bounded));
        assert!(cues.render(&bounded, false).text.len() <= MAX_UTTERANCE_TEXT_BYTES);
        let rendered = cues.render(&bounded, true);
        let wire = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "speech.speak",
            "params": {
                "utteranceId": "utterance",
                "text": rendered.text,
                "cues": rendered.cues,
            },
        }))
        .unwrap();
        assert!(wire.len() < MAX_RPC_FRAME_BYTES);
    }
}
