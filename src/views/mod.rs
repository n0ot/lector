mod lua_repl;
mod message;
mod pty;
mod stack;

pub use lua_repl::LuaReplView;
pub use message::MessageView;
pub use pty::PtyView;
pub use stack::ViewStack;

use crate::{screen_reader::ScreenReader, view::View};
use anyhow::Result;
use std::io::Write;

pub enum ViewAction {
    None,
    Bell,
    PtyInput,
    Push(Box<dyn ViewController>),
    Pop,
    Redraw,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ViewKind {
    Terminal,
    Message,
    LuaRepl,
    Other,
}

pub trait ViewController {
    fn model(&mut self) -> &mut View;
    fn title(&self) -> &str;
    fn kind(&self) -> ViewKind {
        ViewKind::Other
    }
    fn wants_tick(&self) -> bool {
        false
    }
    fn handle_input(
        &mut self,
        sr: &mut ScreenReader,
        input: &[u8],
        pty_stream: &mut dyn Write,
    ) -> Result<ViewAction>;
    fn tick(&mut self, _sr: &mut ScreenReader, _pty_stream: &mut dyn Write) -> Result<ViewAction> {
        Ok(ViewAction::None)
    }
    fn handle_paste(
        &mut self,
        _sr: &mut ScreenReader,
        _contents: &str,
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        Ok(ViewAction::None)
    }
    fn handle_pty_output(&mut self, _buf: &[u8]) -> Result<()> {
        Ok(())
    }
    fn on_resize(&mut self, rows: u16, cols: u16);
}
