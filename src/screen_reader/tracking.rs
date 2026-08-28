use super::{Result, ScreenReader};
use crate::{
    ext::ScreenExt,
    presentation::{ViewId, ViewRevision},
    terminal::Row,
    view::View,
};
use std::collections::{HashMap, HashSet};

pub(super) enum CursorTrackingMode {
    On,
    OffOnce,
}

pub(super) struct PendingDelete {
    view_id: ViewId,
    /// The newest parser revision which already existed when the input was
    /// sent. A physical frame at or before this boundary cannot be the
    /// application's response to the deletion.
    revision_boundary: Option<ViewRevision>,
    last_evaluated_revision: Option<ViewRevision>,
    evaluations: u8,
    input_sequence: u64,
    kind: PendingDeleteKind,
}

pub(super) enum PendingDeleteKind {
    Backspace {
        cursor: (u16, u16),
        candidates: Vec<BackspaceCandidate>,
    },
    Delete {
        application_cursor: (u16, u16),
        target_col: u16,
        text: String,
        row_before: Row,
    },
}

pub(super) struct BackspaceCandidate {
    target: (u16, u16),
    text: String,
    row_before: Row,
    confirmation: BackspaceConfirmation,
}

#[derive(Clone, Copy)]
enum BackspaceConfirmation {
    /// Conventional editing places the cursor where the removed grapheme
    /// began. The coordinate may be on the preceding physical row.
    CursorAtTarget,
    /// Horizontally scrolling one-line editors can keep the cursor pinned to
    /// the right margin and replace the cell under it after a deletion.
    StationaryMargin,
}

enum PendingDeleteEvaluation {
    Confirmed(String),
    Partial,
    NotStarted,
    Rejected,
}

#[derive(Clone, Copy)]
struct BackspaceCursorAnchor {
    intent_index: usize,
    candidate_index: usize,
}

#[derive(Default)]
struct DeleteEvidenceCache {
    row_changed: HashMap<(usize, u16), bool>,
    prefix_unchanged: HashMap<(usize, u16, u16), bool>,
}

impl DeleteEvidenceCache {
    fn row_changed(&mut self, before: &Row, view: &View, row: u16) -> bool {
        let key = (row_identity(before), row);
        *self
            .row_changed
            .entry(key)
            .or_insert_with(|| row_text_changed(before, view, row))
    }

    fn prefix_unchanged(&mut self, before: &Row, view: &View, row: u16, end_col: u16) -> bool {
        let key = (row_identity(before), row, end_col);
        *self
            .prefix_unchanged
            .entry(key)
            .or_insert_with(|| row_prefix_unchanged(before, view, row, end_col))
    }
}

impl ScreenReader {
    pub(crate) fn speak_application_screen(&mut self, view: &View) -> Result<bool> {
        // A printable key can be presented in the first settled frame of a
        // newly entered screen. Preserve the same exact-echo suppression used
        // by cursor-line tracking before widening the announcement.
        let cursor_line = view.line(view.screen().cursor_position().0);
        if self.should_suppress_key_echo(&cursor_line) {
            return Ok(false);
        }

        let contents = view.screen().contents();
        let contents = contents.trim();
        if contents.is_empty() {
            return Ok(false);
        }
        self.speak(contents, false)?;
        Ok(true)
    }

    pub(crate) fn speak_application_cursor_line(&mut self, view: &View) -> Result<bool> {
        let line = view.line(view.screen().cursor_position().0);
        if line.trim().is_empty() || self.should_suppress_key_echo(&line) {
            return Ok(false);
        }
        self.speak(&line, false)?;
        Ok(true)
    }

    pub fn track_cursor(&mut self, view: &mut View) -> Result<()> {
        let (prev_cursor, cursor) = (
            view.prev_screen().cursor_position(),
            view.screen().cursor_position(),
        );

        let mut cursor_report = None;
        if cursor.0 != prev_cursor.0 {
            let line = view.line(cursor.0);
            cursor_report = Some(if line.trim().is_empty() {
                String::new()
            } else {
                line
            });
        } else if cursor.1 != prev_cursor.1 {
            let distance_moved = (cursor.1 as i32 - prev_cursor.1 as i32).abs();
            let prev_word_start = view.screen().find_word_start(prev_cursor.0, prev_cursor.1);
            let word_start = view.screen().find_word_start(cursor.0, cursor.1);
            if word_start != prev_word_start && distance_moved > 1 {
                cursor_report = Some(view.word(cursor.0, cursor.1));
            } else {
                let character = view.character(cursor.0, cursor.1);
                cursor_report = Some(if character.trim().is_empty() {
                    String::new()
                } else {
                    character
                });
            }
        }

        match self.cursor_tracking_mode {
            CursorTrackingMode::On => {
                self.report_application_cursor_indentation_changes(view)?;
                if let Some(text) = cursor_report {
                    self.speak(&text, false)?;
                }
            }
            CursorTrackingMode::OffOnce => self.cursor_tracking_mode = CursorTrackingMode::On,
        }

        Ok(())
    }

    pub fn clear_pending_delete(&mut self) {
        self.pending_deletes.clear();
    }

    pub fn defer_backspace(&mut self, view: &View) {
        let application_cursor = view.screen().cursor_position();
        let view_id = view.view_id();
        let revision_boundary = view.input_intent_revision_boundary();
        let cursor = self
            .pending_deletes
            .back()
            .filter(|pending| {
                pending.view_id == view_id
                    && pending.input_sequence.wrapping_add(1) == self.input_sequence
                    && matches!(pending.kind, PendingDeleteKind::Backspace { .. })
            })
            .and_then(|pending| match &pending.kind {
                PendingDeleteKind::Backspace { candidates, .. } => candidates
                    .iter()
                    .find(|candidate| {
                        matches!(
                            candidate.confirmation,
                            BackspaceConfirmation::CursorAtTarget
                        )
                    })
                    .map(|candidate| candidate.target),
                _ => None,
            })
            .unwrap_or(application_cursor);
        if view.position_at_or_before_active_semantic_input(cursor) {
            return;
        }
        let candidates = backspace_candidates(view, cursor);
        if candidates.is_empty() {
            return;
        }
        self.push_pending_delete(PendingDelete {
            view_id,
            revision_boundary,
            last_evaluated_revision: None,
            evaluations: 0,
            input_sequence: self.input_sequence,
            kind: PendingDeleteKind::Backspace { cursor, candidates },
        });
    }

    pub fn defer_delete(&mut self, view: &View) {
        let (row, col) = view.screen().cursor_position();
        let view_id = view.view_id();
        let revision_boundary = view.input_intent_revision_boundary();
        let virtual_offset = self
            .pending_deletes
            .back()
            .filter(|pending| {
                pending.view_id == view_id
                    && pending.input_sequence.wrapping_add(1) == self.input_sequence
                    && matches!(pending.kind, PendingDeleteKind::Delete { .. })
            })
            .and_then(|pending| match pending.kind {
                PendingDeleteKind::Delete {
                    target_col: pending_col,
                    application_cursor: (pending_row, _),
                    ..
                } if pending_row == row && pending_col >= col => Some(pending_col - col + 1),
                _ => None,
            })
            .unwrap_or(0);
        let virtual_col = col.saturating_add(virtual_offset);
        let text = view
            .screen()
            .cell(row, virtual_col)
            .map(|cell| cell.contents().to_string())
            .unwrap_or_default();
        if text.is_empty() {
            return;
        }
        let Some(row_before) = view.screen().rows.get(usize::from(row)).cloned() else {
            return;
        };
        self.push_pending_delete(PendingDelete {
            view_id,
            revision_boundary,
            last_evaluated_revision: None,
            evaluations: 0,
            input_sequence: self.input_sequence,
            kind: PendingDeleteKind::Delete {
                application_cursor: (row, col),
                target_col: virtual_col,
                text,
                row_before,
            },
        });
    }

    /// Whether a causally later, physically accessible frame contains a
    /// complete deletion result. Cursor movement alone is insufficient:
    /// terminals commonly receive a backspace echo as cursor-left, erase,
    /// cursor-left across separate writes. Requiring the edited row and its
    /// logical cursor position to agree keeps that partial frame behind the
    /// ordinary stabilization window.
    pub(crate) fn has_confirmed_pending_delete(&self, view: &View) -> bool {
        if self.pending_deletes.is_empty()
            || view.prev_screen().screen != view.screen().screen
            || view.accessibility_screen_transition_pending()
        {
            return false;
        }

        let view_id = view.view_id();
        let accessibility_revision = view.accessibility_revision();
        let cursor = view.screen().cursor_position();
        let anchors = backspace_cursor_anchors(&self.pending_deletes, cursor);
        let mut evidence = DeleteEvidenceCache::default();
        self.pending_deletes
            .iter()
            .enumerate()
            .any(|(index, intent)| {
                intent.view_id == view_id
                    && revision_passed(intent.revision_boundary, accessibility_revision)
                    && (accessibility_revision.is_none()
                        || intent.last_evaluated_revision != accessibility_revision)
                    && matches!(
                        evaluate_pending_delete(
                            index,
                            &self.pending_deletes,
                            &anchors,
                            &mut evidence,
                            view,
                            cursor,
                        ),
                        PendingDeleteEvaluation::Confirmed(_)
                    )
            })
    }

    pub(crate) fn resolve_confirmed_pending_delete(&mut self, view: &View) -> Result<bool> {
        if !self.has_confirmed_pending_delete(view) {
            return Ok(false);
        }
        self.resolve_pending_delete(view)
    }

    pub fn resolve_pending_delete(&mut self, view: &View) -> Result<bool> {
        if self.pending_deletes.is_empty() {
            return Ok(false);
        }

        let view_id = view.view_id();
        let accessibility_revision = view.accessibility_revision();
        let cursor = view.screen().cursor_position();
        let pending = std::mem::take(&mut self.pending_deletes);
        let anchors = backspace_cursor_anchors(&pending, cursor);
        let mut evidence = DeleteEvidenceCache::default();
        let mut evaluations = pending
            .iter()
            .enumerate()
            .map(|(index, intent)| {
                if intent.view_id != view_id
                    || !revision_passed(intent.revision_boundary, accessibility_revision)
                    || (accessibility_revision.is_some()
                        && intent.last_evaluated_revision == accessibility_revision)
                {
                    None
                } else {
                    Some(evaluate_pending_delete(
                        index,
                        &pending,
                        &anchors,
                        &mut evidence,
                        view,
                        cursor,
                    ))
                }
            })
            .collect::<Vec<_>>();
        preserve_unobserved_backspace_suffixes(&pending, &mut evaluations);
        let mut retained = std::collections::VecDeque::with_capacity(pending.len());
        let mut spoken = Vec::new();

        for (mut intent, evaluation) in pending.into_iter().zip(evaluations) {
            match evaluation {
                None => retained.push_back(intent),
                Some(PendingDeleteEvaluation::Confirmed(text)) => spoken.push(text),
                Some(PendingDeleteEvaluation::Partial) => {
                    // A deletion can be presented in pieces, such as a
                    // cursor-left frame followed by erase and another
                    // cursor-left. Retain the intent only while the visible
                    // state remains a prefix of that transformation.
                    intent.last_evaluated_revision = accessibility_revision;
                    intent.evaluations = intent.evaluations.saturating_add(1);
                    if intent.evaluations < super::MAX_PENDING_DELETE_PRESENTATIONS {
                        retained.push_back(intent);
                    }
                }
                Some(PendingDeleteEvaluation::NotStarted) => {
                    rebase_pending_backspace(&mut intent, view);
                    intent.revision_boundary = accessibility_revision;
                    intent.last_evaluated_revision = None;
                    intent.evaluations = 0;
                    retained.push_back(intent);
                }
                Some(PendingDeleteEvaluation::Rejected) => {
                    // An incompatible frame disproves this intent. Discard it
                    // so unrelated later output cannot resurrect stale
                    // deletion evidence.
                }
            }
        }
        self.pending_deletes = retained;

        for text in &spoken {
            self.speak(text, false)?;
        }
        Ok(!spoken.is_empty())
    }

    fn push_pending_delete(&mut self, pending: PendingDelete) {
        if self.pending_deletes.len() >= super::MAX_PENDING_DELETE_INTENTS {
            self.pending_deletes.pop_front();
        }
        self.pending_deletes.push_back(pending);
    }

    pub fn track_highlighting(&mut self, view: &mut View) -> Result<()> {
        let (highlights, prev_highlights) = (
            view.screen().get_highlights(),
            view.prev_screen().get_highlights(),
        );
        let previous: HashSet<String> = HashSet::from_iter(prev_highlights.iter().cloned());

        for highlight in highlights {
            if !previous.contains(&highlight) {
                self.speak(&highlight, false)?;
            }
        }
        Ok(())
    }

    pub fn report_application_cursor_indentation_changes(&mut self, view: &mut View) -> Result<()> {
        if !self.indentation_reporting_enabled() {
            return Ok(());
        }
        let (indent_level, changed) = view.application_cursor_indentation_level();
        if changed {
            self.speak(&format!("indent {indent_level}"), false)?;
        }
        Ok(())
    }

    pub fn report_review_cursor_indentation_changes(&mut self, view: &mut View) -> Result<()> {
        if !self.indentation_reporting_enabled() {
            return Ok(());
        }
        let (indent_level, changed) = view.review_cursor_indentation_level();
        if changed {
            self.speak(&format!("indent {indent_level}"), false)?;
        }
        Ok(())
    }
}

/// A settled frame can contain only a prefix of a queued Backspace chain. A
/// later intent is not contradicted merely because the application has not
/// reached it yet; once the confirmed prefix is removed from the queue, the
/// untouched suffix is evaluated against the next presentation.
fn preserve_unobserved_backspace_suffixes(
    intents: &std::collections::VecDeque<PendingDelete>,
    evaluations: &mut [Option<PendingDeleteEvaluation>],
) {
    let mut start = 0;
    while start < intents.len() {
        let PendingDeleteKind::Backspace { .. } = intents[start].kind else {
            start += 1;
            continue;
        };
        let mut end = start + 1;
        while end < intents.len()
            && intents[end].view_id == intents[end - 1].view_id
            && intents[end].input_sequence == intents[end - 1].input_sequence.wrapping_add(1)
            && matches!(intents[end].kind, PendingDeleteKind::Backspace { .. })
        {
            end += 1;
        }

        if let Some(last_confirmed) = (start..end).rev().find(|index| {
            matches!(
                evaluations[*index],
                Some(PendingDeleteEvaluation::Confirmed(_))
            )
        }) {
            for evaluation in &mut evaluations[last_confirmed + 1..end] {
                if evaluation.is_some() {
                    *evaluation = Some(PendingDeleteEvaluation::NotStarted);
                }
            }
        }
        start = end;
    }
}

fn evaluate_pending_delete(
    index: usize,
    intents: &std::collections::VecDeque<PendingDelete>,
    anchors: &[Option<BackspaceCursorAnchor>],
    evidence: &mut DeleteEvidenceCache,
    view: &View,
    cursor: (u16, u16),
) -> PendingDeleteEvaluation {
    let intent = &intents[index];
    match &intent.kind {
        PendingDeleteKind::Backspace {
            cursor: old_cursor,
            candidates,
            ..
        } => {
            let confirmed = candidates.iter().find_map(|candidate| {
                let confirmed = match candidate.confirmation {
                    BackspaceConfirmation::CursorAtTarget => {
                        backspace_cursor_anchor(intents, anchors[index]).is_some_and(|anchor| {
                            cursor < *old_cursor
                                && evidence.prefix_unchanged(
                                    &anchor.row_before,
                                    view,
                                    cursor.0,
                                    cursor.1,
                                )
                        }) && evidence.row_changed(&candidate.row_before, view, candidate.target.0)
                    }
                    BackspaceConfirmation::StationaryMargin => {
                        cursor == *old_cursor
                            && evidence.prefix_unchanged(
                                &candidate.row_before,
                                view,
                                candidate.target.0,
                                candidate.target.1,
                            )
                            && target_cell_changed(&candidate.row_before, view, candidate.target)
                    }
                };
                confirmed.then(|| candidate.text.clone())
            });
            if let Some(text) = confirmed {
                return PendingDeleteEvaluation::Confirmed(text);
            }

            let partial = candidates.iter().any(|candidate| {
                if !matches!(
                    candidate.confirmation,
                    BackspaceConfirmation::CursorAtTarget
                ) || !evidence.prefix_unchanged(
                    &candidate.row_before,
                    view,
                    candidate.target.0,
                    candidate.target.1,
                ) {
                    return false;
                }
                let row_changed =
                    evidence.row_changed(&candidate.row_before, view, candidate.target.0);
                (cursor == candidate.target && !row_changed)
                    || (cursor == *old_cursor && row_changed)
            });
            if partial {
                PendingDeleteEvaluation::Partial
            } else {
                PendingDeleteEvaluation::Rejected
            }
        }
        PendingDeleteKind::Delete {
            application_cursor,
            text,
            row_before,
            ..
        } => {
            if cursor == *application_cursor
                && evidence.prefix_unchanged(
                    row_before,
                    view,
                    application_cursor.0,
                    application_cursor.1,
                )
                && evidence.row_changed(row_before, view, application_cursor.0)
            {
                PendingDeleteEvaluation::Confirmed(text.clone())
            } else {
                PendingDeleteEvaluation::Rejected
            }
        }
    }
}

/// Finds each contiguous Backspace chain's presented cursor once. Every
/// intent at or before that cursor shares the same anchor; intents after it
/// have not yet been observed. This avoids rescanning the suffix for every
/// queued key press.
fn backspace_cursor_anchors(
    intents: &std::collections::VecDeque<PendingDelete>,
    cursor: (u16, u16),
) -> Vec<Option<BackspaceCursorAnchor>> {
    let mut anchors = vec![None; intents.len()];
    let mut start = 0;
    while start < intents.len() {
        let PendingDeleteKind::Backspace { .. } = intents[start].kind else {
            start += 1;
            continue;
        };
        let mut end = start + 1;
        while end < intents.len()
            && intents[end].view_id == intents[end - 1].view_id
            && intents[end].input_sequence == intents[end - 1].input_sequence.wrapping_add(1)
            && matches!(intents[end].kind, PendingDeleteKind::Backspace { .. })
        {
            end += 1;
        }

        let anchor = (start..end).find_map(|intent_index| {
            let PendingDeleteKind::Backspace { candidates, .. } = &intents[intent_index].kind
            else {
                return None;
            };
            candidates
                .iter()
                .enumerate()
                .find(|(_, candidate)| {
                    matches!(
                        candidate.confirmation,
                        BackspaceConfirmation::CursorAtTarget
                    ) && candidate.target == cursor
                })
                .map(|(candidate_index, _)| BackspaceCursorAnchor {
                    intent_index,
                    candidate_index,
                })
        });
        if let Some(anchor) = anchor {
            anchors[start..=anchor.intent_index].fill(Some(anchor));
        }
        start = end;
    }
    anchors
}

fn backspace_cursor_anchor<'a>(
    intents: &'a std::collections::VecDeque<PendingDelete>,
    anchor: Option<BackspaceCursorAnchor>,
) -> Option<&'a BackspaceCandidate> {
    let anchor = anchor?;
    let intent = intents.get(anchor.intent_index)?;
    let PendingDeleteKind::Backspace { candidates, .. } = &intent.kind else {
        return None;
    };
    candidates.get(anchor.candidate_index)
}

fn rebase_pending_backspace(intent: &mut PendingDelete, view: &View) {
    let PendingDeleteKind::Backspace { candidates, .. } = &mut intent.kind else {
        return;
    };
    for candidate in candidates {
        if let Some(row) = view.screen().rows.get(usize::from(candidate.target.0)) {
            candidate.row_before = row.clone();
        }
    }
}

fn backspace_candidates(view: &View, cursor: (u16, u16)) -> Vec<BackspaceCandidate> {
    let screen = view.screen();
    let (row, col) = cursor;
    let mut candidates = Vec::with_capacity(2);

    // Prefer the cross-row interpretation when its narrow structural shape
    // is present. An explicitly printed indentation space is also a valid
    // local candidate, but a cross-row redraw must announce the payload at
    // the preceding margin rather than that decoration.
    if let Some(target) = explicit_continuation_target(screen, cursor) {
        push_backspace_candidate(
            &mut candidates,
            view,
            target,
            BackspaceConfirmation::CursorAtTarget,
        );
    }

    let conventional_target = if col > 0 {
        cell_owner_to_left(screen, row, col)
    } else if row > 0 && screen.row_wrapped(row - 1) {
        last_content_cell(screen, row - 1)
    } else {
        None
    };
    if let Some(target) = conventional_target {
        push_backspace_candidate(
            &mut candidates,
            view,
            target,
            BackspaceConfirmation::CursorAtTarget,
        );
    }

    let (_, columns) = screen.size();
    if col.saturating_add(1) == columns
        && screen
            .cell(row, col)
            .is_some_and(|cell| !cell.is_wide_continuation() && !cell.contents().is_empty())
    {
        push_backspace_candidate(
            &mut candidates,
            view,
            cursor,
            BackspaceConfirmation::StationaryMargin,
        );
    }

    candidates
}

fn push_backspace_candidate(
    candidates: &mut Vec<BackspaceCandidate>,
    view: &View,
    target: (u16, u16),
    confirmation: BackspaceConfirmation,
) {
    if candidates
        .iter()
        .any(|candidate| candidate.target == target)
    {
        return;
    }
    let Some(cell) = view.screen().cell(target.0, target.1) else {
        return;
    };
    let text = cell.contents().to_string();
    if text.is_empty() {
        return;
    }
    let Some(row_before) = view.screen().rows.get(usize::from(target.0)).cloned() else {
        return;
    };
    candidates.push(BackspaceCandidate {
        target,
        text,
        row_before,
        confirmation,
    });
}

fn cell_owner_to_left(
    screen: &crate::terminal::TerminalSnapshot,
    row: u16,
    col: u16,
) -> Option<(u16, u16)> {
    let mut target_col = col.checked_sub(1)?;
    while screen
        .cell(row, target_col)
        .is_some_and(|cell| cell.is_wide_continuation())
    {
        target_col = target_col.checked_sub(1)?;
    }
    screen
        .cell(row, target_col)
        .is_some_and(|cell| !cell.contents().is_empty())
        .then_some((row, target_col))
}

fn last_content_cell(screen: &crate::terminal::TerminalSnapshot, row: u16) -> Option<(u16, u16)> {
    let row_snapshot = screen.rows.get(usize::from(row))?;
    row_snapshot
        .cells
        .iter()
        .enumerate()
        .rev()
        .find(|(_, cell)| !cell.is_wide_continuation() && !cell.contents().is_empty())
        .and_then(|(col, _)| u16::try_from(col).ok())
        .map(|col| (row, col))
}

/// Full-screen input widgets often implement wrapping by explicitly drawing
/// an indented continuation row. Such a row is a hard terminal row even
/// though Backspace at its input origin edits the preceding row. Recognize
/// only the narrow, observable shape: a blank prefix and payload reaching the
/// previous row's right margin.
fn explicit_continuation_target(
    screen: &crate::terminal::TerminalSnapshot,
    cursor: (u16, u16),
) -> Option<(u16, u16)> {
    let (row, col) = cursor;
    let previous_row = row.checked_sub(1)?;
    let current = screen.rows.get(usize::from(row))?;
    if current
        .cells
        .iter()
        .take(usize::from(col))
        .any(|cell| !semantic_blank(cell))
    {
        return None;
    }
    let target = last_content_cell(screen, previous_row)?;
    let (_, columns) = screen.size();
    (target.1.saturating_add(2) >= columns).then_some(target)
}

fn semantic_blank(cell: &crate::terminal::Cell) -> bool {
    !cell.is_wide_continuation()
        && (cell.contents().is_empty() || cell.contents().chars().all(|character| character == ' '))
}

fn row_text_changed(before: &Row, view: &View, row: u16) -> bool {
    let Some(after) = view.screen().rows.get(usize::from(row)) else {
        return true;
    };
    before.cells.len() != after.cells.len()
        || before.cells.iter().zip(after.cells.iter()).any(|(a, b)| {
            a.contents() != b.contents() || a.is_wide_continuation() != b.is_wide_continuation()
        })
}

fn row_identity(row: &Row) -> usize {
    std::sync::Arc::as_ptr(&row.cells) as usize
}

/// A deletion may change its target and everything after it, but it cannot
/// rewrite the cells before the cursor at which that deletion settles. This
/// protected prefix distinguishes an edit from an unrelated line repaint.
fn row_prefix_unchanged(before: &Row, view: &View, row: u16, end_col: u16) -> bool {
    let Some(after) = view.screen().rows.get(usize::from(row)) else {
        return false;
    };
    (0..usize::from(end_col)).all(|col| match (before.cells.get(col), after.cells.get(col)) {
        (Some(before), Some(after)) => {
            before.contents() == after.contents()
                && before.is_wide_continuation() == after.is_wide_continuation()
        }
        (None, None) => true,
        _ => false,
    })
}

fn target_cell_changed(before: &Row, view: &View, (row, col): (u16, u16)) -> bool {
    let before = before.cells.get(usize::from(col));
    let after = view.screen().cell(row, col);
    match (before, after) {
        (Some(before), Some(after)) => {
            before.contents() != after.contents()
                || before.is_wide_continuation() != after.is_wide_continuation()
        }
        (None, None) => false,
        _ => true,
    }
}

fn revision_passed(
    boundary: Option<ViewRevision>,
    accessibility_revision: Option<ViewRevision>,
) -> bool {
    match (boundary, accessibility_revision) {
        (Some(boundary), Some(current)) => current > boundary,
        (None, None) => true,
        // Presentation tracking cannot normally change during an intent. If
        // it ever does, retaining is safer than announcing against a model
        // governed by a different publication contract.
        _ => false,
    }
}
