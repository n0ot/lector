use super::ext::LuaResultExt;
use crate::{keymap::KeyBindings, screen_reader::ScreenReader, speech::symbols};
use anyhow::{Context as AnyhowContext, anyhow};
use mlua::{Error, Function, IntoLua, Lua, Result, Table, Value};
use std::{cell::RefCell, rc::Rc};

macro_rules! add_callbacks_common {
    ($tbl:expr,
        set_option = $set_option:expr,
        get_option = $get_option:expr,
        set_symbol = $set_symbol:expr,
        set_binding = $set_binding:expr,
        get_binding = $get_binding:expr,
        get_symbol = $get_symbol:expr,
        clear_symbols = $clear_symbols:expr,
        set_hook = $set_hook:expr,
        get_hook = $get_hook:expr $(,)?
    ) => {{
        $tbl.set("set_option", $set_option)?;
        $tbl.set("get_option", $get_option)?;
        $tbl.set("set_symbol", $set_symbol)?;
        $tbl.set("set_binding", $set_binding)?;
        $tbl.set("get_binding", $get_binding)?;
        $tbl.set("get_symbol", $get_symbol)?;
        $tbl.set("clear_symbols", $clear_symbols)?;
        $tbl.set("set_hook", $set_hook)?;
        $tbl.set("get_hook", $get_hook)?;
        Ok(())
    }};
}

pub fn setup_static(lua: &Lua, sr_ptr: Rc<RefCell<*mut ScreenReader>>) -> Result<()> {
    let tbl_callbacks = lua.create_table()?;
    add_callbacks_static(lua, &tbl_callbacks, sr_ptr)?;
    lua.load(include_str!("meta.lua"))
        .set_name("meta.lua")
        .call::<()>((tbl_callbacks,))?;
    Ok(())
}

fn add_callbacks_static(
    lua: &Lua,
    tbl_callbacks: &Table,
    sr_ptr: Rc<RefCell<*mut ScreenReader>>,
) -> Result<()> {
    let set_option = lua.create_function_mut({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |_, (key, value): (String, mlua::Value)| {
            with_screen_reader_mut(&sr_ptr, |sr| {
                set_option(sr, &key, value).map_err(Error::external)
            })
        }
    })?;
    let get_option = lua.create_function({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |lua, key: String| {
            with_screen_reader(&sr_ptr, |sr| {
                get_option(lua, sr, &key).map_err(Error::external)
            })
        }
    })?;
    let set_symbol = lua.create_function_mut({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |_, (key, value): (String, mlua::Value)| {
            with_screen_reader_mut(&sr_ptr, |sr| match value {
                mlua::Value::Nil => {
                    sr.speech_mut().remove_symbol(&key);
                    Ok(())
                }
                mlua::Value::Table(table_value) => {
                    let replacement: String = table_value.get(1)?;
                    let level: symbols::Level = AnyhowContext::context(
                        table_value.get::<String>(2)?.parse(),
                        "parse level",
                    )
                    .to_lua_result()?;
                    let include_original: symbols::IncludeOriginal = AnyhowContext::context(
                        table_value.get::<String>(3)?.parse(),
                        "parse include_original",
                    )
                    .to_lua_result()?;
                    let repeat: bool = table_value.get(4)?;
                    sr.speech_mut()
                        .set_symbol(&key, &replacement, level, include_original, repeat);
                    Ok(())
                }
                _ => Err(Error::external(anyhow!(
                    "symbol value must be a table or nil"
                ))),
            })
        }
    })?;
    let set_binding = lua.create_function_mut({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |lua, (key, value): (String, mlua::Value)| {
            with_screen_reader_mut(&sr_ptr, |sr| {
                set_binding(lua, sr, &key, value).map_err(Error::external)
            })
        }
    })?;
    let get_binding = lua.create_function({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |lua, key: String| {
            with_screen_reader(&sr_ptr, |sr| {
                get_binding(lua, sr, &key).map_err(Error::external)
            })
        }
    })?;
    let get_symbol = lua.create_function({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |lua, key: String| {
            with_screen_reader(&sr_ptr, |sr| {
                let value = match sr.speech().symbol(&key) {
                    Some(v) => {
                        let tbl = lua.create_table()?;
                        tbl.set(1, v.replacement.clone())?;
                        tbl.set(2, v.level.to_string())?;
                        tbl.set(3, v.include_original.to_string())?;
                        tbl.set(4, v.repeat)?;
                        Value::Table(tbl)
                    }
                    None => Value::Nil,
                };
                Ok(value)
            })
        }
    })?;
    let clear_symbols = lua.create_function_mut({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |_, ()| {
            with_screen_reader_mut(&sr_ptr, |sr| {
                sr.speech_mut().clear_symbols();
                Ok(())
            })
        }
    })?;
    let set_hook = lua.create_function_mut({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |lua, (key, value): (String, Value)| {
            with_screen_reader_mut(&sr_ptr, |sr| {
                sr.set_hook(lua, &key, value).map_err(Error::external)
            })
        }
    })?;
    let get_hook = lua.create_function({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |lua, key: String| {
            with_screen_reader(&sr_ptr, |sr| {
                sr.get_hook(lua, &key).map_err(Error::external)
            })
        }
    })?;

    add_callbacks_common!(
        tbl_callbacks,
        set_option = set_option,
        get_option = get_option,
        set_symbol = set_symbol,
        set_binding = set_binding,
        get_binding = get_binding,
        get_symbol = get_symbol,
        clear_symbols = clear_symbols,
        set_hook = set_hook,
        get_hook = get_hook,
    )
}

fn get_option(lua: &Lua, sr: &ScreenReader, option: &str) -> anyhow::Result<mlua::Value> {
    match option {
        "speech_rate" => sr.speech().get_rate().into_lua(lua),
        "symbol_level" => sr.speech().symbol_level().to_string().into_lua(lua),
        "help_mode" => sr.help_mode().into_lua(lua),
        "auto_read" => sr.auto_read_enabled().into_lua(lua),
        "suppress_key_echo" => sr.suppress_key_echo().into_lua(lua),
        "review_follows_screen_cursor" | "rev_follows" => {
            sr.review_follows_screen_cursor().into_lua(lua)
        }
        "highlight_tracking" => sr.highlight_tracking_enabled().into_lua(lua),
        "stop_speech_on_focus_loss" => sr.stop_speech_on_focus_loss().into_lua(lua),
        "tmux_bells" => sr.tmux_bell_mode().to_string().into_lua(lua),
        _ => Err(Error::external(anyhow!("unknown option"))),
    }
    .map_err(|e| anyhow!("{}", e))
    .context(format!("get option: {}", option))
}

fn set_binding(lua: &Lua, sr: &mut ScreenReader, key: &str, value: Value) -> anyhow::Result<()> {
    let (mode, key) = sr.key_bindings().split_mode_key(key);
    match value {
        Value::Nil => {
            sr.key_bindings_mut().clear_binding_for_mode(mode, key);
            Ok(())
        }
        Value::String(name) => {
            let name = name.to_str().map_err(|err| anyhow!(err.to_string()))?;
            let action = KeyBindings::builtin_action_from_value(name.as_ref())?;
            sr.key_bindings_mut()
                .set_builtin_binding_for_mode(mode, key.to_string(), action);
            Ok(())
        }
        Value::Table(table) => {
            let (help, func) = parse_binding_table(table)?;
            let ctx = Rc::clone(sr.lua_binding_context(lua).map_err(anyhow::Error::new)?);
            sr.key_bindings_mut().set_lua_binding_for_mode(
                mode,
                key.to_string(),
                help,
                ctx,
                func,
            )?;
            Ok(())
        }
        _ => Err(anyhow!("binding value must be a string, table, or nil")),
    }
}

fn parse_binding_table(table: Table) -> anyhow::Result<(String, Function)> {
    let help = match table.get::<String>("help") {
        Ok(help) => help,
        Err(_) => table.get(1).map_err(|err| anyhow!(err.to_string()))?,
    };
    let func = match table.get::<Function>("fn") {
        Ok(func) => func,
        Err(_) => table.get(2).map_err(|err| anyhow!(err.to_string()))?,
    };
    Ok((help, func))
}

fn get_binding(lua: &Lua, sr: &ScreenReader, key: &str) -> anyhow::Result<Value> {
    let allow_function = sr.owns_lua_context(lua);
    let (mode, key) = sr.key_bindings().split_mode_key(key);
    sr.key_bindings()
        .binding_value_for_lua_mode(mode, key, lua, allow_function)
        .map_err(|err| anyhow!(err.to_string()))
}

fn set_option(sr: &mut ScreenReader, option: &str, value: mlua::Value) -> anyhow::Result<()> {
    use mlua::Value::*;
    (match option {
        "speech_rate" => match value {
            Number(v) => sr
                .speech_mut()
                .set_rate(v as f32)
                .map_err(anyhow::Error::new),
            Integer(v) => sr
                .speech_mut()
                .set_rate(v as f32)
                .map_err(anyhow::Error::new),
            _ => Err(anyhow!("value must be a number")),
        },
        "symbol_level" => match value {
            String(v) => {
                let level = v
                    .to_str()
                    .map_err(|e| anyhow!("{}", e))?
                    .parse::<symbols::Level>()?;
                sr.speech_mut().set_symbol_level(level);
                Ok(())
            }
            _ => Err(anyhow!("value must be a string")),
        },
        "help_mode" => match value {
            Boolean(v) => {
                sr.set_help_mode(v);
                Ok(())
            }
            _ => Err(anyhow!("value must be a boolean")),
        },
        "auto_read" => match value {
            Boolean(v) => {
                sr.set_auto_read_enabled(v);
                Ok(())
            }
            _ => Err(anyhow!("value must be a boolean")),
        },
        "suppress_key_echo" => match value {
            Boolean(v) => {
                sr.set_suppress_key_echo(v);
                Ok(())
            }
            _ => Err(anyhow!("value must be a boolean")),
        },
        "review_follows_screen_cursor" | "rev_follows" => match value {
            Boolean(v) => {
                sr.set_review_follows_screen_cursor(v);
                Ok(())
            }
            _ => Err(anyhow!("value must be a boolean")),
        },
        "highlight_tracking" => match value {
            Boolean(v) => {
                sr.set_highlight_tracking_enabled(v);
                Ok(())
            }
            _ => Err(anyhow!("value must be a boolean")),
        },
        "stop_speech_on_focus_loss" => match value {
            Boolean(v) => {
                sr.set_stop_speech_on_focus_loss(v);
                Ok(())
            }
            _ => Err(anyhow!("value must be a boolean")),
        },
        "tmux_bells" => match value {
            String(v) => {
                let mode = v
                    .to_str()
                    .map_err(|e| anyhow!(e.to_string()))?
                    .parse::<crate::screen_reader::TmuxBellMode>()?;
                sr.set_tmux_bell_mode(mode);
                Ok(())
            }
            _ => Err(anyhow!("value must be a string")),
        },
        _ => Err(anyhow!("unknown option")),
    })
    .map_err(|e| anyhow!("set option: {}: {:?}", option, e))
}

fn with_screen_reader_mut<T>(
    sr_ptr: &Rc<RefCell<*mut ScreenReader>>,
    f: impl FnOnce(&mut ScreenReader) -> Result<T>,
) -> Result<T> {
    let ptr = *sr_ptr.borrow();
    if ptr.is_null() {
        return Err(Error::external(anyhow!("screen reader unavailable")));
    }
    // Safety: the pointer is set by the main thread before any Lua call.
    unsafe { f(&mut *ptr) }
}

fn with_screen_reader<T>(
    sr_ptr: &Rc<RefCell<*mut ScreenReader>>,
    f: impl FnOnce(&ScreenReader) -> Result<T>,
) -> Result<T> {
    let ptr = *sr_ptr.borrow();
    if ptr.is_null() {
        return Err(Error::external(anyhow!("screen reader unavailable")));
    }
    // Safety: the pointer is set by the main thread before any Lua call.
    unsafe { f(&*ptr) }
}
