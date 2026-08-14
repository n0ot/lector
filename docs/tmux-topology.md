# tmux topology and reconciliation

Stop 3.3 adds a UI-independent `TmuxTopology` for each control connection. The
model keeps tmux's stable numeric session (`$`), window (`@`), and pane (`%`)
IDs as identity. Human names and per-session window and pane indexes remain
presentation metadata, so duplicate names, one-based configurations, and a
window linked into multiple sessions do not collapse distinct objects.

The attached session is tracked separately from the set of sessions known to
the server. Each connection starts with the deterministic label `tmux N`; the
label may be replaced with 1 to 256 bytes of user-supplied text. No hostname or
SSH destination is inferred from the gateway transport.

## Explicit inventory transaction

Lector sends twelve commands with machine-oriented `-F` formats: sessions,
windows, panes, attached session, the two base-index options, client name, four
prefix options, and the prefix binding table. tmux returns one `%begin`/`%end`
block for each semicolon-separated command, not one block for the whole input
line. Lector therefore accumulates all twelve successful replies and validates
them as one transaction before replacing the visible model. A partial, failed,
or contradictory snapshot cannot expose half-updated topology or bindings.

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
