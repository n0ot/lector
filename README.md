# Lector

Lector is a terminal screen reader. It speaks what appears in your terminal and lets you review what’s on screen without disrupting the running program.

## What Lector does

- Reads new terminal output automatically as it appears.
- Lets you review lines, words, and characters independently of the app cursor.
- Helps navigate tables in terminal output.
- Provides a clipboard history for quick copy/paste.
- Can be customized with a simple Lua config file.

## Get started

Build a release from a fresh clone with the standard Cargo command:

```bash
cargo build --locked --release
```

Cargo automatically downloads, verifies, builds, and caches Lector's pinned
terminal engine when needed. Normal development and release builds reuse the
same cached native archive; installed Lector binaries have no extra runtime
dependency. Exact pins, supported targets, offline packaging, upgrades, and
maintainer diagnostics are documented in
[docs/ghostty-builds.md](docs/ghostty-builds.md).

For a concise map of runtime ownership, data flow, concurrency boundaries, and
source directories, see [docs/architecture.md](docs/architecture.md).
The [documentation index](docs/README.md) and [contributor guide](CONTRIBUTING.md)
collect the deeper design notes and verification commands.

Run Lector with your shell:

```bash
cargo run -- --shell /bin/zsh
```

The modeled compositor is Lector's only presentation path. Application PTY
bytes are parsed by Ghostty and never written directly to the physical
terminal. After its initial full reconstruction, the compositor consumes
Ghostty's cell/row damage and emits only changed runs. It keeps a confirmed
physical-terminal shadow and falls back to a full redraw after resize, failed
or partial writes, uncertain state, or unsupported content. For unobscured,
rectangular terminal updates it also translates validated scrolling,
line/character insertion and deletion, erasure, and adjacent write hints into
semantic VT operations. Clipped, overlaid, media-changing, ambiguous, or
inconsistent hints automatically return to the dirty-region or full-scene
oracle-tested path. Text damage around unchanged images remains incremental.

OSC 8 hyperlinks are reconstructed with an explicit text-only fallback and
are closed at every output-transaction boundary. Kitty graphics are decoded by
Ghostty into pane-scoped media stores, mapped to collision-safe outer IDs, and
clipped against panes and overlays. Pixel uploads and placement state have
separate lifetimes, so opening an overlay can remove placements and closing it
can restore them without decoding, copying, or retransmitting unchanged pixel
data. Default limits are 32 MiB per image, 64 MiB per pane, 128 MiB per scene,
and 4,096 placements; stale outer uploads are explicitly deleted. If the outer
terminal does not advertise Kitty graphics, Lector renders text and emits no
graphics protocol.

The application and every Lector overlay are independent scene layers. The
application terminal engine continues consuming output while
Message, Review, Lua REPL, table setup, or popup layers are visible, so closing
an overlay reveals the current composed source scene without replaying deferred
PTY bytes. Review and table setup retain frozen, independently navigable
snapshots. Reviewable announcement, error, and confirmation popups close with
`Enter` or `Escape`; confirmations report accept and cancel separately.

### tmux control mode

Run `tmux -CC` from a shell inside Lector to enter the accessible control-mode
integration. Each tmux pane keeps an independent Ghostty engine, scrollback,
review state, and media namespace; splits, hidden windows, overlays, images,
multiple servers, and nested SSH/tmux connections use the same compositor as
ordinary terminal mode. Lector discovers the server's actual prefix and
bindings instead of assuming `C-b` or `C-a`.

Press `M-C` while any tmux connection exists to open Lector's connection
manager. It can switch to another local, nested, or remote connection even
when the selected control process is silent or flooding. Its active row starts
with `*`; compact rows show the stable ID, optional custom label, and tmux
server host without repeating `connection` or `tmux`. With no tmux connections,
`M-C` announces `no tmux connections active`. Press `d` to
detach the selected connection tree gracefully, deepest child first. Confirmed `D`
first sends Control-backslash; if the transport remains stuck, Lector stops
parsing that stream as tmux control and exposes the underlying terminal or
parent pane so commands such as `detach-client` or an SSH `~.` escape can be
entered directly.

Lector requests bounded tmux output flow control, coalesces pause/resume, and
rebuilds pane text/history from an authoritative capture if tmux reports stale
incremental output. A capture cannot reconstruct images, partial parser state,
or already-consumed semantic metadata, so those limitations are explicit and
spoken rather than guessed. Setup, bounds, performance measurements,
troubleshooting, and recovery are in
[docs/tmux-completion.md](docs/tmux-completion.md); prefix and chooser behavior
is in [docs/tmux-prefix.md](docs/tmux-prefix.md).

For crash and hang diagnosis, the repository includes a socket-free hostile
control peer and a kill-bounded live suite covering malformed records, silent
and unread transports, active and hidden floods, window switching, and nested
SSH-like control sessions. See
[`docs/tmux-adversary.md`](docs/tmux-adversary.md).

After the initial outer focus-mode ownership query, all live presentation,
effect, bell, and lifecycle output passes through one serialized scheduler. It
coalesces modeled scene updates at event-loop boundaries with a 4 ms latency
budget, completes any started escape transaction before beginning another,
and keeps application input and terminal replies independent of presentation
backpressure. Child PTY output also yields after 32 KiB or 4 ms, checked after
each read of at most 8 KiB; synchronized output does not receive a larger turn.

Each render carries the exact accessibility state represented by its pixels.
Screen-reading and review commands advance only after that render successfully
flushes; coalesced, replaced, capacity-dropped, or backpressured candidates
remain private. Changed scrollback is revisioned and shared, so ordinary frames
do not copy the bounded history on every update. Raw application input remains
independent of presentation. Consequently, reading a cell and then sending
coordinate-based input has the normal UI time-of-check to time-of-use race.

DEC private mode 2026 gates when a scene may be presented. A real close makes
the final candidate eligible but does not make it readable before its physical
flush. A frame idle for 100 ms, or continuously open for 2 seconds, is released
as a bounded failure case so a broken application cannot freeze the terminal.
Once that exact partial render flushes, accessibility advances to the same
partial generation a sighted user sees; newer parser state cannot leak ahead of
it. Visible tmux panes commit as one composed generation, and overlay/base
announcements follow the physically completed active view. On outer terminals
that support synchronized output, Lector owns one global update boundary.
Audible bells follow the completed visual transaction.

### Virtual terminal capabilities

Lector launches the child with `TERM=xterm-256color` and removes any inherited
`TERMINFO`. This is the widely installed compatibility contract implemented by
the compositor. Lector deliberately does not inherit the physical
terminal's vendor identity or advertise `xterm-ghostty`: using Ghostty's parser
does not mean Lector implements every Ghostty extension. Device, mode,
geometry, pixel-size, color-scheme, keyboard, focus, and clipboard queries from
Lector's root child are answered locally by its Ghostty engine. tmux owns the
PTYs behind a control connection and answers those pane queries itself; Lector's
pane engines are observational shadows and discard their duplicate replies.
Lector puts DA1 last in its bounded physical-terminal startup probes and
consumes replies through that processing fence; those replies are never sent to
the application.

The virtual terminal implements 256 colors, true color, OSC 8 hyperlinks, and
the ordinary `xterm-256color` contract, so an inherited `COLORTERM` remains
valid. Clipboard reads return an empty local reply by default, clipboard writes
go to Lector's clipboard history, and desktop notifications and unknown APC
effects are dropped. Titles, working directories, progress, hyperlinks, and
bells remain modeled Lector state. In the live scheduler, title,
working-directory, progress, clipboard, and notification events remain typed
until the scheduler applies their explicit output policy; sensitive clipboard
and notification payloads are never replayed as raw terminal bytes.

Physical-terminal rendering starts conservatively, then applies terminfo,
bounded probes, and finally explicit environment overrides. The supported
overrides are `LECTOR_OUTER_COLORS` (an integer) and these boolean variables:
`LECTOR_OUTER_TRUE_COLOR`, `LECTOR_OUTER_HYPERLINKS`, `LECTOR_OUTER_SYNC`,
`LECTOR_OUTER_KITTY_KEYBOARD`, `LECTOR_OUTER_KITTY_GRAPHICS`,
`LECTOR_OUTER_FOCUS`, and `LECTOR_OUTER_CLIPBOARD_READ`. Boolean values accept
`true`/`false`, `yes`/`no`, `on`/`off`, or `1`/`0`. These describe the outer
terminal only; they do not change what Lector promises to applications.

Or use the `SHELL` environment variable:

```bash
SHELL=/bin/zsh cargo run
```

## Speech drivers

Lector defaults to the built‑in TTS driver. You can also run a proc‑based driver that speaks JSON‑RPC over stdin/stdout.

Select a driver:

```bash
cargo run -- --shell /bin/zsh --speech-driver tts
cargo run -- --shell /bin/zsh --speech-driver proc --speech-server /path/to/driver
```

### Proc driver protocol

The proc driver speaks line‑delimited JSON‑RPC 2.0. Each request is one JSON object per line, and each response is one JSON object per line.

Supported methods:

- `speak` params `{ "text": "...", "interrupt": true|false }`
- `stop` params `{}` or omitted
- `set_rate` params `{ "rate": 1.0 }`

Example response:

```json
{"jsonrpc":"2.0","id":1,"result":null}
```

### Proc stub server (tests)

There is a tiny proc server binary used by tests to validate the JSON‑RPC driver path without invoking system TTS. It’s called `proc_stub_server`.

### Example proc server (TTS)

Build the bundled TTS proc server and point Lector at it:

```bash
cargo build --release
target/release/lector-tts
```

Then run Lector:

```bash
target/release/lector --shell /bin/zsh --speech-driver proc --speech-server target/release/lector-tts
```

### Recording a diagnostic session

`scripts/lector-trace` is a transparent PTY shim for reproducing interactive
terminal problems while using the real TTS server. It records exact bytes in
both directions, Lector's `--log` diagnostics, and every speech RPC in separate
files. The trace contains everything typed, displayed, and spoken.

When `lector`, `lector-tts`, and `lector-trace` are installed together, run:

```bash
lector-trace --shell "$SHELL"
```

The launcher creates a timestamped directory under the system temporary
directory and prints its path before Lector starts and again after it exits.
Pass `--trace-dir /path/to/new-directory` to choose the location.

## How to use Lector

Think of Lector as having two ways to listen:

1) **Live reading**: Lector speaks new terminal output as it appears.
2) **Review mode**: You can move a “review cursor” around the screen to read past output without moving the application cursor.

If you ever forget keys, toggle **Help Mode** and press any key to hear what it does. (Default: `F1`.)

### Core actions (with defaults)

- **Stop speech** when it’s too noisy. Default: `M-x`.
- **Say the current overlay name**. Default: `M-w`.
- **Toggle auto‑read** if you want to hear only on demand. Default: `M-'`.
- **Toggle stop on focus loss** (interrupt speech when terminal focus leaves). Default: `M-g`.
- **Move and read** by line/word/character using the review cursor.
- **Set a mark and copy** text between the mark and the review cursor.
- **Toggle table mode** to navigate tables by row/column.

You don’t need to memorize everything. Help Mode will tell you what each key does.

### Review overlay (reading past output)

Press `M-r` to capture the current screen and retained scrollback in a frozen
Review overlay. Output from the running application continues in the
background, but cannot move or replace the snapshot. Press `q` to leave Review
and return to the underlying terminal or overlay. `Escape` never
closes Review: it cancels a pending count, motion, search, or visual selection.
Pressing it with nothing to cancel rings the terminal bell.
Review opens with its visible application cursor at the source view's review
cursor. Inside Review, the left-click action (`M-{`) places that visible cursor
at the independent review cursor instead of sending a mouse event to the
background application.

Review has its own dependency-free vi command parser:

- Move with `h`/`j`/`k`/`l` or the arrow keys. `w`, `W`, `b`, `B`, `e`, and
  `E` move by words; `0`, `^`, and `$` move within a line; `gg` and `G` move to
  the beginning and end. Counts work, so `3w` moves forward three words.
- `C-b` and `C-f` move by pages. Moving above or below the displayed page with
  `k` or `j` scrolls the frozen snapshot one line at a time.
- `zt`, `zz`, and `zb` place the cursor line at the top, center, or bottom of
  the terminal. `z<Enter>`, `z.`, and `z-` do the same and move to the first
  nonblank character. A count selects the one-based snapshot line first.
- `[p` and `]p` jump to previous and next OSC 133 prompt markers.
- `f`, `F`, `t`, and `T` find a character on the logical line; `;` and `,`
  repeat that find. `%` finds and jumps between matching `()`, `[]`, and `{}`.
- `/` and `?` search the complete frozen scrollback using regular expressions.
  `n` repeats in the same direction and `N` repeats in the opposite direction;
  searches wrap at the ends.
- `y` supports motions and counts, `yy` yanks lines, `yiw`/`yaw` (and the `W`
  variants) yank text objects, and `v`/`V` start character/line selections.
  Yanked text is placed in Lector's clipboard history and is ready for `F7`.

Invalid chords, unavailable prompt/search/find targets, unmatched `%` braces,
and motions past a boundary ring the terminal bell. Ordinary Lector review
commands (`M-u`/`M-o`, `M-j`/`M-l`, and so on) remain available in the overlay
and are bounded by its currently displayed page.

Lector retains up to 10,000 primary-screen rows. `M-r` works through both the
xterm Meta encoding used by non-Kitty terminals and Kitty keyboard events.
`M-PageUp`/`M-PageDown` and `M-Up`/`M-Down` pass through to the running
application.

When a shell emits the OSC 133 `B` input-boundary marker, ordinary unmodified
Up/Down history navigation speaks the recalled editable input without the
primary prompt. Readline does not emit a fresh marker for every history item;
Lector correlates the forwarded arrow with the redraw after the existing `B`
marker. Without OSC 133 integration, speech uses cursor/diff behavior.

### Copy/paste and clipboard history

- Set a mark with `F5`, move the review cursor, then copy with `F6`.
- Paste the current clipboard entry with `F7`.
- Speak the current clipboard with `M-c`.
- Cycle clipboard history with `M-[` (previous) and `M-]` (next).

## Table navigation

### Supported table types

Lector table mode supports:

- Pipe tables with `|` separators (with or without leading/trailing `|`), including separator/banner rows.
- Fixed-width terminal tables where columns are separated by vertical blank gutters.
- Manually-marked fixed-width tables using tabstops from a chosen header row.

### How to use table mode

1. Move the review cursor onto a row inside a table.
2. Press `M-t` to enter table mode.
3. Navigate and read:

- Move rows with `j`/`k`.
- Move rows with review-style keys `M-u` / `M-o`.
- Jump to top/bottom table row with `g` / `G`.
- Move columns with `h`/`l`.
- Jump to first/last column with `^` / `$`.
- Read the current cell with `i`.
- Read the current cell with review-style key `M-i`.
- Read the current column header with `H`.
- Move by word inside the current cell with `M-j` / `M-l`.
- Read current word inside the current cell with `M-k`.
- Move by character inside the current cell with `M-m` / `M-.`.
- Read current character inside the current cell with `M-,`.

4. Toggle automatic header speaking with `M-h` if needed.
5. Press `Esc` to exit table mode.

### Manual table setup (tabstops)

Use this when auto fixed-width detection is wrong for a screen layout.

1. Move the review cursor to the line you want to use as the header.
2. Press `M-T` to start tabstop setup mode.
3. On that header line:
- Move with `h` / `l`.
- Move by word with `w` / `b`.
- Jump to beginning/end with `^` / `$`.
- Toggle a tabstop with `t` (press again to remove).
4. Press `Enter` to commit tabstops and enter table mode, or `Esc` to cancel.

Manual tabstops are temporary and cleared when table mode exits.

## Clipboard history

Lector keeps multiple clipboard entries (not just one). You can cycle back and forth between them and paste the one you want.

## Configuration (Lua)

Lector reads a config file on startup:

- Linux: `~/.config/lector/init.lua`
- macOS: `~/Library/Application Support/lector/init.lua`

### Common options

```lua
-- speaking rate
lector.o.speech_rate = 1.0

-- how many symbols should be spoken
lector.o.symbol_level = "most"  -- "none", "some", "most", "all", "character"

-- live reading on/off
lector.o.auto_read = true

-- suppress terminal output that echoes recently typed keys (disabled by default)
lector.o.suppress_key_echo = false

-- interrupt speech immediately when terminal focus is lost
lector.o.stop_speech_on_focus_loss = true

-- tmux pane bells: "audible" (default), "spoken", or "off"
lector.o.tmux_bells = "spoken"
```

### Simple key customization

You can remap keys or add your own Lua functions:

```lua
-- map a key to a built-in action
lector.bindings["M-x"] = "lector.stop_speaking"

-- toggle stop-on-focus-loss behavior
lector.bindings["M-g"] = "lector.toggle_stop_speech_on_focus_loss"

-- send mouse clicks to a mouse-aware terminal application at the review cursor
-- these are the default bindings
lector.bindings["M-{"] = "lector.left_click"
lector.bindings["M-}"] = "lector.right_click"

-- add a custom command
lector.bindings["M-v"] = {
  "speak current time",
  function()
    lector.api.speak(os.date("%H:%M"), true)
  end,
}
```

### Lua hooks

Hooks let you respond to Lector events.

```lua
-- set a hook
lector.hooks.on_screen_update = function(ev)
  -- ev.screen / ev.prev_screen are full screen strings
end

-- unset a hook
lector.hooks.on_screen_update = nil
```

Available hooks:

```lua
-- lifecycle
lector.hooks.on_startup = function(ctx) end         -- ctx: { config_path, version, pid }
lector.hooks.on_shutdown = function(reason) end     -- reason: "exit" | "error"
lector.hooks.on_error = function(message, context) end

-- screen + live reading
lector.hooks.on_screen_update = function(ev) end    -- ev: { rows, cols, cursor_row, cursor_col, prev_cursor_row, prev_cursor_col, changed, overlay, screen, prev_screen }
lector.hooks.on_live_read = function(text, meta)    -- meta: { cursor_moves, scrolled }, return string or nil to suppress
  return text
end

-- speech
lector.hooks.on_speech_start = function(text, meta) end  -- meta: { interrupt }
lector.hooks.on_speech_end = function(text, meta) end    -- meta: { interrupt, ok }

-- navigation + mode
lector.hooks.on_review_cursor_move = function(pos) end   -- pos: { row, col, prev_row, prev_col }
lector.hooks.on_mode_change = function(old, new) end     -- "normal" | "table" | "table_setup"
lector.hooks.on_table_mode_enter = function(meta) end    -- meta: { top, bottom, columns, header_row, current_col }
lector.hooks.on_table_mode_exit = function() end

-- clipboard + input
lector.hooks.on_clipboard_change = function(entry, meta) end -- meta: { op, index, size }, op: "push" | "prev" | "next"
lector.hooks.on_key_unhandled = function(key, mode)          -- return true to consume
  return false
end
```

## Lua REPL

Lector has a built‑in Lua REPL so you can try commands while it’s running. Open it with `M-L`, experiment, then close it when you’re done.

- Press `C-l` to clear the REPL screen while keeping the `Esc to close` banner visible.
- REPL history is preserved after closing and reopening the overlay.
- Commands that start with a space and consecutive duplicate commands are not added to REPL history.

## Tips

- If you want Lector to read *only* what you ask for, turn off auto‑read.
- Use table mode when terminal output is column‑structured (CSV, tables, list views).
- If speech feels too fast or slow, adjust `lector.o.speech_rate`.

## Troubleshooting

- If nothing speaks, check that your system TTS works.
- If keys don’t behave as expected, toggle Help Mode and press the key to confirm its mapping.
