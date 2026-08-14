use crate::{
    commands,
    keymap::Binding,
    output_scheduler::{DrainReport, OutputScheduler, OutputSchedulerConfig, ScheduledOutputClass},
    presentation::{
        CursorOwner, GridPoint, IncrementalVtRenderer, PhysicalTerminalLifecycle, PresentedScene,
        RenderCapabilities, RendererBackend, Scene, SceneDamage, SceneOverlay, SceneSurface,
        SurfaceId,
    },
    screen_reader::ScreenReader,
    terminal::{TerminalGeometry, UpdateSummary},
    terminal_input::KeyInput,
    terminal_protocol::{
        ApplicationReplyBroker, PhysicalTerminalProfile, ProbePolicy, StartupProbeBroker,
        TerminalEffectPolicy,
    },
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

const ROOT_SOURCE: SurfaceId = SurfaceId(1);

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
    pending_input: VecDeque<u8>,
    pending_input_last_at: Option<u128>,
    application_replies: ApplicationReplyBroker<SurfaceId>,
    terminal_effect_policy: TerminalEffectPolicy,
    popup_responses: VecDeque<views::PopupResponse>,
    consumed_key_presses: HashSet<(KeyCode, KeyModifiers, KeyEventState)>,
    view_transition_key_presses: HashSet<(KeyCode, KeyModifiers, KeyEventState)>,
    log_enabled: bool,
    lua_repl_history: Vec<String>,
    last_stdin_update: Option<u128>,
    first_pty_update: Option<u128>,
    last_pty_update: Option<u128>,
    scene_renderer: IncrementalVtRenderer,
    presented_scene: PresentedScene,
    physical_profile: PhysicalTerminalProfile,
    startup_probe_broker: Option<StartupProbeBroker>,
    output_scheduler: Option<OutputScheduler>,
    physical_lifecycle: PhysicalTerminalLifecycle,
    clock: Box<dyn Clock>,
}

impl App {
    pub fn new(view_stack: views::ViewStack) -> Result<Self> {
        Self::new_with_clock(view_stack, Box::new(StdClock::new()))
    }

    pub fn new_with_clock(mut view_stack: views::ViewStack, clock: Box<dyn Clock>) -> Result<Self> {
        let geometry = view_stack.root_mut().model().screen().geometry;
        let physical_profile = PhysicalTerminalProfile::conservative(geometry);
        let mut app = Self {
            view_stack,
            pending_input: VecDeque::new(),
            pending_input_last_at: None,
            application_replies: ApplicationReplyBroker::default(),
            terminal_effect_policy: TerminalEffectPolicy::secure_default(),
            popup_responses: VecDeque::new(),
            consumed_key_presses: HashSet::new(),
            view_transition_key_presses: HashSet::new(),
            log_enabled: false,
            lua_repl_history: Vec::new(),
            last_stdin_update: None,
            first_pty_update: None,
            last_pty_update: None,
            scene_renderer: IncrementalVtRenderer::new(RenderCapabilities {
                synchronized_output: false,
                hyperlinks: physical_profile.hyperlinks,
                kitty_graphics: physical_profile.kitty_graphics,
                inline_terminal_effects: true,
            }),
            presented_scene: PresentedScene::blank(geometry),
            physical_profile,
            startup_probe_broker: None,
            output_scheduler: None,
            physical_lifecycle: PhysicalTerminalLifecycle::new(None),
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

    pub fn set_physical_profile(&mut self, profile: PhysicalTerminalProfile) {
        let capabilities = RenderCapabilities {
            synchronized_output: self.output_scheduler.is_none() && profile.synchronized_output,
            hyperlinks: profile.hyperlinks,
            kitty_graphics: profile.kitty_graphics,
            inline_terminal_effects: self.output_scheduler.is_none(),
        };
        if let Some(scheduler) = &mut self.output_scheduler {
            scheduler.set_synchronized_output_supported(profile.synchronized_output);
        }
        self.scene_renderer.set_capabilities(capabilities);
        self.physical_profile = profile;
    }

    pub fn enable_output_scheduler(&mut self, config: OutputSchedulerConfig) {
        self.scene_renderer.set_capabilities(RenderCapabilities {
            synchronized_output: false,
            hyperlinks: self.physical_profile.hyperlinks,
            kitty_graphics: self.physical_profile.kitty_graphics,
            inline_terminal_effects: false,
        });
        self.output_scheduler = Some(OutputScheduler::new(
            config,
            self.physical_profile.synchronized_output,
        ));
    }

    pub fn configure_physical_terminal(&mut self, focus_was_enabled: Option<bool>) {
        self.physical_lifecycle = PhysicalTerminalLifecycle::new(focus_was_enabled);
    }

    pub fn activate_physical_terminal(&mut self, term_out: &mut dyn Write) -> Result<()> {
        let transaction = self.physical_lifecycle.activate();
        self.apply_lifecycle_transaction(term_out, transaction, true)
    }

    pub fn suspend_physical_terminal(&mut self, term_out: &mut dyn Write) -> Result<()> {
        let transaction = self.physical_lifecycle.suspend();
        self.apply_lifecycle_transaction(term_out, transaction, false)
    }

    pub fn resume_physical_terminal(&mut self, term_out: &mut dyn Write) -> Result<()> {
        let transaction = self.physical_lifecycle.resume();
        self.apply_lifecycle_transaction(term_out, transaction, true)
    }

    pub fn shutdown_physical_terminal(&mut self, term_out: &mut dyn Write) -> Result<()> {
        let transaction = self.physical_lifecycle.shutdown();
        self.apply_lifecycle_transaction(term_out, transaction, false)
    }

    fn apply_lifecycle_transaction(
        &mut self,
        term_out: &mut dyn Write,
        transaction: crate::presentation::LifecycleTransaction,
        reconstruct: bool,
    ) -> Result<()> {
        if matches!(transaction.damage, SceneDamage::Full) {
            self.scene_renderer.invalidate();
        }
        if !reconstruct
            && !transaction.bytes.is_empty()
            && let Some(scheduler) = &mut self.output_scheduler
        {
            scheduler.prepare_for_lifecycle_cleanup();
        }
        self.emit_physical_bytes(
            term_out,
            ScheduledOutputClass::Control,
            &transaction.bytes,
            "write physical terminal lifecycle transaction",
        )?;
        if reconstruct && !transaction.bytes.is_empty() {
            self.render_active_view(term_out)?;
        }
        Ok(())
    }

    pub fn drain_scheduled_output(
        &mut self,
        term_out: &mut dyn Write,
        force: bool,
    ) -> Result<DrainReport> {
        let Some(scheduler) = &mut self.output_scheduler else {
            return Ok(DrainReport::default());
        };
        let report = match scheduler.drain_ready(self.clock.now_ms(), force, term_out) {
            Ok(report) => report,
            Err(error) => {
                self.scene_renderer.invalidate();
                return Err(error).context("drain scheduled terminal output");
            }
        };
        for completed in &report.completed_renders {
            self.scene_renderer.confirm(&completed.predicted);
            self.presented_scene = completed.predicted.clone();
        }
        Ok(report)
    }

    pub fn scheduled_output_timeout(&self) -> Option<time::Duration> {
        let deadline = self.output_scheduler.as_ref()?.next_deadline_ms()?;
        let remaining = deadline.saturating_sub(self.clock.now_ms());
        Some(time::Duration::from_millis(
            remaining.try_into().unwrap_or(u64::MAX),
        ))
    }

    pub fn notify_scheduled_output_writable(&mut self) {
        if let Some(scheduler) = &mut self.output_scheduler {
            scheduler.notify_writable();
        }
    }

    pub fn physical_profile(&self) -> &PhysicalTerminalProfile {
        &self.physical_profile
    }

    pub fn presented_scene(&self) -> &PresentedScene {
        &self.presented_scene
    }

    pub fn take_popup_response(&mut self) -> Option<views::PopupResponse> {
        self.popup_responses.pop_front()
    }

    pub fn start_capability_probes(&mut self, term_out: &mut dyn Write) -> Result<()> {
        let mut broker = StartupProbeBroker::new(
            self.physical_profile.clone(),
            ProbePolicy::safe(),
            self.clock.now_ms(),
        );
        let queries = broker.startup_queries();
        self.emit_physical_bytes(
            term_out,
            ScheduledOutputClass::Control,
            &queries,
            "write physical terminal capability probes",
        )?;
        self.startup_probe_broker = Some(broker);
        Ok(())
    }

    pub(super) fn emit_physical_bytes(
        &mut self,
        term_out: &mut dyn Write,
        class: ScheduledOutputClass,
        bytes: &[u8],
        context: &'static str,
    ) -> Result<()> {
        if let Some(scheduler) = &mut self.output_scheduler {
            scheduler.enqueue_bytes(class, bytes.to_vec(), self.clock.now_ms());
            return Ok(());
        }
        term_out.write_all(bytes).with_context(|| context)?;
        term_out.flush().with_context(|| context)
    }

    pub(super) fn emit_physical_bells(
        &mut self,
        term_out: &mut dyn Write,
        count: usize,
    ) -> Result<()> {
        if let Some(scheduler) = &mut self.output_scheduler {
            scheduler.enqueue_bell(count, self.clock.now_ms());
            return Ok(());
        }
        let bells = vec![b'\x07'; count];
        term_out.write_all(&bells).context("write bell")?;
        term_out.flush().context("flush bell")
    }

    fn refresh_probed_profile(&mut self) {
        let Some(profile) = self
            .startup_probe_broker
            .as_ref()
            .map(|broker| broker.profile().clone())
        else {
            return;
        };
        self.set_physical_profile(profile);
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
        self.startup_probe_broker
            .as_ref()
            .is_some_and(|broker| !broker.is_finished())
            || self.view_stack.active_mut().wants_tick()
            || self
                .output_scheduler
                .as_ref()
                .and_then(OutputScheduler::next_deadline_ms)
                .is_some_and(|deadline| deadline <= self.clock.now_ms())
    }

    pub fn has_overlay(&self) -> bool {
        self.view_stack.has_overlay()
    }

    pub fn on_resize(&mut self, rows: u16, cols: u16, term_out: &mut dyn Write) -> Result<()> {
        self.on_resize_with_geometry(TerminalGeometry::from_cells(rows, cols), term_out)
    }

    pub fn on_resize_with_geometry(
        &mut self,
        geometry: TerminalGeometry,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        self.view_stack.on_resize_with_geometry(geometry);
        self.render_active_view(term_out)?;
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

    pub fn show_popup_announcement(
        &mut self,
        sr: &mut ScreenReader,
        title: &str,
        message: &str,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let (rows, cols) = self.view_stack.root_mut().model().size();
        self.handle_view_action(
            sr,
            views::ViewAction::Push(Box::new(views::PopupView::announcement(
                rows, cols, title, message,
            ))),
            term_out,
        )
    }

    pub fn show_popup_error(
        &mut self,
        sr: &mut ScreenReader,
        title: &str,
        message: &str,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        self.show_popup_announcement(sr, title, message, term_out)
    }

    pub fn show_popup_confirmation(
        &mut self,
        sr: &mut ScreenReader,
        title: &str,
        message: &str,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let (rows, cols) = self.view_stack.root_mut().model().size();
        self.handle_view_action(
            sr,
            views::ViewAction::Push(Box::new(views::PopupView::confirmation(
                rows, cols, title, message,
            ))),
            term_out,
        )
    }
}
