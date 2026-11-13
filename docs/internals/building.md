# Building

Lector's terminal engine is `libghostty-vt`, the terminal core of Ghostty. An
ordinary Cargo build downloads it, verifies it, builds it, and links it
statically. You do not install Ghostty or Zig yourself, and the finished
`lector` binary needs neither at run time.

- [Build from a fresh clone](#build-from-a-fresh-clone)
- [Supported targets](#supported-targets)
- [What the build downloads](#what-the-build-downloads)
- [Offline and packaged builds](#offline-and-packaged-builds)
- [Static linking](#static-linking)
- [Troubleshooting](#troubleshooting)
- [Upgrading Ghostty or Zig](#upgrading-ghostty-or-zig)
- [CI caches](#ci-caches)

## Build from a fresh clone

You need:

- A stable Rust toolchain. With rustup, `rust-toolchain.toml` selects the
  tested version automatically.
- `curl`, `tar`, and either `shasum` or `sha256sum`.
- On Linux, a C compiler and the static libc development files:
  `build-essential` and `libc6-dev` on Debian-family systems, or `build-base`
  on Alpine.
- On macOS, the Xcode command line tools. Zig uses the macOS SDK that
  `xcrun --sdk macosx` reports.

Then:

```sh
cargo build --locked --release
```

To install the result, see the [README](../../README.md#install).

To debug the terminal engine itself, build it unoptimized:

```sh
LECTOR_GHOSTTY_OPTIMIZE=Debug cargo build --locked
```

Otherwise both debug and release Rust builds share one optimized
(`ReleaseFast`) engine build, so the engine is compiled only once.

## Supported targets

Tier 1, required to build:

- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `aarch64-unknown-linux-gnu`
- `x86_64-unknown-linux-gnu`

CI builds every Tier 1 target on each pull request, except Intel macOS
(`x86_64-apple-darwin`). That target is built after each merge to `master`
and on manual runs. Its runners are the slowest and are being retired, so a
pull request does not wait for it. An Intel-only failure shows up as a red
run on `master`.

Tier 2, recognized by the build but not yet claimed as supported:

- `aarch64-unknown-linux-musl`
- `x86_64-unknown-linux-musl`
- `aarch64-alpine-linux-musl`
- `x86_64-alpine-linux-musl`

The `alpine` names are what Alpine's packaged Rust uses. They build the same
way as the `unknown` musl targets. Any other target fails early with this
list.

## What the build downloads

On the first build, Cargo:

1. Downloads the official Zig 0.17.0 for your machine and verifies its
   checksum.
2. Downloads the pinned Ghostty source and its dependency archive, and
   verifies both checksums.
3. Builds the static library with Zig, using only those verified archives.

Later builds check the cached library's checksum and metadata and reuse it
without running Zig.

Everything lands under `target/`:

| Path | Contents |
| --- | --- |
| `target/toolchains/zig/0.17.0` | The Zig toolchain |
| `target/ghostty-source` | Downloaded source archives |
| `target/ghostty-zig-cache/<zig-version>/` | Zig's own compiler caches |
| `target/ghostty-prebuilt/<rust-target>/<Debug\|ReleaseFast>/` | The verified static library |

### Pinned versions

| Input | Pin |
| --- | --- |
| `lector-ghostty` adapter | `0.1.0`, from this repository |
| libghostty-vt | `1.0.0`, tag `vt-1.0.0`, commit `0fa3d577a5614150ff8e2c152de5fb9d5c6d2dec` |
| Source archive SHA-256 | `4a6ef758be721923e1969e7b81b443dcc92d8dd3fb5a39600c03a8817a753676` |
| Dependency archive SHA-256 | `4d5ea5e448e4c7ea8fb55ccbaa8668f2a97d44f438cee831e2b7ecd162d85c29` |
| Zig | exactly `0.17.0` |

Archives come from `https://release.files.ghostty.org/libghostty-vt/1.0.0/`.

Lector targets the stable libghostty-vt C ABI, `>=1.0.0, <2.0.0`. That
version is independent of the Ghostty application's version. Builds never
pick up a newer release on their own, and never fall back to a system
library.

### Environment variables

| Variable | Effect |
| --- | --- |
| `LECTOR_GHOSTTY_OPTIMIZE` | `Debug` builds an unoptimized engine. |
| `LECTOR_GHOSTTY_BUILD_ROOT` | Where downloads and builds go. Defaults to the workspace `target`. |
| `LECTOR_TOOLCHAIN_ROOT` | Where Zig is installed. |
| `GHOSTTY_PREBUILT_ROOT` | Where the verified static library is read from. |

### Useful commands

- `cargo ghostty-check` builds and verifies both engine profiles, runs the
  build-information tests, and checks the linked result.
- `scripts/check_build_paths.sh` confirms that, once both profiles exist,
  ordinary Cargo commands reuse them without downloading or rebuilding.
- `scripts/bootstrap_ghostty.sh --optimize ReleaseFast` runs the download and
  build step alone. It uses a matching Zig on `PATH` if there is one.

The bootstrap code ships inside `crates/lector-ghostty/bootstrap`, so the
adapter crate builds without the rest of this repository.

## Offline and packaged builds

An offline build needs the verified library in place before Cargo runs:

1. With network access, run the bootstrap (or a normal build).
2. Keep `target/ghostty-prebuilt`.
3. Build offline.

If your packaging rules do not allow build inputs under `target`, point to a
read-only copy:

```sh
GHOSTTY_PREBUILT_ROOT=/opt/lector/ghostty-prebuilt \
  cargo build --locked --release
```

Each profile directory must contain `static-lib/libghostty-vt.a` and the
`lector-ghostty-build.txt` metadata the bootstrap wrote. The build rejects or
rebuilds a library whose commit, Zig version, target, optimization level, ABI
probe, or checksum does not match. Dropping in a system library is not
supported.

## Static linking

- **Linux.** All targets enable Rust's `crt-static`. The result is a static,
  position-independent executable with no dynamic loader and no shared
  library dependencies. `cargo ghostty-check` rejects a release build that is
  not.
  - glibc targets use a small compiler wrapper to get glibc's `-static-pie`
    mode.
  - musl cross targets link with Zig's bundled `ld.lld`. Native Alpine uses
    its packaged musl linker.
- **macOS.** The binary links Apple's system libraries dynamically, because
  Apple does not provide static versions. `libghostty-vt` itself is still
  linked statically.

One caveat for glibc: a fully static glibc program may still load NSS
modules at run time for user or host name lookups. Wayland clipboard support
and Lua C modules can also load platform libraries. For the most
self-contained binary, use a musl target.

## Troubleshooting

**The first build fails.** The error names the step that failed: a download,
a checksum, the toolchain, or the Zig build. Fix that and run the same Cargo
command again.

**Checksum failure.** This is fatal on purpose. Delete only the cached
download it names, under `target/toolchains` or `target/ghostty-source`, and
retry on a network you trust. Do not bypass verification.

**Metadata mismatch.** The cached library was built for a different commit,
Zig version, target, or profile. The next build regenerates it.

**macOS: `rust-objcopy` cannot load `libLLVM.dylib`.** Run
`rustup component add llvm-tools` from the checkout and retry. This is a Rust
toolchain issue, unrelated to Ghostty. Do not change library search paths.

**Checking the result.** A release binary should show no `libghostty-vt` in
`otool -L` or `ldd`. `cargo ghostty-check` verifies this for you.

## Upgrading Ghostty or Zig

Treat an upgrade as one reviewed change. Never bump one input on its own.

1. Update the Ghostty commit, archive checksums, and Zig version and
   checksums in `build_support/ghostty.rs` and the bootstrap scripts.
2. Run `cargo ghostty-check` to rebuild both profiles. The C probe must
   compile against the new headers before Rust links.
3. Run formatting, Clippy, the full test suite, the recording and harness
   corpus, the ASAN wrapper tests, and `cargo ghostty-bench`.
4. Review any C API or behavior change, and extend the adapter and fixtures
   to match. Do not add a third-party wrapper or a fallback parser.
5. Update the pins on this page and the checked-in benchmark report in the
   same commit.

## CI caches

CI keeps Cargo output in `target/cargo`, apart from the verified Ghostty and
Zig archives. Cache failures never fail a build: every build, link, and test
command still runs.

- **Cargo cache.** Keyed by compiler version, runner OS and architecture,
  Rust target, profile and features, lockfile, Cargo configuration, and build
  and linker inputs. A changed lockfile or configuration can never restore an
  older cache. Alpine uses its own packaged compiler in a disposable
  container, with a separate cache identity.
- **Docker test cache.** Also keyed by the exact container image and compiler.
  Its fingerprint covers Cargo manifests, build scripts, the Ghostty adapter,
  Docker inputs, and the container runner's helpers. When you add a new build
  helper that these depend on, add it to `BUILD_SCRIPTS` in
  `.github/scripts/tmux_cache_key.py`.

Restored native artifacts are verified before use.
