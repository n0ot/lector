# Adversarial tmux control peer

`tmux-control-adversary` is a standalone hostile control-mode peer. Lector can
spawn it as its shell, so every case crosses the real PTY, polling, compositor,
input, logging, and shutdown paths without creating a tmux socket or touching a
user's tmux server.

Build it, Lector, and the non-speaking test server:

```sh
cargo build --bin lector --bin tmux-control-adversary --bin proc_stub_server
```

Put this in `/tmp/lector-adversary.lua` so the run exercises custom process
speech without invoking system TTS:

```lua
lector.o.speech.server = { program = "target/debug/proc_stub_server" }
```

Then select a scenario with an environment variable:

```sh
LECTOR_TMUX_ADVERSARY=hidden-flood \
  target/debug/lector \
  --shell target/debug/tmux-control-adversary \
  --config /tmp/lector-adversary.lua \
  --log-file /tmp/lector-hidden-flood.jsonl
```

Supported scenarios are:

- `normal`: a two-window control client which accepts input, window changes,
  and a normal prefix-`d` detach.
- `malformed`: emits an invalid pane record and then recognizable parent-shell
  output before exiting.
- `silent`: continues draining Lector input but stops producing replies or pane
  output after bootstrap.
- `no-read`: stops draining Lector input after bootstrap.
- `flood`: continuously emits active-pane output while accepting commands.
- `hidden-flood`: continuously emits output for the inactive window while the
  foreground remains interactive and can switch windows.
- `nested`: embeds a second complete control client inside the outer active
  pane, modeling SSH followed by remote `tmux -CC`.

## Concurrency and resource model

The [runtime ownership model](architecture.md#runtime-ownership-and-concurrency)
keeps terminal state on the event-loop thread and blocking speech/logging on
bounded workers. [Latency decisions](performance.md#latency-decisions) define
fair turns and synchronized-output release. The scenarios below exercise those
boundaries through first-party fixtures without a tmux socket.

The principal limits are:

| Boundary | Limit | Saturation behavior |
| --- | ---: | --- |
| One child PTY drain turn | 32 KiB or 4 ms, checked per read of at most 8 KiB | Schedule an immediate fair continuation |
| One physical-input drain turn | 32 KiB or 4 ms, checked per read of at most 8 KiB | Schedule an immediate fair continuation |
| Application synchronized-output hold | 100 ms idle / 2 s absolute | Release the latest render; publish its exact accessibility receipt only after flush |
| Writes queued for a child PTY | 1 MiB | Terminate the owned wedged child |
| Deferred tmux pane output | 64 KiB globally | Pause, discard incrementals, authoritative resync |
| Immediate visible/hidden pane work | 4 KiB each per turn | Defer fairly to later turns |
| Unanswered tmux commands | 512 replies per connection | Discard new commands until replies resume |
| Recursively encoded nested command | 1 MiB / 4,096 commands | Discard that command; later input remains usable |
| Accumulated tmux inventory | 8 MiB / 65,536 lines per connection | Reject inventory and retry/resync |
| Proc speech mailbox | 256 KiB of accounted speech storage | Drop oldest stale speech |
| Physical-output scheduler | 2 MiB by default | Replace/drop unstarted visual work; cap bells |
| Diagnostic memory/file | 2 MiB / 16 MiB | Keep newest queue/file generation |

`M-C` remains available during silent or flooded output. Use `d` for graceful
detach or confirmed `D` for raw-transport recovery, as described in
[connection lifecycle controls](tmux-prefix.md#destructive-lifecycle-behavior).

Run the automated live suite through the process-group watchdog:

```sh
scripts/run-tmux-adversary --timeout 60 --nocapture
```

The live suite also runs the normal control peer with a proc speech server
which accepts a request and then blocks forever. This verifies that speech RPC
latency cannot stall tmux input or detach, and that Lector can terminate the
blocked helper during shutdown without joining its worker.

Each run writes structured JSONL diagnostics beneath
`target/test-artifacts/tmux-adversary/`. Byte records contain total length, an
FNV-1a checksum, a bounded escaped preview, and flood-suppression counts. This
keeps logging useful without making a flood consume unbounded memory, disk, or
terminal bandwidth. Producers never wait for the log sink, the in-memory queue
retains at most 2 MiB of the newest records, and a file log restarts at 16 MiB.
Every later record reports how many earlier records and bytes were dropped.
