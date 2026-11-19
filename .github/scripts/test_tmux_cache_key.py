import tempfile
from pathlib import Path
import unittest

from tmux_cache_key import cargo_fingerprint, ghostty_fingerprint


GHOSTTY_INPUTS = ['build_support/ghostty.rs', 'scripts/lib/lock.sh',
                  'crates/lector-ghostty/build.rs',
                  'crates/lector-ghostty/abi/build_info_probe.c',
                  'crates/lector-ghostty/bootstrap/bootstrap_ghostty.sh',
                  'crates/lector-ghostty/bootstrap/bootstrap_zig.sh',
                  'crates/lector-ghostty/bootstrap/lock.sh',
                  'scripts/bootstrap_ghostty.sh', 'scripts/bootstrap_zig.sh',
                  '.github/scripts/tmux_cache_key.py']

CARGO_ONLY_INPUTS = ['Cargo.lock', 'rust-toolchain.toml', 'Cargo.toml',
                     'crates/lector-ghostty/Cargo.toml', '.cargo/config.toml',
                     'build_support/eh_frames.c',
                     'tests/docker/real-tmux.Dockerfile',
                     'tests/docker/install-nextest.sh',
                     'scripts/test-real-tmux-docker', 'scripts/check-rust-toolchain',
                     # .cargo/config.toml selects this workspace compiler wrapper;
                     # its musl branch selects these linker helpers.
                     'scripts/rustc-static-linux', 'scripts/cargo-musl',
                     'scripts/zig-cc', 'scripts/zig-lld']

SOURCES = ['src/lib.rs', 'crates/lector-ghostty/src/lib.rs',
           'crates/lector-ghostty/src/ffi.rs', 'tests/suite_tmux.rs']


def populate(root, names):
    for name in names:
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text('baseline')


class ContainerCacheKeyTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        populate(self.root, GHOSTTY_INPUTS + CARGO_ONLY_INPUTS + SOURCES)
        self.identity = 'image-id\nlinux/amd64\nrustc 1.2.3\nconfiguration'

    def tearDown(self):
        self.directory.cleanup()

    def keys(self, identity=None):
        identity = identity or self.identity
        return (ghostty_fingerprint(self.root, identity),
                cargo_fingerprint(self.root, identity))

    def changed(self, name):
        path = self.root / name
        path.write_text('changed')
        try:
            return self.keys()
        finally:
            path.write_text('baseline')

    def test_families_are_distinct(self):
        ghostty, cargo = self.keys()
        self.assertTrue(ghostty.startswith('tmux-ghostty-v1-'))
        self.assertTrue(cargo.startswith('tmux-container-v2-'))

    def test_identity_invalidates_both(self):
        ghostty, cargo = self.keys()
        for new_identity in [self.identity + '\ncompiler-change',
                             self.identity.replace('amd64', 'arm64'),
                             self.identity.replace('configuration', 'different-flags')]:
            new_ghostty, new_cargo = self.keys(new_identity)
            self.assertNotEqual(ghostty, new_ghostty)
            self.assertNotEqual(cargo, new_cargo)

    def test_ghostty_inputs_invalidate_both(self):
        ghostty, cargo = self.keys()
        for name in GHOSTTY_INPUTS:
            with self.subTest(name=name):
                new_ghostty, new_cargo = self.changed(name)
                self.assertNotEqual(ghostty, new_ghostty)
                self.assertNotEqual(cargo, new_cargo)

    def test_cargo_inputs_keep_the_ghostty_archive(self):
        ghostty, cargo = self.keys()
        for name in CARGO_ONLY_INPUTS:
            with self.subTest(name=name):
                new_ghostty, new_cargo = self.changed(name)
                self.assertEqual(ghostty, new_ghostty)
                self.assertNotEqual(cargo, new_cargo)

    def test_source_edits_keep_both_prefixes(self):
        # Cargo fingerprints source edits; the archive never depends on them.
        baseline = self.keys()
        for name in SOURCES:
            with self.subTest(name=name):
                self.assertEqual(baseline, self.changed(name))

    def test_host_output_and_unrelated_runners_are_ignored(self):
        baseline = self.keys()
        populate(self.root, ['target/Cargo.toml', 'target/ghostty-prebuilt/build.rs',
                             'scripts/report-summary'])
        self.assertEqual(baseline, self.keys())
        (self.root / 'scripts/report-summary').write_text('changed unrelated runner')
        self.assertEqual(baseline, self.keys())


if __name__ == '__main__':
    unittest.main()
