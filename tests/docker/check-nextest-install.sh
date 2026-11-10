#!/usr/bin/env bash
# Run inside the Ubuntu integration image; no network or Rust build is needed.
set -euo pipefail
installer=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/install-nextest.sh
work_dir=$(mktemp -d)
trap 'rm -rf "$work_dir"' EXIT
mkdir "$work_dir/bin"
cat > "$work_dir/bin/curl" <<'MOCK'
#!/usr/bin/env bash
printf '%s\n' "$@" > "$NEXTEST_TEST_LOG"
printf 'corrupt archive\n' > "${@: -1}"
MOCK
cat > "$work_dir/bin/tar" <<'MOCK'
#!/usr/bin/env bash
# A checksum mismatch must stop installation before extraction.
touch "$NEXTEST_TEST_EXTRACTED"
exit 1
MOCK
chmod +x "$work_dir/bin/curl" "$work_dir/bin/tar"
export PATH="$work_dir/bin:$PATH"
export CARGO_HOME="$work_dir/cargo"
export NEXTEST_TEST_LOG="$work_dir/download"
export NEXTEST_TEST_EXTRACTED="$work_dir/extracted"
for architecture in amd64 arm64; do
    case "$architecture" in
        amd64) target=x86_64-unknown-linux-gnu ;;
        arm64) target=aarch64-unknown-linux-gnu ;;
    esac
    if bash "$installer" "$architecture" > "$work_dir/output" 2>&1; then
        echo "corrupt $architecture archive was accepted" >&2
        exit 1
    fi
    grep -Fx "https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-0.9.143/cargo-nextest-0.9.143-$target.tar.gz" "$NEXTEST_TEST_LOG"
    grep 'FAILED' "$work_dir/output"
    test ! -e "$NEXTEST_TEST_EXTRACTED"
    test ! -e "$CARGO_HOME/bin/cargo-nextest"
done
rm "$NEXTEST_TEST_LOG"
for architecture in unsupported ''; do
    if bash "$installer" "$architecture" > "$work_dir/output" 2>&1; then
        echo "unsupported architecture was accepted" >&2
        exit 1
    fi
    grep 'unsupported nextest Docker architecture:' "$work_dir/output"
    test ! -e "$NEXTEST_TEST_LOG"
done
printf 'nextest architecture selection and checksum rejection verified\n'
