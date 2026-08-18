use super::{Result, ScreenReader};
use crate::view::View;
use similar::{Algorithm, ChangeTag, TextDiff};

#[derive(Default)]
pub(super) struct AutoReadBuffers {
    diff_text: String,
    graphemes: String,
    live_text: String,
    lcs: Vec<usize>,
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum DiffState {
    NoChanges,
    OneDeletion,
    Single,
    Multi,
}

impl ScreenReader {
    pub fn auto_read(&mut self, view: &mut View) -> Result<bool> {
        self.auto_read_impl(view, false)
    }

    pub(crate) fn auto_read_after_input(&mut self, view: &mut View) -> Result<bool> {
        self.auto_read_impl(view, true)
    }

    fn auto_read_impl(&mut self, view: &mut View, prefer_cursor: bool) -> Result<bool> {
        self.report_application_cursor_indentation_changes(view)?;
        if view.screen().contents() == view.prev_screen().contents() {
            return Ok(false);
        }

        let cursor_moves = view.accessibility_update_summary().cursor_operations;
        let scrolled = view.accessibility_update_summary().scroll_operations > 0;
        let changed_row_ranges = view.accessibility_update_summary().changed_rows.clone();

        let mut live_text = std::mem::take(&mut self.auto_read_buffers.live_text);
        view.accessibility_update_summary()
            .printed_text_into(&mut live_text);

        let mut live_read_result = None;
        {
            let text = live_text.trim();
            if !text.is_empty() && (cursor_moves == 0 || scrolled) {
                let mut spoken = false;
                let suppress_echo = self.should_suppress_key_echo(text);
                if !suppress_echo
                    && let Some(text) = self.hook_on_live_read(text, cursor_moves, scrolled)?
                    && !text.is_empty()
                {
                    crate::diagnostics::event(
                        "screen-reader",
                        "auto-read-progress",
                        &format!("speaking live text bytes={}", text.len()),
                    );
                    self.speak(&text, false)?;
                    crate::diagnostics::event(
                        "screen-reader",
                        "auto-read-progress",
                        "finished speaking live text",
                    );
                    spoken = true;
                }
                live_read_result = Some(spoken || !text.is_empty());
            }
        }

        if let Some(result) = live_read_result {
            self.auto_read_buffers.live_text = live_text;
            return Ok(result);
        }
        self.auto_read_buffers.live_text = live_text;

        let mut diff_text = std::mem::take(&mut self.auto_read_buffers.diff_text);
        diff_text.clear();
        let prev_cursor = view.prev_screen().cursor_position();
        let cursor = view.screen().cursor_position();
        let cursor_changed = cursor != prev_cursor;
        let (old_text, new_text, prev_hashes, curr_hashes) = view.full_contents_cached();

        if prev_hashes.len() == curr_hashes.len()
            && prev_hashes == curr_hashes
            && old_text == new_text
        {
            self.auto_read_buffers.diff_text = diff_text;
            return Ok(false);
        }

        let cursor_row = usize::from(cursor.0);
        let cursor_row_changed = prev_hashes
            .get(cursor_row)
            .zip(curr_hashes.get(cursor_row))
            .is_some_and(|(prev, curr)| prev != curr);
        let (single_changed_row, multiple_changed_rows) = if prev_hashes.len() == curr_hashes.len()
        {
            let mut changed_rows = changed_row_ranges
                .iter()
                .flat_map(|range| range.clone())
                .filter(|row| {
                    prev_hashes.get(usize::from(*row)) != curr_hashes.get(usize::from(*row))
                });
            match (changed_rows.next(), changed_rows.next()) {
                (Some(row), None) => (Some(row), false),
                (_, Some(_)) => (None, true),
                (None, None) => (None, false),
            }
        } else {
            (None, false)
        };
        // Full-screen applications commonly redraw a ruler or status line along with an
        // inline edit. Keep the fine-grained insertion diff anchored to the cursor row in
        // that case; otherwise the secondary row makes the update look like unrelated
        // multi-line output and the whole edited line is announced.
        let prefer_inline_cursor_row = prefer_cursor
            && cursor_moves > 0
            && !scrolled
            && cursor.0 == prev_cursor.0
            && cursor.1 > prev_cursor.1
            && cursor_row_changed
            && multiple_changed_rows;
        let (diff_old_text, diff_new_text) = if prefer_inline_cursor_row {
            (
                old_text
                    .split_terminator('\n')
                    .nth(cursor_row)
                    .unwrap_or(""),
                new_text
                    .split_terminator('\n')
                    .nth(cursor_row)
                    .unwrap_or(""),
            )
        } else {
            (old_text, new_text)
        };

        let line_changes = TextDiff::configure()
            .algorithm(Algorithm::Patience)
            .diff_lines(diff_old_text, diff_new_text);

        let mut diff_state = DiffState::NoChanges;
        for change in line_changes.iter_all_changes() {
            diff_state = next_line_diff_state(diff_state, change.tag());
            if change.tag() == ChangeTag::Insert
                && let Some(change_str) = change.as_str()
            {
                diff_text.push_str(change_str);
                diff_text.push('\n');
            }
        }

        let cursor_crossed_changed_row = single_changed_row.is_some_and(|row| {
            if cursor.0 > prev_cursor.0 {
                row >= prev_cursor.0 && row < cursor.0
            } else if cursor.0 < prev_cursor.0 {
                row <= prev_cursor.0 && row > cursor.0
            } else {
                false
            }
        });
        let cursor_on_changed_row = prefer_inline_cursor_row
            || single_changed_row.is_some_and(|row| row == prev_cursor.0 || row == cursor.0);
        if prefer_cursor
            && diff_state == DiffState::Single
            && cursor_moves > 0
            && !scrolled
            && cursor_changed
            && !cursor_on_changed_row
            && !cursor_crossed_changed_row
        {
            diff_text.clear();
            self.auto_read_buffers.diff_text = diff_text;
            return Ok(false);
        }

        if diff_state == DiffState::Single {
            let mut graphemes = std::mem::take(&mut self.auto_read_buffers.graphemes);
            graphemes.clear();
            diff_state = DiffState::NoChanges;
            let mut previous_tag = None;
            for change in TextDiff::configure()
                .algorithm(Algorithm::Patience)
                .diff_graphemes(diff_old_text, diff_new_text)
                .iter_all_changes()
            {
                diff_state = next_grapheme_diff_state(diff_state, previous_tag, change.tag());
                previous_tag = Some(change.tag());
                if diff_state != DiffState::Multi
                    && change.tag() == ChangeTag::Insert
                    && let Some(change_str) = change.as_str()
                {
                    graphemes.push_str(change_str);
                }
            }

            if diff_state == DiffState::Multi {
                graphemes.clear();
                if collect_inserted_fields(
                    diff_old_text,
                    diff_new_text,
                    &mut graphemes,
                    &mut self.auto_read_buffers.lcs,
                ) {
                    std::mem::swap(&mut diff_text, &mut graphemes);
                }
            } else {
                std::mem::swap(&mut diff_text, &mut graphemes);
            }
            self.auto_read_buffers.graphemes = graphemes;
        }

        let suppress_echo = self.should_suppress_key_echo(&diff_text);
        if suppress_echo {
            self.auto_read_buffers.diff_text = diff_text;
            return Ok(true);
        }

        let original_nonempty = !diff_text.is_empty();
        if let Some(text) = self.hook_on_live_read(&diff_text, cursor_moves, scrolled)?
            && !text.is_empty()
        {
            self.speak(&text, false)?;
        }
        self.auto_read_buffers.diff_text = diff_text;
        Ok(original_nonempty)
    }
}

fn next_line_diff_state(state: DiffState, tag: ChangeTag) -> DiffState {
    match state {
        DiffState::NoChanges => match tag {
            ChangeTag::Delete => DiffState::OneDeletion,
            ChangeTag::Equal => DiffState::NoChanges,
            ChangeTag::Insert => DiffState::Multi,
        },
        DiffState::OneDeletion => match tag {
            ChangeTag::Delete => DiffState::Multi,
            ChangeTag::Equal => DiffState::OneDeletion,
            ChangeTag::Insert => DiffState::Single,
        },
        DiffState::Single => match tag {
            ChangeTag::Equal => DiffState::Single,
            _ => DiffState::Multi,
        },
        DiffState::Multi => DiffState::Multi,
    }
}

fn next_grapheme_diff_state(
    state: DiffState,
    previous: Option<ChangeTag>,
    tag: ChangeTag,
) -> DiffState {
    match state {
        DiffState::NoChanges => match tag {
            ChangeTag::Delete => DiffState::OneDeletion,
            ChangeTag::Equal => DiffState::NoChanges,
            ChangeTag::Insert => DiffState::Single,
        },
        DiffState::OneDeletion => match tag {
            ChangeTag::Delete if previous == Some(ChangeTag::Delete) => DiffState::OneDeletion,
            ChangeTag::Equal => DiffState::OneDeletion,
            ChangeTag::Insert if previous == Some(ChangeTag::Delete) => DiffState::Single,
            _ => DiffState::Multi,
        },
        DiffState::Single => match tag {
            ChangeTag::Equal => DiffState::Single,
            ChangeTag::Insert
                if previous == Some(ChangeTag::Insert) || previous == Some(ChangeTag::Delete) =>
            {
                DiffState::Single
            }
            _ => DiffState::Multi,
        },
        DiffState::Multi => DiffState::Multi,
    }
}

fn collect_inserted_fields(
    old_text: &str,
    new_text: &str,
    out: &mut String,
    lcs: &mut Vec<usize>,
) -> bool {
    let old_fields: Vec<_> = old_text.split_whitespace().collect();
    let new_fields: Vec<_> = new_text.split_whitespace().collect();
    let old_len = old_fields.len();
    let new_len = new_fields.len();
    if new_len == 0 {
        return false;
    }

    lcs.clear();
    lcs.resize((old_len + 1) * (new_len + 1), 0);
    for old_idx in (0..old_len).rev() {
        for new_idx in (0..new_len).rev() {
            let idx = old_idx * (new_len + 1) + new_idx;
            lcs[idx] = if old_fields[old_idx] == new_fields[new_idx] {
                lcs[(old_idx + 1) * (new_len + 1) + new_idx + 1] + 1
            } else {
                lcs[(old_idx + 1) * (new_len + 1) + new_idx]
                    .max(lcs[old_idx * (new_len + 1) + new_idx + 1])
            };
        }
    }

    let mut old_idx = 0;
    let mut new_idx = 0;
    let mut deleted_hunk = Vec::new();
    let mut inserted_hunk = Vec::new();
    let mut last_spoken_hunk = String::new();
    let mut spoke = false;
    while old_idx < old_len || new_idx < new_len {
        if old_idx < old_len && new_idx < new_len && old_fields[old_idx] == new_fields[new_idx] {
            flush_inserted_field_hunk(
                &deleted_hunk,
                &inserted_hunk,
                out,
                &mut last_spoken_hunk,
                &mut spoke,
            );
            deleted_hunk.clear();
            inserted_hunk.clear();
            old_idx += 1;
            new_idx += 1;
        } else if new_idx < new_len
            && (old_idx == old_len
                || lcs[old_idx * (new_len + 1) + new_idx + 1]
                    >= lcs[(old_idx + 1) * (new_len + 1) + new_idx])
        {
            inserted_hunk.push(new_fields[new_idx]);
            new_idx += 1;
        } else {
            deleted_hunk.push(old_fields[old_idx]);
            old_idx += 1;
        }
    }
    flush_inserted_field_hunk(
        &deleted_hunk,
        &inserted_hunk,
        out,
        &mut last_spoken_hunk,
        &mut spoke,
    );

    spoke
}

fn flush_inserted_field_hunk(
    deleted: &[&str],
    inserted: &[&str],
    out: &mut String,
    last_spoken_hunk: &mut String,
    spoke: &mut bool,
) {
    if inserted.is_empty() {
        return;
    }

    let mut hunk = String::new();
    if deleted.len() == inserted.len() {
        for (old_field, new_field) in deleted.iter().zip(inserted) {
            append_inserted_field(field_replacement(old_field, new_field), &mut hunk);
        }
    } else {
        for field in inserted {
            append_inserted_field(field, &mut hunk);
        }
    }

    if hunk.is_empty() || hunk == *last_spoken_hunk {
        return;
    }

    if *spoke {
        out.push(' ');
    }
    out.push_str(&hunk);
    last_spoken_hunk.clone_from(&hunk);
    *spoke = true;
}

fn append_inserted_field(field: &str, out: &mut String) {
    if field.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push(' ');
    }
    out.push_str(field);
}

fn field_replacement<'a>(old_field: &str, new_field: &'a str) -> &'a str {
    let mut prefix_len = 0;
    for ((old_idx, old_ch), (new_idx, new_ch)) in
        old_field.char_indices().zip(new_field.char_indices())
    {
        if old_ch != new_ch {
            break;
        }
        prefix_len = new_idx + new_ch.len_utf8();
        debug_assert_eq!(prefix_len, old_idx + old_ch.len_utf8());
    }

    let old_suffix_source = &old_field[prefix_len..];
    let new_suffix_source = &new_field[prefix_len..];
    let mut suffix_len = 0;
    for (old_ch, new_ch) in old_suffix_source
        .chars()
        .rev()
        .zip(new_suffix_source.chars().rev())
    {
        if old_ch != new_ch {
            break;
        }
        suffix_len += new_ch.len_utf8();
    }

    let mut start = prefix_len;
    let mut end = new_field.len() - suffix_len;
    while start > 0 {
        let Some((previous_index, previous_character)) =
            new_field[..start].char_indices().next_back()
        else {
            break;
        };
        if !is_word_char(previous_character) {
            break;
        }
        start = previous_index;
    }
    while end < new_field.len() {
        let Some(next_character) = new_field[end..].chars().next() else {
            break;
        };
        if !is_word_char(next_character) {
            break;
        }
        end += next_character.len_utf8();
    }
    &new_field[start..end]
}

fn is_word_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

#[cfg(test)]
mod tests {
    use super::{collect_inserted_fields, field_replacement};

    #[test]
    fn replacement_expands_to_word_boundaries() {
        assert_eq!(
            field_replacement("status=ready", "status=running"),
            "running"
        );
        assert_eq!(field_replacement("x42", "x900"), "x900");
    }

    #[test]
    fn inserted_fields_preserve_separate_non_duplicate_hunks() {
        let mut output = String::new();
        let mut lcs = Vec::new();
        assert!(collect_inserted_fields(
            "a old b old c",
            "a first b second c",
            &mut output,
            &mut lcs,
        ));
        assert_eq!(output, "first second");
    }

    #[test]
    fn inserted_fields_collapse_adjacent_duplicate_hunks() {
        let mut output = String::new();
        let mut lcs = Vec::new();
        assert!(collect_inserted_fields(
            "a old b old c",
            "a new b new c",
            &mut output,
            &mut lcs,
        ));
        assert_eq!(output, "new");
    }
}
