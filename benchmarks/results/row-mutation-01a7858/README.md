# Row mutation experiment

Base source: `8f317bcf0d3f837a1d8268dbeedd478315168788`.
Candidate source: `01a7858f4442b08c20b74b129bd5aa6c4dc96513`.
No benchmark workload or baseline threshold changed. The candidate changes only
`blit_surface` plus a neutral clipping/shared-row correctness test.

The existing per-cell signed-index conversion, saturating origin addition and
horizontal clipping now form a lazy visible-cell iterator. Peeking establishes
that the row actually receives writes before `Arc::make_mut` runs once outside
the copy loop. Empty or fully clipped rows do not detach cell storage. Row-level
vertical clipping and wrapped metadata rules are unchanged, as are cell clones
and layer order. Shared source snapshots remain immutable.

The target's allocation count and bytes are expected to stay identical: the
optimization removes repeated uniqueness checks after a row is already mutable;
it does not redesign row storage, add row sharing, or change cell representation.
The added test compares all Cell fields (including wide continuations, style and
hyperlink) against a destination-first reference across partial/full clipping,
negative/extreme origins and full-row metadata, checks untouched target cell Arc
identity, and keeps a shared source-row snapshot.

## Reproduction and initial paired measurements

Same machine/environment as the merged `split-scenes-372454f.md` report:
Apple M1 Pro/32 GiB, macOS26.6.2, Rust1.99.1, aarch64-apple-darwin,
verified Ghostty1.0.0 ReleaseFast archive and Zig0.17.0. Both binaries used one
isolated Cargo target, sequential builds with unchanged dependency/toolchain
inputs, and the supported `GHOSTTY_PREBUILT_ROOT` environment. The pristine base
binary was saved before editing candidate source. SHA-256:

- Base: `d2cc4f505cd3b65f8e100c3fdb0ea0576ebbfbb578f40798a82c09f8dc965b9a`
- Candidate: `abbe102bd42e5afcdfcfdda9569a7af71e2c4ec04d651e5bdfe404acc41e561c`

```sh
GHOSTTY_PREBUILT_ROOT=/Users/ncarpenter/git/n0ot/lector/target/ghostty-prebuilt \
  LECTOR_GHOSTTY_OPTIMIZE=ReleaseFast cargo build -r --locked
/path/to/saved-binary --check-baseline benchmarks/ghostty-release-baseline-macos-aarch64.json
```

Other workers paused builds, tests and Docker. No system settings changed.
Run order was base/candidate warmups, then AB/BA/AB; all eight full reports passed
all existing gates. `runs.json` records order, exit codes and wall duration.
Warmups are retained separately and excluded from initial three-run medians.
No report was discarded and no thresholds were relaxed.

Initial three-pair median elapsed deltas: split panes -22.1%, overlays -21.3%,
modeled tmux pane output -12.8%. Every allocation/byte counter matched. Other
workload elapsed deltas ranged from -3.4% to +2.1%; these are retained rather
than described as zero regression. Scheduler coalescing increased 0.8% and
backpressure 1.8%; coalescing p95 ranged 58.9–61.1us base versus 59.3–62.1us candidate
(unchanged gate 120us). Scheduler fixtures construct `PresentedScene::blank` and
only enqueue/drain; they never invoke compose/blit. These observations do not
establish a causal explanation for the small collateral timing shifts.

## Bounded follow-up and combined result

After inspecting the unchanged scheduler path, one additional AB/BA set was
approved to examine the collateral shifts. All four full gates passed; no
further repeats were run. `followups.json` records order/status/duration.
Panes improved 21.63/21.83% and overlays 20.74/20.47% in paired elapsed time;
coalescing shifts were +0.29/+0.25%, backpressure +0.32/+0.23%. Coalescing p95
shifted +0.35/-3.25%, backpressure 0/-1.08%. These do not demonstrate a material
collateral regression, but the small positive elapsed shifts remain disclosed.
All five measured pairs, excluding both warmups, are summarized below; complete
raw reports retain maxima, RSS and all counters rather than selecting winners.

| Workload | Base median ms | Candidate median ms | Elapsed delta | Median p95 delta |
| --- | ---: | ---: | ---: | ---: |
| shell-and-semantic-prompts | 165.862 | 161.835 | -2.43% | -1.53% |
| control-heavy-redraw | 64.697 | 64.000 | -1.08% | +0.00% |
| bounded-scrollback | 1152.343 | 1110.129 | -3.66% | -3.44% |
| single-cell-edits | 18.559 | 18.665 | +0.57% | +0.00% |
| status-line-replacements | 6.383 | 6.540 | +2.46% | +0.08% |
| cursor-moves | 14.832 | 14.701 | -0.88% | +0.00% |
| scrolling-output | 25.132 | 25.183 | +0.20% | +0.32% |
| tmux-like-structural-edits | 51.435 | 51.626 | +0.37% | +0.33% |
| zellij-like-layered-redraws | 19.169 | 19.299 | +0.67% | +0.32% |
| split-scene-panes | 194.409 | 151.342 | -22.15% | -23.19% |
| split-scene-overlays | 202.724 | 160.456 | -20.85% | -18.77% |
| tmux-control-pane-output | 399.321 | 348.843 | -12.64% | -13.14% |
| direct-compositor-pipeline | 103.169 | 101.759 | -1.37% | -1.19% |
| direct-drain-coalescing | 317.924 | 312.148 | -1.82% | -1.41% |
| scrolling-lf-receipts | 75.986 | 74.253 | -2.28% | -2.83% |
| event-boundary-coalescing | 589.729 | 593.046 | +0.56% | +0.42% |
| backpressure-recovery | 74.221 | 75.109 | +1.20% | +1.12% |
| kitty-media-recomposition | 6.560 | 6.527 | -0.50% | -0.02% |

Every reported allocation count/byte, output-size, receipt, queue/backpressure,
renderer-comparison and media transaction/store counter matched in all pairs.
No workload fixture, baseline ceiling, or product representation changed.

## Local validation

On candidate source01a7858, all required checks passed without retries:

```sh
cargo build -r --locked
cargo clippy -r --locked --all-targets --all-features -- -D warnings
cargo ghostty-bench --self-test
cargo test -r --locked --lib --test core --test terminal --test app --test presentation --test live_pty --test tmux
scripts/test-real-tmux-docker
```

Cargo used the same two Ghostty environment variables shown above. Release
library670, App207, core45, live-PTY13, presentation112, terminal112 and modeled
tmux198 passed; ordinary native/external tests stayed ignored. Docker passed
private-terminfo1 and real-tmux11. All twelve full benchmark runs passed the
unchanged checked-in baseline. The follow-up commit only adds these artifacts;
its executable code is identical to measured source01a7858.
