//! Pane-local terminal engines, tmux layout parsing, and scene composition.

use crate::{
    presentation::{
        CursorOwner, GridPoint, GridRect, PresentationError, PresentedViewFrame, Scene,
        SceneSurface, SurfaceId, ViewId,
    },
    terminal::{Cell, Cursor, Row, TerminalGeometry, TerminalSnapshot, UpdateSummary},
    tmux_control::CommandStatus,
    tmux_model::{Pane, PaneCaptureMetadata, PaneId, TmuxTopology},
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
        let (body, floating_body) = if let Some((body, floating)) = body.split_once('<') {
            let floating = floating.strip_suffix('>').ok_or(LayoutError::Malformed)?;
            if body.contains('<')
                || body.contains('>')
                || floating.is_empty()
                || floating.contains('<')
                || floating.contains('>')
            {
                return Err(LayoutError::Malformed);
            }
            (body, Some(floating))
        } else {
            if body.contains('>') {
                return Err(LayoutError::Malformed);
            }
            (body, None)
        };

        // tmux appends floating panes in top-to-bottom z order inside `<...>`.
        // The same panes also appear as overlapping children in the main
        // layout tree. Parse the suffix first so those children can be
        // excluded from the tiled partition checks below.
        let mut floating_panes = Vec::new();
        if let Some(floating_body) = floating_body {
            let no_floating = BTreeSet::new();
            let mut floating_parser = LayoutParser {
                bytes: floating_body.as_bytes(),
                offset: 0,
                nodes: 0,
                panes: Vec::new(),
                dividers: Vec::new(),
                floating_ids: &no_floating,
            };
            loop {
                floating_parser.parse_node(0, None)?;
                if floating_parser.offset == floating_parser.bytes.len() {
                    break;
                }
                floating_parser.expect(b',')?;
            }
            floating_panes = floating_parser.panes;
            if floating_panes.is_empty() {
                return Err(LayoutError::Malformed);
            }
        }
        let mut floating_ids = BTreeSet::new();
        for pane in &floating_panes {
            if !floating_ids.insert(pane.pane_id) {
                return Err(LayoutError::DuplicatePane(pane.pane_id.0));
            }
        }
        let mut parser = LayoutParser {
            bytes: body.as_bytes(),
            offset: 0,
            nodes: 0,
            panes: Vec::new(),
            dividers: Vec::new(),
            floating_ids: &floating_ids,
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
        let pane_by_id = parser
            .panes
            .iter()
            .map(|pane| (pane.pane_id, *pane))
            .collect::<BTreeMap<_, _>>();
        for floating in &floating_panes {
            if pane_by_id.get(&floating.pane_id) != Some(floating) {
                return Err(LayoutError::Malformed);
            }
        }
        let mut panes = parser
            .panes
            .into_iter()
            .filter(|pane| !floating_ids.contains(&pane.pane_id))
            .collect::<Vec<_>>();
        // Scene surfaces are composed bottom-to-top, the reverse of tmux's
        // z-order suffix.
        panes.extend(floating_panes.into_iter().rev());
        Ok(Self {
            panes,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
    floating_ids: &'a BTreeSet<PaneId>,
}

#[derive(Clone, Copy, Debug)]
struct ParsedNode {
    rect: NodeRect,
    floating: bool,
}

impl LayoutParser<'_> {
    fn parse_node(
        &mut self,
        depth: usize,
        parent: Option<NodeRect>,
    ) -> Result<ParsedNode, LayoutError> {
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

        let floating = match self.peek() {
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
                self.floating_ids.contains(&PaneId(pane_id))
            }
            _ => return Err(LayoutError::Malformed),
        };
        Ok(ParsedNode { rect, floating })
    }

    fn parse_children(
        &mut self,
        depth: usize,
        rect: NodeRect,
        kind: SplitKind,
        close: u8,
    ) -> Result<bool, LayoutError> {
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
        let tiled = children
            .iter()
            .filter(|child| !child.floating)
            .map(|child| child.rect)
            .collect::<Vec<_>>();
        let has_floating = tiled.len() != children.len();
        if !has_floating && tiled.len() < 2 {
            return Err(LayoutError::Malformed);
        }
        if has_floating && tiled.len() <= 1 {
            if tiled.first().is_some_and(|child| *child != rect) {
                return Err(LayoutError::InvalidPartition);
            }
            return Ok(tiled.is_empty());
        }
        let valid_partition = match kind {
            SplitKind::LeftRight => {
                tiled.first().is_some_and(|child| {
                    child.x == rect.x && child.y == rect.y && child.height == rect.height
                }) && tiled.last().is_some_and(|child| {
                    u32::from(child.x) + u32::from(child.width)
                        == u32::from(rect.x) + u32::from(rect.width)
                }) && tiled.windows(2).all(|pair| {
                    pair[1].y == rect.y
                        && pair[1].height == rect.height
                        && u32::from(pair[0].x) + u32::from(pair[0].width) + 1
                            == u32::from(pair[1].x)
                })
            }
            SplitKind::TopBottom => {
                tiled.first().is_some_and(|child| {
                    child.x == rect.x && child.y == rect.y && child.width == rect.width
                }) && tiled.last().is_some_and(|child| {
                    u32::from(child.y) + u32::from(child.height)
                        == u32::from(rect.y) + u32::from(rect.height)
                }) && tiled.windows(2).all(|pair| {
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
        for pair in tiled.windows(2) {
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
        Ok(false)
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
    /// Pane or portal views removed from the logical topology while a
    /// physical render can still name them. They remain addressable by
    /// screen-reading commands until a completed replacement scene selects a
    /// different active view.
    retired_presentations: Vec<View>,
    orphan_output: BTreeMap<PaneId, Vec<u8>>,
    prebootstrap_bytes: usize,
    presentation_tracking: bool,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PresentationModel {
    Live,
    Committed,
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
            retired_presentations: Vec::new(),
            orphan_output: BTreeMap::new(),
            prebootstrap_bytes: 0,
            presentation_tracking: false,
        }
    }

    pub fn reconcile(
        &mut self,
        topology: &TmuxTopology,
    ) -> Result<Vec<BootstrapRequest>, TmuxPaneError> {
        let removed_panes = self
            .panes
            .keys()
            .filter(|pane_id| !topology.panes().contains_key(pane_id))
            .copied()
            .collect::<Vec<_>>();
        let mut removed_bytes = 0_usize;
        for pane_id in removed_panes {
            let Some(mut state) = self.panes.remove(&pane_id) else {
                continue;
            };
            removed_bytes = removed_bytes.saturating_add(state.prebootstrap_output.len());
            if self.presentation_tracking {
                self.retired_presentations.push(state.view);
                if let Some(portal) = state.portal.take() {
                    self.retired_presentations.push(portal.view);
                }
            }
        }
        self.prebootstrap_bytes = self.prebootstrap_bytes.saturating_sub(removed_bytes);
        let mut requests = Vec::new();
        for pane in topology.panes().values() {
            let rows = dimension(pane.height);
            let cols = dimension(pane.width);
            let state = self.panes.entry(pane.id).or_insert_with(|| {
                let surface_id = SurfaceId(self.next_surface_id);
                self.next_surface_id = self.next_surface_id.saturating_add(1);
                let mut view = View::new(rows, cols);
                if self.presentation_tracking {
                    view.enable_presentation_tracking();
                }
                PaneState {
                    view,
                    portal: None,
                    metadata: pane.clone(),
                    surface_id,
                    bootstrap_requested: false,
                    bootstrapped: false,
                    prebootstrap_output: self.orphan_output.remove(&pane.id).unwrap_or_default(),
                }
            });
            if state.view.live_size() != (rows, cols) {
                state.view.set_size(rows, cols);
            }
            if let Some(portal) = &mut state.portal
                && portal.view.live_size() != (rows, cols)
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

    pub(crate) fn enable_presentation_tracking(&mut self) {
        self.presentation_tracking = true;
        for state in self.panes.values_mut() {
            state.view.enable_presentation_tracking();
            if let Some(portal) = &mut state.portal {
                portal.view.enable_presentation_tracking();
            }
        }
    }

    pub fn process_output(
        &mut self,
        pane_id: PaneId,
        bytes: &[u8],
    ) -> Result<Option<UpdateSummary>, TmuxPaneError> {
        self.process_output_with_summary_retention(pane_id, bytes, true)
    }

    /// Applies one tmux `%output` batch and returns only that batch's summary.
    ///
    /// The active accessibility model retains its cumulative summary until
    /// finalization. Hidden and visible non-active panes keep their terminal
    /// models live but discard summary metadata after each batch; rendering
    /// and immediate side effects use the returned batch value.
    pub(crate) fn process_output_with_summary_retention(
        &mut self,
        pane_id: PaneId,
        bytes: &[u8],
        retain_for_accessibility: bool,
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
        let update = state
            .view
            .process_changes_with_batch(bytes, retain_for_accessibility);
        // tmux owns the pane PTY and answers its application's terminal
        // queries. This Ghostty instance is an observational shadow only.
        // Clear the view's retained copy, but return this batch's replies so
        // the controller can forward the OSC 10/11 subset with
        // `refresh-client -r` as required by current control mode.
        state.view.discard_shadow_pty_replies();
        Ok(Some(update))
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
            state.view.discard_shadow_pty_replies();
            return Ok(());
        }

        let rows = dimension(state.metadata.height);
        let cols = dimension(state.metadata.width);
        let mut view = View::new(rows, cols);
        if self.presentation_tracking {
            view.enable_presentation_tracking();
        }
        let bytes = reconstruction_bytes(
            state.metadata.alternate_on,
            state.metadata.cursor_x,
            state.metadata.cursor_y,
            state.metadata.cursor_visible,
            &state.metadata.cursor_shape,
            output,
            &[],
        );
        view.process_changes(&bytes);
        let buffered = std::mem::take(&mut state.prebootstrap_output);
        self.prebootstrap_bytes = self.prebootstrap_bytes.saturating_sub(buffered.len());
        if view.contents_full().trim().is_empty() && !buffered.is_empty() {
            view.process_changes(&buffered);
        }
        view.discard_shadow_pty_replies();
        view.finalize_changes(now_ms);
        let retired = std::mem::replace(&mut state.view, view);
        state.bootstrapped = true;
        state.bootstrap_requested = false;
        if self.presentation_tracking {
            self.retired_presentations.push(retired);
        }
        Ok(())
    }

    pub fn apply_resync_capture(
        &mut self,
        metadata: &PaneCaptureMetadata,
        output: &[Vec<u8>],
        pending_escape: &[u8],
        now_ms: u128,
    ) -> Result<(), TmuxPaneError> {
        let state = self
            .panes
            .get_mut(&metadata.pane_id)
            .ok_or(TmuxPaneError::UnknownPane(metadata.pane_id.0))?;
        state.metadata.left = metadata.left;
        state.metadata.top = metadata.top;
        state.metadata.width = metadata.width;
        state.metadata.height = metadata.height;
        state.metadata.dead = metadata.dead;
        state.metadata.cursor_x = metadata.cursor_x;
        state.metadata.cursor_y = metadata.cursor_y;
        state.metadata.cursor_visible = metadata.cursor_visible;
        state
            .metadata
            .cursor_shape
            .clone_from(&metadata.cursor_shape);
        state.metadata.alternate_on = metadata.alternate_on;
        state.metadata.pane_in_mode = metadata.pane_in_mode;
        state.metadata.history_size = metadata.history_size;

        let rows = dimension(metadata.height);
        let cols = dimension(metadata.width);
        let mut view = View::new(rows, cols);
        if self.presentation_tracking {
            view.enable_presentation_tracking();
        }
        let bytes = reconstruction_bytes(
            metadata.alternate_on,
            metadata.cursor_x,
            metadata.cursor_y,
            metadata.cursor_visible,
            &metadata.cursor_shape,
            output,
            pending_escape,
        );
        view.process_changes(&bytes);
        view.discard_shadow_pty_replies();
        view.finalize_changes(now_ms);
        let retired = std::mem::replace(&mut state.view, view);
        state.bootstrapped = true;
        state.bootstrap_requested = false;
        if self.presentation_tracking {
            self.retired_presentations.push(retired);
        }
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

    pub(crate) fn visible_holds_synchronized_output(&self, layout: &TmuxLayout) -> bool {
        layout.panes().iter().any(|pane| {
            self.panes.get(&pane.pane_id).is_some_and(|state| {
                state.portal.as_ref().map_or_else(
                    || state.view.holds_synchronized_output(),
                    |portal| portal.view.holds_synchronized_output(),
                )
            })
        })
    }

    pub(crate) fn capture_live_presentation_frames(
        &mut self,
        layout: &TmuxLayout,
        active_pane: Option<PaneId>,
    ) -> (Option<ViewId>, Vec<PresentedViewFrame>) {
        let mut active_view = None;
        let mut frames = Vec::with_capacity(layout.panes().len());
        for pane in layout.panes() {
            let Some(state) = self.panes.get_mut(&pane.pane_id) else {
                continue;
            };
            let presented_view = state
                .portal
                .as_mut()
                .map_or(&mut state.view, |portal| &mut portal.view);
            if Some(pane.pane_id) == active_pane {
                active_view = Some(presented_view.view_id());
            }
            frames.push(presented_view.capture_live_presentation_frame(state.surface_id));
        }
        (active_view, frames)
    }

    pub(crate) fn capture_committed_presentation_frames(
        &mut self,
        layout: &TmuxLayout,
        active_pane: Option<PaneId>,
    ) -> (Option<ViewId>, Vec<PresentedViewFrame>) {
        let mut active_view = None;
        let mut frames = Vec::with_capacity(layout.panes().len());
        for pane in layout.panes() {
            let Some(state) = self.panes.get_mut(&pane.pane_id) else {
                continue;
            };
            let presented_view = state
                .portal
                .as_mut()
                .map_or(&mut state.view, |portal| &mut portal.view);
            if Some(pane.pane_id) == active_pane {
                active_view = Some(presented_view.view_id());
            }
            frames.push(presented_view.capture_committed_presentation_frame(state.surface_id));
        }
        (active_view, frames)
    }

    pub(crate) fn apply_presented_frame(&mut self, frame: &PresentedViewFrame) -> bool {
        for state in self.panes.values_mut() {
            if state.view.apply_presented_frame(frame.clone()) {
                return true;
            }
            if let Some(portal) = &mut state.portal
                && portal.view.apply_presented_frame(frame.clone())
            {
                return true;
            }
        }
        self.retired_presentations
            .iter_mut()
            .any(|view| view.apply_presented_frame(frame.clone()))
    }

    pub(crate) fn model_by_id_mut(&mut self, view_id: ViewId) -> Option<&mut View> {
        for state in self.panes.values_mut() {
            if state.view.view_id() == view_id {
                return Some(&mut state.view);
            }
            if let Some(portal) = &mut state.portal
                && portal.view.view_id() == view_id
            {
                return Some(&mut portal.view);
            }
        }
        self.retired_presentations
            .iter_mut()
            .find(|view| view.view_id() == view_id)
    }

    pub(crate) fn retain_accessibility_views(&mut self, retained: &[ViewId]) {
        self.retired_presentations
            .retain(|view| retained.contains(&view.view_id()));
    }

    pub fn set_pane_portal(
        &mut self,
        pane_id: PaneId,
        connection_id: u64,
    ) -> Result<(), TmuxPaneError> {
        let presentation_tracking = self.presentation_tracking;
        let (rows, cols, retired) = {
            let state = self
                .panes
                .get_mut(&pane_id)
                .ok_or(TmuxPaneError::UnknownPane(pane_id.0))?;
            state.view.clear_update_summary();
            let (rows, cols) = state.view.live_size();
            let retired = state.portal.take().map(|portal| portal.view);
            (rows, cols, retired)
        };
        if presentation_tracking && let Some(retired) = retired {
            self.retired_presentations.push(retired);
        }
        let mut view = View::new(rows, cols);
        if presentation_tracking {
            view.enable_presentation_tracking();
        }
        render_portal(&mut view);
        self.panes
            .get_mut(&pane_id)
            .expect("a checked pane remains present")
            .portal = Some(PanePortal {
            connection_id,
            view,
        });
        Ok(())
    }

    pub fn clear_pane_portal(&mut self, pane_id: PaneId, connection_id: u64) {
        let Some(state) = self.panes.get_mut(&pane_id) else {
            return;
        };
        let matching = state
            .portal
            .as_ref()
            .is_some_and(|portal| portal.connection_id == connection_id);
        if !matching {
            return;
        }
        state.view.clear_update_summary();
        if let Some(portal) = state.portal.take()
            && self.presentation_tracking
        {
            self.retired_presentations.push(portal.view);
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

    /// Drops update metadata at an accessibility ownership handoff without
    /// changing any pane's terminal contents or parser state.
    pub(crate) fn clear_update_summaries(&mut self) {
        for state in self.panes.values_mut() {
            state.view.clear_update_summary();
            if let Some(portal) = &mut state.portal {
                portal.view.clear_update_summary();
            }
        }
    }

    #[must_use]
    pub fn surface_id(&self, pane_id: PaneId) -> Option<SurfaceId> {
        self.panes.get(&pane_id).map(|state| state.surface_id)
    }

    #[must_use]
    pub fn pane_is_bootstrapped(&self, pane_id: PaneId) -> bool {
        self.panes
            .get(&pane_id)
            .is_some_and(|state| state.bootstrapped)
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
        self.compose_with_model(topology, geometry, PresentationModel::Live)
    }

    /// Composes a connection view from its topology-synchronized active-window
    /// projection. The controller caches this projection so hot output and
    /// presentation paths do not repeatedly parse the same tmux layout.
    pub(crate) fn compose_layout(
        &mut self,
        layout: &TmuxLayout,
        active_pane: Option<PaneId>,
        title: &str,
        geometry: TerminalGeometry,
    ) -> Result<Scene, TmuxPaneError> {
        self.compose_layout_with_model(
            layout,
            active_pane,
            title,
            geometry,
            PresentationModel::Live,
        )
    }

    pub(crate) fn compose_committed_layout(
        &mut self,
        layout: &TmuxLayout,
        active_pane: Option<PaneId>,
        title: &str,
        geometry: TerminalGeometry,
    ) -> Result<Scene, TmuxPaneError> {
        self.compose_layout_with_model(
            layout,
            active_pane,
            title,
            geometry,
            PresentationModel::Committed,
        )
    }

    fn compose_with_model(
        &mut self,
        topology: &TmuxTopology,
        geometry: TerminalGeometry,
        model: PresentationModel,
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

        self.compose_layout_with_model(&layout, active_pane, &window.name, geometry, model)
    }

    fn compose_layout_with_model(
        &mut self,
        layout: &TmuxLayout,
        active_pane: Option<PaneId>,
        title: &str,
        geometry: TerminalGeometry,
        model: PresentationModel,
    ) -> Result<Scene, TmuxPaneError> {
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
            if state.view.live_size() != (pane.rows, pane.cols) {
                state.view.set_size(pane.rows, pane.cols);
            }
            if let Some(portal) = &mut state.portal
                && portal.view.live_size() != (pane.rows, pane.cols)
            {
                portal.view.set_size(pane.rows, pane.cols);
                render_portal(&mut portal.view);
            }
            let presented_view = state
                .portal
                .as_mut()
                .map_or(&mut state.view, |portal| &mut portal.view);
            let mut snapshot = match model {
                PresentationModel::Live => {
                    presented_view.with_live_screen(|view| view.live_screen().clone())
                }
                PresentationModel::Committed => presented_view.committed_presentation_snapshot(),
            };
            snapshot.modes.focus_reporting = Some(pane.pane_id) == active_pane;
            scene
                .panes
                .push(SceneSurface::new(state.surface_id, pane.origin, snapshot));
            if state.portal.is_none()
                && (model == PresentationModel::Live || !state.view.holds_synchronized_output())
            {
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
        scene.effects.title = Some(title.to_owned());
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
    capture_command_for_state(pane.id, pane.alternate_on, pane.pane_in_mode)
}

pub fn capture_command_for_metadata(metadata: &PaneCaptureMetadata) -> Vec<u8> {
    capture_command_for_state(
        metadata.pane_id,
        metadata.alternate_on,
        metadata.pane_in_mode,
    )
}

fn capture_command_for_state(pane_id: PaneId, alternate_on: bool, pane_in_mode: u32) -> Vec<u8> {
    let mut command = b"capture-pane ".to_vec();
    if pane_in_mode > 0 {
        command.extend_from_slice(b"-M ");
    }
    command.extend_from_slice(b"-p -e -F -J ");
    if !alternate_on && pane_in_mode == 0 {
        command.extend_from_slice(b"-S - ");
    }
    command.extend_from_slice(format!("-t %{}\n", pane_id.0).as_bytes());
    command
}

#[must_use]
pub fn pending_escape_capture_command(pane_id: PaneId) -> Vec<u8> {
    format!("capture-pane -p -P -t %{}\n", pane_id.0).into_bytes()
}

fn reconstruction_bytes(
    alternate_on: bool,
    cursor_x: u32,
    cursor_y: u32,
    cursor_visible: bool,
    cursor_shape: &str,
    output: &[Vec<u8>],
    pending_escape: &[u8],
) -> Vec<u8> {
    let mut bytes = Vec::new();
    if alternate_on {
        bytes.extend_from_slice(b"\x1b[?1049h");
    }
    bytes.extend_from_slice(b"\x1b[2J\x1b[H");
    for (index, line) in output.iter().enumerate() {
        if index > 0 {
            bytes.extend_from_slice(b"\r\n");
        }
        let (flags, contents) = capture_line_flags(line);
        if flags.contains(&b'P') {
            bytes.extend_from_slice(b"\x1b]133;A\x1b\\");
        } else if flags.contains(&b'O') {
            bytes.extend_from_slice(b"\x1b]133;C\x1b\\");
        }
        bytes.extend_from_slice(contents);
    }
    let row = cursor_y.saturating_add(1);
    let col = cursor_x.saturating_add(1);
    bytes.extend_from_slice(format!("\x1b[{row};{col}H").as_bytes());
    bytes.extend_from_slice(if cursor_visible {
        b"\x1b[?25h"
    } else {
        b"\x1b[?25l"
    });
    let shape = match cursor_shape {
        "underline" | "2" => 4,
        "bar" | "3" => 6,
        _ => 2,
    };
    bytes.extend_from_slice(format!("\x1b[{shape} q").as_bytes());
    // `capture-pane -P` returns only an incomplete parser sequence. Appending
    // it last preserves that continuation without allowing cursor restoration
    // sequences to become part of it.
    bytes.extend_from_slice(pending_escape);
    bytes
}

fn capture_line_flags(line: &[u8]) -> (&[u8], &[u8]) {
    let Some(space) = line.iter().position(|byte| *byte == b' ') else {
        return (&[], line);
    };
    let flags = &line[..space];
    if flags.is_empty()
        || !flags
            .iter()
            .all(|flag| matches!(flag, b'-' | b'D' | b'O' | b'P' | b'X' | b'H'))
    {
        return (&[], line);
    }
    (flags, &line[space + 1..])
}

#[cfg(test)]
mod synchronization_tests {
    use super::{
        TmuxLayout, TmuxPaneSet, capture_command_for_metadata, pending_escape_capture_command,
    };
    use crate::{
        tmux_control::CommandStatus,
        tmux_model::{PaneCaptureMetadata, PaneId, TmuxTopology},
    };

    const SPLIT: &str = "abcd,20x4,0,0{10x4,0,0,20,9x4,11,0,21}";

    fn split_topology() -> TmuxTopology {
        let lines = [
            b"S\t$1\twork".to_vec(),
            format!("W\t$1\t@10\t1\t1\t{SPLIT}\t{SPLIT}\t*\teditor").into_bytes(),
            b"P\t@10\t%20\t1\t1\t0\t0\t10\t4\t0\t0\t0\t1\t0\t0\t0\t0\tleft".to_vec(),
            b"P\t@10\t%21\t2\t0\t11\t0\t9\t4\t0\t0\t0\t1\t0\t0\t0\t0\tright".to_vec(),
            b"A\t$1".to_vec(),
        ];
        let mut topology = TmuxTopology::new(1);
        topology.replace_inventory(&lines).expect("topology");
        topology
    }

    fn right_only_topology() -> TmuxTopology {
        const RIGHT_ONLY: &str = "dcba,9x4,0,0,21";
        let lines = [
            b"S\t$1\twork".to_vec(),
            format!("W\t$1\t@10\t1\t1\t{RIGHT_ONLY}\t{RIGHT_ONLY}\t*\teditor").into_bytes(),
            b"P\t@10\t%21\t2\t1\t0\t0\t9\t4\t0\t0\t0\t1\t0\t0\t0\t0\tright".to_vec(),
            b"A\t$1".to_vec(),
        ];
        let mut topology = TmuxTopology::new(1);
        topology.replace_inventory(&lines).expect("topology");
        topology
    }

    fn split_with_hidden_topology() -> TmuxTopology {
        const HIDDEN: &str = "dcba,20x4,0,0,22";
        let lines = [
            b"S\t$1\twork".to_vec(),
            format!("W\t$1\t@10\t1\t1\t{SPLIT}\t{SPLIT}\t*\teditor").into_bytes(),
            format!("W\t$1\t@11\t2\t0\t{HIDDEN}\t{HIDDEN}\t-\thidden").into_bytes(),
            b"P\t@10\t%20\t1\t1\t0\t0\t10\t4\t0\t0\t0\t1\t0\t0\t0\t0\tleft".to_vec(),
            b"P\t@10\t%21\t2\t0\t11\t0\t9\t4\t0\t0\t0\t1\t0\t0\t0\t0\tright".to_vec(),
            b"P\t@11\t%22\t1\t1\t0\t0\t20\t4\t0\t0\t0\t1\t0\t0\t0\t0\toffscreen".to_vec(),
            b"A\t$1".to_vec(),
        ];
        let mut topology = TmuxTopology::new(1);
        topology.replace_inventory(&lines).expect("topology");
        topology
    }

    #[test]
    fn authoritative_capture_restores_screen_cursor_semantics_and_parser_continuation() {
        let topology = split_topology();
        let mut panes = TmuxPaneSet::new(1);
        for request in panes.reconcile(&topology).expect("reconcile") {
            panes
                .apply_bootstrap(request.pane_id, CommandStatus::Success, &[], 0)
                .expect("bootstrap");
        }
        let metadata = PaneCaptureMetadata {
            pane_id: PaneId(20),
            left: 0,
            top: 0,
            width: 10,
            height: 4,
            dead: false,
            cursor_x: 3,
            cursor_y: 2,
            cursor_visible: true,
            cursor_shape: "bar".to_owned(),
            alternate_on: true,
            pane_in_mode: 0,
            history_size: 0,
        };
        panes
            .apply_resync_capture(
                &metadata,
                &[b"P prompt".to_vec(), b"O output".to_vec()],
                b"\x1b[",
                10,
            )
            .expect("apply capture");
        let snapshot = panes.pane_view(PaneId(20)).expect("pane").live_screen();
        assert!(snapshot.alternate_screen());
        assert_eq!(snapshot.cursor_position(), (2, 3));
        assert_eq!(snapshot.semantic_marks.len(), 2);

        panes
            .process_output(PaneId(20), b"2JAFTER")
            .expect("complete the pending CSI");
        let contents = panes.pane_view(PaneId(20)).expect("pane").contents_full();
        assert!(contents.contains("AFTER"), "{contents:?}");
        assert!(!contents.contains("prompt"), "{contents:?}");
    }

    #[test]
    fn capture_command_selects_the_authoritative_pane_screen() {
        let mut metadata = PaneCaptureMetadata {
            pane_id: PaneId(20),
            left: 0,
            top: 0,
            width: 10,
            height: 4,
            dead: false,
            cursor_x: 0,
            cursor_y: 0,
            cursor_visible: true,
            cursor_shape: "default".to_owned(),
            alternate_on: false,
            pane_in_mode: 0,
            history_size: 12,
        };
        assert_eq!(
            capture_command_for_metadata(&metadata),
            b"capture-pane -p -e -F -J -S - -t %20\n"
        );
        metadata.alternate_on = true;
        assert_eq!(
            capture_command_for_metadata(&metadata),
            b"capture-pane -p -e -F -J -t %20\n"
        );
        metadata.pane_in_mode = 1;
        assert_eq!(
            capture_command_for_metadata(&metadata),
            b"capture-pane -M -p -e -F -J -t %20\n"
        );
        assert_eq!(
            pending_escape_capture_command(PaneId(20)),
            b"capture-pane -p -P -t %20\n"
        );
    }

    #[test]
    fn ordinary_output_in_one_visible_pane_does_not_release_another_panes_snapshot() {
        let topology = split_topology();
        let layout = TmuxLayout::parse(SPLIT).expect("layout");
        let mut panes = TmuxPaneSet::new(1);
        let requests = panes.reconcile(&topology).expect("reconcile");
        for request in requests {
            panes
                .apply_bootstrap(
                    request.pane_id,
                    CommandStatus::Success,
                    &[format!("pane {}", request.pane_id.0).into_bytes()],
                    0,
                )
                .expect("bootstrap");
        }

        panes
            .process_output(PaneId(20), b"\x1b[?2026h\rpartial")
            .expect("open left transaction");
        assert!(panes.visible_holds_synchronized_output(&layout));
        panes
            .process_output(PaneId(21), b" ordinary")
            .expect("update right pane");
        assert!(panes.visible_holds_synchronized_output(&layout));

        panes
            .process_output(PaneId(20), b"\x1b[?2026l")
            .expect("close left transaction");
        assert!(!panes.visible_holds_synchronized_output(&layout));
    }

    #[test]
    fn low_rate_nonactive_output_keeps_pending_summaries_batch_bounded() {
        const ITERATIONS: usize = 4_096;
        let topology = split_with_hidden_topology();
        let mut panes = TmuxPaneSet::new(1);
        for request in panes.reconcile(&topology).expect("reconcile") {
            panes
                .apply_bootstrap(
                    request.pane_id,
                    CommandStatus::Success,
                    &[format!("pane {}", request.pane_id.0).into_bytes()],
                    0,
                )
                .expect("bootstrap");
        }

        // Pane 21 is visible but not active; pane 22 belongs to the hidden
        // window. Small independent batches model a steady low-rate producer
        // which never trips byte-backlog flow control.
        for iteration in 0..ITERATIONS {
            for (pane_id, prefix) in [(PaneId(21), 'v'), (PaneId(22), 'h')] {
                let payload = format!("\x1b[1;1H\x1b[2K{prefix}-{iteration:04}\x07");
                let update = panes
                    .process_output_with_summary_retention(pane_id, payload.as_bytes(), false)
                    .expect("process output")
                    .expect("bootstrapped pane update");
                assert_eq!(update.batch_count, 1);
                assert_eq!(update.effects.bells, 1);
                let pending = panes.pending_update(pane_id).expect("pane summary");
                assert_eq!(pending.batch_count, 0);
                assert!(pending.printed_runs.is_empty());
                assert!(pending.operations.is_empty());
                assert!(pending.effects.events.is_empty());
            }
        }

        assert_eq!(panes.pane_view(PaneId(21)).unwrap().line(0), "v-4095");
        assert_eq!(panes.pane_view(PaneId(22)).unwrap().line(0), "h-4095");
    }

    #[test]
    fn active_output_keeps_accessibility_summary_until_finalization() {
        let topology = split_topology();
        let mut panes = TmuxPaneSet::new(1);
        for request in panes.reconcile(&topology).expect("reconcile") {
            panes
                .apply_bootstrap(
                    request.pane_id,
                    CommandStatus::Success,
                    &[format!("pane {}", request.pane_id.0).into_bytes()],
                    0,
                )
                .expect("bootstrap");
        }

        for payload in [b"one\x07".as_slice(), b"two\x07".as_slice()] {
            let batch = panes
                .process_output_with_summary_retention(PaneId(20), payload, true)
                .expect("process output")
                .expect("bootstrapped pane update");
            assert_eq!(batch.batch_count, 1);
            assert_eq!(batch.effects.bells, 1);
        }
        let pending = panes.pending_update(PaneId(20)).unwrap();
        assert_eq!(pending.batch_count, 2);
        assert_eq!(pending.effects.bells, 2);

        panes.pane_view_mut(PaneId(20)).unwrap().finalize_changes(1);
        assert_eq!(panes.pending_update(PaneId(20)).unwrap().batch_count, 0);
    }

    #[test]
    fn a_split_composite_publishes_every_visible_pane_from_one_presented_generation() {
        let topology = split_topology();
        let layout = TmuxLayout::parse(SPLIT).expect("layout");
        let mut panes = TmuxPaneSet::new(1);
        panes.enable_presentation_tracking();
        let requests = panes.reconcile(&topology).expect("reconcile");
        for request in requests {
            panes
                .apply_bootstrap(
                    request.pane_id,
                    CommandStatus::Success,
                    &[format!("pane {}", request.pane_id.0).into_bytes()],
                    0,
                )
                .expect("bootstrap");
        }
        let (_, initial) = panes.capture_live_presentation_frames(&layout, Some(PaneId(20)));
        for frame in &initial {
            assert!(panes.apply_presented_frame(frame));
        }

        panes
            .process_output(PaneId(20), b"\x1b[?2026h\r\x1b[2Kleft partial")
            .expect("open the left transaction");
        panes
            .process_output(PaneId(21), b"\r\x1b[2Kright new")
            .expect("update the right pane");

        assert_eq!(panes.pane_view(PaneId(20)).unwrap().line(0), "pane 20");
        assert_eq!(panes.pane_view(PaneId(21)).unwrap().line(0), "pane 21");

        let (_, timed_out_composite) =
            panes.capture_live_presentation_frames(&layout, Some(PaneId(20)));
        for frame in &timed_out_composite {
            assert!(panes.apply_presented_frame(frame));
        }
        assert_eq!(panes.pane_view(PaneId(20)).unwrap().line(0), "left parti");
        assert_eq!(panes.pane_view(PaneId(21)).unwrap().line(0), "right new");
    }

    #[test]
    fn removed_panes_and_portals_remain_readable_until_the_replacement_is_presented() {
        let topology = split_topology();
        let layout = TmuxLayout::parse(SPLIT).expect("layout");
        let mut panes = TmuxPaneSet::new(1);
        panes.enable_presentation_tracking();
        for request in panes.reconcile(&topology).expect("reconcile") {
            panes
                .apply_bootstrap(
                    request.pane_id,
                    CommandStatus::Success,
                    &[format!("pane {}", request.pane_id.0).into_bytes()],
                    0,
                )
                .expect("bootstrap");
        }

        panes
            .set_pane_portal(PaneId(20), 2)
            .expect("install portal");
        let (portal_id, frames) = panes.capture_live_presentation_frames(&layout, Some(PaneId(20)));
        let portal_id = portal_id.expect("active portal id");
        for frame in &frames {
            assert!(panes.apply_presented_frame(frame));
        }
        panes.clear_pane_portal(PaneId(20), 2);
        assert!(panes.model_by_id_mut(portal_id).is_some());
        panes.retain_accessibility_views(&[portal_id]);
        assert!(panes.model_by_id_mut(portal_id).is_some());

        let pane_id = panes.pane_view(PaneId(20)).expect("left pane").view_id();
        let (_, frames) = panes.capture_live_presentation_frames(&layout, Some(PaneId(20)));
        for frame in &frames {
            assert!(panes.apply_presented_frame(frame));
        }
        panes
            .reconcile(&right_only_topology())
            .expect("remove left pane");
        assert!(panes.pane_view(PaneId(20)).is_none());
        assert!(panes.model_by_id_mut(pane_id).is_some());
        panes.retain_accessibility_views(&[pane_id]);
        assert!(panes.model_by_id_mut(pane_id).is_some());

        let replacement = panes.pane_view(PaneId(21)).expect("right pane").view_id();
        panes.retain_accessibility_views(&[replacement]);
        assert!(panes.model_by_id_mut(portal_id).is_none());
        assert!(panes.model_by_id_mut(pane_id).is_none());
    }

    #[test]
    fn resync_replacement_retires_the_previously_presented_pane_view() {
        let topology = split_topology();
        let layout = TmuxLayout::parse(SPLIT).expect("layout");
        let mut panes = TmuxPaneSet::new(1);
        panes.enable_presentation_tracking();
        for request in panes.reconcile(&topology).expect("reconcile") {
            panes
                .apply_bootstrap(
                    request.pane_id,
                    CommandStatus::Success,
                    &[format!("pane {}", request.pane_id.0).into_bytes()],
                    0,
                )
                .expect("bootstrap");
        }
        let (old_id, frames) = panes.capture_live_presentation_frames(&layout, Some(PaneId(20)));
        let old_id = old_id.expect("active pane id");
        for frame in &frames {
            assert!(panes.apply_presented_frame(frame));
        }

        panes
            .apply_bootstrap(
                PaneId(20),
                CommandStatus::Success,
                &[b"resynchronized".to_vec()],
                1,
            )
            .expect("replace pane view");
        let replacement_id = panes
            .pane_view(PaneId(20))
            .expect("replacement pane")
            .view_id();
        assert_ne!(old_id, replacement_id);
        assert!(panes.model_by_id_mut(old_id).is_some());
        panes.retain_accessibility_views(&[old_id]);
        assert!(panes.model_by_id_mut(old_id).is_some());
        panes.retain_accessibility_views(&[replacement_id]);
        assert!(panes.model_by_id_mut(old_id).is_none());
    }
}
