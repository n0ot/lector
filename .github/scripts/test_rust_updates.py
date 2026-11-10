"""Updater uses simulated GitHub responses; no remote writes or downloads."""

import base64
import os
import unittest
from unittest.mock import patch

import rust_updates as update


def encoded(text):
    return {"content": base64.b64encode(text.encode()).decode()}


class RustUpdateTests(unittest.TestCase):
    def setUp(self):
        self.before = '[toolchain]\nchannel = "1.2.3"\n'
        self.after = self.before.replace("1.2.3", "1.2.4")
        self.check = {"status": "completed", "conclusion": "success", "html_url": "https://example.test/run"}
        self.pr = {"number": 1, "body": "Closes #2", "html_url": "https://example.test/pr/1", "head": {"sha": "head"}}
        self.issue = {"number": 2, "node_id": "issue-id", "body": "<!-- rust-toolchain-update:1 -->", "html_url": "https://example.test/issue/2"}

    def manifest(self, release):
        return f'[pkg.rust]\nversion = "{release} (example 2026-01-01)"\n'

    def sync_reads(self):
        return [[{"ref": f"refs/heads/{update.BRANCH}", "object": {"sha": "head"}}],
                {"files": [{"filename": update.PIN}], "merge_base_commit": {"sha": "base"}},
                encoded(self.before), encoded(self.after), encoded(self.before)]

    def test_no_update_or_downgrade(self):
        self.assertIsNone(update.candidate(self.manifest("1.2.3"), "1.2.3"))
        self.assertIsNone(update.candidate(self.manifest("1.2.2"), "1.2.3"))

    def test_newer_release_changes_only_pin(self):
        self.assertEqual(update.candidate(self.manifest("1.2.4"), "1.2.3"), "1.2.4")
        self.assertEqual(update.replace_pin(self.before, "1.2.4"), self.after)
        with self.assertRaises(ValueError):
            update.candidate(self.manifest("nightly"), "1.2.3")

    def test_no_update_repeated_runs_do_not_mutate(self):
        with patch.object(update.Path, "read_text", return_value=self.before), patch.object(update, "run") as command:
            for _ in range(2):
                self.assertIsNone(update.propose("owner/repo", self.manifest("1.2.3")))
            command.assert_not_called()

    def test_prepared_current_branch_is_not_rewritten(self):
        with patch.object(update.Path, "read_text", return_value=self.before), \
                patch.object(update, "run", side_effect=["ref", "", "head", update.PIN, self.after.strip(), "base"]) as command, \
                patch.object(update.subprocess, "run") as ancestor:
            ancestor.return_value.returncode = 0
            self.assertEqual(update.propose("owner/repo", self.manifest("1.2.4")), "head")
            self.assertFalse(any("push" in call.args for call in command.call_args_list))

    def test_repeated_dispatch_reuses_current_validation(self):
        with patch.object(update, "check_run", return_value=self.check), patch.object(update, "api") as api:
            for _ in range(2):
                update.dispatch("owner/repo", "head")
            api.assert_not_called()

    def test_missing_ci_explicitly_dispatches(self):
        with patch.object(update, "check_run", return_value=None), patch.object(update, "api") as api:
            update.dispatch("owner/repo", "head")
            self.assertTrue(api.call_args.args[0].endswith("/dispatches"))
            self.assertEqual(api.call_args.kwargs, {"ref": update.BRANCH})

    def test_unavailable_dispatch_stops(self):
        with patch.object(update, "check_run", return_value=None), patch.object(update, "api", side_effect=RuntimeError("denied")):
            with self.assertRaisesRegex(RuntimeError, "denied"):
                update.dispatch("owner/repo", "head")

    def test_failed_validation_reports_and_keeps_tested_pin(self):
        self.check["conclusion"] = "failure"
        with patch.object(update, "check_run", return_value=self.check), patch.object(update, "api") as api:
            self.assertIn("Master retains its tested pin", update.dispatch("owner/repo", "head"))
            api.assert_not_called()

    def test_merged_branch_left_on_remote_is_a_noop(self):
        for status in ("behind", "identical"):
            responses = [self.sync_reads()[0], {"status": status, "files": []}]
            with patch.object(update, "api", side_effect=responses), patch.object(update, "open_pr") as pr:
                update.sync("owner/repo", "project")
                pr.assert_not_called()

    def test_sync_does_not_queue_failed_or_pending_validation(self):
        for check in (None, dict(self.check, conclusion="failure"), dict(self.check, status="in_progress")):
            with patch.object(update, "api", side_effect=self.sync_reads()), \
                    patch.object(update, "check_run", return_value=check), patch.object(update, "open_pr") as pr:
                update.sync("owner/repo", "project")
                pr.assert_not_called()

    def test_sync_reuses_existing_pr_and_issue(self):
        with patch.object(update, "api", side_effect=self.sync_reads() * 2) as api, \
                patch.object(update, "check_run", return_value=self.check), patch.object(update, "open_pr", return_value=self.pr), \
                patch.object(update, "gh", return_value=update.json.dumps([[self.issue]])), patch.object(update, "queue_issue") as queue:
            for _ in range(2):
                update.sync("owner/repo", "project")
            self.assertTrue(all(not call.kwargs for call in api.call_args_list))
            self.assertEqual(queue.call_count, 2)

    def test_sync_creates_one_linked_review_card_without_merging(self):
        pr = dict(self.pr, body="")
        responses = self.sync_reads() + [pr, self.issue]
        with patch.object(update, "api", side_effect=responses) as api, \
                patch.object(update, "check_run", return_value=self.check), patch.object(update, "open_pr", return_value=None), \
                patch.object(update, "gh", return_value="[[]]"), patch.object(update, "queue_issue") as queue, \
                patch.object(update, "run") as command:
            update.sync("owner/repo", "project")
            self.assertEqual(api.call_args.args[0], "repos/owner/repo/issues")
            self.assertIn("independent agent review", api.call_args.kwargs["body"])
            self.assertIn("Closes #2", command.call_args.kwargs["input"])
            queue.assert_called_once_with("project", "issue-id")

    def test_existing_queue_status_is_never_reset(self):
        with patch.object(update, "graphql", return_value={"addProjectV2ItemById": {"item": {
                "id": "item", "fieldValueByName": {"name": "In progress"}}}}) as graphql:
            update.queue_issue("project", "issue")
            self.assertEqual(graphql.call_count, 1)

    def test_unrelated_edits_are_not_queued(self):
        responses = self.sync_reads()
        responses[3] = encoded(self.after + 'profile = "minimal"\n')
        with patch.object(update, "api", side_effect=responses), patch.object(update, "open_pr") as pr:
            with self.assertRaisesRegex(RuntimeError, "beyond the canonical pin"):
                update.sync("owner/repo", "project")
            pr.assert_not_called()

    def test_missing_auth_fails_before_mutation(self):
        with patch.dict(os.environ, {"GH_TOKEN": "", "GITHUB_REPOSITORY": "owner/repo"}), \
                patch("sys.argv", ["rust_updates.py", "propose"]), patch.object(update, "api") as api:
            with self.assertRaisesRegex(RuntimeError, "GH_TOKEN requires"):
                update.main()
            api.assert_not_called()


if __name__ == "__main__":
    unittest.main()
