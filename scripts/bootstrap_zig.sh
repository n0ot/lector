#!/usr/bin/env bash
set -euo pipefail

ZIG_VERSION=0.16.0

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
toolchain_root=${LECTOR_TOOLCHAIN_ROOT:-"$repo_dir/target/toolchains"}
install_dir="$toolchain_root/zig/$ZIG_VERSION"
zig_bin="$install_dir/zig"

case "$(uname -s):$(uname -m)" in
    Darwin:arm64)
        platform=aarch64-macos
        archive_sha=b23d70deaa879b5c2d486ed3316f7eaa53e84acf6fc9cc747de152450d401489
        ;;
    Darwin:x86_64)
        platform=x86_64-macos
        archive_sha=0387557ed1877bc6a2e1802c8391953baddba76081876301c522f52977b52ba7
        ;;
    Linux:aarch64|Linux:arm64)
        platform=aarch64-linux
        archive_sha=ea4b09bfb22ec6f6c6ceac57ab63efb6b46e17ab08d21f69f3a48b38e1534f17
        ;;
    Linux:x86_64)
        platform=x86_64-linux
        archive_sha=70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00
        ;;
    *)
        echo "no pinned Zig $ZIG_VERSION binary for $(uname -s) $(uname -m)" >&2
        exit 2
        ;;
esac

marker="$install_dir/.lector-zig-toolchain"
if [[ -x "$zig_bin" && -f "$marker" ]] &&
    [[ "$("$zig_bin" version)" == "$ZIG_VERSION" ]] &&
    grep -Fxq "version=$ZIG_VERSION" "$marker" &&
    grep -Fxq "platform=$platform" "$marker" &&
    grep -Fxq "archive_sha256=$archive_sha" "$marker"; then
    echo "reusing verified Zig $ZIG_VERSION at $install_dir" >&2
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
    echo "downloading pinned Zig $ZIG_VERSION for $platform" >&2
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
trap 'rm -rf "$extract_dir"' EXIT
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
trap - EXIT

echo "installed verified Zig $ZIG_VERSION at $install_dir" >&2
printf '%s\n' "$zig_bin"
