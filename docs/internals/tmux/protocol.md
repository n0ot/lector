# Protocol: detecting and parsing control mode

Two components turn a byte stream into tmux control events:

- `TmuxGatewayRouter` watches ordinary terminal output for the start of a
  control stream and decides which bytes belong to which side.
- `TmuxControlParser` parses the control stream itself.

## The gateway router

A control stream starts with the marker `ESC P 1000 p` and ends with
`ESC \`. Everything between is protocol. Everything outside is ordinary
terminal output from the shell that launched tmux, called the *gateway*.

### Exact byte ownership

While reading ordinary output, the router holds back at most the seven bytes
that could be the start of a marker. If they turn out not to be one, every
held byte is released to the gateway terminal in its original order.

When the marker completes, the router creates a connection with a stable ID
and sends all following bytes to the parser. No marker or protocol byte ever
reaches the gateway's terminal engine or the screen.

When the parser reports the end marker, the remaining bytes of the same read
go back to ordinary routing. A clean tmux exit can therefore be followed
directly by the shell's prompt, with nothing lost on either side.

### Lifecycle and failures

The router is in one of three states: direct, control, or awaiting the end
marker.

`%exit` starts a one-second wait for the end marker. These are typed
connection failures:

- no `%exit` before the stream ends;
- a missing or late end marker;
- a malformed record; and
- end of file on the transport.

A failure resets that connection. It does not poison Lector. Cleanup calls
are idempotent.

After a malformed record, the bytes that follow are quarantined. They are
never replayed as terminal output, and Lector never sends a byte to try to
terminate whatever is on the other end. The quarantine ends when a complete
ordinary line shows that SSH, or whatever launched tmux, has exited. That
line is then replayed through the parent pane's parser.

Silence is not a failure. A connection can sit idle for hours, including
while the computer sleeps.

### Surfaces

The first top-level connection gets ID 1 and focus. Until its topology and
panes are ready, its surface says that the connection is active and that
panes are not available yet.

The gateway shell stays behind a read-only *portal*. The portal explains that
control mode is running and that Enter returns to the connection. It never
shows protocol records and never sends input to the control PTY.

On a clean exit, Lector removes the connection and its surfaces, then resumes
the gateway terminal before processing any bytes after the end marker. On a
failure it removes the same state, returns focus to the nearest live parent
or to the terminal, and announces why and where you are now.

## The control parser

`lector::tmux_control::TmuxControlParser` is a standalone streaming parser
for one connection. It knows nothing about Lector's application, scene, or
topology.

`push` can be called with input split at any byte, including inside a marker,
a CRLF pair, a number, or an escape. `finish` succeeds only after the
complete end marker, with no trailing bytes.

### Events

| Wire record | Event |
| --- | --- |
| Start marker | `Started` |
| `%begin` to matching `%end` | Successful `Command` |
| `%begin` to matching `%error` | Failed `Command` |
| `%output` | `Output` for a numeric pane ID |
| `%extended-output` | `ExtendedOutput`, with age and any future fields |
| `%pause`, `%continue` | Flow-control events for a pane |
| `%exit` | `Exit`, with an optional reason |
| Any other valid `%name` | `Notification` |
| End marker | `Ended` |

### Command blocks

tmux never sends notifications inside a command's output. So after `%begin`,
every line is command output until an `%end` or `%error` whose timestamp,
command number, and flags all match.

A line that merely looks like a block marker is payload. This matters when
`capture-pane` returns the transcript of a nested control client.

### Binary data

Pane output is decoded from tmux's three-digit octal escapes into bytes.
Nothing is assumed to be UTF-8: not pane bytes, command output, notification
arguments, or exit reasons.

A start marker that appears in decoded pane data is just pane data. It cannot
affect the outer parser. Nested connections are detected separately, in the
decoded stream. See [nested connections](prefix.md#nested-connections).

### Errors

The parser's limits are in the [limits table](README.md#limits) and are
configurable through `ParserLimits`.

All of these return a classified `ControlParseError`: numeric overflow, a
malformed pane ID, a terminator outside a command, an invalid octal escape, a
missing field, input over a limit, malformed framing, and any unterminated
state.

An error poisons the parser, so half-parsed state cannot be reused by
accident. `reset` discards it and returns to waiting for a start marker.
