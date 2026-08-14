# tmux pane bootstrap and split composition

Stop 3.4 turns a discovered tmux topology into persistent pane-local terminal
state. Every stable tmux pane ID owns one `View` and therefore one independent
Ghostty terminal engine, retained while its window is hidden. Decoded `%output`
and `%extended-output` bytes are delivered only to that engine. Escape, UTF-8,
and graphics parser fragments can consequently remain unfinished while another
pane or window is active and resume in their original owner later.

## Initial-state bootstrap

After a complete inventory, Lector sends one machine-generated `capture-pane`
command per new pane. Normal primary screens use `-p -e -J -S -` to include
attributes and all available tmux history. Alternate screens use `-a`; panes in
a tmux mode use `-M`. The inventory supplies geometry, cursor position and
visibility, named cursor shape, alternate-screen state, tmux-mode state, and
reported history size.

The captured rows are reconstructed into a fresh pane engine and the inventory
cursor metadata is applied afterward. This artificial reconstruction is then
finalized before the pane becomes accessible, so bootstrap text cannot be
mistaken for newly arriving output or spoken as a live change. Output observed
before the capture reply is bounded and retained: a successful capture
supersedes it because the snapshot was taken later in the ordered control
stream, while a failed capture replays it so live bytes are not lost.

Command replies are correlated through an explicit FIFO of inventory and pane
bootstrap request types. A layout invalidation may therefore queue a twelve-part
resync behind outstanding capture replies without either response class being
misinterpreted. Failed inventories retry once, remain transactional, and do
not loop forever on a persistent tmux error.

tmux cannot export a byte-for-byte copy of an existing terminal parser and
media store. In particular, capture output cannot faithfully reconstruct
preexisting Kitty or Sixel image uploads and placements, consumed OSC semantic
state, every hyperlink record, the complete mode save/restore stack, or an
arbitrary parser continuation. Joined captured rows can also preserve content
without preserving tmux's exact historical wrap-cell representation. Lector
seeds the text and style history tmux does expose and treats subsequent live
bytes as authoritative.

The same boundary applies when flow control reports stale incremental output.
Lector performs a fresh capture, explicitly drops unrecoverable media, and
announces a failed capture without trapping later output. See
[`tmux-completion.md`](tmux-completion.md) for the flow policy and recovery
procedure.

## Layout and rendering

`TmuxLayout` parses tmux's checksum-prefixed layout grammar with bounded depth
and node count. It accepts leaf panes, left/right `{...}` splits, top/bottom
`[...]` splits, nesting, and the single-pane visible layout used for zoom. It
rejects zero dimensions, overflow, duplicate IDs, children outside a parent,
overlapping children, gaps other than tmux's one-cell divider, and trailing
data.

The visible layout, rather than the unzoomed stored layout, determines the
scene. Pane rectangles are placed at tmux's coordinates. An engine-neutral
border snapshot fills only internal divider cells and connects nested box
drawing intersections; pane surfaces overwrite their own interiors. No tmux
status line is synthesized. The active visible pane owns the physical cursor,
while other visible panes continue rendering and hidden panes continue parsing.

Pane-local Ghostty damage maps through each surface origin for incremental
output. Split, close, resize, zoom, and active-pane topology changes use a
full-scene diff region, not a terminal clear, so the renderer emits only changed
cells and retains its correctness fallback. Portal and frozen-review surfaces
do not pause hidden pane engines.

## Regression harnesses

`tests/tmux_panes.rs` covers single, horizontal, vertical, nested, zoomed,
malformed, and adversarial layouts; border intersections; history, cursor,
alternate-screen, and tmux-mode bootstrap; quiet accessibility handoff; bounded
pre-inventory output; hidden windows; close cleanup; active-pane changes;
overlapping request/reply classes; partial sequences across pane switches; and
incremental pane damage through `RenderOracle` failure artifacts.

Its non-ignored live fixture launches an isolated tmux server without timing
sleeps, lets `App` perform the actual inventory and captures, and drives a real
split, pane resize, zoom, unzoom, and close. Every stable stage is replayed into
a second Ghostty terminal and compared with the intended composed scene. The
fixture rejects protocol leakage and flicker-prone full-terminal clears and
uses a disposable socket under `target/test-tmux`.
