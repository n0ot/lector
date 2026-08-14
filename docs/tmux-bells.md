# tmux pane bell monitoring

Lector observes BEL as a terminal effect in every pane-output stream delivered
by each tmux control connection. Monitoring covers every pane in every window
of that connection's currently attached session, including inactive panes,
hidden windows, inactive connections, and output received while a Lector
overlay is open. tmux normally does not deliver pane streams for unattached
sessions to one control client, so Lector does not claim that broader scope.

The Lua option `lector.o.tmux_bells` accepts exactly three values:

- `"off"` discards tmux pane bells and is the default.
- `"spoken"` announces the connection, attached session, window, and pane
  using tmux's stable numeric IDs together with their current names or title.
- `"audible"` schedules one physical BEL after the complete compositor
  transaction for a visible pane. Bells from a pane that is not currently
  presented still use the same sole physical-output scheduler.

One pane-output record containing multiple BEL bytes produces one notice.
Repeated notices from the same connection and pane within 250 milliseconds are
coalesced, while different panes and connections remain independent. The last
presented source retains its connection, session, window, and pane IDs and
labels. That state is discarded if its connection or pane disappears, so it
cannot point at a stale target.

`tests/tmux_bells.rs` covers active and inactive split panes, hidden windows,
synthetic output from an unattached session, multiple and inactive
connections, overlays, synchronized-output spans, a 10,000-BEL flood,
source-local duplicate coalescing, Lua configuration, stale-source cleanup,
and scheduler transaction order. The audible render is replayed through a
second headless Ghostty terminal and produces an oracle failure artifact on a
mismatch. Its non-ignored real-server harness creates an isolated split tmux
session, emits BEL from the inactive shell pane, and checks the stable source
reported by Lector without timing sleeps.
