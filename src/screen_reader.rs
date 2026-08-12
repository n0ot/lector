use super::{
    clipboard::Clipboard,
    keymap::{InputMode, KeyBindings},
    speech::{self, Speech},
    table::Session as TableSession,
};
use mlua::{Lua, WeakLua};
use std::{collections::VecDeque, rc::Rc};

mod auto_read;
mod hooks;
mod options;
mod tracking;

use auto_read::AutoReadBuffers;
use hooks::LuaHooks;
use options::Options;
use tracking::{CursorTrackingMode, PendingDelete};

pub type Result<T> = std::result::Result<T, Error>;

const MAX_PENDING_KEY_ECHO_CHARS: usize = 256;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error(transparent)]
    Speech(#[from] speech::Error),
    #[error("Lua: {0}")]
    Lua(String),
    #[error("unknown hook: {0}")]
    UnknownHook(String),
    #[error("hook value must be a function or nil")]
    InvalidHookValue,
    #[error("Lua bindings are only available in init.lua")]
    InvalidLuaBindingContext,
    #[error("Lua hooks are only available in init.lua")]
    InvalidLuaHookContext,
    #[error("on_live_read must return a string or nil")]
    InvalidLiveReadResult,
}

impl Error {
    fn lua(error: mlua::Error) -> Self {
        Self::Lua(error.to_string())
    }
}

pub struct ScreenReader {
    speech: Speech,
    options: Options,
    last_key: Vec<u8>,
    pending_key_echo: VecDeque<char>,
    cursor_tracking_mode: CursorTrackingMode,
    clipboard: Clipboard,
    pass_through: bool,
    key_bindings: KeyBindings,
    table_session: TableSession,
    terminal_focused: bool,
    lua_ctx: Option<Rc<Lua>>,
    lua_ctx_weak: Option<WeakLua>,
    lua_hooks: LuaHooks,
    auto_read_buffers: AutoReadBuffers,
    pending_delete: Option<PendingDelete>,
    pending_history_navigation: bool,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum ClipboardMove {
    Empty,
    Boundary,
    Selected,
}

impl ScreenReader {
    pub fn new(speech: Speech) -> Self {
        ScreenReader {
            speech,
            options: Options::default(),
            last_key: Vec::new(),
            pending_key_echo: VecDeque::new(),
            cursor_tracking_mode: CursorTrackingMode::On,
            clipboard: Default::default(),
            pass_through: false,
            key_bindings: KeyBindings::new(),
            table_session: TableSession::default(),
            terminal_focused: true,
            lua_ctx: None,
            lua_ctx_weak: None,
            lua_hooks: LuaHooks::default(),
            auto_read_buffers: AutoReadBuffers::default(),
            pending_delete: None,
            pending_history_navigation: false,
        }
    }

    pub fn set_lua_context(&mut self, lua: Rc<Lua>) {
        self.lua_ctx_weak = Some(lua.weak());
        self.lua_ctx = Some(lua);
    }

    pub fn input_mode(&self) -> InputMode {
        self.table_session.mode()
    }

    pub(crate) fn speech(&self) -> &Speech {
        &self.speech
    }

    pub(crate) fn speech_mut(&mut self) -> &mut Speech {
        &mut self.speech
    }

    pub fn stop_speaking(&mut self) -> Result<()> {
        self.speech.stop()?;
        Ok(())
    }

    pub fn last_key(&self) -> &[u8] {
        &self.last_key
    }

    pub(crate) fn record_last_key(&mut self, raw: &[u8]) {
        self.last_key.clear();
        self.last_key.extend_from_slice(raw);
    }

    pub(crate) fn record_forwarded_character(&mut self, character: char) {
        if !self.suppress_key_echo() {
            return;
        }
        if self.pending_key_echo.len() == MAX_PENDING_KEY_ECHO_CHARS {
            self.pending_key_echo.pop_front();
        }
        self.pending_key_echo.push_back(character);
    }

    pub(crate) fn set_pending_history_navigation(&mut self) {
        self.pending_history_navigation = true;
    }

    pub(crate) fn clear_pending_history_navigation(&mut self) {
        self.pending_history_navigation = false;
    }

    pub(crate) fn take_pending_history_navigation(&mut self) -> bool {
        std::mem::take(&mut self.pending_history_navigation)
    }

    fn should_suppress_key_echo(&mut self, text: &str) -> bool {
        if !self.suppress_key_echo() || text.is_empty() {
            return false;
        }

        let mut matched = 0;
        for character in text.chars() {
            if self.pending_key_echo.get(matched) != Some(&character) {
                self.pending_key_echo.clear();
                return false;
            }
            matched += 1;
        }
        for _ in 0..matched {
            self.pending_key_echo.pop_front();
        }
        true
    }

    pub(crate) fn request_pass_through(&mut self) {
        self.pass_through = true;
    }

    pub(crate) fn take_pass_through(&mut self) -> bool {
        std::mem::take(&mut self.pass_through)
    }

    pub(crate) fn key_bindings(&self) -> &KeyBindings {
        &self.key_bindings
    }

    pub(crate) fn key_bindings_mut(&mut self) -> &mut KeyBindings {
        &mut self.key_bindings
    }

    pub(crate) fn clipboard_text(&self) -> Option<&str> {
        self.clipboard.get()
    }

    pub fn push_clipboard(&mut self, text: String) -> Result<()> {
        self.clipboard.put(text);
        self.hook_on_clipboard_change("push", self.clipboard.get())
    }

    pub(crate) fn previous_clipboard(&mut self) -> Result<ClipboardMove> {
        if self.clipboard.size() == 0 {
            return Ok(ClipboardMove::Empty);
        }
        if !self.clipboard.prev() {
            return Ok(ClipboardMove::Boundary);
        }
        self.hook_on_clipboard_change("prev", self.clipboard.get())?;
        Ok(ClipboardMove::Selected)
    }

    pub(crate) fn next_clipboard(&mut self) -> Result<ClipboardMove> {
        if self.clipboard.size() == 0 {
            return Ok(ClipboardMove::Empty);
        }
        if !self.clipboard.next() {
            return Ok(ClipboardMove::Boundary);
        }
        self.hook_on_clipboard_change("next", self.clipboard.get())?;
        Ok(ClipboardMove::Selected)
    }

    pub fn terminal_focused(&self) -> bool {
        self.terminal_focused
    }

    pub(crate) fn set_terminal_focused(&mut self, focused: bool) -> Result<()> {
        self.terminal_focused = focused;
        if !focused && self.stop_speech_on_focus_loss() {
            self.stop_speaking()?;
        }
        Ok(())
    }

    pub(crate) fn suppress_cursor_tracking_once(&mut self) {
        self.cursor_tracking_mode = CursorTrackingMode::OffOnce;
    }

    pub fn help_mode(&self) -> bool {
        self.options.help_mode()
    }

    pub fn set_help_mode(&mut self, value: bool) {
        self.options.set_help_mode(value);
    }

    pub(crate) fn toggle_help_mode(&mut self) -> bool {
        self.options.toggle_help_mode()
    }

    pub fn auto_read_enabled(&self) -> bool {
        self.options.auto_read()
    }

    pub fn set_auto_read_enabled(&mut self, value: bool) {
        self.options.set_auto_read(value);
    }

    pub(crate) fn toggle_auto_read(&mut self) -> bool {
        self.options.toggle_auto_read()
    }

    pub fn suppress_key_echo(&self) -> bool {
        self.options.suppress_key_echo()
    }

    pub fn set_suppress_key_echo(&mut self, value: bool) {
        self.options.set_suppress_key_echo(value);
        if !value {
            self.pending_key_echo.clear();
        }
    }

    pub fn review_follows_screen_cursor(&self) -> bool {
        self.options.review_follows_screen_cursor()
    }

    pub fn set_review_follows_screen_cursor(&mut self, value: bool) {
        self.options.set_review_follows_screen_cursor(value);
    }

    pub(crate) fn toggle_review_follows_screen_cursor(&mut self) -> bool {
        self.options.toggle_review_follows_screen_cursor()
    }

    pub fn highlight_tracking_enabled(&self) -> bool {
        self.options.highlight_tracking()
    }

    pub fn set_highlight_tracking_enabled(&mut self, value: bool) {
        self.options.set_highlight_tracking(value);
    }

    pub fn table_header_auto(&self) -> bool {
        self.options.table_header_auto()
    }

    pub(crate) fn toggle_table_header_auto(&mut self) -> bool {
        self.options.toggle_table_header_auto()
    }

    pub fn stop_speech_on_focus_loss(&self) -> bool {
        self.options.stop_speech_on_focus_loss()
    }

    pub fn set_stop_speech_on_focus_loss(&mut self, value: bool) {
        self.options.set_stop_speech_on_focus_loss(value);
    }

    pub(crate) fn toggle_stop_speech_on_focus_loss(&mut self) -> bool {
        self.options.toggle_stop_speech_on_focus_loss()
    }

    pub(crate) fn table_session(&self) -> &TableSession {
        &self.table_session
    }

    pub(crate) fn table_session_mut(&mut self) -> &mut TableSession {
        &mut self.table_session
    }

    pub fn speak(&mut self, text: &str, interrupt: bool) -> Result<()> {
        if text.is_empty() || !self.terminal_focused {
            return Ok(());
        }
        self.call_hook_on_speech_start(text, interrupt)?;
        let result = self.speech.speak(text, interrupt);
        let ok = result.is_ok();
        self.call_hook_on_speech_end(text, interrupt, ok)?;
        Ok(result?)
    }
}

#[cfg(test)]
mod tests {
    use super::{ClipboardMove, ScreenReader};
    use crate::{perform, speech, view::View};
    use mlua::{Lua, Value};
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

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

    fn make_sr() -> (ScreenReader, Rc<RefCell<Vec<String>>>) {
        let speaks = Rc::new(RefCell::new(Vec::new()));
        let driver = TestDriver {
            speaks: Rc::clone(&speaks),
        };
        let speech = speech::Speech::new(Box::new(driver));
        let sr = ScreenReader::new(speech);
        (sr, speaks)
    }

    #[test]
    fn auto_read_returns_false_when_unchanged() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 10);
        let mut reporter = perform::Reporter::new();

        view.process_changes(b"hello");
        view.finalize_changes(0);

        let read = sr.auto_read(&mut view, &mut reporter).unwrap();
        assert!(!read);
        assert!(speaks.borrow().is_empty());
    }

    #[test]
    fn auto_read_speaks_new_text() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 10);
        let mut reporter = perform::Reporter::new();

        view.process_changes(b"hi");
        let read = sr.auto_read(&mut view, &mut reporter).unwrap();
        assert!(read);
        let speaks = speaks.borrow();
        assert_eq!(speaks.len(), 1);
        assert_eq!(speaks[0], "hi");
    }

    #[test]
    fn auto_read_speaks_key_echo_by_default() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 10);
        let mut reporter = perform::Reporter::new();

        sr.record_last_key(b"g");
        view.process_changes(b"abcdefg");
        let read = sr.auto_read(&mut view, &mut reporter).unwrap();
        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["abcdefg"]);
    }

    #[test]
    fn auto_read_suppresses_live_echo_when_enabled() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 10);
        let mut reporter = perform::Reporter::new();

        sr.set_suppress_key_echo(true);
        for character in "abcdefg".chars() {
            sr.record_forwarded_character(character);
        }
        sr.record_last_key(b"g");
        view.process_changes(b"abcdefg");
        let read = sr.auto_read(&mut view, &mut reporter).unwrap();
        assert!(read);
        assert!(speaks.borrow().is_empty());
    }

    #[test]
    fn auto_read_suppresses_slow_character_at_a_time_echo_when_enabled() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 10);
        let mut reporter = perform::Reporter::new();

        sr.set_suppress_key_echo(true);
        for character in "abcdefg".chars() {
            sr.record_forwarded_character(character);
        }
        sr.record_last_key(b"g");

        for byte in b"abcdefg" {
            view.process_changes(&[*byte]);
            let read = sr.auto_read(&mut view, &mut reporter).unwrap();
            assert!(read);
            assert!(speaks.borrow().is_empty());
            view.finalize_changes(0);
        }
    }

    #[test]
    fn auto_read_suppresses_diff_echo_when_enabled() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 10);
        let mut reporter = perform::Reporter::new();

        view.process_changes(b"a");
        view.finalize_changes(0);
        sr.set_suppress_key_echo(true);
        sr.record_forwarded_character('b');
        sr.record_last_key(b"b");
        view.process_changes(b"\rb");
        reporter.cursor_moves = 1;

        let read = sr.auto_read(&mut view, &mut reporter).unwrap();
        assert!(read);
        assert!(speaks.borrow().is_empty());
    }

    #[test]
    fn auto_read_speaks_multiple_inserted_runs_from_single_changed_line() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 40);
        let mut reporter = perform::Reporter::new();

        view.process_changes(b"left one right two");
        view.finalize_changes(0);

        view.process_changes(b"\r\x1B[Kleft alpha right beta");
        reporter.cursor_moves = 1;
        let read = sr.auto_read(&mut view, &mut reporter).unwrap();

        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["alpha beta"]);
    }

    #[test]
    fn auto_read_speaks_short_status_line_replacements() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 120);
        let mut reporter = perform::Reporter::new();

        view.process_changes(
            b"[dev] 1:bash* 2:bash-                                             bash.1",
        );
        view.finalize_changes(0);

        view.process_changes(
            b"\r\x1B[K[dev] 1:caffeinate* 2:bash-                                      caffeinate.1",
        );
        reporter.cursor_moves = 1;
        let read = sr.auto_read(&mut view, &mut reporter).unwrap();

        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["caffeinate"]);
    }

    #[test]
    fn auto_read_speaks_shorter_replacements_to_word_boundaries() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 120);
        let mut reporter = perform::Reporter::new();

        view.process_changes(
            b"[dev] 1:bash* 2:bash-                                             bash.1",
        );
        view.finalize_changes(0);

        view.process_changes(
            b"\r\x1B[K[dev] 1:gh* 2:bash-                                                gh.1",
        );
        reporter.cursor_moves = 1;
        let read = sr.auto_read(&mut view, &mut reporter).unwrap();

        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["gh"]);
    }

    #[test]
    fn auto_read_collapses_contiguous_duplicate_replacement_hunks() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 40);
        let mut reporter = perform::Reporter::new();

        view.process_changes(b"foo bar foo");
        view.finalize_changes(0);

        view.process_changes(b"\r\x1B[Kbum bar bum");
        reporter.cursor_moves = 1;
        let read = sr.auto_read(&mut view, &mut reporter).unwrap();

        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["bum"]);
    }

    #[test]
    fn auto_read_preserves_non_contiguous_duplicate_replacement_hunks() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 40);
        let mut reporter = perform::Reporter::new();

        view.process_changes(b"foo bar baz foo");
        view.finalize_changes(0);

        view.process_changes(b"\r\x1B[Kbum bar bat bum");
        reporter.cursor_moves = 1;
        let read = sr.auto_read(&mut view, &mut reporter).unwrap();

        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["bum bat bum"]);
    }

    #[test]
    fn auto_read_keeps_repeated_words_inside_one_replacement_hunk() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 40);
        let mut reporter = perform::Reporter::new();

        view.process_changes(b"foo foo");
        view.finalize_changes(0);

        view.process_changes(b"\r\x1B[Kbum bum");
        reporter.cursor_moves = 1;
        let read = sr.auto_read(&mut view, &mut reporter).unwrap();

        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["bum bum"]);
    }

    #[test]
    fn vertical_cursor_tracking_stays_silent_on_whitespace_only_line() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 10);

        view.process_changes(b"text\x1B[2;1H \x1B[1;1H");
        view.finalize_changes(0);

        view.process_changes(b"\x1B[2;1H");
        sr.track_cursor(&mut view).unwrap();

        assert!(speaks.borrow().is_empty());
    }

    #[test]
    fn deferred_backspace_only_speaks_when_cursor_moves_left() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 10);

        view.process_changes(b"$ x");
        view.finalize_changes(0);
        sr.defer_backspace(&view);

        view.process_changes(b"\x08\x1B[P");
        let read = sr.resolve_pending_delete(&view).unwrap();
        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["x"]);
    }

    #[test]
    fn deferred_backspace_stays_silent_when_cursor_does_not_move() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 10);

        view.process_changes(b"$ ");
        view.finalize_changes(0);
        sr.defer_backspace(&view);

        let read = sr.resolve_pending_delete(&view).unwrap();
        assert!(!read);
        assert!(speaks.borrow().is_empty());
    }

    #[test]
    fn deferred_delete_speaks_when_screen_changes() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 10);

        view.process_changes(b"abc\x1B[D\x1B[D");
        view.finalize_changes(0);
        sr.defer_delete(&view);

        view.process_changes(b"\x1B[P");
        let read = sr.resolve_pending_delete(&view).unwrap();
        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["b"]);
    }

    struct StopDriver(Rc<Cell<usize>>);

    impl speech::Driver for StopDriver {
        fn speak(&mut self, _text: &str, _interrupt: bool) -> anyhow::Result<()> {
            Ok(())
        }

        fn stop(&mut self) -> anyhow::Result<()> {
            self.0.set(self.0.get() + 1);
            Ok(())
        }

        fn get_rate(&self) -> f32 {
            1.0
        }

        fn set_rate(&mut self, _rate: f32) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn focus_state_owns_focus_loss_speech_policy() {
        let stops = Rc::new(Cell::new(0));
        let speech = speech::Speech::new(Box::new(StopDriver(Rc::clone(&stops))));
        let mut sr = ScreenReader::new(speech);

        sr.set_terminal_focused(false).unwrap();
        assert!(!sr.terminal_focused());
        assert_eq!(stops.get(), 1);

        sr.set_stop_speech_on_focus_loss(false);
        sr.set_terminal_focused(true).unwrap();
        sr.set_terminal_focused(false).unwrap();
        assert_eq!(stops.get(), 1);
    }

    #[test]
    fn lua_context_errors_identify_the_requested_feature() {
        let (mut sr, _) = make_sr();
        let init_lua = Rc::new(Lua::new());
        sr.set_lua_context(Rc::clone(&init_lua));
        let other_lua = Lua::new();

        assert_eq!(
            sr.lua_binding_context(&other_lua).unwrap_err().to_string(),
            "Lua bindings are only available in init.lua"
        );

        let hook = other_lua.create_function(|_, ()| Ok(())).unwrap();
        assert_eq!(
            sr.set_hook(&other_lua, "on_startup", Value::Function(hook))
                .unwrap_err()
                .to_string(),
            "Lua hooks are only available in init.lua"
        );
    }

    #[test]
    fn clipboard_navigation_reports_empty_boundaries_and_selection() {
        let (mut sr, _) = make_sr();
        assert_eq!(sr.next_clipboard().unwrap(), ClipboardMove::Empty);

        sr.push_clipboard("older".to_string()).unwrap();
        sr.push_clipboard("newer".to_string()).unwrap();
        assert_eq!(sr.clipboard_text(), Some("newer"));
        assert_eq!(sr.previous_clipboard().unwrap(), ClipboardMove::Boundary);
        assert_eq!(sr.next_clipboard().unwrap(), ClipboardMove::Selected);
        assert_eq!(sr.clipboard_text(), Some("older"));
        assert_eq!(sr.next_clipboard().unwrap(), ClipboardMove::Boundary);
        assert_eq!(sr.previous_clipboard().unwrap(), ClipboardMove::Selected);
        assert_eq!(sr.clipboard_text(), Some("newer"));
    }
}
