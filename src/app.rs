use crate::{
    commands,
    keymap::Binding,
    output_scheduler::{DrainReport, OutputScheduler, OutputSchedulerConfig, ScheduledOutputClass},
    presentation::{
        CursorOwner, GridPoint, IncrementalVtRenderer, PhysicalTerminalLifecycle, PresentedScene,
        RenderCapabilities, RendererBackend, Scene, SceneDamage, SceneOverlay, SceneSurface,
        SurfaceId, ViewId,
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
    view::View,
    views,
};
use anyhow::{Context, Result};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
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
const TMUX_FORCE_ABANDON_GRACE_MS: u128 = 750;
const TMUX_BELL_COALESCE_MS: u128 = 250;
const TMUX_RECOVERY_QUIET_MS: u128 = 100;
const TMUX_RECOVERY_HARD_DEADLINE_MS: u128 = 1_000;
const TMUX_FLOW_RETRY_BASE_MS: u128 = 100;
const TMUX_FLOW_RETRY_MAX_MS: u128 = 2_000;
const TMUX_HIDDEN_IMMEDIATE_BUDGET_BYTES: usize = 4 * 1024;
const TMUX_BACKGROUND_DRAIN_BUDGET_BYTES: usize = 4 * 1024;
const TMUX_BACKGROUND_PAUSE_THRESHOLD_BYTES: usize = 16 * 1024;
const TMUX_BACKGROUND_OUTPUT_LIMIT_BYTES: usize = 64 * 1024;
const TMUX_PANE_OUTPUT_COALESCE_LIMIT_BYTES: usize = 64 * 1024;
const TMUX_EXPECTED_REPLY_LIMIT: usize = 512;
const TMUX_ROUTED_COMMAND_LIMIT_BYTES: usize = 1024 * 1024;
const TMUX_ROUTED_COMMAND_LIMIT_COUNT: usize = 4_096;
const TMUX_INVENTORY_LIMIT_BYTES: usize = 8 * 1024 * 1024;
const TMUX_INVENTORY_LIMIT_LINES: usize = 65_536;
const KITTY_CTRL_C_RELEASE_HANDOFF_MS: u128 = 100;
const KITTY_CTRL_C_INPUT_HANDOFF_MS: u128 = 500;
const MAX_DEFERRED_KITTY_RELEASES: usize = 64;
pub const TMUX_FLOW_CONTROL_COMMAND: &[u8] = b"refresh-client -f pause-after=1\n";
pub const TMUX_FLOW_CONTROL_VERIFY_COMMAND: &[u8] = b"display-message -p -F '#{client_flags}'\n";
pub const MAX_PENDING_TERMINAL_INPUT_BYTES: usize = 64 * 1024;

pub const DIFF_DELAY: u16 = 30;
pub const MAX_DIFF_DELAY: u16 = 300;
const ESC_TIMEOUT_MS: u128 = 50;
static ANSI_CSI_RE: LazyLock<regex::bytes::Regex> = LazyLock::new(|| {
    regex::bytes::Regex::new(r"^\x1B\[[\x30-\x3F]*[\x20-\x2F]*[\x40-\x7E--[A-D~]]$")
        .expect("ANSI CSI pattern must be valid")
});

fn synchronize_pending_review_cursor(sr: &mut ScreenReader, view: &mut View) -> Result<()> {
    if sr.review_follows_screen_cursor() && view.review_cursor_follow_pending() {
        let old = view.review_cursor_position();
        view.follow_application_cursor();
        sr.hook_on_review_cursor_move(old, view.review_cursor_position())?;
    }
    Ok(())
}

fn format_terminal_accessibility_label(title: &str) -> String {
    if title.is_empty() {
        "terminal".to_owned()
    } else {
        format!("terminal, {title}")
    }
}

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
    pub is_paused: bool,
    pub pause_requested: bool,
    pub resume_requested: bool,
    pub resync_requested: bool,
    pub resync_in_flight: bool,
    pub final_resync_requested: bool,
    pub resync_after_ms: Option<u128>,
    pub recapture_hard_deadline_ms: Option<u128>,
    pub last_extended_output_age_ms: Option<u64>,
    pub skipped_incremental_bytes: usize,
    pub resync_count: u64,
    pub resync_failures: u64,
    pub consecutive_resync_failures: u32,
    pub resync_failure_announced: bool,
    pub limitations: BTreeSet<TmuxResyncLimitation>,
}

#[derive(Default)]
struct PendingTmuxPresentationBatch {
    updates: BTreeMap<(u64, crate::tmux_model::PaneId), UpdateSummary>,
    bell_count: usize,
    last_ordinary_output: Option<(u64, u64)>,
    pending_ordinary_output: Option<crate::tmux_gateway::GatewayEvent>,
}

impl PendingTmuxPresentationBatch {
    /// Keeps the first pane update observable immediately, then combines the
    /// rest of an adjacent same-pane run until the bounded PTY drain reaches
    /// an ordering fence. This avoids paying Ghostty's snapshot cost once per
    /// tmux record while preserving control-record and cross-pane order.
    fn route_gateway_event(
        &mut self,
        event: crate::tmux_gateway::GatewayEvent,
    ) -> (
        Option<crate::tmux_gateway::GatewayEvent>,
        Option<crate::tmux_gateway::GatewayEvent>,
    ) {
        let output_key = match &event {
            crate::tmux_gateway::GatewayEvent::Control {
                connection_id,
                event: crate::tmux_control::ControlEvent::Output { pane_id, .. },
            } => Some((*connection_id, *pane_id)),
            _ => None,
        };

        if let Some(key) = output_key
            && self.last_ordinary_output == Some(key)
        {
            let incoming_len = ordinary_tmux_output_len(&event).unwrap_or(0);
            if let Some(pending) = &mut self.pending_ordinary_output {
                let pending_len = ordinary_tmux_output_len(pending).unwrap_or(0);
                if pending_len.saturating_add(incoming_len) <= TMUX_PANE_OUTPUT_COALESCE_LIMIT_BYTES
                {
                    append_ordinary_tmux_output(pending, event);
                    return (None, None);
                }
                let ready = self.pending_ordinary_output.replace(event);
                return (ready, None);
            }
            if incoming_len <= TMUX_PANE_OUTPUT_COALESCE_LIMIT_BYTES {
                self.pending_ordinary_output = Some(event);
                return (None, None);
            }
            return (Some(event), None);
        }

        let pending = self.pending_ordinary_output.take();
        self.last_ordinary_output = output_key;
        if pending.is_some() {
            (pending, Some(event))
        } else {
            (Some(event), None)
        }
    }

    fn take_pending_gateway_event(&mut self) -> Option<crate::tmux_gateway::GatewayEvent> {
        self.last_ordinary_output = None;
        self.pending_ordinary_output.take()
    }

    fn push(
        &mut self,
        connection_id: u64,
        pane_id: crate::tmux_model::PaneId,
        bell_count: usize,
        update: UpdateSummary,
    ) {
        self.bell_count = self.bell_count.saturating_add(bell_count);
        self.updates
            .entry((connection_id, pane_id))
            .or_default()
            .merge(update);
    }
}

fn ordinary_tmux_output_len(event: &crate::tmux_gateway::GatewayEvent) -> Option<usize> {
    match event {
        crate::tmux_gateway::GatewayEvent::Control {
            event: crate::tmux_control::ControlEvent::Output { bytes, .. },
            ..
        } => Some(bytes.len()),
        _ => None,
    }
}

fn append_ordinary_tmux_output(
    pending: &mut crate::tmux_gateway::GatewayEvent,
    incoming: crate::tmux_gateway::GatewayEvent,
) {
    let (
        crate::tmux_gateway::GatewayEvent::Control {
            connection_id: pending_connection,
            event:
                crate::tmux_control::ControlEvent::Output {
                    pane_id: pending_pane,
                    bytes: pending_bytes,
                },
        },
        crate::tmux_gateway::GatewayEvent::Control {
            connection_id: incoming_connection,
            event:
                crate::tmux_control::ControlEvent::Output {
                    pane_id: incoming_pane,
                    bytes: incoming_bytes,
                },
        },
    ) = (pending, incoming)
    else {
        unreachable!("only matching ordinary tmux output events are coalesced");
    };
    debug_assert_eq!(*pending_connection, incoming_connection);
    debug_assert_eq!(*pending_pane, incoming_pane);
    pending_bytes.extend(incoming_bytes);
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
    forwarded_key_presses: HashMap<(KeyCode, KeyModifiers, KeyEventState), ForwardedKeyPress>,
    deferred_kitty_releases: VecDeque<DeferredKittyRelease>,
    kitty_ctrl_c_input_handoff: Option<KittyInputHandoff>,
    log_enabled: bool,
    lua_repl_history: Vec<String>,
    last_stdin_update: Option<u128>,
    first_pty_update: Option<u128>,
    last_pty_update: Option<u128>,
    scene_renderer: IncrementalVtRenderer,
    presented_scene: PresentedScene,
    presented_accessibility_view: Option<ViewId>,
    presented_accessibility_label: Option<String>,
    presented_accessibility_label_tracks_terminal_title: bool,
    pending_view_announcement: bool,
    pending_active_view_read: Option<ViewId>,
    physical_profile: PhysicalTerminalProfile,
    startup_probe_broker: Option<StartupProbeBroker>,
    output_scheduler: Option<OutputScheduler>,
    physical_lifecycle: PhysicalTerminalLifecycle,
    tmux_gateway: TmuxGatewayRouter,
    tmux_termination_deadline_ms: Option<u128>,
    nested_tmux_gateways: BTreeMap<(u64, u64), NestedTmuxGatewayState>,
    tmux_hierarchy: ConnectionHierarchy,
    tmux_connections: Vec<TmuxConnectionState>,
    pending_tmux_commands: VecDeque<PendingTmuxCommand>,
    active_tmux_connection: Option<u64>,
    pending_tmux_confirmation: Option<PendingTmuxConfirmation>,
    pending_gateway_confirmation: Option<PendingGatewayConfirmation>,
    pending_direct_gateway_input: VecDeque<PendingDirectGatewayInput>,
    pending_graceful_teardown: Option<PendingGracefulTeardown>,
    pending_force_abandon: Option<PendingForceAbandon>,
    last_tmux_bell_source: Option<TmuxBellSource>,
    recent_tmux_bells: BTreeMap<(u64, crate::tmux_model::PaneId), u128>,
    tmux_background_bell_windows: BTreeSet<(u64, crate::tmux_model::WindowId)>,
    pending_tmux_background_output: BTreeMap<(u64, crate::tmux_model::PaneId), VecDeque<u8>>,
    pending_tmux_background_order: VecDeque<(u64, crate::tmux_model::PaneId)>,
    pending_tmux_background_bytes: usize,
    tmux_hidden_output_bytes_this_turn: usize,
    pending_tmux_presentation_batch: Option<PendingTmuxPresentationBatch>,
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
    pending_inventory_bytes: usize,
    pending_inventory_lines: usize,
    inventory_failed: bool,
    inventory_failure_detail: Option<String>,
    expected_replies: VecDeque<ExpectedTmuxReply>,
    has_inventory: bool,
    inventory_retry_count: u8,
    command_history: Vec<String>,
    prefix_state: Option<TmuxPrefixState>,
    key_table_override: Option<String>,
    pane_flow: BTreeMap<crate::tmux_model::PaneId, TmuxPaneFlowState>,
    pending_pane_captures: BTreeMap<crate::tmux_model::PaneId, PendingTmuxPaneCapture>,
    flow_control_policy_accepted: Option<bool>,
    flow_control_verified: Option<bool>,
    flow_control_warning_announced: bool,
    last_announced_location: Option<crate::tmux_model::TmuxLocation>,
}

struct PendingTmuxPaneCapture {
    metadata: crate::tmux_model::PaneCaptureMetadata,
    output: Option<Vec<Vec<u8>>>,
    pending_escape: Vec<u8>,
    parser_continuation_available: bool,
    failed: bool,
}

impl TmuxConnectionState {
    fn append_inventory(&mut self, output: Vec<Vec<u8>>) {
        if self.inventory_failed {
            return;
        }
        let added_bytes = output.iter().map(Vec::len).sum::<usize>();
        let next_bytes = self.pending_inventory_bytes.checked_add(added_bytes);
        let next_lines = self.pending_inventory_lines.checked_add(output.len());
        if next_bytes.is_none_or(|bytes| bytes > TMUX_INVENTORY_LIMIT_BYTES)
            || next_lines.is_none_or(|lines| lines > TMUX_INVENTORY_LIMIT_LINES)
        {
            self.inventory_failed = true;
            self.inventory_failure_detail =
                Some("tmux inventory exceeded Lector's bounded transaction limits".to_owned());
            self.clear_inventory();
            return;
        }
        self.pending_inventory_bytes = next_bytes.expect("inventory byte total checked");
        self.pending_inventory_lines = next_lines.expect("inventory line total checked");
        self.pending_inventory.extend(output);
    }

    fn take_inventory(&mut self) -> Vec<Vec<u8>> {
        self.pending_inventory_bytes = 0;
        self.pending_inventory_lines = 0;
        std::mem::take(&mut self.pending_inventory)
    }

    fn clear_inventory(&mut self) {
        self.pending_inventory.clear();
        self.pending_inventory_bytes = 0;
        self.pending_inventory_lines = 0;
    }

    fn record_inventory_error(&mut self, output: &[Vec<u8>]) {
        self.inventory_failed = true;
        self.clear_inventory();
        if self.inventory_failure_detail.is_some() {
            return;
        }
        let detail = output
            .iter()
            .take(8)
            .map(|line| String::from_utf8_lossy(line))
            .collect::<Vec<_>>()
            .join("\n");
        self.inventory_failure_detail = Some(if detail.is_empty() {
            "tmux rejected an inventory command without an error message".to_owned()
        } else {
            detail.chars().take(4_096).collect()
        });
    }

    fn reset_inventory_attempt(&mut self) {
        self.clear_inventory();
        self.inventory_failed = false;
        self.inventory_failure_detail = None;
    }
}

#[derive(Clone)]
enum ExpectedTmuxReply {
    Inventory,
    FlowControlPolicy,
    FlowControlVerification,
    Bootstrap(crate::tmux_model::PaneId),
    PaneResyncProbe(crate::tmux_model::PaneId),
    PaneResyncCapture(crate::tmux_model::PaneId),
    PaneResyncPendingEscape(crate::tmux_model::PaneId),
    PaneResyncVerify(crate::tmux_model::PaneId),
    PaneResyncContinue(crate::tmux_model::PaneId),
    PanePause(crate::tmux_model::PaneId),
    PaneContinue(crate::tmux_model::PaneId),
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
}

#[derive(Clone, Eq, PartialEq)]
enum TmuxPrefixPhase {
    Awaiting { table: String },
    Repeating { table: String, expires_at_ms: u128 },
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ForwardedInputTarget {
    RootPty,
    TmuxPane {
        connection_id: u64,
        pane_id: crate::tmux_model::PaneId,
    },
}

#[derive(Clone, Copy)]
struct ForwardedKeyPress {
    target: ForwardedInputTarget,
    kitty_keyboard_flags: u8,
}

struct DeferredKittyRelease {
    target: ForwardedInputTarget,
    kitty_keyboard_flags: u8,
    bytes: Vec<u8>,
    release_at_ms: u128,
}

#[derive(Clone, Copy)]
struct KittyInputHandoff {
    target: ForwardedInputTarget,
    deadline_ms: u128,
}

struct PendingGracefulTeardown {
    remaining: VecDeque<u64>,
    awaiting: Option<u64>,
}

struct PendingForceAbandon {
    connection_id: u64,
    deadline_ms: u128,
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
        let geometry = view_stack.root_mut().model().live_screen().geometry;
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
            forwarded_key_presses: HashMap::new(),
            deferred_kitty_releases: VecDeque::new(),
            kitty_ctrl_c_input_handoff: None,
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
            presented_accessibility_view: None,
            presented_accessibility_label: None,
            presented_accessibility_label_tracks_terminal_title: false,
            pending_view_announcement: false,
            pending_active_view_read: None,
            physical_profile,
            startup_probe_broker: None,
            output_scheduler: None,
            physical_lifecycle: PhysicalTerminalLifecycle::new(None),
            tmux_gateway: TmuxGatewayRouter::new(),
            tmux_termination_deadline_ms: None,
            nested_tmux_gateways: BTreeMap::new(),
            tmux_hierarchy: ConnectionHierarchy::new(),
            tmux_connections: Vec::new(),
            pending_tmux_commands: VecDeque::new(),
            active_tmux_connection: None,
            pending_tmux_confirmation: None,
            pending_gateway_confirmation: None,
            pending_direct_gateway_input: VecDeque::new(),
            pending_graceful_teardown: None,
            pending_force_abandon: None,
            last_tmux_bell_source: None,
            recent_tmux_bells: BTreeMap::new(),
            tmux_background_bell_windows: BTreeSet::new(),
            pending_tmux_background_output: BTreeMap::new(),
            pending_tmux_background_order: VecDeque::new(),
            pending_tmux_background_bytes: 0,
            tmux_hidden_output_bytes_this_turn: 0,
            pending_tmux_presentation_batch: None,
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

    #[must_use]
    pub fn debug_tmux_background_pending_bytes(&self) -> usize {
        self.pending_tmux_background_bytes
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

    #[must_use]
    pub fn debug_pending_terminal_input_bytes(&self) -> usize {
        self.pending_input.len()
    }

    #[must_use]
    pub fn debug_tmux_expected_reply_count(&self, connection_id: u64) -> Option<usize> {
        self.tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id)
            .map(|connection| connection.expected_replies.len())
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

    fn tmux_bell_concise_announcement(&self, source: &TmuxBellSource) -> Option<String> {
        let topology = &self
            .tmux_connections
            .iter()
            .find(|connection| connection.id == source.connection_id)?
            .topology;
        let session = topology.session(source.session_id)?;
        let window_index = session
            .windows
            .iter()
            .find_map(|(index, window_id)| (*window_id == source.window_id).then_some(*index))?;
        let pane = topology.pane(source.pane_id)?;
        let pane_count = topology
            .panes()
            .values()
            .filter(|candidate| candidate.window_id == source.window_id)
            .count();
        Some(if pane_count > 1 {
            format!("bell in pane {window_index}.{}", pane.index)
        } else {
            format!("bell in window {window_index}")
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
        let window_is_background = self
            .tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id)
            .and_then(|connection| connection.topology.session(source.session_id))
            .is_some_and(|session| session.active_window != Some(source.window_id));
        if window_is_background
            && !self
                .tmux_background_bell_windows
                .insert((connection_id, source.window_id))
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
            TmuxBellMode::Audible => {
                if window_is_background
                    && let Some(announcement) = self.tmux_bell_concise_announcement(&source)
                {
                    sr.speak(&announcement, false)?;
                }
                if pane_is_visible {
                    Ok(1)
                } else {
                    self.emit_physical_bells(term_out, 1)?;
                    Ok(0)
                }
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
        self.view_stack.enable_presentation_tracking();
        self.presented_accessibility_view = Some(self.view_stack.logical_active_view_id());
        let (label, tracks_terminal_title) = self.view_stack.active_accessibility_label(false);
        self.presented_accessibility_label = Some(label);
        self.presented_accessibility_label_tracks_terminal_title = tracks_terminal_title;
    }

    fn presented_accessibility_model_mut(&mut self) -> &mut View {
        if let Some(view_id) = self.presented_accessibility_view
            && self.view_stack.contains_view_id(view_id)
        {
            return self
                .view_stack
                .model_by_id_mut(view_id)
                .expect("a checked presented accessibility view must remain addressable");
        }
        self.view_stack.active_mut().model()
    }

    fn active_presentation_finalization_pending(&mut self) -> bool {
        if self.output_scheduler.is_none() || self.view_stack.has_overlay() {
            return false;
        }
        let logical_view = self.view_stack.logical_active_view_id();
        self.presented_accessibility_view == Some(logical_view)
            && self
                .view_stack
                .model_by_id_mut(logical_view)
                .is_some_and(|view| view.accessibility_has_unfinalized_presentation())
    }

    fn prune_retired_accessibility_views(&mut self) {
        let mut retained = self
            .output_scheduler
            .as_ref()
            .map_or_else(Vec::new, OutputScheduler::retained_accessibility_view_ids);
        if let Some(presented) = self.presented_accessibility_view {
            retained.push(presented);
        }
        retained.sort_unstable();
        retained.dedup();
        self.view_stack.retain_accessibility_views(&retained);
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

    pub fn begin_physical_terminal_shutdown_fence(
        &mut self,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let transaction = self.physical_lifecycle.begin_shutdown_fence();
        self.apply_lifecycle_transaction(term_out, transaction, false)
    }

    pub fn finish_physical_terminal_shutdown_fence(
        &mut self,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let transaction = self.physical_lifecycle.finish_shutdown_fence();
        self.apply_lifecycle_transaction(term_out, transaction, false)
    }

    pub fn outstanding_startup_primary_device_attributes_replies(&self) -> usize {
        self.startup_probe_broker.as_ref().map_or(0, |broker| {
            broker.outstanding_primary_device_attributes_replies()
        })
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
            self.view_stack
                .apply_presented_bundle(&completed.accessibility);
            if let Some(active_view) = completed.accessibility.active_view {
                self.presented_accessibility_view = Some(active_view);
            }
            self.presented_accessibility_label
                .clone_from(&completed.accessibility.active_label);
            self.presented_accessibility_label_tracks_terminal_title =
                completed.accessibility.active_label_tracks_terminal_title;
        }
        for completed in &report.completed_effects {
            self.presented_scene.apply_terminal_effect(&completed.event);
            if completed.owner == ROOT_SOURCE
                && self.presented_accessibility_label_tracks_terminal_title
                && let crate::terminal::TerminalEvent::TitleChanged(title) = &completed.event
            {
                self.presented_accessibility_label =
                    Some(format_terminal_accessibility_label(title));
            }
        }
        if report.application_synchronization_bypass_completed {
            self.view_stack.complete_compositor_transition();
        }
        self.prune_retired_accessibility_views();
        Ok(report)
    }

    pub fn scheduled_output_timeout(&mut self) -> Option<time::Duration> {
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
        let pane_resync_deadline = self
            .tmux_connections
            .iter()
            .flat_map(|connection| connection.pane_flow.values())
            .filter(|flow| !flow.resync_requested)
            .filter_map(|flow| flow.resync_after_ms)
            .min();
        let pending_input_deadline = self
            .pending_input_last_at
            .map(|last_at| last_at.saturating_add(ESC_TIMEOUT_MS));
        let accessibility_deadline = self
            .active_presentation_finalization_pending()
            .then(|| {
                let last = self.last_pty_update?;
                let first = self.first_pty_update.unwrap_or(last);
                Some(
                    last.saturating_add(DIFF_DELAY as u128)
                        .min(first.saturating_add(MAX_DIFF_DELAY as u128)),
                )
            })
            .flatten();
        let deadline = output_deadline
            .into_iter()
            .chain(gateway_deadline)
            .chain(pane_resync_deadline)
            .chain(pending_input_deadline)
            .chain(accessibility_deadline)
            .chain(
                self.pending_force_abandon
                    .as_ref()
                    .map(|pending| pending.deadline_ms),
            )
            .chain(
                self.startup_probe_broker
                    .as_ref()
                    .and_then(StartupProbeBroker::next_deadline_ms),
            )
            .chain(
                self.deferred_kitty_releases
                    .iter()
                    .map(|release| release.release_at_ms)
                    .min(),
            )
            .min()?;
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

    pub fn flush_pending_clipboard_writes(
        &mut self,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        for bytes in sr.take_terminal_clipboard_writes() {
            self.emit_physical_bytes(
                term_out,
                ScheduledOutputClass::Control,
                &bytes,
                "write OSC 52 system clipboard",
            )?;
        }
        Ok(())
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

    fn log_bytes(&self, label: &str, bytes: &[u8]) {
        if self.log_enabled {
            if bytes.len() == 1
                && matches!(
                    label,
                    "parsed terminal event bytes"
                        | "dispatching decoded key to active view"
                        | "dispatching bytes to active view"
                )
            {
                return;
            }
            crate::diagnostics::bytes("app", label, bytes);
        }
    }

    fn log_event(&self, message: &str) {
        if self.log_enabled {
            crate::diagnostics::event("app", "event", message);
        }
    }

    pub fn wants_tick(&mut self) -> bool {
        let presented_transition_ready =
            self.pending_view_announcement && self.accessibility_announcement_ready();
        let pending_read_ready = self.pending_active_view_read.is_some()
            && self.pending_active_view_read == self.presented_accessibility_view
            && self.logical_accessibility_view_is_presented();
        presented_transition_ready
            || pending_read_ready
            || !self.pending_tmux_background_output.is_empty()
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
                .pending_force_abandon
                .as_ref()
                .is_some_and(|pending| pending.deadline_ms <= self.clock.now_ms())
            || self.tmux_connections.iter().any(|connection| {
                connection.pane_flow.values().any(|flow| {
                    !flow.resync_requested
                        && flow
                            .resync_after_ms
                            .is_some_and(|deadline| deadline <= self.clock.now_ms())
                })
            })
            || self
                .output_scheduler
                .as_ref()
                .and_then(OutputScheduler::next_deadline_ms)
                .is_some_and(|deadline| deadline <= self.clock.now_ms())
            || self
                .deferred_kitty_releases
                .iter()
                .any(|release| release.release_at_ms <= self.clock.now_ms())
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
            self.sync_tmux_panes(parent_connection_id)?;
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
        self.queue_tmux_gateway_path_resumes(connection_id);
        if let Some(connection) = self.view_stack.tmux_connection_mut(connection_id) {
            connection.show_connection();
        }
        self.sync_tmux_panes(connection_id)?;
        self.render_active_view(term_out)?;
        self.announce_view_change(sr)?;
        Ok(true)
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

    pub fn debug_tmux_pane_pending_update_batch_count(
        &mut self,
        connection_id: u64,
        pane_id: u64,
    ) -> Option<usize> {
        self.view_stack
            .tmux_connection_mut(connection_id)?
            .pane_pending_update_batch_count(crate::tmux_model::PaneId(pane_id))
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
        let (rows, cols) = self.view_stack.root_mut().model().live_size();
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
        let (rows, cols) = self.view_stack.root_mut().model().live_size();
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
        let (rows, cols) = self.view_stack.root_mut().model().live_size();
        self.handle_view_action(
            sr,
            views::ViewAction::Push(Box::new(views::PopupView::confirmation(
                rows, cols, title, message,
            ))),
            term_out,
        )
    }
}
