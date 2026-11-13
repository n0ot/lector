# Lector documentation

For installation and everyday use, start with the [user guide](../README.md).
For implementation work, start with [Architecture](architecture.md) for the runtime ownership model,
data flow, and concurrency boundaries; use its [source map](architecture.md#source-map)
to locate implementation ownership. Contributors should also
read the repository-root [CONTRIBUTING guide](../CONTRIBUTING.md).

## Build system

- [Building with libghostty-vt](ghostty-builds.md) covers the pinned native
  dependency, supported targets, offline builds, and upgrades.

## Performance and diagnostics

- [Interactive performance](performance.md) covers stabilization, latency gates,
  native-audio validation, and diagnostic timelines.

## Speech

- [Speech-host protocol](speech-driver-protocol.md) defines custom-host
  configuration, bidirectional NDJSON JSON-RPC framing, capabilities, events,
  queue and pause state, deadlines, restart policy, and process cleanup. Its
  machine-readable method and message schema is
  [`crates/lector-tts/openrpc.json`](../crates/lector-tts/openrpc.json).
- [Application-authored accessibility](application-accessibility.md) documents
  Lector's consumer contract, fail-safe lifecycle, and pane isolation, and links
  the externally maintained producer specification. The
  standalone [`lector.nvim`](https://github.com/n0ot/lector.nvim) plugin is its
  Neovim producer.

## tmux control mode

- [Control-mode behavior](tmux-completion.md) is the integration overview,
  resource-bound reference, and troubleshooting guide.
- [Control parser](tmux-control-parser.md) defines framing and failure bounds.
- [Gateway routing](tmux-gateway.md) defines byte ownership at connection
  boundaries.
- [Control transitions](tmux-control-transitions.md) defines connection-local
  results and the ordered App effects that apply them.
- [Topology](tmux-topology.md) describes identity and reconciliation.
- [Pane composition](tmux-panes.md) describes pane-local engines and layout.
- [Input and sizing](tmux-input.md) covers keyboard, paste, focus, mouse, and
  resize handling.
- [Prefix handling](tmux-prefix.md) covers discovery, command routing, and
  accessible choosers.
- [Bell monitoring](tmux-bells.md) describes pane-scoped bell behavior.
- [Adversarial testing](tmux-adversary.md) documents the hostile control peer
  and live regression suite.
