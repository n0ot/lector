"""Check compiler identity failures without downloading or invoking a toolchain."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class RustToolchainTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        repo = Path(__file__).resolve().parents[2]
        (self.root / "scripts").mkdir()
        shutil.copy(repo / "scripts/check-rust-toolchain", self.root / "scripts")
        (self.root / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "1.2.3"\n')
        self.compiler = self.root / "compiler"
        self.compiler.write_text(
            '#!/bin/sh\n'
            'if [ "$1" = -vV ]; then\n'
            '  printf "release: %s\\nhost: %s\\n" "$TEST_RELEASE" "$TEST_HOST"\n'
            'else printf "%s\\n" "$TEST_LIBDIR"; fi\n'
        )
        self.compiler.chmod(0o755)
        self.env = dict(os.environ, RUSTC=str(self.compiler), TEST_RELEASE="1.2.3",
                        TEST_HOST="x86_64-unknown-linux-gnu", TEST_LIBDIR=str(self.root / "lib"))

    def check(self, *args):
        return subprocess.run([str(self.root / "scripts/check-rust-toolchain"), *args],
                              env=self.env, capture_output=True, text=True)

    def test_accepts_canonical_compiler_and_native_host(self):
        self.assertEqual(self.check("--host", self.env["TEST_HOST"]).returncode, 0)

    def test_rejects_overridden_compiler(self):
        self.env["TEST_RELEASE"] = "9.9.9"
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Compiler mismatch", result.stderr)

    def test_rejects_wrong_host_even_in_packaged_lane(self):
        self.assertNotEqual(self.check("--packaged", "--host", "other-host").returncode, 0)

    def test_packaged_lane_explicitly_reports_version_exception(self):
        self.env["TEST_RELEASE"] = "9.9.9"
        result = self.check("--packaged", "--host", self.env["TEST_HOST"])
        self.assertEqual(result.returncode, 0)
        self.assertIn("Distribution-packaged compiler", result.stdout)

    def test_cross_target_requires_installed_standard_library(self):
        self.assertNotEqual(self.check("--target", "example-target").returncode, 0)
        libdir = Path(self.env["TEST_LIBDIR"])
        libdir.mkdir()
        (libdir / "libcore-example.rlib").touch()
        self.assertEqual(self.check("--target", "example-target").returncode, 0)


if __name__ == "__main__":
    unittest.main()
