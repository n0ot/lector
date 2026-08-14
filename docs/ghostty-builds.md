# Building with libghostty-vt

Ghostty is Lector's mandatory and sole terminal engine. Every PTY byte is
parsed by `GhosttyEngine`, and its normalized cells, cursor, history, modes,
semantic anchors, and effects drive accessibility. Lector no longer depends on
the `vt100` crate or a second production grid parser. The original PTY bytes
remain authoritative for physical presentation until Phase 2's compositor is
enabled.

Once a verified Ghostty archive has been prepared, ordinary builds are
network-free and require neither Zig nor an installed Ghostty application:

```sh
cargo build --locked
cargo build --locked --release
cargo build --locked --no-default-features # compatibility feature off; engine is unchanged
```

Lector does not depend on a third-party Rust wrapper. The local
`crates/lector-ghostty` crate is a deliberately small safe boundary around the
official Ghostty C API. Its raw declarations are private and its C ABI layout is
checked against Ghostty's installed headers during the explicit bootstrap.

## Authoritative engine

The `ghostty-vt` Cargo feature remains as an empty, default compatibility flag
for existing project commands and benchmark target selection. It does not
toggle the dependency or runtime engine: `lector-ghostty` is mandatory with or
without default features. The former `LECTOR_GHOSTTY_SHADOW*` variables and
dual-feeding path have been removed.

CI runs the complete authoritative suite, recording corpus, fragmentation
tests, accessibility harnesses, and debug/release static-link checks. Historical
differential recordings remain versioned because they are valuable regression
inputs; their old classification fields are documentation, not runtime policy.

The adapter normalizes Ghostty cells into Lector's grapheme, width, style,
hyperlink, wrapping, and history model. Lector still exposes at most 10,000
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
explicit-scroll operation counts, normalized changed-row ranges, cursor and
screen identity before and after the write, and synchronized-output mode.
Ghostty exposes the final cursor, screen, mode, and render state but does not
currently expose printable or cursor-operation callbacks. The owned adapter's
narrow `vte` stream observer therefore collects only those operation facts,
OSC 133 boundaries, and the active OSC 8 URI needed to restore today's raw
presentation after an overlay. It does not maintain cells, a cursor, modes,
history, or any second terminal grid. Changed rows come directly from the
before/after Ghostty render state.

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

Raw-output mode still sends the application's original bytes to the physical
terminal, which remains responsible for answering terminal queries. Replies
produced by Ghostty are retained as pane-scoped reply bytes but are not written
to the child PTY until Phase 2's protocol broker owns that route. Ghostty's
current public API intentionally ignores OSC 52 clipboard-read requests; Lector characterizes
that behavior while preserving the original request for the physical terminal.

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
stable persistence format: Ghostty snapshot format version 1 remains a
work-in-progress, and Lector's auxiliary historical OSC 133 anchors are not
yet serialized by this diagnostic path.

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
cargo ghostty-release
```

For a debug build:

```sh
cargo ghostty-debug
```

These targets download the official Zig 0.16.0 binary for the host, verify its
published SHA-256 checksum, and cache it under
`target/toolchains/zig/0.16.0`. They then download and verify the pinned
official Ghostty source, build the required static archive, and invoke Cargo.
Everything under `target` is ignored, so subsequent invocations validate and
reuse the cached toolchain, source, Zig packages, and build outputs.

`cargo ghostty-check` bootstraps and verifies both native profiles, runs the
linked build-information tests, and checks the resulting runtime linkage.

The aliases are defined by this repository in `.cargo/config.toml`; they compile
and run the dependency-free `lector-xtask` workspace utility, so no global Cargo
plugin or Make installation is required. The bootstrap remains intentionally
separate from Cargo build scripts. On its first run,
`scripts/bootstrap_zig.sh` downloads Zig, and
`scripts/bootstrap_ghostty.sh` may download the pinned official source archive
while Zig fetches packages pinned by Ghostty's `build.zig.zon`. Neither
Lector's root `build.rs` nor the adapter's `build.rs` executes Zig, Git, curl,
or any network operation. Cargo consumes only the verified output under:

```text
target/ghostty-prebuilt/<rust-target>/<Debug|ReleaseFast>/
```

If Zig 0.16.0 is already managed externally, the lower-level Ghostty bootstrap
still accepts it on `PATH`:

```sh
scripts/bootstrap_ghostty.sh --optimize ReleaseFast
cargo build --locked --release --features ghostty-vt
```

On macOS, Zig uses the SDK reported by `xcrun --sdk macosx`. Zig 0.16.0 can
consume the current macOS 26 SDK, so Lector no longer carries the SDK-selection
shim that the older toolchain required.

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
script rejects missing or mismatched commit, Zig, target, optimization,
headless-runtime, Kitty-graphics, or C-header ABI metadata. Copying an
unverified system library into place is unsupported. The dedicated
`static-lib` directory prevents an identically named shared library emitted by
Ghostty's install step from being selected by a platform linker.

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

- “verified archive not found” means an ordinary Cargo command ran before the
  matching profile was bootstrapped. Run `cargo ghostty-debug` or
  `cargo ghostty-release` from the repository root.
- A metadata mismatch means the cached archive was built for another commit,
  Zig version, Rust target, or optimization profile. Re-run the corresponding
  project alias; do not copy in a system library.
- A checksum failure is fatal by design. Remove only the named cached download
  under `target/toolchains` or `target/ghostty-source`, then retry on a
  trusted network. Do not bypass verification.
- Cross-target builds need a prebuilt archive for that exact Rust target. Use
  `cargo ghostty-bootstrap --target <triple> --optimize <profile>` first.
- Released binaries should show no `libghostty-vt` dependency in `otool -L` or
  `ldd`. `cargo ghostty-check` verifies this automatically.
