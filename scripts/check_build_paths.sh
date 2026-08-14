#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_dir"

cargo_bin=${CARGO:-cargo}

"$cargo_bin" build --locked
"$cargo_bin" build --locked --release
"$cargo_bin" build --locked --no-default-features
