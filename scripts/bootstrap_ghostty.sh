#!/usr/bin/env bash
set -euo pipefail

GHOSTTY_COMMIT=43fe699071c7dceb161dc3b0c04fce46ade36174
GHOSTTY_ARCHIVE_SHA256=fbff942fc10b4d0a9de146e805922ef2b763226813fc449fdbb22c9ac7dd0f4a
REQUIRED_ZIG_VERSION=0.16.0

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
# Resolved dynamically from this script's repository root.
# shellcheck disable=SC1091
source "$repo_dir/scripts/lib/lock.sh"
target=$(rustc -vV | sed -n 's/^host: //p')
optimize=Debug
prebuilt_root=${GHOSTTY_PREBUILT_ROOT:-"$repo_dir/target/ghostty-prebuilt"}

usage() {
    echo "usage: $0 [--target RUST_TARGET] [--optimize Debug|ReleaseSafe|ReleaseFast|ReleaseSmall]" >&2
}

sha256() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        echo "Ghostty bootstrap requires shasum or sha256sum" >&2
        return 1
    fi
}

while [[ "$#" -gt 0 ]]; do
    case "$1" in
        --target)
            if [[ "$#" -lt 2 || -z "$2" || "$2" == --* ]]; then
                echo "missing value for --target" >&2
                usage
                exit 2
            fi
            target=$2
            shift 2
            ;;
        --optimize)
            if [[ "$#" -lt 2 || -z "$2" || "$2" == --* ]]; then
                echo "missing value for --optimize" >&2
                usage
                exit 2
            fi
            optimize=$2
            shift 2
            ;;
        *)
            usage
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
    aarch64-alpine-linux-musl) zig_target=aarch64-linux-musl ;;
    x86_64-alpine-linux-musl) zig_target=x86_64-linux-musl ;;
    *)
        echo "unsupported Ghostty target: $target" >&2
        exit 2
        ;;
esac

source_parent="$repo_dir/target/ghostty-source"
source_dir="$source_parent/$GHOSTTY_COMMIT"
archive="$source_parent/$GHOSTTY_COMMIT.tar.gz"
prefix="$prebuilt_root/$target/$optimize"
cache_dir="$repo_dir/target/ghostty-zig-cache/$target/$optimize"
global_cache_dir="$repo_dir/target/ghostty-zig-cache/global"
metadata_path="$prefix/lector-ghostty-build.txt"
static_archive="$prefix/static-lib/libghostty-vt.a"
abi_probe_source="$repo_dir/crates/lector-ghostty/abi/build_info_probe.c"
abi_probe_sha=$(sha256 "$abi_probe_source")
bootstrap_lock_dir="$repo_dir/target/bootstrap-locks/ghostty"
extract_dir=

cleanup() {
    if [[ -n "$extract_dir" && -d "$extract_dir" ]]; then
        rm -rf "$extract_dir"
    fi
    lector_release_lock "$bootstrap_lock_dir"
}

trap cleanup EXIT
lector_acquire_lock "$bootstrap_lock_dir" "Ghostty bootstrap"

metadata_has() {
    grep -Fxq "$1" "$metadata_path"
}

if [[ -s "$static_archive" && -f "$metadata_path" ]] &&
    metadata_has "ghostty_commit=$GHOSTTY_COMMIT" &&
    metadata_has "zig_version=$REQUIRED_ZIG_VERSION" &&
    metadata_has "target=$target" &&
    metadata_has "optimize=$optimize" &&
    metadata_has "app_runtime=none" &&
    metadata_has "emit_lib_vt=true" &&
    metadata_has "kitty_graphics=true" &&
    metadata_has "abi_header_check=passed" &&
    metadata_has "abi_probe_sha256=$abi_probe_sha"; then
    recorded_archive_sha=$(sed -n 's/^archive_sha256=//p' "$metadata_path")
    if [[ -n "$recorded_archive_sha" ]] &&
        [[ "$(sha256 "$static_archive")" == "$recorded_archive_sha" ]]; then
        printf 'reusing verified Ghostty archive: %s\n' "$static_archive" >&2
        exit 0
    fi
fi

zig_bin=$(command -v zig || true)
actual_zig=
if [[ -n "$zig_bin" ]]; then
    actual_zig=$("$zig_bin" version 2>/dev/null || true)
fi
if [[ "$actual_zig" != "$REQUIRED_ZIG_VERSION" ]]; then
    zig_bin=$("$repo_dir/scripts/bootstrap_zig.sh")
fi

mkdir -p "$source_parent" "$cache_dir" "$global_cache_dir"

if [[ ! -f "$archive" ]]; then
    archive_part="$archive.part"
    curl --proto '=https' --tlsv1.2 --fail --location \
        --output "$archive_part" \
        "https://codeload.github.com/ghostty-org/ghostty/tar.gz/$GHOSTTY_COMMIT"
    mv "$archive_part" "$archive"
fi

actual_sha=$(sha256 "$archive")
if [[ "$actual_sha" != "$GHOSTTY_ARCHIVE_SHA256" ]]; then
    echo "Ghostty archive checksum mismatch: expected $GHOSTTY_ARCHIVE_SHA256, got $actual_sha" >&2
    exit 1
fi

if [[ ! -f "$source_dir/.lector-ghostty-source" ]]; then
    extract_dir=$(mktemp -d "$source_parent/extract.XXXXXX")
    tar -xzf "$archive" --strip-components=1 -C "$extract_dir"
    printf '%s\n' "$GHOSTTY_COMMIT" > "$extract_dir/.lector-ghostty-source"
    if [[ -e "$source_dir" ]]; then
        echo "unverified Ghostty source directory already exists: $source_dir" >&2
        exit 1
    fi
    mv "$extract_dir" "$source_dir"
    extract_dir=
fi

if [[ "$(<"$source_dir/.lector-ghostty-source")" != "$GHOSTTY_COMMIT" ]]; then
    echo "Ghostty source marker does not match the pinned commit" >&2
    exit 1
fi

mkdir -p "$prefix"
(
    cd "$source_dir"
    ZIG_GLOBAL_CACHE_DIR="$global_cache_dir" "$zig_bin" build \
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
ZIG_GLOBAL_CACHE_DIR="$global_cache_dir" "$zig_bin" cc \
    -target "$zig_target" \
    -std=c11 \
    -DGHOSTTY_STATIC \
    -I "$prefix/include" \
    -c "$abi_probe_source" \
    -o "$abi_probe"

archive_sha=$(sha256 "$archive_path")
metadata_tmp="$metadata_path.tmp.$$"
{
    printf 'ghostty_commit=%s\n' "$GHOSTTY_COMMIT"
    printf 'zig_version=%s\n' "$REQUIRED_ZIG_VERSION"
    printf 'target=%s\n' "$target"
    printf 'optimize=%s\n' "$optimize"
    printf 'app_runtime=none\n'
    printf 'emit_lib_vt=true\n'
    printf 'kitty_graphics=true\n'
    printf 'abi_header_check=passed\n'
    printf 'abi_probe_sha256=%s\n' "$abi_probe_sha"
    printf 'archive_sha256=%s\n' "$archive_sha"
} > "$metadata_tmp"
mv "$metadata_tmp" "$metadata_path"

printf 'verified Ghostty archive: %s\n' "$archive_path" >&2
