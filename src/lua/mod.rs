use self::ext::LuaResultExt;
use crate::screen_reader::ScreenReader;
use anyhow::{Context as AnyhowContext, anyhow};
use mlua::{Error, Function, Lua, LuaOptions, Result, StdLib};
use std::{cell::RefCell, fs::File, io::Read, path::PathBuf, rc::Rc};

mod ext;
mod meta;

pub fn setup<F>(init_lua_file: PathBuf, screen_reader: &mut ScreenReader, after: F) -> Result<()>
where
    F: FnOnce(&mut ScreenReader) -> anyhow::Result<()>,
{
    let lua = Rc::new(Lua::new_with(StdLib::ALL_SAFE | StdLib::JIT, LuaOptions::default())?);
    screen_reader.set_lua_context(Rc::clone(&lua));
    let sr_ptr = Rc::new(RefCell::new(screen_reader as *mut ScreenReader));
    install_api_static(&lua, Rc::clone(&sr_ptr))?;
    meta::setup_static(&lua, Rc::clone(&sr_ptr))?;

    if init_lua_file.is_file() {
        load_file(&lua, &init_lua_file)?.call::<()>(())?;
    }

    if let Err(e) = after(screen_reader) {
        return Err(Error::external(e));
    }

    Ok(())
}

pub fn setup_repl(lua: &Lua, sr_ptr: Rc<RefCell<*mut ScreenReader>>) -> Result<()> {
    install_api_static(lua, Rc::clone(&sr_ptr))?;
    meta::setup_static(lua, sr_ptr)?;
    Ok(())
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

fn install_api_static(lua: &Lua, sr_ptr: Rc<RefCell<*mut ScreenReader>>) -> Result<()> {
    let tbl_lector = lua.create_table()?;
    let tbl_api = lua.create_table()?;
    let speak_fn = lua
        .create_function(move |_, (text, interrupt): (String, bool)| {
            let ptr = *sr_ptr.borrow();
            if ptr.is_null() {
                return Err(Error::external(anyhow!("screen reader unavailable")));
            }
            // Safety: pointer is set by the main thread before any Lua call.
            let sr = unsafe { &mut *ptr };
            sr.speech.speak(&text, interrupt).to_lua_result()
        })?;
    tbl_api.set("speak", speak_fn)?;
    tbl_lector.set("api", tbl_api)?;
    lua.globals().set("lector", tbl_lector)?;
    Ok(())
}
