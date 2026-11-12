"""Exercise compiler-cache isolation through inert bootstrap inputs."""
import hashlib
import json
import os
from pathlib import Path
import re
import select
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class GhosttyCacheTests(unittest.TestCase):
    def test_compiler_versions_isolate_both_caches_and_preserve_old_state(self):
        for packaged in (False, True):
            with self.subTest(packaged=packaged), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                script_path = ('crates/lector-ghostty/bootstrap/bootstrap_ghostty.sh'
                               if packaged else 'scripts/bootstrap_ghostty.sh')
                script = root / script_path
                implementation = root / 'crates/lector-ghostty/bootstrap/bootstrap_ghostty.sh'
                shutil.copytree(ROOT / 'crates/lector-ghostty/bootstrap', implementation.parent)
                if not packaged:
                    script.parent.mkdir(parents=True)
                    shutil.copy(ROOT / script_path, script)
                probe = root / 'crates/lector-ghostty/abi/build_info_probe.c'
                probe.parent.mkdir(parents=True)
                probe.write_text('/* fixture */\n')
                build_root = root / 'target'
                source_parent = build_root / 'ghostty-source'
                source = source_parent / 'fixture-commit'
                source.mkdir(parents=True)
                (source / '.lector-ghostty-source').write_text('fixture-commit\n')
                (source / 'LIB_VERSION').write_text('1.0.0\n')
                (source_parent / 'libghostty-vt-1.0.0-deps').mkdir()
                for name in ('libghostty-vt-1.0.0.tar.gz', 'libghostty-vt-1.0.0-deps.tar.gz'):
                    (source_parent / name).write_bytes(b'fixture archive')
                digest = hashlib.sha256(b'fixture archive').hexdigest()
                legacy = build_root / 'ghostty-zig-cache/global/sentinel'
                legacy.parent.mkdir(parents=True)
                legacy.write_text('keep')
                binaries = root / 'bin'
                binaries.mkdir()
                (binaries / 'rustc').write_text('#!/bin/sh\necho "host: aarch64-apple-darwin"\n')
                (binaries / 'zig').write_text('''#!/usr/bin/env python3
import json, os, sys
if sys.argv[1] == 'version':
    print(os.environ['FIXTURE_VERSION'])
else:
    with open(os.environ['FIXTURE_CALLS'], 'a') as log:
        log.write(json.dumps([os.environ['ZIG_GLOBAL_CACHE_DIR'],
                             sys.argv[sys.argv.index('--cache-dir') + 1]]) + '\\n')
    sys.exit(73)
''')
                for binary in binaries.iterdir():
                    binary.chmod(0o755)
                calls = root / 'calls'
                env = dict(os.environ, PATH=f'{binaries}:{os.environ["PATH"]}',
                           LECTOR_GHOSTTY_BUILD_ROOT=str(build_root), FIXTURE_CALLS=str(calls))
                env.pop('GHOSTTY_PREBUILT_ROOT', None)
                original = (ROOT / 'crates/lector-ghostty/bootstrap/bootstrap_ghostty.sh').read_text()
                for version in ('10.0.0', '11.0.0'):
                    contents = original
                    for name, value in [('GHOSTTY_COMMIT', 'fixture-commit'),
                                        ('GHOSTTY_ARCHIVE_SHA256', digest),
                                        ('GHOSTTY_DEPS_SHA256', digest),
                                        ('REQUIRED_ZIG_VERSION', version)]:
                        contents = re.sub(rf'^{name}=.*$', f'{name}={value}', contents, flags=re.M)
                    implementation.write_text(contents)
                    result = subprocess.run(['bash', str(script), '--optimize', 'ReleaseFast'],
                                            env=dict(env, FIXTURE_VERSION=version),
                                            text=True, capture_output=True)
                    self.assertEqual(result.returncode, 73, result.stderr)
                first, second = [json.loads(line) for line in calls.read_text().splitlines()]
                for old, new in zip(first, second):
                    self.assertNotEqual(old, new)
                    self.assertTrue(Path(old).is_dir())
                    self.assertTrue(Path(new).is_dir())
                    self.assertNotEqual(Path(old), legacy.parent)
                self.assertEqual(legacy.read_text(), 'keep')


class BootstrapEntryPointTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='bootstrap paths with spaces ')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.crate = self.root / 'crates/lector-ghostty'
        shutil.copytree(ROOT / 'crates/lector-ghostty/bootstrap', self.crate / 'bootstrap')
        shutil.copytree(ROOT / 'crates/lector-ghostty/abi', self.crate / 'abi')
        shutil.copytree(ROOT / 'scripts', self.root / 'scripts')
        self.bin = self.root / 'fake tools'
        self.bin.mkdir()
        for name, body in {
            'rustc': 'echo "host: aarch64-apple-darwin"',
            'uname': 'case "$1" in -s) echo Linux;; -m) echo x86_64;; esac',
            'curl': 'echo "unexpected download" >&2; exit 91',
            'zig': 'exit 92',
        }.items():
            binary = self.bin / name
            binary.write_text('#!/bin/sh\n' + body + '\n')
            binary.chmod(0o755)
        self.env = {key: value for key, value in os.environ.items()
                    if key not in ('LECTOR_GHOSTTY_BUILD_ROOT', 'GHOSTTY_PREBUILT_ROOT',
                                   'LECTOR_TOOLCHAIN_ROOT')}
        self.env['PATH'] = f'{self.bin}:{os.environ["PATH"]}'

    def script(self, packaged, name='ghostty'):
        directory = self.crate / 'bootstrap' if packaged else self.root / 'scripts'
        return directory / f'bootstrap_{name}.sh'

    def run_script(self, packaged, args=(), env=None, name='ghostty'):
        return subprocess.run(['bash', str(self.script(packaged, name)), *args],
                              env=self.env | (env or {}), text=True,
                              capture_output=True, timeout=10)

    def prebuilt(self, root):
        prefix = root / 'aarch64-apple-darwin/ReleaseFast'
        archive = prefix / 'static-lib/libghostty-vt.a'
        archive.parent.mkdir(parents=True)
        archive.write_bytes(b'neutral prebuilt fixture')
        script = self.script(True).read_text()
        pin = lambda key: re.search(rf'^{key}=(.*)$', script, re.M)[1]
        values = dict(ghostty_commit=pin('GHOSTTY_COMMIT'),
                      ghostty_version=pin('GHOSTTY_VERSION'),
                      zig_version=pin('REQUIRED_ZIG_VERSION'),
                      target='aarch64-apple-darwin', optimize='ReleaseFast',
                      app_runtime='none', emit_lib_vt='true', kitty_graphics='true',
                      abi_header_check='passed',
                      abi_probe_sha256=hashlib.sha256(
                          (self.crate / 'abi/build_info_probe.c').read_bytes()).hexdigest(),
                      archive_sha256=hashlib.sha256(archive.read_bytes()).hexdigest())
        (prefix / 'lector-ghostty-build.txt').write_text(
            ''.join(f'{key}={value}\n' for key, value in values.items()))
        return archive

    def test_arguments_and_exit_status(self):
        for packaged in (False, True):
            for args in (['--target'], ['--target', '--optimize'], ['--optimize'],
                         ['--unknown'], ['--target', 'unsupported'],
                         ['--optimize', 'unsupported']):
                with self.subTest(packaged=packaged, args=args):
                    result = self.run_script(packaged, args)
                    self.assertEqual(result.returncode, 2, result.stderr)

    def test_default_roots_and_prebuilt_override(self):
        for packaged in (False, True):
            build = (self.crate if packaged else self.root) / 'target'
            archive = self.prebuilt(build / 'ghostty-prebuilt')
            result = self.run_script(packaged, ['--optimize', 'ReleaseFast'])
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(str(archive), result.stderr)
            override = self.root / f'prebuilt override {packaged}'
            archive = self.prebuilt(override)
            result = self.run_script(packaged, ['--optimize', 'ReleaseFast'],
                                     {'GHOSTTY_PREBUILT_ROOT': str(override)})
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(str(archive), result.stderr)

    def read_ready_line(self, stream):
        self.assertTrue(select.select([stream], [], [], 10)[0],
                        'bootstrap fixture did not reach the expected state')
        return stream.readline()

    def test_shared_build_override_serializes_both_entry_points(self):
        build = self.root / 'shared build override'
        self.prebuilt(build / 'ghostty-prebuilt')
        lock = build / 'bootstrap-locks/ghostty'
        holder = subprocess.Popen(['bash', '-c',
            'source "$1"; lector_acquire_lock "$2"; echo ready; read -r release; '
            'lector_release_lock "$2"', 'fixture',
            str(self.crate / 'bootstrap/lock.sh'), str(lock)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        self.addCleanup(lambda: holder.poll() is None and holder.kill())
        self.assertEqual(self.read_ready_line(holder.stdout).strip(), 'ready')
        processes = []
        for packaged in (False, True):
            process = subprocess.Popen(['bash', str(self.script(packaged)),
                                        '--optimize', 'ReleaseFast'],
                env=self.env | {'LECTOR_GHOSTTY_BUILD_ROOT': str(build)},
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            self.addCleanup(lambda p=process: p.poll() is None and p.kill())
            processes.append(process)
            self.assertIn('waiting for Ghostty bootstrap lock', self.read_ready_line(process.stderr))
        holder.communicate('release\n', timeout=10)
        for process in processes:
            _, error = process.communicate(timeout=10)
            self.assertEqual(process.returncode, 0, error)
            self.assertIn('reusing verified Ghostty archive', error)
        self.assertFalse(lock.exists())

    def test_zig_default_and_override_roots_reuse_verified_toolchain(self):
        for packaged in (False, True):
            default = (self.crate if packaged else self.root) / 'target/toolchains'
            for toolchains, env in ((default, {}),
                    (self.root / 'toolchain override',
                     {'LECTOR_TOOLCHAIN_ROOT': str(self.root / 'toolchain override')}),
                    (self.root / 'build override/toolchains',
                     {'LECTOR_GHOSTTY_BUILD_ROOT': str(self.root / 'build override')})):
                install = toolchains / 'zig/0.17.0'
                install.mkdir(parents=True, exist_ok=True)
                zig = install / 'zig'
                zig.write_text('#!/bin/sh\necho 0.17.0\n')
                zig.chmod(0o755)
                script = self.script(True, 'zig').read_text()
                checksum = re.search(r'Linux:x86_64\).*?archive_sha=(\w+)', script, re.S)[1]
                (install / '.lector-zig-toolchain').write_text(
                    'version=0.17.0\nplatform=x86_64-linux\narchive_sha256=' + checksum + '\n')
                result = self.run_script(packaged, env=env, name='zig')
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(Path(result.stdout.strip()).resolve(), zig.resolve())

    def test_tampered_archive_never_reuses_metadata(self):
        for packaged in (False, True):
            build = self.root / f'corrupt cache {packaged}'
            archive = self.prebuilt(build / 'ghostty-prebuilt')
            archive.write_bytes(b'changed fixture')
            result = self.run_script(packaged, ['--optimize', 'ReleaseFast'],
                                     {'LECTOR_GHOSTTY_BUILD_ROOT': str(build)})
            self.assertNotEqual(result.returncode, 0)
            self.assertNotIn('reusing verified Ghostty archive', result.stderr)


if __name__ == '__main__':
    unittest.main()
