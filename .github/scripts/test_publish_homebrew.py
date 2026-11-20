from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

from publish_homebrew import render_formula, update_formula


CHECKSUMS = """1111111111111111111111111111111111111111111111111111111111111111  lector-aarch64-apple-darwin.tar.gz
2222222222222222222222222222222222222222222222222222222222222222  lector-x86_64-apple-darwin.tar.gz
3333333333333333333333333333333333333333333333333333333333333333  lector-aarch64-unknown-linux-musl.tar.gz
4444444444444444444444444444444444444444444444444444444444444444  lector-x86_64-unknown-linux-musl.tar.gz
5555555555555555555555555555555555555555555555555555555555555555  lector-x86_64-unknown-linux-gnu.tar.gz
"""


class HomebrewPublicationTests(unittest.TestCase):
    def test_formula_uses_versioned_archives_and_their_checksums(self):
        formula = render_formula("1.2.3", CHECKSUMS)
        for line in CHECKSUMS.splitlines()[:-1]:
            digest, name = line.split()
            self.assertIn(f'/download/v1.2.3/{name}"\n      sha256 "{digest}"', formula)
        self.assertNotIn("linux-gnu", formula)
        self.assertIn('branch: "master"', formula)

    def test_rejects_incomplete_or_ambiguous_checksums_and_prereleases(self):
        with self.assertRaisesRegex(ValueError, "Missing checksums"):
            render_formula("1.2.3", "")
        with self.assertRaisesRegex(ValueError, "duplicate checksum"):
            render_formula("1.2.3", CHECKSUMS * 2)
        with self.assertRaisesRegex(ValueError, "Expected a stable Lector version"):
            render_formula("1.2.3-rc.1", CHECKSUMS)

    def test_old_job_cannot_roll_back_formula(self):
        with TemporaryDirectory() as directory:
            formula = Path(directory) / "Formula/lector.rb"
            update_formula(formula, "1.10.0", CHECKSUMS)
            update_formula(formula, "1.9.0", CHECKSUMS)
            self.assertEqual(formula.read_text(), render_formula("1.10.0", CHECKSUMS))
