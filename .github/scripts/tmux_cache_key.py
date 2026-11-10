"""Key only container artifacts; never mix them with native runner output."""
import hashlib
import os
from pathlib import Path
import subprocess
import sys


def fingerprint(root, identity):
    paths = {root / 'Cargo.lock', root / 'rust-toolchain.toml'}
    for directory, directories, files in os.walk(root):
        directories[:] = [name for name in directories if name not in ('target', '.git')]
        for name in files:
            path = Path(directory) / name
            relative = path.relative_to(root).as_posix()
            if (name in ('Cargo.toml', 'build.rs')
                    or relative.startswith(('.cargo/', 'scripts/', 'crates/lector-ghostty/', 'tests/docker/'))
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
    identity = f'{image}\n{platform}\n{compiler}\nnextest-lib-and-tmux/default-features/debug\n'
    print(fingerprint(Path(__file__).resolve().parents[2], identity))


if __name__ == '__main__':
    main()
