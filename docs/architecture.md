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
  overload and prioritizes cancellation and rate changes. JSON-RPC pipe readiness and
  absolute deadlines are worker-local; completion or fatal failure reaches the
  main loop through an event-driven control path rather than a polling timer.
  The default native backend is hosted by `lector tts`, which imports the same
  `lector-tts` crate as the independently buildable host binary. Pipe I/O,
  native utterance construction, playback, and platform lifecycle work cannot
  stall terminal processing. Initialization identifies the selected backend
  and independently negotiates rate and voice operations. The host submits
  only Lector's one active utterance to the backend; the host-independent
  Lector manager keeps all never-submitted speech bounded.
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

Runtime speech sequencing is owned by a host-independent manager. It sends at
most one active utterance to a version 2 host, advances its bounded queue only
from correlated terminal evidence, and models playing, suspended, replacing,
and cancelling as distinct transitions. `M-x` toggles suspension; typing and
other ordinary input suspend in one direction. Both preserve the active item
and unsent queue. New non-interrupting speech appends while playback is active,
but replaces retained speech while suspended. Interrupting speech and the
explicit, unbound cancel action always replace or discard the retained queue.
Paragraph gaps are nonblocking manager deadlines which suspension can freeze.
Runtime speech-host replacement is transactional. A candidate process
initializes and restores the configured rate while the old generation remains
owned for rollback; only then is the active generation swapped and the old
child terminated and reaped. Transport failures never replay an uncertain
in-flight speech call.
The supervisor permits a restart only when the preceding recorded crash was at
least 30 seconds earlier, and otherwise asks the event loop to restore the
terminal and exit nonzero. The exact process and protocol contract is in
[`speech-driver-protocol.md`](speech-driver-protocol.md).

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

tmux control records remain ordered on the event-loop thread. Hidden panes do
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

Activation announcements use the title from the completed presentation bundle,
not a later logical controller value. This keeps both the selected document and
its spoken name tied to the same physically flushed scene.

Views removed from the logical stack remain owned only while they are named by
the last presented scene or by a pending/started render receipt. This covers
overlay dismissal, tmux connection and pane removal, portal teardown, and pane
resynchronization without turning permanent output backpressure into unbounded
retention. Terminal-title effects carry their own flush receipt because the
outer terminal can apply an OSC title before a larger cell render completes.

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

Accessibility commit policy is one ordered decision table shared by deadline
scheduling and finalization. An application-declared DEC 2026 close comes
first. OSC 133 prompt, input, command-start, and command-end marks remain
semantic history only; they do not participate in screen stabilization. A
structurally safe, LF-complete primary-screen record may commit after its exact
physical receipt when its reported text validates against the newly presented
logical-line tail. Whole-frame structural provenance remains sticky for the
fallback diff, while the terminal adapter separately records the ordered safe
print suffix after the latest ambiguity. The suffix composes across parser and
presentation fragmentation, so a later record which begins at a physical
record boundary can recover progressive reading after an earlier structural
batch. Mid-record ambiguity remains on the authoritative diff path. Cursor
restoration after recent input is a conservative legacy
boundary. Everything else uses the per-view adaptive quiet deadline and the
bounded hard deadline. Incomplete escape sequences cannot quiet-commit,
structural and title-only bursts do not train the timer, and context switches
discard transient deadline state.

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
  interactions.
- `src/terminal.rs` defines the normalized terminal contract and Ghostty-backed
  engine.
- `src/presentation.rs` and `src/output_scheduler.rs` compose, render, order,
  and recover physical output.
- `src/screen_reader.rs`, `src/screen_reader/`, and `src/commands/` implement
  speech policy, tracking, and user actions.
- `src/view.rs` and `src/views/` implement the application surface and Lector's
  overlay stack.
- `src/tmux_*.rs` contain protocol, topology, pane, input, prefix, and lifecycle
  boundaries.
- `src/speech/` contains Lector's speech manager, process client, supervisor,
  and asynchronous isolation.
- `src/lua/` exposes configuration and the REPL.
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
