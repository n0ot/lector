use super::{Result, ScreenReader};
use crate::{ext::ScreenExt, view::View};
use std::collections::HashSet;

pub(super) enum CursorTrackingMode {
    On,
    OffOnce,
}

pub(super) enum PendingDelete {
    Backspace { cursor: (u16, u16), text: String },
    Delete { text: String },
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
        self.pending_delete = None;
    }

    pub fn defer_backspace(&mut self, view: &View) {
        let (row, col) = view.screen().cursor_position();
        let text = if col > 0 {
            view.screen()
                .cell(row, col - 1)
                .map(|cell| cell.contents().to_string())
                .unwrap_or_default()
        } else {
            String::new()
        };
        self.pending_delete = Some(PendingDelete::Backspace {
            cursor: (row, col),
            text,
        });
    }

    pub fn defer_delete(&mut self, view: &View) {
        let (row, col) = view.screen().cursor_position();
        let text = view
            .screen()
            .cell(row, col)
            .map(|cell| cell.contents().to_string())
            .unwrap_or_default();
        self.pending_delete = Some(PendingDelete::Delete { text });
    }

    pub fn resolve_pending_delete(&mut self, view: &View) -> Result<bool> {
        let Some(pending) = self.pending_delete.take() else {
            return Ok(false);
        };

        let prev_cursor = view.prev_screen().cursor_position();
        let cursor = view.screen().cursor_position();
        let screen_changed =
            view.screen().contents() != view.prev_screen().contents() || cursor != prev_cursor;

        match pending {
            PendingDelete::Backspace {
                cursor: old_cursor,
                text,
            } => {
                if !text.is_empty() && cursor.0 == old_cursor.0 && cursor.1 < old_cursor.1 {
                    self.speak(&text, false)?;
                    return Ok(true);
                }
            }
            PendingDelete::Delete { text } => {
                if !text.is_empty() && screen_changed {
                    self.speak(&text, false)?;
                    return Ok(true);
                }
            }
        }

        Ok(false)
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
