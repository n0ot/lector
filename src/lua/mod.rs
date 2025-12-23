use self::ext::LuaResultExt;
use crate::screen_reader::ScreenReader;
use anyhow::{Context as AnyhowContext, anyhow};
use mlua::{Error, Function, LuaOptions, Result, Scope, StdLib, Table, prelude::*};
use std::{cell::RefCell, fs::File, io::Read, path::PathBuf};

mod ext;
mod meta;

pub fn setup<F>(init_lua_file: PathBuf, screen_reader: &mut ScreenReader, after: F) -> Result<()>
where
    F: FnOnce(&mut ScreenReader) -> anyhow::Result<()>,
{
    let sr = RefCell::new(screen_reader);
    let lua = Lua::new_with(StdLib::ALL_SAFE | StdLib::JIT, LuaOptions::default())?;
    lua.scope(|scope| {
        let tbl_lector = lua.create_table()?;
        let tbl_api = lua.create_table()?;
        add_callbacks(&tbl_api, &scope, &sr)?;
        tbl_lector.set("api", tbl_api)?;
        lua.globals().set("lector", tbl_lector)?;

        meta::setup(&lua, &scope, &sr)?;

        if init_lua_file.is_file() {
            load_file(&lua, &init_lua_file)?.call::<()>(())?;
        }

        let mut screen_reader = sr.borrow_mut();
        if let Err(e) = after(&mut screen_reader) {
            return Err(Error::external(e));
        }

        Ok(())
    })
}

fn load_file(lua: &Lua, path: &PathBuf) -> Result<Function> {
    let path_string = path
        .to_str()
        .ok_or_else(|| anyhow!("convert path to string"))
        .to_lua_result()?
        .to_string();
    let mut f = File::open(path)
        .map_err(anyhow::Error::from)
        .context(format!("open {}", &path_string))
        .to_lua_result()?;
    let mut s = String::new();
    f.read_to_string(&mut s)
        .map_err(anyhow::Error::from)
        .context(format!("read {}", path_string))
        .to_lua_result()?;

    lua.load(&s).set_name(&path_string).into_function()
}

fn add_callbacks<'lua, 'scope>(
    tbl_api: &Table,
    scope: &'lua Scope<'lua, 'scope>,
    screen_reader: &'scope RefCell<&mut ScreenReader>,
) -> Result<()> {
    tbl_api.set(
        "speak",
        scope.create_function_mut(|_, (text, interrupt): (String, bool)| {
            let mut sr = screen_reader.borrow_mut();
            sr.speech.speak(&text, interrupt).to_lua_result()
        })?,
    )?;

    Ok(())
}
