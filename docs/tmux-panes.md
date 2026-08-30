# tmux pane bootstrap and split composition

Lector turns a discovered tmux topology into persistent pane-local terminal
state. Every stable tmux pane ID owns one `View` and therefore one independent
Ghostty terminal engine, retained while its window is hidden. Decoded `%output`
and `%extended-output` bytes are delivered only to that engine. Escape, UTF-8,
and graphics parser fragments can consequently remain unfinished while another
pane or window is active and resume in their original owner later.

That same pane-owned `View` also owns user and application accessibility state:
the Review cursor and modes, APC auto-read and cursor-tracking policy, pending
speech metadata, accessible-document departure checkpoints, and image
namespace. Selecting another pane deactivates only the old pane document; it
does not clear every pane's update state. When the pane is selected again,
Lector compares its current complete document with the exact document which
was presented at departure and reads only the hidden interval's changes. A
tmux winlink is only another address for the stable `@window`; it never creates
another pane `View`. Manual
`link-window`, `move-window`, renumbering, and selection through a different
session consequently preserve one in-memory copy of all of this state.

## Initial-state bootstrap

After a complete inventory, Lector sends one machine-generated `capture-pane`
command per new pane. Normal primary screens use `-p -e -F -J -S -` to include
attributes, line flags, and all available tmux history. Alternate screens use
`-a`; panes in a tmux mode use `-M`. The inventory supplies geometry, cursor position and
visibility, named cursor shape, alternate-screen state, tmux-mode state, and
reported history size.

Native tmux copy mode is the exception: Lector predictively leaves it with a
targeted `copy-mode -q` and opens its own Review overlay for the active pane.
Discovered `copy-mode` prefix bindings are intercepted before tmux enters the
mode. This makes Review navigation, resizing, speech, and clipboard registers
behave consistently in control mode.

Captures are queued with the attached active pane first, followed by its other
visible split panes and then hidden windows or unattached sessions. The
connection becomes interactive as soon as every pane in the visible layout is
bootstrapped. Background captures continue through the same ordered control
FIFO, but cannot delay the first prompt or cause an otherwise unrelated visible
redraw when they finish.

A control client receives pane output only from its attached session. When that
attached session changes, Lector therefore treats every pane in the session
being entered as stale. It authoritatively recaptures the visible window at
once and retains the same stale marker on hidden windows until they are
visited. Returning to a session consequently restores output produced while
the control client was attached elsewhere, even when that output stopped before
the return.

The captured rows are reconstructed into a fresh pane engine and the inventory
cursor metadata is applied afterward. This artificial reconstruction is then
finalized before the pane becomes accessible, so bootstrap text cannot be
mistaken for newly arriving output or spoken as a live change. Output observed
before the capture reply is bounded and retained: a successful nonempty capture
supersedes it because the snapshot was taken later in the ordered control
stream, while a failed capture replays it so live bytes are not lost. If a
successful capture is visually empty but live pane output already supplied a
prompt, Lector replays that bounded output instead. This closes the startup race
where an empty capture could otherwise erase the initial shell prompt.

Command replies are correlated through an explicit FIFO of inventory and pane
bootstrap request types. A layout invalidation may therefore queue a twelve-part
resync behind outstanding capture replies without either response class being
misinterpreted. Failed inventories retry once, remain transactional, and do
not loop forever on a persistent tmux error.

tmux cannot export a byte-for-byte copy of an existing terminal and media
store. `capture-pane -F` lets Lector rebuild the prompt/output line flags and
hyperlinks tmux retained, and `capture-pane -P` restores a pending escape
sequence. Captures cannot faithfully reconstruct preexisting Kitty or
Sixel media, semantic state tmux did not retain, the complete mode save/restore
stack, or exact historical wrap-cell representation. Lector seeds the state
tmux does expose and treats subsequent live bytes as authoritative.

The same boundary applies when flow control reports stale incremental output.
Lector performs a fresh capture, explicitly drops unrecoverable media, and
keeps a failed capture stale while retrying with bounded backoff. See
[`tmux-completion.md`](tmux-completion.md) for the flow policy and recovery
procedure.

## Layout and rendering

`TmuxLayout` parses tmux's checksum-prefixed layout grammar with bounded depth
and node count. It accepts leaf panes, left/right `{...}` splits, top/bottom
`[...]` splits, nesting, the single-pane visible layout used for zoom, and the
`<...>` floating-pane suffix. Floating panes are composed in tmux's
bottom-to-top order and may overlap tiled panes without failing tiled partition
validation. The parser rejects zero dimensions, overflow, duplicate IDs,
children outside a tiled parent, tiled overlaps, gaps other than tmux's
one-cell divider, inconsistent floating geometry, and trailing data.

The visible layout, rather than the unzoomed stored layout, determines the
scene. Pane rectangles are placed at tmux's coordinates. An engine-neutral
border snapshot fills only internal divider cells and connects nested box
drawing intersections; pane surfaces overwrite their own interiors. No tmux
status line is synthesized. The active visible pane owns the physical cursor,
while other visible panes continue rendering. Raw hidden-pane transport is
always parsed far enough to preserve nested control-mode lifecycle boundaries.
Its ordinary terminal payload is applied in bounded background turns;
sustained floods are capped and rebuilt from an authoritative `capture-pane`
snapshot when that pane is presented. Thus a hidden pane normally retains its
exact parser continuation, while overload recovery makes the documented
text-only resynchronization tradeoff instead of starving foreground input.

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
