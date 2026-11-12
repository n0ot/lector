# Release profile experiment

Retain the default release profile; no product setting is changed. Thin LTO was initially promising, but the current-base follow-up below failed a full timing gate while both default runs passed. Combined with higher build cost and a larger application binary, the mixed evidence does not justify global adoption.

## Original experiment

On the original measured source, four balanced-order runs show repeatable improvements in parser/history (median 8.6–10.1%), composition (2.2–7.1%), scheduler (5.5%) and media (4.4%). Thin's parser/history, split-scene, tmux, scheduler and media elapsed ranges do not overlap the default ranges. Renderer differences are small and overlapping (median -1.7% to +1.1%); they do not support a distinct renderer improvement. All 20 full baseline gates passed, with unchanged allocation counts/bytes, outputs and receipt counts. No workload, baseline, panic strategy or native dependency optimization changed.

Fat LTO with implicit codegen defaults regressed split scenes by 14–16% and scheduling by 42%, despite parser gains. Fat LTO with one codegen unit removes those regressions and reduces binary size, but provides broadly similar runtime gains to thin while costing substantially more build time. Neither fat alternative was selected. These original results describe one macOS ARM64 revision, not a universal cross-platform speedup or the final adoption decision.

## Fixed inputs and reproduction

Measured source: `8f317bcf0d3f837a1d8268dbeedd478315168788`, including the merged split-scene/tmux workloads. Rust `1.99.1 (58cf8f9c3 2026-10-13)`, LLVM 23.1.1, host `aarch64-apple-darwin`, macOS 26.6.2. Hardware: MacBookPro18,3, 10 logical CPUs, 32 GiB RAM. Same host, toolchain, Cargo.lock and workload for every profile; no other worker builds or Docker runs during measurements. A required queue-sync process lasting less than one second occurred during the build window. Build times are single observations, not precise reproducible estimates.

Ghostty 1.0.0 commit `0fa3d577a5614150ff8e2c152de5fb9d5c6d2dec`, Zig 0.17.0, `ReleaseFast`, verified archive SHA256 `8416b11e1be8be942db25794a52b988306413f3ee2253d825f8e6150465c185b`. A copy of the existing verified archive was used in the isolated worktree; no shared cache was deleted or modified.

Four fresh Cargo target directories were used. Each profile ran `/usr/bin/time -p cargo build -r --locked -vv` with `CARGO_TARGET_DIR` pointing to its own directory and `LECTOR_GHOSTTY_OPTIMIZE=ReleaseFast`. Profile environment overrides were:

| Profile | Overrides |
| --- | --- |
| Default | none |
| Thin | `CARGO_PROFILE_RELEASE_LTO=thin` |
| Fat | `CARGO_PROFILE_RELEASE_LTO=fat` |
| Fat/1 | `CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1` |

Actual compiler flags are recorded in [builds.json](release-profile-8f317bc/builds.json): all use opt-level=3 and strip debuginfo; thin/fat binaries explicitly receive the corresponding LTO flag, and only fat/1 receives codegen-units=1. Other profiles leave codegen units implicit; no numeric default is inferred from the invocation. No invocation sets panic or incremental compilation; default unwinding is retained. LTO dependency bitcode flags change as Cargo requires.

Clean times include Rust dependencies, vendored C dependencies and all six binaries, with cached dependency sources and verified Ghostty already available. They exclude compiling/downloading Ghostty or Zig and are not cold-machine measurements. After each clean build, the same command was run unchanged, then after appending a neutral comment to `src/main.rs`. The source was restored after that leaf rebuild. This measures Cargo rebuilding/relinking the application, not incremental compilation: release incremental compilation remains disabled. A preliminary successful default build (31.64s) was excluded because `/usr/bin/time -l` subsequently failed a sandbox-denied system-statistics query; its target/log were preserved and all four measured profiles used fresh targets with portable timing.

| Profile | Clean s | No-op s | Leaf rebuild s | Lector bytes | Benchmark bytes |
| --- | ---: | ---: | ---: | ---: | ---: |
| Default | 31.27 | 0.19 | 13.43 | 11,077,296 | 7,412,000 |
| Thin | 44.59 | 0.20 | 28.07 | 11,358,032 | 7,733,232 |
| Fat | 62.98 | 0.20 | 48.60 | 9,204,800 | 6,494,976 |
| Fat/1 | 77.40 | 0.19 | 60.84 | 8,744,784 | 6,285,840 |

Thin increases observed clean build cost 43%, leaf rebuild 109%, and application size 2.5%. The current-base uncertainty below does not justify accepting this cost. All product profiles remain unchanged.

## Runtime samples

Each binary ran `lector-ghostty-bench --check-baseline benchmarks/ghostty-release-baseline-macos-aarch64.json`, one warmup then four measured runs. Orders were default/thin/fat/fat-one for warmup, then default/thin/fat-one/fat; thin/fat/default/fat-one; fat/fat-one/thin/default; fat-one/default/fat/thin. All runs retained unchanged allocation/latency gates. Raw [reports](release-profile-8f317bc/) include every warmup and measured sample, with latency tails, allocations, output and receipt metrics. Warmups are excluded from the table; changes compare medians, and negative means faster. These are workload elapsed times, not total process duration.

| Workload | Default ms (range) | Thin change | Fat change | Fat/1 change | Thin p95 change |
| --- | ---: | ---: | ---: | ---: | ---: |
| shell-and-semantic-prompts | 163.18 (162.94–165.32) | -8.6% | -9.8% | -7.2% | -8.8% |
| control-heavy-redraw | 64.94 (63.95–71.59) | -10.1% | -12.1% | -11.7% | -9.2% |
| bounded-scrollback | 1140.49 (1133.98–1212.63) | -10.0% | -10.7% | -9.1% | -11.3% |
| single-cell-edits | 18.70 (18.48–18.80) | -1.7% | -1.4% | -5.7% | -2.1% |
| status-line-replacements | 6.34 (6.32–6.38) | +0.6% | -1.2% | -5.1% | +1.6% |
| cursor-moves | 14.64 (14.62–14.71) | -0.0% | +0.1% | -0.9% | +0.0% |
| scrolling-output | 24.99 (24.90–25.43) | +1.1% | +2.5% | +2.8% | +1.1% |
| tmux-like-structural-edits | 51.34 (51.23–51.71) | +0.2% | +1.0% | -0.0% | +0.0% |
| zellij-like-layered-redraws | 19.13 (19.02–19.20) | -0.3% | +0.3% | -0.5% | -0.2% |
| split-scene-panes | 193.16 (192.97–193.43) | -2.5% | +15.5% | -2.4% | -1.8% |
| split-scene-overlays | 203.02 (202.42–203.36) | -2.2% | +14.3% | -2.8% | +0.3% |
| tmux-control-pane-output | 400.55 (399.68–405.65) | -3.9% | +2.8% | -4.5% | -0.6% |
| direct-compositor-pipeline | 102.66 (102.32–103.57) | -4.5% | -5.2% | -5.5% | -4.4% |
| direct-drain-coalescing | 316.00 (315.12–317.34) | -6.7% | -7.5% | -7.1% | -5.8% |
| scrolling-lf-receipts | 74.78 (74.60–75.01) | -7.1% | -8.8% | -7.4% | -6.0% |
| event-boundary-coalescing | 590.68 (589.08–592.36) | -5.5% | +42.3% | -5.9% | -2.8% |
| backpressure-recovery | 73.99 (73.69–74.39) | -5.5% | +42.1% | -5.2% | -5.3% |
| kitty-media-recomposition | 6.52 (6.52–6.57) | -4.4% | -3.8% | -4.6% | -3.9% |

## Original candidate validation

All four experimental release builds and all 20 full benchmark gates passed before the product change. During the initial Thin LTO candidate validation, `cargo build -r --locked` passed using the thin target directory with no LTO environment override. The full workspace/all-target/all-feature host suite passed 1,461 tests. Release-mode library/App/live-PTY/presentation tests also passed (1,001 tests), including safe Lua unwind restoration and silent project fixtures. The final manifest-built benchmark passed the unchanged full baseline gate (`final-thin-gate.json`). The unchanged Docker harness passed private-terminfo 1/1 and real-tmux 11/11 using the verified Linux ARM64 image and shared artifact volumes; no retries or timeout changes. Current-revision CI is required before merge.

## Current-master integration and final decision

Original candidate head `2ee7e853bb81a3015d61f05df27afc855976601c` on `8f317bcf0d3f837a1d8268dbeedd478315168788` was independently reviewed and passed all native/static CI. A clean, patch-equivalent rebase produced `f9aee682b444886b1ebe87b402fca86d74994397` on `d4c1b1f3ab65207db37e177c0b36c91dd4425e55`. Intervening changes share terminal identity types/remove an obsolete feature wrapper (#70), encapsulate the bounded accessibility journal (#72), and reduce compositor row mutation (#74). The last change affects measured hot paths, so the original performance result was not assumed to transfer.

Both profiles were rebuilt on this base in their isolated target directories. Thin used the temporary candidate manifest; default used `CARGO_PROFILE_RELEASE_LTO=false`, restoring the original Cargo release setting. Both retained the same Rust/Ghostty inputs. Full host tests passed 1,466 cases and the then-candidate release library/App/live-PTY/presentation suite passed 1,005. These checks did not resolve runtime timing behavior.

A bounded quiet-window follow-up ran default1, thin1, thin2, then a separately authorized default2 continuation. Default1 and thin1 passed the unchanged full baseline; thin2 failed, emitted stderr, and emitted **no JSON report**. Default2 passed. Successful raw reports are `rebased-default-1.json`, `rebased-thin-1.json`, and `rebased-default-2.json`; exact failure is `rebased-thin-2-failure.txt`. No further timing retries were performed.

The failed Thin run reported:

- Direct pipeline throughput 0.460 MiB/s, below 0.500.
- Drain coalescing throughput 0.615 MiB/s, below 0.700, and p95 80,791 ns, above 80,000.
- Scrolling LF receipts p95 180,958 ns, above 150,000.

The successful Thin sample retained split-scene/scheduler gains (panes 137 ms versus default 151–159 ms; overlays 150 versus 160–165; scheduler 570 versus 592–608), but shell workload time was 174 ms versus 161–166 ms and drain timing overlapped. This is mixed evidence, not a reproducible current-base improvement across all relevant workloads. No scheduling/load cause is inferred from the failure, and passing original-base CI or prior runs does not erase it.

The final PR removes the proposed Cargo setting and preserves defaults. It changes only this experiment's evidence. After removing the candidate setting, final `cargo build -r --locked` passed with no LTO environment override. Product source is identical to the current-master default used for the two successful current-base gates; host tests are unchanged. No new Docker run is required for this evidence-only result: current-master compatibility was validated with the merged changes, and this final revision changes no product code, dependencies, profile, workload, or test behavior. Original candidate Docker results above remain historical evidence, not a newly executed current-base check.
