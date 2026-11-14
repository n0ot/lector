"""Check executable ABI selection with inert compiler and Cargo fixtures."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class MuslCargoTests(unittest.TestCase):
    def run_helper(self, host, args=(), target=None):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            compiler = root / "rustc"
            compiler.write_text("#!/usr/bin/env python3\nimport os\nprint('host: ' + os.environ['FIXTURE_HOST'])\n")
            cargo = root / "cargo"
            cargo.write_text("""#!/usr/bin/env python3
import json, os, sys
print(json.dumps({'args': sys.argv[1:], 'target': os.environ['CARGO_BUILD_TARGET'],
                  'zig': os.environ.get('LECTOR_ZIG_CC_TARGET'),
                  'cc': os.environ.get('CC_' + os.environ['CARGO_BUILD_TARGET'].replace('-', '_'))}))
""")
            compiler.chmod(0o755)
            cargo.chmod(0o755)
            env = {key: value for key, value in os.environ.items()
                   if key not in ('CARGO_BUILD_TARGET', 'LECTOR_ZIG_CC_TARGET')
                   and not key.startswith('CC_')}
            env.update(PATH=f"{root}:{env['PATH']}", FIXTURE_HOST=host)
            if target is not None:
                env['CARGO_BUILD_TARGET'] = target
            return subprocess.run([str(ROOT / 'scripts/cargo-musl'), *args],
                                  env=env, text=True, capture_output=True)

    def test_host_libc_does_not_select_executable_libc(self):
        for arch in ('aarch64', 'x86_64'):
            with self.subTest(arch=arch):
                result = self.run_helper(f'{arch}-unknown-linux-gnu', ['build', '--locked'])
                self.assertEqual(result.returncode, 0, result.stderr)
                call = json.loads(result.stdout)
                self.assertEqual(call['target'], f'{arch}-unknown-linux-musl')
                self.assertEqual(call['zig'], f'{arch}-linux-musl')
                self.assertEqual(call['cc'], str(ROOT / 'scripts/zig-cc'))
                self.assertEqual(call['args'], ['build', '--locked'])

    def test_explicit_target_controls_native_dependencies_and_nested_cargo(self):
        for args, target in [(['build', '--target', 'aarch64-unknown-linux-musl'], None),
                             (['build', '--target=aarch64-unknown-linux-musl'], None),
                             (['ghostty-check'], 'aarch64-unknown-linux-musl')]:
            result = self.run_helper('x86_64-unknown-linux-gnu', args, target)
            self.assertEqual(result.returncode, 0, result.stderr)
            call = json.loads(result.stdout)
            self.assertEqual(call['target'], 'aarch64-unknown-linux-musl')
            self.assertEqual(call['zig'], 'aarch64-linux-musl')

    def test_program_arguments_cannot_change_build_target(self):
        result = self.run_helper('x86_64-unknown-linux-gnu',
                                 ['run', '--', '--target', 'application-option'])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)['target'], 'x86_64-unknown-linux-musl')

    def test_native_packaged_musl_preserves_distribution_linker(self):
        result = self.run_helper('x86_64-alpine-linux-musl', ['build'])
        self.assertEqual(result.returncode, 0, result.stderr)
        call = json.loads(result.stdout)
        self.assertEqual(call['target'], 'x86_64-alpine-linux-musl')
        self.assertIsNone(call['zig'])
        self.assertIsNone(call['cc'])

    def test_unsupported_abi_host_or_incomplete_target_never_calls_cargo(self):
        for host, args, target in [('x86_64-unknown-linux-gnu', ['build'], 'x86_64-unknown-linux-gnu'),
                                    ('x86_64-unknown-linux-gnu', ['build', '--target'], None),
                                    ('aarch64-apple-darwin', ['build'], None)]:
            result = self.run_helper(host, args, target)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(result.stdout, '')


class WorkspaceLinkerTests(unittest.TestCase):
    def test_musl_tests_and_binaries_use_the_same_linker_without_changing_host_tools(self):
        with tempfile.TemporaryDirectory() as directory:
            compiler = Path(directory) / 'compiler'
            compiler.write_text('#!/usr/bin/env python3\nimport json, sys\nprint(json.dumps(sys.argv[1:]))\n')
            compiler.chmod(0o755)
            env = {key: value for key, value in os.environ.items()
                   if key != 'LECTOR_ZIG_CC_TARGET'}
            for target in ('aarch64-unknown-linux-musl', 'aarch64-unknown-linux-gnu'):
                for kind in (['--test'], ['--crate-type', 'bin']):
                    args = ['--crate-name', 'sample', '--target', target, *kind]
                    result = subprocess.run([str(ROOT / 'scripts/rustc-static-linux'),
                                             str(compiler), *args], env=env,
                                            text=True, capture_output=True, check=True)
                    forwarded = json.loads(result.stdout)
                    self.assertEqual(forwarded[:len(args)], args)
                    if target.endswith('musl'):
                        self.assertIn('linker=' + str(ROOT / 'scripts/zig-lld'), forwarded)
                        self.assertIn('target-feature=+crt-static', forwarded)
                    else:
                        self.assertEqual(forwarded, args)


if __name__ == '__main__':
    unittest.main()
