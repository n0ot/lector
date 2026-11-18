import hashlib
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

from publish_release import TARGETS, publish, release_assets, release_tag


class ReleaseTests(unittest.TestCase):
    def test_unchanged_version_uses_commit_prerelease(self):
        self.assertEqual(release_tag("abcd", "0.4.1", "0.4.1"), ("build-abcd", True))

    def test_increased_version_uses_stable_tag(self):
        self.assertEqual(release_tag("abcd", "0.4.9", "0.4.10"), ("v0.4.10", False))

    def test_downgrade_does_not_publish(self):
        with self.assertRaises(SystemExit):
            release_tag("abcd", "0.5.0", "0.4.99")

    def test_release_requires_every_target_and_hashes_archives(self):
        with TemporaryDirectory() as tmp:
            directory = Path(tmp)
            with self.assertRaises(SystemExit):
                release_assets(directory)
            for target in TARGETS:
                (directory / f"lector-{target}.tar.gz").write_bytes(target.encode())
            assets = release_assets(directory)
            checksums = [
                f"{hashlib.sha256(asset.read_bytes()).hexdigest()}  {asset.name}"
                for asset in assets[:-1]
            ]
            self.assertEqual(assets[-1].read_text().splitlines(), checksums)

    @patch("publish_release.subprocess.run")
    @patch("publish_release.tag_commit", return_value="other")
    def test_existing_tag_for_another_commit_stops_before_upload(self, _tag_commit, run):
        with self.assertRaisesRegex(SystemExit, "already points"):
            publish("abcd", "0.4.1", "0.4.2", [])
        run.assert_not_called()

    @patch("publish_release.subprocess.run")
    @patch("publish_release.tag_commit", side_effect=["", "previous"])
    def test_new_stable_draft_targets_the_built_commit(self, _tag_commit, run):
        run.return_value = subprocess.CompletedProcess([], 1, "", "release not found\n")
        publish("abcd", "0.4.1", "0.4.2", [Path("dist/lector.tar.gz")])
        self.assertEqual(
            run.call_args_list[1].args[0],
            [
                "gh", "release", "create", "v0.4.2", "--draft", "--target", "abcd",
                "--title", "v0.4.2", "--generate-notes", "--notes-start-tag", "v0.4.1",
            ],
        )
        self.assertEqual(run.call_args.args[0], ["gh", "release", "edit", "v0.4.2", "--draft=false"])

    @patch("publish_release.subprocess.run")
    @patch("publish_release.tag_commit", return_value="")
    def test_interrupted_draft_is_uploaded_before_publishing(self, _tag_commit, run):
        run.return_value = subprocess.CompletedProcess(
            [], 0, '{"isDraft":true,"isPrerelease":true,"targetCommitish":"abcd"}',
        )
        archive = Path("dist/lector.tar.gz")
        publish("abcd", "0.4.1", "0.4.1", [archive])
        self.assertEqual(
            [call.args[0] for call in run.call_args_list[1:]],
            [
                ["gh", "release", "upload", "build-abcd", archive, "--clobber"],
                ["gh", "release", "edit", "build-abcd", "--draft=false", "--latest=false"],
            ],
        )
