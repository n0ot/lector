# Terminal characterization fixtures

These fixtures describe terminal behavior in engine-independent terms. JSON
snapshots contain logical cells and graphemes, cell widths, styles, wrapping,
cursor state, primary/alternate screen identity, retained scrollback, exposed
input modes, title metadata, and OSC 133 semantic marks. Trailing default cells
are omitted; the declared terminal size preserves their extent.

`vt100` 0.16 does not expose OSC 8 link targets or terminal titles. The
normalized cell field therefore records the current link limitation as `null`,
while the raw-presentation characterization separately verifies that title,
OSC 8, Kitty keyboard, mouse, and bell bytes currently reach the physical
terminal intact.

Every snapshot comparison and raw-presentation oracle assertion writes a JSON
failure artifact under `target/terminal-test-artifacts/`. An artifact contains
the source bytes, PTY chunk boundaries, intended scene, emitted bytes, expected
normalized state, and the headless oracle's resulting normalized state.

The pre-existing accessibility suite supplies the rest of Stop 0.1's baseline:
review and frozen-review behavior in `tests/app.rs` and `src/views/review.rs`,
scrollback copying and clipboard history in `src/view.rs` and
`src/commands/clipboard.rs`, table behavior in `tests/scripts/table_*.txt`, Lua
overlays/hooks in `tests/app.rs` and `src/lua/mod.rs`, and auto-read, deletion,
key-echo suppression, cursor tracking, focus, and semantic-history speech in
`tests/app.rs` and `src/screen_reader.rs`.
