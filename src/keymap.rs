use crate::commands::{self, Action};
use mlua::{Function, Lua, RegistryKey, Value};
use std::{collections::HashMap, rc::Rc};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("Lua: {0}")]
    Lua(String),
    #[error("binding action must start with \"{BUILTIN_PREFIX}\"")]
    InvalidBuiltinPrefix,
    #[error("unknown action {0}")]
    UnknownAction(String),
}

fn lua_error(error: mlua::Error) -> Error {
    Error::Lua(error.to_string())
}

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
    pub fn help_text(&self) -> &str {
        match self {
            Binding::Builtin(action) => action.help_text(),
            Binding::Lua(binding) => &binding.help,
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
        let func: Function = self.lua.registry_value(&self.func).map_err(lua_error)?;
        func.call::<()>(()).map_err(lua_error)
    }
}

const NORMAL_BINDINGS: &[(&str, Action)] = &[
    ("F1", Action::ToggleHelp),
    ("M-'", Action::ToggleAutoRead),
    ("M-\"", Action::ToggleReviewCursorFollowsScreenCursor),
    ("M-s", Action::ToggleSymbolLevel),
    ("M-r", Action::OpenReview),
    ("M-w", Action::SayOverlay),
    ("M-n", Action::PassNextKey),
    ("M-x", Action::StopSpeaking),
    ("M-u", Action::RevLinePrev),
    ("M-o", Action::RevLineNext),
    ("M-U", Action::RevLinePrevNonBlank),
    ("M-O", Action::RevLineNextNonBlank),
    ("M-i", Action::RevLineRead),
    ("M-m", Action::RevCharPrev),
    ("M-.", Action::RevCharNext),
    ("M-,", Action::RevCharRead),
    ("M-<", Action::RevCharReadPhonetic),
    ("M-j", Action::RevWordPrev),
    ("M-l", Action::RevWordNext),
    ("M-k", Action::RevWordRead),
    ("M-y", Action::RevTop),
    ("M-p", Action::RevBottom),
    ("M-h", Action::RevFirst),
    ("M-;", Action::RevLast),
    ("M-a", Action::RevReadAttributes),
    ("Backspace", Action::Backspace),
    ("C-h", Action::Backspace),
    ("Delete", Action::Delete),
    ("F12", Action::SayTime),
    ("M-L", Action::OpenLuaRepl),
    ("M-C", Action::OpenTmuxConnectionChooser),
    ("F5", Action::SetMark),
    ("F6", Action::Copy),
    ("F7", Action::Paste),
    ("M-c", Action::SayClipboard),
    ("M-[", Action::PreviousClipboard),
    ("M-]", Action::NextClipboard),
    ("M-t", Action::ToggleTableMode),
    ("M-{", Action::LeftClick),
    ("M-}", Action::RightClick),
    ("M-T", Action::StartTableSetupMode),
    ("M-g", Action::ToggleStopSpeechOnFocusLoss),
];

const TABLE_BINDINGS: &[(&str, Action)] = &[
    ("Esc", Action::ExitTableMode),
    ("M-u", Action::TableRowPrev),
    ("M-o", Action::TableRowNext),
    ("M-i", Action::TableCellRead),
    ("j", Action::TableRowNext),
    ("k", Action::TableRowPrev),
    ("g", Action::TableRowTop),
    ("G", Action::TableRowBottom),
    ("h", Action::TableColPrev),
    ("l", Action::TableColNext),
    ("^", Action::TableColFirst),
    ("$", Action::TableColLast),
    ("i", Action::TableCellRead),
    ("M-j", Action::TableWordPrev),
    ("M-l", Action::TableWordNext),
    ("M-k", Action::TableWordRead),
    ("M-m", Action::TableCharPrev),
    ("M-.", Action::TableCharNext),
    ("M-,", Action::TableCharRead),
    ("H", Action::TableHeaderRead),
    ("M-h", Action::ToggleTableHeaderRead),
    ("M-H", Action::ToggleTableHeaderRead),
];

const TABLE_SETUP_BINDINGS: &[(&str, Action)] = &[
    ("Esc", Action::CancelTableSetupMode),
    ("Enter", Action::CommitTableSetupMode),
    ("t", Action::ToggleTableSetupTabstop),
    ("h", Action::RevCharPrev),
    ("l", Action::RevCharNext),
    ("i", Action::RevCharRead),
    ("^", Action::RevFirst),
    ("$", Action::RevLast),
    ("w", Action::RevWordNext),
    ("b", Action::RevWordPrev),
];

pub struct KeyBindings {
    normal: HashMap<String, Binding>,
    table: HashMap<String, Binding>,
    table_setup: HashMap<String, Binding>,
}

impl KeyBindings {
    pub fn new() -> Self {
        Self {
            normal: Self::default_map(NORMAL_BINDINGS),
            table: Self::default_map(TABLE_BINDINGS),
            table_setup: Self::default_map(TABLE_SETUP_BINDINGS),
        }
    }

    pub fn binding_for_mode(&self, mode: InputMode, key: &str) -> Option<&Binding> {
        let binding = self.bindings(mode).get(key);
        if mode != InputMode::Normal {
            return binding.or_else(|| self.normal.get(key));
        }
        binding
    }

    pub fn set_builtin_binding_for_mode(&mut self, mode: InputMode, key: String, action: Action) {
        self.replace_binding(mode, key, Binding::Builtin(action));
    }

    pub fn set_lua_binding_for_mode(
        &mut self,
        mode: InputMode,
        key: String,
        help: String,
        lua: Rc<Lua>,
        func: Function,
    ) -> Result<()> {
        let func_key = lua.create_registry_value(func).map_err(lua_error)?;
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

    pub fn clear_binding_for_mode(&mut self, mode: InputMode, key: &str) {
        if let Some(binding) = self.bindings_mut(mode).remove(key) {
            binding.cleanup();
        }
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
            Binding::Builtin(action) => Ok(Value::String(lua.create_string(format!(
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
            return Err(Error::InvalidBuiltinPrefix);
        };
        commands::builtin_action_from_name(name).ok_or_else(|| Error::UnknownAction(value.into()))
    }

    pub fn split_mode_key<'a>(&self, key: &'a str) -> (InputMode, &'a str) {
        let mut parts = key.splitn(2, ':');
        let prefix = parts.next().unwrap_or("");
        let rest = parts.next();
        if let Some(mode) = InputMode::from_prefix(prefix)
            && let Some(rest) = rest
            && !rest.is_empty()
        {
            return (mode, rest);
        }
        (InputMode::Normal, key)
    }

    fn replace_binding(&mut self, mode: InputMode, key: String, binding: Binding) {
        if let Some(prev) = self.bindings_mut(mode).insert(key, binding) {
            prev.cleanup();
        }
    }

    fn default_map(defaults: &[(&str, Action)]) -> HashMap<String, Binding> {
        defaults
            .iter()
            .map(|(key, action)| ((*key).to_string(), Binding::Builtin(*action)))
            .collect()
    }

    fn bindings(&self, mode: InputMode) -> &HashMap<String, Binding> {
        match mode {
            InputMode::Normal => &self.normal,
            InputMode::Table => &self.table,
            InputMode::TableSetup => &self.table_setup,
        }
    }

    fn bindings_mut(&mut self, mode: InputMode) -> &mut HashMap<String, Binding> {
        match mode {
            InputMode::Normal => &mut self.normal,
            InputMode::Table => &mut self.table,
            InputMode::TableSetup => &mut self.table_setup,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Binding, Error, InputMode, KeyBindings};
    use crate::commands::Action;
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
            .set_lua_binding_for_mode(
                InputMode::Normal,
                "M-f".to_string(),
                "test".to_string(),
                lua.clone(),
                func,
            )
            .unwrap();

        let binding = bindings.binding_for_mode(InputMode::Normal, "M-f").unwrap();
        match binding {
            Binding::Lua(binding) => binding.call().unwrap(),
            Binding::Builtin(_) => panic!("expected lua binding"),
        }

        let count: i32 = lua.globals().get("count").unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn mode_bindings_fall_back_to_normal_bindings() {
        let bindings = KeyBindings::new();

        assert!(matches!(
            bindings.binding_for_mode(InputMode::Table, "F1"),
            Some(Binding::Builtin(Action::ToggleHelp))
        ));
        assert!(matches!(
            bindings.binding_for_mode(InputMode::Table, "j"),
            Some(Binding::Builtin(Action::TableRowNext))
        ));
    }

    #[test]
    fn default_review_and_overlay_bindings_follow_the_overlay_convention() {
        let bindings = KeyBindings::new();

        assert!(matches!(
            bindings.binding_for_mode(InputMode::Normal, "M-r"),
            Some(Binding::Builtin(Action::OpenReview))
        ));
        assert!(matches!(
            bindings.binding_for_mode(InputMode::Normal, "M-w"),
            Some(Binding::Builtin(Action::SayOverlay))
        ));
        for key in ["M-z", "M-PageUp", "M-PageDown", "M-Up", "M-Down"] {
            assert!(
                bindings.binding_for_mode(InputMode::Normal, key).is_none(),
                "key={key}"
            );
        }
    }

    #[test]
    fn mode_prefixes_are_only_split_when_valid() {
        let bindings = KeyBindings::new();

        assert_eq!(
            bindings.split_mode_key("table:M-j"),
            (InputMode::Table, "M-j")
        );
        assert_eq!(
            bindings.split_mode_key("table_setup:Enter"),
            (InputMode::TableSetup, "Enter")
        );
        assert_eq!(
            bindings.split_mode_key("custom:M-j"),
            (InputMode::Normal, "custom:M-j")
        );
        assert_eq!(
            bindings.split_mode_key("table:"),
            (InputMode::Normal, "table:")
        );
    }

    #[test]
    fn builtin_values_validate_prefix_and_action_name() {
        assert_eq!(
            KeyBindings::builtin_action_from_value("lector.toggle_help").unwrap(),
            Action::ToggleHelp
        );
        assert_eq!(
            KeyBindings::builtin_action_from_value("toggle_help")
                .unwrap_err()
                .to_string(),
            "binding action must start with \"lector.\""
        );
        assert_eq!(
            KeyBindings::builtin_action_from_value("lector.missing")
                .unwrap_err()
                .to_string(),
            "unknown action lector.missing"
        );
    }

    #[test]
    fn input_modes_round_trip_through_configuration_names() {
        for (name, mode) in [
            ("normal", InputMode::Normal),
            ("table", InputMode::Table),
            ("table_setup", InputMode::TableSetup),
        ] {
            assert_eq!(InputMode::from_prefix(name), Some(mode));
            assert_eq!(mode.as_str(), name);
        }
        assert_eq!(InputMode::from_prefix("TABLE"), None);
        assert_eq!(InputMode::from_prefix(""), None);
    }

    #[test]
    fn builtin_bindings_can_be_replaced_and_cleared_per_mode() {
        let mut bindings = KeyBindings::new();

        bindings.set_builtin_binding_for_mode(
            InputMode::Table,
            "j".to_string(),
            Action::TableRowTop,
        );
        assert!(matches!(
            bindings.binding_for_mode(InputMode::Table, "j"),
            Some(Binding::Builtin(Action::TableRowTop))
        ));
        assert_eq!(
            bindings
                .binding_for_mode(InputMode::Table, "j")
                .unwrap()
                .help_text(),
            Action::TableRowTop.help_text()
        );

        bindings.clear_binding_for_mode(InputMode::Table, "j");
        assert!(bindings.binding_for_mode(InputMode::Table, "j").is_none());
        bindings.clear_binding_for_mode(InputMode::Table, "missing");
    }

    #[test]
    fn connection_manager_is_reachable_without_stealing_application_control_backslash() {
        let bindings = KeyBindings::new();
        assert!(matches!(
            bindings.binding_for_mode(InputMode::Normal, "M-C"),
            Some(Binding::Builtin(Action::OpenTmuxConnectionChooser))
        ));
        assert!(
            bindings
                .binding_for_mode(InputMode::Normal, "C-\\")
                .is_none()
        );
        assert!(
            bindings
                .binding_for_mode(InputMode::Normal, "C-4")
                .is_none()
        );
    }

    #[test]
    fn lua_binding_values_support_introspection_with_or_without_functions() {
        let lua = Rc::new(Lua::new_with(StdLib::ALL_SAFE, LuaOptions::default()).unwrap());
        let func = lua.load("return function() return 42 end").eval().unwrap();
        let mut bindings = KeyBindings::new();
        bindings
            .set_lua_binding_for_mode(
                InputMode::Normal,
                "M-f".to_string(),
                "run test callback".to_string(),
                Rc::clone(&lua),
                func,
            )
            .unwrap();

        let builtin = bindings
            .binding_value_for_lua_mode(InputMode::Normal, "F1", &lua, true)
            .unwrap();
        assert_eq!(
            builtin.as_string().unwrap().to_str().unwrap(),
            "lector.toggle_help"
        );
        assert!(matches!(
            bindings
                .binding_value_for_lua_mode(InputMode::Normal, "missing", &lua, true)
                .unwrap(),
            mlua::Value::Nil
        ));

        let without_function = bindings
            .binding_value_for_lua_mode(InputMode::Normal, "M-f", &lua, false)
            .unwrap()
            .as_table()
            .unwrap()
            .clone();
        assert_eq!(
            without_function.get::<String>(1).unwrap(),
            "run test callback"
        );
        assert!(matches!(
            without_function.get::<mlua::Value>(2).unwrap(),
            mlua::Value::Nil
        ));

        let with_function = bindings
            .binding_value_for_lua_mode(InputMode::Normal, "M-f", &lua, true)
            .unwrap()
            .as_table()
            .unwrap()
            .clone();
        assert_eq!(
            with_function
                .get::<mlua::Function>(2)
                .unwrap()
                .call::<i32>(())
                .unwrap(),
            42
        );
    }

    #[test]
    fn lua_callback_failures_are_reported_as_keymap_errors() {
        let lua = Rc::new(Lua::new_with(StdLib::ALL_SAFE, LuaOptions::default()).unwrap());
        let func = lua
            .load("return function() error('expected callback failure') end")
            .eval::<mlua::Function>()
            .unwrap();
        let mut bindings = KeyBindings::new();
        bindings
            .set_lua_binding_for_mode(
                InputMode::Normal,
                "M-f".to_string(),
                "fail".to_string(),
                Rc::clone(&lua),
                func,
            )
            .unwrap();

        let Binding::Lua(binding) = bindings.binding_for_mode(InputMode::Normal, "M-f").unwrap()
        else {
            panic!("expected Lua binding");
        };
        let error = binding.call().unwrap_err();
        assert!(matches!(error, Error::Lua(_)));
        assert!(error.to_string().contains("expected callback failure"));
    }
}
