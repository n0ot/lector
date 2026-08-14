use std::ffi::{c_int, c_void};

pub(crate) type ResultCode = c_int;
pub(crate) const SUCCESS: ResultCode = 0;
pub(crate) const OUT_OF_MEMORY: ResultCode = -1;
pub(crate) const INVALID_VALUE: ResultCode = -2;
pub(crate) const OUT_OF_SPACE: ResultCode = -3;
pub(crate) const NO_VALUE: ResultCode = -4;
pub(crate) const IO_ERROR: ResultCode = -5;
pub(crate) const LIMIT_EXCEEDED: ResultCode = -6;

pub(crate) type BuildInfo = c_int;
pub(crate) const BUILD_INFO_SIMD: BuildInfo = 1;
pub(crate) const BUILD_INFO_KITTY_GRAPHICS: BuildInfo = 2;
pub(crate) const BUILD_INFO_TMUX_CONTROL_MODE: BuildInfo = 3;
pub(crate) const BUILD_INFO_OPTIMIZE: BuildInfo = 4;
pub(crate) const BUILD_INFO_VERSION_STRING: BuildInfo = 5;
pub(crate) const BUILD_INFO_VERSION_MAJOR: BuildInfo = 6;
pub(crate) const BUILD_INFO_VERSION_MINOR: BuildInfo = 7;
pub(crate) const BUILD_INFO_VERSION_PATCH: BuildInfo = 8;
pub(crate) const BUILD_INFO_VERSION_PRE: BuildInfo = 9;
pub(crate) const BUILD_INFO_VERSION_BUILD: BuildInfo = 10;

pub(crate) type Terminal = *mut c_void;
pub(crate) type KittyGraphics = *mut c_void;
pub(crate) type KittyGraphicsImage = *const c_void;
pub(crate) type KittyGraphicsPlacementIterator = *mut c_void;
pub(crate) type SnapshotDecoder = *mut c_void;
pub(crate) type RenderState = *mut c_void;
pub(crate) type RenderStateRowIterator = *mut c_void;
pub(crate) type RenderStateRowCells = *mut c_void;

#[repr(C)]
pub(crate) struct AllocatorVtable {
    pub(crate) alloc: unsafe extern "C" fn(*mut c_void, usize, u8, usize) -> *mut c_void,
    pub(crate) resize:
        unsafe extern "C" fn(*mut c_void, *mut c_void, usize, u8, usize, usize) -> bool,
    pub(crate) remap:
        unsafe extern "C" fn(*mut c_void, *mut c_void, usize, u8, usize, usize) -> *mut c_void,
    pub(crate) free: unsafe extern "C" fn(*mut c_void, *mut c_void, usize, u8, usize),
}

#[repr(C)]
pub(crate) struct Allocator {
    pub(crate) ctx: *mut c_void,
    pub(crate) vtable: *const AllocatorVtable,
}

pub(crate) type TerminalData = c_int;
pub(crate) const TERMINAL_DATA_CURSOR_X: TerminalData = 3;
pub(crate) const TERMINAL_DATA_CURSOR_Y: TerminalData = 4;
pub(crate) const TERMINAL_DATA_ACTIVE_SCREEN: TerminalData = 6;
pub(crate) const TERMINAL_DATA_CURSOR_VISIBLE: TerminalData = 7;
pub(crate) const TERMINAL_DATA_KITTY_KEYBOARD_FLAGS: TerminalData = 8;
pub(crate) const TERMINAL_DATA_CURSOR_STYLE: TerminalData = 10;
pub(crate) const TERMINAL_DATA_TITLE: TerminalData = 12;
pub(crate) const TERMINAL_DATA_PWD: TerminalData = 13;
pub(crate) const TERMINAL_DATA_TOTAL_ROWS: TerminalData = 14;
pub(crate) const TERMINAL_DATA_SCROLLBACK_ROWS: TerminalData = 15;
pub(crate) const TERMINAL_DATA_WIDTH_PX: TerminalData = 16;
pub(crate) const TERMINAL_DATA_HEIGHT_PX: TerminalData = 17;
pub(crate) const TERMINAL_DATA_KITTY_GRAPHICS: TerminalData = 30;
pub(crate) const TERMINAL_DATA_CONTINUATION_MAX_BYTES: TerminalData = 36;
pub(crate) const TERMINAL_DATA_MODE: TerminalData = 37;

pub(crate) type TerminalOption = c_int;
pub(crate) const TERMINAL_OPT_USERDATA: TerminalOption = 0;
pub(crate) const TERMINAL_OPT_WRITE_PTY: TerminalOption = 1;
pub(crate) const TERMINAL_OPT_BELL: TerminalOption = 2;
pub(crate) const TERMINAL_OPT_ENQUIRY: TerminalOption = 3;
pub(crate) const TERMINAL_OPT_XTVERSION: TerminalOption = 4;
pub(crate) const TERMINAL_OPT_TITLE_CHANGED: TerminalOption = 5;
pub(crate) const TERMINAL_OPT_SIZE: TerminalOption = 6;
pub(crate) const TERMINAL_OPT_COLOR_SCHEME: TerminalOption = 7;
pub(crate) const TERMINAL_OPT_DEVICE_ATTRIBUTES: TerminalOption = 8;
pub(crate) const TERMINAL_OPT_KITTY_IMAGE_STORAGE_LIMIT: TerminalOption = 15;
pub(crate) const TERMINAL_OPT_PWD_CHANGED: TerminalOption = 25;
pub(crate) const TERMINAL_OPT_CLIPBOARD_WRITE: TerminalOption = 26;
pub(crate) const TERMINAL_OPT_SCROLLBACK_MAX_BYTES: TerminalOption = 27;
pub(crate) const TERMINAL_OPT_SCROLLBACK_MAX_LINES: TerminalOption = 28;
pub(crate) const TERMINAL_OPT_DESKTOP_NOTIFICATION: TerminalOption = 29;
pub(crate) const TERMINAL_OPT_PROGRESS_REPORT: TerminalOption = 30;
pub(crate) const TERMINAL_OPT_CONTINUATION_MAX_BYTES: TerminalOption = 31;
pub(crate) const TERMINAL_OPT_UNKNOWN_SEQUENCE: TerminalOption = 35;
pub(crate) const TERMINAL_OPT_UNKNOWN_MAX_BYTES: TerminalOption = 36;

pub(crate) type ColorScheme = c_int;
pub(crate) const COLOR_SCHEME_LIGHT: ColorScheme = 0;
pub(crate) const COLOR_SCHEME_DARK: ColorScheme = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SizeReportSize {
    pub(crate) rows: u16,
    pub(crate) columns: u16,
    pub(crate) cell_width: u32,
    pub(crate) cell_height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct DeviceAttributesPrimary {
    pub(crate) conformance_level: u16,
    pub(crate) features: [u16; 64],
    pub(crate) num_features: usize,
}

impl Default for DeviceAttributesPrimary {
    fn default() -> Self {
        Self {
            conformance_level: 0,
            features: [0; 64],
            num_features: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DeviceAttributesSecondary {
    pub(crate) device_type: u16,
    pub(crate) firmware_version: u16,
    pub(crate) rom_cartridge: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DeviceAttributesTertiary {
    pub(crate) unit_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DeviceAttributes {
    pub(crate) primary: DeviceAttributesPrimary,
    pub(crate) secondary: DeviceAttributesSecondary,
    pub(crate) tertiary: DeviceAttributesTertiary,
}

pub(crate) type TerminalScreen = c_int;
pub(crate) const TERMINAL_SCREEN_PRIMARY: TerminalScreen = 0;
pub(crate) const TERMINAL_SCREEN_ALTERNATE: TerminalScreen = 1;

pub(crate) type RenderStateData = c_int;
pub(crate) const RENDER_STATE_DATA_COLS: RenderStateData = 1;
pub(crate) const RENDER_STATE_DATA_ROWS: RenderStateData = 2;
pub(crate) const RENDER_STATE_DATA_DIRTY: RenderStateData = 3;
pub(crate) const RENDER_STATE_DATA_ROW_ITERATOR: RenderStateData = 4;
pub(crate) const RENDER_STATE_DATA_CURSOR_VISUAL_STYLE: RenderStateData = 10;

pub(crate) type RenderStateDirty = c_int;
pub(crate) const RENDER_STATE_DIRTY_FALSE: RenderStateDirty = 0;
pub(crate) const RENDER_STATE_DIRTY_PARTIAL: RenderStateDirty = 1;
pub(crate) const RENDER_STATE_DIRTY_FULL: RenderStateDirty = 2;

pub(crate) type RenderStateOption = c_int;
pub(crate) const RENDER_STATE_OPTION_DIRTY: RenderStateOption = 0;

pub(crate) type RenderCursorVisualStyle = c_int;
pub(crate) const RENDER_CURSOR_VISUAL_STYLE_BAR: RenderCursorVisualStyle = 0;
pub(crate) const RENDER_CURSOR_VISUAL_STYLE_BLOCK: RenderCursorVisualStyle = 1;
pub(crate) const RENDER_CURSOR_VISUAL_STYLE_UNDERLINE: RenderCursorVisualStyle = 2;
pub(crate) const RENDER_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW: RenderCursorVisualStyle = 3;

pub(crate) type RenderStateRowData = c_int;
pub(crate) const RENDER_STATE_ROW_DATA_DIRTY: RenderStateRowData = 1;
pub(crate) const RENDER_STATE_ROW_DATA_RAW: RenderStateRowData = 2;
pub(crate) const RENDER_STATE_ROW_DATA_CELLS: RenderStateRowData = 3;

pub(crate) type RenderStateRowOption = c_int;
pub(crate) const RENDER_STATE_ROW_OPTION_DIRTY: RenderStateRowOption = 0;

pub(crate) type RenderStateRowCellsData = c_int;
pub(crate) const RENDER_STATE_ROW_CELLS_DATA_RAW: RenderStateRowCellsData = 1;
pub(crate) const RENDER_STATE_ROW_CELLS_DATA_STYLE: RenderStateRowCellsData = 2;
pub(crate) const RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_UTF8: RenderStateRowCellsData = 9;

pub(crate) type Cell = u64;
pub(crate) type CellData = c_int;
pub(crate) const CELL_DATA_WIDE: CellData = 3;
pub(crate) const CELL_DATA_HAS_HYPERLINK: CellData = 7;

pub(crate) type CellWide = c_int;
pub(crate) const CELL_WIDE_NARROW: CellWide = 0;
pub(crate) const CELL_WIDE_WIDE: CellWide = 1;
pub(crate) const CELL_WIDE_SPACER_TAIL: CellWide = 2;
pub(crate) const CELL_WIDE_SPACER_HEAD: CellWide = 3;

pub(crate) type Row = u64;
pub(crate) type RowData = c_int;
pub(crate) const ROW_DATA_WRAP: RowData = 1;

pub(crate) type PointTag = c_int;
pub(crate) const POINT_TAG_VIEWPORT: PointTag = 1;
pub(crate) const POINT_TAG_SCREEN: PointTag = 2;
pub(crate) const POINT_TAG_HISTORY: PointTag = 3;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PointCoordinate {
    pub(crate) x: u16,
    pub(crate) _padding: u16,
    pub(crate) y: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) union PointValue {
    pub(crate) coordinate: PointCoordinate,
    pub(crate) _padding: [u64; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Point {
    pub(crate) tag: PointTag,
    pub(crate) value: PointValue,
}

impl Point {
    pub(crate) fn coordinate(tag: PointTag, x: u16, y: u32) -> Self {
        let mut value = PointValue { _padding: [0; 2] };
        value.coordinate = PointCoordinate { x, _padding: 0, y };
        Self { tag, value }
    }
}

#[repr(C)]
pub(crate) struct GridRef {
    pub(crate) size: usize,
    pub(crate) node: *mut c_void,
    pub(crate) x: u16,
    pub(crate) y: u16,
}

pub(crate) type TrackedGridRef = *mut c_void;

pub(crate) type StyleColorTag = c_int;
pub(crate) const STYLE_COLOR_NONE: StyleColorTag = 0;
pub(crate) const STYLE_COLOR_PALETTE: StyleColorTag = 1;
pub(crate) const STYLE_COLOR_RGB: StyleColorTag = 2;

pub(crate) const SGR_UNDERLINE_NONE: c_int = 0;
pub(crate) const SGR_UNDERLINE_SINGLE: c_int = 1;
pub(crate) const SGR_UNDERLINE_DOUBLE: c_int = 2;
pub(crate) const SGR_UNDERLINE_CURLY: c_int = 3;
pub(crate) const SGR_UNDERLINE_DOTTED: c_int = 4;
pub(crate) const SGR_UNDERLINE_DASHED: c_int = 5;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ColorRgb {
    pub(crate) r: u8,
    pub(crate) g: u8,
    pub(crate) b: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) union StyleColorValue {
    pub(crate) palette: u8,
    pub(crate) rgb: ColorRgb,
    pub(crate) _padding: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct StyleColor {
    pub(crate) tag: StyleColorTag,
    pub(crate) value: StyleColorValue,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Style {
    pub(crate) size: usize,
    pub(crate) fg_color: StyleColor,
    pub(crate) bg_color: StyleColor,
    pub(crate) underline_color: StyleColor,
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    pub(crate) faint: bool,
    pub(crate) blink: bool,
    pub(crate) inverse: bool,
    pub(crate) invisible: bool,
    pub(crate) strikethrough: bool,
    pub(crate) overline: bool,
    pub(crate) underline: c_int,
}

impl Style {
    pub(crate) fn init() -> Self {
        // Sized C structs are initialized with their byte size and zeroed
        // trailing fields, per GHOSTTY_INIT_SIZED.
        let mut value: Self = unsafe { std::mem::zeroed() };
        value.size = std::mem::size_of::<Self>();
        value
    }
}

pub(crate) type Mode = u16;
pub(crate) const MODE_APPLICATION_CURSOR: Mode = 1;
pub(crate) const MODE_X10_MOUSE: Mode = 9;
pub(crate) const MODE_APPLICATION_KEYPAD: Mode = 66;
pub(crate) const MODE_NORMAL_MOUSE: Mode = 1000;
pub(crate) const MODE_BUTTON_MOUSE: Mode = 1002;
pub(crate) const MODE_ANY_MOUSE: Mode = 1003;
pub(crate) const MODE_FOCUS_EVENT: Mode = 1004;
pub(crate) const MODE_UTF8_MOUSE: Mode = 1005;
pub(crate) const MODE_SGR_MOUSE: Mode = 1006;
pub(crate) const MODE_BRACKETED_PASTE: Mode = 2004;
pub(crate) const MODE_SYNCHRONIZED_OUTPUT: Mode = 2026;

pub(crate) type KittyGraphicsData = c_int;
pub(crate) const KITTY_GRAPHICS_DATA_PLACEMENT_ITERATOR: KittyGraphicsData = 1;

pub(crate) type KittyGraphicsPlacementData = c_int;
pub(crate) const KITTY_PLACEMENT_DATA_IMAGE_ID: KittyGraphicsPlacementData = 1;
pub(crate) const KITTY_PLACEMENT_DATA_PLACEMENT_ID: KittyGraphicsPlacementData = 2;
pub(crate) const KITTY_PLACEMENT_DATA_IS_VIRTUAL: KittyGraphicsPlacementData = 3;
pub(crate) const KITTY_PLACEMENT_DATA_X_OFFSET: KittyGraphicsPlacementData = 4;
pub(crate) const KITTY_PLACEMENT_DATA_Y_OFFSET: KittyGraphicsPlacementData = 5;
pub(crate) const KITTY_PLACEMENT_DATA_Z: KittyGraphicsPlacementData = 12;

pub(crate) type KittyGraphicsImageData = c_int;
pub(crate) const KITTY_IMAGE_DATA_NUMBER: KittyGraphicsImageData = 2;
pub(crate) const KITTY_IMAGE_DATA_WIDTH: KittyGraphicsImageData = 3;
pub(crate) const KITTY_IMAGE_DATA_HEIGHT: KittyGraphicsImageData = 4;
pub(crate) const KITTY_IMAGE_DATA_FORMAT: KittyGraphicsImageData = 5;
pub(crate) const KITTY_IMAGE_DATA_DATA_PTR: KittyGraphicsImageData = 7;
pub(crate) const KITTY_IMAGE_DATA_DATA_LEN: KittyGraphicsImageData = 8;

pub(crate) type KittyImageFormat = c_int;
pub(crate) const KITTY_IMAGE_FORMAT_RGB: KittyImageFormat = 0;
pub(crate) const KITTY_IMAGE_FORMAT_RGBA: KittyImageFormat = 1;
pub(crate) const KITTY_IMAGE_FORMAT_GRAY_ALPHA: KittyImageFormat = 3;
pub(crate) const KITTY_IMAGE_FORMAT_GRAY: KittyImageFormat = 4;

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct KittyGraphicsPlacementRenderInfo {
    pub(crate) size: usize,
    pub(crate) pixel_width: u32,
    pub(crate) pixel_height: u32,
    pub(crate) grid_cols: u32,
    pub(crate) grid_rows: u32,
    pub(crate) viewport_col: i32,
    pub(crate) viewport_row: i32,
    pub(crate) viewport_visible: bool,
    pub(crate) source_x: u32,
    pub(crate) source_y: u32,
    pub(crate) source_width: u32,
    pub(crate) source_height: u32,
}

impl KittyGraphicsPlacementRenderInfo {
    pub(crate) fn init() -> Self {
        // Sized C structs are initialized with their byte size and zeroed
        // trailing fields, per GHOSTTY_INIT_SIZED.
        let mut value: Self = unsafe { std::mem::zeroed() };
        value.size = std::mem::size_of::<Self>();
        value
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct GhosttyString {
    pub(crate) ptr: *const u8,
    pub(crate) len: usize,
}

pub(crate) type ClipboardLocation = c_int;
pub(crate) const CLIPBOARD_LOCATION_STANDARD: ClipboardLocation = 0;
pub(crate) const CLIPBOARD_LOCATION_SELECTION: ClipboardLocation = 1;
pub(crate) const CLIPBOARD_LOCATION_PRIMARY: ClipboardLocation = 2;

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct ClipboardContent {
    pub(crate) mime: GhosttyString,
    pub(crate) data: GhosttyString,
}

#[repr(C)]
pub(crate) struct ClipboardWrite {
    pub(crate) size: usize,
    pub(crate) location: ClipboardLocation,
    pub(crate) contents: *const ClipboardContent,
    pub(crate) contents_len: usize,
}

pub(crate) type ClipboardWriteResult = c_int;
pub(crate) const CLIPBOARD_WRITE_RESULT_SUCCESS: ClipboardWriteResult = 0;

#[repr(C)]
pub(crate) struct DesktopNotification {
    pub(crate) size: usize,
    pub(crate) title: GhosttyString,
    pub(crate) body: GhosttyString,
}

pub(crate) type ProgressState = c_int;
pub(crate) const PROGRESS_STATE_REMOVE: ProgressState = 0;
pub(crate) const PROGRESS_STATE_SET: ProgressState = 1;
pub(crate) const PROGRESS_STATE_ERROR: ProgressState = 2;
pub(crate) const PROGRESS_STATE_INDETERMINATE: ProgressState = 3;
pub(crate) const PROGRESS_STATE_PAUSE: ProgressState = 4;

#[repr(C)]
pub(crate) struct ProgressReport {
    pub(crate) size: usize,
    pub(crate) state: ProgressState,
    pub(crate) progress: i8,
}

pub(crate) type UnknownSequenceTag = c_int;
pub(crate) const UNKNOWN_SEQUENCE_APC: UnknownSequenceTag = 0;

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct UnknownStringSequence {
    pub(crate) truncated: bool,
    pub(crate) content: GhosttyString,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) union UnknownSequenceValue {
    pub(crate) apc: UnknownStringSequence,
    pub(crate) _padding: [u64; 16],
}

#[repr(C)]
pub(crate) struct UnknownSequence {
    pub(crate) tag: UnknownSequenceTag,
    pub(crate) value: UnknownSequenceValue,
}

const _: () = {
    use std::mem::{offset_of, size_of};

    // All supported libghostty targets are 64-bit. These assertions pair with
    // the C header probe so an upstream layout change cannot silently make the
    // hand-written private declarations unsound.
    assert!(size_of::<usize>() == 8);
    assert!(size_of::<SizeReportSize>() == 12);
    assert!(offset_of!(SizeReportSize, rows) == 0);
    assert!(offset_of!(SizeReportSize, columns) == 2);
    assert!(offset_of!(SizeReportSize, cell_width) == 4);
    assert!(offset_of!(SizeReportSize, cell_height) == 8);
    assert!(size_of::<DeviceAttributesPrimary>() == 144);
    assert!(offset_of!(DeviceAttributesPrimary, conformance_level) == 0);
    assert!(offset_of!(DeviceAttributesPrimary, features) == 2);
    assert!(offset_of!(DeviceAttributesPrimary, num_features) == 136);
    assert!(size_of::<DeviceAttributesSecondary>() == 6);
    assert!(size_of::<DeviceAttributesTertiary>() == 4);
    assert!(size_of::<DeviceAttributes>() == 160);
    assert!(offset_of!(DeviceAttributes, primary) == 0);
    assert!(offset_of!(DeviceAttributes, secondary) == 144);
    assert!(offset_of!(DeviceAttributes, tertiary) == 152);
    assert!(size_of::<ClipboardContent>() == 32);
    assert!(offset_of!(ClipboardContent, mime) == 0);
    assert!(offset_of!(ClipboardContent, data) == 16);
    assert!(size_of::<ClipboardWrite>() == 32);
    assert!(offset_of!(ClipboardWrite, size) == 0);
    assert!(offset_of!(ClipboardWrite, location) == 8);
    assert!(offset_of!(ClipboardWrite, contents) == 16);
    assert!(offset_of!(ClipboardWrite, contents_len) == 24);
    assert!(size_of::<DesktopNotification>() == 40);
    assert!(offset_of!(DesktopNotification, title) == 8);
    assert!(offset_of!(DesktopNotification, body) == 24);
    assert!(size_of::<ProgressReport>() == 16);
    assert!(offset_of!(ProgressReport, state) == 8);
    assert!(offset_of!(ProgressReport, progress) == 12);
    assert!(size_of::<UnknownStringSequence>() == 24);
    assert!(offset_of!(UnknownStringSequence, content) == 8);
    assert!(size_of::<UnknownSequenceValue>() == 128);
    assert!(size_of::<UnknownSequence>() == 136);
    assert!(offset_of!(UnknownSequence, tag) == 0);
    assert!(offset_of!(UnknownSequence, value) == 8);
    assert!(size_of::<KittyGraphicsPlacementRenderInfo>() == 56);
    assert!(offset_of!(KittyGraphicsPlacementRenderInfo, size) == 0);
    assert!(offset_of!(KittyGraphicsPlacementRenderInfo, pixel_width) == 8);
    assert!(offset_of!(KittyGraphicsPlacementRenderInfo, viewport_visible) == 32);
    assert!(offset_of!(KittyGraphicsPlacementRenderInfo, source_x) == 36);
};

#[repr(C)]
pub(crate) struct GhosttyBuffer {
    pub(crate) ptr: *mut u8,
    pub(crate) cap: usize,
    pub(crate) len: usize,
}

#[repr(C)]
pub(crate) struct TerminalModeConfig {
    pub(crate) mode: Mode,
    pub(crate) value: bool,
}

unsafe extern "C" {
    pub(crate) fn ghostty_build_info(data: BuildInfo, out: *mut c_void) -> ResultCode;
    pub(crate) fn ghostty_terminal_new(
        allocator: *const Allocator,
        terminal: *mut Terminal,
        cols: u16,
        rows: u16,
    ) -> ResultCode;
    pub(crate) fn ghostty_terminal_free(terminal: Terminal);
    pub(crate) fn ghostty_terminal_reset(terminal: Terminal);
    pub(crate) fn ghostty_terminal_resize(
        terminal: Terminal,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> ResultCode;
    pub(crate) fn ghostty_terminal_vt_write(terminal: Terminal, data: *const u8, len: usize);
    pub(crate) fn ghostty_terminal_continuation_buf(
        terminal: Terminal,
        buf: *mut u8,
        buf_len: usize,
        out_written: *mut usize,
    ) -> ResultCode;
    pub(crate) fn ghostty_terminal_get(
        terminal: Terminal,
        data: TerminalData,
        out: *mut c_void,
    ) -> ResultCode;
    pub(crate) fn ghostty_terminal_set(
        terminal: Terminal,
        option: TerminalOption,
        value: *const c_void,
    ) -> ResultCode;
    pub(crate) fn ghostty_snapshot_encode_buf(
        terminal: Terminal,
        buf: *mut u8,
        buf_len: usize,
        out_written: *mut usize,
    ) -> ResultCode;
    pub(crate) fn ghostty_snapshot_decoder_new_buf(
        allocator: *const Allocator,
        decoder: *mut SnapshotDecoder,
        ptr: *const u8,
        len: usize,
    ) -> ResultCode;
    pub(crate) fn ghostty_snapshot_decoder_free(decoder: SnapshotDecoder);
    pub(crate) fn ghostty_snapshot_decoder_decode(
        decoder: SnapshotDecoder,
        terminal: *mut Terminal,
    ) -> ResultCode;
    pub(crate) fn ghostty_terminal_grid_ref(
        terminal: Terminal,
        point: Point,
        out_ref: *mut GridRef,
    ) -> ResultCode;
    pub(crate) fn ghostty_terminal_grid_ref_track(
        terminal: Terminal,
        point: Point,
        out_ref: *mut TrackedGridRef,
    ) -> ResultCode;

    pub(crate) fn ghostty_kitty_graphics_get(
        graphics: KittyGraphics,
        data: KittyGraphicsData,
        out: *mut c_void,
    ) -> ResultCode;
    pub(crate) fn ghostty_kitty_graphics_image(
        graphics: KittyGraphics,
        image_id: u32,
    ) -> KittyGraphicsImage;
    pub(crate) fn ghostty_kitty_graphics_image_get(
        image: KittyGraphicsImage,
        data: KittyGraphicsImageData,
        out: *mut c_void,
    ) -> ResultCode;
    pub(crate) fn ghostty_kitty_graphics_placement_iterator_new(
        allocator: *const Allocator,
        iterator: *mut KittyGraphicsPlacementIterator,
    ) -> ResultCode;
    pub(crate) fn ghostty_kitty_graphics_placement_iterator_free(
        iterator: KittyGraphicsPlacementIterator,
    );
    pub(crate) fn ghostty_kitty_graphics_placement_next(
        iterator: KittyGraphicsPlacementIterator,
    ) -> bool;
    pub(crate) fn ghostty_kitty_graphics_placement_get(
        iterator: KittyGraphicsPlacementIterator,
        data: KittyGraphicsPlacementData,
        out: *mut c_void,
    ) -> ResultCode;
    pub(crate) fn ghostty_kitty_graphics_placement_render_info(
        iterator: KittyGraphicsPlacementIterator,
        image: KittyGraphicsImage,
        terminal: Terminal,
        out: *mut KittyGraphicsPlacementRenderInfo,
    ) -> ResultCode;

    pub(crate) fn ghostty_render_state_new(
        allocator: *const Allocator,
        state: *mut RenderState,
    ) -> ResultCode;
    pub(crate) fn ghostty_render_state_free(state: RenderState);
    pub(crate) fn ghostty_render_state_update(state: RenderState, terminal: Terminal)
    -> ResultCode;
    pub(crate) fn ghostty_render_state_get(
        state: RenderState,
        data: RenderStateData,
        out: *mut c_void,
    ) -> ResultCode;
    pub(crate) fn ghostty_render_state_set(
        state: RenderState,
        option: RenderStateOption,
        value: *const c_void,
    ) -> ResultCode;
    pub(crate) fn ghostty_render_state_row_iterator_new(
        allocator: *const Allocator,
        iterator: *mut RenderStateRowIterator,
    ) -> ResultCode;
    pub(crate) fn ghostty_render_state_row_iterator_free(iterator: RenderStateRowIterator);
    pub(crate) fn ghostty_render_state_row_iterator_next(iterator: RenderStateRowIterator) -> bool;
    pub(crate) fn ghostty_render_state_row_get(
        iterator: RenderStateRowIterator,
        data: RenderStateRowData,
        out: *mut c_void,
    ) -> ResultCode;
    pub(crate) fn ghostty_render_state_row_set(
        iterator: RenderStateRowIterator,
        option: RenderStateRowOption,
        value: *const c_void,
    ) -> ResultCode;
    pub(crate) fn ghostty_render_state_row_cells_new(
        allocator: *const Allocator,
        cells: *mut RenderStateRowCells,
    ) -> ResultCode;
    pub(crate) fn ghostty_render_state_row_cells_free(cells: RenderStateRowCells);
    pub(crate) fn ghostty_render_state_row_cells_next(cells: RenderStateRowCells) -> bool;
    pub(crate) fn ghostty_render_state_row_cells_get(
        cells: RenderStateRowCells,
        data: RenderStateRowCellsData,
        out: *mut c_void,
    ) -> ResultCode;

    pub(crate) fn ghostty_cell_get(cell: Cell, data: CellData, out: *mut c_void) -> ResultCode;
    pub(crate) fn ghostty_row_get(row: Row, data: RowData, out: *mut c_void) -> ResultCode;
    pub(crate) fn ghostty_grid_ref_cell(reference: *const GridRef, out: *mut Cell) -> ResultCode;
    pub(crate) fn ghostty_grid_ref_row(reference: *const GridRef, out: *mut Row) -> ResultCode;
    pub(crate) fn ghostty_grid_ref_graphemes(
        reference: *const GridRef,
        buffer: *mut u32,
        buffer_len: usize,
        out_len: *mut usize,
    ) -> ResultCode;
    pub(crate) fn ghostty_grid_ref_hyperlink_uri(
        reference: *const GridRef,
        buffer: *mut u8,
        buffer_len: usize,
        out_len: *mut usize,
    ) -> ResultCode;
    pub(crate) fn ghostty_grid_ref_style(reference: *const GridRef, out: *mut Style) -> ResultCode;
    pub(crate) fn ghostty_tracked_grid_ref_free(reference: TrackedGridRef);
    pub(crate) fn ghostty_tracked_grid_ref_has_value(reference: TrackedGridRef) -> bool;
    pub(crate) fn ghostty_tracked_grid_ref_point(
        reference: TrackedGridRef,
        tag: PointTag,
        out: *mut PointCoordinate,
    ) -> ResultCode;
}
