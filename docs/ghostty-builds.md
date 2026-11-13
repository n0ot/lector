# Building with libghostty-vt

Ordinary Cargo builds download, verify, and cache Lector's pinned terminal
engine. The local `lector-ghostty` adapter links the official C API statically;
installed Lector binaries need no Ghostty runtime installation. See the
[adapter and composition model](architecture.md#ghostty-adapter-and-composition)
for runtime ownership, snapshots, and rendering contracts.

## Fresh-clone developer build

Install a stable Rust toolchain plus `curl`, `tar`, and either `shasum` or
`sha256sum`. Linux builders also need a C compiler and the target libc's static
development archives (`build-essential` plus `libc6-dev` on Debian-family
glibc systems, or `build-base` on Alpine). Then a complete release build is one
command:

```sh
cargo build --locked --release
```

To build both Rust and the Ghostty core in debug mode for engine debugging:

```sh
LECTOR_GHOSTTY_OPTIMIZE=Debug cargo build --locked
```

On a cache miss, Cargo downloads the official Zig 0.17.0 binary for the host,
verifies its published SHA-256 checksum, and caches it under
`target/toolchains/zig/0.17.0`. It then downloads and verifies the pinned
official Ghostty source and builds the required static archive. Everything
under `target` is ignored. Subsequent Rust development and release builds both
select `ReleaseFast`, validate the archive checksum and build metadata, and
reuse that one native artifact without entering Zig. The separate `Debug`
archive is created and reused only when `LECTOR_GHOSTTY_OPTIMIZE=Debug` is set.

Zig's internal local and global caches live under
`target/ghostty-zig-cache/<zig-version>/`. Compiler upgrades select a separate
cache namespace; old caches remain untouched. These compiler caches are
distinct from the checksum-verified native archives in `ghostty-prebuilt`.

`cargo ghostty-check` bootstraps and verifies both native profiles, runs the
linked build-information tests, and checks the resulting runtime linkage.

The repository scripts delegate to the implementation shipped inside
`crates/lector-ghostty/bootstrap`, so the published adapter crate builds without
repository-only files. Both entry points return immediately for a verified
cache hit. On a cache miss they download checksum-pinned Zig, Ghostty source,
and Ghostty dependency archives; Zig builds with those offline dependencies.
The repository wrappers explicitly default `LECTOR_GHOSTTY_BUILD_ROOT` to the
workspace's `target` directory. Direct standalone scripts default to the crate's
`target`; standalone Cargo builds use a bootstrap directory under `OUT_DIR`.
`LECTOR_GHOSTTY_BUILD_ROOT` overrides these defaults, while
`LECTOR_TOOLCHAIN_ROOT` and `GHOSTTY_PREBUILT_ROOT` independently override the
Zig installation and native archive roots. Cargo consumes the verified output
under:

```text
target/ghostty-prebuilt/<rust-target>/<Debug|ReleaseFast>/
```

Maintainers can invoke the lower-level bootstrap directly. It accepts a
matching Zig 0.17.0 on `PATH` and otherwise obtains the pinned toolchain:

```sh
scripts/bootstrap_ghostty.sh --optimize ReleaseFast
cargo build --locked --release
```

On macOS, Zig 0.17.0 uses the macOS 26 SDK reported by `xcrun --sdk macosx`.

## Offline, packaging, and release builds

An offline builder must populate the verified prebuilt directory before
running Cargo. It can run the bootstrap during a network-enabled fetch/build
phase, preserve `target/ghostty-prebuilt`, and use that directory during the
offline Cargo phase. Set `GHOSTTY_PREBUILT_ROOT` to an immutable alternative
root when package policy does not permit build inputs under `target`:

```sh
GHOSTTY_PREBUILT_ROOT=/opt/lector/ghostty-prebuilt \
  cargo build --locked --release
```

Each profile directory must contain `static-lib/libghostty-vt.a` and the
`lector-ghostty-build.txt` metadata written by the bootstrap. The adapter build
script rejects or rebuilds missing and mismatched commit, Zig, target,
Ghostty app-version, optimization, headless-runtime, Kitty-graphics, C-header
ABI, ABI-probe hash, or archive checksum state. Copying an unverified system
library into place is unsupported. The explicit Ghostty app-version also keeps
the enclosing Lector repository's branch or release tag from affecting the
vendored source build. The dedicated `static-lib` directory prevents an
identically named shared library emitted by Ghostty's install step from being
selected by a platform linker.

Release builders use the same pinned source and exact Zig version. Released
Lector binaries do not require Zig, the Ghostty application, or a shared
`libghostty-vt` at runtime. All Linux targets enable Rust's `crt-static` target
feature for the final Lector crate. Musl targets use Zig's bundled `ld.lld`
directly for the Tier 2 cross-target builds so the final link is a static PIE
and the AArch64 build retains the Cortex-A53 erratum mitigation. The native
Alpine build uses its packaged musl linker. Glibc targets use a small
compiler-driver wrapper to select glibc's working `-static-pie` mode without
rustc's conflicting `-static -no-pie` combination. The resulting ELF is
position-independent and has neither a dynamic interpreter nor `DT_NEEDED`
entries. Libc is resolved by the final Rust link; it is not copied
into `libghostty-vt.a`. macOS remains dynamically linked to Apple's system
libraries, which are not available as supported static archives.

A glibc ELF with no declared shared dependencies may still ask glibc to load
matching NSS modules when account or network-name lookup falls back to those
services. Optional facilities such as Wayland clipboard integration and Lua C
modules can also load their own platform modules explicitly. Use a musl target
for the most hermetic distributable executable; these runtime integrations are
not link-time dependencies of Lector itself.

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
- `aarch64-alpine-linux-musl`
- `x86_64-alpine-linux-musl`

The `*-alpine-linux-musl` names are used by Alpine's packaged Rust toolchains
and map to the same Zig musl targets as their `*-unknown-linux-musl`
counterparts. Other targets fail early with the supported list. Run
`cargo ghostty-check` for the native debug/release ABI and static-link checks.
On Linux, that check also rejects a release artifact that is not PIE or has an
ELF interpreter or `DT_NEEDED` entry. After both verified
profiles exist, `scripts/check_build_paths.sh` confirms that ordinary Cargo
commands reuse them without bootstrapping or network access.

## Pinned inputs

- Lector adapter: `lector-ghostty = 0.1.0`, from this repository.
- libghostty-vt: `1.0.0`, tag `vt-1.0.0`, source commit
  `0fa3d577a5614150ff8e2c152de5fb9d5c6d2dec`.
- Official source archive SHA-256:
  `4a6ef758be721923e1969e7b81b443dcc92d8dd3fb5a39600c03a8817a753676`.
- Matching dependency archive SHA-256:
  `4d5ea5e448e4c7ea8fb55ccbaa8668f2a97d44f438cee831e2b7ecd162d85c29`.
- Zig: exactly `0.17.0` for building the archive.

Lector targets the stable libghostty-vt C ABI `>=1.0.0, <2.0.0`, whose
version is independent of the Ghostty application. Bundled builds remain
pinned to the exact tested release and checksums above. Future 1.x upgrades
require a tested PR; builds never resolve a moving latest version.

Source and dependency archives come from
`https://release.files.ghostty.org/libghostty-vt/1.0.0/`. Both checksums are
verified before extraction. The source's `LIB_VERSION` selects the standalone
library build, and `--system` supplies all dependencies without Zig fetching
packages. Lector links `libghostty-vt.a` statically and never falls back to an
arbitrary system library. Cached or externally supplied archives must match
the exact release, target, optimization, ABI probe, and archive checksum;
unsupported major versions are not accepted.

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
- If macOS reports that `rust-objcopy` cannot load `libLLVM.dylib`, install
  the matching official component with `rustup component add llvm-tools`
  from the checkout, then retry. This Rust toolchain issue is separate from
  Ghostty compilation; do not override dynamic-library search paths.
- A metadata mismatch means the cached archive was built for another commit,
  Zig version, Rust target, or optimization profile. The next ordinary Cargo
  build regenerates it; do not copy in a system library.
- A checksum failure is fatal by design. Remove only the named cached download
  under `target/toolchains` or `target/ghostty-source`, then retry on a
  trusted network. Do not bypass verification.
- Cross-target builds prepare and cache an archive for the exact Rust target.
- Released binaries should show no `libghostty-vt` dependency in `otool -L` or
  `ldd`. `cargo ghostty-check` verifies this automatically.

### CI Cargo compilation cache

CI places Cargo outputs in `target/cargo`, separate from verified Ghostty/Zig
archives. A commit-pinned Rust cache action retains dependencies and registry
state while pruning workspace binaries and incremental artifacts. Restore/save
failures are nonfatal; all build, link, inspection and test commands still run.

Keys partition actual compiler metadata, runner OS/architecture, Rust target,
lane profiles/features, lockfile, Cargo configuration and build/linker inputs.
The input fingerprint is part of the fallback prefix too, so changed lockfiles
or configurations cannot restore a previous compilation archive. Alpine uses
its own packaged compiler inside its disposable container, with a separate
Cargo home and identity; it never keys against the runner's rustup compiler.

### Container preparation and CI timing

The Docker integration cache additionally uses the exact container image and
compiler identity. Its source fingerprint includes Cargo manifests/lockfile and
configuration, build scripts, the Ghostty adapter, Docker inputs, and the
container runner's bootstrap and compiler/linker helpers. An unrelated developer
or test-runner script does not invalidate these artifacts. Add new transitive build helpers to
`BUILD_SCRIPTS` in `.github/scripts/tmux_cache_key.py` when introducing them.
Cargo still checks changed application/test sources, and restored native
artifacts are verified before use. Cache misses and failed restores retain the
existing bounded rebuild fallback and full integration test enumeration.
