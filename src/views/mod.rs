mod lua_repl;
mod message;
mod popup;
mod pty;
mod review;
mod stack;
mod text_input;
mod tmux_chooser;
mod tmux_command;
mod tmux_connection;
mod tmux_connections;
mod tmux_portal;

pub use lua_repl::LuaReplView;
pub use message::MessageView;
pub use popup::{PopupResponse, PopupView};
pub use pty::PtyView;
pub use review::ReviewView;
pub use stack::ViewStack;
pub use tmux_chooser::{TmuxChooserTarget, TmuxChooserView};
pub use tmux_command::TmuxCommandView;
pub use tmux_connection::TmuxConnectionView;
pub use tmux_connections::{
    TmuxConnectionChooserView, TmuxConnectionItem, TmuxConnectionRenameView, TmuxConnectionTarget,
};
pub use tmux_portal::TmuxPortalView;

use crate::{
    screen_reader::ScreenReader, terminal::TerminalGeometry, terminal_input::KeyInput, view::View,
};
use std::any::Any;
use std::io::Write;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("view I/O")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    ScreenReader(#[from] crate::screen_reader::Error),
    #[error("Lua: {0}")]
    Lua(String),
}

impl Error {
    fn lua(error: impl std::fmt::Display) -> Self {
        Self::Lua(error.to_string())
    }
}

pub enum ViewAction {
    None,
    Bell,
    PtyInput,
    Push(Box<dyn ViewController>),
    Pop,
    PopupResponse(PopupResponse),
    ActivateTmuxConnection(u64),
    ActivateTerminal,
    TmuxConnectionRename {
        connection_id: u64,
        label: String,
    },
    TmuxChooserSelect {
        connection_id: u64,
        target: TmuxChooserTarget,
    },
    TmuxCommandSubmit {
        connection_id: u64,
        command: String,
    },
    TmuxInput {
        connection_id: u64,
        pane_id: crate::tmux_model::PaneId,
        bytes: Vec<u8>,
    },
    Redraw,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ViewKind {
    Terminal,
    Message,
    LuaRepl,
    Review,
    Popup,
    TableSetup,
    TmuxConnection,
    TmuxConnectionChooser,
    TmuxConnectionRename,
    TmuxChooser,
    TmuxCommand,
    TmuxPortal,
    Other,
}

pub trait ViewController {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn model(&mut self) -> &mut View;
    fn title(&self) -> &str;
    fn kind(&self) -> ViewKind {
        ViewKind::Other
    }
    /// Gives overlays a chance to interpret Lector's mouse-click actions as
    /// local cursor placement instead of forwarding a mouse event.
    fn place_application_cursor_at_review_cursor(&mut self) -> Option<ViewAction> {
        None
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
    /// Handles a decoded key while retaining its original bytes for views, such
    /// as the PTY, that must preserve the terminal's exact input protocol.
    fn handle_key_input(
        &mut self,
        sr: &mut ScreenReader,
        _key: &KeyInput,
        raw: &[u8],
        pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        self.handle_input(sr, raw, pty_stream)
    }
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
    fn on_resize_with_geometry(&mut self, geometry: TerminalGeometry) {
        self.on_resize(geometry.rows, geometry.cols);
    }
}

#[cfg(test)]
mod tests {
    use super::{Error, Result, ViewAction, ViewController, ViewKind};
    use crate::{screen_reader::ScreenReader, speech, view::View};
    use std::{any::Any, io::Write};

    struct SilentDriver;

    impl speech::Driver for SilentDriver {
        fn speak(&mut self, _text: &str, _interrupt: bool) -> anyhow::Result<()> {
            Ok(())
        }

        fn stop(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        fn get_rate(&self) -> f32 {
            1.0
        }

        fn set_rate(&mut self, _rate: f32) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct MinimalView(View);

    impl ViewController for MinimalView {
        fn as_any(&self) -> &dyn Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }

        fn model(&mut self) -> &mut View {
            &mut self.0
        }

        fn title(&self) -> &str {
            "minimal"
        }

        fn handle_input(
            &mut self,
            _sr: &mut ScreenReader,
            _input: &[u8],
            _pty_stream: &mut dyn Write,
        ) -> Result<ViewAction> {
            Ok(ViewAction::None)
        }

        fn on_resize(&mut self, rows: u16, cols: u16) {
            self.0.set_size(rows, cols);
        }
    }

    #[test]
    fn controller_defaults_are_inert() {
        let mut view = MinimalView(View::new(2, 3));
        let speech = speech::Speech::new(Box::new(SilentDriver));
        let mut sr = ScreenReader::new(speech);
        let mut output = Vec::new();

        assert_eq!(view.kind(), ViewKind::Other);
        assert!(!view.wants_tick());
        assert!(matches!(
            view.tick(&mut sr, &mut output).unwrap(),
            ViewAction::None
        ));
        assert!(matches!(
            view.handle_paste(&mut sr, "text", &mut output).unwrap(),
            ViewAction::None
        ));
        view.handle_pty_output(b"ignored").unwrap();
        assert!(view.model().contents_full().trim().is_empty());
    }

    #[test]
    fn lua_errors_preserve_the_original_message() {
        assert_eq!(Error::lua("bad chunk").to_string(), "Lua: bad chunk");
    }
}
