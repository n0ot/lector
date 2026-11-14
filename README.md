# Lector

Lector is a screen reader for the terminal. It speaks new output as it
appears, and lets you go back and read anything on screen without disturbing
the program you are running.

## What you get

- **Live reading.** New terminal output is spoken as it arrives.
- **Review.** Read by line, word, or character, search, and copy text, all
  without moving the application's cursor.
- **Table navigation.** Move through terminal tables cell by cell, with row and
  column headers.
- **Clipboard history.** Keep the last ten things you copied and paste any of
  them.
- **tmux support.** Each pane of a `tmux -CC` session gets its own reading and
  review state.
- **Lua configuration.** Change options, rebind keys, and script your own
  commands.

## Install

For portable Linux binaries, download the `lector-<target>` artifact from a
successful [CI run](https://github.com/n0ot/lector/actions/workflows/ghostty-build.yml).
Choose `x86_64-unknown-linux-musl` for x86-64 or
`aarch64-unknown-linux-musl` for ARM64, extract the contained `.tar.gz`, and
place `lector` on your `PATH`. These executables work on both glibc and musl
distributions without installing Rust or a separate musl runtime.

To build from source, you need:

- [Rust](https://rustup.rs), installed with rustup
- `curl` and `tar`
- On Linux, a C compiler (`build-essential` on Debian and Ubuntu,
  `build-base` on Alpine)
- On macOS, the Xcode command line tools (`xcode-select --install`)

Then clone the repository and install:

```bash
git clone https://github.com/n0ot/lector.git
cd lector
# macOS and Alpine's packaged Rust:
cargo install --locked --path . --bin lector

# Other Linux distributions (x86-64 or ARM64):
rustup target add "$(uname -m)-unknown-linux-musl"
scripts/cargo-musl install --locked --path . --bin lector
```

This puts `lector` in `~/.cargo/bin`, which rustup adds to your `PATH`. The
first build downloads Lector's terminal engine and takes a few minutes.

To update later, run `git pull` and the same install command again.

Lector runs on macOS and Linux. Linux builds statically link musl so the
executable can run on both musl and glibc distributions of the same CPU
architecture. The [build guide](docs/internals/building.md)
covers supported platforms, offline builds, and build problems.

## Run

```bash
lector
```

Lector starts the shell named by your `SHELL` environment variable. To use a
different one:

```bash
lector --shell /bin/zsh
```

Exit your shell to leave Lector.

## Your first five minutes

In key names, `M-` means Alt (Option on a Mac) and `C-` means Control.

| Key | What it does |
| --- | --- |
| `F1` | Toggle help mode. While it is on, press any key to hear what it does. |
| `M-x` | Pause or resume speech. |
| `M-i` | Read the current line. `M-u` and `M-o` read the previous and next line. |
| `M-r` | Open Review to read, search, and copy earlier output. Press `q` to leave. |
| `M-'` | Turn automatic reading off or on. |
| `M-?` | Browse every key binding. |

Typing also pauses speech, so you never have to wait for Lector to finish.

## Documentation

**Using Lector**

- [Keys](docs/guide/keys.md): every default key binding.
- [Review](docs/guide/review.md): read, search, and copy earlier output.
- [Tables](docs/guide/tables.md): navigate tabular output.
- [Clipboard](docs/guide/clipboard.md): copy, paste, and clipboard history.
- [Speech](docs/guide/speech.md): voices, rate, and speech servers.
- [tmux](docs/guide/tmux.md): use Lector with `tmux -CC`.
- [Configuration](docs/guide/configuration.md): the config file and its options.
- [Scripting](docs/guide/scripting.md): key bindings, hooks, and automation in Lua.
- [Troubleshooting](docs/guide/troubleshooting.md): fixes for common problems.

**Building on Lector**

- [Speech-host protocol](docs/protocols/speech-host.md): write your own speech
  server.
- [Application accessibility protocol](docs/protocols/application-accessibility.md):
  let your terminal application tell Lector what to say.

**Working on Lector**

- [Contributing](CONTRIBUTING.md): set up, test, and submit changes.
- [Internals](docs/README.md#internals): architecture and design notes.

## License

Lector is released under the [MIT License](LICENSE).
