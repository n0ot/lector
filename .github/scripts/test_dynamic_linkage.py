"""Model ELF linkage states without invoking a system linker or executable."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class DynamicLinkageTests(unittest.TestCase):
    def check(self, interpreter, library):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifact = root / 'artifact'
            artifact.touch()
            reader = root / 'readelf'
            reader.write_text('''#!/usr/bin/env python3
import os, sys
if sys.argv[1] == '-l' and os.environ['FIXTURE_INTERPRETER']:
    print('Requesting program interpreter: /runtime/loader')
if sys.argv[1] == '-d' and os.environ['FIXTURE_LIBRARY']:
    print('(NEEDED) Shared library: [' + os.environ['FIXTURE_LIBRARY'] + ']')
''')
            reader.chmod(0o755)
            env = dict(os.environ, PATH=f"{root}:{os.environ['PATH']}",
                       FIXTURE_INTERPRETER=interpreter, FIXTURE_LIBRARY=library)
            return subprocess.run([str(ROOT / 'scripts/check-dynamic-libc'), str(artifact)],
                                  env=env, text=True, capture_output=True)

    def test_native_libc_names_are_accepted(self):
        for library in ('libc.so.6', 'libc.musl-fixture.so.1'):
            self.assertEqual(self.check('1', library).returncode, 0)

    def test_both_loader_and_shared_libc_are_required(self):
        for interpreter, library in [('', 'libc.so.6'), ('1', ''),
                                      ('1', 'libunrelated.so.1'), ('', '')]:
            self.assertNotEqual(self.check(interpreter, library).returncode, 0)


if __name__ == '__main__':
    unittest.main()
