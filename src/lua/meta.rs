use super::ext::LuaResultExt;
use crate::{keymap::KeyBindings, screen_reader::ScreenReader, speech::symbols};
use anyhow::{Context as AnyhowContext, anyhow};
use mlua::{Error, Function, IntoLua, Lua, Result, Scope, Table, Value};
use std::{cell::RefCell, rc::Rc};

macro_rules! add_callbacks_common {
    ($tbl:expr,
        set_option = $set_option:expr,
        get_option = $get_option:expr,
        set_symbol = $set_symbol:expr,
        set_binding = $set_binding:expr,
        get_binding = $get_binding:expr,
        get_symbol = $get_symbol:expr,
        clear_symbols = $clear_symbols:expr $(,)?
    ) => {{
        $tbl.set("set_option", $set_option)?;
        $tbl.set("get_option", $get_option)?;
        $tbl.set("set_symbol", $set_symbol)?;
        $tbl.set("set_binding", $set_binding)?;
        $tbl.set("get_binding", $get_binding)?;
        $tbl.set("get_symbol", $get_symbol)?;
        $tbl.set("clear_symbols", $clear_symbols)?;
        Ok(())
    }};
}

#[allow(dead_code)]
pub fn setup<'lua, 'scope>(
    lua: &Lua,
    scope: &'lua Scope<'lua, 'scope>,
    sr: &'scope RefCell<&mut ScreenReader>,
) -> Result<()> {
    let tbl_callbacks = lua.create_table()?;
    add_callbacks(&tbl_callbacks, &scope, &sr)?;
    lua.load(include_str!("meta.lua"))
        .set_name("meta.lua")
        .call::<()>( (tbl_callbacks,) )?;

    Ok(())
}

pub fn setup_static(lua: &Lua, sr_ptr: Rc<RefCell<*mut ScreenReader>>) -> Result<()> {
    let tbl_callbacks = lua.create_table()?;
    add_callbacks_static(lua, &tbl_callbacks, sr_ptr)?;
    lua.load(include_str!("meta.lua"))
        .set_name("meta.lua")
        .call::<()>( (tbl_callbacks,) )?;
    Ok(())
}

#[allow(dead_code)]
fn add_callbacks<'lua, 'scope>(
    tbl_callbacks: &Table,
    scope: &'lua Scope<'lua, 'scope>,
    screen_reader: &'scope RefCell<&mut ScreenReader>,
) -> Result<()> {
    let set_option = scope.create_function_mut(|_, (key, value): (String, mlua::Value)| {
        let mut sr = screen_reader.borrow_mut();
        set_option(&mut sr, &key, value).to_lua_result()
    })?;
    let get_option = scope.create_function(|lua, key: String| {
        let sr = screen_reader.borrow();
        get_option(lua, &sr, &key).to_lua_result()
    })?;
    let set_symbol = scope.create_function_mut(|_, (key, value): (String, mlua::Value)| {
        let mut sr = screen_reader.borrow_mut();
        match value {
            mlua::Value::Nil => {
                sr.speech.symbols_map.remove(&key);
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
                sr.speech
                    .symbols_map
                    .put(&key, &replacement, level, include_original, repeat);
                Ok(())
            }
            _ => Err(Error::external(anyhow!(
                "symbol value must be a table or nil"
            ))),
        }
    })?;
    let set_binding = scope.create_function_mut(|lua, (key, value): (String, mlua::Value)| {
        let mut sr = screen_reader.borrow_mut();
        set_binding(lua, &mut sr, &key, value).to_lua_result()
    })?;
    let get_binding = scope.create_function(|lua, key: String| {
        let sr = screen_reader.borrow();
        get_binding(lua, &sr, &key).to_lua_result()
    })?;
    let get_symbol = scope.create_function(|ctx, key: String| {
        let sr = screen_reader.borrow();
        match sr.speech.symbols_map.get(&key) {
            Some(v) => {
                let tbl = ctx.create_table()?;
                tbl.set(1, v.replacement.clone())?;
                tbl.set(2, v.level.to_string())?;
                tbl.set(3, v.include_original.to_string())?;
                tbl.set(4, v.repeat)?;
                Ok(Value::Table(tbl))
            }
            None => Ok(Value::Nil),
        }
    })?;
    let clear_symbols = scope.create_function_mut(|_, ()| {
        let mut sr = screen_reader.borrow_mut();
        sr.speech.symbols_map.clear();
        Ok(())
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
    )
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
            with_screen_reader_mut(&sr_ptr, |sr| {
                match value {
                    mlua::Value::Nil => {
                        sr.speech.symbols_map.remove(&key);
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
                        sr.speech
                            .symbols_map
                            .put(&key, &replacement, level, include_original, repeat);
                        Ok(())
                    }
                    _ => Err(Error::external(anyhow!(
                        "symbol value must be a table or nil"
                    ))),
                }
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
                let value = match sr.speech.symbols_map.get(&key) {
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
                sr.speech.symbols_map.clear();
                Ok(())
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
    )
}

fn get_option<'lua>(
    lua: &'lua Lua,
    sr: &ScreenReader,
    option: &str,
) -> anyhow::Result<mlua::Value> {
    match option {
        "speech_rate" => sr.speech.get_rate().into_lua(lua),
        "symbol_level" => sr.speech.symbol_level.to_string().into_lua(lua),
        "help_mode" => sr.help_mode.into_lua(lua),
        "auto_read" => sr.auto_read.into_lua(lua),
        "review_follows_screen_cursor" | "rev_follows" => {
            sr.review_follows_screen_cursor.into_lua(lua)
        }
        "highlight_tracking" => sr.highlight_tracking.into_lua(lua),
        _ => Err(Error::external(anyhow!("unknown option"))),
    }
    .map_err(|e| anyhow!("{}", e))
    .context(format!("get option: {}", option))
}

fn set_binding(
    lua: &Lua,
    sr: &mut ScreenReader,
    key: &str,
    value: Value,
) -> anyhow::Result<()> {
    match value {
        Value::Nil => {
            sr.key_bindings.clear_binding(key);
            Ok(())
        }
        Value::String(name) => {
            let name = name.to_str().map_err(|err| anyhow!(err.to_string()))?;
            let action = KeyBindings::builtin_action_from_value(name.as_ref())?;
            sr.key_bindings
                .set_builtin_binding(key.to_string(), action);
            Ok(())
        }
        Value::Table(table) => {
            let (help, func) = parse_binding_table(table)?;
            let Some(ctx) = sr.lua_ctx.as_ref() else {
                return Err(anyhow!("lua bindings are only available in init.lua"));
            };
            let Some(weak_ctx) = sr.lua_ctx_weak.as_ref() else {
                return Err(anyhow!("lua bindings are only available in init.lua"));
            };
            if *weak_ctx != lua.weak() {
                return Err(anyhow!("lua bindings are only available in init.lua"));
            }
            sr.key_bindings
                .set_lua_binding(key.to_string(), help, Rc::clone(ctx), func)?;
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
    let allow_function = sr
        .lua_ctx_weak
        .as_ref()
        .map(|ctx| *ctx == lua.weak())
        .unwrap_or(false);
    sr.key_bindings
        .binding_value_for_lua(key, lua, allow_function)
        .map_err(|err| anyhow!(err.to_string()))
}

fn set_option(sr: &mut ScreenReader, option: &str, value: mlua::Value) -> anyhow::Result<()> {
    use mlua::Value::*;
    (match option {
        "speech_rate" => match value {
            Number(v) => sr.speech.set_rate(v as f32),
            Integer(v) => sr.speech.set_rate(v as f32),
            _ => Err(anyhow!("value must be a number")),
        },
        "symbol_level" => match value {
            String(v) => {
                sr.speech.symbol_level = v
                    .to_str()
                    .map_err(|e| anyhow!("{}", e))?
                    .parse::<symbols::Level>()?;
                Ok(())
            }
            _ => Err(anyhow!("value must be a string")),
        },
        "help_mode" => match value {
            Boolean(v) => {
                sr.help_mode = v;
                Ok(())
            }
            _ => Err(anyhow!("value must be a boolean")),
        },
        "auto_read" => match value {
            Boolean(v) => {
                sr.auto_read = v;
                Ok(())
            }
            _ => Err(anyhow!("value must be a boolean")),
        },
        "review_follows_screen_cursor" | "rev_follows" => match value {
            Boolean(v) => {
                sr.review_follows_screen_cursor = v;
                Ok(())
            }
            _ => Err(anyhow!("value must be a boolean")),
        },
        "highlight_tracking" => match value {
            Boolean(v) => {
                sr.highlight_tracking = v;
                Ok(())
            }
            _ => Err(anyhow!("value must be a boolean")),
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
