"""Publish the validated archives from a master push to GitHub Releases."""

import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib


TARGETS = (
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-musl",
    "x86_64-unknown-linux-musl",
)


def version_at(revision):
    manifest = subprocess.check_output(["git", "show", f"{revision}:Cargo.toml"], text=True)
    version = tomllib.loads(manifest)["package"]["version"]
    if not re.fullmatch(r"(0|[1-9][0-9]*)(\.(0|[1-9][0-9]*)){2}", version):
        raise SystemExit(f"Expected a numeric Lector version at {revision}, got {version}")
    return version


def release_tag(commit, previous, version):
    if version == previous:
        return f"build-{commit}", True
    if tuple(map(int, version.split("."))) <= tuple(map(int, previous.split("."))):
        raise SystemExit(f"Lector version must increase from {previous} to release {version}")
    return f"v{version}", False


def release_assets(directory):
    names = [f"lector-{target}.tar.gz" for target in TARGETS]
    if set(names) != {path.name for path in directory.glob("*.tar.gz")}:
        raise SystemExit("Expected one release archive for each supported target")
    with (directory / "SHA256SUMS").open("w", encoding="utf-8") as checksums:
        for name in names:
            with (directory / name).open("rb") as archive:
                digest = hashlib.file_digest(archive, "sha256").hexdigest()
            checksums.write(f"{digest}  {name}\n")
    return [directory / name for name in [*names, "SHA256SUMS"]]


def tag_commit(tag):
    result = subprocess.run(
        ["git", "rev-parse", "--verify", "--quiet", f"refs/tags/{tag}^{{commit}}"],
        capture_output=True, text=True,
    )
    return result.stdout.strip()


def publish(commit, previous, version, assets):
    tag, prerelease = release_tag(commit, previous, version)
    tagged_commit = tag_commit(tag)
    if tagged_commit and tagged_commit != commit:
        raise SystemExit(f"Tag {tag} already points to {tagged_commit}, expected {commit}")

    result = subprocess.run(
        ["gh", "release", "view", tag, "--json", "assets,isDraft,isPrerelease,targetCommitish"],
        capture_output=True, text=True,
    )
    if result.returncode == 0:
        release = json.loads(result.stdout)
        target = tagged_commit or release["targetCommitish"]
        if target != commit or release["isPrerelease"] != prerelease:
            raise SystemExit(f"Existing release {tag} does not match the commit or release type")
        if not release["isDraft"]:
            if not tagged_commit or {asset.name for asset in assets} != {
                asset["name"] for asset in release["assets"]
            }:
                raise SystemExit(f"Published release {tag} is missing the expected tag or assets")
            print(f"Release {tag} is already published")
            return
    elif result.stderr.strip() == "release not found":
        flags = ["--target", commit, "--title", tag]
        if tagged_commit:
            flags.append("--verify-tag")
        if prerelease:
            flags += ["--prerelease", "--latest=false", "--notes", f"Lector {version} build from {commit}."]
        else:
            flags.append("--generate-notes")
            previous_tag = f"v{previous}"
            if tag_commit(previous_tag):
                flags += ["--notes-start-tag", previous_tag]
        subprocess.run(["gh", "release", "create", tag, "--draft", *flags], check=True)
    else:
        raise SystemExit(result.stderr.strip() or f"Could not look up release {tag}")

    # Reruns can replace assets from an interrupted draft upload.
    subprocess.run(["gh", "release", "upload", tag, *assets, "--clobber"], check=True)
    flags = ["--latest=false"] if prerelease else []
    subprocess.run(["gh", "release", "edit", tag, "--draft=false", *flags], check=True)


if __name__ == "__main__":
    commit = os.environ["GITHUB_SHA"]
    previous = version_at(os.environ["BASE_SHA"])
    publish(commit, previous, version_at(commit), release_assets(Path("dist")))
