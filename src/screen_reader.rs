use super::{
    clipboard::Clipboard,
    ext::ScreenExt,
    keymap::{InputMode, KeyBindings},
    perform,
    speech::Speech,
    table::TableState,
    view::View,
};
use anyhow::{Result, anyhow};
use mlua::{Function, Lua, RegistryKey, Value, WeakLua};
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
    pub input_mode: InputMode,
    pub table_state: Option<TableState>,
    pub table_setup_state: Option<TableSetupState>,
    pub table_header_auto: bool,
    pub stop_speech_on_focus_loss: bool,
    pub terminal_focused: bool,
    pub lua_ctx: Option<Rc<Lua>>,
    pub lua_ctx_weak: Option<WeakLua>,
    lua_hooks: LuaHooks,
    diff_text: String,
    diff_graphemes: String,
    live_text: String,
    pending_delete: Option<PendingDelete>,
}

enum PendingDelete {
    Backspace { cursor: (u16, u16), text: String },
    Delete { text: String },
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
            input_mode: InputMode::Normal,
            table_state: None,
            table_setup_state: None,
            table_header_auto: true,
            stop_speech_on_focus_loss: true,
            terminal_focused: true,
            lua_ctx: None,
            lua_ctx_weak: None,
            lua_hooks: LuaHooks::default(),
            diff_text: String::new(),
            diff_graphemes: String::new(),
            live_text: String::new(),
            pending_delete: None,
        }
    }

    pub fn set_lua_context(&mut self, lua: Rc<Lua>) {
        self.lua_ctx_weak = Some(lua.weak());
        self.lua_ctx = Some(lua);
    }

    pub fn speak(&mut self, text: &str, interrupt: bool) -> Result<()> {
        if text.is_empty() || !self.terminal_focused {
            return Ok(());
        }
        self.call_hook_on_speech_start(text, interrupt)?;
        let result = self.speech.speak(text, interrupt);
        let ok = result.is_ok();
        self.call_hook_on_speech_end(text, interrupt, ok)?;
        result
    }

    pub fn set_hook(&mut self, lua: &Lua, name: &str, value: Value) -> anyhow::Result<()> {
        match value {
            Value::Nil => {
                let Some(slot) = self.lua_hooks.slot_mut(name) else {
                    return Err(anyhow!("unknown hook: {}", name));
                };
                if let Some(key) = slot.take() {
                    lua.remove_registry_value(key)
                        .map_err(|err| anyhow!(err.to_string()))?;
                }
                Ok(())
            }
            Value::Function(func) => {
                self.ensure_lua_context(lua)?;
                let Some(slot) = self.lua_hooks.slot_mut(name) else {
                    return Err(anyhow!("unknown hook: {}", name));
                };
                if let Some(key) = slot.take() {
                    lua.remove_registry_value(key)
                        .map_err(|err| anyhow!(err.to_string()))?;
                }
                let key = lua
                    .create_registry_value(func)
                    .map_err(|err| anyhow!(err.to_string()))?;
                *slot = Some(key);
                Ok(())
            }
            _ => Err(anyhow!("hook value must be a function or nil")),
        }
    }

    pub fn get_hook(&self, lua: &Lua, name: &str) -> anyhow::Result<Value> {
        let Some(slot) = self.lua_hooks.slot(name) else {
            return Err(anyhow!("unknown hook: {}", name));
        };
        let Some(key) = slot else {
            return Ok(Value::Nil);
        };
        self.ensure_lua_context(lua)?;
        let func: Function = lua
            .registry_value(key)
            .map_err(|err| anyhow!(err.to_string()))?;
        Ok(Value::Function(func))
    }

    pub fn hook_on_startup(&mut self, config_path: &str) -> Result<()> {
        let Some(key) = &self.lua_hooks.on_startup else {
            return Ok(());
        };
        let Some(lua) = self.lua_ctx.as_ref() else {
            return Ok(());
        };
        let tbl = lua.create_table().map_err(|err| anyhow!(err.to_string()))?;
        tbl.set("config_path", config_path)
            .map_err(|err| anyhow!(err.to_string()))?;
        tbl.set("version", env!("CARGO_PKG_VERSION"))
            .map_err(|err| anyhow!(err.to_string()))?;
        tbl.set("pid", std::process::id())
            .map_err(|err| anyhow!(err.to_string()))?;
        let func: Function = lua
            .registry_value(key)
            .map_err(|err| anyhow!(err.to_string()))?;
        func.call::<()>(tbl).map_err(|err| anyhow!(err.to_string()))
    }

    pub fn hook_on_shutdown(&mut self, reason: &str) -> Result<()> {
        let Some(key) = &self.lua_hooks.on_shutdown else {
            return Ok(());
        };
        let Some(lua) = self.lua_ctx.as_ref() else {
            return Ok(());
        };
        let func: Function = lua
            .registry_value(key)
            .map_err(|err| anyhow!(err.to_string()))?;
        func.call::<()>(reason.to_string())
            .map_err(|err| anyhow!(err.to_string()))
    }

    pub fn hook_on_error(&mut self, message: &str, context: &str) -> Result<()> {
        let Some(key) = &self.lua_hooks.on_error else {
            return Ok(());
        };
        let Some(lua) = self.lua_ctx.as_ref() else {
            return Ok(());
        };
        let func: Function = lua
            .registry_value(key)
            .map_err(|err| anyhow!(err.to_string()))?;
        func.call::<()>((message.to_string(), context.to_string()))
            .map_err(|err| anyhow!(err.to_string()))
    }

    pub fn hook_on_screen_update(&mut self, view: &View, overlay_active: bool) -> Result<()> {
        let Some(key) = &self.lua_hooks.on_screen_update else {
            return Ok(());
        };
        let Some(lua) = self.lua_ctx.as_ref() else {
            return Ok(());
        };
        let (rows, cols) = view.size();
        let (cursor_row, cursor_col) = view.screen().cursor_position();
        let (prev_cursor_row, prev_cursor_col) = view.prev_screen().cursor_position();
        let changed = view.screen().contents() != view.prev_screen().contents();
        let tbl = lua.create_table().map_err(|err| anyhow!(err.to_string()))?;
        tbl.set("rows", rows)
            .map_err(|err| anyhow!(err.to_string()))?;
        tbl.set("cols", cols)
            .map_err(|err| anyhow!(err.to_string()))?;
        tbl.set("cursor_row", cursor_row)
            .map_err(|err| anyhow!(err.to_string()))?;
        tbl.set("cursor_col", cursor_col)
            .map_err(|err| anyhow!(err.to_string()))?;
        tbl.set("prev_cursor_row", prev_cursor_row)
            .map_err(|err| anyhow!(err.to_string()))?;
        tbl.set("prev_cursor_col", prev_cursor_col)
            .map_err(|err| anyhow!(err.to_string()))?;
        tbl.set("changed", changed)
            .map_err(|err| anyhow!(err.to_string()))?;
        tbl.set("overlay", overlay_active)
            .map_err(|err| anyhow!(err.to_string()))?;
        tbl.set("screen", view.contents_full())
            .map_err(|err| anyhow!(err.to_string()))?;
        tbl.set("prev_screen", view.prev_screen().contents_full())
            .map_err(|err| anyhow!(err.to_string()))?;
        let func: Function = lua
            .registry_value(key)
            .map_err(|err| anyhow!(err.to_string()))?;
        func.call::<()>(tbl).map_err(|err| anyhow!(err.to_string()))
    }

    pub fn hook_on_review_cursor_move(
        &mut self,
        old_pos: (u16, u16),
        new_pos: (u16, u16),
    ) -> Result<()> {
        let Some(key) = &self.lua_hooks.on_review_cursor_move else {
            return Ok(());
        };
        if old_pos == new_pos {
            return Ok(());
        }
        let Some(lua) = self.lua_ctx.as_ref() else {
            return Ok(());
        };
        let tbl = lua.create_table().map_err(|err| anyhow!(err.to_string()))?;
        tbl.set("row", new_pos.0)
            .map_err(|err| anyhow!(err.to_string()))?;
        tbl.set("col", new_pos.1)
            .map_err(|err| anyhow!(err.to_string()))?;
        tbl.set("prev_row", old_pos.0)
            .map_err(|err| anyhow!(err.to_string()))?;
        tbl.set("prev_col", old_pos.1)
            .map_err(|err| anyhow!(err.to_string()))?;
        let func: Function = lua
            .registry_value(key)
            .map_err(|err| anyhow!(err.to_string()))?;
        func.call::<()>(tbl).map_err(|err| anyhow!(err.to_string()))
    }

    pub fn hook_on_mode_change(&mut self, old: InputMode, new: InputMode) -> Result<()> {
        let Some(key) = &self.lua_hooks.on_mode_change else {
            return Ok(());
        };
        if old == new {
            return Ok(());
        }
        let Some(lua) = self.lua_ctx.as_ref() else {
            return Ok(());
        };
        let func: Function = lua
            .registry_value(key)
            .map_err(|err| anyhow!(err.to_string()))?;
        func.call::<()>((old.as_str().to_string(), new.as_str().to_string()))
            .map_err(|err| anyhow!(err.to_string()))
    }

    pub fn hook_on_table_mode_enter(&mut self, table_state: &TableState) -> Result<()> {
        let Some(key) = &self.lua_hooks.on_table_mode_enter else {
            return Ok(());
        };
        let Some(lua) = self.lua_ctx.as_ref() else {
            return Ok(());
        };
        let model = &table_state.model;
        let tbl = lua.create_table().map_err(|err| anyhow!(err.to_string()))?;
        tbl.set("top", model.top)
            .map_err(|err| anyhow!(err.to_string()))?;
        tbl.set("bottom", model.bottom)
            .map_err(|err| anyhow!(err.to_string()))?;
        tbl.set("columns", model.columns.len())
            .map_err(|err| anyhow!(err.to_string()))?;
        if let Some(row) = model.header_row {
            tbl.set("header_row", row)
                .map_err(|err| anyhow!(err.to_string()))?;
        } else {
            tbl.set("header_row", Value::Nil)
                .map_err(|err| anyhow!(err.to_string()))?;
        }
        tbl.set("current_col", table_state.current_col)
            .map_err(|err| anyhow!(err.to_string()))?;
        let func: Function = lua
            .registry_value(key)
            .map_err(|err| anyhow!(err.to_string()))?;
        func.call::<()>(tbl).map_err(|err| anyhow!(err.to_string()))
    }

    pub fn hook_on_table_mode_exit(&mut self) -> Result<()> {
        let Some(key) = &self.lua_hooks.on_table_mode_exit else {
            return Ok(());
        };
        let Some(lua) = self.lua_ctx.as_ref() else {
            return Ok(());
        };
        let func: Function = lua
            .registry_value(key)
            .map_err(|err| anyhow!(err.to_string()))?;
        func.call::<()>(()).map_err(|err| anyhow!(err.to_string()))
    }

    pub fn hook_on_clipboard_change(&mut self, op: &str, entry: Option<&str>) -> Result<()> {
        let Some(key) = &self.lua_hooks.on_clipboard_change else {
            return Ok(());
        };
        let Some(lua) = self.lua_ctx.as_ref() else {
            return Ok(());
        };
        let meta = lua.create_table().map_err(|err| anyhow!(err.to_string()))?;
        meta.set("op", op).map_err(|err| anyhow!(err.to_string()))?;
        meta.set("index", self.clipboard.index())
            .map_err(|err| anyhow!(err.to_string()))?;
        meta.set("size", self.clipboard.size())
            .map_err(|err| anyhow!(err.to_string()))?;
        let entry = match entry {
            Some(value) => Value::String(
                lua.create_string(value)
                    .map_err(|err| anyhow!(err.to_string()))?,
            ),
            None => Value::Nil,
        };
        let func: Function = lua
            .registry_value(key)
            .map_err(|err| anyhow!(err.to_string()))?;
        func.call::<()>((entry, meta))
            .map_err(|err| anyhow!(err.to_string()))
    }

    pub fn hook_on_key_unhandled(&mut self, key: Option<&str>, mode: InputMode) -> Result<bool> {
        let Some(key_ref) = &self.lua_hooks.on_key_unhandled else {
            return Ok(false);
        };
        let Some(lua) = self.lua_ctx.as_ref() else {
            return Ok(false);
        };
        let func: Function = lua
            .registry_value(key_ref)
            .map_err(|err| anyhow!(err.to_string()))?;
        let key_value = match key {
            Some(value) => Value::String(
                lua.create_string(value)
                    .map_err(|err| anyhow!(err.to_string()))?,
            ),
            None => Value::Nil,
        };
        let res: Value = func
            .call((key_value, mode.as_str().to_string()))
            .map_err(|err| anyhow!(err.to_string()))?;
        Ok(matches!(res, Value::Boolean(true)))
    }

    pub fn hook_on_live_read(
        &mut self,
        text: &str,
        cursor_moves: usize,
        scrolled: bool,
    ) -> Result<Option<String>> {
        let Some(key) = &self.lua_hooks.on_live_read else {
            return Ok(Some(text.to_string()));
        };
        let Some(lua) = self.lua_ctx.as_ref() else {
            return Ok(Some(text.to_string()));
        };
        let meta = lua.create_table().map_err(|err| anyhow!(err.to_string()))?;
        meta.set("cursor_moves", cursor_moves)
            .map_err(|err| anyhow!(err.to_string()))?;
        meta.set("scrolled", scrolled)
            .map_err(|err| anyhow!(err.to_string()))?;
        let func: Function = lua
            .registry_value(key)
            .map_err(|err| anyhow!(err.to_string()))?;
        let res: Value = func
            .call((text.to_string(), meta))
            .map_err(|err| anyhow!(err.to_string()))?;
        match res {
            Value::Nil => Ok(None),
            Value::Boolean(false) => Ok(None),
            Value::String(s) => Ok(Some(
                s.to_str()
                    .map_err(|err| anyhow!(err.to_string()))?
                    .to_string(),
            )),
            _ => Err(anyhow!("on_live_read must return a string or nil")),
        }
    }

    fn call_hook_on_speech_start(&mut self, text: &str, interrupt: bool) -> Result<()> {
        let Some(key) = &self.lua_hooks.on_speech_start else {
            return Ok(());
        };
        let Some(lua) = self.lua_ctx.as_ref() else {
            return Ok(());
        };
        let meta = lua.create_table().map_err(|err| anyhow!(err.to_string()))?;
        meta.set("interrupt", interrupt)
            .map_err(|err| anyhow!(err.to_string()))?;
        let func: Function = lua
            .registry_value(key)
            .map_err(|err| anyhow!(err.to_string()))?;
        func.call::<()>((text.to_string(), meta))
            .map_err(|err| anyhow!(err.to_string()))
    }

    fn call_hook_on_speech_end(&mut self, text: &str, interrupt: bool, ok: bool) -> Result<()> {
        let Some(key) = &self.lua_hooks.on_speech_end else {
            return Ok(());
        };
        let Some(lua) = self.lua_ctx.as_ref() else {
            return Ok(());
        };
        let meta = lua.create_table().map_err(|err| anyhow!(err.to_string()))?;
        meta.set("interrupt", interrupt)
            .map_err(|err| anyhow!(err.to_string()))?;
        meta.set("ok", ok).map_err(|err| anyhow!(err.to_string()))?;
        let func: Function = lua
            .registry_value(key)
            .map_err(|err| anyhow!(err.to_string()))?;
        func.call::<()>((text.to_string(), meta))
            .map_err(|err| anyhow!(err.to_string()))
    }

    fn ensure_lua_context(&self, lua: &Lua) -> anyhow::Result<()> {
        let Some(weak_ctx) = self.lua_ctx_weak.as_ref() else {
            return Err(anyhow!("lua hooks are only available in init.lua"));
        };
        if *weak_ctx != lua.weak() {
            return Err(anyhow!("lua hooks are only available in init.lua"));
        }
        Ok(())
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
            let prev_word_start = view.screen().find_word_start(prev_cursor.0, prev_cursor.1);
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
                    self.speak(&s, false)?;
                }
            }
            CursorTrackingMode::OffOnce => self.cursor_tracking_mode = CursorTrackingMode::On,
            CursorTrackingMode::Off => {}
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
        let prev_hl_set: HashSet<String> = HashSet::from_iter(prev_highlights.iter().cloned());

        for hl in highlights {
            if !prev_hl_set.contains(&hl) {
                self.speak(&hl, false)?;
            }
        }
        Ok(())
    }

    /// Report indentation changes, if any, for the line under the application cursor
    pub fn report_application_cursor_indentation_changes(&mut self, view: &mut View) -> Result<()> {
        let (indent_level, changed) = view.application_cursor_indentation_level();
        if changed {
            self.speak(&format!("indent {}", indent_level), false)?;
        }

        Ok(())
    }

    /// Report indentation changes, if any, for the line under the review cursor
    pub fn report_review_cursor_indentation_changes(&mut self, view: &mut View) -> Result<()> {
        let (indent_level, changed) = view.review_cursor_indentation_level();
        if changed {
            self.speak(&format!("indent {}", indent_level), false)?;
        }

        Ok(())
    }

    /// Read what's changed between the current and previous screen.
    /// If anything was read, the value in the result will be true.
    pub fn auto_read(&mut self, view: &mut View, reporter: &mut perform::Reporter) -> Result<bool> {
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
        let mut live_text = std::mem::take(&mut self.live_text);
        live_text.clear();
        if !view.next_bytes.is_empty() {
            let (rows, cols) = view.size();
            let mut parser = vt100::Parser::new(rows * 10, cols, 0);
            parser.process(format!("\x1B[{}B", rows * 10).as_bytes());
            parser.process(&view.next_bytes);
            live_text = parser.screen().contents();
        }
        let mut live_read_result = None;
        {
            let text = live_text.trim();
            if !text.is_empty() && (cursor_moves == 0 || scrolled) {
                // Don't echo typed keys
                let mut spoken = false;
                match std::str::from_utf8(&self.last_key) {
                    Ok(s) if text == s => {}
                    _ => {
                        let text = self.hook_on_live_read(text, cursor_moves, scrolled)?;
                        if let Some(text) = text
                            && !text.is_empty()
                        {
                            self.speak(&text, false)?;
                            spoken = true;
                        }
                    }
                }

                // We still want to report that text was read when suppressing echo or hook output,
                // so that cursor tracking doesn't read the character that follows as we type.
                live_read_result = Some(spoken || !text.is_empty());
            }
        }

        if let Some(result) = live_read_result {
            self.live_text = live_text;
            return Ok(result);
        }

        self.live_text = live_text;

        // Do a diff instead
        let mut diff_text = std::mem::take(&mut self.diff_text);
        diff_text.clear();
        let (old_text, new_text, prev_hashes, curr_hashes) = view.full_contents_cached();

        if prev_hashes.len() == curr_hashes.len()
            && prev_hashes == curr_hashes
            && old_text == new_text
        {
            self.diff_text = diff_text;
            return Ok(false);
        }

        let line_changes = TextDiff::configure()
            .algorithm(Algorithm::Patience)
            .diff_lines(old_text, new_text);
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
            if change.tag() == ChangeTag::Insert
                && let Some(change_str) = change.as_str()
            {
                diff_text.push_str(change_str);
                diff_text.push('\n');
            }
        }

        if diff_state == DiffState::Single {
            let mut grapheme_buf = std::mem::take(&mut self.diff_graphemes);
            grapheme_buf.clear();
            // Prefer the precise single-edit behavior. If there are multiple changed regions on
            // the line, fall back to changed whitespace-delimited fields below.
            diff_state = DiffState::NoChanges;
            let mut prev_tag = None;
            for change in TextDiff::configure()
                .algorithm(Algorithm::Patience)
                .diff_graphemes(old_text, new_text)
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
                    continue;
                }
                if change.tag() == ChangeTag::Insert
                    && let Some(change_str) = change.as_str()
                {
                    grapheme_buf.push_str(change_str);
                }
            }

            if diff_state == DiffState::Multi {
                grapheme_buf.clear();
                if collect_inserted_fields(old_text, new_text, &mut grapheme_buf) {
                    std::mem::swap(&mut diff_text, &mut grapheme_buf);
                }
            } else {
                std::mem::swap(&mut diff_text, &mut grapheme_buf);
            }
            self.diff_graphemes = grapheme_buf;
        }

        // Don't echo typed keys
        match std::str::from_utf8(&self.last_key) {
            // We still want to report that text was read when suppressing echo,
            // so that cursor tracking doesn't read the character that follows as we type.
            Ok(s) if diff_text == s => {
                self.diff_text = diff_text;
                Ok(true)
            }
            _ => {
                let original_nonempty = !diff_text.is_empty();
                let text = self.hook_on_live_read(&diff_text, cursor_moves, scrolled)?;
                if let Some(text) = text
                    && !text.is_empty()
                {
                    self.speak(&text, false)?;
                }
                self.diff_text = diff_text;
                Ok(original_nonempty)
            }
        }
    }
}

fn collect_inserted_fields(old_text: &str, new_text: &str, out: &mut String) -> bool {
    let old_fields: Vec<_> = old_text.split_whitespace().collect();
    let new_fields: Vec<_> = new_text.split_whitespace().collect();
    let old_len = old_fields.len();
    let new_len = new_fields.len();
    if new_len == 0 {
        return false;
    }

    let mut lcs = vec![0; (old_len + 1) * (new_len + 1)];
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
    let mut spoke = false;
    while old_idx < old_len || new_idx < new_len {
        if old_idx < old_len && new_idx < new_len && old_fields[old_idx] == new_fields[new_idx] {
            flush_inserted_field_hunk(&deleted_hunk, &inserted_hunk, out, &mut spoke);
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
    flush_inserted_field_hunk(&deleted_hunk, &inserted_hunk, out, &mut spoke);

    spoke
}

fn flush_inserted_field_hunk(
    deleted: &[&str],
    inserted: &[&str],
    out: &mut String,
    spoke: &mut bool,
) {
    if inserted.is_empty() {
        return;
    }

    if deleted.len() == inserted.len() {
        for (old_field, new_field) in deleted.iter().zip(inserted) {
            append_inserted_field(field_replacement(old_field, new_field), out, spoke);
        }
    } else {
        for field in inserted {
            append_inserted_field(field, out, spoke);
        }
    }
}

fn append_inserted_field(field: &str, out: &mut String, spoke: &mut bool) {
    if field.is_empty() {
        return;
    }
    if *spoke {
        out.push(' ');
    }
    out.push_str(field);
    *spoke = true;
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
        let Some((prev_idx, prev_ch)) = new_field[..start].char_indices().next_back() else {
            break;
        };
        if !is_word_char(prev_ch) {
            break;
        }
        start = prev_idx;
    }

    while end < new_field.len() {
        let Some(next_ch) = new_field[end..].chars().next() else {
            break;
        };
        if !is_word_char(next_ch) {
            break;
        }
        end += next_ch.len_utf8();
    }

    &new_field[start..end]
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

#[derive(Clone, Debug)]
pub struct TableSetupState {
    pub header_row: u16,
    pub tabstops: Vec<u16>,
}

#[derive(Default)]
struct LuaHooks {
    on_startup: Option<RegistryKey>,
    on_shutdown: Option<RegistryKey>,
    on_screen_update: Option<RegistryKey>,
    on_live_read: Option<RegistryKey>,
    on_review_cursor_move: Option<RegistryKey>,
    on_mode_change: Option<RegistryKey>,
    on_table_mode_enter: Option<RegistryKey>,
    on_table_mode_exit: Option<RegistryKey>,
    on_clipboard_change: Option<RegistryKey>,
    on_speech_start: Option<RegistryKey>,
    on_speech_end: Option<RegistryKey>,
    on_key_unhandled: Option<RegistryKey>,
    on_error: Option<RegistryKey>,
}

impl LuaHooks {
    fn slot_mut(&mut self, name: &str) -> Option<&mut Option<RegistryKey>> {
        match name {
            "on_startup" => Some(&mut self.on_startup),
            "on_shutdown" => Some(&mut self.on_shutdown),
            "on_screen_update" => Some(&mut self.on_screen_update),
            "on_live_read" => Some(&mut self.on_live_read),
            "on_review_cursor_move" => Some(&mut self.on_review_cursor_move),
            "on_mode_change" => Some(&mut self.on_mode_change),
            "on_table_mode_enter" => Some(&mut self.on_table_mode_enter),
            "on_table_mode_exit" => Some(&mut self.on_table_mode_exit),
            "on_clipboard_change" => Some(&mut self.on_clipboard_change),
            "on_speech_start" => Some(&mut self.on_speech_start),
            "on_speech_end" => Some(&mut self.on_speech_end),
            "on_key_unhandled" => Some(&mut self.on_key_unhandled),
            "on_error" => Some(&mut self.on_error),
            _ => None,
        }
    }

    fn slot(&self, name: &str) -> Option<&Option<RegistryKey>> {
        match name {
            "on_startup" => Some(&self.on_startup),
            "on_shutdown" => Some(&self.on_shutdown),
            "on_screen_update" => Some(&self.on_screen_update),
            "on_live_read" => Some(&self.on_live_read),
            "on_review_cursor_move" => Some(&self.on_review_cursor_move),
            "on_mode_change" => Some(&self.on_mode_change),
            "on_table_mode_enter" => Some(&self.on_table_mode_enter),
            "on_table_mode_exit" => Some(&self.on_table_mode_exit),
            "on_clipboard_change" => Some(&self.on_clipboard_change),
            "on_speech_start" => Some(&self.on_speech_start),
            "on_speech_end" => Some(&self.on_speech_end),
            "on_key_unhandled" => Some(&self.on_key_unhandled),
            "on_error" => Some(&self.on_error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ScreenReader;
    use crate::{perform, speech, view::View};
    use std::{cell::RefCell, rc::Rc};

    struct TestDriver {
        speaks: Rc<RefCell<Vec<String>>>,
    }

    impl speech::Driver for TestDriver {
        fn speak(&mut self, text: &str, _interrupt: bool) -> anyhow::Result<()> {
            self.speaks.borrow_mut().push(text.to_string());
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

    fn make_sr() -> (ScreenReader, Rc<RefCell<Vec<String>>>) {
        let speaks = Rc::new(RefCell::new(Vec::new()));
        let driver = TestDriver {
            speaks: Rc::clone(&speaks),
        };
        let speech = speech::Speech::new(Box::new(driver));
        let sr = ScreenReader::new(speech);
        (sr, speaks)
    }

    #[test]
    fn auto_read_returns_false_when_unchanged() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 10);
        let mut reporter = perform::Reporter::new();

        view.process_changes(b"hello");
        view.finalize_changes(0);

        let read = sr.auto_read(&mut view, &mut reporter).unwrap();
        assert!(!read);
        assert!(speaks.borrow().is_empty());
    }

    #[test]
    fn auto_read_speaks_new_text() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 10);
        let mut reporter = perform::Reporter::new();

        view.process_changes(b"hi");
        let read = sr.auto_read(&mut view, &mut reporter).unwrap();
        assert!(read);
        let speaks = speaks.borrow();
        assert_eq!(speaks.len(), 1);
        assert_eq!(speaks[0], "hi");
    }

    #[test]
    fn auto_read_suppresses_echo_of_last_key() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 10);
        let mut reporter = perform::Reporter::new();

        sr.last_key = b"hi".to_vec();
        view.process_changes(b"hi");
        let read = sr.auto_read(&mut view, &mut reporter).unwrap();
        assert!(read);
        assert!(speaks.borrow().is_empty());
    }

    #[test]
    fn auto_read_speaks_multiple_inserted_runs_from_single_changed_line() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 40);
        let mut reporter = perform::Reporter::new();

        view.process_changes(b"left one right two");
        view.finalize_changes(0);

        view.process_changes(b"\r\x1B[Kleft alpha right beta");
        reporter.cursor_moves = 1;
        let read = sr.auto_read(&mut view, &mut reporter).unwrap();

        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["alpha beta"]);
    }

    #[test]
    fn auto_read_speaks_short_status_line_replacements() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 120);
        let mut reporter = perform::Reporter::new();

        view.process_changes(
            b"[dev] 1:bash* 2:bash-                                             bash.1",
        );
        view.finalize_changes(0);

        view.process_changes(
            b"\r\x1B[K[dev] 1:caffeinate* 2:bash-                                      caffeinate.1",
        );
        reporter.cursor_moves = 1;
        let read = sr.auto_read(&mut view, &mut reporter).unwrap();

        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["caffeinate caffeinate"]);
    }

    #[test]
    fn auto_read_speaks_shorter_replacements_to_word_boundaries() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 120);
        let mut reporter = perform::Reporter::new();

        view.process_changes(
            b"[dev] 1:bash* 2:bash-                                             bash.1",
        );
        view.finalize_changes(0);

        view.process_changes(
            b"\r\x1B[K[dev] 1:gh* 2:bash-                                                gh.1",
        );
        reporter.cursor_moves = 1;
        let read = sr.auto_read(&mut view, &mut reporter).unwrap();

        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["gh gh"]);
    }

    #[test]
    fn deferred_backspace_only_speaks_when_cursor_moves_left() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 10);

        view.process_changes(b"$ x");
        view.finalize_changes(0);
        sr.defer_backspace(&view);

        view.process_changes(b"\x08\x1B[P");
        let read = sr.resolve_pending_delete(&view).unwrap();
        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["x"]);
    }

    #[test]
    fn deferred_backspace_stays_silent_when_cursor_does_not_move() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 10);

        view.process_changes(b"$ ");
        view.finalize_changes(0);
        sr.defer_backspace(&view);

        let read = sr.resolve_pending_delete(&view).unwrap();
        assert!(!read);
        assert!(speaks.borrow().is_empty());
    }

    #[test]
    fn deferred_delete_speaks_when_screen_changes() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 10);

        view.process_changes(b"abc\x1B[D\x1B[D");
        view.finalize_changes(0);
        sr.defer_delete(&view);

        view.process_changes(b"\x1B[P");
        let read = sr.resolve_pending_delete(&view).unwrap();
        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["b"]);
    }
}
