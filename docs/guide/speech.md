# Speech

Lector speaks through your system's text-to-speech out of the box. This page
covers how to control speech, tune the voice, and use a different speech
server.

## Controlling speech

- `M-x` pauses or resumes speech. Typing pauses it too.
- Pausing keeps everything that was waiting to be spoken. When you resume,
  Lector continues from the word it stopped on, or from the start of that
  utterance if the speech engine cannot resume mid-sentence.
- Cancelling throws away the current speech and everything queued behind it.
  It has no default key. Bind `lector.cancel_speaking` if you want one (see
  [Scripting](scripting.md#key-bindings)).
- `M-'` turns automatic reading off or on. With it off, Lector speaks only
  when you ask.
- `M-g` chooses whether speech stops when the terminal loses focus.

## Rate, pitch, volume, and voice

Set these in your [configuration file](configuration.md):

```lua
lector.o.speech.rate = 65      -- 0 is slowest, 50 is normal, 100 is fastest
lector.o.speech.pitch = 1.0
lector.o.speech.volume = 0.8
lector.o.speech.voice = "voice-id"
```

- **Rate** always runs from 0 to 100, whatever speech engine you use.
- **Pitch** and **volume** use whatever range the speech engine itself uses.
- **Voice** takes a voice ID. To see the IDs, run
  `lector tts --backend av-foundation --list-voices` with your backend's
  name (see [below](#choosing-a-speech-engine)), or print them from the
  [Lua REPL](scripting.md#lua-repl):

  ```lua
  for _, v in ipairs(lector.o.speech.voices) do
    print(v.id, v.name, v.language, v.gender)
  end
  ```

Not every engine supports every setting. If the engine cannot report a
setting, reading it gives `nil`. Assigning a setting the engine does not
support, a rate outside 0 to 100, or an unknown voice raises an error and
leaves the old value in place.

> **Upgrading from an older Lector?** Rate used to be in the speech engine's
> own units. An old value such as `1.0` is still accepted, but now means
> "almost the slowest". Pick a new value between 0 and 100.

### Pauses between paragraphs

Lector leaves a short silence between paragraphs:

```lua
lector.o.speech.paragraph_pause_ms = 100  -- the default; 0 means no pause
```

## Choosing a speech engine

Lector's built-in speech server supports several engines ("backends"), such as
AVFoundation on macOS, Speech Dispatcher on Linux, and NVDA on Windows. It
picks the first one available. To see and choose:

```bash
lector tts --list-backends
lector tts --backend av-foundation --list-voices
```

To use a specific backend, start the speech server yourself with the options
you want:

```lua
lector.o.speech.server = {
  program = "lector",
  args = { "tts", "--backend", "speech-dispatcher" },
}
```

`program` is run directly with exactly the arguments in `args`. No shell is
involved, so there is no quoting to worry about.

`lector.o.speech.server` is read once, at startup. The default value is
`"native"`, which means the built-in server with automatic backend selection.

## Speech from another machine

The speech server talks to Lector over standard input and output, so it can
run anywhere you can pipe to. A common setup is running Lector on a Linux
server while speaking on your Mac or Windows machine.

The speech server is also available as a small standalone program,
`lector-tts`, so the speaking machine does not need all of Lector. Install it
there from a clone of this repository:

```bash
cargo install --locked --path crates/lector-tts
```

Then point Lector at it, for example over SSH:

```lua
lector.o.speech.server = {
  program = "ssh",
  args = { "my-mac", "lector-tts", "--backend", "av-foundation" },
}
```

Setting up SSH, including authentication and reconnecting, is up to you.

## Switching servers while running

From the [Lua REPL](scripting.md#lua-repl) or a hook, you can switch speech
servers without restarting:

```lua
lector.api.set_speech("native")
lector.api.set_speech({
  program = "/opt/speech/bin/other-server",
  args = { "--voice", "Samantha" },
})
```

The call returns immediately. Lector keeps using the old server until the new
one is ready. If the new one fails to start, the old one stays, and Lector
shows an error.

## Writing your own speech server

Any program that speaks the
[speech-host protocol](../protocols/speech-host.md) can be a speech server.

## If speech stops working

See [Troubleshooting](troubleshooting.md#nothing-is-spoken).
