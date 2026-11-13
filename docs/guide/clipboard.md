# Clipboard

Lector has its own clipboard history and can also use your system clipboard.

## Copy and paste

1. Move the review cursor to the start of the text and press `F5` to set a
   mark.
2. Move the review cursor to the end of the text and press `F6` to copy.
3. Press `F7` to paste.

Inside [Review](review.md) you can also copy with vi-style commands such as
`yy` and `yiw`.

| Key | Action |
| --- | --- |
| `F5` | Set a mark |
| `F6` | Copy from the mark to the review cursor |
| `F7` | Paste |
| `M-c` | Say what is on the clipboard |
| `M-[` / `M-]` | Select the previous or next history entry |

## Two clipboards

Lector calls each clipboard a *register*, as vi does.

| Register | What it is |
| --- | --- |
| `"` | Lector's own history of the last ten copies. This is the default. |
| `+` | Your system clipboard. |

`F6`, `F7`, and `M-c` use the default register. To make the system clipboard
the default, add this to your [configuration](configuration.md):

```lua
lector.o.clipboard.default_register = "+"
```

In Review, you can choose a register for a single copy by typing it first:
`"+yy` copies the line to the system clipboard.

## Working over SSH

When Lector runs on a remote machine, it cannot reach your local system
clipboard directly. Ask it to copy through your terminal instead:

```lua
lector.o.clipboard.system_provider = "osc52"
```

This uses the OSC 52 terminal feature, which most modern terminals support.
It is write-only: you can copy to the system clipboard, but Lector cannot paste
from it or read it aloud. Lector tells you when you try.

The default provider, `"native"`, works on macOS, Windows, Wayland, and X11.

## Applications and the clipboard

When a terminal application copies text using OSC 52, the text goes into
Lector's history. Applications cannot change your system clipboard. By
default, an application that asks to read the clipboard gets an empty answer.

## From Lua

You can read and change both clipboards from your configuration. See
[Scripting](scripting.md#clipboard).
