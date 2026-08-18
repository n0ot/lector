# Lector documentation

Start with [Architecture](architecture.md) for the runtime ownership model,
data flow, concurrency boundaries, and source map. Contributors should also
read the repository-root [CONTRIBUTING guide](../CONTRIBUTING.md).

## Build system

- [Building with libghostty-vt](ghostty-builds.md) covers the pinned native
  dependency, supported targets, offline builds, and upgrades.

## tmux control mode

- [Complete behavior](tmux-completion.md) is the integration overview,
  resource-bound reference, and troubleshooting guide.
- [Control parser](tmux-control-parser.md) defines framing and failure bounds.
- [Gateway routing](tmux-gateway.md) defines byte ownership at connection
  boundaries.
- [Topology](tmux-topology.md) describes identity and reconciliation.
- [Pane composition](tmux-panes.md) describes pane-local engines and layout.
- [Input and sizing](tmux-input.md) covers keyboard, paste, focus, mouse, and
  resize handling.
- [Prefix handling](tmux-prefix.md) covers discovery, command routing, and
  accessible choosers.
- [Bell monitoring](tmux-bells.md) describes pane-scoped bell behavior.
- [Adversarial testing](tmux-adversary.md) documents the hostile control peer
  and live regression suite.
