# Current-base validation of split-scene and tmux benchmarks

Validated source: `81dfdc69075da90e1ad36eeca3bb0e5f25dcfee7`.
Base: `9fbe4ec85dd148700cde132d2f6b2da58e26a73a`.
The following artifact commit changes no executable source.

The statistical baseline remains the three quiet measurements of source
`372454f37364353f75d0abf7c1e6c258016f14d9` on allocation-repair base
`fda95bdadcf6488e1319fae5567d2dffceade066`; see the corresponding report.
A clean rebase onto the safe-Lua-access change preserved all four patches
(`git range-diff` reported `=` for each). That base change scopes Lua callbacks;
benchmark screen readers have no configured hooks and return before entering
those scopes. No benchmark fixture or measurement fence changed during rebase.

The subsequent verification-only change requires exact ready/final tmux pane
contents after trimming trailing terminal whitespace, instead of substring
matches. Both checks remain outside measurement fences. This gate validates
that final source rather than relabeling the earlier statistical measurements.

The full release benchmark passed all existing and added thresholds in a
parent-coordinated quiet window with all other workers CPU-idle. Raw output is
`split-scenes-81dfdc6-gate.json`. A structural comparison against this base's
baseline JSON confirmed every existing record, measured value, threshold and
metadata value unchanged; only the three new workloads and their provenance
are added. No retries or threshold changes were needed.

Current-base local validation passed:

- `cargo build -r --locked` using the same verified ReleaseFast Ghostty archive
  and separate target directory documented in the statistical report.
- Release all-target/all-feature clippy with warnings denied; benchmark self-test.
- Release library666, App207, core45, live-PTY13, presentation111, terminal112,
  modeled tmux198 tests; native/external tests remained ignored.
- Following the exact-state assertion change: release build, strict clippy,
  benchmark self-test and all three benchmark report/baseline contract tests.
- Full release benchmark `--check-baseline` against the unchanged existing gates
  and the three additive gates.

Commands (with the two Ghostty environment variables shown in the statistical
report for Cargo invocations):

```sh
cargo build -r --locked
cargo clippy -r --locked --all-targets --all-features -- -D warnings
cargo ghostty-bench --self-test
cargo test -r --locked --lib --test core --test terminal --test app --test presentation --test live_pty --test tmux
cargo test -r --locked --test terminal ghostty_benchmark
target/release/lector-ghostty-bench --check-baseline benchmarks/ghostty-release-baseline-macos-aarch64.json
scripts/test-real-tmux-docker
```

Docker validation passed: private-terminfo overlay1 and real-tmux11. The harness
also verified nextest architecture selection/checksum rejection. External
programs ran only inside the container; no native speech test was invoked.
