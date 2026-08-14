# tmux control-mode parser fixtures

These fixtures are protocol streams, not terminal presentation recordings. The
JSON string decodes to the exact bytes supplied to `TmuxControlParser`, including
the opening DCS marker and closing ST marker.

- `documented.json` starts with the `%begin` example from the tmux 3.7b
  `CONTROL MODE` manual and adds one instance of each typed record handled at
  Stop 3.1. Its `%output` payload covers NUL-adjacent bytes, backslash, and
  `0xff` after octal decoding.
- `local-tmux-3.7b.json` was captured on macOS on 2026-08-14 from an isolated
  local `tmux -CC` client running under a PTY. It preserves the PTY's CRLF line
  endings, startup command reply, session notifications, pane output, exit, and
  ST terminator.

`tests/tmux_control_parser.rs` replays both fixtures in one chunk, with a split
at every byte boundary, and one byte per parser call. This makes every protocol
line and both framing markers independent of PTY read boundaries.
