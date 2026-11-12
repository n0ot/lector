#!/usr/bin/env bash
# Compatibility entry point; the packaged crate owns the portable lock protocol.
# shellcheck source-path=SCRIPTDIR
# shellcheck source=../../crates/lector-ghostty/bootstrap/lock.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/crates/lector-ghostty/bootstrap/lock.sh"
