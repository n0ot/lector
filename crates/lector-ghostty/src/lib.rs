//! Lector-owned safe boundary for the pinned official `libghostty-vt` C ABI.
//!
//! This crate deliberately exposes only APIs that Lector uses and verifies.
//! Raw declarations and all `unsafe` calls remain private to this crate.

#![deny(unsafe_op_in_unsafe_fn)]

mod ffi;

use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::cell::Cell;
use std::{
    borrow::Cow, cell::RefCell, collections::BTreeMap, ffi::c_void, fmt, marker::PhantomData,
    mem::MaybeUninit, ops::RangeInclusive, ptr::NonNull, rc::Rc, sync::Arc,
};

/// An error reported by the Ghostty C ABI or by validation at the Rust boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    OutOfMemory,
    InvalidValue,
    OutOfSpace,
    NoValue,
    IoError,
    LimitExceeded,
    UnknownResult(i32),
    NullHandle,
    NullString,
    InvalidUtf8,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "libghostty-vt error: {self:?}")
    }
}

impl std::error::Error for Error {}

/// The optimization mode recorded by the linked Ghostty archive.
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptimizeMode {
    Debug = 0,
    ReleaseSafe = 1,
    ReleaseSmall = 2,
    ReleaseFast = 3,
}

impl TryFrom<i32> for OptimizeMode {
    type Error = Error;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Debug),
            1 => Ok(Self::ReleaseSafe),
            2 => Ok(Self::ReleaseSmall),
            3 => Ok(Self::ReleaseFast),
            _ => Err(Error::InvalidValue),
        }
    }
}

/// Compile-time information reported by the linked official Ghostty archive.
pub mod build_info {
    use super::{Error, OptimizeMode, ffi, query, query_string};

    pub fn supports_simd() -> Result<bool, Error> {
        query(ffi::BUILD_INFO_SIMD)
    }

    pub fn supports_kitty_graphics() -> Result<bool, Error> {
        query(ffi::BUILD_INFO_KITTY_GRAPHICS)
    }

    pub fn supports_tmux_control_mode() -> Result<bool, Error> {
        query(ffi::BUILD_INFO_TMUX_CONTROL_MODE)
    }

    pub fn optimize_mode() -> Result<OptimizeMode, Error> {
        query::<i32>(ffi::BUILD_INFO_OPTIMIZE)?.try_into()
    }

    pub fn version_string() -> Result<&'static str, Error> {
        query_string(ffi::BUILD_INFO_VERSION_STRING)
    }

    pub fn major_version() -> Result<usize, Error> {
        query(ffi::BUILD_INFO_VERSION_MAJOR)
    }

    pub fn minor_version() -> Result<usize, Error> {
        query(ffi::BUILD_INFO_VERSION_MINOR)
    }

    pub fn patch_version() -> Result<usize, Error> {
        query(ffi::BUILD_INFO_VERSION_PATCH)
    }

    pub fn pre_version() -> Result<&'static str, Error> {
        query_string(ffi::BUILD_INFO_VERSION_PRE)
    }

    pub fn build_version() -> Result<&'static str, Error> {
        query_string(ffi::BUILD_INFO_VERSION_BUILD)
    }
}

/// A color from Ghostty normalized without resolving palette entries.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorSnapshot {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnderlineSnapshot {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// The complete presentation-relevant style attributes of a Ghostty cell.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct StyleSnapshot {
    pub foreground: ColorSnapshot,
    pub background: ColorSnapshot,
    pub underline_color: ColorSnapshot,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    pub underline: UnderlineSnapshot,
}

/// An exact OSC 133 boundary observed in the same stream Ghostty consumes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticKindSnapshot {
    PromptStart,
    InputStart,
    CommandStart,
    CommandFinished { exit_code: Option<i32> },
}

/// A retained OSC 133 boundary anchored to Ghostty's grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticMarkSnapshot {
    pub kind: SemanticKindSnapshot,
    pub row: usize,
    pub col: u16,
    pub alternate_screen: bool,
}

/// A normalized cell from Ghostty's current visible viewport.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct CellSnapshot {
    pub grapheme: Cow<'static, str>,
    pub width: u8,
    pub continuation: bool,
    pub style: StyleSnapshot,
    pub hyperlink: Option<String>,
}

impl Default for CellSnapshot {
    fn default() -> Self {
        Self {
            grapheme: Cow::Borrowed(""),
            width: 1,
            continuation: false,
            style: StyleSnapshot::default(),
            hyperlink: None,
        }
    }
}

impl CellSnapshot {
    pub fn contents(&self) -> &str {
        &self.grapheme
    }

    pub fn has_contents(&self) -> bool {
        !self.grapheme.is_empty()
    }

    pub fn is_wide(&self) -> bool {
        self.width == 2 && !self.continuation
    }

    pub fn is_wide_continuation(&self) -> bool {
        self.continuation
    }

    pub fn fgcolor(&self) -> ColorSnapshot {
        self.style.foreground
    }

    pub fn bgcolor(&self) -> ColorSnapshot {
        self.style.background
    }

    pub fn bold(&self) -> bool {
        self.style.bold
    }

    pub fn dim(&self) -> bool {
        self.style.dim
    }

    pub fn italic(&self) -> bool {
        self.style.italic
    }

    pub fn underline(&self) -> bool {
        self.style.underline != UnderlineSnapshot::None
    }

    pub fn inverse(&self) -> bool {
        self.style.inverse
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KittyImageFormatSnapshot {
    Rgb,
    Rgba,
    GrayAlpha,
    Gray,
}

/// A copied, mutation-safe view of one Kitty image placement on the active
/// Ghostty screen. Pixel bytes are decoded and uncompressed by Ghostty before
/// they cross this safe boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KittyImagePlacementSnapshot {
    pub image_id: u32,
    pub placement_id: u32,
    pub image_number: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub rendered_pixel_width: u32,
    pub rendered_pixel_height: u32,
    pub format: KittyImageFormatSnapshot,
    pub data: Arc<[u8]>,
    pub data_digest: u64,
    pub x_offset: u32,
    pub y_offset: u32,
    pub viewport_col: i32,
    pub viewport_row: i32,
    pub grid_cols: u32,
    pub grid_rows: u32,
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub z_index: i32,
    pub virtual_placement: bool,
    pub visible: bool,
}

/// A normalized visible row from Ghostty.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct RowSnapshot {
    pub cells: Arc<Vec<CellSnapshot>>,
    pub wrapped: bool,
}

impl RowSnapshot {
    pub fn contents(&self) -> String {
        self.text()
    }

    pub fn text(&self) -> String {
        let mut output = String::new();
        self.append_contents_to(&mut output);
        output
    }

    /// Appends this row's visible text without allocating an intermediate
    /// string. Trailing blank cells remain omitted, matching [`Self::text`].
    pub fn append_contents_to(&self, output: &mut String) {
        self.append_contents_range_to(output, 0, self.cells.len());
    }

    /// Appends the visible text in a range of physical cells without
    /// allocating an intermediate string. Trailing blank cells remain omitted.
    pub fn append_contents_range_to(&self, output: &mut String, start: usize, width: usize) {
        let end = start.saturating_add(width).min(self.cells.len());
        let start = start.min(end);
        let mut pending_spaces = 0;
        for cell in &self.cells[start..end] {
            if cell.continuation {
                continue;
            }
            if cell.grapheme.is_empty() {
                pending_spaces += 1;
            } else {
                output.extend(std::iter::repeat_n(' ', pending_spaces));
                pending_spaces = 0;
                output.push_str(&cell.grapheme);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CursorShapeSnapshot {
    Bar,
    #[default]
    Block,
    Underline,
    BlockHollow,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CursorSnapshot {
    pub row: u16,
    pub col: u16,
    pub visible: bool,
    pub shape: CursorShapeSnapshot,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MouseProtocol {
    #[default]
    None,
    Press,
    PressRelease,
    ButtonMotion,
    AnyMotion,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MouseEncoding {
    #[default]
    Default,
    Utf8,
    Sgr,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModesSnapshot {
    pub application_keypad: bool,
    pub application_cursor: bool,
    pub bracketed_paste: bool,
    pub synchronized_output: bool,
    pub focus_reporting: bool,
    pub kitty_keyboard_flags: u8,
    pub mouse_protocol: MouseProtocol,
    pub mouse_encoding: MouseEncoding,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardLocationSnapshot {
    Standard,
    Selection,
    Primary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardContentSnapshot {
    pub mime: String,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgressStateSnapshot {
    Remove,
    Set,
    Error,
    Indeterminate,
    Pause,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuerySnapshot {
    Enquiry,
    XtVersion,
    Size,
    ColorScheme,
    DeviceAttributes,
    Clipboard,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TerminalColorScheme {
    Light,
    #[default]
    Dark,
}

/// Virtual terminal values supplied to Ghostty's query callbacks.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TerminalProfile {
    pub rows: u16,
    pub columns: u16,
    pub cell_width: u32,
    pub cell_height: u32,
    pub color_scheme: TerminalColorScheme,
    pub enquiry: Vec<u8>,
    pub version: String,
    pub da_conformance: u16,
    pub da_features: Vec<u16>,
    pub da_device_type: u16,
    pub da_firmware_version: u16,
    pub da_unit_id: u32,
    pub clipboard_read: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EffectSnapshot {
    Bell,
    TitleChanged(String),
    WorkingDirectoryChanged(String),
    ClipboardWrite {
        location: ClipboardLocationSnapshot,
        contents: Vec<ClipboardContentSnapshot>,
    },
    DesktopNotification {
        title: String,
        body: String,
    },
    ProgressReport {
        state: ProgressStateSnapshot,
        progress: Option<u8>,
    },
    Query(QuerySnapshot),
    PtyReply(Vec<u8>),
    UnknownSequence {
        content: Vec<u8>,
        truncated: bool,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PrintBoundarySnapshot {
    #[default]
    Continue,
    LineFeed,
    CarriageReturn,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PrintedRunSnapshot {
    pub text: String,
    pub boundary: PrintBoundarySnapshot,
}

/// A non-authoritative operation hint observed in the same VT stream Ghostty
/// consumed. Renderers must validate these hints against Ghostty's resulting
/// cells before using them as an optimization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationSnapshot {
    ScrollUp { top: u16, bottom: u16, count: u16 },
    ScrollDown { top: u16, bottom: u16, count: u16 },
    InsertLines { row: u16, bottom: u16, count: u16 },
    DeleteLines { row: u16, bottom: u16, count: u16 },
    InsertChars { row: u16, col: u16, count: u16 },
    DeleteChars { row: u16, col: u16, count: u16 },
    EraseChars { row: u16, col: u16, count: u16 },
    WriteRun { row: u16, col: u16, text: String },
}

/// Dirty state consumed from Ghostty's stateful render API for one write.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum RenderDamageSnapshot {
    #[default]
    None,
    Rows(Vec<RangeInclusive<u16>>),
    Full,
}

impl RenderDamageSnapshot {
    fn merge(&mut self, next: Self) {
        *self = match (std::mem::take(self), next) {
            (Self::Full, _) | (_, Self::Full) => Self::Full,
            (Self::None, damage) | (damage, Self::None) => damage,
            (Self::Rows(mut left), Self::Rows(mut right)) => {
                left.append(&mut right);
                normalize_row_ranges(&mut left);
                Self::Rows(left)
            }
        };
    }

    fn changed_rows(&self, row_count: usize) -> Vec<RangeInclusive<u16>> {
        match self {
            Self::None => Vec::new(),
            Self::Rows(rows) => rows.clone(),
            Self::Full => (row_count > 0)
                .then(|| 0..=row_index(row_count - 1))
                .into_iter()
                .collect(),
        }
    }
}

/// Operation and damage facts produced by the same write that mutated Ghostty.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UpdateSnapshot {
    pub effects: Vec<EffectSnapshot>,
    pub pty_replies: Vec<u8>,
    pub printed_runs: Vec<PrintedRunSnapshot>,
    /// The observed stream used cursor-addressed or otherwise structural
    /// terminal operations, so its printed runs are not a linear output
    /// record. This is accessibility provenance only; Ghostty's snapshot
    /// remains authoritative.
    pub output_report_structural: bool,
    /// Ghostty retained an incomplete parser continuation after this update.
    /// Even an observed LF is not an accessibility boundary until the
    /// authoritative terminal parser has returned to ground state.
    pub parser_continuation: bool,
    pub operations: Vec<OperationSnapshot>,
    pub cursor_operations: usize,
    /// Cursor operations observed after the most recent LF in this update.
    /// A TUI commonly prints rows linearly and then addresses its application
    /// cursor, whereas ordinary line output leaves the cursor after the LF.
    pub cursor_operations_after_last_line_feed: usize,
    pub scroll_operations: usize,
    /// Primary-screen retained history may have changed during this update.
    pub history_changed: bool,
    pub damage: RenderDamageSnapshot,
    pub changed_rows: Vec<RangeInclusive<u16>>,
    pub cursor_before: CursorSnapshot,
    pub cursor_after: CursorSnapshot,
    pub alternate_screen_before: bool,
    pub alternate_screen_after: bool,
    pub synchronized_output: bool,
    /// This update ended exactly at a real true-to-false synchronized-output
    /// boundary. Ordinary bytes after the close make this false.
    pub synchronized_output_closed: bool,
    /// This update ended at an OSC 133 `B` input-start boundary, followed only
    /// by controls proven not to alter the displayed output. Visible text,
    /// structural output, another semantic phase, or an incomplete parser
    /// continuation clears the boundary.
    pub semantic_input_boundary: bool,
    /// This update ended exactly at a real hidden-to-visible cursor
    /// transition. This records application painting behavior; it is not a
    /// transaction or accessibility commit boundary.
    pub cursor_visibility_restored: bool,
    /// The visible terminal model immediately after an actual false-to-true
    /// synchronized-output transition. The marker itself only changes mode,
    /// so after clearing that mode bit this is the exact committed model from
    /// immediately before the transaction opened. Retained history is
    /// included only when the prefix before this opener changed it; callers
    /// can otherwise reuse their previous committed history allocation.
    pub synchronized_output_open_snapshot: Option<TerminalSnapshot>,
}

/// Ghostty state normalized for Lector's engine-neutral consumers.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TerminalSnapshot {
    pub rows: Arc<Vec<RowSnapshot>>,
    pub scrollback: Vec<RowSnapshot>,
    pub cursor: CursorSnapshot,
    pub width_px: u32,
    pub height_px: u32,
    pub alternate_screen: bool,
    pub modes: ModesSnapshot,
    pub title: Option<String>,
    pub working_directory: Option<String>,
    /// Monotonic lineage coordinate of the first row in Lector's bounded
    /// primary-screen history window. This advances when the oldest logical
    /// row is evicted even though `scrollback_extent` remains at its cap.
    /// It is deliberately distinct from Ghostty's page-relative grid offset,
    /// which can move backwards when Ghostty prunes a whole allocation page.
    pub history_origin: usize,
    pub scrollback_extent: usize,
    pub semantic_marks: Vec<SemanticMarkSnapshot>,
}

/// An unstable Ghostty snapshot plus the Lector observer continuation needed
/// for a diagnostic round-trip.
///
/// This is intentionally opaque and is not a persistence or compatibility
/// promise. Ghostty snapshot format version 1 is unstable, and Lector does not
/// use this path for live runtime correctness.
pub struct DiagnosticSnapshot {
    bytes: Vec<u8>,
    observer_continuation: Vec<u8>,
    active_hyperlink: Option<String>,
    scrollback_capacity: usize,
    terminal_profile: TerminalProfile,
    primary_history_origin: usize,
    primary_history_high_water: usize,
}

impl TerminalSnapshot {
    pub fn size(&self) -> (u16, u16) {
        (
            self.rows.len().try_into().unwrap_or(u16::MAX),
            self.rows
                .first()
                .map_or(0, |row| row.cells.len().try_into().unwrap_or(u16::MAX)),
        )
    }
}

struct TerminalHandle(NonNull<c_void>);

impl TerminalHandle {
    fn new(rows: u16, cols: u16) -> Result<Self, Error> {
        Self::new_with_allocator(rows, cols, std::ptr::null())
    }

    fn new_with_allocator(
        rows: u16,
        cols: u16,
        allocator: *const ffi::Allocator,
    ) -> Result<Self, Error> {
        if rows == 0 || cols == 0 {
            return Err(Error::InvalidValue);
        }
        let mut handle = std::ptr::null_mut();
        // SAFETY: `handle` points to writable storage, dimensions were
        // validated, and the private caller supplies either null or a valid
        // allocator for the duration of this synchronous call.
        let result = unsafe { ffi::ghostty_terminal_new(allocator, &mut handle, cols, rows) };
        if let Err(error) = result_from_code(result) {
            debug_assert!(handle.is_null(), "Ghostty returned a handle after an error");
            return Err(error);
        }
        NonNull::new(handle).map(Self).ok_or(Error::NullHandle)
    }

    fn as_ptr(&self) -> ffi::Terminal {
        self.0.as_ptr()
    }

    fn from_raw(handle: ffi::Terminal) -> Result<Self, Error> {
        NonNull::new(handle).map(Self).ok_or(Error::NullHandle)
    }

    fn set_scrollback_capacity(&self, capacity: usize) -> Result<(), Error> {
        // The C terminal starts with Ghostty's small byte limit. It can prune
        // a complete page long before the independent line limit, leaving
        // fewer rows than Lector's contract. Remove it and bound retention by
        // the physical line limit below instead.
        result_from_code(unsafe {
            ffi::ghostty_terminal_set(
                self.as_ptr(),
                ffi::TERMINAL_OPT_SCROLLBACK_MAX_BYTES,
                std::ptr::null(),
            )
        })?;
        // SAFETY: the handle is valid and Ghostty synchronously copies the
        // documented size_t option from this pointer.
        result_from_code(unsafe {
            ffi::ghostty_terminal_set(
                self.as_ptr(),
                ffi::TERMINAL_OPT_SCROLLBACK_MAX_LINES,
                (&capacity as *const usize).cast(),
            )
        })
    }

    fn set_option(&self, option: ffi::TerminalOption, value: *const c_void) -> Result<(), Error> {
        // SAFETY: the handle is valid. Each caller supplies the pointer type
        // documented for the selected option, and Ghostty consumes it
        // synchronously except for userdata and function pointers whose
        // lifetimes are tied to the owning `Terminal` below.
        result_from_code(unsafe { ffi::ghostty_terminal_set(self.as_ptr(), option, value) })
    }
}

impl Drop for TerminalHandle {
    fn drop(&mut self) {
        // SAFETY: this handle was created successfully and is owned here.
        unsafe { ffi::ghostty_terminal_free(self.as_ptr()) };
    }
}

struct KittyPlacementIteratorHandle(NonNull<c_void>);

impl KittyPlacementIteratorHandle {
    fn new() -> Result<Self, Error> {
        let mut handle = std::ptr::null_mut();
        // SAFETY: the output points to writable storage and null selects
        // Ghostty's default allocator. A successful result owns the iterator.
        result_from_code(unsafe {
            ffi::ghostty_kitty_graphics_placement_iterator_new(std::ptr::null(), &mut handle)
        })?;
        NonNull::new(handle).map(Self).ok_or(Error::NullHandle)
    }

    fn as_ptr(&self) -> ffi::KittyGraphicsPlacementIterator {
        self.0.as_ptr()
    }

    fn populate(&mut self, graphics: ffi::KittyGraphics) -> Result<(), Error> {
        let mut handle = self.as_ptr();
        // SAFETY: both handles are valid and `handle` is writable storage for
        // the documented pre-allocated iterator query.
        result_from_code(unsafe {
            ffi::ghostty_kitty_graphics_get(
                graphics,
                ffi::KITTY_GRAPHICS_DATA_PLACEMENT_ITERATOR,
                (&mut handle as *mut ffi::KittyGraphicsPlacementIterator).cast(),
            )
        })?;
        self.0 = NonNull::new(handle).ok_or(Error::NullHandle)?;
        Ok(())
    }
}

impl Drop for KittyPlacementIteratorHandle {
    fn drop(&mut self) {
        // SAFETY: this iterator is uniquely owned and is independent of the
        // borrowed graphics/image handles it was populated from.
        unsafe { ffi::ghostty_kitty_graphics_placement_iterator_free(self.as_ptr()) };
    }
}

struct SnapshotDecoderHandle(NonNull<c_void>);

impl SnapshotDecoderHandle {
    fn new(bytes: &[u8]) -> Result<Self, Error> {
        let mut handle = std::ptr::null_mut();
        // SAFETY: Ghostty borrows this non-null byte slice only for the
        // decoder's synchronous lifetime, which remains nested inside the
        // caller that owns `bytes`.
        result_from_code(unsafe {
            ffi::ghostty_snapshot_decoder_new_buf(
                std::ptr::null(),
                &mut handle,
                bytes.as_ptr(),
                bytes.len(),
            )
        })?;
        NonNull::new(handle).map(Self).ok_or(Error::NullHandle)
    }

    fn decode(&self) -> Result<TerminalHandle, Error> {
        let mut terminal = std::ptr::null_mut();
        // SAFETY: the decoder is valid and `terminal` points to writable
        // storage. A successful complete decode transfers terminal ownership
        // to the caller.
        result_from_code(unsafe {
            ffi::ghostty_snapshot_decoder_decode(self.0.as_ptr(), &mut terminal)
        })?;
        TerminalHandle::from_raw(terminal)
    }
}

impl Drop for SnapshotDecoderHandle {
    fn drop(&mut self) {
        // SAFETY: this decoder handle is uniquely owned and may be freed after
        // its one-shot decode completes or fails.
        unsafe { ffi::ghostty_snapshot_decoder_free(self.0.as_ptr()) };
    }
}

const UNKNOWN_SEQUENCE_MAX_BYTES: usize = 256;
const CONTINUATION_MAX_BYTES: usize = 64 * 1024;
const KITTY_IMAGE_STORAGE_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

fn stable_digest(bytes: &[u8]) -> u64 {
    let mut digest = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        digest ^= u64::from(*byte);
        digest = digest.wrapping_mul(0x0000_0100_0000_01b3);
    }
    digest
}

#[derive(Default)]
struct EffectSink {
    events: Vec<EffectSnapshot>,
    pty_replies: Vec<u8>,
    error: Option<Error>,
    terminal_profile: TerminalProfile,
}

impl EffectSink {
    fn record_error(&mut self, error: Error) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }

    fn take(&mut self) -> Result<(Vec<EffectSnapshot>, Vec<u8>), Error> {
        if let Some(error) = self.error.take() {
            self.events.clear();
            self.pty_replies.clear();
            return Err(error);
        }
        Ok((
            std::mem::take(&mut self.events),
            std::mem::take(&mut self.pty_replies),
        ))
    }
}

unsafe fn effect_sink<'a>(userdata: *mut c_void) -> Option<&'a mut EffectSink> {
    // SAFETY: the caller guarantees this is the registered boxed sink and that
    // the callback's temporary exclusive access does not overlap another use.
    unsafe { userdata.cast::<EffectSink>().as_mut() }
}

fn callback_string(value: ffi::GhosttyString) -> Result<String, Error> {
    if value.len == 0 {
        return Ok(String::new());
    }
    if value.ptr.is_null() {
        return Err(Error::NullString);
    }
    // SAFETY: callback strings are borrowed for the callback duration and are
    // copied before returning to Ghostty.
    let bytes = unsafe { std::slice::from_raw_parts(value.ptr, value.len) };
    Ok(std::str::from_utf8(bytes)
        .map_err(|_| Error::InvalidUtf8)?
        .to_owned())
}

fn callback_terminal_string(
    terminal: ffi::Terminal,
    tag: ffi::TerminalData,
) -> Result<String, Error> {
    let mut value = ffi::GhosttyString::default();
    // SAFETY: the callback-provided terminal remains valid for the callback
    // duration and `value` is writable storage for the requested string.
    result_from_code(unsafe {
        ffi::ghostty_terminal_get(
            terminal,
            tag,
            (&mut value as *mut ffi::GhosttyString).cast(),
        )
    })?;
    callback_string(value)
}

extern "C" fn effect_write_pty(
    _terminal: ffi::Terminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    // SAFETY: this callback receives the stable userdata registered below and
    // Ghostty invokes callbacks synchronously on the owning thread.
    let Some(sink) = (unsafe { effect_sink(userdata) }) else {
        return;
    };
    if len > 0 && data.is_null() {
        sink.record_error(Error::InvalidValue);
        return;
    }
    // SAFETY: Ghostty guarantees the response bytes remain valid for this
    // callback. A zero-length null slice is represented without dereference.
    let bytes = if len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(data, len) }
    };
    sink.pty_replies.extend_from_slice(bytes);
    sink.events.push(EffectSnapshot::PtyReply(bytes.to_vec()));
}

extern "C" fn effect_bell(_terminal: ffi::Terminal, userdata: *mut c_void) {
    // SAFETY: see `effect_write_pty`.
    if let Some(sink) = unsafe { effect_sink(userdata) } {
        sink.events.push(EffectSnapshot::Bell);
    }
}

extern "C" fn effect_title_changed(terminal: ffi::Terminal, userdata: *mut c_void) {
    // SAFETY: see `effect_write_pty`.
    let Some(sink) = (unsafe { effect_sink(userdata) }) else {
        return;
    };
    match callback_terminal_string(terminal, ffi::TERMINAL_DATA_TITLE) {
        Ok(title) => sink.events.push(EffectSnapshot::TitleChanged(title)),
        Err(error) => sink.record_error(error),
    }
}

extern "C" fn effect_pwd_changed(terminal: ffi::Terminal, userdata: *mut c_void) {
    // SAFETY: see `effect_write_pty`.
    let Some(sink) = (unsafe { effect_sink(userdata) }) else {
        return;
    };
    match callback_terminal_string(terminal, ffi::TERMINAL_DATA_PWD) {
        Ok(path) => sink
            .events
            .push(EffectSnapshot::WorkingDirectoryChanged(path)),
        Err(error) => sink.record_error(error),
    }
}

extern "C" fn effect_enquiry(
    _terminal: ffi::Terminal,
    userdata: *mut c_void,
) -> ffi::GhosttyString {
    // SAFETY: see `effect_write_pty`.
    let Some(sink) = (unsafe { effect_sink(userdata) }) else {
        return ffi::GhosttyString::default();
    };
    sink.events
        .push(EffectSnapshot::Query(QuerySnapshot::Enquiry));
    ffi::GhosttyString {
        ptr: sink.terminal_profile.enquiry.as_ptr(),
        len: sink.terminal_profile.enquiry.len(),
    }
}

extern "C" fn effect_xtversion(
    _terminal: ffi::Terminal,
    userdata: *mut c_void,
) -> ffi::GhosttyString {
    // SAFETY: see `effect_write_pty`.
    let Some(sink) = (unsafe { effect_sink(userdata) }) else {
        return ffi::GhosttyString::default();
    };
    sink.events
        .push(EffectSnapshot::Query(QuerySnapshot::XtVersion));
    ffi::GhosttyString {
        ptr: sink.terminal_profile.version.as_ptr(),
        len: sink.terminal_profile.version.len(),
    }
}

extern "C" fn effect_size(
    _terminal: ffi::Terminal,
    userdata: *mut c_void,
    out: *mut ffi::SizeReportSize,
) -> bool {
    // SAFETY: see `effect_write_pty`.
    if let Some(sink) = unsafe { effect_sink(userdata) } {
        sink.events.push(EffectSnapshot::Query(QuerySnapshot::Size));
        let Some(out) = (unsafe { out.as_mut() }) else {
            sink.record_error(Error::InvalidValue);
            return false;
        };
        *out = ffi::SizeReportSize {
            rows: sink.terminal_profile.rows,
            columns: sink.terminal_profile.columns,
            cell_width: sink.terminal_profile.cell_width,
            cell_height: sink.terminal_profile.cell_height,
        };
        return true;
    }
    false
}

extern "C" fn effect_color_scheme(
    _terminal: ffi::Terminal,
    userdata: *mut c_void,
    out: *mut ffi::ColorScheme,
) -> bool {
    // SAFETY: see `effect_write_pty`.
    if let Some(sink) = unsafe { effect_sink(userdata) } {
        sink.events
            .push(EffectSnapshot::Query(QuerySnapshot::ColorScheme));
        let Some(out) = (unsafe { out.as_mut() }) else {
            sink.record_error(Error::InvalidValue);
            return false;
        };
        *out = match sink.terminal_profile.color_scheme {
            TerminalColorScheme::Light => ffi::COLOR_SCHEME_LIGHT,
            TerminalColorScheme::Dark => ffi::COLOR_SCHEME_DARK,
        };
        return true;
    }
    false
}

extern "C" fn effect_device_attributes(
    _terminal: ffi::Terminal,
    userdata: *mut c_void,
    out: *mut ffi::DeviceAttributes,
) -> bool {
    // SAFETY: see `effect_write_pty`.
    if let Some(sink) = unsafe { effect_sink(userdata) } {
        sink.events
            .push(EffectSnapshot::Query(QuerySnapshot::DeviceAttributes));
        let Some(out) = (unsafe { out.as_mut() }) else {
            sink.record_error(Error::InvalidValue);
            return false;
        };
        let features_len = sink.terminal_profile.da_features.len().min(64);
        let mut features = [0; 64];
        features[..features_len]
            .copy_from_slice(&sink.terminal_profile.da_features[..features_len]);
        *out = ffi::DeviceAttributes {
            primary: ffi::DeviceAttributesPrimary {
                conformance_level: sink.terminal_profile.da_conformance,
                features,
                num_features: features_len,
            },
            secondary: ffi::DeviceAttributesSecondary {
                device_type: sink.terminal_profile.da_device_type,
                firmware_version: sink.terminal_profile.da_firmware_version,
                rom_cartridge: 0,
            },
            tertiary: ffi::DeviceAttributesTertiary {
                unit_id: sink.terminal_profile.da_unit_id,
            },
        };
        return true;
    }
    false
}

extern "C" fn effect_clipboard_write(
    _terminal: ffi::Terminal,
    userdata: *mut c_void,
    write: *const ffi::ClipboardWrite,
) -> ffi::ClipboardWriteResult {
    // SAFETY: see `effect_write_pty`.
    let Some(sink) = (unsafe { effect_sink(userdata) }) else {
        return ffi::CLIPBOARD_WRITE_RESULT_SUCCESS;
    };
    let Some(write) = (unsafe { write.as_ref() }) else {
        sink.record_error(Error::InvalidValue);
        return ffi::CLIPBOARD_WRITE_RESULT_SUCCESS;
    };
    if write.size < std::mem::size_of::<ffi::ClipboardWrite>() {
        sink.record_error(Error::InvalidValue);
        return ffi::CLIPBOARD_WRITE_RESULT_SUCCESS;
    }
    let location = match write.location {
        ffi::CLIPBOARD_LOCATION_STANDARD => ClipboardLocationSnapshot::Standard,
        ffi::CLIPBOARD_LOCATION_SELECTION => ClipboardLocationSnapshot::Selection,
        ffi::CLIPBOARD_LOCATION_PRIMARY => ClipboardLocationSnapshot::Primary,
        _ => {
            sink.record_error(Error::InvalidValue);
            return ffi::CLIPBOARD_WRITE_RESULT_SUCCESS;
        }
    };
    if write.contents_len > 0 && write.contents.is_null() {
        sink.record_error(Error::InvalidValue);
        return ffi::CLIPBOARD_WRITE_RESULT_SUCCESS;
    }
    let contents = if write.contents_len == 0 {
        &[]
    } else {
        // SAFETY: Ghostty guarantees this borrowed array has `contents_len`
        // entries for the callback duration.
        unsafe { std::slice::from_raw_parts(write.contents, write.contents_len) }
    };
    let mut copied = Vec::with_capacity(contents.len());
    for content in contents {
        let mime = match callback_string(content.mime) {
            Ok(value) => value,
            Err(error) => {
                sink.record_error(error);
                return ffi::CLIPBOARD_WRITE_RESULT_SUCCESS;
            }
        };
        let data = if content.data.len == 0 {
            Vec::new()
        } else if content.data.ptr.is_null() {
            sink.record_error(Error::NullString);
            return ffi::CLIPBOARD_WRITE_RESULT_SUCCESS;
        } else {
            // SAFETY: content bytes are borrowed for this callback and copied.
            unsafe { std::slice::from_raw_parts(content.data.ptr, content.data.len) }.to_vec()
        };
        copied.push(ClipboardContentSnapshot { mime, data });
    }
    sink.events.push(EffectSnapshot::ClipboardWrite {
        location,
        contents: copied,
    });
    ffi::CLIPBOARD_WRITE_RESULT_SUCCESS
}

extern "C" fn effect_desktop_notification(
    _terminal: ffi::Terminal,
    userdata: *mut c_void,
    notification: *const ffi::DesktopNotification,
) {
    // SAFETY: see `effect_write_pty`.
    let Some(sink) = (unsafe { effect_sink(userdata) }) else {
        return;
    };
    let Some(notification) = (unsafe { notification.as_ref() }) else {
        sink.record_error(Error::InvalidValue);
        return;
    };
    if notification.size < std::mem::size_of::<ffi::DesktopNotification>() {
        sink.record_error(Error::InvalidValue);
        return;
    }
    match (
        callback_string(notification.title),
        callback_string(notification.body),
    ) {
        (Ok(title), Ok(body)) => sink
            .events
            .push(EffectSnapshot::DesktopNotification { title, body }),
        (Err(error), _) | (_, Err(error)) => sink.record_error(error),
    }
}

extern "C" fn effect_progress_report(
    _terminal: ffi::Terminal,
    userdata: *mut c_void,
    report: *const ffi::ProgressReport,
) {
    // SAFETY: see `effect_write_pty`.
    let Some(sink) = (unsafe { effect_sink(userdata) }) else {
        return;
    };
    let Some(report) = (unsafe { report.as_ref() }) else {
        sink.record_error(Error::InvalidValue);
        return;
    };
    if report.size < std::mem::size_of::<ffi::ProgressReport>() {
        sink.record_error(Error::InvalidValue);
        return;
    }
    let state = match report.state {
        ffi::PROGRESS_STATE_REMOVE => ProgressStateSnapshot::Remove,
        ffi::PROGRESS_STATE_SET => ProgressStateSnapshot::Set,
        ffi::PROGRESS_STATE_ERROR => ProgressStateSnapshot::Error,
        ffi::PROGRESS_STATE_INDETERMINATE => ProgressStateSnapshot::Indeterminate,
        ffi::PROGRESS_STATE_PAUSE => ProgressStateSnapshot::Pause,
        _ => {
            sink.record_error(Error::InvalidValue);
            return;
        }
    };
    let progress = if report.progress < 0 {
        None
    } else {
        Some(report.progress as u8)
    };
    sink.events
        .push(EffectSnapshot::ProgressReport { state, progress });
}

extern "C" fn effect_unknown_sequence(
    _terminal: ffi::Terminal,
    userdata: *mut c_void,
    sequence: *const ffi::UnknownSequence,
) {
    // SAFETY: see `effect_write_pty`.
    let Some(sink) = (unsafe { effect_sink(userdata) }) else {
        return;
    };
    let Some(sequence) = (unsafe { sequence.as_ref() }) else {
        sink.record_error(Error::InvalidValue);
        return;
    };
    if sequence.tag != ffi::UNKNOWN_SEQUENCE_APC {
        sink.record_error(Error::InvalidValue);
        return;
    }
    // SAFETY: the tag identifies the active APC member of the C union.
    let apc = unsafe { sequence.value.apc };
    if apc.content.len > 0 && apc.content.ptr.is_null() {
        sink.record_error(Error::NullString);
        return;
    }
    let content = if apc.content.len == 0 {
        Vec::new()
    } else {
        // SAFETY: content is valid for the callback and copied immediately.
        unsafe { std::slice::from_raw_parts(apc.content.ptr, apc.content.len) }.to_vec()
    };
    sink.events.push(EffectSnapshot::UnknownSequence {
        content,
        truncated: apc.truncated,
    });
}

fn register_effects(terminal: &TerminalHandle, sink: &mut EffectSink) -> Result<(), Error> {
    terminal.set_option(
        ffi::TERMINAL_OPT_USERDATA,
        (sink as *mut EffectSink).cast::<c_void>(),
    )?;
    for (option, callback) in [
        (
            ffi::TERMINAL_OPT_WRITE_PTY,
            effect_write_pty as *const c_void,
        ),
        (ffi::TERMINAL_OPT_BELL, effect_bell as *const c_void),
        (
            ffi::TERMINAL_OPT_TITLE_CHANGED,
            effect_title_changed as *const c_void,
        ),
        (
            ffi::TERMINAL_OPT_PWD_CHANGED,
            effect_pwd_changed as *const c_void,
        ),
        (ffi::TERMINAL_OPT_ENQUIRY, effect_enquiry as *const c_void),
        (
            ffi::TERMINAL_OPT_XTVERSION,
            effect_xtversion as *const c_void,
        ),
        (ffi::TERMINAL_OPT_SIZE, effect_size as *const c_void),
        (
            ffi::TERMINAL_OPT_COLOR_SCHEME,
            effect_color_scheme as *const c_void,
        ),
        (
            ffi::TERMINAL_OPT_DEVICE_ATTRIBUTES,
            effect_device_attributes as *const c_void,
        ),
        (
            ffi::TERMINAL_OPT_CLIPBOARD_WRITE,
            effect_clipboard_write as *const c_void,
        ),
        (
            ffi::TERMINAL_OPT_DESKTOP_NOTIFICATION,
            effect_desktop_notification as *const c_void,
        ),
        (
            ffi::TERMINAL_OPT_PROGRESS_REPORT,
            effect_progress_report as *const c_void,
        ),
        (
            ffi::TERMINAL_OPT_UNKNOWN_SEQUENCE,
            effect_unknown_sequence as *const c_void,
        ),
    ] {
        terminal.set_option(option, callback)?;
    }
    terminal.set_option(
        ffi::TERMINAL_OPT_UNKNOWN_MAX_BYTES,
        (&UNKNOWN_SEQUENCE_MAX_BYTES as *const usize).cast(),
    )
}

struct RenderStateHandle(NonNull<c_void>);

impl RenderStateHandle {
    fn new() -> Result<Self, Error> {
        Self::new_with_allocator(std::ptr::null())
    }

    fn new_with_allocator(allocator: *const ffi::Allocator) -> Result<Self, Error> {
        let mut handle = std::ptr::null_mut();
        // SAFETY: `handle` points to writable storage and the private caller
        // supplies either null or a valid allocator for this call.
        let result = unsafe { ffi::ghostty_render_state_new(allocator, &mut handle) };
        if let Err(error) = result_from_code(result) {
            debug_assert!(handle.is_null(), "Ghostty returned a handle after an error");
            return Err(error);
        }
        NonNull::new(handle).map(Self).ok_or(Error::NullHandle)
    }

    fn as_ptr(&self) -> ffi::RenderState {
        self.0.as_ptr()
    }
}

impl Drop for RenderStateHandle {
    fn drop(&mut self) {
        // SAFETY: this handle was created successfully and is owned here.
        unsafe { ffi::ghostty_render_state_free(self.as_ptr()) };
    }
}

struct RowIteratorHandle(NonNull<c_void>);

impl RowIteratorHandle {
    fn new() -> Result<Self, Error> {
        Self::new_with_allocator(std::ptr::null())
    }

    fn new_with_allocator(allocator: *const ffi::Allocator) -> Result<Self, Error> {
        let mut handle = std::ptr::null_mut();
        // SAFETY: `handle` points to writable storage and the private caller
        // supplies either null or a valid allocator for this call.
        let result = unsafe { ffi::ghostty_render_state_row_iterator_new(allocator, &mut handle) };
        if let Err(error) = result_from_code(result) {
            debug_assert!(handle.is_null(), "Ghostty returned a handle after an error");
            return Err(error);
        }
        NonNull::new(handle).map(Self).ok_or(Error::NullHandle)
    }

    fn as_ptr(&self) -> ffi::RenderStateRowIterator {
        self.0.as_ptr()
    }
}

impl Drop for RowIteratorHandle {
    fn drop(&mut self) {
        // SAFETY: this handle was created successfully and is owned here.
        unsafe { ffi::ghostty_render_state_row_iterator_free(self.as_ptr()) };
    }
}

struct RowCellsHandle(NonNull<c_void>);

impl RowCellsHandle {
    fn new() -> Result<Self, Error> {
        Self::new_with_allocator(std::ptr::null())
    }

    fn new_with_allocator(allocator: *const ffi::Allocator) -> Result<Self, Error> {
        let mut handle = std::ptr::null_mut();
        // SAFETY: `handle` points to writable storage and the private caller
        // supplies either null or a valid allocator for this call.
        let result = unsafe { ffi::ghostty_render_state_row_cells_new(allocator, &mut handle) };
        if let Err(error) = result_from_code(result) {
            debug_assert!(handle.is_null(), "Ghostty returned a handle after an error");
            return Err(error);
        }
        NonNull::new(handle).map(Self).ok_or(Error::NullHandle)
    }

    fn as_ptr(&self) -> ffi::RenderStateRowCells {
        self.0.as_ptr()
    }
}

impl Drop for RowCellsHandle {
    fn drop(&mut self) {
        // SAFETY: this handle was created successfully and is owned here.
        unsafe { ffi::ghostty_render_state_row_cells_free(self.as_ptr()) };
    }
}

/// A sparse, owned Ghostty anchor that follows a cell through scrolling and
/// resize/reflow. It becomes valueless when Ghostty discards the cell.
pub struct TrackedGridRef {
    handle: NonNull<c_void>,
    _thread_bound: PhantomData<Rc<()>>,
}

impl TrackedGridRef {
    /// Returns this anchor's position in Ghostty's full active screen,
    /// including scrollback, or `None` after its location is discarded.
    pub fn screen_position(&self) -> Result<Option<(usize, u16)>, Error> {
        // SAFETY: the owned tracked handle is valid. Ghostty explicitly
        // permits these queries after the originating terminal is freed.
        if !unsafe { ffi::ghostty_tracked_grid_ref_has_value(self.handle.as_ptr()) } {
            return Ok(None);
        }
        let mut coordinate = ffi::PointCoordinate::default();
        // SAFETY: output is valid writable storage and the tag requests the
        // documented full-screen coordinate system.
        let result = unsafe {
            ffi::ghostty_tracked_grid_ref_point(
                self.handle.as_ptr(),
                ffi::POINT_TAG_SCREEN,
                &mut coordinate,
            )
        };
        if result == ffi::NO_VALUE {
            return Ok(None);
        }
        result_from_code(result)?;
        Ok(Some((coordinate.y as usize, coordinate.x)))
    }
}

impl Drop for TrackedGridRef {
    fn drop(&mut self) {
        // SAFETY: the handle is owned here and Ghostty permits freeing it
        // even after its originating terminal has been freed.
        unsafe { ffi::ghostty_tracked_grid_ref_free(self.handle.as_ptr()) };
    }
}

struct TrackedSemanticMark {
    kind: SemanticKindSnapshot,
    reference: TrackedGridRef,
    alternate_screen: bool,
    last_position: (usize, u16),
    row_offset: isize,
}

/// The stable primary active-top/history-boundary cell. Ghostty moves this
/// reference when it prunes physical history pages, which lets us distinguish
/// page-coordinate rebasing from eviction out of Lector's logical window.
struct HistoryLineageAnchor {
    reference: TrackedGridRef,
    absolute_row: usize,
}

#[derive(Clone)]
struct CachedKittyImage {
    data: Arc<[u8]>,
    data_digest: u64,
}

#[derive(Default)]
struct StreamObserver {
    events: Vec<SemanticKindSnapshot>,
    printed_runs: Vec<PrintedRunSnapshot>,
    current_print: String,
    output_report_structural: bool,
    raw_escape_pending: bool,
    raw_utf8_continuations: u8,
    raw_utf8_next_min: u8,
    raw_utf8_next_max: u8,
    operations: Vec<OperationSnapshot>,
    operation_rows: u16,
    operation_cols: u16,
    operation_row: u16,
    operation_col: u16,
    scroll_region: Option<(u16, u16)>,
    origin_mode: bool,
    left_right_margin_mode: bool,
    autowrap: bool,
    alternate_screen: bool,
    operation_reliable: bool,
    last_printed: Option<char>,
    cursor_operations: usize,
    cursor_operations_after_last_line_feed: usize,
    scroll_operations: usize,
    history_cleared: bool,
    history_changed: bool,
    active_hyperlink: Option<String>,
    clipboard_read_queries: Vec<Vec<u8>>,
    default_color_queries: Vec<u8>,
    synchronized_output_boundary: Option<bool>,
    cursor_visibility_boundary: Option<bool>,
    semantic_input_boundary: bool,
}

impl StreamObserver {
    fn begin_update(&mut self, snapshot: &TerminalSnapshot) {
        self.operations.clear();
        self.semantic_input_boundary = false;
        self.output_report_structural = snapshot.alternate_screen
            || !snapshot.cursor.visible
            || snapshot.modes.synchronized_output
            || snapshot.modes.application_keypad
            || snapshot.modes.application_cursor
            || snapshot.modes.mouse_protocol != MouseProtocol::None
            || self.scroll_region.is_some()
            || self.origin_mode
            || self.left_right_margin_mode;
        self.operation_rows = snapshot.rows.len().try_into().unwrap_or(u16::MAX);
        self.operation_cols = snapshot
            .rows
            .first()
            .map_or(0, |row| row.cells.len().try_into().unwrap_or(u16::MAX));
        self.operation_row = snapshot.cursor.row;
        self.operation_col = snapshot.cursor.col;
        self.alternate_screen = snapshot.alternate_screen;
        self.operation_reliable = self.operation_rows > 0 && self.operation_cols > 0;
        if self.scroll_region.is_some_and(|(top, bottom)| {
            top >= self.operation_rows || bottom >= self.operation_rows
        }) {
            self.scroll_region = None;
        }
    }

    fn scroll_bounds(&self) -> (u16, u16) {
        self.scroll_region
            .unwrap_or((0, self.operation_rows.saturating_sub(1)))
    }

    fn parameter(params: &vte::Params, index: usize, default: u16) -> u16 {
        params
            .iter()
            .nth(index)
            .and_then(|values| values.first())
            .copied()
            .filter(|value| *value != 0)
            .unwrap_or(default)
    }

    fn count(params: &vte::Params) -> u16 {
        Self::parameter(params, 0, 1)
    }

    fn record_cursor_operation(&mut self) {
        self.cursor_operations = self.cursor_operations.saturating_add(1);
        self.cursor_operations_after_last_line_feed = self
            .cursor_operations_after_last_line_feed
            .saturating_add(1);
    }

    fn private_modes_are_output_neutral(params: &vte::Params) -> bool {
        let mut count = 0usize;
        for values in params.iter() {
            for value in values {
                count = count.saturating_add(1);
                if *value != 2004 {
                    return false;
                }
            }
        }
        count == 1
    }

    /// Observe raw framing which `vte::Perform` does not report. In
    /// particular, vte 0.15 silently enters SOS/PM/APC string state, so the
    /// `ESC _` Kitty-graphics introducer has no callback which could taint the
    /// accessibility print report. Retain a single ESC across update
    /// boundaries so a fragmented introducer is classified identically. The
    /// 8-bit C1 APC byte is structural when it occurs at a UTF-8 code-point
    /// boundary. Track continuation bytes across updates so the same byte in a
    /// valid multibyte character does not unnecessarily disable linear output.
    fn observe_raw_byte(&mut self, byte: u8) {
        if self.raw_utf8_continuations > 0 {
            if (self.raw_utf8_next_min..=self.raw_utf8_next_max).contains(&byte) {
                self.raw_utf8_continuations -= 1;
                self.raw_utf8_next_min = 0x80;
                self.raw_utf8_next_max = 0xbf;
                self.raw_escape_pending = false;
                return;
            }
            self.raw_utf8_continuations = 0;
        }
        if byte == 0x9f || (self.raw_escape_pending && byte == b'_') {
            self.mark_structural_output();
        }
        self.raw_escape_pending = byte == b'\x1b';
        (
            self.raw_utf8_continuations,
            self.raw_utf8_next_min,
            self.raw_utf8_next_max,
        ) = match byte {
            0xc2..=0xdf => (1, 0x80, 0xbf),
            0xe0 => (2, 0xa0, 0xbf),
            0xe1..=0xec | 0xee..=0xef => (2, 0x80, 0xbf),
            0xed => (2, 0x80, 0x9f),
            0xf0 => (3, 0x90, 0xbf),
            0xf1..=0xf3 => (3, 0x80, 0xbf),
            0xf4 => (3, 0x80, 0x8f),
            _ => (0, 0, 0),
        };
    }

    fn mark_visible_output(&mut self) {
        self.semantic_input_boundary = false;
    }

    fn mark_structural_output(&mut self) {
        self.output_report_structural = true;
        self.semantic_input_boundary = false;
    }

    fn record_write(&mut self, character: char) {
        if self.operation_col >= self.operation_cols {
            self.operation_reliable = false;
            return;
        }
        if self.operation_col == self.operation_cols.saturating_sub(1) {
            if !self.alternate_screen && self.operation_row == self.operation_rows.saturating_sub(1)
            {
                // The C render API does not expose wrap-pending. Treat a
                // primary-screen right-margin write on the bottom row as a
                // possible history mutation; a false positive only refreshes
                // the bounded history cache once.
                self.history_changed = true;
            }
            // Ghostty owns wrap-pending truth. The C render API does not expose
            // that transient state, so right-margin writes remain a diff case.
            self.operation_reliable = false;
            return;
        }
        if !character.is_ascii() {
            self.operation_reliable = false;
            return;
        }
        match self.operations.last_mut() {
            Some(OperationSnapshot::WriteRun { row, col, text })
                if *row == self.operation_row
                    // `operations` is private observer state, and every
                    // character entering a WriteRun passed the ASCII guard
                    // above. Its byte length is therefore its exact column
                    // length; rescanning the growing String would make a
                    // fragmented line quadratic.
                    && col.saturating_add(text.len().try_into().unwrap_or(u16::MAX))
                        == self.operation_col =>
            {
                text.push(character);
            }
            _ => self.operations.push(OperationSnapshot::WriteRun {
                row: self.operation_row,
                col: self.operation_col,
                text: character.to_string(),
            }),
        }
        self.operation_col = self.operation_col.saturating_add(1);
        self.last_printed = Some(character);
    }

    fn record_erase(&mut self, row: u16, col: u16, count: u16) {
        let count = count.min(self.operation_cols.saturating_sub(col));
        if count > 0 && row < self.operation_rows {
            self.operations
                .push(OperationSnapshot::EraseChars { row, col, count });
        }
    }

    fn line_feed(&mut self) {
        let (top, bottom) = self.scroll_bounds();
        if self.operation_row == bottom {
            if !self.alternate_screen && top == 0 && bottom == self.operation_rows.saturating_sub(1)
            {
                self.history_changed = true;
            }
            self.operations.push(OperationSnapshot::ScrollUp {
                top,
                bottom,
                count: 1,
            });
        } else {
            self.operation_row = self
                .operation_row
                .saturating_add(1)
                .min(self.operation_rows.saturating_sub(1));
        }
    }

    fn has_boundary_events(&self) -> bool {
        !self.events.is_empty()
            || !self.clipboard_read_queries.is_empty()
            || !self.default_color_queries.is_empty()
            || self.synchronized_output_boundary.is_some()
            || self.cursor_visibility_boundary.is_some()
    }

    fn take_synchronized_output_boundary(&mut self) -> Option<bool> {
        self.synchronized_output_boundary.take()
    }

    fn take_cursor_visibility_boundary(&mut self) -> Option<bool> {
        self.cursor_visibility_boundary.take()
    }

    fn take_clipboard_read_queries(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.clipboard_read_queries)
    }

    fn take_default_color_queries(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.default_color_queries)
    }

    fn take_semantic_events(&mut self) -> Vec<SemanticKindSnapshot> {
        std::mem::take(&mut self.events)
    }

    fn take_history_checkpoint_change(&mut self) -> bool {
        let changed = std::mem::take(&mut self.history_changed) || self.history_cleared;
        self.history_cleared = false;
        changed
    }

    fn flush_print(&mut self) {
        if self.current_print.is_empty() {
            return;
        }
        self.printed_runs.push(PrintedRunSnapshot {
            text: std::mem::take(&mut self.current_print),
            boundary: PrintBoundarySnapshot::Continue,
        });
    }

    fn push_boundary(&mut self, boundary: PrintBoundarySnapshot) {
        self.flush_print();
        if boundary == PrintBoundarySnapshot::LineFeed {
            self.cursor_operations_after_last_line_feed = 0;
        }
        if boundary == PrintBoundarySnapshot::LineFeed
            && let Some(previous) = self.printed_runs.last_mut()
            && previous.text.is_empty()
            && previous.boundary == PrintBoundarySnapshot::CarriageReturn
        {
            previous.boundary = PrintBoundarySnapshot::LineFeed;
            return;
        }
        self.printed_runs.push(PrintedRunSnapshot {
            text: String::new(),
            boundary,
        });
    }

    fn take_update(&mut self, snapshot: &TerminalSnapshot) -> StreamUpdate {
        self.flush_print();
        let history_changed = self.take_history_checkpoint_change();
        let operations = if self.operation_reliable
            && self.operation_row == snapshot.cursor.row
            && self.operation_col == snapshot.cursor.col
        {
            std::mem::take(&mut self.operations)
        } else {
            self.operations.clear();
            Vec::new()
        };
        StreamUpdate {
            printed_runs: std::mem::take(&mut self.printed_runs),
            output_report_structural: std::mem::take(&mut self.output_report_structural),
            semantic_input_boundary: std::mem::take(&mut self.semantic_input_boundary),
            operations,
            cursor_operations: std::mem::take(&mut self.cursor_operations),
            cursor_operations_after_last_line_feed: std::mem::take(
                &mut self.cursor_operations_after_last_line_feed,
            ),
            scroll_operations: std::mem::take(&mut self.scroll_operations),
            history_changed,
        }
    }
}

#[derive(Default)]
struct StreamUpdate {
    printed_runs: Vec<PrintedRunSnapshot>,
    output_report_structural: bool,
    semantic_input_boundary: bool,
    operations: Vec<OperationSnapshot>,
    cursor_operations: usize,
    cursor_operations_after_last_line_feed: usize,
    scroll_operations: usize,
    history_changed: bool,
}

impl vte::Perform for StreamObserver {
    fn print(&mut self, character: char) {
        self.mark_visible_output();
        self.current_print.push(character);
        self.record_write(character);
    }
    fn execute(&mut self, byte: u8) {
        self.flush_print();
        match byte {
            b'\x08' => {
                self.mark_structural_output();
                self.record_cursor_operation();
                self.operation_col = self.operation_col.saturating_sub(1);
            }
            b'\r' => {
                self.mark_visible_output();
                self.push_boundary(PrintBoundarySnapshot::CarriageReturn);
                self.operation_col = 0;
            }
            b'\n' => {
                self.mark_visible_output();
                self.push_boundary(PrintBoundarySnapshot::LineFeed);
                self.line_feed();
            }
            b'\x0b' | b'\x0c' => {
                self.mark_structural_output();
                self.push_boundary(PrintBoundarySnapshot::LineFeed);
                self.line_feed();
            }
            b'\t' => {
                self.mark_structural_output();
                self.operation_reliable = false;
            }
            // NUL and BEL do not alter printed-text provenance. Other C0
            // controls may select character sets or mutate terminal state in
            // ways this observer does not model conservatively.
            b'\0' | b'\x07' => {}
            _ => {
                self.mark_structural_output();
            }
        }
    }
    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {
        self.flush_print();
        self.mark_structural_output();
    }
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        self.flush_print();
        if let [b"52", location, b"?"] = params {
            self.clipboard_read_queries.push(location.to_vec());
            return;
        }
        if let [kind @ (b"10" | b"11"), b"?"] = params {
            self.default_color_queries
                .push(if *kind == b"10" { 10 } else { 11 });
            return;
        }
        if params.first() == Some(&b"8".as_slice()) {
            let uri = params.get(2..).unwrap_or_default().iter().enumerate().fold(
                Vec::new(),
                |mut uri, (index, part)| {
                    if index > 0 {
                        uri.push(b';');
                    }
                    uri.extend_from_slice(part);
                    uri
                },
            );
            self.active_hyperlink = if uri.is_empty() {
                None
            } else {
                String::from_utf8(uri).ok()
            };
            return;
        }
        let [b"133", marker, rest @ ..] = params else {
            // Known title, working-directory, hyperlink, palette, clipboard,
            // and notification OSCs do not address text cells. Unknown OSCs
            // may carry inline media or proprietary screen mutations.
            if !matches!(
                params.first().copied(),
                Some(
                    b"0" | b"1" | b"2" | b"4" | b"7" | b"8" | b"9" | b"10" | b"11" | b"12" | b"52"
                )
            ) {
                self.mark_structural_output();
            }
            return;
        };
        let kind = match *marker {
            b"A" => SemanticKindSnapshot::PromptStart,
            b"B" => SemanticKindSnapshot::InputStart,
            b"C" => SemanticKindSnapshot::CommandStart,
            b"D" => SemanticKindSnapshot::CommandFinished {
                exit_code: rest
                    .first()
                    .and_then(|value| std::str::from_utf8(value).ok())
                    .and_then(|value| value.parse().ok()),
            },
            _ => {
                self.semantic_input_boundary = false;
                return;
            }
        };
        self.semantic_input_boundary = matches!(kind, SemanticKindSnapshot::InputStart);
        self.events.push(kind);
    }
    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        self.flush_print();
        let output_neutral_private_mode = !ignore
            && intermediates == b"?"
            && matches!(action, 'h' | 'l')
            && Self::private_modes_are_output_neutral(params);
        if !(!ignore && intermediates.is_empty() && matches!(action, 'm' | 'n' | 'c'))
            && !output_neutral_private_mode
        {
            self.mark_structural_output();
        }
        if intermediates == b"?" && matches!(action, 'h' | 'l') {
            let enabled = action == 'h';
            for values in params.iter() {
                for value in values {
                    match *value {
                        6 => self.origin_mode = enabled,
                        7 => self.autowrap = enabled,
                        69 => self.left_right_margin_mode = enabled,
                        47 | 1047 | 1049 => {
                            self.alternate_screen = enabled;
                            self.scroll_region = None;
                            self.operation_reliable = false;
                        }
                        25 => self.cursor_visibility_boundary = Some(enabled),
                        // Input and reporting modes do not alter the location
                        // or meaning of cell operations.
                        2026 => self.synchronized_output_boundary = Some(enabled),
                        1 | 12 | 66 | 67 | 1000 | 1002 | 1003 | 1004 | 1005 | 1006 | 1015
                        | 1016 | 2004 => {}
                        // Unmodeled private modes can save/restore the cursor,
                        // resize/reset the grid, or change how later controls
                        // are interpreted. The dirty-state path remains safe.
                        _ => self.operation_reliable = false,
                    }
                }
            }
            return;
        }
        if intermediates.is_empty() {
            if action == 'J'
                && params
                    .iter()
                    .next()
                    .and_then(|values| values.first())
                    .copied()
                    == Some(3)
            {
                self.history_cleared = true;
                self.history_changed = true;
            }
            match action {
                'A' => {
                    self.record_cursor_operation();
                    self.operation_row = self.operation_row.saturating_sub(Self::count(params));
                }
                'B' => {
                    self.record_cursor_operation();
                    self.operation_row = self
                        .operation_row
                        .saturating_add(Self::count(params))
                        .min(self.operation_rows.saturating_sub(1));
                }
                'C' => {
                    self.record_cursor_operation();
                    self.operation_col = self
                        .operation_col
                        .saturating_add(Self::count(params))
                        .min(self.operation_cols.saturating_sub(1));
                }
                'D' => {
                    self.record_cursor_operation();
                    self.operation_col = self.operation_col.saturating_sub(Self::count(params));
                }
                'E' => {
                    self.record_cursor_operation();
                    self.operation_row = self
                        .operation_row
                        .saturating_add(Self::count(params))
                        .min(self.operation_rows.saturating_sub(1));
                    self.operation_col = 0;
                }
                'F' => {
                    self.record_cursor_operation();
                    self.operation_row = self.operation_row.saturating_sub(Self::count(params));
                    self.operation_col = 0;
                }
                'G' | '`' => {
                    self.record_cursor_operation();
                    self.operation_col = Self::parameter(params, 0, 1)
                        .saturating_sub(1)
                        .min(self.operation_cols.saturating_sub(1));
                }
                'H' | 'f' => {
                    self.record_cursor_operation();
                    let mut row = Self::parameter(params, 0, 1).saturating_sub(1);
                    if self.origin_mode {
                        row = row.saturating_add(self.scroll_bounds().0);
                    }
                    self.operation_row = row.min(self.operation_rows.saturating_sub(1));
                    self.operation_col = Self::parameter(params, 1, 1)
                        .saturating_sub(1)
                        .min(self.operation_cols.saturating_sub(1));
                }
                'd' => {
                    self.record_cursor_operation();
                    self.operation_row = Self::parameter(params, 0, 1)
                        .saturating_sub(1)
                        .min(self.operation_rows.saturating_sub(1));
                }
                'r' if !self.left_right_margin_mode => {
                    let top = Self::parameter(params, 0, 1).saturating_sub(1);
                    let bottom = Self::parameter(params, 1, self.operation_rows)
                        .saturating_sub(1)
                        .min(self.operation_rows.saturating_sub(1));
                    if top < bottom && bottom < self.operation_rows {
                        self.scroll_region = (top != 0
                            || bottom != self.operation_rows.saturating_sub(1))
                        .then_some((top, bottom));
                        self.operation_row = if self.origin_mode { top } else { 0 };
                        self.operation_col = 0;
                    } else {
                        self.operation_reliable = false;
                    }
                }
                'S' | 'T' => {
                    self.scroll_operations += 1;
                    let (top, bottom) = self.scroll_bounds();
                    if action == 'S'
                        && !self.alternate_screen
                        && top == 0
                        && bottom == self.operation_rows.saturating_sub(1)
                    {
                        self.history_changed = true;
                    }
                    let operation = if action == 'S' {
                        OperationSnapshot::ScrollUp {
                            top,
                            bottom,
                            count: Self::count(params),
                        }
                    } else {
                        OperationSnapshot::ScrollDown {
                            top,
                            bottom,
                            count: Self::count(params),
                        }
                    };
                    self.operations.push(operation);
                }
                'L' | 'M' => {
                    let (_, bottom) = self.scroll_bounds();
                    if self.operation_row <= bottom {
                        let operation = if action == 'L' {
                            OperationSnapshot::InsertLines {
                                row: self.operation_row,
                                bottom,
                                count: Self::count(params),
                            }
                        } else {
                            OperationSnapshot::DeleteLines {
                                row: self.operation_row,
                                bottom,
                                count: Self::count(params),
                            }
                        };
                        self.operations.push(operation);
                    }
                }
                '@' => self.operations.push(OperationSnapshot::InsertChars {
                    row: self.operation_row,
                    col: self.operation_col,
                    count: Self::count(params),
                }),
                'P' => self.operations.push(OperationSnapshot::DeleteChars {
                    row: self.operation_row,
                    col: self.operation_col,
                    count: Self::count(params),
                }),
                'X' => {
                    self.record_erase(self.operation_row, self.operation_col, Self::count(params))
                }
                'K' => match Self::parameter(params, 0, 0) {
                    0 => self.record_erase(
                        self.operation_row,
                        self.operation_col,
                        self.operation_cols.saturating_sub(self.operation_col),
                    ),
                    1 => self.record_erase(
                        self.operation_row,
                        0,
                        self.operation_col.saturating_add(1),
                    ),
                    2 => self.record_erase(self.operation_row, 0, self.operation_cols),
                    _ => self.operation_reliable = false,
                },
                'J' => match Self::parameter(params, 0, 0) {
                    0 => {
                        self.record_erase(
                            self.operation_row,
                            self.operation_col,
                            self.operation_cols.saturating_sub(self.operation_col),
                        );
                        for row in self.operation_row.saturating_add(1)..self.operation_rows {
                            self.record_erase(row, 0, self.operation_cols);
                        }
                    }
                    1 => {
                        for row in 0..self.operation_row {
                            self.record_erase(row, 0, self.operation_cols);
                        }
                        self.record_erase(
                            self.operation_row,
                            0,
                            self.operation_col.saturating_add(1),
                        );
                    }
                    2 => {
                        for row in 0..self.operation_rows {
                            self.record_erase(row, 0, self.operation_cols);
                        }
                    }
                    3 => {}
                    _ => self.operation_reliable = false,
                },
                'b' => {
                    if let Some(character) = self.last_printed {
                        for _ in 0..Self::count(params) {
                            self.record_write(character);
                        }
                    } else {
                        self.operation_reliable = false;
                    }
                }
                // SGR and device/query controls do not invalidate cell
                // operation hints; their modeled effects are reconciled later.
                'm' | 'n' | 'c' => {}
                // Save/restore, standard modes, window operations, and any
                // control not explicitly modeled above are ambiguous here.
                _ => self.operation_reliable = false,
            }
        } else {
            // Intermediate-bearing controls include resets and cursor/style
            // operations whose terminal-state effects are not modeled here.
            self.operation_reliable = false;
        }
    }
    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        self.flush_print();
        self.mark_structural_output();
        if !intermediates.is_empty() {
            self.operation_reliable = false;
            return;
        }
        match byte {
            b'D' => self.line_feed(),
            b'E' => {
                self.operation_col = 0;
                self.line_feed();
            }
            b'M' => {
                let (top, bottom) = self.scroll_bounds();
                if self.operation_row == top {
                    self.operations.push(OperationSnapshot::ScrollDown {
                        top,
                        bottom,
                        count: 1,
                    });
                } else {
                    self.operation_row = self.operation_row.saturating_sub(1);
                }
            }
            b'c' => {
                self.scroll_region = None;
                self.origin_mode = false;
                self.left_right_margin_mode = false;
                self.autowrap = true;
                self.operation_reliable = false;
            }
            _ => {}
        }
    }
}

/// A safe, owning terminal boundary over the official libghostty-vt C API.
///
/// The `Rc` marker intentionally makes this type neither `Send` nor `Sync`.
/// Lector keeps each Ghostty terminal on the thread that created it.
pub struct Terminal {
    // Dependents precede their owners so Rust drops them in this order.
    row_cells: RowCellsHandle,
    row_iterator: RowIteratorHandle,
    render_state: RenderStateHandle,
    terminal: TerminalHandle,
    // The terminal is declared first so it is freed before the callback
    // userdata it references. The box keeps the userdata address stable even
    // when this Rust owner moves.
    effect_sink: Box<EffectSink>,
    snapshot: TerminalSnapshot,
    scrollback_capacity: usize,
    stream_parser: vte::Parser,
    stream_observer: StreamObserver,
    /// Monotonic identity of the oldest row in Lector's bounded primary
    /// history window. Never derive this directly from Ghostty's current
    /// physical scrollback extent: whole-page pruning rebases that extent.
    primary_history_origin: usize,
    /// First unassigned row identity. Used only when Ghostty discards the
    /// lineage anchor and the new grid must be placed after the entire old
    /// interval rather than risk aliasing surviving cached coordinates.
    primary_history_high_water: usize,
    primary_history_anchor: Option<HistoryLineageAnchor>,
    /// Primary-screen history changed since the last synchronized-output
    /// opening checkpoint. This spans `advance` calls, allowing View to keep
    /// only cheap visible committed state during ordinary scrolling and to
    /// request the expensive bounded-history copy only at an actual opener.
    history_changed_since_open_checkpoint: bool,
    semantic_marks: Vec<TrackedSemanticMark>,
    /// Decoded image bytes copied out of Ghostty once and then shared through
    /// the presentation pipeline. Each inspection compares Ghostty's borrowed
    /// bytes before reusing an entry, so in-place image mutation remains exact.
    kitty_image_cache: RefCell<BTreeMap<u32, CachedKittyImage>>,
    #[cfg(test)]
    snapshot_row_reads: usize,
    #[cfg(test)]
    history_row_reads: Cell<usize>,
    _thread_bound: PhantomData<Rc<()>>,
}

impl Terminal {
    pub fn new(rows: u16, cols: u16) -> Result<Self, Error> {
        Self::new_with_scrollback(rows, cols, 10_000)
    }

    pub fn new_with_scrollback(
        rows: u16,
        cols: u16,
        scrollback_capacity: usize,
    ) -> Result<Self, Error> {
        Self::new_with_profile(
            rows,
            cols,
            scrollback_capacity,
            TerminalProfile {
                rows,
                columns: cols,
                ..TerminalProfile::default()
            },
        )
    }

    pub fn new_with_profile(
        rows: u16,
        cols: u16,
        scrollback_capacity: usize,
        profile: TerminalProfile,
    ) -> Result<Self, Error> {
        // Keep callback userdata alive until after the terminal even if a
        // later constructor step fails and local variables unwind.
        let mut effect_sink = Box::new(EffectSink {
            terminal_profile: profile,
            ..EffectSink::default()
        });
        let terminal = TerminalHandle::new(rows, cols)?;
        // Ghostty prunes complete pages when its physical line limit is
        // crossed. Keep one logical window of line headroom so a page prune
        // cannot undershoot Lector's requested window; the byte limit is
        // disabled above so it cannot preempt this policy. Lector exposes and
        // anchors only the newest requested rows.
        terminal.set_scrollback_capacity(scrollback_capacity.saturating_mul(2))?;
        terminal.set_option(
            ffi::TERMINAL_OPT_CONTINUATION_MAX_BYTES,
            (&CONTINUATION_MAX_BYTES as *const usize).cast(),
        )?;
        terminal.set_option(
            ffi::TERMINAL_OPT_KITTY_IMAGE_STORAGE_LIMIT,
            (&KITTY_IMAGE_STORAGE_LIMIT_BYTES as *const u64).cast(),
        )?;
        register_effects(&terminal, &mut effect_sink)?;
        let render_state = RenderStateHandle::new()?;
        let row_iterator = RowIteratorHandle::new()?;
        let row_cells = RowCellsHandle::new()?;
        let mut result = Self {
            row_cells,
            row_iterator,
            render_state,
            terminal,
            effect_sink,
            snapshot: TerminalSnapshot::default(),
            scrollback_capacity,
            stream_parser: vte::Parser::new(),
            stream_observer: StreamObserver::default(),
            primary_history_origin: 0,
            primary_history_high_water: 0,
            primary_history_anchor: None,
            history_changed_since_open_checkpoint: false,
            semantic_marks: Vec::new(),
            kitty_image_cache: RefCell::new(BTreeMap::new()),
            #[cfg(test)]
            snapshot_row_reads: 0,
            #[cfg(test)]
            history_row_reads: Cell::new(0),
            _thread_bound: PhantomData,
        };
        result.refresh_snapshot()?;
        Ok(result)
    }

    pub fn advance(&mut self, bytes: &[u8]) -> Result<UpdateSnapshot, Error> {
        let cursor_before = self.snapshot.cursor;
        let alternate_screen_before = self.snapshot.alternate_screen;
        let scrollback_extent_before = self.snapshot.scrollback_extent;
        let history_origin_before = self.snapshot.history_origin;
        self.stream_observer.begin_update(&self.snapshot);
        let mut render_damage = RenderDamageSnapshot::None;
        let mut synchronized_output_open_snapshot = None;
        let mut synchronized_output_close_end = None;
        let mut cursor_visibility_restore_end = None;
        let mut history_extent_at_latest_checkpoint = scrollback_extent_before;
        let mut history_origin_at_latest_checkpoint = history_origin_before;
        let mut history_changed_before_latest_checkpoint = false;
        let new_semantic_start = self.semantic_marks.len();
        let mut segment_start = 0;
        for (index, byte) in bytes.iter().copied().enumerate() {
            self.stream_observer.observe_raw_byte(byte);
            self.stream_parser
                .advance(&mut self.stream_observer, &[byte]);
            if !self.stream_observer.has_boundary_events() {
                continue;
            }
            let synchronized_before = self.snapshot.modes.synchronized_output;
            let cursor_visible_before = self.snapshot.cursor.visible;
            self.write_vt(&bytes[segment_start..=index]);
            self.answer_clipboard_queries();
            self.answer_default_color_queries();
            render_damage.merge(self.refresh_snapshot()?);
            self.anchor_semantic_events(scrollback_extent_before)?;
            let synchronization_boundary = self.stream_observer.take_synchronized_output_boundary();
            let cursor_visibility_boundary = self.stream_observer.take_cursor_visibility_boundary();
            if synchronization_boundary == Some(false)
                && synchronized_before
                && !self.snapshot.modes.synchronized_output
            {
                synchronized_output_close_end = Some(index + 1);
            }
            if synchronization_boundary == Some(true)
                && !synchronized_before
                && self.snapshot.modes.synchronized_output
            {
                self.refresh_semantic_marks()?;
                let segment_history_changed = self.stream_observer.take_history_checkpoint_change()
                    || self.snapshot.scrollback_extent != history_extent_at_latest_checkpoint
                    || self.snapshot.history_origin != history_origin_at_latest_checkpoint;
                history_changed_before_latest_checkpoint |= segment_history_changed;
                // Visible state is captured for every frame. Full history is
                // materially more expensive, so include it only when bytes
                // before this opener actually changed primary-screen history;
                // View retains the previous committed history otherwise.
                let history_changed_before_open =
                    self.history_changed_since_open_checkpoint || segment_history_changed;
                let primary_checkpoint = !self.snapshot.alternate_screen;
                let mut snapshot = if history_changed_before_open && primary_checkpoint {
                    self.snapshot_with_history()?
                } else {
                    self.snapshot.clone()
                };
                snapshot.modes.synchronized_output = false;
                synchronized_output_open_snapshot = Some(snapshot);
                // An alternate-screen opener cannot checkpoint the primary
                // history which changed before the switch. Keep it dirty so
                // the next primary opener receives a coherent full copy.
                if primary_checkpoint {
                    self.history_changed_since_open_checkpoint = false;
                }
                history_extent_at_latest_checkpoint = self.snapshot.scrollback_extent;
                history_origin_at_latest_checkpoint = self.snapshot.history_origin;
            }
            if cursor_visibility_boundary == Some(true)
                && !cursor_visible_before
                && self.snapshot.cursor.visible
            {
                cursor_visibility_restore_end = Some(index + 1);
            }
            segment_start = index + 1;
        }
        if segment_start < bytes.len() {
            self.write_vt(&bytes[segment_start..]);
            self.answer_clipboard_queries();
            self.answer_default_color_queries();
            render_damage.merge(self.refresh_snapshot()?);
        }
        self.recalibrate_new_semantic_marks(new_semantic_start)?;
        self.refresh_semantic_marks()?;
        let mut stream = self.stream_observer.take_update(&self.snapshot);
        let history_changed_after_latest_checkpoint = stream.history_changed
            || self.snapshot.scrollback_extent != history_extent_at_latest_checkpoint
            || self.snapshot.history_origin != history_origin_at_latest_checkpoint;
        self.history_changed_since_open_checkpoint |= history_changed_after_latest_checkpoint;
        stream.history_changed |= history_changed_before_latest_checkpoint;
        let (effects, pty_replies) = self.effect_sink.take()?;
        // Ghostty represents both an unset string and a reported empty string
        // as a zero-length terminal-data value. The callback itself is the
        // authoritative evidence that the application explicitly cleared the
        // value, so retain that distinction in the owned snapshot.
        if let Some(title) = effects.iter().rev().find_map(|effect| match effect {
            EffectSnapshot::TitleChanged(title) => Some(title),
            _ => None,
        }) {
            self.snapshot.title = Some(title.clone());
        }
        if let Some(path) = effects.iter().rev().find_map(|effect| match effect {
            EffectSnapshot::WorkingDirectoryChanged(path) => Some(path),
            _ => None,
        }) {
            self.snapshot.working_directory = Some(path.clone());
        }
        let changed_rows = render_damage.changed_rows(self.snapshot.rows.len());
        let parser_continuation = terminal_has_continuation(&self.terminal).unwrap_or(true);
        Ok(UpdateSnapshot {
            effects,
            pty_replies,
            printed_runs: stream.printed_runs,
            output_report_structural: stream.output_report_structural,
            parser_continuation,
            operations: stream.operations,
            cursor_operations: stream.cursor_operations,
            cursor_operations_after_last_line_feed: stream.cursor_operations_after_last_line_feed,
            scroll_operations: stream.scroll_operations,
            history_changed: stream.history_changed,
            damage: render_damage,
            changed_rows,
            cursor_before,
            cursor_after: self.snapshot.cursor,
            alternate_screen_before,
            alternate_screen_after: self.snapshot.alternate_screen,
            synchronized_output: self.snapshot.modes.synchronized_output,
            synchronized_output_closed: !self.snapshot.modes.synchronized_output
                && synchronized_output_close_end == Some(bytes.len()),
            semantic_input_boundary: stream.semantic_input_boundary && !parser_continuation,
            cursor_visibility_restored: self.snapshot.cursor.visible
                && cursor_visibility_restore_end == Some(bytes.len()),
            synchronized_output_open_snapshot,
        })
    }

    fn answer_clipboard_queries(&mut self) {
        for location in self.stream_observer.take_clipboard_read_queries() {
            self.effect_sink
                .events
                .push(EffectSnapshot::Query(QuerySnapshot::Clipboard));
            if location.len() > 16 || !location.iter().all(|byte| byte.is_ascii_alphanumeric()) {
                continue;
            }
            // Clipboard reads never escape to the outer terminal. The secure
            // virtual profile answers with empty content; a future local
            // clipboard provider can fill this without changing routing.
            let _advertised = self.effect_sink.terminal_profile.clipboard_read;
            self.effect_sink.pty_replies.extend_from_slice(b"\x1b]52;");
            self.effect_sink.pty_replies.extend_from_slice(&location);
            self.effect_sink.pty_replies.extend_from_slice(b";\x1b\\");
        }
    }

    fn answer_default_color_queries(&mut self) {
        for query in self.stream_observer.take_default_color_queries() {
            // libghostty-vt exposes the light/dark preference but no exact
            // default-color callback. Give applications a deterministic
            // virtual default consistent with that preference. tmux control
            // clients can then return this raw OSC report with
            // `refresh-client -r` without routing it through pane input.
            let light =
                self.effect_sink.terminal_profile.color_scheme == TerminalColorScheme::Light;
            let white = b"rgb:ffff/ffff/ffff";
            let black = b"rgb:0000/0000/0000";
            let color: &[u8] = match (query, light) {
                (10, true) | (11, false) => black,
                (10, false) | (11, true) => white,
                _ => continue,
            };
            self.effect_sink.pty_replies.extend_from_slice(b"\x1b]");
            self.effect_sink
                .pty_replies
                .extend_from_slice(query.to_string().as_bytes());
            self.effect_sink.pty_replies.push(b';');
            self.effect_sink.pty_replies.extend_from_slice(color);
            self.effect_sink.pty_replies.extend_from_slice(b"\x1b\\");
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<(), Error> {
        self.resize_with_geometry(rows, cols, 0, 0)
    }

    pub fn resize_with_geometry(
        &mut self,
        rows: u16,
        cols: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> Result<(), Error> {
        if rows == 0 || cols == 0 {
            return Err(Error::InvalidValue);
        }
        let cell_layout_changed = self.effect_sink.terminal_profile.rows != rows
            || self.effect_sink.terminal_profile.columns != cols;
        self.effect_sink.terminal_profile.rows = rows;
        self.effect_sink.terminal_profile.columns = cols;
        self.effect_sink.terminal_profile.cell_width = cell_width_px;
        self.effect_sink.terminal_profile.cell_height = cell_height_px;
        // Reflow changes row boundaries non-uniformly, so a single numeric
        // origin shift cannot preserve arbitrary old logical positions. Start
        // a disjoint lineage; Ghostty's separate tracked marks still follow
        // individual cells through reflow where possible.
        if cell_layout_changed {
            self.primary_history_origin = self.primary_history_high_water.max(
                self.primary_history_origin
                    .saturating_add(self.snapshot.scrollback_extent)
                    .saturating_add(self.snapshot.rows.len()),
            );
            self.primary_history_anchor = None;
        }
        // Reflow may alter retained history even when its logical extent stays
        // at the configured cap. The next opener must receive a full history
        // checkpoint rather than reuse View's pre-resize allocation.
        self.history_changed_since_open_checkpoint = true;
        // SAFETY: the terminal handle is valid and dimensions were validated.
        let result = unsafe {
            ffi::ghostty_terminal_resize(
                self.terminal.as_ptr(),
                cols,
                rows,
                cell_width_px,
                cell_height_px,
            )
        };
        result_from_code(result)?;
        self.refresh_snapshot()?;
        self.refresh_semantic_marks()
    }

    /// Encode Ghostty's unstable snapshot format for diagnostics and future
    /// persistence experiments. Live Lector behavior never depends on this.
    pub fn diagnostic_snapshot(&self) -> Result<DiagnosticSnapshot, Error> {
        Ok(DiagnosticSnapshot {
            bytes: snapshot_bytes(&self.terminal)?,
            observer_continuation: continuation_bytes(&self.terminal)?,
            active_hyperlink: self.stream_observer.active_hyperlink.clone(),
            scrollback_capacity: self.scrollback_capacity,
            terminal_profile: self.effect_sink.terminal_profile.clone(),
            primary_history_origin: self.primary_history_origin,
            primary_history_high_water: self.primary_history_high_water,
        })
    }

    /// Restore an unstable diagnostic snapshot produced by this exact adapter
    /// and pinned Ghostty build.
    pub fn restore_diagnostic_snapshot(snapshot: DiagnosticSnapshot) -> Result<Self, Error> {
        let DiagnosticSnapshot {
            bytes,
            observer_continuation,
            active_hyperlink,
            scrollback_capacity,
            terminal_profile,
            primary_history_origin,
            primary_history_high_water,
        } = snapshot;
        // Declare callback storage before the decoded terminal so partial
        // constructor unwinding always frees the terminal first.
        let mut effect_sink = Box::new(EffectSink {
            terminal_profile,
            ..EffectSink::default()
        });
        let decoder = SnapshotDecoderHandle::new(&bytes)?;
        let terminal = decoder.decode()?;
        drop(decoder);

        terminal.set_option(
            ffi::TERMINAL_OPT_CONTINUATION_MAX_BYTES,
            (&CONTINUATION_MAX_BYTES as *const usize).cast(),
        )?;
        terminal.set_option(
            ffi::TERMINAL_OPT_KITTY_IMAGE_STORAGE_LIMIT,
            (&KITTY_IMAGE_STORAGE_LIMIT_BYTES as *const u64).cast(),
        )?;
        register_effects(&terminal, &mut effect_sink)?;
        let render_state = RenderStateHandle::new()?;
        let row_iterator = RowIteratorHandle::new()?;
        let row_cells = RowCellsHandle::new()?;
        let mut stream_parser = vte::Parser::new();
        let mut stream_observer = StreamObserver::default();
        stream_parser.advance(&mut stream_observer, &observer_continuation);
        stream_observer.begin_update(&TerminalSnapshot::default());
        let _ = stream_observer.take_update(&TerminalSnapshot::default());
        stream_observer.active_hyperlink = active_hyperlink;
        let mut result = Self {
            row_cells,
            row_iterator,
            render_state,
            terminal,
            effect_sink,
            snapshot: TerminalSnapshot::default(),
            scrollback_capacity,
            stream_parser,
            stream_observer,
            primary_history_origin,
            primary_history_high_water,
            primary_history_anchor: None,
            // The restored terminal can already contain retained history
            // which no external View cache has observed.
            history_changed_since_open_checkpoint: true,
            // OSC 133 grid references are deliberately not part of this
            // diagnostic path yet. Runtime correctness never restores here.
            semantic_marks: Vec::new(),
            kitty_image_cache: RefCell::new(BTreeMap::new()),
            #[cfg(test)]
            snapshot_row_reads: 0,
            #[cfg(test)]
            history_row_reads: Cell::new(0),
            _thread_bound: PhantomData,
        };
        result.refresh_snapshot()?;
        Ok(result)
    }

    pub fn reset(&mut self) -> Result<(), Error> {
        // The reset grid must occupy a disjoint lineage interval so an old
        // cached position cannot alias a new cell with the same local row.
        self.primary_history_origin = self.primary_history_high_water.max(
            self.primary_history_origin
                .saturating_add(self.snapshot.scrollback_extent)
                .saturating_add(self.snapshot.rows.len()),
        );
        self.primary_history_anchor = None;
        // SAFETY: the terminal handle is valid.
        unsafe { ffi::ghostty_terminal_reset(self.terminal.as_ptr()) };
        self.stream_parser = vte::Parser::new();
        self.stream_observer = StreamObserver::default();
        self.history_changed_since_open_checkpoint = true;
        let _ = self.effect_sink.take()?;
        self.semantic_marks.clear();
        self.refresh_snapshot().map(|_| ())
    }

    pub fn snapshot(&self) -> &TerminalSnapshot {
        &self.snapshot
    }

    /// The current SGR pen applied to the next printed grapheme.
    pub fn cursor_style(&self) -> Result<StyleSnapshot, Error> {
        let mut style = ffi::Style::init();
        // SAFETY: the terminal is valid and the sized GhosttyStyle output is
        // initialized according to the official C API contract.
        result_from_code(unsafe {
            ffi::ghostty_terminal_get(
                self.terminal.as_ptr(),
                ffi::TERMINAL_DATA_CURSOR_STYLE,
                (&mut style as *mut ffi::Style).cast(),
            )
        })?;
        normalize_style(style)
    }

    /// The currently active OSC 8 URI applied to subsequently printed cells.
    pub fn active_hyperlink(&self) -> Option<&str> {
        self.stream_observer.active_hyperlink.as_deref()
    }

    /// Copies all Kitty image placements and referenced decoded pixel data
    /// from the active screen. Every borrowed C handle is consumed before the
    /// method returns, so callers may retain the result across mutations.
    pub fn kitty_image_placements(&self) -> Result<Vec<KittyImagePlacementSnapshot>, Error> {
        let graphics = terminal_query::<ffi::KittyGraphics>(
            &self.terminal,
            ffi::TERMINAL_DATA_KITTY_GRAPHICS,
        )?;
        if graphics.is_null() {
            return Err(Error::NullHandle);
        }
        let mut iterator = KittyPlacementIteratorHandle::new()?;
        iterator.populate(graphics)?;
        let mut placements = Vec::new();
        let retained_images = self.kitty_image_cache.borrow();
        let mut current_images = BTreeMap::<u32, CachedKittyImage>::new();
        // SAFETY: the iterator is valid and the terminal remains immutable for
        // this complete traversal.
        while unsafe { ffi::ghostty_kitty_graphics_placement_next(iterator.as_ptr()) } {
            let image_id =
                kitty_placement_query::<u32>(&iterator, ffi::KITTY_PLACEMENT_DATA_IMAGE_ID)?;
            // SAFETY: the graphics handle remains valid while the terminal is
            // immutable. A null result means the placement is malformed.
            let image = unsafe { ffi::ghostty_kitty_graphics_image(graphics, image_id) };
            if image.is_null() {
                return Err(Error::NullHandle);
            }
            let image_data = if let Some(current) = current_images.get(&image_id) {
                current.clone()
            } else {
                let data_len = kitty_image_query::<usize>(image, ffi::KITTY_IMAGE_DATA_DATA_LEN)?;
                let data_ptr =
                    kitty_image_query::<*const u8>(image, ffi::KITTY_IMAGE_DATA_DATA_PTR)?;
                if data_len > 0 && data_ptr.is_null() {
                    return Err(Error::NullString);
                }
                // SAFETY: Ghostty documents exactly `data_len` decoded bytes
                // borrowed until the terminal mutates. The comparison or Arc
                // copy completes synchronously inside that lifetime.
                let borrowed = if data_len == 0 {
                    &[]
                } else {
                    unsafe { std::slice::from_raw_parts(data_ptr, data_len) }
                };
                let current = retained_images
                    .get(&image_id)
                    .filter(|cached| cached.data.as_ref() == borrowed)
                    .cloned()
                    .unwrap_or_else(|| CachedKittyImage {
                        data: Arc::from(borrowed),
                        data_digest: stable_digest(borrowed),
                    });
                current_images.insert(image_id, current.clone());
                current
            };
            let format = match kitty_image_query::<ffi::KittyImageFormat>(
                image,
                ffi::KITTY_IMAGE_DATA_FORMAT,
            )? {
                ffi::KITTY_IMAGE_FORMAT_RGB => KittyImageFormatSnapshot::Rgb,
                ffi::KITTY_IMAGE_FORMAT_RGBA => KittyImageFormatSnapshot::Rgba,
                ffi::KITTY_IMAGE_FORMAT_GRAY_ALPHA => KittyImageFormatSnapshot::GrayAlpha,
                ffi::KITTY_IMAGE_FORMAT_GRAY => KittyImageFormatSnapshot::Gray,
                _ => return Err(Error::InvalidValue),
            };
            let mut render = ffi::KittyGraphicsPlacementRenderInfo::init();
            // SAFETY: all handles are valid during this immutable traversal and
            // the sized output struct follows the official header layout.
            result_from_code(unsafe {
                ffi::ghostty_kitty_graphics_placement_render_info(
                    iterator.as_ptr(),
                    image,
                    self.terminal.as_ptr(),
                    &mut render,
                )
            })?;
            placements.push(KittyImagePlacementSnapshot {
                image_id,
                placement_id: kitty_placement_query(
                    &iterator,
                    ffi::KITTY_PLACEMENT_DATA_PLACEMENT_ID,
                )?,
                image_number: kitty_image_query(image, ffi::KITTY_IMAGE_DATA_NUMBER)?,
                pixel_width: kitty_image_query(image, ffi::KITTY_IMAGE_DATA_WIDTH)?,
                pixel_height: kitty_image_query(image, ffi::KITTY_IMAGE_DATA_HEIGHT)?,
                rendered_pixel_width: render.pixel_width,
                rendered_pixel_height: render.pixel_height,
                format,
                data: Arc::clone(&image_data.data),
                data_digest: image_data.data_digest,
                x_offset: kitty_placement_query(&iterator, ffi::KITTY_PLACEMENT_DATA_X_OFFSET)?,
                y_offset: kitty_placement_query(&iterator, ffi::KITTY_PLACEMENT_DATA_Y_OFFSET)?,
                viewport_col: render.viewport_col,
                viewport_row: render.viewport_row,
                grid_cols: render.grid_cols,
                grid_rows: render.grid_rows,
                source_x: render.source_x,
                source_y: render.source_y,
                source_width: render.source_width,
                source_height: render.source_height,
                z_index: kitty_placement_query(&iterator, ffi::KITTY_PLACEMENT_DATA_Z)?,
                virtual_placement: kitty_placement_query(
                    &iterator,
                    ffi::KITTY_PLACEMENT_DATA_IS_VIRTUAL,
                )?,
                visible: render.viewport_visible,
            });
        }
        placements.sort_by_key(|placement| {
            (
                placement.z_index,
                placement.image_id,
                placement.placement_id,
            )
        });
        drop(retained_images);
        *self.kitty_image_cache.borrow_mut() = current_images;
        Ok(placements)
    }

    fn write_vt(&mut self, bytes: &[u8]) {
        // SAFETY: the terminal handle is valid and the borrowed byte slice
        // remains valid for the duration of this synchronous call.
        unsafe {
            ffi::ghostty_terminal_vt_write(self.terminal.as_ptr(), bytes.as_ptr(), bytes.len())
        };
    }

    /// Captures the newest logically retained history rows. Ghostty allocates
    /// scrollback in pages and may physically retain more than its configured
    /// line limit; this boundary enforces Lector's exact logical cap.
    pub fn snapshot_with_history(&self) -> Result<TerminalSnapshot, Error> {
        let mut snapshot = self.snapshot.clone();
        let actual_extent = self.actual_scrollback_extent()?;
        let logical_extent = actual_extent.min(self.scrollback_capacity);
        let origin = actual_extent.saturating_sub(logical_extent);
        let (_, cols) = snapshot.size();
        let mut history = Vec::with_capacity(logical_extent);
        for row in origin..actual_extent {
            history.push(self.read_grid_row(row, cols)?);
        }
        snapshot.scrollback = history;
        snapshot.scrollback_extent = logical_extent;
        Ok(snapshot)
    }

    /// Reads the suffix of the current logical history window beginning at a
    /// window-relative row. Presentation receipts use this to carry only rows
    /// added since their previous history generation instead of materializing
    /// the complete bounded scrollback after every line feed.
    pub fn history_rows_from(&self, logical_start: usize) -> Result<Vec<RowSnapshot>, Error> {
        let actual_extent = self.actual_scrollback_extent()?;
        let logical_extent = actual_extent.min(self.scrollback_capacity);
        if logical_start > logical_extent {
            return Err(Error::InvalidValue);
        }
        let physical_start = actual_extent
            .saturating_sub(logical_extent)
            .saturating_add(logical_start);
        let (_, cols) = self.snapshot.size();
        let mut history = Vec::with_capacity(logical_extent.saturating_sub(logical_start));
        for row in physical_start..actual_extent {
            history.push(self.read_grid_row(row, cols)?);
        }
        Ok(history)
    }

    pub fn scrollback_extent(&self) -> usize {
        self.snapshot.scrollback_extent
    }

    /// The current page-relative Ghostty rows older than Lector's logical
    /// window. This may move backwards after whole-page pruning and is used
    /// only to translate Ghostty tracked references, never as snapshot
    /// lineage identity.
    pub fn physical_history_origin(&self) -> Result<usize, Error> {
        Ok(self
            .actual_scrollback_extent()?
            .saturating_sub(self.scrollback_capacity))
    }

    /// Tracks a point in Ghostty's physical full-screen coordinate space.
    /// Callers should add `physical_history_origin()` to a Lector-local row
    /// first; the monotonic snapshot lineage is a different coordinate space.
    pub fn track_screen_position(&self, row: usize, col: u16) -> Result<TrackedGridRef, Error> {
        let total_rows = terminal_query::<usize>(&self.terminal, ffi::TERMINAL_DATA_TOTAL_ROWS)?;
        let (_, cols) = self.snapshot.size();
        if row >= total_rows || col >= cols {
            return Err(Error::InvalidValue);
        }
        self.track_position(ffi::POINT_TAG_SCREEN, row, col)
    }

    fn track_position(
        &self,
        tag: ffi::PointTag,
        row: usize,
        col: u16,
    ) -> Result<TrackedGridRef, Error> {
        let row = u32::try_from(row).map_err(|_| Error::LimitExceeded)?;
        let point = ffi::Point::coordinate(tag, col, row);
        let mut handle = std::ptr::null_mut();
        // SAFETY: the terminal and output pointers are valid and callers
        // bounds-check the point in the selected coordinate system.
        result_from_code(unsafe {
            ffi::ghostty_terminal_grid_ref_track(self.terminal.as_ptr(), point, &mut handle)
        })?;
        Ok(TrackedGridRef {
            handle: NonNull::new(handle).ok_or(Error::NullHandle)?,
            _thread_bound: PhantomData,
        })
    }

    fn anchor_semantic_events(&mut self, previous_scrollback_extent: usize) -> Result<(), Error> {
        let events = self.stream_observer.take_semantic_events();
        if events.is_empty() {
            return Ok(());
        }
        let alternate_screen = self.snapshot.alternate_screen;
        let active_row = usize::from(self.snapshot.cursor.row);
        // Ghostty's screen coordinate space omits unwritten active rows. Just
        // after scrolling at the logical history cap, an OSC marker can
        // therefore observe a shorter extent until text materializes the
        // blank cursor row. Preserve the already-full logical window while
        // Ghostty still reports history; a real history clear reports zero.
        let logical_scrollback_extent = if previous_scrollback_extent == self.scrollback_capacity
            && !self.stream_observer.history_cleared
        {
            self.scrollback_capacity
        } else {
            self.snapshot.scrollback_extent
        };
        let logical_row = logical_scrollback_extent.saturating_add(active_row);
        let col = self.snapshot.cursor.col;
        for &kind in &events {
            let actual_history = self.actual_scrollback_extent()?;
            let physical_anchor_row = actual_history.saturating_add(active_row);
            let reference = self.track_screen_position(physical_anchor_row, col)?;
            let (physical_row, _) = reference.screen_position()?.ok_or(Error::NoValue)?;
            let origin = actual_history.saturating_sub(self.scrollback_capacity);
            let observed_logical_row = physical_row.saturating_sub(origin);
            self.semantic_marks.push(TrackedSemanticMark {
                kind,
                reference,
                alternate_screen,
                last_position: (logical_row, col),
                row_offset: signed_offset(logical_row, observed_logical_row)?,
            });
        }
        Ok(())
    }

    fn refresh_semantic_marks(&mut self) -> Result<(), Error> {
        let current_alternate = self.snapshot.alternate_screen;
        let actual_history = self.actual_scrollback_extent()?;
        let origin = actual_history.saturating_sub(self.scrollback_capacity);
        let logical_rows = self
            .snapshot
            .scrollback_extent
            .saturating_add(self.snapshot.rows.len());
        let mut retained = Vec::with_capacity(self.semantic_marks.len());
        let mut normalized = Vec::with_capacity(self.semantic_marks.len());
        for mut mark in self.semantic_marks.drain(..) {
            let Some((physical_row, col)) = mark.reference.screen_position()? else {
                continue;
            };
            if mark.alternate_screen != current_alternate {
                normalized.push(SemanticMarkSnapshot {
                    kind: mark.kind,
                    row: mark.last_position.0,
                    col: mark.last_position.1,
                    alternate_screen: mark.alternate_screen,
                });
                retained.push(mark);
                continue;
            }
            if physical_row < origin {
                continue;
            }
            let Some(row) = apply_signed_offset(physical_row - origin, mark.row_offset) else {
                continue;
            };
            if row >= logical_rows {
                continue;
            }
            normalized.push(SemanticMarkSnapshot {
                kind: mark.kind,
                row,
                col,
                alternate_screen: mark.alternate_screen,
            });
            mark.last_position = (row, col);
            retained.push(mark);
        }
        self.semantic_marks = retained;
        self.snapshot.semantic_marks = normalized;
        Ok(())
    }

    fn recalibrate_new_semantic_marks(&mut self, start: usize) -> Result<(), Error> {
        if start >= self.semantic_marks.len() {
            return Ok(());
        }
        let actual_history = self.actual_scrollback_extent()?;
        let origin = actual_history.saturating_sub(self.scrollback_capacity);
        for mark in &mut self.semantic_marks[start..] {
            let Some((physical_row, _)) = mark.reference.screen_position()? else {
                continue;
            };
            if physical_row < origin {
                continue;
            }
            mark.row_offset = signed_offset(mark.last_position.0, physical_row - origin)?;
        }
        Ok(())
    }

    fn refresh_snapshot(&mut self) -> Result<RenderDamageSnapshot, Error> {
        // SAFETY: both owned handles are valid and are only accessed from
        // this thread while `&mut self` excludes concurrent mutation.
        let result = unsafe {
            ffi::ghostty_render_state_update(self.render_state.as_ptr(), self.terminal.as_ptr())
        };
        result_from_code(result)?;

        let global_dirty = render_query::<ffi::RenderStateDirty>(
            &self.render_state,
            ffi::RENDER_STATE_DATA_DIRTY,
        )?;
        let rows = render_query::<u16>(&self.render_state, ffi::RENDER_STATE_DATA_ROWS)?;
        let cols = render_query::<u16>(&self.render_state, ffi::RENDER_STATE_DATA_COLS)?;
        let mut iterator = self.row_iterator.as_ptr();
        render_query_into(
            &self.render_state,
            ffi::RENDER_STATE_DATA_ROW_ITERATOR,
            &mut iterator,
        )?;
        if iterator != self.row_iterator.as_ptr() {
            return Err(Error::InvalidValue);
        }

        let mut can_reuse_rows = global_dirty != ffi::RENDER_STATE_DIRTY_FULL
            && self.snapshot.rows.len() == usize::from(rows)
            && self
                .snapshot
                .rows
                .iter()
                .all(|row| row.cells.len() == usize::from(cols));
        let mut normalized_rows = Vec::with_capacity(usize::from(rows));
        let mut replacement_rows = Vec::new();
        let mut dirty_ranges = Vec::new();
        for row in 0..rows {
            // SAFETY: the iterator was populated from the current render
            // state and remains valid until its next render-state update.
            if !unsafe { ffi::ghostty_render_state_row_iterator_next(self.row_iterator.as_ptr()) } {
                return Err(Error::NoValue);
            }
            let dirty =
                row_iterator_query::<bool>(&self.row_iterator, ffi::RENDER_STATE_ROW_DATA_DIRTY)?;
            if dirty {
                append_dirty_row(&mut dirty_ranges, row);
            }
            if can_reuse_rows {
                if dirty {
                    replacement_rows.push((usize::from(row), self.read_row(row, cols)?));
                }
            } else {
                normalized_rows.push(self.read_row(row, cols)?);
            }
            let clean = false;
            // SAFETY: the iterator is positioned on the current live row and
            // the option's documented value type is `bool`.
            result_from_code(unsafe {
                ffi::ghostty_render_state_row_set(
                    self.row_iterator.as_ptr(),
                    ffi::RENDER_STATE_ROW_OPTION_DIRTY,
                    (&clean as *const bool).cast(),
                )
            })?;
        }
        // Detect an ABI or iteration contract change rather than silently
        // truncating a viewport.
        // SAFETY: same iterator validity as above.
        if unsafe { ffi::ghostty_render_state_row_iterator_next(self.row_iterator.as_ptr()) } {
            return Err(Error::InvalidValue);
        }
        if global_dirty == ffi::RENDER_STATE_DIRTY_PARTIAL && dirty_ranges.is_empty() {
            // A partial frame without row flags violates Ghostty's two-layer
            // contract. Re-read the full viewport so the safe full-damage
            // interpretation below also has an authoritative snapshot.
            let mut iterator = self.row_iterator.as_ptr();
            render_query_into(
                &self.render_state,
                ffi::RENDER_STATE_DATA_ROW_ITERATOR,
                &mut iterator,
            )?;
            if iterator != self.row_iterator.as_ptr() {
                return Err(Error::InvalidValue);
            }
            can_reuse_rows = false;
            normalized_rows.clear();
            for row in 0..rows {
                // SAFETY: the iterator was reset from the current render
                // state and remains valid until its next state update.
                if !unsafe {
                    ffi::ghostty_render_state_row_iterator_next(self.row_iterator.as_ptr())
                } {
                    return Err(Error::NoValue);
                }
                normalized_rows.push(self.read_row(row, cols)?);
            }
            // SAFETY: same iterator validity as above.
            if unsafe { ffi::ghostty_render_state_row_iterator_next(self.row_iterator.as_ptr()) } {
                return Err(Error::InvalidValue);
            }
        }
        if can_reuse_rows {
            normalized_rows = Arc::unwrap_or_clone(std::mem::take(&mut self.snapshot.rows));
            for (row, replacement) in replacement_rows {
                normalized_rows[row] = replacement;
            }
        }
        let clean = ffi::RENDER_STATE_DIRTY_FALSE;
        // SAFETY: the render state is live and the option's documented value
        // type is `GhosttyRenderStateDirty` (a header-checked C enum).
        result_from_code(unsafe {
            ffi::ghostty_render_state_set(
                self.render_state.as_ptr(),
                ffi::RENDER_STATE_OPTION_DIRTY,
                (&clean as *const ffi::RenderStateDirty).cast(),
            )
        })?;

        let screen = terminal_query::<ffi::TerminalScreen>(
            &self.terminal,
            ffi::TERMINAL_DATA_ACTIVE_SCREEN,
        )?;
        let alternate_screen = match screen {
            ffi::TERMINAL_SCREEN_PRIMARY => false,
            ffi::TERMINAL_SCREEN_ALTERNATE => true,
            _ => return Err(Error::InvalidValue),
        };
        let title = terminal_string(&self.terminal, ffi::TERMINAL_DATA_TITLE)?;
        let working_directory = terminal_string(&self.terminal, ffi::TERMINAL_DATA_PWD)?;
        let actual_scrollback_extent = self.actual_scrollback_extent()?;
        let scrollback_extent = actual_scrollback_extent.min(self.scrollback_capacity);
        if !alternate_screen {
            self.refresh_primary_history_lineage(
                actual_scrollback_extent,
                scrollback_extent,
                usize::from(rows),
            )?;
        }
        self.snapshot = TerminalSnapshot {
            rows: Arc::new(normalized_rows),
            scrollback: Vec::new(),
            cursor: CursorSnapshot {
                row: terminal_query(&self.terminal, ffi::TERMINAL_DATA_CURSOR_Y)?,
                col: terminal_query(&self.terminal, ffi::TERMINAL_DATA_CURSOR_X)?,
                visible: terminal_query(&self.terminal, ffi::TERMINAL_DATA_CURSOR_VISIBLE)?,
                shape: match render_query::<ffi::RenderCursorVisualStyle>(
                    &self.render_state,
                    ffi::RENDER_STATE_DATA_CURSOR_VISUAL_STYLE,
                )? {
                    ffi::RENDER_CURSOR_VISUAL_STYLE_BAR => CursorShapeSnapshot::Bar,
                    ffi::RENDER_CURSOR_VISUAL_STYLE_BLOCK => CursorShapeSnapshot::Block,
                    ffi::RENDER_CURSOR_VISUAL_STYLE_UNDERLINE => CursorShapeSnapshot::Underline,
                    ffi::RENDER_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW => {
                        CursorShapeSnapshot::BlockHollow
                    }
                    _ => return Err(Error::InvalidValue),
                },
            },
            width_px: terminal_query(&self.terminal, ffi::TERMINAL_DATA_WIDTH_PX)?,
            height_px: terminal_query(&self.terminal, ffi::TERMINAL_DATA_HEIGHT_PX)?,
            alternate_screen,
            modes: read_modes(&self.terminal)?,
            title,
            working_directory,
            history_origin: self.primary_history_origin,
            scrollback_extent,
            semantic_marks: Vec::new(),
        };
        match global_dirty {
            ffi::RENDER_STATE_DIRTY_FULL => Ok(RenderDamageSnapshot::Full),
            ffi::RENDER_STATE_DIRTY_PARTIAL if dirty_ranges.is_empty() => {
                // A partial frame without row flags violates Ghostty's
                // two-layer contract. Full damage is the safe interpretation.
                Ok(RenderDamageSnapshot::Full)
            }
            ffi::RENDER_STATE_DIRTY_PARTIAL | ffi::RENDER_STATE_DIRTY_FALSE
                if !dirty_ranges.is_empty() =>
            {
                Ok(RenderDamageSnapshot::Rows(dirty_ranges))
            }
            ffi::RENDER_STATE_DIRTY_FALSE => Ok(RenderDamageSnapshot::None),
            _ => Err(Error::InvalidValue),
        }
    }

    fn read_row(&mut self, row: u16, cols: u16) -> Result<RowSnapshot, Error> {
        #[cfg(test)]
        {
            self.snapshot_row_reads = self.snapshot_row_reads.saturating_add(1);
        }
        let raw_row =
            row_iterator_query::<ffi::Row>(&self.row_iterator, ffi::RENDER_STATE_ROW_DATA_RAW)?;
        let wrapped = row_query::<bool>(raw_row, ffi::ROW_DATA_WRAP)?;
        let mut cells = self.row_cells.as_ptr();
        row_iterator_query_into(
            &self.row_iterator,
            ffi::RENDER_STATE_ROW_DATA_CELLS,
            &mut cells,
        )?;
        if cells != self.row_cells.as_ptr() {
            return Err(Error::InvalidValue);
        }

        let mut normalized_cells = Vec::with_capacity(usize::from(cols));
        for col in 0..cols {
            // SAFETY: the row-cells iterator was populated for the current
            // render row and has not been invalidated.
            if !unsafe { ffi::ghostty_render_state_row_cells_next(self.row_cells.as_ptr()) } {
                return Err(Error::NoValue);
            }
            normalized_cells.push(self.read_cell(row, col)?);
        }
        // SAFETY: same row-cells iterator validity as above.
        if unsafe { ffi::ghostty_render_state_row_cells_next(self.row_cells.as_ptr()) } {
            return Err(Error::InvalidValue);
        }
        Ok(RowSnapshot {
            cells: Arc::new(normalized_cells),
            wrapped,
        })
    }

    fn read_cell(&self, row: u16, col: u16) -> Result<CellSnapshot, Error> {
        let raw_cell =
            row_cells_query::<ffi::Cell>(&self.row_cells, ffi::RENDER_STATE_ROW_CELLS_DATA_RAW)?;
        let wide = cell_query::<ffi::CellWide>(raw_cell, ffi::CELL_DATA_WIDE)?;
        let (width, continuation) = match wide {
            ffi::CELL_WIDE_NARROW => (1, false),
            ffi::CELL_WIDE_WIDE => (2, false),
            ffi::CELL_WIDE_SPACER_TAIL => (0, true),
            ffi::CELL_WIDE_SPACER_HEAD => (0, true),
            _ => return Err(Error::InvalidValue),
        };
        Ok(CellSnapshot {
            grapheme: row_cell_grapheme(&self.row_cells)?,
            width,
            continuation,
            style: normalize_style(row_cell_style(&self.row_cells)?)?,
            hyperlink: if cell_query(raw_cell, ffi::CELL_DATA_HAS_HYPERLINK)? {
                let reference = terminal_grid_ref(
                    &self.terminal,
                    ffi::POINT_TAG_VIEWPORT,
                    col,
                    u32::from(row),
                )?;
                grid_ref_hyperlink(&reference)?
            } else {
                None
            },
        })
    }

    fn actual_scrollback_extent(&self) -> Result<usize, Error> {
        terminal_query(&self.terminal, ffi::TERMINAL_DATA_SCROLLBACK_ROWS)
    }

    fn refresh_primary_history_lineage(
        &mut self,
        actual_history: usize,
        logical_history: usize,
        visible_rows: usize,
    ) -> Result<(), Error> {
        let physical_window_origin = actual_history.saturating_sub(logical_history);
        if let Some(anchor) = self.primary_history_anchor.take() {
            let candidate = anchor
                .reference
                .screen_position()?
                .and_then(|(physical_row, _)| {
                    if physical_row >= physical_window_origin {
                        anchor
                            .absolute_row
                            .checked_sub(physical_row - physical_window_origin)
                    } else {
                        anchor
                            .absolute_row
                            .checked_add(physical_window_origin - physical_row)
                    }
                });
            if let Some(candidate) =
                candidate.filter(|candidate| *candidate >= self.primary_history_origin)
            {
                self.primary_history_origin = candidate;
            } else {
                // A missing or non-monotonic boundary cannot map the old
                // model exactly (notably after reset/reflow). Start a disjoint
                // monotonic interval; consumers can safely clamp or discard
                // every position from the previous interval.
                self.primary_history_origin = self
                    .primary_history_origin
                    .max(self.primary_history_high_water);
            }
        }

        self.primary_history_high_water = self.primary_history_high_water.max(
            self.primary_history_origin
                .saturating_add(logical_history)
                .saturating_add(visible_rows),
        );
        let reference = self.track_position(ffi::POINT_TAG_VIEWPORT, 0, 0)?;
        self.primary_history_anchor = Some(HistoryLineageAnchor {
            reference,
            absolute_row: self.primary_history_origin.saturating_add(logical_history),
        });
        Ok(())
    }

    fn read_grid_row(&self, row: usize, cols: u16) -> Result<RowSnapshot, Error> {
        #[cfg(test)]
        self.history_row_reads
            .set(self.history_row_reads.get().saturating_add(1));
        let row = u32::try_from(row).map_err(|_| Error::LimitExceeded)?;
        let row_reference = terminal_grid_ref(&self.terminal, ffi::POINT_TAG_HISTORY, 0, row)?;
        let raw_row = grid_ref_query_row(&row_reference)?;
        let wrapped = row_query::<bool>(raw_row, ffi::ROW_DATA_WRAP)?;
        let mut cells = Vec::with_capacity(usize::from(cols));
        for col in 0..cols {
            let reference = terminal_grid_ref(&self.terminal, ffi::POINT_TAG_HISTORY, col, row)?;
            cells.push(read_grid_cell(&reference)?);
        }
        Ok(RowSnapshot {
            cells: Arc::new(cells),
            wrapped,
        })
    }
}

fn signed_offset(expected: usize, observed: usize) -> Result<isize, Error> {
    if expected >= observed {
        isize::try_from(expected - observed).map_err(|_| Error::LimitExceeded)
    } else {
        isize::try_from(observed - expected)
            .map(|offset| -offset)
            .map_err(|_| Error::LimitExceeded)
    }
}

fn apply_signed_offset(value: usize, offset: isize) -> Option<usize> {
    if offset >= 0 {
        value.checked_add(offset as usize)
    } else {
        value.checked_sub(offset.unsigned_abs())
    }
}

fn terminal_query<T>(handle: &TerminalHandle, tag: ffi::TerminalData) -> Result<T, Error> {
    let mut value = MaybeUninit::<T>::uninit();
    // SAFETY: private callers pair each tag with the output type documented
    // by Ghostty, and the owned terminal handle is valid.
    let result =
        unsafe { ffi::ghostty_terminal_get(handle.as_ptr(), tag, value.as_mut_ptr().cast()) };
    result_from_code(result)?;
    // SAFETY: success promises initialization of the correctly typed output.
    Ok(unsafe { value.assume_init() })
}

fn kitty_placement_query<T>(
    iterator: &KittyPlacementIteratorHandle,
    tag: ffi::KittyGraphicsPlacementData,
) -> Result<T, Error> {
    let mut value = MaybeUninit::<T>::uninit();
    // SAFETY: private callers pair each placement tag with the documented
    // output type and invoke this only while the iterator is positioned.
    result_from_code(unsafe {
        ffi::ghostty_kitty_graphics_placement_get(iterator.as_ptr(), tag, value.as_mut_ptr().cast())
    })?;
    // SAFETY: success promises initialization of the correctly typed output.
    Ok(unsafe { value.assume_init() })
}

fn kitty_image_query<T>(
    image: ffi::KittyGraphicsImage,
    tag: ffi::KittyGraphicsImageData,
) -> Result<T, Error> {
    let mut value = MaybeUninit::<T>::uninit();
    // SAFETY: private callers pair each image tag with the documented output
    // type and the borrowed image handle remains valid for this query.
    result_from_code(unsafe {
        ffi::ghostty_kitty_graphics_image_get(image, tag, value.as_mut_ptr().cast())
    })?;
    // SAFETY: success promises initialization of the correctly typed output.
    Ok(unsafe { value.assume_init() })
}

fn snapshot_bytes(handle: &TerminalHandle) -> Result<Vec<u8>, Error> {
    let mut required = 0usize;
    // SAFETY: this is the documented size query with a null, zero-length
    // destination and valid output counter.
    let first = unsafe {
        ffi::ghostty_snapshot_encode_buf(handle.as_ptr(), std::ptr::null_mut(), 0, &mut required)
    };
    if first != ffi::OUT_OF_SPACE {
        result_from_code(first)?;
    }
    if required == 0 {
        return Err(Error::InvalidValue);
    }
    let mut bytes = vec![0; required];
    let mut written = 0usize;
    // SAFETY: `bytes` owns the writable destination for the duration of this
    // synchronous encode and `written` is valid output storage.
    result_from_code(unsafe {
        ffi::ghostty_snapshot_encode_buf(
            handle.as_ptr(),
            bytes.as_mut_ptr(),
            bytes.len(),
            &mut written,
        )
    })?;
    if written > bytes.len() {
        return Err(Error::OutOfSpace);
    }
    bytes.truncate(written);
    Ok(bytes)
}

fn continuation_bytes(handle: &TerminalHandle) -> Result<Vec<u8>, Error> {
    let configured = terminal_query::<usize>(handle, ffi::TERMINAL_DATA_CONTINUATION_MAX_BYTES)?;
    if configured == 0 {
        return Err(Error::InvalidValue);
    }
    let mut required = 0usize;
    // SAFETY: this is the documented continuation size query.
    let first = unsafe {
        ffi::ghostty_terminal_continuation_buf(
            handle.as_ptr(),
            std::ptr::null_mut(),
            0,
            &mut required,
        )
    };
    if first != ffi::OUT_OF_SPACE {
        result_from_code(first)?;
    }
    if required == 0 {
        return Ok(Vec::new());
    }
    let mut bytes = vec![0; required];
    let mut written = 0usize;
    // SAFETY: `bytes` is a writable destination and the terminal is not being
    // mutated concurrently through this shared owning-thread reference.
    result_from_code(unsafe {
        ffi::ghostty_terminal_continuation_buf(
            handle.as_ptr(),
            bytes.as_mut_ptr(),
            bytes.len(),
            &mut written,
        )
    })?;
    if written > bytes.len() {
        return Err(Error::OutOfSpace);
    }
    bytes.truncate(written);
    Ok(bytes)
}

fn terminal_has_continuation(handle: &TerminalHandle) -> Result<bool, Error> {
    let mut required = 0usize;
    // SAFETY: this is the documented size query with a null destination and
    // valid output counter. A nonzero requirement is exactly Ghostty's
    // retained parser continuation.
    let result = unsafe {
        ffi::ghostty_terminal_continuation_buf(
            handle.as_ptr(),
            std::ptr::null_mut(),
            0,
            &mut required,
        )
    };
    if result != ffi::OUT_OF_SPACE {
        result_from_code(result)?;
    }
    Ok(required != 0)
}

fn terminal_string(
    handle: &TerminalHandle,
    tag: ffi::TerminalData,
) -> Result<Option<String>, Error> {
    let value = terminal_query::<ffi::GhosttyString>(handle, tag)?;
    if value.len == 0 {
        return Ok(None);
    }
    if value.ptr.is_null() {
        return Err(Error::NullString);
    }
    // SAFETY: this borrowed terminal string remains valid until the next
    // mutation. It is copied before this function returns.
    let bytes = unsafe { std::slice::from_raw_parts(value.ptr, value.len) };
    Ok(Some(
        std::str::from_utf8(bytes)
            .map_err(|_| Error::InvalidUtf8)?
            .to_owned(),
    ))
}

fn terminal_mode(handle: &TerminalHandle, mode: ffi::Mode) -> Result<bool, Error> {
    let mut config = ffi::TerminalModeConfig { mode, value: false };
    // SAFETY: the handle is valid and the mode query struct matches the
    // frozen GhosttyTerminalModeConfig layout.
    let result = unsafe {
        ffi::ghostty_terminal_get(
            handle.as_ptr(),
            ffi::TERMINAL_DATA_MODE,
            (&mut config as *mut ffi::TerminalModeConfig).cast(),
        )
    };
    result_from_code(result)?;
    Ok(config.value)
}

fn read_modes(handle: &TerminalHandle) -> Result<ModesSnapshot, Error> {
    let x10 = terminal_mode(handle, ffi::MODE_X10_MOUSE)?;
    let normal = terminal_mode(handle, ffi::MODE_NORMAL_MOUSE)?;
    let button = terminal_mode(handle, ffi::MODE_BUTTON_MOUSE)?;
    let any = terminal_mode(handle, ffi::MODE_ANY_MOUSE)?;
    let utf8 = terminal_mode(handle, ffi::MODE_UTF8_MOUSE)?;
    let sgr = terminal_mode(handle, ffi::MODE_SGR_MOUSE)?;
    Ok(ModesSnapshot {
        application_keypad: terminal_mode(handle, ffi::MODE_APPLICATION_KEYPAD)?,
        application_cursor: terminal_mode(handle, ffi::MODE_APPLICATION_CURSOR)?,
        bracketed_paste: terminal_mode(handle, ffi::MODE_BRACKETED_PASTE)?,
        synchronized_output: terminal_mode(handle, ffi::MODE_SYNCHRONIZED_OUTPUT)?,
        focus_reporting: terminal_mode(handle, ffi::MODE_FOCUS_EVENT)?,
        kitty_keyboard_flags: terminal_query(handle, ffi::TERMINAL_DATA_KITTY_KEYBOARD_FLAGS)?,
        mouse_protocol: if any {
            MouseProtocol::AnyMotion
        } else if button {
            MouseProtocol::ButtonMotion
        } else if normal {
            MouseProtocol::PressRelease
        } else if x10 {
            MouseProtocol::Press
        } else {
            MouseProtocol::None
        },
        mouse_encoding: if sgr {
            MouseEncoding::Sgr
        } else if utf8 {
            MouseEncoding::Utf8
        } else {
            MouseEncoding::Default
        },
    })
}

fn append_dirty_row(ranges: &mut Vec<RangeInclusive<u16>>, row: u16) {
    if let Some(previous) = ranges.last_mut()
        && row <= previous.end().saturating_add(1)
    {
        let start = *previous.start();
        *previous = start..=row.max(*previous.end());
    } else {
        ranges.push(row..=row);
    }
}

fn normalize_row_ranges(ranges: &mut Vec<RangeInclusive<u16>>) {
    ranges.sort_unstable_by_key(|range| *range.start());
    let mut merged: Vec<RangeInclusive<u16>> = Vec::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        if let Some(previous) = merged.last_mut()
            && range.start() <= &previous.end().saturating_add(1)
        {
            let start = *previous.start();
            let end = (*previous.end()).max(*range.end());
            *previous = start..=end;
        } else {
            merged.push(range);
        }
    }
    *ranges = merged;
}

fn row_index(row: usize) -> u16 {
    row.try_into().unwrap_or(u16::MAX)
}

fn render_query<T>(handle: &RenderStateHandle, tag: ffi::RenderStateData) -> Result<T, Error> {
    let mut value = MaybeUninit::<T>::uninit();
    render_query_into(handle, tag, value.as_mut_ptr())?;
    // SAFETY: success promises initialization of the correctly typed output.
    Ok(unsafe { value.assume_init() })
}

fn render_query_into<T>(
    handle: &RenderStateHandle,
    tag: ffi::RenderStateData,
    value: *mut T,
) -> Result<(), Error> {
    // SAFETY: private callers pair each tag with its documented output type.
    result_from_code(unsafe { ffi::ghostty_render_state_get(handle.as_ptr(), tag, value.cast()) })
}

fn row_iterator_query<T>(
    handle: &RowIteratorHandle,
    tag: ffi::RenderStateRowData,
) -> Result<T, Error> {
    let mut value = MaybeUninit::<T>::uninit();
    row_iterator_query_into(handle, tag, value.as_mut_ptr())?;
    // SAFETY: success promises initialization of the correctly typed output.
    Ok(unsafe { value.assume_init() })
}

fn row_iterator_query_into<T>(
    handle: &RowIteratorHandle,
    tag: ffi::RenderStateRowData,
    value: *mut T,
) -> Result<(), Error> {
    // SAFETY: private callers pair each tag with its documented output type.
    result_from_code(unsafe {
        ffi::ghostty_render_state_row_get(handle.as_ptr(), tag, value.cast())
    })
}

fn row_cells_query<T>(
    handle: &RowCellsHandle,
    tag: ffi::RenderStateRowCellsData,
) -> Result<T, Error> {
    let mut value = MaybeUninit::<T>::uninit();
    // SAFETY: private callers pair each tag with its documented output type.
    let result = unsafe {
        ffi::ghostty_render_state_row_cells_get(handle.as_ptr(), tag, value.as_mut_ptr().cast())
    };
    result_from_code(result)?;
    // SAFETY: success promises initialization of the correctly typed output.
    Ok(unsafe { value.assume_init() })
}

fn row_cell_grapheme(handle: &RowCellsHandle) -> Result<Cow<'static, str>, Error> {
    let mut inline = [0_u8; 16];
    let mut output = ffi::GhosttyBuffer {
        ptr: inline.as_mut_ptr(),
        cap: inline.len(),
        len: 0,
    };
    // SAFETY: the row-cells handle is positioned and output points to a
    // correctly laid-out GhosttyBuffer.
    let first = unsafe {
        ffi::ghostty_render_state_row_cells_get(
            handle.as_ptr(),
            ffi::RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_UTF8,
            (&mut output as *mut ffi::GhosttyBuffer).cast(),
        )
    };
    if first == ffi::SUCCESS {
        if output.len > inline.len() {
            return Err(Error::OutOfSpace);
        }
        return compact_utf8_grapheme(&inline[..output.len]);
    }
    if first != ffi::OUT_OF_SPACE {
        result_from_code(first)?;
    }
    let mut bytes = vec![0; output.len];
    output.ptr = bytes.as_mut_ptr();
    output.cap = bytes.len();
    // SAFETY: `output` describes writable storage owned by `bytes`, and the
    // row-cells iterator remains positioned on the same cell.
    result_from_code(unsafe {
        ffi::ghostty_render_state_row_cells_get(
            handle.as_ptr(),
            ffi::RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_UTF8,
            (&mut output as *mut ffi::GhosttyBuffer).cast(),
        )
    })?;
    if output.len > bytes.len() {
        return Err(Error::OutOfSpace);
    }
    bytes.truncate(output.len);
    String::from_utf8(bytes)
        .map(Cow::Owned)
        .map_err(|_| Error::InvalidUtf8)
}

fn row_cell_style(handle: &RowCellsHandle) -> Result<ffi::Style, Error> {
    let mut style = ffi::Style::init();
    // SAFETY: the iterator is positioned on a live cell and `style` is a
    // correctly initialized sized GhosttyStyle output.
    result_from_code(unsafe {
        ffi::ghostty_render_state_row_cells_get(
            handle.as_ptr(),
            ffi::RENDER_STATE_ROW_CELLS_DATA_STYLE,
            (&mut style as *mut ffi::Style).cast(),
        )
    })?;
    Ok(style)
}

fn normalize_style(style: ffi::Style) -> Result<StyleSnapshot, Error> {
    Ok(StyleSnapshot {
        foreground: normalize_style_color(style.fg_color)?,
        background: normalize_style_color(style.bg_color)?,
        underline_color: normalize_style_color(style.underline_color)?,
        bold: style.bold,
        dim: style.faint,
        italic: style.italic,
        blink: style.blink,
        inverse: style.inverse,
        invisible: style.invisible,
        strikethrough: style.strikethrough,
        overline: style.overline,
        underline: match style.underline {
            ffi::SGR_UNDERLINE_NONE => UnderlineSnapshot::None,
            ffi::SGR_UNDERLINE_SINGLE => UnderlineSnapshot::Single,
            ffi::SGR_UNDERLINE_DOUBLE => UnderlineSnapshot::Double,
            ffi::SGR_UNDERLINE_CURLY => UnderlineSnapshot::Curly,
            ffi::SGR_UNDERLINE_DOTTED => UnderlineSnapshot::Dotted,
            ffi::SGR_UNDERLINE_DASHED => UnderlineSnapshot::Dashed,
            _ => return Err(Error::InvalidValue),
        },
    })
}

fn normalize_style_color(color: ffi::StyleColor) -> Result<ColorSnapshot, Error> {
    match color.tag {
        ffi::STYLE_COLOR_NONE => Ok(ColorSnapshot::Default),
        // SAFETY: the active union member is selected by the checked tag.
        ffi::STYLE_COLOR_PALETTE => Ok(ColorSnapshot::Indexed(unsafe { color.value.palette })),
        ffi::STYLE_COLOR_RGB => {
            // SAFETY: the active union member is selected by the checked tag.
            let rgb = unsafe { color.value.rgb };
            Ok(ColorSnapshot::Rgb(rgb.r, rgb.g, rgb.b))
        }
        _ => Err(Error::InvalidValue),
    }
}

fn terminal_grid_ref(
    handle: &TerminalHandle,
    tag: ffi::PointTag,
    col: u16,
    row: u32,
) -> Result<ffi::GridRef, Error> {
    let point = ffi::Point::coordinate(tag, col, row);
    let mut reference = ffi::GridRef {
        size: std::mem::size_of::<ffi::GridRef>(),
        node: std::ptr::null_mut(),
        x: 0,
        y: 0,
    };
    // SAFETY: the terminal and output are valid. Callers use a documented
    // coordinate space and immediately consume the untracked reference.
    result_from_code(unsafe {
        ffi::ghostty_terminal_grid_ref(handle.as_ptr(), point, &mut reference)
    })?;
    if reference.node.is_null() {
        return Err(Error::NullHandle);
    }
    Ok(reference)
}

fn grid_ref_query_cell(reference: &ffi::GridRef) -> Result<ffi::Cell, Error> {
    let mut cell = MaybeUninit::<ffi::Cell>::uninit();
    // SAFETY: the untracked reference is still valid because no terminal
    // mutation occurs between its creation and this query.
    result_from_code(unsafe { ffi::ghostty_grid_ref_cell(reference, cell.as_mut_ptr()) })?;
    // SAFETY: Ghostty initialized the output on success.
    Ok(unsafe { cell.assume_init() })
}

fn grid_ref_query_row(reference: &ffi::GridRef) -> Result<ffi::Row, Error> {
    let mut row = MaybeUninit::<ffi::Row>::uninit();
    // SAFETY: the untracked reference remains valid and output is writable.
    result_from_code(unsafe { ffi::ghostty_grid_ref_row(reference, row.as_mut_ptr()) })?;
    // SAFETY: Ghostty initialized the output on success.
    Ok(unsafe { row.assume_init() })
}

fn grid_ref_grapheme(reference: &ffi::GridRef) -> Result<Cow<'static, str>, Error> {
    let mut inline = [0_u32; 8];
    let mut len = 0;
    // SAFETY: `inline` contains writable u32 elements and the untracked
    // reference remains valid throughout this bounded query.
    let first = unsafe {
        ffi::ghostty_grid_ref_graphemes(reference, inline.as_mut_ptr(), inline.len(), &mut len)
    };
    if first == ffi::SUCCESS {
        if len > inline.len() {
            return Err(Error::OutOfSpace);
        }
        return grapheme_from_codepoints(&inline[..len]);
    }
    if first != ffi::OUT_OF_SPACE {
        result_from_code(first)?;
    }
    let mut codepoints = vec![0; len];
    // SAFETY: the buffer contains `len` writable u32 elements and the
    // untracked reference is still valid.
    result_from_code(unsafe {
        ffi::ghostty_grid_ref_graphemes(
            reference,
            codepoints.as_mut_ptr(),
            codepoints.len(),
            &mut len,
        )
    })?;
    if len > codepoints.len() {
        return Err(Error::OutOfSpace);
    }
    grapheme_from_codepoints(&codepoints[..len])
}

fn compact_utf8_grapheme(bytes: &[u8]) -> Result<Cow<'static, str>, Error> {
    if bytes.is_empty() {
        return Ok(Cow::Borrowed(""));
    }
    if let [byte] = bytes
        && let Some(grapheme) = borrowed_printable_ascii(*byte)
    {
        return Ok(Cow::Borrowed(grapheme));
    }
    std::str::from_utf8(bytes)
        .map(|grapheme| Cow::Owned(grapheme.to_owned()))
        .map_err(|_| Error::InvalidUtf8)
}

fn grapheme_from_codepoints(codepoints: &[u32]) -> Result<Cow<'static, str>, Error> {
    if codepoints.is_empty() {
        return Ok(Cow::Borrowed(""));
    }
    if let [codepoint] = codepoints
        && let Ok(byte) = u8::try_from(*codepoint)
        && let Some(grapheme) = borrowed_printable_ascii(byte)
    {
        return Ok(Cow::Borrowed(grapheme));
    }
    let mut grapheme = String::with_capacity(codepoints.len().saturating_mul(4));
    for &codepoint in codepoints {
        grapheme.push(char::from_u32(codepoint).ok_or(Error::InvalidValue)?);
    }
    Ok(Cow::Owned(grapheme))
}

fn borrowed_printable_ascii(byte: u8) -> Option<&'static str> {
    static PRINTABLE_ASCII: &[u8] =
        b" !\"#$%&'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~";
    let index = usize::from(byte.checked_sub(b' ')?);
    let bytes = PRINTABLE_ASCII.get(index..=index)?;
    // Every byte in this static table is printable ASCII and therefore UTF-8.
    std::str::from_utf8(bytes).ok()
}

fn grid_ref_hyperlink(reference: &ffi::GridRef) -> Result<Option<String>, Error> {
    let mut len = 0;
    // SAFETY: null with zero length is Ghostty's documented sizing query.
    let first = unsafe {
        ffi::ghostty_grid_ref_hyperlink_uri(reference, std::ptr::null_mut(), 0, &mut len)
    };
    if first == ffi::SUCCESS && len == 0 {
        return Ok(None);
    }
    if first != ffi::OUT_OF_SPACE {
        result_from_code(first)?;
    }
    let mut bytes = vec![0; len];
    // SAFETY: the buffer contains `len` writable bytes and the reference is
    // valid until the next terminal mutation.
    result_from_code(unsafe {
        ffi::ghostty_grid_ref_hyperlink_uri(reference, bytes.as_mut_ptr(), bytes.len(), &mut len)
    })?;
    if len > bytes.len() {
        return Err(Error::OutOfSpace);
    }
    bytes.truncate(len);
    Ok(Some(
        String::from_utf8(bytes).map_err(|_| Error::InvalidUtf8)?,
    ))
}

fn grid_ref_style(reference: &ffi::GridRef) -> Result<ffi::Style, Error> {
    let mut style = ffi::Style::init();
    // SAFETY: `style` is a correctly initialized sized output and the
    // untracked reference has not been invalidated.
    result_from_code(unsafe { ffi::ghostty_grid_ref_style(reference, &mut style) })?;
    Ok(style)
}

fn read_grid_cell(reference: &ffi::GridRef) -> Result<CellSnapshot, Error> {
    let raw_cell = grid_ref_query_cell(reference)?;
    let wide = cell_query::<ffi::CellWide>(raw_cell, ffi::CELL_DATA_WIDE)?;
    let (width, continuation) = match wide {
        ffi::CELL_WIDE_NARROW => (1, false),
        ffi::CELL_WIDE_WIDE => (2, false),
        ffi::CELL_WIDE_SPACER_TAIL | ffi::CELL_WIDE_SPACER_HEAD => (0, true),
        _ => return Err(Error::InvalidValue),
    };
    Ok(CellSnapshot {
        grapheme: grid_ref_grapheme(reference)?,
        width,
        continuation,
        style: normalize_style(grid_ref_style(reference)?)?,
        hyperlink: if cell_query(raw_cell, ffi::CELL_DATA_HAS_HYPERLINK)? {
            grid_ref_hyperlink(reference)?
        } else {
            None
        },
    })
}

fn row_query<T>(row: ffi::Row, tag: ffi::RowData) -> Result<T, Error> {
    let mut value = MaybeUninit::<T>::uninit();
    // SAFETY: the row came from the live iterator and private callers pair
    // each tag with its documented output type.
    let result = unsafe { ffi::ghostty_row_get(row, tag, value.as_mut_ptr().cast()) };
    result_from_code(result)?;
    // SAFETY: success promises initialization of the correctly typed output.
    Ok(unsafe { value.assume_init() })
}

fn cell_query<T>(cell: ffi::Cell, tag: ffi::CellData) -> Result<T, Error> {
    let mut value = MaybeUninit::<T>::uninit();
    // SAFETY: the cell came from the live iterator and private callers pair
    // each tag with its documented output type.
    let result = unsafe { ffi::ghostty_cell_get(cell, tag, value.as_mut_ptr().cast()) };
    result_from_code(result)?;
    // SAFETY: success promises initialization of the correctly typed output.
    Ok(unsafe { value.assume_init() })
}

fn query<T>(tag: ffi::BuildInfo) -> Result<T, Error> {
    let mut value = MaybeUninit::<T>::uninit();
    // SAFETY: Every caller is private to this crate and pairs each official
    // build-info tag with the output type documented by Ghostty's C header.
    let result = unsafe { ffi::ghostty_build_info(tag, value.as_mut_ptr().cast()) };
    result_from_code(result)?;
    // SAFETY: Ghostty promises to initialize the correctly typed output after
    // returning SUCCESS.
    Ok(unsafe { value.assume_init() })
}

fn query_string(tag: ffi::BuildInfo) -> Result<&'static str, Error> {
    let value = query::<ffi::GhosttyString>(tag)?;
    if value.len == 0 {
        return Ok("");
    }
    if value.ptr.is_null() {
        return Err(Error::NullString);
    }
    // SAFETY: Ghostty documents build-info strings as immutable for the
    // process lifetime. Non-empty strings were checked for a non-null pointer.
    let bytes = unsafe { std::slice::from_raw_parts(value.ptr, value.len) };
    std::str::from_utf8(bytes).map_err(|_| Error::InvalidUtf8)
}

fn result_from_code(code: ffi::ResultCode) -> Result<(), Error> {
    match code {
        ffi::SUCCESS => Ok(()),
        ffi::OUT_OF_MEMORY => Err(Error::OutOfMemory),
        ffi::INVALID_VALUE => Err(Error::InvalidValue),
        ffi::OUT_OF_SPACE => Err(Error::OutOfSpace),
        ffi::NO_VALUE => Err(Error::NoValue),
        ffi::IO_ERROR => Err(Error::IoError),
        ffi::LIMIT_EXCEEDED => Err(Error::LimitExceeded),
        other => Err(Error::UnknownResult(other)),
    }
}

const _: () = {
    assert!(std::mem::size_of::<ffi::ResultCode>() == std::mem::size_of::<i32>());
    assert!(std::mem::size_of::<ffi::BuildInfo>() == std::mem::size_of::<i32>());
    assert!(std::mem::size_of::<ffi::GhosttyString>() == 2 * std::mem::size_of::<usize>());
    assert!(std::mem::align_of::<ffi::GhosttyString>() == std::mem::align_of::<usize>());
    assert!(std::mem::offset_of!(ffi::GhosttyString, ptr) == 0);
    assert!(std::mem::offset_of!(ffi::GhosttyString, len) == std::mem::size_of::<usize>());
    assert!(std::mem::size_of::<ffi::GhosttyBuffer>() == 3 * std::mem::size_of::<usize>());
    assert!(std::mem::offset_of!(ffi::GhosttyBuffer, ptr) == 0);
    assert!(std::mem::offset_of!(ffi::GhosttyBuffer, cap) == std::mem::size_of::<usize>());
    assert!(std::mem::offset_of!(ffi::GhosttyBuffer, len) == 2 * std::mem::size_of::<usize>());
    assert!(std::mem::size_of::<OptimizeMode>() == std::mem::size_of::<i32>());
    assert!(std::mem::size_of::<ffi::Cell>() == std::mem::size_of::<u64>());
    assert!(std::mem::size_of::<ffi::Row>() == std::mem::size_of::<u64>());
    assert!(std::mem::size_of::<ffi::Mode>() == std::mem::size_of::<u16>());
    assert!(std::mem::size_of::<ffi::TerminalModeConfig>() == 4);
    assert!(std::mem::offset_of!(ffi::TerminalModeConfig, mode) == 0);
    assert!(std::mem::offset_of!(ffi::TerminalModeConfig, value) == 2);
    assert!(std::mem::size_of::<ffi::Allocator>() == 2 * std::mem::size_of::<usize>());
    assert!(std::mem::offset_of!(ffi::Allocator, ctx) == 0);
    assert!(std::mem::offset_of!(ffi::Allocator, vtable) == std::mem::size_of::<usize>());
    assert!(std::mem::size_of::<ffi::PointCoordinate>() == 8);
    assert!(std::mem::offset_of!(ffi::PointCoordinate, x) == 0);
    assert!(std::mem::offset_of!(ffi::PointCoordinate, y) == 4);
    assert!(std::mem::size_of::<ffi::PointValue>() == 16);
    assert!(std::mem::offset_of!(ffi::Point, tag) == 0);
    assert!(std::mem::offset_of!(ffi::GridRef, size) == 0);
    assert!(std::mem::offset_of!(ffi::GridRef, node) == std::mem::size_of::<usize>());
    assert!(std::mem::size_of::<ffi::ColorRgb>() == 3);
    assert!(std::mem::size_of::<ffi::StyleColorValue>() == 8);
    assert!(std::mem::offset_of!(ffi::Style, size) == 0);
    assert!(std::mem::offset_of!(ffi::Style, fg_color) == std::mem::size_of::<usize>());
    if std::mem::size_of::<usize>() == 8 {
        assert!(std::mem::size_of::<ffi::Point>() == 24);
        assert!(std::mem::offset_of!(ffi::Point, value) == 8);
        assert!(std::mem::size_of::<ffi::GridRef>() == 24);
        assert!(std::mem::size_of::<ffi::StyleColor>() == 16);
        assert!(std::mem::size_of::<ffi::Style>() == 72);
    }
};

/// Exercises each allocation-returning handle constructor with an allocator
/// that always fails. This is public only so Lector's cross-crate integration
/// test can verify the private FFI boundary.
#[doc(hidden)]
pub fn allocation_failure_probe() -> Result<(), Error> {
    let allocator = ffi::Allocator {
        ctx: std::ptr::null_mut(),
        vtable: &FAILING_VTABLE,
    };
    let allocator = &allocator as *const ffi::Allocator;
    let results = [
        TerminalHandle::new_with_allocator(2, 2, allocator).map(|_| ()),
        RenderStateHandle::new_with_allocator(allocator).map(|_| ()),
        RowIteratorHandle::new_with_allocator(allocator).map(|_| ()),
        RowCellsHandle::new_with_allocator(allocator).map(|_| ()),
    ];
    if results
        .into_iter()
        .all(|result| result == Err(Error::OutOfMemory))
    {
        Ok(())
    } else {
        Err(Error::InvalidValue)
    }
}

unsafe extern "C" fn fail_alloc(
    _context: *mut c_void,
    _length: usize,
    _alignment: u8,
    _return_address: usize,
) -> *mut c_void {
    std::ptr::null_mut()
}

unsafe extern "C" fn fail_resize(
    _context: *mut c_void,
    _memory: *mut c_void,
    _memory_length: usize,
    _alignment: u8,
    _new_length: usize,
    _return_address: usize,
) -> bool {
    false
}

unsafe extern "C" fn fail_remap(
    _context: *mut c_void,
    _memory: *mut c_void,
    _memory_length: usize,
    _alignment: u8,
    _new_length: usize,
    _return_address: usize,
) -> *mut c_void {
    std::ptr::null_mut()
}

unsafe extern "C" fn no_op_free(
    _context: *mut c_void,
    _memory: *mut c_void,
    _memory_length: usize,
    _alignment: u8,
    _return_address: usize,
) {
}

static FAILING_VTABLE: ffi::AllocatorVtable = ffi::AllocatorVtable {
    alloc: fail_alloc,
    resize: fail_resize,
    remap: fail_remap,
    free: no_op_free,
};

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, sync::Arc};

    use super::{
        CellSnapshot, OperationSnapshot, RenderDamageSnapshot, RowSnapshot, StreamObserver,
        Terminal,
    };

    #[test]
    fn row_content_appenders_share_range_and_trailing_blank_semantics() {
        let cell = |grapheme| CellSnapshot {
            grapheme: Cow::Borrowed(grapheme),
            ..CellSnapshot::default()
        };
        let row = RowSnapshot {
            cells: Arc::new(vec![
                cell("a"),
                CellSnapshot::default(),
                cell("界"),
                CellSnapshot {
                    continuation: true,
                    ..CellSnapshot::default()
                },
                CellSnapshot::default(),
                cell(" "),
            ]),
            wrapped: false,
        };

        let mut full = String::from("prefix:");
        row.append_contents_to(&mut full);
        assert_eq!(full, "prefix:a 界  ");

        let mut middle = String::new();
        row.append_contents_range_to(&mut middle, 1, 3);
        assert_eq!(middle, " 界");

        let mut trailing_blank = String::new();
        row.append_contents_range_to(&mut trailing_blank, 0, 2);
        assert_eq!(trailing_blank, "a");
    }

    #[test]
    fn partial_snapshot_refresh_reads_only_ghostty_dirty_rows() {
        let mut terminal = Terminal::new(24, 80).expect("create terminal");
        terminal.snapshot_row_reads = 0;

        let update = terminal
            .advance(b"one line")
            .expect("advance one dirty row");

        assert_eq!(
            update.damage,
            RenderDamageSnapshot::Rows(std::iter::once(0..=0).collect())
        );
        assert_eq!(
            terminal.snapshot_row_reads, 1,
            "an ordinary one-line update must not re-read the other 23 rows"
        );
        assert_eq!(terminal.snapshot().rows[0].text(), "one line");
    }

    #[test]
    fn history_suffix_capture_reads_only_new_rows_at_the_logical_cap() {
        const CAPACITY: usize = 10_000;
        let mut terminal =
            Terminal::new_with_scrollback(2, 8, CAPACITY).expect("create capped history terminal");
        let mut output = Vec::with_capacity((CAPACITY + 2) * 9);
        for row in 0..(CAPACITY + 2) {
            output.extend_from_slice(format!("r{row:06}\r\n").as_bytes());
        }
        terminal.advance(&output).expect("fill logical history cap");
        assert_eq!(terminal.snapshot().scrollback_extent, CAPACITY);

        terminal.history_row_reads.set(0);
        let suffix = terminal
            .history_rows_from(CAPACITY - 1)
            .expect("read newest logical history row");
        assert_eq!(suffix.len(), 1);
        assert_eq!(suffix[0].contents(), "r010000");
        assert_eq!(
            terminal.history_row_reads.get(),
            1,
            "suffix capture must not revisit the other 9,999 retained rows"
        );
    }

    #[test]
    fn ascii_observer_coalesces_a_large_fragmented_run_without_rescanning_text() {
        const CHARACTERS: usize = 16_384;
        let mut observer = StreamObserver {
            operation_rows: 1,
            operation_cols: u16::MAX,
            operation_reliable: true,
            ..StreamObserver::default()
        };

        for _ in 0..CHARACTERS {
            observer.record_write('x');
        }

        assert_eq!(observer.operations.len(), 1);
        let OperationSnapshot::WriteRun { row, col, text } = &observer.operations[0] else {
            panic!("fragmented ASCII writes must remain one observer run");
        };
        assert_eq!((*row, *col), (0, 0));
        assert_eq!(text.len(), CHARACTERS);
        assert!(text.is_ascii());
    }
}
