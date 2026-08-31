use super::{Error, Result, ViewAction, ViewController, ViewKind};
use crate::{
    line_editor::{EditorAction, LineEditor},
    lua,
    screen_reader::ScreenReader,
    terminal_input::KeyInput,
    view::View,
};
use mlua::{
    Error as LuaError, Function, HookTriggers, Lua, LuaOptions, MultiValue, StdLib, Table, Thread,
    ThreadStatus, Value, VmState,
};
use std::{any::Any, cell::RefCell, io::Write, rc::Rc};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const CLOSE_HINT: &str = "Esc to close";

struct ReplOutput {
    lines: Vec<String>,
}

#[derive(Clone)]
enum TranscriptLine {
    Input { continuation: bool, text: String },
    Output(String),
}

struct LuaReplState {
    transcript: Vec<TranscriptLine>,
    editor: LineEditor,
    pending_lines: Vec<String>,
    lua: Lua,
    env: Table,
    thread: Option<Thread>,
    print_buffer: Rc<RefCell<ReplOutput>>,
    screen_reader_ptr: Rc<RefCell<*mut ScreenReader>>,
}

#[derive(Clone)]
pub struct LuaReplSession {
    state: Rc<RefCell<LuaReplState>>,
}

pub struct LuaReplView {
    view: View,
    title: String,
    session: LuaReplSession,
    rendered_input: String,
    rendered_cursor: usize,
}

impl LuaReplSession {
    pub fn new(history: Vec<String>) -> Result<Self> {
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

        let mut editor = LineEditor::new();
        editor.set_history(history);
        Ok(Self {
            state: Rc::new(RefCell::new(LuaReplState {
                transcript: initial_transcript(),
                editor,
                pending_lines: Vec::new(),
                lua,
                env,
                thread: None,
                print_buffer,
                screen_reader_ptr,
            })),
        })
    }
}

impl LuaReplView {
    pub fn new(rows: u16, cols: u16, history: Vec<String>) -> Result<Self> {
        let session = LuaReplSession::new(history)?;
        Ok(Self::from_session(rows, cols, session))
    }

    pub fn from_session(rows: u16, cols: u16, session: LuaReplSession) -> Self {
        let mut repl = Self {
            view: View::new(rows, cols),
            title: "Lua REPL".to_string(),
            session,
            rendered_input: String::new(),
            rendered_cursor: 0,
        };
        repl.render_full();
        repl
    }

    pub fn history(&self) -> Vec<String> {
        self.session.state.borrow().editor.history().to_vec()
    }

    fn set_screen_reader(&mut self, sr: &mut ScreenReader) {
        let screen_reader_ptr = Rc::clone(&self.session.state.borrow().screen_reader_ptr);
        *screen_reader_ptr.borrow_mut() = sr as *mut ScreenReader;
    }

    fn is_continuing(&self) -> bool {
        !self.session.state.borrow().pending_lines.is_empty()
    }

    fn prompt(&self) -> &'static str {
        if self.is_continuing() { "... " } else { "> " }
    }

    fn append_output(&mut self, text: &str) -> Vec<String> {
        let mut added = Vec::new();
        let mut state = self.session.state.borrow_mut();
        for line in text.split('\n') {
            let line = line.to_string();
            state.transcript.push(TranscriptLine::Output(line.clone()));
            added.push(line);
        }
        trim_transcript(&mut state.transcript);
        added
    }

    fn drain_print_buffer(&mut self) -> Vec<String> {
        let print_buffer = Rc::clone(&self.session.state.borrow().print_buffer);
        let added = std::mem::take(&mut print_buffer.borrow_mut().lines);
        if !added.is_empty() {
            let mut state = self.session.state.borrow_mut();
            state
                .transcript
                .extend(added.iter().cloned().map(TranscriptLine::Output));
            trim_transcript(&mut state.transcript);
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
        self.write_bytes(self.prompt().as_bytes());
        self.rendered_input.clear();
        self.rendered_cursor = 0;
    }

    fn try_append_input(&mut self) -> bool {
        let (input, cursor) = {
            let state = self.session.state.borrow();
            (state.editor.input().to_string(), state.editor.cursor())
        };
        let input_len = input.graphemes(true).count();
        let prev_input = self.rendered_input.as_str();
        let prev_len = prev_input.graphemes(true).count();
        let (_, cols) = self.view.size();
        let available = usize::from(cols).saturating_sub(self.prompt().len());
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
        let (transcript, pending_lines, input, editor_cursor) = {
            let state = self.session.state.borrow();
            (
                state.transcript.clone(),
                state.pending_lines.clone(),
                state.editor.input().to_string(),
                state.editor.cursor(),
            )
        };
        let prompt = if pending_lines.is_empty() {
            "> "
        } else {
            "... "
        };
        let available = cols.saturating_sub(prompt.len());
        let (visible_input, cursor_width) = visible_input_window(&input, editor_cursor, available);
        let cursor_col = prompt.len() + cursor_width;

        let body_rows = rows.saturating_sub(1);
        let mut body_lines = transcript
            .iter()
            .map(|line| render_transcript_line(line, cols))
            .collect::<Vec<_>>();
        body_lines.extend(
            pending_lines
                .iter()
                .enumerate()
                .map(|(index, line)| render_input_line(index != 0, line, cols)),
        );
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
        self.rendered_input = input;
        self.rendered_cursor = editor_cursor;
    }

    fn classify_input(&self, input: &str) -> Result<CompileOutcome> {
        let state = self.session.state.borrow();
        classify_input(&state.lua, &state.env, input).map_err(Error::lua)
    }

    fn start_eval(&mut self, func: Function) -> Result<()> {
        let lua = self.session.state.borrow().lua.clone();
        let thread = lua.create_thread(func).map_err(Error::lua)?;
        thread
            .set_hook(
                HookTriggers::new().every_nth_instruction(1000),
                |_lua, _debug| Ok(VmState::Yield),
            )
            .map_err(Error::lua)?;
        self.session.state.borrow_mut().thread = Some(thread);
        Ok(())
    }

    fn resume_eval(&mut self) -> Result<(bool, Vec<String>)> {
        let thread = self.session.state.borrow().thread.clone();
        let Some(thread) = thread else {
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
                    self.session.state.borrow_mut().thread = None;
                }
                Ok((true, added))
            }
            Err(err) => {
                let added = self.append_output(&format!("Error: {}", err));
                self.session.state.borrow_mut().thread = None;
                Ok((true, added))
            }
        }
    }

    fn clear_screen(&mut self) {
        self.session.state.borrow_mut().transcript = initial_transcript();
        self.render_full();
    }

    fn clear_current_line(&mut self) -> ViewAction {
        let mut state = self.session.state.borrow_mut();
        if state.editor.input().is_empty() {
            return ViewAction::None;
        }
        state.editor.clear();
        drop(state);
        self.render_full();
        ViewAction::Redraw
    }

    fn abort_continuation(&mut self) -> ViewAction {
        let mut state = self.session.state.borrow_mut();
        if state.pending_lines.is_empty() {
            return ViewAction::None;
        }
        state.pending_lines.clear();
        state.editor.clear();
        drop(state);
        self.render_full();
        ViewAction::Redraw
    }

    fn submit_input(&mut self) -> Result<ViewAction> {
        let (line, source, pending_empty) = {
            let state = self.session.state.borrow();
            let line = state.editor.input().to_string();
            let mut source = state.pending_lines.join("\n");
            if !source.is_empty() {
                source.push('\n');
            }
            source.push_str(&line);
            (line, source, state.pending_lines.is_empty())
        };
        if pending_empty && line.trim().is_empty() {
            return Ok(ViewAction::Bell);
        }

        match self.classify_input(&source)? {
            CompileOutcome::Incomplete => {
                self.write_bytes(b"\r\n");
                let mut state = self.session.state.borrow_mut();
                state.pending_lines.push(line);
                state.editor.clear();
                drop(state);
                self.write_prompt();
                Ok(ViewAction::Redraw)
            }
            CompileOutcome::Complete(func) => {
                self.write_bytes(b"\r\n");
                self.commit_submission(&source, line);
                if let Err(err) = self.start_eval(func) {
                    let added = self.append_output(&format!("Error: {err}"));
                    self.write_output_lines(&added);
                    self.write_prompt();
                }
                Ok(ViewAction::Redraw)
            }
            CompileOutcome::Error(err) => {
                self.write_bytes(b"\r\n");
                self.commit_submission(&source, line);
                let added = self.append_output(&format!("Error: {}", err));
                self.write_output_lines(&added);
                self.write_prompt();
                Ok(ViewAction::Redraw)
            }
        }
    }

    fn commit_submission(&mut self, source: &str, line: String) {
        let mut state = self.session.state.borrow_mut();
        state.editor.commit_history_entry(source);
        let pending = std::mem::take(&mut state.pending_lines);
        for (index, text) in pending.into_iter().chain(std::iter::once(line)).enumerate() {
            state.transcript.push(TranscriptLine::Input {
                continuation: index != 0,
                text,
            });
        }
        trim_transcript(&mut state.transcript);
        state.editor.clear();
        self.rendered_input.clear();
        self.rendered_cursor = 0;
    }

    fn apply_editor_action(
        &mut self,
        action: EditorAction,
        deleted_text: Option<String>,
        sr: &mut ScreenReader,
    ) -> Result<ViewAction> {
        match action {
            EditorAction::Submit => self.submit_input(),
            EditorAction::Changed => {
                if !self.try_append_input() {
                    self.apply_editor_update();
                }
                if let Some(text) = deleted_text {
                    // The global Backspace binding may have captured a
                    // screen-derived candidate before dispatch reached this
                    // overlay. The owned buffer is authoritative here; do not
                    // announce both candidates.
                    sr.clear_pending_delete();
                    sr.speak(&text, false)?;
                    // The exact removed grapheme came from the owned editor
                    // buffer. Reading the visual replacement as well is both
                    // redundant and wrong for a horizontally scrolling input
                    // window, where the cursor can remain at the margin.
                    Ok(ViewAction::RedrawSilently)
                } else {
                    Ok(ViewAction::Redraw)
                }
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
        ViewKind::LuaRepl
    }

    fn wants_tick(&self) -> bool {
        self.session.state.borrow().thread.is_some()
    }

    fn handle_input(
        &mut self,
        sr: &mut ScreenReader,
        input: &[u8],
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        self.set_screen_reader(sr);
        if input == b"\x1B" {
            self.session.state.borrow_mut().thread = None;
            return Ok(ViewAction::Pop);
        }
        if input == b"\x0C" {
            self.clear_screen();
            return Ok(ViewAction::Redraw);
        }
        if self.session.state.borrow().thread.is_some() {
            return Ok(ViewAction::Bell);
        }
        if input == b"\x03" && self.is_continuing() {
            return Ok(self.abort_continuation());
        }
        if input == b"\x15" {
            return Ok(self.clear_current_line());
        }
        let deleted_text = {
            let state = self.session.state.borrow();
            legacy_deleted_text(&state.editor, input)
        };
        let action = self.session.state.borrow_mut().editor.handle_bytes(input);
        self.apply_editor_action(action, deleted_text, sr)
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
            self.session.state.borrow_mut().thread = None;
            return Ok(ViewAction::Pop);
        }
        if matches!(key.control_code(), Some(0x0C)) {
            self.clear_screen();
            return Ok(ViewAction::Redraw);
        }
        if self.session.state.borrow().thread.is_some() {
            return Ok(ViewAction::Bell);
        }
        if matches!(key.control_code(), Some(0x03)) && self.is_continuing() {
            return Ok(self.abort_continuation());
        }
        if matches!(key.control_code(), Some(0x15)) {
            return Ok(self.clear_current_line());
        }
        let deleted_text = {
            let state = self.session.state.borrow();
            key_deleted_text(&state.editor, key)
        };
        let action = self.session.state.borrow_mut().editor.handle_key_input(key);
        self.apply_editor_action(action, deleted_text, sr)
    }

    fn handle_paste(
        &mut self,
        sr: &mut ScreenReader,
        contents: &str,
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        self.set_screen_reader(sr);
        if self.session.state.borrow().thread.is_some() {
            return Ok(ViewAction::Bell);
        }
        let contents = contents.replace("\r\n", "\n").replace('\r', "\n");
        let action = self
            .session
            .state
            .borrow_mut()
            .editor
            .handle_text(&contents);
        self.apply_editor_action(action, None, sr)
    }

    fn tick(&mut self, sr: &mut ScreenReader, _pty_stream: &mut dyn Write) -> Result<ViewAction> {
        self.set_screen_reader(sr);
        if self.session.state.borrow().thread.is_none() {
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
            if self.session.state.borrow().thread.is_none() {
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

fn legacy_deleted_text(editor: &LineEditor, input: &[u8]) -> Option<String> {
    match input {
        b"\x7f" | b"\x08" => grapheme_before_cursor(editor),
        b"\x1b[3~" => grapheme_at_cursor(editor),
        _ => None,
    }
}

fn key_deleted_text(editor: &LineEditor, key: &KeyInput) -> Option<String> {
    use terminput::{KeyCode, KeyModifiers};

    let event = key.normalized_event();
    match event.code {
        KeyCode::Backspace
            if !event
                .modifiers
                .intersects(KeyModifiers::CTRL | KeyModifiers::ALT | KeyModifiers::META) =>
        {
            grapheme_before_cursor(editor)
        }
        KeyCode::Delete => grapheme_at_cursor(editor),
        _ => None,
    }
}

fn grapheme_before_cursor(editor: &LineEditor) -> Option<String> {
    editor
        .cursor()
        .checked_sub(1)
        .and_then(|index| editor.input().graphemes(true).nth(index))
        .map(str::to_owned)
}

fn grapheme_at_cursor(editor: &LineEditor) -> Option<String> {
    editor
        .input()
        .graphemes(true)
        .nth(editor.cursor())
        .map(str::to_owned)
}

enum CompileOutcome {
    Complete(Function),
    Incomplete,
    Error(LuaError),
}

fn classify_input(lua: &Lua, env: &Table, input: &str) -> mlua::Result<CompileOutcome> {
    if let Some(rest) = input.strip_prefix('=') {
        return Ok(
            match compile_function(lua, env, &format!("return {rest}")) {
                Ok(func) => CompileOutcome::Complete(func),
                Err(err) if syntax_error_is_incomplete(&err) => CompileOutcome::Incomplete,
                Err(err) => CompileOutcome::Error(err),
            },
        );
    }

    let expression_error = match compile_function(lua, env, &format!("return {input}")) {
        Ok(func) => return Ok(CompileOutcome::Complete(func)),
        Err(err @ LuaError::SyntaxError { .. }) => err,
        Err(err) => return Err(err),
    };
    match compile_function(lua, env, input) {
        Ok(func) => Ok(CompileOutcome::Complete(func)),
        Err(statement_error @ LuaError::SyntaxError { .. }) => {
            if syntax_error_is_incomplete(&expression_error)
                || syntax_error_is_incomplete(&statement_error)
            {
                Ok(CompileOutcome::Incomplete)
            } else {
                Ok(CompileOutcome::Error(statement_error))
            }
        }
        Err(err) => Err(err),
    }
}

fn compile_function(lua: &Lua, env: &Table, source: &str) -> mlua::Result<Function> {
    lua.load(source)
        .set_name("repl")
        .set_environment(env.clone())
        .into_function()
}

fn syntax_error_is_incomplete(error: &LuaError) -> bool {
    matches!(
        error,
        LuaError::SyntaxError {
            incomplete_input: true,
            ..
        }
    )
}

fn initial_transcript() -> Vec<TranscriptLine> {
    vec![TranscriptLine::Output("Lua REPL ready.".to_string())]
}

fn trim_transcript(transcript: &mut Vec<TranscriptLine>) {
    const MAX_LINES: usize = 1000;
    if transcript.len() > MAX_LINES {
        let excess = transcript.len() - MAX_LINES;
        transcript.drain(0..excess);
    }
}

fn render_transcript_line(line: &TranscriptLine, cols: usize) -> String {
    match line {
        TranscriptLine::Input { continuation, text } => {
            render_input_line(*continuation, text, cols)
        }
        TranscriptLine::Output(text) => text.clone(),
    }
}

fn render_input_line(continuation: bool, text: &str, cols: usize) -> String {
    let prompt = if continuation { "... " } else { "> " };
    let displayed = text
        .graphemes(true)
        .map(display_grapheme)
        .collect::<String>();
    truncate_to_width(&format!("{prompt}{displayed}"), cols)
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
    use super::{
        CLOSE_HINT, CompileOutcome, LuaReplSession, LuaReplView, classify_input, truncate_to_width,
        visible_input_window,
    };
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

    fn enter(repl: &mut LuaReplView, sr: &mut ScreenReader, input: &[u8]) {
        repl.handle_input(sr, input, &mut Vec::new())
            .expect("enter REPL input");
    }

    fn finish_eval(repl: &mut LuaReplView, sr: &mut ScreenReader) {
        for _ in 0..100 {
            if !repl.wants_tick() {
                return;
            }
            repl.tick(sr, &mut Vec::new())
                .expect("resume Lua evaluation");
        }
        panic!("Lua evaluation did not finish");
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
    fn lua_repl_backspace_speaks_the_owned_character_when_input_scrolls_horizontally() {
        let mut repl = LuaReplView::new(4, 8, Vec::new()).expect("create lua repl");
        let (mut sr, speaks) = make_screen_reader();
        enter(&mut repl, &mut sr, b"aaaaaam");
        speaks.borrow_mut().clear();

        let action = repl
            .handle_input(&mut sr, b"\x7f", &mut Vec::new())
            .expect("handle backspace");

        assert!(matches!(action, crate::views::ViewAction::RedrawSilently));
        assert_eq!(repl.session.state.borrow().editor.input(), "aaaaaa");
        assert_eq!(speaks.borrow().as_slice(), ["m"]);
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
        assert!(screen.contents().contains("Lua REPL ready."));
        assert!(screen.contents().contains("> "));
    }

    #[test]
    fn lua_parser_drives_statement_and_expression_continuation() {
        let mut repl = LuaReplView::new(10, 40, Vec::new()).expect("create lua repl");
        let (mut sr, _speaks) = make_screen_reader();

        enter(&mut repl, &mut sr, b"function foo()\r");
        assert!(repl.model().screen().contents().contains("... "));
        enter(&mut repl, &mut sr, b"return 41 +\r");
        assert!(repl.model().screen().contents().contains("... "));
        enter(&mut repl, &mut sr, b"1\r");
        assert!(repl.model().screen().contents().contains("... "));
        enter(&mut repl, &mut sr, b"end\r");
        finish_eval(&mut repl, &mut sr);

        assert_eq!(
            repl.history(),
            ["function foo()\nreturn 41 +\n1\nend".to_string()]
        );

        enter(&mut repl, &mut sr, b"foo()\r");
        finish_eval(&mut repl, &mut sr);
        let contents = repl.model().screen().contents();
        assert!(contents.lines().any(|line| line.trim() == "42"));
        assert!(contents.contains("> "));
    }

    #[test]
    fn lua_repl_inspect_builtin_displays_nested_tables() {
        let mut repl = LuaReplView::new(14, 70, Vec::new()).expect("create lua repl");
        let (mut sr, _speaks) = make_screen_reader();

        enter(
            &mut repl,
            &mut sr,
            b"lector.inspect({name = \"demo\", nested = {1, 2}})\r",
        );
        finish_eval(&mut repl, &mut sr);

        let contents = repl.model().screen().contents();
        assert!(
            contents
                .lines()
                .any(|line| line.trim() == "name = \"demo\",")
        );
        assert!(contents.lines().any(|line| line.trim() == "nested = {"));
        assert!(contents.lines().any(|line| line.trim() == "1,"));
        assert!(contents.lines().any(|line| line.trim() == "2,"));
    }

    #[test]
    fn lua_parser_classifies_general_incomplete_constructs() {
        let session = LuaReplSession::new(Vec::new()).expect("create Lua session");
        let state = session.state.borrow();

        for source in [
            "if true then",
            "repeat\nlocal value = 1",
            "local value = {",
            "local value = [[unterminated",
            "1 +",
        ] {
            assert!(
                matches!(
                    classify_input(&state.lua, &state.env, source).expect("classify Lua"),
                    CompileOutcome::Incomplete
                ),
                "source={source:?}"
            );
        }
        assert!(matches!(
            classify_input(&state.lua, &state.env, "local = 1").expect("classify invalid Lua"),
            CompileOutcome::Error(_)
        ));
    }

    #[test]
    fn lua_repl_ctrl_c_aborts_continuation_without_history_or_lua_changes() {
        let mut repl = LuaReplView::new(10, 50, Vec::new()).expect("create lua repl");
        let (mut sr, _speaks) = make_screen_reader();

        enter(&mut repl, &mut sr, b"function abandoned()\r");
        enter(&mut repl, &mut sr, b"return 7");
        enter(&mut repl, &mut sr, b"\x03");
        let contents = repl.model().screen().contents();
        assert!(contents.contains("> "));
        assert!(!contents.contains("function abandoned"));
        assert!(repl.history().is_empty());

        enter(&mut repl, &mut sr, b"abandoned == nil\r");
        finish_eval(&mut repl, &mut sr);
        assert!(
            repl.model()
                .screen()
                .contents()
                .lines()
                .any(|line| line.trim() == "true")
        );
    }

    #[test]
    fn lua_repl_ctrl_l_preserves_pending_and_current_input() {
        let mut repl = LuaReplView::new(10, 50, Vec::new()).expect("create lua repl");
        let (mut sr, _speaks) = make_screen_reader();
        repl.append_output("old output");
        repl.render_full();

        enter(&mut repl, &mut sr, b"function kept()\r");
        enter(&mut repl, &mut sr, b"return 9");
        enter(&mut repl, &mut sr, b"\x0c");

        let contents = repl.model().screen().contents();
        assert!(contents.contains("Lua REPL ready."));
        assert!(!contents.contains("old output"));
        assert!(contents.contains("> function kept()"));
        assert!(contents.contains("... return 9"));
    }

    #[test]
    fn lua_repl_ctrl_l_preserves_main_prompt_draft() {
        let mut repl = LuaReplView::new(8, 40, Vec::new()).expect("create lua repl");
        let (mut sr, _speaks) = make_screen_reader();
        repl.append_output("old output");
        repl.render_full();

        enter(&mut repl, &mut sr, b"not submitted");
        enter(&mut repl, &mut sr, b"\x0c");

        let contents = repl.model().screen().contents();
        assert!(contents.contains("Lua REPL ready."));
        assert!(contents.contains("> not submitted"));
        assert!(!contents.contains("old output"));
    }

    #[test]
    fn lua_repl_ctrl_u_clears_only_the_current_line() {
        let mut repl = LuaReplView::new(10, 50, Vec::new()).expect("create lua repl");
        let (mut sr, _speaks) = make_screen_reader();

        enter(&mut repl, &mut sr, b"function kept()\r");
        enter(&mut repl, &mut sr, b"discard me");
        enter(&mut repl, &mut sr, b"\x15");

        let contents = repl.model().screen().contents();
        assert!(contents.contains("> function kept()"));
        assert!(contents.contains("... "));
        assert!(!contents.contains("discard me"));
    }

    #[test]
    fn lua_repl_session_restores_transcript_draft_continuation_and_environment() {
        let session = LuaReplSession::new(Vec::new()).expect("create Lua session");
        let mut first = LuaReplView::from_session(12, 60, session.clone());
        let (mut sr, _speaks) = make_screen_reader();

        enter(&mut first, &mut sr, b"saved = 12\r");
        finish_eval(&mut first, &mut sr);
        enter(&mut first, &mut sr, b"function pending()\r");
        enter(&mut first, &mut sr, b"return saved");
        drop(first);

        let mut reopened = LuaReplView::from_session(12, 60, session);
        let contents = reopened.model().screen().contents();
        assert!(contents.contains("> saved = 12"));
        assert!(contents.contains("> function pending()"));
        assert!(contents.contains("... return saved"));

        enter(&mut reopened, &mut sr, b"\x03");
        enter(&mut reopened, &mut sr, b"saved\r");
        finish_eval(&mut reopened, &mut sr);
        assert!(
            reopened
                .model()
                .screen()
                .contents()
                .lines()
                .any(|line| line.trim() == "12")
        );
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
