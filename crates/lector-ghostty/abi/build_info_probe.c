#include <stddef.h>

#include <ghostty/vt/build_info.h>
#include <ghostty/vt/grid_ref.h>
#include <ghostty/vt/grid_ref_tracked.h>
#include <ghostty/vt/kitty_graphics.h>
#include <ghostty/vt/point.h>
#include <ghostty/vt/render.h>
#include <ghostty/vt/screen.h>
#include <ghostty/vt/sgr.h>
#include <ghostty/vt/snapshot.h>
#include <ghostty/vt/style.h>
#include <ghostty/vt/terminal.h>

_Static_assert(sizeof(int) == 4, "Lector's Rust adapter requires a 32-bit c_int");
_Static_assert(sizeof(GhosttyResult) == sizeof(int),
               "GhosttyResult must use the documented c_int ABI");
_Static_assert(sizeof(GhosttyBuildInfo) == sizeof(int),
               "GhosttyBuildInfo must use the documented c_int ABI");
_Static_assert(sizeof(GhosttyOptimizeMode) == sizeof(int),
               "GhosttyOptimizeMode must use the documented c_int ABI");
_Static_assert(sizeof(GhosttyString) == 2 * sizeof(size_t),
               "GhosttyString must remain a pointer-length pair");
_Static_assert(_Alignof(GhosttyString) == _Alignof(size_t),
               "GhosttyString alignment changed");
_Static_assert(offsetof(GhosttyString, ptr) == 0,
               "GhosttyString.ptr offset changed");
_Static_assert(offsetof(GhosttyString, len) == sizeof(size_t),
               "GhosttyString.len offset changed");
_Static_assert(sizeof(GhosttyBuffer) == 3 * sizeof(size_t),
               "GhosttyBuffer layout changed");
_Static_assert(offsetof(GhosttyBuffer, ptr) == 0,
               "GhosttyBuffer.ptr offset changed");
_Static_assert(offsetof(GhosttyBuffer, cap) == sizeof(size_t),
               "GhosttyBuffer.cap offset changed");
_Static_assert(offsetof(GhosttyBuffer, len) == 2 * sizeof(size_t),
               "GhosttyBuffer.len offset changed");
_Static_assert(sizeof(GhosttyTerminalModeConfig) == 4,
               "GhosttyTerminalModeConfig layout changed");
_Static_assert(GHOSTTY_COLOR_SCHEME_LIGHT == 0 &&
                   GHOSTTY_COLOR_SCHEME_DARK == 1,
               "GhosttyColorScheme values changed");
_Static_assert(GHOSTTY_RENDER_STATE_DIRTY_FALSE == 0 &&
                   GHOSTTY_RENDER_STATE_DIRTY_PARTIAL == 1 &&
                   GHOSTTY_RENDER_STATE_DIRTY_FULL == 2 &&
                   GHOSTTY_RENDER_STATE_DATA_DIRTY == 3 &&
                   GHOSTTY_RENDER_STATE_OPTION_DIRTY == 0 &&
                   GHOSTTY_RENDER_STATE_ROW_DATA_DIRTY == 1 &&
                   GHOSTTY_RENDER_STATE_ROW_OPTION_DIRTY == 0,
               "Ghostty render damage values changed");
_Static_assert(sizeof(GhosttySizeReportSize) == 12,
               "GhosttySizeReportSize layout changed");
_Static_assert(offsetof(GhosttySizeReportSize, rows) == 0 &&
                   offsetof(GhosttySizeReportSize, columns) == 2 &&
                   offsetof(GhosttySizeReportSize, cell_width) == 4 &&
                   offsetof(GhosttySizeReportSize, cell_height) == 8,
               "GhosttySizeReportSize fields changed");
_Static_assert(sizeof(GhosttyDeviceAttributesPrimary) == 144 &&
                   offsetof(GhosttyDeviceAttributesPrimary, features) == 2 &&
                   offsetof(GhosttyDeviceAttributesPrimary, num_features) == 136,
               "GhosttyDeviceAttributesPrimary layout changed");
_Static_assert(sizeof(GhosttyDeviceAttributesSecondary) == 6 &&
                   sizeof(GhosttyDeviceAttributesTertiary) == 4 &&
                   sizeof(GhosttyDeviceAttributes) == 160,
               "GhosttyDeviceAttributes layout changed");
_Static_assert(offsetof(GhosttyDeviceAttributes, secondary) == 144 &&
                   offsetof(GhosttyDeviceAttributes, tertiary) == 152,
               "GhosttyDeviceAttributes fields changed");
_Static_assert(offsetof(GhosttyTerminalModeConfig, mode) == 0,
               "GhosttyTerminalModeConfig.mode offset changed");
_Static_assert(offsetof(GhosttyTerminalModeConfig, value) == 2,
               "GhosttyTerminalModeConfig.value offset changed");
_Static_assert(sizeof(GhosttyClipboardContent) == 4 * sizeof(size_t),
               "GhosttyClipboardContent layout changed");
_Static_assert(offsetof(GhosttyClipboardContent, mime) == 0,
               "GhosttyClipboardContent.mime offset changed");
_Static_assert(offsetof(GhosttyClipboardContent, data) == 2 * sizeof(size_t),
               "GhosttyClipboardContent.data offset changed");
_Static_assert(sizeof(GhosttyClipboardWrite) == 4 * sizeof(size_t),
               "GhosttyClipboardWrite layout changed");
_Static_assert(offsetof(GhosttyClipboardWrite, size) == 0,
               "GhosttyClipboardWrite.size offset changed");
_Static_assert(offsetof(GhosttyClipboardWrite, location) == sizeof(size_t),
               "GhosttyClipboardWrite.location offset changed");
_Static_assert(offsetof(GhosttyClipboardWrite, contents) == 2 * sizeof(size_t),
               "GhosttyClipboardWrite.contents offset changed");
_Static_assert(offsetof(GhosttyClipboardWrite, contents_len) ==
                   3 * sizeof(size_t),
               "GhosttyClipboardWrite.contents_len offset changed");
_Static_assert(sizeof(GhosttyTerminalDesktopNotification) == 5 * sizeof(size_t),
               "GhosttyTerminalDesktopNotification layout changed");
_Static_assert(offsetof(GhosttyTerminalDesktopNotification, title) ==
                   sizeof(size_t),
               "GhosttyTerminalDesktopNotification.title offset changed");
_Static_assert(offsetof(GhosttyTerminalDesktopNotification, body) ==
                   3 * sizeof(size_t),
               "GhosttyTerminalDesktopNotification.body offset changed");
_Static_assert(offsetof(GhosttyTerminalProgressReport, state) == sizeof(size_t),
               "GhosttyTerminalProgressReport.state offset changed");
_Static_assert(offsetof(GhosttyTerminalProgressReport, progress) ==
                   sizeof(size_t) + sizeof(int),
               "GhosttyTerminalProgressReport.progress offset changed");
_Static_assert(sizeof(GhosttyTerminalUnknownSequenceValue) == 128,
               "GhosttyTerminalUnknownSequenceValue layout changed");
_Static_assert(offsetof(GhosttyTerminalUnknownStringSequence, content) ==
                   sizeof(size_t),
               "GhosttyTerminalUnknownStringSequence.content offset changed");
_Static_assert(offsetof(GhosttyTerminalUnknownSequence, tag) == 0,
               "GhosttyTerminalUnknownSequence.tag offset changed");
_Static_assert(sizeof(GhosttyCell) == sizeof(uint64_t),
               "GhosttyCell layout changed");
_Static_assert(sizeof(GhosttyRow) == sizeof(uint64_t),
               "GhosttyRow layout changed");
_Static_assert(sizeof(GhosttyMode) == sizeof(uint16_t),
               "GhosttyMode layout changed");
_Static_assert(sizeof(GhosttyPointCoordinate) == 8,
               "GhosttyPointCoordinate layout changed");
_Static_assert(offsetof(GhosttyPointCoordinate, x) == 0,
               "GhosttyPointCoordinate.x offset changed");
_Static_assert(offsetof(GhosttyPointCoordinate, y) == 4,
               "GhosttyPointCoordinate.y offset changed");
_Static_assert(sizeof(GhosttyPointValue) == 16,
               "GhosttyPointValue layout changed");
_Static_assert(offsetof(GhosttyGridRef, size) == 0,
               "GhosttyGridRef.size offset changed");
_Static_assert(offsetof(GhosttyGridRef, node) == sizeof(size_t),
               "GhosttyGridRef.node offset changed");
_Static_assert(sizeof(GhosttyColorRgb) == 3,
               "GhosttyColorRgb layout changed");
_Static_assert(sizeof(GhosttyStyleColorValue) == sizeof(uint64_t),
               "GhosttyStyleColorValue layout changed");
_Static_assert(offsetof(GhosttyStyle, size) == 0,
               "GhosttyStyle.size offset changed");
_Static_assert(offsetof(GhosttyStyle, fg_color) == sizeof(size_t),
               "GhosttyStyle.fg_color offset changed");
_Static_assert(offsetof(GhosttyKittyGraphicsPlacementRenderInfo, size) == 0,
               "GhosttyKittyGraphicsPlacementRenderInfo.size offset changed");
_Static_assert(offsetof(GhosttyKittyGraphicsPlacementRenderInfo, pixel_width) ==
                   sizeof(size_t),
               "GhosttyKittyGraphicsPlacementRenderInfo.pixel_width offset changed");
_Static_assert(offsetof(GhosttyKittyGraphicsPlacementRenderInfo,
                        viewport_visible) == 32,
               "GhosttyKittyGraphicsPlacementRenderInfo.viewport_visible offset changed");
_Static_assert(offsetof(GhosttyKittyGraphicsPlacementRenderInfo, source_x) ==
                   36,
               "GhosttyKittyGraphicsPlacementRenderInfo.source_x offset changed");
#if SIZE_MAX == UINT64_MAX
_Static_assert(sizeof(GhosttyTerminalProgressReport) == 16,
               "GhosttyTerminalProgressReport layout changed");
_Static_assert(sizeof(GhosttyTerminalUnknownStringSequence) == 24,
               "GhosttyTerminalUnknownStringSequence layout changed");
_Static_assert(offsetof(GhosttyTerminalUnknownSequence, value) == 8,
               "GhosttyTerminalUnknownSequence.value offset changed");
_Static_assert(sizeof(GhosttyTerminalUnknownSequence) == 136,
               "GhosttyTerminalUnknownSequence layout changed");
_Static_assert(sizeof(GhosttyPoint) == 24, "GhosttyPoint layout changed");
_Static_assert(offsetof(GhosttyPoint, value) == 8,
               "GhosttyPoint.value offset changed");
_Static_assert(sizeof(GhosttyGridRef) == 24,
               "GhosttyGridRef layout changed");
_Static_assert(sizeof(GhosttyStyleColor) == 16,
               "GhosttyStyleColor layout changed");
_Static_assert(sizeof(GhosttyStyle) == 72, "GhosttyStyle layout changed");
_Static_assert(sizeof(GhosttyKittyGraphicsPlacementRenderInfo) == 56,
               "GhosttyKittyGraphicsPlacementRenderInfo layout changed");
#endif

_Static_assert(GHOSTTY_SUCCESS == 0, "GhosttyResult values changed");
_Static_assert(GHOSTTY_LIMIT_EXCEEDED == -6, "GhosttyResult values changed");
_Static_assert(GHOSTTY_OPTIMIZE_DEBUG == 0,
               "GhosttyOptimizeMode values changed");
_Static_assert(GHOSTTY_OPTIMIZE_RELEASE_FAST == 3,
               "GhosttyOptimizeMode values changed");
_Static_assert(GHOSTTY_BUILD_INFO_SIMD == 1,
               "GhosttyBuildInfo values changed");
_Static_assert(GHOSTTY_BUILD_INFO_VERSION_BUILD == 10,
               "GhosttyBuildInfo values changed");
_Static_assert(GHOSTTY_TERMINAL_DATA_CURSOR_X == 3,
               "GhosttyTerminalData values changed");
_Static_assert(GHOSTTY_TERMINAL_DATA_CURSOR_STYLE == 10,
               "GhosttyTerminalData cursor style changed");
_Static_assert(GHOSTTY_RENDER_STATE_DATA_CURSOR_VISUAL_STYLE == 10 &&
                   GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR == 0 &&
                   GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK == 1 &&
                   GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE == 2 &&
                   GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW == 3,
               "Ghostty render cursor visual style values changed");
_Static_assert(GHOSTTY_TERMINAL_DATA_KITTY_KEYBOARD_FLAGS == 8 &&
                   GHOSTTY_TERMINAL_DATA_TITLE == 12 &&
                   GHOSTTY_TERMINAL_DATA_PWD == 13,
               "GhosttyTerminalData effect values changed");
_Static_assert(GHOSTTY_TERMINAL_DATA_MODE == 37,
               "GhosttyTerminalData values changed");
_Static_assert(GHOSTTY_TERMINAL_DATA_SCROLLBACK_ROWS == 15,
               "GhosttyTerminalData scrollback value changed");
_Static_assert(GHOSTTY_TERMINAL_DATA_WIDTH_PX == 16 &&
                   GHOSTTY_TERMINAL_DATA_HEIGHT_PX == 17 &&
                   GHOSTTY_TERMINAL_DATA_CONTINUATION_MAX_BYTES == 36,
               "GhosttyTerminalData geometry/continuation values changed");
_Static_assert(GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS == 30,
               "GhosttyTerminalData Kitty graphics value changed");
_Static_assert(GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_STORAGE_LIMIT == 15,
               "GhosttyTerminalOption Kitty graphics value changed");
_Static_assert(GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES == 28,
               "GhosttyTerminalOption values changed");
_Static_assert(GHOSTTY_TERMINAL_OPT_CONTINUATION_MAX_BYTES == 31,
               "GhosttyTerminalOption continuation value changed");
_Static_assert(GHOSTTY_SNAPSHOT_DECODER_OPT_MAX_CONTINUATION_BYTES == 0,
               "GhosttySnapshotDecoderOption values changed");
_Static_assert(GHOSTTY_SNAPSHOT_DECODER_DATA_SOURCE_OFFSET == 2,
               "GhosttySnapshotDecoderData values changed");
_Static_assert(GHOSTTY_TERMINAL_OPT_USERDATA == 0 &&
                   GHOSTTY_TERMINAL_OPT_WRITE_PTY == 1 &&
                   GHOSTTY_TERMINAL_OPT_BELL == 2 &&
                   GHOSTTY_TERMINAL_OPT_ENQUIRY == 3 &&
                   GHOSTTY_TERMINAL_OPT_XTVERSION == 4 &&
                   GHOSTTY_TERMINAL_OPT_TITLE_CHANGED == 5 &&
                   GHOSTTY_TERMINAL_OPT_SIZE == 6 &&
                   GHOSTTY_TERMINAL_OPT_COLOR_SCHEME == 7 &&
                   GHOSTTY_TERMINAL_OPT_DEVICE_ATTRIBUTES == 8,
               "GhosttyTerminalOption core effect values changed");
_Static_assert(GHOSTTY_TERMINAL_OPT_PWD_CHANGED == 25 &&
                   GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE == 26 &&
                   GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION == 29 &&
                   GHOSTTY_TERMINAL_OPT_PROGRESS_REPORT == 30 &&
                   GHOSTTY_TERMINAL_OPT_UNKNOWN_SEQUENCE == 35 &&
                   GHOSTTY_TERMINAL_OPT_UNKNOWN_MAX_BYTES == 36,
               "GhosttyTerminalOption effect values changed");
_Static_assert(GHOSTTY_TERMINAL_UNKNOWN_SEQUENCE_APC == 0,
               "GhosttyTerminalUnknownSequenceTag values changed");
_Static_assert(GHOSTTY_CLIPBOARD_LOCATION_STANDARD == 0 &&
                   GHOSTTY_CLIPBOARD_LOCATION_SELECTION == 1 &&
                   GHOSTTY_CLIPBOARD_LOCATION_PRIMARY == 2,
               "GhosttyClipboardLocation values changed");
_Static_assert(GHOSTTY_TERMINAL_PROGRESS_STATE_REMOVE == 0 &&
                   GHOSTTY_TERMINAL_PROGRESS_STATE_PAUSE == 4,
               "GhosttyTerminalProgressState values changed");
_Static_assert(GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR == 4,
               "GhosttyRenderStateData values changed");
_Static_assert(GHOSTTY_RENDER_STATE_ROW_DATA_CELLS == 3,
               "GhosttyRenderStateRowData values changed");
_Static_assert(GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_UTF8 == 9,
               "GhosttyRenderStateRowCellsData values changed");
_Static_assert(GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE == 2,
               "GhosttyRenderStateRowCellsData style value changed");
_Static_assert(GHOSTTY_CELL_DATA_WIDE == 3,
               "GhosttyCellData values changed");
_Static_assert(GHOSTTY_CELL_WIDE_SPACER_HEAD == 3,
               "GhosttyCellWide values changed");
_Static_assert(GHOSTTY_CELL_DATA_HAS_HYPERLINK == 7,
               "GhosttyCellData hyperlink value changed");
_Static_assert(GHOSTTY_ROW_DATA_WRAP == 1,
               "GhosttyRowData values changed");
_Static_assert(GHOSTTY_POINT_TAG_HISTORY == 3,
               "GhosttyPointTag values changed");
_Static_assert(GHOSTTY_STYLE_COLOR_RGB == 2,
               "GhosttyStyleColorTag values changed");
_Static_assert(GHOSTTY_SGR_UNDERLINE_NONE == 0 &&
                   GHOSTTY_SGR_UNDERLINE_SINGLE == 1 &&
                   GHOSTTY_SGR_UNDERLINE_DOUBLE == 2 &&
                   GHOSTTY_SGR_UNDERLINE_CURLY == 3 &&
                   GHOSTTY_SGR_UNDERLINE_DOTTED == 4 &&
                   GHOSTTY_SGR_UNDERLINE_DASHED == 5,
               "GhosttySgrUnderline values changed");
_Static_assert(GHOSTTY_KITTY_GRAPHICS_DATA_PLACEMENT_ITERATOR == 1 &&
                   GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_IMAGE_ID == 1 &&
                   GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_PLACEMENT_ID == 2 &&
                   GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_IS_VIRTUAL == 3 &&
                   GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_X_OFFSET == 4 &&
                   GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_Y_OFFSET == 5 &&
                   GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_Z == 12,
               "Ghostty Kitty placement data values changed");
_Static_assert(GHOSTTY_KITTY_IMAGE_DATA_NUMBER == 2 &&
                   GHOSTTY_KITTY_IMAGE_DATA_WIDTH == 3 &&
                   GHOSTTY_KITTY_IMAGE_DATA_HEIGHT == 4 &&
                   GHOSTTY_KITTY_IMAGE_DATA_FORMAT == 5 &&
                   GHOSTTY_KITTY_IMAGE_DATA_DATA_PTR == 7 &&
                   GHOSTTY_KITTY_IMAGE_DATA_DATA_LEN == 8,
               "Ghostty Kitty image data values changed");
