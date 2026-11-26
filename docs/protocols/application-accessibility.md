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
An application can send `query;<id>` to discover optional commands. Lector
returns `capabilities;<id>;cue=blank;link=1;inline=link` in the same APC envelope to the
querying PTY or tmux control-mode pane. The ID must be 1 to 32 ASCII digits.

- `cue;blank;<hex>` plays the blank tone or speaks the decoded fallback.
  An empty fallback makes the request silent when tones are unavailable.
  To report indentation as part of a semantic line, send
  `cue;blank;indent=<0..65535>;<hex>`.
- `say-link;<hex>` reads the decoded text with a link indication at its
  first spoken word. Lector uses spoken link feedback when applicable.

The `inline=link` capability also allows `say;links=<ranges>;<hex>` and
`line;indent=<columns>;links=<ranges>;<hex>`. Each range is a half-open UTF-8
byte span written as `start-end`. Send up to 32 nonempty spans in ascending
order, separated by commas. Spans may touch and must not overlap. Lector
indicates each link at its first spoken text and applies spoken feedback
when tones are unavailable. Producers use ordinary `say` and `line` with
receivers that omit this capability.

Capabilities describe the commands the receiver accepts. Lector applies the
user's tone preference and active speech host support at playback time.

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

**Requested speech and cues share the same policy.** Lector applies the user's
speech and tone settings to application feedback.

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
