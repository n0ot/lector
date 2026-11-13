# Input: keys, paste, mouse, focus, and resize

In control mode, Lector cannot write to a pane directly. Everything goes to
tmux as a command.

## Keys

Lector never places application bytes inside tmux command syntax. Input is
sent as hexadecimal, one two-digit argument per byte:

```text
send-keys -H -t %21 00 1b 5b 31 32 30 3b 35 3a 31 75
```

This is safe for every possible byte: NUL, invalid UTF-8, newlines, quotes,
semicolons, and tmux format markers.

- Input is assigned to whichever pane is active when the complete input
  event has been decoded.
- It is split into commands of at most 4,096 bytes.
- Neighboring key events are combined into one command, until some other
  command needs to go between them.
- When a batch is waiting, the event loop asks for an immediate turn.
  Batching adds no timer delay.

## The command queue

Inventory, capture, recovery, input, flow-control, and resize commands all
share one FIFO per connection, so nothing can overtake input that was
accepted first.

Lector records what kind of reply it expects only after the command has been
written successfully. Replies to `send-keys`, `refresh-client`, and
flow-control commands are consumed explicitly, so they can never be mistaken
for an inventory or a capture.

## Paste

Lector parses a bracketed paste from the outer terminal as one event. It
then checks the active pane's own mode, and adds the `CSI 200 ~` and
`CSI 201 ~` brackets only if that pane asked for bracketed paste.

The result takes the same hexadecimal path as keys, so a large or hostile
paste needs no quoting.

## Terminal queries

Lector owns the root child's PTY and answers its terminal queries directly.
It does not own a pane's PTY: tmux does, and tmux answers.

Each pane's engine in Lector still produces replies as it parses, but they
are discarded. Sending them with `send-keys` would turn a terminal report
into typed input.

Default colors are the one exception. tmux forwards OSC 10 and OSC 11 queries
to control clients, so Lector answers them:

```text
refresh-client -r '%N:<report>'
```

If a pane asks during startup, the answer waits until Lector has learned the
outer terminal's real colors, or the probe deadline has passed. The colors
match what a direct child would be told. See
[queries](../terminal.md#queries).

## Focus

Focus events from the outer terminal always update Lector's own focus state.
They are forwarded to tmux only if the active pane asked for focus reporting.

## Mouse

A mouse event is accepted only if it lands inside the active pane, not on a
border or another pane.

1. The position is translated to pane-local coordinates, counting from zero.
2. The event is re-encoded in the format the pane asked for: default, UTF-8,
   or SGR.
3. Press, release, drag, and motion events are each allowed only if the pane
   enabled that kind of reporting.

Events that fail these checks are dropped. They are never forwarded with
scene coordinates.

Lector's own click actions (`M-{` and `M-}`) already produce pane-local
coordinates and use the same input queue.

## Resize

Resizing the outer terminal changes the physical scene at once, but does not
resize any pane engine. Lector tells tmux the new size, collapsing a burst of
resizes to the latest:

```text
refresh-client -C 120x40
```

Pane engines change size only when tmux replies with the new layout. tmux
stays the authority on geometry.

The size still reaches tmux while a Review overlay or portal is open. Review
keeps its frozen document and adjusts its own viewport. Closing Review then
shows a tmux view that has already taken on the new layout.
