import tempfile
from pathlib import Path
import unittest

from cargo_cache_key import fingerprint


class CargoCacheKeyTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "Cargo.lock").write_text("dependencies-a")
        (self.root / ".cargo").mkdir()
        (self.root / ".cargo/config.toml").write_text("configuration-a")

    def key(self, compiler="compiler-a", target="native-target", lane="native-debug-release-all-features"):
        return fingerprint(self.root, compiler, target, lane)

    def test_same_inputs_reuse_key(self):
        self.assertEqual(self.key(), self.key())

    def test_compiler_target_and_lane_are_distinct(self):
        original = self.key()
        self.assertNotEqual(original, self.key(compiler="compiler-b"))
        self.assertNotEqual(original, self.key(target="cross-target"))
        self.assertNotEqual(original, self.key(lane="alpine-packaged-release-default"))

    def test_lockfile_and_configuration_invalidate(self):
        for name in ("Cargo.lock", ".cargo/config.toml"):
            original = self.key()
            (self.root / name).write_text("changed")
            self.assertNotEqual(original, self.key())

    def test_shared_bootstrap_inputs_invalidate(self):
        for name in ("bootstrap_ghostty.sh", "bootstrap_zig.sh", "lock.sh"):
            path = self.root / "crates/lector-ghostty/bootstrap" / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("baseline")
            original = self.key()
            path.write_text("changed")
            self.assertNotEqual(original, self.key())

    def test_workspace_sources_and_workflow_do_not_discard_dependencies(self):
        original = self.key()
        for name in ("src/main.rs", "crates/lector-tts/src/lib.rs",
                     "crates/lector-ghostty/src/lib.rs", "xtask/src/main.rs",
                     ".github/workflows/ghostty-build.yml", "docs/building.md"):
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("first revision")
            self.assertEqual(original, self.key(), name)
            path.write_text("another revision")
            self.assertEqual(original, self.key(), name)
            path.unlink()
            self.assertEqual(original, self.key(), name)

    def test_manifests_build_scripts_and_native_inputs_partition_fallbacks(self):
        for name in ("crates/sample/Cargo.toml", "crates/sample/build.rs",
                     "crates/lector-ghostty/abi/build_info_probe.c",
                     "crates/lector-tts/android/Bridge.java", "build_support/helper.rs",
                     "scripts/zig-cc", "scripts/cargo-musl"):
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            original = self.key()
            path.write_text("first revision")
            self.assertNotEqual(original, self.key(), name)
            before = self.key()
            path.write_text("changed")
            self.assertNotEqual(before, self.key(), name)
            path.unlink()
            self.assertEqual(original, self.key(), name)

    def test_build_outputs_never_affect_input_key(self):
        original = self.key()
        (self.root / "target").mkdir()
        (self.root / "target/generated").write_text("artifact")
        self.assertEqual(original, self.key())


if __name__ == "__main__":
    unittest.main()
