# Configuration

Lector is configured with a Lua file called `init.lua`. You do not need one to
get started. Everything has a sensible default.

## Where the file lives

By default:

```text
~/.config/lector/init.lua
```

If you set `XDG_CONFIG_HOME` to an absolute path, Lector looks in
`$XDG_CONFIG_HOME/lector/init.lua` instead.

To use a different file, pass it on the command line or set an environment
variable. Lector uses the first of these that applies:

1. `--config PATH`
2. The `LECTOR_CONFIG` environment variable
3. The default location above

```bash
lector --config ./my-config.lua
```

To ignore every config file and start with defaults:

```bash
lector --no-config
```

Notes for particular setups:

- A file named with `--config` or `LECTOR_CONFIG` must exist.
- On macOS, Lector still reads the older location,
  `~/Library/Application Support/lector/init.lua`, when there is no file in
  the new one.
- On platforms other than Unix, Lector uses the platform's usual
  configuration directory unless `XDG_CONFIG_HOME` is set.

## A first config file

```lua
-- Speak a little faster than normal.
lector.o.speech.rate = 65

-- Speak only some punctuation.
lector.o.symbol_level = "some"

-- Make the system clipboard the default.
lector.o.clipboard.default_register = "+"
```

## Reloading

Press `M-R` to reload your configuration without restarting.

- If the file has an error, Lector shows it and keeps your previous
  configuration.
- Key bindings and hooks you removed from the file are removed.
- `lector.o.speech.server` cannot change on reload. Restart Lector to change
  it, or use [`lector.api.set_speech`](speech.md#switching-servers-while-running).

## Options

Options live under `lector.o`.

### General

| Option | Default | Meaning |
| --- | --- | --- |
| `auto_read` | `true` | Speak new output as it appears. |
| `symbol_level` | | How much punctuation to speak: `"none"`, `"some"`, `"most"`, `"all"`, or `"character"`. |
| `report_indentation` | `true` | Announce changes in indentation. |
| `suppress_key_echo` | `false` | Do not speak output that just echoes what you typed. |
| `stop_speech_on_focus_loss` | | Stop speaking when the terminal loses focus. |
| `tmux_bells` | `"audible"` | How [tmux bells](tmux.md#bells) are reported: `"audible"`, `"spoken"`, or `"off"`. |
| `legacy_escape_timeout_ms` | `30` | See [Troubleshooting](troubleshooting.md#alt-keys-are-not-recognized). |

### Speech

See [Speech](speech.md) for details.

| Option | Default | Meaning |
| --- | --- | --- |
| `speech.server` | `"native"` | Which speech server to run. Startup only. |
| `speech.rate` | | 0 (slowest) to 100 (fastest); 50 is normal. |
| `speech.pitch` | | Pitch, in the speech engine's own range. |
| `speech.volume` | | Volume, in the speech engine's own range. |
| `speech.voice` | | A voice ID. |
| `speech.voices` | | Read-only list of available voices. |
| `speech.paragraph_pause_ms` | `100` | Silence between paragraphs, in milliseconds. |

### Clipboard

See [Clipboard](clipboard.md) for details.

| Option | Default | Meaning |
| --- | --- | --- |
| `clipboard.default_register` | `'"'` | `'"'` for Lector's history, `"+"` for the system clipboard. |
| `clipboard.system_provider` | `"native"` | `"native"`, or `"osc52"` to copy through the terminal. |

## Splitting your config into files

Lua files next to `init.lua` can be loaded with `require`:

```lua
-- Loads pager.lua from the same folder as init.lua.
local pager = require("pager")

-- Loads tools/pager.lua or tools/pager/init.lua.
local tools = require("tools.pager")
```

`lector.config_dir` holds the folder containing your config file.

## Going further

[Scripting](scripting.md) covers key bindings, hooks, and automation.
[`examples/lua-config`](../../examples/lua-config) is a complete, working
config folder you can copy.
