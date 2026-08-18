# tmux topology and reconciliation

Each control connection owns a UI-independent `TmuxTopology`. The model keeps
tmux's stable numeric session (`$`), window (`@`), and pane (`%`)
IDs as identity. Human names and per-session window and pane indexes remain
presentation metadata, so duplicate names, one-based configurations, and a
window linked into multiple sessions do not collapse distinct objects.

The attached session is tracked separately from the set of sessions known to
the server. Each connection starts with the deterministic label `tmux N`; the
label may be replaced with 1 to 256 bytes of user-supplied text. No hostname or
SSH destination is inferred from the gateway transport; the connection manager
uses tmux's own `#{host}` server format instead.

## Explicit inventory transaction

Lector sends twelve independently parsed commands: machine-oriented `-F`
queries for sessions, windows, panes, attached session, the two base-index
options, attached server host and client name, and four key options, followed
by `list-keys -a`.
`list-keys -F` is newer than the rest of the inventory API, so Lector parses the
older command's canonical, reloadable `bind-key` syntax, including quoting,
escapes, tables, and repeat flags. This keeps key discovery compatible with
older tmux releases without falling back to whitespace parsing.

Each command receives its own `%begin`/`%end` or `%error` block. Lector
accumulates all twelve and validates them as one transaction before replacing
the visible model. A partial, failed, or contradictory snapshot cannot expose
half-updated topology or bindings. A failed transaction retries once; a second
startup failure becomes a persistent accessible error instead of an indefinite
readiness screen, while a failed refresh retains the last valid topology.

Command output is arbitrary text. Only `%end` or `%error` carrying the exact
tag from the surrounding `%begin` terminates a block; block-looking lines with
other tags remain payload. This matters when `capture-pane` contains the text
of a nested control client. A genuinely malformed protocol-looking record is
quarantined rather than exposed as terminal text: Lector sends
Control-backslash to terminate that control client, preserves its tmux
sessions, and drains through DCS ST before restoring the parent shell.

`list-windows -a` and `list-panes -a` repeat objects linked into more than one
session. Identical pane records are deduplicated by stable ID; conflicting
metadata rejects the snapshot. Window links retain both their session ID and
per-session index.

## Notification reconciliation and resync

Known session, window, layout, focus, and pane-exit notifications update the
model by stable ID. A notification that references an unseen object leaves a
safe stub, marks the model uncertain, and requests one full inventory after the
current inventory transaction. Duplicate invalidations do not enqueue
duplicate resyncs. Unknown notification names are ignored for forward
compatibility.

Full replacement is transactional and idempotent. It clears objects from an
old server generation while preserving the connection identity and user label.
`App::debug_tmux_topology` exposes a text hierarchy containing connection,
session, window, and pane IDs, attached state, indexes, names, and titles; it
never includes raw control-protocol records.

## Regression coverage

`tests/tmux_topology.rs` covers duplicate names, configurable indexes, linked
windows, duplicate linked-pane records, renames, focus changes, layout and pane
exit, out-of-order notifications, dropped-notification recovery, malformed
transaction rollback, server restart, empty optional fields, tab-bearing names,
and application command batching. Its non-ignored real-PTY oracle starts an
isolated tmux server, creates a second session with a genuinely linked window,
collects the twelve actual reply blocks, and parses the resulting topology. The
fixture requires tmux on `PATH` and permission to create a local Unix socket
under `target/test-tmux`.
