#!/usr/bin/env bash
set -euo pipefail

# Official cargo-nextest 0.9.143 release archives and published SHA-256 sums:
# https://github.com/nextest-rs/nextest/releases/tag/cargo-nextest-0.9.143
version=0.9.143
case "${1:-}" in
    amd64)
        target=x86_64-unknown-linux-gnu
        checksum=66786b9abe23920d022a182d1416b1bbc8130dd4872a9553d76985a1708dcd1e
        ;;
    arm64)
        target=aarch64-unknown-linux-gnu
        checksum=2a64b3566a92508550a7ab29c3e8db25472ca37730ecb4d22100b6aa440c2a68
        ;;
    *)
        echo "unsupported nextest Docker architecture: ${1:-<unset>}" >&2
        exit 2
        ;;
esac

archive="cargo-nextest-$version-$target.tar.gz"
url="https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-$version/$archive"
work_dir=$(mktemp -d)
trap 'rm -rf "$work_dir"' EXIT
curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
    "$url" --output "$work_dir/$archive"
printf '%s  %s\n' "$checksum" "$work_dir/$archive" | sha256sum --check --strict
mkdir -p "${CARGO_HOME:?}/bin"
tar --extract --gzip --file "$work_dir/$archive" --directory "$CARGO_HOME/bin" cargo-nextest
installed_version=$("$CARGO_HOME/bin/cargo-nextest" --version)
case "$installed_version" in
    "cargo-nextest $version" | "cargo-nextest $version "*) printf '%s\n' "$installed_version" ;;
    *) echo "unexpected nextest version: $installed_version" >&2; exit 1 ;;
esac
