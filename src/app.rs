use crate::{
    commands, keymap::Binding, perform, screen_reader::ScreenReader, terminal_input::KeyInput,
    views,
};
use anyhow::{Context, Result};
use std::{
    collections::{HashSet, VecDeque},
    io::Write,
    sync::LazyLock,
    time,
};
use terminput::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

mod input;
mod protocol;
mod pty;
mod view_stack;

use protocol::{
    FOCUS_IN_EVENT, FOCUS_OUT_EVENT, ModifyOtherKeysStatus, SequenceStatus, focus_event_status,
    is_invalid_ss3_prefix, modify_other_keys_status, osc_status, timed_out_event,
};

pub const DIFF_DELAY: u16 = 30;
pub const MAX_DIFF_DELAY: u16 = 300;
const ESC_TIMEOUT_MS: u128 = 50;
static ANSI_CSI_RE: LazyLock<regex::bytes::Regex> = LazyLock::new(|| {
    regex::bytes::Regex::new(r"^\x1B\[[\x30-\x3F]*[\x20-\x2F]*[\x40-\x7E--[A-D~]]$")
        .expect("ANSI CSI pattern must be valid")
});
pub trait Clock {
    fn now_ms(&self) -> u128;
}

pub struct StdClock {
    start: time::Instant,
}

impl Default for StdClock {
    fn default() -> Self {
        Self::new()
    }
}

impl StdClock {
    pub fn new() -> Self {
        Self {
            start: time::Instant::now(),
        }
    }
}

impl Clock for StdClock {
    fn now_ms(&self) -> u128 {
        self.start.elapsed().as_millis()
    }
}

pub struct App {
    view_stack: views::ViewStack,
    vte_parser: vte::Parser,
    reporter: perform::Reporter,
    pending_input: VecDeque<u8>,
    pending_input_last_at: Option<u128>,
    focus_mode: protocol::FocusModeFilter,
    filtered_pty_output: Vec<u8>,
    focus_mode_changes: Vec<bool>,
    focus_mode_queries: Vec<bool>,
    pending_focus_mode_reports: Vec<u8>,
    deferred_pty_output: Vec<u8>,
    consumed_key_presses: HashSet<(KeyCode, KeyModifiers, KeyEventState)>,
    view_transition_key_presses: HashSet<(KeyCode, KeyModifiers, KeyEventState)>,
    log_enabled: bool,
    lua_repl_history: Vec<String>,
    last_stdin_update: Option<u128>,
    first_pty_update: Option<u128>,
    last_pty_update: Option<u128>,
    displayed_alternate_screen: bool,
    clock: Box<dyn Clock>,
}

impl App {
    pub fn new(view_stack: views::ViewStack) -> Result<Self> {
        Self::new_with_clock(view_stack, Box::new(StdClock::new()))
    }

    pub fn new_with_clock(view_stack: views::ViewStack, clock: Box<dyn Clock>) -> Result<Self> {
        let mut app = Self {
            view_stack,
            vte_parser: vte::Parser::new(),
            reporter: perform::Reporter::new(),
            pending_input: VecDeque::new(),
            pending_input_last_at: None,
            focus_mode: protocol::FocusModeFilter::default(),
            filtered_pty_output: Vec::new(),
            focus_mode_changes: Vec::new(),
            focus_mode_queries: Vec::new(),
            pending_focus_mode_reports: Vec::new(),
            deferred_pty_output: Vec::new(),
            consumed_key_presses: HashSet::new(),
            view_transition_key_presses: HashSet::new(),
            log_enabled: false,
            lua_repl_history: Vec::new(),
            last_stdin_update: None,
            first_pty_update: None,
            last_pty_update: None,
            displayed_alternate_screen: false,
            clock,
        };
        let now_ms = app.clock.now_ms();
        app.view_stack
            .active_mut()
            .model()
            .set_previous_screen_time(now_ms);
        Ok(app)
    }

    pub fn set_logging(&mut self, enabled: bool) {
        self.log_enabled = enabled;
    }

    fn format_bytes(bytes: &[u8]) -> String {
        let mut out = String::new();
        for &byte in bytes {
            match byte {
                b'\x1B' => out.push_str("\\e"),
                b'\r' => out.push_str("\\r"),
                b'\n' => out.push_str("\\n"),
                b'\t' => out.push_str("\\t"),
                0x20..=0x7E => out.push(byte as char),
                _ => out.push_str(&format!("\\x{byte:02X}")),
            }
        }
        out
    }

    fn log_bytes(&self, label: &str, bytes: &[u8]) {
        if self.log_enabled {
            eprintln!(
                "{label}: [{} bytes] {}",
                bytes.len(),
                Self::format_bytes(bytes)
            );
        }
    }

    fn log_event(&self, message: &str) {
        if self.log_enabled {
            eprintln!("{message}");
        }
    }

    pub fn wants_tick(&mut self) -> bool {
        self.view_stack.active_mut().wants_tick()
    }

    pub fn has_overlay(&self) -> bool {
        self.view_stack.has_overlay()
    }

    pub fn on_resize(&mut self, rows: u16, cols: u16, term_out: &mut dyn Write) -> Result<()> {
        self.view_stack.on_resize(rows, cols);
        if self.view_stack.has_overlay() {
            self.render_active_view(term_out)?;
        }
        Ok(())
    }

    pub fn show_message(
        &mut self,
        sr: &mut ScreenReader,
        title: &str,
        message: &str,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let (rows, cols) = self.view_stack.root_mut().model().size();
        self.view_stack.push(Box::new(views::MessageView::new(
            rows, cols, title, message,
        )));
        self.render_active_view(term_out)?;
        self.announce_view_change(sr)?;
        Ok(())
    }
}
