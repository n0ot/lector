"""Remove closed-PR caches and redundant Docker snapshots; dry-run by default."""

import argparse
from collections import defaultdict
import json
import os
import re
import subprocess


PR_REF = re.compile(r"refs/pull/([1-9][0-9]*)/merge")
SNAPSHOT = re.compile(r"(tmux-container-v1-[0-9a-f]{64})-[0-9a-f]{40}")


def api(path, method="GET"):
    result = subprocess.run(["gh", "api", "--method", method, path],
                            check=True, capture_output=True, text=True)
    return json.loads(result.stdout) if result.stdout.strip() else None


def inventory(repo):
    caches = []
    page = 1
    while True:
        batch = api(f"repos/{repo}/actions/caches?per_page=100&page={page}")["actions_caches"]
        caches.extend(batch)
        if len(batch) < 100:
            return caches
        page += 1


def deletion_plan(caches, pr_states, keep=2):
    if keep < 1:
        raise ValueError("At least one snapshot must be retained")
    deletions = []
    snapshots = defaultdict(list)
    for cache in caches:
        ref = cache["ref"]
        pr = PR_REF.fullmatch(ref)
        if pr and pr_states.get(int(pr[1])) == "closed":
            deletions.append((cache, "closed pull request"))
            continue
        # Preserve unknown refs, open/unknown PRs and non-Docker cache families.
        # Active PRs can need older snapshots while revisions run concurrently.
        match = SNAPSHOT.fullmatch(cache["key"])
        if ref.startswith("refs/heads/") and match:
            snapshots[(ref, cache["version"], match[1])].append(cache)
    for group in snapshots.values():
        ordered = sorted(group, key=lambda c: (c["created_at"], c["id"]), reverse=True)
        deletions.extend((cache, "older Docker snapshot") for cache in ordered[keep:])
    return sorted(deletions, key=lambda item: item[0]["id"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY"))
    parser.add_argument("--apply", action="store_true")
    args = parser.parse_args()
    if not args.repo or not re.fullmatch(r"[\w.-]+/[\w.-]+", args.repo):
        parser.error("--repo owner/name is required")
    caches = inventory(args.repo)
    numbers = {int(match[1]) for cache in caches
               if (match := PR_REF.fullmatch(cache["ref"]))}
    states = {n: api(f"repos/{args.repo}/pulls/{n}")["state"] for n in sorted(numbers)}
    plan = deletion_plan(caches, states)
    print(f"Inventory: {len(caches)} entries, "
          f"{sum(c['size_in_bytes'] for c in caches) / 1e9:.3f} GB")
    print(f"{'Apply' if args.apply else 'Dry run'}: {len(plan)} deletions, "
          f"{sum(c['size_in_bytes'] for c, _ in plan) / 1e9:.3f} GB")
    for cache, reason in plan:
        print(f"{cache['id']} {cache['ref']} {cache['key']}: {reason}")
        if args.apply:
            # Recheck closure in case the PR reopened after inventory collection.
            match = PR_REF.fullmatch(cache["ref"])
            if match and api(f"repos/{args.repo}/pulls/{match[1]}")["state"] != "closed":
                print("Preserved reopened pull request")
                continue
            # Delete only the inventoried ID, never all entries matching a key.
            try:
                api(f"repos/{args.repo}/actions/caches/{int(cache['id'])}", "DELETE")
            except subprocess.CalledProcessError as error:
                if "(HTTP 404)" not in error.stderr:
                    raise
                print("Already removed")


if __name__ == "__main__":
    main()
