# tmux control-mode parser

`lector::tmux_control::TmuxControlParser` is a standalone, streaming parser for
one framed `tmux -CC` connection. It has no dependency on Lector's application,
scene, accessibility, or future tmux-topology models.

## Framing and records

The parser requires the opening `ESC P 1000 p` DCS marker and the final `ESC \\`
ST marker. Calls to `push` may end after any byte, including inside either
marker, a CRLF pair, a number, or an octal escape. A successful `finish` requires
the complete ST marker and no trailing bytes.

The event mapping is:

| Wire record | Event |
| --- | --- |
| DCS marker | `Started` |
| `%begin` through matching `%end` | successful `Command` |
| `%begin` through matching `%error` | failed `Command` |
| `%output` | binary `Output` scoped to a numeric pane ID |
| `%extended-output` | binary `ExtendedOutput` with age and preserved future fields |
| `%pause`, `%continue` | pane-scoped flow-control events |
| `%exit` | `Exit` with an optional byte reason |
| any other valid `%name` record | topology-independent `Notification` |
| ST marker | `Ended` |

tmux guarantees that asynchronous notifications do not occur inside a command
output block. Accordingly, every line remains command output until an `%end`
or `%error` whose timestamp, command number, and flags match the opening
`%begin`. A block-looking line with any other tag is payload, which is required
when `capture-pane` contains the transcript of a nested control client.
Notifications between consecutive command blocks remain independent events.

Pane output is decoded from tmux's three-digit octal escapes into `Vec<u8>`.
Neither pane bytes, command output, notification arguments, nor exit reasons are
required to be UTF-8. A decoded nested `ESC P 1000 p` remains pane data and
cannot alter the outer parser state.

## Bounds and failure behavior

Default retained-memory limits are:

- 64 KiB for one control line;
- 64 KiB for one notification;
- 4 MiB of payload across one command reply;
- 65,536 lines across one command reply, independently bounding empty-line
  metadata.

All limits are configurable through `ParserLimits`. Numeric overflow, malformed
pane IDs, unexpected terminators outside a command, invalid or out-of-range
octal escapes, missing required fields, over-limit input, malformed framing,
and every unterminated state return a classified `ControlParseError`. An error
poisons the parser so partially retained state cannot be reused accidentally.
`reset` explicitly discards that state and returns to the start-marker boundary.

The parser intentionally does not detect the marker inside an ordinary PTY byte
stream or preserve bytes before and after a connection. Those source-boundary
responsibilities belong to `TmuxGatewayRouter`, documented in
[`tmux-gateway.md`](tmux-gateway.md).
