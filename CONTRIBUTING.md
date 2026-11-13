# Contributing to Lector

Thanks for helping. This page covers how to set up, test, and submit a
change.

## Finding your way around

Lector is a Cargo workspace:

| Package | What it is |
| --- | --- |
| `lector` | The application |
| `crates/lector-ghostty` | The terminal engine adapter |
| `crates/lector-tts` | The speech library and speech server |
| `xtask` | Maintenance commands |

The [architecture overview](docs/internals/architecture.md) explains how the
pieces fit, and its [source map](docs/internals/architecture.md#source-map)
shows where each part lives. Read it before changing who owns what, or any
protocol. The [documentation index](docs/README.md#internals) lists the
deeper design notes.

## Setting up

Install the prerequisites from the
[build guide](docs/internals/building.md#build-from-a-fresh-clone), then
build once:

```bash
cargo build --locked
```

With rustup, `rust-toolchain.toml` selects the exact Rust version the project
is tested with. `scripts/check-rust-toolchain` confirms which compiler is
actually being used.

The pinned components include `llvm-tools`, which supplies the LLVM shared
library used by Rust's debug-stripping tools on macOS. Keep it installed
when installing the toolchain manually as well.

The test commands use [cargo-nextest](https://nexte.st). Install the pinned
version:

```bash
cargo install cargo-nextest --version 0.9.143 --locked
```

## Testing

### While you work

These are quick:

```bash
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets --all-features
cargo test --locked --lib
```

Add the integration test closest to your change, for example:

```bash
cargo test --locked --test core capability_broker::
```

The integration test targets are `app`, `core`, `terminal`, `presentation`,
`speech`, `tmux`, `live_pty`, and `tmux_adversary_live`.

### Before you open a pull request

Run what CI runs:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
git grep -Il -E '^#!.*(ba)?sh' -- scripts tests/fixtures/pty | xargs shellcheck -x
cargo nextest run --locked --workspace --all-targets --all-features
cargo test --locked --workspace --doc --all-features
cargo ghostty-check
scripts/check_build_paths.sh
```

Also build a release binary, which is what users run:

```bash
cargo build -r --locked
```

### If you changed tmux support

Run the adversarial suite:

```bash
scripts/run-tmux-adversary --timeout 60 --nocapture
```

And the tests against a real tmux, which run only in Docker:

```bash
scripts/test-real-tmux-docker
```

The Docker run never uses your own tmux, its server, or its configuration.
Your checkout is mounted read-only. The first run needs network access to
build the image. Later runs reuse cached build output. Extra arguments are
passed to nextest. Set `LECTOR_REAL_TMUX_PLATFORM`, for example to
`linux/amd64`, to test another architecture under emulation.

See [testing tmux support](docs/internals/tmux/testing.md) for what each
level covers.

### Native speech tests

Ordinary test runs are silent. They never start real speech and do not need
working audio.

If you changed native speech or its test harness, also run the native suite
on a Mac with a working audio session:

```bash
scripts/test-native-speech
```

The suite is muted and each test has a 60-second limit. A timeout is a
failure, not a pass or a skip. Do not change system audio settings to make it
pass. Changes unrelated to speech do not need this suite.

### If you changed performance-sensitive code

See [Performance](docs/internals/performance.md) for the latency gate and the
release benchmark.

## Writing tests

- **Test the general rule, not the one report.** When you fix a bug, work out
  which invariant was broken and test that, at the lowest layer that can
  express it.
- **Use simulated input.** Prefer explicit byte sequences and modeled state
  over launching real programs. Use the project's own fixture programs when
  process or platform behavior is the thing being tested.
- **Keep reproductions neutral.** Reduce a bug report to minimal dummy data
  that keeps only the structure that matters. Do not copy user data, command
  output, or configuration into tests.
- **External programs run only in Docker.** Use one only when Lector
  deliberately implements compatibility with it, as with tmux. Ordinary test
  commands must never run them on the host.

## Submitting a change

1. Work on a branch and open a pull request against `master`. Do not push to
   `master` directly.
2. Make sure the local checks above pass first. Passing CI does not replace
   them.
3. Wait for CI. A change is done only when every required check passes on the
   latest revision and the branch is up to date with `master`.

Pull requests that change only Markdown files skip compilation in CI.

If you rename or add CI jobs, keep the `CI` gate job and the repository's
branch protection in step, so that every required job stays enforced.

## Documentation

Documentation is organized by reader:

| Folder | Reader | Style |
| --- | --- | --- |
| `README.md` | Someone deciding whether to try Lector | Short. Install, run, first keys. |
| `docs/guide/` | People using Lector | Task-focused. No implementation detail. |
| `docs/protocols/` | People writing software that talks to Lector | Precise specifications. |
| `docs/internals/` | People changing Lector | How and why it works. |

When you write:

- Put each fact in one place and link to it from elsewhere.
- Lead with what the reader needs to do or know. Put background after.
- Prefer short sentences, lists, and tables to long paragraphs.
- Keep implementation detail out of the user guide. Link to the internals.

## Maintainer notes

### Rust updates

The weekly `Tested Rust updates` workflow reads Rust's stable release
manifest and maintains one branch, `automation/rust-toolchain`, containing
only the version bump. It runs the full build matrix on that branch.

- A failure leaves `master` untouched and shows up in CI and the workflow
  summary. Fix the cause and re-run the failed CI.
- Unexpected edits on that branch stop the updater safely.

An agent-queue orchestrator turns a validated update into a pull request and
a queued issue:

```bash
python3 .github/scripts/rust_updates.py sync --project <PROJECT_NODE_ID>
```

Run it before dispatching work and at the session's existing 30-minute
checkpoints, not in a polling loop of its own. It uses the session's `gh`
authentication, creates or reuses one pull request and one issue, and never
resets an existing card's status. The update then goes through the normal
review, CI, and protected merge. A separate GitHub reviewer account is not
required.

While no orchestrator is running, the branch and its CI are still prepared.
The pull request simply waits.

The workflow dispatches CI explicitly because GitHub does not start workflows
from pushes made with a workflow token. See
[GitHub's documentation](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/trigger-a-workflow).

Alpine is a separate compatibility lane in CI. It uses Alpine's own packaged
Rust, not the pinned version, and logs which compiler it used.
