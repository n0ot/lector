"""Publish the Cargo packages for a stable Lector release."""

import hashlib
from io import BytesIO
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tomllib
from urllib.error import HTTPError
from urllib.request import Request, urlopen

from publish_release import release_tag, version_at


PACKAGES = {
    "lector-ghostty": "crates/lector-ghostty",
    "lector-tts": "crates/lector-tts",
    "lector": "",
}
INDEX = "https://index.crates.io/le/ct/"
DOWNLOADS = "https://static.crates.io/crates/"


def validate_manifests(version, root=Path(".")):
    dependencies = tomllib.loads((root / "Cargo.toml").read_text())["dependencies"]
    for name, directory in PACKAGES.items():
        path = root / directory / "Cargo.toml"
        manifest = tomllib.loads(path.read_text())
        if manifest["package"]["version"] != version:
            raise SystemExit(f"{name} must be bumped to {version} in {path}")

    for key in ("lector-ghostty", "tts"):
        if dependencies[key].get("version") != f"={version}":
            raise SystemExit(f"Set the {key} dependency version to ={version} in Cargo.toml")


def stable_version(commit, base):
    previous, version = version_at(base), version_at(commit)
    _, prerelease = release_tag(commit, previous, version)
    if prerelease:
        return ""
    validate_manifests(version)
    return version


def registry_version(name, version):
    request = Request(INDEX + name, headers={"Cache-Control": "no-cache"})
    try:
        with urlopen(request, timeout=30) as response:
            return next(
                (entry for line in response if (entry := json.loads(line))["vers"] == version),
                None,
            )
    except HTTPError as error:
        if error.code == 404:
            return None
        raise


def already_uploaded(name, version, commit):
    entry = registry_version(name, version)
    if entry is None:
        return False
    package = f"{name}-{version}"
    if entry["yanked"]:
        raise SystemExit(f"{package} already exists on crates.io and has been yanked")

    with urlopen(f"{DOWNLOADS}{name}/{package}.crate", timeout=30) as response:
        payload = response.read()
    if hashlib.sha256(payload).hexdigest() != entry["cksum"]:
        raise SystemExit(f"{package} does not match the crates.io index checksum")
    with tarfile.open(fileobj=BytesIO(payload), mode="r:gz") as archive:
        try:
            vcs = json.load(archive.extractfile(f"{package}/.cargo_vcs_info.json"))
        except KeyError:
            raise SystemExit(f"{package} has no Cargo revision metadata") from None

    git = vcs.get("git", {})
    if git.get("sha1") != commit or git.get("dirty") or vcs.get("path_in_vcs") != PACKAGES[name]:
        raise SystemExit(f"{package} already exists on crates.io from a different checkout")
    return True


def publish(version, commit):
    if version_at(commit) != version:
        raise SystemExit("Stable release version does not match the publish revision")
    validate_manifests(version)

    # Check all three versions before any irreversible upload. A rerun can
    # resume after a partial release when earlier uploads match this commit.
    pending = []
    for name in PACKAGES:
        if already_uploaded(name, version, commit):
            print(f"{name} {version} is already published from {commit}", flush=True)
        else:
            pending.append(name)
    for name in pending:
        # Verify each package against the registry in dependency order.
        subprocess.run(
            ["cargo", "publish", "--locked", "--registry", "crates-io", "-p", name],
            check=True,
        )


if __name__ == "__main__":
    match sys.argv[1:]:
        case ["check"]:
            version = stable_version(os.environ["GITHUB_SHA"], os.environ["BASE_SHA"])
            with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
                print(f"stable_version={version}", file=output)
        case ["publish"]:
            publish(os.environ["RELEASE_VERSION"], os.environ["GITHUB_SHA"])
        case _:
            raise SystemExit("usage: publish_crates.py check|publish")
