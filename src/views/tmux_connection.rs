use super::{Result, ViewAction, ViewController, ViewKind};
use crate::{
    presentation::{Scene, SurfaceId},
    screen_reader::ScreenReader,
    terminal::{TerminalGeometry, UpdateSummary},
    terminal_input::KeyInput,
    tmux_control::CommandStatus,
    tmux_model::{PaneId, TmuxTopology},
    tmux_panes::{BootstrapRequest, LayoutPane, TmuxLayout, TmuxPaneError, TmuxPaneSet},
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
    showing_portal: bool,
}

pub(crate) struct PaneOutput {
    pub update: UpdateSummary,
    pub replies: Vec<u8>,
    pub bells: usize,
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
            showing_portal: false,
        }
    }

    pub fn sync_topology(
        &mut self,
        topology: &TmuxTopology,
    ) -> std::result::Result<Vec<BootstrapRequest>, TmuxPaneError> {
        let requests = self.panes.reconcile(topology)?;
        self.topology.clone_from(topology);
        self.render_placeholder();
        Ok(requests)
    }

    pub(crate) fn process_output(
        &mut self,
        pane_id: PaneId,
        bytes: &[u8],
    ) -> std::result::Result<Option<PaneOutput>, TmuxPaneError> {
        let update_before = self.panes.pending_update(pane_id);
        let replies_before = update_before.map_or(0, |update| update.pty_replies.len());
        let bells_before = update_before.map_or(0, |update| update.effects.bells);
        let update = self.panes.process_output(pane_id, bytes)?;
        Ok(update.map(|update| PaneOutput {
            replies: update
                .pty_replies
                .get(replies_before..)
                .unwrap_or_default()
                .to_vec(),
            bells: update.effects.bells.saturating_sub(bells_before),
            update,
        }))
    }

    pub fn apply_bootstrap(
        &mut self,
        pane_id: PaneId,
        status: CommandStatus,
        output: &[Vec<u8>],
        now_ms: u128,
    ) -> std::result::Result<(), TmuxPaneError> {
        self.panes.apply_bootstrap(pane_id, status, output, now_ms)
    }

    pub fn composed_scene(
        &mut self,
        geometry: TerminalGeometry,
    ) -> std::result::Result<Scene, TmuxPaneError> {
        self.panes.compose(&self.topology, geometry)
    }

    pub(crate) fn resource_usage(
        &mut self,
    ) -> std::result::Result<crate::tmux_panes::TmuxResourceUsage, TmuxPaneError> {
        self.panes.resource_usage()
    }

    pub(crate) fn pane_capture_command(&self, pane_id: PaneId) -> Option<Vec<u8>> {
        self.topology
            .pane(pane_id)
            .map(crate::tmux_panes::capture_command)
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
    pub fn surface_id(&self, pane_id: PaneId) -> Option<SurfaceId> {
        self.panes.surface_id(pane_id)
    }

    #[must_use]
    pub fn connection_id(&self) -> u64 {
        self.connection_id
    }

    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.panes.all_bootstrapped()
            && active_layout(&self.topology).is_some_and(|layout| {
                !layout.panes().is_empty()
                    && layout.panes().iter().all(|pane| {
                        self.topology.pane(pane.pane_id).is_some()
                            && self.panes.pane_view(pane.pane_id).is_some()
                    })
                    && active_pane(&self.topology).is_some()
            })
    }

    #[must_use]
    pub fn is_showing_portal(&self) -> bool {
        self.showing_portal || self.active_pane_portal_target().is_some()
    }

    pub fn show_portal(&mut self) {
        self.showing_portal = true;
    }

    pub fn show_connection(&mut self) {
        self.showing_portal = false;
    }

    #[must_use]
    pub fn accessible_title(&self) -> String {
        if self.is_showing_portal() {
            return "tmux portal".to_owned();
        }
        let window_name = self
            .topology
            .attached_session()
            .and_then(|session_id| self.topology.session(session_id))
            .and_then(|session| session.active_window)
            .and_then(|window_id| self.topology.window(window_id))
            .map(|window| window.name.as_str())
            .unwrap_or("no active window");
        format!("tmux, {}, {window_name}", self.topology.label())
    }

    #[must_use]
    pub fn is_active_pane(&self, pane_id: PaneId) -> bool {
        active_pane(&self.topology) == Some(pane_id)
    }

    #[must_use]
    pub fn is_pane_visible(&self, pane_id: PaneId) -> bool {
        let Some(session) = self
            .topology
            .attached_session()
            .and_then(|id| self.topology.session(id))
        else {
            return false;
        };
        let Some(window) = session
            .active_window
            .and_then(|id| self.topology.window(id))
        else {
            return false;
        };
        let layout_text = if window.visible_layout.is_empty() {
            &window.layout
        } else {
            &window.visible_layout
        };
        TmuxLayout::parse(layout_text)
            .ok()
            .is_some_and(|layout| layout.pane(pane_id).is_some())
    }

    #[must_use]
    pub fn translate_mouse_input(&self, event: MouseEvent) -> Option<ViewAction> {
        if self.is_showing_portal() || !self.is_ready() {
            return None;
        }
        let pane = self.active_layout_pane()?;
        let screen = self.panes.pane_view(pane.pane_id)?.screen();
        let bytes = crate::tmux_input::translate_mouse(
            event,
            pane,
            screen.mouse_protocol_mode(),
            screen.mouse_protocol_encoding(),
        )?;
        Some(self.input_action(pane.pane_id, bytes))
    }

    fn active_layout_pane(&self) -> Option<LayoutPane> {
        let pane_id = active_pane(&self.topology)?;
        let session = self.topology.session(self.topology.attached_session()?)?;
        let window = self.topology.window(session.active_window?)?;
        let layout_text = if window.visible_layout.is_empty() {
            &window.layout
        } else {
            &window.visible_layout
        };
        TmuxLayout::parse(layout_text).ok()?.pane(pane_id).copied()
    }

    fn active_pane_portal_target(&self) -> Option<u64> {
        self.panes.pane_portal_target(active_pane(&self.topology)?)
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
        active_pane(&self.topology).map_or(ViewAction::None, |pane_id| {
            self.input_action(pane_id, bytes.to_vec())
        })
    }

    fn render_placeholder(&mut self) {
        let text = if let Some(session) = self
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
                Some(window) if active_pane(&self.topology).is_none() => format!(
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
            let pane_id = active_pane(&self.topology);
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
        let Some(pane_id) = active_pane(&self.topology) else {
            return Ok(ViewAction::None);
        };
        let Some(view) = self.panes.pane_view(pane_id) else {
            return Ok(ViewAction::None);
        };
        let mut bytes = Vec::with_capacity(contents.len().saturating_add(12));
        if view.screen().bracketed_paste() {
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

fn active_pane(topology: &TmuxTopology) -> Option<PaneId> {
    let session = topology.session(topology.attached_session()?)?;
    let window = topology.window(session.active_window?)?;
    let layout = active_layout(topology)?;
    window
        .active_pane
        .filter(|pane_id| layout.pane(*pane_id).is_some() && topology.pane(*pane_id).is_some())
        .or_else(|| {
            layout
                .panes()
                .iter()
                .map(|pane| pane.pane_id)
                .find(|pane_id| topology.pane(*pane_id).is_some())
        })
}

fn active_layout(topology: &TmuxTopology) -> Option<TmuxLayout> {
    let session = topology.session(topology.attached_session()?)?;
    let window = topology.window(session.active_window?)?;
    let layout_text = if window.visible_layout.is_empty() {
        &window.layout
    } else {
        &window.visible_layout
    };
    TmuxLayout::parse(layout_text).ok()
}

fn render_text(view: &mut View, text: &str) {
    view.clear_update_summary();
    let mut bytes = b"\x1b[2J\x1b[H".to_vec();
    bytes.extend_from_slice(text.as_bytes());
    view.process_changes(&bytes);
}
