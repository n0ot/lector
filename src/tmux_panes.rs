//! Pane-local terminal engines, tmux layout parsing, and scene composition.

use crate::{
    presentation::{
        CursorOwner, GridPoint, GridRect, PresentationError, Scene, SceneSurface, SurfaceId,
    },
    terminal::{Cell, Cursor, Row, TerminalGeometry, TerminalSnapshot, UpdateSummary},
    tmux_control::CommandStatus,
    tmux_model::{Pane, PaneId, TmuxTopology},
    view::View,
};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

const MAX_LAYOUT_DEPTH: usize = 64;
const MAX_LAYOUT_NODES: usize = 4096;
const MAX_PREBOOTSTRAP_BYTES: usize = 4 * 1024 * 1024;
const MAX_ORPHAN_PANES: usize = 4096;
const PANE_PORTAL_TEXT: &str = "tmux control mode is running.\r\n\
Press Enter to switch to the nested session in this pane.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LayoutPane {
    pub pane_id: PaneId,
    pub origin: GridPoint,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SplitKind {
    LeftRight,
    TopBottom,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Divider {
    kind: SplitKind,
    origin: GridPoint,
    length: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TmuxLayout {
    panes: Vec<LayoutPane>,
    dividers: Vec<Divider>,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum LayoutError {
    #[error("malformed tmux layout")]
    Malformed,
    #[error("tmux layout nesting or node count exceeded its bound")]
    TooComplex,
    #[error("tmux layout contains duplicate pane %{0}")]
    DuplicatePane(u64),
    #[error("tmux layout child lies outside its parent")]
    ChildOutsideParent,
    #[error("tmux layout children do not partition their parent")]
    InvalidPartition,
}

impl TmuxLayout {
    pub fn parse(layout: &str) -> Result<Self, LayoutError> {
        let (checksum, body) = layout.split_once(',').ok_or(LayoutError::Malformed)?;
        if checksum.len() != 4 || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(LayoutError::Malformed);
        }
        let mut parser = LayoutParser {
            bytes: body.as_bytes(),
            offset: 0,
            nodes: 0,
            panes: Vec::new(),
            dividers: Vec::new(),
        };
        parser.parse_node(0, None)?;
        if parser.offset != parser.bytes.len() || parser.panes.is_empty() {
            return Err(LayoutError::Malformed);
        }
        let mut ids = BTreeSet::new();
        for pane in &parser.panes {
            if !ids.insert(pane.pane_id) {
                return Err(LayoutError::DuplicatePane(pane.pane_id.0));
            }
        }
        Ok(Self {
            panes: parser.panes,
            dividers: parser.dividers,
        })
    }

    #[must_use]
    pub fn panes(&self) -> &[LayoutPane] {
        &self.panes
    }

    #[must_use]
    pub fn pane(&self, pane_id: PaneId) -> Option<&LayoutPane> {
        self.panes.iter().find(|pane| pane.pane_id == pane_id)
    }

    #[must_use]
    pub fn border_snapshot(&self, geometry: TerminalGeometry) -> TerminalSnapshot {
        const NORTH: u8 = 1;
        const SOUTH: u8 = 2;
        const WEST: u8 = 4;
        const EAST: u8 = 8;

        let mut masks = vec![vec![0_u8; usize::from(geometry.cols)]; usize::from(geometry.rows)];
        for divider in &self.dividers {
            for offset in 0..divider.length {
                let (row, col, bits) = match divider.kind {
                    SplitKind::LeftRight => (
                        divider.origin.row.saturating_add(i32::from(offset)),
                        divider.origin.col,
                        NORTH | SOUTH,
                    ),
                    SplitKind::TopBottom => (
                        divider.origin.row,
                        divider.origin.col.saturating_add(i32::from(offset)),
                        WEST | EAST,
                    ),
                };
                if row >= 0
                    && col >= 0
                    && row < i32::from(geometry.rows)
                    && col < i32::from(geometry.cols)
                {
                    masks[usize::try_from(row).expect("non-negative row")]
                        [usize::try_from(col).expect("non-negative column")] |= bits;
                }
            }
        }

        let occupied = masks
            .iter()
            .map(|row| row.iter().map(|mask| *mask != 0).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        for row in 0..usize::from(geometry.rows) {
            for col in 0..usize::from(geometry.cols) {
                if !occupied[row][col] {
                    continue;
                }
                if row > 0 && occupied[row - 1][col] {
                    masks[row][col] |= NORTH;
                }
                if row + 1 < occupied.len() && occupied[row + 1][col] {
                    masks[row][col] |= SOUTH;
                }
                if col > 0 && occupied[row][col - 1] {
                    masks[row][col] |= WEST;
                }
                if col + 1 < occupied[row].len() && occupied[row][col + 1] {
                    masks[row][col] |= EAST;
                }
            }
        }

        let rows = masks
            .into_iter()
            .map(|masks| Row {
                cells: masks
                    .into_iter()
                    .map(|mask| Cell {
                        grapheme: border_character(mask).to_owned(),
                        ..Cell::default()
                    })
                    .collect(),
                wrapped: false,
            })
            .collect();
        TerminalSnapshot {
            rows,
            cursor: Cursor {
                visible: false,
                ..Cursor::default()
            },
            geometry,
            ..TerminalSnapshot::default()
        }
    }
}

fn border_character(mask: u8) -> &'static str {
    match mask {
        0 => "",
        1..=3 => "│",
        4 | 8 | 12 => "─",
        10 => "┌",
        6 => "┐",
        9 => "└",
        5 => "┘",
        14 => "┬",
        13 => "┴",
        11 => "├",
        7 => "┤",
        15 => "┼",
        _ => "┼",
    }
}

#[derive(Clone, Copy, Debug)]
struct NodeRect {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

impl NodeRect {
    fn contains(self, child: Self) -> bool {
        u32::from(child.x) >= u32::from(self.x)
            && u32::from(child.y) >= u32::from(self.y)
            && u32::from(child.x) + u32::from(child.width)
                <= u32::from(self.x) + u32::from(self.width)
            && u32::from(child.y) + u32::from(child.height)
                <= u32::from(self.y) + u32::from(self.height)
    }
}

struct LayoutParser<'a> {
    bytes: &'a [u8],
    offset: usize,
    nodes: usize,
    panes: Vec<LayoutPane>,
    dividers: Vec<Divider>,
}

impl LayoutParser<'_> {
    fn parse_node(
        &mut self,
        depth: usize,
        parent: Option<NodeRect>,
    ) -> Result<NodeRect, LayoutError> {
        if depth > MAX_LAYOUT_DEPTH || self.nodes == MAX_LAYOUT_NODES {
            return Err(LayoutError::TooComplex);
        }
        self.nodes += 1;
        let width = self.number_u16()?;
        self.expect(b'x')?;
        let height = self.number_u16()?;
        self.expect(b',')?;
        let x = self.number_u16()?;
        self.expect(b',')?;
        let y = self.number_u16()?;
        if width == 0 || height == 0 {
            return Err(LayoutError::Malformed);
        }
        let rect = NodeRect {
            x,
            y,
            width,
            height,
        };
        if parent.is_some_and(|parent| !parent.contains(rect)) {
            return Err(LayoutError::ChildOutsideParent);
        }

        match self.peek() {
            Some(b'{') => self.parse_children(depth, rect, SplitKind::LeftRight, b'}')?,
            Some(b'[') => self.parse_children(depth, rect, SplitKind::TopBottom, b']')?,
            Some(b',') => {
                self.offset += 1;
                let pane_id = self.number_u64()?;
                self.panes.push(LayoutPane {
                    pane_id: PaneId(pane_id),
                    origin: GridPoint::new(i32::from(y), i32::from(x)),
                    rows: height,
                    cols: width,
                });
            }
            _ => return Err(LayoutError::Malformed),
        }
        Ok(rect)
    }

    fn parse_children(
        &mut self,
        depth: usize,
        rect: NodeRect,
        kind: SplitKind,
        close: u8,
    ) -> Result<(), LayoutError> {
        self.offset += 1;
        let mut children = Vec::new();
        loop {
            children.push(self.parse_node(depth + 1, Some(rect))?);
            match self.peek() {
                Some(byte) if byte == close => {
                    self.offset += 1;
                    break;
                }
                Some(b',') => self.offset += 1,
                _ => return Err(LayoutError::Malformed),
            }
        }
        if children.len() < 2 {
            return Err(LayoutError::Malformed);
        }
        let valid_partition = match kind {
            SplitKind::LeftRight => {
                children.first().is_some_and(|child| {
                    child.x == rect.x && child.y == rect.y && child.height == rect.height
                }) && children.last().is_some_and(|child| {
                    u32::from(child.x) + u32::from(child.width)
                        == u32::from(rect.x) + u32::from(rect.width)
                }) && children.windows(2).all(|pair| {
                    pair[1].y == rect.y
                        && pair[1].height == rect.height
                        && u32::from(pair[0].x) + u32::from(pair[0].width) + 1
                            == u32::from(pair[1].x)
                })
            }
            SplitKind::TopBottom => {
                children.first().is_some_and(|child| {
                    child.x == rect.x && child.y == rect.y && child.width == rect.width
                }) && children.last().is_some_and(|child| {
                    u32::from(child.y) + u32::from(child.height)
                        == u32::from(rect.y) + u32::from(rect.height)
                }) && children.windows(2).all(|pair| {
                    pair[1].x == rect.x
                        && pair[1].width == rect.width
                        && u32::from(pair[0].y) + u32::from(pair[0].height) + 1
                            == u32::from(pair[1].y)
                })
            }
        };
        if !valid_partition {
            return Err(LayoutError::InvalidPartition);
        }
        for pair in children.windows(2) {
            let next = pair[1];
            let divider = match kind {
                SplitKind::LeftRight => Divider {
                    kind,
                    origin: GridPoint::new(i32::from(rect.y), i32::from(next.x.saturating_sub(1))),
                    length: rect.height,
                },
                SplitKind::TopBottom => Divider {
                    kind,
                    origin: GridPoint::new(i32::from(next.y.saturating_sub(1)), i32::from(rect.x)),
                    length: rect.width,
                },
            };
            self.dividers.push(divider);
        }
        Ok(())
    }

    fn number_u16(&mut self) -> Result<u16, LayoutError> {
        self.number_u64()?
            .try_into()
            .map_err(|_| LayoutError::Malformed)
    }

    fn number_u64(&mut self) -> Result<u64, LayoutError> {
        let start = self.offset;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.offset += 1;
        }
        if self.offset == start {
            return Err(LayoutError::Malformed);
        }
        std::str::from_utf8(&self.bytes[start..self.offset])
            .map_err(|_| LayoutError::Malformed)?
            .parse()
            .map_err(|_| LayoutError::Malformed)
    }

    fn expect(&mut self, expected: u8) -> Result<(), LayoutError> {
        if self.peek() != Some(expected) {
            return Err(LayoutError::Malformed);
        }
        self.offset += 1;
        Ok(())
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.offset).copied()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BootstrapRequest {
    pub pane_id: PaneId,
    pub command: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum TmuxPaneError {
    #[error(transparent)]
    Layout(#[from] LayoutError),
    #[error(transparent)]
    Presentation(#[from] PresentationError),
    #[error("tmux pane %{0} is unknown")]
    UnknownPane(u64),
    #[error("tmux pane output before bootstrap exceeded {MAX_PREBOOTSTRAP_BYTES} bytes")]
    PrebootstrapOutputTooLong,
    #[error("tmux output referenced too many panes before inventory")]
    TooManyPrebootstrapPanes,
    #[error("the attached tmux session or active window is missing")]
    MissingActiveWindow,
    #[error("the active tmux window has no usable layout")]
    MissingLayout,
}

struct PaneState {
    view: View,
    portal: Option<PanePortal>,
    metadata: Pane,
    surface_id: SurfaceId,
    bootstrap_requested: bool,
    bootstrapped: bool,
    prebootstrap_output: Vec<u8>,
}

struct PanePortal {
    connection_id: u64,
    view: View,
}

pub struct TmuxPaneSet {
    connection_id: u64,
    border_id: SurfaceId,
    next_surface_id: u64,
    panes: BTreeMap<PaneId, PaneState>,
    orphan_output: BTreeMap<PaneId, Vec<u8>>,
    prebootstrap_bytes: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TmuxResourceUsage {
    pub pane_count: usize,
    pub scrollback_rows: usize,
    pub retained_text_bytes: usize,
    pub image_bytes: usize,
    pub image_uploads: usize,
    pub image_placements: usize,
}

impl TmuxPaneSet {
    #[must_use]
    pub fn new(connection_id: u64) -> Self {
        let namespace = 0x8000_0000_0000_0000_u64
            | (connection_id.wrapping_mul(0x1_0000_0000) & 0x7fff_ffff_0000_0000);
        Self {
            connection_id,
            border_id: SurfaceId(namespace),
            next_surface_id: namespace.saturating_add(1),
            panes: BTreeMap::new(),
            orphan_output: BTreeMap::new(),
            prebootstrap_bytes: 0,
        }
    }

    pub fn reconcile(
        &mut self,
        topology: &TmuxTopology,
    ) -> Result<Vec<BootstrapRequest>, TmuxPaneError> {
        let removed_bytes = self
            .panes
            .iter()
            .filter(|(pane_id, _)| !topology.panes().contains_key(pane_id))
            .map(|(_, state)| state.prebootstrap_output.len())
            .sum::<usize>();
        self.prebootstrap_bytes = self.prebootstrap_bytes.saturating_sub(removed_bytes);
        self.panes
            .retain(|pane_id, _| topology.panes().contains_key(pane_id));
        let mut requests = Vec::new();
        for pane in topology.panes().values() {
            let rows = dimension(pane.height);
            let cols = dimension(pane.width);
            let state = self.panes.entry(pane.id).or_insert_with(|| {
                let surface_id = SurfaceId(self.next_surface_id);
                self.next_surface_id = self.next_surface_id.saturating_add(1);
                PaneState {
                    view: View::new(rows, cols),
                    portal: None,
                    metadata: pane.clone(),
                    surface_id,
                    bootstrap_requested: false,
                    bootstrapped: false,
                    prebootstrap_output: self.orphan_output.remove(&pane.id).unwrap_or_default(),
                }
            });
            if state.view.size() != (rows, cols) {
                state.view.set_size(rows, cols);
            }
            if let Some(portal) = &mut state.portal
                && portal.view.size() != (rows, cols)
            {
                portal.view.set_size(rows, cols);
                render_portal(&mut portal.view);
            }
            state.metadata.clone_from(pane);
            if !state.bootstrapped && !state.bootstrap_requested {
                let command = capture_command(pane);
                state.bootstrap_requested = true;
                requests.push(BootstrapRequest {
                    pane_id: pane.id,
                    command,
                });
            }
        }
        let stale_orphan_bytes = self.orphan_output.values().map(Vec::len).sum::<usize>();
        self.prebootstrap_bytes = self.prebootstrap_bytes.saturating_sub(stale_orphan_bytes);
        self.orphan_output.clear();
        Ok(requests)
    }

    pub fn process_output(
        &mut self,
        pane_id: PaneId,
        bytes: &[u8],
    ) -> Result<Option<UpdateSummary>, TmuxPaneError> {
        let Some(state) = self.panes.get_mut(&pane_id) else {
            if !self.orphan_output.contains_key(&pane_id)
                && self.orphan_output.len() == MAX_ORPHAN_PANES
            {
                return Err(TmuxPaneError::TooManyPrebootstrapPanes);
            }
            let output = self.orphan_output.entry(pane_id).or_default();
            let next_total = self
                .prebootstrap_bytes
                .checked_add(bytes.len())
                .ok_or(TmuxPaneError::PrebootstrapOutputTooLong)?;
            if next_total > MAX_PREBOOTSTRAP_BYTES {
                return Err(TmuxPaneError::PrebootstrapOutputTooLong);
            }
            let next_pane_len = output
                .len()
                .checked_add(bytes.len())
                .ok_or(TmuxPaneError::PrebootstrapOutputTooLong)?;
            if next_pane_len > MAX_PREBOOTSTRAP_BYTES {
                return Err(TmuxPaneError::PrebootstrapOutputTooLong);
            }
            output.extend_from_slice(bytes);
            self.prebootstrap_bytes = next_total;
            return Ok(None);
        };
        if !state.bootstrapped {
            let next_total = self
                .prebootstrap_bytes
                .checked_add(bytes.len())
                .ok_or(TmuxPaneError::PrebootstrapOutputTooLong)?;
            let next_pane_len = state
                .prebootstrap_output
                .len()
                .checked_add(bytes.len())
                .ok_or(TmuxPaneError::PrebootstrapOutputTooLong)?;
            if next_total > MAX_PREBOOTSTRAP_BYTES || next_pane_len > MAX_PREBOOTSTRAP_BYTES {
                return Err(TmuxPaneError::PrebootstrapOutputTooLong);
            }
            state.prebootstrap_output.extend_from_slice(bytes);
            self.prebootstrap_bytes = next_total;
            return Ok(None);
        }
        state.view.process_changes(bytes);
        Ok(Some(state.view.update_summary().clone()))
    }

    pub fn apply_bootstrap(
        &mut self,
        pane_id: PaneId,
        status: CommandStatus,
        output: &[Vec<u8>],
        now_ms: u128,
    ) -> Result<(), TmuxPaneError> {
        let state = self
            .panes
            .get_mut(&pane_id)
            .ok_or(TmuxPaneError::UnknownPane(pane_id.0))?;
        if status == CommandStatus::Error {
            state.bootstrapped = true;
            state.bootstrap_requested = false;
            let buffered = std::mem::take(&mut state.prebootstrap_output);
            self.prebootstrap_bytes = self.prebootstrap_bytes.saturating_sub(buffered.len());
            state.view.process_changes(&buffered);
            return Ok(());
        }

        let rows = dimension(state.metadata.height);
        let cols = dimension(state.metadata.width);
        let mut view = View::new(rows, cols);
        let bytes = reconstruction_bytes(&state.metadata, output);
        view.process_changes(&bytes);
        view.finalize_changes(now_ms);
        state.view = view;
        state.bootstrapped = true;
        state.bootstrap_requested = false;
        self.prebootstrap_bytes = self
            .prebootstrap_bytes
            .saturating_sub(state.prebootstrap_output.len());
        state.prebootstrap_output.clear();
        Ok(())
    }

    #[must_use]
    pub fn pane_view(&self, pane_id: PaneId) -> Option<&View> {
        self.panes.get(&pane_id).map(|state| &state.view)
    }

    #[must_use]
    pub fn pane_view_mut(&mut self, pane_id: PaneId) -> Option<&mut View> {
        self.panes.get_mut(&pane_id).map(|state| &mut state.view)
    }

    pub fn pane_portal_view_mut(&mut self, pane_id: PaneId) -> Option<&mut View> {
        self.panes
            .get_mut(&pane_id)
            .and_then(|state| state.portal.as_mut())
            .map(|portal| &mut portal.view)
    }

    pub fn set_pane_portal(
        &mut self,
        pane_id: PaneId,
        connection_id: u64,
    ) -> Result<(), TmuxPaneError> {
        let state = self
            .panes
            .get_mut(&pane_id)
            .ok_or(TmuxPaneError::UnknownPane(pane_id.0))?;
        let (rows, cols) = state.view.size();
        let mut view = View::new(rows, cols);
        render_portal(&mut view);
        state.portal = Some(PanePortal {
            connection_id,
            view,
        });
        Ok(())
    }

    pub fn clear_pane_portal(&mut self, pane_id: PaneId, connection_id: u64) {
        let Some(state) = self.panes.get_mut(&pane_id) else {
            return;
        };
        if state
            .portal
            .as_ref()
            .is_some_and(|portal| portal.connection_id == connection_id)
        {
            state.portal = None;
        }
    }

    #[must_use]
    pub fn pane_portal_target(&self, pane_id: PaneId) -> Option<u64> {
        self.panes
            .get(&pane_id)?
            .portal
            .as_ref()
            .map(|portal| portal.connection_id)
    }

    #[must_use]
    pub fn pending_update(&self, pane_id: PaneId) -> Option<&UpdateSummary> {
        self.panes
            .get(&pane_id)
            .map(|state| state.view.update_summary())
    }

    #[must_use]
    pub fn surface_id(&self, pane_id: PaneId) -> Option<SurfaceId> {
        self.panes.get(&pane_id).map(|state| state.surface_id)
    }

    #[must_use]
    pub fn all_bootstrapped(&self) -> bool {
        !self.panes.is_empty() && self.panes.values().all(|state| state.bootstrapped)
    }

    #[must_use]
    pub fn connection_id(&self) -> u64 {
        self.connection_id
    }

    pub fn resource_usage(&mut self) -> Result<TmuxResourceUsage, TmuxPaneError> {
        let mut usage = TmuxResourceUsage {
            pane_count: self.panes.len(),
            ..TmuxResourceUsage::default()
        };
        for state in self.panes.values_mut() {
            usage.scrollback_rows = usage
                .scrollback_rows
                .saturating_add(state.view.scrollback_len());
            usage.retained_text_bytes = usage
                .retained_text_bytes
                .saturating_add(state.view.contents_full().len());
            let (bytes, uploads, placements) = state.view.with_live_screen(|view| {
                let media = view.presentation_media()?;
                Ok::<_, PresentationError>((
                    media.total_bytes(),
                    media.upload_count(),
                    media.placement_count(),
                ))
            })?;
            usage.image_bytes = usage.image_bytes.saturating_add(bytes);
            usage.image_uploads = usage.image_uploads.saturating_add(uploads);
            usage.image_placements = usage.image_placements.saturating_add(placements);
        }
        Ok(usage)
    }

    pub fn compose(
        &mut self,
        topology: &TmuxTopology,
        geometry: TerminalGeometry,
    ) -> Result<Scene, TmuxPaneError> {
        let session_id = topology
            .attached_session()
            .ok_or(TmuxPaneError::MissingActiveWindow)?;
        let session = topology
            .session(session_id)
            .ok_or(TmuxPaneError::MissingActiveWindow)?;
        let window_id = session
            .active_window
            .ok_or(TmuxPaneError::MissingActiveWindow)?;
        let window = topology
            .window(window_id)
            .ok_or(TmuxPaneError::MissingActiveWindow)?;
        let layout_text = if window.visible_layout.is_empty() {
            &window.layout
        } else {
            &window.visible_layout
        };
        if layout_text.is_empty() {
            return Err(TmuxPaneError::MissingLayout);
        }
        let layout = TmuxLayout::parse(layout_text)?;
        let active_pane = window
            .active_pane
            .filter(|pane_id| layout.pane(*pane_id).is_some())
            .or_else(|| layout.panes().first().map(|pane| pane.pane_id));

        let mut scene = Scene::new(geometry);
        scene.panes.push(SceneSurface::new(
            self.border_id,
            GridPoint::new(0, 0),
            layout.border_snapshot(geometry),
        ));
        for pane in layout.panes() {
            let state = self
                .panes
                .get_mut(&pane.pane_id)
                .ok_or(TmuxPaneError::UnknownPane(pane.pane_id.0))?;
            if state.view.size() != (pane.rows, pane.cols) {
                state.view.set_size(pane.rows, pane.cols);
            }
            if let Some(portal) = &mut state.portal
                && portal.view.size() != (pane.rows, pane.cols)
            {
                portal.view.set_size(pane.rows, pane.cols);
                render_portal(&mut portal.view);
            }
            let presented_view = state
                .portal
                .as_mut()
                .map_or(&mut state.view, |portal| &mut portal.view);
            let mut snapshot = presented_view.with_live_screen(|view| view.screen().clone());
            snapshot.modes.focus_reporting = Some(pane.pane_id) == active_pane;
            scene
                .panes
                .push(SceneSurface::new(state.surface_id, pane.origin, snapshot));
            if state.portal.is_none() {
                state
                    .view
                    .with_live_screen(|view| -> Result<(), PresentationError> {
                        view.presentation_media()?.append_to_scene(
                            state.surface_id,
                            pane.origin,
                            GridRect::new(pane.origin, pane.rows, pane.cols),
                            &mut scene,
                        )?;
                        Ok(())
                    })?;
            }
        }
        scene.cursor_owner = active_pane
            .and_then(|pane_id| self.surface_id(pane_id))
            .map_or(CursorOwner::Hidden, CursorOwner::Pane);
        scene.effects.title = Some(window.name.clone());
        Ok(scene)
    }
}

fn render_portal(view: &mut View) {
    view.clear_update_summary();
    let mut bytes = b"\x1b[2J\x1b[H".to_vec();
    bytes.extend_from_slice(PANE_PORTAL_TEXT.as_bytes());
    view.process_changes(&bytes);
}

fn dimension(value: u32) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX).max(1)
}

pub fn capture_command(pane: &Pane) -> Vec<u8> {
    let mut command = b"capture-pane ".to_vec();
    if pane.pane_in_mode > 0 {
        command.extend_from_slice(b"-M ");
    } else if pane.alternate_on {
        command.extend_from_slice(b"-a ");
    }
    command.extend_from_slice(b"-p -e -J ");
    if !pane.alternate_on && pane.pane_in_mode == 0 {
        command.extend_from_slice(b"-S - ");
    }
    command.extend_from_slice(format!("-t %{}\n", pane.id.0).as_bytes());
    command
}

fn reconstruction_bytes(pane: &Pane, output: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    if pane.alternate_on {
        bytes.extend_from_slice(b"\x1b[?1049h");
    }
    bytes.extend_from_slice(b"\x1b[2J\x1b[H");
    for (index, line) in output.iter().enumerate() {
        if index > 0 {
            bytes.extend_from_slice(b"\r\n");
        }
        bytes.extend_from_slice(line);
    }
    let row = pane.cursor_y.saturating_add(1);
    let col = pane.cursor_x.saturating_add(1);
    bytes.extend_from_slice(format!("\x1b[{row};{col}H").as_bytes());
    bytes.extend_from_slice(if pane.cursor_visible {
        b"\x1b[?25h"
    } else {
        b"\x1b[?25l"
    });
    let shape = match pane.cursor_shape.as_str() {
        "underline" | "2" => 4,
        "bar" | "3" => 6,
        _ => 2,
    };
    bytes.extend_from_slice(format!("\x1b[{shape} q").as_bytes());
    bytes
}
