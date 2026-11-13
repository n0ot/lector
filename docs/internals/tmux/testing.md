# Testing tmux support

tmux support is tested at three levels:

1. **Modeled tests** feed protocol bytes to Lector directly. They are fast,
   deterministic, and run everywhere.
2. **The adversarial peer** is a fake tmux that misbehaves on purpose, driven
   through a real PTY.
3. **Real-tmux tests** run an actual tmux server, but only inside Docker.

For the commands that run each level, see
[Contributing](../../../CONTRIBUTING.md#testing).

## Modeled tests

These live in the `tmux` test target. Physical output is checked against a
second Ghostty terminal acting as an oracle (`RenderOracle`).

| File | Covers |
| --- | --- |
| `tests/tmux_control_parser.rs` | Framing and records. Replays fixtures whole, split at every byte, and one byte at a time. |
| `tests/tmux_gateway.rs`, `tests/tmux_recovery.rs` | Split markers, byte ownership, portals, malformed streams, end of file, timeouts, and restoring the parent. |
| `tests/tmux_topology.rs` | Duplicate names, custom indexes, linked windows, renames, focus, out-of-order and dropped notifications, rollback, and server restart. |
| `tests/tmux_panes.rs` | Layout validation, bootstrap races, lasting pane state, reply matching, and incremental composition. |
| `tests/tmux_input.rs` | All 256 byte values, invalid UTF-8, command-shaped input, every mouse encoding, paste chunking, query ownership, resize storms, and `%pause` recovery. |
| `tests/tmux_prefix.rs`, `tests/tmux_interaction.rs` | Discovered commands, stable targeting, and choosers. |
| `tests/tmux_lifecycle.rs`, `tests/tmux_connections.rs`, `tests/tmux_nested.rs` | Cleanup, connection state, nested streams, and carrier windows. |
| `tests/tmux_bells.rs` | Bell scope, latching and rearming, stale-stream loss, configuration, and scheduler order. |
| `tests/tmux_completion.rs` | Flow control, resynchronization, and resource limits. |

Parser fixtures are described in
[`tests/fixtures/tmux-control`](../../../tests/fixtures/tmux-control/README.md).

## The adversarial peer

`tmux-control-adversary` is a standalone program that speaks control mode
and behaves badly. Lector runs it as its shell, so every scenario crosses the
real PTY, polling, compositor, input, logging, and shutdown code. It creates
no tmux socket and never touches a real tmux server.

### Scenarios

| Scenario | Behavior |
| --- | --- |
| `normal` | A two-window client that accepts input, window changes, and a normal detach. |
| `malformed` | Sends an invalid pane record, then ordinary shell output, then exits. |
| `silent` | Keeps reading Lector's input, but stops replying after bootstrap. |
| `no-read` | Stops reading Lector's input after bootstrap. |
| `flood` | Floods the active pane with output while still accepting commands. |
| `hidden-flood` | Floods an inactive window while the foreground stays usable. |
| `nested` | Embeds a complete second control client in the active pane, like SSH followed by a remote `tmux -CC`. |

The suite also runs the `normal` scenario against a speech server that
accepts a request and then blocks forever. This checks that a stuck speech
server cannot stall tmux input or detach, and that Lector can still shut
down.

### Run the suite

```sh
scripts/run-tmux-adversary --timeout 60 --nocapture
```

The script runs the suite under a watchdog that kills the whole process group
on timeout.

### Run one scenario by hand

Build Lector, the peer, and a silent speech server:

```sh
cargo build --bin lector --bin tmux-control-adversary --bin proc_stub_server
```

Write a config that uses the silent server, so no real speech is produced:

```lua
-- /tmp/lector-adversary.lua
lector.o.speech.server = { program = "target/debug/proc_stub_server" }
```

Then choose a scenario with `LECTOR_TMUX_ADVERSARY`:

```sh
LECTOR_TMUX_ADVERSARY=hidden-flood \
  target/debug/lector \
  --shell target/debug/tmux-control-adversary \
  --config /tmp/lector-adversary.lua \
  --log-file /tmp/lector-hidden-flood.jsonl
```

`M-C` stays usable during silent or flooding output. Use `d` to detach or
`D` to abandon the connection.

### Limits under test

The scenarios push on these limits. tmux-specific ones are listed in the
[limits table](README.md#limits).

| Boundary | Limit | When it is reached |
| --- | ---: | --- |
| One child PTY or input drain turn | 32 KiB or 4 ms | Schedule an immediate fair continuation |
| Synchronized-output hold | 100 ms idle, 2 s total | Release the latest render |
| Writes queued for a child PTY | 1 MiB | Terminate the wedged child |
| Speech mailbox | 256 KiB | Drop the oldest stale speech |
| Physical-output scheduler | 2 MiB | Replace or drop unstarted renders; cap bells |
| Diagnostic log | 2 MiB in memory, 16 MiB on disk | Keep the newest records |

### Diagnostics

Each run writes JSONL logs under `target/test-artifacts/tmux-adversary/`.

A byte record holds the total length, an FNV-1a checksum, a short escaped
preview, and counts of suppressed records. That keeps logs useful during a
flood without unbounded memory, disk, or terminal traffic. Producers never
wait for the log, and each later record says how many earlier records and
bytes were dropped.

## Real-tmux tests

Tests that need a real tmux server are ignored by ordinary Cargo and nextest
runs. They run only in a disposable container:

```sh
scripts/test-real-tmux-docker
```

The container supplies tmux and isolated sockets. Nothing touches the host's
tmux binary, server, or configuration. Tests wait on events with bounded
timeouts, never on fixed sleeps.

What they add over the modeled tests:

- **Input.** A child that echoes every byte it receives as hexadecimal gives
  an independent check of the whole input path: NUL, control bytes, UTF-8,
  invalid UTF-8, legacy Meta, Kitty keyboard, focus, mouse, bracketed paste,
  rapid resize, and a 128 KiB output flood.
- **Topology.** A second session with a truly linked window, parsed from the
  twelve real inventory replies.
- **Panes.** Split, resize, zoom, and close, checked against a second Ghostty
  terminal.
- **Gateway.** A real tmux lifecycle, with abrupt transport loss injected.
- **Bells.** A real server ringing a pane.
