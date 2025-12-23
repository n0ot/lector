use super::{ViewAction, ViewController, ViewKind};
use crate::{screen_reader::ScreenReader, view::View};
use anyhow::Result;

pub struct MessageView {
    view: View,
    title: String,
    text: String,
}

impl MessageView {
    pub fn new(rows: u16, cols: u16, title: impl Into<String>, text: impl Into<String>) -> Self {
        let title = title.into();
        let text = text.into();
        let mut view = View::new(rows, cols);
        render_message(&mut view, &text);
        Self { view, title, text }
    }

    fn render(&mut self) {
        render_message(&mut self.view, &self.text);
    }
}

impl ViewController for MessageView {
    fn model(&mut self) -> &mut View {
        &mut self.view
    }

    fn title(&self) -> &str {
        &self.title
    }

    fn kind(&self) -> ViewKind {
        ViewKind::Message
    }

    fn handle_input(
        &mut self,
        _sr: &mut ScreenReader,
        input: &[u8],
        _pty_stream: &mut ptyprocess::stream::Stream,
    ) -> Result<ViewAction> {
        if input == b"\x1B" || input == b"\r" || input == b"\n" {
            Ok(ViewAction::Pop)
        } else {
            Ok(ViewAction::None)
        }
    }

    fn on_resize(&mut self, rows: u16, cols: u16) {
        self.view.set_size(rows, cols);
        self.render();
    }
}

fn render_message(view: &mut View, text: &str) {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"\x1B[2J\x1B[H");
    for line in text.lines() {
        bytes.extend_from_slice(line.as_bytes());
        bytes.extend_from_slice(b"\r\n");
    }
    bytes.extend_from_slice(b"\r\nPress Enter or Escape to close.");
    view.next_bytes.clear();
    view.process_changes(&bytes);
}
