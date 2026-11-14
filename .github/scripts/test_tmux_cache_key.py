import tempfile
from pathlib import Path
import unittest

from tmux_cache_key import fingerprint


class ContainerCacheKeyTests(unittest.TestCase):
    def test_compiler_platform_lock_and_configuration_invalidate(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            files = ['Cargo.lock', 'rust-toolchain.toml', 'Cargo.toml',
                     '.cargo/config.toml', 'build_support/ghostty.rs', 'scripts/lib/lock.sh',
                     'crates/lector-ghostty/build.rs',
                     'crates/lector-ghostty/bootstrap/bootstrap_ghostty.sh',
                     'crates/lector-ghostty/bootstrap/bootstrap_zig.sh',
                     'crates/lector-ghostty/bootstrap/lock.sh',
                     'tests/docker/real-tmux.Dockerfile',
                     'tests/docker/install-nextest.sh',
                     '.github/scripts/tmux_cache_key.py',
                     'scripts/test-real-tmux-docker', 'scripts/check-rust-toolchain',
                     'scripts/bootstrap_ghostty.sh', 'scripts/bootstrap_zig.sh',
                     # .cargo/config.toml selects this workspace compiler wrapper;
                     # its musl branch selects these linker helpers.
                     'scripts/rustc-static-linux', 'scripts/cargo-musl',
                     'scripts/zig-cc', 'scripts/zig-lld']
            for name in files:
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text('baseline')
            identity = 'image-id\nlinux/amd64\nrustc 1.2.3\nconfiguration'
            baseline = fingerprint(root, identity)
            for new_identity in [identity + '\ncompiler-change', identity.replace('amd64', 'arm64'),
                                 identity.replace('configuration', 'different-flags')]:
                self.assertNotEqual(baseline, fingerprint(root, new_identity))
            for name in files:
                with self.subTest(name=name):
                    path = root / name
                    path.write_text('changed')
                    self.assertNotEqual(baseline, fingerprint(root, identity))
                    path.write_text('baseline')
            (root / 'target').mkdir()
            (root / 'target/Cargo.toml').write_text('unrelated host output')
            self.assertEqual(baseline, fingerprint(root, identity))
            (root / 'src').mkdir()
            (root / 'src/lib.rs').write_text('source edit checked by Cargo fingerprints')
            self.assertEqual(baseline, fingerprint(root, identity))

    def test_unrelated_runner_changes_do_not_discard_container_artifacts(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'Cargo.lock').write_text('lock')
            (root / 'rust-toolchain.toml').write_text('compiler')
            (root / 'scripts').mkdir()
            baseline = fingerprint(root, 'container and compiler identity')
            runner = root / 'scripts/report-summary'
            runner.write_text('first version')
            self.assertEqual(baseline, fingerprint(root, 'container and compiler identity'))
            runner.write_text('changed unrelated runner')
            self.assertEqual(baseline, fingerprint(root, 'container and compiler identity'))
            runner.unlink()
            self.assertEqual(baseline, fingerprint(root, 'container and compiler identity'))


if __name__ == '__main__':
    unittest.main()
