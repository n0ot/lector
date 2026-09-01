use super::{
    clipboard::{Clipboard, ClipboardRegister, SystemClipboard, SystemClipboardProvider},
    keymap::{InputMode, KeyBindings},
    speech::{self, ReaderSpeechEvent, ReaderSupport, Speech, SpeechServerSpec},
    table::Session as TableSession,
};
use mlua::{Lua, WeakLua};
use std::{collections::VecDeque, fmt, rc::Rc, str::FromStr};

mod auto_read;
mod hooks;
mod options;
mod tracking;
mod visual_focus;

use auto_read::AutoReadBuffers;
use hooks::LuaHooks;
use options::Options;
use tracking::{CursorTrackingMode, PendingDelete};
use visual_focus::PendingVisualFocusInput;

pub type Result<T> = std::result::Result<T, Error>;

const MAX_PENDING_KEY_ECHO_CHARS: usize = 256;
const MAX_PENDING_DELETE_INTENTS: usize = 64;
const MAX_PENDING_DELETE_PRESENTATIONS: u8 = 64;

#[derive(Clone, Copy)]
struct PendingKeyEcho {
    character: char,
    screen: crate::terminal::ScreenIdentity,
}

/// How bells received from panes in a tmux control connection are presented.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TmuxBellMode {
    /// Discard pane bells.
    Off,
    /// Speak stable connection, session, window, and pane context.
    Spoken,
    /// Speak concise context for a background window and emit one physical BEL
    /// at a scheduler transaction boundary.
    #[default]
    Audible,
}

impl fmt::Display for TmuxBellMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Off => "off",
            Self::Spoken => "spoken",
            Self::Audible => "audible",
        })
    }
}

impl FromStr for TmuxBellMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "spoken" => Ok(Self::Spoken),
            "audible" => Ok(Self::Audible),
            _ => anyhow::bail!("tmux bell mode must be off, spoken, or audible"),
        }
    }
}

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
    #[error("lector.o.speech.server is startup-only; use lector.api.set_speech() at runtime")]
    InvalidLuaSpeechConfigContext,
    #[error("on_live_read must return a string or nil")]
    InvalidLiveReadResult,
    #[error("clipboard: {0}")]
    Clipboard(String),
}

impl Error {
    fn lua(error: mlua::Error) -> Self {
        Self::Lua(error.to_string())
    }
}

pub struct ScreenReader {
    speech: Speech,
    speech_server_spec: SpeechServerSpec,
    pending_speech_reconfiguration: Option<SpeechServerSpec>,
    lua_configuration_open: bool,
    options: Options,
    last_key: Vec<u8>,
    pending_key_echo: VecDeque<PendingKeyEcho>,
    key_echo_stream_active: bool,
    cursor_tracking_mode: CursorTrackingMode,
    clipboard: Clipboard,
    system_clipboard: SystemClipboard,
    pass_through: bool,
    key_bindings: KeyBindings,
    table_session: TableSession,
    terminal_focused: bool,
    lua_ctx: Option<Rc<Lua>>,
    lua_ctx_weak: Option<WeakLua>,
    lua_hooks: LuaHooks,
    auto_read_buffers: AutoReadBuffers,
    pending_deletes: VecDeque<PendingDelete>,
    input_sequence: u64,
    pending_history_navigation: bool,
    pending_visual_focus_input: Option<PendingVisualFocusInput>,
    reader_support: ReaderSupport,
    reader_speech_events: VecDeque<ReaderSpeechEvent>,
    reader_auto_read_suppressed: bool,
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
            speech_server_spec: SpeechServerSpec::default(),
            pending_speech_reconfiguration: None,
            lua_configuration_open: false,
            options: Options::default(),
            last_key: Vec::new(),
            pending_key_echo: VecDeque::new(),
            key_echo_stream_active: false,
            cursor_tracking_mode: CursorTrackingMode::On,
            clipboard: Default::default(),
            system_clipboard: Default::default(),
            pass_through: false,
            key_bindings: KeyBindings::new(),
            table_session: TableSession::default(),
            terminal_focused: true,
            lua_ctx: None,
            lua_ctx_weak: None,
            lua_hooks: LuaHooks::default(),
            auto_read_buffers: AutoReadBuffers::default(),
            pending_deletes: VecDeque::new(),
            input_sequence: 0,
            pending_history_navigation: false,
            pending_visual_focus_input: None,
            reader_support: ReaderSupport::default(),
            reader_speech_events: VecDeque::new(),
            reader_auto_read_suppressed: false,
        }
    }

    #[doc(hidden)]
    pub fn set_reader_support(&mut self, support: ReaderSupport) {
        self.reader_support = support;
        if !support.is_supported() {
            self.reader_speech_events.clear();
        }
    }

    pub(crate) fn reader_support(&self) -> ReaderSupport {
        self.reader_support
    }

    #[doc(hidden)]
    pub fn push_reader_speech_events(
        &mut self,
        events: impl IntoIterator<Item = ReaderSpeechEvent>,
    ) {
        const MAX_EVENTS: usize = 256;
        for event in events {
            if self.reader_speech_events.len() == MAX_EVENTS {
                let _ = self.reader_speech_events.pop_front();
            }
            self.reader_speech_events.push_back(event);
        }
    }

    pub(crate) fn take_reader_speech_events(&mut self) -> Vec<ReaderSpeechEvent> {
        self.reader_speech_events.drain(..).collect()
    }

    pub fn set_lua_context(&mut self, lua: Rc<Lua>) {
        self.lua_ctx_weak = Some(lua.weak());
        self.lua_ctx = Some(lua);
        self.lua_configuration_open = true;
    }

    /// Finish the startup-only portion of `init.lua` configuration.
    ///
    /// Hooks registered by `init.lua` run after this boundary, so changing the
    /// speech server from a hook must use the asynchronous
    /// `lector.api.set_speech()` path just like the Lua REPL does.
    pub(crate) fn finish_lua_configuration(&mut self) {
        self.lua_configuration_open = false;
    }

    pub fn speech_server_spec(&self) -> &SpeechServerSpec {
        &self.speech_server_spec
    }

    pub(crate) fn set_startup_speech_server_spec(&mut self, spec: SpeechServerSpec) -> Result<()> {
        if !self.lua_configuration_open {
            return Err(Error::InvalidLuaSpeechConfigContext);
        }
        self.speech_server_spec = spec;
        Ok(())
    }

    /// Queue a nonblocking, transactional speech-server replacement request.
    ///
    /// Only the latest unconsumed request is retained. The active
    /// configuration is intentionally unchanged until the core has started and
    /// handshaken the candidate and calls [`Self::commit_speech_reconfiguration`].
    pub fn request_speech_reconfiguration(&mut self, spec: SpeechServerSpec) {
        self.pending_speech_reconfiguration = Some(spec);
    }

    pub fn take_speech_reconfiguration(&mut self) -> Option<SpeechServerSpec> {
        self.pending_speech_reconfiguration.take()
    }

    pub fn commit_speech_reconfiguration(&mut self, spec: SpeechServerSpec) {
        self.speech_server_spec = spec;
    }

    /// Select the process backend that the speech worker should start or
    /// transactionally replace.
    pub fn configure_speech_server(&mut self, spec: SpeechServerSpec) -> Result<()> {
        self.speech.configure_server(spec)?;
        Ok(())
    }

    /// Cross the deferred speech-start boundary after Lua configuration.
    pub fn start_speech(&mut self) -> Result<()> {
        self.speech.start()?;
        Ok(())
    }

    /// Begin bounded worker and child-process teardown.
    pub fn shutdown_speech(&mut self) {
        self.speech.shutdown();
    }

    /// Keep the terminal and overlays usable after every configured speech
    /// host has failed. The unavailable driver deliberately reports optional
    /// settings as unsupported, so later Lua assignments still fail clearly.
    pub fn disable_speech(&mut self) {
        self.speech.shutdown();
        self.speech = Speech::silent();
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

    pub fn cancel_speaking(&mut self) -> Result<()> {
        self.speech.cancel()?;
        Ok(())
    }

    pub fn pause_speaking(&mut self) -> Result<()> {
        self.speech.pause()?;
        Ok(())
    }

    pub fn resume_speaking(&mut self) -> Result<()> {
        self.speech.resume()?;
        Ok(())
    }

    pub fn toggle_speaking(&mut self) -> Result<()> {
        self.speech.toggle()?;
        Ok(())
    }

    pub fn last_key(&self) -> &[u8] {
        &self.last_key
    }

    pub(crate) fn record_last_key(&mut self, raw: &[u8]) {
        self.input_sequence = self.input_sequence.wrapping_add(1);
        self.last_key.clear();
        self.last_key.extend_from_slice(raw);
    }

    #[cfg(test)]
    pub(crate) fn record_forwarded_character(&mut self, character: char) {
        if !self.suppress_key_echo() {
            return;
        }
        if self.pending_key_echo.len() == MAX_PENDING_KEY_ECHO_CHARS {
            self.pending_key_echo.pop_front();
        }
        self.pending_key_echo.push_back(PendingKeyEcho {
            character,
            screen: crate::terminal::ScreenIdentity::Primary,
        });
    }

    pub(crate) fn record_forwarded_key(
        &mut self,
        text: Option<&str>,
        screen: crate::terminal::ScreenIdentity,
    ) {
        if text.is_none_or(str::is_empty) {
            self.key_echo_stream_active = false;
        }
        let Some(text) = text.filter(|_| self.suppress_key_echo()) else {
            return;
        };
        for character in text.chars() {
            if self.pending_key_echo.len() == MAX_PENDING_KEY_ECHO_CHARS {
                self.pending_key_echo.pop_front();
            }
            self.pending_key_echo
                .push_back(PendingKeyEcho { character, screen });
        }
    }

    fn has_pending_key_echo(&self) -> bool {
        self.suppress_key_echo() && !self.pending_key_echo.is_empty()
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

    pub(crate) fn has_pending_history_navigation(&self) -> bool {
        self.pending_history_navigation
    }

    pub(crate) fn should_suppress_key_echo(&mut self, text: &str) -> bool {
        // Screen diffs and completed terminal records can append a synthetic
        // row delimiter which did not come from a printable key. Preserve
        // spaces exactly, but exclude those record delimiters from matching.
        let text = text.trim_end_matches(['\r', '\n']);
        if !self.suppress_key_echo() || text.is_empty() {
            return false;
        }

        let character_count = text.chars().count();
        if character_count > self.pending_key_echo.len() {
            // A restored prompt can reach accessibility in the same linear
            // record as input typed immediately afterward. Suppress that
            // record only when *all* currently pending input is its exact
            // suffix; accepting a partial suffix would let ordinary command
            // output which happens to end in one typed character disappear.
            let pending_is_exact_suffix = !self.pending_key_echo.is_empty()
                && text
                    .chars()
                    .rev()
                    .zip(self.pending_key_echo.iter().rev())
                    .all(|(character, pending)| character == pending.character);
            if pending_is_exact_suffix {
                self.pending_key_echo.clear();
                self.key_echo_stream_active = true;
                return true;
            }
            return false;
        }
        for start in 0..=self.pending_key_echo.len() - character_count {
            if text.chars().enumerate().all(|(offset, character)| {
                self.pending_key_echo
                    .get(start + offset)
                    .is_some_and(|pending| pending.character == character)
            }) {
                // Full-screen applications accept printable commands which
                // never echo (for example Neovim's normal-mode `a`) before
                // later echoing insert-mode typeahead. A matching suffix is
                // the first positive acknowledgement: consume the stale
                // command prefix with it, but never discard later typeahead
                // merely because an unrelated status redraw arrived first.
                for _ in 0..start + character_count {
                    self.pending_key_echo.pop_front();
                }
                self.key_echo_stream_active = true;
                return true;
            }
        }
        false
    }

    fn should_suppress_cursor_row_key_echo(&mut self, text: &str) -> bool {
        if self.should_suppress_key_echo(text) {
            return true;
        }
        if !self.key_echo_stream_active {
            return false;
        }

        // A cursor-addressed editor can expose newly inserted text together
        // with indentation cells which were already part of the logical line.
        // Only the terminal-confirmed cursor-row echo path may discard that
        // layout prefix; ordinary output retains the strict whole-text rule.
        let without_record_boundary = text.trim_end_matches(['\r', '\n']);
        let without_layout_prefix = without_record_boundary.trim_start_matches([' ', '\t']);
        without_layout_prefix != without_record_boundary
            && !without_layout_prefix.is_empty()
            && self.should_suppress_key_echo(without_layout_prefix)
    }

    /// Primary and alternate input can overlap while physical presentation
    /// catches up. Preserve only acknowledgements belonging to the screen
    /// context which actually reached accessibility.
    pub(crate) fn retain_pending_key_echo_for_screen(
        &mut self,
        screen: crate::terminal::ScreenIdentity,
    ) {
        self.pending_key_echo
            .retain(|pending| pending.screen == screen);
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

    pub(crate) fn read_clipboard(&mut self, register: ClipboardRegister) -> Result<Option<String>> {
        match register {
            ClipboardRegister::Internal => Ok(self.clipboard.get().map(str::to_owned)),
            ClipboardRegister::System => self
                .system_clipboard
                .read(self.options.system_clipboard_provider())
                .map_err(|error| Error::Clipboard(error.to_string())),
        }
    }

    pub(crate) fn write_clipboard(
        &mut self,
        register: ClipboardRegister,
        text: String,
    ) -> Result<()> {
        match register {
            ClipboardRegister::Internal => self.push_clipboard(text),
            ClipboardRegister::System => self
                .system_clipboard
                .write(self.options.system_clipboard_provider(), text)
                .map_err(|error| Error::Clipboard(error.to_string())),
        }
    }

    pub(crate) fn clear_clipboard(&mut self, register: ClipboardRegister) -> Result<()> {
        match register {
            ClipboardRegister::Internal => {
                self.clipboard.clear();
                self.hook_on_clipboard_change("clear", None)
            }
            ClipboardRegister::System => self
                .system_clipboard
                .clear(self.options.system_clipboard_provider())
                .map_err(|error| Error::Clipboard(error.to_string())),
        }
    }

    pub(crate) fn internal_clipboard_entries(&self) -> Vec<String> {
        self.clipboard.entries()
    }

    pub(crate) fn internal_clipboard_index(&self) -> Option<usize> {
        self.clipboard.selected_index()
    }

    pub(crate) fn select_internal_clipboard(&mut self, index: usize) -> Result<()> {
        if !self.clipboard.select_index(index) {
            return Err(Error::Clipboard(format!(
                "internal clipboard index must be between 1 and {}",
                self.clipboard.size()
            )));
        }
        self.hook_on_clipboard_change("select", self.clipboard.get())
    }

    pub(crate) fn take_terminal_clipboard_writes(&mut self) -> Vec<Vec<u8>> {
        self.system_clipboard.take_terminal_writes()
    }

    pub(crate) fn clipboard_default_register(&self) -> ClipboardRegister {
        self.options.clipboard_default_register()
    }

    pub(crate) fn set_clipboard_default_register(&mut self, value: ClipboardRegister) {
        self.options.set_clipboard_default_register(value);
    }

    pub(crate) fn system_clipboard_provider(&self) -> SystemClipboardProvider {
        self.options.system_clipboard_provider()
    }

    pub(crate) fn set_system_clipboard_provider(&mut self, value: SystemClipboardProvider) {
        self.options.set_system_clipboard_provider(value);
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
            self.pause_speaking()?;
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
        !self.reader_auto_read_suppressed && self.options.auto_read()
    }

    pub fn set_auto_read_enabled(&mut self, value: bool) {
        self.options.set_auto_read(value);
    }

    pub(crate) fn set_reader_auto_read_suppressed(&mut self, suppressed: bool) {
        self.reader_auto_read_suppressed = suppressed;
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
            self.key_echo_stream_active = false;
        }
    }

    pub fn indentation_reporting_enabled(&self) -> bool {
        self.options.report_indentation()
    }

    pub fn set_indentation_reporting_enabled(&mut self, value: bool) {
        self.options.set_report_indentation(value);
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

    pub fn tmux_bell_mode(&self) -> TmuxBellMode {
        self.options.tmux_bell_mode()
    }

    pub fn set_tmux_bell_mode(&mut self, value: TmuxBellMode) {
        self.options.set_tmux_bell_mode(value);
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

    pub fn speak(
        &mut self,
        text: &str,
        interrupt: bool,
    ) -> Result<Option<crate::speech::protocol::UtteranceId>> {
        if text.is_empty() || !self.terminal_focused {
            return Ok(None);
        }
        self.call_hook_on_speech_start(text, interrupt)?;
        let result = self.speech.speak(text, interrupt);
        let ok = result.is_ok();
        self.call_hook_on_speech_end(text, interrupt, ok)?;
        result.map(Some).map_err(Into::into)
    }

    pub(crate) fn speak_for_reader(
        &mut self,
        text: &str,
    ) -> Result<crate::speech::protocol::UtteranceId> {
        self.call_hook_on_speech_start(text, true)?;
        let result = self.speech.speak_for_reader(text);
        let ok = result.is_ok();
        self.call_hook_on_speech_end(text, true, ok)?;
        result.map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClipboardMove, MAX_PENDING_DELETE_INTENTS, MAX_PENDING_DELETE_PRESENTATIONS, ScreenReader,
    };
    use crate::{
        speech,
        view::{AccessibleDocumentComparison, View},
    };
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
        view.process_changes(b"hello");
        view.finalize_changes(0);

        let read = sr.auto_read(&mut view).unwrap();
        assert!(!read);
        assert!(speaks.borrow().is_empty());
    }

    #[test]
    fn auto_read_speaks_new_text() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 10);
        view.process_changes(b"hi");
        let read = sr.auto_read(&mut view).unwrap();
        assert!(read);
        let speaks = speaks.borrow();
        assert_eq!(speaks.len(), 1);
        assert_eq!(speaks[0], "hi");
    }

    #[test]
    fn structural_long_output_reads_rows_that_scrolled_out_of_the_visible_grid() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 24);
        view.finalize_changes(0);

        let mut output = Vec::new();
        for line in 0..20 {
            output.extend_from_slice(format!("item-{line:02}\tvalue-{line:02}\r\n").as_bytes());
        }
        view.process_changes(&output);

        assert!(sr.auto_read(&mut view).unwrap());
        let spoken = speaks.borrow().join(" ");
        for line in 0..20 {
            assert!(
                spoken.contains(&format!("item-{line:02}")),
                "missing line {line} from {spoken:?}"
            );
        }
    }

    #[test]
    fn completed_linear_output_after_history_growth_keeps_document_diff_lazy() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new_with_scrollback_for_test(2, 24, 128);
        view.process_changes(b"first record\r\nsecond record\r\n");
        view.finalize_changes(0);

        view.process_changes(b"new tail record\r\n");

        assert!(view.accessibility_document_changed());
        assert!(view.accessibility_completes_linear_output_record());
        assert!(!view.document_contents_cache_is_prepared_for_test());
        assert!(sr.auto_read(&mut view).unwrap());
        assert_eq!(speaks.borrow().as_slice(), ["new tail record"]);
        assert!(
            !view.document_contents_cache_is_prepared_for_test(),
            "a validated tail record must not serialize retained history"
        );
    }

    #[test]
    fn completed_linear_lines_are_read_after_scrolling_out_of_view() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new_with_scrollback_for_test(3, 24, 128);
        view.finalize_changes(0);

        let mut output = Vec::new();
        for line in 0..12 {
            output.extend_from_slice(format!("record-{line:02}\r\n").as_bytes());
        }
        view.process_changes(&output);

        assert!(
            view.scrollback_len() >= 9,
            "the earliest records must be off-screen"
        );
        assert!(sr.auto_read(&mut view).unwrap());

        let spoken = speaks.borrow().join(" ");
        for line in 0..12 {
            assert!(
                spoken.contains(&format!("record-{line:02}")),
                "missing scrolled record {line} from {spoken:?}"
            );
        }
    }

    #[test]
    fn completed_output_with_ambiguous_prefix_diffs_every_printed_line() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 40);
        view.process_changes(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        view.finalize_changes(0);

        // A horizontal tab is an ambiguity barrier because its destination
        // depends on terminal tab stops. The suffix after the final tab is
        // independently valid, but even a semantic command boundary cannot
        // make it account for earlier output from the command.
        view.process_changes(b"show-records\r\n\x1b]133;C;\x07lorem\tipsum\r\ndolor\r\n\tsit\r\n");

        assert!(view.accessibility_completes_linear_output_record());
        assert!(sr.auto_read(&mut view).unwrap());
        let spoken = speaks.borrow().join(" ");
        assert!(spoken.contains("lorem"), "{spoken:?}");
        assert!(spoken.contains("dolor"), "{spoken:?}");
    }

    #[test]
    fn structural_output_that_only_extends_the_visible_grid_reads_the_new_text() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(6, 40);
        view.process_changes(b"existing");
        view.finalize_changes(0);

        view.process_changes(b"\r\nfirst\tvalue\r\nsecond\tvalue");

        assert!(sr.auto_read(&mut view).unwrap());
        let spoken = speaks.borrow().join(" ");
        assert!(spoken.contains("first"), "{spoken:?}");
        assert!(spoken.contains("second"), "{spoken:?}");
        assert!(!spoken.contains("existing"), "{spoken:?}");
    }

    #[test]
    fn bounded_history_eviction_does_not_repeat_retained_rows() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new_with_scrollback_for_test(2, 24, 2);
        view.process_changes(b"old-0\r\nold-1\r\nold-2");
        view.finalize_changes(0);

        view.process_changes(b"\r\nnew-3\tvalue");

        assert!(sr.auto_read(&mut view).unwrap());
        let spoken = speaks.borrow().join(" ");
        assert!(spoken.contains("new-3"), "{spoken:?}");
        assert!(!spoken.contains("old-1"), "{spoken:?}");
        assert!(!spoken.contains("old-2"), "{spoken:?}");
    }

    #[test]
    fn bounded_history_eviction_reads_a_new_tail_identical_to_the_evicted_head() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new_with_scrollback_for_test(2, 24, 2);
        view.process_changes(b"same\tvalue\r\nsame\tvalue\r\nsame\tvalue");
        view.finalize_changes(0);

        view.process_changes(b"\r\nsame\tvalue");

        assert!(sr.auto_read(&mut view).unwrap());
        assert!(
            speaks.borrow().iter().any(|spoken| spoken.contains("same")),
            "the textually identical new row still has a distinct document identity"
        );
    }

    #[test]
    fn returning_document_reads_only_output_since_its_departure_checkpoint() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(3, 30);
        view.process_changes(b"already heard");
        view.finalize_changes(0);
        view.mark_accessible_document_context_active();
        assert!(view.deactivate_accessible_document_context());

        // Hidden contexts may still be independently finalized. Reactivation
        // must restore the departure checkpoint rather than that newer global
        // speech baseline.
        view.process_changes(b"\r\narrived while hidden");
        view.finalize_changes(1);
        assert!(view.activate_accessible_document_context());

        assert!(sr.auto_read(&mut view).unwrap());
        let spoken = speaks.borrow().join(" ");
        assert!(spoken.contains("arrived while hidden"), "{spoken:?}");
        assert!(!spoken.contains("already heard"), "{spoken:?}");
    }

    #[test]
    fn resize_invalidates_a_suspended_document_checkpoint() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(3, 30);
        view.process_changes(b"visible before resize");
        view.finalize_changes(0);
        view.mark_accessible_document_context_active();
        assert!(view.deactivate_accessible_document_context());

        view.set_size(4, 20);
        assert!(view.activate_accessible_document_context());
        assert!(sr.auto_read(&mut view).unwrap());
        assert!(
            speaks
                .borrow()
                .iter()
                .any(|spoken| spoken.contains("visible before resize"))
        );
    }

    #[test]
    fn disjoint_history_lineage_diffs_the_still_identified_visible_grid() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new_with_scrollback_for_test(3, 24, 1);
        view.process_changes(b"header\r\nold-middle\r\nfooter");
        view.finalize_changes(0);

        // Scroll beyond the retained-history window, then reconstruct two
        // unchanged visible rows around one changed row. The complete document
        // no longer has a common retained lineage, but visible row coordinates
        // remain exact because geometry and screen identity did not change.
        view.process_changes(
            b"\r\njunk-1\r\njunk-2\r\njunk-3\x1b[Hheader\x1b[2;1Hnew-middle\x1b[3;1Hfooter",
        );

        assert_eq!(
            view.prepare_accessible_document_comparison(),
            AccessibleDocumentComparison::VisibleGrid
        );
        assert!(sr.auto_read(&mut view).unwrap());
        let spoken = speaks.borrow().join(" ");
        assert!(spoken.contains("new"), "{spoken:?}");
        assert!(!spoken.contains("header"), "{spoken:?}");
        assert!(!spoken.contains("footer"), "{spoken:?}");
    }

    #[test]
    fn primary_screen_scroll_region_reads_only_the_inserted_tui_row() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(5, 24);
        view.process_changes(b"header\x1b[2;1Hrow-a\x1b[3;1Hrow-b\x1b[4;1Hrow-c\x1b[5;1Hfooter");
        view.finalize_changes(0);

        view.process_changes(b"\x1b[2;4r\x1b[4;1H\nrow-d\x1b[r");

        assert!(sr.auto_read(&mut view).unwrap());
        let spoken = speaks.borrow().join(" ");
        assert!(spoken.contains("row-d"), "{spoken:?}");
        assert!(!spoken.contains("header"), "{spoken:?}");
        assert!(!spoken.contains("footer"), "{spoken:?}");
    }

    #[test]
    fn alternate_screen_transition_and_repaint_use_visible_context_then_diff() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 24);
        view.process_changes(b"primary-one\r\nprimary-two");
        view.finalize_changes(0);

        view.process_changes(b"\x1b[?1049h\x1b[2J\x1b[Hmenu-one\x1b[2;1Hmenu-two\x1b[4;1Hstatus-a");
        assert!(sr.auto_read(&mut view).unwrap());
        view.finalize_changes(1);

        view.process_changes(b"\x1b[2;1Hmenu-next\x1b[4;1Hstatus-b");
        assert!(sr.auto_read(&mut view).unwrap());

        let spoken = speaks.borrow();
        assert_eq!(spoken.len(), 2, "{spoken:?}");
        assert!(spoken[0].contains("menu-one"), "{spoken:?}");
        assert!(spoken[0].contains("menu-two"), "{spoken:?}");
        assert!(spoken[0].contains("status-a"), "{spoken:?}");
        assert!(spoken[1].contains("menu-next"), "{spoken:?}");
        assert!(spoken[1].contains("status-b"), "{spoken:?}");
        assert!(!spoken[1].contains("menu-one"), "{spoken:?}");
    }

    #[test]
    fn returning_from_alternate_screen_uses_the_primary_document_checkpoint() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 24);
        view.process_changes(b"primary-one\r\nprimary-two\r\nprimary-three");
        view.finalize_changes(0);
        view.process_changes(b"\x1b[?1049h\x1b[2J\x1b[Halternate");
        assert!(sr.auto_read(&mut view).unwrap());
        view.finalize_changes(1);
        speaks.borrow_mut().clear();

        view.process_changes(b"\x1b[?1049l");

        assert!(!sr.auto_read(&mut view).unwrap());
        assert!(speaks.borrow().is_empty());
    }

    #[test]
    fn resize_reintroduces_the_entire_visible_grid() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 24);
        view.process_changes(b"resize-one\r\nresize-two\r\nresize-three");
        view.finalize_changes(0);

        view.set_size(5, 18);

        assert!(sr.auto_read(&mut view).unwrap());
        let spoken = speaks.borrow().join(" ");
        assert!(spoken.contains("resize-one"), "{spoken:?}");
        assert!(spoken.contains("resize-two"), "{spoken:?}");
        assert!(spoken.contains("resize-three"), "{spoken:?}");
    }

    #[test]
    fn terminal_reset_reintroduces_the_entire_visible_grid() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 24);
        view.process_changes(b"reset-one\r\nreset-two\r\nreset-three");
        view.finalize_changes(0);

        // DECSTR preserves the grid but invalidates terminal-state continuity;
        // changing one row must therefore reintroduce all visible rows.
        view.process_changes(b"\x1b[!p\x1b[2;1Hchanged-two");

        assert!(sr.auto_read(&mut view).unwrap());
        let spoken = speaks.borrow().join(" ");
        assert!(spoken.contains("reset-one"), "{spoken:?}");
        assert!(spoken.contains("changed-two"), "{spoken:?}");
        assert!(spoken.contains("reset-three"), "{spoken:?}");
    }

    #[test]
    fn auto_read_speaks_key_echo_by_default() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 10);
        sr.record_last_key(b"g");
        view.process_changes(b"abcdefg");
        let read = sr.auto_read(&mut view).unwrap();
        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["abcdefg"]);
    }

    #[test]
    fn auto_read_suppresses_live_echo_when_enabled() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 10);
        sr.set_suppress_key_echo(true);
        for character in "abcdefg".chars() {
            sr.record_forwarded_character(character);
        }
        sr.record_last_key(b"g");
        view.process_changes(b"abcdefg");
        let read = sr.auto_read(&mut view).unwrap();
        assert!(read);
        assert!(speaks.borrow().is_empty());
    }

    #[test]
    fn auto_read_suppresses_slow_character_at_a_time_echo_when_enabled() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 10);
        sr.set_suppress_key_echo(true);
        for character in "abcdefg".chars() {
            sr.record_forwarded_character(character);
        }
        sr.record_last_key(b"g");

        for byte in b"abcdefg" {
            view.process_changes(&[*byte]);
            let read = sr.auto_read(&mut view).unwrap();
            assert!(read);
            assert!(speaks.borrow().is_empty());
            view.finalize_changes(0);
        }
    }

    #[test]
    fn key_echo_suppression_skips_an_unechoed_full_screen_command_prefix() {
        let (mut sr, _speaks) = make_sr();
        sr.set_suppress_key_echo(true);
        for character in "ahello".chars() {
            sr.record_forwarded_character(character);
        }

        assert!(!sr.should_suppress_key_echo("INSERT"));
        assert!(sr.should_suppress_key_echo("h"));
        assert!(sr.should_suppress_key_echo("ello\n"));
        assert!(sr.pending_key_echo.is_empty());
    }

    #[test]
    fn key_echo_suppression_accepts_complete_input_after_a_restored_prompt() {
        let (mut sr, _speaks) = make_sr();
        sr.set_suppress_key_echo(true);
        for character in "edit notes.txt".chars() {
            sr.record_forwarded_character(character);
        }

        assert!(sr.should_suppress_key_echo("demo:~$ edit notes.txt\n"));
        assert!(sr.pending_key_echo.is_empty());
    }

    #[test]
    fn auto_read_suppresses_diff_echo_when_enabled() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 10);
        view.process_changes(b"a");
        view.finalize_changes(0);
        sr.set_suppress_key_echo(true);
        sr.record_forwarded_character('b');
        sr.record_last_key(b"b");
        view.process_changes(b"\x1B[1Gb");

        let read = sr.auto_read(&mut view).unwrap();
        assert!(read);
        assert!(speaks.borrow().is_empty());
    }

    #[test]
    fn auto_read_suppresses_full_screen_editor_echo_without_renderer_hints() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(5, 24);
        view.process_changes(b"\x1b[2J\x1b[H\x1b[5;1H[No Name]\x1b[1;1H");
        view.finalize_changes(0);

        sr.set_suppress_key_echo(true);
        sr.record_forwarded_key(Some("ahello"), crate::terminal::ScreenIdentity::Primary);
        sr.record_last_key(b"o");
        view.process_changes(
            b"\x1b[?2026h\x1b[Hhello\x1b[2;1H~\x1b[3;1H~\x1b[4;1H-- INSERT --\x1b[5;1H[No Name] [+]\x1b[1;6H\x1b[?2026l",
        );
        // Renderer damage is optional under physical-output backpressure; the
        // exact committed snapshots and fixed-size output provenance remain.
        view.clear_renderer_damage_hints();

        let read = sr.auto_read_after_input(&mut view).unwrap();

        assert!(read);
        assert!(speaks.borrow().is_empty());
        assert!(sr.pending_key_echo.is_empty());
    }

    #[test]
    fn cursor_shape_mode_transition_reads_status_without_consuming_later_echo() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(5, 24);
        view.process_changes(b"\x1b[2J\x1b[H\x1b[4;3H[No Name]\x1b[1;1H");
        view.finalize_changes(0);

        sr.set_suppress_key_echo(true);
        sr.record_forwarded_key(Some("a"), crate::terminal::ScreenIdentity::Primary);
        sr.record_last_key(b"a");
        view.process_changes(
            b"\x1b[?2026h\x1b[6 q\x1b[4;1Hi [No Name]\x1b[5;1H-- INSERT --\x1b[1;1H\x1b[?2026l",
        );

        // The mode transition is recognized from terminal state even when
        // the scheduler no longer considers its originating input recent.
        let read = sr.auto_read(&mut view).unwrap();

        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["i"]);
        assert_eq!(sr.pending_key_echo.len(), 1);
        assert!(sr.key_echo_stream_active);

        view.finalize_changes(0);
        sr.record_forwarded_key(Some("T"), crate::terminal::ScreenIdentity::Primary);
        sr.record_last_key(b"T");
        view.process_changes(
            b"\x1b[?2026h\x1b[1;1H  T\x1b[4;1Hi [+] [No Name]\x1b[1;4H\x1b[?2026l",
        );

        let read = sr.auto_read_after_input(&mut view).unwrap();

        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["i"]);
        assert!(sr.pending_key_echo.is_empty());
    }

    #[test]
    fn stable_multirow_interface_replacement_uses_the_ordinary_diff() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(5, 24);
        view.process_changes(
            b"\x1b[2J\x1b[Hold history item\x1b[2;1H----------------\x1b[3;1H>\x1b[4;1H1/100\x1b[5;1H$ stale prompt\x1b[3;1H",
        );
        view.finalize_changes(0);

        view.process_changes(b"\x1b[Hnew history item\x1b[4;1H2/100\x1b[3;1H");
        view.clear_renderer_damage_hints();

        let read = sr.auto_read_after_input(&mut view).unwrap();

        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["new history item 2 slash 100"]);
    }

    #[test]
    fn recent_input_reads_a_status_diff_when_the_cursor_is_stationary() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(5, 24);
        view.process_changes(b"cursor text\x1b[1;1H");
        view.finalize_changes(0);

        sr.record_forwarded_key(None, crate::terminal::ScreenIdentity::Primary);
        view.process_changes(b"\x1b[5;1Hfile status\x1b[1;1H");
        view.clear_renderer_damage_hints();

        let read = sr.auto_read_after_input(&mut view).unwrap();

        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["file status"]);
    }

    #[test]
    fn counted_cursor_move_reads_the_destination_instead_of_the_ruler() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(5, 24);
        view.process_changes(b"first\x1b[2;1Hsecond\x1b[3;1Hthird\x1b[5;1H1,1\x1b[1;1H");
        view.finalize_changes(0);

        sr.set_suppress_key_echo(true);
        sr.record_forwarded_key(Some("3G"), crate::terminal::ScreenIdentity::Primary);
        view.process_changes(b"\x1b[5;1H3,1\x1b[3;1H");
        view.clear_renderer_damage_hints();

        let read = sr.auto_read_after_input(&mut view).unwrap();

        assert!(!read);
        assert!(speaks.borrow().is_empty());
        sr.track_cursor(&mut view).unwrap();
        assert_eq!(speaks.borrow().as_slice(), ["third"]);
    }

    #[test]
    fn active_echo_stream_does_not_hide_a_later_cursor_only_move() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(3, 24);
        view.process_changes(b"first\x1b[2;1Hsecond\x1b[1;1H");
        view.finalize_changes(0);

        sr.set_suppress_key_echo(true);
        sr.record_forwarded_key(Some("x"), crate::terminal::ScreenIdentity::Primary);
        view.process_changes(b"x");
        assert!(sr.auto_read_after_input(&mut view).unwrap());
        assert!(sr.key_echo_stream_active);
        assert!(speaks.borrow().is_empty());
        view.finalize_changes(0);

        sr.record_forwarded_key(Some("j"), crate::terminal::ScreenIdentity::Primary);
        view.process_changes(b"\x1b[2;1H");

        let read = sr.auto_read_after_input(&mut view).unwrap();

        assert!(!read);
        sr.track_cursor(&mut view).unwrap();
        assert_eq!(speaks.borrow().as_slice(), ["second"]);
    }

    #[test]
    fn pending_echo_does_not_silence_stable_cursor_addressed_output() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(5, 24);
        view.process_changes(b"old one\x1b[2;1Hold two\x1b[4;1Hworking\x1b[5;1H>\x1b[5;2H");
        view.finalize_changes(0);

        sr.set_suppress_key_echo(true);
        sr.record_forwarded_key(Some("x"), crate::terminal::ScreenIdentity::Primary);
        view.process_changes(
            b"\x1b[1;1Hdone one\x1b[2;1Hdone two\x1b[4;1Hfinished\x1b[5;1H$ \x1b[5;3H",
        );
        view.clear_renderer_damage_hints();

        let read = sr.auto_read_after_input(&mut view).unwrap();

        assert!(read);
        let spoken = speaks.borrow().join(" ");
        assert!(spoken.contains("done one"));
        assert!(spoken.contains("finished"));
        assert!(spoken.contains('$'));
    }

    #[test]
    fn failed_cursor_row_echo_candidate_restores_the_full_diff() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(5, 24);
        view.process_changes(b"abc\x1b[2;1Hold result\x1b[3;1Hold detail\x1b[5;1H1,4\x1b[1;4H");
        view.finalize_changes(0);

        sr.set_suppress_key_echo(true);
        sr.key_echo_stream_active = true;
        sr.record_forwarded_key(Some("x"), crate::terminal::ScreenIdentity::Primary);
        view.process_changes(
            b"\x1b[1;1Habcq\x1b[2;1Himportant result\x1b[3;1Hnew detail\x1b[5;1H1,5\x1b[1;5H",
        );
        view.clear_renderer_damage_hints();

        let read = sr.auto_read_after_input(&mut view).unwrap();

        assert!(read);
        let spoken = speaks.borrow().join(" ");
        assert!(spoken.contains("important result"));
        assert!(spoken.contains("new detail"));
    }

    #[test]
    fn stable_spinner_repaint_reads_changes_instead_of_the_prompt_cursor() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(5, 28);
        view.process_changes(
            b"answer so far\x1b[2;1Hthinking 1\x1b[3;1Htokens 10\x1b[4;1H>\x1b[4;2H",
        );
        view.finalize_changes(0);

        view.process_changes(b"\x1b[2;1Hthinking 2\x1b[3;1Htokens 20\x1b[4;2H");
        view.clear_renderer_damage_hints();

        let read = sr.auto_read_after_input(&mut view).unwrap();

        assert!(read);
        let spoken = speaks.borrow().join(" ");
        assert!(spoken.contains("thinking 2"));
        assert!(spoken.contains("tokens 20"));
        assert!(!spoken.contains("greater"));
    }

    #[test]
    fn screen_transition_keeps_echo_from_the_presented_context() {
        let (mut sr, _speaks) = make_sr();
        sr.set_suppress_key_echo(true);

        sr.record_last_key(b"o");
        sr.record_forwarded_key(Some("old"), crate::terminal::ScreenIdentity::Alternate);
        sr.record_last_key(b"n");
        sr.record_forwarded_key(Some("new"), crate::terminal::ScreenIdentity::Primary);

        sr.retain_pending_key_echo_for_screen(crate::terminal::ScreenIdentity::Primary);

        assert_eq!(sr.pending_key_echo.len(), 3);
        assert!(sr.should_suppress_key_echo("new\n"));
        assert!(sr.pending_key_echo.is_empty());
    }

    #[test]
    fn enter_does_not_silence_real_broad_output_without_renderer_hints() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(5, 24);
        view.finalize_changes(0);

        sr.record_forwarded_key(None, crate::terminal::ScreenIdentity::Primary);
        view.process_changes(
            b"\x1b[2J\x1b[Hfirst result\r\nsecond result\r\nthird result\r\nfourth result",
        );
        view.clear_renderer_damage_hints();

        let read = sr.auto_read_after_input(&mut view).unwrap();

        assert!(read);
        assert!(speaks.borrow().join(" ").contains("first result"));
    }

    #[test]
    fn printable_command_does_not_silence_real_broad_output_without_renderer_hints() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(5, 24);
        view.finalize_changes(0);

        sr.set_suppress_key_echo(true);
        sr.record_forwarded_key(Some("s"), crate::terminal::ScreenIdentity::Primary);
        view.process_changes(
            b"\x1b[2J\x1b[Hfirst result\r\nsecond result\r\nthird result\r\nfourth result",
        );
        view.clear_renderer_damage_hints();

        let read = sr.auto_read_after_input(&mut view).unwrap();

        assert!(read);
        assert!(speaks.borrow().join(" ").contains("first result"));
    }

    #[test]
    fn auto_read_speaks_multiple_inserted_runs_from_single_changed_line() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 40);
        view.process_changes(b"left one right two");
        view.finalize_changes(0);

        view.process_changes(b"\x1B[1G\x1B[Kleft alpha right beta");
        let read = sr.auto_read(&mut view).unwrap();

        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["alpha beta"]);
    }

    #[test]
    fn auto_read_speaks_short_status_line_replacements() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 120);
        view.process_changes(
            b"[dev] 1:bash* 2:bash-                                             bash.1",
        );
        view.finalize_changes(0);

        view.process_changes(
            b"\x1B[1G\x1B[K[dev] 1:caffeinate* 2:bash-                                      caffeinate.1",
        );
        let read = sr.auto_read(&mut view).unwrap();

        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["caffeinate"]);
    }

    #[test]
    fn auto_read_speaks_shorter_replacements_to_word_boundaries() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 120);
        view.process_changes(
            b"[dev] 1:bash* 2:bash-                                             bash.1",
        );
        view.finalize_changes(0);

        view.process_changes(
            b"\x1B[1G\x1B[K[dev] 1:gh* 2:bash-                                                gh.1",
        );
        let read = sr.auto_read(&mut view).unwrap();

        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["gh"]);
    }

    #[test]
    fn auto_read_collapses_contiguous_duplicate_replacement_hunks() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 40);
        view.process_changes(b"foo bar foo");
        view.finalize_changes(0);

        view.process_changes(b"\x1B[1G\x1B[Kbum bar bum");
        let read = sr.auto_read(&mut view).unwrap();

        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["bum"]);
    }

    #[test]
    fn auto_read_preserves_non_contiguous_duplicate_replacement_hunks() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 40);
        view.process_changes(b"foo bar baz foo");
        view.finalize_changes(0);

        view.process_changes(b"\x1B[1G\x1B[Kbum bar bat bum");
        let read = sr.auto_read(&mut view).unwrap();

        assert!(read);
        assert_eq!(speaks.borrow().as_slice(), ["bum bat bum"]);
    }

    #[test]
    fn auto_read_keeps_repeated_words_inside_one_replacement_hunk() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 40);
        view.process_changes(b"foo foo");
        view.finalize_changes(0);

        view.process_changes(b"\x1B[1G\x1B[Kbum bum");
        let read = sr.auto_read(&mut view).unwrap();

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
    fn backspace_at_empty_semantic_input_does_not_delete_the_prompt_space() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(1, 10);

        // Some Bash/Ghostty prompt cycles emit A but omit B, and the visible
        // prompt can arrive in a later PTY read.
        view.process_changes(b"\x1b]133;A\x07");
        view.finalize_changes(0);
        view.process_changes(b"$ ");
        view.finalize_changes(0);
        sr.defer_backspace(&view);

        // Enter at the bottom of the screen scrolls away the prompt before
        // the replacement prompt is drawn. That changed row and leftward
        // cursor used to confirm a deletion of the prompt's trailing space.
        view.process_changes(b"\r\n");
        assert!(!sr.resolve_pending_delete(&view).unwrap());
        assert!(speaks.borrow().is_empty());
        assert!(sr.pending_deletes.is_empty());
    }

    #[test]
    fn incompatible_redraw_discards_an_unobserved_markerless_backspace() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(1, 10);

        // With no semantic markers, the prompt's trailing space is initially
        // indistinguishable from editable input. An unrelated redraw changes
        // the protected prefix, so it cannot be evidence for that deletion.
        view.process_changes(b"$ ");
        view.finalize_changes(0);
        sr.record_last_key(b"\x7f");
        sr.defer_backspace(&view);
        assert_eq!(sr.pending_deletes.len(), 1);

        view.process_changes(b"\r#");
        assert!(!sr.resolve_pending_delete(&view).unwrap());
        assert!(sr.pending_deletes.is_empty());
        assert!(speaks.borrow().is_empty());

        // Once contradicted, the stale intent cannot be resurrected by a
        // later frame which happens to resemble a deletion.
        view.process_changes(b"\r$ ");
        view.process_changes(b"\x08\x1b[P");
        assert!(!sr.resolve_pending_delete(&view).unwrap());
        assert!(speaks.borrow().is_empty());
    }

    #[test]
    fn backspace_crossing_a_soft_wrap_announces_the_preceding_row_character() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(3, 5);

        // "abcde" soft-wraps, with the cursor moved before "f" at the
        // beginning of the continuation row.
        view.process_changes(b"abcdef\x1b[D");
        view.finalize_changes(0);
        assert_eq!(view.screen().cursor_position(), (1, 0));
        assert!(view.screen().row_wrapped(0));

        sr.record_last_key(b"\x7f");
        sr.defer_backspace(&view);
        // Simulate removing "e", shifting "f" left, and placing the cursor
        // where the removed character began.
        view.process_changes(b"\x1b[1;5Hf\x1b[2;1H \x1b[1;5H");

        assert!(sr.resolve_pending_delete(&view).unwrap());
        assert_eq!(speaks.borrow().as_slice(), ["e"]);
    }

    #[test]
    fn backspace_crossing_an_explicitly_drawn_wrap_announces_the_previous_row_character() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(3, 8);

        // Simulate a full-screen input widget. Its first row reaches the
        // margin, but it explicitly positions the cursor after a two-cell
        // indentation on an unwrapped terminal row.
        view.process_changes(b"> aaaaaa\x1b[2;1H  ");
        view.finalize_changes(0);
        assert_eq!(view.screen().cursor_position(), (1, 2));
        assert!(!view.screen().row_wrapped(0));

        sr.record_last_key(b"\x7f");
        sr.defer_backspace(&view);
        view.process_changes(b"\x1b[1;8H \x1b[1;8H");

        assert!(sr.resolve_pending_delete(&view).unwrap());
        assert_eq!(speaks.borrow().as_slice(), ["a"]);
    }

    #[test]
    fn backspace_in_a_horizontally_scrolling_editor_uses_the_changed_margin_cell() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(1, 8);

        // The editor pins its cursor to the right margin. Deleting "m" keeps
        // the cursor stationary and reveals another "a" in that cell. The
        // character to the cursor's left is also "a" and must not be spoken.
        view.process_changes(b"> aaaaam");
        view.finalize_changes(0);
        assert_eq!(view.screen().cursor_position(), (0, 7));

        sr.record_last_key(b"\x7f");
        sr.defer_backspace(&view);
        view.process_changes(b"\r> aaaaaa");

        assert!(sr.resolve_pending_delete(&view).unwrap());
        assert_eq!(speaks.borrow().as_slice(), ["m"]);
    }

    #[test]
    fn queued_backspaces_stop_at_the_semantic_input_boundary() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(1, 10);

        view.process_changes(b"\x1b]133;A\x07$ ");
        view.finalize_changes(0);
        view.note_forwarded_application_input();
        view.process_changes(b"x");
        view.finalize_changes(1);
        sr.record_last_key(b"\x7f");
        sr.defer_backspace(&view);
        sr.record_last_key(b"\x7f");
        sr.defer_backspace(&view);

        view.process_changes(b"\x08\x1b[P");
        assert!(sr.resolve_pending_delete(&view).unwrap());
        assert_eq!(speaks.borrow().as_slice(), ["x"]);
        assert!(sr.pending_deletes.is_empty());
    }

    #[test]
    fn deferred_backspace_stays_pending_after_cursor_only_partial_echo() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(4, 10);

        view.process_changes(b"$ x");
        view.finalize_changes(0);
        sr.defer_backspace(&view);

        view.process_changes(b"\x08");
        assert!(!sr.has_confirmed_pending_delete(&view));
        assert!(!sr.resolve_pending_delete(&view).unwrap());
        assert!(speaks.borrow().is_empty());

        view.process_changes(b"\x1B[P");
        assert!(sr.has_confirmed_pending_delete(&view));
        assert!(sr.resolve_pending_delete(&view).unwrap());
        assert_eq!(speaks.borrow().as_slice(), ["x"]);
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

    #[test]
    fn queued_pre_input_presentation_does_not_consume_backspace() {
        use crate::presentation::SurfaceId;

        let (mut sr, speaks) = make_sr();
        let mut view = View::new(2, 12);
        view.process_changes(b"abc");
        view.finalize_changes(0);
        view.enable_presentation_tracking();

        // This status repaint was parsed before the key press, but has not
        // reached the physical terminal yet.
        view.process_changes(b"\x1b7\x1b[2;1Hstatus\x1b8");
        let pre_input_frame = view.capture_live_presentation_frame(SurfaceId(1));
        sr.record_last_key(b"\x7f");
        sr.defer_backspace(&view);

        assert!(view.apply_presented_frame(pre_input_frame));
        assert!(!sr.resolve_pending_delete(&view).unwrap());
        assert!(speaks.borrow().is_empty());

        view.process_changes(b"\x08\x1b[P");
        let deletion_frame = view.capture_live_presentation_frame(SurfaceId(1));
        assert!(view.apply_presented_frame(deletion_frame));
        assert!(sr.resolve_pending_delete(&view).unwrap());
        assert_eq!(speaks.borrow().as_slice(), ["c"]);
    }

    #[test]
    fn ordinary_key_does_not_discard_pending_backspace() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(2, 12);
        view.process_changes(b"abc");
        view.finalize_changes(0);

        sr.record_last_key(b"\x7f");
        sr.defer_backspace(&view);
        sr.record_last_key(b"x");

        view.process_changes(b"\x08\x1b[P");
        assert!(sr.resolve_pending_delete(&view).unwrap());
        assert_eq!(speaks.borrow().as_slice(), ["c"]);
    }

    #[test]
    fn rapid_backspaces_speak_each_virtual_character_once_in_input_order() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(2, 12);
        view.process_changes(b"abc");
        view.finalize_changes(0);

        sr.record_last_key(b"\x7f");
        sr.defer_backspace(&view);
        sr.record_last_key(b"\x7f");
        sr.defer_backspace(&view);

        view.process_changes(b"\x08\x1b[P\x08\x1b[P");
        assert!(sr.resolve_pending_delete(&view).unwrap());
        assert!(!sr.resolve_pending_delete(&view).unwrap());
        assert_eq!(speaks.borrow().as_slice(), ["c", "b"]);
    }

    #[test]
    fn rapid_deletes_speak_each_virtual_character_once_in_input_order() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(2, 12);
        view.process_changes(b"abc\x1b[3D");
        view.finalize_changes(0);

        sr.record_last_key(b"\x1b[3~");
        sr.defer_delete(&view);
        sr.record_last_key(b"\x1b[3~");
        sr.defer_delete(&view);

        view.process_changes(b"\x1b[P\x1b[P");
        assert!(sr.resolve_pending_delete(&view).unwrap());
        assert!(!sr.resolve_pending_delete(&view).unwrap());
        assert_eq!(speaks.borrow().as_slice(), ["a", "b"]);
    }

    #[test]
    fn rapid_backspaces_remain_ordered_across_separate_presentations() {
        use crate::presentation::SurfaceId;

        let (mut sr, speaks) = make_sr();
        let mut view = View::new(2, 12);
        view.process_changes(b"abc");
        view.finalize_changes(0);
        view.enable_presentation_tracking();

        sr.record_last_key(b"\x7f");
        sr.defer_backspace(&view);
        view.process_changes(b"\x08\x1b[P");
        let first = view.capture_live_presentation_frame(SurfaceId(1));

        // The parser has seen the first echo, but accessibility still exposes
        // the original physical line when the second key arrives.
        sr.record_last_key(b"\x7f");
        sr.defer_backspace(&view);
        assert!(view.apply_presented_frame(first));
        assert!(sr.resolve_pending_delete(&view).unwrap());
        assert_eq!(speaks.borrow().as_slice(), ["c"]);

        view.process_changes(b"\x08\x1b[P");
        let second = view.capture_live_presentation_frame(SurfaceId(1));
        assert!(view.apply_presented_frame(second));
        assert!(sr.resolve_pending_delete(&view).unwrap());
        assert!(!sr.resolve_pending_delete(&view).unwrap());
        assert_eq!(speaks.borrow().as_slice(), ["c", "b"]);
    }

    #[test]
    fn queued_backspaces_can_settle_in_multiple_multi_delete_frames() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(2, 12);
        view.process_changes(b"abcde");
        view.finalize_changes(0);

        for _ in 0..5 {
            sr.record_last_key(b"\x7f");
            sr.defer_backspace(&view);
        }

        // The application publishes only the first three deletions. The
        // remaining two intents have not started; they are neither confirmed
        // nor contradicted by this frame.
        view.process_changes(b"\x08\x1b[P\x08\x1b[P\x08\x1b[P");
        assert!(sr.resolve_pending_delete(&view).unwrap());
        assert_eq!(speaks.borrow().as_slice(), ["e", "d", "c"]);
        assert_eq!(sr.pending_deletes.len(), 2);

        view.process_changes(b"\x08\x1b[P\x08\x1b[P");
        assert!(sr.resolve_pending_delete(&view).unwrap());
        assert_eq!(speaks.borrow().as_slice(), ["e", "d", "c", "b", "a"]);
        assert!(sr.pending_deletes.is_empty());
    }

    #[test]
    fn queued_backspaces_can_settle_across_an_explicitly_drawn_row_boundary() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(3, 8);

        // Simulate a TUI input widget with a hard terminal row and a two-cell
        // continuation indent. The logical input is "abcdefgh".
        view.process_changes(b"> abcde\x1b[2;1H  fgh");
        view.finalize_changes(0);
        assert_eq!(view.screen().cursor_position(), (1, 5));
        assert!(!view.screen().row_wrapped(0));

        for _ in 0..5 {
            sr.record_last_key(b"\x7f");
            sr.defer_backspace(&view);
        }

        // The first presentation removes h, g, and f, stopping at the
        // continuation origin without changing the preceding hard row.
        view.process_changes(b"\x1b[2;3H   \x1b[2;3H");
        assert!(sr.resolve_pending_delete(&view).unwrap());
        assert_eq!(speaks.borrow().as_slice(), ["h", "g", "f"]);
        assert_eq!(sr.pending_deletes.len(), 2);

        // The next presentation traverses the drawn row boundary and removes
        // e and d from the preceding row.
        view.process_changes(b"\x1b[1;6H  \x1b[1;6H");
        assert!(sr.resolve_pending_delete(&view).unwrap());
        assert_eq!(speaks.borrow().as_slice(), ["h", "g", "f", "e", "d"]);
        assert!(sr.pending_deletes.is_empty());
    }

    #[test]
    fn ignored_backspace_does_not_block_a_later_confirmed_one() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(2, 12);
        view.process_changes(b"ab");
        view.finalize_changes(0);

        sr.record_last_key(b"\x7f");
        sr.defer_backspace(&view);
        // The application ignores the first deletion and later redraws a
        // longer input line before accepting another backspace.
        view.process_changes(b"c");
        view.finalize_changes(1);
        sr.record_last_key(b"x");
        sr.record_last_key(b"\x7f");
        sr.defer_backspace(&view);

        view.process_changes(b"\x08\x1b[P");
        assert!(sr.resolve_pending_delete(&view).unwrap());
        assert_eq!(speaks.borrow().as_slice(), ["c"]);
    }

    #[test]
    fn pending_backspace_is_scoped_to_its_originating_view() {
        let (mut sr, speaks) = make_sr();
        let mut origin = View::new(2, 12);
        origin.process_changes(b"abc");
        origin.finalize_changes(0);
        let mut other = View::new(2, 12);
        other.process_changes(b"xyz");
        other.finalize_changes(0);

        sr.record_last_key(b"\x7f");
        sr.defer_backspace(&origin);
        other.process_changes(b"\x08\x1b[P");

        assert!(!sr.resolve_pending_delete(&other).unwrap());
        assert!(speaks.borrow().is_empty());

        origin.process_changes(b"\x08\x1b[P");
        assert!(sr.resolve_pending_delete(&origin).unwrap());
        assert_eq!(speaks.borrow().as_slice(), ["c"]);
    }

    #[test]
    fn pending_deletion_intents_have_a_hard_resource_bound() {
        let (mut sr, _) = make_sr();
        let mut view = View::new(1, 80);
        view.process_changes(&[b'x'; MAX_PENDING_DELETE_INTENTS + 1]);

        for _ in 0..=MAX_PENDING_DELETE_INTENTS {
            sr.record_last_key(b"\x7f");
            sr.defer_backspace(&view);
        }

        assert_eq!(sr.pending_deletes.len(), MAX_PENDING_DELETE_INTENTS);
    }

    #[test]
    fn unconfirmed_deletion_intent_has_a_bounded_lifetime() {
        let (mut sr, speaks) = make_sr();
        let mut view = View::new(1, 8);
        view.process_changes(b"abc");
        view.finalize_changes(0);
        sr.record_last_key(b"\x7f");
        sr.defer_backspace(&view);

        for _ in 0..MAX_PENDING_DELETE_PRESENTATIONS {
            assert!(!sr.resolve_pending_delete(&view).unwrap());
        }

        assert!(sr.pending_deletes.is_empty());
        assert!(speaks.borrow().is_empty());
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
