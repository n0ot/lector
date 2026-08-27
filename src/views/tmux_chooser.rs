use super::{
    Result, ViewAction, ViewController, ViewKind,
    text_input::{truncate_display_width, visible_input_window},
};
use crate::{
    line_editor::{EditorAction, LineEditor},
    screen_reader::ScreenReader,
    terminal_input::KeyInput,
    tmux_model::{PaneId, SessionId, TmuxTopology, WindowId},
    view::View,
};
use std::{any::Any, io::Write};
use terminput::KeyCode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TmuxChooserTarget {
    Session(SessionId),
    Window(WindowId),
    Pane(PaneId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TmuxChooserScope {
    Sessions,
    Windows(SessionId),
    Panes(WindowId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ChooserItem {
    target: TmuxChooserTarget,
    label: String,
}

pub struct TmuxChooserView {
    view: View,
    title: String,
    connection_id: u64,
    scope: TmuxChooserScope,
    items: Vec<ChooserItem>,
    editor: LineEditor,
    selected: Option<TmuxChooserTarget>,
    viewport_start: usize,
}

impl TmuxChooserView {
    #[must_use]
    pub fn sessions(rows: u16, cols: u16, connection_id: u64, topology: &TmuxTopology) -> Self {
        Self::new(
            rows,
            cols,
            connection_id,
            "tmux sessions",
            TmuxChooserScope::Sessions,
            topology.attached_session().map(TmuxChooserTarget::Session),
            topology,
        )
    }

    #[must_use]
    pub fn windows(rows: u16, cols: u16, connection_id: u64, topology: &TmuxTopology) -> Self {
        let session_id = topology.attached_session().unwrap_or(SessionId(u64::MAX));
        let selected = topology
            .session(session_id)
            .and_then(|session| session.active_window)
            .map(TmuxChooserTarget::Window);
        Self::new(
            rows,
            cols,
            connection_id,
            "tmux windows",
            TmuxChooserScope::Windows(session_id),
            selected,
            topology,
        )
    }

    #[must_use]
    pub fn panes(rows: u16, cols: u16, connection_id: u64, topology: &TmuxTopology) -> Self {
        let window_id = topology
            .attached_session()
            .and_then(|session_id| topology.session(session_id))
            .and_then(|session| session.active_window)
            .unwrap_or(WindowId(u64::MAX));
        let selected = topology
            .window(window_id)
            .and_then(|window| window.active_pane)
            .map(TmuxChooserTarget::Pane);
        Self::new(
            rows,
            cols,
            connection_id,
            "tmux panes",
            TmuxChooserScope::Panes(window_id),
            selected,
            topology,
        )
    }

    fn new(
        rows: u16,
        cols: u16,
        connection_id: u64,
        title: &str,
        scope: TmuxChooserScope,
        selected: Option<TmuxChooserTarget>,
        topology: &TmuxTopology,
    ) -> Self {
        let mut chooser = Self {
            view: View::new(rows, cols),
            title: title.to_owned(),
            connection_id,
            scope,
            items: Vec::new(),
            editor: LineEditor::new(),
            selected,
            viewport_start: 0,
        };
        chooser.sync_topology(topology);
        chooser
    }

    #[must_use]
    pub fn connection_id(&self) -> u64 {
        self.connection_id
    }

    #[must_use]
    pub fn selected_target(&self) -> Option<TmuxChooserTarget> {
        self.selected
    }

    pub fn sync_topology(&mut self, topology: &TmuxTopology) {
        self.items = match self.scope {
            TmuxChooserScope::Sessions => topology
                .sessions()
                .values()
                .map(|session| ChooserItem {
                    target: TmuxChooserTarget::Session(session.id),
                    label: format!("{}{} {}", '$', session.id.0, session.name),
                })
                .collect(),
            TmuxChooserScope::Windows(session_id) => topology
                .session(session_id)
                .into_iter()
                .flat_map(|session| &session.windows)
                .filter(|(index, window_id)| {
                    !topology.is_internal_window_link(session_id, **index, **window_id)
                })
                .filter_map(|(index, window_id)| {
                    let window = topology.window(*window_id)?;
                    Some(ChooserItem {
                        target: TmuxChooserTarget::Window(*window_id),
                        label: format!("@{} {index} {}", window_id.0, window.name),
                    })
                })
                .collect(),
            TmuxChooserScope::Panes(window_id) => {
                let mut panes = topology
                    .panes()
                    .values()
                    .filter(|pane| pane.window_id == window_id)
                    .map(|pane| ChooserItem {
                        target: TmuxChooserTarget::Pane(pane.id),
                        label: format!("%{} {} {}", pane.id.0, pane.index, pane.title),
                    })
                    .collect::<Vec<_>>();
                panes.sort_by_key(|item| match item.target {
                    TmuxChooserTarget::Pane(pane_id) => topology
                        .pane(pane_id)
                        .map_or((u32::MAX, u64::MAX), |pane| (pane.index, pane.id.0)),
                    _ => (u32::MAX, u64::MAX),
                });
                panes
            }
        };
        self.reconcile_selection();
        self.render();
    }

    fn matching_items(&self) -> Vec<&ChooserItem> {
        let query = self.editor.input().to_lowercase();
        self.items
            .iter()
            .filter(|item| query.is_empty() || item.label.to_lowercase().contains(&query))
            .collect()
    }

    fn reconcile_selection(&mut self) {
        let selected = self.selected;
        let next = {
            let matching = self.matching_items();
            selected
                .filter(|target| matching.iter().any(|item| item.target == *target))
                .or_else(|| matching.first().map(|item| item.target))
        };
        self.selected = next;
    }

    fn move_selection(&mut self, delta: isize) -> bool {
        let matching = self.matching_items();
        let Some(current) = self.selected else {
            self.selected = matching.first().map(|item| item.target);
            return self.selected.is_some();
        };
        let Some(index) = matching.iter().position(|item| item.target == current) else {
            self.selected = matching.first().map(|item| item.target);
            return self.selected.is_some();
        };
        let next = index.saturating_add_signed(delta);
        if next >= matching.len() || next == index {
            return false;
        }
        self.selected = Some(matching[next].target);
        true
    }

    fn selected_label(&self) -> Option<&str> {
        let selected = self.selected?;
        self.items
            .iter()
            .find(|item| item.target == selected)
            .map(|item| item.label.as_str())
    }

    fn choose(&self) -> ViewAction {
        self.selected
            .map_or(ViewAction::Bell, |target| ViewAction::TmuxChooserSelect {
                connection_id: self.connection_id,
                target,
            })
    }

    fn apply_editor_action(
        &mut self,
        sr: &mut ScreenReader,
        action: EditorAction,
    ) -> Result<ViewAction> {
        match action {
            EditorAction::Changed => {
                self.reconcile_selection();
                self.render();
                if let Some(label) = self.selected_label() {
                    sr.speak(label, false)?;
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
            sr.speak(label, false)?;
        }
        Ok(ViewAction::Redraw)
    }

    fn empty_text(&self) -> &'static str {
        match self.scope {
            TmuxChooserScope::Sessions => "no matching sessions",
            TmuxChooserScope::Windows(_) => "no matching windows",
            TmuxChooserScope::Panes(_) => "no matching panes",
        }
    }

    fn render(&mut self) {
        let (rows, cols) = self.view.size();
        let (visible_query, query_cursor_width) = visible_input_window(
            self.editor.input(),
            self.editor.cursor(),
            usize::from(cols).saturating_sub(8),
        );
        let mut lines = vec![format!("search: {visible_query}")];
        let item_capacity = usize::from(rows).saturating_sub(2);
        let (matching_len, selected_index) = {
            let matching = self.matching_items();
            (
                matching.len(),
                self.selected
                    .and_then(|selected| matching.iter().position(|item| item.target == selected)),
            )
        };
        if item_capacity == 0 {
            self.viewport_start = 0;
        } else {
            let max_start = matching_len.saturating_sub(item_capacity);
            self.viewport_start = self.viewport_start.min(max_start);
            if let Some(selected_index) = selected_index {
                if selected_index < self.viewport_start {
                    self.viewport_start = selected_index;
                } else if selected_index >= self.viewport_start.saturating_add(item_capacity) {
                    self.viewport_start = selected_index
                        .saturating_add(1)
                        .saturating_sub(item_capacity)
                        .min(max_start);
                }
            }
        }
        let matching = self.matching_items();
        if matching.is_empty() && item_capacity > 0 {
            lines.push(self.empty_text().to_owned());
        } else {
            for item in matching
                .into_iter()
                .skip(self.viewport_start)
                .take(item_capacity)
            {
                lines.push(item.label.clone());
            }
        }
        if rows > 1 {
            lines.push("Up/Down select, Enter choose, Escape cancel".to_owned());
        }
        let mut bytes = b"\x1b[2J\x1b[H".to_vec();
        for (index, line) in lines.into_iter().take(usize::from(rows)).enumerate() {
            if index > 0 {
                bytes.extend_from_slice(b"\r\n");
            }
            bytes.extend_from_slice(truncate_display_width(&line, usize::from(cols)).as_bytes());
        }
        let selected_cursor_row = selected_index
            .filter(|index| {
                *index >= self.viewport_start
                    && *index < self.viewport_start.saturating_add(item_capacity)
            })
            .map(|index| index.saturating_sub(self.viewport_start).saturating_add(2));
        let (cursor_row, cursor_col) = selected_cursor_row.map_or_else(
            || {
                (
                    1,
                    query_cursor_width
                        .saturating_add(9)
                        .min(usize::from(cols))
                        .max(1),
                )
            },
            |row| (row, 1),
        );
        bytes.extend_from_slice(format!("\x1b[{cursor_row};{cursor_col}H").as_bytes());
        self.view.clear_update_summary();
        self.view.process_changes(&bytes);
        self.view.clear_update_summary();
    }
}

impl ViewController for TmuxChooserView {
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
        ViewKind::TmuxChooser
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
                self.apply_editor_action(sr, action)
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
                self.apply_editor_action(sr, action)
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
        self.apply_editor_action(sr, action)
    }

    fn on_resize(&mut self, rows: u16, cols: u16) {
        self.view.set_size(rows, cols);
        self.render();
    }
}
