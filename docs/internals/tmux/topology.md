# Topology: sessions, windows, and panes

Each control connection owns a `TmuxTopology`: a model of that server's
sessions, windows, and panes. It is independent of the interface.

## Identity

tmux gives everything a stable numeric ID: `$` for sessions, `@` for windows,
and `%` for panes. Lector uses those as identity. Names and indexes are
labels only.

This keeps distinct things distinct when two windows share a name, when
indexes start at 1, or when one window is linked into several sessions.

- Sessions hold *winlinks*, each mapping an index to an `@window` ID.
- Windows and panes are stored once per connection.
- A pane's terminal engine and reading state are keyed by `%pane` ID.

So linking a window into another session, moving it, or selecting a different
winlink does not copy or hand off anything. It selects the same pane object,
with its parser state, history, images, Review state, and
[accessibility policy](../../protocols/application-accessibility.md) intact.
Only removing the pane releases it.

The attached session is tracked separately from the set of known sessions.

### Connection labels

Each connection starts with the label `tmux N`. The user can replace it with
1 to 256 bytes of text. Lector does not infer a hostname from the transport.
The connection manager shows tmux's own `#{host}` value instead.

A connection keeps its number for life. When it ends, the number is free
again, and a new connection takes the lowest free number.

## Inventory

To learn the topology, Lector sends twelve commands as one *inventory*:

- `-F` format queries for sessions, windows, and panes;
- the attached session;
- the two base-index options;
- the server host and client name;
- four key options; and
- `list-keys -a`.

Each command gets its own reply block. Lector collects all twelve and
validates them together before replacing the model. A partial, failed, or
self-contradictory inventory is never shown, so the topology and key bindings
are always consistent with each other.

- A failed inventory is retried once.
- A second failure at startup becomes a lasting, readable error, not an
  endless "getting ready" screen.
- A failed refresh keeps the last valid topology.

`list-windows -a` and `list-panes -a` repeat anything linked into more than
one session. Identical pane records are merged by ID. Conflicting records
reject the inventory.

Key bindings are parsed from `list-keys -a` in tmux's canonical `bind-key`
syntax, with full quoting, escapes, tables, and repeat flags. There is no
fallback to splitting on spaces. See [prefix handling](prefix.md).

## Notifications

Between inventories, tmux's notifications keep the model current. Session,
window, layout, focus, and pane-exit notifications update it by stable ID.

- A notification about something Lector has not seen leaves a stub, marks the
  model uncertain, and requests one full inventory.
- Repeated invalidations request only one.
- Unknown notification names are ignored, for forward compatibility.

A full replacement is transactional and idempotent. It clears anything from
an older server generation, and keeps the connection's identity and label.

## Debugging

`App::debug_tmux_topology` returns a text tree of the connection, sessions,
windows, and panes, with IDs, attached state, indexes, names, and titles. It
never includes raw protocol records.
