use super::{Result, ScreenReader};
use crate::{
    presentation::{ViewId, ViewRevision},
    terminal::{Style, TerminalSnapshot},
    view::View,
};

/// A one-shot causal claim that the child was sent a key press. The key's
/// identity is deliberately irrelevant: applications may bind focus movement
/// to letters, control keys, function keys, or dynamically configured input.
/// The revision boundary prevents an older queued physical frame from
/// satisfying newer input, while `input_sequence` invalidates the claim when
/// any later key is observed.
pub(super) struct PendingVisualFocusInput {
    view_id: ViewId,
    revision_boundary: Option<ViewRevision>,
    input_sequence: u64,
}

impl ScreenReader {
    /// Invalidate both interpretations paired with the most recently
    /// forwarded key. Non-key input and view changes cannot satisfy either.
    pub(crate) fn clear_pending_visual_focus_input(&mut self) {
        self.pending_visual_focus_input = None;
        self.clear_pending_history_navigation();
    }

    /// Record any decoded key press which actually reached the child. Raw byte
    /// traffic, empty transcodes, mouse/paste events, and keys consumed by
    /// Lector never create visual-focus evidence. Whether the key moved focus
    /// is decided entirely from the later presented frame.
    pub(crate) fn record_forwarded_visual_focus_input(
        &mut self,
        view_id: ViewId,
        revision_boundary: Option<ViewRevision>,
        forwarded: bool,
    ) {
        if !forwarded {
            self.clear_pending_history_navigation();
        }
        self.pending_visual_focus_input = forwarded.then_some(PendingVisualFocusInput {
            view_id,
            revision_boundary,
            input_sequence: self.input_sequence,
        });
    }

    /// Consume the input claim at the first causally later, physically
    /// accessible frame. Stabilization and presentation receipts are enforced
    /// by the caller before auto-read reaches this method.
    fn take_visual_focus_input_response(&mut self, view: &View) -> bool {
        let Some(pending) = self.pending_visual_focus_input.as_ref() else {
            return false;
        };
        if pending.input_sequence != self.input_sequence || pending.view_id != view.view_id() {
            self.clear_pending_visual_focus_input();
            return false;
        }
        if !revision_passed(pending.revision_boundary, view.accessibility_revision()) {
            return false;
        }
        self.pending_visual_focus_input.take().is_some()
    }

    /// `Some(false)` keeps both interpretations pending on an older physical
    /// receipt, `Some(true)` permits arbitration, and `None` means there is no
    /// valid claim for this view.
    pub(crate) fn visual_focus_response_presentation_ready(&self, view: &View) -> Option<bool> {
        let pending = self.pending_visual_focus_input.as_ref()?;
        if pending.input_sequence != self.input_sequence || pending.view_id != view.view_id() {
            return None;
        }
        Some(revision_passed(
            pending.revision_boundary,
            view.accessibility_revision(),
        ))
    }

    /// Read a physically proven visual-focus transfer. Callers may give this
    /// exact physical evidence precedence over a higher-level interpretation:
    /// a shell semantic marker can remain on the primary screen while a child
    /// interface temporarily owns it.
    pub(crate) fn read_visual_focus_transfer(&mut self, view: &View) -> Result<bool> {
        let update = view.accessibility_update_summary();
        let cursor_moves = update.cursor_operations;
        let scrolled = update.scroll_operations > 0;
        if !self.take_visual_focus_input_response(view) {
            return Ok(false);
        }
        // Preserve the explicitly enabled legacy black-on-yellow tracker as a
        // separate policy and avoid speaking the same transfer twice.
        if self.highlight_tracking_enabled()
            || scrolled
            || !(update.output_report_structural && !scrolled)
            || update.has_linear_output_report()
        {
            return Ok(false);
        }

        let mut text = String::new();
        if !collect_visual_focus_transfer(view.prev_screen(), view.screen(), &mut text) {
            return Ok(false);
        }

        let original_nonempty = !text.is_empty();
        if let Some(text) = self.hook_on_live_read(&text, cursor_moves, scrolled)?
            && !text.is_empty()
        {
            self.speak(&text, false)?;
        }
        Ok(original_nonempty)
    }
}

fn revision_passed(
    boundary: Option<ViewRevision>,
    accessibility_revision: Option<ViewRevision>,
) -> bool {
    match (boundary, accessibility_revision) {
        (Some(boundary), Some(current)) => current > boundary,
        (None, None) => true,
        _ => false,
    }
}

struct StyleChangeGroup {
    before: Style,
    after: Style,
    cells: Vec<(u16, u16)>,
}

#[derive(Clone, Copy)]
enum RareStyleSide {
    Before,
    After,
}

const MAX_FOCUS_STYLE_CHANGE_GROUPS: usize = 16;

#[derive(Clone, Copy)]
struct CellSpan {
    row: u16,
    start: u16,
    end: u16,
}

/// Detect one of the deliberately narrow visual focus representations which
/// can be proven from two terminal snapshots. False negatives fall back to
/// existing cursor/review behavior, whereas a false positive speaks unrelated
/// paint as focus.
fn collect_visual_focus_transfer(
    previous: &TerminalSnapshot,
    current: &TerminalSnapshot,
    out: &mut String,
) -> bool {
    out.clear();
    if collect_style_focus_transfer(previous, current, out) {
        return true;
    }
    collect_moving_gutter_marker(previous, current, out)
}

/// Detect a bounded bundle of reciprocal style transfers between two textual
/// rows. A focus theme may style one bounded payload run, match fragments
/// within it, and a separate gutter independently; all of those channels still
/// have to exchange exact styles between the same two rows, and every channel
/// with rarity evidence must agree on which row gained the rare style.
/// Keep this path independent from textual markers so its intentionally strict,
/// coordinate-stable behavior does not change.
fn collect_style_focus_transfer(
    previous: &TerminalSnapshot,
    current: &TerminalSnapshot,
    out: &mut String,
) -> bool {
    out.clear();
    if previous.screen != current.screen
        || previous.geometry != current.geometry
        || previous.size() != current.size()
        || previous.rows.len() != current.rows.len()
        || !focus_cursor_state_stable(previous, current)
    {
        return false;
    }

    let mut groups: Vec<StyleChangeGroup> = Vec::with_capacity(4);
    let mut changed_rows = [None, None];
    for (row_index, (old_row, new_row)) in previous.rows.iter().zip(current.rows.iter()).enumerate()
    {
        if old_row.wrapped != new_row.wrapped || old_row.cells.len() != new_row.cells.len() {
            return false;
        }
        for (col_index, (old_cell, new_cell)) in
            old_row.cells.iter().zip(new_row.cells.iter()).enumerate()
        {
            // Coordinate-stable text is the strongest evidence available from
            // a VT grid. Reject even small textual or hyperlink mutations here;
            // ordinary auto-read remains responsible for those frames.
            if old_cell.grapheme != new_cell.grapheme
                || old_cell.width != new_cell.width
                || old_cell.continuation != new_cell.continuation
                || old_cell.hyperlink != new_cell.hyperlink
            {
                return false;
            }
            if old_cell.style == new_cell.style {
                continue;
            }
            if !focus_style_change(&old_cell.style, &new_cell.style) {
                return false;
            }

            let Ok(row) = u16::try_from(row_index) else {
                return false;
            };
            let Ok(col) = u16::try_from(col_index) else {
                return false;
            };
            if !remember_changed_row(&mut changed_rows, row) {
                return false;
            }

            if let Some(group) = groups
                .iter_mut()
                .find(|group| group.before == old_cell.style && group.after == new_cell.style)
            {
                group.cells.push((row, col));
            } else {
                if groups.len() == MAX_FOCUS_STYLE_CHANGE_GROUPS {
                    return false;
                }
                groups.push(StyleChangeGroup {
                    before: old_cell.style.clone(),
                    after: new_cell.style.clone(),
                    cells: vec![(row, col)],
                });
            }
        }
    }

    // Preserve the original single-channel path, including horizontal focus
    // transfers within one row. Compound transfers below need two distinct
    // rows so their independent style channels have an unambiguous owner.
    if groups.len() == 2 && collect_single_style_focus_transfer(previous, current, &groups, out) {
        return true;
    }

    let [Some(first_row), Some(second_row)] = changed_rows else {
        return false;
    };
    if groups.len() < 2 || !groups.len().is_multiple_of(2) {
        return false;
    }

    let mut paired = vec![false; groups.len()];
    let mut destination = None;
    let mut decisive_pairs = Vec::with_capacity(groups.len() / 2);
    for index in 0..groups.len() {
        if paired[index] {
            continue;
        }
        let Some(group_row) = style_change_group_row(&groups[index]) else {
            return false;
        };
        let Some(reverse_index) = groups.iter().enumerate().find_map(|(candidate, group)| {
            (candidate != index
                && !paired[candidate]
                && group.before == groups[index].after
                && group.after == groups[index].before)
                .then_some(candidate)
        }) else {
            return false;
        };
        let Some(reverse_row) = style_change_group_row(&groups[reverse_index]) else {
            return false;
        };
        if !rows_are_the_focus_pair(first_row, second_row, group_row, reverse_row) {
            return false;
        }

        paired[index] = true;
        paired[reverse_index] = true;
        if let Some(pair_destination) = reciprocal_style_pair_destination(
            previous,
            current,
            &groups[index],
            group_row,
            reverse_row,
        ) {
            if destination.is_some_and(|known| known != pair_destination) {
                return false;
            }
            destination = Some(pair_destination);
            decisive_pairs.push((index, reverse_index));
        }
    }

    // Key identity supplies no directional semantics. At least one style pair
    // must make the selected side demonstrably rarer than its baseline, and
    // every such pair must identify the same destination row. Ambiguous pairs
    // may corroborate that row but can never choose it.
    let Some(destination) = destination else {
        return false;
    };
    let source = if destination == first_row {
        second_row
    } else {
        first_row
    };
    let Some(removed_span) =
        single_meaningful_style_change_span(previous, current, previous, source)
    else {
        return false;
    };
    let Some(added_span) =
        single_meaningful_style_change_span(previous, current, current, destination)
    else {
        return false;
    };

    // Decoration may participate in the same row transfer, but it cannot be
    // the only directional evidence. Preserve the old requirement that a rare
    // style itself moves between meaningful text in the source and destination.
    if !decisive_pairs.iter().any(|(first, second)| {
        let (removed, added) = if style_change_group_row(&groups[*first]) == Some(source) {
            (&groups[*first], &groups[*second])
        } else {
            (&groups[*second], &groups[*first])
        };
        group_intersects_alphanumeric_text(previous, removed, removed_span)
            && group_intersects_alphanumeric_text(current, added, added_span)
    }) {
        return false;
    }

    // A visible hardware cursor anywhere on either candidate row has more
    // authoritative ownership than an inferred style cursor.
    if current.cursor.visible && (current.cursor.row == source || current.cursor.row == destination)
    {
        return false;
    }

    finish_style_focus_span(current, added_span, out)
}

fn collect_single_style_focus_transfer(
    previous: &TerminalSnapshot,
    current: &TerminalSnapshot,
    groups: &[StyleChangeGroup],
    out: &mut String,
) -> bool {
    if groups[0].before != groups[1].after || groups[0].after != groups[1].before {
        return false;
    }

    let Some(first_span) = single_contiguous_span(&groups[0].cells) else {
        return false;
    };
    let Some(second_span) = single_contiguous_span(&groups[1].cells) else {
        return false;
    };
    if spans_overlap(first_span, second_span) {
        return false;
    }

    let added_index = match rare_style_side(previous, current, &groups[0].before, &groups[0].after)
    {
        Some(RareStyleSide::Before) => 1,
        Some(RareStyleSide::After) => 0,
        None => return false,
    };
    let (removed_span, added_span) = if added_index == 0 {
        (second_span, first_span)
    } else {
        (first_span, second_span)
    };
    if !bounded_text_run(previous, removed_span)
        || !bounded_text_run(current, added_span)
        || span_alphanumeric_count(previous, removed_span) < 2
        || span_alphanumeric_count(current, added_span) < 2
    {
        return false;
    }
    if current.cursor.visible
        && (span_contains(removed_span, current.cursor_position())
            || span_contains(added_span, current.cursor_position()))
    {
        return false;
    }

    finish_style_focus_span(current, added_span, out)
}

fn finish_style_focus_span(
    current: &TerminalSnapshot,
    added_span: CellSpan,
    out: &mut String,
) -> bool {
    append_span_text(current, added_span, out);
    let trimmed = out.trim();
    if trimmed
        .chars()
        .filter(|character| character.is_alphanumeric())
        .count()
        < 2
    {
        out.clear();
        return false;
    }
    if trimmed.len() != out.len() {
        let trimmed = trimmed.to_owned();
        out.clear();
        out.push_str(&trimmed);
    }
    true
}

fn style_change_group_row(group: &StyleChangeGroup) -> Option<u16> {
    let row = group.cells.first()?.0;
    group.cells.iter().all(|cell| cell.0 == row).then_some(row)
}

fn rows_are_the_focus_pair(first: u16, second: u16, left: u16, right: u16) -> bool {
    left != right && (left == first && right == second || left == second && right == first)
}

fn reciprocal_style_pair_destination(
    previous: &TerminalSnapshot,
    current: &TerminalSnapshot,
    forward: &StyleChangeGroup,
    forward_row: u16,
    reverse_row: u16,
) -> Option<u16> {
    match rare_style_side(previous, current, &forward.before, &forward.after) {
        Some(RareStyleSide::Before) => Some(reverse_row),
        Some(RareStyleSide::After) => Some(forward_row),
        None => None,
    }
}

fn rare_style_side(
    previous: &TerminalSnapshot,
    current: &TerminalSnapshot,
    before: &Style,
    after: &Style,
) -> Option<RareStyleSide> {
    let before_count =
        textual_style_count(previous, before).saturating_add(textual_style_count(current, before));
    let after_count =
        textual_style_count(previous, after).saturating_add(textual_style_count(current, after));

    // A two-to-one prevalence margin establishes which exact style is the
    // selection. A style absent from all nonblank cells supplies no evidence.
    if before_count > 0 && before_count.saturating_mul(2) <= after_count {
        Some(RareStyleSide::Before)
    } else if after_count > 0 && after_count.saturating_mul(2) <= before_count {
        Some(RareStyleSide::After)
    } else {
        None
    }
}

/// Find the sole bounded, text-bearing component in one changed row. Multiple
/// reciprocal style channels may occupy that component. Separate punctuation
/// decorations, such as a stationary gutter bar, remain outside the spoken
/// span and do not make the row ambiguous.
fn single_meaningful_style_change_span(
    previous: &TerminalSnapshot,
    current: &TerminalSnapshot,
    text: &TerminalSnapshot,
    row: u16,
) -> Option<CellSpan> {
    let old_cells = &previous.rows.get(usize::from(row))?.cells;
    let new_cells = &current.rows.get(usize::from(row))?.cells;
    if old_cells.len() != new_cells.len() {
        return None;
    }

    let mut candidate = None;
    let mut start = None;
    for column in 0..=old_cells.len() {
        let changed =
            column < old_cells.len() && old_cells[column].style != new_cells[column].style;
        if changed {
            start.get_or_insert(column);
            continue;
        }
        let Some(component_start) = start.take() else {
            continue;
        };
        let component_end = column.saturating_sub(1);
        let Ok(start) = u16::try_from(component_start) else {
            return None;
        };
        let Ok(end) = u16::try_from(component_end) else {
            return None;
        };
        let span = CellSpan { row, start, end };
        if span_alphanumeric_count(text, span) < 2 {
            continue;
        }
        if candidate.is_some() || !bounded_text_run(text, span) {
            return None;
        }
        candidate = Some(span);
    }
    candidate
}

fn group_intersects_alphanumeric_text(
    snapshot: &TerminalSnapshot,
    group: &StyleChangeGroup,
    span: CellSpan,
) -> bool {
    group.cells.iter().any(|(row, col)| {
        *row == span.row
            && *col >= span.start
            && *col <= span.end
            && snapshot
                .rows
                .get(usize::from(*row))
                .and_then(|row| row.cells.get(usize::from(*col)))
                .is_some_and(cell_has_word_content)
    })
}

/// Detect a short, punctuation-like line pointer which moved between two
/// otherwise stable rows. The marker's glyph is learned from the reciprocal
/// text change; `>`, Unicode pointers, and application-configured tokens all
/// take the same path. Unchanged copies, such as an identical input prompt,
/// are deliberately irrelevant.
fn collect_moving_gutter_marker(
    previous: &TerminalSnapshot,
    current: &TerminalSnapshot,
    out: &mut String,
) -> bool {
    out.clear();
    if previous.screen != current.screen
        || previous.geometry != current.geometry
        || previous.size() != current.size()
        || previous.rows.len() != current.rows.len()
        || !focus_cursor_state_stable(previous, current)
    {
        return false;
    }

    let mut removed_cells = Vec::with_capacity(3);
    let mut added_cells = Vec::with_capacity(3);
    for (row_index, (old_row, new_row)) in previous.rows.iter().zip(current.rows.iter()).enumerate()
    {
        if old_row.wrapped != new_row.wrapped || old_row.cells.len() != new_row.cells.len() {
            return false;
        }
        for (col_index, (old_cell, new_cell)) in
            old_row.cells.iter().zip(new_row.cells.iter()).enumerate()
        {
            // Width, continuation ownership, and links are semantic structure,
            // not cursor decoration. They must remain exact everywhere.
            if old_cell.width != new_cell.width
                || old_cell.continuation != new_cell.continuation
                || old_cell.hyperlink != new_cell.hyperlink
            {
                return false;
            }
            if same_visible_grapheme(old_cell, new_cell) {
                continue;
            }

            let Ok(row) = u16::try_from(row_index) else {
                return false;
            };
            let Ok(col) = u16::try_from(col_index) else {
                return false;
            };
            if marker_cell(old_cell) && semantic_blank(new_cell) {
                removed_cells.push((row, col));
            } else if semantic_blank(old_cell) && marker_cell(new_cell) {
                added_cells.push((row, col));
            } else {
                return false;
            }
            if removed_cells.len() > 3 || added_cells.len() > 3 {
                return false;
            }
        }
    }

    let Some(removed_span) = single_contiguous_span(&removed_cells) else {
        return false;
    };
    let Some(added_span) = single_contiguous_span(&added_cells) else {
        return false;
    };
    if removed_span.row == added_span.row
        || removed_span.start != added_span.start
        || removed_span.end != added_span.end
        || previous.rows[usize::from(removed_span.row)].wrapped
        || current.rows[usize::from(added_span.row)].wrapped
        || !stable_leading_decoration(previous, removed_span.row, removed_span.start)
        || !stable_leading_decoration(current, removed_span.row, removed_span.start)
        || !stable_leading_decoration(previous, added_span.row, added_span.start)
        || !stable_leading_decoration(current, added_span.row, added_span.start)
    {
        return false;
    }

    let mut removed_marker = String::new();
    let mut added_marker = String::new();
    append_span_text(previous, removed_span, &mut removed_marker);
    append_span_text(current, added_span, &mut added_marker);
    if removed_marker != added_marker || removed_marker.is_empty() {
        return false;
    }

    let (label_start, marker_requires_style) = if let Some((start, has_blank_separator)) =
        aligned_marker_label_start(previous, current, removed_span, added_span)
    {
        (start, !has_blank_separator)
    } else if let Some(start) =
        aligned_adjacent_marker_label_start(previous, current, removed_span, added_span)
    {
        (start, true)
    } else {
        return false;
    };
    if row_payload_alphanumeric_count(previous, removed_span.row, label_start) < 2
        || row_payload_alphanumeric_count(current, added_span.row, label_start) < 2
        || !marker_and_payload_visible(previous, removed_span, label_start)
        || !marker_and_payload_visible(current, added_span, label_start)
    {
        return false;
    }

    // A visible terminal cursor on either candidate row gives the application
    // cursor a more authoritative owner than an inferred textual pointer.
    if current.cursor.visible
        && (current.cursor.row == removed_span.row || current.cursor.row == added_span.row)
    {
        return false;
    }

    let Some(reciprocal_style) =
        marker_style_evidence(previous, current, removed_span.row, added_span.row)
    else {
        return false;
    };
    if marker_requires_style && !reciprocal_style
        || !marker_requires_style
            && !reciprocal_style
            && !has_stable_marker_peer(previous, current, removed_span, added_span, label_start)
    {
        return false;
    }

    let (_, columns) = current.size();
    if columns == 0 {
        return false;
    }
    append_span_text(
        current,
        CellSpan {
            row: added_span.row,
            start: label_start,
            end: columns - 1,
        },
        out,
    );
    let trimmed = out.trim();
    if trimmed
        .chars()
        .filter(|character| character.is_alphanumeric())
        .count()
        < 2
    {
        out.clear();
        return false;
    }
    if trimmed.len() != out.len() {
        let trimmed = trimmed.to_owned();
        out.clear();
        out.push_str(&trimmed);
    }
    true
}

fn semantic_blank(cell: &crate::terminal::Cell) -> bool {
    !cell.continuation
        && (cell.grapheme.is_empty() || cell.grapheme.chars().all(|character| character == ' '))
}

fn focus_cursor_state_stable(previous: &TerminalSnapshot, current: &TerminalSnapshot) -> bool {
    let before = previous.cursor;
    let after = current.cursor;
    before.visible == after.visible
        && before.shape == after.shape
        && (!before.visible || (before.row == after.row && before.col == after.col))
}

fn same_visible_grapheme(before: &crate::terminal::Cell, after: &crate::terminal::Cell) -> bool {
    before.grapheme == after.grapheme || semantic_blank(before) && semantic_blank(after)
}

fn marker_cell(cell: &crate::terminal::Cell) -> bool {
    cell.width == 1
        && !cell.continuation
        && !semantic_blank(cell)
        && cell
            .grapheme
            .chars()
            .all(|character| !character.is_alphanumeric() && !character.is_whitespace())
}

fn gutter_decoration_cell(cell: &crate::terminal::Cell) -> bool {
    cell.width == 1
        && !cell.continuation
        && cell.hyperlink.is_none()
        && !cell.style.invisible
        && !cell.style.blink
        && cell
            .grapheme
            .chars()
            .all(|character| !character.is_alphanumeric())
}

fn stable_leading_decoration(snapshot: &TerminalSnapshot, row: u16, marker_start: u16) -> bool {
    snapshot.rows.get(usize::from(row)).is_some_and(|row| {
        row.cells
            .iter()
            .take(usize::from(marker_start))
            .all(gutter_decoration_cell)
    })
}

fn marker_label_start(snapshot: &TerminalSnapshot, row: u16, marker_end: u16) -> Option<u16> {
    let cells = &snapshot.rows.get(usize::from(row))?.cells;
    let separator = marker_end.checked_add(1)?;
    if !cells
        .get(usize::from(separator))
        .is_some_and(gutter_decoration_cell)
    {
        return None;
    }
    let payload = separator.checked_add(1)?;
    if cells.get(usize::from(payload)).is_none_or(semantic_blank) {
        return None;
    }
    Some(payload)
}

fn aligned_marker_label_start(
    previous: &TerminalSnapshot,
    current: &TerminalSnapshot,
    removed: CellSpan,
    added: CellSpan,
) -> Option<(u16, bool)> {
    let start = marker_label_start(previous, removed.row, removed.end)?;
    if marker_label_start(current, removed.row, removed.end) != Some(start)
        || marker_label_start(previous, added.row, added.end) != Some(start)
        || marker_label_start(current, added.row, added.end) != Some(start)
    {
        return None;
    }
    let has_blank_separator = [
        (previous, removed.row, removed.end),
        (current, removed.row, removed.end),
        (previous, added.row, added.end),
        (current, added.row, added.end),
    ]
    .into_iter()
    .any(|(snapshot, row, marker_end)| {
        marker_end
            .checked_add(1)
            .and_then(|column| {
                snapshot
                    .rows
                    .get(usize::from(row))?
                    .cells
                    .get(usize::from(column))
            })
            .is_some_and(semantic_blank)
    });
    Some((start, has_blank_separator))
}

fn adjacent_marker_label_start(
    snapshot: &TerminalSnapshot,
    row: u16,
    marker_end: u16,
) -> Option<u16> {
    let start = marker_end.checked_add(1)?;
    snapshot
        .rows
        .get(usize::from(row))?
        .cells
        .get(usize::from(start))
        .is_some_and(|cell| !cell.continuation && !semantic_blank(cell))
        .then_some(start)
}

fn aligned_adjacent_marker_label_start(
    previous: &TerminalSnapshot,
    current: &TerminalSnapshot,
    removed: CellSpan,
    added: CellSpan,
) -> Option<u16> {
    let start = adjacent_marker_label_start(previous, removed.row, removed.end)?;
    (adjacent_marker_label_start(current, removed.row, removed.end) == Some(start)
        && adjacent_marker_label_start(previous, added.row, added.end) == Some(start)
        && adjacent_marker_label_start(current, added.row, added.end) == Some(start))
    .then_some(start)
}

fn row_payload_alphanumeric_count(snapshot: &TerminalSnapshot, row: u16, start: u16) -> usize {
    snapshot
        .rows
        .get(usize::from(row))
        .into_iter()
        .flat_map(|row| row.cells.iter().skip(usize::from(start)))
        .filter(|cell| !cell.continuation)
        .flat_map(|cell| cell.grapheme.chars())
        .filter(|character| character.is_alphanumeric())
        .count()
}

fn marker_and_payload_visible(
    snapshot: &TerminalSnapshot,
    marker: CellSpan,
    label_start: u16,
) -> bool {
    let Some(row) = snapshot.rows.get(usize::from(marker.row)) else {
        return false;
    };
    row.cells
        .iter()
        .enumerate()
        .filter(|(column, cell)| {
            (*column >= usize::from(marker.start) && *column <= usize::from(marker.end)
                || *column >= usize::from(label_start))
                && !semantic_blank(cell)
        })
        .all(|(_, cell)| !cell.style.invisible && !cell.style.blink)
}

/// Return whether any exact reciprocal style transition corroborates the
/// marker. Additional visible style changes are safe when confined to the two
/// candidate rows: fzf-like interfaces can independently style the pointer,
/// current item, and matching text. `None` rejects style motion elsewhere or
/// any blink/conceal transition.
fn marker_style_evidence(
    previous: &TerminalSnapshot,
    current: &TerminalSnapshot,
    removed_row: u16,
    added_row: u16,
) -> Option<bool> {
    let mut transitions: Vec<(u16, Style, Style)> = Vec::new();
    for (row_index, (old_row, new_row)) in previous.rows.iter().zip(current.rows.iter()).enumerate()
    {
        for (old_cell, new_cell) in old_row.cells.iter().zip(new_row.cells.iter()) {
            if old_cell.style == new_cell.style {
                continue;
            }
            let row = u16::try_from(row_index).ok()?;
            if (row != removed_row && row != added_row)
                || old_cell.style.invisible
                || new_cell.style.invisible
                || old_cell.style.blink
                || new_cell.style.blink
            {
                return None;
            }
            if !transitions.iter().any(|(known_row, before, after)| {
                *known_row == row && *before == old_cell.style && *after == new_cell.style
            }) {
                transitions.push((row, old_cell.style.clone(), new_cell.style.clone()));
            }
        }
    }

    Some(transitions.iter().any(|(row, before, after)| {
        transitions
            .iter()
            .any(|(other_row, other_before, other_after)| {
                *row != *other_row
                    && ((*row == removed_row && *other_row == added_row)
                        || (*row == added_row && *other_row == removed_row))
                    && *before == *other_after
                    && *after == *other_before
            })
    }))
}

fn has_stable_marker_peer(
    previous: &TerminalSnapshot,
    current: &TerminalSnapshot,
    removed: CellSpan,
    added: CellSpan,
    label_start: u16,
) -> bool {
    previous
        .rows
        .iter()
        .zip(current.rows.iter())
        .enumerate()
        .any(|(row_index, (old_row, new_row))| {
            let Ok(row) = u16::try_from(row_index) else {
                return false;
            };
            if row == removed.row || row == added.row || old_row.wrapped || new_row.wrapped {
                return false;
            }
            let marker_blank = (removed.start..=removed.end).all(|column| {
                old_row
                    .cells
                    .get(usize::from(column))
                    .is_some_and(semantic_blank)
                    && new_row
                        .cells
                        .get(usize::from(column))
                        .is_some_and(semantic_blank)
            });
            marker_blank
                && stable_leading_decoration(previous, row, removed.start)
                && stable_leading_decoration(current, row, removed.start)
                && marker_label_start(previous, row, removed.end) == Some(label_start)
                && marker_label_start(current, row, removed.end) == Some(label_start)
                && row_payload_alphanumeric_count(current, row, label_start) >= 2
                && marker_and_payload_visible(
                    current,
                    CellSpan {
                        row,
                        start: removed.start,
                        end: removed.end,
                    },
                    label_start,
                )
        })
}

fn focus_style_change(before: &Style, after: &Style) -> bool {
    if before.invisible || after.invisible || before.blink || after.blink {
        return false;
    }
    let underline_visible =
        before.underline != Default::default() || after.underline != Default::default();
    before.foreground != after.foreground
        || before.background != after.background
        || before.bold != after.bold
        || before.inverse != after.inverse
        || before.underline != after.underline
        || (underline_visible && before.underline_color != after.underline_color)
}

fn remember_changed_row(rows: &mut [Option<u16>; 2], row: u16) -> bool {
    if rows.iter().flatten().any(|known| *known == row) {
        return true;
    }
    if let Some(slot) = rows.iter_mut().find(|known| known.is_none()) {
        *slot = Some(row);
        true
    } else {
        false
    }
}

fn textual_style_count(snapshot: &TerminalSnapshot, style: &Style) -> usize {
    snapshot
        .rows
        .iter()
        .flat_map(|row| row.cells.iter())
        .filter(|cell| {
            !cell.continuation
                && cell.style == *style
                && cell
                    .grapheme
                    .chars()
                    .any(|character| !character.is_whitespace())
        })
        .count()
}

fn single_contiguous_span(cells: &[(u16, u16)]) -> Option<CellSpan> {
    let &(row, start) = cells.first()?;
    let mut end = start;
    for &(cell_row, col) in &cells[1..] {
        if cell_row != row || col != end.saturating_add(1) {
            return None;
        }
        end = col;
    }
    Some(CellSpan { row, start, end })
}

fn spans_overlap(left: CellSpan, right: CellSpan) -> bool {
    left.row == right.row && left.start <= right.end && right.start <= left.end
}

fn span_contains(span: CellSpan, position: (u16, u16)) -> bool {
    position.0 == span.row && position.1 >= span.start && position.1 <= span.end
}

fn bounded_text_run(snapshot: &TerminalSnapshot, span: CellSpan) -> bool {
    let Some(row) = snapshot.rows.get(usize::from(span.row)) else {
        return false;
    };
    let left_is_word = span
        .start
        .checked_sub(1)
        .and_then(|col| row.cells.get(usize::from(col)))
        .is_some_and(cell_has_word_content);
    let right_is_word = row
        .cells
        .get(usize::from(span.end.saturating_add(1)))
        .is_some_and(cell_has_word_content);
    !left_is_word && !right_is_word
}

fn cell_has_word_content(cell: &crate::terminal::Cell) -> bool {
    !cell.continuation
        && cell
            .grapheme
            .chars()
            .any(|character| character.is_alphanumeric())
}

fn span_alphanumeric_count(snapshot: &TerminalSnapshot, span: CellSpan) -> usize {
    let Some(row) = snapshot.rows.get(usize::from(span.row)) else {
        return 0;
    };
    row.cells
        .iter()
        .skip(usize::from(span.start))
        .take(usize::from(
            span.end.saturating_sub(span.start).saturating_add(1),
        ))
        .filter(|cell| !cell.continuation)
        .flat_map(|cell| cell.grapheme.chars())
        .filter(|character| character.is_alphanumeric())
        .count()
}

fn append_span_text(snapshot: &TerminalSnapshot, span: CellSpan, out: &mut String) {
    let Some(row) = snapshot.rows.get(usize::from(span.row)) else {
        return;
    };
    for cell in row
        .cells
        .iter()
        .skip(usize::from(span.start))
        .take(usize::from(
            span.end.saturating_sub(span.start).saturating_add(1),
        ))
    {
        if !cell.continuation {
            if cell.grapheme.is_empty() {
                out.push(' ');
            } else {
                out.push_str(&cell.grapheme);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::collect_visual_focus_transfer;
    use crate::{
        screen_reader::ScreenReader,
        speech,
        terminal::{Cell, Color, Cursor, Row, Style, TerminalGeometry, TerminalSnapshot},
        view::View,
    };
    use std::{borrow::Cow, ops::RangeInclusive, sync::Arc};

    const WIDTH: usize = 24;

    fn inverse() -> Style {
        Style {
            inverse: true,
            ..Style::default()
        }
    }

    fn colored_bold() -> Style {
        Style {
            foreground: Color::Rgb(232, 111, 176),
            background: Color::Indexed(24),
            bold: true,
            ..Style::default()
        }
    }

    fn snapshot(
        lines: &[&str],
        styles: &[(usize, RangeInclusive<usize>, Style)],
    ) -> TerminalSnapshot {
        let rows = lines
            .iter()
            .enumerate()
            .map(|(row_index, text)| {
                let mut cells = (0..WIDTH).map(|_| Cell::default()).collect::<Vec<_>>();
                for (col, character) in text.chars().take(WIDTH).enumerate() {
                    if character != ' ' {
                        cells[col].grapheme = Cow::Owned(character.to_string());
                    }
                }
                for (style_row, columns, style) in styles {
                    if *style_row != row_index {
                        continue;
                    }
                    for col in columns.clone() {
                        if let Some(cell) = cells.get_mut(col) {
                            cell.style = style.clone();
                        }
                    }
                }
                Row {
                    cells: Arc::new(cells),
                    wrapped: false,
                }
            })
            .collect::<Vec<_>>();
        TerminalSnapshot {
            rows: Arc::new(rows),
            geometry: TerminalGeometry::from_cells(lines.len() as u16, WIDTH as u16),
            cursor: Cursor {
                visible: false,
                ..Cursor::default()
            },
            ..TerminalSnapshot::default()
        }
    }

    fn set_grapheme(
        snapshot: &mut TerminalSnapshot,
        row: usize,
        column: usize,
        grapheme: &'static str,
    ) {
        let rows = Arc::make_mut(&mut snapshot.rows);
        let cells = Arc::make_mut(&mut rows[row].cells);
        cells[column].grapheme = Cow::Borrowed(grapheme);
    }

    fn set_style(
        snapshot: &mut TerminalSnapshot,
        row: usize,
        columns: RangeInclusive<usize>,
        style: Style,
    ) {
        let rows = Arc::make_mut(&mut snapshot.rows);
        let cells = Arc::make_mut(&mut rows[row].cells);
        for column in columns {
            cells[column].style = style.clone();
        }
    }

    fn set_hardware_cursor(snapshot: &mut TerminalSnapshot, row: u16, col: u16) {
        snapshot.cursor = Cursor {
            row,
            col,
            visible: true,
            ..Cursor::default()
        };
    }

    fn detects(previous: &TerminalSnapshot, current: &TerminalSnapshot) -> Option<String> {
        let mut output = String::new();
        collect_visual_focus_transfer(previous, current, &mut output).then_some(output)
    }

    struct SilentDriver;

    impl speech::Driver for SilentDriver {
        fn speak(&mut self, _text: &str, _interrupt: bool) -> anyhow::Result<()> {
            Ok(())
        }

        fn stop(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        fn get_rate(&self) -> f32 {
            1.0
        }

        fn set_rate(&mut self, _rate: f32) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn screen_reader() -> ScreenReader {
        ScreenReader::new(speech::Speech::new(Box::new(SilentDriver)))
    }

    #[test]
    fn controller_consumed_key_does_not_leave_an_intent() {
        let mut reader = screen_reader();
        let view = View::new(2, 10);
        reader.record_last_key(b"\x1b[B");
        reader.record_forwarded_visual_focus_input(
            view.view_id(),
            view.input_intent_revision_boundary(),
            true,
        );
        assert!(reader.pending_visual_focus_input.is_some());

        reader.record_forwarded_visual_focus_input(
            view.view_id(),
            view.input_intent_revision_boundary(),
            false,
        );
        assert!(reader.pending_visual_focus_input.is_none());
    }

    #[test]
    fn invalidating_focus_evidence_clears_its_paired_history_interpretation() {
        let mut reader = screen_reader();
        let view = View::new(2, 10);
        reader.record_forwarded_visual_focus_input(
            view.view_id(),
            view.input_intent_revision_boundary(),
            true,
        );
        reader.set_pending_history_navigation();

        reader.clear_pending_visual_focus_input();

        assert!(reader.pending_visual_focus_input.is_none());
        assert!(!reader.has_pending_history_navigation());
    }

    #[test]
    fn detects_fzf_pointer_while_identical_prompt_stays_put() {
        let mut previous = snapshot(&["> Alpha", "  Bravo", "> query"], &[(0, 0..=6, inverse())]);
        let mut current = snapshot(&["  Alpha", "> Bravo", "> query"], &[(1, 0..=6, inverse())]);
        set_hardware_cursor(&mut previous, 2, 2);
        set_hardware_cursor(&mut current, 2, 2);

        assert_eq!(detects(&previous, &current).as_deref(), Some("Bravo"));
    }

    #[test]
    fn marker_transfer_ignores_hidden_cursor_bookkeeping_coordinates() {
        let mut previous = snapshot(&["> Alpha", "  Bravo", "  Charlie"], &[]);
        let mut current = snapshot(&["  Alpha", "> Bravo", "  Charlie"], &[]);
        previous.cursor.row = 2;
        previous.cursor.col = 9;
        current.cursor.row = 0;
        current.cursor.col = 7;

        assert_eq!(detects(&previous, &current).as_deref(), Some("Bravo"));
    }

    #[test]
    fn marker_only_transfer_requires_and_accepts_a_stable_peer_row() {
        let previous = snapshot(&["▌ Alpha", "  Bravo", "  Charlie", "▌ query"], &[]);
        let current = snapshot(&["  Alpha", "▌ Bravo", "  Charlie", "▌ query"], &[]);

        assert_eq!(detects(&previous, &current).as_deref(), Some("Bravo"));

        let previous = snapshot(&["> Alpha", "  Bravo", "> query"], &[]);
        let current = snapshot(&["  Alpha", "> Bravo", "> query"], &[]);
        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn accepts_two_and_three_cell_application_configured_markers() {
        for (previous_lines, current_lines) in [
            (
                ["-> Alpha", "   Bravo", "   Charlie"],
                ["   Alpha", "-> Bravo", "   Charlie"],
            ),
            (
                ["[>] Alpha", "    Bravo", "    Charlie"],
                ["    Alpha", "[>] Bravo", "    Charlie"],
            ),
        ] {
            let previous = snapshot(&previous_lines, &[]);
            let current = snapshot(&current_lines, &[]);

            assert_eq!(detects(&previous, &current).as_deref(), Some("Bravo"));
        }
    }

    #[test]
    fn accepts_fzf_multi_selection_column_while_pointer_moves() {
        let previous = snapshot(&[">>Alpha", "  Bravo", "  Charlie"], &[]);
        let current = snapshot(&[" >Alpha", "> Bravo", "  Charlie"], &[]);

        assert_eq!(detects(&previous, &current).as_deref(), Some("Bravo"));
    }

    #[test]
    fn accepts_ncurses_adjacent_mark_with_reciprocal_style() {
        let previous = snapshot(&["-Alpha", " Bravo", " Charlie"], &[(0, 0..=5, inverse())]);
        let current = snapshot(&[" Alpha", "-Bravo", " Charlie"], &[(1, 0..=5, inverse())]);

        assert_eq!(detects(&previous, &current).as_deref(), Some("Bravo"));
    }

    #[test]
    fn rejects_adjacent_marker_without_reciprocal_style() {
        let previous = snapshot(&["-Alpha", " Bravo", " Charlie"], &[]);
        let current = snapshot(&[" Alpha", "-Bravo", " Charlie"], &[]);

        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn rejects_unstyled_adjacent_punctuation_payload_with_a_peer() {
        let previous = snapshot(&[">./foo", " ./bar", " ./baz"], &[]);
        let current = snapshot(&[" ./foo", ">./bar", " ./baz"], &[]);

        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn accepts_a_static_nonword_border_before_the_pointer() {
        let previous = snapshot(&["│> Alpha", "│  Bravo", "│  Charlie"], &[]);
        let current = snapshot(&["│  Alpha", "│> Bravo", "│  Charlie"], &[]);

        assert_eq!(detects(&previous, &current).as_deref(), Some("Bravo"));
    }

    #[test]
    fn preserves_punctuation_at_the_start_of_an_item() {
        let previous = snapshot(&["> ./foo", "  ./bar", "  ./baz"], &[]);
        let current = snapshot(&["  ./foo", "> ./bar", "  ./baz"], &[]);

        assert_eq!(detects(&previous, &current).as_deref(), Some("./bar"));
    }

    #[test]
    fn treats_empty_and_printed_space_cells_as_the_same_blank_gutter() {
        let mut previous = snapshot(&["> Alpha", "  Bravo", "  Charlie"], &[]);
        let mut current = snapshot(&["  Alpha", "> Bravo", "  Charlie"], &[]);
        set_grapheme(&mut previous, 1, 0, " ");
        set_grapheme(&mut current, 0, 0, " ");

        assert_eq!(detects(&previous, &current).as_deref(), Some("Bravo"));
    }

    #[test]
    fn peer_backed_marker_allows_safe_nonreciprocal_row_styles() {
        let first = Style {
            italic: true,
            dim: true,
            ..Style::default()
        };
        let second = Style {
            strikethrough: true,
            overline: true,
            ..Style::default()
        };
        let previous = snapshot(&["> Alpha", "  Bravo", "  Charlie"], &[(0, 0..=6, first)]);
        let current = snapshot(
            &["  Alpha", "> Bravo", "  Charlie"],
            &[(0, 0..=6, second.clone()), (1, 0..=6, second)],
        );

        assert_eq!(detects(&previous, &current).as_deref(), Some("Bravo"));
    }

    #[test]
    fn peerless_marker_accepts_a_reciprocal_pair_among_extra_styles() {
        let pointer = Style {
            foreground: Color::Indexed(2),
            bold: true,
            ..Style::default()
        };
        let extra = Style {
            italic: true,
            ..Style::default()
        };
        let mut previous = snapshot(&["> Alpha", "  Bravo", "> query"], &[(0, 0..=6, inverse())]);
        let mut current = snapshot(&["  Alpha", "> Bravo", "> query"], &[(1, 0..=6, inverse())]);
        set_style(&mut previous, 0, 0..=0, pointer);
        set_style(&mut current, 0, 1..=1, extra);

        assert_eq!(detects(&previous, &current).as_deref(), Some("Bravo"));
    }

    #[test]
    fn rejects_peerless_marker_with_only_nonreciprocal_styles() {
        let previous = snapshot(&["> Alpha", "  Bravo", "> query"], &[(0, 0..=6, inverse())]);
        let current = snapshot(
            &["  Alpha", "> Bravo", "> query"],
            &[(1, 0..=6, colored_bold())],
        );

        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn rejects_marker_motion_with_any_other_text_change() {
        let previous = snapshot(&["> Alpha", "  Bravo", "  Charlie", "1/3"], &[]);
        let current = snapshot(&["  Alpha", "> Bravo", "  Charlie", "2/3"], &[]);

        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn rejects_interior_punctuation_even_with_a_peer_and_style_transfer() {
        let previous = snapshot(
            &["A > Alpha", "A   Bravo", "A   Charlie"],
            &[(0, 0..=8, inverse())],
        );
        let current = snapshot(
            &["A   Alpha", "A > Bravo", "A   Charlie"],
            &[(1, 0..=8, inverse())],
        );

        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn rejects_unaligned_labels_and_unseparated_markers() {
        let previous = snapshot(&["> Alpha", "   Bravo", "  Charlie"], &[]);
        let current = snapshot(&["  Alpha", ">  Bravo", "  Charlie"], &[]);
        assert_eq!(detects(&previous, &current), None);

        let previous = snapshot(&[">Alpha", " Bravo", " Charlie"], &[]);
        let current = snapshot(&[" Alpha", ">Bravo", " Charlie"], &[]);
        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn rejects_word_markers_and_markers_wider_than_three_cells() {
        let previous = snapshot(&["x Alpha", "  Bravo", "  Charlie"], &[]);
        let current = snapshot(&["  Alpha", "x Bravo", "  Charlie"], &[]);
        assert_eq!(detects(&previous, &current), None);

        let previous = snapshot(&["---- Alpha", "     Bravo", "     Charlie"], &[]);
        let current = snapshot(&["     Alpha", "---- Bravo", "     Charlie"], &[]);
        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn rejects_multiple_marker_transfers() {
        let previous = snapshot(
            &["> Alpha", "  Bravo", "> Charlie", "  Delta", "  Echo"],
            &[],
        );
        let current = snapshot(
            &["  Alpha", "> Bravo", "  Charlie", "> Delta", "  Echo"],
            &[],
        );

        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn rejects_marker_styles_that_blink_or_conceal() {
        for unsafe_style in [
            Style {
                blink: true,
                ..Style::default()
            },
            Style {
                invisible: true,
                ..Style::default()
            },
        ] {
            let previous = snapshot(
                &["> Alpha", "  Bravo", "  Charlie"],
                &[(0, 0..=6, unsafe_style.clone())],
            );
            let current = snapshot(
                &["  Alpha", "> Bravo", "  Charlie"],
                &[(1, 0..=6, unsafe_style.clone())],
            );

            assert_eq!(detects(&previous, &current), None);
        }
    }

    #[test]
    fn rejects_style_or_hyperlink_changes_outside_marker_rows() {
        let previous = snapshot(&["> Alpha", "  Bravo", "  Charlie", "Status"], &[]);
        let mut current = snapshot(&["  Alpha", "> Bravo", "  Charlie", "Status"], &[]);
        set_style(&mut current, 3, 0..=5, colored_bold());
        assert_eq!(detects(&previous, &current), None);

        let previous = snapshot(&["> Alpha", "  Bravo", "  Charlie"], &[]);
        let mut current = snapshot(&["  Alpha", "> Bravo", "  Charlie"], &[]);
        let rows = Arc::make_mut(&mut current.rows);
        let cells = Arc::make_mut(&mut rows[2].cells);
        cells[2].hyperlink = Some("https://example.invalid".into());
        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn detects_inverse_transfer_without_key_identity() {
        let lines = ["Alpha", "Bravo", "Delta", "Gamma"];
        let previous = snapshot(&lines, &[(0, 0..=4, inverse())]);
        let current = snapshot(&lines, &[(1, 0..=4, inverse())]);

        assert_eq!(detects(&previous, &current).as_deref(), Some("Bravo"));
    }

    #[test]
    fn preserves_single_style_focus_transfer_within_one_row() {
        let lines = ["Alpha Bravo", "Delta Gamma", "Third Option"];
        let previous = snapshot(&lines, &[(0, 0..=4, inverse())]);
        let current = snapshot(&lines, &[(0, 6..=10, inverse())]);

        assert_eq!(detects(&previous, &current).as_deref(), Some("Bravo"));
    }

    fn stationary_gutter_style_bundle() -> (TerminalSnapshot, TerminalSnapshot) {
        let inactive_gutter = Style {
            dim: true,
            ..Style::default()
        };
        let active_gutter = Style {
            bold: true,
            ..Style::default()
        };
        let active_payload = Style {
            bold: true,
            inverse: true,
            ..Style::default()
        };
        let lines = ["▌ Alpha", "▌ Bravo", "▌ Delta", "▌ Gamma"];
        let previous = snapshot(
            &lines,
            &[
                (0, 0..=0, active_gutter.clone()),
                (0, 2..=6, active_payload.clone()),
                (1, 0..=0, inactive_gutter.clone()),
                (2, 0..=0, inactive_gutter.clone()),
                (3, 0..=0, inactive_gutter.clone()),
            ],
        );
        let current = snapshot(
            &lines,
            &[
                (0, 0..=0, inactive_gutter.clone()),
                (1, 0..=0, active_gutter),
                (1, 2..=6, active_payload),
                (2, 0..=0, inactive_gutter.clone()),
                (3, 0..=0, inactive_gutter),
            ],
        );
        (previous, current)
    }

    #[test]
    fn detects_compound_row_style_transfer_with_stationary_gutter() {
        let (previous, current) = stationary_gutter_style_bundle();

        assert_eq!(detects(&previous, &current).as_deref(), Some("Bravo"));
    }

    #[test]
    fn rejects_compound_transfer_on_a_visible_cursor_row() {
        let (mut previous, mut current) = stationary_gutter_style_bundle();
        set_hardware_cursor(&mut previous, 0, 0);
        set_hardware_cursor(&mut current, 0, 0);

        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn detects_stationary_gutter_and_payload_sharing_one_style_channel() {
        let selected = inverse();
        let lines = ["▌ Alpha", "▌ Bravo", "▌ Delta", "▌ Gamma"];
        let previous = snapshot(
            &lines,
            &[(0, 0..=0, selected.clone()), (0, 2..=6, selected.clone())],
        );
        let current = snapshot(
            &lines,
            &[(1, 0..=0, selected.clone()), (1, 2..=6, selected)],
        );

        assert_eq!(detects(&previous, &current).as_deref(), Some("Bravo"));
    }

    #[test]
    fn detects_payload_with_an_independently_styled_match_fragment() {
        let inactive_gutter = Style {
            dim: true,
            ..Style::default()
        };
        let active_gutter = Style {
            bold: true,
            ..Style::default()
        };
        let active_payload = Style {
            bold: true,
            inverse: true,
            ..Style::default()
        };
        let inactive_match = Style {
            foreground: Color::Indexed(2),
            ..Style::default()
        };
        let active_match = Style {
            foreground: Color::Indexed(2),
            bold: true,
            inverse: true,
            ..Style::default()
        };
        let lines = ["▌ Alpha", "▌ Bravo", "▌ Delta", "▌ Gamma"];
        let previous = snapshot(
            &lines,
            &[
                (0, 0..=0, active_gutter.clone()),
                (0, 2..=6, active_payload.clone()),
                (0, 3..=3, active_match.clone()),
                (1, 0..=0, inactive_gutter.clone()),
                (1, 3..=3, inactive_match.clone()),
                (2, 0..=0, inactive_gutter.clone()),
                (2, 3..=3, inactive_match.clone()),
                (3, 0..=0, inactive_gutter.clone()),
                (3, 3..=3, inactive_match.clone()),
            ],
        );
        let current = snapshot(
            &lines,
            &[
                (0, 0..=0, inactive_gutter.clone()),
                (0, 3..=3, inactive_match.clone()),
                (1, 0..=0, active_gutter),
                (1, 2..=6, active_payload),
                (1, 3..=3, active_match),
                (2, 0..=0, inactive_gutter.clone()),
                (2, 3..=3, inactive_match.clone()),
                (3, 0..=0, inactive_gutter),
                (3, 3..=3, inactive_match),
            ],
        );

        assert_eq!(detects(&previous, &current).as_deref(), Some("Bravo"));
    }

    #[test]
    fn rejects_direction_selected_only_by_punctuation() {
        let selected_decoration = inverse();
        let first_payload = Style {
            foreground: Color::Indexed(1),
            ..Style::default()
        };
        let second_payload = Style {
            foreground: Color::Indexed(4),
            ..Style::default()
        };
        let lines = ["!Alpha", "!Bravo", "!Delta", "!Gamma"];
        let previous = snapshot(
            &lines,
            &[
                (0, 0..=0, selected_decoration.clone()),
                (0, 1..=5, first_payload.clone()),
                (1, 1..=5, second_payload.clone()),
            ],
        );
        let current = snapshot(
            &lines,
            &[
                (0, 1..=5, second_payload),
                (1, 0..=0, selected_decoration),
                (1, 1..=5, first_payload),
            ],
        );

        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn rejects_unpaired_style_channel_in_candidate_rows() {
        let (previous, mut current) = stationary_gutter_style_bundle();
        set_style(
            &mut current,
            0,
            1..=1,
            Style {
                foreground: Color::Indexed(1),
                ..Style::default()
            },
        );

        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn rejects_compound_style_channels_with_conflicting_destinations() {
        let inactive_prefix = Style {
            dim: true,
            ..Style::default()
        };
        let active_prefix = Style {
            bold: true,
            ..Style::default()
        };
        let selected_payload = inverse();
        let lines = ["xyAlpha", "xyBravo", "xyDelta", "xyGamma"];
        let previous = snapshot(
            &lines,
            &[
                (0, 0..=1, inactive_prefix.clone()),
                (0, 2..=6, selected_payload.clone()),
                (1, 0..=1, active_prefix.clone()),
                (2, 0..=1, inactive_prefix.clone()),
                (3, 0..=1, inactive_prefix.clone()),
            ],
        );
        let current = snapshot(
            &lines,
            &[
                (0, 0..=1, active_prefix),
                (1, 0..=1, inactive_prefix.clone()),
                (1, 2..=6, selected_payload),
                (2, 0..=1, inactive_prefix.clone()),
                (3, 0..=1, inactive_prefix),
            ],
        );

        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn style_transfer_ignores_hidden_cursor_bookkeeping_coordinates() {
        let lines = ["Alpha", "Bravo", "Delta", "Gamma"];
        let mut previous = snapshot(&lines, &[(0, 0..=4, inverse())]);
        let mut current = snapshot(&lines, &[(1, 0..=4, inverse())]);
        previous.cursor.row = 3;
        previous.cursor.col = 5;
        current.cursor.row = 1;
        current.cursor.col = 5;

        assert_eq!(detects(&previous, &current).as_deref(), Some("Bravo"));
    }

    #[test]
    fn rejects_hidden_cursor_shape_changes() {
        let lines = ["Alpha", "Bravo", "Delta", "Gamma"];
        let previous = snapshot(&lines, &[(0, 0..=4, inverse())]);
        let mut current = snapshot(&lines, &[(1, 0..=4, inverse())]);
        current.cursor.shape = crate::terminal::CursorShape::Bar;

        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn rejects_two_option_transfer_when_neither_style_identifies_selection() {
        let lines = ["Alpha", "Bravo"];
        let first = Style {
            foreground: Color::Indexed(1),
            ..Style::default()
        };
        let second = Style {
            foreground: Color::Indexed(4),
            ..Style::default()
        };
        let previous = snapshot(
            &lines,
            &[(0, 0..=4, first.clone()), (1, 0..=4, second.clone())],
        );
        let current = snapshot(&lines, &[(0, 0..=4, second), (1, 0..=4, first)]);

        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn detects_arbitrary_foreground_background_and_bold_transfers() {
        let lines = ["Alpha", "Bravo", "Delta", "Gamma"];
        for selected in [
            Style {
                foreground: Color::Rgb(232, 111, 176),
                ..Style::default()
            },
            Style {
                background: Color::Indexed(24),
                ..Style::default()
            },
            Style {
                bold: true,
                ..Style::default()
            },
            colored_bold(),
        ] {
            let previous = snapshot(&lines, &[(0, 0..=4, selected.clone())]);
            let current = snapshot(&lines, &[(1, 0..=4, selected)]);

            assert_eq!(detects(&previous, &current).as_deref(), Some("Bravo"));
        }
    }

    #[test]
    fn duplicate_labels_are_disambiguated_by_coordinates() {
        let lines = ["Same", "Same", "Same", "Same"];
        let previous = snapshot(&lines, &[(0, 0..=3, inverse())]);
        let current = snapshot(&lines, &[(1, 0..=3, inverse())]);

        assert_eq!(detects(&previous, &current).as_deref(), Some("Same"));
    }

    #[test]
    fn style_rarity_identifies_destination_without_key_identity() {
        let lines = ["Alpha", "Bravo", "Delta", "Gamma"];
        let previous = snapshot(&lines, &[(3, 0..=4, inverse())]);
        let current = snapshot(&lines, &[(0, 0..=4, inverse())]);

        assert_eq!(detects(&previous, &current).as_deref(), Some("Alpha"));
    }

    #[test]
    fn rejects_whole_screen_recolor() {
        let lines = ["Alpha", "Bravo", "Delta", "Gamma"];
        let recolor = Style {
            foreground: Color::Rgb(64, 200, 255),
            ..Style::default()
        };
        let previous = snapshot(&lines, &[]);
        let current = snapshot(
            &lines,
            &[
                (0, 0..=4, recolor.clone()),
                (1, 0..=4, recolor.clone()),
                (2, 0..=4, recolor.clone()),
                (3, 0..=4, recolor),
            ],
        );

        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn rejects_global_two_style_theme_swap() {
        let lines = ["Alpha", "Bravo", "Delta", "Gamma"];
        let first = Style {
            foreground: Color::Indexed(1),
            ..Style::default()
        };
        let second = Style {
            foreground: Color::Indexed(4),
            ..Style::default()
        };
        let previous = snapshot(
            &lines,
            &[
                (0, 0..=4, first.clone()),
                (1, 0..=4, second.clone()),
                (2, 0..=4, first.clone()),
                (3, 0..=4, second.clone()),
            ],
        );
        let current = snapshot(
            &lines,
            &[
                (0, 0..=4, second.clone()),
                (1, 0..=4, first.clone()),
                (2, 0..=4, second),
                (3, 0..=4, first),
            ],
        );

        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn rejects_multiple_simultaneous_transfers() {
        let lines = ["Alpha", "Bravo", "Delta", "Gamma"];
        let previous = snapshot(&lines, &[(0, 0..=4, inverse()), (2, 0..=4, inverse())]);
        let current = snapshot(&lines, &[(1, 0..=4, inverse()), (3, 0..=4, inverse())]);

        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn rejects_moving_shimmer_inside_text() {
        let lines = ["Loading package", "Another item", "Third option"];
        let shimmer = Style {
            foreground: Color::Rgb(250, 250, 250),
            bold: true,
            ..Style::default()
        };
        let previous = snapshot(&lines, &[(0, 0..=1, shimmer.clone())]);
        let current = snapshot(&lines, &[(0, 1..=2, shimmer)]);

        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn rejects_spinner_style_motion_without_meaningful_text() {
        let lines = ["- \\ | /", "Ready", "Waiting"];
        let shimmer = colored_bold();
        let previous = snapshot(&lines, &[(0, 0..=0, shimmer.clone())]);
        let current = snapshot(&lines, &[(0, 2..=2, shimmer)]);

        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn rejects_transfer_from_decoration_to_a_text_label() {
        let lines = ["--", "Bravo", "Delta", "Gamma"];
        let selected = colored_bold();
        let previous = snapshot(&lines, &[(0, 0..=1, selected.clone())]);
        let current = snapshot(&lines, &[(1, 0..=4, selected)]);

        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn rejects_blinking_style_even_when_it_also_changes_color() {
        let lines = ["Alpha", "Bravo", "Delta", "Gamma"];
        let blinking = Style {
            foreground: Color::Indexed(5),
            blink: true,
            ..Style::default()
        };
        let previous = snapshot(&lines, &[(0, 0..=4, blinking.clone())]);
        let current = snapshot(&lines, &[(1, 0..=4, blinking)]);

        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn rejects_concealed_style_transfer() {
        let lines = ["Alpha", "Bravo", "Delta", "Gamma"];
        let concealed = Style {
            foreground: Color::Indexed(5),
            invisible: true,
            ..Style::default()
        };
        let previous = snapshot(&lines, &[(0, 0..=4, concealed.clone())]);
        let current = snapshot(&lines, &[(1, 0..=4, concealed)]);

        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn rejects_text_changes_even_when_styles_transfer() {
        let previous = snapshot(
            &["Alpha", "Bravo", "Delta", "Gamma"],
            &[(0, 0..=4, inverse())],
        );
        let current = snapshot(
            &["Alpha", "Brava", "Delta", "Gamma"],
            &[(1, 0..=4, inverse())],
        );

        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn rejects_cursor_motion_so_existing_cursor_tracking_can_win() {
        let lines = ["Alpha", "Bravo", "Delta", "Gamma"];
        let mut previous = snapshot(&lines, &[(0, 0..=4, inverse())]);
        previous.cursor = Cursor {
            row: 0,
            col: 0,
            visible: true,
            ..Cursor::default()
        };
        let mut current = snapshot(&lines, &[(1, 0..=4, inverse())]);
        current.cursor = Cursor {
            row: 1,
            col: 0,
            visible: true,
            ..Cursor::default()
        };

        assert_eq!(detects(&previous, &current), None);
    }

    #[test]
    fn rejects_pixel_geometry_change() {
        let lines = ["Alpha", "Bravo", "Delta", "Gamma"];
        let previous = snapshot(&lines, &[(0, 0..=4, inverse())]);
        let mut current = snapshot(&lines, &[(1, 0..=4, inverse())]);
        current.geometry = TerminalGeometry::new(lines.len() as u16, WIDTH as u16, 8, 16);

        assert_eq!(detects(&previous, &current), None);
    }
}
