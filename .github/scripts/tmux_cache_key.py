"""Key only container artifacts; never mix them with native runner output."""
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


def fingerprint(root, identity):
    paths = {root / 'Cargo.lock', root / 'rust-toolchain.toml'}
    for directory, directories, files in os.walk(root):
        directories[:] = [name for name in directories if name not in ('target', '.git')]
        for name in files:
            path = Path(directory) / name
            relative = path.relative_to(root).as_posix()
            if (name in ('Cargo.toml', 'build.rs')
                    or relative in BUILD_SCRIPTS
                    or relative.startswith(('.cargo/', 'build_support/', 'scripts/lib/',
                                            'crates/lector-ghostty/', 'tests/docker/'))
                    or relative == '.github/scripts/tmux_cache_key.py'):
                paths.add(path)
    digest = hashlib.sha256(identity.encode())
    for path in sorted(paths):
        digest.update(str(path.relative_to(root)).encode() + b'\0')
        digest.update(path.read_bytes() + b'\0')
    return 'tmux-container-v1-' + digest.hexdigest()


def main():
    image, platform = sys.argv[1:]
    # Reuse the runner's exact platform and canonical compiler; host rustc is irrelevant.
    compiler = subprocess.check_output(
        ['docker', 'run', '--rm', '--pull', 'never', '--platform', platform,
         image, 'rustc', '-vV'], text=True)
    identity = f'{image}\n{platform}\n{compiler}\nnative-dynamic/nextest-lib-and-tmux/default-features/debug\n'
    print(fingerprint(Path(__file__).resolve().parents[2], identity))


if __name__ == '__main__':
    main()
