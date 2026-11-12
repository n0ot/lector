# Split-scene and tmux-control baseline after allocation repair

Measured source: `372454f37364353f75d0abf7c1e6c258016f14d9`.
Base: `fda95bdadcf6488e1319fae5567d2dffceade066` (includes allocation repair #68).
The subsequent baseline/artifact commit changes no executable source.

2026-11-12; target `aarch64-apple-darwin`; Rust
`1.99.1 (58cf8f9c3 2026-10-13)`; Apple M1 Pro, 32 GiB RAM;
macOS 26.6.2 (25G83); mlua 0.12.2. Ghostty 1.0.0 commit
`0fa3d577a5614150ff8e2c152de5fb9d5c6d2dec`, Zig 0.17.0, ReleaseFast;
verified archive SHA-256
`8416b11e1be8be942db25794a52b988306413f3ee2253d825f8e6150465c185b`.

```sh
GHOSTTY_PREBUILT_ROOT=/Users/ncarpenter/git/n0ot/lector/target/ghostty-prebuilt \
  LECTOR_GHOSTTY_OPTIMIZE=ReleaseFast cargo build -r --locked
GHOSTTY_PREBUILT_ROOT=/Users/ncarpenter/git/n0ot/lector/target/ghostty-prebuilt \
  LECTOR_GHOSTTY_OPTIMIZE=ReleaseFast cargo ghostty-bench --self-test
```

The artifact root is machine-local; the regular bootstrap can produce the same
verified archive elsewhere. The benchmark itself uses a separate Cargo target.
Other workers held builds/tests during the full-process warmup, three measured
runs, and final gate. No system settings were changed. Warmup took 8.75 s;
measured runs took 8.78, 8.79 and 8.76 s; the final gate took 8.83 s.

```sh
target/release/lector-ghostty-bench --output benchmarks/results/split-scenes-372454f-warmup.json
target/release/lector-ghostty-bench --output benchmarks/results/split-scenes-372454f-run1.json
target/release/lector-ghostty-bench --output benchmarks/results/split-scenes-372454f-run2.json
target/release/lector-ghostty-bench --output benchmarks/results/split-scenes-372454f-run3.json
target/release/lector-ghostty-bench \
  --check-baseline benchmarks/ghostty-release-baseline-macos-aarch64.json \
  --output benchmarks/results/split-scenes-372454f-gate.json
```

Only the declared warmup was discarded. All repetitions and the final gate are
preserved; no measurement retry or failed measurement run was omitted. New measured baseline fields use
the worst repetition per metric. Only the three added workload thresholds were
refreshed: half minimum throughput, twice maximum p95, 10% allocation/byte/output
headroom, and 100% verified frames/physical receipts. Every baseline workload,
measured value and ceiling already present in master is unchanged; a JSON equality
check against `fda95bd` verified all original arrays and the first three compositor
entries. The historical [141e52e capture](split-scenes-141e52e.md) remains archived,
including its inherited gate failures, which #68 repaired without relaxed ceilings.

| Workload (10,000 iterations) | p95 range (ns) | Allocations | Allocated bytes | Verified frames / receipts |
| --- | ---: | ---: | ---: | ---: |
| split-scene-panes | 19,417–20,083 | 1,300,000 | 1,419,440,000 | 10,000 |
| split-scene-overlays | 20,292–20,375 | 1,440,000 | 1,424,120,000 | 10,000 |
| tmux-control-pane-output | 40,083–40,458 | 2,160,030 | 2,968,831,284 | 10,000 |

## Measurement and correctness boundaries

`split-scene-panes` and `split-scene-overlays` call production
`PresentedScene::compose` inside each latency/allocation fence. The first pane
is non-covering, so composition executes `blit_surface`, as do subsequent panes
and overlays. The fixtures contain top/left/right/bottom clipping, a full-width
row, plain and styled content, and hyperlinks. Both variants are prebuilt and
warmed before measurement. Composition results must equal prevalidated frames.

The final overlay fixture has ten visible overlapping cells: a top layer at
(5, 60), z=20, and a lower layer at (7, 70), z=10, each 3×20. The lower layer clips
at the right edge. Insertion order is deliberately opposite to z-order, and
blank cells in the top layer must occlude lower text. Setup rejects a fixture
without visible overlap. An independent destination-first reference selects the
topmost covering source for every cell, including blank and clipped regions;
full VT rendering and RenderOracle verification then check every measured frame.
Fixture construction, references, full rendering, oracle replay and validation
allocations are outside the composition fences.

For these scene-only workloads, `input_bytes` is encoded fixture-equivalent size,
not parsed wire traffic; output bytes count verification rendering and completed
renders count verified frames. `measurement_scope` exposes that distinction.

`tmux-control-pane-output` measures real gateway/control parsing and App dispatch,
including `process_tmux_control`, terminal mutation, rendering, scheduler writes,
flush confirmation and physical receipt publication. Its neutral handshake and
input strings are prepared before measurement; each escaped output message
arrives in two fragments. A ready pane is required before timing, and final pane
text plus one receipt per update are checked. Input bytes are wire bytes. No tmux
process is launched by the benchmark; speech uses a silent double.

## Successful validation

- Release build and all-target/all-feature release clippy with warnings denied.
- `cargo ghostty-bench --self-test`.
- Release terminal112, presentation111, App206, live-PTY fixture13 and modeled
  tmux198 tests; ordinary native/external tests stayed ignored.
- The three `ghostty_benchmark` contract tests after refreshing baseline JSON.
- Full release benchmark with every existing gate plus the three added gates.
- `scripts/test-real-tmux-docker`: private-terminfo1 and real-tmux11 passed.

Schema 1 is extended additively with workload names and `measurement_scope`.
Consumers must start from the merged benchmark revision and require all six
compositor workloads, as the report/baseline contract tests do. Existing renderer,
full-history, scheduler/backpressure and media workloads and gates are retained.
No product optimization is part of this benchmark change.
