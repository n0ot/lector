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
Message, Review, Lua REPL, or popup layers are visible, so closing
an overlay reveals the current composed source scene without replaying deferred
PTY bytes. Review retains a frozen, independently navigable snapshot, and its
table setup operates within that same full-history document. Reviewable
announcement, error, and confirmation popups close with
`Enter` or `Escape`; confirmations report accept and cancel separately.

### tmux control mode

Run `tmux -CC` from a shell inside Lector to enter the accessible control-mode
integration. Each tmux pane keeps an independent Ghostty engine, scrollback,
review state, and media namespace; splits, hidden windows, overlays, images,
multiple servers, and nested SSH/tmux connections use the same compositor as
ordinary terminal mode. Lector discovers the server's actual prefix and
bindings instead of assuming `C-b` or `C-a`.

Pane state follows tmux's stable `%pane` and `@window` IDs rather than session
names or window indexes. Moving or linking a window therefore preserves the
same Review cursor, terminal and media state, modes, and APC accessibility
settings. Returning to an ordinary session authoritatively refreshes panes
which may have changed while its control client was elsewhere. For a nested
`tmux -CC` running through SSH, Lector first creates and verifies a hidden
high-index carrier winlink in the destination session, so the framed child
stream remains live for hours without a tunnel or remote helper.

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

An in-place application title change or tmux rename updates Lector's model but
is not announced automatically. A context handoff announces the current title
only after the matching composed frame is physically stable; it does not read
the full `tmux session index title` label. Each overlay, base view, tmux pane,
and primary or alternate terminal screen is an independently checkpointed
accessible document. The checkpoint is the exact document presented when the
user left. Returning compares the current document with that checkpoint, so
only output which arrived while it was hidden is read; an unchanged return is
not introduced again. A context with no checkpoint reads its visible screen in
full. Resize/reflow and terminal reset increment the document's comparison
epoch, making an older checkpoint incomparable and conservatively introducing
the current visible screen.

For crash and hang diagnosis, the repository includes a socket-free hostile
control peer and a kill-bounded live suite covering malformed records, silent
and unread transports, active and hidden floods, window switching, and nested
SSH-like control sessions. See
[`docs/tmux-adversary.md`](docs/tmux-adversary.md).

After the initial outer focus-mode ownership query, all live presentation,
effect, bell, and lifecycle output passes through one serialized scheduler. It
coalesces modeled scene updates within each bounded event-loop turn and makes
the resulting scene immediately eligible, completes any started escape
transaction before beginning another,
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
The latency design, live direct/tmux gates, and diagnostic timeline are in
[docs/performance.md](docs/performance.md).

Auto-read treats retained history followed by the visible grid as one terminal
document. Ordinary bottom-margin scrolling is therefore an append: rows moving
from the grid into history keep their document position and are not announced
again. The parallel print observer remains a fast, causally precise path for
validated line-oriented output; structural or ambiguous output falls back to
the authoritative document diff. Cursor-addressed TUIs normally replace the
visible suffix, so they retain the existing fine-grained TUI diff behavior.

DEC private mode 2026 gates when a scene may be presented. A real close makes
the final candidate eligible but does not make it readable before its physical
flush. A frame idle for 100 ms, or continuously open for 2 seconds, is released
as a bounded failure case so a broken application cannot freeze the terminal.
Once that exact partial render flushes, accessibility advances to the same
partial generation a sighted user sees; review can inspect it immediately and
auto-read resumes through the ordinary quiet/streaming fallback from that
receipt. The stale opening marker is ignored until its real close, and newer
parser state cannot leak ahead of its pixels. Visible tmux panes commit as one
composed generation, and overlay/base
announcements follow the physically completed active view. On outer terminals
that support synchronized output, Lector owns one global update boundary.
Audible bells follow the completed visual transaction.

### Virtual terminal capabilities

Lector launches the child with `TERM=xterm-256color`. It removes any inherited
`TERMINFO`, then supplies a target- and content-keyed private cache entry with
the implemented `Sync` capability. When available, bounded `infocmp` and `tic`
runs adapt the host's own `xterm-256color` entry. Lector also embeds a compiled
ncurses entry and extracts it automatically on supported macOS and Linux
systems when those tools are unavailable, fail, or time out. If the normal
cache cannot be used, a process-owned temporary entry remains alive as long as
the child process needs it. Lector writes neither the system terminfo database
nor the user's normal terminfo database, and requires no setup command. This
lets an ordinary local nested tmux client bracket its redraws with DEC mode 2026
without changing the public terminal name sent over SSH. Lector also answers
direct `DECRQM 2026` capability queries. This is the widely installed
compatibility contract implemented by the compositor. Lector deliberately does
not inherit the physical terminal's vendor identity or advertise
`xterm-ghostty`: using Ghostty's parser does not mean Lector implements every
Ghostty extension. Device, mode, geometry, pixel-size, color-scheme, keyboard,
focus, and clipboard queries from Lector's root child are answered locally by
its Ghostty engine. tmux owns the PTYs behind a control connection and answers
those pane queries itself; Lector's pane engines are observational shadows and
discard their duplicate replies.
Lector puts DA1 last in its bounded physical-terminal startup probes and
consumes replies through that processing fence; those replies are never sent to
the application. The same probe set reads the outer terminal's exact OSC 10/11
default foreground and background plus its native light/dark report when
available. Lector mirrors those values into OSC 10/11 and color-scheme replies
for direct children, including applications which query during startup; the
wait for outer replies is strictly bounded. For tmux-owned panes, Lector routes
the exact OSC 10/11 values through tmux's report API when tmux asks for them;
native color-scheme query handling remains tmux-owned. A semantic light/dark
report wins over a luminance guess while the exact colors themselves are
preserved, so one-shot adaptive-theme queries see the same startup defaults
they would see without Lector in the path.

Ghostty's parser recognizes more private modes than Lector implements end to
end. URXVT mouse encoding 1015, pixel-coordinate mouse mode 1016, live
color-scheme notifications 2031, and in-band resize reports 2048 are therefore
reported as unsupported and kept reset even when an application enables them
without querying first. This keeps applications on Lector's implemented mouse
encodings and normal PTY resize path instead of advertising a partially working
extension.

The private database is local process state: SSH normally sends the public
`TERM` name, not Lector's `TERMINFO` directory. A tmux client on a remote host
therefore uses its remote terminfo database and the bounded legacy stabilization
path unless that host independently advertises `Sync`. Direct mode queries can
still traverse SSH and be answered by the local Lector terminal.

Accessibility stabilization uses declared boundaries before timers. A DEC 2026
close and an OSC 133 `B` prompt-input boundary commit immediately after their
exact render flushes. A structurally safe primary-screen record ending in LF
or CRLF is validated against those presented pixels and read immediately; this
keeps line-oriented programs snappy without inferring anything from the Enter
key. After recent user input, a redraw ending in a real hidden-to-visible cursor
transition is a conservative legacy hint. Remaining unmarked output starts
with a 30 ms quiet window, adapts per view between 8 and 60 ms, increases
immediately after a detected late continuation, and retains the 300 ms
streaming-output cap.

A cursor-addressed primary-screen repaint which hides the application cursor
and newly populates multiple previously blank rows is treated as a bounded new
interface or modal and that changed region is read in full. Cursor-addressed
transcript growth above a prompt cursor uses the inserted-text diff, and
prefix-preserving streaming extensions omit parallel status-row replacements.
Other settled replacement-style redraws use the ordinary inserted-text diff:
recent input or a stationary application cursor does not make changed text safe
to discard. When the application cursor actually moves while only an unrelated
single row changes, cursor tracking reads the destination instead of that
incidental ruler or status update. Key-echo suppression consumes only an exact
terminal acknowledgement of queued input.

For TUIs which hide or park the hardware cursor, or leave it at an input prompt,
Lector can also recognize two deliberately narrow visual-focus representations
without depending on a particular framework, color, or key binding. The common
gate requires a decoded key press with nonempty input actually sent to the
application and a causally later presented frame. Lector assigns no meaning to
the key itself, so application-defined bindings, access keys, and remaps use the
same path as arrows.

A style-only move must be either one exact reciprocal style transfer between
bounded text runs or a bounded row bundle of one or more reciprocal style
transitions between exactly two stable rows. In a bundle, at least one
component must move a selected style which is demonstrably rarer than its
baseline across meaningful text, and every
directional component must identify the same destination. This permits a theme
to style one bounded payload run, match fragments within it, and a separate
gutter independently without assigning application-specific meaning to the
exact color or emphasis values. A textual line pointer may instead move one
compact, punctuation-like token between the same
leading-gutter columns of two stable, aligned item rows. Its glyph is learned
from the frame, so `>`, Unicode pointers, and configured markers behave
identically; an unchanged copy at the prompt is irrelevant. A marker-only list
needs an unchanged peer row, while a two-row list needs corroborating reciprocal
styling. A marker printed directly against its item text is likewise accepted
only with reciprocal style evidence. Geometry, wrapping, links, cursor
visibility, and cursor shape remain exact; coordinates may differ only while
the cursor remains hidden. Global restyles, ambiguous style swaps, prompt
edits, spinners, marker replacements, multiple simultaneous moves, scrolling or
reordering, unrelated text updates, conceal/blink changes, and real cursor
movement stay on the existing reading paths rather than being guessed as focus.

The virtual terminal implements 256 colors, true color, OSC 8 hyperlinks, and
the ordinary `xterm-256color` contract, so an inherited `COLORTERM` remains
valid. Application-originated OSC 52 clipboard reads return an empty local
reply by default, while application-originated writes always enter Lector's
internal clipboard history. This terminal-effect policy is independent of
`lector.o.clipboard.default_register` and
`lector.o.clipboard.system_provider`; it never writes the host or
outer-terminal clipboard. Desktop notifications and unknown APC effects are
dropped. Titles, working directories, progress, hyperlinks, and bells remain
modeled Lector state. In the live scheduler, title, working-directory,
progress, clipboard, and notification events remain typed
until the scheduler applies their explicit output policy; sensitive clipboard
and notification payloads are never replayed as raw terminal bytes.

Inside Lector, after attaching a fresh ordinary tmux client, the loaded
capability can be checked with:

```sh
infocmp -1 -x xterm-256color | grep Sync
tmux info | grep Sync
```

The `tmux info` check applies only to an ordinary tty client. A pure `tmux -CC`
client has no rendering tty, so it is not expected to report or use `Sync`;
Lector stabilizes control-mode `%output` at its own compositor boundary instead.

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

## Speech

Lector uses native speech by default. `lector tts` runs its cross-platform
speech host, keeping system speech and platform event loops away from terminal
input and rendering. A normal Lector installation therefore remains
self-contained; there is no second executable to install on the same machine.

The same implementation is also built as the standalone `lector-tts` binary.
It is not Windows-specific: it can run wherever its selected backend is
supported, independently of whether the full Lector terminal application runs
there. This makes arrangements such as Lector on Linux with speech on macOS or
Windows possible over a user-provided stdio bridge.

Inspect and select native engines and voices with:

```bash
lector tts --list-backends
lector tts --backend av-foundation --list-voices
lector tts --backend av-foundation --voice VOICE_ID

# The standalone executable has the same options and protocol behavior.
lector-tts --backend nvda
```

The host reports its selected backend during protocol initialization. Rate,
voice listing, current-voice reporting, and voice selection are negotiated
independently. Lector does not call unsupported rate operations or invent a
`default` voice for externally managed backends such as NVDA.

Speech selection is Lua configuration, not a command-line driver setting:

```lua
-- The default.
lector.o.speech.server = "native"

-- Or start a custom server with an exact argument vector.
lector.o.speech.server = {
  program = "/opt/lector/bin/lector-tts",
  args = { "--backend", "speech-dispatcher" },
}
```

For example, an SSH stdio bridge can be expressed without any special Lector
transport support:

```lua
lector.o.speech.server = {
  program = "ssh",
  args = { "speech-mac", "lector-tts", "--backend", "av-foundation" },
}
```

SSH setup, authentication, reconnect behavior, and the security of that link
remain the user's responsibility.

Lector invokes `program` directly; it does not perform shell parsing. At
startup it loads `init.lua`, starts and initializes the selected server, and
then restores the configured speech rate only when the negotiated backend can
set it. Assigning `lector.o.speech.server` is a startup-only configuration
operation.

Speech timing, rate, and voice controls live in the same namespace:

```lua
lector.o.speech.rate = 1.25

-- Delay between paragraphs; 100 milliseconds by default.
-- Set to 0 to add no paragraph delay.
lector.o.speech.paragraph_pause_ms = 100

local voices = lector.o.speech.voices
if voices ~= nil then
  for _, voice in ipairs(voices) do
    print(voice.id, voice.name, voice.language, voice.gender)
  end
end

lector.o.speech.voice = "backend-provided-voice-id"
```

Paragraph pause is Lector presentation policy rather than a speech-host
setting. It must be a non-negative integer number of milliseconds, and changes
apply to paragraph boundaries in speech submitted after the assignment.

The negotiated operations remain independent. `rate`, `voice`, or `voices`
returns `nil` when the active host cannot report that value, and `voices` is a
read-only array of `{id, name, language, gender}` tables when listing is
available. Assigning an unsupported rate or voice, or selecting an ID absent
from an available voice list, raises a Lua error without changing the option.
In the Lua REPL the submitted chunk stops and the prompt remains available. In
`init.lua`, an immediately detectable error skips the rest of the chunk while
earlier successful assignments remain in effect. A value that only the
deferred speech-host handshake can reject fails the startup configuration
boundary instead. Both cases open an error overlay, and configuration errors
are also written to the structured diagnostic log when `--log` or `--log-file`
enables logging.

The Lua REPL and hooks can request a nonblocking, transactional runtime switch:

```lua
lector.api.set_speech("native")
lector.api.set_speech({
  program = "/opt/lector-speech/bin/other-server",
  args = { "--voice", "Samantha" },
})
```

The call returns immediately. Lector retains the old server for rollback while
the candidate initializes; speech requested during that handshake waits in the
bounded worker queue. A failed candidate leaves the old setting committed and
calls `lector.hooks.on_error(message, "speech-reconfigure")`.

Custom servers use the bidirectional version 2 speech-host protocol: bounded
UTF-8 NDJSON JSON-RPC 2.0 over stdin/stdout, with explicit capabilities and
correlated lifecycle/progress events. The canonical
[`crates/lector-tts/openrpc.json`](crates/lector-tts/openrpc.json) makes the
methods machine-readable,
and [the speech driver protocol](docs/speech-driver-protocol.md) defines exact
framing, initialization, deadlines, errors, process cleanup, and the 30-second
restart policy. Speech RPC and deadlines run only on the speech worker, so a
slow or hung server cannot add a polling floor or block Lector's terminal loop.

### Recording a diagnostic session

`scripts/lector-trace` is a transparent PTY shim for reproducing interactive
terminal problems while using the real TTS server. It records exact bytes in
both directions and Lector's `--log` diagnostics in separate files. With the
built-in native server, it also records every speech RPC, so the trace contains
everything typed, displayed, and spoken. A custom server can provide the same
speech trace by honoring `LECTOR_SPEECH_RPC_LOG`.

When `lector` and `lector-trace` are installed together, run:

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

- **Pause/resume speech** with `M-x`. Ordinary input also pauses speech. A
  capable host resumes at the beginning of the interrupted word; hosts with
  safe stop-completion evidence can instead restart the current utterance from
  its beginning. Pausing preserves queued announcements and paragraphs.
- **Cancel speech** has no default binding. Cancellation stops speech and
  discards the current utterance and everything queued behind it.
- **Say the current overlay name**. Default: `M-w`.
- **Toggle auto‑read** if you want to hear only on demand. Default: `M-'`.
- **Toggle stop on focus loss** (interrupt speech when terminal focus leaves). Default: `M-g`.
- **Move and read** by line/word/character using the review cursor.
- **Set a mark and copy** text between the mark and the review cursor.
- **Navigate terminal tables** from the frozen Review overlay.

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
  Yanked text is placed in the configured default register and is ready for
  `F7`. Prefix a yank with `""` for Lector's internal history or `"+` for the
  system clipboard; for example, `"+yiw` copies the inner word to the system
  clipboard.

Invalid chords, unavailable prompt/search/find targets, unmatched `%` braces,
and motions past a boundary ring the terminal bell. Ordinary Lector review
commands (`M-u`/`M-o`, `M-j`/`M-l`, and so on) remain available in the overlay
and are bounded by its currently displayed page.

Lector retains up to 10,000 primary-screen rows. `M-r` works through both the
xterm Meta encoding used by non-Kitty terminals and Kitty keyboard events.
`M-PageUp`/`M-PageDown` and `M-Up`/`M-Down` pass through to the running
application.

Resizing keeps the captured Review document frozen but creates a new viewport
for the new terminal geometry. Lector keeps the cursor at the same screen row
and column when possible. At the top, bottom, or horizontal content boundary,
it shifts the viewport to show additional surrounding content instead; a
narrower viewport pans without changing the cursor's logical position. Page
motions use the new height immediately.

When a shell emits the OSC 133 `B` input-boundary marker, ordinary unmodified
Up/Down history navigation speaks the recalled editable input without the
primary prompt. Readline does not emit a fresh marker for every history item;
Lector correlates the forwarded arrow with the redraw after the existing `B`
marker. If a temporary interface reuses that primary screen while the shell
marker remains, an exact visual-focus transfer takes precedence over the stale
semantic boundary. Without OSC 133 integration, speech uses cursor/diff
behavior. A stable multi-row repaint joined by terminal soft-wrap metadata is
read as one logical cursor line; because no semantic prompt boundary exists in
that fallback, the prompt may be included.

### Copy/paste and clipboard history

- Set a mark with `F5`, move the review cursor, then copy with `F6`.
- Paste the configured default clipboard register with `F7`.
- Speak the configured default clipboard register with `M-c`.
- Cycle clipboard history with `M-[` (previous) and `M-]` (next).

The default register is `"`, Lector's ten-entry internal history. The `+`
register is the system clipboard. Native system clipboard access works on
macOS, Windows, Wayland, and X11 hosts supported by `arboard`. The optional
`osc52` provider writes through the outer terminal instead, which is useful
when Lector is running remotely; OSC 52 cannot read the terminal's clipboard,
so system-register paste and read operations report that the provider is
write-only.

## Table navigation

### Supported table types

Lector table mode supports:

- Pipe tables with `|` separators (with or without leading/trailing `|`), including separator/banner rows.
- Fixed-width terminal tables where columns are separated by vertical blank gutters.
- Manually-marked fixed-width tables with custom column names and optional bounds.

### How to use table mode

1. Open Review with `M-r`. Its Vim cursor can reach every retained scrollback
   row, not only the visible page.
2. Move the Review overlay application cursor onto a table and press `gt` to
   detect it. `gt` always discards the previous active table before attempting
   detection; if detection fails, no table remains active. `gT` has the same
   replacement behavior before starting manual setup.
3. Move by logical cell:

- `[|`: previous cell, wrapping to the preceding row.
- `]|`: next cell, wrapping to the following row.
- `{|`: same column in the row above.
- `}|`: same column in the row below.

Cell jumps temporarily suppress ordinary cursor tracking and speak the row,
column label, and complete cell as separate utterances. Ordinary Vim motions
remain available and announce table, row, and column boundaries when crossed.
If an ordinary motion leaves the active table, a cell motion re-enters at the
first cell in its requested direction. The active table lasts until Review
closes or `gt` or `gT` replaces it.

Press `gH` on a column to use its cells as optional row headers. On a row
change, Lector speaks the row number, row-header cell, destination column, and
destination cell as separate utterances. Press `gH` on that column again to
turn row headers off, or press it on another column to move the designation.

### Manual table setup (tabstops)

Use this when automatic detection is wrong for a screen layout.

1. In Review, move to the table's header row or first data row and press `gT`.
2. Navigate with ordinary Vim motions and press `Space` at the start of each
   column. The first tabstop is the table's left boundary; the final column
   consumes the rest of each row by default.
3. Press `H` to switch between “headers from first row” and “no header row; use
   custom names or column numbers.”
4. Press `c` anywhere at or after a tabstop to edit that column's name. `Enter`
   saves the name, `Escape` discards the current edit, and `C-u` clears the
   field. Unnamed columns are announced as `column N`.
5. Optionally move to the final data row and press `gB` to mark the bottom.
   Move to the final display column that should be included and press `gR` to
   set an optional right edge. Repeating either command on its marker clears
   it.
6. Press `Enter` to save and begin cell navigation, or `Escape` to cancel the
   entire setup.

Manual setup, column names, and the row-header designation last for the active
Review table only. Cancelling a fresh `gT` setup does not restore the table it
replaced.

## Clipboard history

Lector keeps multiple clipboard entries (not just one). You can cycle back and forth between them and paste the one you want.

## Configuration (Lua)

On Unix, Lector reads `$XDG_CONFIG_HOME/lector/init.lua` on startup, with the
configuration root defaulting to `$HOME/.config`. `XDG_CONFIG_HOME` must be an
absolute path, as required by the
[XDG Base Directory specification](https://specifications.freedesktop.org/basedir/latest/);
an unset, empty, or relative value uses `$HOME/.config`. On other platforms,
an absolute `XDG_CONFIG_HOME` is also honored; otherwise Lector uses the
platform-native configuration directory.

Configuration is selected in this order:

1. The file passed to `--config PATH`.
2. The file named by `LECTOR_CONFIG`.
3. The XDG path above.

An explicit file selected by `--config` or `LECTOR_CONFIG` must exist. On
macOS, Lector also recognizes the former
`~/Library/Application Support/lector/init.lua` location when no valid
`XDG_CONFIG_HOME` was explicitly selected and the new XDG file does not exist.
This compatibility fallback lets existing installations migrate without
changing the XDG behavior.

Use `--no-config` to skip every configuration source and start with defaults,
including native speech. It is mutually exclusive with `--config` and takes
precedence over `LECTOR_CONFIG`.

```bash
lector --shell /bin/zsh --config ./lector-demo.lua
lector --shell /bin/zsh --no-config
```

### Common options

```lua
-- speech backend; this top-level option is read before speech starts
lector.o.speech.server = "native"

-- speaking rate
lector.o.speech.rate = 1.0

-- milliseconds of additional silence between paragraphs
lector.o.speech.paragraph_pause_ms = 100

-- backend-provided voice ID; inspect lector.o.speech.voices after startup
lector.o.speech.voice = "voice-id"

-- how many symbols should be spoken
lector.o.symbol_level = "most"  -- "none", "some", "most", "all", "character"

-- live reading on/off
lector.o.auto_read = true

-- suppress terminal output that echoes recently typed keys (disabled by default)
lector.o.suppress_key_echo = false

-- report indentation changes for the application and review cursors
-- (enabled by default; set to false to disable)
lector.o.report_indentation = false

-- interrupt speech immediately when terminal focus is lost
lector.o.stop_speech_on_focus_loss = true

-- tmux pane bells: "audible" (default), "spoken", or "off"
lector.o.tmux_bells = "spoken"

-- `"` uses Lector's history; `+` uses the system clipboard
lector.o.clipboard.default_register = '"'

-- "native" (default) uses arboard; "osc52" writes through the outer terminal
lector.o.clipboard.system_provider = "native"
```

### Clipboard API

The internal ring and system clipboard have separate namespaces. `entries` is
a newest-first snapshot and `index` is one-based.

```lua
local current = lector.clipboard.internal.text
local history = lector.clipboard.internal.entries
local selected = lector.clipboard.internal.index

lector.clipboard.internal.text = "save in Lector's history"
lector.clipboard.internal.index = 2
lector.clipboard.internal.text = nil -- clear all internal entries

local system_text = lector.clipboard.system.text -- native provider only
lector.clipboard.system.text = "copy outside Lector"
lector.clipboard.system.text = nil -- clear the system clipboard
```

Explicit binding actions `lector.paste_internal`, `lector.paste_system`,
`lector.say_internal_clipboard`, and `lector.say_system_clipboard` bypass the
configured default register.

### Simple key customization

You can remap keys or add your own Lua functions:

```lua
-- this is the default pause/resume binding
lector.bindings["M-x"] = "lector.toggle_speaking"

-- optional one-way controls; these keys are otherwise unbound by Lector
lector.bindings["M-z"] = "lector.pause_speaking"
lector.bindings["M-Z"] = "lector.resume_speaking"
lector.bindings["C-M-z"] = "lector.cancel_speaking"

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

`lector.api.speak(text, interrupt)` returns an opaque string logical speech ID
when speech is submitted, or `nil` when empty/unfocused speech is suppressed.
With reliable terminal events, one logical request can contain several
protocol utterances at paragraph boundaries. Otherwise Lector conservatively
submits one normalized utterance. Treat the returned ID as a string; its
format is not an API.

The four speech controls above are binding action names, not functions in
`lector.api`. `pause_speaking` and `resume_speaking` are idempotent one-way
actions, `toggle_speaking` switches between them, and `cancel_speaking` is the
only action that deliberately discards retained speech. A non-interrupting
`speak` appends while speech is playing. If speech has been paused, any new
`speak` request replaces the retained speech and begins the new request;
`interrupt = true` always performs that replacement even while speech is
playing.

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

-- Internal clipboard ring only. System clipboard changes do not call this hook.
-- meta: { op, index, size }
-- op: "push" | "prev" | "next" | "select" | "clear"
-- entry is nil after clear; otherwise it is the selected internal entry.
lector.hooks.on_clipboard_change = function(entry, meta) end
lector.hooks.on_key_unhandled = function(key, mode)          -- return true to consume
  return false
end
```

`on_speech_start` and `on_speech_end` bracket submission to Lector's speech
manager; they do not claim that audible playback started or ended. Playback
lifecycle is correlated internally by the speech-host events documented in
the protocol.

`on_startup` is the post-start boundary. It runs only after `init.lua` has
finished, the selected speech server has initialized, the physical terminal
and startup probes are active, and initial child output has been presented. It
runs immediately before the normal input loop, so startup announcements belong
there rather than at top level:

```lua
lector.hooks.on_startup = function(_)
  lector.api.speak("welcome to Lector", false)
end
```

## Lua REPL

Lector has a built‑in Lua REPL so you can try commands while it’s running. Open it with `M-L`, experiment, then close it when you’re done.

- Lua automatically continues incomplete statements and expressions at a `... ` prompt.
- Press `C-c` to discard a pending multiline chunk, or `C-u` to clear the current line.
- Press `C-l` to clear submitted input and output. Unsubmitted input and the `Esc to close` banner remain visible.
- The transcript, unsubmitted input, history, and Lua environment are preserved after closing and reopening the overlay.
- Commands that start with a space and consecutive duplicate commands are not added to REPL history.
- Use `lector.inspect(value)` to pretty-print nested tables. It returns a string, so entering `lector.inspect(my_table)` displays it directly. Cycles are shown as `<cycle>`, and nesting beyond 20 table levels as `<max depth>`.

## Tips

- If you want Lector to read *only* what you ask for, turn off auto‑read.
- Use Review table navigation when terminal output is column‑structured (CSV, tables, list views).
- If speech feels too fast or slow, adjust `lector.o.speech.rate`.

## Troubleshooting

- If nothing speaks, check that your system TTS works.
- If keys don’t behave as expected, toggle Help Mode and press the key to confirm its mapping.
