# Review

Review lets you read, search, and copy everything in the terminal, including
output that has scrolled away. It uses vi-style keys.

Press `M-r` to open Review. Press `q` to leave.

Review works on a frozen snapshot. The running program keeps going in the
background, but nothing it prints can move or change what you are reading.
Lector keeps up to 10,000 lines of scrollback.

## The basics

| Key | Action |
| --- | --- |
| `h` `j` `k` `l` or arrows | Move left, down, up, right |
| `w` `b` `e` | Next word, previous word, end of word |
| `0` `^` `$` | Start of line, first non-blank, end of line |
| `gg` `G` | Top, bottom |
| `C-b` `C-f` | Page up, page down |
| `/` `?` | Search forward, backward |
| `n` `N` | Repeat the search, same or opposite direction |
| `yy` | Copy the line |
| `q` | Leave Review |

As you move, Lector reads where you land: the line when you change rows, the
word when you jump to another word, and the character for smaller moves.
Spaces and blank lines are named.

`Escape` never closes Review. It cancels whatever is in progress, such as a
count, a search, or a selection. If nothing is in progress, Lector says
"review mode" to remind you that `q` is the way out.

A motion that cannot go anywhere, or a key Review does not understand, rings
the terminal bell.

## Moving around

Counts work as in vi: `3w` moves forward three words.

- **Words:** `w`, `b`, `e` use word rules. `W`, `B`, `E` treat anything between
  spaces as one word.
- **Find a character:** `f` and `F` jump to a character on the line. `t` and `T`
  stop just before it. `;` repeats the find and `,` repeats it backward.
- **Brackets:** `%` jumps between matching `()`, `[]`, and `{}`.
- **Scrolling:** `zt`, `zz`, and `zb` put the cursor line at the top, middle, or
  bottom of the screen. `z<Enter>`, `z.`, and `z-` do the same and move to the
  first non-blank character. With a count, they go to that line number first.
- **Shell prompts:** `[p` and `]p` jump to the previous and next prompt. This
  needs a shell that marks its prompts (OSC 133 shell integration).

The regular [review cursor keys](keys.md#the-review-cursor) such as `M-u` and
`M-o` still work inside Review. They stay within the page currently shown.

## Searching

`/` searches forward and `?` searches backward, using regular expressions.
Searches cover the whole snapshot and wrap around at the ends.

- `n` repeats the search in the same direction. `N` goes the other way.
- `*` and `#` search forward and backward for the word under the cursor.
  `g*` and `g#` also match it inside longer words.
- Submitting an empty `/` or `?` reuses the last pattern.

After a search, Lector reads the match and a count such as "21 of 29". If a
search fails, Lector tells you why and leaves the search open so you can fix
it or press `Escape`.

## Marks and jumps

- `ma` through `mz` set a named mark.
- `` `a `` jumps to the exact position of mark `a`. `'a` jumps to the start of
  its line.
- `C-o` goes back to where you were before a jump. `C-i` (or `Tab`) goes
  forward again.

Searches, mark jumps, `gg`, `G`, `%`, and prompt jumps count as jumps. Ordinary
movement does not.

Marks, your last search, and your jump history are remembered when you close
and reopen Review. Each view and each tmux pane keeps its own. They are
forgotten when Lector exits.

A mark can expire. If the line it pointed to scrolls out of history, is
redrawn, or is rewrapped by a resize, Lector tells you the mark is no longer
available rather than guessing where it went.

## Copying

`y` copies ("yanks") text, followed by a motion that says how much.

| Keys | Copies |
| --- | --- |
| `yy` | The line |
| `yw`, `y$`, `y3j` | Whatever the motion covers |
| `yiw` / `yaw` | The word under the cursor, without or with surrounding space |
| `v` then move, then `y` | A selection by character |
| `V` then move, then `y` | A selection by line |

Press `v` or `V` again to cancel a selection.

Copied text goes to your default clipboard register and is ready to paste with
`F7`. To pick a register, type it first:

- `""` uses Lector's clipboard history, for example `""yy`.
- `"+` uses the system clipboard, for example `"+yiw`.

See [Clipboard](clipboard.md) for more about registers.

## Links

With the cursor on a link:

- `gx` opens an `http` or `https` link in your browser.
- `gy` copies the link's address to Lector's clipboard history. `"+gy` copies
  it to the system clipboard instead.

Lector prefers a link's real destination when the terminal provides one
(OSC 8 hyperlinks). Otherwise it recognizes plain web addresses in the text,
even when they wrap across lines. If opening fails, Lector says so.

## Clicking

`M-{` moves Review's cursor to the review cursor. Outside Review, the same key
sends a left click to the application at the review cursor, and `M-}` sends a
right click.

## Tables

Review can detect tables and move through them cell by cell. See
[Tables](tables.md).

## Good to know

- Resizing the terminal keeps the snapshot and your place in it.
- Review feedback works even when automatic reading is off.
- In shells with prompt marking, pressing Up or Down at the prompt reads the
  recalled command without re-reading the prompt.
