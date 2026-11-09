import subprocess
import unittest
from unittest.mock import patch

from ci_policy import BUILD_JOBS, changed_paths, ci_passed, requires_build


class ChangePolicyTests(unittest.TestCase):
    def test_markdown_only(self):
        self.assertFalse(requires_build([b"guide.md", b"docs/topic with spaces.md"]))

    def test_code_mixed_or_unknown_diff_requires_build(self):
        for paths in ([], [b"src/lib.rs"], [b"guide.md", b"Cargo.lock"],
                      [b".github/workflows/check.yml"], [b"script.sh\n.md", b"script.sh"]):
            with self.subTest(paths=paths):
                self.assertTrue(requires_build(paths))

    def test_diff_keeps_both_sides_of_renames_and_unusual_names(self):
        with patch("ci_policy.subprocess.run") as run:
            run.return_value.stdout = b"src/old.rs\0docs/new.md\0docs/line\nbreak.md\0"
            paths = changed_paths("abc123")
            self.assertIn("--no-renames", run.call_args.args[0])
            self.assertEqual(paths[-1], b"docs/line\nbreak.md")
            self.assertTrue(requires_build(paths))

    def test_unavailable_base_requires_build(self):
        for base in ("", "0" * 40):
            self.assertTrue(requires_build(changed_paths(base)))
        with patch("ci_policy.subprocess.run", side_effect=subprocess.CalledProcessError(1, "git")):
            self.assertTrue(requires_build(changed_paths("abc123")))


class GateTests(unittest.TestCase):
    def needs(self, build="true", result="success"):
        return {"changes": {"result": "success", "outputs": {"build": build}},
                **{job: {"result": result} for job in BUILD_JOBS}}

    def test_successful_build_and_documentation_exemption(self):
        self.assertTrue(ci_passed(self.needs()))
        self.assertTrue(ci_passed(self.needs("false", "success")))

    def test_each_unsuccessful_build_blocks_merge(self):
        for job in BUILD_JOBS:
            for result in ("failure", "cancelled", "skipped", None):
                needs = self.needs()
                needs[job]["result"] = result
                self.assertFalse(ci_passed(needs))

    def test_failed_or_missing_detection_blocks_merge(self):
        self.assertFalse(ci_passed({}))
        self.assertFalse(ci_passed(self.needs("")))
        for result in ("failure", "cancelled", "skipped"):
            needs = self.needs("false", "success")
            needs["changes"]["result"] = result
            self.assertFalse(ci_passed(needs))


if __name__ == "__main__":
    unittest.main()
