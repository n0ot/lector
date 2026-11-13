# Split-scene and tmux-control baseline

Historical pre-repair capture. The allocation failures below were subsequently
repaired by #68. The [current baseline](split-scenes-372454f.md) uses the final
overlapping fixture and passes every unchanged pre-existing gate.

Source revision: `141e52e` (`141e52e4d33010b7e895817731f4cef7687303c1`); base: `099f35f`.
The later baseline/artifact commit changes no executable benchmark source.

- Target: `aarch64-apple-darwin`, Rust `1.99.1 (58cf8f9c3 2026-10-13)`.
- Machine: Apple M1 Pro, 32 GiB RAM; macOS 26.6.2 (25G83).
- Ghostty: `0fa3d577a5614150ff8e2c152de5fb9d5c6d2dec`, version 1.0.0,
  Zig 0.17.0, `ReleaseFast`, verified archive SHA-256
  `8416b11e1be8be942db25794a52b988306413f3ee2253d825f8e6150465c185b`.
- Build: `GHOSTTY_PREBUILT_ROOT=/Users/ncarpenter/git/n0ot/lector/target/ghostty-prebuilt LECTOR_GHOSTTY_OPTIMIZE=ReleaseFast cargo build -r --locked`.
  The artifact root is machine-specific; use a verified ReleaseFast archive on
  another machine, or let the normal bootstrap create one.

The orchestrator held all other workers' builds/tests during timing. One full
process warmup was discarded, followed by three sequential full-process runs:

```sh
target/release/lector-ghostty-bench --output benchmarks/results/split-scenes-141e52e-warmup.json
target/release/lector-ghostty-bench --output benchmarks/results/split-scenes-141e52e-run1.json
target/release/lector-ghostty-bench --output benchmarks/results/split-scenes-141e52e-run2.json
target/release/lector-ghostty-bench --output benchmarks/results/split-scenes-141e52e-run3.json
```

Each new workload has 10,000 measured iterations. Scene workloads additionally
warm each prebuilt variant once, excluding scene construction, rendering and
oracle checks from composition fences. Tmux bootstrap is excluded; the measured
input enters the real control parser in fragments. See `docs/internals/performance.md`
for counter meanings and correctness checks. Raw reports include all existing
workloads and p50/p95/max latencies, throughput and allocation/output counters.
No repetitions were discarded except the declared warmup. New baseline values
use worst repetition per metric, with half-throughput/twice-p95 limits and 10%
allocation/byte/output headroom; receipts/verified frames must remain 100%.
This accommodates timing noise without changing existing workload ceilings.

| Workload | p95 range (ns) | Allocations | Allocated bytes |
| --- | ---: | ---: | ---: |
| split-scene-panes | 20,125–20,625 | 1,300,000 | 1,419,440,000 |
| split-scene-overlays | 21,000–21,500 | 1,440,000 | 1,424,120,000 |
| tmux-control-pane-output | 40,958–41,708 | 2,190,029 | 2,970,761,220 |

## Gate failures at this historical revision

All new workloads and correctness checks pass. The full
`--check-baseline benchmarks/ghostty-release-baseline-macos-aarch64.json` run
fails these unchanged allocation ceilings:

| Existing workload | Observed allocations | Ceiling |
| --- | ---: | ---: |
| shell-and-semantic-prompts | 275,949 | 270,000 |
| control-heavy-redraw | 236,468 | 210,000 |
| direct-compositor-pipeline | 465,488 | 450,000 |
| direct-drain-coalescing | 1,313,800 | 1,200,000 |

The orchestrator independently reproduced **exactly the same allocation
counts** on unmodified master `099f35f`. That comparison's timing was contaminated
by a concurrent test build and is not performance evidence. Existing workload
measurement code has not substantively changed since the baseline revision
`51a2331`, but production state handling and the toolchain/Ghostty versions have.
The existing baseline dates to 2026-08-21 and Ghostty 0.1.0-dev. New allocation
sites in linear-output suffix handling and cell-write evidence are candidates
for investigation, not proven causes. No existing ceilings were relaxed and no
product optimization was made. This was an unresolved local validation
blocker at capture time; passing correctness tests or CI did not make the full
gate pass. It was subsequently resolved by the separately reviewed #68 repair.

Local successful checks at this source revision: release build; benchmark
self-test; terminal suite (111); presentation suite (111); modeled tmux suite
(198 passed, 11 Docker-only ignored); app suite (206); live PTY project fixtures
(13 passed, 2 ignored). Native speech and external host programs were not run.
