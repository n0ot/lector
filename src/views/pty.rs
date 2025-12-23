use super::{ViewAction, ViewController, ViewKind};
use crate::{screen_reader::ScreenReader, view::View};
use anyhow::Result;
use std::io::Write;

pub struct PtyView {
    view: View,
}

impl PtyView {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            view: View::new(rows, cols),
        }
    }
}

impl ViewController for PtyView {
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
        sr.speech.speak("pasted", false)?;
        Ok(ViewAction::PtyInput)
    }

    fn handle_pty_output(&mut self, buf: &[u8]) -> Result<()> {
        self.view.process_changes(buf);
        Ok(())
    }

    fn on_resize(&mut self, rows: u16, cols: u16) {
        self.view.set_size(rows, cols);
    }
}
