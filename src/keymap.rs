use crate::commands::{self, Action};
use anyhow::{Result, anyhow};
use mlua::{Function, Lua, RegistryKey, Value};
use std::{collections::HashMap, rc::Rc};

pub const BUILTIN_PREFIX: &str = "lector.";

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum InputMode {
    Normal,
    Table,
    TableSetup,
}

impl InputMode {
    pub fn from_prefix(prefix: &str) -> Option<Self> {
        match prefix {
            "normal" => Some(InputMode::Normal),
            "table" => Some(InputMode::Table),
            "table_setup" => Some(InputMode::TableSetup),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            InputMode::Normal => "normal",
            InputMode::Table => "table",
            InputMode::TableSetup => "table_setup",
        }
    }
}

#[derive(Debug)]
pub enum Binding {
    Builtin(Action),
    Lua(LuaBinding),
}

impl Binding {
    pub fn help_text(&self) -> String {
        match self {
            Binding::Builtin(action) => action.help_text(),
            Binding::Lua(binding) => binding.help.clone(),
        }
    }

    fn cleanup(self) {
        if let Binding::Lua(binding) = self {
            let _ = binding.lua.remove_registry_value(binding.func);
        }
    }
}

#[derive(Debug)]
pub struct LuaBinding {
    pub help: String,
    pub lua: Rc<Lua>,
    pub func: RegistryKey,
}

impl LuaBinding {
    pub fn call(&self) -> Result<()> {
        let func: Function = self
            .lua
            .registry_value(&self.func)
            .map_err(|err| anyhow!(err.to_string()))?;
        func.call::<()>(()).map_err(|err| anyhow!(err.to_string()))
    }
}

pub struct KeyBindings {
    bindings: HashMap<InputMode, HashMap<String, Binding>>,
}

impl KeyBindings {
    pub fn new() -> Self {
        let mut bindings = HashMap::new();
        bindings.insert(InputMode::Normal, HashMap::new());
        bindings.insert(InputMode::Table, HashMap::new());
        bindings.insert(InputMode::TableSetup, HashMap::new());

        let normal = bindings.get_mut(&InputMode::Normal).unwrap();
        normal.insert("F1".to_string(), Binding::Builtin(Action::ToggleHelp));
        normal.insert("M-'".to_string(), Binding::Builtin(Action::ToggleAutoRead));
        normal.insert(
            "M-\"".to_string(),
            Binding::Builtin(Action::ToggleReviewCursorFollowsScreenCursor),
        );
        normal.insert(
            "M-s".to_string(),
            Binding::Builtin(Action::ToggleSymbolLevel),
        );
        normal.insert("M-n".to_string(), Binding::Builtin(Action::PassNextKey));
        normal.insert("M-x".to_string(), Binding::Builtin(Action::StopSpeaking));
        normal.insert("M-u".to_string(), Binding::Builtin(Action::RevLinePrev));
        normal.insert("M-o".to_string(), Binding::Builtin(Action::RevLineNext));
        normal.insert(
            "M-U".to_string(),
            Binding::Builtin(Action::RevLinePrevNonBlank),
        );
        normal.insert(
            "M-O".to_string(),
            Binding::Builtin(Action::RevLineNextNonBlank),
        );
        normal.insert("M-i".to_string(), Binding::Builtin(Action::RevLineRead));
        normal.insert("M-m".to_string(), Binding::Builtin(Action::RevCharPrev));
        normal.insert("M-.".to_string(), Binding::Builtin(Action::RevCharNext));
        normal.insert("M-,".to_string(), Binding::Builtin(Action::RevCharRead));
        normal.insert(
            "M-<".to_string(),
            Binding::Builtin(Action::RevCharReadPhonetic),
        );
        normal.insert("M-j".to_string(), Binding::Builtin(Action::RevWordPrev));
        normal.insert("M-l".to_string(), Binding::Builtin(Action::RevWordNext));
        normal.insert("M-k".to_string(), Binding::Builtin(Action::RevWordRead));
        normal.insert("M-y".to_string(), Binding::Builtin(Action::RevTop));
        normal.insert("M-p".to_string(), Binding::Builtin(Action::RevBottom));
        normal.insert("M-h".to_string(), Binding::Builtin(Action::RevFirst));
        normal.insert("M-;".to_string(), Binding::Builtin(Action::RevLast));
        normal.insert(
            "M-a".to_string(),
            Binding::Builtin(Action::RevReadAttributes),
        );
        normal.insert("Backspace".to_string(), Binding::Builtin(Action::Backspace));
        normal.insert("C-h".to_string(), Binding::Builtin(Action::Backspace));
        normal.insert("Delete".to_string(), Binding::Builtin(Action::Delete));
        normal.insert("F12".to_string(), Binding::Builtin(Action::SayTime));
        normal.insert("M-L".to_string(), Binding::Builtin(Action::OpenLuaRepl));
        normal.insert("F5".to_string(), Binding::Builtin(Action::SetMark));
        normal.insert("F6".to_string(), Binding::Builtin(Action::Copy));
        normal.insert("F7".to_string(), Binding::Builtin(Action::Paste));
        normal.insert("M-c".to_string(), Binding::Builtin(Action::SayClipboard));
        normal.insert(
            "M-[".to_string(),
            Binding::Builtin(Action::PreviousClipboard),
        );
        normal.insert("M-]".to_string(), Binding::Builtin(Action::NextClipboard));
        normal.insert("M-t".to_string(), Binding::Builtin(Action::ToggleTableMode));
        normal.insert(
            "M-T".to_string(),
            Binding::Builtin(Action::StartTableSetupMode),
        );
        normal.insert(
            "M-g".to_string(),
            Binding::Builtin(Action::ToggleStopSpeechOnFocusLoss),
        );

        let table = bindings.get_mut(&InputMode::Table).unwrap();
        table.insert("Esc".to_string(), Binding::Builtin(Action::ExitTableMode));
        table.insert("M-i".to_string(), Binding::Builtin(Action::TableCellRead));
        table.insert("j".to_string(), Binding::Builtin(Action::TableRowNext));
        table.insert("k".to_string(), Binding::Builtin(Action::TableRowPrev));
        table.insert("h".to_string(), Binding::Builtin(Action::TableColPrev));
        table.insert("l".to_string(), Binding::Builtin(Action::TableColNext));
        table.insert("i".to_string(), Binding::Builtin(Action::TableCellRead));
        table.insert("H".to_string(), Binding::Builtin(Action::TableHeaderRead));
        table.insert(
            "M-h".to_string(),
            Binding::Builtin(Action::ToggleTableHeaderRead),
        );
        table.insert(
            "M-H".to_string(),
            Binding::Builtin(Action::ToggleTableHeaderRead),
        );

        let table_setup = bindings.get_mut(&InputMode::TableSetup).unwrap();
        table_setup.insert(
            "Esc".to_string(),
            Binding::Builtin(Action::CancelTableSetupMode),
        );
        table_setup.insert(
            "Enter".to_string(),
            Binding::Builtin(Action::CommitTableSetupMode),
        );
        table_setup.insert(
            "t".to_string(),
            Binding::Builtin(Action::ToggleTableSetupTabstop),
        );
        table_setup.insert("h".to_string(), Binding::Builtin(Action::RevCharPrev));
        table_setup.insert("l".to_string(), Binding::Builtin(Action::RevCharNext));
        table_setup.insert("i".to_string(), Binding::Builtin(Action::RevCharRead));

        Self { bindings }
    }

    pub fn binding_for(&self, key: &str) -> Option<&Binding> {
        self.binding_for_mode(InputMode::Normal, key)
    }

    pub fn binding_for_mode(&self, mode: InputMode, key: &str) -> Option<&Binding> {
        if let Some(bindings) = self.bindings.get(&mode) {
            if let Some(binding) = bindings.get(key) {
                return Some(binding);
            }
        }
        if mode != InputMode::Normal {
            return self
                .bindings
                .get(&InputMode::Normal)
                .and_then(|bindings| bindings.get(key));
        }
        None
    }

    pub fn set_builtin_binding(&mut self, key: String, action: Action) {
        self.set_builtin_binding_for_mode(InputMode::Normal, key, action);
    }

    pub fn set_builtin_binding_for_mode(&mut self, mode: InputMode, key: String, action: Action) {
        self.replace_binding(mode, key, Binding::Builtin(action));
    }

    pub fn set_lua_binding(
        &mut self,
        key: String,
        help: String,
        lua: Rc<Lua>,
        func: Function,
    ) -> Result<()> {
        self.set_lua_binding_for_mode(InputMode::Normal, key, help, lua, func)
    }

    pub fn set_lua_binding_for_mode(
        &mut self,
        mode: InputMode,
        key: String,
        help: String,
        lua: Rc<Lua>,
        func: Function,
    ) -> Result<()> {
        let func_key = lua
            .create_registry_value(func)
            .map_err(|err| anyhow!(err.to_string()))?;
        self.replace_binding(
            mode,
            key,
            Binding::Lua(LuaBinding {
                help,
                lua,
                func: func_key,
            }),
        );
        Ok(())
    }

    pub fn clear_binding(&mut self, key: &str) {
        self.clear_binding_for_mode(InputMode::Normal, key);
    }

    pub fn clear_binding_for_mode(&mut self, mode: InputMode, key: &str) {
        if let Some(bindings) = self.bindings.get_mut(&mode) {
            if let Some(binding) = bindings.remove(key) {
                binding.cleanup();
            }
        }
    }

    pub fn binding_value_for_lua(
        &self,
        key: &str,
        lua: &Lua,
        allow_function: bool,
    ) -> mlua::Result<Value> {
        self.binding_value_for_lua_mode(InputMode::Normal, key, lua, allow_function)
    }

    pub fn binding_value_for_lua_mode(
        &self,
        mode: InputMode,
        key: &str,
        lua: &Lua,
        allow_function: bool,
    ) -> mlua::Result<Value> {
        let Some(binding) = self.binding_for_mode(mode, key) else {
            return Ok(Value::Nil);
        };

        match binding {
            Binding::Builtin(action) => Ok(Value::String(lua.create_string(&format!(
                "{}{}",
                BUILTIN_PREFIX,
                commands::builtin_action_name(*action)
            ))?)),
            Binding::Lua(binding) => {
                let tbl = lua.create_table()?;
                tbl.set(1, binding.help.as_str())?;
                if allow_function {
                    let func: Function = binding.lua.registry_value(&binding.func)?;
                    tbl.set(2, func)?;
                } else {
                    tbl.set(2, Value::Nil)?;
                }
                Ok(Value::Table(tbl))
            }
        }
    }

    pub fn builtin_action_from_value(value: &str) -> Result<Action> {
        let Some(name) = value.strip_prefix(BUILTIN_PREFIX) else {
            return Err(anyhow!(
                "binding action must start with \"{}\"",
                BUILTIN_PREFIX
            ));
        };
        commands::builtin_action_from_name(name).ok_or_else(|| anyhow!("unknown action {}", value))
    }

    pub fn split_mode_key<'a>(&self, key: &'a str) -> (InputMode, &'a str) {
        let mut parts = key.splitn(2, ':');
        let prefix = parts.next().unwrap_or("");
        let rest = parts.next();
        if let Some(mode) = InputMode::from_prefix(prefix) {
            if let Some(rest) = rest {
                if !rest.is_empty() {
                    return (mode, rest);
                }
            }
        }
        (InputMode::Normal, key)
    }

    fn replace_binding(&mut self, mode: InputMode, key: String, binding: Binding) {
        let bindings = self.bindings.get_mut(&mode).expect("missing bindings map");
        if let Some(prev) = bindings.insert(key, binding) {
            prev.cleanup();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Binding, KeyBindings};
    use mlua::{Lua, LuaOptions, StdLib};
    use std::rc::Rc;

    #[test]
    fn lua_binding_executes() {
        let lua = Rc::new(Lua::new_with(StdLib::ALL_SAFE, LuaOptions::default()).unwrap());
        lua.globals().set("count", 0).unwrap();
        let func = lua
            .load("return function() count = count + 1 end")
            .eval::<mlua::Function>()
            .unwrap();

        let mut bindings = KeyBindings::new();
        bindings
            .set_lua_binding("M-f".to_string(), "test".to_string(), lua.clone(), func)
            .unwrap();

        let binding = bindings.binding_for("M-f").unwrap();
        match binding {
            Binding::Lua(binding) => binding.call().unwrap(),
            Binding::Builtin(_) => panic!("expected lua binding"),
        }

        let count: i32 = lua.globals().get("count").unwrap();
        assert_eq!(count, 1);
    }
}
