"""Key only container artifacts; never mix them with native runner output.

The container cache has two parts with different inputs:

- ``ghostty``: the verified Ghostty archive and Zig toolchain. These depend on
  the container image, the platform, and the Ghostty/Zig bootstrap, never on
  Rust sources. The bootstrap also re-verifies a restored archive's metadata.
- ``cargo``: the Cargo registry and debug build output. These additionally
  depend on the compiler, lockfile, manifests and build helpers. Ordinary
  Rust source edits keep the same key prefix, so a fallback restore reuses
  dependencies and Cargo's own fingerprints rebuild what changed.
"""
import hashlib
import os
from pathlib import Path
import subprocess
import sys


# Scripts used by the container runner, bootstrap, and Cargo compiler/linker chain.
# Keep this boundary aligned when those scripts acquire another dependency;
# unrelated developer/test runners do not change the container's build inputs.
BUILD_SCRIPTS = frozenset({
    'scripts/test-real-tmux-docker',
    'scripts/check-rust-toolchain',
    'scripts/bootstrap_ghostty.sh',
    'scripts/bootstrap_zig.sh',
    'scripts/rustc-static-linux',
    'scripts/cargo-musl',
    'scripts/zig-cc',
    'scripts/zig-lld',
})

# Everything the Ghostty archive and Zig toolchain are built from or checked
# against. The pins live in the bootstrap scripts and build_support/ghostty.rs.
GHOSTTY_FILES = frozenset({
    'build_support/ghostty.rs',
    'crates/lector-ghostty/build.rs',
    'scripts/bootstrap_ghostty.sh',
    'scripts/bootstrap_zig.sh',
    '.github/scripts/tmux_cache_key.py',
})
GHOSTTY_DIRECTORIES = ('scripts/lib/', 'crates/lector-ghostty/bootstrap/',
                       'crates/lector-ghostty/abi/')

CARGO_DIRECTORIES = ('.cargo/', 'build_support/', 'scripts/lib/', 'tests/docker/',
                     'crates/lector-ghostty/bootstrap/', 'crates/lector-ghostty/abi/')


def _walk(root):
    for directory, directories, files in os.walk(root):
        directories[:] = [name for name in directories if name not in ('target', '.git')]
        for name in files:
            path = Path(directory) / name
            yield path, path.relative_to(root).as_posix()


def _digest(root, identity, paths):
    digest = hashlib.sha256(identity.encode())
    for path in sorted(paths):
        digest.update(str(path.relative_to(root)).encode() + b'\0')
        digest.update(path.read_bytes() + b'\0')
    return digest.hexdigest()


def ghostty_fingerprint(root, identity):
    paths = {path for path, relative in _walk(root)
             if relative in GHOSTTY_FILES or relative.startswith(GHOSTTY_DIRECTORIES)}
    return 'tmux-ghostty-v1-' + _digest(root, identity, paths)


def cargo_fingerprint(root, identity):
    paths = {root / 'Cargo.lock', root / 'rust-toolchain.toml'}
    for path, relative in _walk(root):
        if (path.name in ('Cargo.toml', 'build.rs')
                or relative in BUILD_SCRIPTS
                or relative.startswith(CARGO_DIRECTORIES)
                or relative == '.github/scripts/tmux_cache_key.py'):
            paths.add(path)
    return 'tmux-container-v2-' + _digest(root, identity, paths)


def main():
    image, platform = sys.argv[1:]
    # Reuse the runner's exact platform and canonical compiler; host rustc is irrelevant.
    compiler = subprocess.check_output(
        ['docker', 'run', '--rm', '--pull', 'never', '--platform', platform,
         image, 'rustc', '-vV'], text=True)
    root = Path(__file__).resolve().parents[2]
    ghostty_identity = f'{image}\n{platform}\nghostty-prebuilt/toolchains\n'
    cargo_identity = f'{image}\n{platform}\n{compiler}\nnative-dynamic/nextest-lib-and-tmux/default-features/debug\n'
    print(f'ghostty={ghostty_fingerprint(root, ghostty_identity)}')
    print(f'cargo={cargo_fingerprint(root, cargo_identity)}')


if __name__ == '__main__':
    main()
