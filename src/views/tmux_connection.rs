use super::{Result, ViewAction, ViewController, ViewKind};
use crate::{
    presentation::{PresentedFrameIndex, PresentedViewFrame, Scene, SurfaceId, ViewId},
    screen_reader::ScreenReader,
    terminal::{TerminalGeometry, UpdateSummary},
    terminal_input::KeyInput,
    tmux_control::CommandStatus,
    tmux_model::{PaneCaptureMetadata, PaneId, TmuxTopology},
    tmux_panes::{
        BootstrapRequest, LayoutError, LayoutPane, TmuxLayout, TmuxPaneError, TmuxPaneSet,
    },
    view::View,
};
use std::{any::Any, io::Write};
use terminput::MouseEvent;

const CONNECTION_TEXT: &str = "tmux connection is active.\r\n\
Waiting for tmux session and pane inventory.";
const PORTAL_TEXT: &str = "tmux control mode is running.\r\n\
Press Enter to switch to the active session in this connection.";

pub struct TmuxConnectionView {
    connection_id: u64,
    placeholder: View,
    portal: View,
    panes: TmuxPaneSet,
    topology: TmuxTopology,
    active_window: ActiveWindowProjection,
    showing_portal: bool,
    inventory_error: Option<String>,
}

pub(crate) struct PaneOutput {
    pub update: UpdateSummary,
    pub bells: usize,
}

/// Parsed, topology-validated data for the active tmux window. A topology sync
/// is the sole invalidation boundary, so output, input, composition, and
/// accessibility paths can share one layout parse without stale observations.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ActiveWindowProjection {
    MissingActiveWindow,
    MissingLayout,
    InvalidLayout(LayoutError),
    Ready(ActiveWindow),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActiveWindow {
    layout: TmuxLayout,
    active_pane: Option<PaneId>,
    title: String,
}

impl ActiveWindowProjection {
    fn from_topology(topology: &TmuxTopology) -> Self {
        let Some(session) = topology
            .attached_session()
            .and_then(|session_id| topology.session(session_id))
        else {
            return Self::MissingActiveWindow;
        };
        let Some(window) = session
            .active_window
            .and_then(|window_id| topology.window(window_id))
        else {
            return Self::MissingActiveWindow;
        };
        let layout_text = if window.visible_layout.is_empty() {
            &window.layout
        } else {
            &window.visible_layout
        };
        if layout_text.is_empty() {
            return Self::MissingLayout;
        }
        let layout = match TmuxLayout::parse(layout_text) {
            Ok(layout) => layout,
            Err(error) => return Self::InvalidLayout(error),
        };
        let active_pane = window
            .active_pane
            .filter(|pane_id| layout.pane(*pane_id).is_some() && topology.pane(*pane_id).is_some())
            .or_else(|| {
                layout
                    .panes()
                    .iter()
                    .map(|pane| pane.pane_id)
                    .find(|pane_id| topology.pane(*pane_id).is_some())
            });
        Self::Ready(ActiveWindow {
            layout,
            active_pane,
            title: window.name.clone(),
        })
    }

    fn ready(&self) -> Option<&ActiveWindow> {
        match self {
            Self::Ready(active) => Some(active),
            Self::MissingActiveWindow | Self::MissingLayout | Self::InvalidLayout(_) => None,
        }
    }

    fn active_pane(&self) -> Option<PaneId> {
        self.ready().and_then(|active| active.active_pane)
    }

    fn for_composition(&self) -> std::result::Result<&ActiveWindow, TmuxPaneError> {
        match self {
            Self::Ready(active) => Ok(active),
            Self::MissingActiveWindow => Err(TmuxPaneError::MissingActiveWindow),
            Self::MissingLayout => Err(TmuxPaneError::MissingLayout),
            Self::InvalidLayout(error) => Err(TmuxPaneError::Layout(error.clone())),
        }
    }
}

impl TmuxConnectionView {
    #[must_use]
    pub fn new(rows: u16, cols: u16, connection_id: u64) -> Self {
        let mut placeholder = View::new(rows, cols);
        render_text(&mut placeholder, CONNECTION_TEXT);
        let mut portal = View::new(rows, cols);
        render_text(&mut portal, PORTAL_TEXT);
        Self {
            connection_id,
            placeholder,
            portal,
            panes: TmuxPaneSet::new(connection_id),
            topology: TmuxTopology::new(connection_id),
            active_window: ActiveWindowProjection::MissingActiveWindow,
            showing_portal: false,
            inventory_error: None,
        }
    }

    pub fn sync_topology(
        &mut self,
        topology: &TmuxTopology,
    ) -> std::result::Result<Vec<BootstrapRequest>, TmuxPaneError> {
        let next_active_window = ActiveWindowProjection::from_topology(topology);
        let previous_active_pane = self.active_window.active_pane();
        let mut requests = self.panes.reconcile(topology)?;
        let active_pane = next_active_window.active_pane();
        let visible_layout = next_active_window.ready().map(|active| &active.layout);
        requests.sort_by_key(|request| {
            if Some(request.pane_id) == active_pane {
                0
            } else if visible_layout
                .as_ref()
                .is_some_and(|layout| layout.pane(request.pane_id).is_some())
            {
                1
            } else {
                2
            }
        });
        if previous_active_pane != active_pane {
            // A pane handoff is announced from the new pane's complete model.
            // Neither the old pane's unfinalized speech metadata nor metadata
            // retained from an earlier visit belongs to that announcement.
            self.panes.clear_update_summaries();
        }
        self.topology.clone_from(topology);
        self.active_window = next_active_window;
        self.inventory_error = None;
        self.render_placeholder();
        Ok(requests)
    }

    pub(crate) fn process_output(
        &mut self,
        pane_id: PaneId,
        bytes: &[u8],
        retain_for_accessibility: bool,
    ) -> std::result::Result<Option<PaneOutput>, TmuxPaneError> {
        let update = self.panes.process_output_with_summary_retention(
            pane_id,
            bytes,
            retain_for_accessibility,
        )?;
        Ok(update.map(|update| PaneOutput {
            bells: update.effects.bells,
            update,
        }))
    }

    pub fn apply_bootstrap(
        &mut self,
        pane_id: PaneId,
        status: CommandStatus,
        output: &[Vec<u8>],
        now_ms: u128,
    ) -> std::result::Result<Vec<u8>, TmuxPaneError> {
        self.panes.apply_bootstrap(pane_id, status, output, now_ms)
    }

    pub fn apply_bootstrap_with_line_flags(
        &mut self,
        pane_id: PaneId,
        status: CommandStatus,
        output: &[Vec<u8>],
        line_flags: bool,
        now_ms: u128,
    ) -> std::result::Result<Vec<u8>, TmuxPaneError> {
        self.panes
            .apply_bootstrap_with_line_flags(pane_id, status, output, line_flags, now_ms)
    }

    pub fn apply_resync_capture(
        &mut self,
        metadata: &PaneCaptureMetadata,
        output: &[Vec<u8>],
        pending_escape: &[u8],
        now_ms: u128,
    ) -> std::result::Result<(), TmuxPaneError> {
        self.topology
            .update_pane_capture_metadata(metadata)
            .map_err(|_| TmuxPaneError::UnknownPane(metadata.pane_id.0))?;
        self.panes
            .apply_resync_capture(metadata, output, pending_escape, now_ms)
    }

    pub fn apply_resync_capture_with_line_flags(
        &mut self,
        metadata: &PaneCaptureMetadata,
        output: &[Vec<u8>],
        pending_escape: &[u8],
        line_flags: bool,
        now_ms: u128,
    ) -> std::result::Result<(), TmuxPaneError> {
        self.topology
            .update_pane_capture_metadata(metadata)
            .map_err(|_| TmuxPaneError::UnknownPane(metadata.pane_id.0))?;
        self.panes.apply_resync_capture_with_line_flags(
            metadata,
            output,
            pending_escape,
            line_flags,
            now_ms,
        )
    }

    pub fn composed_scene(
        &mut self,
        geometry: TerminalGeometry,
    ) -> std::result::Result<Scene, TmuxPaneError> {
        let active = self.active_window.for_composition()?;
        self.panes
            .compose_layout(&active.layout, active.active_pane, &active.title, geometry)
    }

    pub(crate) fn composed_committed_scene(
        &mut self,
        geometry: TerminalGeometry,
    ) -> std::result::Result<Scene, TmuxPaneError> {
        let active = self.active_window.for_composition()?;
        self.panes.compose_committed_layout(
            &active.layout,
            active.active_pane,
            &active.title,
            geometry,
        )
    }

    pub(crate) fn visible_holds_synchronized_output(&self) -> bool {
        !self.showing_portal
            && self
                .active_window
                .ready()
                .is_some_and(|active| self.panes.visible_holds_synchronized_output(&active.layout))
    }

    pub(crate) fn resource_usage(
        &mut self,
    ) -> std::result::Result<crate::tmux_panes::TmuxResourceUsage, TmuxPaneError> {
        self.panes.resource_usage()
    }

    pub(crate) fn set_pane_portal(
        &mut self,
        pane_id: PaneId,
        child_connection_id: u64,
    ) -> std::result::Result<(), TmuxPaneError> {
        self.panes.set_pane_portal(pane_id, child_connection_id)
    }

    pub(crate) fn clear_pane_portal(&mut self, pane_id: PaneId, child_connection_id: u64) {
        self.panes.clear_pane_portal(pane_id, child_connection_id);
    }

    #[must_use]
    pub(crate) fn pane_portal_target(&self, pane_id: PaneId) -> Option<u64> {
        self.panes.pane_portal_target(pane_id)
    }

    #[must_use]
    pub(crate) fn pane_contents(&self, pane_id: PaneId) -> Option<String> {
        self.panes
            .pane_view(pane_id)
            .map(|view| view.screen().contents_full())
    }

    #[must_use]
    pub(crate) fn pane_pending_update_batch_count(&self, pane_id: PaneId) -> Option<usize> {
        self.panes
            .pending_update(pane_id)
            .map(|update| update.batch_count)
    }

    #[must_use]
    pub fn surface_id(&self, pane_id: PaneId) -> Option<SurfaceId> {
        self.panes.surface_id(pane_id)
    }

    pub(crate) fn capture_live_presentation_frames(
        &mut self,
    ) -> (Option<ViewId>, Vec<PresentedViewFrame>) {
        let Some(active) = self.active_window.ready() else {
            return (Some(self.model().view_id()), Vec::new());
        };
        self.panes
            .capture_live_presentation_frames(&active.layout, active.active_pane)
    }

    pub(crate) fn capture_committed_presentation_frames(
        &mut self,
    ) -> (Option<ViewId>, Vec<PresentedViewFrame>) {
        let Some(active) = self.active_window.ready() else {
            return (Some(self.model().view_id()), Vec::new());
        };
        self.panes
            .capture_committed_presentation_frames(&active.layout, active.active_pane)
    }

    pub(crate) fn apply_presented_frame(&mut self, frame: &PresentedViewFrame) -> bool {
        self.placeholder.apply_presented_frame_ref(frame)
            || self.portal.apply_presented_frame_ref(frame)
            || self.panes.apply_presented_frame(frame)
    }

    pub(crate) fn apply_presented_frames(&mut self, frames: &PresentedFrameIndex<'_>) -> usize {
        let mut applied = 0usize;
        for view in [&mut self.placeholder, &mut self.portal] {
            if let Some(frame) = frames.get(view.view_id())
                && view.apply_presented_frame_ref(frame)
            {
                applied = applied.saturating_add(1);
            }
        }
        applied.saturating_add(self.panes.apply_presented_frames(frames))
    }

    pub(crate) fn model_by_id_mut(&mut self, view_id: ViewId) -> Option<&mut View> {
        if self.placeholder.view_id() == view_id {
            return Some(&mut self.placeholder);
        }
        if self.portal.view_id() == view_id {
            return Some(&mut self.portal);
        }
        self.panes.model_by_id_mut(view_id)
    }

    pub(crate) fn retain_accessibility_views(&mut self, retained: &[ViewId]) {
        self.panes.retain_accessibility_views(retained);
    }

    #[must_use]
    pub fn connection_id(&self) -> u64 {
        self.connection_id
    }

    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.active_window.ready().is_some_and(|active| {
            !active.layout.panes().is_empty()
                && active.layout.panes().iter().all(|pane| {
                    self.topology.pane(pane.pane_id).is_some()
                        && self.panes.pane_is_bootstrapped(pane.pane_id)
                })
                && active.active_pane.is_some()
        })
    }

    #[must_use]
    pub fn is_showing_portal(&self) -> bool {
        self.showing_portal || self.active_pane_portal_target().is_some()
    }

    /// A connection-level portal replaces the entire connection. A pane
    /// portal replaces only that pane, so the parent tmux key tables remain
    /// active for changing panes, windows, sessions, and running commands.
    #[must_use]
    pub fn is_showing_connection_portal(&self) -> bool {
        self.showing_portal
    }

    #[must_use]
    pub(crate) fn active_input_pane(&self) -> Option<PaneId> {
        if self.showing_portal || !self.is_ready() || self.active_pane_portal_target().is_some() {
            return None;
        }
        self.active_window.active_pane()
    }

    pub fn set_inventory_error(&mut self, detail: &str) {
        self.inventory_error = Some(detail.to_owned());
        self.render_placeholder();
    }

    pub fn show_portal(&mut self) {
        self.panes.clear_update_summaries();
        self.showing_portal = true;
    }

    pub fn show_connection(&mut self) {
        // Re-entering a connection is a full accessibility handoff even when
        // its tmux topology did not change while it was in the background.
        self.panes.clear_update_summaries();
        self.showing_portal = false;
    }

    #[must_use]
    pub fn accessible_title(&self) -> String {
        if self.is_showing_portal() {
            return "tmux portal".to_owned();
        }
        self.topology.attached_location().map_or_else(
            || format!("tmux, {}, no active window", self.topology.label()),
            |location| location.accessible_title(),
        )
    }

    #[must_use]
    pub fn is_active_pane(&self, pane_id: PaneId) -> bool {
        self.active_window.active_pane() == Some(pane_id)
    }

    #[must_use]
    pub fn is_pane_visible(&self, pane_id: PaneId) -> bool {
        self.active_window
            .ready()
            .is_some_and(|active| active.layout.pane(pane_id).is_some())
    }

    #[must_use]
    pub fn translate_mouse_input(&self, event: MouseEvent) -> Option<ViewAction> {
        if self.is_showing_portal() || !self.is_ready() {
            return None;
        }
        let pane = self.active_layout_pane()?;
        let screen = self.panes.pane_view(pane.pane_id)?.live_screen();
        let bytes = crate::tmux_input::translate_mouse(
            event,
            pane,
            screen.mouse_protocol_mode(),
            screen.mouse_protocol_encoding(),
        )?;
        Some(self.input_action(pane.pane_id, bytes))
    }

    fn active_layout_pane(&self) -> Option<LayoutPane> {
        let active = self.active_window.ready()?;
        active.layout.pane(active.active_pane?).copied()
    }

    fn active_pane_portal_target(&self) -> Option<u64> {
        self.panes
            .pane_portal_target(self.active_window.active_pane()?)
    }

    fn input_action(&self, pane_id: PaneId, bytes: Vec<u8>) -> ViewAction {
        ViewAction::TmuxInput {
            connection_id: self.connection_id,
            pane_id,
            bytes,
        }
    }

    fn active_input_action(&self, bytes: &[u8]) -> ViewAction {
        if self.is_showing_portal() || !self.is_ready() || bytes.is_empty() {
            return ViewAction::None;
        }
        self.active_window
            .active_pane()
            .map_or(ViewAction::None, |pane_id| {
                self.input_action(pane_id, bytes.to_vec())
            })
    }

    fn render_placeholder(&mut self) {
        let text = if let Some(detail) = &self.inventory_error {
            format!(
                "tmux connection could not become ready.\r\n{detail}\r\nUse Lector's connection commands to switch away or close this connection."
            )
        } else if let Some(session) = self
            .topology
            .attached_session()
            .and_then(|session_id| self.topology.session(session_id))
        {
            match session
                .active_window
                .and_then(|window_id| self.topology.window(window_id))
            {
                None => format!(
                    "tmux connection is active.\r\nsession ${} {} has no active window.\r\nWaiting for tmux to create or select a window.",
                    session.id.0, session.name
                ),
                Some(window) if self.active_window.active_pane().is_none() => format!(
                    "tmux connection is active.\r\nwindow @{} {} in session ${} {} has no active pane.\r\nWaiting for tmux pane inventory.",
                    window.id.0, window.name, session.id.0, session.name
                ),
                Some(window) => format!(
                    "tmux connection is active.\r\nwindow @{} {} in session ${} {} is becoming ready.",
                    window.id.0, window.name, session.id.0, session.name
                ),
            }
        } else {
            CONNECTION_TEXT.to_owned()
        };
        render_text(&mut self.placeholder, &text);
    }
}

impl ViewController for TmuxConnectionView {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn model(&mut self) -> &mut View {
        if self.showing_portal {
            return &mut self.portal;
        }
        if self.is_ready() {
            let pane_id = self.active_window.active_pane();
            if let Some(pane_id) = pane_id {
                if self.panes.pane_portal_target(pane_id).is_some() {
                    return self
                        .panes
                        .pane_portal_view_mut(pane_id)
                        .expect("a pane portal target must own a portal view");
                }
                if let Some(view) = self.panes.pane_view_mut(pane_id) {
                    return view;
                }
            }
        }
        &mut self.placeholder
    }

    fn set_virtual_terminal_colors(
        &mut self,
        colors: crate::terminal_protocol::VirtualTerminalColors,
    ) {
        self.placeholder.set_virtual_terminal_colors(colors);
        self.portal.set_virtual_terminal_colors(colors);
        self.panes.set_virtual_terminal_colors(colors);
    }

    fn enable_presentation_tracking(&mut self) {
        self.placeholder.enable_presentation_tracking();
        self.portal.enable_presentation_tracking();
        self.panes.enable_presentation_tracking();
    }

    fn title(&self) -> &str {
        if self.is_showing_portal() {
            "tmux portal"
        } else {
            "tmux"
        }
    }

    fn kind(&self) -> ViewKind {
        if self.is_showing_portal() {
            ViewKind::TmuxPortal
        } else {
            ViewKind::TmuxConnection
        }
    }

    fn handle_input(
        &mut self,
        _sr: &mut ScreenReader,
        input: &[u8],
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        if self.showing_portal && matches!(input, b"\r" | b"\n") {
            Ok(ViewAction::ActivateTmuxConnection(self.connection_id))
        } else if let Some(connection_id) = self.active_pane_portal_target() {
            Ok(if matches!(input, b"\r" | b"\n") {
                ViewAction::ActivateTmuxConnection(connection_id)
            } else {
                ViewAction::None
            })
        } else {
            Ok(self.active_input_action(input))
        }
    }

    fn handle_key_input(
        &mut self,
        _sr: &mut ScreenReader,
        key: &KeyInput,
        raw: &[u8],
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        if self.showing_portal
            && !key.is_release()
            && (matches!(key.control_code(), Some(b'\n' | b'\r'))
                || key.event().code == terminput::KeyCode::Enter)
        {
            Ok(ViewAction::ActivateTmuxConnection(self.connection_id))
        } else if let Some(connection_id) = self.active_pane_portal_target() {
            Ok(
                if !key.is_release()
                    && (matches!(key.control_code(), Some(b'\n' | b'\r'))
                        || key.event().code == terminput::KeyCode::Enter)
                {
                    ViewAction::ActivateTmuxConnection(connection_id)
                } else {
                    ViewAction::None
                },
            )
        } else {
            Ok(self.active_input_action(raw))
        }
    }

    fn handle_paste(
        &mut self,
        sr: &mut ScreenReader,
        contents: &str,
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        if self.is_showing_portal() || !self.is_ready() {
            return Ok(ViewAction::None);
        }
        let Some(pane_id) = self.active_window.active_pane() else {
            return Ok(ViewAction::None);
        };
        let Some(view) = self.panes.pane_view(pane_id) else {
            return Ok(ViewAction::None);
        };
        let mut bytes = Vec::with_capacity(contents.len().saturating_add(12));
        if view.live_screen().bracketed_paste() {
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(contents.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
        } else {
            bytes.extend_from_slice(contents.as_bytes());
        }
        sr.speak("pasted", false)?;
        Ok(self.input_action(pane_id, bytes))
    }

    fn on_resize(&mut self, rows: u16, cols: u16) {
        self.placeholder.set_size(rows, cols);
        self.render_placeholder();
        self.portal.set_size(rows, cols);
        render_text(&mut self.portal, PORTAL_TEXT);
    }
}

fn render_text(view: &mut View, text: &str) {
    view.clear_update_summary();
    let mut bytes = b"\x1b[2J\x1b[H".to_vec();
    bytes.extend_from_slice(text.as_bytes());
    view.process_changes(&bytes);
}

#[cfg(test)]
mod tests {
    use super::TmuxConnectionView;
    use crate::{
        presentation::CursorOwner,
        terminal::TerminalGeometry,
        tmux_control::CommandStatus,
        tmux_model::{PaneId, TmuxTopology},
    };

    const SPLIT_LAYOUT: &str = "abcd,20x4,0,0{10x4,0,0,20,9x4,11,0,21}";

    fn topology_with_layout(
        layout: &str,
        visible_layout: &str,
        active_pane: Option<PaneId>,
        title: &str,
    ) -> TmuxTopology {
        let lines = [
            b"S\t$1\twork".to_vec(),
            format!("W\t$1\t@10\t1\t1\t{layout}\t{visible_layout}\t*\t{title}").into_bytes(),
            format!(
                "P\t@10\t%20\t1\t{}\t0\t0\t10\t4\t0\t0\t0\t1\t0\t0\t0\t0\tleft",
                usize::from(active_pane == Some(PaneId(20)))
            )
            .into_bytes(),
            format!(
                "P\t@10\t%21\t2\t{}\t11\t0\t9\t4\t0\t0\t0\t1\t0\t0\t0\t0\tright",
                usize::from(active_pane == Some(PaneId(21)))
            )
            .into_bytes(),
            b"A\t$1".to_vec(),
        ];
        let mut topology = TmuxTopology::new(1);
        topology.replace_inventory(&lines).expect("topology");
        topology
    }

    fn ready_connection() -> TmuxConnectionView {
        let topology = topology_with_layout(SPLIT_LAYOUT, SPLIT_LAYOUT, Some(PaneId(20)), "editor");
        let mut connection = TmuxConnectionView::new(4, 20, 1);
        for request in connection.sync_topology(&topology).expect("sync topology") {
            connection
                .apply_bootstrap(
                    request.pane_id,
                    CommandStatus::Success,
                    &[format!("pane {}", request.pane_id.0).into_bytes()],
                    0,
                )
                .expect("bootstrap");
        }
        connection
    }

    fn topology_with_two_windows(active_window: u64) -> TmuxTopology {
        let lines = [
            b"S\t$1\twork".to_vec(),
            format!(
                "W\t$1\t@10\t1\t{}\taaaa,20x4,0,0,20\taaaa,20x4,0,0,20\t{}\tleft",
                usize::from(active_window == 10),
                if active_window == 10 { "*" } else { "" }
            )
            .into_bytes(),
            format!(
                "W\t$1\t@11\t2\t{}\tbbbb,20x4,0,0,30\tbbbb,20x4,0,0,30\t{}\tright",
                usize::from(active_window == 11),
                if active_window == 11 { "*" } else { "" }
            )
            .into_bytes(),
            b"P\t@10\t%20\t1\t1\t0\t0\t20\t4\t0\t0\t0\t1\t0\t0\t0\t0\t0\tleft-pane".to_vec(),
            b"P\t@11\t%30\t1\t1\t0\t0\t20\t4\t0\t0\t0\t1\t0\t0\t0\t0\t0\tright-pane".to_vec(),
            b"A\t$1".to_vec(),
        ];
        let mut topology = TmuxTopology::new(1);
        topology.replace_inventory(&lines).expect("topology");
        topology
    }

    fn ready_two_window_connection() -> TmuxConnectionView {
        let topology = topology_with_two_windows(10);
        let mut connection = TmuxConnectionView::new(4, 20, 1);
        for request in connection.sync_topology(&topology).expect("sync topology") {
            connection
                .apply_bootstrap(
                    request.pane_id,
                    CommandStatus::Success,
                    &[format!("pane {}", request.pane_id.0).into_bytes()],
                    0,
                )
                .expect("bootstrap");
        }
        connection
    }

    fn topology_with_first_window_linked_into_another_session(index: u32) -> TmuxTopology {
        let lines = [
            b"S\t$1\twork".to_vec(),
            b"S\t$2\tlinked".to_vec(),
            b"W\t$1\t@10\t7\t0\taaaa,20x4,0,0,20\taaaa,20x4,0,0,20\t-\tleft".to_vec(),
            format!("W\t$2\t@10\t{index}\t1\taaaa,20x4,0,0,20\taaaa,20x4,0,0,20\t*\tleft")
                .into_bytes(),
            b"W\t$1\t@11\t8\t1\tbbbb,20x4,0,0,30\tbbbb,20x4,0,0,30\t*\tright".to_vec(),
            b"P\t@10\t%20\t1\t1\t0\t0\t20\t4\t0\t0\t0\t1\t0\t0\t0\t0\t0\tleft-pane".to_vec(),
            b"P\t@11\t%30\t1\t1\t0\t0\t20\t4\t0\t0\t0\t1\t0\t0\t0\t0\t0\tright-pane".to_vec(),
            b"A\t$2".to_vec(),
        ];
        let mut topology = TmuxTopology::new(1);
        topology.replace_inventory(&lines).expect("linked topology");
        topology
    }

    #[test]
    fn switching_away_and_back_drops_stale_active_update_metadata() {
        let mut connection = ready_connection();
        connection
            .process_output(PaneId(20), b"old", true)
            .expect("old output");
        assert_eq!(
            connection
                .panes
                .pending_update(PaneId(20))
                .unwrap()
                .printed_text(),
            "old"
        );

        connection.show_portal();
        connection.show_connection();
        assert_eq!(
            connection
                .panes
                .pending_update(PaneId(20))
                .unwrap()
                .batch_count,
            0
        );

        let fresh = connection
            .process_output(PaneId(20), b"new", true)
            .expect("fresh output")
            .expect("bootstrapped pane update");
        assert_eq!(fresh.update.printed_text(), "new");
        assert_eq!(
            connection
                .panes
                .pending_update(PaneId(20))
                .unwrap()
                .printed_text(),
            "new"
        );
    }

    #[test]
    fn cached_active_window_matches_direct_composition_and_invalidates_on_sync() {
        let mut connection = ready_connection();
        let geometry = TerminalGeometry::from_cells(4, 20);
        let direct = connection
            .panes
            .compose(&connection.topology, geometry)
            .expect("direct split composition");
        let cached = connection
            .composed_scene(geometry)
            .expect("cached split composition");
        assert_eq!(cached, direct);
        assert!(connection.is_ready());
        assert!(connection.is_active_pane(PaneId(20)));
        assert!(connection.is_pane_visible(PaneId(20)));
        assert!(connection.is_pane_visible(PaneId(21)));

        let zoomed_layout = "beef,20x4,0,0,21";
        let replacement =
            topology_with_layout(SPLIT_LAYOUT, zoomed_layout, Some(PaneId(21)), "zoomed");
        assert!(
            connection
                .sync_topology(&replacement)
                .expect("sync replacement topology")
                .is_empty(),
            "surviving panes should not need another bootstrap"
        );

        assert!(connection.is_ready());
        assert!(connection.is_active_pane(PaneId(21)));
        assert!(!connection.is_pane_visible(PaneId(20)));
        assert!(connection.is_pane_visible(PaneId(21)));
        assert_eq!(connection.active_input_pane(), Some(PaneId(21)));
        let direct = connection
            .panes
            .compose(&connection.topology, geometry)
            .expect("direct zoomed composition");
        let cached = connection
            .composed_scene(geometry)
            .expect("cached zoomed composition");
        assert_eq!(cached, direct);
        assert_eq!(cached.effects.title.as_deref(), Some("zoomed"));
        assert_eq!(cached.panes.len(), 2, "border plus one visible pane");
        assert_eq!(
            cached.cursor_owner,
            CursorOwner::Pane(connection.surface_id(PaneId(21)).expect("right surface"))
        );
    }

    #[test]
    fn cached_active_window_preserves_composition_error_parity() {
        let geometry = TerminalGeometry::from_cells(4, 20);
        let cases = [
            topology_with_layout("", "", Some(PaneId(20)), "missing"),
            topology_with_layout("not-a-layout", "not-a-layout", Some(PaneId(20)), "invalid"),
            TmuxTopology::new(1),
        ];

        for topology in cases {
            let mut connection = ready_connection();
            connection
                .sync_topology(&topology)
                .expect("sync non-renderable topology");
            assert!(!connection.is_ready());
            assert!(!connection.is_pane_visible(PaneId(20)));
            assert_eq!(connection.active_input_pane(), None);

            let direct = connection
                .panes
                .compose(&connection.topology, geometry)
                .expect_err("direct composition should fail")
                .to_string();
            let cached = connection
                .composed_scene(geometry)
                .expect_err("cached composition should fail")
                .to_string();
            assert_eq!(cached, direct);
        }
    }

    #[test]
    fn active_panes_restore_their_independent_review_cursors() {
        let mut connection = ready_connection();
        let left = connection
            .panes
            .pane_view_mut(PaneId(20))
            .expect("left pane");
        left.prepare_review_cursor_for_activation();
        left.set_review_cursor_position((0, 1));

        let right_active =
            topology_with_layout(SPLIT_LAYOUT, SPLIT_LAYOUT, Some(PaneId(21)), "right");
        connection
            .sync_topology(&right_active)
            .expect("activate right pane");
        let right = connection
            .panes
            .pane_view_mut(PaneId(21))
            .expect("right pane");
        right.prepare_review_cursor_for_activation();
        right.set_review_cursor_position((0, 2));

        let left_active =
            topology_with_layout(SPLIT_LAYOUT, SPLIT_LAYOUT, Some(PaneId(20)), "left");
        connection
            .sync_topology(&left_active)
            .expect("reactivate left pane");
        let left = connection
            .panes
            .pane_view_mut(PaneId(20))
            .expect("left pane");
        left.prepare_review_cursor_for_activation();
        assert_eq!(left.review_cursor_position(), (0, 1));

        let right = connection
            .panes
            .pane_view_mut(PaneId(21))
            .expect("right pane");
        assert_eq!(right.review_cursor_position(), (0, 2));
    }

    #[test]
    fn active_tmux_windows_restore_their_independent_review_cursors() {
        let mut connection = ready_two_window_connection();
        let first = connection
            .panes
            .pane_view_mut(PaneId(20))
            .expect("first window pane");
        first.prepare_review_cursor_for_activation();
        first.set_review_cursor_position((0, 1));

        connection
            .sync_topology(&topology_with_two_windows(11))
            .expect("activate second window");
        let second = connection
            .panes
            .pane_view_mut(PaneId(30))
            .expect("second window pane");
        second.prepare_review_cursor_for_activation();
        second.set_review_cursor_position((0, 2));

        connection
            .sync_topology(&topology_with_two_windows(10))
            .expect("reactivate first window");
        let first = connection
            .panes
            .pane_view_mut(PaneId(20))
            .expect("first window pane");
        first.prepare_review_cursor_for_activation();
        assert_eq!(first.review_cursor_position(), (0, 1));

        let second = connection
            .panes
            .pane_view_mut(PaneId(30))
            .expect("second window pane");
        assert_eq!(second.review_cursor_position(), (0, 2));
    }

    #[test]
    fn relinking_and_moving_a_window_preserves_its_single_pane_view_object() {
        let mut connection = ready_two_window_connection();
        let surface_id = connection.surface_id(PaneId(20)).expect("first surface");
        let pane = connection
            .panes
            .pane_view_mut(PaneId(20))
            .expect("first window pane");
        pane.prepare_review_cursor_for_activation();
        pane.set_review_cursor_position((1, 0));
        pane.process_changes(b"\x1b_Lector;A11y;1;set;auto=0;cursor=0\x1b\\");
        assert!(
            pane.application_accessibility_policy()
                .suppress_cursor_tracking
        );

        for index in [99, 3, 1_000_000] {
            let topology = topology_with_first_window_linked_into_another_session(index);
            assert!(
                connection
                    .sync_topology(&topology)
                    .expect("relink stable window")
                    .is_empty(),
                "a surviving %pane ID was rebuilt after moving its @window"
            );
            assert_eq!(connection.surface_id(PaneId(20)), Some(surface_id));
            let pane = connection
                .panes
                .pane_view_mut(PaneId(20))
                .expect("same linked window pane");
            assert_eq!(pane.review_cursor_position(), (1, 0));
            let policy = pane.application_accessibility_policy();
            assert!(policy.suppress_auto_read && policy.suppress_cursor_tracking);
        }
    }
}
