use super::{Result, ViewAction, ViewController, ViewKind};
use crate::{screen_reader::ScreenReader, terminal::TerminalGeometry, view::View};
use std::any::Any;
use std::io::Write;

pub struct PtyView {
    view: View,
}

impl PtyView {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self::new_with_geometry(TerminalGeometry::from_cells(rows, cols))
    }

    pub fn new_with_geometry(geometry: TerminalGeometry) -> Self {
        let mut view = View::new(geometry.rows, geometry.cols);
        view.set_size_with_geometry(geometry);
        Self { view }
    }
}

impl ViewController for PtyView {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn model(&mut self) -> &mut View {
        &mut self.view
    }

    fn title(&self) -> &str {
        "Terminal"
    }

    fn kind(&self) -> ViewKind {
        ViewKind::Terminal
    }

    fn handle_input(
        &mut self,
        _sr: &mut ScreenReader,
        input: &[u8],
        pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        pty_stream.write_all(input)?;
        pty_stream.flush()?;
        Ok(ViewAction::PtyInput)
    }

    fn handle_paste(
        &mut self,
        sr: &mut ScreenReader,
        contents: &str,
        pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        if self.view.screen().bracketed_paste() {
            write!(pty_stream, "\x1B[200~{}\x1B[201~", contents)?;
        } else {
            write!(pty_stream, "{}", contents)?;
        }
        pty_stream.flush()?;
        sr.speak("pasted", false)?;
        Ok(ViewAction::PtyInput)
    }

    fn handle_pty_output(&mut self, buf: &[u8]) -> Result<()> {
        self.view.process_changes(buf);
        Ok(())
    }

    fn on_resize(&mut self, rows: u16, cols: u16) {
        self.view.set_size(rows, cols);
    }

    fn on_resize_with_geometry(&mut self, geometry: TerminalGeometry) {
        self.view.set_size_with_geometry(geometry);
    }
}

#[cfg(test)]
mod tests {
    use super::{PtyView, ViewAction, ViewController, ViewKind};
    use crate::{screen_reader::ScreenReader, speech};
    use std::{cell::RefCell, rc::Rc};

    struct RecordingDriver(Rc<RefCell<Vec<String>>>);

    impl speech::Driver for RecordingDriver {
        fn speak(&mut self, text: &str, _interrupt: bool) -> anyhow::Result<()> {
            self.0.borrow_mut().push(text.to_owned());
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

    fn screen_reader() -> (ScreenReader, Rc<RefCell<Vec<String>>>) {
        let output = Rc::new(RefCell::new(Vec::new()));
        let speech = speech::Speech::new(Box::new(RecordingDriver(Rc::clone(&output))));
        (ScreenReader::new(speech), output)
    }

    #[test]
    fn bracketed_paste_wraps_contents_and_announces_it() {
        let mut view = PtyView::new(3, 10);
        let (mut sr, speech) = screen_reader();
        let mut pty = Vec::new();
        view.handle_pty_output(b"\x1B[?2004h").unwrap();

        let action = view.handle_paste(&mut sr, "a\nb", &mut pty).unwrap();

        assert!(matches!(action, ViewAction::PtyInput));
        assert_eq!(pty, b"\x1B[200~a\nb\x1B[201~");
        assert_eq!(speech.borrow().as_slice(), ["pasted"]);
    }

    #[test]
    fn input_metadata_and_resize_match_terminal_behavior() {
        let mut view = PtyView::new(3, 10);
        let (mut sr, _speech) = screen_reader();
        let mut pty = Vec::new();

        assert_eq!(view.title(), "Terminal");
        assert_eq!(view.kind(), ViewKind::Terminal);
        assert!(view.as_any().is::<PtyView>());
        assert!(matches!(
            view.handle_input(&mut sr, b"abc", &mut pty).unwrap(),
            ViewAction::PtyInput
        ));
        assert_eq!(pty, b"abc");
        view.on_resize(5, 12);
        assert_eq!(view.model().size(), (5, 12));
    }
}
