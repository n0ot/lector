# Contributing to Lector

Lector is a Cargo workspace containing the main `lector` package, the
`lector-ghostty` native adapter, and the `lector-xtask` maintenance tool. Read
[the architecture guide](docs/architecture.md) before changing runtime
ownership or protocol boundaries; [the documentation index](docs/README.md)
links the more detailed contracts.

## Repository map

- `src/main.rs`, `src/app.rs`, and `src/app/`: event-loop and application
  orchestration.
- `src/terminal.rs`, `src/presentation.rs`, and `src/output_scheduler.rs`:
  terminal modeling and physical presentation.
- `src/screen_reader/`, `src/commands/`, `src/views/`, `src/speech/`, and
  `src/lua/`: accessible interaction and speech.
- `src/tmux_*.rs`: tmux control-mode protocol and state boundaries.
- `src/bin/`: explicitly declared support binaries and benchmarks.
- `crates/lector-ghostty/` and `build_support/`: native Ghostty adapter and
  shared build logic.
- `xtask/`: pinned-dependency maintenance commands.
- `tests/` and `tests/fixtures/pty/`: integration tests and live fixtures.
- `scripts/`: developer checks, dependency bootstrap, tracing, and live tests.
- `docs/`: design contracts and operational documentation.

## Fast verification

Run these while iterating:

```bash
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets --all-features
cargo test --locked --lib
```

Add the narrow integration test affected by the change, for example
`cargo test --locked --test capability_broker`.

## Full verification

Before submitting a change, mirror the main CI checks:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
git grep -Il -E '^#!.*(ba)?sh' -- scripts tests/fixtures/pty | xargs shellcheck -x
cargo test --locked --workspace --all-targets --all-features
cargo ghostty-check
scripts/check_build_paths.sh
```

Changes to tmux control mode should also run the kill-bounded live suite on a
machine with tmux installed:

```bash
scripts/run-tmux-adversary --timeout 60 --nocapture
```

Native dependency prerequisites, cache behavior, and supported targets are in
[the Ghostty build guide](docs/ghostty-builds.md).
