use super::{Result, ScreenReader};
use crate::{
    ext::ScreenExt,
    presentation::{ViewId, ViewRevision},
    view::View,
};
use std::collections::HashSet;

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
        text: String,
    },
    Delete {
        cursor: (u16, u16),
        text: String,
        line_before: String,
    },
}

impl ScreenReader {
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
        let (row, col) = view.screen().cursor_position();
        let view_id = view.view_id();
        let revision_boundary = view.input_intent_revision_boundary();
        let virtual_offset = self
            .pending_deletes
            .back()
            .filter(|pending| {
                pending.view_id == view_id
                    && pending.input_sequence.wrapping_add(1) == self.input_sequence
                    && matches!(pending.kind, PendingDeleteKind::Backspace { .. })
            })
            .and_then(|pending| match pending.kind {
                PendingDeleteKind::Backspace {
                    cursor: (pending_row, pending_col),
                    ..
                } if pending_row == row && pending_col <= col => Some(col - pending_col + 1),
                _ => None,
            })
            .unwrap_or(0);
        let virtual_col = col.saturating_sub(virtual_offset);
        let text = if virtual_col > 0 {
            view.screen()
                .cell(row, virtual_col - 1)
                .map(|cell| cell.contents().to_string())
                .unwrap_or_default()
        } else {
            String::new()
        };
        if text.is_empty() {
            return;
        }
        self.push_pending_delete(PendingDelete {
            view_id,
            revision_boundary,
            last_evaluated_revision: None,
            evaluations: 0,
            input_sequence: self.input_sequence,
            kind: PendingDeleteKind::Backspace {
                cursor: (row, virtual_col),
                text,
            },
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
                    cursor: (pending_row, pending_col),
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
        self.push_pending_delete(PendingDelete {
            view_id,
            revision_boundary,
            last_evaluated_revision: None,
            evaluations: 0,
            input_sequence: self.input_sequence,
            kind: PendingDeleteKind::Delete {
                cursor: (row, virtual_col),
                text,
                line_before: view.line(row),
            },
        });
    }

    pub fn resolve_pending_delete(&mut self, view: &View) -> Result<bool> {
        if self.pending_deletes.is_empty() {
            return Ok(false);
        }

        let view_id = view.view_id();
        let accessibility_revision = view.accessibility_revision();
        let cursor = view.screen().cursor_position();
        let mut pending = std::mem::take(&mut self.pending_deletes);
        let mut retained = std::collections::VecDeque::with_capacity(pending.len());
        let mut spoken = Vec::new();

        while let Some(mut intent) = pending.pop_front() {
            if intent.view_id != view_id {
                retained.push_back(intent);
                continue;
            }
            if !revision_passed(intent.revision_boundary, accessibility_revision) {
                retained.push_back(intent);
                continue;
            }
            if accessibility_revision.is_some()
                && intent.last_evaluated_revision == accessibility_revision
            {
                retained.push_back(intent);
                continue;
            }

            let confirmed = match &intent.kind {
                PendingDeleteKind::Backspace {
                    cursor: old_cursor, ..
                } => cursor.0 == old_cursor.0 && cursor.1 < old_cursor.1,
                PendingDeleteKind::Delete {
                    cursor: (row, _),
                    line_before,
                    ..
                } => view.line(*row) != *line_before,
            };
            if confirmed {
                spoken.push(match intent.kind {
                    PendingDeleteKind::Backspace { text, .. }
                    | PendingDeleteKind::Delete { text, .. } => text,
                });
            } else {
                // The first post-input presentation can be unrelated output.
                // Keep this intent until a frame actually exhibits its
                // deletion, subject to the global resource bound enforced at
                // insertion. It must not block an independently confirmable
                // later intent: an application may ignore one deletion while
                // handling the next.
                intent.last_evaluated_revision = accessibility_revision;
                intent.evaluations = intent.evaluations.saturating_add(1);
                if intent.evaluations < super::MAX_PENDING_DELETE_PRESENTATIONS {
                    retained.push_back(intent);
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
        let (indent_level, changed) = view.application_cursor_indentation_level();
        if changed {
            self.speak(&format!("indent {indent_level}"), false)?;
        }
        Ok(())
    }

    pub fn report_review_cursor_indentation_changes(&mut self, view: &mut View) -> Result<()> {
        let (indent_level, changed) = view.review_cursor_indentation_level();
        if changed {
            self.speak(&format!("indent {indent_level}"), false)?;
        }
        Ok(())
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
