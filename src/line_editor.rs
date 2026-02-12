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
    }

    pub fn commit_history(&mut self) {
        if !self.input.trim().is_empty() {
            self.history.push(self.input.clone());
        }
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

    pub fn len_chars(&self) -> usize {
        self.input.len()
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
        self.cursor = self.input.len();
        true
    }

    fn history_down(&mut self) -> bool {
        let Some(idx) = self.history_index else {
            return false;
        };
        if idx + 1 >= self.history.len() {
            self.history_index = None;
            self.input = self.history_draft.clone();
            self.cursor = self.input.len();
            return true;
        }
        let next_index = idx + 1;
        self.history_index = Some(next_index);
        self.input = self.history[next_index].clone();
        self.cursor = self.input.len();
        true
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
                self.cursor = self.input.len();
                EditorAction::Changed
            }
            b'\x10' => {
                if self.history_up() {
                    EditorAction::Changed
                } else {
                    EditorAction::Bell
                }
            }
            b'\x0E' => {
                if self.history_down() {
                    EditorAction::Changed
                } else {
                    EditorAction::Bell
                }
            }
            b'\x17' => {
                if self.erase_word_left() {
                    EditorAction::Changed
                } else {
                    EditorAction::Bell
                }
            }
            b'\r' | b'\n' => EditorAction::Submit,
            b'\x7F' | b'\x08' => {
                if self.cursor == 0 && self.input.is_empty() {
                    EditorAction::Bell
                } else if self.cursor == 0 {
                    EditorAction::None
                } else {
                    self.backspace();
                    EditorAction::Changed
                }
            }
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
            b'A' => {
                if self.history_up() {
                    EditorAction::Changed
                } else {
                    EditorAction::Bell
                }
            }
            b'B' => {
                if self.history_down() {
                    EditorAction::Changed
                } else {
                    EditorAction::Bell
                }
            }
            b'H' => {
                self.cursor = 0;
                EditorAction::Changed
            }
            b'F' => {
                self.cursor = self.input.len();
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
            b'A' => {
                if self.history_up() {
                    EditorAction::Changed
                } else {
                    EditorAction::Bell
                }
            }
            b'B' => {
                if self.history_down() {
                    EditorAction::Changed
                } else {
                    EditorAction::Bell
                }
            }
            b'H' => {
                self.cursor = 0;
                EditorAction::Changed
            }
            b'F' => {
                self.cursor = self.input.len();
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
        if self.cursor < self.input.len() {
            self.cursor += 1;
        }
    }

    fn insert_str(&mut self, s: &str) {
        self.input.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.cursor - 1;
        self.input.replace_range(start..self.cursor, "");
        self.cursor -= 1;
    }

    fn move_word_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut idx = self.cursor;
        while idx > 0 && !is_word_byte(self.input.as_bytes()[idx - 1]) {
            idx -= 1;
        }
        while idx > 0 && is_word_byte(self.input.as_bytes()[idx - 1]) {
            idx -= 1;
        }
        self.cursor = idx;
    }

    fn move_word_right(&mut self) {
        let len = self.input.len();
        if self.cursor >= len {
            return;
        }
        let mut idx = self.cursor;
        while idx < len && !is_word_byte(self.input.as_bytes()[idx]) {
            idx += 1;
        }
        while idx < len && is_word_byte(self.input.as_bytes()[idx]) {
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
        self.input.replace_range(start..end, "");
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
                self.cursor = self.input.len();
                true
            }
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
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
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
}
