import contextlib
import io
import unittest
from unittest.mock import patch

from cache_retention import deletion_plan, inventory, main


def cache(number, ref="refs/heads/master", identity="a", version="format-a"):
    return {"id": number, "ref": ref, "version": version,
            "key": f"tmux-container-v1-{identity * 64}-{number:040x}",
            "created_at": f"2026-01-{number:02d}T00:00:00Z", "size_in_bytes": 100}


class CacheRetentionTests(unittest.TestCase):
    def test_only_confirmed_closed_prs_are_removed(self):
        entries = [cache(1, "refs/pull/11/merge"), cache(2, "refs/pull/12/merge"),
                   cache(3, "refs/pull/13/merge"), cache(4, "refs/tags/release")]
        # Closed PR cleanup covers every cache family, not only Docker snapshots.
        entries[0]["key"] = "dependency-cache"
        self.assertEqual(deletion_plan(entries, {11: "closed", 12: "open"}),
                         [(entries[0], "closed pull request")])

    def test_two_newest_snapshots_per_branch_identity_and_format_survive(self):
        entries = [cache(i) for i in range(1, 5)]
        entries += [cache(i, "refs/heads/feature") for i in range(5, 8)]
        entries += [cache(8, identity="b"), cache(9, version="format-b")]
        entries += [cache(i, "refs/pull/11/merge") for i in range(10, 14)]
        ordinary = cache(14)
        ordinary["key"] = "ghostty-native-artifact"
        entries.append(ordinary)
        self.assertEqual([c["id"] for c, _ in deletion_plan(entries, {11: "open"})],
                         [1, 2, 5])
        # Enumeration and access order do not determine which snapshots survive.
        self.assertEqual(deletion_plan(entries[::-1], {11: "open"}),
                         deletion_plan(entries, {11: "open"}))

    def test_unknown_keys_and_refs_survive_and_zero_retention_is_rejected(self):
        entry = cache(1)
        entry["key"] += "-unknown-suffix"
        self.assertEqual(deletion_plan([entry], {}), [])
        with self.assertRaises(ValueError):
            deletion_plan([], {}, keep=0)

    def test_inventory_reads_all_pages(self):
        with patch("cache_retention.api", side_effect=[
                {"actions_caches": [cache(1)] * 100}, {"actions_caches": [cache(2)]}]) as api:
            self.assertEqual(len(inventory("example/project")), 101)
            self.assertIn("page=2", api.call_args.args[0])

    def run_main(self, apply, reopened=False):
        entry = cache(1, "refs/pull/11/merge")
        replies = [{"state": "closed"}]
        if apply:
            replies.append({"state": "open" if reopened else "closed"})
            if not reopened:
                replies.append(None)
        with patch("cache_retention.inventory", return_value=[entry]), \
                patch("cache_retention.api", side_effect=replies) as api, \
                patch("sys.argv", ["cache_retention.py", "--repo", "example/project"]
                      + (["--apply"] if apply else [])), \
                contextlib.redirect_stdout(io.StringIO()):
            main()
            return api.call_args_list

    def test_default_is_read_only(self):
        self.assertEqual(len(self.run_main(False)), 1)

    def test_apply_deletes_only_the_inventoried_id_and_rechecks_closure(self):
        calls = self.run_main(True)
        self.assertEqual(calls[-1].args,
                         ("repos/example/project/actions/caches/1", "DELETE"))
        self.assertEqual(len(self.run_main(True, reopened=True)), 2)


if __name__ == "__main__":
    unittest.main()
