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
    pub table_header_auto: bool,
    pub lua_ctx: Option<Rc<Lua>>,
    pub lua_ctx_weak: Option<WeakLua>,
    lua_hooks: LuaHooks,
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
            table_header_auto: true,
            lua_ctx: None,
            lua_ctx_weak: None,
            lua_hooks: LuaHooks::default(),
        }
    }

    pub fn set_lua_context(&mut self, lua: Rc<Lua>) {
        self.lua_ctx_weak = Some(lua.weak());
        self.lua_ctx = Some(lua);
    }

    pub fn speak(&mut self, text: &str, interrupt: bool) -> Result<()> {
        if text.is_empty() {
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
        func.call::<()>(tbl)
            .map_err(|err| anyhow!(err.to_string()))
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
        func.call::<()>((
            message.to_string(),
            context.to_string(),
        ))
        .map_err(|err| anyhow!(err.to_string()))
    }

    pub fn hook_on_screen_update(
        &mut self,
        view: &View,
        overlay_active: bool,
    ) -> Result<()> {
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
        func.call::<()>(tbl)
            .map_err(|err| anyhow!(err.to_string()))
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
        func.call::<()>(tbl)
            .map_err(|err| anyhow!(err.to_string()))
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
        func.call::<()>((
            old.as_str().to_string(),
            new.as_str().to_string(),
        ))
        .map_err(|err| anyhow!(err.to_string()))
    }

    pub fn hook_on_table_mode_enter(
        &mut self,
        table_state: &TableState,
    ) -> Result<()> {
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
        func.call::<()>(tbl)
            .map_err(|err| anyhow!(err.to_string()))
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
        func.call::<()>(())
            .map_err(|err| anyhow!(err.to_string()))
    }

    pub fn hook_on_clipboard_change(
        &mut self,
        op: &str,
        entry: Option<&str>,
    ) -> Result<()> {
        let Some(key) = &self.lua_hooks.on_clipboard_change else {
            return Ok(());
        };
        let Some(lua) = self.lua_ctx.as_ref() else {
            return Ok(());
        };
        let meta = lua.create_table().map_err(|err| anyhow!(err.to_string()))?;
        meta.set("op", op)
            .map_err(|err| anyhow!(err.to_string()))?;
        meta.set("index", self.clipboard.index())
            .map_err(|err| anyhow!(err.to_string()))?;
        meta.set("size", self.clipboard.size())
            .map_err(|err| anyhow!(err.to_string()))?;
        let entry = match entry {
            Some(value) => Value::String(lua.create_string(value).map_err(|err| anyhow!(err.to_string()))?),
            None => Value::Nil,
        };
        let func: Function = lua
            .registry_value(key)
            .map_err(|err| anyhow!(err.to_string()))?;
        func.call::<()>( (entry, meta) )
            .map_err(|err| anyhow!(err.to_string()))
    }

    pub fn hook_on_key_unhandled(
        &mut self,
        key: Option<&str>,
        mode: InputMode,
    ) -> Result<bool> {
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
            Some(value) => Value::String(lua.create_string(value).map_err(|err| anyhow!(err.to_string()))?),
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
        func.call::<()>( (text.to_string(), meta) )
            .map_err(|err| anyhow!(err.to_string()))
    }

    fn call_hook_on_speech_end(
        &mut self,
        text: &str,
        interrupt: bool,
        ok: bool,
    ) -> Result<()> {
        let Some(key) = &self.lua_hooks.on_speech_end else {
            return Ok(());
        };
        let Some(lua) = self.lua_ctx.as_ref() else {
            return Ok(());
        };
        let meta = lua.create_table().map_err(|err| anyhow!(err.to_string()))?;
        meta.set("interrupt", interrupt)
            .map_err(|err| anyhow!(err.to_string()))?;
        meta.set("ok", ok)
            .map_err(|err| anyhow!(err.to_string()))?;
        let func: Function = lua
            .registry_value(key)
            .map_err(|err| anyhow!(err.to_string()))?;
        func.call::<()>( (text.to_string(), meta) )
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
                    self.speak(&s, false)?;
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
                self.speak(&hl, false)?;
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
            self.speak(&format!("indent {}", indent_level), false)?;
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
            self.speak(&format!("indent {}", indent_level), false)?;
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
        let mut text = String::new();
        if !view.next_bytes.is_empty() {
            let (rows, cols) = view.size();
            let mut parser = vt100::Parser::new(rows * 10, cols, 0);
            parser.process(format!("\x1B[{}B", rows * 10).as_bytes());
            parser.process(&view.next_bytes);
            text = parser.screen().contents();
        }
        let text = text.trim();

        if !text.is_empty() && (cursor_moves == 0 || scrolled) {
            // Don't echo typed keys
            let mut spoken = false;
            match std::str::from_utf8(&self.last_key) {
                Ok(s) if text == s => {}
                _ => {
                    let text = self.hook_on_live_read(text, cursor_moves, scrolled)?;
                    if let Some(text) = text {
                        if !text.is_empty() {
                            self.speak(&text, false)?;
                            spoken = true;
                        }
                    }
                }
            }

            // We still want to report that text was read when suppressing echo or hook output,
            // so that cursor tracking doesn't read the character that follows as we type.
            return Ok(spoken || !text.is_empty());
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
                let original_nonempty = !text.is_empty();
                let text = self.hook_on_live_read(&text, cursor_moves, scrolled)?;
                if let Some(text) = text {
                    if !text.is_empty() {
                        self.speak(&text, false)?;
                    }
                }
                Ok(original_nonempty)
            }
        }
    }
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
}
