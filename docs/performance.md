# Interactive performance

Lector's interactive path is single-owner and event driven: physical input is
forwarded, ready child output is drained in a bounded turn, the newest
authoritative scene is rendered, and its accessibility model is published only
after the physical output flush succeeds.

## Latency decisions

- Lector's terminal process does not pump `CFRunLoopRunInMode` or cap every
  `mio` wait at 10 ms. The default native TTS backend runs in an internal speech
  host process. Only that host services AVFoundation's main run loop, using a
  zero-wait pump on a 10 ms idle cadence. Cancellation settlement is likewise
  confined to the host.
- Native TTS and custom process speech both sit behind the bounded asynchronous
  speech worker. A slow pipe or AVFoundation utterance build cannot block
  terminal input, tmux processing, rendering, or teardown. On macOS the native
  host keeps non-interrupting utterances in its own bounded queue instead of
  flooding `AVSpeechSynthesizer` before lifecycle callbacks arrive.
- Speech transport uses nonblocking readiness and absolute deadlines: five
  seconds for initialization and one second for an ordinary RPC. Those clocks
  exist only while the speech worker has a call in flight. They never cap a
  `mio` wait, pump the terminal process's Core Foundation run loop, or poll on
  the main thread. A hung process becomes a bounded speech failure while the
  terminal continues at full speed. See the
  [speech protocol](speech-driver-protocol.md) for restart and shutdown policy.
- The output scheduler has no intentional visual delay. One PTY readiness turn
  already drains and coalesces immediately ready data up to its 32 KiB or 4 ms
  fairness bound. Every read mutates the terminal model and emits replies in
  order, while scene damage is accumulated without composing intermediate
  frames. The authoritative root or tmux scene is captured once at the drain
  boundary and is then eligible at once.
- An unstarted queued render is replaceable against the still-confirmed
  physical scene. Only a render whose bytes have started, or whose receipt is
  waiting for flush, invalidates incremental rendering for its successor.
- Accessibility stabilization is boundary-first. A DEC 2026 close is carried
  with its exact render receipt and becomes readable immediately after that
  receipt flushes. An OSC 133 prompt-start marker holds prompt auto-read until
  its `B` input boundary, which is another immediate semantic commit. Output
  after either boundary uses ordinary stabilization again.
- A confirmed Backspace or Delete result can be announced immediately after
  its physical receipt without finalizing the surrounding accessibility
  update. Confirmation requires both the expected logical cursor position and
  a changed input row, so a split echo containing only the first cursor-left
  operation remains behind the ordinary stabilization window. The complete
  update retains its ordinary semantic or quiet-window boundary.
- On the primary screen, a structurally safe print stream ending in LF or CRLF
  is another real boundary. After the matching physical receipt, Lector checks
  the completed logical record against the presented screen/history tail and
  reads that record without a quiet-window delay. The record journal preserves
  application newlines across physical wrapping and scrolling; a trailing
  fragment, cursor-addressed redraw, standalone carriage return, parser
  continuation, alternate screen, or missing journal range falls back to the
  screen model and stabilization policy. This is output-driven and has no
  Enter-key heuristic.
- Truly unmarked output uses a per-view adaptive quiet window. It starts at 30
  ms, is bounded between 8 and 60 ms, decreases only after repeated clean
  bursts, and increases immediately when a continuation arrives shortly after
  an ordinary finalization without intervening input. The 300 ms maximum delay
  remains the progressive-reading boundary for continuously changing output.

Direct mode and tmux `-CC` both retain the application's byte provenance, so
they share the complete-record path. An ordinary attached tmux client exposes
tmux's rendered VT stream instead; an inner application's newline or
alternate-screen boundary may be opaque there. Lector applies the same safety
classifier to the stream it can observe, but otherwise relies on tmux's outer
DEC 2026 transaction or the bounded quiet fallback. Cursor visibility is only
a painting hint: applications such as fzf and Neovim can restore it in the
middle of a larger redraw, so it is deliberately not a speech boundary.
Control mode adds parsing and `send-keys -H` routing, but no timer-based delay.

## Regression and latency gates

`tests/live_pty.rs` drives a real physical PTY and a headless Ghostty oracle.
It measures 20 key-to-pixel samples for each of direct terminal, ordinary tmux,
and tmux control mode. The median must remain below 25 ms and p95 below 100 ms;
the wider tail bound avoids turning normal host scheduling noise into flaky
failures while the median catches a reintroduced polling floor.

The 2026-08-19 local macOS run measured:

| Transport | Median key-to-pixel | p95 key-to-pixel |
| --- | ---: | ---: |
| Direct terminal | 1.22 ms | 1.40 ms |
| Ordinary attached tmux | 1.08 ms | 1.18 ms |
| tmux `-CC` | 1.38 ms | 1.43 ms |

The small control-mode delta supports treating tmux parsing and routing as
normal processing cost, not as the earlier timer-shaped latency source.

The release benchmark separately exercises the production compositor with one
update per drain, with four modeled reads per drain, and with one LF per receipt
after pre-filling the full 10,000-row history window. It counts allocations and
bytes as well as latency, output size, throughput, and completed physical
receipts. The at-cap workload makes a return to O(total history) receipt copies
fail by hundreds of times rather than hiding behind growth from an empty
window. Scheduler coalescing and backpressure workloads fence their allocator
counters around scheduler calls, excluding synthetic batch construction, and
have their own scheduler-owned allocation and retained-byte ceilings. These
gates catch both a return to per-read scene composition and a subtler regression
which keeps cumulative accessibility metadata behind a blocked physical writer.

```sh
cargo run --locked --release --features ghostty-vt \
  --bin lector-ghostty-bench -- \
  --check-baseline benchmarks/ghostty-release-baseline-macos-aarch64.json
```

The deterministic Neovim-style regression in `tests/app.rs` opens DEC 2026,
replaces several transient full-screen draws, blocks the final physical flush,
and verifies that neither pixels nor speech escape. It then verifies that only
the final text is read immediately after the exact close render flushes.

The native speech regressions mute AVFoundation and exercise the actual Lector
binary and its internal host. They verify that speech begins again after the
startup utterance, that a queued utterance can be stopped and replaced, and
that terminal input still reaches pixels in under 100 ms while the native host
accepts a deliberately long utterance. The worker's adversarial tests
separately prove that a permanently blocked backend cannot block or grow the
foreground mailbox. Protocol regressions cover initialization and ordinary
deadlines, strict 1 MiB framing, kill-and-reap recovery, and the boundary below,
at, and above the 30-second restart interval without adding a timer to the
interactive path.

Run the focused gates with:

```sh
cargo test --locked --test live_pty direct_native_speech_continues_after_startup_and_never_blocks_input
cargo test --locked --test proc_driver native_tts_server_advances_past_its_first_utterance
cargo test --locked --test app neovim_atomic_redraw
cargo test --locked --test live_pty key_to_pixel_latency -- --nocapture
```

Real tmux tests create private Unix sockets and may need permission when run in
a filesystem sandbox.

## Diagnostic timeline

With Lector diagnostics enabled, records in the `latency` scope mark:

1. `input-received` when bytes arrive from the physical terminal;
2. `input-dispatched` after they are routed to the direct or tmux input path;
3. `child-pty-write` when bytes are actually accepted by the child PTY;
4. `source-output-read` when the response is read from the direct or tmux PTY;
5. `presentation-flushed` after the exact render bytes successfully flush;
6. `accessibility-finalization-start` when a stable presented revision begins
   screen-reader processing; and
7. `accessibility-finalized` after speech has been handed to the bounded worker.

The common monotonic `elapsed_us` field makes these stages directly
subtractable without adding synchronous profiling I/O to the event loop.
