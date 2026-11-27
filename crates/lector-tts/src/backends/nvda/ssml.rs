//! Pure SSML and marker correlation for the NVDA controller.

use std::{fmt::Write as _, ops::Range};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    protocol::{SoundCue, SoundCueAt},
    speech_fallback::append_cue_announcement,
};

pub(super) struct Ssml {
    pub(super) text: String,
    pub(super) marks: Vec<Vec<SoundCue>>,
}

pub(super) struct MarkProgress {
    pub(super) token: u64,
    pub(super) next: usize,
}

impl MarkProgress {
    pub(super) fn reach(&mut self, token: u64, index: usize, count: usize) -> Option<Range<usize>> {
        if self.token != token || index < self.next || index >= count {
            return None;
        }
        let skipped = self.next..index;
        self.next = index + 1;
        Some(skipped)
    }
}

pub(super) fn parse_mark(bytes: &[u8]) -> Option<(u64, usize)> {
    let name = std::str::from_utf8(bytes).ok()?;
    let (token, index) = name.split_once(':')?;
    Some((token.parse().ok()?, index.parse().ok()?))
}

pub(super) fn make_ssml(text: &str, cues: &[SoundCueAt], token: u64, spoken: bool) -> Ssml {
    let mut xml = String::from("<speak>");
    let mut marks = vec![Vec::new()];
    write_mark(&mut xml, token, 0);
    if spoken {
        escape(&mut xml, &spoken_text(text, cues));
    } else {
        let mut previous = 0;
        let mut overlays = Vec::new();
        for (offset, cue, needs_label) in cue_positions(text, cues) {
            if offset != previous {
                flush_marks(&mut xml, &mut marks, &mut overlays, token, previous);
                escape(&mut xml, &text[previous..offset]);
            }
            if needs_label {
                append_cue_announcement(&mut xml, text, offset, cue);
            } else {
                overlays.push(cue);
            }
            previous = offset;
        }
        flush_marks(&mut xml, &mut marks, &mut overlays, token, previous);
        escape(&mut xml, &text[previous..]);
    }
    xml.push_str("</speak>");
    Ssml { text: xml, marks }
}

// NVDA turns an SSML marker into a separate speech command. If the link
// starts inside a word, let the voice finish that word and speak its label.
// Reuse the word end when multiple link boundaries fall in the same word.
fn cue_positions<'a>(
    text: &'a str,
    cues: &'a [SoundCueAt],
) -> impl Iterator<Item = (usize, SoundCue, bool)> + 'a {
    let mut boundaries = text
        .split_word_bound_indices()
        .map(|(offset, _)| offset)
        .chain(std::iter::once(text.len()))
        .peekable();
    cues.iter().map(move |cue| {
        while boundaries.next_if(|offset| *offset < cue.offset).is_some() {}
        let offset = boundaries
            .peek()
            .copied()
            .expect("validated cue is within the text");
        (offset, cue.cue, offset != cue.offset)
    })
}

fn flush_marks(
    xml: &mut String,
    marks: &mut Vec<Vec<SoundCue>>,
    overlays: &mut Vec<SoundCue>,
    token: u64,
    offset: usize,
) {
    if overlays.is_empty() {
        return;
    }
    if offset == 0 {
        // NVDA coalesces adjacent callbacks. Put initial cues on the start marker.
        marks[0] = std::mem::take(overlays);
    } else {
        write_mark(xml, token, marks.len());
        marks.push(std::mem::take(overlays));
    }
}

pub(super) fn spoken_text(text: &str, cues: &[SoundCueAt]) -> String {
    let mut result = String::new();
    let mut previous = 0;
    for (offset, cue, _) in cue_positions(text, cues) {
        result.push_str(&text[previous..offset]);
        append_cue_announcement(&mut result, text, offset, cue);
        previous = offset;
    }
    result.push_str(&text[previous..]);
    result
}

fn write_mark(out: &mut String, token: u64, index: usize) {
    write!(out, "<mark name=\"{token}:{index}\"/>").expect("writing to a String does not fail");
}

fn escape(out: &mut String, text: &str) {
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\t' | '\n' | '\r' => out.push(character),
            other if other < ' ' || matches!(other, '\u{fffe}' | '\u{ffff}') => out.push(' '),
            other => out.push(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_status_uses_the_start_marker_and_keeps_the_original_fallback() {
        let text = "Fixture is done";
        let cues = [SoundCueAt {
            cue: SoundCue::ProgramStatus,
            offset: 0,
        }];
        let plan = make_ssml(text, &cues, 7, false);
        assert_eq!(plan.marks, [vec![SoundCue::ProgramStatus]]);
        assert_eq!(
            plan.text,
            r#"<speak><mark name="7:0"/>Fixture is done</speak>"#
        );
        assert_eq!(spoken_text(text, &cues), text);
    }

    #[test]
    fn first_link_and_direction_share_the_speech_start_marker() {
        let cues = [
            SoundCueAt {
                cue: SoundCue::Left,
                offset: 0,
            },
            SoundCueAt {
                cue: SoundCue::Link,
                offset: 0,
            },
            SoundCueAt {
                cue: SoundCue::Link,
                offset: 6,
            },
        ];
        let plan = make_ssml("café & tea", &cues, 44, false);
        assert_eq!(
            plan.marks,
            [vec![SoundCue::Left, SoundCue::Link], vec![SoundCue::Link]]
        );
        assert_eq!(
            plan.text,
            "<speak><mark name=\"44:0\"/>café <mark name=\"44:1\"/>&amp; tea</speak>"
        );
    }

    #[test]
    fn malformed_source_characters_are_escaped_and_subword_cues_are_spoken() {
        let cues = [SoundCueAt {
            cue: SoundCue::Link,
            offset: 2,
        }];
        let plan = make_ssml("mint <\0>", &cues, 6, false);
        assert_eq!(plan.marks, [Vec::new()]);
        assert_eq!(
            plan.text,
            "<speak><mark name=\"6:0\"/>mint link &lt; &gt;</speak>"
        );
    }

    #[test]
    fn interior_links_preserve_combining_characters_and_later_marks() {
        let text = "cafe\u{301} menu";
        let cues = [2, 4, 7].map(|offset| SoundCueAt {
            cue: SoundCue::Link,
            offset,
        });
        let plan = make_ssml(text, &cues, 3, false);
        assert_eq!(
            plan.text,
            "<speak><mark name=\"3:0\"/>cafe\u{301} link link <mark name=\"3:1\"/>menu</speak>"
        );
        assert_eq!(plan.marks, [vec![], vec![SoundCue::Link]]);
    }

    #[test]
    fn progress_rejects_stale_and_duplicate_markers_and_reports_missing_groups() {
        let mut progress = MarkProgress { token: 81, next: 0 };
        assert_eq!(parse_mark(b"81:2"), Some((81, 2)));
        assert_eq!(parse_mark(b"81:2:0"), None);
        assert_eq!(progress.reach(80, 0, 3), None);
        assert_eq!(progress.reach(81, 1, 3), Some(0..1));
        assert_eq!(progress.reach(81, 1, 3), None);
        assert_eq!(progress.reach(81, 3, 3), None);
        assert_eq!(progress.reach(81, 2, 3), Some(2..2));
    }
}
