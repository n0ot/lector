#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_dir"

cargo_bin=${CARGO:-cargo}
host_target=$(rustc -vV | sed -n 's/^host: //p')
prebuilt_root=${GHOSTTY_PREBUILT_ROOT:-"$repo_dir/target/ghostty-prebuilt"}

debug_lector=$(LECTOR_GHOSTTY_OPTIMIZE=Debug \
    "$cargo_bin" build --locked --features ghostty-vt --bin lector \
        --message-format=json-render-diagnostics | \
    "$repo_dir/scripts/cargo-artifact" lector bin)
LECTOR_GHOSTTY_OPTIMIZE=Debug \
    "$cargo_bin" test --locked --features ghostty-vt --test ghostty_build
release_lector=$("$cargo_bin" build --locked --release --features ghostty-vt --bin lector \
    --message-format=json-render-diagnostics | \
    "$repo_dir/scripts/cargo-artifact" lector bin)
ghostty_test_binary=$("$cargo_bin" test --locked --release --features ghostty-vt \
    --test ghostty_build --no-run --message-format=json-render-diagnostics | \
    "$repo_dir/scripts/cargo-artifact" ghostty_build test)
"$ghostty_test_binary"

"$debug_lector" --version
"$release_lector" --version

ghostty_archive="$prebuilt_root/$host_target/ReleaseFast/static-lib/libghostty-vt.a"
if [[ ! -f "$ghostty_archive" ]]; then
    echo "release build did not produce the expected static libghostty-vt archive" >&2
    exit 1
fi
if ! file "$ghostty_archive" | grep -q 'ar archive'; then
    echo "release libghostty-vt artifact is not a static archive: $ghostty_archive" >&2
    exit 1
fi

case "$(uname -s)" in
    Darwin)
        if otool -L "$release_lector" "$ghostty_test_binary" | grep -q 'libghostty-vt'; then
            echo "release artifacts unexpectedly depend on a shared libghostty-vt" >&2
            exit 1
        fi
        if otool -L "$ghostty_test_binary" | grep -Eq '/(AppKit|SwiftUI|UIKit|QuartzCore)\.framework/'; then
            echo "headless Ghostty smoke test unexpectedly links a GUI framework" >&2
            exit 1
        fi
        ;;
    Linux)
        if ldd "$release_lector" "$ghostty_test_binary" | grep -q 'libghostty-vt'; then
            echo "release artifacts unexpectedly depend on a shared libghostty-vt" >&2
            exit 1
        fi
        if ldd "$ghostty_test_binary" | grep -Eq 'lib(gtk|gdk|adwaita|wayland|X11)'; then
            echo "headless Ghostty smoke test unexpectedly links a GUI library" >&2
            exit 1
        fi
        ;;
esac
