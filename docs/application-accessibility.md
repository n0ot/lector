# Application-authored accessibility

Full-screen terminal applications often know more than a terminal emulator can
infer from cursor movement and changed cells. Lector supports a small, generic
protocol through which any application can replace selected automatic
heuristics with semantic speech. Lector does not identify the sending
application and contains no application-specific behavior.

The integration is optional. An application which sends no protocol messages
gets Lector's normal auto-reading, cursor tracking, and deletion announcements.

The producer-facing protocol specification is maintained by
[`lector.nvim`](https://github.com/n0ot/lector.nvim/blob/master/PROTOCOL.md). This
document describes Lector's consumer implementation and additional safety
invariants.

## Consumer invariants

1. Parse the versioned, namespaced Application Program Command (APC) payload
   and claim only exact, complete messages.
2. Store suppression policy and requested speech on the exact terminal view
   which received the message.
3. Publish that policy with the same presentation receipt as the view's cells.
   A coalesced, dropped, or backpressured render cannot expose policy from the
   wrong generation.
4. Apply automatic-reading and cursor-tracking suppression independently.
   Cursor suppression includes pending Backspace and Delete announcements.
5. Route requested speech through the ordinary `ScreenReader` speech path, so
   global auto-read and outer-terminal focus policy still apply.
6. Restore defaults on explicit end, alternate/primary screen handoff, RIS,
   DECSTR, pane replacement or resynchronization, PTY teardown, or view
   teardown.
7. Keep state and speech pane-local in tmux control mode. Speech from a hidden
   or inactive pane is discarded and is never replayed after a pane switch.

The reset rules are deliberately narrower than “anything which redraws”.
Attribute reset (`CSI 0 m`), erase-display (`CSI 2 J`), cursor addressing, and
ordinary mode changes do not end an accessibility session. Full reset (`RIS`,
`ESC c`) and soft reset (`DECSTR`, `CSI ! p`) do.

## Wire-format limits

Version 1 messages use this seven-bit APC envelope:

```text
ESC _ Lector;A11y;1;<command> ESC \
```

Lector recognizes `set`, `say`, `line`, and `end` as defined by the canonical
specification. Speech is limited to 2,000 UTF-8 bytes, must not contain control
characters, and is retained in a per-view queue limited to 32 entries and 32
KiB. The underlying unknown-sequence capture is capped at 4 KiB, which
accommodates the hexadecimal encoding while keeping parser memory bounded.

Invalid UTF-8, invalid hexadecimal, unknown versions or commands,
duplicate/missing settings, incomplete strings, and truncated strings have no
protocol effect.

APC is private application-to-terminal control data. Lector does not assign a
new meaning to OSC 200, 201, or 202.

## Focus and user settings

Application policy is a suppressive overlay. It cannot turn on a Lector option
which the user disabled. Requested speech is treated as application-authored
automatic reading, so global auto-read must be enabled. It is sent through
`ScreenReader::speak`, which drops new speech while the outer terminal lacks
focus.

Focused-out messages are consumed, not queued for later playback. The existing
stop-on-focus-loss option continues to determine whether speech already in
progress is interrupted at focus loss.

## Neovim producer

[`n0ot/lector.nvim`](https://github.com/n0ot/lector.nvim) is the independently
versioned MIT Neovim plugin which produces semantic editor events for this
protocol. It is compatible with Lector and with any other terminal screen
reader implementing the same protocol. Lector does not vendor its Lua source
or contain Neovim-specific behavior.
