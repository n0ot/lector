"""Fingerprint Cargo build inputs that must also partition fallback cache restores."""

import argparse
import hashlib
import os
from pathlib import Path
import subprocess


CONFIG_PATHS = (
    "Cargo.lock", "Cargo.toml", "rust-toolchain.toml", ".cargo/config.toml",
    "scripts/rustc-static-linux", "scripts/cargo-musl", "scripts/setup",
    "scripts/zig-cc", "scripts/zig-lld",
    "scripts/bootstrap_zig.sh", "scripts/bootstrap_ghostty.sh",
)


def fingerprint(root, compiler, target, lane):
    digest = hashlib.sha256()
    for value in (compiler, target, lane):
        digest.update(value.encode() + b"\0")
    paths = set(CONFIG_PATHS)
    # Workspace outputs are pruned by rust-cache. Ordinary Rust source edits
    # must not discard third-party dependencies; Cargo fingerprints those edits.
    # Keep manifests and native/build helpers in the fallback compatibility key.
    for directory, directories, files in os.walk(root):
        directories[:] = [name for name in directories if name not in ("target", ".git")]
        for name in files:
            path = Path(directory) / name
            relative = path.relative_to(root).as_posix()
            if (name in ("Cargo.toml", "build.rs")
                    or relative.startswith((".cargo/", "build_support/", "scripts/lib/",
                                            "crates/lector-ghostty/bootstrap/",
                                            "crates/lector-ghostty/abi/",
                                            "crates/lector-tts/android/"))):
                paths.add(relative)
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
