use crate::terminal_input::KeyInput;
use terminput::{KeyCode, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

pub struct LineEditor {
    input: String,
    cursor: usize,
    state: InputState,
    csi_buf: Vec<u8>,
    history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
}

#[derive(Copy, Clone)]
enum InputState {
    Normal,
    Esc,
    Csi,
    Ss3,
}

#[derive(Copy, Clone)]
pub enum EditorAction {
    None,
    Changed,
    Submit,
    Bell,
}

impl Default for LineEditor {
    fn default() -> Self {
        Self::new()
    }
}

impl LineEditor {
    pub fn new() -> Self {
        Self {
            input: String::new(),
            cursor: 0,
            state: InputState::Normal,
            csi_buf: Vec::new(),
            history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
        }
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn clear(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.history_index = None;
        self.history_draft.clear();
    }

    pub fn commit_history(&mut self) {
        let line = self.input.clone();
        self.commit_history_entry(&line);
    }

    pub fn commit_history_entry(&mut self, line: &str) {
        let ignored = line.trim().is_empty()
            || line.starts_with(' ')
            || self.history.last().is_some_and(|entry| entry == line);
        if !ignored {
            self.history.push(line.to_string());
        }
        self.history_index = None;
        self.history_draft.clear();
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    pub fn set_history(&mut self, history: Vec<String>) {
        self.history = history;
        self.history_index = None;
        self.history_draft.clear();
    }

    pub fn handle_bytes(&mut self, bytes: &[u8]) -> EditorAction {
        let mut action = EditorAction::None;
        for &b in bytes {
            action = match self.state {
                InputState::Normal => self.handle_byte(b),
                InputState::Esc => self.handle_esc(b),
                InputState::Csi => self.handle_csi(b),
                InputState::Ss3 => self.handle_ss3(b),
            };
            if matches!(action, EditorAction::Submit) {
                return action;
            }
        }
        action
    }

    pub fn handle_key_input(&mut self, key: &KeyInput) -> EditorAction {
        if key.is_release() {
            return EditorAction::None;
        }
        if let Some(text) = key.text() {
            return self.handle_text(&text);
        }
        if let Some(control) = key.control_code() {
            return self.handle_byte(control);
        }

        let event = key.normalized_event();
        let word_modifier = event
            .modifiers
            .intersects(KeyModifiers::CTRL | KeyModifiers::ALT | KeyModifiers::META);
        match event.code {
            KeyCode::Enter => EditorAction::Submit,
            KeyCode::Backspace if word_modifier => {
                if self.erase_word_left() {
                    EditorAction::Changed
                } else {
                    EditorAction::Bell
                }
            }
            KeyCode::Backspace => self.handle_backspace(),
            KeyCode::Delete => {
                if self.delete() {
                    EditorAction::Changed
                } else {
                    EditorAction::Bell
                }
            }
            KeyCode::Left if word_modifier => {
                self.move_word_left();
                EditorAction::Changed
            }
            KeyCode::Right if word_modifier => {
                self.move_word_right();
                EditorAction::Changed
            }
            KeyCode::Left => {
                self.move_left();
                EditorAction::Changed
            }
            KeyCode::Right => {
                self.move_right();
                EditorAction::Changed
            }
            KeyCode::Up => self.handle_history_up(),
            KeyCode::Down => self.handle_history_down(),
            KeyCode::Home => {
                self.cursor = 0;
                EditorAction::Changed
            }
            KeyCode::End => {
                self.cursor = self.len_graphemes();
                EditorAction::Changed
            }
            KeyCode::Char(ch)
                if event
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::META) =>
            {
                match ch.to_ascii_lowercase() {
                    'b' => {
                        self.move_word_left();
                        EditorAction::Changed
                    }
                    'f' => {
                        self.move_word_right();
                        EditorAction::Changed
                    }
                    _ => EditorAction::None,
                }
            }
            _ => EditorAction::None,
        }
    }

    pub fn handle_text(&mut self, text: &str) -> EditorAction {
        if text.is_empty() {
            return EditorAction::None;
        }
        self.insert_str(text);
        EditorAction::Changed
    }

    pub fn len_graphemes(&self) -> usize {
        self.input.graphemes(true).count()
    }

    fn history_up(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        let next_index = match self.history_index {
            Some(0) => 0,
            Some(idx) => idx.saturating_sub(1),
            None => {
                self.history_draft = self.input.clone();
                self.history.len() - 1
            }
        };
        self.history_index = Some(next_index);
        self.input = self.history[next_index].clone();
        self.cursor = self.len_graphemes();
        true
    }

    fn history_down(&mut self) -> bool {
        let Some(idx) = self.history_index else {
            return false;
        };
        if idx + 1 >= self.history.len() {
            self.history_index = None;
            self.input = self.history_draft.clone();
            self.cursor = self.len_graphemes();
            return true;
        }
        let next_index = idx + 1;
        self.history_index = Some(next_index);
        self.input = self.history[next_index].clone();
        self.cursor = self.len_graphemes();
        true
    }

    fn handle_history_up(&mut self) -> EditorAction {
        if self.history_up() {
            EditorAction::Changed
        } else {
            EditorAction::Bell
        }
    }

    fn handle_history_down(&mut self) -> EditorAction {
        if self.history_down() {
            EditorAction::Changed
        } else {
            EditorAction::Bell
        }
    }

    fn handle_backspace(&mut self) -> EditorAction {
        if self.cursor == 0 && self.input.is_empty() {
            EditorAction::Bell
        } else if self.cursor == 0 {
            EditorAction::None
        } else {
            self.backspace();
            EditorAction::Changed
        }
    }

    fn handle_byte(&mut self, byte: u8) -> EditorAction {
        match byte {
            b'\x1B' => {
                self.state = InputState::Esc;
                EditorAction::None
            }
            b'\x01' => {
                self.cursor = 0;
                EditorAction::Changed
            }
            b'\x05' => {
                self.cursor = self.len_graphemes();
                EditorAction::Changed
            }
            b'\x10' => self.handle_history_up(),
            b'\x0E' => self.handle_history_down(),
            b'\x17' => {
                if self.erase_word_left() {
                    EditorAction::Changed
                } else {
                    EditorAction::Bell
                }
            }
            b'\r' | b'\n' => EditorAction::Submit,
            b'\x7F' | b'\x08' => self.handle_backspace(),
            _ => {
                if byte.is_ascii() && !byte.is_ascii_control() {
                    let ch = byte as char;
                    self.insert_str(&ch.to_string());
                    EditorAction::Changed
                } else {
                    EditorAction::None
                }
            }
        }
    }

    fn handle_esc(&mut self, byte: u8) -> EditorAction {
        match byte {
            b'[' => {
                self.state = InputState::Csi;
                self.csi_buf.clear();
            }
            b'O' => self.state = InputState::Ss3,
            b'b' => {
                self.move_word_left();
                self.state = InputState::Normal;
                return EditorAction::Changed;
            }
            b'f' => {
                self.move_word_right();
                self.state = InputState::Normal;
                return EditorAction::Changed;
            }
            b'\x7F' | b'\x08' => {
                let changed = self.erase_word_left();
                self.state = InputState::Normal;
                return if changed {
                    EditorAction::Changed
                } else {
                    EditorAction::Bell
                };
            }
            _ => self.state = InputState::Normal,
        }
        EditorAction::None
    }

    fn handle_csi(&mut self, byte: u8) -> EditorAction {
        self.csi_buf.push(byte);
        if !(0x40..=0x7E).contains(&byte) {
            return EditorAction::None;
        }
        self.state = InputState::Normal;
        let action = match byte {
            b'D' => {
                self.move_left();
                EditorAction::Changed
            }
            b'C' => {
                self.move_right();
                EditorAction::Changed
            }
            b'A' => self.handle_history_up(),
            b'B' => self.handle_history_down(),
            b'H' => {
                self.cursor = 0;
                EditorAction::Changed
            }
            b'F' => {
                self.cursor = self.len_graphemes();
                EditorAction::Changed
            }
            b'~' => {
                if self.handle_csi_tilde() {
                    EditorAction::Changed
                } else {
                    EditorAction::None
                }
            }
            _ => EditorAction::None,
        };
        self.csi_buf.clear();
        action
    }

    fn handle_ss3(&mut self, byte: u8) -> EditorAction {
        self.state = InputState::Normal;
        match byte {
            b'D' => {
                self.move_left();
                EditorAction::Changed
            }
            b'C' => {
                self.move_right();
                EditorAction::Changed
            }
            b'A' => self.handle_history_up(),
            b'B' => self.handle_history_down(),
            b'H' => {
                self.cursor = 0;
                EditorAction::Changed
            }
            b'F' => {
                self.cursor = self.len_graphemes();
                EditorAction::Changed
            }
            _ => EditorAction::None,
        }
    }

    fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    fn move_right(&mut self) {
        if self.cursor < self.len_graphemes() {
            self.cursor += 1;
        }
    }

    fn insert_str(&mut self, s: &str) {
        let byte_index = self.byte_index(self.cursor);
        self.input.insert_str(byte_index, s);
        let inserted_end = byte_index + s.len();
        self.cursor = 0;
        for (start, grapheme) in self.input.grapheme_indices(true) {
            self.cursor += 1;
            if start + grapheme.len() >= inserted_end {
                break;
            }
        }
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.cursor - 1;
        let byte_start = self.byte_index(start);
        let byte_end = self.byte_index(self.cursor);
        self.input.replace_range(byte_start..byte_end, "");
        self.cursor -= 1;
    }

    fn delete(&mut self) -> bool {
        if self.cursor >= self.len_graphemes() {
            return false;
        }
        let byte_start = self.byte_index(self.cursor);
        let byte_end = self.byte_index(self.cursor + 1);
        self.input.replace_range(byte_start..byte_end, "");
        true
    }

    fn move_word_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let graphemes: Vec<&str> = self.input.graphemes(true).collect();
        let mut idx = self.cursor.min(graphemes.len());
        while idx > 0 && !is_word_grapheme(graphemes[idx - 1]) {
            idx -= 1;
        }
        while idx > 0 && is_word_grapheme(graphemes[idx - 1]) {
            idx -= 1;
        }
        self.cursor = idx;
    }

    fn move_word_right(&mut self) {
        let graphemes: Vec<&str> = self.input.graphemes(true).collect();
        let len = graphemes.len();
        if self.cursor >= len {
            return;
        }
        let mut idx = self.cursor;
        while idx < len && !is_word_grapheme(graphemes[idx]) {
            idx += 1;
        }
        while idx < len && is_word_grapheme(graphemes[idx]) {
            idx += 1;
        }
        self.cursor = idx;
    }

    fn erase_word_left(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let original = self.cursor;
        self.move_word_left();
        let start = self.cursor;
        let end = original;
        if start == end {
            return false;
        }
        let byte_start = self.byte_index(start);
        let byte_end = self.byte_index(end);
        self.input.replace_range(byte_start..byte_end, "");
        true
    }

    fn handle_csi_tilde(&mut self) -> bool {
        let param = self.parse_csi_param();
        match param {
            Some(1) | Some(7) => {
                self.cursor = 0;
                true
            }
            Some(4) | Some(8) => {
                self.cursor = self.len_graphemes();
                true
            }
            Some(3) => self.delete(),
            _ => false,
        }
    }

    fn parse_csi_param(&self) -> Option<u16> {
        let mut value: u16 = 0;
        let mut saw_digit = false;
        for &b in &self.csi_buf {
            if b.is_ascii_digit() {
                saw_digit = true;
                value = value.saturating_mul(10).saturating_add((b - b'0') as u16);
            } else if b == b';' {
                break;
            }
        }
        if saw_digit { Some(value) } else { None }
    }

    fn byte_index(&self, char_index: usize) -> usize {
        self.input
            .grapheme_indices(true)
            .nth(char_index)
            .map(|(index, _)| index)
            .unwrap_or(self.input.len())
    }
}

fn is_word_grapheme(grapheme: &str) -> bool {
    grapheme
        .chars()
        .next()
        .is_some_and(|ch| ch.is_alphanumeric() || ch == '_')
}

#[cfg(test)]
mod tests {
    use super::{EditorAction, LineEditor};

    fn feed(editor: &mut LineEditor, bytes: &[u8]) -> EditorAction {
        editor.handle_bytes(bytes)
    }

    #[test]
    fn inserts_ascii_and_moves_cursor() {
        let mut editor = LineEditor::new();
        let action = feed(&mut editor, b"abc");
        assert!(matches!(action, EditorAction::Changed));
        assert_eq!(editor.input(), "abc");
        assert_eq!(editor.cursor(), 3);
    }

    #[test]
    fn moves_left_and_right_with_arrows() {
        let mut editor = LineEditor::new();
        feed(&mut editor, b"abc");
        feed(&mut editor, b"\x1B[D");
        assert_eq!(editor.cursor(), 2);
        feed(&mut editor, b"\x1B[C");
        assert_eq!(editor.cursor(), 3);
    }

    #[test]
    fn ctrl_a_and_ctrl_e_move_to_ends() {
        let mut editor = LineEditor::new();
        feed(&mut editor, b"abc");
        feed(&mut editor, b"\x01");
        assert_eq!(editor.cursor(), 0);
        feed(&mut editor, b"\x05");
        assert_eq!(editor.cursor(), 3);
    }

    #[test]
    fn insert_in_middle() {
        let mut editor = LineEditor::new();
        feed(&mut editor, b"ac");
        feed(&mut editor, b"\x1B[D");
        feed(&mut editor, b"b");
        assert_eq!(editor.input(), "abc");
        assert_eq!(editor.cursor(), 2);
    }

    #[test]
    fn backspace_in_middle() {
        let mut editor = LineEditor::new();
        feed(&mut editor, b"abc");
        feed(&mut editor, b"\x1B[D");
        let action = feed(&mut editor, b"\x7F");
        assert!(matches!(action, EditorAction::Changed));
        assert_eq!(editor.input(), "ac");
        assert_eq!(editor.cursor(), 1);
    }

    #[test]
    fn backspace_at_start_bells_when_empty() {
        let mut editor = LineEditor::new();
        let action = feed(&mut editor, b"\x7F");
        assert!(matches!(action, EditorAction::Bell));
        assert_eq!(editor.input(), "");
        assert_eq!(editor.cursor(), 0);
    }

    #[test]
    fn history_navigation_with_draft_restore() {
        let mut editor = LineEditor::new();
        feed(&mut editor, b"first");
        editor.commit_history();
        editor.clear();
        feed(&mut editor, b"draft");
        let action = feed(&mut editor, b"\x10");
        assert!(matches!(action, EditorAction::Changed));
        assert_eq!(editor.input(), "first");
        let action = feed(&mut editor, b"\x0E");
        assert!(matches!(action, EditorAction::Changed));
        assert_eq!(editor.input(), "draft");
    }

    #[test]
    fn history_ignores_leading_space_and_consecutive_duplicates() {
        let mut editor = LineEditor::new();
        feed(&mut editor, b" first");
        editor.commit_history();
        assert!(editor.history().is_empty());

        editor.clear();
        feed(&mut editor, b"first");
        editor.commit_history();
        assert_eq!(editor.history(), ["first"]);

        editor.clear();
        feed(&mut editor, b"first");
        editor.commit_history();
        assert_eq!(editor.history(), ["first"]);

        editor.clear();
        feed(&mut editor, b"second");
        editor.commit_history();
        assert_eq!(editor.history(), ["first", "second"]);
    }

    #[test]
    fn submit_on_enter() {
        let mut editor = LineEditor::new();
        let action = feed(&mut editor, b"\n");
        assert!(matches!(action, EditorAction::Submit));
    }

    #[test]
    fn home_end_keys_move_to_ends() {
        let mut editor = LineEditor::new();
        feed(&mut editor, b"abc");
        feed(&mut editor, b"\x1B[H");
        assert_eq!(editor.cursor(), 0);
        feed(&mut editor, b"\x1B[F");
        assert_eq!(editor.cursor(), 3);
        feed(&mut editor, b"\x1B[1~");
        assert_eq!(editor.cursor(), 0);
        feed(&mut editor, b"\x1B[4~");
        assert_eq!(editor.cursor(), 3);
    }

    #[test]
    fn alt_b_f_move_by_word() {
        let mut editor = LineEditor::new();
        feed(&mut editor, b"abc def");
        feed(&mut editor, b"\x1Bb");
        assert_eq!(editor.cursor(), 4);
        feed(&mut editor, b"\x1Bb");
        assert_eq!(editor.cursor(), 0);
        feed(&mut editor, b"\x1Bf");
        assert_eq!(editor.cursor(), 3);
        feed(&mut editor, b"\x1Bf");
        assert_eq!(editor.cursor(), 7);
    }

    #[test]
    fn erase_word_left_with_ctrl_w_and_alt_backspace() {
        let mut editor = LineEditor::new();
        feed(&mut editor, b"abc def");
        let action = feed(&mut editor, b"\x17");
        assert!(matches!(action, EditorAction::Changed));
        assert_eq!(editor.input(), "abc ");
        assert_eq!(editor.cursor(), 4);
        let action = feed(&mut editor, b"\x1B\x7F");
        assert!(matches!(action, EditorAction::Changed));
        assert_eq!(editor.input(), "");
        assert_eq!(editor.cursor(), 0);
    }

    #[test]
    fn default_history_and_character_count_accessors_cover_empty_and_loaded_state() {
        let mut editor = LineEditor::default();
        assert_eq!(editor.len_graphemes(), 0);
        editor.set_history(vec!["first".into(), "second".into()]);

        assert!(matches!(feed(&mut editor, b"\x10"), EditorAction::Changed));
        assert_eq!(editor.input(), "second");
        assert_eq!(editor.len_graphemes(), 6);
        assert!(matches!(feed(&mut editor, b"\x10"), EditorAction::Changed));
        assert_eq!(editor.input(), "first");
        assert!(matches!(feed(&mut editor, b"\x10"), EditorAction::Changed));
        assert_eq!(editor.input(), "first");
    }

    #[test]
    fn history_navigation_bells_without_a_valid_destination() {
        let mut editor = LineEditor::new();
        assert!(matches!(feed(&mut editor, b"\x10"), EditorAction::Bell));
        assert!(matches!(feed(&mut editor, b"\x0E"), EditorAction::Bell));
        assert!(matches!(feed(&mut editor, b"\x1B[A"), EditorAction::Bell));
        assert!(matches!(feed(&mut editor, b"\x1B[B"), EditorAction::Bell));
        assert!(matches!(feed(&mut editor, b"\x1BOA"), EditorAction::Bell));
        assert!(matches!(feed(&mut editor, b"\x1BOB"), EditorAction::Bell));
    }

    #[test]
    fn ss3_navigation_and_cursor_edges_do_not_move_out_of_bounds() {
        let mut editor = LineEditor::new();
        feed(&mut editor, b"abc");
        feed(&mut editor, b"\x1BOH");
        assert_eq!(editor.cursor(), 0);
        feed(&mut editor, b"\x1BOD");
        assert_eq!(editor.cursor(), 0);
        feed(&mut editor, b"\x1BOC");
        assert_eq!(editor.cursor(), 1);
        feed(&mut editor, b"\x1BOF");
        assert_eq!(editor.cursor(), 3);
        feed(&mut editor, b"\x1BOC");
        assert_eq!(editor.cursor(), 3);

        feed(&mut editor, b"\x01");
        assert!(matches!(feed(&mut editor, b"\x7F"), EditorAction::None));
        assert_eq!(editor.input(), "abc");
    }

    #[test]
    fn unsupported_escape_sequences_are_ignored_and_parser_state_recovers() {
        let mut editor = LineEditor::new();

        for sequence in [
            b"\x1Bx".as_slice(),
            b"\x1B[2~",
            b"\x1B[;~",
            b"\x1B[999999~",
            b"\x1BOx",
        ] {
            assert!(matches!(feed(&mut editor, sequence), EditorAction::None));
        }
        assert!(matches!(feed(&mut editor, b"a"), EditorAction::Changed));
        assert_eq!(editor.input(), "a");
    }

    #[test]
    fn unicode_editing_uses_grapheme_boundaries() {
        let mut editor = LineEditor::new();
        assert!(matches!(editor.handle_text("a e"), EditorAction::Changed));
        assert!(matches!(
            editor.handle_text("\u{301}"),
            EditorAction::Changed
        ));
        assert_eq!(editor.len_graphemes(), 3);
        assert_eq!(editor.cursor(), 3);
        assert!(matches!(editor.handle_text("界"), EditorAction::Changed));
        assert_eq!(editor.len_graphemes(), 4);
        assert_eq!(editor.cursor(), 4);

        editor.backspace();
        assert_eq!(editor.input(), "a e\u{301}");
        assert_eq!(editor.cursor(), 3);
        editor.backspace();
        assert_eq!(editor.input(), "a ");
        assert_eq!(editor.cursor(), 2);

        editor.cursor = 0;
        assert!(editor.delete());
        assert_eq!(editor.input(), " ");
        assert_eq!(editor.cursor(), 0);
    }
}
