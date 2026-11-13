# Troubleshooting

## Nothing is spoken

1. Check that your system's text-to-speech works outside Lector.
2. Press `M-x`. Speech may simply be paused.
3. Press `M-'`. Automatic reading may be turned off.
4. See which speech engines Lector can find:

   ```bash
   lector tts --list-backends
   ```

5. Start Lector without your configuration to rule it out:

   ```bash
   lector --no-config
   ```

## A key does not do what I expect

Press `F1` to turn on help mode, then press the key. Lector says what it is
bound to. Press `F1` again to leave help mode.

`M-?` lists every binding, including ones from your configuration.

If an application needs a key that Lector uses, press `M-n` first. The next
key goes straight to the application.

## Alt keys are not recognized

On macOS, many terminals need a setting such as "Use Option as Meta key"
before Option works as Alt.

Lector asks your terminal for precise key reporting (the Kitty keyboard
protocol, or xterm's `modifyOtherKeys`). Terminals that support neither send
Alt+key as Escape followed by the key, which looks the same as pressing
Escape and then the key quickly. Lector tells them apart by timing. If Alt
keys are missed, or a quick Escape is mistaken for Alt, adjust the wait:

```lua
lector.o.legacy_escape_timeout_ms = 30  -- the default
```

## Speech is too fast or too slow

Set `lector.o.speech.rate` between 0 and 100. See
[Speech](speech.md#rate-pitch-volume-and-voice).

## Lector reads too much

- Turn off automatic reading with `M-'` and read on demand.
- Speak less punctuation with `M-s`.
- If your typing is echoed back, set `lector.o.suppress_key_echo = true`.
- Filter what is read with the
  [`on_live_read` hook](scripting.md#screen-and-reading).

## Colors, images, or links look wrong

Lector presents itself to applications as `xterm-256color`. It supports 256
colors, true color, hyperlinks, and Kitty graphics when your terminal does.

Lector works out what your terminal supports automatically. If it guesses
wrong, you can tell it with environment variables:

| Variable | Value |
| --- | --- |
| `LECTOR_OUTER_COLORS` | Number of colors, such as `256` |
| `LECTOR_OUTER_TRUE_COLOR` | `true` or `false` |
| `LECTOR_OUTER_HYPERLINKS` | `true` or `false` |
| `LECTOR_OUTER_SYNC` | `true` or `false` |
| `LECTOR_OUTER_KITTY_KEYBOARD` | `true` or `false` |
| `LECTOR_OUTER_KITTY_GRAPHICS` | `true` or `false` |
| `LECTOR_OUTER_FOCUS` | `true` or `false` |
| `LECTOR_OUTER_CLIPBOARD_READ` | `true` or `false` |

`yes`/`no`, `on`/`off`, and `1`/`0` work too. These describe your terminal
only. They do not change what Lector offers to applications.

## tmux problems

See [tmux: when something goes wrong](tmux.md#when-something-goes-wrong).

## Reporting a bug

A recording helps a great deal. `lector-trace` runs Lector and records
everything typed, displayed, and spoken. Run it from your clone of the Lector
repository:

```bash
scripts/lector-trace --shell "$SHELL"
```

It prints the folder where the recording is saved, both when it starts and
when it exits. Use `--trace-dir PATH` to choose the folder.

A trace contains everything you type, including passwords. Reproduce the
problem with nothing sensitive on screen, and look through the files before
sharing them.

For a lighter record, `lector --log-file PATH` writes Lector's own diagnostic
log.
