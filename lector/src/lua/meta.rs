use super::ext::LuaResultExt;
use crate::{screen_reader::ScreenReader, speech::symbols};
use anyhow::{anyhow, Context as AnyhowContext};
use rlua::{Context, Error, Result, Scope, Table, ToLua, Value};
use std::cell::RefCell;

pub fn setup<'lua, 'scope>(
    ctx: &Context<'lua>,
    scope: &Scope<'lua, 'scope>,
    sr: &'scope RefCell<&mut ScreenReader>,
) -> Result<()> {
    let tbl_callbacks = ctx.create_table()?;
    add_callbacks(&tbl_callbacks, &scope, &sr)?;
    ctx.load(include_str!("meta.lua"))
        .set_name("meta.lua")?
        .call::<_, ()>(tbl_callbacks)?;

    Ok(())
}

fn add_callbacks<'lua, 'scope>(
    tbl_callbacks: &Table<'lua>,
    scope: &Scope<'lua, 'scope>,
    screen_reader: &'scope RefCell<&mut ScreenReader>,
) -> Result<()> {
    tbl_callbacks.set(
        "set_option",
        scope.create_function_mut(|_, (key, value): (String, rlua::Value)| {
            let mut sr = screen_reader.borrow_mut();
            set_option(&mut sr, &key, value).to_lua_result()
        })?,
    )?;
    tbl_callbacks.set(
        "get_option",
        scope.create_function(|ctx, key: String| {
            let sr = screen_reader.borrow();
            get_option(ctx, &sr, &key).to_lua_result()
        })?,
    )?;

    tbl_callbacks.set(
        "set_symbol",
        scope.create_function_mut(|_, (key, value): (String, rlua::Value)| {
            let mut sr = screen_reader.borrow_mut();
            match value {
                rlua::Value::Nil => {
                    sr.speech.symbols_map.remove(&key);
                    Ok(())
                }
                rlua::Value::Table(table_value) => {
                    let replacement: String = table_value.get(1)?;
                    let level: symbols::Level = table_value
                        .get::<usize, String>(2)?
                        .parse()
                        .context("parse level")
                        .to_lua_result()?;
                    let include_original: symbols::IncludeOriginal = table_value
                        .get::<usize, String>(3)?
                        .parse()
                        .context("parse include_original")
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
        })?,
    )?;
    tbl_callbacks.set(
        "get_symbol",
        scope.create_function(|ctx, key: String| {
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
        })?,
    )?;
    tbl_callbacks.set(
        "clear_symbols",
        scope.create_function_mut(|_, ()| {
            let mut sr = screen_reader.borrow_mut();
            sr.speech.symbols_map.clear();
            Ok(())
        })?,
    )?;

    Ok(())
}

fn get_option<'lua>(
    ctx: Context<'lua>,
    sr: &ScreenReader,
    option: &str,
) -> anyhow::Result<rlua::Value<'lua>> {
    match option {
        "speech_rate" => sr.speech.get_rate().to_lua(ctx),
        "symbol_level" => sr.speech.symbol_level.to_string().to_lua(ctx),
        "help_mode" => sr.help_mode.to_lua(ctx),
        "auto_read" => sr.auto_read.to_lua(ctx),
        "review_follows_screen_cursor" | "rev_follows" => {
            sr.review_follows_screen_cursor.to_lua(ctx)
        }
        _ => Err(Error::external(anyhow!("unknown option"))),
    }
    .context(format!("get option: {}", option))
}

fn set_option(sr: &mut ScreenReader, option: &str, value: rlua::Value) -> anyhow::Result<()> {
    use rlua::Value::*;
    match option {
        "speech_rate" => match value {
            Number(v) => sr.speech.set_rate(v as f32),
            _ => Err(anyhow!("value must be a number")),
        },
        "symbol_level" => match value {
            String(v) => {
                sr.speech.symbol_level = v.to_str()?.parse::<symbols::Level>()?;
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
        _ => Err(anyhow!("unknown option")),
    }
    .context(format!("set option: {}", option))
}
