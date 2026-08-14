use super::{Result, ViewAction, ViewController, ViewKind};
use crate::{screen_reader::ScreenReader, terminal_input::KeyInput, view::View};
use std::any::Any;
use std::io::Write;

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
    fn as_any(&self) -> &dyn Any {
        self
    }

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
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        if input == b"\x1B" || input == b"\r" || input == b"\n" {
            Ok(ViewAction::Pop)
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
        if key.is_release() {
            return Ok(ViewAction::None);
        }
        if matches!(key.control_code(), Some(b'\n' | b'\r' | b'\x1B'))
            || matches!(
                key.event().code,
                terminput::KeyCode::Enter | terminput::KeyCode::Esc
            )
        {
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
    view.clear_update_summary();
    view.process_changes(&bytes);
}

#[cfg(test)]
mod tests {
    use super::{MessageView, ViewAction, ViewController, ViewKind};
    use crate::{screen_reader::ScreenReader, speech};

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

    fn screen_reader() -> ScreenReader {
        ScreenReader::new(speech::Speech::new(Box::new(SilentDriver)))
    }

    #[test]
    fn renders_metadata_body_and_close_hint() {
        let mut message = MessageView::new(5, 40, "Notice", "first\nsecond");

        assert_eq!(message.title(), "Notice");
        assert_eq!(message.kind(), ViewKind::Message);
        assert!(message.as_any().is::<MessageView>());
        assert_eq!(message.model().line(0), "first");
        assert_eq!(message.model().line(1), "second");
        assert!(
            message
                .model()
                .contents_full()
                .contains("Press Enter or Escape to close.")
        );
    }

    #[test]
    fn only_escape_and_line_endings_close_the_message() {
        let mut message = MessageView::new(4, 30, "Notice", "body");
        let mut sr = screen_reader();
        let mut pty = Vec::new();

        assert!(matches!(
            message.handle_input(&mut sr, b"x", &mut pty).unwrap(),
            ViewAction::None
        ));
        for input in [b"\x1B".as_slice(), b"\r", b"\n"] {
            assert!(matches!(
                message.handle_input(&mut sr, input, &mut pty).unwrap(),
                ViewAction::Pop
            ));
        }
    }

    #[test]
    fn resize_renders_from_scratch_at_the_new_dimensions() {
        let mut message = MessageView::new(6, 40, "Notice", "body");
        message.model().process_changes(b"stale");

        message.on_resize(5, 20);

        assert_eq!(message.model().size(), (5, 20));
        assert_eq!(message.model().line(0), "body");
        assert!(!message.model().contents_full().contains("stale"));
    }
}
