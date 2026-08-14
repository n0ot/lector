use super::{Result, ViewAction, ViewController, ViewKind};
use crate::{
    line_editor::{EditorAction, LineEditor},
    screen_reader::ScreenReader,
    terminal_input::KeyInput,
    view::View,
};
use std::{any::Any, io::Write};
use terminput::KeyCode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TmuxConnectionTarget {
    Terminal,
    Connection(u64),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TmuxConnectionItem {
    pub connection_id: u64,
    pub label: String,
}

pub struct TmuxConnectionChooserView {
    view: View,
    items: Vec<TmuxConnectionItem>,
    selected: TmuxConnectionTarget,
    editor: LineEditor,
    viewport_start: usize,
}

impl TmuxConnectionChooserView {
    #[must_use]
    pub fn new(
        rows: u16,
        cols: u16,
        items: Vec<TmuxConnectionItem>,
        active_connection: Option<u64>,
    ) -> Self {
        let mut chooser = Self {
            view: View::new(rows, cols),
            items,
            selected: active_connection.map_or(
                TmuxConnectionTarget::Terminal,
                TmuxConnectionTarget::Connection,
            ),
            editor: LineEditor::new(),
            viewport_start: 0,
        };
        chooser.reconcile_selection(active_connection);
        chooser.render();
        chooser
    }

    pub fn sync(&mut self, items: Vec<TmuxConnectionItem>, active_connection: Option<u64>) {
        self.items = items;
        self.reconcile_selection(active_connection);
        self.render();
    }

    fn targets(&self) -> Vec<(TmuxConnectionTarget, String)> {
        let mut targets = vec![(TmuxConnectionTarget::Terminal, "terminal".to_owned())];
        targets.extend(self.items.iter().map(|item| {
            (
                TmuxConnectionTarget::Connection(item.connection_id),
                format!("connection {} {}", item.connection_id, item.label),
            )
        }));
        let query = self.editor.input().to_lowercase();
        targets
            .into_iter()
            .filter(|(_, label)| query.is_empty() || label.to_lowercase().contains(&query))
            .collect()
    }

    fn reconcile_selection(&mut self, active_connection: Option<u64>) {
        let targets = self.targets();
        if targets.iter().any(|(target, _)| *target == self.selected) {
            return;
        }
        let active = active_connection.map_or(
            TmuxConnectionTarget::Terminal,
            TmuxConnectionTarget::Connection,
        );
        self.selected = targets
            .iter()
            .find(|(target, _)| *target == active)
            .or_else(|| targets.first())
            .map_or(TmuxConnectionTarget::Terminal, |(target, _)| *target);
    }

    fn move_selection(&mut self, delta: isize) -> bool {
        let targets = self.targets();
        let Some(index) = targets
            .iter()
            .position(|(target, _)| *target == self.selected)
        else {
            return false;
        };
        let next = index.saturating_add_signed(delta);
        if next >= targets.len() || next == index {
            return false;
        }
        self.selected = targets[next].0;
        true
    }

    fn selected_label(&self) -> Option<String> {
        self.targets()
            .into_iter()
            .find(|(target, _)| *target == self.selected)
            .map(|(_, label)| label)
    }

    fn choose(&self) -> ViewAction {
        match self.selected {
            TmuxConnectionTarget::Terminal => ViewAction::ActivateTerminal,
            TmuxConnectionTarget::Connection(connection_id) => {
                ViewAction::ActivateTmuxConnection(connection_id)
            }
        }
    }

    fn edit(&mut self, sr: &mut ScreenReader, action: EditorAction) -> Result<ViewAction> {
        match action {
            EditorAction::Changed => {
                self.reconcile_selection(None);
                self.render();
                if let Some(label) = self.selected_label() {
                    sr.speak(&label, false)?;
                }
                Ok(ViewAction::Redraw)
            }
            EditorAction::Submit => Ok(self.choose()),
            EditorAction::Bell => Ok(ViewAction::Bell),
            EditorAction::None => Ok(ViewAction::None),
        }
    }

    fn move_and_announce(&mut self, sr: &mut ScreenReader, delta: isize) -> Result<ViewAction> {
        if !self.move_selection(delta) {
            return Ok(ViewAction::Bell);
        }
        self.render();
        if let Some(label) = self.selected_label() {
            sr.speak(&label, false)?;
        }
        Ok(ViewAction::Redraw)
    }

    fn render(&mut self) {
        let (rows, cols) = self.view.size();
        let mut lines = vec![format!("search: {}", self.editor.input())];
        let capacity = usize::from(rows).saturating_sub(2);
        let targets = self.targets();
        let selected_index = targets
            .iter()
            .position(|(target, _)| *target == self.selected);
        let max_start = targets.len().saturating_sub(capacity);
        self.viewport_start = self.viewport_start.min(max_start);
        if let Some(selected_index) = selected_index {
            if selected_index < self.viewport_start {
                self.viewport_start = selected_index;
            } else if selected_index >= self.viewport_start.saturating_add(capacity) {
                self.viewport_start = selected_index
                    .saturating_add(1)
                    .saturating_sub(capacity)
                    .min(max_start);
            }
        }
        if targets.is_empty() && capacity > 0 {
            lines.push("no matching connections".to_owned());
        } else {
            lines.extend(
                targets
                    .into_iter()
                    .skip(self.viewport_start)
                    .take(capacity)
                    .map(|(target, label)| {
                        format!(
                            "{} {label}",
                            if target == self.selected { ">" } else { " " }
                        )
                    }),
            );
        }
        if rows > 1 {
            lines.push("Up/Down select, Enter choose, Escape cancel".to_owned());
        }
        render_lines(&mut self.view, &lines, cols);
    }
}

impl ViewController for TmuxConnectionChooserView {
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
        "tmux connections"
    }

    fn kind(&self) -> ViewKind {
        ViewKind::TmuxConnectionChooser
    }

    fn handle_input(
        &mut self,
        sr: &mut ScreenReader,
        input: &[u8],
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        match input {
            b"\x1b" => Ok(ViewAction::Pop),
            b"\r" | b"\n" => Ok(self.choose()),
            b"\x1b[A" => self.move_and_announce(sr, -1),
            b"\x1b[B" => self.move_and_announce(sr, 1),
            _ => {
                let action = self.editor.handle_bytes(input);
                self.edit(sr, action)
            }
        }
    }

    fn handle_key_input(
        &mut self,
        sr: &mut ScreenReader,
        key: &KeyInput,
        _raw: &[u8],
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        if key.is_release() {
            return Ok(ViewAction::None);
        }
        match key.event().code {
            KeyCode::Esc => Ok(ViewAction::Pop),
            KeyCode::Enter => Ok(self.choose()),
            KeyCode::Up => self.move_and_announce(sr, -1),
            KeyCode::Down => self.move_and_announce(sr, 1),
            _ => {
                let action = self.editor.handle_key_input(key);
                self.edit(sr, action)
            }
        }
    }

    fn handle_paste(
        &mut self,
        sr: &mut ScreenReader,
        contents: &str,
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        let action = self.editor.handle_text(contents);
        self.edit(sr, action)
    }

    fn on_resize(&mut self, rows: u16, cols: u16) {
        self.view.set_size(rows, cols);
        self.render();
    }
}

pub struct TmuxConnectionRenameView {
    view: View,
    connection_id: u64,
    editor: LineEditor,
}

impl TmuxConnectionRenameView {
    #[must_use]
    pub fn new(rows: u16, cols: u16, connection_id: u64) -> Self {
        let mut rename = Self {
            view: View::new(rows, cols),
            connection_id,
            editor: LineEditor::new(),
        };
        rename.render();
        rename
    }

    fn edit(&mut self, action: EditorAction) -> ViewAction {
        match action {
            EditorAction::Changed => {
                self.render();
                ViewAction::Redraw
            }
            EditorAction::Submit => ViewAction::TmuxConnectionRename {
                connection_id: self.connection_id,
                label: self.editor.input().to_owned(),
            },
            EditorAction::Bell => ViewAction::Bell,
            EditorAction::None => ViewAction::None,
        }
    }

    fn render(&mut self) {
        let (_, cols) = self.view.size();
        render_lines(
            &mut self.view,
            &[
                format!("new label: {}", self.editor.input()),
                "Enter rename, Escape cancel".to_owned(),
            ],
            cols,
        );
    }
}

impl ViewController for TmuxConnectionRenameView {
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
        "rename tmux connection"
    }

    fn kind(&self) -> ViewKind {
        ViewKind::TmuxConnectionRename
    }

    fn handle_input(
        &mut self,
        _sr: &mut ScreenReader,
        input: &[u8],
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        Ok(match input {
            b"\x1b" => ViewAction::Pop,
            _ => {
                let action = self.editor.handle_bytes(input);
                self.edit(action)
            }
        })
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
        Ok(if key.event().code == KeyCode::Esc {
            ViewAction::Pop
        } else {
            let action = self.editor.handle_key_input(key);
            self.edit(action)
        })
    }

    fn handle_paste(
        &mut self,
        _sr: &mut ScreenReader,
        contents: &str,
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        let action = self.editor.handle_text(contents);
        Ok(self.edit(action))
    }

    fn on_resize(&mut self, rows: u16, cols: u16) {
        self.view.set_size(rows, cols);
        self.render();
    }
}

fn render_lines(view: &mut View, lines: &[String], cols: u16) {
    let mut bytes = b"\x1b[2J\x1b[H".to_vec();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            bytes.extend_from_slice(b"\r\n");
        }
        bytes.extend_from_slice(
            super::text_input::truncate_display_width(line, usize::from(cols)).as_bytes(),
        );
    }
    view.clear_update_summary();
    view.process_changes(&bytes);
    view.clear_update_summary();
}

#[cfg(test)]
mod tests {
    use super::{
        TmuxConnectionChooserView, TmuxConnectionItem, TmuxConnectionRenameView, ViewAction,
        ViewController,
    };
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

    fn items() -> Vec<TmuxConnectionItem> {
        vec![
            TmuxConnectionItem {
                connection_id: 1,
                label: "shared".to_owned(),
            },
            TmuxConnectionItem {
                connection_id: 2,
                label: "shared".to_owned(),
            },
        ]
    }

    #[test]
    fn chooser_uses_stable_ids_for_duplicate_labels_and_reconciles_removal() {
        let mut chooser = TmuxConnectionChooserView::new(8, 60, items(), Some(2));
        let mut sr = screen_reader();
        let mut output = Vec::new();
        let contents = chooser.model().contents_full();
        assert!(contents.contains("connection 1 shared"), "{contents:?}");
        assert!(contents.contains("connection 2 shared"), "{contents:?}");

        assert!(matches!(
            chooser
                .handle_input(&mut sr, b"\x1b[A", &mut output)
                .unwrap(),
            ViewAction::Redraw
        ));
        assert!(matches!(
            chooser.handle_input(&mut sr, b"\r", &mut output).unwrap(),
            ViewAction::ActivateTmuxConnection(1)
        ));

        chooser.sync(items().into_iter().skip(1).collect(), Some(2));
        assert!(matches!(
            chooser.handle_input(&mut sr, b"\r", &mut output).unwrap(),
            ViewAction::ActivateTmuxConnection(2)
        ));
        chooser.handle_input(&mut sr, b"term", &mut output).unwrap();
        assert!(matches!(
            chooser.handle_input(&mut sr, b"\r", &mut output).unwrap(),
            ViewAction::ActivateTerminal
        ));
    }

    #[test]
    fn rename_editor_returns_the_stable_connection_id_and_exact_input() {
        let mut rename = TmuxConnectionRenameView::new(4, 40, 27);
        let mut sr = screen_reader();
        let mut output = Vec::new();
        assert!(matches!(
            rename
                .handle_input(&mut sr, b"remote work", &mut output)
                .unwrap(),
            ViewAction::Redraw
        ));
        assert!(matches!(
            rename.handle_input(&mut sr, b"\r", &mut output).unwrap(),
            ViewAction::TmuxConnectionRename {
                connection_id: 27,
                label
            } if label == "remote work"
        ));
    }
}
