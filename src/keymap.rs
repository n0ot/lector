use crate::commands::{self, Action};
use anyhow::{Result, anyhow};
use mlua::{Function, Lua, RegistryKey, Value};
use std::{collections::HashMap, rc::Rc};

pub const BUILTIN_PREFIX: &str = "lector.";

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
        func.call::<()>(())
            .map_err(|err| anyhow!(err.to_string()))
    }
}

pub struct KeyBindings {
    bindings: HashMap<String, Binding>,
}

impl KeyBindings {
    pub fn new() -> Self {
        let mut bindings = HashMap::new();
        bindings.insert("F1".to_string(), Binding::Builtin(Action::ToggleHelp));
        bindings.insert(
            "M-'".to_string(),
            Binding::Builtin(Action::ToggleAutoRead),
        );
        bindings.insert(
            "M-\"".to_string(),
            Binding::Builtin(Action::ToggleReviewCursorFollowsScreenCursor),
        );
        bindings.insert(
            "M-s".to_string(),
            Binding::Builtin(Action::ToggleSymbolLevel),
        );
        bindings.insert("M-n".to_string(), Binding::Builtin(Action::PassNextKey));
        bindings.insert("M-x".to_string(), Binding::Builtin(Action::StopSpeaking));
        bindings.insert("M-u".to_string(), Binding::Builtin(Action::RevLinePrev));
        bindings.insert("M-o".to_string(), Binding::Builtin(Action::RevLineNext));
        bindings.insert(
            "M-U".to_string(),
            Binding::Builtin(Action::RevLinePrevNonBlank),
        );
        bindings.insert(
            "M-O".to_string(),
            Binding::Builtin(Action::RevLineNextNonBlank),
        );
        bindings.insert("M-i".to_string(), Binding::Builtin(Action::RevLineRead));
        bindings.insert("M-m".to_string(), Binding::Builtin(Action::RevCharPrev));
        bindings.insert("M-.".to_string(), Binding::Builtin(Action::RevCharNext));
        bindings.insert("M-,".to_string(), Binding::Builtin(Action::RevCharRead));
        bindings.insert(
            "M-<".to_string(),
            Binding::Builtin(Action::RevCharReadPhonetic),
        );
        bindings.insert("M-j".to_string(), Binding::Builtin(Action::RevWordPrev));
        bindings.insert("M-l".to_string(), Binding::Builtin(Action::RevWordNext));
        bindings.insert("M-k".to_string(), Binding::Builtin(Action::RevWordRead));
        bindings.insert("M-y".to_string(), Binding::Builtin(Action::RevTop));
        bindings.insert("M-p".to_string(), Binding::Builtin(Action::RevBottom));
        bindings.insert("M-h".to_string(), Binding::Builtin(Action::RevFirst));
        bindings.insert("M-;".to_string(), Binding::Builtin(Action::RevLast));
        bindings.insert(
            "M-a".to_string(),
            Binding::Builtin(Action::RevReadAttributes),
        );
        bindings.insert("Backspace".to_string(), Binding::Builtin(Action::Backspace));
        bindings.insert("C-h".to_string(), Binding::Builtin(Action::Backspace));
        bindings.insert("Delete".to_string(), Binding::Builtin(Action::Delete));
        bindings.insert("F12".to_string(), Binding::Builtin(Action::SayTime));
        bindings.insert("M-L".to_string(), Binding::Builtin(Action::OpenLuaRepl));
        bindings.insert("F5".to_string(), Binding::Builtin(Action::SetMark));
        bindings.insert("F6".to_string(), Binding::Builtin(Action::Copy));
        bindings.insert("F7".to_string(), Binding::Builtin(Action::Paste));
        bindings.insert("M-c".to_string(), Binding::Builtin(Action::SayClipboard));
        bindings.insert(
            "M-[".to_string(),
            Binding::Builtin(Action::PreviousClipboard),
        );
        bindings.insert(
            "M-]".to_string(),
            Binding::Builtin(Action::NextClipboard),
        );
        Self { bindings }
    }

    pub fn binding_for(&self, key: &str) -> Option<&Binding> {
        self.bindings.get(key)
    }

    pub fn set_builtin_binding(&mut self, key: String, action: Action) {
        self.replace_binding(key, Binding::Builtin(action));
    }

    pub fn set_lua_binding(
        &mut self,
        key: String,
        help: String,
        lua: Rc<Lua>,
        func: Function,
    ) -> Result<()> {
        let func_key = lua
            .create_registry_value(func)
            .map_err(|err| anyhow!(err.to_string()))?;
        self.replace_binding(
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
        if let Some(binding) = self.bindings.remove(key) {
            binding.cleanup();
        }
    }

    pub fn binding_value_for_lua(
        &self,
        key: &str,
        lua: &Lua,
        allow_function: bool,
    ) -> mlua::Result<Value> {
        let Some(binding) = self.bindings.get(key) else {
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

    fn replace_binding(&mut self, key: String, binding: Binding) {
        if let Some(prev) = self.bindings.insert(key, binding) {
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
