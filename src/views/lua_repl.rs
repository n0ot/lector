use super::{ViewAction, ViewController, ViewKind};
use crate::{lua, screen_reader::ScreenReader, view::View};
use anyhow::{anyhow, Result};
use mlua::{
    Error, HookTriggers, Lua, LuaOptions, MultiValue, StdLib, Table, Thread, ThreadStatus, Value,
    VmState,
};
use std::{
    cell::RefCell,
    rc::Rc,
};

struct LineEditor {
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

impl LineEditor {
    fn new() -> Self {
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

    fn clear(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.history_index = None;
    }

    fn commit_history(&mut self) {
        if !self.input.trim().is_empty() {
            self.history.push(self.input.clone());
        }
        self.history_index = None;
        self.history_draft.clear();
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
        self.cursor = self.len_chars();
        true
    }

    fn history_down(&mut self) -> bool {
        let Some(idx) = self.history_index else {
            return false;
        };
        if idx + 1 >= self.history.len() {
            self.history_index = None;
            self.input = self.history_draft.clone();
            self.cursor = self.len_chars();
            return true;
        }
        let next_index = idx + 1;
        self.history_index = Some(next_index);
        self.input = self.history[next_index].clone();
        self.cursor = self.len_chars();
        true
    }

    fn handle_bytes(&mut self, bytes: &[u8]) -> EditorAction {
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
                self.cursor = self.len_chars();
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
            _ => self.state = InputState::Normal,
        }
        EditorAction::None
    }

    fn handle_csi(&mut self, byte: u8) -> EditorAction {
        self.csi_buf.push(byte);
        if !(byte >= 0x40 && byte <= 0x7E) {
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
            _ => EditorAction::None,
        }
    }

    fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    fn move_right(&mut self) {
        if self.cursor < self.len_chars() {
            self.cursor += 1;
        }
    }

    fn insert_str(&mut self, s: &str) {
        let idx = self.byte_index(self.cursor);
        self.input.insert_str(idx, s);
        self.cursor += s.chars().count();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.byte_index(self.cursor - 1);
        let end = self.byte_index(self.cursor);
        self.input.replace_range(start..end, "");
        self.cursor -= 1;
    }

    fn len_chars(&self) -> usize {
        self.input.chars().count()
    }

    fn byte_index(&self, char_index: usize) -> usize {
        if char_index == 0 {
            return 0;
        }
        self.input
            .char_indices()
            .nth(char_index)
            .map(|(idx, _)| idx)
            .unwrap_or_else(|| self.input.len())
    }
}

#[derive(Copy, Clone)]
enum EditorAction {
    None,
    Changed,
    Submit,
    Bell,
}

struct ReplOutput {
    lines: Vec<String>,
}

pub struct LuaReplView {
    view: View,
    title: String,
    output: Vec<String>,
    editor: LineEditor,
    lua: Lua,
    env: Table,
    thread: Option<Thread>,
    print_buffer: Rc<RefCell<ReplOutput>>,
    screen_reader_ptr: Rc<RefCell<*mut ScreenReader>>,
}

impl LuaReplView {
    pub fn new(rows: u16, cols: u16) -> Result<Self> {
        let lua = Lua::new_with(StdLib::ALL_SAFE | StdLib::JIT, LuaOptions::default())
            .map_err(|e| anyhow!(e.to_string()))?;
        let print_buffer = Rc::new(RefCell::new(ReplOutput { lines: Vec::new() }));
        let print_buffer_clone = Rc::clone(&print_buffer);
        let screen_reader_ptr = Rc::new(RefCell::new(std::ptr::null_mut()));
        lua::setup_repl(&lua, Rc::clone(&screen_reader_ptr))
            .map_err(|e| anyhow!(e.to_string()))?;
        let print_fn = lua
            .create_function(move |_lua, args: MultiValue| {
                let mut pieces = Vec::new();
                for value in args {
                    pieces.push(format_value(value));
                }
                let line = pieces.join("\t");
                print_buffer_clone.borrow_mut().lines.push(line);
                Ok(())
            })
            .map_err(|e| anyhow!(e.to_string()))?;
        lua.globals()
            .set("print", print_fn)
            .map_err(|e| anyhow!(e.to_string()))?;

        let env = lua
            .create_table()
            .map_err(|e| anyhow!(e.to_string()))?;
        let env_meta = lua
            .create_table()
            .map_err(|e| anyhow!(e.to_string()))?;
        env_meta
            .set("__index", lua.globals())
            .map_err(|e| anyhow!(e.to_string()))?;
        env.set_metatable(Some(env_meta));
        env.set("_G", env.clone())
            .map_err(|e| anyhow!(e.to_string()))?;

        let view = View::new(rows, cols);
        let mut repl = Self {
            view,
            title: "Lua REPL".to_string(),
            output: Vec::new(),
            editor: LineEditor::new(),
            lua,
            env,
            thread: None,
            print_buffer,
            screen_reader_ptr,
        };
        repl.append_output("Lua REPL ready.");
        repl.render();
        Ok(repl)
    }

    fn set_screen_reader(&mut self, sr: &mut ScreenReader) {
        *self.screen_reader_ptr.borrow_mut() = sr as *mut ScreenReader;
    }

    fn append_output(&mut self, text: &str) {
        for line in text.split('\n') {
            self.output.push(line.to_string());
        }
        const MAX_LINES: usize = 1000;
        if self.output.len() > MAX_LINES {
            let excess = self.output.len() - MAX_LINES;
            self.output.drain(0..excess);
        }
    }

    fn drain_print_buffer(&mut self) {
        let mut buffer = self.print_buffer.borrow_mut();
        for line in buffer.lines.drain(..) {
            self.output.push(line);
        }
    }

    fn render(&mut self) {
        let (rows, cols) = self.view.size();
        let cols = cols as usize;
        let prompt = "> ";
        let available = cols.saturating_sub(prompt.len());
        let total_chars = self.editor.len_chars();
        let cursor = self.editor.cursor.min(total_chars);
        let start = if cursor > available {
            cursor.saturating_sub(available)
        } else {
            0
        };
        let visible_input: String = self
            .editor
            .input
            .chars()
            .skip(start)
            .take(available)
            .collect();
        let cursor_col = prompt.len() + (cursor - start);

        let mut lines: Vec<String> = Vec::new();
        lines.extend(self.output.iter().cloned());
        lines.push(format!("{}{}", prompt, visible_input));
        let rows = rows as usize;
        let lines = if lines.len() > rows {
            lines[lines.len() - rows..].to_vec()
        } else {
            lines
        };
        let cursor_row = if lines.is_empty() { 1 } else { lines.len() };
        let cursor_col = cursor_col + 1;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x1B[2J\x1B[H");
        for line in lines {
            bytes.extend_from_slice(line.as_bytes());
            bytes.extend_from_slice(b"\r\n");
        }
        bytes.extend_from_slice(format!("\x1B[{};{}H", cursor_row, cursor_col).as_bytes());

        self.view.next_bytes.clear();
        self.view.process_changes(&bytes);
        self.view.next_bytes.clear();
    }

    fn start_eval(&mut self, input: &str) -> Result<()> {
        let func = if let Some(rest) = input.strip_prefix('=') {
            self.lua
                .load(&format!("return {}", rest))
                .set_name("repl")
                .set_environment(self.env.clone())
                .into_function()
                .map_err(|e| anyhow!(e.to_string()))?
        } else {
            let expr_code = format!("return {}", input);
            match self
                .lua
                .load(&expr_code)
                .set_name("repl")
                .set_environment(self.env.clone())
                .into_function()
            {
                Ok(func) => func,
                Err(Error::SyntaxError { .. }) => self
                    .lua
                    .load(input)
                    .set_name("repl")
                    .set_environment(self.env.clone())
                    .into_function()
                    .map_err(|e| anyhow!(e.to_string()))?,
                Err(err) => return Err(anyhow!(err.to_string())),
            }
        };
        let thread = self
            .lua
            .create_thread(func)
            .map_err(|e| anyhow!(e.to_string()))?;
        thread.set_hook(
            HookTriggers::new().every_nth_instruction(1000),
            |_lua, _debug| Ok(VmState::Yield),
        )
        .map_err(|e| anyhow!(e.to_string()))?;
        self.thread = Some(thread);
        Ok(())
    }

    fn resume_eval(&mut self) -> Result<bool> {
        let Some(thread) = &self.thread else {
            return Ok(false);
        };
        match thread.resume::<MultiValue>(()) {
            Ok(values) => {
                if thread.status() == ThreadStatus::Finished {
                    if !values.is_empty() {
                        let mut pieces = Vec::new();
                        for value in values {
                            pieces.push(format_value(value));
                        }
                        self.append_output(&pieces.join("\t"));
                    }
                    self.thread = None;
                }
                Ok(true)
            }
            Err(err) => {
                self.append_output(&format!("Error: {}", err));
                self.thread = None;
                Ok(true)
            }
        }
    }
}

impl ViewController for LuaReplView {
    fn model(&mut self) -> &mut View {
        &mut self.view
    }

    fn title(&self) -> &str {
        &self.title
    }

    fn kind(&self) -> ViewKind {
        ViewKind::LuaRepl
    }

    fn wants_tick(&self) -> bool {
        self.thread.is_some()
    }

    fn handle_input(
        &mut self,
        sr: &mut ScreenReader,
        input: &[u8],
        _pty_stream: &mut ptyprocess::stream::Stream,
    ) -> Result<ViewAction> {
        self.set_screen_reader(sr);
        if input == b"\x04" {
            self.thread = None;
            return Ok(ViewAction::Pop);
        }
        if self.thread.is_some() {
            return Ok(ViewAction::Bell);
        }
        match self.editor.handle_bytes(input) {
            EditorAction::Submit => {
                let line = self.editor.input.clone();
                if line.trim().is_empty() {
                    return Ok(ViewAction::Bell);
                }
                self.append_output(&format!("> {}", line));
                self.editor.commit_history();
                self.editor.clear();
                if let Err(err) = self.start_eval(&line) {
                    self.append_output(&format!("Error: {}", err));
                    self.render();
                    return Ok(ViewAction::Redraw);
                }
                self.render();
                Ok(ViewAction::Redraw)
            }
            EditorAction::Changed => {
                self.render();
                Ok(ViewAction::Redraw)
            }
            EditorAction::Bell => Ok(ViewAction::Bell),
            EditorAction::None => Ok(ViewAction::None),
        }
    }

    fn tick(
        &mut self,
        sr: &mut ScreenReader,
        _pty_stream: &mut ptyprocess::stream::Stream,
    ) -> Result<ViewAction> {
        self.set_screen_reader(sr);
        if self.thread.is_none() {
            return Ok(ViewAction::None);
        }
        let progressed = self.resume_eval()?;
        self.drain_print_buffer();
        if progressed {
            self.render();
            return Ok(ViewAction::Redraw);
        }
        Ok(ViewAction::None)
    }

    fn on_resize(&mut self, rows: u16, cols: u16) {
        self.view.set_size(rows, cols);
        self.render();
    }
}

fn format_value(value: Value) -> String {
    match value {
        Value::Nil => "nil".to_string(),
        Value::Boolean(v) => v.to_string(),
        Value::Integer(v) => v.to_string(),
        Value::Number(v) => v.to_string(),
        Value::String(v) => v
            .to_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|_| "<binary>".to_string()),
        Value::Table(_) => "table".to_string(),
        Value::Function(_) => "function".to_string(),
        Value::Thread(_) => "thread".to_string(),
        Value::UserData(_) => "userdata".to_string(),
        Value::LightUserData(_) => "lightuserdata".to_string(),
        Value::Error(err) => err.to_string(),
        _ => "value".to_string(),
    }
}
