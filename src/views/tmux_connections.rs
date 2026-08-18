use super::{Result, ViewAction, ViewController, ViewKind};
use crate::{
    line_editor::{EditorAction, LineEditor},
    screen_reader::ScreenReader,
    terminal_input::KeyInput,
    view::View,
};
use std::{any::Any, io::Write};
use terminput::KeyCode;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TmuxConnectionItem {
    pub connection_id: u64,
    pub label: String,
    pub host: Option<String>,
}

pub struct TmuxConnectionChooserView {
    view: View,
    items: Vec<TmuxConnectionItem>,
    active_connection_id: Option<u64>,
    selected_connection_id: Option<u64>,
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
            active_connection_id: active_connection,
            selected_connection_id: active_connection,
            viewport_start: 0,
        };
        chooser.reconcile_selection(active_connection);
        chooser.render();
        chooser
    }

    pub fn sync(&mut self, items: Vec<TmuxConnectionItem>, active_connection: Option<u64>) {
        self.items = items;
        self.active_connection_id = active_connection;
        self.reconcile_selection(active_connection);
        self.render();
    }

    fn targets(&self) -> Vec<(u64, String)> {
        self.items
            .iter()
            .map(|item| {
                let active_marker = if Some(item.connection_id) == self.active_connection_id {
                    "* "
                } else {
                    "  "
                };
                let default_label = format!("tmux {}", item.connection_id);
                let mut fields = vec![item.connection_id.to_string()];
                if item.label != default_label {
                    fields.push(item.label.clone());
                }
                if let Some(host) = item.host.as_deref() {
                    fields.push(host.to_owned());
                }
                (
                    item.connection_id,
                    format!("{active_marker}{}", fields.join(", ")),
                )
            })
            .collect()
    }

    fn reconcile_selection(&mut self, active_connection: Option<u64>) {
        let targets = self.targets();
        if self.selected_connection_id.is_some_and(|selected| {
            targets
                .iter()
                .any(|(connection_id, _)| *connection_id == selected)
        }) {
            return;
        }
        self.selected_connection_id = targets
            .iter()
            .find(|(connection_id, _)| Some(*connection_id) == active_connection)
            .or_else(|| targets.first())
            .map(|(connection_id, _)| *connection_id);
    }

    fn move_selection(&mut self, delta: isize) -> bool {
        let targets = self.targets();
        let Some(index) = targets
            .iter()
            .position(|(connection_id, _)| Some(*connection_id) == self.selected_connection_id)
        else {
            return false;
        };
        let next = index.saturating_add_signed(delta);
        if next >= targets.len() || next == index {
            return false;
        }
        self.selected_connection_id = Some(targets[next].0);
        true
    }

    fn selected_label(&self) -> Option<String> {
        self.targets()
            .into_iter()
            .find(|(connection_id, _)| Some(*connection_id) == self.selected_connection_id)
            .map(|(_, label)| label)
    }

    fn choose(&self) -> ViewAction {
        self.selected_connection_id
            .map_or(ViewAction::Bell, ViewAction::ActivateTmuxConnection)
    }

    fn control(&self, action: crate::tmux_lifecycle::GatewayControlAction) -> ViewAction {
        self.selected_connection_id
            .map_or(ViewAction::Bell, |connection_id| {
                ViewAction::TmuxConnectionControl {
                    connection_id,
                    action,
                }
            })
    }

    fn key_action(&self, character: u8) -> Option<ViewAction> {
        use crate::tmux_lifecycle::GatewayControlAction;
        Some(match character {
            b'd' => self.control(GatewayControlAction::GracefulDetach),
            b'D' => self.control(GatewayControlAction::ForceAbandon),
            _ => return None,
        })
    }

    fn move_and_announce(&mut self, sr: &mut ScreenReader, delta: isize) -> Result<ViewAction> {
        if !self.move_selection(delta) {
            return Ok(ViewAction::Bell);
        }
        self.render();
        if let Some(label) = self.selected_label() {
            sr.speak(&label, false)?;
        }
        // The row was announced explicitly above. Normal redraw autoread also
        // reports the indentation change between an active `* ` row and an
        // inactive row's two leading spaces.
        Ok(ViewAction::RedrawSilently)
    }

    fn render(&mut self) {
        let (rows, cols) = self.view.size();
        let mut lines = Vec::new();
        let capacity = usize::from(rows).saturating_sub(1);
        let targets = self.targets();
        let selected_index = targets
            .iter()
            .position(|(connection_id, _)| Some(*connection_id) == self.selected_connection_id);
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
            lines.push("no tmux connections".to_owned());
        } else {
            lines.extend(
                targets
                    .into_iter()
                    .skip(self.viewport_start)
                    .take(capacity)
                    .map(|(_, label)| label),
            );
        }
        if rows > 1 {
            lines.push(
                "Up/Down select, Enter switch, d detach, D expose raw transport, Escape cancel"
                    .to_owned(),
            );
        }
        let cursor = selected_index
            .filter(|index| {
                *index >= self.viewport_start
                    && *index < self.viewport_start.saturating_add(capacity)
            })
            .map(|index| {
                (
                    index.saturating_sub(self.viewport_start).saturating_add(1),
                    // Keep the cursor on the selected row without covering the
                    // active connection's leading `*` with a block cursor.
                    2,
                )
            });
        render_lines(&mut self.view, &lines, cols, cursor);
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
            [character] if self.key_action(*character).is_some() => {
                Ok(self.key_action(*character).expect("checked manager action"))
            }
            _ => Ok(ViewAction::Bell),
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
            KeyCode::Char(character) if character.is_ascii() => {
                Ok(self.key_action(character as u8).unwrap_or(ViewAction::Bell))
            }
            _ => Ok(ViewAction::Bell),
        }
    }

    fn handle_paste(
        &mut self,
        sr: &mut ScreenReader,
        contents: &str,
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        let _ = (sr, contents);
        Ok(ViewAction::Bell)
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
            None,
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

fn render_lines(view: &mut View, lines: &[String], cols: u16, cursor: Option<(usize, usize)>) {
    let mut bytes = b"\x1b[2J\x1b[H".to_vec();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            bytes.extend_from_slice(b"\r\n");
        }
        bytes.extend_from_slice(
            super::text_input::truncate_display_width(line, usize::from(cols)).as_bytes(),
        );
    }
    if let Some((row, col)) = cursor {
        bytes.extend_from_slice(format!("\x1b[{};{}H", row.max(1), col.max(1)).as_bytes());
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
                host: Some("local.example".to_owned()),
            },
            TmuxConnectionItem {
                connection_id: 2,
                label: "shared".to_owned(),
                host: Some("remote.example".to_owned()),
            },
        ]
    }

    #[test]
    fn chooser_uses_stable_ids_for_duplicate_labels_and_reconciles_removal() {
        let mut chooser = TmuxConnectionChooserView::new(8, 60, items(), Some(2));
        let mut sr = screen_reader();
        let mut output = Vec::new();
        let contents = chooser.model().contents_full();
        assert!(
            contents.contains("1, shared, local.example"),
            "{contents:?}"
        );
        assert!(
            contents.contains("2, shared, remote.example"),
            "{contents:?}"
        );
        assert!(
            contents.contains("* 2, shared, remote.example"),
            "{contents:?}"
        );
        assert!(
            !contents.contains("> "),
            "selector marker remained: {contents:?}"
        );
        let cursor_row = chooser.model().screen().cursor_position().0;
        assert_eq!(chooser.model().screen().cursor_position().1, 1);
        assert!(
            chooser
                .model()
                .line(cursor_row)
                .contains("2, shared, remote.example")
        );

        assert!(matches!(
            chooser
                .handle_input(&mut sr, b"\x1b[A", &mut output)
                .unwrap(),
            ViewAction::RedrawSilently
        ));
        let cursor_row = chooser.model().screen().cursor_position().0;
        assert!(
            chooser
                .model()
                .line(cursor_row)
                .contains("1, shared, local.example")
        );
        assert!(matches!(
            chooser.handle_input(&mut sr, b"\r", &mut output).unwrap(),
            ViewAction::ActivateTmuxConnection(1)
        ));
        assert!(matches!(
            chooser.handle_input(&mut sr, b"d", &mut output).unwrap(),
            ViewAction::TmuxConnectionControl {
                connection_id: 1,
                action: crate::tmux_lifecycle::GatewayControlAction::GracefulDetach,
            }
        ));

        chooser.sync(items().into_iter().skip(1).collect(), Some(2));
        assert!(matches!(
            chooser.handle_input(&mut sr, b"\r", &mut output).unwrap(),
            ViewAction::ActivateTmuxConnection(2)
        ));
        assert!(matches!(
            chooser.handle_input(&mut sr, b"D", &mut output).unwrap(),
            ViewAction::TmuxConnectionControl {
                connection_id: 2,
                action: crate::tmux_lifecycle::GatewayControlAction::ForceAbandon,
            }
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
