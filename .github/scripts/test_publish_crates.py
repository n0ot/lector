import hashlib
from io import BytesIO
import json
from pathlib import Path
import tarfile
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import call, patch

from publish_crates import (
    PACKAGES, already_uploaded, publish, stable_version, validate_manifests,
)


class CrateReleaseTests(unittest.TestCase):
    @patch("publish_crates.validate_manifests")
    @patch("publish_crates.version_at", side_effect=["1.2.3", "1.2.3"])
    def test_same_version_skips_registry_release(self, _version_at, validate):
        self.assertEqual(stable_version("head", "base"), "")
        validate.assert_not_called()

    @patch("publish_crates.validate_manifests")
    @patch("publish_crates.version_at", side_effect=["1.2.3", "1.2.4"])
    def test_bump_validates_all_manifests(self, _version_at, validate):
        self.assertEqual(stable_version("head", "base"), "1.2.4")
        validate.assert_called_once_with("1.2.4")

    def test_release_requires_matching_versions_and_exact_requirements(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            for name, folder in PACKAGES.items():
                manifest = root / folder / "Cargo.toml"
                manifest.parent.mkdir(parents=True, exist_ok=True)
                manifest.write_text(f'[package]\nname = "{name}"\nversion = "1.2.4"\n')
            root_manifest = root / "Cargo.toml"
            package = root_manifest.read_text()
            root_manifest.write_text(
                package + '[dependencies]\nlector-ghostty = { version = "=1.2.4" }\n'
                'tts = { version = "1.2.4" }\n'
            )
            with self.assertRaisesRegex(SystemExit, "tts dependency version"):
                validate_manifests("1.2.4", root)
            root_manifest.write_text(root_manifest.read_text().replace(
                'tts = { version = "1.2.4" }', 'tts = { version = "=1.2.4" }',
            ))
            validate_manifests("1.2.4", root)
            with self.assertRaisesRegex(SystemExit, "lector-ghostty must be bumped"):
                validate_manifests("1.2.5", root)

    @patch("publish_crates.subprocess.run")
    @patch("publish_crates.already_uploaded", side_effect=[True, False, False])
    @patch("publish_crates.validate_manifests")
    @patch("publish_crates.version_at", return_value="1.2.4")
    def test_partial_release_resumes_in_dependency_order(self, _version, _validate, existing, run):
        publish("1.2.4", "head")
        self.assertEqual(existing.call_args_list, [
            call("lector-ghostty", "1.2.4", "head"), call("lector-tts", "1.2.4", "head"),
            call("lector", "1.2.4", "head"),
        ])
        self.assertEqual(run.call_args_list, [
            call(["cargo", "publish", "--locked", "--registry", "crates-io", "-p", name], check=True)
            for name in ("lector-tts", "lector")
        ])

    @patch("publish_crates.registry_version")
    @patch("publish_crates.urlopen")
    def test_existing_version_must_come_from_the_same_checkout(self, download, entry):
        tarball = BytesIO()
        with tarfile.open(fileobj=tarball, mode="w:gz") as archive:
            data = json.dumps({"git": {"sha1": "a"}, "path_in_vcs": ""}).encode()
            member = tarfile.TarInfo("lector-1.2.4/.cargo_vcs_info.json")
            member.size = len(data)
            archive.addfile(member, BytesIO(data))
        payload = tarball.getvalue()
        entry.return_value = {"yanked": False, "cksum": hashlib.sha256(payload).hexdigest()}
        download.side_effect = [BytesIO(payload), BytesIO(payload)]
        self.assertTrue(already_uploaded("lector", "1.2.4", "a"))
        with self.assertRaisesRegex(SystemExit, "different checkout"):
            already_uploaded("lector", "1.2.4", "b")
