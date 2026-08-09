use mlua::{Error, Result};

pub trait LuaResultExt<T> {
    fn to_lua_result(self) -> Result<T>;
}

impl<T> LuaResultExt<T> for anyhow::Result<T> {
    fn to_lua_result(self) -> Result<T> {
        match self {
            Ok(r) => Ok(r),
            Err(e) => Err(Error::external(e)),
        }
    }
}

impl<T> LuaResultExt<T> for crate::screen_reader::Result<T> {
    fn to_lua_result(self) -> Result<T> {
        self.map_err(Error::external)
    }
}
