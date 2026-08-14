//! Lector-owned safe boundary for the pinned official `libghostty-vt` C ABI.
//!
//! This crate deliberately exposes only APIs that Lector uses and verifies.
//! Raw declarations and all `unsafe` calls remain private to this crate.

#![deny(unsafe_op_in_unsafe_fn)]

mod ffi;

use std::{
    ffi::c_void, fmt, marker::PhantomData, mem::MaybeUninit, ops::RangeInclusive, ptr::NonNull,
    rc::Rc,
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
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ColorSnapshot {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// The review-relevant style attributes of a Ghostty cell.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StyleSnapshot {
    pub foreground: ColorSnapshot,
    pub background: ColorSnapshot,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
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
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CellSnapshot {
    pub grapheme: String,
    pub width: u8,
    pub continuation: bool,
    pub style: StyleSnapshot,
    pub hyperlink: Option<String>,
}

/// A normalized visible row from Ghostty.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RowSnapshot {
    pub cells: Vec<CellSnapshot>,
    pub wrapped: bool,
}

impl RowSnapshot {
    pub fn text(&self) -> String {
        let mut output = String::new();
        let mut pending_spaces = 0;
        for cell in &self.cells {
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
        output
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CursorSnapshot {
    pub row: u16,
    pub col: u16,
    pub visible: bool,
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

/// Operation and damage facts produced by the same write that mutated Ghostty.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UpdateSnapshot {
    pub effects: Vec<EffectSnapshot>,
    pub pty_replies: Vec<u8>,
    pub printed_runs: Vec<PrintedRunSnapshot>,
    pub cursor_operations: usize,
    pub scroll_operations: usize,
    pub changed_rows: Vec<RangeInclusive<u16>>,
    pub cursor_before: CursorSnapshot,
    pub cursor_after: CursorSnapshot,
    pub alternate_screen_before: bool,
    pub alternate_screen_after: bool,
    pub synchronized_output: bool,
}

/// Ghostty state normalized for Lector's engine-neutral consumers.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TerminalSnapshot {
    pub rows: Vec<RowSnapshot>,
    pub scrollback: Vec<RowSnapshot>,
    pub cursor: CursorSnapshot,
    pub width_px: u32,
    pub height_px: u32,
    pub alternate_screen: bool,
    pub modes: ModesSnapshot,
    pub title: Option<String>,
    pub working_directory: Option<String>,
    pub scrollback_extent: usize,
    pub semantic_marks: Vec<SemanticMarkSnapshot>,
}

/// An unstable Ghostty snapshot plus the Lector observer continuation needed
/// for a diagnostic round-trip.
///
/// This is intentionally opaque and is not a persistence or compatibility
/// promise. Ghostty snapshot format version 1 is still work in progress, and
/// Lector does not use this path for live runtime correctness.
pub struct DiagnosticSnapshot {
    bytes: Vec<u8>,
    observer_continuation: Vec<u8>,
    active_hyperlink: Option<String>,
    scrollback_capacity: usize,
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

#[derive(Default)]
struct EffectSink {
    events: Vec<EffectSnapshot>,
    pty_replies: Vec<u8>,
    error: Option<Error>,
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
    if let Some(sink) = unsafe { effect_sink(userdata) } {
        sink.events
            .push(EffectSnapshot::Query(QuerySnapshot::Enquiry));
    }
    ffi::GhosttyString::default()
}

extern "C" fn effect_xtversion(
    _terminal: ffi::Terminal,
    userdata: *mut c_void,
) -> ffi::GhosttyString {
    // SAFETY: see `effect_write_pty`.
    if let Some(sink) = unsafe { effect_sink(userdata) } {
        sink.events
            .push(EffectSnapshot::Query(QuerySnapshot::XtVersion));
    }
    ffi::GhosttyString::default()
}

extern "C" fn effect_size(
    _terminal: ffi::Terminal,
    userdata: *mut c_void,
    _out: *mut c_void,
) -> bool {
    // SAFETY: see `effect_write_pty`.
    if let Some(sink) = unsafe { effect_sink(userdata) } {
        sink.events.push(EffectSnapshot::Query(QuerySnapshot::Size));
    }
    false
}

extern "C" fn effect_color_scheme(
    _terminal: ffi::Terminal,
    userdata: *mut c_void,
    _out: *mut c_void,
) -> bool {
    // SAFETY: see `effect_write_pty`.
    if let Some(sink) = unsafe { effect_sink(userdata) } {
        sink.events
            .push(EffectSnapshot::Query(QuerySnapshot::ColorScheme));
    }
    false
}

extern "C" fn effect_device_attributes(
    _terminal: ffi::Terminal,
    userdata: *mut c_void,
    _out: *mut c_void,
) -> bool {
    // SAFETY: see `effect_write_pty`.
    if let Some(sink) = unsafe { effect_sink(userdata) } {
        sink.events
            .push(EffectSnapshot::Query(QuerySnapshot::DeviceAttributes));
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

#[derive(Default)]
struct StreamObserver {
    events: Vec<SemanticKindSnapshot>,
    printed_runs: Vec<PrintedRunSnapshot>,
    current_print: String,
    cursor_operations: usize,
    scroll_operations: usize,
    history_cleared: bool,
    active_hyperlink: Option<String>,
}

impl StreamObserver {
    fn take_semantic_events(&mut self) -> Vec<SemanticKindSnapshot> {
        std::mem::take(&mut self.events)
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

    fn take_update(&mut self) -> StreamUpdate {
        self.flush_print();
        self.history_cleared = false;
        StreamUpdate {
            printed_runs: std::mem::take(&mut self.printed_runs),
            cursor_operations: std::mem::take(&mut self.cursor_operations),
            scroll_operations: std::mem::take(&mut self.scroll_operations),
        }
    }
}

#[derive(Default)]
struct StreamUpdate {
    printed_runs: Vec<PrintedRunSnapshot>,
    cursor_operations: usize,
    scroll_operations: usize,
}

impl vte::Perform for StreamObserver {
    fn print(&mut self, character: char) {
        self.current_print.push(character);
    }
    fn execute(&mut self, byte: u8) {
        self.flush_print();
        match byte {
            b'\x08' => self.cursor_operations += 1,
            b'\r' => self.push_boundary(PrintBoundarySnapshot::CarriageReturn),
            b'\n' | b'\x0b' | b'\x0c' => self.push_boundary(PrintBoundarySnapshot::LineFeed),
            _ => {}
        }
    }
    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {
        self.flush_print();
    }
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        self.flush_print();
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
            _ => return,
        };
        self.events.push(kind);
    }
    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        self.flush_print();
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
            }
            match action {
                'A'..='H' => self.cursor_operations += 1,
                'S' | 'T' => self.scroll_operations += 1,
                _ => {}
            }
        }
    }
    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {
        self.flush_print();
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
    semantic_marks: Vec<TrackedSemanticMark>,
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
        // Keep callback userdata alive until after the terminal even if a
        // later constructor step fails and local variables unwind.
        let mut effect_sink = Box::<EffectSink>::default();
        let terminal = TerminalHandle::new(rows, cols)?;
        // Ghostty prunes complete pages when its physical line limit is
        // crossed. Keep one logical window of line headroom so a page prune
        // cannot undershoot Lector's requested window; the byte limit is
        // disabled above so it cannot preempt this policy. Lector still
        // exposes and anchors only the newest requested rows.
        terminal.set_scrollback_capacity(scrollback_capacity.saturating_mul(2))?;
        terminal.set_option(
            ffi::TERMINAL_OPT_CONTINUATION_MAX_BYTES,
            (&CONTINUATION_MAX_BYTES as *const usize).cast(),
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
            semantic_marks: Vec::new(),
            _thread_bound: PhantomData,
        };
        result.refresh_snapshot()?;
        Ok(result)
    }

    pub fn advance(&mut self, bytes: &[u8]) -> Result<UpdateSnapshot, Error> {
        let before = self.snapshot.clone();
        let new_semantic_start = self.semantic_marks.len();
        let mut segment_start = 0;
        for (index, byte) in bytes.iter().copied().enumerate() {
            self.stream_parser
                .advance(&mut self.stream_observer, &[byte]);
            if self.stream_observer.events.is_empty() {
                continue;
            }
            self.write_vt(&bytes[segment_start..=index]);
            self.refresh_snapshot()?;
            self.anchor_semantic_events(before.scrollback_extent)?;
            segment_start = index + 1;
        }
        if segment_start < bytes.len() {
            self.write_vt(&bytes[segment_start..]);
            self.refresh_snapshot()?;
        }
        self.recalibrate_new_semantic_marks(new_semantic_start)?;
        self.refresh_semantic_marks()?;
        let stream = self.stream_observer.take_update();
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
        Ok(UpdateSnapshot {
            effects,
            pty_replies,
            printed_runs: stream.printed_runs,
            cursor_operations: stream.cursor_operations,
            scroll_operations: stream.scroll_operations,
            changed_rows: changed_row_ranges(&before, &self.snapshot),
            cursor_before: before.cursor,
            cursor_after: self.snapshot.cursor,
            alternate_screen_before: before.alternate_screen,
            alternate_screen_after: self.snapshot.alternate_screen,
            synchronized_output: self.snapshot.modes.synchronized_output,
        })
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
        } = snapshot;
        // Declare callback storage before the decoded terminal so partial
        // constructor unwinding always frees the terminal first.
        let mut effect_sink = Box::<EffectSink>::default();
        let decoder = SnapshotDecoderHandle::new(&bytes)?;
        let terminal = decoder.decode()?;
        drop(decoder);

        terminal.set_option(
            ffi::TERMINAL_OPT_CONTINUATION_MAX_BYTES,
            (&CONTINUATION_MAX_BYTES as *const usize).cast(),
        )?;
        register_effects(&terminal, &mut effect_sink)?;
        let render_state = RenderStateHandle::new()?;
        let row_iterator = RowIteratorHandle::new()?;
        let row_cells = RowCellsHandle::new()?;
        let mut stream_parser = vte::Parser::new();
        let mut stream_observer = StreamObserver::default();
        stream_parser.advance(&mut stream_observer, &observer_continuation);
        let _ = stream_observer.take_update();
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
            // OSC 133 grid references are deliberately not part of this
            // diagnostic path yet. Runtime correctness never restores here.
            semantic_marks: Vec::new(),
            _thread_bound: PhantomData,
        };
        result.refresh_snapshot()?;
        Ok(result)
    }

    pub fn reset(&mut self) -> Result<(), Error> {
        // SAFETY: the terminal handle is valid.
        unsafe { ffi::ghostty_terminal_reset(self.terminal.as_ptr()) };
        self.stream_parser = vte::Parser::new();
        self.stream_observer = StreamObserver::default();
        let _ = self.effect_sink.take()?;
        self.semantic_marks.clear();
        self.refresh_snapshot()
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

    pub fn scrollback_extent(&self) -> usize {
        self.snapshot.scrollback_extent
    }

    /// The number of physically retained Ghostty rows older than Lector's
    /// logical scrollback window.
    pub fn history_origin(&self) -> Result<usize, Error> {
        Ok(self
            .actual_scrollback_extent()?
            .saturating_sub(self.scrollback_capacity))
    }

    /// Tracks a point in Ghostty's physical full-screen coordinate space.
    /// Callers should add `history_origin()` to a Lector logical row first.
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
        for kind in events {
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

    fn refresh_snapshot(&mut self) -> Result<(), Error> {
        // SAFETY: both owned handles are valid and are only accessed from
        // this thread while `&mut self` excludes concurrent mutation.
        let result = unsafe {
            ffi::ghostty_render_state_update(self.render_state.as_ptr(), self.terminal.as_ptr())
        };
        result_from_code(result)?;

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

        let mut normalized_rows = Vec::with_capacity(usize::from(rows));
        for row in 0..rows {
            // SAFETY: the iterator was populated from the current render
            // state and remains valid until its next render-state update.
            if !unsafe { ffi::ghostty_render_state_row_iterator_next(self.row_iterator.as_ptr()) } {
                return Err(Error::NoValue);
            }
            normalized_rows.push(self.read_row(row, cols)?);
        }
        // Detect an ABI or iteration contract change rather than silently
        // truncating a viewport.
        // SAFETY: same iterator validity as above.
        if unsafe { ffi::ghostty_render_state_row_iterator_next(self.row_iterator.as_ptr()) } {
            return Err(Error::InvalidValue);
        }

        let screen = terminal_query::<ffi::TerminalScreen>(
            &self.terminal,
            ffi::TERMINAL_DATA_ACTIVE_SCREEN,
        )?;
        let title = terminal_string(&self.terminal, ffi::TERMINAL_DATA_TITLE)?;
        let working_directory = terminal_string(&self.terminal, ffi::TERMINAL_DATA_PWD)?;
        let scrollback_extent = self
            .actual_scrollback_extent()?
            .min(self.scrollback_capacity);
        self.snapshot = TerminalSnapshot {
            rows: normalized_rows,
            scrollback: Vec::new(),
            cursor: CursorSnapshot {
                row: terminal_query(&self.terminal, ffi::TERMINAL_DATA_CURSOR_Y)?,
                col: terminal_query(&self.terminal, ffi::TERMINAL_DATA_CURSOR_X)?,
                visible: terminal_query(&self.terminal, ffi::TERMINAL_DATA_CURSOR_VISIBLE)?,
            },
            width_px: terminal_query(&self.terminal, ffi::TERMINAL_DATA_WIDTH_PX)?,
            height_px: terminal_query(&self.terminal, ffi::TERMINAL_DATA_HEIGHT_PX)?,
            alternate_screen: match screen {
                ffi::TERMINAL_SCREEN_PRIMARY => false,
                ffi::TERMINAL_SCREEN_ALTERNATE => true,
                _ => return Err(Error::InvalidValue),
            },
            modes: read_modes(&self.terminal)?,
            title,
            working_directory,
            scrollback_extent,
            semantic_marks: Vec::new(),
        };
        Ok(())
    }

    fn read_row(&mut self, row: u16, cols: u16) -> Result<RowSnapshot, Error> {
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
            cells: normalized_cells,
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

    fn read_grid_row(&self, row: usize, cols: u16) -> Result<RowSnapshot, Error> {
        let row = u32::try_from(row).map_err(|_| Error::LimitExceeded)?;
        let row_reference = terminal_grid_ref(&self.terminal, ffi::POINT_TAG_HISTORY, 0, row)?;
        let raw_row = grid_ref_query_row(&row_reference)?;
        let wrapped = row_query::<bool>(raw_row, ffi::ROW_DATA_WRAP)?;
        let mut cells = Vec::with_capacity(usize::from(cols));
        for col in 0..cols {
            let reference = terminal_grid_ref(&self.terminal, ffi::POINT_TAG_HISTORY, col, row)?;
            cells.push(read_grid_cell(&reference)?);
        }
        Ok(RowSnapshot { cells, wrapped })
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

fn changed_row_ranges(
    before: &TerminalSnapshot,
    after: &TerminalSnapshot,
) -> Vec<RangeInclusive<u16>> {
    let row_count = before.rows.len().max(after.rows.len());
    let mut ranges = Vec::new();
    let mut start = None;
    for row in 0..row_count {
        let changed = before.rows.get(row) != after.rows.get(row)
            || before.alternate_screen != after.alternate_screen;
        match (start, changed) {
            (None, true) => start = Some(row),
            (Some(first), false) => {
                ranges.push(row_index(first)..=row_index(row.saturating_sub(1)));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(first) = start {
        ranges.push(row_index(first)..=row_index(row_count.saturating_sub(1)));
    }
    ranges
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

fn row_cell_grapheme(handle: &RowCellsHandle) -> Result<String, Error> {
    let mut output = ffi::GhosttyBuffer {
        ptr: std::ptr::null_mut(),
        cap: 0,
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
    if first == ffi::SUCCESS && output.len == 0 {
        return Ok(String::new());
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
    String::from_utf8(bytes).map_err(|_| Error::InvalidUtf8)
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
        bold: style.bold,
        dim: style.faint,
        italic: style.italic,
        underline: style.underline != 0,
        inverse: style.inverse,
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

fn grid_ref_grapheme(reference: &ffi::GridRef) -> Result<String, Error> {
    let mut len = 0;
    // SAFETY: null with zero length is Ghostty's documented sizing query.
    let first =
        unsafe { ffi::ghostty_grid_ref_graphemes(reference, std::ptr::null_mut(), 0, &mut len) };
    if first == ffi::SUCCESS && len == 0 {
        return Ok(String::new());
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
    let mut grapheme = String::new();
    for codepoint in codepoints.into_iter().take(len) {
        grapheme.push(char::from_u32(codepoint).ok_or(Error::InvalidValue)?);
    }
    Ok(grapheme)
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
