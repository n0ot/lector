#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_dir"

cargo_bin=${CARGO:-cargo}
host_target=$(rustc -vV | sed -n 's/^host: //p')
zig_bin=$(scripts/bootstrap_zig.sh)
PATH="$(dirname "$zig_bin"):$PATH"
export PATH

scripts/bootstrap_ghostty.sh --target "$host_target" --optimize Debug
scripts/bootstrap_ghostty.sh --target "$host_target" --optimize ReleaseFast

"$cargo_bin" build --locked --features ghostty-vt
"$cargo_bin" test --locked --features ghostty-vt --test ghostty_build
"$cargo_bin" build --locked --release --features ghostty-vt
"$cargo_bin" test --locked --release --features ghostty-vt --test ghostty_build

target/debug/lector --version
target/release/lector --version

ghostty_archive="target/ghostty-prebuilt/$host_target/ReleaseFast/static-lib/libghostty-vt.a"
if [[ ! -f "$ghostty_archive" ]]; then
    echo "release build did not produce the expected static libghostty-vt archive" >&2
    exit 1
fi
if ! file "$ghostty_archive" | grep -q 'ar archive'; then
    echo "release libghostty-vt artifact is not a static archive: $ghostty_archive" >&2
    exit 1
fi

ghostty_test_binary=$(find target/release/build/lector \
    -type f -name 'ghostty_build-*' -perm -111 -print -quit)
if [[ -z "$ghostty_test_binary" ]]; then
    echo "could not locate the release Ghostty smoke-test binary" >&2
    exit 1
fi

case "$(uname -s)" in
    Darwin)
        if otool -L target/release/lector "$ghostty_test_binary" | grep -q 'libghostty-vt'; then
            echo "release artifacts unexpectedly depend on a shared libghostty-vt" >&2
            exit 1
        fi
        if otool -L "$ghostty_test_binary" | grep -Eq '/(AppKit|SwiftUI|UIKit|QuartzCore)\.framework/'; then
            echo "headless Ghostty smoke test unexpectedly links a GUI framework" >&2
            exit 1
        fi
        ;;
    Linux)
        if ldd target/release/lector "$ghostty_test_binary" | grep -q 'libghostty-vt'; then
            echo "release artifacts unexpectedly depend on a shared libghostty-vt" >&2
            exit 1
        fi
        if ldd "$ghostty_test_binary" | grep -Eq 'lib(gtk|gdk|adwaita|wayland|X11)'; then
            echo "headless Ghostty smoke test unexpectedly links a GUI library" >&2
            exit 1
        fi
        ;;
esac
