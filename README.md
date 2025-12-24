# Lector

Lector is a terminal screen reader. It speaks what appears in your terminal and lets you review what’s on screen without disrupting the running program.

## What Lector does

- Reads new terminal output automatically as it appears.
- Lets you review lines, words, and characters independently of the app cursor.
- Helps navigate tables in terminal output.
- Provides a clipboard history for quick copy/paste.
- Can be customized with a simple Lua config file.

## Get started

Build:

```bash
cargo build --release
```

Run Lector with your shell:

```bash
cargo run -- --shell /bin/zsh
```

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

## How to use Lector

Think of Lector as having two ways to listen:

1) **Live reading**: Lector speaks new terminal output as it appears.
2) **Review mode**: You can move a “review cursor” around the screen to read past output without moving the application cursor.

If you ever forget keys, toggle **Help Mode** and press any key to hear what it does. (Default: `F1`.)

### Core actions (with defaults)

- **Stop speech** when it’s too noisy. Default: `M-x`.
- **Toggle auto‑read** if you want to hear only on demand. Default: `M-'`.
- **Move and read** by line/word/character using the review cursor.
- **Set a mark and copy** text between the mark and the review cursor.
- **Toggle table mode** to navigate tables by row/column.

You don’t need to memorize everything. Help Mode will tell you what each key does.

### Review mode (reading past output)

Use the review cursor to move around without changing the application’s cursor.

- Line: previous/next line is `M-u` / `M-o`. Read current line is `M-i`.
- Word: previous/next word is `M-j` / `M-l`. Read current word is `M-k`.
- Character: previous/next character is `M-m` / `M-.`. Read current character is `M-,`.
- Quickly jump to top/bottom of the screen with `M-y` / `M-p`.

### Copy/paste and clipboard history

- Set a mark with `F5`, move the review cursor, then copy with `F6`.
- Paste the current clipboard entry with `F7`.
- Speak the current clipboard with `M-c`.
- Cycle clipboard history with `M-[` (previous) and `M-]` (next).

## Table navigation

When table mode is on, Lector will detect tables like:

- Pipe‑delimited (`|`), comma‑delimited, or tab‑delimited rows.
- Fixed‑width columns separated by consistent spacing.

Turn table mode on/off with `M-t`. Then:

- Move rows with `j`/`k` (or `M-o`/`M-u`).
- Move columns with `h`/`l`.
- Read the current cell with `i`.
- Read the current column header with `H`.
- Exit table mode with `Esc`.

You can also toggle automatic header speaking with `M-h`.

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
```

### Simple key customization

You can remap keys or add your own Lua functions:

```lua
-- map a key to a built-in action
lector.bindings["M-x"] = "lector.stop_speaking"

-- add a custom command
lector.bindings["M-r"] = {
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
lector.hooks.on_mode_change = function(old, new) end     -- "normal" | "table"
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

## Tips

- If you want Lector to read *only* what you ask for, turn off auto‑read.
- Use table mode when terminal output is column‑structured (CSV, tables, list views).
- If speech feels too fast or slow, adjust `lector.o.speech_rate`.

## Troubleshooting

- If nothing speaks, check that your system TTS works.
- If keys don’t behave as expected, toggle Help Mode and press the key to confirm its mapping.
