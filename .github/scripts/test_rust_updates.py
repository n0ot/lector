"""Updater decisions use simulated GitHub responses; no remote writes or downloads."""

import os
import unittest
from unittest.mock import patch

import rust_updates as update


class RustUpdateTests(unittest.TestCase):
    def setUp(self):
        self.validation = patch.object(update, "validate_pr")
        self.validation.start()
        self.addCleanup(self.validation.stop)
        self.pr = {"number": 1, "user": {"login": "author"}, "head": {"sha": "head"}}
        self.run = {"status": "completed", "conclusion": "success", "html_url": "https://example.test/run"}

    def manifest(self, release):
        return f'[pkg.rust]\nversion = "{release} (example 2026-01-01)"\n'

    def test_no_update_or_downgrade(self):
        self.assertIsNone(update.candidate(self.manifest("1.2.3"), "1.2.3"))
        self.assertIsNone(update.candidate(self.manifest("1.2.2"), "1.2.3"))

    def test_newer_release_changes_only_pin(self):
        self.assertEqual(update.candidate(self.manifest("1.3.0"), "1.2.3"), "1.3.0")
        before = '[toolchain]\nchannel = "1.2.3"\nprofile = "minimal"\n'
        self.assertEqual(update.replace_pin(before, "1.3.0"), before.replace("1.2.3", "1.3.0"))
        with self.assertRaises(ValueError):
            update.candidate(self.manifest("nightly"), "1.2.3")

    def test_current_release_repeated_runs_do_not_mutate(self):
        with patch.object(update.Path, "read_text", return_value='[toolchain]\nchannel = "1.2.3"\n'), \
                patch.object(update, "open_pr", return_value=None), patch.object(update, "run") as command:
            for _ in range(2):
                self.assertIsNone(update.propose("owner/repo", self.manifest("1.2.3")))
            command.assert_not_called()

    def test_existing_current_revision_is_not_redispatched(self):
        with patch.object(update, "api", return_value={"workflow_runs": [{"status": "in_progress"}]}) as api, \
                patch.object(update, "report") as report:
            for _ in range(2):
                update.reconcile("owner/repo", self.pr, "reviewer")
            self.assertEqual(api.call_count, 2)
            self.assertTrue(all(not call.kwargs for call in api.call_args_list))
            report.assert_not_called()

    def test_missing_ci_explicitly_dispatches_instead_of_trusting_pr_event(self):
        with patch.object(update, "api", side_effect=[{"workflow_runs": []}, None]) as api, \
                patch.object(update, "report"):
            update.reconcile("owner/repo", self.pr, "reviewer")
            self.assertEqual(api.call_args.kwargs, {"ref": update.BRANCH})
            self.assertTrue(api.call_args.args[0].endswith("/dispatches"))

    def test_unavailable_dispatch_stops_without_merge(self):
        with patch.object(update, "api", side_effect=[{"workflow_runs": []}, RuntimeError("denied")]), \
                patch.object(update, "gh") as gh:
            with self.assertRaisesRegex(RuntimeError, "denied"):
                update.reconcile("owner/repo", self.pr, "reviewer")
            gh.assert_not_called()

    def test_failed_validation_reports_and_keeps_tested_pin(self):
        self.run["conclusion"] = "failure"
        with patch.object(update, "api", return_value={"workflow_runs": [self.run]}), \
                patch.object(update, "report") as report, patch.object(update, "gh") as gh:
            update.reconcile("owner/repo", self.pr, "reviewer")
            self.assertIn("Master retains its tested pin", report.call_args.args[2])
            gh.assert_not_called()

    def test_missing_reviewer_blocks_merge_after_ci(self):
        with patch.object(update, "api", return_value={"workflow_runs": [self.run]}), \
                patch.object(update, "report") as report, patch.object(update, "gh") as gh:
            update.reconcile("owner/repo", self.pr, "")
            self.assertIn("RUST_UPDATE_REVIEWER", report.call_args.args[2])
            gh.assert_not_called()

    def test_review_must_be_independent_current_and_not_superseded(self):
        review = {"user": {"login": "reviewer"}, "state": "APPROVED", "commit_id": "head"}
        self.assertTrue(update.approved([review], "reviewer", "author", "head"))
        self.assertFalse(update.approved([review], "reviewer", "reviewer", "head"))
        self.assertFalse(update.approved([review], "reviewer", "author", "new-head"))
        for state in ("CHANGES_REQUESTED", "DISMISSED"):
            self.assertFalse(update.approved([review, dict(review, state=state)], "reviewer", "author", "head"))

    def test_reviewed_validated_pin_uses_protected_exact_head_merge(self):
        self.pr["base"] = {"sha": "base"}
        review = {"user": {"login": "reviewer"}, "state": "APPROVED", "commit_id": "head"}
        responses = [{"workflow_runs": [self.run]}]
        with patch.object(update, "api", side_effect=responses), patch.object(update, "gh", side_effect=[
                update.json.dumps([[review]]),
                update.json.dumps({"headRefOid": "head", "mergeStateStatus": "CLEAN"}), ""]) as gh:
            update.reconcile("owner/repo", self.pr, "reviewer")
            self.assertEqual(gh.call_args.args, ("pr", "merge", "1", "--repo", "owner/repo",
                             "--squash", "--delete-branch", "--match-head-commit", "head"))

    def test_pin_validation_rejects_unrelated_edits_before_dispatch(self):
        import base64
        self.validation.stop()
        self.pr["base"] = {"sha": "base"}
        before = '[toolchain]\nchannel = "1.2.3"\n'
        after = before.replace("1.2.3", "1.2.4") + 'profile = "minimal"\n'
        responses = [[{"filename": update.PIN}],
                     {"content": base64.b64encode(before.encode()).decode()},
                     {"content": base64.b64encode(after.encode()).decode()}]
        with patch.object(update, "api", side_effect=responses) as api:
            with self.assertRaisesRegex(RuntimeError, "only advance"):
                update.reconcile("owner/repo", self.pr, "reviewer")
            self.assertTrue(all(not call.kwargs for call in api.call_args_list))

    def test_missing_auth_fails_before_network_or_repository_mutation(self):
        with patch.dict(os.environ, {"GH_TOKEN": ""}), patch.object(update, "api") as api:
            with self.assertRaisesRegex(RuntimeError, "GH_TOKEN is required"):
                update.main()
            api.assert_not_called()


if __name__ == "__main__":
    unittest.main()
