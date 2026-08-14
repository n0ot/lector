use crate::{
    commands,
    keymap::Binding,
    output_scheduler::{DrainReport, OutputScheduler, OutputSchedulerConfig, ScheduledOutputClass},
    presentation::{
        CursorOwner, GridPoint, IncrementalVtRenderer, PhysicalTerminalLifecycle, PresentedScene,
        RenderCapabilities, RendererBackend, Scene, SceneDamage, SceneOverlay, SceneSurface,
        SurfaceId,
    },
    screen_reader::{ScreenReader, TmuxBellMode},
    terminal::{TerminalGeometry, UpdateSummary},
    terminal_input::KeyInput,
    terminal_protocol::{
        ApplicationReplyBroker, PhysicalTerminalProfile, ProbePolicy, StartupProbeBroker,
        TerminalEffectPolicy,
    },
    tmux_gateway::TmuxGatewayRouter,
    tmux_lifecycle::{ConnectionHierarchy, GatewayOrigin},
    tmux_model::TmuxTopology,
    views,
};
use anyhow::{Context, Result};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet, VecDeque},
    io::Write,
    sync::LazyLock,
    time,
};
use terminput::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

mod input;
mod protocol;
mod pty;
mod tmux_interaction;
mod tmux_prefix;
mod view_stack;

use protocol::{
    FOCUS_IN_EVENT, FOCUS_OUT_EVENT, ModifyOtherKeysStatus, SequenceStatus, focus_event_status,
    is_invalid_ss3_prefix, modify_other_keys_status, osc_status, timed_out_event,
};

const ROOT_SOURCE: SurfaceId = SurfaceId(1);
const MAX_NESTED_TMUX_GATEWAYS: usize = 4_096;
const TMUX_TERMINATOR_TIMEOUT_MS: u128 = 1_000;
const TMUX_BELL_COALESCE_MS: u128 = 250;
const TMUX_MAX_EXTENDED_OUTPUT_AGE_MS: u64 = 5_000;
pub const TMUX_FLOW_CONTROL_COMMAND: &[u8] = b"refresh-client -f pause-after=1\n";

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

/// Stable tmux topology metadata for the most recently presented pane bell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TmuxBellSource {
    pub connection_id: u64,
    pub connection_label: String,
    pub session_id: crate::tmux_model::SessionId,
    pub session_name: String,
    pub window_id: crate::tmux_model::WindowId,
    pub window_name: String,
    pub pane_id: crate::tmux_model::PaneId,
    pub pane_title: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TmuxFlowStatus {
    #[default]
    Running,
    Paused,
    Resynchronizing,
    ResyncFailed,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum TmuxResyncLimitation {
    KittyImages,
    ParserContinuation,
    SemanticMetadata,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TmuxPaneFlowState {
    pub status: TmuxFlowStatus,
    pub resume_requested: bool,
    pub last_extended_output_age_ms: Option<u64>,
    pub skipped_incremental_bytes: usize,
    pub resync_count: u64,
    pub resync_failures: u64,
    pub limitations: BTreeSet<TmuxResyncLimitation>,
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
    tmux_gateway: TmuxGatewayRouter,
    tmux_termination_deadline_ms: Option<u128>,
    nested_tmux_gateways: BTreeMap<(u64, u64), NestedTmuxGatewayState>,
    next_tmux_connection_id: u64,
    tmux_hierarchy: ConnectionHierarchy,
    tmux_connections: Vec<TmuxConnectionState>,
    pending_tmux_commands: VecDeque<PendingTmuxCommand>,
    active_tmux_connection: Option<u64>,
    pending_tmux_confirmation: Option<PendingTmuxConfirmation>,
    pending_gateway_confirmation: Option<PendingGatewayConfirmation>,
    pending_direct_gateway_input: VecDeque<PendingDirectGatewayInput>,
    last_tmux_bell_source: Option<TmuxBellSource>,
    recent_tmux_bells: BTreeMap<(u64, crate::tmux_model::PaneId), u128>,
    clock: Box<dyn Clock>,
}

struct NestedTmuxGatewayState {
    router: TmuxGatewayRouter,
    active_local_connection_id: Option<u64>,
    active_global_connection_id: Option<u64>,
    termination_deadline_ms: Option<u128>,
}

impl NestedTmuxGatewayState {
    fn new() -> Self {
        Self {
            router: TmuxGatewayRouter::new(),
            active_local_connection_id: None,
            active_global_connection_id: None,
            termination_deadline_ms: None,
        }
    }
}

struct TmuxConnectionState {
    id: u64,
    topology: TmuxTopology,
    initial_command_seen: bool,
    inventory_replies_remaining: usize,
    pending_inventory: Vec<Vec<u8>>,
    inventory_failed: bool,
    expected_replies: VecDeque<ExpectedTmuxReply>,
    has_inventory: bool,
    inventory_retry_count: u8,
    command_history: Vec<String>,
    prefix_state: Option<TmuxPrefixState>,
    pane_flow: BTreeMap<crate::tmux_model::PaneId, TmuxPaneFlowState>,
}

#[derive(Clone)]
enum ExpectedTmuxReply {
    Inventory,
    Bootstrap(crate::tmux_model::PaneId),
    PaneResync(crate::tmux_model::PaneId),
    Ignored,
    UserCommand {
        description: String,
        show_success: bool,
    },
}

struct PendingTmuxCommand {
    connection_id: u64,
    bytes: Vec<u8>,
    expected_replies: Vec<ExpectedTmuxReply>,
    kind: PendingTmuxCommandKind,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PendingTmuxCommandKind {
    Ordinary,
    Resize,
    Input(crate::tmux_model::PaneId),
}

struct TmuxPrefixState {
    phase: TmuxPrefixPhase,
    expires_at_ms: u128,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TmuxPrefixPhase {
    Awaiting,
    Repeating,
}

struct PendingTmuxConfirmation {
    connection_id: u64,
    command: String,
    target: TmuxConfirmationTarget,
}

struct PendingGatewayConfirmation {
    connection_id: u64,
    action: crate::tmux_lifecycle::GatewayControlAction,
}

struct PendingDirectGatewayInput {
    connection_id: u64,
    bytes: Vec<u8>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TmuxConfirmationTarget {
    Pane(crate::tmux_model::PaneId),
    Window(crate::tmux_model::WindowId),
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
            tmux_gateway: TmuxGatewayRouter::new(),
            tmux_termination_deadline_ms: None,
            nested_tmux_gateways: BTreeMap::new(),
            next_tmux_connection_id: 1,
            tmux_hierarchy: ConnectionHierarchy::new(),
            tmux_connections: Vec::new(),
            pending_tmux_commands: VecDeque::new(),
            active_tmux_connection: None,
            pending_tmux_confirmation: None,
            pending_gateway_confirmation: None,
            pending_direct_gateway_input: VecDeque::new(),
            last_tmux_bell_source: None,
            recent_tmux_bells: BTreeMap::new(),
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

    /// Returns enough stable topology state to find the last presented bell source.
    #[must_use]
    pub fn last_tmux_bell_source(&self) -> Option<&TmuxBellSource> {
        self.last_tmux_bell_source.as_ref()
    }

    #[must_use]
    pub fn debug_tmux_pane_flow_state(
        &self,
        connection_id: u64,
        pane_id: u64,
    ) -> Option<&TmuxPaneFlowState> {
        self.tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id)?
            .pane_flow
            .get(&crate::tmux_model::PaneId(pane_id))
    }

    pub fn debug_tmux_resource_usage(
        &mut self,
        connection_id: u64,
    ) -> Option<crate::tmux_panes::TmuxResourceUsage> {
        self.view_stack
            .tmux_connection_mut(connection_id)?
            .resource_usage()
            .ok()
    }

    #[must_use]
    pub fn debug_scheduled_output_pending_bytes(&self) -> usize {
        self.output_scheduler
            .as_ref()
            .map_or(0, OutputScheduler::pending_bytes)
    }

    #[must_use]
    pub fn debug_tmux_pending_command_bytes(&self) -> usize {
        self.pending_tmux_commands
            .iter()
            .map(|command| command.bytes.len())
            .sum()
    }

    fn tmux_bell_source(
        &self,
        connection_id: u64,
        pane_id: crate::tmux_model::PaneId,
    ) -> Option<TmuxBellSource> {
        let topology = &self
            .tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id)?
            .topology;
        let session_id = topology.attached_session()?;
        let session = topology.session(session_id)?;
        let pane = topology.pane(pane_id)?;
        if !session
            .windows
            .values()
            .any(|window_id| *window_id == pane.window_id)
        {
            // A control client does not normally receive this stream. Keep the
            // boundary explicit if a synthetic or future source supplies it.
            return None;
        }
        let window = topology.window(pane.window_id)?;
        Some(TmuxBellSource {
            connection_id,
            connection_label: topology.label().to_owned(),
            session_id,
            session_name: session.name.clone(),
            window_id: window.id,
            window_name: window.name.clone(),
            pane_id,
            pane_title: pane.title.clone(),
        })
    }

    fn present_tmux_bell(
        &mut self,
        sr: &mut ScreenReader,
        connection_id: u64,
        pane_id: crate::tmux_model::PaneId,
        pane_is_visible: bool,
        term_out: &mut dyn Write,
    ) -> Result<usize> {
        let mode = sr.tmux_bell_mode();
        if mode == TmuxBellMode::Off {
            return Ok(0);
        }
        let Some(source) = self.tmux_bell_source(connection_id, pane_id) else {
            return Ok(0);
        };
        let now_ms = self.clock.now_ms();
        let key = (connection_id, pane_id);
        if self
            .recent_tmux_bells
            .get(&key)
            .is_some_and(|previous| now_ms.saturating_sub(*previous) < TMUX_BELL_COALESCE_MS)
        {
            return Ok(0);
        }
        self.recent_tmux_bells.insert(key, now_ms);
        self.last_tmux_bell_source = Some(source.clone());
        match mode {
            TmuxBellMode::Off => Ok(0),
            TmuxBellMode::Spoken => {
                sr.speak(
                    &format!(
                        "bell in tmux connection {} {}, session {} {}, window {} {}, pane {} {}",
                        source.connection_id,
                        source.connection_label,
                        source.session_id.0,
                        source.session_name,
                        source.window_id.0,
                        source.window_name,
                        source.pane_id.0,
                        source.pane_title,
                    ),
                    false,
                )?;
                Ok(0)
            }
            TmuxBellMode::Audible if pane_is_visible => Ok(1),
            TmuxBellMode::Audible => {
                self.emit_physical_bells(term_out, 1)?;
                Ok(0)
            }
        }
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
        let output_deadline = self
            .output_scheduler
            .as_ref()
            .and_then(OutputScheduler::next_deadline_ms);
        let gateway_deadline = self
            .tmux_termination_deadline_ms
            .into_iter()
            .chain(
                self.nested_tmux_gateways
                    .values()
                    .filter_map(|gateway| gateway.termination_deadline_ms),
            )
            .min();
        let deadline = match (output_deadline, gateway_deadline) {
            (Some(output), Some(gateway)) => output.min(gateway),
            (Some(deadline), None) | (None, Some(deadline)) => deadline,
            (None, None) => return None,
        };
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
            || !self.pending_tmux_commands.is_empty()
            || !self.pending_direct_gateway_input.is_empty()
            || self
                .tmux_termination_deadline_ms
                .is_some_and(|deadline| deadline <= self.clock.now_ms())
            || self.nested_tmux_gateways.values().any(|gateway| {
                gateway
                    .termination_deadline_ms
                    .is_some_and(|deadline| deadline <= self.clock.now_ms())
            })
            || self
                .output_scheduler
                .as_ref()
                .and_then(OutputScheduler::next_deadline_ms)
                .is_some_and(|deadline| deadline <= self.clock.now_ms())
    }

    pub fn has_overlay(&self) -> bool {
        self.view_stack.has_overlay()
    }

    #[must_use]
    pub fn tmux_connection_count(&self) -> usize {
        self.tmux_connections.len()
    }

    #[must_use]
    pub fn active_tmux_connection(&self) -> Option<u64> {
        self.active_tmux_connection
    }

    pub fn show_tmux_gateway(
        &mut self,
        connection_id: u64,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<bool> {
        if !self
            .tmux_connections
            .iter()
            .any(|connection| connection.id == connection_id)
        {
            return Ok(false);
        }
        if let Some(GatewayOrigin::Pane {
            parent_connection_id,
            ..
        }) = self.tmux_hierarchy.origin(connection_id)
        {
            if !self
                .view_stack
                .activate_tmux_connection(parent_connection_id)
            {
                return Ok(false);
            }
            self.active_tmux_connection = Some(parent_connection_id);
            if let Some(connection) = self
                .tmux_connections
                .iter_mut()
                .find(|connection| connection.id == connection_id)
            {
                connection.prefix_state = None;
            }
            self.pending_tmux_confirmation = None;
            self.pending_gateway_confirmation = None;
            if let Some(parent) = self.view_stack.tmux_connection_mut(parent_connection_id) {
                parent.show_connection();
            }
            self.render_active_view(term_out)?;
            self.announce_view_change(sr)?;
            return Ok(true);
        }
        if !self.view_stack.activate_tmux_connection(connection_id) {
            return Ok(false);
        }
        self.active_tmux_connection = Some(connection_id);
        if let Some(connection) = self
            .tmux_connections
            .iter_mut()
            .find(|connection| connection.id == connection_id)
        {
            connection.prefix_state = None;
        }
        self.pending_tmux_confirmation = None;
        self.pending_gateway_confirmation = None;
        let Some(connection) = self.view_stack.tmux_connection_mut(connection_id) else {
            return Ok(false);
        };
        connection.show_portal();
        self.render_active_view(term_out)?;
        self.announce_view_change(sr)?;
        Ok(true)
    }

    pub fn activate_tmux_connection(
        &mut self,
        connection_id: u64,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<bool> {
        if !self
            .tmux_connections
            .iter()
            .any(|connection| connection.id == connection_id)
            || !self.view_stack.activate_tmux_connection(connection_id)
        {
            return Ok(false);
        }
        self.pending_tmux_confirmation = None;
        self.pending_gateway_confirmation = None;
        self.active_tmux_connection = Some(connection_id);
        if let Some(connection) = self.view_stack.tmux_connection_mut(connection_id) {
            connection.show_connection();
        }
        self.render_active_view(term_out)?;
        self.announce_view_change(sr)?;
        Ok(true)
    }

    pub fn activate_terminal_mode(
        &mut self,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        self.pending_tmux_confirmation = None;
        self.pending_gateway_confirmation = None;
        self.active_tmux_connection = None;
        self.view_stack.activate_terminal();
        self.render_active_view(term_out)?;
        self.announce_view_change(sr)
    }

    #[must_use]
    pub fn debug_tmux_topology(&self, connection_id: u64) -> Option<String> {
        self.tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id)
            .map(|connection| connection.topology.debug_dump())
    }

    #[must_use]
    pub fn debug_tmux_gateway_origin(&self, connection_id: u64) -> Option<GatewayOrigin> {
        self.tmux_hierarchy.origin(connection_id)
    }

    pub fn debug_tmux_pane_portal_target(
        &mut self,
        connection_id: u64,
        pane_id: u64,
    ) -> Option<u64> {
        self.view_stack
            .tmux_connection_mut(connection_id)?
            .pane_portal_target(crate::tmux_model::PaneId(pane_id))
    }

    pub fn debug_tmux_pane_contents(&mut self, connection_id: u64, pane_id: u64) -> Option<String> {
        self.view_stack
            .tmux_connection_mut(connection_id)?
            .pane_contents(crate::tmux_model::PaneId(pane_id))
    }

    #[must_use]
    pub fn debug_nested_tmux_gateway_count(&self) -> usize {
        self.nested_tmux_gateways.len()
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
        if let Some(connection_id) = self.active_tmux_connection
            && self.view_stack.tmux_connection_mut(connection_id).is_some()
        {
            self.queue_tmux_resize(connection_id, geometry);
        }
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
