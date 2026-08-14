use super::{
    Result, ViewAction, ViewController, ViewKind,
    text_input::{truncate_display_width, visible_input_window},
};
use crate::{
    line_editor::{EditorAction, LineEditor},
    screen_reader::ScreenReader,
    terminal_input::KeyInput,
    view::View,
};
use std::{any::Any, io::Write};
use terminput::KeyCode;

pub struct TmuxCommandView {
    view: View,
    connection_id: u64,
    editor: LineEditor,
}

impl TmuxCommandView {
    #[must_use]
    pub fn new(rows: u16, cols: u16, connection_id: u64, history: Vec<String>) -> Self {
        let mut editor = LineEditor::new();
        editor.set_history(history);
        let mut command = Self {
            view: View::new(rows, cols),
            connection_id,
            editor,
        };
        command.render();
        command
    }

    fn submit(&mut self) -> ViewAction {
        let command = self.editor.input().trim().to_owned();
        if command.is_empty() {
            return ViewAction::Bell;
        }
        self.editor.commit_history();
        ViewAction::TmuxCommandSubmit {
            connection_id: self.connection_id,
            command,
        }
    }

    fn apply_editor_action(&mut self, action: EditorAction) -> ViewAction {
        match action {
            EditorAction::Submit => self.submit(),
            EditorAction::Changed => {
                self.render();
                ViewAction::Redraw
            }
            EditorAction::Bell => ViewAction::Bell,
            EditorAction::None => ViewAction::None,
        }
    }

    fn render(&mut self) {
        let (rows, cols) = self.view.size();
        let (visible_input, input_cursor_width) = visible_input_window(
            self.editor.input(),
            self.editor.cursor(),
            usize::from(cols).saturating_sub(2),
        );
        let lines = [
            "Enter submits, Escape cancels".to_owned(),
            format!(": {visible_input}"),
        ];
        let mut bytes = b"\x1b[2J\x1b[H".to_vec();
        for (index, line) in lines.into_iter().take(usize::from(rows)).enumerate() {
            if index > 0 {
                bytes.extend_from_slice(b"\r\n");
            }
            bytes.extend_from_slice(truncate_display_width(&line, usize::from(cols)).as_bytes());
        }
        let cursor_col = input_cursor_width.saturating_add(3).min(usize::from(cols));
        bytes.extend_from_slice(format!("\x1b[2;{}H", cursor_col.max(1)).as_bytes());
        self.view.clear_update_summary();
        self.view.process_changes(&bytes);
        self.view.clear_update_summary();
    }
}

impl ViewController for TmuxCommandView {
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
        "tmux command"
    }

    fn kind(&self) -> ViewKind {
        ViewKind::TmuxCommand
    }

    fn handle_input(
        &mut self,
        _sr: &mut ScreenReader,
        input: &[u8],
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        if input == b"\x1b" {
            return Ok(ViewAction::Pop);
        }
        let action = self.editor.handle_bytes(input);
        Ok(self.apply_editor_action(action))
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
        if key.event().code == KeyCode::Esc {
            return Ok(ViewAction::Pop);
        }
        let action = self.editor.handle_key_input(key);
        Ok(self.apply_editor_action(action))
    }

    fn handle_paste(
        &mut self,
        _sr: &mut ScreenReader,
        contents: &str,
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        let action = self.editor.handle_text(contents);
        Ok(self.apply_editor_action(action))
    }

    fn on_resize(&mut self, rows: u16, cols: u16) {
        self.view.set_size(rows, cols);
        self.render();
    }
}
