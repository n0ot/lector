# Panes: state, layout, rendering, and bells

Every tmux pane ID owns one `View`, and so one terminal engine. Decoded
`%output` and `%extended-output` bytes go only to that engine.

The view stays alive while its window is hidden. An escape sequence, UTF-8
character, or image transfer that is cut off when you switch away simply
continues when more output arrives. The view also holds the pane's reading
policy, Review state, [checkpoints](../reading-model.md#switching-contexts),
and images.

## Bootstrap

Control mode only sends output from now on. To show what was already on
screen, Lector asks tmux for each new pane's contents after an
[inventory](topology.md#inventory).

### The capture

| Pane state | `capture-pane` flags | Result |
| --- | --- | --- |
| Primary screen | `-p -e -F -J -S -` | Attributes, line flags, and all history |
| Alternate screen | Current display only | No history |
| In a tmux mode | Adds `-M` | The mode's screen |

The `-a` flag is never used. It selects the saved grid, not the one currently
displayed.

The inventory supplies the pane's size, cursor position, visibility and
shape, alternate-screen state, mode state, and history size.

### Order

Captures are queued in this order:

1. the attached session's active pane;
2. the other visible panes in that window; then
3. hidden windows and other sessions.

The connection becomes usable as soon as every visible pane is ready.
Background captures continue through the same queue, and never delay the
first prompt or cause a visible redraw when they finish.

### Applying a capture

The captured rows are rebuilt into a fresh engine, and the cursor is then
placed from the inventory. This rebuilt state is finalized before the pane
becomes readable, so it is never mistaken for new output and spoken.

Output can arrive before the capture's reply. That output is kept, within a
bound, and then:

| Capture result | What happens to the early output |
| --- | --- |
| Succeeded, with content | Discarded. The capture was taken later, so it already includes it. |
| Failed | Replayed, so nothing is lost. |
| Succeeded, but empty, and the early output drew a prompt | Replayed. This closes a startup race that could erase the first prompt. |

### Changing sessions

A control client receives output only for its attached session. When the
attached session changes, every pane in the session being entered is stale.
Lector recaptures the visible window at once and leaves hidden windows marked
stale until you visit them. Returning to a session therefore shows what
happened while you were away.

### Copy mode

Lector uses its own [Review](../../guide/review.md) in place of tmux's copy
mode, so navigation, speech, and clipboard behave the same everywhere.

- `copy-mode` bindings are intercepted before tmux enters the mode.
- If a pane is found in copy mode anyway, for example because another client
  put it there, Lector sends a targeted `copy-mode -q` and opens Review when
  that pane is active.

### What a capture cannot restore

A capture is text and the metadata tmux exposes. It is not a saved terminal.
See [resynchronization](flow-control.md#resynchronization) for what is lost.

## Layout

`TmuxLayout` parses tmux's layout string, with limits on depth and node
count. It accepts:

- single panes;
- left-right splits `{...}` and top-bottom splits `[...]`, nested freely;
- the single-pane layout tmux reports for a zoomed window; and
- the `<...>` suffix for floating panes.

It rejects zero sizes, overflow, duplicate IDs, children outside their
parent, overlapping tiles, gaps other than tmux's one-cell divider,
inconsistent floating geometry, and trailing data.

The layout is parsed once per topology change and cached. It is not reparsed
for each query.

## Rendering

The *visible* layout decides the scene, so a zoomed window shows one pane.

- Panes are drawn at tmux's coordinates.
- Dividers are drawn with box characters, joined correctly at intersections.
- Floating panes are drawn bottom to top and may overlap tiled panes.
- No tmux status line is drawn.
- The active pane owns the physical cursor. Other visible panes keep
  updating.

Each pane's damage is mapped through its position in the scene for
incremental drawing. Splitting, closing, resizing, zooming, and changing the
active pane compare the whole scene instead of clearing the terminal, so only
cells that really changed are redrawn.

Hidden panes keep running. Their output is always scanned far enough to
track [nested connections](prefix.md#nested-connections), and applied to
their engines in bounded background turns. See
[flow control](flow-control.md#fairness) for what happens under a flood.

## Images

Images belong to a pane's surface, not to tmux's numeric image IDs. Two panes
or two connections can reuse the same Kitty image and placement IDs without
clashing in the outer terminal.

- Placements are clipped to the pane and to any overlay.
- A hidden window keeps its images cached but contributes no placements.
  Switching back restores them without resending data.
- If a pane does not know the outer terminal's pixel size, placement
  geometry is derived from the physical cell size.

The general image design and limits are in
[Terminal](../terminal.md#images).

## Bells

Lector watches for BEL in every pane's output: every pane of every window in
the attached session, on every connection, even while an overlay is open.
tmux does not deliver output for unattached sessions, so those are not
covered.

The rules, as seen by the user, are in the
[tmux guide](../../guide/tmux.md#bells). Underneath:

- **Background windows latch.** A background window reports its first bell,
  then stays quiet until visited. The latch is shared by all panes of the
  window. Visiting acknowledges and rearms it.
- **Coalescing.** One output record with several BEL bytes is one bell. For
  the current window, bells from the same connection and pane within 250 ms
  are merged. Different panes and connections stay independent.
- **Ordering.** An audible bell is scheduled after the complete compositor
  transaction, through the same output scheduler as everything else.
- **Stale panes are silent.** While a pane is
  [stale](flow-control.md#resynchronization), all of its bytes and effects
  are discarded. A real bell in that gap can be lost. That is deliberate:
  interpreting bytes whose beginning is missing could invent a bell that
  never happened.

The source of the last bell is remembered by ID and label, and forgotten if
its connection or pane disappears.
