#!/usr/bin/env bash
set -euo pipefail

GHOSTTY_COMMIT=43fe699071c7dceb161dc3b0c04fce46ade36174
GHOSTTY_ARCHIVE_SHA256=fbff942fc10b4d0a9de146e805922ef2b763226813fc449fdbb22c9ac7dd0f4a
REQUIRED_ZIG_VERSION=0.16.0

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
target=$(rustc -vV | sed -n 's/^host: //p')
optimize=Debug

while [[ "$#" -gt 0 ]]; do
    case "$1" in
        --target)
            target=$2
            shift 2
            ;;
        --optimize)
            optimize=$2
            shift 2
            ;;
        *)
            echo "usage: $0 [--target RUST_TARGET] [--optimize Debug|ReleaseSafe|ReleaseFast|ReleaseSmall]" >&2
            exit 2
            ;;
    esac
done

case "$optimize" in
    Debug|ReleaseSafe|ReleaseFast|ReleaseSmall) ;;
    *)
        echo "unsupported Ghostty optimization mode: $optimize" >&2
        exit 2
        ;;
esac

case "$target" in
    aarch64-apple-darwin) zig_target=aarch64-macos-none ;;
    x86_64-apple-darwin) zig_target=x86_64-macos-none ;;
    aarch64-unknown-linux-gnu) zig_target=aarch64-linux-gnu ;;
    x86_64-unknown-linux-gnu) zig_target=x86_64-linux-gnu ;;
    aarch64-unknown-linux-musl) zig_target=aarch64-linux-musl ;;
    x86_64-unknown-linux-musl) zig_target=x86_64-linux-musl ;;
    *)
        echo "unsupported Ghostty target: $target" >&2
        exit 2
        ;;
esac

actual_zig=$(zig version 2>/dev/null || true)
if [[ "$actual_zig" != "$REQUIRED_ZIG_VERSION" ]]; then
    echo "Ghostty bootstrap requires Zig $REQUIRED_ZIG_VERSION on PATH; found ${actual_zig:-nothing}" >&2
    exit 1
fi

source_parent="$repo_dir/target/ghostty-source"
source_dir="$source_parent/$GHOSTTY_COMMIT"
archive="$source_parent/$GHOSTTY_COMMIT.tar.gz"
prefix="$repo_dir/target/ghostty-prebuilt/$target/$optimize"
cache_dir="$repo_dir/target/ghostty-zig-cache/$target/$optimize"
global_cache_dir="$repo_dir/target/ghostty-zig-cache/global"

mkdir -p "$source_parent" "$cache_dir" "$global_cache_dir"

if [[ ! -f "$archive" ]]; then
    archive_part="$archive.part"
    curl --proto '=https' --tlsv1.2 --fail --location \
        --output "$archive_part" \
        "https://codeload.github.com/ghostty-org/ghostty/tar.gz/$GHOSTTY_COMMIT"
    mv "$archive_part" "$archive"
fi

actual_sha=$(shasum -a 256 "$archive" | awk '{print $1}')
if [[ "$actual_sha" != "$GHOSTTY_ARCHIVE_SHA256" ]]; then
    echo "Ghostty archive checksum mismatch: expected $GHOSTTY_ARCHIVE_SHA256, got $actual_sha" >&2
    exit 1
fi

if [[ ! -f "$source_dir/.lector-ghostty-source" ]]; then
    extract_dir=$(mktemp -d "$source_parent/extract.XXXXXX")
    trap 'rm -rf "$extract_dir"' EXIT
    tar -xzf "$archive" --strip-components=1 -C "$extract_dir"
    printf '%s\n' "$GHOSTTY_COMMIT" > "$extract_dir/.lector-ghostty-source"
    if [[ -e "$source_dir" ]]; then
        echo "unverified Ghostty source directory already exists: $source_dir" >&2
        exit 1
    fi
    mv "$extract_dir" "$source_dir"
    trap - EXIT
fi

if [[ "$(<"$source_dir/.lector-ghostty-source")" != "$GHOSTTY_COMMIT" ]]; then
    echo "Ghostty source marker does not match the pinned commit" >&2
    exit 1
fi

mkdir -p "$prefix"
(
    cd "$source_dir"
    ZIG_GLOBAL_CACHE_DIR="$global_cache_dir" zig build \
        -Demit-lib-vt=true \
        -Demit-xcframework=false \
        -Dapp-runtime=none \
        -Dtarget="$zig_target" \
        -Doptimize="$optimize" \
        --prefix "$prefix" \
        --cache-dir "$cache_dir"
)

archive_path="$prefix/lib/libghostty-vt.a"
if [[ ! -f "$archive_path" ]]; then
    echo "Ghostty did not produce the expected static archive: $archive_path" >&2
    exit 1
fi

# Keep the Rust link search path free of the identically named dylib that
# Ghostty also installs. This makes static selection unambiguous even for an
# otherwise empty Rust unit-test harness on macOS.
static_lib_dir="$prefix/static-lib"
mkdir -p "$static_lib_dir"
cp "$archive_path" "$static_lib_dir/libghostty-vt.a"
archive_path="$static_lib_dir/libghostty-vt.a"

abi_probe="$prefix/lector-ghostty-build-info-abi.o"
ZIG_GLOBAL_CACHE_DIR="$global_cache_dir" zig cc \
    -target "$zig_target" \
    -std=c11 \
    -DGHOSTTY_STATIC \
    -I "$prefix/include" \
    -c "$repo_dir/crates/lector-ghostty/abi/build_info_probe.c" \
    -o "$abi_probe"

{
    printf 'ghostty_commit=%s\n' "$GHOSTTY_COMMIT"
    printf 'zig_version=%s\n' "$REQUIRED_ZIG_VERSION"
    printf 'target=%s\n' "$target"
    printf 'optimize=%s\n' "$optimize"
    printf 'app_runtime=none\n'
    printf 'emit_lib_vt=true\n'
    printf 'kitty_graphics=true\n'
    printf 'abi_header_check=passed\n'
} > "$prefix/lector-ghostty-build.txt"

printf 'verified Ghostty archive: %s\n' "$archive_path"
