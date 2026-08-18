# Terminal characterization fixtures

These fixtures describe terminal behavior in engine-independent terms. JSON
snapshots contain logical cells and graphemes, cell widths, styles, wrapping,
cursor state, primary/alternate screen identity, retained scrollback, exposed
input modes, title metadata, and OSC 133 semantic marks. Trailing default cells
are omitted; the declared terminal size preserves their extent.

Normalized fixture snapshots record hyperlinks as `null`. Separate compositor
characterization verifies titles, OSC 8 links, Kitty keyboard and mouse modes,
semantic marks, and bells through the modeled render path.

Every snapshot comparison and compositor-oracle assertion writes a JSON
failure artifact under `target/terminal-test-artifacts/`. An artifact contains
the source bytes, PTY chunk boundaries, intended scene, emitted bytes, expected
normalized state, and the headless oracle's resulting normalized state.

The accessibility suite supplies the rest of the behavioral baseline:
review and frozen-review behavior in `tests/app.rs` and `src/views/review.rs`,
scrollback copying and clipboard history in `src/view.rs` and
`src/commands/clipboard.rs`, table behavior in `tests/scripts/table_*.txt`, Lua
overlays/hooks in `tests/app.rs` and `src/lua/mod.rs`, and auto-read, deletion,
key-echo suppression, cursor tracking, focus, and semantic-history speech in
`tests/app.rs` and `src/screen_reader.rs`.
