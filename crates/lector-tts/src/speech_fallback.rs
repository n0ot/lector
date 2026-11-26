//! Spoken cue insertion and the corresponding UTF-8 progress mapping.

use crate::protocol::SoundCue;

pub const MAX_LINK_ANNOUNCEMENT_BYTES: usize = "link".len() + 2;

#[derive(Clone, Debug)]
pub struct InsertedLink {
    start: usize,
    end: usize,
    canonical: usize,
}

/// Insert the spoken label at an original text boundary. The caller has
/// appended the original text up to that boundary.
pub fn append_link_announcement(result: &mut String, text: &str, offset: usize) -> InsertedLink {
    append_cue_announcement(result, text, offset, SoundCue::Link)
}

/// Insert a cue's spoken label at a boundary of the original text.
pub(crate) fn append_cue_announcement(
    result: &mut String,
    text: &str,
    offset: usize,
    cue: SoundCue,
) -> InsertedLink {
    append_spoken_announcement(result, text, offset, cue.spoken_label())
}

/// Insert a custom spoken cue label at a UTF-8 character boundary.
pub fn append_spoken_announcement(
    result: &mut String,
    text: &str,
    offset: usize,
    label: &str,
) -> InsertedLink {
    let start = result.len();
    if result.ends_with(|character: char| !character.is_whitespace()) {
        result.push(' ');
    }
    result.push_str(label);
    if text[offset..].starts_with(|character: char| !character.is_whitespace()) {
        result.push(' ');
    }
    InsertedLink {
        start,
        end: result.len(),
        canonical: offset,
    }
}

impl InsertedLink {
    #[must_use]
    pub fn inserted_bytes(&self) -> usize {
        self.end - self.start
    }
}

/// Map an offset reported by a host back to the original text. The spoken
/// label itself maps to the source boundary where it was inserted.
#[must_use]
pub fn canonical_offset(offset: usize, inserted_links: &[InsertedLink]) -> usize {
    let preceding = inserted_links.partition_point(|link| link.start <= offset);
    preceding.checked_sub(1).map_or(offset, |index| {
        let link = &inserted_links[index];
        link.canonical + offset.saturating_sub(link.end)
    })
}
