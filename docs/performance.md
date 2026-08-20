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
  fairness bound, then the newest scene is eligible at once.
- An unstarted queued render is replaceable against the still-confirmed
  physical scene. Only a render whose bytes have started, or whose receipt is
  waiting for flush, invalidates incremental rendering for its successor.
- Ordinary accessibility diffs retain the 30 ms quiet-window debounce and
  300 ms maximum delay. A DEC 2026 transaction is different: no working frame
  is presented or read, and an exact close-at-end marker is carried with its
  render receipt. The final frame can be read immediately after that receipt's
  successful flush. Output after the close clears the marker and uses ordinary
  stabilization.

These rules apply equally to a direct child terminal, an ordinary attached tmux
client, and a tmux `-CC` control client. Control mode adds parsing and
`send-keys -H` routing, but no timer-based delay.

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
