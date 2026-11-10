"""Exercise compiler-cache isolation through inert bootstrap inputs."""
import hashlib
import json
import os
from pathlib import Path
import re
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
                script.parent.mkdir(parents=True)
                lock = script.parent / ('lock.sh' if packaged else 'lib/lock.sh')
                lock.parent.mkdir(exist_ok=True)
                shutil.copy(ROOT / 'scripts/lib/lock.sh', lock)
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
                original = (ROOT / script_path).read_text()
                for version in ('10.0.0', '11.0.0'):
                    contents = original
                    for name, value in [('GHOSTTY_COMMIT', 'fixture-commit'),
                                        ('GHOSTTY_ARCHIVE_SHA256', digest),
                                        ('GHOSTTY_DEPS_SHA256', digest),
                                        ('REQUIRED_ZIG_VERSION', version)]:
                        contents = re.sub(rf'^{name}=.*$', f'{name}={value}', contents, flags=re.M)
                    script.write_text(contents)
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


if __name__ == '__main__':
    unittest.main()
