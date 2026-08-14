use super::{Result, ViewAction, ViewController, ViewKind};
use crate::{screen_reader::ScreenReader, terminal_input::KeyInput, view::View};
use std::{any::Any, io::Write};

const PORTAL_TEXT: &str = "tmux control mode is running.\r\n\
Press Enter to switch to the active session in this connection.";

pub struct TmuxPortalView {
    view: View,
    connection_id: u64,
}

impl TmuxPortalView {
    #[must_use]
    pub fn new(rows: u16, cols: u16, connection_id: u64) -> Self {
        let mut view = View::new(rows, cols);
        render(&mut view);
        Self {
            view,
            connection_id,
        }
    }
}

impl ViewController for TmuxPortalView {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn model(&mut self) -> &mut View {
        &mut self.view
    }

    fn title(&self) -> &str {
        "tmux portal"
    }

    fn kind(&self) -> ViewKind {
        ViewKind::TmuxPortal
    }

    fn handle_input(
        &mut self,
        _sr: &mut ScreenReader,
        input: &[u8],
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        if matches!(input, b"\r" | b"\n") {
            Ok(ViewAction::ActivateTmuxConnection(self.connection_id))
        } else {
            Ok(ViewAction::None)
        }
    }

    fn handle_key_input(
        &mut self,
        _sr: &mut ScreenReader,
        key: &KeyInput,
        _raw: &[u8],
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        if !key.is_release()
            && (matches!(key.control_code(), Some(b'\n' | b'\r'))
                || key.event().code == terminput::KeyCode::Enter)
        {
            Ok(ViewAction::ActivateTmuxConnection(self.connection_id))
        } else {
            Ok(ViewAction::None)
        }
    }

    fn on_resize(&mut self, rows: u16, cols: u16) {
        self.view.set_size(rows, cols);
        render(&mut self.view);
    }
}

fn render(view: &mut View) {
    view.clear_update_summary();
    let mut bytes = b"\x1b[2J\x1b[H".to_vec();
    bytes.extend_from_slice(PORTAL_TEXT.as_bytes());
    view.process_changes(&bytes);
}
