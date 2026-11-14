//! The only bridge from a Lua VM back to the reader.
//!
//! Persistent callbacks capture a VM-local slot, never a reader address. Each
//! Rust-to-Lua entry lends its exclusive reader reference to an mlua scope.
//! Callbacks can borrow that scoped userdata only until the call returns or
//! yields. A nested Rust hook reborrows the reader into a child scope; its
//! handle temporarily replaces the parent handle. No slot borrow survives a
//! callback, and restoration runs on errors and Rust unwinding too. mlua
//! invalidates escaped userdata on scope exit, independently of our slot.
//!
//! Configuration access additionally checks VM ownership on entry and on each
//! borrow. Thus a retained old VM cannot target the replacement generation,
//! even during nested calls. REPL VMs are authorized by the caller for each
//! resume, and retain Lua values, not a reader, between resumes. Lua-triggered
//! reentry without a Rust reborrow (for example an argument metamethod) is
//! conservatively rejected by mlua's userdata borrow checks.

use crate::screen_reader::ScreenReader;
use mlua::{AnyUserData, Error, Lua, Result, WeakLua};
use std::{
    cell::{Cell, RefCell},
    mem::DropGuard,
    rc::Rc,
};

// Bound Rust/Lua cross-boundary recursion independently of borrow safety.
const MAX_ENTRY_DEPTH: usize = 32;

#[derive(Clone)]
pub(super) struct ReaderAccess(Rc<AccessState>);

struct AccessState {
    active: RefCell<Option<AnyUserData>>,
    configuration: Option<WeakLua>,
    depth: Cell<usize>,
}

impl ReaderAccess {
    pub(super) fn install(lua: &Lua, configuration: bool) -> Self {
        let access = Self(Rc::new(AccessState {
            active: RefCell::new(None),
            configuration: configuration.then(|| lua.weak()),
            depth: Cell::new(0),
        }));
        lua.set_app_data(access.clone());
        access
    }

    fn validate(&self, reader: &ScreenReader) -> Result<()> {
        if let Some(context) = &self.0.configuration
            && !reader.owns_lua_generation(context)
        {
            return Err(Error::runtime(
                "screen reader configuration is no longer active",
            ));
        }
        Ok(())
    }

    fn active(&self) -> Result<AnyUserData> {
        self.0
            .active
            .borrow()
            .clone()
            .ok_or_else(|| Error::runtime("screen reader unavailable outside Lua entry"))
    }

    pub(super) fn with_mut<T>(&self, f: impl FnOnce(&mut ScreenReader) -> Result<T>) -> Result<T> {
        self.active()?
            .borrow_mut_scoped::<ScreenReader, _>(|reader| {
                self.validate(reader)?;
                f(reader)
            })?
    }

    pub(super) fn with<T>(&self, f: impl FnOnce(&ScreenReader) -> Result<T>) -> Result<T> {
        self.active()?.borrow_scoped::<ScreenReader, _>(|reader| {
            self.validate(reader)?;
            f(reader)
        })?
    }
}

/// Call only after cloning all Lua handles and snapshotting reader arguments;
/// the closure must not capture any reference into `reader`.
pub(crate) fn enter<T>(
    lua: &Lua,
    reader: &mut ScreenReader,
    call: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let access = lua
        .app_data_ref::<ReaderAccess>()
        .map(|access| access.clone());
    let Some(access) = access else {
        // A plain Lua VM without installed reader callbacks needs no bridge.
        return call();
    };
    access.validate(reader)?;
    if access.0.depth.get() >= MAX_ENTRY_DEPTH {
        return Err(Error::runtime("Lua reader entry nesting limit exceeded"));
    }
    lua.scope(|scope| {
        let handle = scope.create_any_userdata_ref_mut(reader)?;
        let previous = access.0.active.replace(Some(handle));
        access.0.depth.set(access.0.depth.get() + 1);
        let _restore = DropGuard::new((access, previous), |(access, previous)| {
            access.0.active.replace(previous);
            access.0.depth.set(access.0.depth.get() - 1);
        });
        call()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaped_handles_expire_and_incompatible_reentry_is_rejected() {
        let lua = Lua::new();
        let access = ReaderAccess::install(&lua, false);
        let mut reader = crate::lua::tests::screen_reader();
        let handle = enter(&lua, &mut reader, || {
            access.with(|_| {
                assert!(access.with_mut(|_| Ok(())).is_err());
                Ok(())
            })?;
            access.with_mut(|reader| {
                enter(&lua, reader, || {
                    access.with_mut(|reader| {
                        reader.set_help_mode(true);
                        Ok(())
                    })
                })
            })?;
            access.active()
        })
        .unwrap();
        assert!(reader.help_mode());
        assert!(access.active().is_err());
        assert!(handle.borrow_mut_scoped::<ScreenReader, _>(|_| ()).is_err());
    }

    #[test]
    fn restoring_parent_handle_cannot_restore_retired_generation_authority() {
        let mut reader = crate::lua::tests::screen_reader();
        let old = crate::lua::install_configuration_context(&mut reader).unwrap();
        let access = old.app_data_ref::<ReaderAccess>().unwrap().clone();
        let replacement = Rc::new(Lua::new());
        enter(&old, &mut reader, || {
            access.with_mut(|reader| {
                enter(&old, reader, || {
                    access.with_mut(|reader| {
                        reader.set_lua_context(replacement.clone());
                        Ok(())
                    })
                })
            })?;
            assert!(access.with(|_| Ok(())).is_err());
            assert!(access.with_mut(|_| Ok(())).is_err());
            Ok(())
        })
        .unwrap();
        assert!(access.active().is_err());
        assert!(enter(&old, &mut reader, || Ok(())).is_err());
    }
}
