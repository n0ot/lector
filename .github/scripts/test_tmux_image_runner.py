"""Exercise runner selection with an inert Docker command fixture."""
import json
import os
from pathlib import Path
import subprocess
import shutil
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
IMAGE = "sha256:" + "a" * 64


class TmuxImageRunnerTests(unittest.TestCase):
    def run_runner(self, **settings):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            docker = root / "docker"
            docker.write_text("""#!/usr/bin/env python3
import json, os, sys
with open(os.environ['DOCKER_CALLS'], 'a') as log:
    log.write(json.dumps(sys.argv[1:]) + '\\n')
if os.environ.get('FAIL_PREPARE') and 'list' in sys.argv:
    sys.exit(1)
if sys.argv[1:3] == ['image', 'inspect']:
    if os.environ.get('MISSING_IMAGE'):
        sys.exit(1)
    print(os.environ.get('IMAGE_PLATFORM', 'linux/amd64'))
""")
            docker.chmod(0o755)
            calls = root / "calls"
            env = {key: value for key, value in os.environ.items()
                   if not key.startswith('LECTOR_REAL_TMUX_') and key not in ('LECTOR_LINUX_FULL_SUITE', 'LECTOR_TEST_STATIC')}
            env.update(PATH=f"{root}:{env['PATH']}", DOCKER_CALLS=str(calls), **settings)
            result = subprocess.run([str(ROOT / 'scripts/test-real-tmux-docker')],
                                    env=env, text=True, capture_output=True)
            return result, [json.loads(line) for line in calls.read_text().splitlines()] if calls.exists() else []

    def test_fresh_checkout_prepares_nested_target_mountpoint(self):
        with tempfile.TemporaryDirectory() as directory:
            checkout = Path(directory)
            script = checkout / 'scripts/test-real-tmux-docker'
            script.parent.mkdir()
            shutil.copy(ROOT / 'scripts/test-real-tmux-docker', script)
            self.assertFalse((checkout / 'target').exists())
            with patch(__name__ + '.ROOT', checkout):
                result, _ = self.run_runner(LECTOR_REAL_TMUX_PLATFORM='linux/amd64',
                                            LECTOR_REAL_TMUX_PREBUILT_IMAGE=IMAGE)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue((checkout / 'target').is_dir())

    def test_default_build_and_prebuilt_run_identical_test_commands(self):
        normal, normal_calls = self.run_runner(LECTOR_REAL_TMUX_PLATFORM='linux/amd64')
        prebuilt, prebuilt_calls = self.run_runner(
            LECTOR_REAL_TMUX_PLATFORM='linux/amd64', LECTOR_REAL_TMUX_PREBUILT_IMAGE=IMAGE)
        self.assertEqual(normal.returncode, 0, normal.stderr)
        self.assertEqual(prebuilt.returncode, 0, prebuilt.stderr)
        self.assertEqual(normal_calls[0][0], 'build')
        self.assertFalse(any(call[0] == 'build' for call in prebuilt_calls))
        self.assertEqual(len(normal_calls), 5)
        self.assertEqual(len(prebuilt_calls), 5)
        for original, reused in zip(normal_calls[1:], prebuilt_calls[1:]):
            self.assertEqual(original[0], 'run')
            self.assertIn('--pull', reused)
            self.assertEqual(reused[reused.index('--pull') + 1], 'never')
            index = reused.index(IMAGE)
            self.assertEqual(original[:index], reused[:index])
            self.assertEqual(original[index + 1:], reused[index + 1:])

    def test_bind_cache_preserves_commands_and_prepare_does_not_run_tests(self):
        with tempfile.TemporaryDirectory() as directory:
            result, calls = self.run_runner(
                LECTOR_REAL_TMUX_CACHE_DIR=directory, LECTOR_REAL_TMUX_PREBUILT_IMAGE=IMAGE,
                LECTOR_REAL_TMUX_PLATFORM='linux/amd64', LECTOR_REAL_TMUX_PREPARE_ONLY='1')
            self.assertEqual(result.returncode, 0, result.stderr)
            runs = calls[1:]
            for call in runs:
                self.assertIn('/work/target/test-tmux:mode=1777', call)
                self.assertIn(directory + '/target:/work/target', call)
                self.assertIn(directory + '/cargo-registry:/opt/cargo/registry', call)
                self.assertIn(directory + '/cargo-git:/opt/cargo/git', call)
                self.assertNotIn('lector-real-tmux-target:/work/target', call)
            commands = [call[call.index(IMAGE) + 1:] for call in runs]
            self.assertIn(['bash', 'scripts/bootstrap_zig.sh'], commands)
            self.assertIn(['cargo', 'ghostty-bootstrap', '--optimize', 'ReleaseFast'], commands)
            self.assertEqual(sum(command[:3] == ['cargo', 'nextest', 'list'] for command in commands), 2)
            self.assertFalse(any(command[:3] == ['cargo', 'nextest', 'run'] for command in commands))
            failed, failed_calls = self.run_runner(
                LECTOR_REAL_TMUX_CACHE_DIR=directory, LECTOR_REAL_TMUX_PREBUILT_IMAGE=IMAGE,
                LECTOR_REAL_TMUX_PLATFORM='linux/amd64', LECTOR_REAL_TMUX_PREPARE_ONLY='1',
                FAIL_PREPARE='1')
            self.assertNotEqual(failed.returncode, 0)
            self.assertFalse(any('run' in call[call.index(IMAGE) + 1:] for call in failed_calls[1:]))

    def test_invalid_identity_or_missing_platform_never_runs(self):
        for image, platform in [('mutable:tag', 'linux/amd64'), (IMAGE, '')]:
            with self.subTest(image=image, platform=platform):
                result, calls = self.run_runner(LECTOR_REAL_TMUX_PREBUILT_IMAGE=image,
                                               LECTOR_REAL_TMUX_PLATFORM=platform)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(calls, [])

    def test_static_compatibility_requires_explicit_selection(self):
        result, calls = self.run_runner(LECTOR_TEST_STATIC='1')
        self.assertEqual(result.returncode, 0, result.stderr)
        commands = [call[call.index('lector-real-tmux:ubuntu-24.04') + 1:]
                    for call in calls if call[0] == 'run']
        self.assertEqual(sum(command[:3] == ['scripts/cargo-musl', 'nextest', 'run']
                             for command in commands), 2)

    def test_wrong_platform_or_missing_local_image_never_runs_or_builds(self):
        for extra in [{'IMAGE_PLATFORM': 'linux/arm64'}, {'MISSING_IMAGE': '1'}]:
            with self.subTest(extra=extra):
                result, calls = self.run_runner(LECTOR_REAL_TMUX_PREBUILT_IMAGE=IMAGE,
                                               LECTOR_REAL_TMUX_PLATFORM='linux/amd64', **extra)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(len(calls), 1)
                self.assertEqual(calls[0][:2], ['image', 'inspect'])


if __name__ == '__main__':
    unittest.main()
