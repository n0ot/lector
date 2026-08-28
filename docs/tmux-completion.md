# Complete tmux control-mode behavior

Lector treats `tmux -CC` as a source of pane-local terminal byte streams. tmux
remains authoritative for sessions, windows, pane geometry, layout, and process
lifetime; Lector owns each pane's Ghostty terminal engine, accessibility state,
scene composition, media namespace, and physical-terminal output.

Lector does not launch or configure tmux. Start it from the shell running under
Lector:

```sh
tmux -CC
```

The control-mode marker is detected automatically. The active tmux pane then
uses the same compositor, scheduler, overlays, review model, terminal query
broker, and Ghostty render oracle as ordinary terminal mode. Prefix discovery,
choosers, command entry, nested connections, and recovery are described in
[`tmux-prefix.md`](tmux-prefix.md) and [`tmux-gateway.md`](tmux-gateway.md).

## Images and scene changes

Kitty image uploads and placements belong to a stable Lector surface, not to a
tmux numeric image ID alone. Two panes or connections may therefore reuse the
same Kitty image and placement IDs without colliding in the outer terminal.
Placements are clipped to their pane rectangle and to overlay occlusion. Hidden
windows retain their pane-local media stores but contribute no placements to
the visible scene; switching back restores them without retransmitting an
unchanged upload.

Split changes, window changes, pane resize, overlay open/close, and stale outer
state all retain the full-scene correctness fallback. Pixel placement geometry
is derived from the physical cell dimensions when the inner tmux pane does not
know the outer terminal's pixel size. A terminal which does not advertise Kitty
graphics receives the complete text scene and no Kitty protocol bytes.

## Flow control

At control-client startup Lector sends this policy before its inventory:

```text
refresh-client -f pause-after=1
```

It then reads `#{client_flags}` and requires the returned flag set to contain
`pause-after=1`. A successful command reply alone is not treated as proof that
the policy stuck.

This asks tmux to pause a pane after one second of unread control-client output
instead of allowing an unlimited backlog. tmux pause is deliberately lossy: it
discards queued pane blocks and advances the control client's pane offset while
new output is ignored. `%pause %N` therefore marks Lector's shadow terminal as
stale; it is not merely a quiet state. If the pane is in the presented window,
Lector queues exactly one ordered resume command, even if tmux repeats the
notification:

```text
refresh-client -A '%N:continue'
```

Paused background panes stay paused until their window or connection is
presented. `%continue %N`, or a successful explicit-resume reply, confirms only
that incremental delivery restarted; neither can restore bytes discarded by
tmux. Lector follows the resume with an authoritative `capture-pane` and skips
incremental pane mutation until that capture completes. Output racing the
capture schedules one final capture after a short quiet interval, capped by a
one-second hard deadline from the first raced byte. Continuous output therefore
cannot keep the visible pane on an old recovery snapshot indefinitely. At that
deadline Lector explicitly pauses delivery, probes the pane while paused, then
sends `continue` and the final capture commands as one parsed tmux command
sequence.
tmux resets the pane-output offset on `continue`; the immediately following
snapshot therefore contains the paused interval, while output processed after
the sequence resumes as incremental delivery. This is the finite final round for
the recovery epoch rather than another quiet-period retry.
Input, inventory, capture, resize, and resume commands share one FIFO, so
recovery cannot overtake input already accepted for another protocol operation.
Failed resume or capture commands remain stale and retry with a bounded delay
while the pane is visible; they cannot leave a visible pane permanently paused
without another wakeup.

A pane carrying a nested `tmux -CC` stream is never treated as an ordinary
recoverable terminal. Lector continuously drains it, including while another
connection or overlay is visible, and links its stable window into the outer
client's attached session before a session switch. If tmux nevertheless emits
`%pause` for that pane, it has positively discarded framed protocol bytes.
Lector drops that nested connection instead of issuing `continue` and guessing
at an unrecoverable byte boundary.

Lector accepts every valid `%extended-output` record normally and records its
reported age as delivery-latency telemetry. The age is not a loss marker: tmux
uses `%pause` to report the lossy boundary, and a large age can also mean only
that intact output waited in a queue. Recovery therefore starts from `%pause`
or Lector's own bounded-backlog pause, never from an arbitrary age threshold.

If the installed tmux does not accept or retain `pause-after`, both policy
replies are consumed without corrupting later reply correlation and Lector
announces that automatic loss-bounded recovery is unavailable. Ordinary
`%output` remains functional, but upgrading tmux is recommended.

Lector also enforces fairness below tmux's flow control. One event-loop turn
reads at most 32 KiB from the root PTY, then gives terminal input, signals,
protocol replies, and queued commands another chance to run. While a pane stream
is contiguous, raw pane bytes are scanned for nested `tmux -CC` lifecycle
markers before ordinary terminal bytes for the presented window are modeled.
Visible damage from one bounded root-PTY drain is then composed and rendered
once before ready user input is dispatched. Background pane engines get a 4 KiB
immediate allowance and a round-robin 4 KiB drain turn. This is a single-threaded
bounded scheduler, not one worker thread per pane.

Once one pane's deferred terminal payload reaches 16 KiB, Lector proactively
requests tmux's documented `pane:pause` state instead of waiting for the
one-second fallback. The request coalesces per pane. Selection queues
`pane:continue` before the required capture. This is a resynchronization
boundary, not a replay boundary: tmux does not buffer the paused interval for
Lector.

The aggregate deferred background-terminal queue is limited to 64 KiB. If it
would exceed that limit, Lector drops the affected pane's queued terminal
payload and records a text-resynchronization requirement. Further raw bytes are
inert until recovery, including bytes that resemble nested control markers. The
recovery capture is delayed while the pane stays in the background. Selecting
that window or connection discards any older deferred payload and queues one
authoritative capture before normal incremental presentation resumes.
Foreground input and control commands therefore do not sit behind an unbounded
hidden-pane parse workload.

## Authoritative text resynchronization

For one stale interval Lector discards all further pane output without feeding
it to a terminal or side-effect parser. Once any bytes are missing, later bytes
may begin inside OSC, CSI, DCS, APC, UTF-8, or another stateful sequence; even a
standalone-looking control has no trustworthy meaning. Bells, terminal queries,
clipboard requests, notifications, and every other effect in that interval are
therefore suppressed together with screen mutations.

For an ordinary pane this discard happens before nested-control marker
detection as well, and the inactive marker detector is reset at the capture
boundary. A pane carrying a live nested tmux connection is transport-critical:
Lector keeps that carrier observable, and a reported loss fails the nested
connection rather than attempting to reconstruct its control protocol.

Recovery is an ordered snapshot transaction:

```text
display-message -p -t %N -F <pane capture metadata>
capture-pane -p -e -F -J -S - -t %N
capture-pane -p -P -t %N
display-message -p -t %N -F <pane capture metadata>
```

The first metadata sample chooses the current pane display or tmux-mode screen
(`-M`). On the primary screen, `-S -` includes exposed history; on the current
alternate screen, history is omitted. Lector deliberately does not use `-a`,
which selects `pane->base.saved_grid` rather than the currently displayed grid.
`-e` preserves styles, `-F` preserves the line flags tmux
exposes for prompt/output semantics and hyperlinks, and `-P` captures an
incomplete escape sequence. The second metadata sample must still match the
screen, dimensions, and tmux-mode basis of the capture; otherwise Lector rejects
the internally inconsistent snapshot and retries.

A successful transaction replaces that pane's terminal with the captured
text, exposed history, styles, hyperlinks, prompt/output boundaries, current
geometry and cursor, followed by the pending parser continuation. Live output
after the ordered transaction is processed normally. A pane removed while the
replies are in flight discards the stale transaction harmlessly. A failed
transaction remains stale and retries with bounded backoff while visible.

tmux capture data is not a serialized terminal emulator. Resynchronization can
restore the state listed above, but cannot faithfully recover:

- Kitty image uploads or placements;
- semantic metadata which tmux does not represent in `-F` line flags;
- exact historical wrap cells or terminal mode save/restore stacks;
- parser continuation if the installed tmux cannot complete the `-P` capture.

Lector drops unrecoverable media during replacement, records the applicable
limitations in pane flow state, and announces them after recovery. This is an
intentional correctness boundary: guessed parser or media state would be more
dangerous than a documented recovery limit.

## Resource bounds and measured workloads

The default retained-state bounds are:

- 64 KiB per control line or notification;
- 4 MiB and 65,536 lines per command reply;
- 4 MiB total pre-bootstrap pane output and 4,096 orphan pane records;
- 64 KiB of aggregate deferred background-pane terminal output, processed in
  4 KiB turns;
- 10,000 exposed scrollback rows per pane;
- 32 MiB per image, 64 MiB per pane, 128 MiB per scene, and 4,096 placements;
- 2 MiB of scheduled physical output under the default scheduler;
- 4,096 live connection records and pane gateway detectors, with a nesting
  depth of 64.

Repeated `%pause` events coalesce to one resume and one authoritative capture.
Resize storms coalesce to the latest geometry. Adjacent pane input batches
coalesce while preserving their ordering boundary. Unstarted visual renders
can be replaced by the newest authoritative scene, while partial transactions
and terminal effects complete without interleaving.

The macOS ARM64 release baseline includes 5,000-iteration renderer workloads
for both tmux-like structural edits and Zellij-like layered redraws. The
checked-in completion measurements were:

| Workload | p95 render | Output/full-scene ratio | Semantic coverage | Semantic/pure-diff bytes |
| --- | ---: | ---: | ---: | ---: |
| tmux-like structural edits | 67.0 us | 2.87% | 100% | 9.02% |
| Zellij-like layered redraws | 72.7 us | 2.82% | 83.32% | 45.34% |

The mixed tmux soak uses 16 panes across eight windows while switching,
resizing, emitting output, images, and bells. It compares every presented
frame with a second headless Ghostty terminal, asserts bounded scrollback and
media, records process CPU time and PTY-to-render p95, and constructs a full
redraw alongside every incremental result. Separate harnesses measure 10,000
bytes of key-to-PTY queuing/encoding and 100 rapid connection switches. The
benchmark also gates peak RSS, scheduler backlog/recovery, and Kitty media
throughput. Real-PTY direct, attached-tmux, and control-mode key-to-pixel gates
are documented separately in [performance.md](performance.md).

The final macOS ARM64 release soak retained 1,056 scrollback rows, 2,336 text
bytes, and 64 image bytes across its 16 panes (averages of 66 rows, 146 text
bytes, and 4 image bytes per pane). Its PTY-to-render p95 was 404 us and the
192-frame workload used 144 ms of process CPU. It emitted 157,163 bytes versus
176,914 bytes for the independently constructed full redraws. The final
benchmark run measured 67 us tmux-like and 73 us Zellij-like p95, a maximum
127-byte scheduler backlog, and 161 MiB/s Kitty recomposition throughput.

Run the completion gates with:

```sh
cargo nextest run --locked --all-targets
cargo test --release --test tmux tmux_completion:: -- --nocapture
cargo ghostty-bench
```

Real tmux harnesses require `tmux` on `PATH` and permission to create disposable
Unix sockets beneath `target/test-tmux`. CI builds and checks linked Ghostty
metadata on all four Tier 1 targets and runs the complete suite on Linux
x86-64.

## Troubleshooting and recovery

- If Lector stays on its ordinary terminal view, confirm that the program
  really emitted the `tmux -CC` control marker; ordinary `tmux` does not use
  control mode.
- If a pane is reported paused, Lector should queue one `:continue` command
  when that pane is visible, followed by an authoritative capture. Repeated
  pauses do not imply repeated commands. Persistent recovery failures usually
  indicate that the control PTY cannot accept writes or the tmux client has
  disconnected.
- A spoken resynchronization message means tmux paused lossy pane delivery or
  Lector deliberately bounded an overloaded pane. Text/history and recoverable
  parser metadata were rebuilt, but rerun image-producing output if that state
  matters.
- A spoken resynchronization failure means `capture-pane` failed. The pane will
  remain stale and retry while visible; switching windows or restarting the
  control client may help if failures persist.
- On malformed protocol, missing terminators, server death, SSH loss, or PTY
  EOF, Lector removes the failed connection, releases its pending state, and
  returns to the nearest live parent connection or ordinary terminal. Gateway
  graceful teardown and raw-transport fallback are documented in
  [`tmux-prefix.md`](tmux-prefix.md).

The primary automated coverage is in `tests/tmux_completion.rs`, with real
transport and lifecycle coverage distributed across the `tmux_*` integration
harnesses. No supported test is ignored; live fixtures use bounded event waits
rather than timing sleeps.
