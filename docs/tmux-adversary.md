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

Terminal, view-stack, tmux topology, parser, pane, and compositor state have
one owner: Lector's polling event-loop thread. No worker can mutate that state,
so ordering is explicit and there are no locks around the screen model. Each
poll turn drains only a bounded amount of PTY work before handling physical
input and scheduled output. Child output yields after 32 KiB or 4 ms, checked
after each read of at most 8 KiB. Reaching either limit requests an immediate
later turn without waiting for another readiness edge. DEC 2026 synchronized
output uses the same budget rather than extending a flood's ownership of the
loop.

Each pane continues mutating its live terminal model during an open DEC 2026
frame while accessibility reads the last physically completed generation. Raw
input remains routable immediately. A close makes the working frame eligible;
neither close nor timeout publishes it before the render flushes. An idle or
hard timeout may release a partial render, after which the exact flushed partial
generation becomes readable. A newer parser frame cannot be spoken ahead of
the pixels. All visible panes in a split carry receipts in the same composed
generation. Coordinate-based actions retain the ordinary time-of-check to
time-of-use race because the pane is not frozen.

Blocking external side effects do not run on that owner. The proc speech
backend and diagnostic writer each own one process-lifetime worker. Producers
only enter short in-memory queue critical sections; they never wait for backend
pipe I/O, a filesystem, or stderr. Speech overload discards stale announcements while preserving and
coalescing stop/rate controls. Logging overload retains the newest diagnostic
tail and records loss counters.

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

Exceptional transport controls are independent of ordinary tmux command/reply
flow. Thus a reply-silent connection cannot consume the escape needed to back
out. `M-C` opens the connection manager independently of the child; confirmed
uppercase `D` first sends Control-backslash to the selected connection's
transport. If the peer still does not exit within 750 ms, Lector abandons only
the control parser and exposes the still-live raw root terminal or parent pane.
This preserves the recovery channel for a raw `detach-client`, SSH escape, or
other transport-specific command instead of terminating Lector's PTY child.
Lowercase `d` is the normal graceful path and waits for nested descendants to
exit deepest-first before detaching their parents.

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
