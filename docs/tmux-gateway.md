# Direct tmux gateway routing

`TmuxGatewayRouter` is the source boundary between an ordinary direct PTY and
one top-level `tmux -CC` control stream. Lector does not launch or configure
tmux. A user may run `tmux -CC` directly or through any
shell function or script.

## Exact byte ownership

While the source is ordinary terminal output, the router retains at most the
seven-byte prefix of `ESC P 1000 p`. A mismatch releases every retained byte to
the gateway terminal in its original order, including overlapping marker
lookalikes. Completing the marker emits a stable connection ID and moves all
subsequent bytes into `TmuxControlParser`; none of the marker or control records
reach the gateway Ghostty engine or the physical output scheduler.

The control parser is advanced until it emits `Ended` for `ESC \\`. Bytes later
in the same PTY read immediately return to direct routing, so a clean tmux exit
may be followed by the parent shell's prompt without losing or reclassifying
either side of the boundary.

The router has explicit direct, control, and awaiting-terminator lifecycle
states. `%exit` starts a one-second bounded wait for DCS ST. Missing `%exit`, a
missing or timed-out ST, malformed records, and transport EOF produce typed
connection failures and reset the router instead of poisoning Lector. A
non-protocol line which reveals that SSH or another nested launcher died is
replayed through the preserved parent pane parser; malformed `%` records remain
private protocol data. Repeated EOF, timeout, pane removal, and descendant
cleanup calls are idempotent.

## Connection and portal surfaces

The first detected top-level connection is assigned ID 1 and focused
automatically. During the short interval before topology and pane rendering
are available, its active surface explicitly says that the tmux connection is
active and pane presentation is unavailable.

The gateway is retained behind that surface as a read-only portal. It never
contains protocol records, never writes input to the control PTY, and offers no
action for reviewing the pre-tmux gateway screen. Its text explains that tmux
control mode is running and that Enter returns to the active connection.
`App::show_tmux_gateway` supplies the navigation boundary used by connection
management; Enter reconstructs and focuses the connection surface.

On clean `%exit` plus ST, Lector removes the connection and both temporary
surfaces, then resumes the original gateway engine before processing any bytes
after ST. Ordinary terminal input, including `C-a`, is unchanged before a
connection starts. Abrupt failure removes the same state, returns focus to the
nearest live parent (or terminal mode), and announces both the reason and that
resulting location.

## Regression harness

`tests/tmux_gateway.rs` covers one-shot, every-boundary, and bytewise routing;
marker lookalikes; arbitrary launcher input; startup command errors; immediate
exit; same-read pre-marker and post-ST bytes; portal navigation; scheduled
physical output; and final state through a second Ghostty render oracle. Its
real PTY test starts an isolated disposable tmux server with an explicit socket
under `target/test-tmux`, captures an actual `-CC` lifecycle, and replays it one
byte at a time through `App`. The test requires `tmux` on `PATH` and permission
to create a local Unix socket.

`tests/tmux_recovery.rs` cuts EOF at every byte of a complete stream and covers
clean and incomplete endings, malformed protocol, reconstructed shell output,
timeouts, repeated cleanup, parent-pane loss, scoped exceptional writes, write
faults, and spoken recovery context. Its real harness kills an isolated tmux
server, removes the observed final ST to inject abrupt nested transport loss,
and proves the preserved parent pane resumes after the bounded timeout.
