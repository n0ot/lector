# Architecture

Lector is both a terminal implementation and a screen reader. Application PTY
bytes never pass through to the physical terminal: Lector parses them into
modeled state, composes that state with its own UI, and renders the resulting
scene.

## Runtime ownership and concurrency

The main `mio` event loop owns `Process`, `App`, terminal engines, tmux state,
views, and presentation state. Those objects are mutated on that thread rather
than shared behind locks. The Ghostty adapter reinforces this boundary by
making its native handles non-`Send`.

Bounded workers isolate side effects that may block:

- The speech supervisor owns both native and custom process backends behind the
  bounded asynchronous speech worker. Its mailbox drops stale speech under
  overload and prioritizes cancellation and speech-setting changes. JSON-RPC pipe readiness and
  absolute deadlines are worker-local; completion or fatal failure reaches the
  main loop through an event-driven control path rather than a polling timer.
  The default native backend is hosted by `lector tts`, which imports the same
  `lector-tts` crate as the independently buildable host binary. Pipe I/O,
  native utterance construction, playback, and platform lifecycle work cannot
  stall terminal processing. Initialization identifies the selected backend
  and independently negotiates rate, pitch, volume, and voice operations. The host submits
  only Lector's one active utterance to the backend; the host-independent
  Lector manager keeps all never-submitted speech bounded.
- Explicit Review URL opening uses one worker owned by `App`. The worker
  launches the desktop helper with one literal URL argument and null standard
  streams, waits for it, and reaps it even if the overlay closes. A bounded
  result channel is polled at 50 ms deadlines while pending. Repeated requests
  report busy; shutdown does not wait for or kill a long-lived browser helper.
- `diagnostics` owns log output. Producers enqueue records into a byte-bounded
  queue, and the event loop never performs the underlying file or stderr I/O.

Startup preserves the Unix PTY post-fork boundary and gives Lua one coherent
lifecycle:

1. Lector spawns the application PTY before starting worker threads.
2. It creates Lua and evaluates `init.lua`; top-level code selects a speech
   process but does not need a separate pre-start configuration file.
3. It starts and initializes the committed speech server, with one fresh
   startup retry on failure.
4. It activates the physical terminal, starts bounded capability probes, and presents
   initial child output.
5. It calls `on_startup` immediately before entering the normal input loop.

Lua callbacks borrow the reader only during an explicit Rust-to-Lua entry
(`src/lua/access.rs`). Persistent callbacks own a VM-local access slot; each
configuration evaluation, hook call, binding resume, and REPL resume installs
an mlua scoped userdata reference in that slot. Returning, yielding, errors,
and Rust unwinding all restore the previous slot and invalidate the scoped
userdata. Moving or dropping the reader between entries leaves no reader
address in the Lua runtime. A nested hook uses a child scope reborrowed from
its caller, so no incompatible reader reference stays usable across mutation.
Configuration VMs must still own the reader's current generation at entry and
at each callback borrow. Reload candidates have their own reader and VM; a
failed reload leaves the live generation unchanged, while a successful reload
cannot reactivate retained old callbacks. The persistent REPL obtains access
only for each resume and retains its Lua environment independently of the
configuration generation.

Lua argument tables are parsed before reader access and response data is
snapshotted before allocating Lua tables. Unmediated reentry while a reader
borrow is already active fails with an mlua borrow error. Nested Rust-to-Lua
entries are limited to 32 per VM to bound recursive hook stacks; reaching this
limit follows ordinary hook-error recovery without disabling unrelated hooks.
This boundary uses safe Rust and mlua's scoped borrowing APIs, with no custom
raw-pointer lifetime or aliasing mechanism.

A host-independent speech manager owns a bounded queue and sends at most one
active utterance to the host. The supervisor owns transactional host replacement,
rollback, restart, and cleanup. See the [speech playback state machine](speech-driver-protocol.md#9-lector-playback-and-queue-semantics)
and [failure recovery](speech-driver-protocol.md#10-errors-failure-recovery-and-shutdown)
for lifecycle evidence, queue transitions, and deadlines.

Terminal input, PTYs, and physical-terminal output are nonblocking and drained
in bounded turns. Physical input and child PTY output each yield after 32 KiB
or 4 ms, checked between reads of at most 8 KiB, and request an immediate later
turn when ready data remains. An open synchronized-output frame does not expand
the PTY budget. `OutputScheduler` is the single ordering point for physical
output; it bounds queued work, completes a partially written escape transaction
before starting another, and can replace unstarted rendering from authoritative
scene state.

One child-PTY drain is also the presentation transaction. Every read is parsed
into the live terminal model before the read callback returns, and generated
replies and nonvisual effects keep their stream order. Physical bells remain
owned by the pending visual transaction and follow only an accepted scene. A
transport-neutral
`PendingPresentationBatch` merges only renderer damage and semantic operation
hints. The common direct and one-pane cases use inline optional slots; storage
for multiple pane sources is allocated only when it is needed. At the fairness
boundary, Lector selects the authoritative root or composed tmux scene and
captures it once. Cancellation invalidates incremental renderer state so the
next successful publication reconstructs any model changes that were never
presented.

tmux control records remain ordered on the event-loop thread. Connection-local
transitions return results to App, which applies commands, hierarchy changes,
recovery, and presentation in the phases defined by the
[control transition contract](tmux-control-transitions.md#ordered-app-effects).
A transition result can carry several effects together; topology synchronization
and rendering are not mutually exclusive outcomes. Hidden panes do
not retain cumulative speech/render summaries: each pane batch is applied as a
bounded delta, and only the active accessibility pane keeps metadata until its
next stabilization point. Adjacent ordinary output records for the same pane
inside one root-PTY read are combined up to 64 KiB, with control records,
extended output, and pane changes acting as ordering fences. That coalescer is
flushed before `handle_pty` returns, so it never defers model mutation or a
protocol reply across reads. The final visible state from the bounded drain is
composed and rendered once before ready user input is dispatched. The active
tmux layout is parsed into one
cached projection when topology changes instead of being reparsed for each
visibility, input, and composition query. Selecting the live viewport is a
no-op, and `Scrollback(0)` never materializes retained history. A lossy tmux
pause marks the affected pane stale; visibility resumes it and obtains an
authoritative capture instead of pretending the missing bytes were buffered.

Ghostty's render damage also bounds snapshot work. An ordinary partial update
re-reads and normalizes only rows Ghostty marked dirty, moving unchanged owned
rows forward without copying their cells, graphemes, styles, or hyperlinks.
Lector applies the same row ranges to its normalized live snapshot and avoids
pre/post full-screen copies for presentation-tracked views at the live
viewport. Full damage, geometry changes, selected scrollback, screen
transitions, and inconsistent damage metadata retain the authoritative
full-reconstruction fallback.

The incremental renderer keeps ordinary viewport scrolling structural even
when visible rows contain soft wraps or one parser batch interleaves a write
between two scrolls, as line-oriented programs commonly do. It scrolls the
physical region, explicitly clears wrap metadata on newly introduced rows,
then validates and repairs the complete affected region against Ghostty's
authoritative final scene. Any mismatch still falls back to a full render.

## Synchronized-output consistency

Each terminal view maintains a live Ghostty model and a physically presented
accessibility model. Parsing, protocol replies, rendering candidates, and raw
application input use the live model. Screen reading, review navigation,
history, and review marks use only the last presented model.

Every scheduled render owns an opaque accessibility bundle containing stable
view identities, model revisions, and the exact visible snapshots represented
by that scene. Changed scrollback generations are shared across candidate
frames; unchanged history is not copied. History receipts carry bounded,
Arc-linked append/evict deltas over an absolute window and use `VecDeque` at
the presented boundary, so steady scrolling reads and installs only newly
retained rows. Screen changes, reflow, resets, clears, disjoint intervals, and
bounded chain compaction create independently applicable full roots. Every
chain is validated before the committed deque moves, preserving exact state
when intermediate frames are replaced or an older started frame completes.
The bundle follows its render through coalescing, replacement, partial writes,
and backpressure. Only a successful flush returns it as a completed render,
and applying completed bundles in order is the sole accessibility publication
path. A capacity-dropped or replaced render therefore cannot become readable,
and an older render which was already started publishes its own snapshot rather
than a newer parser state.

DEC private mode 2026 controls render eligibility, not accessibility commit.
While the mode is open, the scheduler holds the composed scene. A real close
makes the newest candidate eligible, but its contents remain unreadable until
that exact render flushes. Prefixes, fragmented markers, and close/reopen
sequences cannot expose a logical checkpoint which was never physically
presented.

When a batch ends exactly at a real close, that stabilization fact travels in
the same accessibility frame as the pixels. Once the matching physical flush
receipt arrives, auto-read may finalize immediately instead of waiting for the
ordinary 30 ms debounce. Bytes after the close clear this fast-path marker, so
the next ordinary frame still stabilizes normally.

Raw input to the application is not deferred behind synchronized output.
Snapshot consistency therefore does not make a read-and-act sequence atomic:
the application can change its working screen between an accessibility read
and coordinate-based input. That is the normal coordinate time-of-check to
time-of-use boundary.

If a frame is idle for 100 ms or remains continuously open for the 2-second
hard cap, the scheduler releases the newest partial render so physical output
cannot remain wedged. Accessibility stays on the previous frame while that
render is backpressured, then advances to precisely the partial generation that
flushed. Review may inspect that generation immediately. Its receipt rebases
auto-read onto the ordinary adaptive quiet window and 300 ms streaming cap, so
the stale parser mode cannot silence speech indefinitely; it is not treated as
a successful atomic close. Further output in the broken epoch follows the same
presentation fence, and a later real close ends the epoch only when its
close-or-newer render flushes. A visible tmux split is one composed generation,
so one pane's hold
also prevents ordinary changes in another visible pane from being read early.

Overlay and base transitions carry the active accessibility view in the same
bundle. Screen-derived commands and transition announcements retain the old
view until the replacement scene flushes. Coordinate-dependent clicks and raw
input still target the live logical UI, preserving the ordinary UI
time-of-check to time-of-use behavior described above.

## Accessible document contexts

`View` is the common accessible-document model used by the terminal, Review,
the Lua REPL, overlays, and each persistent tmux pane or portal. The compositor
and tmux topology code report only two ownership transitions to that model:
deactivation and activation. They do not implement their own screen-diff
rules.

Deactivation records the exact complete document which was accessible when
the context lost ownership. This departure checkpoint is independent of
`prev_screen`, parser update evidence, and the speech-finalization revision.
Taking it therefore cannot consume pending speech, discard an unpresented
revision, or move a stabilization deadline. A hidden model may continue to
parse, render, and even finalize changes without overwriting the checkpoint.

Activation selects the checkpoint for the now-current primary or alternate
screen and makes it the comparison baseline. Automatic reading receives one
of three capabilities from the model:

- `CompleteDocument` when retained-history row identity is continuous;
- `VisibleGrid` when only fixed visible coordinates remain trustworthy; or
- `Reintroduce` when no mapping is safe.

A first visit has no checkpoint and therefore introduces the visible grid.
A return uses the departure checkpoint and reads only changes made while the
context was hidden. Primary and alternate screens have separate checkpoint
slots, so returning from an unchanged alternate screen to primary is silent
while a first alternate-screen visit is introduced.

Each `View` also owns a comparison epoch. Resize/reflow and terminal reset
advance it at the terminal-model boundary. Checkpoints from an older epoch are
incomparable and yield `Reintroduce`; auto-read does not need a resize-, reset-,
overlay-, or tmux-specific branch. A complete-history lineage gap yields the
more limited `VisibleGrid` comparison instead of guessing history identity.

In-place application-title changes and tmux renames update the model silently.
A context handoff announces the current title rather than the full tmux location
label. Activation announcements use the title from the completed presentation
bundle, not a later logical controller value. This keeps both the selected document and
its spoken name tied to the same physically flushed scene.

Views removed from the logical stack remain owned only while they are named by
the last presented scene or by a pending/started render receipt. This covers
overlay dismissal, tmux connection and pane removal, portal teardown, and pane
resynchronization without turning permanent output backpressure into unbounded
retention. Terminal-title effects carry their own flush receipt because the
outer terminal can apply an OSC title before a larger cell render completes.

Visible-grid and complete-document text caches belong to `View`. Each cache
keeps current and previous text with row hashes, is prepared lazily, and copies
its prepared current value to the previous baseline on finalization. Mutation
invalidates current contents while retaining reusable storage; an unprepared
cache cannot supply a previous baseline. These caches accelerate comparison;
they do not publish accessibility state or consume journal evidence.

Each view also keeps a bounded, revisioned journal of accessibility evidence.
The journal contains only facts which can affect a reading decision, such as
completed print records, structural taint, parser continuation, cursor and
screen identities, and changed-row ranges; renderer operations, effects, and
other heavyweight update data are not duplicated. A frame carries a compact
epoch/revision selector rather than a cloned cumulative report. Its successful
receipt moves exactly the selected evidence into the presented state. Missing
evidence caused by an oversized or evicted journal range is sticky until that
range is baselined, forcing the authoritative snapshot-diff fallback instead
of guessing.

Accessibility finalization and deadline scheduling share one ordered decision
table: synchronized-output close, validated LF-complete primary-screen record,
quiet window, then hard deadline. OSC 133 marks remain semantic history; cursor
restoration records painting provenance. Neither accelerates stabilization.
Incomplete escape sequences cannot quiet-commit, structural and title-only
bursts do not train the timer, and context switches discard transient deadlines.
See [latency decisions](performance.md#latency-decisions) for timing and record
validation.

Whole-frame structural provenance remains sticky for the
fallback diff, while the terminal adapter separately records the ordered safe
print suffix after the latest ambiguity. The suffix composes across parser and
presentation fragmentation, so a later record which begins at a physical
record boundary can recover progressive reading after an earlier structural
batch. Mid-record ambiguity remains on the authoritative diff path.

Auto-read's authoritative comparison document is the retained history followed
by the live grid—the same complete terminal document exposed by Review. This
ordering encodes the central scrolling invariant directly: when a bottom row is
added, older visible rows move into history without moving within the combined
document, so only the new tail is inserted. A primary-screen TUI normally edits
the visible suffix; an alternate-screen TUI has its own history-and-grid
identity. The bounded parallel print observer supplies immediate record
boundaries and exact causal text when it can validate ordinary line-oriented
output, but structural output such as tabs and cursor-addressed painting falls
back to the combined document rather than a visible-grid-only diff.

History origin and extent provide continuity across append and bounded head
eviction. Geometry/reflow changes, primary/alternate handoffs, and terminal
resets invalidate every visible row identity; after the corresponding physical
frame is presented, Lector reads the complete visible grid and establishes that
generation as the next baseline. A disjoint history interval invalidates only
the complete-document comparison: unchanged geometry and screen identity still
provide exact visible-grid coordinates, so Lector explicitly diffs the previous
and current visible grids. Resize is itself an accessibility update even when
the application emits no PTY bytes.

The invariant is exact at completed presentation boundaries. When the outer
terminal supports DEC 2026, Lector's global wrapper also hides partial VT
serialization. An outer terminal without that capability may briefly display
a partially written transaction during nonblocking backpressure; Lector does
not claim an accessibility model for half an escape stream, but it never reads
ahead of the last completed generation and converges exactly when the render
flushes.

## Terminal compatibility and reading behavior

Lector launches the child with `TERM=xterm-256color`. It removes any inherited
`TERMINFO`, then supplies a target- and content-keyed private cache entry with
the implemented `Sync` capability. When available, bounded `infocmp` and `tic`
runs adapt the host's own `xterm-256color` entry. Lector also embeds a compiled
ncurses entry and extracts it automatically on supported macOS and Linux
systems when those tools are unavailable, fail, or time out. If the normal
cache cannot be used, a process-owned temporary entry remains alive as long as
the child process needs it. Lector writes neither the system terminfo database
nor the user's normal terminfo database, and requires no setup command. This
lets an ordinary local nested tmux client bracket its redraws with DEC mode 2026
without changing the public terminal name sent over SSH. Lector also answers
direct `DECRQM 2026` capability queries. This is the widely installed
compatibility contract implemented by the compositor. Lector does
not inherit the physical terminal's vendor identity or advertise
`xterm-ghostty`: using Ghostty's parser does not mean Lector implements every
Ghostty extension. Device, mode, geometry, pixel-size, color-scheme, keyboard,
focus, and clipboard queries from Lector's root child are answered locally by
its Ghostty engine. tmux owns the PTYs behind a control connection and answers
those pane queries itself; Lector's pane engines are observational shadows and
discard their duplicate replies.
Lector puts DA1 last in its bounded physical-terminal startup probes and
consumes replies through that processing fence; those replies are never sent to
the application. A timeout releases pending input while retaining ownership of
the delayed reply vocabulary; the ordered DA1 response is the exact point at
which the broker relinquishes the input stream completely. The same probe set
reads the outer terminal's exact OSC 10/11
default foreground and background plus its native light/dark report when
available. Lector mirrors those values into OSC 10/11 and color-scheme replies
for direct children, including applications which query during startup; the
wait for outer replies is strictly bounded. For tmux-owned panes, Lector routes
the exact OSC 10/11 values through tmux's report API when tmux asks for them;
native color-scheme query handling remains tmux-owned. A semantic light/dark
report wins over a luminance guess while the exact colors themselves are
preserved, so one-shot adaptive-theme queries see the same startup defaults
they would see without Lector in the path.

Ghostty's parser recognizes more private modes than Lector implements end to
end. URXVT mouse encoding 1015, pixel-coordinate mouse mode 1016, live
color-scheme notifications 2031, and in-band resize reports 2048 are therefore
reported as unsupported and kept reset even when an application enables them
without querying first. This keeps applications on Lector's implemented mouse
encodings and normal PTY resize path instead of advertising a partially working
extension.

The private database is local process state: SSH normally sends the public
`TERM` name, not Lector's `TERMINFO` directory. A tmux client on a remote host
therefore uses its remote terminfo database and the bounded legacy stabilization
path unless that host independently advertises `Sync`. Direct mode queries can
still traverse SSH and be answered by the local Lector terminal.

Accessibility uses physically presented output: synchronized-output close,
validated linear-output records, an adaptive quiet window, and a hard deadline.
OSC 133 marks annotate semantic history and cursor restoration records painting
provenance; neither is a speech boundary. See [stabilization policy](performance.md#latency-decisions)
for timing and validation rules.

A cursor-addressed primary-screen repaint which hides the application cursor
and newly populates multiple previously blank rows is treated as a bounded new
interface or modal and that changed region is read in full. Cursor-addressed
transcript growth above a prompt cursor uses the inserted-text diff. Parallel
status changes remain readable during streaming output, typing, and mode
transitions: their position, length, or timing does not prove they are
incidental. When the application cursor moves while another row changes,
Lector reads the changed text and then tracks the destination. Key-echo
suppression consumes only an exact terminal acknowledgement of queued input;
it does not discard changes elsewhere in the same frame.

Deletion feedback waits for a confirmed edit. An unchanged presentation cannot
distinguish an ignored key from a delayed edit, so Lector retains the intent
within its bounded presentation lifetime. Incompatible redraws still discard
it, and no deleted character is spoken without confirming evidence.

For TUIs which hide or park the hardware cursor, or leave it at an input prompt,
Lector can also recognize two narrow visual-focus representations
without depending on a particular framework, color, or key binding. The common
gate requires a decoded key press with nonempty input actually sent to the
application and a causally later presented frame. Lector assigns no meaning to
the key itself, so application-defined bindings, access keys, and remaps use the
same path as arrows.

A style-only move must be either one exact reciprocal style transfer between
bounded text runs or a bounded row bundle of one or more reciprocal style
transitions between exactly two stable rows. In a bundle, at least one
component must move a selected style which is demonstrably rarer than its
baseline across meaningful text, and every
directional component must identify the same destination. This permits a theme
to style one bounded payload run, match fragments within it, and a separate
gutter independently without assigning application-specific meaning to the
exact color or emphasis values. A textual line pointer may instead move one
compact, punctuation-like token between the same
leading-gutter columns of two stable, aligned item rows. Its glyph is learned
from the frame, so `>`, Unicode pointers, and configured markers behave
identically; an unchanged copy at the prompt is irrelevant. A marker-only list
needs an unchanged peer row, while a two-row list needs corroborating reciprocal
styling. A marker printed directly against its item text is likewise accepted
only with reciprocal style evidence. Geometry, wrapping, links, cursor
visibility, and cursor shape remain exact; coordinates may differ only while
the cursor remains hidden. Global restyles, ambiguous style swaps, prompt
edits, spinners, marker replacements, multiple simultaneous moves, scrolling or
reordering, unrelated text updates, conceal/blink changes, and real cursor
movement stay on the existing reading paths rather than being guessed as focus.

The virtual terminal implements 256 colors, true color, OSC 8 hyperlinks, and
the ordinary `xterm-256color` contract, so an inherited `COLORTERM` remains
valid. Application-originated OSC 52 clipboard reads return an empty local
reply by default, while application-originated writes always enter Lector's
internal clipboard history. This terminal-effect policy is independent of
`lector.o.clipboard.default_register` and
`lector.o.clipboard.system_provider`; it never writes the host or
outer-terminal clipboard. Desktop notifications and unknown APC effects are
dropped. Titles, working directories, progress, hyperlinks, and bells remain
modeled Lector state. In the live scheduler, title, working-directory,
progress, clipboard, and notification events remain typed
until the scheduler applies their explicit output policy; sensitive clipboard
and notification payloads are never replayed as raw terminal bytes.

Inside Lector, after attaching a fresh ordinary tmux client, the loaded
capability can be checked with:

```sh
infocmp -1 -x xterm-256color | grep Sync
tmux info | grep Sync
```

The `tmux info` check applies only to an ordinary tty client. A pure `tmux -CC`
client has no rendering tty, so it is not expected to report or use `Sync`;
Lector stabilizes control-mode `%output` at its own compositor boundary instead.

Physical-terminal rendering starts conservatively, then applies terminfo,
bounded probes, and finally explicit environment overrides. The supported
overrides are `LECTOR_OUTER_COLORS` (an integer) and these boolean variables:
`LECTOR_OUTER_TRUE_COLOR`, `LECTOR_OUTER_HYPERLINKS`, `LECTOR_OUTER_SYNC`,
`LECTOR_OUTER_KITTY_KEYBOARD`, `LECTOR_OUTER_KITTY_GRAPHICS`,
`LECTOR_OUTER_FOCUS`, and `LECTOR_OUTER_CLIPBOARD_READ`. Boolean values accept
`true`/`false`, `yes`/`no`, `on`/`off`, or `1`/`0`. These describe the outer
terminal only; they do not change what Lector promises to applications.


## Ghostty adapter and composition

The `lector-ghostty` engine is mandatory across Cargo feature selections.
The local crate wraps the official C API with private raw
bindings and bootstrap-checked ABI layouts. Native handles remain on their
constructing thread. `GhosttyEngine` exposes snapshot and viewport methods;
`advance_update`, `resize_update`, `resize_with_geometry_update`, and
`reset_update` produce Lector's update summaries, alongside fallible inherent
and `try_*` methods.

The adapter normalizes Ghostty cells into Lector's grapheme, width, style,
hyperlink, wrapping, and history model. Background-only cells combine Ghostty's
cell content color with its style metadata, including when captured from
scrollback, so erased regions retain their application-selected color.
Lector exposes at most 10,000
scrollback rows. Ghostty's allocator prunes complete pages and therefore may
physically retain additional rows; the adapter disables the independent byte
cap, configures 20,000 lines of physical headroom, and clips its public model
to the newest 10,000 logical rows. Sparse review marks use Ghostty tracked grid references and become
invalid when their cell falls outside that logical window, even if an older
physical page has not yet been reclaimed.

Ghostty's public cell and row semantic APIs distinguish prompt, input, and
output content, but they do not expose every historical OSC 133 A/B/C/D
boundary or the exit status carried by `D`. The owned adapter therefore keeps
a narrow fragmented-stream OSC 133 observer alongside Ghostty and anchors each
completed boundary to Ghostty with a tracked grid reference. This preserves
exact command phases and exit status across scrolling and reflow without
treating Ghostty's current `cursor_at_prompt` boolean as semantic history.

Auto-read consumes an engine-neutral `UpdateSummary` produced by the same
advance call that mutates terminal state. It records printable runs, cursor and
explicit-scroll operation counts, Ghostty's official full/partial render
damage, normalized changed-row ranges, cursor and screen identity before and
after the write, synchronized-output mode, and whether the batch crossed an
actual false-to-true DEC private mode 2026 boundary. At that boundary the
adapter splits processing at the marker and captures a full screen-and-history
snapshot. Clearing the marker's mode bit yields the exact committed model from
immediately before the frame: preceding bytes in the same input slice are
included and following bytes are excluded, even when the escape sequence was
fragmented across reads.

Ghostty exposes the final cursor, screen, mode, and render state but does not
expose printable or cursor-operation callbacks. The owned adapter's
narrow `vte` stream observer therefore collects operation hints, OSC 133
boundaries, and the active OSC 8 URI needed to annotate semantic operation
hints. For semantic rendering it speculatively tracks only the
cursor, scroll margins, origin, autowrap, and horizontal-margin mode required
to locate an operation. It owns no cells, history, or second terminal grid,
and discards all hints unless its final cursor agrees with Ghostty's
authoritative state. Changed rows come from Ghostty's stateful global and
per-row dirty flags; the adapter acknowledges both layers after each snapshot
so damage cannot leak into a later frame.

The compositor maps pane-local dirty rows into clipped scene
coordinates and compares only those candidate cells with its confirmed
`PresentedScene`. Incremental runs use absolute cursor placement and
self-contained style and hyperlink state. Failed or partial physical writes,
resize, suspend/resume, changed image state, inconsistent shadows, and damaged
wrap metadata all invalidate the optimization and use the full-scene,
oracle-tested fallback. Text damage around unchanged images can still use the
incremental path without retransmitting image data.

When operation hints are consistent with the authoritative scene, the
compositor can apply full or partial scrolling, line/character insertion and
deletion, erasure, and adjacent ASCII write runs directly to both the physical
terminal and a cloned shadow. It accepts that shadow only after affected rows
match Ghostty's intended scene and the complete transaction is flushed. Any
occlusion, clipping, media changes, wide-cell ambiguity, geometry mismatch,
unsupported operation, or failed validation falls back to the same
dirty-region or full-scene correctness path. The release benchmark checks
semantic-path coverage and compares bytes and latency with a pure dirty-region
renderer on both a tmux-style structural workload and a Zellij-style layered
redraw workload. Both retain an independently constructed full-redraw result as
the correctness reference; the checked-in baseline gates semantic coverage,
bytes, latency, scheduler bounds, peak RSS, and media throughput.

OSC 8 hyperlinks remain part of the modeled cell state and are closed at every
physical transaction boundary. Ghostty-decoded Kitty graphics live in bounded
pane-scoped stores: uploads and placements have separate lifetimes, unchanged
pixel buffers are shared, and scene composition maps pane-local identifiers to
collision-safe outer identifiers. Placements are clipped to pane and overlay
geometry, including partial occlusion; hidden uploads can remain cached while
their placements are removed and later restored without retransmission. The
outer backend chunks uploads, deletes stale placement and upload identifiers,
and conservatively rebuilds its cache after physical-state uncertainty. A
terminal without advertised Kitty support receives the complete text scene and
no graphics protocol. The default limits are 32 MiB per image, 64 MiB per
pane, 128 MiB per scene, and 4,096 placements.

Terminal side effects use the same pane boundary. Ghostty callbacks copy bell,
title, working-directory, clipboard-write, desktop-notification, progress,
query, and bounded unknown-sequence data into owned typed events before the
callback returns. The callback userdata is boxed at a stable address, accessed
only during synchronous terminal writes, drained after those writes return,
and dropped after the Ghostty terminal handle. Unsupported APC payloads are
limited to 256 bytes.

Focus reporting, mouse protocol and encoding, bracketed paste, and Kitty
keyboard flags are normalized into the terminal snapshot. Lector's existing
focus, paste, mouse, and keyboard forwarding reads that pane-local state. Title
and working-directory values are retained exactly as applications report them,
including empty clears and raw `file://` URIs; they are not interpreted as SSH
destinations.

Resize handling reads the outer terminal's complete Unix `winsize`, derives
per-cell pixel dimensions from its cell and grid-pixel fields, and routes the
same geometry to both the child PTY and the pane model. The PTY receives rows,
columns, and total grid pixels before the child is notified with `SIGWINCH`;
Ghostty receives rows, columns, and per-cell pixels. Cell-only callers remain
supported and explicitly carry zero pixel dimensions. Ghostty's primary-screen
reflow and alternate-screen crop behavior are authoritative.

The adapter enables a bounded 64 KiB replay continuation and exposes an
explicitly diagnostic Ghostty snapshot round-trip. Automated tests encode and
restore visible state, scrollback, geometry, modes, styles, and unfinished
UTF-8 and CSI input. This is not used by live Lector behavior or presented as a
stable persistence format: Ghostty snapshot format version 1 is unstable, and
Lector's auxiliary OSC 133 history anchors are not serialized by this
diagnostic path.

The compositor gives the root terminal and every stacked Lector view a stable
scene layer. Hidden engines keep parsing; a fully occluded update may complete
with zero physical bytes. Popping a layer recomposes current snapshots. Frozen
Review and table setup own independent cursors. Announcement/error popups report
dismissal, and confirmations distinguish Enter acceptance from Escape cancellation.

After the initial focus-mode query, stdout is nonblocking and registered for
writable readiness only after `EAGAIN`. Scheduler writes retry `EINTR`, resume
partial transactions without interleaving, and restore descriptor flags before
terminal cleanup. Modeled updates have a 64 KiB per-drain write budget.
Replacement renders compare the complete viewport with the confirmed scene,
including earlier unflushed changes. Typed title, working-directory, progress,
clipboard, and notification effects receive policy at activation; their bounded,
UTF-8-safe payloads count toward retention even under backpressure. Zero-byte
secure-policy effects complete normally.

## Data flow

```text
application PTY
  -> tmux gateway/parser when applicable
  -> pane-local Ghostty engine
  -> normalized terminal view
  -> composed scene
  -> incremental renderer + accessibility bundle
  -> output scheduler (replace/write/flush)
  -> physical terminal + presented accessibility model
```

Physical input is decoded into either Lector commands or application input.
Direct application input goes to the root PTY; tmux pane input is encoded as
control commands. Engine-generated replies are returned only to the PTY that
owns the engine. Pane engines that shadow tmux-owned PTYs discard duplicate
replies because tmux is authoritative there.

## Source map

- `src/main.rs` owns process setup, polling, signal handling, and lifecycle.
- `src/app.rs` and `src/app/` orchestrate input, views, presentation, and tmux
  interactions. `TmuxConnectionState::step_control` in `src/app.rs` owns
  connection-local inventory and flow transitions; `App::process_tmux_control`
  applies their results in protocol order.
- `src/terminal.rs` owns the concrete Ghostty adapter and normalized update,
  snapshot, and accessibility evidence contracts. Shared cell, row, style,
  cursor, mode, and effect leaf types are re-exported from `lector-ghostty`;
  consumers use this boundary rather than a parallel terminal implementation.
- `src/terminal_protocol.rs` handles terminal protocol observations and policy;
  `src/terminal_input.rs` encodes application input.
- `src/presentation.rs` and `src/output_scheduler.rs` compose, render, order,
  and recover physical output. Presentation owns scenes and receipt bundles;
  the scheduler owns physical write/flush order, not accessibility policy.
- `src/screen_reader.rs`, `src/screen_reader/`, and `src/commands/` implement
  speech policy, tracking, and user actions.
- `src/view.rs` owns accessible-document state, text caches, comparison epochs,
  and the bounded accessibility journal. `src/views/` owns concrete surfaces
  and the overlay stack.
- `src/tmux_*.rs` contain protocol, topology, pane, input, prefix, and lifecycle
  boundaries. `src/tmux_model.rs` owns UI-independent topology and notification
  reconciliation.
- `src/speech/` contains Lector's speech manager, process client, supervisor,
  and asynchronous isolation.
- `src/lua/` exposes configuration and the REPL. `src/lua/access.rs` owns
  scoped reader access; `src/lua/options.rs` owns Lua option names, aliases,
  conversion, and errors. Reader/speech storage, defaults, and invariant-enforcing
  setters remain with their Rust owners.
- `crates/lector-ghostty/` is the safe Rust boundary around the native Ghostty
  terminal engine.
- `crates/lector-tts/` contains the cross-platform TTS library, standalone
  `lector-tts` host, shared protocol schema, and the entrypoint used by
  `lector tts`.
- `src/bin/` contains support tools, test servers, and benchmarks declared
  explicitly in `Cargo.toml`.
- `tests/` contains integration and policy tests; `tests/fixtures/pty/` contains
  Lua configuration fixtures. Live PTY byte producers are first-party binaries
  declared in `Cargo.toml`.
- `build_support/`, `crates/lector-ghostty/build.rs`, and `xtask/` implement the
  pinned native dependency workflow. `scripts/` contains developer and live
  test entry points.

## Invariants to preserve

- Application output always follows the modeled render path.
- One event-loop thread owns mutable UI, terminal, and tmux state.
- Spawn the PTY child before starting diagnostics or speech worker threads;
  Unix PTY launch performs post-fork setup.
- Load top-level Lua speech configuration before spawning its selected speech
  server; run `on_startup` only after server initialization and initial
  physical presentation.
- Each tmux pane has its own terminal engine and media namespace.
- The output scheduler is the only live physical-output ordering path.
- Queues and parser buffers have explicit bounds and defined overload behavior.
- Overlays are scene layers; they do not pause application parsing.
- Uncertain physical state falls back to an authoritative redraw.

The [documentation index](README.md) links the detailed tmux and build-system
contracts behind these boundaries.
