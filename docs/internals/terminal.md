# Terminal

Lector contains a full terminal emulator. This page covers what applications
see when they run inside Lector, how Lector talks to your real ("outer")
terminal, and how the emulator and renderer are built.

## What applications see

Lector launches the child with `TERM=xterm-256color`. It implements 256
colors, true color, OSC 8 hyperlinks, and the usual `xterm-256color`
behavior. It does not copy the outer terminal's identity, and it does not
claim to be `xterm-ghostty`: using Ghostty's parser does not mean implementing
every Ghostty extension.

### Private terminfo entry

Lector supports synchronized output (DEC mode 2026) and wants programs such
as tmux to use it. The stock `xterm-256color` entry does not advertise it, so
Lector gives the child a private copy that adds the `Sync` capability:

- Lector removes any inherited `TERMINFO` and points the child at a private
  cache entry, keyed by target and content.
- When `infocmp` and `tic` are available, Lector adapts the host's own
  `xterm-256color` entry, with a time limit.
- Otherwise it extracts a compiled entry embedded in the binary (macOS and
  Linux).
- If the cache cannot be used, a temporary entry lives as long as the child
  needs it.

Lector never writes to the system or user terminfo database, and needs no
setup command.

This is local state. SSH sends only the `TERM` name, so a remote tmux uses
the remote terminfo database and will not see `Sync` unless that host
advertises it too. Direct `DECRQM 2026` queries still travel over SSH and are
answered by Lector.

To check from inside Lector, after attaching an ordinary tmux client:

```sh
infocmp -1 -x xterm-256color | grep Sync
tmux info | grep Sync
```

A `tmux -CC` client has no rendering terminal, so `tmux info` shows nothing
there. Lector stabilizes control-mode output itself.

### Queries

Lector's root terminal engine answers the child's queries locally: device
attributes, modes, geometry, pixel size, color scheme, keyboard, focus, and
clipboard.

Default colors are the exception. At startup Lector reads the outer
terminal's exact foreground and background (OSC 10 and 11) and its light or
dark report, and passes those on to applications that ask. An explicit light
or dark report wins over a guess from brightness. Adaptive themes therefore
see the same answer with or without Lector.

In tmux control mode, tmux owns the panes' PTYs and answers their queries.
See [tmux input](tmux/input.md#terminal-queries).

### Unsupported modes

Ghostty's parser recognizes some modes that Lector does not implement end to
end. Lector reports these as unsupported and keeps them off, even if an
application enables them without asking:

- URXVT mouse encoding (1015)
- Pixel-coordinate mouse mode (1016)
- Live color-scheme notifications (2031)
- In-band resize reports (2048)

### Side effects

| Effect | What Lector does |
| --- | --- |
| Clipboard write (OSC 52) | Adds it to Lector's clipboard history. Never touches the system or outer-terminal clipboard. |
| Clipboard read (OSC 52) | Returns an empty reply by default. |
| Desktop notifications, unknown APC | Dropped. |
| Title, working directory, progress, hyperlinks, bells | Kept as modeled state. |

This policy is separate from the user's clipboard options. Clipboard and
notification payloads are never replayed as raw terminal bytes.

Titles and working directories are kept exactly as reported, including empty
values and raw `file://` URIs. They are not interpreted as SSH destinations.

## The outer terminal

### Startup probes

At startup Lector sends a bounded set of queries to the outer terminal to
learn what it supports. The device-attributes query (DA1) goes last, and its
reply marks the point where everything before it has been answered. Probe
replies are consumed and never reach the application.

If the probes time out, Lector releases pending input but still recognizes
late replies, so they do not turn into stray keystrokes.

### Capabilities and overrides

Rendering starts conservatively, then applies terminfo, then probe results,
then environment overrides. The user-facing override variables are listed in
[Troubleshooting](../guide/troubleshooting.md#colors-images-or-links-look-wrong).
They describe the outer terminal only and never change what Lector offers to
applications.

### Keyboard

Lector asks the outer terminal for the Kitty keyboard protocol
(disambiguate-escape-codes and report-alternate-keys), or xterm
`modifyOtherKeys` mode 2 when Kitty is unavailable. The alternate-key data
preserves shifted punctuation for the active keyboard layout.

Lector decodes those into semantic keys, then re-encodes each key for whatever
keyboard mode the child asked for, dropping details the child did not request.

A terminal that supports neither leaves the classic ambiguity between Escape
followed by a key and an Alt-modified key. Only that fallback uses
`lector.o.legacy_escape_timeout_ms`. Probe timeouts are separate.

### Output

Standard output is nonblocking. Writes retry on `EINTR`, resume partial
transactions without interleaving, and restore the descriptor's flags on exit.
Modeled updates have a 64 KiB write budget per drain. A fully hidden update
can complete with zero bytes written.

## The Ghostty adapter

The `lector-ghostty` crate wraps Ghostty's official C API. It is always
enabled. Native handles stay on the thread that created them. See
[Building](building.md) for how the library is built and pinned.

### Normalization

The adapter turns Ghostty's cells into Lector's own model of graphemes,
widths, styles, hyperlinks, wrapping, and history. Erased regions keep the
background color the application chose.

### Scrollback

Lector exposes at most 10,000 scrollback rows. Ghostty frees memory a page at
a time and may hold more, so the adapter gives Ghostty 20,000 lines of
headroom and clips its public view to the newest 10,000.

Review marks use Ghostty's tracked grid references. A mark becomes invalid
when its cell leaves the 10,000-row window, even if Ghostty has not yet freed
that page.

### Update summaries

Each call that advances the terminal also returns an `UpdateSummary`, which
[automatic reading](reading-model.md) consumes. It records printed runs,
cursor and scroll operation counts, Ghostty's render damage, changed rows,
cursor and screen identity before and after, and whether a
synchronized-output frame was opened.

When a frame opens, the adapter splits processing at the marker and snapshots
the screen and history as they were just before it. This holds even when the
escape sequence is split across reads.

### The stream observer

Ghostty reports final state but has no callbacks for "text was printed" or
"the cursor moved". A small `vte`-based observer runs beside it to collect:

- operation hints, used for fast rendering and for reading;
- OSC 133 prompt boundaries and exit statuses, which Ghostty's API does not
  fully expose;
- the active OSC 8 link.

The observer tracks only the cursor, margins, and a few modes. It owns no
cells and is not a second terminal. Its hints are discarded unless its final
cursor matches Ghostty's.

Each OSC 133 boundary is anchored with a tracked grid reference, so command
phases and exit statuses survive scrolling and reflow.

### Damage

Ghostty marks which rows changed. An ordinary update re-reads only those rows
and moves unchanged rows forward without copying. Full damage, geometry
changes, selected scrollback, screen switches, and inconsistent metadata fall
back to a full rebuild. The adapter clears Ghostty's dirty flags after each
snapshot so damage cannot leak into a later frame.

### Terminal effects

Ghostty callbacks copy bells, titles, working directories, clipboard writes,
notifications, progress, queries, and bounded unknown sequences into owned
events before returning. Unsupported APC payloads are capped at 256 bytes.

### Resize

Lector reads the outer terminal's full size, including pixels, and sends the
same geometry to the child PTY and the pane model. The PTY gets its new size
before the child receives `SIGWINCH`. Ghostty's reflow on the primary screen,
and cropping on the alternate screen, are authoritative.

### Snapshot round-trip

The adapter has a diagnostic snapshot round-trip used only by tests. It is
not used at runtime and is not a persistence format: Ghostty's snapshot
format is unstable, and Lector's prompt anchors are not included.

## Rendering

The compositor builds one scene from all visible layers and compares it with
the `PresentedScene`, the last scene known to be on the physical terminal.

There are three levels of rendering, each a fallback for the one before:

1. **Semantic.** When the observer's hints agree with the final scene, the
   compositor replays the operation itself: scrolling, inserting and deleting
   lines or characters, erasing, and runs of ASCII. It applies these to the
   terminal and to a cloned shadow, and accepts the shadow only if the
   affected rows match Ghostty's scene and the write flushes.
2. **Dirty region.** Pane-local dirty rows are mapped to scene coordinates,
   and only those cells are compared and redrawn. Each run carries its own
   cursor position, style, and link state.
3. **Full scene.** Everything is compared against the confirmed scene.

Anything uncertain drops to a lower level: failed or partial writes, resize,
suspend and resume, changed images, occlusion, clipping, wide-cell ambiguity,
or a failed validation. Tests check the full-scene path against an
independent oracle.

Scrolling stays fast even when rows are soft-wrapped or a write falls between
two scrolls. The renderer scrolls the region, clears wrap flags on the new
rows, then validates the whole region against Ghostty's final scene.

Hyperlinks are closed at the end of every physical write.

### Images

Kitty graphics decoded by Ghostty are stored per pane:

- Uploads and placements have separate lifetimes, and unchanged pixel data is
  shared.
- Pane-local IDs are mapped to outer IDs that cannot collide.
- Placements are clipped to their pane and to overlays.
- A hidden pane keeps its uploads cached, and its placements return without
  resending the data.
- Text changes around an unchanged image do not resend the image.
- After any uncertainty, the outer cache is rebuilt.

Limits are 32 MiB per image, 64 MiB per pane, 128 MiB per scene, and 4,096
placements. A terminal without Kitty graphics gets the full text scene and no
graphics bytes.
