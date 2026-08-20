//! Engine-independent scene, presentation shadow, renderer, and render oracle.
//!
//! These contracts keep source-terminal state separate from physical-terminal
//! presentation. Every renderer change can therefore be proved against a
//! second Ghostty terminal without trusting one exact VT serialization.

use crate::terminal::{
    Cell, Color, Cursor, CursorShape, GhosttyEngine, Row, ScreenIdentity, Style, TerminalDamage,
    TerminalEvent, TerminalGeometry, TerminalModes, TerminalOperation, TerminalSnapshot,
    UnderlineStyle, UpdateSummary,
};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    path::PathBuf,
    sync::Arc,
};

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SurfaceId(pub u64);

/// Stable identity of a logical view whose accessibility state can outlive a
/// particular scene composition.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ViewId(pub u64);

/// Monotonic version of a view's authoritative terminal model.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ViewRevision(pub u64);

/// The exact view state represented by one physically presented surface.
///
/// Visible state is retained with the render transaction. Scrollback is
/// revisioned separately and shared only when it changed, so accessibility
/// remains generation-exact without copying the full history every frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PresentedViewFrame {
    pub view_id: ViewId,
    pub revision: ViewRevision,
    pub surface_id: SurfaceId,
    /// Visible state for this render generation. Scrollback rows are carried
    /// separately so unchanged history can be shared instead of copied for
    /// every frame.
    pub snapshot: TerminalSnapshot,
    pub history_revision: u64,
    pub history: Option<Arc<[Row]>>,
    /// The source update for this exact revision ended at an explicit
    /// synchronized-output commit boundary.
    pub explicitly_stable: bool,
}

/// Accessibility frames committed atomically with a physical render flush.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PresentedAccessibilityBundle {
    /// View which owns screen-relative accessibility commands for this scene.
    pub active_view: Option<ViewId>,
    /// Exact user-facing identity of the active view in this scene. Controller
    /// titles (notably tmux locations) can change without changing `ViewId`, so
    /// this label must cross the same flush boundary as the view frames.
    pub active_label: Option<String>,
    /// Terminal views derive their spoken label from the outer terminal's OSC
    /// title. That effect has its own flush receipt and may complete before the
    /// associated cell render under backpressure.
    pub active_label_tracks_terminal_title: bool,
    pub frames: Vec<PresentedViewFrame>,
}

impl PresentedAccessibilityBundle {
    pub fn new(active_view: Option<ViewId>, frames: Vec<PresentedViewFrame>) -> Self {
        Self {
            active_view,
            active_label: None,
            active_label_tracks_terminal_title: false,
            frames,
        }
    }

    pub fn with_active_label(
        mut self,
        active_label: impl Into<String>,
        tracks_terminal_title: bool,
    ) -> Self {
        self.active_label = Some(active_label.into());
        self.active_label_tracks_terminal_title = tracks_terminal_title;
        self
    }

    pub fn is_empty(&self) -> bool {
        self.active_view.is_none() && self.active_label.is_none() && self.frames.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct GridPoint {
    pub row: i32,
    pub col: i32,
}

impl GridPoint {
    pub const fn new(row: i32, col: i32) -> Self {
        Self { row, col }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct GridRect {
    pub origin: GridPoint,
    pub rows: u16,
    pub cols: u16,
}

impl GridRect {
    pub const fn new(origin: GridPoint, rows: u16, cols: u16) -> Self {
        Self { origin, rows, cols }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SceneSurface {
    pub id: SurfaceId,
    pub origin: GridPoint,
    pub snapshot: TerminalSnapshot,
}

impl SceneSurface {
    pub fn new(id: SurfaceId, origin: GridPoint, snapshot: TerminalSnapshot) -> Self {
        Self {
            id,
            origin,
            snapshot,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SceneOverlay {
    pub surface: SceneSurface,
    pub z_index: i32,
}

impl SceneOverlay {
    pub fn new(surface: SceneSurface, z_index: i32) -> Self {
        Self { surface, z_index }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorOwner {
    Pane(SurfaceId),
    Overlay(SurfaceId),
    #[default]
    Hidden,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PixelFormat {
    Rgb,
    #[default]
    Rgba,
    GrayAlpha,
    Gray,
}

/// Image state observable on the outer terminal.
///
/// The digest is over the decoded, uncompressed pixel bytes. Keeping only the
/// digest in the presentation shadow makes oracle artifacts small while still
/// detecting replacement of an image with the same dimensions and IDs.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct PresentedImage {
    pub image_id: u32,
    pub placement_id: u32,
    pub image_number: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub rendered_pixel_width: u32,
    pub rendered_pixel_height: u32,
    pub format: PixelFormat,
    pub data_len: usize,
    pub data_digest: u64,
    pub grid_rect: GridRect,
    pub x_offset: u32,
    pub y_offset: u32,
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub z_index: i32,
    pub virtual_placement: bool,
    pub visible: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaLimits {
    pub maximum_image_bytes: usize,
    pub maximum_pane_bytes: usize,
    pub maximum_scene_bytes: usize,
    pub maximum_placements: usize,
}

impl Default for MediaLimits {
    fn default() -> Self {
        Self {
            maximum_image_bytes: 32 * 1024 * 1024,
            maximum_pane_bytes: 64 * 1024 * 1024,
            maximum_scene_bytes: 128 * 1024 * 1024,
            maximum_placements: 4096,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SceneImageUpload {
    pub owner: SurfaceId,
    pub image_id: u32,
    pub image_number: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub format: PixelFormat,
    pub data: Arc<[u8]>,
    pub data_digest: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct PresentedImageUpload {
    image_id: u32,
    pixel_width: u32,
    pixel_height: u32,
    format: PixelFormat,
    data_len: usize,
    data_digest: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MediaSyncReport {
    pub uploads_added: usize,
    pub uploads_removed: usize,
    pub placements_added: usize,
    pub placements_removed: usize,
}

#[derive(Clone, Debug)]
struct PaneImageUpload {
    image_number: u32,
    pixel_width: u32,
    pixel_height: u32,
    format: PixelFormat,
    data: Arc<[u8]>,
    data_digest: u64,
}

/// Retained, pane-scoped decoded media. Uploads are deduplicated independently
/// from their placement state so hiding or moving a placement never duplicates
/// its pixel payload.
pub struct PaneMediaStore {
    limits: MediaLimits,
    uploads: BTreeMap<u32, PaneImageUpload>,
    placements: Vec<PresentedImage>,
    total_bytes: usize,
}

impl PaneMediaStore {
    pub fn new(limits: MediaLimits) -> Self {
        Self {
            limits,
            uploads: BTreeMap::new(),
            placements: Vec::new(),
            total_bytes: 0,
        }
    }

    pub const fn animation_exposed_by_engine() -> bool {
        false
    }

    pub fn synchronize(
        &mut self,
        placements: &[lector_ghostty::KittyImagePlacementSnapshot],
    ) -> Result<MediaSyncReport, PresentationError> {
        if placements.len() > self.limits.maximum_placements {
            return Err(PresentationError::MediaLimitExceeded {
                resource: "placements",
                requested: placements.len(),
                limit: self.limits.maximum_placements,
            });
        }

        let mut uploads = BTreeMap::new();
        let mut normalized = Vec::with_capacity(placements.len());
        let mut total_bytes = 0usize;
        for placement in placements {
            let (format, data) = normalize_pixel_data(placement.format, &placement.data);
            if data.len() > self.limits.maximum_image_bytes {
                return Err(PresentationError::MediaLimitExceeded {
                    resource: "image bytes",
                    requested: data.len(),
                    limit: self.limits.maximum_image_bytes,
                });
            }
            let digest = stable_digest(&data);
            match uploads.entry(placement.image_id) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    total_bytes = total_bytes.saturating_add(data.len());
                    let data = self
                        .uploads
                        .get(&placement.image_id)
                        .filter(|existing| {
                            existing.pixel_width == placement.pixel_width
                                && existing.pixel_height == placement.pixel_height
                                && existing.format == format
                                && existing.data_digest == digest
                        })
                        .map_or_else(|| Arc::from(data), |existing| Arc::clone(&existing.data));
                    entry.insert(PaneImageUpload {
                        image_number: placement.image_number,
                        pixel_width: placement.pixel_width,
                        pixel_height: placement.pixel_height,
                        format,
                        data,
                        data_digest: digest,
                    });
                }
                std::collections::btree_map::Entry::Occupied(entry) => {
                    let existing = entry.get();
                    if existing.pixel_width != placement.pixel_width
                        || existing.pixel_height != placement.pixel_height
                        || existing.format != format
                        || existing.data_digest != digest
                    {
                        return Err(PresentationError::InconsistentMediaUpload(
                            placement.image_id,
                        ));
                    }
                }
            }
            normalized.push(presented_image_from_ghostty(placement, format, digest));
        }
        if total_bytes > self.limits.maximum_pane_bytes {
            return Err(PresentationError::MediaLimitExceeded {
                resource: "pane image bytes",
                requested: total_bytes,
                limit: self.limits.maximum_pane_bytes,
            });
        }

        let report = MediaSyncReport {
            uploads_added: uploads
                .iter()
                .filter(|(id, upload)| {
                    self.uploads
                        .get(id)
                        .is_none_or(|existing| !pane_upload_matches(existing, upload))
                })
                .count(),
            uploads_removed: self
                .uploads
                .iter()
                .filter(|(id, upload)| {
                    uploads
                        .get(id)
                        .is_none_or(|replacement| !pane_upload_matches(upload, replacement))
                })
                .count(),
            placements_added: normalized
                .iter()
                .filter(|placement| !self.placements.contains(placement))
                .count(),
            placements_removed: self
                .placements
                .iter()
                .filter(|placement| !normalized.contains(placement))
                .count(),
        };
        self.uploads = uploads;
        self.placements = normalized;
        self.total_bytes = total_bytes;
        Ok(report)
    }

    pub fn append_to_scene(
        &self,
        owner: SurfaceId,
        origin: GridPoint,
        clip: GridRect,
        scene: &mut Scene,
    ) -> Result<(), PresentationError> {
        let existing_bytes = scene
            .image_uploads
            .iter()
            .map(|upload| upload.data.len())
            .sum::<usize>();
        let requested = existing_bytes.saturating_add(self.total_bytes);
        if requested > self.limits.maximum_scene_bytes {
            return Err(PresentationError::MediaLimitExceeded {
                resource: "scene image bytes",
                requested,
                limit: self.limits.maximum_scene_bytes,
            });
        }
        scene
            .image_uploads
            .extend(
                self.uploads
                    .iter()
                    .map(|(&image_id, upload)| SceneImageUpload {
                        owner,
                        image_id,
                        image_number: upload.image_number,
                        pixel_width: upload.pixel_width,
                        pixel_height: upload.pixel_height,
                        format: upload.format,
                        data: upload.data.clone(),
                        data_digest: upload.data_digest,
                    }),
            );
        for placement in &self.placements {
            let mut image = placement.clone();
            image.grid_rect.origin.row = image.grid_rect.origin.row.saturating_add(origin.row);
            image.grid_rect.origin.col = image.grid_rect.origin.col.saturating_add(origin.col);
            let Some(clipped) = clip_image_placement(&image, clip) else {
                continue;
            };
            scene.images.push(SceneImagePlacement {
                owner,
                image: clipped,
            });
        }
        Ok(())
    }

    pub const fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    pub fn upload_count(&self) -> usize {
        self.uploads.len()
    }

    pub fn placement_count(&self) -> usize {
        self.placements.len()
    }
}

fn pane_upload_matches(left: &PaneImageUpload, right: &PaneImageUpload) -> bool {
    left.pixel_width == right.pixel_width
        && left.pixel_height == right.pixel_height
        && left.format == right.format
        && left.data_digest == right.data_digest
}

/// A pane-scoped image placement in the logical scene. `owner` is retained
/// even when different panes reuse the same protocol image and placement IDs;
/// the renderer maps those namespaced IDs into the outer terminal's ID space.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct SceneImagePlacement {
    pub owner: SurfaceId,
    pub image: PresentedImage,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct TerminalWideEffects {
    pub title: Option<String>,
    pub working_directory: Option<String>,
    pub bell_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Scene {
    pub geometry: TerminalGeometry,
    pub panes: Vec<SceneSurface>,
    pub overlays: Vec<SceneOverlay>,
    pub cursor_owner: CursorOwner,
    pub image_uploads: Vec<SceneImageUpload>,
    pub images: Vec<SceneImagePlacement>,
    pub effects: TerminalWideEffects,
}

impl Scene {
    pub fn new(geometry: TerminalGeometry) -> Self {
        Self {
            geometry,
            panes: Vec::new(),
            overlays: Vec::new(),
            cursor_owner: CursorOwner::Hidden,
            image_uploads: Vec::new(),
            images: Vec::new(),
            effects: TerminalWideEffects::default(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PresentedScene {
    geometry: TerminalGeometry,
    rows: Arc<Vec<Row>>,
    cursor: Cursor,
    cursor_owner: CursorOwner,
    screen: ScreenIdentity,
    modes: TerminalModes,
    title: Option<String>,
    working_directory: Option<String>,
    image_uploads: Vec<PresentedImageUpload>,
    images: Vec<PresentedImage>,
    bell_count: usize,
}

impl PresentedScene {
    pub fn blank(geometry: TerminalGeometry) -> Self {
        Self {
            geometry,
            rows: Arc::new(blank_rows(geometry)),
            cursor: Cursor {
                visible: false,
                ..Cursor::default()
            },
            cursor_owner: CursorOwner::Hidden,
            screen: ScreenIdentity::Primary,
            modes: TerminalModes::default(),
            title: None,
            working_directory: None,
            image_uploads: Vec::new(),
            images: Vec::new(),
            bell_count: 0,
        }
    }

    pub fn from_engine(engine: &GhosttyEngine) -> Result<Self, PresentationError> {
        let snapshot = engine.normalized_snapshot();
        let images: Vec<PresentedImage> = engine
            .kitty_image_placements()?
            .into_iter()
            .map(|placement| PresentedImage {
                image_id: placement.image_id,
                placement_id: placement.placement_id,
                image_number: placement.image_number,
                pixel_width: placement.pixel_width,
                pixel_height: placement.pixel_height,
                rendered_pixel_width: placement.rendered_pixel_width,
                rendered_pixel_height: placement.rendered_pixel_height,
                format: match placement.format {
                    lector_ghostty::KittyImageFormatSnapshot::Rgb => PixelFormat::Rgb,
                    lector_ghostty::KittyImageFormatSnapshot::Rgba => PixelFormat::Rgba,
                    lector_ghostty::KittyImageFormatSnapshot::GrayAlpha => PixelFormat::GrayAlpha,
                    lector_ghostty::KittyImageFormatSnapshot::Gray => PixelFormat::Gray,
                },
                data_len: placement.data.len(),
                data_digest: stable_digest(&placement.data),
                grid_rect: GridRect::new(
                    GridPoint::new(placement.viewport_row, placement.viewport_col),
                    u16::try_from(placement.grid_rows).unwrap_or(u16::MAX),
                    u16::try_from(placement.grid_cols).unwrap_or(u16::MAX),
                ),
                x_offset: placement.x_offset,
                y_offset: placement.y_offset,
                source_x: placement.source_x,
                source_y: placement.source_y,
                source_width: placement.source_width,
                source_height: placement.source_height,
                z_index: placement.z_index,
                virtual_placement: placement.virtual_placement,
                visible: placement.visible,
            })
            .collect();
        let mut image_uploads = images
            .iter()
            .map(|image| PresentedImageUpload {
                image_id: image.image_id,
                pixel_width: image.pixel_width,
                pixel_height: image.pixel_height,
                format: image.format,
                data_len: image.data_len,
                data_digest: image.data_digest,
            })
            .collect::<Vec<_>>();
        image_uploads.sort_by_key(|upload| upload.image_id);
        image_uploads.dedup();
        Ok(Self::from_snapshot_and_images(
            snapshot,
            image_uploads,
            images,
        ))
    }

    fn from_snapshot_and_images(
        snapshot: TerminalSnapshot,
        image_uploads: Vec<PresentedImageUpload>,
        images: Vec<PresentedImage>,
    ) -> Self {
        Self {
            geometry: snapshot.geometry,
            rows: snapshot.rows,
            cursor: snapshot.cursor,
            cursor_owner: CursorOwner::Hidden,
            screen: snapshot.screen,
            modes: snapshot.modes,
            title: snapshot.title,
            working_directory: snapshot.working_directory,
            image_uploads,
            images,
            bell_count: 0,
        }
    }

    pub fn compose(scene: &Scene) -> Result<Self, PresentationError> {
        let mut presented = Self::blank(scene.geometry);
        let mut panes = scene.panes.iter();
        if let Some(first) = panes.next() {
            if first.origin == GridPoint::new(0, 0)
                && first.snapshot.geometry == scene.geometry
                && first.snapshot.rows.len() == usize::from(scene.geometry.rows)
            {
                presented.rows = first.snapshot.rows.clone();
            } else {
                let rows = Arc::make_mut(&mut presented.rows);
                blit_surface(rows, scene.geometry, first);
            }
        }
        for pane in panes {
            let rows = Arc::make_mut(&mut presented.rows);
            blit_surface(rows, scene.geometry, pane);
        }

        let mut overlays: Vec<(usize, &SceneOverlay)> = scene.overlays.iter().enumerate().collect();
        overlays.sort_by_key(|(order, overlay)| (overlay.z_index, *order));
        for (_, overlay) in overlays {
            let rows = Arc::make_mut(&mut presented.rows);
            blit_surface(rows, scene.geometry, &overlay.surface);
        }

        presented.cursor_owner = scene.cursor_owner;
        if let Some(surface) = cursor_surface(scene) {
            presented.screen = surface.snapshot.screen;
            presented.modes = surface.snapshot.modes.clone();
            let row = surface
                .origin
                .row
                .saturating_add(i32::from(surface.snapshot.cursor.row));
            let col = surface
                .origin
                .col
                .saturating_add(i32::from(surface.snapshot.cursor.col));
            let in_bounds = row >= 0
                && col >= 0
                && row < i32::from(scene.geometry.rows)
                && col < i32::from(scene.geometry.cols);
            presented.cursor = Cursor {
                row: u16::try_from(row.max(0)).unwrap_or(u16::MAX),
                col: u16::try_from(col.max(0)).unwrap_or(u16::MAX),
                visible: surface.snapshot.cursor.visible && in_bounds,
                shape: surface.snapshot.cursor.shape,
            };
        }
        presented.title.clone_from(&scene.effects.title);
        presented
            .working_directory
            .clone_from(&scene.effects.working_directory);
        presented.image_uploads = compose_image_uploads(scene);
        presented.images = compose_images(scene);
        presented.bell_count = scene.effects.bell_count;
        Ok(presented)
    }

    pub const fn geometry(&self) -> TerminalGeometry {
        self.geometry
    }

    pub fn row_text(&self, row: u16) -> String {
        self.rows
            .get(usize::from(row))
            .map_or_else(String::new, Row::contents)
    }

    pub const fn cursor_owner(&self) -> CursorOwner {
        self.cursor_owner
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn working_directory(&self) -> Option<&str> {
        self.working_directory.as_deref()
    }

    /// Advances terminal-wide state which was flushed independently of a
    /// cell render. Keeping this shadow current prevents later scene diffs
    /// from treating an already-visible OSC effect as unpresented.
    pub(crate) fn apply_terminal_effect(&mut self, event: &TerminalEvent) {
        match event {
            TerminalEvent::TitleChanged(title) => self.title = Some(title.clone()),
            TerminalEvent::WorkingDirectoryChanged(directory) => {
                self.working_directory = Some(directory.clone());
            }
            TerminalEvent::Bell
            | TerminalEvent::ClipboardWrite { .. }
            | TerminalEvent::DesktopNotification { .. }
            | TerminalEvent::ProgressReport { .. }
            | TerminalEvent::Query(_)
            | TerminalEvent::PtyReply(_)
            | TerminalEvent::UnknownSequence { .. } => {}
        }
    }

    pub fn images(&self) -> &[PresentedImage] {
        &self.images
    }

    pub const fn cursor(&self) -> Cursor {
        self.cursor
    }

    pub const fn screen(&self) -> ScreenIdentity {
        self.screen
    }

    pub fn into_terminal_snapshot(self) -> TerminalSnapshot {
        TerminalSnapshot {
            rows: self.rows,
            cursor: self.cursor,
            geometry: self.geometry,
            screen: self.screen,
            modes: self.modes,
            title: self.title,
            working_directory: self.working_directory,
            ..TerminalSnapshot::default()
        }
    }

    fn physically_matches(&self, other: &Self) -> bool {
        self.screen == other.screen && self.physical_fields_match(other)
    }

    fn physically_matches_in_owned_alternate(&self, other: &Self) -> bool {
        self.screen == ScreenIdentity::Alternate && self.physical_fields_match(other)
    }

    fn physical_fields_match(&self, other: &Self) -> bool {
        self.geometry == other.geometry
            && (Arc::ptr_eq(&self.rows, &other.rows)
                || rows_physically_match(&self.rows, &other.rows))
            && self.cursor == other.cursor
            && self.modes == other.modes
            && terminal_string_matches(&self.title, &other.title)
            && terminal_string_matches(&self.working_directory, &other.working_directory)
            && self.images == other.images
            && self.bell_count == other.bell_count
    }
}

fn terminal_string_matches(left: &Option<String>, right: &Option<String>) -> bool {
    left == right
        || left.as_deref().unwrap_or_default().is_empty()
            && right.as_deref().unwrap_or_default().is_empty()
}

fn rows_physically_match(left: &[Row], right: &[Row]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.wrapped == right.wrapped
                && left.cells.len() == right.cells.len()
                && left
                    .cells
                    .iter()
                    .zip(right.cells.iter())
                    .all(|(left, right)| {
                        let left_blank = matches!(left.grapheme.as_ref(), "" | " ");
                        let right_blank = matches!(right.grapheme.as_ref(), "" | " ");
                        (left.grapheme == right.grapheme || left_blank && right_blank)
                            && left.width == right.width
                            && left.continuation == right.continuation
                            && left.style == right.style
                            && left.hyperlink == right.hyperlink
                    })
        })
}

fn stable_digest(bytes: &[u8]) -> u64 {
    // FNV-1a is deterministic, tiny, and sufficient for regression identity;
    // this is not a security boundary.
    let mut digest = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        digest ^= u64::from(*byte);
        digest = digest.wrapping_mul(0x0000_0100_0000_01b3);
    }
    digest
}

fn normalize_pixel_data(
    format: lector_ghostty::KittyImageFormatSnapshot,
    data: &[u8],
) -> (PixelFormat, Vec<u8>) {
    match format {
        lector_ghostty::KittyImageFormatSnapshot::Rgb => (PixelFormat::Rgb, data.to_vec()),
        lector_ghostty::KittyImageFormatSnapshot::Rgba => (PixelFormat::Rgba, data.to_vec()),
        lector_ghostty::KittyImageFormatSnapshot::GrayAlpha => {
            let mut rgba = Vec::with_capacity(data.len().saturating_mul(2));
            for pair in data.as_chunks::<2>().0 {
                rgba.extend_from_slice(&[pair[0], pair[0], pair[0], pair[1]]);
            }
            (PixelFormat::Rgba, rgba)
        }
        lector_ghostty::KittyImageFormatSnapshot::Gray => {
            let mut rgb = Vec::with_capacity(data.len().saturating_mul(3));
            for &gray in data {
                rgb.extend_from_slice(&[gray, gray, gray]);
            }
            (PixelFormat::Rgb, rgb)
        }
    }
}

fn presented_image_from_ghostty(
    placement: &lector_ghostty::KittyImagePlacementSnapshot,
    format: PixelFormat,
    data_digest: u64,
) -> PresentedImage {
    PresentedImage {
        image_id: placement.image_id,
        placement_id: placement.placement_id,
        image_number: placement.image_number,
        pixel_width: placement.pixel_width,
        pixel_height: placement.pixel_height,
        rendered_pixel_width: placement.rendered_pixel_width,
        rendered_pixel_height: placement.rendered_pixel_height,
        format,
        data_len: placement.data.len(),
        data_digest,
        grid_rect: GridRect::new(
            GridPoint::new(placement.viewport_row, placement.viewport_col),
            u16::try_from(placement.grid_rows).unwrap_or(u16::MAX),
            u16::try_from(placement.grid_cols).unwrap_or(u16::MAX),
        ),
        x_offset: placement.x_offset,
        y_offset: placement.y_offset,
        source_x: placement.source_x,
        source_y: placement.source_y,
        source_width: placement.source_width,
        source_height: placement.source_height,
        z_index: placement.z_index,
        virtual_placement: placement.virtual_placement,
        visible: placement.visible,
    }
}

fn clip_image_placement(image: &PresentedImage, clip: GridRect) -> Option<PresentedImage> {
    let intersection = intersect_rect(image.grid_rect, clip)?;
    let mut clipped = image.clone();
    let top_cells = intersection
        .origin
        .row
        .saturating_sub(image.grid_rect.origin.row)
        .max(0) as u32;
    let left_cells = intersection
        .origin
        .col
        .saturating_sub(image.grid_rect.origin.col)
        .max(0) as u32;
    let original_rows = u32::from(image.grid_rect.rows.max(1));
    let original_cols = u32::from(image.grid_rect.cols.max(1));
    let source_width = if image.source_width == 0 {
        image.pixel_width
    } else {
        image.source_width
    };
    let source_height = if image.source_height == 0 {
        image.pixel_height
    } else {
        image.source_height
    };
    let left_pixels = source_width.saturating_mul(left_cells) / original_cols;
    let top_pixels = source_height.saturating_mul(top_cells) / original_rows;
    let kept_width = (source_width.saturating_mul(u32::from(intersection.cols)) / original_cols)
        .max(u32::from(source_width > 0));
    let kept_height = (source_height.saturating_mul(u32::from(intersection.rows)) / original_rows)
        .max(u32::from(source_height > 0));
    clipped.grid_rect = intersection;
    clipped.source_x = clipped.source_x.saturating_add(left_pixels);
    clipped.source_y = clipped.source_y.saturating_add(top_pixels);
    clipped.source_width = kept_width;
    clipped.source_height = kept_height;
    if left_cells > 0 {
        clipped.x_offset = 0;
    }
    if top_cells > 0 {
        clipped.y_offset = 0;
    }
    clipped.rendered_pixel_width = clipped
        .rendered_pixel_width
        .saturating_mul(u32::from(intersection.cols))
        / original_cols;
    clipped.rendered_pixel_height = clipped
        .rendered_pixel_height
        .saturating_mul(u32::from(intersection.rows))
        / original_rows;
    if image.rendered_pixel_width > 0 {
        clipped.rendered_pixel_width = clipped.rendered_pixel_width.max(1);
    }
    if image.rendered_pixel_height > 0 {
        clipped.rendered_pixel_height = clipped.rendered_pixel_height.max(1);
    }
    Some(clipped)
}

fn intersect_rect(left: GridRect, right: GridRect) -> Option<GridRect> {
    let top = left.origin.row.max(right.origin.row);
    let left_col = left.origin.col.max(right.origin.col);
    let bottom = left
        .origin
        .row
        .saturating_add(i32::from(left.rows))
        .min(right.origin.row.saturating_add(i32::from(right.rows)));
    let right_col = left
        .origin
        .col
        .saturating_add(i32::from(left.cols))
        .min(right.origin.col.saturating_add(i32::from(right.cols)));
    (bottom > top && right_col > left_col).then(|| {
        GridRect::new(
            GridPoint::new(top, left_col),
            u16::try_from(bottom - top).unwrap_or(u16::MAX),
            u16::try_from(right_col - left_col).unwrap_or(u16::MAX),
        )
    })
}

fn subtract_image_placement(image: &PresentedImage, occluder: GridRect) -> Vec<PresentedImage> {
    let Some(intersection) = intersect_rect(image.grid_rect, occluder) else {
        return vec![image.clone()];
    };
    let image_bottom = image
        .grid_rect
        .origin
        .row
        .saturating_add(i32::from(image.grid_rect.rows));
    let image_right = image
        .grid_rect
        .origin
        .col
        .saturating_add(i32::from(image.grid_rect.cols));
    let intersection_bottom = intersection
        .origin
        .row
        .saturating_add(i32::from(intersection.rows));
    let intersection_right = intersection
        .origin
        .col
        .saturating_add(i32::from(intersection.cols));
    let candidates = [
        GridRect::new(
            image.grid_rect.origin,
            u16::try_from(intersection.origin.row - image.grid_rect.origin.row).unwrap_or(0),
            image.grid_rect.cols,
        ),
        GridRect::new(
            GridPoint::new(intersection_bottom, image.grid_rect.origin.col),
            u16::try_from(image_bottom - intersection_bottom).unwrap_or(0),
            image.grid_rect.cols,
        ),
        GridRect::new(
            GridPoint::new(intersection.origin.row, image.grid_rect.origin.col),
            intersection.rows,
            u16::try_from(intersection.origin.col - image.grid_rect.origin.col).unwrap_or(0),
        ),
        GridRect::new(
            GridPoint::new(intersection.origin.row, intersection_right),
            intersection.rows,
            u16::try_from(image_right - intersection_right).unwrap_or(0),
        ),
    ];
    candidates
        .into_iter()
        .filter(|rect| rect.rows > 0 && rect.cols > 0)
        .filter_map(|rect| clip_image_placement(image, rect))
        .collect()
}

#[derive(Clone)]
struct ComposedImage {
    owner: SurfaceId,
    source_image_id: u32,
    image: PresentedImage,
}

fn compose_images(scene: &Scene) -> Vec<PresentedImage> {
    compose_image_records(scene)
        .into_iter()
        .map(|record| record.image)
        .collect()
}

fn compose_image_uploads(scene: &Scene) -> Vec<PresentedImageUpload> {
    let image_ids = outer_image_ids(scene);
    let mut uploads = BTreeMap::new();
    for upload in &scene.image_uploads {
        let key = (upload.owner, upload.image_id, upload.data_digest);
        let Some(&image_id) = image_ids.get(&key) else {
            continue;
        };
        uploads.insert(
            image_id,
            PresentedImageUpload {
                image_id,
                pixel_width: upload.pixel_width,
                pixel_height: upload.pixel_height,
                format: upload.format,
                data_len: upload.data.len(),
                data_digest: upload.data_digest,
            },
        );
    }
    uploads.into_values().collect()
}

fn outer_image_ids(scene: &Scene) -> BTreeMap<(SurfaceId, u32, u64), u32> {
    let mut keys = scene
        .image_uploads
        .iter()
        .map(|upload| (upload.owner, upload.image_id, upload.data_digest))
        .collect::<BTreeSet<_>>();
    for placement in &scene.images {
        let digest = scene
            .image_uploads
            .iter()
            .find(|upload| {
                upload.owner == placement.owner && upload.image_id == placement.image.image_id
            })
            .map_or(placement.image.data_digest, |upload| upload.data_digest);
        keys.insert((placement.owner, placement.image.image_id, digest));
    }

    let mut used = BTreeSet::new();
    keys.into_iter()
        .map(|key @ (owner, source_image_id, digest)| {
            let outer = allocate_outer_id(
                stable_namespace_seed(owner, source_image_id, digest, 0x494d_4147_4500_0001),
                &mut used,
            );
            (key, outer)
        })
        .collect()
}

fn compose_image_records(scene: &Scene) -> Vec<ComposedImage> {
    let scene_bounds = GridRect::new(
        GridPoint::new(0, 0),
        scene.geometry.rows,
        scene.geometry.cols,
    );
    let mut overlay_layers: Vec<(i32, usize, SurfaceId, GridRect)> = scene
        .overlays
        .iter()
        .enumerate()
        .map(|(order, overlay)| {
            (
                overlay.z_index,
                order,
                overlay.surface.id,
                GridRect::new(
                    overlay.surface.origin,
                    overlay.surface.snapshot.geometry.rows,
                    overlay.surface.snapshot.geometry.cols,
                ),
            )
        })
        .collect();
    overlay_layers.sort_by_key(|(z, order, _, _)| (*z, *order));

    let mut used_placement_ids = BTreeSet::new();
    let image_ids = outer_image_ids(scene);
    let mut result = Vec::new();
    for placement in &scene.images {
        let Some(image) = clip_image_placement(&placement.image, scene_bounds) else {
            continue;
        };
        let owner_layer = overlay_layers
            .iter()
            .position(|(_, _, id, _)| *id == placement.owner);
        if !image.visible {
            continue;
        }
        let mut fragments = vec![image];
        for (index, (_, _, _, rect)) in overlay_layers.iter().enumerate() {
            if owner_layer.is_some_and(|owner| index <= owner) {
                continue;
            }
            fragments = fragments
                .iter()
                .flat_map(|fragment| subtract_image_placement(fragment, *rect))
                .collect();
            if fragments.is_empty() {
                break;
            }
        }
        let upload = scene.image_uploads.iter().find(|upload| {
            upload.owner == placement.owner && upload.image_id == placement.image.image_id
        });
        let digest = upload.map_or(placement.image.data_digest, |upload| upload.data_digest);
        let source_image_id = placement.image.image_id;
        let outer_image_id = image_ids[&(placement.owner, source_image_id, digest)];
        for mut fragment in fragments {
            if let Some(upload) = upload {
                fragment.data_len = upload.data.len();
                fragment.format = upload.format;
                fragment.pixel_width = upload.pixel_width;
                fragment.pixel_height = upload.pixel_height;
            }
            // Pane engines may not know the physical cell size even when the
            // outer terminal does. Kitty derives a placement's rendered pixel
            // dimensions from its cell rectangle in that case, so keep the
            // presentation shadow in the same physical units.
            if fragment.rendered_pixel_width == 0 && scene.geometry.cell_width_px > 0 {
                fragment.rendered_pixel_width =
                    u32::from(fragment.grid_rect.cols).saturating_mul(scene.geometry.cell_width_px);
            }
            if fragment.rendered_pixel_height == 0 && scene.geometry.cell_height_px > 0 {
                fragment.rendered_pixel_height = u32::from(fragment.grid_rect.rows)
                    .saturating_mul(scene.geometry.cell_height_px);
            }
            fragment.image_id = outer_image_id;
            fragment.placement_id = allocate_outer_id(
                stable_namespace_seed(
                    placement.owner,
                    placement.image.placement_id,
                    placement_fragment_digest(&fragment),
                    0x504c_4143_4500_0001,
                ),
                &mut used_placement_ids,
            );
            result.push(ComposedImage {
                owner: placement.owner,
                source_image_id,
                image: fragment,
            });
        }
    }
    result.sort_by_key(|record| {
        (
            record.image.z_index,
            record.image.image_id,
            record.image.placement_id,
        )
    });
    result
}

fn placement_fragment_digest(image: &PresentedImage) -> u64 {
    let mut bytes = Vec::with_capacity(48);
    bytes.extend_from_slice(&image.grid_rect.origin.row.to_le_bytes());
    bytes.extend_from_slice(&image.grid_rect.origin.col.to_le_bytes());
    bytes.extend_from_slice(&image.grid_rect.rows.to_le_bytes());
    bytes.extend_from_slice(&image.grid_rect.cols.to_le_bytes());
    bytes.extend_from_slice(&image.source_x.to_le_bytes());
    bytes.extend_from_slice(&image.source_y.to_le_bytes());
    bytes.extend_from_slice(&image.source_width.to_le_bytes());
    bytes.extend_from_slice(&image.source_height.to_le_bytes());
    bytes.extend_from_slice(&image.x_offset.to_le_bytes());
    bytes.extend_from_slice(&image.y_offset.to_le_bytes());
    bytes.extend_from_slice(&image.z_index.to_le_bytes());
    bytes.push(u8::from(image.virtual_placement));
    stable_digest(&bytes)
}

fn stable_namespace_seed(owner: SurfaceId, source_id: u32, extra: u64, tag: u64) -> u64 {
    let mut bytes = [0u8; 28];
    bytes[..8].copy_from_slice(&owner.0.to_le_bytes());
    bytes[8..12].copy_from_slice(&source_id.to_le_bytes());
    bytes[12..20].copy_from_slice(&extra.to_le_bytes());
    bytes[20..].copy_from_slice(&tag.to_le_bytes());
    stable_digest(&bytes)
}

fn allocate_outer_id(seed: u64, used: &mut BTreeSet<u32>) -> u32 {
    let mut id = (seed as u32).max(1);
    while !used.insert(id) {
        id = id.wrapping_add(1).max(1);
    }
    id
}

fn blank_rows(geometry: TerminalGeometry) -> Vec<Row> {
    (0..geometry.rows)
        .map(|_| Row {
            cells: Arc::new(vec![Cell::default(); usize::from(geometry.cols)]),
            wrapped: false,
        })
        .collect()
}

fn blit_surface(target: &mut [Row], geometry: TerminalGeometry, surface: &SceneSurface) {
    for (source_row_index, source_row) in surface.snapshot.rows.iter().enumerate() {
        let Ok(source_row_index) = i32::try_from(source_row_index) else {
            break;
        };
        let target_row = surface.origin.row.saturating_add(source_row_index);
        if target_row < 0 || target_row >= i32::from(geometry.rows) {
            continue;
        }
        let target_row = usize::try_from(target_row).expect("non-negative clipped row");
        for (source_col_index, cell) in source_row.cells.iter().enumerate() {
            let Ok(source_col_index) = i32::try_from(source_col_index) else {
                break;
            };
            let target_col = surface.origin.col.saturating_add(source_col_index);
            if target_col < 0 || target_col >= i32::from(geometry.cols) {
                continue;
            }
            let target_col = usize::try_from(target_col).expect("non-negative clipped column");
            Arc::make_mut(&mut target[target_row].cells)[target_col] = cell.clone();
        }
        if surface.origin.col <= 0
            && surface
                .origin
                .col
                .saturating_add(i32::from(surface.snapshot.geometry.cols))
                >= i32::from(geometry.cols)
        {
            target[target_row].wrapped = source_row.wrapped;
        }
    }
}

fn cursor_surface(scene: &Scene) -> Option<&SceneSurface> {
    match scene.cursor_owner {
        CursorOwner::Pane(id) => scene.panes.iter().find(|surface| surface.id == id),
        CursorOwner::Overlay(id) => scene
            .overlays
            .iter()
            .find(|overlay| overlay.surface.id == id)
            .map(|overlay| &overlay.surface),
        CursorOwner::Hidden => None,
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SceneDamage {
    #[default]
    None,
    Full,
    Cursor,
    Resize {
        previous: TerminalGeometry,
        next: TerminalGeometry,
    },
    Surfaces(Vec<SurfaceId>),
    Regions(Vec<GridRect>),
    Operations {
        owner: SurfaceId,
        regions: Vec<GridRect>,
        operations: Vec<SceneOperation>,
    },
    Effects,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SceneOperation {
    ScrollUp { region: GridRect, count: u16 },
    ScrollDown { region: GridRect, count: u16 },
    InsertLines { region: GridRect, count: u16 },
    DeleteLines { region: GridRect, count: u16 },
    InsertChars { region: GridRect, count: u16 },
    DeleteChars { region: GridRect, count: u16 },
    EraseChars { region: GridRect },
    WriteRun { origin: GridPoint, text: String },
}

impl SceneDamage {
    pub fn regions(regions: impl IntoIterator<Item = GridRect>) -> Self {
        Self::Regions(
            regions
                .into_iter()
                .filter(|region| region.rows > 0 && region.cols > 0)
                .collect(),
        )
    }

    /// Map pane-local Ghostty row damage into compositor coordinates.
    pub fn from_terminal_damage(
        surface: &SceneSurface,
        damage: &TerminalDamage,
        scene_geometry: TerminalGeometry,
    ) -> Self {
        match damage {
            TerminalDamage::Full => Self::Full,
            TerminalDamage::None => Self::Cursor,
            TerminalDamage::Rows(ranges) => {
                let regions = ranges.iter().filter_map(|range| {
                    let start =
                        (*range.start()).min(surface.snapshot.geometry.rows.saturating_sub(1));
                    let end = (*range.end()).min(surface.snapshot.geometry.rows.saturating_sub(1));
                    if start > end {
                        return None;
                    }
                    clip_grid_rect(
                        GridRect::new(
                            GridPoint::new(
                                surface.origin.row.saturating_add(i32::from(start)),
                                surface.origin.col,
                            ),
                            end.saturating_sub(start).saturating_add(1),
                            surface.snapshot.geometry.cols,
                        ),
                        scene_geometry,
                    )
                });
                let result = Self::regions(regions);
                if matches!(&result, Self::Regions(regions) if regions.is_empty()) {
                    Self::Cursor
                } else {
                    result
                }
            }
        }
    }

    /// Preserve validated, non-authoritative operation hints alongside the
    /// ordinary dirty regions used when a structural fast path is unsafe.
    pub fn from_terminal_update(
        surface: &SceneSurface,
        update: &UpdateSummary,
        scene_geometry: TerminalGeometry,
    ) -> Self {
        if update.operations.is_empty() {
            return Self::from_terminal_damage(surface, &update.damage, scene_geometry);
        }
        let mut regions = terminal_damage_regions(surface, &update.damage, scene_geometry);
        let operations: Vec<_> = update
            .operations
            .iter()
            .map(|operation| map_terminal_operation(surface, operation))
            .collect();
        for operation in &operations {
            let region = scene_operation_region(operation);
            if let Some(region) = clip_grid_rect(region, scene_geometry) {
                regions.push(region);
            }
        }
        normalize_grid_regions(&mut regions);
        Self::Operations {
            owner: surface.id,
            regions,
            operations,
        }
    }

    /// Merge pane-local damage from one PTY drain without discarding its
    /// compositor bounds. Structural operation hints cannot be replayed
    /// across multiple owners, so their affected regions join the ordinary
    /// cell diff instead.
    pub fn from_terminal_updates<'a>(
        updates: impl IntoIterator<Item = (&'a SceneSurface, &'a UpdateSummary)>,
        scene_geometry: TerminalGeometry,
    ) -> Self {
        let mut regions = Vec::new();
        for (surface, update) in updates {
            regions.extend(terminal_damage_regions(
                surface,
                &update.damage,
                scene_geometry,
            ));
            for operation in &update.operations {
                let operation = map_terminal_operation(surface, operation);
                if let Some(region) =
                    clip_grid_rect(scene_operation_region(&operation), scene_geometry)
                {
                    regions.push(region);
                }
            }
        }
        normalize_grid_regions(&mut regions);
        if regions.is_empty() {
            Self::Cursor
        } else {
            Self::Regions(regions)
        }
    }
}

fn scene_operation_region(operation: &SceneOperation) -> GridRect {
    match operation {
        SceneOperation::ScrollUp { region, .. }
        | SceneOperation::ScrollDown { region, .. }
        | SceneOperation::InsertLines { region, .. }
        | SceneOperation::DeleteLines { region, .. }
        | SceneOperation::InsertChars { region, .. }
        | SceneOperation::DeleteChars { region, .. }
        | SceneOperation::EraseChars { region } => *region,
        SceneOperation::WriteRun { origin, text } => GridRect::new(
            *origin,
            1,
            text.chars().count().try_into().unwrap_or(u16::MAX),
        ),
    }
}

fn terminal_damage_regions(
    surface: &SceneSurface,
    damage: &TerminalDamage,
    scene_geometry: TerminalGeometry,
) -> Vec<GridRect> {
    match damage {
        TerminalDamage::None => Vec::new(),
        TerminalDamage::Full => clip_grid_rect(
            GridRect::new(
                surface.origin,
                surface.snapshot.geometry.rows,
                surface.snapshot.geometry.cols,
            ),
            scene_geometry,
        )
        .into_iter()
        .collect(),
        TerminalDamage::Rows(ranges) => ranges
            .iter()
            .filter_map(|range| {
                let start = (*range.start()).min(surface.snapshot.geometry.rows.saturating_sub(1));
                let end = (*range.end()).min(surface.snapshot.geometry.rows.saturating_sub(1));
                (start <= end)
                    .then(|| {
                        GridRect::new(
                            GridPoint::new(
                                surface.origin.row.saturating_add(i32::from(start)),
                                surface.origin.col,
                            ),
                            end.saturating_sub(start).saturating_add(1),
                            surface.snapshot.geometry.cols,
                        )
                    })
                    .and_then(|region| clip_grid_rect(region, scene_geometry))
            })
            .collect(),
    }
}

fn map_terminal_operation(surface: &SceneSurface, operation: &TerminalOperation) -> SceneOperation {
    let row_region = |top: u16, bottom: u16| {
        GridRect::new(
            GridPoint::new(
                surface.origin.row.saturating_add(i32::from(top)),
                surface.origin.col,
            ),
            bottom.saturating_sub(top).saturating_add(1),
            surface.snapshot.geometry.cols,
        )
    };
    let char_region = |row: u16, col: u16, count: Option<u16>| {
        GridRect::new(
            GridPoint::new(
                surface.origin.row.saturating_add(i32::from(row)),
                surface.origin.col.saturating_add(i32::from(col)),
            ),
            1,
            count.unwrap_or_else(|| surface.snapshot.geometry.cols.saturating_sub(col)),
        )
    };
    match operation {
        TerminalOperation::ScrollUp { top, bottom, count } => SceneOperation::ScrollUp {
            region: row_region(*top, *bottom),
            count: *count,
        },
        TerminalOperation::ScrollDown { top, bottom, count } => SceneOperation::ScrollDown {
            region: row_region(*top, *bottom),
            count: *count,
        },
        TerminalOperation::InsertLines { row, bottom, count } => SceneOperation::InsertLines {
            region: row_region(*row, *bottom),
            count: *count,
        },
        TerminalOperation::DeleteLines { row, bottom, count } => SceneOperation::DeleteLines {
            region: row_region(*row, *bottom),
            count: *count,
        },
        TerminalOperation::InsertChars { row, col, count } => SceneOperation::InsertChars {
            region: char_region(*row, *col, None),
            count: *count,
        },
        TerminalOperation::DeleteChars { row, col, count } => SceneOperation::DeleteChars {
            region: char_region(*row, *col, None),
            count: *count,
        },
        TerminalOperation::EraseChars { row, col, count } => SceneOperation::EraseChars {
            region: char_region(*row, *col, Some(*count)),
        },
        TerminalOperation::WriteRun { row, col, text } => SceneOperation::WriteRun {
            origin: GridPoint::new(
                surface.origin.row.saturating_add(i32::from(*row)),
                surface.origin.col.saturating_add(i32::from(*col)),
            ),
            text: text.clone(),
        },
    }
}

fn normalize_grid_regions(regions: &mut Vec<GridRect>) {
    regions.sort_unstable_by_key(|region| (region.origin.row, region.origin.col));
    regions.dedup();
}

fn clip_grid_rect(rect: GridRect, geometry: TerminalGeometry) -> Option<GridRect> {
    let top = rect.origin.row.max(0).min(i32::from(geometry.rows));
    let left = rect.origin.col.max(0).min(i32::from(geometry.cols));
    let bottom = rect
        .origin
        .row
        .saturating_add(i32::from(rect.rows))
        .max(0)
        .min(i32::from(geometry.rows));
    let right = rect
        .origin
        .col
        .saturating_add(i32::from(rect.cols))
        .max(0)
        .min(i32::from(geometry.cols));
    (bottom > top && right > left).then(|| {
        GridRect::new(
            GridPoint::new(top, left),
            u16::try_from(bottom - top).expect("clipped row extent fits u16"),
            u16::try_from(right - left).expect("clipped column extent fits u16"),
        )
    })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OutputTransaction {
    pub resize: Option<TerminalGeometry>,
    pub bytes: Vec<u8>,
}

impl OutputTransaction {
    pub fn new(bytes: impl AsRef<[u8]>) -> Self {
        Self {
            resize: None,
            bytes: bytes.as_ref().to_vec(),
        }
    }

    pub fn with_resize(geometry: TerminalGeometry, bytes: impl AsRef<[u8]>) -> Self {
        Self {
            resize: Some(geometry),
            bytes: bytes.as_ref().to_vec(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RenderBatch {
    pub transactions: Vec<OutputTransaction>,
    pub predicted: PresentedScene,
}

impl RenderBatch {
    pub fn new(transactions: Vec<OutputTransaction>, predicted: PresentedScene) -> Self {
        Self {
            transactions,
            predicted,
        }
    }
}

pub trait RendererBackend {
    fn render(
        &mut self,
        scene: &Scene,
        damage: &SceneDamage,
        presented: &PresentedScene,
    ) -> Result<RenderBatch, PresentationError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderCapabilities {
    /// The outer terminal has explicitly reported support for DEC mode 2026.
    pub synchronized_output: bool,
    /// OSC 8 is supported by the outer terminal.
    pub hyperlinks: bool,
    /// Kitty graphics APC upload and placement commands are supported.
    pub kitty_graphics: bool,
    /// Title, working-directory, and related effects are encoded inside render
    /// transactions. Scheduled mode disables this so typed scheduler work owns
    /// the physical effect boundary.
    pub inline_terminal_effects: bool,
}

impl Default for RenderCapabilities {
    fn default() -> Self {
        Self {
            synchronized_output: false,
            hyperlinks: true,
            kitty_graphics: false,
            inline_terminal_effects: true,
        }
    }
}

/// A correctness-first renderer that reconstructs the complete scene from
/// modeled terminal state. It never consumes or replays source application
/// bytes. It remains the correctness fallback for incremental presentation.
pub struct FullSceneVtRenderer {
    capabilities: RenderCapabilities,
}

impl FullSceneVtRenderer {
    pub const fn new(capabilities: RenderCapabilities) -> Self {
        Self { capabilities }
    }

    pub fn set_capabilities(&mut self, capabilities: RenderCapabilities) {
        self.capabilities = capabilities;
    }

    fn encode(
        &self,
        scene: &Scene,
        intended: &PresentedScene,
        retained_uploads: Option<&[PresentedImageUpload]>,
    ) -> Result<Vec<u8>, PresentationError> {
        let mut bytes = Vec::new();
        if self.capabilities.synchronized_output {
            bytes.extend_from_slice(b"\x1b[?2026h");
        }

        // The physical lifecycle owns one outer alternate screen for the
        // entire Lector session. `intended.screen` remains the active child
        // screen identity; changing it reconstructs that logical state here
        // without ever switching the host terminal's screen.
        // Establish a complete, capability-safe baseline. Horizontal margins
        // are reset while DECLRMM is enabled and then disabled again, avoiding
        // CSI s's save-cursor meaning outside that mode.
        bytes.extend_from_slice(b"\x1b[?6l\x1b[?7h\x1b[4l\x1b[r\x1b[?69h");
        bytes.extend_from_slice(format!("\x1b[1;{}s", intended.geometry.cols).as_bytes());
        bytes.extend_from_slice(b"\x1b[?69l\x1b[0m\x1b]8;;\x1b\\");
        if self.capabilities.kitty_graphics {
            if let Some(retained_uploads) = retained_uploads {
                // Placement state is rebuilt independently from the upload
                // cache. Unreferenced pixel data is explicitly released while
                // still-live pane uploads survive overlays and scene switches.
                bytes.extend_from_slice(b"\x1b_Ga=d,d=a\x1b\\");
                for stale in retained_uploads
                    .iter()
                    .filter(|upload| !intended.image_uploads.contains(upload))
                {
                    bytes.extend_from_slice(
                        format!("\x1b_Ga=d,d=I,i={}\x1b\\", stale.image_id).as_bytes(),
                    );
                }
            } else {
                // An uncertain physical shadow must discard unknown data as
                // well as placements before reconstructing the scene.
                bytes.extend_from_slice(b"\x1b_Ga=d,d=A\x1b\\");
            }
        }
        if retained_uploads.is_some() {
            // CSI 2 J is specified to delete intersecting Kitty resources in
            // supporting terminals. Rewriting every modeled cell provides the
            // same text reconciliation without discarding the upload cache.
            bytes.extend_from_slice(b"\x1b[H");
        } else {
            bytes.extend_from_slice(b"\x1b[2J\x1b[H");
        }

        if self.capabilities.inline_terminal_effects {
            write_terminal_string(&mut bytes, 2, intended.title.as_deref());
            write_terminal_string(&mut bytes, 7, intended.working_directory.as_deref());
        }
        write_full_rows(&mut bytes, intended);
        if self.capabilities.kitty_graphics {
            write_kitty_images(
                &mut bytes,
                scene,
                intended,
                retained_uploads.unwrap_or_default(),
            )?;
        }

        bytes.extend_from_slice(b"\x1b]8;;\x1b\\\x1b[0m");
        write_cursor(&mut bytes, intended.cursor);
        write_modes(
            &mut bytes,
            &intended.modes,
            self.capabilities.synchronized_output,
        );
        bytes.extend(std::iter::repeat_n(b'\x07', intended.bell_count));
        Ok(bytes)
    }

    fn render_with_retained_uploads(
        &mut self,
        scene: &Scene,
        presented: &PresentedScene,
        retain_known_uploads: bool,
    ) -> Result<RenderBatch, PresentationError> {
        let mut predicted = PresentedScene::compose(scene)?;
        apply_render_capabilities(&mut predicted, self.capabilities);
        let retain_known_uploads = retain_known_uploads
            && presented.geometry == predicted.geometry
            && presented.screen == predicted.screen;
        let retained_uploads = retain_known_uploads.then_some(presented.image_uploads.as_slice());
        let bytes = self.encode(scene, &predicted, retained_uploads)?;
        let transaction = if presented.geometry != scene.geometry {
            OutputTransaction::with_resize(scene.geometry, bytes)
        } else {
            OutputTransaction::new(bytes)
        };
        Ok(RenderBatch::new(vec![transaction], predicted))
    }
}

impl RendererBackend for FullSceneVtRenderer {
    fn render(
        &mut self,
        scene: &Scene,
        _damage: &SceneDamage,
        presented: &PresentedScene,
    ) -> Result<RenderBatch, PresentationError> {
        self.render_with_retained_uploads(scene, presented, false)
    }
}

fn apply_render_capabilities(presented: &mut PresentedScene, capabilities: RenderCapabilities) {
    if !capabilities.hyperlinks
        && presented
            .rows
            .iter()
            .any(|row| row.cells.iter().any(|cell| cell.hyperlink.is_some()))
    {
        for row in Arc::make_mut(&mut presented.rows) {
            if row.cells.iter().any(|cell| cell.hyperlink.is_some()) {
                for cell in Arc::make_mut(&mut row.cells) {
                    cell.hyperlink = None;
                }
            }
        }
    }
    if !capabilities.kitty_graphics {
        presented.image_uploads.clear();
        presented.images.clear();
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RenderStrategy {
    #[default]
    Noop,
    Incremental,
    SemanticFastPath,
    FullFallback,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RenderStats {
    pub rows_considered: usize,
    pub cells_compared: usize,
    pub cells_emitted: usize,
    pub bytes_emitted: usize,
}

/// A physical-terminal renderer that treats Ghostty damage as a candidate
/// region, diffs only those cells against the confirmed presentation shadow,
/// and retains the full-scene renderer as its correctness fallback.
pub struct IncrementalVtRenderer {
    capabilities: RenderCapabilities,
    full: FullSceneVtRenderer,
    confirmed: Option<PresentedScene>,
    last_strategy: RenderStrategy,
    last_stats: RenderStats,
}

impl IncrementalVtRenderer {
    pub const fn new(capabilities: RenderCapabilities) -> Self {
        Self {
            capabilities,
            full: FullSceneVtRenderer::new(capabilities),
            confirmed: None,
            last_strategy: RenderStrategy::Noop,
            last_stats: RenderStats {
                rows_considered: 0,
                cells_compared: 0,
                cells_emitted: 0,
                bytes_emitted: 0,
            },
        }
    }

    pub fn set_capabilities(&mut self, capabilities: RenderCapabilities) {
        if self.capabilities != capabilities {
            self.invalidate();
        }
        self.capabilities = capabilities;
        self.full.set_capabilities(capabilities);
    }

    /// Confirm that every byte in the preceding batch reached the physical
    /// terminal. Until confirmation, the next render uses the full fallback.
    pub fn confirm(&mut self, presented: &PresentedScene) {
        self.confirmed = Some(presented.clone());
    }

    /// Mark the physical shadow uncertain after a partial/failed write,
    /// suspend/resume, external corruption, or renderer exception.
    pub fn invalidate(&mut self) {
        self.confirmed = None;
    }

    pub const fn last_strategy(&self) -> RenderStrategy {
        self.last_strategy
    }

    pub const fn last_stats(&self) -> RenderStats {
        self.last_stats
    }

    fn full_fallback(
        &mut self,
        scene: &Scene,
        presented: &PresentedScene,
        retain_known_uploads: bool,
    ) -> Result<RenderBatch, PresentationError> {
        self.last_strategy = RenderStrategy::FullFallback;
        let result = self
            .full
            .render_with_retained_uploads(scene, presented, retain_known_uploads);
        match result {
            Ok(batch) => {
                self.last_stats = RenderStats {
                    rows_considered: usize::from(scene.geometry.rows),
                    cells_compared: usize::from(scene.geometry.rows)
                        .saturating_mul(usize::from(scene.geometry.cols)),
                    cells_emitted: usize::from(scene.geometry.rows)
                        .saturating_mul(usize::from(scene.geometry.cols)),
                    bytes_emitted: batch
                        .transactions
                        .iter()
                        .map(|transaction| transaction.bytes.len())
                        .sum(),
                };
                Ok(batch)
            }
            Err(error) => {
                self.invalidate();
                Err(error)
            }
        }
    }

    fn can_increment(
        &self,
        damage: &SceneDamage,
        presented: &PresentedScene,
        intended: &PresentedScene,
        confirmed_matches_presented: bool,
    ) -> bool {
        confirmed_matches_presented
            && presented.geometry == intended.geometry
            && presented.screen == intended.screen
            && presented.image_uploads == intended.image_uploads
            && presented.images == intended.images
            && !matches!(
                damage,
                SceneDamage::Full | SceneDamage::Resize { .. } | SceneDamage::Surfaces(_)
            )
    }

    fn incremental_batch(
        &mut self,
        damage: &SceneDamage,
        presented: &PresentedScene,
        intended: PresentedScene,
    ) -> Option<RenderBatch> {
        let mut row_candidates = vec![None::<(usize, usize)>; intended.rows.len()];
        let regions = match damage {
            SceneDamage::Regions(regions) | SceneDamage::Operations { regions, .. } => {
                Some(regions)
            }
            _ => None,
        };
        if let Some(regions) = regions {
            for region in regions {
                let Some(region) = clip_grid_rect(*region, intended.geometry) else {
                    continue;
                };
                let row_start = usize::try_from(region.origin.row).ok()?;
                let col_start = usize::try_from(region.origin.col).ok()?;
                let row_end = row_start.saturating_add(usize::from(region.rows));
                let col_end = col_start.saturating_add(usize::from(region.cols));
                for candidate in row_candidates.get_mut(row_start..row_end)? {
                    *candidate = Some(match *candidate {
                        Some((start, end)) => (start.min(col_start), end.max(col_end)),
                        None => (col_start, col_end),
                    });
                }
            }
        }

        let mut body = Vec::new();
        let mut stats = RenderStats::default();
        let mut wrote_cells = false;
        for (row_index, candidate) in row_candidates.into_iter().enumerate() {
            let Some((start, end)) = candidate else {
                continue;
            };
            let previous_row = presented.rows.get(row_index)?;
            let intended_row = intended.rows.get(row_index)?;
            // Wrap metadata is established by actual wrapping, not a direct VT
            // setter. A damaged wrapped row therefore uses the proven full
            // reconstruction path.
            if previous_row.wrapped || intended_row.wrapped {
                return None;
            }
            let end = end
                .min(previous_row.cells.len())
                .min(intended_row.cells.len());
            let start = start.min(end);
            stats.rows_considered = stats.rows_considered.saturating_add(1);
            stats.cells_compared = stats.cells_compared.saturating_add(end - start);
            let mut ranges = changed_cell_ranges(previous_row, intended_row, start, end);
            expand_and_merge_cell_ranges(&mut ranges, previous_row, intended_row);
            for (start, end) in ranges {
                write_incremental_run(&mut body, row_index, start, end, intended_row);
                stats.cells_emitted = stats.cells_emitted.saturating_add(end - start);
                wrote_cells = true;
            }
        }

        if self.capabilities.inline_terminal_effects {
            if !terminal_string_matches(&presented.title, &intended.title) {
                write_terminal_string(&mut body, 2, intended.title.as_deref());
            }
            if !terminal_string_matches(&presented.working_directory, &intended.working_directory) {
                write_terminal_string(&mut body, 7, intended.working_directory.as_deref());
            }
        }
        if presented.modes != intended.modes {
            write_mode_changes(&mut body, &presented.modes, &intended.modes);
        }
        if wrote_cells || presented.cursor != intended.cursor {
            write_cursor(&mut body, intended.cursor);
        }
        body.extend(std::iter::repeat_n(b'\x07', intended.bell_count));

        let bytes = if self.capabilities.synchronized_output && !body.is_empty() {
            let mut wrapped = b"\x1b[?2026h".to_vec();
            wrapped.extend_from_slice(&body);
            write_private_mode(&mut wrapped, 2026, intended.modes.synchronized_output);
            wrapped
        } else {
            body
        };
        stats.bytes_emitted = bytes.len();
        self.last_stats = stats;
        self.last_strategy = if bytes.is_empty() {
            RenderStrategy::Noop
        } else {
            RenderStrategy::Incremental
        };
        let transactions = (!bytes.is_empty())
            .then(|| OutputTransaction::new(bytes))
            .into_iter()
            .collect();
        Some(RenderBatch::new(transactions, intended))
    }

    fn semantic_batch(
        &mut self,
        scene: &Scene,
        owner: SurfaceId,
        operations: &[SceneOperation],
        presented: &PresentedScene,
        intended: PresentedScene,
    ) -> SemanticAttempt {
        if operations.is_empty()
            || scene.panes.len() != 1
            || !scene.overlays.is_empty()
            || scene.panes[0].id != owner
            || scene.panes[0].origin != GridPoint::new(0, 0)
            || scene.panes[0].snapshot.geometry != scene.geometry
            || !scene.images.is_empty()
        {
            return SemanticAttempt::NotApplicable(intended);
        }

        let mut working = presented.clone();
        let mut body = b"\x1b[?6l\x1b[?69l\x1b[0m\x1b]8;;\x1b\\".to_vec();
        let mut repairs = Vec::new();
        for operation in operations {
            let result = apply_scene_operation(
                &mut working,
                &mut body,
                &mut repairs,
                operation,
                intended.geometry,
            );
            if !result {
                return SemanticAttempt::NotApplicable(intended);
            }
        }

        normalize_grid_regions(&mut repairs);
        let mut stats = RenderStats::default();
        if !repair_semantic_regions(&mut body, &mut working, &intended, &repairs, &mut stats) {
            return SemanticAttempt::Inconsistent(intended);
        }
        if !rows_physically_match(&working.rows, &intended.rows) {
            return SemanticAttempt::Inconsistent(intended);
        }

        if self.capabilities.inline_terminal_effects {
            if !terminal_string_matches(&presented.title, &intended.title) {
                write_terminal_string(&mut body, 2, intended.title.as_deref());
            }
            if !terminal_string_matches(&presented.working_directory, &intended.working_directory) {
                write_terminal_string(&mut body, 7, intended.working_directory.as_deref());
            }
        }
        if presented.modes != intended.modes {
            write_mode_changes(&mut body, &presented.modes, &intended.modes);
        }
        write_cursor(&mut body, intended.cursor);
        body.extend(std::iter::repeat_n(b'\x07', intended.bell_count));

        let bytes = if self.capabilities.synchronized_output {
            let mut wrapped = b"\x1b[?2026h".to_vec();
            wrapped.extend_from_slice(&body);
            write_private_mode(&mut wrapped, 2026, intended.modes.synchronized_output);
            wrapped
        } else {
            body
        };
        stats.bytes_emitted = bytes.len();
        self.last_stats = stats;
        self.last_strategy = RenderStrategy::SemanticFastPath;
        SemanticAttempt::Batch(RenderBatch::new(
            vec![OutputTransaction::new(bytes)],
            intended,
        ))
    }
}

enum SemanticAttempt {
    Batch(RenderBatch),
    NotApplicable(PresentedScene),
    Inconsistent(PresentedScene),
}

impl RendererBackend for IncrementalVtRenderer {
    fn render(
        &mut self,
        scene: &Scene,
        damage: &SceneDamage,
        presented: &PresentedScene,
    ) -> Result<RenderBatch, PresentationError> {
        let mut intended = PresentedScene::compose(scene)?;
        apply_render_capabilities(&mut intended, self.capabilities);
        let confirmed_matches_presented = self.confirmed.as_ref().is_some_and(|confirmed| {
            confirmed.physically_matches(presented)
                && confirmed.image_uploads == presented.image_uploads
        });
        let full_damage = matches!(
            damage,
            SceneDamage::Full | SceneDamage::Resize { .. } | SceneDamage::Surfaces(_)
        );
        if confirmed_matches_presented
            && full_damage
            && presented.physically_matches(&intended)
            && presented.image_uploads == intended.image_uploads
        {
            self.last_stats = RenderStats::default();
            self.last_strategy = RenderStrategy::Noop;
            return Ok(RenderBatch::new(Vec::new(), intended));
        }
        if !self.can_increment(damage, presented, &intended, confirmed_matches_presented) {
            return self.full_fallback(scene, presented, confirmed_matches_presented);
        }
        if let SceneDamage::Operations {
            owner, operations, ..
        } = damage
        {
            let only_writes = operations
                .iter()
                .all(|operation| matches!(operation, SceneOperation::WriteRun { .. }));
            let erase_and_write = operations
                .iter()
                .any(|operation| matches!(operation, SceneOperation::WriteRun { .. }))
                && operations.iter().all(|operation| {
                    matches!(
                        operation,
                        SceneOperation::WriteRun { .. } | SceneOperation::EraseChars { .. }
                    )
                });
            if only_writes || erase_and_write {
                return match self.incremental_batch(damage, presented, intended) {
                    Some(batch) => {
                        if self.last_strategy == RenderStrategy::Incremental {
                            self.last_strategy = RenderStrategy::SemanticFastPath;
                        }
                        Ok(batch)
                    }
                    None => self.full_fallback(scene, presented, confirmed_matches_presented),
                };
            }
            match self.semantic_batch(scene, *owner, operations, presented, intended) {
                SemanticAttempt::Batch(batch) => return Ok(batch),
                SemanticAttempt::NotApplicable(intended) => {
                    return match self.incremental_batch(damage, presented, intended) {
                        Some(batch) => Ok(batch),
                        None => self.full_fallback(scene, presented, confirmed_matches_presented),
                    };
                }
                SemanticAttempt::Inconsistent(_intended) => {
                    return self.full_fallback(scene, presented, confirmed_matches_presented);
                }
            }
        }
        match self.incremental_batch(damage, presented, intended) {
            Some(batch) => Ok(batch),
            None => self.full_fallback(scene, presented, confirmed_matches_presented),
        }
    }
}

fn apply_scene_operation(
    working: &mut PresentedScene,
    bytes: &mut Vec<u8>,
    repairs: &mut Vec<GridRect>,
    operation: &SceneOperation,
    geometry: TerminalGeometry,
) -> bool {
    let rows = Arc::make_mut(&mut working.rows);
    match operation {
        SceneOperation::ScrollUp { region, count }
        | SceneOperation::DeleteLines { region, count } => {
            let Some((top, bottom, left, right)) = exact_region(*region, geometry) else {
                return false;
            };
            if left != 0 || right != usize::from(geometry.cols) {
                return false;
            }
            let count = usize::from(*count).min(bottom.saturating_sub(top));
            if count == 0
                || (matches!(operation, SceneOperation::DeleteLines { .. })
                    && rows[top..bottom].iter().any(|row| row.wrapped))
            {
                return false;
            }
            shift_repair_rows(repairs, top, bottom, count, true);
            write_vertical_region(bytes, top, bottom);
            if matches!(operation, SceneOperation::DeleteLines { .. }) {
                write_absolute_cursor(bytes, top, 0);
                bytes.extend_from_slice(format!("\x1b[{}M", count).as_bytes());
            } else {
                bytes.extend_from_slice(format!("\x1b[{}S", count).as_bytes());
            }
            reset_vertical_region(bytes);
            if matches!(operation, SceneOperation::ScrollUp { .. }) {
                // Some terminals leave a soft-wrap marker on rows introduced
                // by SU. Clear those new rows explicitly before repairing
                // their cells; rows moved by the scroll retain their markers.
                for row in bottom - count..bottom {
                    write_absolute_cursor(bytes, row, 0);
                    bytes.extend_from_slice(b"\x1b[2K");
                }
            }
            rows[top..bottom].rotate_left(count);
            for row in &mut rows[bottom - count..bottom] {
                *row = blank_row(geometry.cols);
            }
            repairs.push(GridRect::new(
                GridPoint::new((bottom - count).try_into().unwrap_or(i32::MAX), 0),
                count.try_into().unwrap_or(u16::MAX),
                geometry.cols,
            ));
            true
        }
        SceneOperation::ScrollDown { region, count }
        | SceneOperation::InsertLines { region, count } => {
            let Some((top, bottom, left, right)) = exact_region(*region, geometry) else {
                return false;
            };
            if left != 0 || right != usize::from(geometry.cols) {
                return false;
            }
            let count = usize::from(*count).min(bottom.saturating_sub(top));
            if count == 0
                || (matches!(operation, SceneOperation::InsertLines { .. })
                    && rows[top..bottom].iter().any(|row| row.wrapped))
            {
                return false;
            }
            shift_repair_rows(repairs, top, bottom, count, false);
            write_vertical_region(bytes, top, bottom);
            if matches!(operation, SceneOperation::InsertLines { .. }) {
                write_absolute_cursor(bytes, top, 0);
                bytes.extend_from_slice(format!("\x1b[{}L", count).as_bytes());
            } else {
                bytes.extend_from_slice(format!("\x1b[{}T", count).as_bytes());
            }
            reset_vertical_region(bytes);
            if matches!(operation, SceneOperation::ScrollDown { .. }) {
                // Match the SU path above for rows introduced by SD.
                for row in top..top + count {
                    write_absolute_cursor(bytes, row, 0);
                    bytes.extend_from_slice(b"\x1b[2K");
                }
            }
            rows[top..bottom].rotate_right(count);
            for row in &mut rows[top..top + count] {
                *row = blank_row(geometry.cols);
            }
            repairs.push(GridRect::new(
                GridPoint::new(top.try_into().unwrap_or(i32::MAX), 0),
                count.try_into().unwrap_or(u16::MAX),
                geometry.cols,
            ));
            true
        }
        SceneOperation::InsertChars { region, count }
        | SceneOperation::DeleteChars { region, count } => {
            let Some((top, bottom, left, right)) = exact_region(*region, geometry) else {
                return false;
            };
            if bottom != top + 1 || right != usize::from(geometry.cols) {
                return false;
            }
            let row = &mut rows[top];
            if row.wrapped || row.cells[left..right].iter().any(cell_is_wide) {
                return false;
            }
            let count = usize::from(*count).min(right.saturating_sub(left));
            if count == 0 {
                return false;
            }
            let cells = Arc::make_mut(&mut row.cells);
            write_absolute_cursor(bytes, top, left);
            if matches!(operation, SceneOperation::InsertChars { .. }) {
                bytes.extend_from_slice(format!("\x1b[{}@", count).as_bytes());
                cells[left..right].rotate_right(count);
                cells[left..left + count].fill(Cell::default());
                repairs.push(GridRect::new(
                    GridPoint::new(
                        top.try_into().unwrap_or(i32::MAX),
                        left.try_into().unwrap_or(i32::MAX),
                    ),
                    1,
                    count.try_into().unwrap_or(u16::MAX),
                ));
            } else {
                bytes.extend_from_slice(format!("\x1b[{}P", count).as_bytes());
                cells[left..right].rotate_left(count);
                cells[right - count..right].fill(Cell::default());
                repairs.push(GridRect::new(
                    GridPoint::new(
                        top.try_into().unwrap_or(i32::MAX),
                        (right - count).try_into().unwrap_or(i32::MAX),
                    ),
                    1,
                    count.try_into().unwrap_or(u16::MAX),
                ));
            }
            true
        }
        SceneOperation::EraseChars { region } => {
            let Some((top, bottom, left, right)) = exact_region(*region, geometry) else {
                return false;
            };
            if bottom != top + 1 || rows[top].wrapped {
                return false;
            }
            write_absolute_cursor(bytes, top, left);
            bytes.extend_from_slice(format!("\x1b[{}X", right - left).as_bytes());
            Arc::make_mut(&mut rows[top].cells)[left..right].fill(Cell::default());
            repairs.push(*region);
            true
        }
        SceneOperation::WriteRun { origin, text } => {
            if !text
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || byte == b' ')
                || origin.row < 0
                || origin.col < 0
            {
                return false;
            }
            let cols = text.chars().count().try_into().unwrap_or(u16::MAX);
            let region = GridRect::new(*origin, 1, cols);
            let Some((top, _bottom, left, right)) = exact_region(region, geometry) else {
                return false;
            };
            write_absolute_cursor(bytes, top, left);
            bytes.extend_from_slice(text.as_bytes());
            for (cell, character) in Arc::make_mut(&mut rows[top].cells)[left..right]
                .iter_mut()
                .zip(text.chars())
            {
                *cell = Cell {
                    grapheme: character.to_string().into(),
                    ..Cell::default()
                };
            }
            repairs.push(region);
            true
        }
    }
}

fn exact_region(
    region: GridRect,
    geometry: TerminalGeometry,
) -> Option<(usize, usize, usize, usize)> {
    if clip_grid_rect(region, geometry) != Some(region) {
        return None;
    }
    let top = usize::try_from(region.origin.row).ok()?;
    let left = usize::try_from(region.origin.col).ok()?;
    Some((
        top,
        top.saturating_add(usize::from(region.rows)),
        left,
        left.saturating_add(usize::from(region.cols)),
    ))
}

fn shift_repair_rows(
    repairs: &mut Vec<GridRect>,
    top: usize,
    bottom: usize,
    count: usize,
    upward: bool,
) {
    let pending = std::mem::take(repairs);
    for repair in pending {
        for offset in 0..repair.rows {
            let row = repair.origin.row.saturating_add(i32::from(offset));
            let Ok(row_index) = usize::try_from(row) else {
                continue;
            };
            let shifted = if row_index < top || row_index >= bottom {
                Some(row_index)
            } else if upward {
                (row_index >= top.saturating_add(count)).then(|| row_index - count)
            } else {
                (row_index < bottom.saturating_sub(count)).then(|| row_index + count)
            };
            let Some(row_index) = shifted else {
                continue;
            };
            repairs.push(GridRect::new(
                GridPoint::new(row_index.try_into().unwrap_or(i32::MAX), repair.origin.col),
                1,
                repair.cols,
            ));
        }
    }
}

fn blank_row(cols: u16) -> Row {
    Row {
        cells: Arc::new(vec![Cell::default(); usize::from(cols)]),
        wrapped: false,
    }
}

fn cell_is_wide(cell: &Cell) -> bool {
    cell.continuation || cell.width > 1
}

fn write_vertical_region(bytes: &mut Vec<u8>, top: usize, bottom: usize) {
    bytes.extend_from_slice(format!("\x1b[{};{}r", top + 1, bottom).as_bytes());
}

fn reset_vertical_region(bytes: &mut Vec<u8>) {
    bytes.extend_from_slice(b"\x1b[r");
}

fn write_absolute_cursor(bytes: &mut Vec<u8>, row: usize, col: usize) {
    bytes.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
}

fn repair_semantic_regions(
    bytes: &mut Vec<u8>,
    working: &mut PresentedScene,
    intended: &PresentedScene,
    regions: &[GridRect],
    stats: &mut RenderStats,
) -> bool {
    let mut candidates = vec![None::<(usize, usize)>; intended.rows.len()];
    for region in regions {
        let Some((top, bottom, left, right)) = exact_region(*region, intended.geometry) else {
            return false;
        };
        for candidate in &mut candidates[top..bottom] {
            *candidate = Some(match *candidate {
                Some((start, end)) => (start.min(left), end.max(right)),
                None => (left, right),
            });
        }
    }
    let rows = Arc::make_mut(&mut working.rows);
    for (row_index, candidate) in candidates.into_iter().enumerate() {
        let Some((left, right)) = candidate else {
            continue;
        };
        let Some(previous_row) = rows.get(row_index) else {
            return false;
        };
        let Some(intended_row) = intended.rows.get(row_index) else {
            return false;
        };
        if previous_row.wrapped != intended_row.wrapped {
            return false;
        }
        stats.rows_considered = stats.rows_considered.saturating_add(1);
        stats.cells_compared = stats.cells_compared.saturating_add(right - left);
        let mut ranges = changed_cell_ranges(previous_row, intended_row, left, right);
        expand_and_merge_cell_ranges(&mut ranges, previous_row, intended_row);
        for (start, end) in ranges {
            write_incremental_run(bytes, row_index, start, end, intended_row);
            stats.cells_emitted = stats.cells_emitted.saturating_add(end - start);
            Arc::make_mut(&mut rows[row_index].cells)[start..end]
                .clone_from_slice(&intended_row.cells[start..end]);
        }
    }
    true
}

fn changed_cell_ranges(
    previous: &Row,
    intended: &Row,
    start: usize,
    end: usize,
) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut run_start = None;
    for col in start..end {
        let changed = !cells_physically_match(&previous.cells[col], &intended.cells[col]);
        match (run_start, changed) {
            (None, true) => run_start = Some(col),
            (Some(first), false) => {
                ranges.push((first, col));
                run_start = None;
            }
            _ => {}
        }
    }
    if let Some(first) = run_start {
        ranges.push((first, end));
    }
    ranges
}

fn expand_and_merge_cell_ranges(ranges: &mut Vec<(usize, usize)>, previous: &Row, intended: &Row) {
    let cols = previous.cells.len().min(intended.cells.len());
    for (start, end) in ranges.iter_mut() {
        while *start > 0
            && (previous.cells[*start].continuation || intended.cells[*start].continuation)
        {
            *start -= 1;
        }
        loop {
            let mut expanded = *end;
            for row in [previous, intended] {
                if *start < row.cells.len() {
                    expanded = expanded
                        .max(start.saturating_add(usize::from(row.cells[*start].width.max(1))));
                }
                while expanded < cols && row.cells[expanded].continuation {
                    expanded += 1;
                }
            }
            expanded = expanded.min(cols);
            if expanded == *end {
                break;
            }
            *end = expanded;
        }
    }
    ranges.sort_unstable_by_key(|range| range.0);
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        if let Some(previous) = merged.last_mut()
            && range.0 <= previous.1
        {
            previous.1 = previous.1.max(range.1);
        } else {
            merged.push(range);
        }
    }
    *ranges = merged;
}

fn cells_physically_match(left: &Cell, right: &Cell) -> bool {
    let left_blank = matches!(left.grapheme.as_ref(), "" | " ");
    let right_blank = matches!(right.grapheme.as_ref(), "" | " ");
    (left.grapheme == right.grapheme || left_blank && right_blank)
        && left.width == right.width
        && left.continuation == right.continuation
        && left.style == right.style
        && left.hyperlink == right.hyperlink
}

fn write_incremental_run(bytes: &mut Vec<u8>, row: usize, start: usize, end: usize, cells: &Row) {
    bytes.extend_from_slice(format!("\x1b[{};{}H", row + 1, start + 1).as_bytes());
    bytes.extend_from_slice(b"\x1b[0m\x1b]8;;\x1b\\");
    let mut active_style = Style::default();
    let mut active_link: Option<&str> = None;
    for cell in &cells.cells[start..end] {
        if cell.continuation {
            continue;
        }
        let link = cell.hyperlink.as_deref();
        if link != active_link {
            write_hyperlink(bytes, link);
            active_link = link;
        }
        if cell.style != active_style {
            write_style(bytes, &cell.style);
            active_style.clone_from(&cell.style);
        }
        if cell.grapheme.is_empty() {
            bytes.push(b' ');
        } else {
            bytes.extend_from_slice(cell.grapheme.as_bytes());
        }
    }
    if active_link.is_some() {
        write_hyperlink(bytes, None);
    }
    bytes.extend_from_slice(b"\x1b[0m");
}

fn write_mode_changes(bytes: &mut Vec<u8>, previous: &TerminalModes, intended: &TerminalModes) {
    if previous.application_keypad != intended.application_keypad {
        bytes.extend_from_slice(if intended.application_keypad {
            b"\x1b="
        } else {
            b"\x1b>"
        });
    }
    for (mode, before, after) in [
        (1, previous.application_cursor, intended.application_cursor),
        (2004, previous.bracketed_paste, intended.bracketed_paste),
        (1004, previous.focus_reporting, intended.focus_reporting),
    ] {
        if before != after {
            write_private_mode(bytes, mode, after);
        }
    }
    if previous.mouse_protocol != intended.mouse_protocol
        || previous.mouse_encoding != intended.mouse_encoding
    {
        for mode in [9, 1000, 1002, 1003, 1005, 1006] {
            write_private_mode(bytes, mode, false);
        }
        let protocol = match intended.mouse_protocol {
            crate::terminal::MouseProtocol::None => None,
            crate::terminal::MouseProtocol::Press => Some(9),
            crate::terminal::MouseProtocol::PressRelease => Some(1000),
            crate::terminal::MouseProtocol::ButtonMotion => Some(1002),
            crate::terminal::MouseProtocol::AnyMotion => Some(1003),
        };
        if let Some(mode) = protocol {
            write_private_mode(bytes, mode, true);
        }
        match intended.mouse_encoding {
            crate::terminal::MouseEncoding::Default => {}
            crate::terminal::MouseEncoding::Utf8 => write_private_mode(bytes, 1005, true),
            crate::terminal::MouseEncoding::Sgr => write_private_mode(bytes, 1006, true),
        }
    }
    if previous.kitty_keyboard_flags != intended.kitty_keyboard_flags {
        bytes.extend_from_slice(format!("\x1b[={}u", intended.kitty_keyboard_flags).as_bytes());
    }
    if previous.synchronized_output != intended.synchronized_output {
        write_private_mode(bytes, 2026, intended.synchronized_output);
    }
}

const KITTY_BASE64_CHUNK_BYTES: usize = 4096;

fn write_kitty_images(
    bytes: &mut Vec<u8>,
    scene: &Scene,
    intended: &PresentedScene,
    retained_uploads: &[PresentedImageUpload],
) -> Result<(), PresentationError> {
    let records = compose_image_records(scene);
    if records.len() != intended.images.len()
        || records
            .iter()
            .zip(&intended.images)
            .any(|(record, image)| record.image != *image)
    {
        return Err(PresentationError::InconsistentComposedMedia);
    }
    let image_ids = outer_image_ids(scene);
    let mut uploaded = BTreeSet::new();
    for upload in &scene.image_uploads {
        let image_id = image_ids[&(upload.owner, upload.image_id, upload.data_digest)];
        if !uploaded.insert(image_id) {
            continue;
        }
        let intended_upload = PresentedImageUpload {
            image_id,
            pixel_width: upload.pixel_width,
            pixel_height: upload.pixel_height,
            format: upload.format,
            data_len: upload.data.len(),
            data_digest: upload.data_digest,
        };
        if !retained_uploads.contains(&intended_upload) {
            write_kitty_upload(bytes, image_id, upload);
        }
    }
    for record in records {
        if !scene
            .image_uploads
            .iter()
            .any(|upload| upload.owner == record.owner && upload.image_id == record.source_image_id)
        {
            return Err(PresentationError::MissingMediaUpload {
                owner: record.owner,
                image_id: record.source_image_id,
            });
        }
        write_kitty_placement(bytes, &record.image);
    }
    Ok(())
}

fn write_kitty_upload(bytes: &mut Vec<u8>, image_id: u32, upload: &SceneImageUpload) {
    let encoded = encode_base64(&upload.data);
    let format = match upload.format {
        PixelFormat::Rgb | PixelFormat::Gray => 24,
        PixelFormat::Rgba | PixelFormat::GrayAlpha => 32,
    };
    let chunks: Vec<&[u8]> = if encoded.is_empty() {
        vec![b""]
    } else {
        encoded
            .as_bytes()
            .chunks(KITTY_BASE64_CHUNK_BYTES)
            .collect()
    };
    for (index, chunk) in chunks.iter().enumerate() {
        let more = usize::from(index + 1 < chunks.len());
        if index == 0 {
            bytes.extend_from_slice(
                format!(
                    "\x1b_Ga=t,t=d,f={format},s={},v={},i={image_id},q=2,m={more};",
                    upload.pixel_width, upload.pixel_height
                )
                .as_bytes(),
            );
        } else {
            bytes.extend_from_slice(format!("\x1b_Gm={more};").as_bytes());
        }
        bytes.extend_from_slice(chunk);
        bytes.extend_from_slice(b"\x1b\\");
    }
}

fn write_kitty_placement(bytes: &mut Vec<u8>, image: &PresentedImage) {
    bytes.extend_from_slice(
        format!(
            "\x1b[{};{}H\x1b_Ga=p,i={},p={},x={},y={},w={},h={},X={},Y={},c={},r={},C=1,U={},z={};\x1b\\",
            image.grid_rect.origin.row.saturating_add(1),
            image.grid_rect.origin.col.saturating_add(1),
            image.image_id,
            image.placement_id,
            image.source_x,
            image.source_y,
            image.source_width,
            image.source_height,
            image.x_offset,
            image.y_offset,
            image.grid_rect.cols,
            image.grid_rect.rows,
            usize::from(image.virtual_placement),
            image.z_index,
        )
        .as_bytes(),
    );
}

fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        encoded.push(TABLE[usize::from(a >> 2)] as char);
        encoded.push(TABLE[usize::from((a & 0x03) << 4 | b >> 4)] as char);
        encoded.push(if chunk.len() > 1 {
            TABLE[usize::from((b & 0x0f) << 2 | c >> 6)] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            TABLE[usize::from(c & 0x3f)] as char
        } else {
            '='
        });
    }
    encoded
}

fn write_terminal_string(bytes: &mut Vec<u8>, code: u8, value: Option<&str>) {
    let value = value.unwrap_or_default();
    bytes.extend_from_slice(b"\x1b]");
    bytes.extend_from_slice(code.to_string().as_bytes());
    bytes.push(b';');
    bytes.extend(value.bytes().filter(|byte| *byte >= b' ' && *byte != 0x7f));
    bytes.extend_from_slice(b"\x1b\\");
}

fn write_full_rows(bytes: &mut Vec<u8>, intended: &PresentedScene) {
    for (row_index, row) in intended.rows.iter().enumerate() {
        if !row.wrapped {
            // A hard line may legitimately occupy the final column. Disable
            // autowrap while reconstructing it so the outer terminal does not
            // invent a soft-wrap marker that is absent from the source scene.
            bytes.extend_from_slice(b"\x1b[?7l");
        }
        write_row_at(bytes, row_index, row);
        if !row.wrapped {
            bytes.extend_from_slice(b"\x1b[?7h");
        }
        // Filling the final column leaves autowrap pending. One temporary cell
        // on the following row realizes Ghostty's wrap boundary; that cell is
        // overwritten when the following row is reconstructed.
        if row.wrapped && row_index + 1 < intended.rows.len() {
            bytes.push(b' ');
        }
    }

    // A last visible row can retain a wrap marker after scroll/reflow. Create
    // that state the same way a VT does naturally: wrap from the penultimate
    // row, hard-advance at the bottom, then reconstruct the scrolled scene.
    // Printing into the new bottom row preserves its wrap metadata.
    if intended.rows.len() >= 2 && intended.rows.last().is_some_and(|row| row.wrapped) {
        let last = intended.rows.len() - 1;
        bytes.extend_from_slice(
            format!("\x1b[{};{}H\x1b[0mXX\r\n", last, intended.geometry.cols).as_bytes(),
        );
        for (row_index, row) in intended.rows.iter().enumerate() {
            write_row_at(bytes, row_index, row);
            if row.wrapped && row_index < last {
                bytes.push(b' ');
            }
        }
    }
}

fn write_row_at(bytes: &mut Vec<u8>, row_index: usize, row: &Row) {
    bytes.extend_from_slice(format!("\x1b[{};1H", row_index + 1).as_bytes());
    if !row.wrapped {
        // Repainting cells does not itself clear a pre-existing soft-wrap
        // marker on several terminals. EL resets that row metadata before the
        // complete hard line is reconstructed.
        bytes.extend_from_slice(b"\x1b[2K");
    }
    bytes.extend_from_slice(b"\x1b[0m");
    let mut active_style = Style::default();
    let mut active_link: Option<&str> = None;
    for cell in row.cells.iter() {
        if cell.continuation {
            continue;
        }
        let link = cell.hyperlink.as_deref();
        if link != active_link {
            write_hyperlink(bytes, link);
            active_link = link;
        }
        if cell.style != active_style {
            write_style(bytes, &cell.style);
            active_style.clone_from(&cell.style);
        }
        if cell.grapheme.is_empty() {
            bytes.push(b' ');
        } else {
            bytes.extend_from_slice(cell.grapheme.as_bytes());
        }
    }
    if active_link.is_some() {
        write_hyperlink(bytes, None);
    }
}

fn write_hyperlink(bytes: &mut Vec<u8>, link: Option<&str>) {
    bytes.extend_from_slice(b"\x1b]8;;");
    if let Some(link) = link {
        bytes.extend(link.bytes().filter(|byte| *byte >= b' ' && *byte != 0x7f));
    }
    bytes.extend_from_slice(b"\x1b\\");
}

fn write_style(bytes: &mut Vec<u8>, style: &Style) {
    bytes.extend_from_slice(b"\x1b[0");
    for enabled in [
        (style.bold, ";1"),
        (style.dim, ";2"),
        (style.italic, ";3"),
        (style.blink, ";5"),
        (style.inverse, ";7"),
        (style.invisible, ";8"),
        (style.strikethrough, ";9"),
        (style.overline, ";53"),
    ] {
        if enabled.0 {
            bytes.extend_from_slice(enabled.1.as_bytes());
        }
    }
    match style.underline {
        UnderlineStyle::None => {}
        UnderlineStyle::Single => bytes.extend_from_slice(b";4:1"),
        UnderlineStyle::Double => bytes.extend_from_slice(b";4:2"),
        UnderlineStyle::Curly => bytes.extend_from_slice(b";4:3"),
        UnderlineStyle::Dotted => bytes.extend_from_slice(b";4:4"),
        UnderlineStyle::Dashed => bytes.extend_from_slice(b";4:5"),
    }
    write_color(bytes, style.foreground, 38);
    write_color(bytes, style.background, 48);
    write_color(bytes, style.underline_color, 58);
    bytes.push(b'm');
}

fn write_color(bytes: &mut Vec<u8>, color: Color, selector: u8) {
    match color {
        Color::Default => {}
        Color::Indexed(index) => {
            bytes.extend_from_slice(format!(";{selector};5;{index}").as_bytes());
        }
        Color::Rgb(red, green, blue) => {
            bytes.extend_from_slice(format!(";{selector};2;{red};{green};{blue}").as_bytes());
        }
    }
}

fn write_cursor(bytes: &mut Vec<u8>, cursor: Cursor) {
    bytes.extend_from_slice(
        format!(
            "\x1b[{};{}H\x1b[{} q\x1b[?25{}",
            cursor.row.saturating_add(1),
            cursor.col.saturating_add(1),
            match cursor.shape {
                CursorShape::Block | CursorShape::BlockHollow => 2,
                CursorShape::Underline => 4,
                CursorShape::Bar => 6,
            },
            if cursor.visible { 'h' } else { 'l' },
        )
        .as_bytes(),
    );
}

fn write_modes(bytes: &mut Vec<u8>, modes: &TerminalModes, synchronized_wrapper: bool) {
    bytes.extend_from_slice(if modes.application_keypad {
        b"\x1b="
    } else {
        b"\x1b>"
    });
    write_private_mode(bytes, 1, modes.application_cursor);
    write_private_mode(bytes, 2004, modes.bracketed_paste);
    write_private_mode(bytes, 1004, modes.focus_reporting);
    for mode in [9, 1000, 1002, 1003, 1005, 1006] {
        write_private_mode(bytes, mode, false);
    }
    let mouse_mode = match modes.mouse_protocol {
        crate::terminal::MouseProtocol::None => None,
        crate::terminal::MouseProtocol::Press => Some(9),
        crate::terminal::MouseProtocol::PressRelease => Some(1000),
        crate::terminal::MouseProtocol::ButtonMotion => Some(1002),
        crate::terminal::MouseProtocol::AnyMotion => Some(1003),
    };
    if let Some(mode) = mouse_mode {
        write_private_mode(bytes, mode, true);
    }
    match modes.mouse_encoding {
        crate::terminal::MouseEncoding::Default => {}
        crate::terminal::MouseEncoding::Utf8 => write_private_mode(bytes, 1005, true),
        crate::terminal::MouseEncoding::Sgr => write_private_mode(bytes, 1006, true),
    }
    bytes.extend_from_slice(format!("\x1b[={}u", modes.kitty_keyboard_flags).as_bytes());
    if modes.synchronized_output {
        write_private_mode(bytes, 2026, true);
    } else if synchronized_wrapper {
        // This is deliberately last so the complete redraw is committed as a
        // single transaction on supporting terminals.
        write_private_mode(bytes, 2026, false);
    } else {
        // This is a state reset, not a structural wrapper. Terminals without
        // mode-2026 support safely ignore the unknown private mode.
        write_private_mode(bytes, 2026, false);
    }
}

fn write_private_mode(bytes: &mut Vec<u8>, mode: u16, enabled: bool) {
    bytes.extend_from_slice(format!("\x1b[?{mode}{}", if enabled { 'h' } else { 'l' }).as_bytes());
}

#[derive(Debug, thiserror::Error)]
pub enum PresentationError {
    #[error("Ghostty render oracle: {0}")]
    Ghostty(#[from] lector_ghostty::Error),
    #[error("render oracle artifact I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("render oracle artifact serialization: {0}")]
    Json(#[from] serde_json::Error),
    #[error("media limit exceeded for {resource}: requested {requested}, limit {limit}")]
    MediaLimitExceeded {
        resource: &'static str,
        requested: usize,
        limit: usize,
    },
    #[error("image {0} was reported with inconsistent decoded payloads")]
    InconsistentMediaUpload(u32),
    #[error("scene media composition changed while encoding")]
    InconsistentComposedMedia,
    #[error("surface {owner:?} image {image_id} has a placement but no retained upload")]
    MissingMediaUpload { owner: SurfaceId, image_id: u32 },
}

pub struct RenderOracle {
    terminal: GhosttyEngine,
}

impl RenderOracle {
    pub fn new(geometry: TerminalGeometry) -> Result<Self, PresentationError> {
        let mut terminal = GhosttyEngine::new(geometry.rows, geometry.cols)?;
        terminal.resize_with_geometry(geometry)?;
        terminal.advance(b"\x1b[?1049h")?;
        Ok(Self { terminal })
    }

    pub fn verify(
        &mut self,
        case_name: &str,
        intended: &PresentedScene,
        batch: &RenderBatch,
    ) -> Result<(), OracleFailure> {
        let (initial, initial_error) = self.capture();
        if let Some(error) = initial_error {
            return Err(self.failure(
                case_name,
                initial.clone(),
                intended,
                batch,
                initial,
                vec![format!("initial outer-state capture failed: {error}")],
            ));
        }
        let mut observed_bells = 0usize;
        for transaction in &batch.transactions {
            if let Some(geometry) = transaction.resize
                && let Err(error) = self.terminal.resize_with_geometry(geometry)
            {
                let (resulting, capture_error) = self.capture();
                let mut mismatches = vec![format!("outer resize failed: {error}")];
                if let Some(error) = capture_error {
                    mismatches.push(format!("resulting outer-state capture failed: {error}"));
                }
                return Err(
                    self.failure(case_name, initial, intended, batch, resulting, mismatches)
                );
            }
            match self.terminal.advance(&transaction.bytes) {
                Ok(update) => observed_bells = observed_bells.saturating_add(update.effects.bells),
                Err(error) => {
                    let (resulting, capture_error) = self.capture();
                    let mut mismatches = vec![format!("outer terminal advance failed: {error}")];
                    if let Some(error) = capture_error {
                        mismatches.push(format!("resulting outer-state capture failed: {error}"));
                    }
                    return Err(
                        self.failure(case_name, initial, intended, batch, resulting, mismatches)
                    );
                }
            }
        }
        let (mut resulting, capture_error) = self.capture();
        resulting.bell_count = observed_bells;
        let mut mismatches = Vec::new();
        if let Some(error) = capture_error {
            mismatches.push(format!("resulting outer-state capture failed: {error}"));
        }
        if &batch.predicted != intended {
            mismatches.push("renderer prediction differs from intended scene".to_string());
        }
        if !resulting.physically_matches_in_owned_alternate(intended) {
            mismatches.push("outer terminal state differs from intended scene".to_string());
        }
        if mismatches.is_empty() {
            Ok(())
        } else {
            Err(self.failure(case_name, initial, intended, batch, resulting, mismatches))
        }
    }

    fn capture(&self) -> (PresentedScene, Option<PresentationError>) {
        match PresentedScene::from_engine(&self.terminal) {
            Ok(scene) => (scene, None),
            Err(error) => (
                PresentedScene::from_snapshot_and_images(
                    self.terminal.normalized_snapshot(),
                    Vec::new(),
                    Vec::new(),
                ),
                Some(error),
            ),
        }
    }

    fn failure(
        &self,
        case_name: &str,
        initial: PresentedScene,
        intended: &PresentedScene,
        batch: &RenderBatch,
        resulting: PresentedScene,
        mismatches: Vec<String>,
    ) -> OracleFailure {
        let path = oracle_artifact_path(case_name);
        let artifact = OracleFailureArtifact {
            case_name: case_name.to_owned(),
            initial_outer_state: initial,
            intended_scene: intended.clone(),
            emitted_transactions: batch.transactions.clone(),
            predicted_scene: batch.predicted.clone(),
            resulting_outer_state: resulting,
            mismatches: mismatches.clone(),
        };
        let write_error = write_oracle_artifact(&path, &artifact)
            .err()
            .map(|error| error.to_string());
        OracleFailure {
            artifact_path: path,
            mismatches,
            write_error,
        }
    }
}

#[derive(Serialize)]
struct OracleFailureArtifact {
    case_name: String,
    initial_outer_state: PresentedScene,
    intended_scene: PresentedScene,
    emitted_transactions: Vec<OutputTransaction>,
    predicted_scene: PresentedScene,
    resulting_outer_state: PresentedScene,
    mismatches: Vec<String>,
}

fn oracle_artifact_path(case_name: &str) -> PathBuf {
    let safe_name: String = case_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    PathBuf::from("target")
        .join("render-oracle-failures")
        .join(format!("{safe_name}.json"))
}

fn write_oracle_artifact(
    path: &PathBuf,
    artifact: &OracleFailureArtifact,
) -> Result<(), PresentationError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let encoded = serde_json::to_vec_pretty(artifact)?;
    fs::write(path, encoded)?;
    Ok(())
}

#[derive(Debug)]
pub struct OracleFailure {
    artifact_path: PathBuf,
    mismatches: Vec<String>,
    write_error: Option<String>,
}

impl OracleFailure {
    pub fn artifact_path(&self) -> &std::path::Path {
        &self.artifact_path
    }
}

impl fmt::Display for OracleFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "render oracle mismatch: {}; artifact: {}",
            self.mismatches.join("; "),
            self.artifact_path.display()
        )?;
        if let Some(error) = &self.write_error {
            write!(formatter, "; artifact write failed: {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for OracleFailure {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LifecycleState {
    Inactive,
    Active,
    Suspended,
    ShutdownFence,
    Shutdown,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LifecycleTransaction {
    pub bytes: Vec<u8>,
    pub damage: SceneDamage,
}

/// Owns the host terminal's alternate screen and global modes for the complete
/// active Lector session. Child primary/alternate screens remain modeled state
/// and never change this physical container.
pub struct PhysicalTerminalLifecycle {
    state: LifecycleState,
    focus_was_enabled: Option<bool>,
}

impl PhysicalTerminalLifecycle {
    pub const fn new(focus_was_enabled: Option<bool>) -> Self {
        Self {
            state: LifecycleState::Inactive,
            focus_was_enabled,
        }
    }

    pub fn activate(&mut self) -> LifecycleTransaction {
        if self.state != LifecycleState::Inactive {
            return LifecycleTransaction::default();
        }
        self.state = LifecycleState::Active;
        LifecycleTransaction {
            bytes: b"\x1b[?1049h\x1b[?1004h".to_vec(),
            damage: SceneDamage::Full,
        }
    }

    pub fn suspend(&mut self) -> LifecycleTransaction {
        if self.state != LifecycleState::Active {
            return LifecycleTransaction::default();
        }
        self.state = LifecycleState::Suspended;
        LifecycleTransaction {
            bytes: self.cleanup_bytes(),
            damage: SceneDamage::Full,
        }
    }

    pub fn resume(&mut self) -> LifecycleTransaction {
        if self.state != LifecycleState::Suspended {
            return LifecycleTransaction::default();
        }
        self.state = LifecycleState::Active;
        LifecycleTransaction {
            bytes: b"\x1b[?1049h\x1b[?1004h".to_vec(),
            damage: SceneDamage::Full,
        }
    }

    pub fn shutdown(&mut self) -> LifecycleTransaction {
        if self.state == LifecycleState::Shutdown {
            return LifecycleTransaction::default();
        }
        let bytes = match self.state {
            LifecycleState::Active => self.cleanup_bytes(),
            LifecycleState::ShutdownFence => b"\x1b[?1049l".to_vec(),
            LifecycleState::Inactive | LifecycleState::Suspended | LifecycleState::Shutdown => {
                Vec::new()
            }
        };
        self.state = LifecycleState::Shutdown;
        LifecycleTransaction {
            bytes,
            damage: SceneDamage::None,
        }
    }

    /// Reset every owned mode and place DA1 after all output which may cause a
    /// terminal reply. The alternate screen remains active until the caller
    /// has consumed that reply or reached its bounded timeout.
    pub fn begin_shutdown_fence(&mut self) -> LifecycleTransaction {
        if self.state != LifecycleState::Active {
            return LifecycleTransaction::default();
        }
        self.state = LifecycleState::ShutdownFence;
        let mut bytes = self.cleanup_modes_bytes();
        bytes.extend_from_slice(crate::terminal_protocol::SHUTDOWN_FENCE_QUERY);
        LifecycleTransaction {
            bytes,
            damage: SceneDamage::None,
        }
    }

    /// Release the alternate screen only after the shutdown fence has
    /// completed. Nothing emitted here is allowed to request another reply.
    pub fn finish_shutdown_fence(&mut self) -> LifecycleTransaction {
        if self.state != LifecycleState::ShutdownFence {
            return LifecycleTransaction::default();
        }
        self.state = LifecycleState::Shutdown;
        LifecycleTransaction {
            bytes: b"\x1b[?1049l".to_vec(),
            damage: SceneDamage::None,
        }
    }

    fn cleanup_bytes(&self) -> Vec<u8> {
        let mut bytes = self.cleanup_modes_bytes();
        bytes.extend_from_slice(b"\x1b[?1049l");
        bytes
    }

    fn cleanup_modes_bytes(&self) -> Vec<u8> {
        let mut bytes = b"\x1b[?2026l\x1b[0m\x1b]8;;\x1b\\\x1b>\x1b[?1l\x1b[?2004l\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[=0u\x1b[?25h".to_vec();
        if !matches!(self.focus_was_enabled, Some(true)) {
            bytes.extend_from_slice(b"\x1b[?1004l");
        }
        bytes
    }
}
