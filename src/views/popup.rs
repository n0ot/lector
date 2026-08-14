use super::{Result, ViewAction, ViewController, ViewKind};
use crate::{screen_reader::ScreenReader, terminal_input::KeyInput, view::View};
use std::{any::Any, io::Write};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PopupResponse {
    Dismissed,
    Confirmed,
    Cancelled,
}

pub struct PopupView {
    view: View,
    title: String,
    text: String,
    confirmation: bool,
}

impl PopupView {
    pub fn announcement(
        rows: u16,
        cols: u16,
        title: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self::new(rows, cols, title, text, false)
    }

    pub fn confirmation(
        rows: u16,
        cols: u16,
        title: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self::new(rows, cols, title, text, true)
    }

    fn new(
        rows: u16,
        cols: u16,
        title: impl Into<String>,
        text: impl Into<String>,
        confirmation: bool,
    ) -> Self {
        let mut popup = Self {
            view: View::new(rows, cols),
            title: title.into(),
            text: text.into(),
            confirmation,
        };
        popup.render();
        popup
    }

    fn response(&self, enter: bool) -> PopupResponse {
        if !self.confirmation {
            PopupResponse::Dismissed
        } else if enter {
            PopupResponse::Confirmed
        } else {
            PopupResponse::Cancelled
        }
    }

    fn render(&mut self) {
        let mut bytes = b"\x1b[2J\x1b[H".to_vec();
        for line in self.text.lines() {
            bytes.extend_from_slice(line.as_bytes());
            bytes.extend_from_slice(b"\r\n");
        }
        bytes.extend_from_slice(b"\r\n");
        bytes.extend_from_slice(if self.confirmation {
            b"Press Enter to confirm or Escape to cancel."
        } else {
            b"Press Enter or Escape to close."
        });
        self.view.clear_update_summary();
        self.view.process_changes(&bytes);
    }
}

impl ViewController for PopupView {
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
        &self.title
    }

    fn kind(&self) -> ViewKind {
        ViewKind::Popup
    }

    fn handle_input(
        &mut self,
        _sr: &mut ScreenReader,
        input: &[u8],
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        match input {
            b"\r" | b"\n" => Ok(ViewAction::PopupResponse(self.response(true))),
            b"\x1b" => Ok(ViewAction::PopupResponse(self.response(false))),
            _ => Ok(ViewAction::None),
        }
    }

    fn handle_key_input(
        &mut self,
        _sr: &mut ScreenReader,
        key: &KeyInput,
        _raw: &[u8],
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        if key.is_release() {
            return Ok(ViewAction::None);
        }
        if matches!(key.control_code(), Some(b'\n' | b'\r'))
            || matches!(key.event().code, terminput::KeyCode::Enter)
        {
            Ok(ViewAction::PopupResponse(self.response(true)))
        } else if matches!(key.control_code(), Some(b'\x1b'))
            || matches!(key.event().code, terminput::KeyCode::Esc)
        {
            Ok(ViewAction::PopupResponse(self.response(false)))
        } else {
            Ok(ViewAction::None)
        }
    }

    fn on_resize(&mut self, rows: u16, cols: u16) {
        self.view.set_size(rows, cols);
        self.render();
    }
}
