use crate::{screen_reader::ScreenReader, view::View};
use anyhow::Result;
use std::io::Write;

pub enum ViewAction {
    None,
    PtyInput,
    Push(Box<dyn ViewController>),
    Pop,
    Redraw,
}

pub trait ViewController {
    fn model(&mut self) -> &mut View;
    fn handle_input(
        &mut self,
        sr: &mut ScreenReader,
        input: &[u8],
        pty_stream: &mut ptyprocess::stream::Stream,
    ) -> Result<ViewAction>;
    fn handle_paste(
        &mut self,
        _sr: &mut ScreenReader,
        _contents: &str,
        _pty_stream: &mut ptyprocess::stream::Stream,
    ) -> Result<ViewAction> {
        Ok(ViewAction::None)
    }
    fn handle_pty_output(&mut self, _buf: &[u8]) -> Result<()> {
        Ok(())
    }
    fn on_resize(&mut self, rows: u16, cols: u16);
}

pub struct ViewStack {
    views: Vec<Box<dyn ViewController>>,
}

impl ViewStack {
    pub fn new(root: Box<dyn ViewController>) -> Self {
        Self { views: vec![root] }
    }

    pub fn active_mut(&mut self) -> &mut dyn ViewController {
        self.views
            .last_mut()
            .expect("view stack should always have a root view")
            .as_mut()
    }

    pub fn root_mut(&mut self) -> &mut dyn ViewController {
        self.views
            .first_mut()
            .expect("view stack should always have a root view")
            .as_mut()
    }

    pub fn push(&mut self, view: Box<dyn ViewController>) {
        self.views.push(view);
    }

    pub fn pop(&mut self) -> bool {
        if self.views.len() <= 1 {
            return false;
        }
        self.views.pop();
        true
    }

    pub fn has_overlay(&self) -> bool {
        self.views.len() > 1
    }

    pub fn on_resize(&mut self, rows: u16, cols: u16) {
        for view in &mut self.views {
            view.on_resize(rows, cols);
        }
    }
}

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

    fn handle_input(
        &mut self,
        _sr: &mut ScreenReader,
        input: &[u8],
        pty_stream: &mut ptyprocess::stream::Stream,
    ) -> Result<ViewAction> {
        pty_stream.write_all(input)?;
        pty_stream.flush()?;
        Ok(ViewAction::PtyInput)
    }

    fn handle_paste(
        &mut self,
        sr: &mut ScreenReader,
        contents: &str,
        pty_stream: &mut ptyprocess::stream::Stream,
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
