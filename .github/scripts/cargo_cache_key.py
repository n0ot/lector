"""Fingerprint Cargo build inputs that must also partition fallback cache restores."""

import argparse
import hashlib
import os
from pathlib import Path
import subprocess


CONFIG_PATHS = (
    "Cargo.lock", "Cargo.toml", "rust-toolchain.toml", ".cargo/config.toml",
    ".github/workflows/ghostty-build.yml", "scripts/rustc-static-linux",
    "scripts/static-pie-cc", "scripts/zig-cc", "scripts/zig-lld",
    "scripts/bootstrap_zig.sh", "scripts/bootstrap_ghostty.sh",
)


def fingerprint(root, compiler, target, lane):
    digest = hashlib.sha256()
    for value in (compiler, target, lane):
        digest.update(value.encode() + b"\0")
    paths = set(CONFIG_PATHS)
    for directory in ("crates", "xtask", "build_support"):
        paths.update(str(path.relative_to(root)) for path in (root / directory).rglob("*")
                     if path.is_file())
    for name in sorted(paths):
        path = root / name
        if path.is_file():
            digest.update(name.encode() + b"\0" + path.read_bytes() + b"\0")
    return digest.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument("--lane", required=True)
    parser.add_argument("--compiler-metadata", type=Path)
    args = parser.parse_args()
    compiler = (args.compiler_metadata.read_text() if args.compiler_metadata else
                subprocess.check_output([os.environ.get("RUSTC", "rustc"), "-vV"], text=True))
    key = fingerprint(Path.cwd(), compiler, args.target, args.lane)
    print(f"Cargo cache inputs: {args.lane} / {args.target} / {key}")
    with open(os.environ["GITHUB_OUTPUT"], "a") as output:
        output.write(f"key={key}\n")


if __name__ == "__main__":
    main()
