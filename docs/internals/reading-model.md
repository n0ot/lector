# Reading model

This page explains how Lector decides what to speak and when. It covers:

1. [Live and presented models](#live-and-presented-models): why speech never
   gets ahead of the screen.
2. [When output is stable](#when-output-is-stable): how long Lector waits
   before reading.
3. [What changed](#what-changed): how Lector works out what to read.
4. [Switching contexts](#switching-contexts): overlays, panes, and screens.
5. [Reading heuristics](#reading-heuristics): special cases such as menus and
   deletion.

One rule runs through all of it: **when the evidence cannot tell two outcomes
apart, take the conservative path instead of guessing.**

## Live and presented models

Each terminal view keeps two models:

- The **live** model reflects every byte parsed so far. Parsing, protocol
  replies, rendering, and raw application input use it.
- The **presented** model reflects what has been flushed to the physical
  terminal. Screen reading, Review, history, and marks use only this one.

Every scheduled render carries an *accessibility bundle*: the view identities,
model revisions, and exact snapshots that the render shows. The bundle follows
its render through coalescing, replacement, partial writes, and backpressure.
Only a successful flush returns it, and applying returned bundles in order is
the only way the presented model advances.

So a render that is dropped or replaced can never become readable, and an
older render that had already started publishes its own snapshot, not a newer
one.

History is shared between bundles rather than copied. Receipts carry bounded
append and evict deltas, so steady scrolling installs only the new rows.
Screen changes, reflow, resets, clears, and gaps produce a full, independent
snapshot instead.

Views that have left the interface stay alive while the last presented scene,
or a render in flight, still names them. This covers closing overlays and
removing panes without keeping them forever under backpressure.

### Input is not held back

Raw input to the application is never delayed to line up with what was read.
The application can change its screen between the moment you hear something
and the moment your key or click arrives. That gap is inherent to any screen
reader, and Lector does not try to hide it.

### Outer terminals without synchronized output

When the outer terminal supports synchronized output (DEC mode 2026), Lector
wraps its own writes so a partly drawn frame is never visible. Without it, the
terminal may briefly show part of a frame under backpressure. Lector still
never reads ahead of the last complete frame.

## When output is stable

Lector waits for output to settle before reading it, so it does not speak a
half-drawn screen. It uses the first of these signals that applies, in order:

1. **The application closes a synchronized-output frame.**
2. **A complete line of plain output arrives.**
3. **A quiet window passes.**
4. **A hard deadline is reached.**

### Synchronized output

Applications such as Neovim bracket each redraw with DEC mode 2026. While the
frame is open, the scheduler holds the scene. When the frame closes, the
newest scene becomes eligible, and it is readable the moment that exact render
flushes. No further delay applies.

A frame that never closes must not wedge the screen. If a frame is idle for
100 ms, or open for 2 seconds in total, the scheduler releases the newest
partial render. Review can inspect it at once. Automatic reading falls back to
the quiet window below, because the frame was abandoned, not completed.

In a visible tmux split, all panes form one scene. One pane holding a frame
open delays reading in the others too.

### Complete lines

On the primary screen, plain printed output that ends in a newline is a real
boundary. After the matching flush, Lector checks the completed line against
the presented screen and reads it without waiting.

This applies only to straightforward output. A trailing partial line, cursor
positioning, a bare carriage return, an unfinished escape sequence, the
alternate screen, or missing evidence all fall back to the quiet window.

One earlier bit of cursor positioning does not disqualify later output
forever. A later line that starts cleanly at a line boundary qualifies again.

This path is driven by output alone. It does not look at whether you pressed
Enter.

### Quiet window

Unmarked output waits for a short pause. The window adapts per view:

- It starts at 30 ms and stays between 8 and 60 ms.
- It shrinks only after repeated clean bursts.
- It grows at once if more output arrives just after Lector decided the
  screen was stable.

Unfinished escape sequences never count as quiet. Title-only and purely
structural bursts do not train the timer. Switching context discards pending
deadlines.

### Hard deadline

Output that never pauses is read anyway after 300 ms, so continuous output is
read progressively.

### What is not a boundary

- **Shell prompt marks (OSC 133).** They label prompts, input, and output for
  Review and history. They never speed up or delay reading.
- **Cursor visibility.** Applications show and hide the cursor in the middle
  of a redraw, so it proves nothing.

### Deletion

A confirmed Backspace or Delete can be announced right after its flush,
without finalizing the rest of the update. Confirmation needs both the
expected cursor position and a changed input row.

If the screen does not change, Lector cannot tell an ignored key from a slow
one. It keeps the pending deletion for a bounded time and drops it on an
incompatible redraw. It never announces a deleted character without evidence.

### Through tmux

Direct mode and `tmux -CC` both give Lector the application's own bytes, so
both get the complete-line path. An ordinary attached tmux client gives
Lector tmux's redrawn output instead, where the application's newlines may be
invisible. There Lector relies on tmux's own synchronized-output frames or the
quiet window. See [terminal compatibility](terminal.md#what-applications-see)
for how tmux learns that Lector supports them.

## What changed

Once output is stable, Lector works out what to read.

### The document

Lector compares complete documents: retained history followed by the visible
grid. This is the same document Review shows.

Ordering it that way makes scrolling simple. When a line is added at the
bottom, older lines move into history but keep their place in the document, so
only the new tail counts as inserted. The primary and alternate screens are
separate documents.

### Evidence journal

Each view keeps a bounded journal of facts that affect reading: completed
print records, cursor and screen identity, changed rows, and whether anything
structural happened. A render carries a small pointer into the journal, and
its flush moves exactly that evidence into the presented state.

When ordinary line output can be validated, the journal supplies the exact
text that was printed, immediately. Otherwise Lector diffs the documents.

If part of the journal was too large or was evicted, that gap is remembered
until the next baseline, and Lector uses the document diff instead.

### When identity is lost

Some events make old and new rows incomparable:

- **Resize, reflow, screen switch, or terminal reset.** No row identity
  survives. Lector reads the visible grid and makes it the new baseline. A
  resize counts as an update even if the application prints nothing.
- **A gap in history.** Only the history comparison is lost. If the geometry
  and screen are unchanged, Lector diffs the visible grids.

### Text caches

Each view caches the text of its visible grid and complete document, with
row hashes, for the current and previous state. The caches make comparison
fast. They do not publish anything or consume journal evidence.

## Switching contexts

Every readable surface is a `View`: the terminal, Review, the REPL, popups,
and each tmux pane. The compositor and tmux code report only two events to a
view: it was **deactivated**, or it was **activated**. They contain no
screen-diff logic of their own.

**Deactivation** records a checkpoint: the complete document that was readable
when the view lost focus. Taking it does not disturb pending speech or
deadlines. The hidden view keeps parsing and rendering without overwriting the
checkpoint.

**Activation** compares the current document with the checkpoint, and gives
automatic reading one of three answers:

| Answer | When | Effect |
| --- | --- | --- |
| `CompleteDocument` | History is continuous | Read what changed while hidden |
| `VisibleGrid` | Only screen coordinates can be trusted | Diff the visible grids |
| `Reintroduce` | Nothing can be compared | Read the visible grid afresh |

The consequences:

- A first visit has no checkpoint, so the screen is introduced.
- A return reads only what changed while you were away.
- The primary and alternate screens have separate checkpoints. Returning from
  an unchanged alternate screen is silent.
- Each view has a comparison epoch that advances on resize and reset. A
  checkpoint from an older epoch yields `Reintroduce`. No resize-, overlay-,
  or tmux-specific cases are needed.

### Titles

A title change in place is silent. A context switch announces the current
title. The announced title comes from the flushed bundle, so the name you hear
always matches the screen that was actually shown.

Overlay and base transitions travel in the same bundle as their pixels.
Commands that read the screen keep using the old view until the new scene
flushes.

## Reading heuristics

These handle common application behavior without knowing about any particular
application.

### New interfaces

A repaint on the primary screen that hides the cursor and fills several
previously blank rows is treated as a new interface or dialog. The changed
region is read in full.

Text added above a prompt, as chat-style programs do, is read as inserted
text.

### Status lines and cursor moves

Status changes elsewhere on screen stay readable during streaming output and
typing. Their position, length, or timing does not prove they are unimportant.

When the cursor moves and another row changes in the same frame, Lector reads
the changed text and then follows the cursor.

### Key echo

Key-echo suppression removes only an exact echo of what you typed. It never
discards other changes in the same frame.

### Menus without a cursor

Many full-screen programs hide the cursor and show the selection with color or
a marker. Lector can follow two such patterns. Both require a key press that
was actually sent to the application, followed by a later presented frame.
Lector gives the key itself no meaning, so custom bindings work the same as
arrows.

- **A moving highlight.** The frame must show a style moving from one bounded
  run of text to another, with matching "lost it" and "gained it" changes
  between exactly two otherwise stable rows. The moved style must be clearly
  rarer than the surrounding text. No particular color is assumed.
- **A moving pointer.** One short punctuation-like marker, such as `>`, moves
  between the same gutter columns of two stable, aligned rows. The marker is
  learned from the frame. A marker-only list needs an unchanged neighboring
  row. A two-row list, or a marker printed directly against its text, also
  needs a matching style change.

Geometry, wrapping, links, and cursor shape must be unchanged. Anything
ambiguous is left to the ordinary reading paths: global restyles, spinners,
prompt edits, several simultaneous moves, scrolling, reordering, unrelated
text changes, and real cursor movement.

### Application-authored speech

Applications can also tell Lector exactly what to say. See the
[application accessibility protocol](../protocols/application-accessibility.md).
