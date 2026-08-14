# tmux prefix discovery and emulation

Lector owns the prefix state while a ready tmux control connection is visible.
It does not assume `C-b`, `C-a`, zero-based windows, or Emacs keys. Every full
inventory transaction asks the active tmux server for `prefix`, `prefix2`,
`base-index`, `pane-base-index`, `mode-keys`, `repeat-time`, and the complete
`prefix` table using a machine-oriented format:

```text
B<TAB>#{key_string}<TAB>#{key_repeat}<TAB>#{key_command}
```

The option and binding records are published transactionally with sessions,
windows, and panes. A reload therefore replaces the whole table rather than
mixing old and new bindings. Malformed, duplicate, oversized, non-UTF-8, or
control-byte-bearing records reject the inventory without exposing a partial
configuration. A completed `source-file`, bind/unbind, or option-setting
binding automatically requests a fresh inventory even when tmux reports an
error, because a source file can apply early lines before a later line fails.
Configuration reloads therefore do not depend on an unrelated topology
notification.

## Prefix state

Outside tmux mode, input remains unchanged; in particular, `C-a` is still sent
to an ordinary terminal application. In tmux mode, Lector consumes the
discovered primary or secondary prefix and waits up to one second for a table
key. Escape cancels. An unbound key is consumed and announced, matching tmux's
prefix ownership rather than leaking the key to the pane.

After a repeatable binding, only another repeatable table key received within
the server's configured `repeat-time` remains in prefix-repeat state. Any
unrelated or late key leaves repeat state and is processed normally, so typing
cannot become trapped. Pending prefix state is stored per connection, so
incomplete prefixes on two connections cannot consume one another's keys and
each can resume independently until its own deadline. Entering a connection's
portal clears that connection's prefix; transport shutdown drops the state with
its owning connection. Confirmation overlays remain bound to the stable
connection and target IDs they captured.

## Commands and accessibility boundaries

Every ordinary discovered command, including tmux command chains and quoted
formats, is sent unchanged through the connection's ordered control FIFO.
Bindings containing a NUL, carriage return, or newline are rejected before a
newline terminator is added, preventing one discovered record from injecting a
second control command. Command replies retain an explicit type; an error opens
a reviewable Lector popup and cannot be mistaken for inventory or pane capture.

Lector classifies commands that require local accessible interaction:

- `confirm-before` for pane and window destruction becomes a Lector-owned
  Enter/Escape confirmation. The prompt names the actual stable `%pane` or
  `@window` ID, title, window, and session captured when it opens. Acceptance
  sends an explicitly targeted command, even if focus changed in the meantime;
  if that target disappeared, Lector reports it and sends nothing.
- `detach-client` detaches immediately. With one tmux connection it preserves
  the ordinary command; with multiple connections it requires the discovered
  control-client identity and emits an exact client target instead of risking
  an ambiguous detach.
- `send-prefix` sends the configured prefix bytes to the attached active pane
  through the binary-safe hexadecimal input path.
- `choose-tree -s`, `choose-tree -w`, `display-panes`, and `command-prompt`
  open Lector-owned accessible session, window, pane, and command controls.
- bindings that switch to any non-prefix key table, including the user's
  `root` and `passthrough` toggles, are announced as unsupported. Lector does
  not pretend to emulate that table or consume its later root keys.

Number selection, previous/next/last window and session commands, pane
directions, next/last pane, window creation, configured splits, command
chains, and any other safe ordinary binding preserve the server's discovered
command text. This keeps tmux authoritative over the semantics and targets.

## Destructive lifecycle behavior

Killing a pane or window reconciles its terminal engine immediately. Any text,
partial parser state, replies, and Kitty image media owned by a removed pane are
dropped, while surviving pane engines remain intact. If a session temporarily
has no active window, Lector keeps the connection reachable and names the
session and state instead of showing a stale pane or an implementation
placeholder. A later inventory can repopulate that same view.

Connection ownership is tracked as a bounded parent/gateway hierarchy. Closing
a gateway pane or window recursively resolves descendants in postorder, and
repeated or simultaneous cleanup is idempotent. Direct control shutdown clears
pending prefix commands and confirmations before returning to the gateway.

Five configurable Lector actions provide exceptional gateway control without
opening a second live view of a child pane:

- `detach_tmux_connection` sends the same safely scoped graceful detach used by
  the discovered prefix binding.
- `interrupt_tmux_gateway` sends Control-C to the transport which owns the
  selected connection, not to the selected child pane.
- `force_close_tmux_gateway` confirms before sending Control-backslash.
- `send_tmux_ssh_escape_disconnect` and `send_tmux_ssh_escape_help` confirm
  before sending the line-start `~.` or `~?` sequence to that exact transport.

For nested connections, each exceptional sequence is encoded as input to the
owning parent gateway pane and recursively wrapped to the direct PTY. A failed
physical write is never retried, because replaying a partial signal or SSH
escape after the transport changes could affect an unrelated shell. Navigation,
connection loss, or target removal cancels a pending confirmation.

The portal is deliberately not a second live rendering of the child pane. It
remains a read-only explanation of the control transport while child output is
processed only in the child engine. Enter is the normal route back to that
connection; other portal input is consumed without reaching either transport.

## Nested control connections

Lector recognizes a nested `tmux -CC` only inside the already decoded byte
stream of its parent pane. This is the path used when a local tmux pane runs an
SSH client and the remote shell starts tmux control mode. Octal quoting from
each outer `%output` record is removed before marker detection, so protocol
text cannot leak into a pane terminal engine and a marker-shaped outer control
record cannot be mistaken for a child.

The originating pane becomes a read-only portal while the child is active and
Lector automatically focuses the child connection. The parent's pane engine,
partial escape-sequence state, history, media, and every sibling pane remain
alive behind that portal. Switching to the parent shows the portal in its real
layout; Enter returns to the child. A clean child exit removes only that portal
and restores the preserved parent pane at its current parser boundary.

Connections use globally unique Lector IDs even when every nested server uses
the same tmux `$session`, `@window`, and `%pane` IDs. A child command is encoded
as binary-safe hexadecimal `send-keys` input to its gateway pane, then wrapped
again for every ancestor until it reaches the direct PTY transport. Reply FIFOs
remain attached to the control connection that owns each wrapping layer.
Nesting is bounded to 64 connections in one ancestry chain, with at most 4,096
live connection records and 4,096 pane gateway detectors. Destroying a gateway
pane or window removes every descendant deepest-first and releases its detector.

## Accessible choosers and command entry

The session chooser is scoped to the visible tmux connection. The window
chooser captures that connection's attached session, and the pane chooser
captures its active window. They never flatten similarly named or numbered
targets from another scope. Each row includes tmux's stable `$session`,
`@window`, or `%pane` ID, so duplicate display names remain distinguishable.

Type to filter case-insensitively, use Up/Down to move, Enter to select, and
Escape to cancel. The selected stable ID remains selected across rename and
topology notifications when it still exists; destroyed targets disappear and
selection moves to the first remaining match. Long lists scroll within a
bounded viewport while keeping the selection and help line visible. Resize
rebuilds the chooser from its logical state.

The top-level connection chooser lists terminal mode and every tmux connection.
Rows always include the stable connection ID, so duplicate user labels remain
unambiguous. `rename_tmux_connection` changes only the current stable identity;
the label survives inventory resynchronization but is discarded when that
connection ends. The corresponding configurable actions are
`open_tmux_connection_chooser`, `rename_tmux_connection`,
`open_tmux_session_chooser`, `open_tmux_window_chooser`,
`open_tmux_pane_chooser`, and `open_tmux_command_prompt`. The connection chooser
is available whenever any tmux connection exists. Connection-local controls
bell outside a ready, visible tmux connection.

The command prompt uses the same Unicode-aware editor as Lector's other local
controls. Enter submits, Escape cancels, Up/Down traverse a per-connection
100-entry history, and long input follows the cursor without splitting a
grapheme. Pasted control characters are rendered as visible symbols and a
command containing NUL, carriage return, or newline is rejected before it can
reach the control FIFO. Server-aware completion is not guessed locally;
history is the practical completion aid until a typed tmux completion query is
designed.

Command output and errors, tmux `%message` and `%config-error` notifications,
and Lector-owned confirmations use temporary reviewable popup overlays. Enter
or Escape dismisses announcements; confirmations preserve their distinct
confirm/cancel responses. `M-w` announces `tmux`, the connection label, and
the current window name. Ordinary terminal mode retains its separate
`terminal` wording.

## Regression harnesses

`tests/tmux_prefix.rs` uses records captured from the user's current `C-a`
configuration, including repeatable pane motion, confirmations, quoted format
expressions, command chains, `send-prefix`, and custom key-table toggles. Its
application harness covers primary and secondary prefixes, exact everyday
commands, fragmented Kitty key events, timeout and cancel, repeat expiry,
confirmation accept/cancel, command errors, malformed data, binding reload,
portal/connection changes, and safe fallback behavior.

The non-ignored real-server oracle launches an isolated tmux instance with a
small `C-a` fixture, discovers a second live window, and proves that Lector's
emulated `C-a n` changes the actual control client's active window. It uses
bounded channel waits and no timing sleeps.

`tests/tmux_interaction.rs` covers duplicate names, stable-ID search and
selection, empty and destroyed scopes, external renames, bounded scrolling,
cancel/resize, speech, command history and rejection, popups, `M-w`, and a
second-Ghostty physical render oracle. Its non-ignored isolated real-server
harness switches a live session through the chooser and carries a command and
its result through the actual control connection.

`tests/tmux_lifecycle.rs` adds deterministic one/multiple-connection detach
selection, active/inactive stable kill targets, only-window loss and recovery,
failed commands, stale confirmations, simultaneous server/target destruction,
portal isolation, and recursive gateway ownership. Its non-ignored destructive
harness creates, splits, kills a pane, kills a window, and detaches through an
isolated real tmux server. It uses bounded event waits with no sleeps and a drop
guard that kills the disposable server on success or panic. `tests/tmux_panes.rs`
also proves that pane removal releases Kitty media and the closed engine without
disturbing survivors.

`tests/tmux_connections.rs` gives separate routers identical tmux object IDs and
proves connection-scoped pane state, input, replies, popups, speech, prefix
state, command history, detach targets, labels, and removal. It includes 50
rapid bidirectional switches and a non-ignored harness with two independent
disposable tmux servers and PTYs.

`tests/tmux_nested.rs` fragments a decoded child marker one byte at a time,
checks binary pane output and parent bytes on both sides of the child, routes
commands through one and two nested layers, repeats identical object IDs at
every level, preserves parent and sibling engines, exercises startup failure
and cleanup bounds, and verifies detector release. Its non-ignored local
loopback harness launches an outer tmux on a real PTY, starts a second real
`tmux -CC` inside that pane, and proves discovery, portal ownership, bootstrap,
and echoed child input through both control transports with bounded waits and
panic-safe server cleanup.

`tests/tmux_recovery.rs` adds every-byte EOF, malformed/timeout/parent-loss
recovery, direct and nested exceptional-action scoping, confirmation, and
at-most-once write-fault coverage. Its non-ignored real-server case kills tmux,
fault-injects a lost final terminator into a nested parent pane, and verifies
that ordinary parent-shell output resumes.

Pane-wide bell behavior and its `off`, `spoken`, and `audible` Lua option are
documented separately in [tmux-bells.md](tmux-bells.md).
