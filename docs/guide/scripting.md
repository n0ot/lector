# Scripting

Your [configuration file](configuration.md) is a Lua program. Beyond setting
options, it can rebind keys, add commands of your own, and react to what
happens in the terminal.

This page covers:

- [Key bindings](#key-bindings)
- [Speaking](#speaking)
- [Clipboard](#clipboard)
- [Hooks](#hooks)
- [Automation](#automation): read the screen and drive the application
- [Lua REPL](#lua-repl): try things out live
- [When a script fails](#when-a-script-fails)

## Key bindings

Assign to `lector.bindings` to bind a key. Bind a built-in action by name:

```lua
lector.bindings["M-z"] = "lector.cancel_speaking"
```

Or bind your own function, with a description for help mode:

```lua
lector.bindings["M-v"] = {
  "speak current time",
  function()
    lector.api.speak(os.date("%H:%M"), true)
  end,
}
```

Key names are explained in [Keys](keys.md#reading-key-names). To find an
action's name, open the binding browser with `M-?`.

## Speaking

```lua
lector.api.speak("hello", false)  -- wait for current speech to finish
lector.api.speak("hello", true)   -- interrupt current speech
```

If speech is paused, a new `speak` replaces whatever was paused.

`speak` returns an ID string for the request, or `nil` if nothing was spoken
(for example, when the text is empty). Do not rely on the ID's format.

Pausing, resuming, and cancelling are key binding actions rather than
functions: `lector.pause_speaking`, `lector.resume_speaking`,
`lector.toggle_speaking`, and `lector.cancel_speaking`.

## Clipboard

Lector's clipboard history and the system clipboard are separate tables:

```lua
-- Lector's history
local current = lector.clipboard.internal.text
local history = lector.clipboard.internal.entries  -- newest first
local selected = lector.clipboard.internal.index   -- starts at 1

lector.clipboard.internal.text = "add an entry"
lector.clipboard.internal.index = 2                -- select the second entry
lector.clipboard.internal.text = nil               -- clear the history

-- The system clipboard
local text = lector.clipboard.system.text          -- "native" provider only
lector.clipboard.system.text = "copy outside Lector"
lector.clipboard.system.text = nil                 -- clear it
```

## Hooks

A hook is a function Lector calls when something happens. Set a hook by
assigning a function, and remove it by assigning `nil`:

```lua
lector.hooks.on_startup = function()
  lector.api.speak("welcome to Lector", false)
end

lector.hooks.on_startup = nil
```

### Startup, reload, and shutdown

| Hook | Called | Arguments |
| --- | --- | --- |
| `on_startup(ctx)` | Once Lector is fully started and ready for input | `ctx`: `{ config_path, version, pid }` |
| `on_reload(ctx)` | After a successful `M-R` reload | Same as `on_startup` |
| `on_shutdown(reason)` | When Lector exits | `reason`: `"exit"` or `"error"` |
| `on_error(message, context)` | When Lector reports an error | |

Put startup announcements in `on_startup`, not at the top of your file. Speech
is not ready while the file is still loading. In the same way, anything that
should happen after a reload belongs in `on_reload`. `on_startup` does not run
again on reload.

### Screen and reading

| Hook | Called | Arguments |
| --- | --- | --- |
| `on_screen_update(ev)` | When the screen changes | `ev`: `{ rows, cols, cursor_row, cursor_col, prev_cursor_row, prev_cursor_col, changed, overlay, screen, prev_screen }` |
| `on_live_read(text, meta)` | Before new output is spoken | `meta`: `{ cursor_moves, scrolled }` |
| `on_review_cursor_move(pos)` | When the review cursor moves | `pos`: `{ row, col, prev_row, prev_col }` |

`on_live_read` can change what is spoken. Return the text to speak, or `nil`
to stay silent:

```lua
lector.hooks.on_live_read = function(text, meta)
  if text:match("^%s*$") then
    return nil
  end
  return text
end
```

### Speech

| Hook | Called | Arguments |
| --- | --- | --- |
| `on_speech_start(text, meta)` | When Lector queues text to speak | `meta`: `{ interrupt }` |
| `on_speech_end(text, meta)` | When queuing finishes | `meta`: `{ interrupt, ok }` |

These tell you what Lector sent to be spoken. They do not tell you when the
sound actually starts or stops.

### Keys, modes, and clipboard

| Hook | Called | Arguments |
| --- | --- | --- |
| `on_key_unhandled(key, mode)` | For a key Lector has no binding for | Return `true` to swallow the key |
| `on_mode_change(old, new)` | When the input mode changes | `"normal"`, `"table"`, or `"table_setup"` |
| `on_table_mode_enter(meta)` | On entering table mode | `meta`: `{ top, bottom, columns, header_row, current_col }` |
| `on_table_mode_exit()` | On leaving table mode | |
| `on_clipboard_change(entry, meta)` | When Lector's clipboard history changes | `meta`: `{ op, index, size }` |

For `on_clipboard_change`, `op` is `"push"`, `"prev"`, `"next"`, `"select"`,
or `"clear"`, and `entry` is `nil` after a clear. Changes to the system
clipboard do not call this hook.

## Automation

A key binding can read the screen, send keys to the application, and wait for
it to respond. This is how you build things like "read this whole document
aloud, turning pages as you go".

[`examples/lua-config`](../../examples/lua-config) does exactly that: it binds
`M-P` to read through a pager continuously. Try it with
`lector --config examples/lua-config/init.lua`.

Automation functions look like they block, but they do not freeze Lector.
Only your binding waits.

### Looking at the screen

```lua
local view = lector.api.view()
```

`view` is a snapshot of what is on screen right now.

- `view.rows` is the number of rows.
- `view.cursor` is the application's cursor: `{ row, col, visible }`.
- `view.review` is the review cursor: `{ row, col }`.
- `view:same_content(other)` tells you whether two snapshots show the same
  text, ignoring colors and cursor position.

Rows and columns count from 0. A range includes its start and excludes its
end.

### Reading aloud

```lua
local reader = lector.api.reader()
local result = reader:read(view, view.review, { row = view.rows - 1, col = 0 })
reader:close()
```

`reader:read(view, first, last)` speaks that part of the screen and waits
until it finishes. The review cursor follows along word by word.

It returns a table:

- `status` is `"completed"` or `"cancelled"`.
- `cause` says why it was cancelled.
- `position` is where reading stopped: `{ row, col }`.

Pressing any key stops the reading. `read` then returns with
`status = "cancelled"` and `cause = "key"`, and that key is not passed to the
application. Reading is also cancelled if the screen changes underneath it.

While a reader is open, automatic reading is switched off so the two do not
talk over each other. It is restored when you call `reader:close()` or your
binding ends.

Readers need a speech engine that reports its progress word by word.
`lector.api.reader()` raises an error if yours cannot.

### Sending keys

```lua
local receipt = view:send_keys("<C-f>")
local response = receipt:wait_for_stable_screen()
```

- `view:send_keys(spec)` sends keys written in Neovim's notation. Plain text
  is typed as is. Special keys go in angle brackets: `<Space>`, `<Esc>`,
  `<CR>`, `<Tab>`, `<BS>`, `<C-x>`, and `<lt>` for a literal `<`.
- `view:send_text(text)` types text exactly as written.

Both return a receipt. `receipt:wait_for_stable_screen()` waits until the
application has finished redrawing, then returns a table:

- `status` is `"presented"` if the screen was redrawn, `"no_response"` if the
  application did nothing before the wait timed out, or `"cancelled"` if you
  pressed a key while a reader was open.
- `view` is a fresh snapshot.
- `content_changed` says whether the text changed.
- `effects.bells` counts the bells rung in the meantime.

## Lua REPL

The REPL lets you run Lua inside a running Lector. It is the quickest way to
try an option or test a function. Press `M-L` to open it and `Escape` to close
it.

- Unfinished statements continue on a `...` prompt.
- `C-c` discards an unfinished statement. `C-u` clears the line.
- `C-l` clears the screen.
- `lector.inspect(value)` pretty-prints a table.
- Your variables, history, and output are still there when you reopen it.
- A command that starts with a space is left out of the history.

Be careful with long-running code. The REPL runs on the same thread as the
rest of Lector, so a loop that never ends will freeze Lector, and there is no
key that can interrupt it. For long jobs, call `coroutine.yield()` regularly:

```lua
for i = 1, 1000000 do
  do_a_little_work(i)
  if i % 1000 == 0 then coroutine.yield() end
end
```

Each `yield` gives Lector a chance to handle input and redraw. While the job
is paused at a `yield`, `Escape` cancels it. Changes it already made are kept.

## When a script fails

Lector tries to keep going:

- **An error in `init.lua`** opens an error popup. Lines that ran before the
  error stay in effect. On reload, the previous configuration is kept instead.
- **An error in a key binding** stops that binding, closes any reader it
  opened, and shows the error with a traceback.
- **An error in a hook** shows the error and switches that hook off until you
  reload. A failed `on_live_read` leaves the text unchanged, and a failed
  `on_key_unhandled` does not swallow the key.

While a reload is being evaluated, `lector.api.speak`,
`lector.api.set_speech`, and the system clipboard are unavailable. Do that
work in `on_reload`.

Errors are also written to the log when you run Lector with `--log` or
`--log-file`.
