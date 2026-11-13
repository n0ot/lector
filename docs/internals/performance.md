# Performance

Lector sits between you and every program you run, so it must not add delay
you can feel. This page covers the design choices that keep it fast, the tests
that keep it that way, and how to measure a slow path.

## Why it is fast

- **No intentional visual delay.** One PTY turn drains what is ready, up to
  32 KiB or 4 ms. Every read updates the model at once, but the scene is
  composed only once, at the end of the turn, and is eligible to draw
  immediately.
- **Stale frames are replaced, not drawn.** A queued render that has not
  started can be swapped for a newer one. Only a render whose bytes are
  already going out forces its successor to do extra work.
- **Speech never blocks the terminal.** All speech I/O runs on a worker with
  absolute deadlines: five seconds to initialize and one second per call.
- **No timers on the input path.** tmux control mode adds parsing and
  `send-keys` routing, but no timer-based delay.
- **Reading waits for evidence, not a clock, where it can.** See
  [when output is stable](reading-model.md#when-output-is-stable).

## Latency gate

`tests/live_pty.rs` drives a real PTY and measures 20 key-to-pixel samples
against a headless Ghostty oracle, using a project-built fixture.

| Measure | Limit |
| --- | ---: |
| Median | 25 ms |
| 95th percentile | 100 ms |

The median catches a reintroduced polling floor. The looser tail limit keeps
ordinary scheduling noise from making the test flaky.

```sh
cargo test --locked --test live_pty key_to_pixel_latency -- --nocapture
```

For reference, a local macOS run on 2026-08-19 measured the following. These
describe that run and are not a guarantee for other machines.

| Transport | Median | 95th percentile |
| --- | ---: | ---: |
| Direct terminal | 1.22 ms | 1.40 ms |
| Ordinary attached tmux | 1.08 ms | 1.18 ms |
| tmux `-CC` | 1.38 ms | 1.43 ms |

## Atomic redraw test

A Neovim-style test in `tests/app.rs` opens a synchronized-output frame,
replaces several full-screen draws, and blocks the final flush. It checks
that no pixels or speech escape early, and that only the final text is read
once the closing render flushes.

```sh
cargo test --locked --test app neovim_atomic_redraw
```

## Release benchmark

The release benchmark runs the production compositor and checks results
against a checked-in baseline:

```sh
cargo run --locked --release \
  --bin lector-ghostty-bench -- \
  --check-baseline benchmarks/ghostty-release-baseline-macos-aarch64.json
```

`cargo ghostty-bench` is a shortcut for the benchmark run.

It gates semantic-rendering coverage, output bytes, latency, scheduler
bounds, peak memory, allocations, and image throughput.

### Workloads

| Workload | What it measures |
| --- | --- |
| One update per drain | The basic ingest-to-receipt path |
| Four reads per drain | That reads in one drain are composed once |
| One line per receipt at full history | That history receipts cost O(new rows), not O(history) |
| Scheduler coalescing and backpressure | Scheduler-owned allocation and retained bytes |
| tmux-style structural edits, Zellij-style layered redraws | Semantic rendering against a dirty-region renderer and a full-redraw reference |
| `split-scene-panes`, `split-scene-overlays` | `PresentedScene::compose` alone |
| `tmux-control-pane-output` | Control-mode parsing through to the receipt |
| 16-pane soak, input encoding, connection switching, image throughput | tmux-specific paths |

Things to know when reading or extending the report:

- The full-history workload pre-fills all 10,000 rows first. A return to
  copying the whole history then fails by a wide margin instead of hiding
  behind growth from empty.
- Each workload has a `measurement_scope`. `scene-composition` workloads
  exclude scene construction, VT rendering, and oracle replay from both timing
  and allocation counts. `ingest-through-receipt` workloads cover everything
  from input bytes to the completed receipt.
- The split-scene workloads use panes that do not cover the screen, negative
  origins, clipping, a hyperlinked row, and overlays inserted out of order, so
  the copy, occlusion, and ordering paths all run. Every measured composition
  is verified against rendered output.
- `tmux-control-pane-output` launches no tmux. A modeled handshake sets up a
  pane, then escaped `%output` records arrive in two fragments through
  `TmuxGatewayRouter`.
- New report fields are additive. Baseline consumers must require all six
  compositor workloads, as the benchmark contract test does.

Historical reports live in [`benchmarks/results`](../../benchmarks/results).
The [row-mutation report](../../benchmarks/results/row-mutation-01a7858/README.md)
is a good example. Each report describes its own machine and revision only.

## Speech isolation tests

Speech must never slow down the terminal. Tests cover this at several levels:

- Worker tests prove that a permanently blocked speech backend cannot block
  or grow the foreground mailbox.
- Protocol tests cover initialization and call deadlines, the 1 MiB frame
  limit, kill-and-reap recovery, and the 30-second restart window.
- The opt-in native speech suite checks that input still reaches the screen
  in under 100 ms while the real speech engine is talking. See
  [Contributing](../../CONTRIBUTING.md#native-speech-tests) for when and how
  to run it.

## Measuring a slow path

Run Lector with `--log-file PATH`. Records in the `latency` scope mark each
stage of a keystroke's round trip:

1. `input-received`: bytes arrive from the physical terminal.
2. `input-dispatched`: they are routed to the direct or tmux input path.
3. `child-pty-write`: the child PTY accepts them.
4. `source-output-read`: the response is read back.
5. `presentation-flushed`: the render reaches the physical terminal.
6. `accessibility-finalization-start`: the screen reader starts on a stable
   revision.
7. `accessibility-finalized`: speech has been handed to the worker.

Every record has a monotonic `elapsed_us` field, so subtracting two stages
gives the time between them. Logging goes through a bounded queue and adds no
blocking I/O to the event loop.
