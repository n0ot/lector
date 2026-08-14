use super::{Error, Result, ViewAction, ViewController, ViewKind};
use crate::{
    line_editor::{EditorAction, LineEditor},
    lua,
    screen_reader::ScreenReader,
    terminal_input::KeyInput,
    view::View,
};
use mlua::{
    Error as LuaError, HookTriggers, Lua, LuaOptions, MultiValue, StdLib, Table, Thread,
    ThreadStatus, Value, VmState,
};
use std::{any::Any, cell::RefCell, io::Write, rc::Rc};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const CLOSE_HINT: &str = "Esc to close";

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
    pub fn new(rows: u16, cols: u16, history: Vec<String>) -> Result<Self> {
        let lua = Lua::new_with(StdLib::ALL_SAFE | StdLib::JIT, LuaOptions::default())
            .map_err(Error::lua)?;
        let print_buffer = Rc::new(RefCell::new(ReplOutput { lines: Vec::new() }));
        let print_buffer_clone = Rc::clone(&print_buffer);
        let screen_reader_ptr = Rc::new(RefCell::new(std::ptr::null_mut()));
        lua::setup_repl(&lua, Rc::clone(&screen_reader_ptr)).map_err(Error::lua)?;
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
            .map_err(Error::lua)?;
        lua.globals().set("print", print_fn).map_err(Error::lua)?;

        let env = lua.create_table().map_err(Error::lua)?;
        let env_meta = lua.create_table().map_err(Error::lua)?;
        env_meta.set("__index", lua.globals()).map_err(Error::lua)?;
        env.set_metatable(Some(env_meta)).map_err(Error::lua)?;
        env.set("_G", env.clone()).map_err(Error::lua)?;

        let view = View::new(rows, cols);
        let mut editor = LineEditor::new();
        editor.set_history(history);
        let mut repl = Self {
            view,
            title: "Lua REPL".to_string(),
            output: Vec::new(),
            editor,
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
        repl.render_full();
        Ok(repl)
    }

    pub fn history(&self) -> &[String] {
        self.editor.history()
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
        let input_len = input.graphemes(true).count();
        let prev_input = self.rendered_input.as_str();
        let prev_len = prev_input.graphemes(true).count();
        let (_, cols) = self.view.size();
        let available = usize::from(cols).saturating_sub(2);
        if cursor == input_len
            && self.rendered_cursor == prev_len
            && input_len > prev_len
            && input.starts_with(prev_input)
            && !input.chars().any(char::is_control)
            && UnicodeWidthStr::width(input.as_str()) <= available
        {
            let added = &input[prev_input.len()..];
            self.write_bytes(added.as_bytes());
            self.rendered_input = input;
            self.rendered_cursor = cursor;
            return true;
        }
        false
    }

    fn apply_editor_update(&mut self) {
        self.render_full();
    }

    fn render_full(&mut self) {
        let (rows, cols) = self.view.size();
        let rows = rows as usize;
        let cols = cols as usize;
        let prompt = "> ";
        let available = cols.saturating_sub(prompt.len());
        let (visible_input, cursor_width) =
            visible_input_window(self.editor.input(), self.editor.cursor(), available);
        let cursor_col = prompt.len() + cursor_width;

        let body_rows = rows.saturating_sub(1);
        let mut body_lines: Vec<String> = self.output.to_vec();
        body_lines.push(format!("{}{}", prompt, visible_input));
        let body_lines = if body_lines.len() > body_rows {
            body_lines[body_lines.len() - body_rows..].to_vec()
        } else {
            body_lines
        };
        let body_len = body_lines.len();
        let mut lines: Vec<String> = vec![truncate_to_width(CLOSE_HINT, cols)];
        lines.extend(body_lines);
        let cursor_row = if body_rows == 0 { 1 } else { body_len + 1 };
        let cursor_col = cursor_col + 1;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x1B[2J\x1B[H");
        for (idx, line) in lines.iter().enumerate() {
            bytes.extend_from_slice(line.as_bytes());
            if idx + 1 < lines.len() {
                bytes.extend_from_slice(b"\r\n");
            }
        }
        bytes.extend_from_slice(format!("\x1B[{};{}H", cursor_row, cursor_col).as_bytes());

        self.view.clear_update_summary();
        self.view.process_changes(&bytes);
        self.view.clear_update_summary();
        self.rendered_input = self.editor.input().to_string();
        self.rendered_cursor = self.editor.cursor();
    }

    fn start_eval(&mut self, input: &str) -> Result<()> {
        let func = if let Some(rest) = input.strip_prefix('=') {
            self.lua
                .load(format!("return {}", rest))
                .set_name("repl")
                .set_environment(self.env.clone())
                .into_function()
                .map_err(Error::lua)?
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
                Err(LuaError::SyntaxError { .. }) => self
                    .lua
                    .load(input)
                    .set_name("repl")
                    .set_environment(self.env.clone())
                    .into_function()
                    .map_err(Error::lua)?,
                Err(err) => return Err(Error::lua(err)),
            }
        };
        let thread = self.lua.create_thread(func).map_err(Error::lua)?;
        thread
            .set_hook(
                HookTriggers::new().every_nth_instruction(1000),
                |_lua, _debug| Ok(VmState::Yield),
            )
            .map_err(Error::lua)?;
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

    fn clear_screen(&mut self) {
        self.output.clear();
        self.render_full();
    }

    fn apply_editor_action(&mut self, action: EditorAction) -> Result<ViewAction> {
        match action {
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
}

impl ViewController for LuaReplView {
    fn as_any(&self) -> &dyn Any {
        self
    }

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
        if input == b"\x1B" {
            self.thread = None;
            return Ok(ViewAction::Pop);
        }
        if input == b"\x0C" {
            self.clear_screen();
            return Ok(ViewAction::Redraw);
        }
        if self.thread.is_some() {
            return Ok(ViewAction::Bell);
        }
        let action = self.editor.handle_bytes(input);
        self.apply_editor_action(action)
    }

    fn handle_key_input(
        &mut self,
        sr: &mut ScreenReader,
        key: &KeyInput,
        _raw: &[u8],
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        self.set_screen_reader(sr);
        if key.is_release() {
            return Ok(ViewAction::None);
        }
        if matches!(key.control_code(), Some(0x1B))
            || matches!(key.event().code, terminput::KeyCode::Esc)
        {
            self.thread = None;
            return Ok(ViewAction::Pop);
        }
        if matches!(key.control_code(), Some(0x0C)) {
            self.clear_screen();
            return Ok(ViewAction::Redraw);
        }
        if self.thread.is_some() {
            return Ok(ViewAction::Bell);
        }
        let action = self.editor.handle_key_input(key);
        self.apply_editor_action(action)
    }

    fn handle_paste(
        &mut self,
        sr: &mut ScreenReader,
        contents: &str,
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        self.set_screen_reader(sr);
        if self.thread.is_some() {
            return Ok(ViewAction::Bell);
        }
        let contents = contents.replace("\r\n", "\n").replace('\r', "\n");
        let action = self.editor.handle_text(&contents);
        self.apply_editor_action(action)
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

fn truncate_to_width(text: &str, width: usize) -> String {
    let mut used = 0;
    text.graphemes(true)
        .take_while(|grapheme| {
            let grapheme_width = UnicodeWidthStr::width(*grapheme);
            if used + grapheme_width > width {
                return false;
            }
            used += grapheme_width;
            true
        })
        .collect()
}

fn visible_input_window(input: &str, cursor: usize, available: usize) -> (String, usize) {
    let graphemes: Vec<&str> = input.graphemes(true).collect();
    let cursor = cursor.min(graphemes.len());
    let mut start = cursor;
    let mut cursor_width = 0;
    while start > 0 {
        let width = UnicodeWidthStr::width(display_grapheme(graphemes[start - 1]));
        if cursor_width + width > available {
            break;
        }
        cursor_width += width;
        start -= 1;
    }

    let mut end = cursor;
    let mut total_width = cursor_width;
    while end < graphemes.len() {
        let width = UnicodeWidthStr::width(display_grapheme(graphemes[end]));
        if total_width + width > available {
            break;
        }
        total_width += width;
        end += 1;
    }
    (
        graphemes[start..end]
            .iter()
            .map(|grapheme| display_grapheme(grapheme))
            .collect(),
        cursor_width,
    )
}

fn display_grapheme(grapheme: &str) -> &str {
    match grapheme {
        "\n" => "↵",
        "\t" => "⇥",
        "\r" => "↵",
        _ if grapheme.chars().any(char::is_control) => "�",
        _ => grapheme,
    }
}

#[cfg(test)]
mod tests {
    use super::{CLOSE_HINT, LuaReplView, truncate_to_width, visible_input_window};
    use crate::{screen_reader::ScreenReader, speech, views::ViewController};
    use std::{cell::RefCell, rc::Rc};

    struct TestDriver {
        speaks: Rc<RefCell<Vec<String>>>,
    }

    impl speech::Driver for TestDriver {
        fn speak(&mut self, text: &str, _interrupt: bool) -> anyhow::Result<()> {
            self.speaks.borrow_mut().push(text.to_string());
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

    fn make_screen_reader() -> (ScreenReader, Rc<RefCell<Vec<String>>>) {
        let speaks = Rc::new(RefCell::new(Vec::new()));
        let driver = TestDriver {
            speaks: Rc::clone(&speaks),
        };
        let sr = ScreenReader::new(speech::Speech::new(Box::new(driver)));
        (sr, speaks)
    }

    #[test]
    fn lua_repl_renders_close_hint_in_top_row() {
        let mut repl = LuaReplView::new(6, 20, Vec::new()).expect("create lua repl");
        let top_row = repl.model().screen().contents_between(0, 0, 0, 20);
        assert!(top_row.starts_with(CLOSE_HINT));
    }

    #[test]
    fn lua_repl_esc_closes_view() {
        let mut repl = LuaReplView::new(6, 20, Vec::new()).expect("create lua repl");
        let (mut sr, _speaks) = make_screen_reader();
        let action = repl
            .handle_input(&mut sr, b"\x1B", &mut Vec::new())
            .expect("handle esc");
        assert!(matches!(action, crate::views::ViewAction::Pop));
    }

    #[test]
    fn lua_repl_ctrl_d_does_not_close_view() {
        let mut repl = LuaReplView::new(6, 20, Vec::new()).expect("create lua repl");
        let (mut sr, _speaks) = make_screen_reader();
        let action = repl
            .handle_input(&mut sr, b"\x04", &mut Vec::new())
            .expect("handle ctrl-d");
        assert!(matches!(action, crate::views::ViewAction::None));
    }

    #[test]
    fn lua_repl_close_hint_stays_pinned_after_output_overflows() {
        let mut repl = LuaReplView::new(4, 20, Vec::new()).expect("create lua repl");
        repl.append_output("line 1\nline 2\nline 3\nline 4\nline 5");
        repl.render_full();
        let screen = repl.model().screen();
        assert!(screen.contents_between(0, 0, 0, 20).starts_with(CLOSE_HINT));
        assert!(screen.contents_between(1, 0, 1, 20).contains("line 4"));
        assert!(screen.contents_between(2, 0, 2, 20).contains("line 5"));
        assert!(screen.contents_between(3, 0, 3, 20).contains(">"));
    }

    #[test]
    fn lua_repl_auto_read_speaks_incoming_output_without_banner() {
        let mut repl = LuaReplView::new(6, 30, Vec::new()).expect("create lua repl");
        let (mut sr, speaks) = make_screen_reader();
        repl.model().finalize_changes(0);
        let added = repl.append_output("alpha\nbeta\ngamma\ndelta");
        repl.write_output_lines(&added);
        repl.write_prompt();

        let read = sr.auto_read(repl.model()).expect("auto read");
        assert!(read);

        let speaks = speaks.borrow();
        assert!(speaks.iter().any(|text| text.contains("alpha")));
        assert!(speaks.iter().any(|text| text.contains("delta")));
        assert!(!speaks.iter().any(|text| text.contains(CLOSE_HINT)));
    }

    #[test]
    fn lua_repl_ctrl_l_clears_output_and_keeps_close_hint() {
        let mut repl = LuaReplView::new(6, 30, Vec::new()).expect("create lua repl");
        let (mut sr, _speaks) = make_screen_reader();
        repl.append_output("alpha\nbeta");
        repl.render_full();

        let action = repl
            .handle_input(&mut sr, b"\x0C", &mut Vec::new())
            .expect("handle ctrl-l");

        assert!(matches!(action, crate::views::ViewAction::Redraw));
        let screen = repl.model().screen();
        assert!(screen.contents_between(0, 0, 0, 30).starts_with(CLOSE_HINT));
        assert!(!screen.contents_between(1, 0, 1, 30).contains("alpha"));
        assert!(screen.contents().contains("> "));
    }

    #[test]
    fn lua_repl_accepts_existing_history() {
        let repl = LuaReplView::new(6, 30, vec!["print(1)".to_string(), "print(2)".to_string()])
            .expect("create lua repl");
        assert_eq!(repl.history(), ["print(1)", "print(2)"]);
    }

    #[test]
    fn unicode_windows_preserve_graphemes_and_terminal_cell_widths() {
        assert_eq!(truncate_to_width("a界b", 3), "a界");
        assert_eq!(truncate_to_width("e\u{301}x", 1), "e\u{301}");

        let (visible, cursor_width) = visible_input_window("a界bc", 3, 4);
        assert_eq!(visible, "a界b");
        assert_eq!(cursor_width, 4);

        let (visible, cursor_width) = visible_input_window("e\u{301}界x", 1, 3);
        assert_eq!(visible, "e\u{301}界");
        assert_eq!(cursor_width, 1);
    }
}
