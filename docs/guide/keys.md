# Keys

This page lists Lector's default key bindings. You can change any of them in
your [configuration](scripting.md#key-bindings).

## Reading key names

- `M-` means Alt (Option on a Mac). `M-x` is Alt+x.
- `C-` means Control. `C-f` is Control+f.
- An uppercase letter means Shift. `M-R` is Alt+Shift+r.

Two things help when you forget a key:

- Press `F1` to turn on help mode, then press any key to hear what it does.
  Press `F1` again to leave.
- Press `M-?` to open a searchable list of every binding. It works like
  [Review](review.md), and `q` closes it.

## Speech and reading

| Key | Action |
| --- | --- |
| `M-x` | Pause or resume speech |
| `M-'` | Toggle automatic reading of new output |
| `M-s` | Change how much punctuation is spoken |
| `M-g` | Toggle stopping speech when the terminal loses focus |
| `M-w` | Say where you are (the current overlay or tmux location) |
| `F12` | Say the time |

Typing also pauses speech. Pausing keeps whatever was waiting to be spoken, so
`M-x` picks up where it left off.

## The review cursor

The review cursor reads the screen without moving the application's cursor.

| Key | Action |
| --- | --- |
| `M-u` / `M-i` / `M-o` | Previous, current, next line |
| `M-U` / `M-O` | Previous, next non-blank line |
| `M-j` / `M-k` / `M-l` | Previous, current, next word |
| `M-m` / `M-,` / `M-.` | Previous, current, next character |
| `M-<` | Current character, spelled phonetically |
| `M-h` / `M-;` | Beginning, end of the line |
| `M-y` / `M-p` | Top, bottom of the screen |
| `M-a` | Read text attributes at the cursor |
| `M-"` | Toggle whether the review cursor follows the application cursor |
| `M-{` / `M-}` | Left click, right click at the review cursor |

To read output that has scrolled off the screen, open [Review](review.md) with
`M-r`.

## Clipboard

| Key | Action |
| --- | --- |
| `F5` | Set a mark at the review cursor |
| `F6` | Copy from the mark to the review cursor |
| `F7` | Paste |
| `M-c` | Say the clipboard |
| `M-[` / `M-]` | Previous, next clipboard history entry |

See [Clipboard](clipboard.md) for registers and history.

## Lector itself

| Key | Action |
| --- | --- |
| `F1` | Toggle help mode |
| `M-?` | Browse key bindings |
| `M-r` | Open [Review](review.md) |
| `M-n` | Send the next key straight to the application |
| `M-R` | Reload your [configuration](configuration.md) |
| `M-L` | Open the [Lua REPL](scripting.md#lua-repl) |
| `M-C` | Open the [tmux connection manager](tmux.md#managing-connections) |

`M-n` is useful when an application needs a key that Lector normally handles.
`M-PageUp`, `M-PageDown`, `M-Up`, and `M-Down` always pass through to the
application.

## Actions without a default key

Some actions have no key until you bind one. The most useful are:

- `lector.cancel_speaking` stops speech and throws away everything queued.
- `lector.pause_speaking` and `lector.resume_speaking` are one-way versions of
  `M-x`.
- `lector.paste_internal` and `lector.paste_system` paste from a specific
  clipboard.

[Scripting](scripting.md#key-bindings) shows how to bind them.
