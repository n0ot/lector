# Lector documentation

New here? Start with the [README](../README.md) to install Lector and learn
the first few keys.

## User guide

How to use Lector day to day.

- [Keys](guide/keys.md): every default key binding.
- [Review](guide/review.md): read, search, and copy earlier output.
- [Tables](guide/tables.md): navigate tabular output.
- [Clipboard](guide/clipboard.md): copy, paste, and clipboard history.
- [Speech](guide/speech.md): voices, rate, and speech servers.
- [tmux](guide/tmux.md): use Lector with `tmux -CC`.
- [Configuration](guide/configuration.md): the config file and its options.
- [Scripting](guide/scripting.md): key bindings, hooks, and automation in Lua.
- [Troubleshooting](guide/troubleshooting.md): fixes for common problems.

## Protocols

Specifications for software that talks to Lector.

- [Speech-host protocol](protocols/speech-host.md): how Lector talks to a
  speech server. Read this to write your own.
- [Application accessibility protocol](protocols/application-accessibility.md):
  how a terminal application can tell Lector what to say.

## Internals

How Lector works, for people changing it. Read
[Contributing](../CONTRIBUTING.md) first for setup and testing.

- [Architecture](internals/architecture.md): the big picture, threads, data
  flow, and where the code lives. Start here.
- [Reading model](internals/reading-model.md): how Lector decides what to
  speak and when.
- [Terminal](internals/terminal.md): terminal emulation, compatibility, and
  rendering.
- [Performance](internals/performance.md): latency targets, benchmarks, and
  timing diagnostics.
- [Building](internals/building.md): the native terminal engine, supported
  targets, offline builds, and upgrades.
- [tmux](internals/tmux/README.md): control-mode integration.
