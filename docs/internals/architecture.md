# Architecture

Lector is a terminal emulator and a screen reader in one process. Bytes from
the application never pass straight through to your terminal. Lector parses
them into a model of the screen, adds its own interface on top, and draws the
result. The screen reader reads from that same model.

This page gives the overall shape. Deeper topics have their own pages:

- [Reading model](reading-model.md): what gets spoken and when.
- [Terminal](terminal.md): emulation, compatibility, and rendering.
- [tmux](tmux/README.md): control-mode integration.

## Data flow

```text
application PTY
  -> tmux gateway and parser (control mode only)
  -> per-pane Ghostty terminal engine
  -> normalized terminal view
  -> composed scene
  -> incremental renderer + accessibility bundle
  -> output scheduler
  -> physical terminal + presented accessibility model
```

Input travels the other way. Lector decodes each key into either a Lector
command or application input. Application input goes to the root PTY, or in
tmux control mode is encoded as a tmux command for the active pane.

The most important idea is the last line of the diagram. Lector keeps two
versions of each screen: the **live** model, which reflects every byte parsed
so far, and the **presented** model, which reflects what has actually been
written to the physical terminal. The screen reader only ever reads the
presented model, so speech never gets ahead of the screen. The
[reading model](reading-model.md) explains this in detail.

## Threads

### The event loop

One thread runs a `mio` event loop and owns all mutable interface state:
`Process`, `App`, terminal engines, tmux state, views, and presentation
state. Nothing is shared behind locks. The Ghostty adapter enforces this by
making its native handles non-`Send`.

All I/O on this thread is nonblocking and handled in bounded turns, so no
single source can starve the others:

- Terminal input and child PTY output each yield after 32 KiB or 4 ms, checked
  between reads of at most 8 KiB. If data remains, they ask for another turn.
- One PTY drain is one presentation transaction. Every read is parsed into the
  live model before the next read, and replies stay in stream order. The scene
  is composed and captured once, at the end of the drain.
- `OutputScheduler` is the only path to the physical terminal. It bounds
  queued work, finishes a partly written escape sequence before starting
  another, and can replace rendering that has not started with a newer scene.

### Workers

Anything that might block runs on a bounded worker thread.

- **Speech.** The speech supervisor runs the speech server process and all its
  pipe I/O. Its mailbox drops stale speech under overload and puts
  cancellation and setting changes first. Results reach the event loop as
  events, not by polling. See the
  [speech-host protocol](../protocols/speech-host.md) for the queue and
  failure rules.
- **Opening links.** One worker launches the desktop URL opener for Review's
  `gx`, waits for it, and reaps it. A second request while one is running
  reports "busy". Shutdown does not wait for or kill a browser.
- **Diagnostics.** Log records go into a byte-bounded queue. The event loop
  never does file or stderr I/O itself.

## Startup order

The order matters and should not change:

1. Spawn the application PTY, before any worker threads exist. Unix PTY launch
   does work after `fork` that is unsafe with other threads running.
2. Create the Lua VM and run `init.lua`. Top-level code chooses the speech
   server.
3. Start and initialize the speech server, with one retry on failure.
4. Activate the physical terminal, start capability probes, and present the
   child's initial output.
5. Call the `on_startup` hook, then enter the event loop.

## Lua

Lua runs on the event-loop thread. Rust code holds the state Lua wants to
read and change, so access is lent to Lua only for the duration of each call
into it (`src/lua/access.rs`):

- Each config evaluation, hook call, binding resume, and REPL resume installs
  a scoped reference. Returning, yielding, erroring, and unwinding all remove
  it. Between calls, Lua holds no reference at all.
- A nested hook gets a child scope borrowed from its caller. Calls into Lua
  are limited to 32 deep per VM. Going past that is an ordinary hook error.
- Each configuration has a generation. A reload builds a new VM. If it fails,
  the live generation is untouched. If it succeeds, callbacks from the old
  generation can no longer run.
- The REPL keeps its own environment, independent of the configuration
  generation.

This uses safe Rust and mlua's scoped borrowing. There are no raw pointers.

## Views and the compositor

`View` is the common model for anything readable: the terminal, Review, the
Lua REPL, popups, and each tmux pane. Each view owns its terminal engine,
review state, and reading history.

The compositor gives the root terminal and each stacked view a layer in the
scene. Hidden engines keep parsing. An overlay does not pause the application
underneath it. Closing a layer recomposes from current state.

## Source map

| Path | Owns |
| --- | --- |
| `src/main.rs` | Process setup, polling, signals, lifecycle |
| `src/app.rs`, `src/app/` | Input, views, presentation, and tmux orchestration |
| `src/terminal.rs` | The Ghostty adapter, update summaries, snapshots |
| `src/terminal_protocol.rs` | Terminal protocol observation and policy |
| `src/terminal_input.rs` | Encoding input for the application |
| `src/presentation.rs` | Scenes and render receipts |
| `src/output_scheduler.rs` | Physical write and flush order |
| `src/screen_reader.rs`, `src/screen_reader/` | Speech policy and tracking |
| `src/commands/` | User actions |
| `src/view.rs` | Readable-document state, text caches, the accessibility journal |
| `src/views/` | Concrete surfaces and the overlay stack |
| `src/tmux_*.rs` | tmux protocol, topology, panes, input, prefix, lifecycle |
| `src/speech/` | Speech manager, process client, supervisor |
| `src/lua/` | Configuration, hooks, REPL; `access.rs` for scoped access, `options.rs` for option names |
| `crates/lector-ghostty/` | Safe Rust wrapper around the Ghostty terminal engine |
| `crates/lector-tts/` | TTS library, the `lector-tts` server, and the protocol schema |
| `src/bin/` | Support tools, test servers, benchmarks |
| `tests/` | Integration tests; `tests/fixtures/pty/` holds Lua fixtures |
| `build_support/`, `xtask/` | The pinned native dependency workflow |
| `scripts/` | Developer and test entry points |

## Invariants

Keep these true when changing Lector:

- Application output always goes through the modeled render path.
- One event-loop thread owns all mutable interface, terminal, and tmux state.
- The PTY child is spawned before any worker thread starts.
- Lua's speech choice is loaded before the speech server starts, and
  `on_startup` runs only after the server is ready and the first frame is
  shown.
- Each tmux pane has its own terminal engine and image namespace.
- The output scheduler is the only path to the physical terminal.
- Every queue and parser buffer has a bound and a defined overload behavior.
- Overlays are scene layers. They do not pause application parsing.
- When physical state is uncertain, fall back to a full redraw.
- The screen reader reads only what has been presented.
