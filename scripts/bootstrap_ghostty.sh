#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
# The crate owns the implementation so published archives remain self-contained.
# Repository entry points keep their workspace cache default explicitly.
export LECTOR_GHOSTTY_BUILD_ROOT=${LECTOR_GHOSTTY_BUILD_ROOT:-"$repo_dir/target"}
exec "$repo_dir/crates/lector-ghostty/bootstrap/bootstrap_ghostty.sh" "$@"
