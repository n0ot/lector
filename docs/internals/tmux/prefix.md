# Prefix, commands, and connections

In control mode tmux does not see your keystrokes as key presses, so Lector
has to act as tmux's keyboard handler. This page covers how Lector learns
your bindings, runs commands, replaces tmux's visual interfaces, follows
nested connections, and tears connections down.

The user's view of all this is in the [tmux guide](../../guide/tmux.md).

## Discovering bindings

Lector assumes nothing: not `C-b`, not zero-based windows, not Emacs keys.
Every [inventory](topology.md#inventory) asks the server for its options and
key tables.

- The query goes through the connection itself. A remote tmux reached over
  SSH reports its own live bindings, including tables made by plugins.
  Lector never reads or looks for a `tmux.conf`.
- Bindings are published together with the topology, as one transaction. A
  reload replaces the whole table.
- A malformed, duplicate, oversized, or non-UTF-8 record, or one containing
  control bytes, rejects the inventory.
- After any `source-file`, bind, unbind, or option-setting command, Lector
  requests a fresh inventory, even if tmux reported an error. A config file
  can apply its early lines before a later line fails.

## Prefix state

Outside tmux mode, input is untouched. `C-a` still reaches an ordinary
program.

In tmux mode, Lector consumes the primary or secondary prefix, says "tmux",
and waits for the next key.

- There is no timeout. A slow speech response cannot turn a tmux command into
  pane input.
- Escape cancels.
- An unbound key is consumed and announced. It does not leak into the pane,
  which matches tmux.

**Repeat.** After a repeatable binding, another repeatable key within the
server's `repeat-time` continues the prefix. Any other key, or a late one,
ends the repeat and is handled normally, so typing cannot get stuck.

**Scope.** Prefix and key-table state are stored per connection. Unfinished
prefixes on two connections cannot consume each other's keys. Entering a
connection's portal, or opening the connection manager, clears its prefix.

## Running commands

Discovered commands are sent to tmux as written, including command chains
and quoted formats, through the connection's ordered queue. tmux stays the
authority on what its commands mean.

There are a few deliberate exceptions.

### Numbered windows

A simple `select-window -t N` or `select-window -t :=N` is resolved by
Lector. It finds window `N` in the attached session and sends that window's
stable `@window` ID, so another client or session cannot change what the
number means.

If there is no such window, Lector says "can't find window: N" and sends
nothing.

### Injection safety

A binding or typed command that contains NUL, a carriage return, or a
newline is rejected before it reaches the queue. One record can never turn
into two commands.

### Commands Lector handles itself

| Command | What Lector does |
| --- | --- |
| `copy-mode`, `copy-mode -u` | Opens Review. `-u` opens one page up. Nothing is sent to tmux. See [copy mode](panes.md#copy-mode). |
| `confirm-before` (kill pane or window) | Shows its own Enter or Escape prompt. See below. |
| `detach-client` | Detaches this connection, and any nested ones first. |
| `send-prefix` | Sends the prefix bytes to the active pane through the hexadecimal input path. |
| `choose-tree -s`, `choose-tree -w`, `display-panes` | Opens a session, window, or pane chooser. |
| `command-prompt` | Opens Lector's command prompt. |
| Key-table switches | Enters the table tmux reports. An unbound key in a root or custom table goes to the pane unchanged. |

**Confirmations** capture their target when they open: the stable `%pane` or
`@window` ID, with its title, window, and session. Accepting sends a command
aimed at exactly that ID, even if focus has moved. If the target is gone,
Lector says so and sends nothing. Navigating away, losing the connection, or
losing the target cancels the prompt.

### Replies

Every command reply has a known type. An error opens a popup that can be
reviewed. It cannot be mistaken for inventory or capture data.

Output and errors from commands, tmux's `%message` and `%config-error`
notifications, and Lector's confirmations all use temporary popup overlays.

## Choosers

Each chooser is scoped so that similar names from elsewhere never mix in:

| Chooser | Lists |
| --- | --- |
| Sessions | The visible connection's sessions |
| Windows | That connection's attached session |
| Panes | That session's active window |

Each row includes the stable ID, so duplicate names stay distinguishable.

The selection is held by stable ID. It survives renames and topology
changes. If the selected item is destroyed, selection moves to the first
remaining match. Long lists scroll in a bounded viewport that keeps the
selection and help line visible. The physical cursor sits on the selected
row.

The command prompt keeps 100 history entries per connection. Pasted control
characters are shown as visible symbols.

Connection-local controls ring the bell when no ready tmux connection is
visible.

## Nested connections

A nested connection is `tmux -CC` started inside a pane of another control
connection, usually after SSH.

### Detection

Lector looks for the control marker only in a pane's *decoded* output, after
removing the octal quoting of the outer `%output` records. Protocol text
therefore never reaches a pane's terminal engine, and an outer record that
happens to look like a marker is not mistaken for a child.

### The portal

The pane where the child started becomes a read-only *portal*, and Lector
focuses the child connection.

- The parent pane's engine, partial escape state, history, and images stay
  alive behind the portal. So do its sibling panes.
- The portal is an explanation, not a second view of the child. Enter returns
  to the child. Other input is consumed and reaches neither side.
- The parent connection's prefix commands still work over a portal.
- When the child exits cleanly, the portal is removed and the parent pane
  resumes exactly where its parser left off.

### Routing

Connections have globally unique Lector IDs, even when every server uses the
same tmux IDs.

A command for a child is encoded as hexadecimal `send-keys` input to its
gateway pane, then wrapped again for each ancestor until it reaches the real
PTY. Each layer's replies stay with the connection that owns that layer.

### Carrier windows

tmux knows nothing about nesting. The outer server sees only pane bytes, and
it delivers output only for the attached session. If the outer client
switched session, the child's stream would stop.

So before switching the outer client's session, Lector:

1. links every live gateway window into the destination session;
2. verifies the exact window ID and index; and only then
3. sends `switch-client`.

The child keeps arriving through the same pane while you work elsewhere.
Returning to it uses the pane that is already linked.

Details:

- Carrier links use the reserved index range 1,000,000 to 1,004,095, and are
  hidden from the window chooser. A collision is retried within the range.
- An inventory finds a carrier again by its `@window` ID. If
  `renumber-windows` moved it, Lector moves that exact window back and
  verifies the result.
- Links that are no longer needed are removed with an ID guard, so a window
  the user put at the same index is never removed.

Session chooser actions and simple previous, next, or explicit
`switch-client` commands all use this transaction. While carriers exist,
Lector refuses session changes whose destination it cannot prove in advance:
ambiguous or compound commands, last-session, attach, and foreground
new-session.

A `%pause` on a gateway pane is fatal for the child. See
[flow control](flow-control.md#nested-connections).

## Ending connections

### Killing panes and windows

Killing a pane or window releases its engine at once, with its text, partial
parser state, pending replies, and images. Other panes are untouched.

If a session is briefly left with no active window, Lector keeps the
connection reachable and names the session and its state. It never shows a
stale pane.

### The connection manager

`M-C` opens the connection manager. Rows start with `*` for the active
connection and include the server's `#{host}`, for example
`* 3, build.example`.

The manager does not offer the raw terminal while a control connection is
alive, because that would only show an uninterpreted protocol stream. It
stays responsive while child output is paused, silent, or flooding.

### Graceful detach (`d`)

Lector sends a plain `detach-client` on the selected connection's own
channel. The channel already identifies the client, so no client name needs
to be looked up.

Nested connections detach deepest first. Lector waits for each one to
actually end before detaching its parent, so no nested client is orphaned and
SSH and parent shells stay alive. If one level stalls, the cascade stops
there.

Closing a gateway pane or window removes its descendants in the same order.
Cleanup is idempotent.

### Forced abandon (`D`)

This is the escape hatch, and it asks for confirmation.

1. Lector sends Control-backslash to the transport that owns the connection.
2. If the transport has not exited after 750 ms, Lector releases only its own
   parser and presentation state.

A direct connection returns to the raw terminal. A nested one returns to the
raw parent pane. The PTY and SSH process are not killed, so the user can type
tmux commands, finish a partial line, or use SSH's escape sequence. A later
`tmux -CC` marker is detected as usual.

For a nested connection, Control-backslash is encoded as input to the parent
gateway pane and wrapped up to the real PTY. A failed write is never retried:
replaying a signal byte after the transport has changed could hit an
unrelated shell.

### Actions

- `detach_tmux_connection` performs the graceful detach.
- `force_abandon_tmux_gateway` performs the forced abandon.
- `rename_tmux_connection` changes the connection's label. It survives
  inventories and is discarded when the connection ends.

## Announcements

`M-w` names the current location, such as "tmux, dev, 1: editor", or
"tmux, dev, 1.1: editor" in a split window.

- A window change announces the window title.
- A session change announces the session, then the window title.
- Empty pane contents are silent. They are not described as a blank screen.
- Ordinary terminal mode keeps its separate "terminal" wording.
