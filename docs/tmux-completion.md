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

This asks tmux to pause a pane after one second of unread control-client output
instead of allowing an unlimited backlog. `%pause %N` moves that pane to an
explicit paused state and queues exactly one ordered resume command, even if
tmux repeats the notification:

```text
refresh-client -A %N:continue
```

`%continue %N` returns the pane to running state. Input, inventory, capture,
resize, and resume commands share one FIFO, so a resume cannot overtake input
already accepted for another protocol operation.

Lector accepts fresh `%extended-output` normally and records its reported age.
The current maximum accepted age is 5,000 ms. Older output means Lector cannot
prove that every preceding incremental byte is present, so that output is not
fed into the pane engine and the pane enters resynchronization.

If the installed tmux does not accept `pause-after`, the policy command's error
reply is consumed without corrupting later reply correlation. Ordinary
`%output` remains functional, but tmux cannot provide this bounded upstream
pause policy; upgrading tmux is recommended.

## Authoritative text resynchronization

For one stale interval Lector coalesces all further pane output and queues one:

```text
capture-pane -p -e -J -S - -t %N
```

A successful reply replaces that pane's terminal with the captured text,
styles, history, cursor, and geometry metadata. Live output after the ordered
capture reply is processed normally. A pane removed while the reply is in
flight discards the stale reply harmlessly. A failed capture is recorded and
spoken as a resynchronization failure; later fresh pane output is still
accepted so the connection does not become trapped.

tmux capture data is not a serialized terminal emulator. Resynchronization can
restore text and exposed history, but cannot faithfully recover:

- Kitty image uploads or placements;
- a partially received UTF-8, escape, or graphics sequence;
- semantic metadata already consumed from OSC 133 and similar protocols.

Lector drops unrecoverable media during replacement, records all three
limitations in pane flow state, and announces them after recovery. This is an
intentional correctness boundary: guessed parser or media state would be more
dangerous than a documented text-only recovery.

## Resource bounds and measured workloads

The default retained-state bounds are:

- 64 KiB per control line or notification;
- 4 MiB and 65,536 lines per command reply;
- 4 MiB total pre-bootstrap pane output and 4,096 orphan pane records;
- 10,000 exposed scrollback rows per pane;
- 32 MiB per image, 64 MiB per pane, 128 MiB per scene, and 4,096 placements;
- 2 MiB of scheduled physical output under the default scheduler;
- 4,096 live connection records and pane gateway detectors, with a nesting
  depth of 64.

Repeated `%pause` events coalesce to one resume. Resize storms coalesce to the
latest geometry. Adjacent pane input batches coalesce while preserving their
ordering boundary. Unstarted visual renders can be replaced by the newest
authoritative scene, while partial transactions and terminal effects complete
without interleaving.

The macOS ARM64 release baseline includes 5,000-iteration renderer workloads
for both tmux-like structural edits and Zellij-like layered redraws. The
checked-in Stop 3.13 measurements were:

| Workload | p95 render | Output/full-scene ratio | Semantic coverage | Semantic/pure-diff bytes |
| --- | ---: | ---: | ---: | ---: |
| tmux-like structural edits | 119.8 us | 3.93% | 100% | 12.37% |
| Zellij-like layered redraws | 123.6 us | 3.10% | 83.32% | 49.95% |

The mixed tmux soak uses 16 panes across eight windows while switching,
resizing, emitting output, images, and bells. It compares every presented
frame with a second headless Ghostty terminal, asserts bounded scrollback and
media, records process CPU time and PTY-to-render p95, and constructs a full
redraw alongside every incremental result. Separate harnesses measure 10,000
bytes of key-to-PTY queuing/encoding and 100 rapid connection switches. The
benchmark also gates peak RSS, scheduler backlog/recovery, and Kitty media
throughput.

The final macOS ARM64 release soak retained 1,056 scrollback rows, 2,336 text
bytes, and 64 image bytes across its 16 panes (averages of 66 rows, 146 text
bytes, and 4 image bytes per pane). Its PTY-to-render p95 was 778 us and the
192-frame workload used 168 ms of process CPU. It emitted 158,699 bytes versus
178,450 bytes for the independently constructed full redraws. The final
benchmark run measured 79 us tmux-like and 82 us Zellij-like p95, a maximum
111-byte scheduler backlog, and 93 MiB/s Kitty recomposition throughput.

Run the completion gates with:

```sh
cargo test --locked --all-targets
cargo test --release --test tmux_completion -- --nocapture
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
- If a pane is reported paused, Lector should queue one `:continue` command.
  Repeated pauses do not imply repeated commands. Persistent pauses usually
  indicate that the control PTY cannot accept writes or the tmux client has
  disconnected.
- A spoken resynchronization message means tmux reported output older than the
  accepted window. Text/history were rebuilt, but rerun image-producing or
  semantic-shell output if that state matters.
- A spoken resynchronization failure means `capture-pane` failed. The pane will
  accept later fresh output, but its preceding state is incomplete; switching
  windows or restarting the control client is the safest recovery.
- On malformed protocol, missing terminators, server death, SSH loss, or PTY
  EOF, Lector removes the failed connection, releases its pending state, and
  returns to the nearest live parent connection or ordinary terminal. Gateway
  interrupt, force-close, and SSH escape actions are documented in
  [`tmux-prefix.md`](tmux-prefix.md).

The primary automated coverage is in `tests/tmux_completion.rs`, with real
transport and lifecycle coverage distributed across the `tmux_*` integration
harnesses. No supported test is ignored; live fixtures use bounded event waits
rather than timing sleeps.
