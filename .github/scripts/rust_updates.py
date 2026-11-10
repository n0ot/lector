"""Maintain one Rust update PR; only a configured independent review can unlock merge."""

import base64
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib
import urllib.request

BRANCH = "automation/rust-toolchain"
PIN = "rust-toolchain.toml"
MANIFEST = "https://static.rust-lang.org/dist/channel-rust-stable.toml"
MARKER = "<!-- rust-toolchain-status -->"


def run(*args, **kwargs):
    return subprocess.run(args, check=True, text=True, capture_output=True, **kwargs).stdout.strip()


def gh(*args):
    return run("gh", *args)


def api(path, **fields):
    args = ["api", path]
    if fields:
        args += ["--method", "POST", "--input", "-"]
        return json.loads(run("gh", *args, input=json.dumps(fields)) or "null")
    return json.loads(gh(*args))


def version(value):
    if not re.fullmatch(r"\d+\.\d+\.\d+", value):
        raise ValueError(f"Expected an exact stable Rust release, got {value!r}")
    return tuple(map(int, value.split(".")))


def candidate(manifest, current):
    release = tomllib.loads(manifest)["pkg"]["rust"]["version"].split()[0]
    return release if version(release) > version(current) else None


def replace_pin(text, release):
    version(release)
    result, count = re.subn(r'^channel = "[0-9]+\.[0-9]+\.[0-9]+"$',
                            f'channel = "{release}"', text, flags=re.MULTILINE)
    if count != 1:
        raise ValueError("Canonical pin must contain exactly one exact channel")
    return result


def report(repo, pr, message):
    body = f"{MARKER}\n{message}"
    comments = api(f"repos/{repo}/issues/{pr['number']}/comments?per_page=100")
    existing = next((c for c in comments if c["body"].startswith(MARKER)
                     and c["user"]["login"] == pr["user"]["login"]), None)
    if existing:
        if existing["body"] != body:
            run("gh", "api", f"repos/{repo}/issues/comments/{existing['id']}",
                "--method", "PATCH", "--input", "-", input=json.dumps({"body": body}))
    else:
        api(f"repos/{repo}/issues/{pr['number']}/comments", body=body)


def open_pr(repo):
    owner = repo.split("/")[0]
    prs = api(f"repos/{repo}/pulls?state=open&head={owner}:{BRANCH}&base=master")
    if len(prs) > 1:
        raise RuntimeError("Multiple update PRs found; refusing ambiguous ownership")
    return prs[0] if prs else None


def propose(repo, manifest):
    current_text = Path(PIN).read_text()
    current = tomllib.loads(current_text)["toolchain"]["channel"]
    release = candidate(manifest, current)
    if release is None:
        print(f"Rust {current} is current; no update")
        return open_pr(repo)
    pr = open_pr(repo)
    # Rebuild only the owned branch, and only if its diff is still our one pin.
    if pr:
        files = api(f"repos/{repo}/pulls/{pr['number']}/files")
        if [f["filename"] for f in files] != [PIN]:
            raise RuntimeError("Update branch contains other changes; refusing to overwrite")
        run("git", "fetch", "origin", BRANCH)
        old = run("git", "rev-parse", "FETCH_HEAD")
        if old != pr["head"]["sha"]:
            raise RuntimeError("Update branch changed during inspection; retry later")
        previous = run("git", "show", f"{old}:{PIN}") + "\n"
        expected = replace_pin(current_text, release)
        base = run("git", "rev-parse", "HEAD")
        ancestors = subprocess.run(["git", "merge-base", "--is-ancestor", base, old], check=False)
        if previous == expected and ancestors.returncode == 0:
            return pr
        if replace_pin(previous, current) != current_text:
            raise RuntimeError("Update branch contains edits beyond the pin; refusing overwrite")
    else:
        # An orphan branch is not silently adopted or overwritten.
        if run("git", "ls-remote", "--heads", "origin", BRANCH):
            raise RuntimeError("Orphan update branch exists; inspect it before retrying")
        old = ""
    run("git", "checkout", "-b", BRANCH)
    Path(PIN).write_text(replace_pin(current_text, release))
    run("git", "add", PIN)
    run("git", "-c", "user.name=github-actions[bot]", "-c",
        "user.email=41898282+github-actions[bot]@users.noreply.github.com",
        "commit", "-m", f"Update tested Rust pin to {release}")
    run("git", "push", f"--force-with-lease=refs/heads/{BRANCH}:{old}", "origin", f"HEAD:refs/heads/{BRANCH}")
    title = f"Update tested Rust pin to {release}"
    body = (f"Update only the canonical Rust pin to {release} from {MANIFEST}. "
            "The last tested master pin remains unchanged until full protected CI and "
            "independent review of this exact revision pass.")
    if pr:
        run("gh", "api", f"repos/{repo}/pulls/{pr['number']}", "--method", "PATCH",
            "--input", "-", input=json.dumps({"title": title, "body": body}))
    else:
        api(f"repos/{repo}/pulls", title=title, body=body, head=BRANCH, base="master")
    return open_pr(repo)


def approved(reviews, reviewer, author, head):
    if not reviewer or reviewer == author:
        return False
    relevant = [r for r in reviews if r["user"]["login"] == reviewer
                and r["state"] in ("APPROVED", "CHANGES_REQUESTED", "DISMISSED")]
    return bool(relevant and relevant[-1]["state"] == "APPROVED"
                and relevant[-1]["commit_id"] == head)


def reconcile(repo, pr, reviewer):
    head = pr["head"]["sha"]
    runs = api(f"repos/{repo}/actions/workflows/ghostty-build.yml/runs?head_sha={head}&event=workflow_dispatch")
    if not runs["workflow_runs"]:
        # This explicit event is supported with GITHUB_TOKEN, unlike implicit PR CI.
        api(f"repos/{repo}/actions/workflows/ghostty-build.yml/dispatches", ref=BRANCH)
        report(repo, pr, "Full CI explicitly dispatched for this revision; waiting for results and independent review.")
        return
    check = runs["workflow_runs"][0]
    if check["status"] != "completed":
        return
    if check["conclusion"] != "success":
        report(repo, pr, f"Validation failed: {check['html_url']}. Master retains its tested pin. "
               "Inspect the failure and rerun that workflow after fixing infrastructure, or update this PR.")
        return
    if not reviewer:
        report(repo, pr, "CI passed, but automated merge is blocked: configure RUST_UPDATE_REVIEWER "
               "with an independently operated reviewer app/account that submits a GitHub APPROVED review "
               "on this exact head. No reviewer integration is bundled or implicitly provisioned.")
        return
    pages = json.loads(gh("api", "--paginate", "--slurp",
                          f"repos/{repo}/pulls/{pr['number']}/reviews?per_page=100"))
    reviews = [review for page in pages for review in page]
    if not approved(reviews, reviewer, pr["user"]["login"], head):
        report(repo, pr, f"CI passed; awaiting independent review by @{reviewer} on `{head}`. "
               "A stale approval, author approval, or changes-requested review cannot authorize merge.")
        return
    # Refuse a foreign branch, arbitrary edits, or a changed head before asking GitHub to merge.
    files = api(f"repos/{repo}/pulls/{pr['number']}/files")
    if [f["filename"] for f in files] != [PIN]:
        raise RuntimeError("Reviewed update is no longer limited to the canonical pin")
    base_pin = api(f"repos/{repo}/contents/{PIN}?ref={pr['base']['sha']}")
    head_pin = api(f"repos/{repo}/contents/{PIN}?ref={head}")
    before = base64.b64decode(base_pin["content"]).decode()
    after = base64.b64decode(head_pin["content"]).decode()
    release = tomllib.loads(after)["toolchain"]["channel"]
    previous = tomllib.loads(before)["toolchain"]["channel"]
    if version(release) <= version(previous) or replace_pin(before, release) != after:
        raise RuntimeError("Update must only advance the exact canonical Rust release")
    state = json.loads(gh("pr", "view", str(pr["number"]), "--repo", repo,
                          "--json", "headRefOid,mergeStateStatus"))
    if state["headRefOid"] != head or state["mergeStateStatus"] != "CLEAN":
        report(repo, pr, "Waiting for an up-to-date, mergeable branch. The next scheduled run refreshes the base; "
               "a new revision requires fresh CI and review.")
        return
    gh("pr", "merge", str(pr["number"]), "--repo", repo, "--squash", "--delete-branch",
       "--match-head-commit", head)


def main():
    if not os.environ.get("GH_TOKEN"):
        raise RuntimeError("GH_TOKEN is required with contents, pull-requests and actions write access")
    repo = os.environ["GITHUB_REPOSITORY"]
    pr = None
    try:
        if sys.argv[1] == "propose":
            with urllib.request.urlopen(MANIFEST, timeout=30) as response:
                manifest = response.read().decode()
            pr = propose(repo, manifest)
        else:
            pr = open_pr(repo)
        if pr:
            reconcile(repo, pr, os.environ.get("RUST_UPDATE_REVIEWER", ""))
    except (RuntimeError, ValueError, subprocess.CalledProcessError) as error:
        detail = getattr(error, "stderr", "") or str(error)
        if pr:
            report(repo, pr, f"Rust updater stopped safely: {detail[:1500]}\nMaster was not modified. "
                   "Check workflow permissions, PR creation policy, and explicit CI dispatch access.")
        raise


if __name__ == "__main__":
    main()
