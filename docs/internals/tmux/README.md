# tmux internals

This section explains how Lector's tmux control-mode support works. For how
to *use* it, see the [tmux guide](../../guide/tmux.md).

## The idea

In control mode (`tmux -CC`), tmux does not draw anything. It sends a text
protocol describing sessions, windows, and panes, along with each pane's raw
output. Lector draws the panes itself.

The division of labor:

- **tmux owns** sessions, windows, pane geometry, layout, and the processes.
- **Lector owns** each pane's terminal engine, reading state, scene
  composition, images, and everything written to the physical terminal.

Each pane gets its own terminal engine and its own `View`, so reading,
Review, and scrollback work per pane exactly as they do in a plain terminal.
Panes share the same compositor, scheduler, and overlays as ordinary terminal
mode.

Lector never launches or configures tmux. The user runs `tmux -CC`, and
Lector recognizes the control-mode marker in the output.

## Reading order

| Page | Covers |
| --- | --- |
| [Protocol](protocol.md) | Detecting a control stream, framing, and parsing it |
| [Topology](topology.md) | Sessions, windows, and panes, and keeping them in sync |
| [Panes](panes.md) | Building pane state, layout, rendering, images, and bells |
| [Flow control](flow-control.md) | Staying responsive under heavy output, and recovering from loss |
| [Input](input.md) | Keys, paste, mouse, focus, and resize |
| [Prefix and commands](prefix.md) | Prefix handling, choosers, nested connections, and teardown |
| [Control transitions](transitions.md) | The ordered effects of each control event |
| [Testing](testing.md) | Test harnesses and the adversarial peer |

## Principles

A few rules shape the whole design:

- **Stable IDs are identity.** tmux's `$session`, `@window`, and `%pane` IDs
  identify things. Names and indexes are only labels.
- **One ordered queue per connection.** Input, inventory, capture, resize,
  and recovery commands share one FIFO, and each reply is matched to the
  command that caused it.
- **Changes are transactional.** An inventory is applied whole or not at all.
- **Never guess across lost bytes.** Once any pane output is missing, later
  bytes mean nothing until the pane is rebuilt from an authoritative capture.
- **Everything is bounded.** See below.

## Limits

| What | Limit | When it is reached |
| --- | ---: | --- |
| One control line or notification | 64 KiB | Parse error; the connection fails |
| One command reply | 4 MiB and 65,536 lines | Parse error |
| One inventory | 8 MiB and 65,536 lines | Reject and retry |
| Pane output held before bootstrap | 4 MiB, 4,096 orphan pane records | |
| Deferred background pane output | 64 KiB in total | Pause the pane, drop the backlog, resync |
| Pane work per turn | 4 KiB visible, 4 KiB hidden | Deferred fairly to later turns |
| Unanswered commands | 512 per connection | New commands dropped until replies resume |
| Input command size | 4,096 bytes | Split into several commands |
| Nested command encoding | 1 MiB, 4,096 commands | That command is dropped |
| Connections and gateway detectors | 4,096 each | |
| Nesting depth | 64 | |
| Scrollback per pane | 10,000 rows | Oldest rows dropped |
| Images | 32 MiB each, 64 MiB per pane, 128 MiB per scene, 4,096 placements | |
| Scheduled physical output | 2 MiB | Unstarted renders replaced or dropped |

The general turn limits (32 KiB or 4 ms per drain) are described in
[Architecture](../architecture.md#the-event-loop).
