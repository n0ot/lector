# Application accessibility protocol

A full-screen terminal application usually knows more about what just
happened than a screen reader can work out from changed cells. This protocol
lets an application tell Lector what to say, and ask Lector to stay quiet
while it does.

It is optional. An application that sends nothing gets Lector's normal
automatic reading, cursor tracking, and deletion announcements.

## For application authors

The protocol itself is specified in the
[`lector.nvim` repository](https://github.com/n0ot/lector.nvim/blob/master/PROTOCOL.md).
Start there. It is not specific to Neovim or to Lector: any application can
send it, and any screen reader can implement it.

[`lector.nvim`](https://github.com/n0ot/lector.nvim) is a Neovim plugin that
uses it, and a good working example.

The rest of this page describes how Lector behaves as a receiver.

## Message format

Messages are APC escape sequences in seven-bit form:

```text
ESC _ Lector;A11y;1;<command> ESC \
```

Lector understands the version 1 commands `set`, `say`, `line`, and `end`.

### Limits

| Limit | Value |
| --- | --- |
| Speech text per message | 2,000 UTF-8 bytes, no control characters |
| Queued speech per view | 32 entries and 32 KiB |
| Whole sequence | 4 KiB |

A message that is invalid in any way has no effect. That includes invalid
UTF-8 or hexadecimal, an unknown version or command, duplicate or missing
settings, and incomplete or truncated strings.

## How Lector applies messages

**Messages belong to one view.** Policy and speech are stored on the exact
terminal view that received the message. In tmux control mode that means the
pane. Speech from a hidden or inactive pane is discarded. It is never played
later when you switch to that pane.

**Messages take effect with the screen.** Policy is published with the same
[presentation receipt](../internals/reading-model.md#live-and-presented-models)
as the view's cells. A render that is merged, dropped, or delayed cannot
apply policy from the wrong moment.

**Suppression is fine-grained.** Automatic reading and cursor tracking are
suppressed independently. Suppressing cursor tracking also suppresses pending
Backspace and Delete announcements.

**Requested speech is ordinary speech.** It goes through the same path as
everything else Lector says, so the user's settings still apply.

## The user stays in charge

An application can only make Lector quieter. It cannot turn on anything the
user turned off.

- Requested speech counts as automatic reading. If the user has turned
  automatic reading off, it is not spoken.
- While the terminal does not have focus, new requested speech is dropped. It
  is not saved for later.
- The user's stop-on-focus-loss setting still decides whether speech already
  in progress is interrupted.

## When a session ends

Lector restores its default behavior when any of these happens:

- the application sends `end`;
- the terminal switches between the primary and alternate screen;
- a full reset (`RIS`, `ESC c`) or soft reset (`DECSTR`, `CSI ! p`);
- the pane is replaced or resynchronized; or
- the PTY or the view goes away.

Redrawing alone does not end a session. Neither does resetting attributes
(`CSI 0 m`), erasing the display (`CSI 2 J`), moving the cursor, or changing
ordinary modes.
