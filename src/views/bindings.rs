use super::{Result, ReviewView, ViewAction, ViewController, ViewKind};
use crate::{
    keymap::{InputMode, KeyBindings},
    review::{document::ReviewDocument, navigation::SharedNavigation, parser::HELP_BINDINGS},
    screen_reader::ScreenReader,
    terminal::HistoryPosition,
    terminal_input::KeyInput,
    view::View,
};
use std::{
    any::Any,
    cell::{Cell, RefCell},
    io::Write,
    ops::Range,
    rc::Rc,
};

#[derive(Clone, Debug, Eq, PartialEq)]
struct Entry {
    id: String,
    text: String,
}

struct SessionState {
    navigation: SharedNavigation,
    entries: Vec<Entry>,
    spans: Vec<Range<usize>>,
    cols: u16,
    cursor: HistoryPosition,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            navigation: SharedNavigation::default(),
            entries: Vec::new(),
            spans: Vec::new(),
            cols: 0,
            cursor: HistoryPosition { row: 0, col: 0 },
        }
    }
}

/// This generated document owns memory independently of every terminal/pane.
#[derive(Clone, Default)]
pub(crate) struct BindingsSession(Rc<RefCell<SessionState>>);

pub(crate) struct BindingsView {
    closed: bool,
    review: ReviewView,
    session: BindingsSession,
    revision: Rc<Cell<u64>>,
    rendered_revision: u64,
    entries: Vec<Entry>,
    spans: Vec<Range<usize>>,
    cols: u16,
}

fn entries(bindings: &KeyBindings) -> Vec<Entry> {
    let mut entries = Vec::new();
    for mode in [InputMode::Normal, InputMode::Table, InputMode::TableSetup] {
        entries.push(Entry {
            id: format!("heading:{}", mode.as_str()),
            text: format!("{} bindings", mode.as_str()),
        });
        let mut groups: Vec<(String, Entry)> = Vec::new();
        for binding in bindings
            .describe_effective(mode)
            .into_iter()
            .filter(|binding| !binding.inherited)
        {
            if let Some((_, entry)) = groups.iter_mut().find(|(id, _)| *id == binding.action_id) {
                entry.text.push_str(&format!(", {}", binding.key));
            } else {
                // Builtins retain their row when shortcuts are added or removed.
                // Custom rows retain their key across replacement callbacks.
                let id = if binding.action_id.starts_with("builtin:") {
                    binding.action_id.clone()
                } else {
                    binding.key.clone()
                };
                groups.push((
                    binding.action_id,
                    Entry {
                        id: format!("{}:{id}", mode.as_str()),
                        text: format!("{}  {}", binding.help, binding.key),
                    },
                ));
            }
        }
        entries.extend(groups.into_iter().map(|(_, entry)| entry));
    }
    entries.push(Entry {
        id: "heading:review".into(),
        text: "Review keys and command syntax (when not intercepted by configured bindings)".into(),
    });
    for (key, help) in HELP_BINDINGS {
        entries.push(Entry {
            id: format!("review:{key}"),
            text: format!("{help}  {key}"),
        });
    }
    entries
}

fn make_document(entries: &[Entry], cols: u16) -> (ReviewDocument, Vec<Range<usize>>) {
    ReviewDocument::from_lines(
        &entries
            .iter()
            .map(|entry| entry.text.clone())
            .collect::<Vec<_>>(),
        cols,
    )
}

fn relocate(
    old_entries: &[Entry],
    old_spans: &[Range<usize>],
    old_cols: u16,
    cursor: HistoryPosition,
    new_entries: &[Entry],
    new_spans: &[Range<usize>],
    new_cols: u16,
) -> HistoryPosition {
    let Some(old_index) = old_spans.iter().position(|span| span.contains(&cursor.row)) else {
        return HistoryPosition { row: 0, col: 0 };
    };
    let find = |old: usize| {
        new_entries
            .iter()
            .position(|entry| entry.id == old_entries[old].id)
    };
    let same = find(old_index);
    let target = same
        .or_else(|| (old_index + 1..old_entries.len()).find_map(find))
        .or_else(|| (0..old_index).rev().find_map(find))
        .unwrap_or(0);
    let span = &new_spans[target];
    let offset = if same.is_some() {
        (cursor.row - old_spans[old_index].start) * usize::from(old_cols.max(2))
            + usize::from(cursor.col)
    } else {
        0
    };
    let width = usize::from(new_cols.max(2));
    HistoryPosition {
        row: (span.start + offset / width).min(span.end - 1),
        col: (offset % width) as u16,
    }
}

impl BindingsView {
    pub(crate) fn new(rows: u16, cols: u16, session: BindingsSession, sr: &ScreenReader) -> Self {
        let entries = entries(sr.key_bindings());
        let (document, spans) = make_document(&entries, cols);
        let state = session.0.borrow();
        let cursor = relocate(
            &state.entries,
            &state.spans,
            state.cols,
            state.cursor,
            &entries,
            &spans,
            cols,
        );
        if state.entries != entries || state.cols != cols {
            state.navigation.borrow_mut().invalidate_from(0);
        }
        let review = ReviewView::new_generated(
            rows,
            cols,
            "Bindings",
            document,
            state.navigation.clone(),
            cursor,
        );
        drop(state);
        let revision = sr.key_bindings().revision_observer();
        let rendered_revision = revision.get();
        Self {
            closed: false,
            review,
            session,
            revision,
            rendered_revision,
            entries,
            spans,
            cols,
        }
    }

    fn refresh(&mut self, sr: &ScreenReader) -> bool {
        if !self.wants_tick() {
            return false;
        }
        self.rendered_revision = self.revision.get();
        let updated = entries(sr.key_bindings());
        if updated == self.entries {
            return false;
        }
        self.replace_entries(updated, self.cols);
        true
    }

    fn replace_entries(&mut self, entries: Vec<Entry>, cols: u16) {
        let (document, spans) = make_document(&entries, cols);
        let cursor = relocate(
            &self.entries,
            &self.spans,
            self.cols,
            self.review.document_cursor(),
            &entries,
            &spans,
            cols,
        );
        self.session
            .0
            .borrow()
            .navigation
            .borrow_mut()
            .invalidate_from(0);
        self.review.replace_generated_document(document, cursor);
        self.entries = entries;
        self.spans = spans;
        self.cols = cols;
    }

    fn after_refresh(action: ViewAction, refreshed: bool) -> ViewAction {
        if refreshed && matches!(action, ViewAction::None) {
            ViewAction::Redraw
        } else {
            action
        }
    }
}

impl ViewController for BindingsView {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
    fn model(&mut self) -> &mut View {
        self.review.model()
    }
    fn title(&self) -> &str {
        "Bindings"
    }
    fn kind(&self) -> ViewKind {
        ViewKind::Review
    }
    fn on_close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        let mut state = self.session.0.borrow_mut();
        state.cursor = self.review.document_cursor();
        state.entries = self.entries.clone();
        state.spans = self.spans.clone();
        state.cols = self.cols;
    }
    fn wants_tick(&self) -> bool {
        self.revision.get() != self.rendered_revision
    }
    fn tick(&mut self, sr: &mut ScreenReader, _: &mut dyn Write) -> Result<ViewAction> {
        Ok(if self.refresh(sr) {
            ViewAction::Redraw
        } else {
            ViewAction::None
        })
    }
    fn place_application_cursor_at_review_cursor(&mut self) -> Option<ViewAction> {
        self.review.place_application_cursor_at_review_cursor()
    }
    fn handle_input(
        &mut self,
        sr: &mut ScreenReader,
        input: &[u8],
        output: &mut dyn Write,
    ) -> Result<ViewAction> {
        let refreshed = self.refresh(sr);
        Ok(Self::after_refresh(
            self.review.handle_input(sr, input, output)?,
            refreshed,
        ))
    }
    fn handle_key_input(
        &mut self,
        sr: &mut ScreenReader,
        key: &KeyInput,
        raw: &[u8],
        output: &mut dyn Write,
    ) -> Result<ViewAction> {
        let refreshed = self.refresh(sr);
        Ok(Self::after_refresh(
            self.review.handle_key_input(sr, key, raw, output)?,
            refreshed,
        ))
    }
    fn handle_paste(
        &mut self,
        sr: &mut ScreenReader,
        contents: &str,
        output: &mut dyn Write,
    ) -> Result<ViewAction> {
        let refreshed = self.refresh(sr);
        Ok(Self::after_refresh(
            self.review.handle_paste(sr, contents, output)?,
            refreshed,
        ))
    }
    fn on_resize(&mut self, rows: u16, cols: u16) {
        self.review.on_resize(rows, cols);
        if !self.closed && cols != self.cols {
            self.replace_entries(self.entries.clone(), cols);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{commands::Action, speech};

    struct RecordingDriver(Rc<RefCell<Vec<String>>>);
    impl speech::Driver for RecordingDriver {
        fn speak(&mut self, text: &str, _: bool) -> anyhow::Result<()> {
            self.0.borrow_mut().push(text.into());
            Ok(())
        }
        fn stop(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn get_rate(&self) -> f32 {
            1.0
        }
        fn set_rate(&mut self, _: f32) -> anyhow::Result<()> {
            Ok(())
        }
    }
    fn setup() -> (ScreenReader, Rc<RefCell<Vec<String>>>) {
        let spoken = Rc::new(RefCell::new(Vec::new()));
        (
            ScreenReader::new(speech::Speech::new(Box::new(RecordingDriver(
                spoken.clone(),
            )))),
            spoken,
        )
    }
    fn input(view: &mut BindingsView, sr: &mut ScreenReader, text: &[u8]) {
        view.handle_input(sr, text, &mut Vec::new()).unwrap();
    }
    fn jump(view: &mut BindingsView, sr: &mut ScreenReader, id: &str) {
        let index = view
            .entries
            .iter()
            .position(|entry| entry.id == id)
            .unwrap();
        input(
            view,
            sr,
            format!("{}G", view.spans[index].start + 1).as_bytes(),
        );
    }
    fn current_id(view: &BindingsView) -> &str {
        let row = view.review.document_cursor().row;
        let index = view
            .spans
            .iter()
            .position(|span| span.contains(&row))
            .unwrap();
        &view.entries[index].id
    }
    #[test]
    fn list_starts_with_bindings_and_groups_only_explicit_actions() {
        let entries = entries(&KeyBindings::new());
        assert_eq!(entries[0].id, "heading:normal");
        assert!(
            entries
                .iter()
                .all(|entry| !entry.text.contains("inherited"))
        );
        let backspace = entries
            .iter()
            .find(|entry| entry.id == "normal:builtin:backspace")
            .unwrap();
        assert_eq!(
            backspace.text,
            format!("{}  Backspace, C-h", Action::Backspace.help_text())
        );
        assert!(
            !entries
                .iter()
                .any(|entry| entry.id == "table:builtin:backspace")
        );
        let next = entries
            .iter()
            .find(|entry| entry.id == "table:builtin:table_row_next")
            .unwrap();
        assert_eq!(
            next.text,
            format!("{}  M-o, j", Action::TableRowNext.help_text())
        );
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.id == "review:gx")
                .unwrap()
                .text,
            "open the HTTP or HTTPS link under the Review cursor  gx"
        );
    }

    #[test]
    fn lua_aliases_group_by_callback_not_description() {
        let lua = Rc::new(mlua::Lua::new());
        let callback = lua.create_function(|_, ()| Ok(())).unwrap();
        let different = lua.create_function(|_, ()| Ok(())).unwrap();
        let mut bindings = KeyBindings::new();
        for (key, function) in [("a", callback.clone()), ("b", callback), ("c", different)] {
            bindings
                .set_lua_binding_for_mode(
                    InputMode::Normal,
                    key.into(),
                    "custom action".into(),
                    lua.clone(),
                    function,
                )
                .unwrap();
        }
        let entries = entries(&bindings);
        let custom: Vec<_> = entries
            .iter()
            .filter(|entry| entry.text.starts_with("custom action"))
            .collect();
        assert_eq!(custom.len(), 2);
        assert_eq!(custom[0].text, "custom action  a, b");
        assert_eq!(custom[1].text, "custom action  c");
    }

    #[test]
    fn removing_one_shortcut_preserves_the_action_row() {
        let (mut sr, _) = setup();
        let mut view = BindingsView::new(10, 80, BindingsSession::default(), &sr);
        jump(&mut view, &mut sr, "normal:builtin:backspace");
        sr.key_bindings_mut()
            .clear_binding_for_mode(InputMode::Normal, "Backspace");
        view.tick(&mut sr, &mut Vec::new()).unwrap();
        assert_eq!(current_id(&view), "normal:builtin:backspace");
        let row = view
            .entries
            .iter()
            .find(|entry| entry.id == current_id(&view))
            .unwrap();
        assert_eq!(row.text, format!("{}  C-h", Action::Backspace.help_text()));
    }

    #[test]
    fn retired_view_destruction_cannot_overwrite_a_newer_session() {
        let (mut sr, _) = setup();
        let session = BindingsSession::default();
        let mut old = BindingsView::new(10, 80, session.clone(), &sr);
        jump(&mut old, &mut sr, "review:gx");
        old.on_close();
        let mut current = BindingsView::new(10, 80, session.clone(), &sr);
        jump(&mut current, &mut sr, "normal:builtin:open_review");
        input(&mut current, &mut sr, b"ma");
        current.on_close();
        drop(current);
        // Presentation retirement may resize an older owner before dropping it.
        old.on_resize(8, 70);
        drop(old);
        let mut reopened = BindingsView::new(10, 80, session, &sr);
        assert_eq!(current_id(&reopened), "normal:builtin:open_review");
        input(&mut reopened, &mut sr, b"gg`a");
        assert_eq!(current_id(&reopened), "normal:builtin:open_review");
    }

    #[test]
    fn refresh_keeps_an_in_progress_search() {
        let (mut sr, _) = setup();
        let mut view = BindingsView::new(10, 80, BindingsSession::default(), &sr);
        input(&mut view, &mut sr, b"/open the ");
        sr.key_bindings_mut().set_builtin_binding_for_mode(
            InputMode::Normal,
            "aaa".into(),
            Action::OpenBindings,
        );
        view.tick(&mut sr, &mut Vec::new()).unwrap();
        input(&mut view, &mut sr, b"HTTP\r");
        assert_eq!(current_id(&view), "review:gx");
    }

    #[test]
    fn mutations_and_reload_preserve_entry_and_refresh_without_speech_or_idle_ticks() {
        let (mut sr, spoken) = setup();
        let mut view = BindingsView::new(10, 80, BindingsSession::default(), &sr);
        jump(&mut view, &mut sr, "normal:builtin:open_review");
        assert!(!view.wants_tick());
        sr.key_bindings_mut().set_builtin_binding_for_mode(
            InputMode::Normal,
            "aaa".into(),
            Action::OpenBindings,
        );
        spoken.borrow_mut().clear();
        assert!(view.wants_tick());
        assert!(matches!(
            view.tick(&mut sr, &mut Vec::new()).unwrap(),
            ViewAction::Redraw
        ));
        assert_eq!(current_id(&view), "normal:builtin:open_review");
        assert!(spoken.borrow().is_empty());
        assert!(!view.wants_tick());
        let mut replacement = KeyBindings::new();
        replacement.set_builtin_binding_for_mode(
            InputMode::Normal,
            "bbb".into(),
            Action::OpenBindings,
        );
        sr.key_bindings_mut().replace_from(replacement);
        assert!(view.wants_tick());
        view.tick(&mut sr, &mut Vec::new()).unwrap();
        assert!(
            view.entries
                .iter()
                .any(|entry| entry.text.ends_with(", bbb"))
        );
        assert!(
            !view
                .entries
                .iter()
                .any(|entry| entry.text.ends_with(", aaa"))
        );
        assert_eq!(current_id(&view), "normal:builtin:open_review");
        assert!(spoken.borrow().is_empty());
    }
    #[test]
    fn removed_entry_uses_next_survivor_and_resize_preserves_identity() {
        let (mut sr, _) = setup();
        sr.key_bindings_mut().set_builtin_binding_for_mode(
            InputMode::Normal,
            "aaa".into(),
            Action::OpenBindings,
        );
        let mut view = BindingsView::new(10, 80, BindingsSession::default(), &sr);
        jump(&mut view, &mut sr, "normal:builtin:open_bindings");
        let old_index = view
            .entries
            .iter()
            .position(|entry| entry.id == "normal:builtin:open_bindings")
            .unwrap();
        let next = view.entries[old_index + 1].id.clone();
        sr.key_bindings_mut()
            .clear_binding_for_mode(InputMode::Normal, "aaa");
        sr.key_bindings_mut()
            .clear_binding_for_mode(InputMode::Normal, "M-?");
        view.tick(&mut sr, &mut Vec::new()).unwrap();
        assert_eq!(current_id(&view), next);
        view.on_resize(6, 25);
        assert_eq!(current_id(&view), next);
    }
    #[test]
    fn generated_document_supports_search_yank_and_persistent_private_marks() {
        let (mut sr, _) = setup();
        let session = BindingsSession::default();
        let mut view = BindingsView::new(10, 80, session.clone(), &sr);
        input(&mut view, &mut sr, b"/open the HTTP\r");
        assert_eq!(current_id(&view), "review:gx");
        input(&mut view, &mut sr, b"0");
        input(&mut view, &mut sr, b"mayy");
        assert!(sr.clipboard_text().unwrap().contains("open the HTTP"));
        view.on_close();
        drop(view);
        let mut view = BindingsView::new(10, 80, session, &sr);
        input(&mut view, &mut sr, b"gg`a");
        assert_eq!(current_id(&view), "review:gx");
        input(&mut view, &mut sr, b"ggn");
        assert_eq!(current_id(&view), "review:gx");
        let mut independent = BindingsView::new(10, 80, BindingsSession::default(), &sr);
        input(&mut independent, &mut sr, b"`a");
        assert_eq!(
            independent.review.document_cursor(),
            HistoryPosition { row: 0, col: 0 }
        );
    }
}
