# tmux

Lector works with tmux's control mode. Each pane gets its own reading state,
its own [Review](review.md), and its own scrollback, and Lector announces
where you are as you move between windows and sessions.

## Getting started

From a shell running inside Lector, start tmux with `-CC`:

```sh
tmux -CC
```

That is all. Lector notices control mode on its own, so it also works from a
script, an alias, or over SSH. Plain `tmux` without `-CC` runs as an ordinary
terminal program instead.

Your tmux prefix and key bindings work as usual. Lector asks the tmux server
for them, so it follows your real configuration, including a custom prefix
and bindings added by plugins. If you reload your tmux configuration, Lector
picks up the changes.

## The prefix key

When you press your prefix, Lector says "tmux" and waits for the next key.

- There is no timeout. Take as long as you like.
- `Escape` cancels.
- A key with no binding is announced and dropped. It is not sent to the
  program in the pane.

## What Lector does differently

Most tmux commands behave exactly as they do in tmux. A few open Lector's own
accessible version instead:

| tmux command | What you get in Lector |
| --- | --- |
| Copy mode (`copy-mode`) | Lector's [Review](review.md) |
| Session and window trees (`choose-tree`) | A filterable session or window list |
| Pane numbers (`display-panes`) | A filterable pane list |
| Command prompt (`command-prompt`) | A command prompt with history |
| Kill confirmations (`confirm-before`) | A prompt naming exactly what will be closed |

In the lists, type to filter, press Up and Down to move, `Enter` to select,
and `Escape` to cancel.

In the command prompt, `Enter` runs the command, `Escape` cancels, and Up and
Down recall earlier commands.

Output and errors from tmux commands appear in a popup that you can read like
any other screen. `Enter` or `Escape` closes it.

## Where am I?

Press `M-w`. Lector answers with something like "tmux, dev, 1: editor", or
"tmux, dev, 1.1: editor" when the window is split.

Lector also announces the window title when you change windows, and the
session and window when you change sessions.

## Bells

Lector tells you when a program in a background window rings the bell. Only
the first bell from a window is reported until you visit that window, so a
noisy program cannot flood you.

Choose how bells are reported in your [configuration](configuration.md):

```lua
lector.o.tmux_bells = "audible"
```

| Value | Behavior |
| --- | --- |
| `"audible"` | The default. Rings the terminal bell and says where it came from, such as "bell in window 1". Bells in the window you are in just ring. |
| `"spoken"` | Announces the connection, session, window, and pane by name. |
| `"off"` | Ignores pane bells. |

## Managing connections

Press `M-C` to open the connection manager. It lists every tmux connection
Lector is following, with `*` marking the current one.

| Key | Action |
| --- | --- |
| `Enter` | Switch to the selected connection |
| `d` | Detach cleanly |
| `D` | Force a stuck connection loose (asks first) |
| `Escape` | Close the manager |

Use `D` only when `d` does not work, for example after a network drop. It
gives you back the raw terminal underneath, without killing your shell or SSH
session, so you can type commands or use SSH's escape sequence (usually `~.`
at the start of a line).

## tmux inside tmux

If you start `tmux -CC` inside a pane, typically after SSH-ing to another
machine, Lector follows that connection too and switches to it. The pane it
started from shows a placeholder; press `Enter` on it to return to the inner
connection. Use `M-C` to move between connections.

Detaching the outer connection detaches the inner ones first.

While an inner connection is open, a few session-switching commands that
Lector cannot follow safely are refused. Use the session list instead.

## When something goes wrong

**"Resynchronized" messages.** If a pane produces output faster than it can be
delivered, tmux drops some of it. Lector then rebuilds the pane from tmux's
copy of the screen and tells you. Text and history are restored. Images are
not, so re-run the command if you need them.

**Lector stays in the ordinary terminal view.** Check that you started tmux
with `-CC`.

**A connection ends unexpectedly.** If tmux, SSH, or the network dies, Lector
closes that connection, tells you why, and returns you to the nearest
connection that is still alive, or to the plain terminal.

**"Automatic recovery is unavailable."** Your tmux is too old to support the
flow control Lector relies on. Everything still works, but upgrading tmux is
recommended.

## Key binding actions

These actions can be bound to keys in your
[configuration](scripting.md#key-bindings):

- `lector.open_tmux_connection_chooser` (default `M-C`)
- `lector.open_tmux_session_chooser`
- `lector.open_tmux_window_chooser`
- `lector.open_tmux_pane_chooser`
- `lector.open_tmux_command_prompt`
- `lector.rename_tmux_connection`
- `lector.detach_tmux_connection`
- `lector.force_abandon_tmux_gateway`

For how all of this works underneath, see the
[tmux internals](../internals/tmux/README.md).
