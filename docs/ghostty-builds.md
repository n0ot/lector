# Building with libghostty-vt

Ghostty is Lector's mandatory and sole terminal engine. Every PTY byte is
parsed by `GhosttyEngine`, and its normalized cells, cursor, history, modes,
semantic anchors, and effects drive accessibility. Lector has no `vt100`
dependency or second production grid parser. The mandatory compositor renders
from Ghostty state and never replays source application bytes.

Use ordinary Cargo commands. On a cache miss, the adapter build automatically
downloads and verifies the pinned build inputs and prepares the native archive:

```sh
cargo build --locked
cargo build --locked --release
cargo build --locked --no-default-features # compatibility feature off; engine is unchanged
```

Ordinary development and test builds link the `ReleaseFast` Ghostty archive
even when Rust itself uses its debuggable development profile. Terminal
parsing is on the interactive hot path, and Ghostty's Zig `Debug` mode is too
slow to remain responsive during sustained output such as `yes`. Set
`LECTOR_GHOSTTY_OPTIMIZE=Debug` only when debugging the Ghostty core itself.

Lector does not depend on a third-party Rust wrapper. The local
`crates/lector-ghostty` crate is a deliberately small safe boundary around the
official Ghostty C API. Its raw declarations are private and its C ABI layout is
checked against Ghostty's installed headers during the automatic bootstrap.

## Authoritative engine

The `ghostty-vt` Cargo feature is an empty, default flag accepted by project
commands and benchmark target selection. It does not toggle the dependency or
runtime engine: `lector-ghostty` is mandatory with or without default features.

CI runs the complete authoritative suite, recording corpus, fragmentation
tests, accessibility harnesses, and debug/release static-link checks. Checked-in
differential recordings are regression inputs; their classification fields are
documentation, not runtime policy.

The adapter normalizes Ghostty cells into Lector's grapheme, width, style,
hyperlink, wrapping, and history model. Lector exposes at most 10,000
scrollback rows. Ghostty's allocator prunes complete pages and therefore may
physically retain additional rows; the adapter disables the independent byte
cap, configures 20,000 lines of physical headroom, and clips its public model
to the newest 10,000 logical rows. Sparse review marks use Ghostty tracked grid references and become
invalid when their cell falls outside that logical window, even if an older
physical page has not yet been reclaimed.

Ghostty's public cell and row semantic APIs distinguish prompt, input, and
output content, but they do not expose every historical OSC 133 A/B/C/D
boundary or the exit status carried by `D`. The owned adapter therefore keeps
a narrow fragmented-stream OSC 133 observer alongside Ghostty and anchors each
completed boundary to Ghostty with a tracked grid reference. This preserves
exact command phases and exit status across scrolling and reflow without
treating Ghostty's current `cursor_at_prompt` boolean as semantic history.

Auto-read consumes an engine-neutral `UpdateSummary` produced by the same
advance call that mutates terminal state. It records printable runs, cursor and
explicit-scroll operation counts, Ghostty's official full/partial render
damage, normalized changed-row ranges, cursor and screen identity before and
after the write, synchronized-output mode, and whether the batch crossed an
actual false-to-true DEC private mode 2026 boundary. At that boundary the
adapter splits processing at the marker and captures a full screen-and-history
snapshot. Clearing the marker's mode bit yields the exact committed model from
immediately before the frame: preceding bytes in the same input slice are
included and following bytes are excluded, even when the escape sequence was
fragmented across reads.

Ghostty exposes the final cursor, screen, mode, and render state but does not
expose printable or cursor-operation callbacks. The owned adapter's
narrow `vte` stream observer therefore collects operation hints, OSC 133
boundaries, and the active OSC 8 URI needed to annotate semantic operation
hints. For semantic rendering it speculatively tracks only the
cursor, scroll margins, origin, autowrap, and horizontal-margin mode required
to locate an operation. It owns no cells, history, or second terminal grid,
and discards all hints unless its final cursor agrees with Ghostty's
authoritative state. Changed rows come from Ghostty's stateful global and
per-row dirty flags; the adapter acknowledges both layers after each snapshot
so damage cannot leak into a later frame.

The compositor maps pane-local dirty rows into clipped scene
coordinates and compares only those candidate cells with its confirmed
`PresentedScene`. Incremental runs use absolute cursor placement and
self-contained style and hyperlink state. Failed or partial physical writes,
resize, suspend/resume, changed image state, inconsistent shadows, and damaged
wrap metadata all invalidate the optimization and use the full-scene,
oracle-tested fallback. Text damage around unchanged images can still use the
incremental path without retransmitting image data.

When operation hints are consistent with the authoritative scene, the
compositor can apply full or partial scrolling, line/character insertion and
deletion, erasure, and adjacent ASCII write runs directly to both the physical
terminal and a cloned shadow. It accepts that shadow only after affected rows
match Ghostty's intended scene and the complete transaction is flushed. Any
occlusion, clipping, media changes, wide-cell ambiguity, geometry mismatch,
unsupported operation, or failed validation falls back to the same
dirty-region or full-scene correctness path. The release benchmark checks
semantic-path coverage and compares bytes and latency with a pure dirty-region
renderer on both a tmux-style structural workload and a Zellij-style layered
redraw workload. Both retain an independently constructed full-redraw result as
the correctness reference; the checked-in baseline gates semantic coverage,
bytes, latency, scheduler bounds, peak RSS, and media throughput.

OSC 8 hyperlinks remain part of the modeled cell state and are closed at every
physical transaction boundary. Ghostty-decoded Kitty graphics live in bounded
pane-scoped stores: uploads and placements have separate lifetimes, unchanged
pixel buffers are shared, and scene composition maps pane-local identifiers to
collision-safe outer identifiers. Placements are clipped to pane and overlay
geometry, including partial occlusion; hidden uploads can remain cached while
their placements are removed and later restored without retransmission. The
outer backend chunks uploads, deletes stale placement and upload identifiers,
and conservatively rebuilds its cache after physical-state uncertainty. A
terminal without advertised Kitty support receives the complete text scene and
no graphics protocol. The default limits are 32 MiB per image, 64 MiB per
pane, 128 MiB per scene, and 4,096 placements.

After its bounded initial focus-mode ownership query, the live event loop
routes physical presentation and capability-control bytes through a single
`OutputScheduler`. Standard output is nonblocking while the loop runs and is
registered for writable readiness only after `EAGAIN`; the original descriptor
flags are restored before terminal lifecycle cleanup. Scheduler writes retry
`EINTR`, resume partial transactions without interleaving, and confirm a
predicted `PresentedScene` only after its bytes flush successfully. Fatal
writer errors invalidate renderer state for an authoritative reconstruction.

Physical input and child PTY output are also nonblocking. Each drain turn is
limited to 32 KiB or 4 ms, with the time and byte limits checked between
reads of at most 8 KiB. Hitting a limit schedules an immediate continuation so an
edge-triggered readiness notification is not lost, while returning control to
input, presentation, and other ready sources. An open DEC 2026 frame receives
no larger PTY budget.

Modeled visual updates have a 4 ms event-boundary latency budget and a 64 KiB
per-drain write budget. An unstarted render can be replaced by the newest
authoritative scene, while started transactions, lifecycle/control bytes, and
bells are retained. This bounds obsolete incremental visual work
without delaying terminal replies or input sent to the application PTY.
Application mode 2026 is pane-local batching intent: Lector owns at most one
physical synchronized-output wrapper and strips nested renderer wrappers. Each
render carries stable view identities, model revisions, visible snapshots, and
shared changed-history generations. Replacement drops bytes and accessibility
state together; started renders retain both through their successful flush.
Raw application input and terminal replies are not held behind presentation.
This does not eliminate the normal coordinate time-of-check to time-of-use race
between reading a cell and sending input to an application that remains live.

A real close makes its render eligible but publishes nothing to accessibility
until that exact render flushes. Activity refreshes a 100 ms idle timeout, while
a 2-second hard cap bounds a continuously updating transaction. On timeout the
scheduler may release a partial render and ignores further holds in the epoch
until its real close. The old frame remains readable during backpressure; after
flush, the exact released generation becomes readable, never newer parser
state. Lector-owned overlays and resizes remain responsive, and their active
accessibility owner changes at the same completed scene boundary. Bells are
emitted only after the visual transaction and its global synchronization
boundary. Title,
working-directory, progress, clipboard, and notification events remain typed
scheduler work until activation; output policy is applied at that boundary
instead of retaining raw escape strings. Pending effect payloads are bounded
and UTF-8-safe, zero-byte secure-policy effects complete normally, and
backpressured work counts against the same retention cap.

The modeled compositor keeps the root terminal and every stacked Lector view as
separate, stable-z scene surfaces. Source engines continue parsing while an
overlay is visible; hidden source damage can therefore be confirmed as a valid
zero-byte physical update when the overlay fully occludes it. Popping a layer
recomposes from current authoritative snapshots instead of replaying buffered
PTY data. Message, Review, Lua REPL, table-setup, and reviewable popup layers
retain their existing input rules, while frozen Review and table-setup layers
own independent cursors. Announcement and error popups produce a dismissal;
confirmation popups distinguish `Enter` acceptance from `Escape` cancellation.

Terminal side effects use the same pane boundary. Ghostty callbacks copy bell,
title, working-directory, clipboard-write, desktop-notification, progress,
query, and bounded unknown-sequence data into owned typed events before the
callback returns. The callback userdata is boxed at a stable address, accessed
only during synchronous terminal writes, drained after those writes return,
and dropped after the Ghostty terminal handle. Unsupported APC payloads are
limited to 256 bytes.

Focus reporting, mouse protocol and encoding, bracketed paste, and Kitty
keyboard flags are normalized into the terminal snapshot. Lector's existing
focus, paste, mouse, and keyboard forwarding reads that pane-local state. Title
and working-directory values are retained exactly as applications report them,
including empty clears and raw `file://` URIs; they are not interpreted as SSH
destinations.

Lector virtualizes application-facing terminal identity. Application
capability queries are never placed in physical-terminal output, are answered
from the owning pane's stable Lector profile, and routed back only to that
pane's PTY. A narrow observer fills
the one current public-C-API gap for OSC 52 clipboard reads and generates the
secure empty local reply; reads never reach the physical terminal.

The child receives `TERM=xterm-256color`, and Lector removes inherited
`TERMINFO` so a nested instance cannot accidentally pair that public name with
its parent's private database. Lector does not inherit an outer vendor identity
or advertise `xterm-ghostty`; the Ghostty engine supplies parsing and terminal
state, not Ghostty's complete application-facing protocol contract. Lector
derives a separate physical profile from conservative defaults, outer
terminfo, bounded startup probes, and explicit overrides. DA1 is the final
physical query, and probe replies are consumed through its processing fence
before input parsing. Clipboard writes enter Lector's local clipboard
history; desktop notifications and unknown APC effects are dropped; title,
working directory, progress, hyperlink, and bell state remain modeled. This
separation keeps application promises independent of the outer terminal's
identity and capabilities.

Resize handling reads the outer terminal's complete Unix `winsize`, derives
per-cell pixel dimensions from its cell and grid-pixel fields, and routes the
same geometry to both the child PTY and the pane model. The PTY receives rows,
columns, and total grid pixels before the child is notified with `SIGWINCH`;
Ghostty receives rows, columns, and per-cell pixels. Cell-only callers remain
supported and explicitly carry zero pixel dimensions. Ghostty's primary-screen
reflow and alternate-screen crop behavior are authoritative.

The adapter enables a bounded 64 KiB replay continuation and exposes an
explicitly diagnostic Ghostty snapshot round-trip. Automated tests encode and
restore visible state, scrollback, geometry, modes, styles, and unfinished
UTF-8 and CSI input. This is not used by live Lector behavior or presented as a
stable persistence format: Ghostty snapshot format version 1 is unstable, and
Lector's auxiliary OSC 133 history anchors are not serialized by this
diagnostic path.

## Pinned inputs

- Lector adapter: `lector-ghostty = 0.1.0`, from this repository.
- Ghostty source commit: `43fe699071c7dceb161dc3b0c04fce46ade36174`
  from the upstream `main` branch on 2026-08-13. Libghostty does not yet have
  independent tagged releases, so Lector pins a reviewed commit rather than a
  floating branch.
- Official source archive SHA-256:
  `fbff942fc10b4d0a9de146e805922ef2b763226813fc449fdbb22c9ac7dd0f4a`.
- Zig: exactly `0.16.0` for building the archive.

The source archive comes from Ghostty's official GitHub repository. The
bootstrap verifies its checksum before extraction and builds with
`emit-lib-vt=true`, `emit-xcframework=false`, and `app-runtime=none`. Lector
links `libghostty-vt.a` statically and never falls back to an arbitrary system
library.

## Fresh-clone developer build

Install a stable Rust toolchain plus `curl`, `tar`, and either `shasum` or
`sha256sum`. Then a complete release build is one command:

```sh
cargo build --locked --release
```

To build both Rust and the Ghostty core in debug mode for engine debugging:

```sh
LECTOR_GHOSTTY_OPTIMIZE=Debug cargo build --locked
```

On a cache miss, Cargo downloads the official Zig 0.16.0 binary for the host,
verifies its published SHA-256 checksum, and caches it under
`target/toolchains/zig/0.16.0`. It then downloads and verifies the pinned
official Ghostty source and builds the required static archive. Everything
under `target` is ignored. Subsequent Rust development and release builds both
select `ReleaseFast`, validate the archive checksum and build metadata, and
reuse that one native artifact without entering Zig. The separate `Debug`
archive is created and reused only when `LECTOR_GHOSTTY_OPTIMIZE=Debug` is set.

`cargo ghostty-check` bootstraps and verifies both native profiles, runs the
linked build-information tests, and checks the resulting runtime linkage.

The adapter's build script runs `scripts/bootstrap_ghostty.sh`; that script
returns immediately for a verified cache hit. On its first run,
`scripts/bootstrap_zig.sh` downloads Zig, and the Ghostty bootstrap downloads
the pinned official source archive while Zig fetches packages pinned by
Ghostty's `build.zig.zon`. Cargo consumes the verified output under:

```text
target/ghostty-prebuilt/<rust-target>/<Debug|ReleaseFast>/
```

Maintainers can invoke the lower-level bootstrap directly. It accepts a
matching Zig 0.16.0 on `PATH` and otherwise obtains the pinned toolchain:

```sh
scripts/bootstrap_ghostty.sh --optimize ReleaseFast
cargo build --locked --release --features ghostty-vt
```

On macOS, Zig 0.16.0 uses the macOS 26 SDK reported by `xcrun --sdk macosx`.

## Offline, packaging, and release builds

An offline builder must populate the verified prebuilt directory before
running Cargo. It can run the bootstrap during a network-enabled fetch/build
phase, preserve `target/ghostty-prebuilt`, and use that directory during the
offline Cargo phase. Set `GHOSTTY_PREBUILT_ROOT` to an immutable alternative
root when package policy does not permit build inputs under `target`:

```sh
GHOSTTY_PREBUILT_ROOT=/opt/lector/ghostty-prebuilt \
  cargo build --locked --release --features ghostty-vt
```

Each profile directory must contain `static-lib/libghostty-vt.a` and the
`lector-ghostty-build.txt` metadata written by the bootstrap. The adapter build
script rejects or rebuilds missing and mismatched commit, Zig, target,
optimization, headless-runtime, Kitty-graphics, C-header ABI, ABI-probe hash,
or archive checksum state. Copying an unverified system library into place is
unsupported. The dedicated `static-lib` directory prevents an identically
named shared library emitted by Ghostty's install step from being selected by
a platform linker.

Release builders use the same pinned source and exact Zig version. Released
Lector binaries do not require Zig, the Ghostty application, or a shared
`libghostty-vt` at runtime. Platform runtime libraries supplied by the OS may
still be dynamically linked normally.

## Supported targets

Tier 1 builds are required on:

- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `aarch64-unknown-linux-gnu`
- `x86_64-unknown-linux-gnu`

The build mapping also recognizes these Tier 2 candidates, but support is not
claimed until their compile/package checks pass:

- `aarch64-unknown-linux-musl`
- `x86_64-unknown-linux-musl`

Other targets fail early with the supported list. Run `cargo ghostty-check` for
the native debug/release ABI and static-link checks. After both verified
profiles exist, `scripts/check_build_paths.sh` confirms that ordinary Cargo
commands reuse them without bootstrapping or network access.

## Upgrading Ghostty or Zig

Treat an upgrade as one reviewed compatibility change; never float either
input independently.

1. Update the Ghostty commit, official archive checksum, and required Zig
   version/checksums in `build_support/ghostty.rs` and the bootstrap scripts.
2. Regenerate both verified profiles with `cargo ghostty-check`. The C probe
   must compile against the new official headers before Rust is linked.
3. Run formatting, Clippy, the complete suite, the authoritative recording and
   harness corpus, ASAN wrapper tests, and `cargo ghostty-bench`.
4. Review any C API or behavior change and extend the owned adapter and
   fixtures explicitly. Do not add a third-party wrapper or a fallback parser.
5. Update this document's pins and checked-in benchmark report in the same
   commit.

## Troubleshooting

- A bootstrap failure on a fresh build reports the failed download, checksum,
  toolchain, or Zig build directly. Re-run the same ordinary Cargo command
  after correcting that underlying problem.
- A metadata mismatch means the cached archive was built for another commit,
  Zig version, Rust target, or optimization profile. The next ordinary Cargo
  build regenerates it; do not copy in a system library.
- A checksum failure is fatal by design. Remove only the named cached download
  under `target/toolchains` or `target/ghostty-source`, then retry on a
  trusted network. Do not bypass verification.
- Cross-target builds prepare and cache an archive for the exact Rust target.
- Released binaries should show no `libghostty-vt` dependency in `otool -L` or
  `ldd`. `cargo ghostty-check` verifies this automatically.
