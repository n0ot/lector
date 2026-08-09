use super::{Error, Result, ScreenReader};
use crate::{ext::ScreenExt, keymap::InputMode, table::TableState, view::View};
use mlua::{Function, Lua, RegistryKey, Value};

impl ScreenReader {
    pub(crate) fn lua_binding_context(&self, lua: &Lua) -> Result<&std::rc::Rc<Lua>> {
        if !self.owns_lua_context(lua) {
            return Err(Error::InvalidLuaBindingContext);
        }
        self.lua_ctx.as_ref().ok_or(Error::InvalidLuaBindingContext)
    }

    pub(crate) fn owns_lua_context(&self, lua: &Lua) -> bool {
        self.lua_ctx_weak
            .as_ref()
            .is_some_and(|context| *context == lua.weak())
    }

    pub fn set_hook(&mut self, lua: &Lua, name: &str, value: Value) -> Result<()> {
        match value {
            Value::Nil => {
                let Some(slot) = self.lua_hooks.slot_mut(name) else {
                    return Err(Error::UnknownHook(name.into()));
                };
                if let Some(key) = slot.take() {
                    lua.remove_registry_value(key).map_err(Error::lua)?;
                }
                Ok(())
            }
            Value::Function(func) => {
                self.ensure_lua_hook_context(lua)?;
                let Some(slot) = self.lua_hooks.slot_mut(name) else {
                    return Err(Error::UnknownHook(name.into()));
                };
                if let Some(key) = slot.take() {
                    lua.remove_registry_value(key).map_err(Error::lua)?;
                }
                let key = lua.create_registry_value(func).map_err(Error::lua)?;
                *slot = Some(key);
                Ok(())
            }
            _ => Err(Error::InvalidHookValue),
        }
    }

    pub fn get_hook(&self, lua: &Lua, name: &str) -> Result<Value> {
        let Some(slot) = self.lua_hooks.slot(name) else {
            return Err(Error::UnknownHook(name.into()));
        };
        let Some(key) = slot else {
            return Ok(Value::Nil);
        };
        self.ensure_lua_hook_context(lua)?;
        let func: Function = lua.registry_value(key).map_err(Error::lua)?;
        Ok(Value::Function(func))
    }

    pub fn hook_on_startup(&mut self, config_path: &str) -> Result<()> {
        let Some(key) = &self.lua_hooks.on_startup else {
            return Ok(());
        };
        let Some(lua) = self.lua_ctx.as_ref() else {
            return Ok(());
        };
        let tbl = lua.create_table().map_err(Error::lua)?;
        tbl.set("config_path", config_path).map_err(Error::lua)?;
        tbl.set("version", env!("CARGO_PKG_VERSION"))
            .map_err(Error::lua)?;
        tbl.set("pid", std::process::id()).map_err(Error::lua)?;
        let func: Function = lua.registry_value(key).map_err(Error::lua)?;
        func.call::<()>(tbl).map_err(Error::lua)
    }

    pub fn hook_on_shutdown(&mut self, reason: &str) -> Result<()> {
        let Some(key) = &self.lua_hooks.on_shutdown else {
            return Ok(());
        };
        let Some(lua) = self.lua_ctx.as_ref() else {
            return Ok(());
        };
        let func: Function = lua.registry_value(key).map_err(Error::lua)?;
        func.call::<()>(reason.to_string()).map_err(Error::lua)
    }

    pub fn hook_on_error(&mut self, message: &str, context: &str) -> Result<()> {
        let Some(key) = &self.lua_hooks.on_error else {
            return Ok(());
        };
        let Some(lua) = self.lua_ctx.as_ref() else {
            return Ok(());
        };
        let func: Function = lua.registry_value(key).map_err(Error::lua)?;
        func.call::<()>((message.to_string(), context.to_string()))
            .map_err(Error::lua)
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
        let tbl = lua.create_table().map_err(Error::lua)?;
        tbl.set("rows", rows).map_err(Error::lua)?;
        tbl.set("cols", cols).map_err(Error::lua)?;
        tbl.set("cursor_row", cursor_row).map_err(Error::lua)?;
        tbl.set("cursor_col", cursor_col).map_err(Error::lua)?;
        tbl.set("prev_cursor_row", prev_cursor_row)
            .map_err(Error::lua)?;
        tbl.set("prev_cursor_col", prev_cursor_col)
            .map_err(Error::lua)?;
        tbl.set("changed", changed).map_err(Error::lua)?;
        tbl.set("overlay", overlay_active).map_err(Error::lua)?;
        tbl.set("screen", view.contents_full())
            .map_err(Error::lua)?;
        tbl.set("prev_screen", view.prev_screen().contents_full())
            .map_err(Error::lua)?;
        let func: Function = lua.registry_value(key).map_err(Error::lua)?;
        func.call::<()>(tbl).map_err(Error::lua)
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
        let tbl = lua.create_table().map_err(Error::lua)?;
        tbl.set("row", new_pos.0).map_err(Error::lua)?;
        tbl.set("col", new_pos.1).map_err(Error::lua)?;
        tbl.set("prev_row", old_pos.0).map_err(Error::lua)?;
        tbl.set("prev_col", old_pos.1).map_err(Error::lua)?;
        let func: Function = lua.registry_value(key).map_err(Error::lua)?;
        func.call::<()>(tbl).map_err(Error::lua)
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
        let func: Function = lua.registry_value(key).map_err(Error::lua)?;
        func.call::<()>((old.as_str().to_string(), new.as_str().to_string()))
            .map_err(Error::lua)
    }

    pub(crate) fn hook_on_table_mode_enter(&mut self, table_state: &TableState) -> Result<()> {
        let Some(key) = &self.lua_hooks.on_table_mode_enter else {
            return Ok(());
        };
        let Some(lua) = self.lua_ctx.as_ref() else {
            return Ok(());
        };
        let model = table_state.model();
        let tbl = lua.create_table().map_err(Error::lua)?;
        tbl.set("top", model.top()).map_err(Error::lua)?;
        tbl.set("bottom", model.bottom()).map_err(Error::lua)?;
        tbl.set("columns", model.column_count())
            .map_err(Error::lua)?;
        if let Some(row) = model.header_row() {
            tbl.set("header_row", row).map_err(Error::lua)?;
        } else {
            tbl.set("header_row", Value::Nil).map_err(Error::lua)?;
        }
        tbl.set("current_col", table_state.current_col())
            .map_err(Error::lua)?;
        let func: Function = lua.registry_value(key).map_err(Error::lua)?;
        func.call::<()>(tbl).map_err(Error::lua)
    }

    pub fn hook_on_table_mode_exit(&mut self) -> Result<()> {
        let Some(key) = &self.lua_hooks.on_table_mode_exit else {
            return Ok(());
        };
        let Some(lua) = self.lua_ctx.as_ref() else {
            return Ok(());
        };
        let func: Function = lua.registry_value(key).map_err(Error::lua)?;
        func.call::<()>(()).map_err(Error::lua)
    }

    pub fn hook_on_clipboard_change(&self, op: &str, entry: Option<&str>) -> Result<()> {
        let Some(key) = &self.lua_hooks.on_clipboard_change else {
            return Ok(());
        };
        let Some(lua) = self.lua_ctx.as_ref() else {
            return Ok(());
        };
        let meta = lua.create_table().map_err(Error::lua)?;
        meta.set("op", op).map_err(Error::lua)?;
        meta.set("index", self.clipboard.index())
            .map_err(Error::lua)?;
        meta.set("size", self.clipboard.size())
            .map_err(Error::lua)?;
        let entry = match entry {
            Some(value) => Value::String(lua.create_string(value).map_err(Error::lua)?),
            None => Value::Nil,
        };
        let func: Function = lua.registry_value(key).map_err(Error::lua)?;
        func.call::<()>((entry, meta)).map_err(Error::lua)
    }

    pub fn hook_on_key_unhandled(&mut self, key: Option<&str>, mode: InputMode) -> Result<bool> {
        let Some(key_ref) = &self.lua_hooks.on_key_unhandled else {
            return Ok(false);
        };
        let Some(lua) = self.lua_ctx.as_ref() else {
            return Ok(false);
        };
        let func: Function = lua.registry_value(key_ref).map_err(Error::lua)?;
        let key_value = match key {
            Some(value) => Value::String(lua.create_string(value).map_err(Error::lua)?),
            None => Value::Nil,
        };
        let result: Value = func
            .call((key_value, mode.as_str().to_string()))
            .map_err(Error::lua)?;
        Ok(matches!(result, Value::Boolean(true)))
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
        let meta = lua.create_table().map_err(Error::lua)?;
        meta.set("cursor_moves", cursor_moves).map_err(Error::lua)?;
        meta.set("scrolled", scrolled).map_err(Error::lua)?;
        let func: Function = lua.registry_value(key).map_err(Error::lua)?;
        let result: Value = func.call((text.to_string(), meta)).map_err(Error::lua)?;
        match result {
            Value::Nil | Value::Boolean(false) => Ok(None),
            Value::String(value) => Ok(Some(value.to_str().map_err(Error::lua)?.to_string())),
            _ => Err(Error::InvalidLiveReadResult),
        }
    }

    pub(super) fn call_hook_on_speech_start(&mut self, text: &str, interrupt: bool) -> Result<()> {
        let Some(key) = &self.lua_hooks.on_speech_start else {
            return Ok(());
        };
        let Some(lua) = self.lua_ctx.as_ref() else {
            return Ok(());
        };
        let meta = lua.create_table().map_err(Error::lua)?;
        meta.set("interrupt", interrupt).map_err(Error::lua)?;
        let func: Function = lua.registry_value(key).map_err(Error::lua)?;
        func.call::<()>((text.to_string(), meta))
            .map_err(Error::lua)
    }

    pub(super) fn call_hook_on_speech_end(
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
        let meta = lua.create_table().map_err(Error::lua)?;
        meta.set("interrupt", interrupt).map_err(Error::lua)?;
        meta.set("ok", ok).map_err(Error::lua)?;
        let func: Function = lua.registry_value(key).map_err(Error::lua)?;
        func.call::<()>((text.to_string(), meta))
            .map_err(Error::lua)
    }

    fn ensure_lua_hook_context(&self, lua: &Lua) -> Result<()> {
        let Some(weak_ctx) = self.lua_ctx_weak.as_ref() else {
            return Err(Error::InvalidLuaHookContext);
        };
        if *weak_ctx != lua.weak() {
            return Err(Error::InvalidLuaHookContext);
        }
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct LuaHooks {
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
