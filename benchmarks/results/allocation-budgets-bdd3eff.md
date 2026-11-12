# Restoring the existing allocation budgets (#66)

Base: `099f35fe2ffba5348fb49941a35fdb8eaffb477f`.
Measured repair: `bdd3eff9281e50bdc98e780a9d9fca1d9631d494`.
The artifact commit changes no executable source. No baseline values, workload
boundaries, counters, or ceilings were changed; #49's new workloads are separate.

## Environment and commands

2026-11-12; Apple M1 Pro, 32 GiB RAM; macOS 26.6.2 (25G83);
`aarch64-apple-darwin`; Rust `1.99.1 (58cf8f9c3 2026-10-13)`;
Ghostty `0fa3d577a5614150ff8e2c152de5fb9d5c6d2dec` / 1.0.0, Zig 0.17.0,
ReleaseFast. Verified Ghostty archive SHA-256:
`8416b11e1be8be942db25794a52b988306413f3ee2253d825f8e6150465c185b`.

```sh
GHOSTTY_PREBUILT_ROOT=/Users/ncarpenter/git/n0ot/lector/target/ghostty-prebuilt \
  LECTOR_GHOSTTY_OPTIMIZE=ReleaseFast cargo build -r --locked
GHOSTTY_PREBUILT_ROOT=/Users/ncarpenter/git/n0ot/lector/target/ghostty-prebuilt \
  LECTOR_GHOSTTY_OPTIMIZE=ReleaseFast cargo ghostty-bench --self-test
target/release/lector-ghostty-bench --output REPORT.json \
  --check-baseline benchmarks/ghostty-release-baseline-macos-aarch64.json
```

The prebuilt root is a machine-local artifact reuse setting; the regular
bootstrap can produce the same verified build on another machine. Each worktree
has its own Cargo target. Base comparisons used the separately built and verified
primary-checkout release binary; candidate comparisons used this worktree's binary.

Other workers held builds/tests during native measurements. A full-process
warmup for each revision was recorded but excluded from the comparison. The
planned order was base/candidate warmup, candidate/base 1, base/candidate 2,
candidate/base 3. Candidate 3 failed only the scrolling timing gate; the CLI exits
before saving JSON on failure. Its exact error is retained in
`allocation-budgets-bdd3eff-failed-run.txt`. A bounded follow-up ran base 3,
candidate 3-report without the gate, and candidate 4 with the full unchanged gate.
No further timing retries were run. Candidate warmup, 1, 2 and 4 passed every gate.

The failed scrolling sample was p95 205,500 ns and 0.208 MiB/s, against 150,000 ns
and 0.220 MiB/s limits. Follow-up candidate samples were 75,709/78,833 ns and
0.288/0.284 MiB/s, versus base 75,333 ns and 0.287 MiB/s. A read-only host activity
check after the outlier observed unrelated editor and audio activity; the system
reported no recorded thermal/performance warning. This does not prove the cause,
but the bounded paired follow-up did not reproduce the failure. No processes or
system settings were changed. The table below describes saved completed reports;
it does **not** erase the separate failed sample.

| Workload | Allocations: base → repair | Allocated bytes: base → repair | Base p95 range (ns) | Repair saved-report p95 range (ns) |
| --- | ---: | ---: | ---: | ---: |
| shell-and-semantic-prompts | 275,949 → 259,949 | 361,744,504 → 360,460,504 | 13,334–13,583 | 13,250–13,334 |
| control-heavy-redraw | 236,468 → 195,508 | 138,159,152 → 134,872,112 | 6,125–6,334 | 6,042–6,208 |
| direct-compositor-pipeline | 465,488 → 435,488 | 178,292,363 → 175,162,363 | 10,459–10,750 | 10,292–10,708 |
| direct-drain-coalescing | 1,313,800 → 1,193,800 | 591,863,236 → 579,343,236 | 31,959–32,417 | 31,209–32,334 |
| scrolling-lf-receipts | 129,000 → 125,000 | 163,403,000 → 163,082,000 | 74,750–77,042 | 75,709–78,833 |

## Attribution and representation tradeoffs

Experiments were cumulative against the same base, with identical benchmark
inputs. Diagnostic timings were not used to establish a speedup.

| Experiment | Shell allocations | Control allocations | Direct allocations | Coalescing allocations |
| --- | ---: | ---: | ---: | ---: |
| Base | 275949 | 236468 | 465488 | 1313800 |
| In-place printed-run normalization | 267949 | 215988 | 445488 | 1233800 |
| Plus amortized observer String growth | 263949 | 205748 | 445488 | 1233800 |
| Plus inline single-span cell-write evidence | 259949 | 195508 | 435488 | 1193800 |
| Existing ceiling | 270000 | 210000 | 450000 | 1200000 |

Printed-run conversion previously built a second Vec for normalization. The
repair normalizes in place with the same coalescing function used by summary
merges. Moving text into an empty predecessor avoids copying it while preserving
CR/LF and Continue boundaries. A neutral grouping test covers all partitions of
boundary-bearing text, including a CR/LF pair with nonempty LF text.

Observer WriteRuns previously began with an exact one-byte `char::to_string`
allocation and reallocated on the next print. `String::new` followed by `push`
uses ordinary amortized growth, removing one allocation for multicharacter runs.
Isolated characters use seven more allocated bytes each in this experiment:
+70,000 direct and +280,000 coalescing bytes, with unchanged allocation counts.
The final repair still reduces total bytes for both and passes every byte ceiling.

Temporary atomic histogram instrumentation found exactly one cell-write span in
10,000 direct, 40,000 coalescing and 1,000 scrolling source updates (no zero or
multiple-span source updates in those workloads). It was removed before final
measurement. `SmallVec<[TerminalCellWrite; 1]>` retains all additional spans by
spilling; the explicit direct smallvec dependency uses the already-locked 1.15.1.
On this target `UpdateSummary` grows from 304 to 312 bytes; `TerminalCellWrite`
is 16 bytes. Timings above include the increased layout/copy cost.

The changed field is public but doc-hidden; its concrete type changes from Vec
to SmallVec. There are no repository consumers constructing it as a Vec, and
UpdateSummary is not serialized. No serde boundary changes. The accessibility
journal's fixed `size_of<AccessibilityJournalEntry>` already includes inline
storage, so it is counted once; spilled **capacity**, rather than merely length,
is charged additionally. A test covers inline, spilled, reserved and cleared
storage. A separate test verifies revision and deletion boundaries after multiple
disjoint writes spill. Existing structural suffix, fragmentation, PTY replies,
receipt publication, history, scheduler/backpressure and media gates remain intact.

An empty-target printed-Vec move experiment produced no allocation benefit and
was removed. No identity-type cleanup or benchmark-source changes are included.

## Validation

- `cargo build -r --locked` and `cargo ghostty-bench --self-test`: passed.
- `cargo test -r --locked --lib --test terminal --test app --test presentation --test live_pty --test tmux`: passed (655 library, 112 terminal, 206 app, 111 presentation, 13 live PTY fixture, 198 modeled tmux; native/external ignored tests stayed inert).
- `cargo test -r --locked -p lector-ghostty --lib`: 6 passed.
- Full unchanged release gate: candidate warmup, 1, 2 and 4 passed; the isolated timing failure is documented above.
- `scripts/test-real-tmux-docker`: passed (private terminfo fixture 1/1 and real tmux compatibility 11/11), using the default Docker image and named cache volumes.
