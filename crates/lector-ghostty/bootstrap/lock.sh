#!/usr/bin/env bash

# Portable, directory-based lock for bootstrap scripts. `mkdir` is atomic on
# the filesystems supported by the build, unlike checking for a marker before
# creating it. Callers must arrange for lector_release_lock to run on EXIT.

lector_acquire_lock() {
    local lock_dir=$1
    local description=${2:-bootstrap resource}
    local timeout_seconds=${3:-3600}
    local deadline
    local owner=
    local announced=false
    local stale_dir=

    if [[ ! "$timeout_seconds" =~ ^[1-9][0-9]*$ ]]; then
        echo "invalid lock timeout: $timeout_seconds" >&2
        return 2
    fi

    mkdir -p "$(dirname "$lock_dir")"
    deadline=$((SECONDS + timeout_seconds))
    while ! mkdir "$lock_dir" 2>/dev/null; do
        owner=
        if [[ -f "$lock_dir/owner-pid" ]]; then
            owner=$(<"$lock_dir/owner-pid")
        fi
        if [[ "$owner" =~ ^[1-9][0-9]*$ ]] && ! kill -0 "$owner" 2>/dev/null; then
            # A killed bootstrap cannot run its EXIT trap. Rename first so
            # only one contender reclaims the abandoned directory.
            stale_dir="$lock_dir.stale.$$.$RANDOM"
            if mv "$lock_dir" "$stale_dir" 2>/dev/null; then
                rm -rf -- "$stale_dir"
                echo "reclaimed stale $description lock from pid $owner" >&2
                continue
            fi
        fi
        if [[ "$announced" == false ]]; then
            echo "waiting for $description lock: $lock_dir" >&2
            announced=true
        fi
        if ((SECONDS >= deadline)); then
            echo "timed out waiting for $description lock: $lock_dir${owner:+ (owner pid $owner)}" >&2
            return 1
        fi
        sleep 1
    done

    if ! printf '%s\n' "$$" > "$lock_dir/owner-pid"; then
        rmdir "$lock_dir" 2>/dev/null || true
        return 1
    fi
}

lector_release_lock() {
    local lock_dir=$1
    local owner=

    if [[ -f "$lock_dir/owner-pid" ]]; then
        owner=$(<"$lock_dir/owner-pid")
    fi
    if [[ "$owner" != "$$" ]]; then
        return 0
    fi

    rm -f "$lock_dir/owner-pid"
    rmdir "$lock_dir" 2>/dev/null || {
        echo "could not release bootstrap lock: $lock_dir" >&2
        return 1
    }
}
