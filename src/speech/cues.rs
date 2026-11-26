//! User cues carried with speech until the active host's capabilities are known.

use super::protocol::{SoundCue, SoundCueAt, TextPosition};
use std::borrow::Cow;

pub(super) use tts::speech_fallback::InsertedLink;
use tts::speech_fallback::{
    MAX_LINK_ANNOUNCEMENT_BYTES as MAX_MARKER_BYTES, append_link_announcement,
    append_spoken_announcement, canonical_offset,
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
    Inline {
        direction: Option<(SoundCue, &'static str)>,
        links: Vec<SpeechLink>,
        tones: bool,
    },
}

impl SpeechCues {
    pub(super) fn valid_for(&self, text: &str) -> bool {
        match self {
            Self::None => true,
            Self::Standalone(cue) => *cue != SoundCue::Link,
            Self::Inline {
                direction, links, ..
            } => {
                direction.is_none_or(|(cue, _)| cue.is_direction())
                    && links.iter().all(|link| {
                        (!link.audible || link.offset < text.len())
                            && text.is_char_boundary(link.offset)
                    })
                    && links.windows(2).all(|pair| pair[0] < pair[1])
            }
        }
    }

    pub(super) fn queue_bytes(&self) -> usize {
        match self {
            Self::Inline { links, .. } => size_of::<SpeechLink>().saturating_mul(links.capacity()),
            _ => 0,
        }
    }

    pub(super) fn bound_text(&mut self, text: &str, limit: usize) -> String {
        const TRUNCATION_SUFFIX: &str = " … speech truncated";
        let direction_bytes = match self {
            Self::Inline {
                direction: Some((_, fallback)),
                ..
            } => fallback.len() + 1,
            _ => 0,
        };
        let mut rendered_bytes = text.len().saturating_add(direction_bytes);
        if let Self::Inline { links, .. } = self {
            rendered_bytes =
                rendered_bytes.saturating_add(links.len().saturating_mul(MAX_MARKER_BYTES));
        }
        if rendered_bytes <= limit {
            self.compact_links();
            return text.to_owned();
        }

        let budget = limit.saturating_sub(TRUNCATION_SUFFIX.len() + direction_bytes);
        let mut end = budget.min(text.len());
        if let Self::Inline { links, .. } = self {
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
        if let Self::Inline { links, .. } = self {
            links.truncate(links.partition_point(|link| link.offset < end));
        }
        self.compact_links();
        format!("{}{TRUNCATION_SUFFIX}", &text[..end])
    }

    fn compact_links(&mut self) {
        if let Self::Inline { links, .. } = self
            && links.capacity() != links.len()
        {
            *links = std::mem::take(links).into_boxed_slice().into_vec();
        }
    }

    pub(super) fn render<'a>(
        &self,
        text: &'a str,
        supported: bool,
        directional_overlay: bool,
    ) -> RenderedCues<'a> {
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
            Self::Inline {
                direction,
                links,
                tones,
            } => {
                let overlay = supported && *tones;
                let mut inserted = 0;
                if let Some((cue, fallback)) = direction {
                    if overlay && directional_overlay {
                        rendered.cues.push(SoundCueAt {
                            cue: *cue,
                            offset: 0,
                        });
                    } else {
                        let mut result = String::new();
                        let announcement =
                            append_spoken_announcement(&mut result, text, 0, fallback);
                        inserted = announcement.inserted_bytes();
                        rendered.inserted_links.push(announcement);
                        rendered.text = Cow::Owned(result);
                    }
                }
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
    fn direction_and_link_can_share_the_readback_onset() {
        let mut cues = SpeechCues::Inline {
            direction: Some((SoundCue::Bottom, "bottom of table")),
            links: vec![SpeechLink {
                offset: 0,
                audible: true,
            }],
            tones: true,
        };

        let render = cues.render("café", true, true);
        assert_eq!(render.text, "café");
        assert_eq!(
            render.cues,
            [
                SoundCueAt {
                    cue: SoundCue::Bottom,
                    offset: 0
                },
                SoundCueAt {
                    cue: SoundCue::Link,
                    offset: 0
                },
            ]
        );
        assert!(SoundCueAt::valid_for(&render.text, &render.cues));

        let render = cues.render("café", true, false);
        assert_eq!(render.text, "bottom of table café");
        assert_eq!(
            render.cues,
            [SoundCueAt {
                cue: SoundCue::Link,
                offset: 16
            }]
        );
        assert!(SoundCueAt::valid_for_protocol(
            &render.text,
            &render.cues,
            super::super::protocol::ProtocolVersion { major: 2, minor: 3 },
        ));
        assert_eq!(canonical_offset(16, &render.inserted_links), 0);
        assert_eq!(canonical_offset(21, &render.inserted_links), 5);

        let SpeechCues::Inline { tones, .. } = &mut cues else {
            unreachable!()
        };
        *tones = false;
        let render = cues.render("café", true, true);
        assert_eq!(render.text, "bottom of table link café");
        assert!(render.cues.is_empty());
        assert_eq!(canonical_offset(21, &render.inserted_links), 0);
        assert_eq!(canonical_offset(26, &render.inserted_links), 5);
    }

    #[test]
    fn dense_cue_truncation_releases_excess_queue_storage() {
        let text = "a".repeat(MAX_UTTERANCE_TEXT_BYTES);
        let mut cues = SpeechCues::Inline {
            direction: None,
            links: (0..text.len())
                .map(|offset| SpeechLink {
                    offset,
                    audible: true,
                })
                .collect(),
            tones: true,
        };
        assert_eq!(cues.queue_bytes(), text.len() * size_of::<SpeechLink>());
        let bounded = cues.bound_text(&text, MAX_UTTERANCE_TEXT_BYTES);
        let SpeechCues::Inline { links, .. } = &cues else {
            unreachable!("bounded link announcements must retain their metadata");
        };
        assert!(!links.is_empty());
        assert!(links.len() < text.len());
        assert_eq!(links.capacity(), links.len());
        assert_eq!(
            cues.queue_bytes(),
            links.capacity() * size_of::<SpeechLink>()
        );
        assert!(bounded.len() + cues.queue_bytes() < 256 * 1024);
    }

    #[test]
    fn queue_bytes_counts_spare_capacity_before_compaction() {
        let mut links = Vec::with_capacity(1024);
        links.push(SpeechLink {
            offset: 0,
            audible: true,
        });
        let original_capacity = links.capacity();
        let mut cues = SpeechCues::Inline {
            direction: None,
            links,
            tones: true,
        };
        assert_eq!(
            cues.queue_bytes(),
            original_capacity * size_of::<SpeechLink>()
        );
        assert_eq!(cues.bound_text("text", MAX_UTTERANCE_TEXT_BYTES), "text");
        assert_eq!(cues.queue_bytes(), size_of::<SpeechLink>());
    }

    #[test]
    fn dense_cues_leave_room_for_a_pcm_host_or_spoken_fallback() {
        // A control byte has the maximum JSON escape expansion, and every
        // byte starts a distinct link.
        let text = "\0".repeat(MAX_UTTERANCE_TEXT_BYTES);
        let mut cues = SpeechCues::Inline {
            direction: None,
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
        assert!(cues.render(&bounded, false, false).text.len() <= MAX_UTTERANCE_TEXT_BYTES);
        let rendered = cues.render(&bounded, true, true);
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
