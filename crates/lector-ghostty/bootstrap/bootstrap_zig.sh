#!/usr/bin/env bash
set -euo pipefail

ZIG_VERSION=0.17.0

bootstrap_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# Resolved dynamically from this crate's bootstrap directory.
# shellcheck disable=SC1091
source "$bootstrap_dir/lock.sh"
build_root=${LECTOR_GHOSTTY_BUILD_ROOT:-"$bootstrap_dir/../target"}
toolchain_root=${LECTOR_TOOLCHAIN_ROOT:-"$build_root/toolchains"}
install_dir="$toolchain_root/zig/$ZIG_VERSION"
zig_bin="$install_dir/zig"

progress() {
    if [[ "${LECTOR_BOOTSTRAP_QUIET:-0}" != 1 ]]; then
        echo "$@" >&2
    fi
}

case "$(uname -s):$(uname -m)" in
    Darwin:arm64)
        platform=aarch64-macos
        archive_sha=b607e9b9234790a008116ae5bdb71c6243b84b9fb42a53a9e70fde41c06c536a
        ;;
    Darwin:x86_64)
        platform=x86_64-macos
        archive_sha=4f9a1c5269aa17ebda5e6d3c2b89d6cbf36f7d2b22a0306e9ab98f25f95529c6
        ;;
    Linux:aarch64|Linux:arm64)
        platform=aarch64-linux
        archive_sha=9e8d11661d4ae3bd57702a3832781e23ad151dde5798e16a5ccd503f65234ff8
        ;;
    Linux:x86_64)
        platform=x86_64-linux
        archive_sha=1cbe9df9f27e6b78d14ccbca43b6703a404ef79ef1c463de901d7f088d4e2026
        ;;
    *)
        echo "no pinned Zig $ZIG_VERSION binary for $(uname -s) $(uname -m)" >&2
        exit 2
        ;;
esac

bootstrap_lock_dir="$toolchain_root/bootstrap-locks/zig-$ZIG_VERSION"
extract_dir=

cleanup() {
    if [[ -n "$extract_dir" && -d "$extract_dir" ]]; then
        rm -rf "$extract_dir"
    fi
    lector_release_lock "$bootstrap_lock_dir"
}

trap cleanup EXIT
lector_acquire_lock "$bootstrap_lock_dir" "Zig $ZIG_VERSION bootstrap"

marker="$install_dir/.lector-zig-toolchain"
if [[ -x "$zig_bin" && -f "$marker" ]] &&
    [[ "$("$zig_bin" version)" == "$ZIG_VERSION" ]] &&
    grep -Fxq "version=$ZIG_VERSION" "$marker" &&
    grep -Fxq "platform=$platform" "$marker" &&
    grep -Fxq "archive_sha256=$archive_sha" "$marker"; then
    progress "reusing verified Zig $ZIG_VERSION at $install_dir"
    printf '%s\n' "$zig_bin"
    exit 0
fi

if [[ -e "$install_dir" ]]; then
    echo "invalid cached Zig toolchain at $install_dir; remove that version directory and retry" >&2
    exit 1
fi

download_dir="$toolchain_root/downloads"
archive_name="zig-$platform-$ZIG_VERSION.tar.xz"
archive="$download_dir/$archive_name"
url="https://ziglang.org/download/$ZIG_VERSION/$archive_name"
mkdir -p "$download_dir" "$(dirname "$install_dir")"

sha256() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        echo "Zig bootstrap requires shasum or sha256sum" >&2
        return 1
    fi
}

if [[ ! -f "$archive" ]]; then
    archive_part="$archive.part"
    progress "downloading pinned Zig $ZIG_VERSION for $platform"
    curl --proto '=https' --tlsv1.2 --fail --location \
        --output "$archive_part" "$url"
    mv "$archive_part" "$archive"
fi

actual_sha=$(sha256 "$archive")
if [[ "$actual_sha" != "$archive_sha" ]]; then
    echo "Zig archive checksum mismatch: expected $archive_sha, got $actual_sha" >&2
    exit 1
fi

extract_dir=$(mktemp -d "$toolchain_root/extract.XXXXXX")
tar -xJf "$archive" --strip-components=1 -C "$extract_dir"

actual_version=$("$extract_dir/zig" version)
if [[ "$actual_version" != "$ZIG_VERSION" ]]; then
    echo "downloaded Zig reports $actual_version, expected $ZIG_VERSION" >&2
    exit 1
fi

{
    printf 'version=%s\n' "$ZIG_VERSION"
    printf 'platform=%s\n' "$platform"
    printf 'archive_sha256=%s\n' "$archive_sha"
} > "$extract_dir/.lector-zig-toolchain"

mv "$extract_dir" "$install_dir"
extract_dir=

progress "installed verified Zig $ZIG_VERSION at $install_dir"
printf '%s\n' "$zig_bin"
