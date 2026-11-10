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
- `tests/` and `tests/fixtures/pty/`: integration tests and Lua configuration
  fixtures.
- `scripts/`: developer checks, dependency bootstrap, tracing, and live tests.
- `docs/`: design contracts and operational documentation.

## Rust toolchain

`rust-toolchain.toml` is the exact tested Rust release for local rustup,
native/static CI, and the Ubuntu tmux image. Run `scripts/check-rust-toolchain`
to verify the compiler actually invoked, including any environment override.
CI also checks the native host or installed cross-target standard library.
Alpine intentionally uses its distribution-packaged Rust as a separate
compatibility lane: it has no rustup shim, so the repository pin does not
select its compiler. That lane logs and verifies its compiler host explicitly.

## Fast verification

Run these while iterating:

```bash
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets --all-features
cargo test --locked --lib
```

Add the narrow integration test affected by the change, for example
`cargo test --locked --test core capability_broker::`. The consolidated
integration targets are `app`, `core`, `terminal`, `presentation`, `speech`,
`tmux`, `live_pty`, and `tmux_adversary_live`. The small `ghostty_build` target
stays separate so CI can verify that its headless linkage has no GUI dependency.

## Full verification

Before submitting a change, mirror the main CI checks:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
git grep -Il -E '^#!.*(ba)?sh' -- scripts tests/fixtures/pty | xargs shellcheck -x
cargo nextest run --locked --workspace --all-targets --all-features
cargo test --locked --workspace --doc --all-features
cargo ghostty-check
scripts/check_build_paths.sh
```

Nextest keeps the process-sensitive live PTY and real-tmux tests serialized and
limits other process suites through `.config/nextest.toml`. Install
the pinned project version with
`cargo install cargo-nextest --version 0.9.143 --locked` when it is not already
available.

Changes to tmux control mode should also run the kill-bounded adversary suite:

```bash
scripts/run-tmux-adversary --timeout 60 --nocapture
```

The real-tmux integration group runs only in a disposable Ubuntu 24.04
container, without using the host Rust toolchain, tmux binary, tmux server, or
configuration. Ordinary Cargo and Nextest runs skip these tests:

```bash
scripts/test-real-tmux-docker
```

The runner also checks host-terminfo integration against the container's
`infocmp` and `tic`; ordinary unit tests use the bundled terminfo fixture.
The checkout is mounted read-only. Docker-managed volumes retain Cargo,
Ghostty, and test artifacts between runs; tmux processes are discarded with
the container, while socket paths stay in Docker-managed storage rather than
the checkout or host tmux runtime. The first run needs network access to build
the image and populate those caches. Set
`LECTOR_REAL_TMUX_PLATFORM`, for example to `linux/amd64`, when an explicitly
emulated architecture is useful. Extra arguments are forwarded to Nextest.

Native dependency prerequisites, cache behavior, and supported targets are in
[the Ghostty build guide](docs/ghostty-builds.md).

## Tested Rust updates

The weekly `Tested Rust updates` workflow reads Rust's official stable manifest
and maintains one `automation/rust-toolchain` PR. It changes only the canonical
pin, explicitly dispatches the full build matrix, and retains the tested master
pin on failure. Repeated runs reuse the PR; a changed base or pin requires new
CI and review. Failed validation is linked in a single updated PR status comment.

Operational setup requires Actions permission to create PRs (repository Actions
settings), and `contents`, `pull-requests`, and `actions` write permissions for
its workflow token. No PAT is needed: explicit `workflow_dispatch` is supported
with `GITHUB_TOKEN`, avoiding suppressed or approval-gated implicit PR runs.
See [GitHub's event documentation](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/trigger-a-workflow).

Set repository variable `RUST_UPDATE_REVIEWER` to the account of an independently
operated review agent/app. That integration must actually review update PRs and
submit an `APPROVED` GitHub review on the current commit; merely setting a name
does not install or start a reviewer. It must differ from the PR author. Missing,
stale, dismissed, or negative review blocks merge, even when CI passes. The
workflow never self-approves. No reviewer service or credentials are bundled;
until one is configured, PR creation/testing works but automatic merging is
intentionally incomplete.

Protected merging runs after CI or a review event, or the next weekly/manual
run. It requires a current approved head, a clean/up-to-date branch, and the
existing protected checks; there is no administrator bypass. If authentication
or dispatch fails, inspect the workflow log and PR status comment, fix access,
and rerun `Tested Rust updates`. An orphan or unexpectedly modified automation
branch requires inspection before recovery; it is never silently overwritten.
