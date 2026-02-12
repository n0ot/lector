use super::{ViewAction, ViewController, ViewKind};
use crate::{
    line_editor::{EditorAction, LineEditor},
    lua,
    screen_reader::ScreenReader,
    view::View,
};
use anyhow::{Result, anyhow};
use mlua::{
    Error, HookTriggers, Lua, LuaOptions, MultiValue, StdLib, Table, Thread, ThreadStatus, Value,
    VmState,
};
use std::{cell::RefCell, io::Write, rc::Rc};

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
    rendered_input: String,
    rendered_cursor: usize,
}

impl LuaReplView {
    pub fn new(rows: u16, cols: u16) -> Result<Self> {
        let lua = Lua::new_with(StdLib::ALL_SAFE | StdLib::JIT, LuaOptions::default())
            .map_err(|e| anyhow!(e.to_string()))?;
        let print_buffer = Rc::new(RefCell::new(ReplOutput { lines: Vec::new() }));
        let print_buffer_clone = Rc::clone(&print_buffer);
        let screen_reader_ptr = Rc::new(RefCell::new(std::ptr::null_mut()));
        lua::setup_repl(&lua, Rc::clone(&screen_reader_ptr)).map_err(|e| anyhow!(e.to_string()))?;
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

        let env = lua.create_table().map_err(|e| anyhow!(e.to_string()))?;
        let env_meta = lua.create_table().map_err(|e| anyhow!(e.to_string()))?;
        env_meta
            .set("__index", lua.globals())
            .map_err(|e| anyhow!(e.to_string()))?;
        env.set_metatable(Some(env_meta))
            .map_err(|e| anyhow!(e.to_string()))?;
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
            rendered_input: String::new(),
            rendered_cursor: 0,
        };
        let added = repl.append_output("Lua REPL ready.");
        repl.write_output_lines(&added);
        repl.write_prompt();
        Ok(repl)
    }

    fn set_screen_reader(&mut self, sr: &mut ScreenReader) {
        *self.screen_reader_ptr.borrow_mut() = sr as *mut ScreenReader;
    }

    fn append_output(&mut self, text: &str) -> Vec<String> {
        let mut added = Vec::new();
        for line in text.split('\n') {
            let line = line.to_string();
            self.output.push(line.clone());
            added.push(line);
        }
        const MAX_LINES: usize = 1000;
        if self.output.len() > MAX_LINES {
            let excess = self.output.len() - MAX_LINES;
            self.output.drain(0..excess);
        }
        added
    }

    fn drain_print_buffer(&mut self) -> Vec<String> {
        let mut buffer = self.print_buffer.borrow_mut();
        let mut added = Vec::new();
        for line in buffer.lines.drain(..) {
            self.output.push(line.clone());
            added.push(line);
        }
        added
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        self.view.process_changes(bytes);
    }

    fn write_output_lines(&mut self, lines: &[String]) {
        for line in lines {
            self.write_bytes(line.as_bytes());
            self.write_bytes(b"\r\n");
        }
    }

    fn write_prompt(&mut self) {
        self.write_bytes(b"> ");
        self.rendered_input.clear();
        self.rendered_cursor = 0;
    }

    fn try_append_input(&mut self) -> bool {
        let input = self.editor.input().to_string();
        let cursor = self.editor.cursor();
        let input_len = input.chars().count();
        let prev_input = self.rendered_input.as_str();
        let prev_len = prev_input.chars().count();
        if cursor == input_len
            && self.rendered_cursor == prev_len
            && input_len > prev_len
            && input.starts_with(prev_input)
        {
            let added = &input[prev_input.len()..];
            self.write_bytes(added.as_bytes());
            self.rendered_input = input;
            self.rendered_cursor = cursor;
            return true;
        }
        false
    }

    fn redraw_input_line(&mut self) {
        let input = self.editor.input().to_string();
        let cursor = self.editor.cursor();
        let input_len = input.chars().count();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\r\x1B[K");
        bytes.extend_from_slice(b"> ");
        bytes.extend_from_slice(input.as_bytes());
        if input_len > cursor {
            bytes.extend_from_slice(format!("\x1B[{}D", input_len - cursor).as_bytes());
        }
        self.write_bytes(&bytes);
        self.rendered_input = input;
        self.rendered_cursor = cursor;
    }

    fn apply_editor_update(&mut self) {
        let new_input = self.editor.input().to_string();
        let new_cursor = self.editor.cursor();
        let prev_input = self.rendered_input.clone();
        let prev_cursor = self.rendered_cursor;

        if new_input == prev_input {
            if new_cursor + 1 == prev_cursor {
                self.write_bytes(b"\x08");
                self.rendered_cursor = new_cursor;
                return;
            }
            if new_cursor == prev_cursor + 1 {
                self.write_bytes(b"\x1B[C");
                self.rendered_cursor = new_cursor;
                return;
            }
        }

        let new_chars: Vec<char> = new_input.chars().collect();
        let prev_chars: Vec<char> = prev_input.chars().collect();

        if new_chars.len() == prev_chars.len() + 1 && new_cursor == prev_cursor + 1 {
            if new_chars[..prev_cursor] == prev_chars[..prev_cursor]
                && new_chars[prev_cursor + 1..] == prev_chars[prev_cursor..]
            {
                let inserted = new_chars[prev_cursor];
                let mut bytes = Vec::new();
                bytes.extend_from_slice(b"\x1B[1@");
                bytes.extend_from_slice(inserted.to_string().as_bytes());
                self.write_bytes(&bytes);
                self.rendered_input = new_input;
                self.rendered_cursor = new_cursor;
                return;
            }
        }

        if new_chars.len() + 1 == prev_chars.len() && new_cursor + 1 == prev_cursor {
            if new_chars[..new_cursor] == prev_chars[..new_cursor]
                && new_chars[new_cursor..] == prev_chars[new_cursor + 1..]
            {
                if new_cursor == new_chars.len() {
                    self.write_bytes(b"\x08 \x08");
                } else {
                    self.write_bytes(b"\x08\x1B[1P");
                }
                self.rendered_input = new_input;
                self.rendered_cursor = new_cursor;
                return;
            }
        }

        self.redraw_input_line();
    }

    fn render_full(&mut self) {
        let (rows, cols) = self.view.size();
        let cols = cols as usize;
        let prompt = "> ";
        let available = cols.saturating_sub(prompt.len());
        let total_chars = self.editor.len_chars();
        let cursor = self.editor.cursor().min(total_chars);
        let start = if cursor > available {
            cursor.saturating_sub(available)
        } else {
            0
        };
        let visible_input: String = self
            .editor
            .input()
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
        self.rendered_input = self.editor.input().to_string();
        self.rendered_cursor = self.editor.cursor();
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
        thread
            .set_hook(
                HookTriggers::new().every_nth_instruction(1000),
                |_lua, _debug| Ok(VmState::Yield),
            )
            .map_err(|e| anyhow!(e.to_string()))?;
        self.thread = Some(thread);
        Ok(())
    }

    fn resume_eval(&mut self) -> Result<(bool, Vec<String>)> {
        let Some(thread) = &self.thread else {
            return Ok((false, Vec::new()));
        };
        match thread.resume::<MultiValue>(()) {
            Ok(values) => {
                let mut added = Vec::new();
                if thread.status() == ThreadStatus::Finished {
                    if !values.is_empty() {
                        let mut pieces = Vec::new();
                        for value in values {
                            pieces.push(format_value(value));
                        }
                        added = self.append_output(&pieces.join("\t"));
                    }
                    self.thread = None;
                }
                Ok((true, added))
            }
            Err(err) => {
                let added = self.append_output(&format!("Error: {}", err));
                self.thread = None;
                Ok((true, added))
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
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        self.set_screen_reader(sr);
        if input.iter().any(|&b| b == 0x04) {
            self.thread = None;
            return Ok(ViewAction::Pop);
        }
        if self.thread.is_some() {
            return Ok(ViewAction::Bell);
        }
        match self.editor.handle_bytes(input) {
            EditorAction::Submit => {
                let line = self.editor.input().to_string();
                if line.trim().is_empty() {
                    return Ok(ViewAction::Bell);
                }
                self.write_bytes(b"\r\n");
                self.editor.commit_history();
                self.editor.clear();
                self.rendered_input.clear();
                self.rendered_cursor = 0;
                if let Err(err) = self.start_eval(&line) {
                    let added = self.append_output(&format!("Error: {}", err));
                    self.write_output_lines(&added);
                    self.write_prompt();
                    return Ok(ViewAction::Redraw);
                }
                Ok(ViewAction::Redraw)
            }
            EditorAction::Changed => {
                if !self.try_append_input() {
                    self.apply_editor_update();
                }
                Ok(ViewAction::Redraw)
            }
            EditorAction::Bell => Ok(ViewAction::Bell),
            EditorAction::None => Ok(ViewAction::None),
        }
    }

    fn tick(&mut self, sr: &mut ScreenReader, _pty_stream: &mut dyn Write) -> Result<ViewAction> {
        self.set_screen_reader(sr);
        if self.thread.is_none() {
            return Ok(ViewAction::None);
        }
        let (progressed, added) = self.resume_eval()?;
        let printed = self.drain_print_buffer();
        if !added.is_empty() {
            self.write_output_lines(&added);
        }
        if !printed.is_empty() {
            self.write_output_lines(&printed);
        }
        if progressed {
            if self.thread.is_none() {
                self.write_prompt();
            }
            return Ok(ViewAction::Redraw);
        }
        Ok(ViewAction::None)
    }

    fn on_resize(&mut self, rows: u16, cols: u16) {
        self.view.set_size(rows, cols);
        self.render_full();
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
