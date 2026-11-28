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
  [Review](review.md), and `q` closes it. Each mode lists related built-in
  actions together, followed by custom Lua actions in key order. Review keys
  and command syntax appear at the end.

## Speech and reading

| Key | Action |
| --- | --- |
| `M-x` | Toggle speech pause |
| `M--` | Decrease speech rate by five percentage points |
| `M-=` | Increase speech rate by five percentage points |
| `M-'` | Toggle automatic reading of new output |
| `M-s` | Cycle symbol level (how much punctuation is spoken) |
| `M-g` | Toggle stopping speech when the terminal loses focus |
| `M-w` | Say the current title and any working or blocked status |
| `M-W` | Say the full location and any working or blocked status |
| `F12` | Say time |

Both keys say the overlay's name when Review, a chooser, or another overlay
is visible. In a plain terminal, the detailed form starts with "terminal".
An untitled terminal is called "terminal" in both forms. To customize the
bindings, use `say_location` for the brief form and `say_overlay` for the
detailed form.

Typing also pauses speech. Pausing keeps whatever was waiting to be spoken, so
`M-x` picks up where it left off.

## The review cursor

The review cursor reads the screen without moving the application's cursor.

| Key | Action |
| --- | --- |
| `M-u` / `M-o` | Previous / next line |
| `M-i` | Say current line |
| `M-U` / `M-O` | Previous, next non-blank line |
| `M-j` / `M-l` | Previous / next word |
| `M-k` | Say current word |
| `M-K` | Spell current word phonetically |
| `M-m` / `M-.` | Previous / next character |
| `M-,` | Say current character |
| `M-<` | Say current character phonetically |
| `M-h` / `M-;` | Beginning, end of the line |
| `M-y` / `M-p` | Top, bottom of the screen |
| `M-a` | Say attributes at the review cursor |
| `M-"` | Toggle whether the review cursor follows the application cursor |
| `M-{` / `M-}` | Left click, right click at the review cursor |

To read output that has scrolled off the screen, open [Review](review.md) with
`M-r`.

`M-K` also works in Review. It spells the word using the same phonetic alphabet
as `M-<`, says “cap” for uppercase letters, and names digits and punctuation
at every symbol level. To rebind it, use `lector.review_word_read_phonetic`.

## Clipboard

| Key | Action |
| --- | --- |
| `F5` | Set a mark at the review cursor |
| `F6` | Copy from the mark to the review cursor |
| `F7` | Paste |
| `M-c` | Say clipboard |
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
