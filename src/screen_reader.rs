use super::{clipboard::Clipboard, ext::ScreenExt, keymap::KeyBindings, perform, speech::Speech, view::View};
use anyhow::Result;
use mlua::{Lua, WeakLua};
use similar::{Algorithm, ChangeTag, TextDiff};
use std::collections::HashSet;
use std::rc::Rc;

#[allow(dead_code)]
pub enum CursorTrackingMode {
    On,
    Off,
    OffOnce,
}

pub struct ScreenReader {
    pub speech: Speech,
    pub help_mode: bool,
    pub auto_read: bool,
    pub review_follows_screen_cursor: bool,
    pub last_key: Vec<u8>,
    pub cursor_tracking_mode: CursorTrackingMode,
    pub highlight_tracking: bool,
    pub clipboard: Clipboard,
    pub pass_through: bool,
    pub key_bindings: KeyBindings,
    pub lua_ctx: Option<Rc<Lua>>,
    pub lua_ctx_weak: Option<WeakLua>,
}

impl ScreenReader {
    pub fn new(speech: Speech) -> Self {
        ScreenReader {
            speech,
            help_mode: false,
            auto_read: true,
            review_follows_screen_cursor: true,
            last_key: Vec::new(),
            cursor_tracking_mode: CursorTrackingMode::On,
            highlight_tracking: false,
            clipboard: Default::default(),
            pass_through: false,
            key_bindings: KeyBindings::new(),
            lua_ctx: None,
            lua_ctx_weak: None,
        }
    }

    pub fn set_lua_context(&mut self, lua: Rc<Lua>) {
        self.lua_ctx_weak = Some(lua.weak());
        self.lua_ctx = Some(lua);
    }

    pub fn track_cursor(&mut self, view: &mut View) -> Result<()> {
        let (prev_cursor, cursor) = (
            view.prev_screen().cursor_position(),
            view.screen().cursor_position(),
        );

        let mut cursor_report: Option<String> = None;
        if cursor.0 != prev_cursor.0 {
            // It moved to a different line
            cursor_report = Some(view.line(cursor.0));
        } else if cursor.1 != prev_cursor.1 {
            // The cursor moved left or right
            let distance_moved = (cursor.1 as i32 - prev_cursor.1 as i32).abs();
            let prev_word_start =
                view.screen().find_word_start(prev_cursor.0, prev_cursor.1);
            let word_start = view.screen().find_word_start(cursor.0, cursor.1);
            if word_start != prev_word_start && distance_moved > 1 {
                // The cursor moved to a different word.
                cursor_report = Some(view.word(cursor.0, cursor.1));
            } else {
                let ch = view.character(cursor.0, cursor.1);
                // Avoid randomly saying "space".
                // Unfortunately this means moving the cursor manually over a space will say
                // nothing.
                let ch = if ch.trim().is_empty() {
                    "".to_string()
                } else {
                    ch
                };
                cursor_report = Some(ch);
            }
        }

        match &self.cursor_tracking_mode {
            CursorTrackingMode::On => {
                self.report_application_cursor_indentation_changes(view)?;
                if let Some(s) = cursor_report {
                    self.speech.speak(&s, false)?;
                }
            }
            CursorTrackingMode::OffOnce => self.cursor_tracking_mode = CursorTrackingMode::On,
            CursorTrackingMode::Off => {}
        }

        Ok(())
    }

    pub fn track_highlighting(&mut self, view: &mut View) -> Result<()> {
        let (highlights, prev_highlights) =
            (view.screen().get_highlights(), view.prev_screen().get_highlights());
        let prev_hl_set: HashSet<String> = HashSet::from_iter(prev_highlights.iter().cloned());

        for hl in highlights {
            if !prev_hl_set.contains(&hl) {
                self.speech.speak(&hl, false)?;
            }
        }
        Ok(())
    }

    /// Report indentation changes, if any, for the line under the application cursor
    pub fn report_application_cursor_indentation_changes(
        &mut self,
        view: &mut View,
    ) -> Result<()> {
        let (indent_level, changed) = view.application_cursor_indentation_level();
        if changed {
            self.speech
                .speak(&format!("indent {}", indent_level), false)?;
        }

        Ok(())
    }

    /// Report indentation changes, if any, for the line under the review cursor
    pub fn report_review_cursor_indentation_changes(
        &mut self,
        view: &mut View,
    ) -> Result<()> {
        let (indent_level, changed) = view.review_cursor_indentation_level();
        if changed {
            self.speech
                .speak(&format!("indent {}", indent_level), false)?;
        }

        Ok(())
    }

    /// Read what's changed between the current and previous screen.
    /// If anything was read, the value in the result will be true.
    pub fn auto_read(
        &mut self,
        view: &mut View,
        reporter: &mut perform::Reporter,
    ) -> Result<bool> {
        self.report_application_cursor_indentation_changes(view)?;
        if view.screen().contents() == view.prev_screen().contents() {
            return Ok(false);
        }

        // Try to read any incoming text.
        // Fall back to a screen diff if that makes more sense.
        let cursor_moves = reporter.cursor_moves;
        let scrolled = reporter.scrolled;
        reporter.reset();
        // Play the new bytes onto a blank screen,
        // so screen.contents() only returns the new text.
        // Using a much taller screen so that we capture text, even if it scrolled off of the real
        // screen.
        let (rows, cols) = view.size();
        let mut parser = vt100::Parser::new(rows * 10, cols, 0);
        parser.process(format!("\x1B[{}B", rows * 10).as_bytes());
        parser.process(&view.next_bytes);
        let text = parser.screen().contents();
        let text = text.trim();

        if !text.is_empty() && (cursor_moves == 0 || scrolled) {
            // Don't echo typed keys
            match std::str::from_utf8(&self.last_key) {
                Ok(s) if text == s => {}
                _ => self.speech.speak(&text, false)?,
            }

            // We still want to report that text was read when suppressing echo,
            // so that cursor tracking doesn't read the character that follows as we type.
            return Ok(true);
        }

        // Do a diff instead
        let mut text = String::new();
        let old = view.prev_screen().contents_full();
        let new = view.screen().contents_full();

        let line_changes = TextDiff::configure()
            .algorithm(Algorithm::Patience)
            .diff_lines(&old, &new);
        // One deletion followed by one insertion, and no other changes,
        // means only a single line changed. In that case, only report what changed in that
        // line.
        // Otherwise, report the entire lines that were added.
        #[derive(PartialEq)]
        enum DiffState {
            /// Nothing has changed
            NoChanges,
            /// A single line was deleted
            OneDeletion,
            /// One deletion followed by one insertion
            Single,
            /// Anything else (including a single insertion)
            Multi,
        }
        let mut diff_state = DiffState::NoChanges;
        for change in line_changes.iter_all_changes() {
            diff_state = match diff_state {
                DiffState::NoChanges => match change.tag() {
                    ChangeTag::Delete => DiffState::OneDeletion,
                    ChangeTag::Equal => DiffState::NoChanges,
                    ChangeTag::Insert => DiffState::Multi,
                },
                DiffState::OneDeletion => match change.tag() {
                    ChangeTag::Delete => DiffState::Multi,
                    ChangeTag::Equal => DiffState::OneDeletion,
                    ChangeTag::Insert => DiffState::Single,
                },
                DiffState::Single => match change.tag() {
                    ChangeTag::Equal => DiffState::Single,
                    _ => DiffState::Multi,
                },
                DiffState::Multi => DiffState::Multi,
            };
            if change.tag() == ChangeTag::Insert {
                text.push_str(&format!("{}\n", change));
            }
        }

        if diff_state == DiffState::Single {
            let mut graphemes = String::new();
            // If there isn't just a single change, just read the whole line.
            diff_state = DiffState::NoChanges;
            let mut prev_tag = None;
            for change in TextDiff::configure()
                .algorithm(Algorithm::Patience)
                .diff_graphemes(&old, &new)
                .iter_all_changes()
            {
                diff_state = match diff_state {
                    DiffState::NoChanges => match change.tag() {
                        ChangeTag::Delete => DiffState::OneDeletion,
                        ChangeTag::Equal => DiffState::NoChanges,
                        ChangeTag::Insert => DiffState::Single,
                    },
                    DiffState::OneDeletion => match change.tag() {
                        ChangeTag::Delete if prev_tag == Some(ChangeTag::Delete) => {
                            DiffState::OneDeletion
                        }
                        ChangeTag::Equal => DiffState::OneDeletion,
                        ChangeTag::Insert if prev_tag == Some(ChangeTag::Delete) => {
                            DiffState::Single
                        }
                        _ => DiffState::Multi,
                    },
                    DiffState::Single => match change.tag() {
                        ChangeTag::Equal => DiffState::Single,
                        ChangeTag::Insert
                            if prev_tag == Some(ChangeTag::Insert)
                                || prev_tag == Some(ChangeTag::Delete) =>
                        {
                            DiffState::Single
                        }
                        _ => DiffState::Multi,
                    },
                    DiffState::Multi => DiffState::Multi,
                };
                prev_tag = Some(change.tag());
                if diff_state == DiffState::Multi {
                    continue; // Revert to the line diff.
                }
                if change.tag() == ChangeTag::Insert {
                    graphemes.push_str(change.as_str().unwrap_or(""));
                }
            }

            if diff_state != DiffState::Multi {
                text = graphemes;
            }
        }

        // Don't echo typed keys
        match std::str::from_utf8(&self.last_key) {
            // We still want to report that text was read when suppressing echo,
            // so that cursor tracking doesn't read the character that follows as we type.
            Ok(s) if text == s => Ok(true),
            _ => {
                self.speech.speak(&text, false)?;
                Ok(!text.is_empty())
            }
        }
    }
}
