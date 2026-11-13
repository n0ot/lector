# tmux pane bootstrap and split composition

Lector turns a discovered tmux topology into persistent pane-local terminal
state. Every stable tmux pane ID owns one `View` and therefore one independent
Ghostty terminal engine, retained while its window is hidden. Decoded `%output`
and `%extended-output` bytes are delivered only to that engine. Escape, UTF-8,
and graphics parser fragments can consequently remain unfinished while another
pane or window is active and resume in their original owner later.

The pane-owned `View` also retains accessibility policy, Review state, departure
checkpoints, and media. Window links and moves select that same object; only
pane removal releases it. See [stable topology identities](tmux-topology.md)
and [accessible document contexts](architecture.md#accessible-document-contexts).

## Initial-state bootstrap

After a complete inventory, Lector sends one machine-generated `capture-pane`
command per new pane. Normal primary screens use `-p -e -F -J -S -` to include
attributes, line flags, and all available tmux history. Alternate screens capture the current display without history; panes in a
tmux mode use `-M`. The `-a` flag would select the saved grid, so it is omitted. The inventory supplies geometry, cursor position and
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

Capture restores the text and metadata tmux exposes, rather than a serialized
terminal. See [authoritative text resynchronization](tmux-completion.md#authoritative-text-resynchronization)
for pending escape capture, media and mode limitations, stale-state handling,
and recovery.

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

`tests/tmux_panes.rs`, in the `tmux` target, checks layout validation, bootstrap
races, persistent pane state, request correlation, and incremental composition
through `RenderOracle`. Its real-server fixture exercises split, resize, zoom,
and close against a second Ghostty terminal. That fixture is ignored by ordinary
Cargo runs and selected by `scripts/test-real-tmux-docker`; tmux and disposable
Unix sockets belong to the container environment. See the
[Docker procedure](../CONTRIBUTING.md#full-verification).
