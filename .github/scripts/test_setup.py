"""Exercise one-time checkout setup with inert Rust tool fixtures."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[2]


class SetupTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / 'scripts').mkdir()
        (self.root / '.cargo').mkdir()
        shutil.copy(ROOT / 'scripts/setup', self.root / 'scripts/setup')
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        (self.bin / 'rustc').write_text('#!/bin/sh\nprintf "host: %s\\n" "$FIXTURE_HOST"\n')
        (self.bin / 'rustup').write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$FIXTURE_LOG"\nexit "${FIXTURE_FAILURE:-0}"\n')
        for tool in self.bin.iterdir():
            tool.chmod(0o755)
        self.config = self.root / '.cargo/lector-local.toml'
        self.log = self.root / 'rustup.log'

    def run_setup(self, host, failure='0'):
        env = dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}",
                   FIXTURE_HOST=host, FIXTURE_LOG=str(self.log), FIXTURE_FAILURE=failure)
        return subprocess.run([str(self.root / 'scripts/setup')], env=env,
                              capture_output=True, text=True)

    def test_linux_setup_is_repeatable_and_configures_nested_builds(self):
        for arch in ('aarch64', 'x86_64'):
            result = self.run_setup(f'{arch}-unknown-linux-gnu')
            self.assertEqual(result.returncode, 0, result.stderr)
            first = self.config.read_text()
            config = tomllib.loads(first)
            self.assertEqual(config['build']['target'], f'{arch}-unknown-linux-musl')
            self.assertEqual(config['env']['CARGO_BUILD_TARGET'], config['build']['target'])
            self.assertEqual(self.run_setup(f'{arch}-unknown-linux-gnu').returncode, 0)
            self.assertEqual(first, self.config.read_text())
            self.assertIn(f'target add {arch}-unknown-linux-musl', self.log.read_text())

    def test_macos_keeps_native_cargo_without_writing_configuration(self):
        self.assertEqual(self.run_setup('aarch64-apple-darwin').returncode, 0)
        self.assertFalse(self.config.exists())
        self.assertFalse(self.log.exists())

    def test_packaged_alpine_needs_no_rustup(self):
        self.assertEqual(self.run_setup('x86_64-alpine-linux-musl').returncode, 0)
        self.assertEqual(tomllib.loads(self.config.read_text())['build']['target'],
                         'x86_64-alpine-linux-musl')
        self.assertFalse(self.log.exists())

    def test_failed_target_install_does_not_commit_configuration(self):
        self.assertNotEqual(self.run_setup('aarch64-unknown-linux-gnu', '1').returncode, 0)
        self.assertFalse(self.config.exists())

    def test_manually_maintained_configuration_is_preserved(self):
        self.config.write_text('# custom configuration\n')
        result = self.run_setup('aarch64-unknown-linux-gnu')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.config.read_text(), '# custom configuration\n')

    def test_unsupported_host_does_not_write_configuration(self):
        self.assertNotEqual(self.run_setup('sample-unknown-other').returncode, 0)
        self.assertFalse(self.config.exists())
        self.assertFalse(self.log.exists())


if __name__ == '__main__':
    unittest.main()
