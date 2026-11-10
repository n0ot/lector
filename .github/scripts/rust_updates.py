"""Prepare tested Rust updates and hand them to an active agent queue for review."""

import base64
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib
import urllib.request

BRANCH = "automation/rust-toolchain"
PIN = "rust-toolchain.toml"
MANIFEST = "https://static.rust-lang.org/dist/channel-rust-stable.toml"


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
        return None
    old = ""
    if run("git", "ls-remote", "--heads", "origin", BRANCH):
        run("git", "fetch", "origin", BRANCH)
        old = run("git", "rev-parse", "FETCH_HEAD")
        if run("git", "diff", "--name-only", f"HEAD...{old}") != PIN:
            raise RuntimeError("Update branch contains other changes; refusing to overwrite")
        previous = run("git", "show", f"{old}:{PIN}") + "\n"
        expected = replace_pin(current_text, release)
        base = run("git", "rev-parse", "HEAD")
        ancestors = subprocess.run(["git", "merge-base", "--is-ancestor", base, old], check=False)
        if previous == expected and ancestors.returncode == 0:
            return old
        if replace_pin(previous, current) != current_text:
            raise RuntimeError("Update branch contains edits beyond the pin; refusing overwrite")
    run("git", "checkout", "-b", BRANCH)
    Path(PIN).write_text(replace_pin(current_text, release))
    run("git", "add", PIN)
    run("git", "-c", "user.name=github-actions[bot]", "-c",
        "user.email=41898282+github-actions[bot]@users.noreply.github.com",
        "commit", "-m", f"Update tested Rust pin to {release}")
    run("git", "push", f"--force-with-lease=refs/heads/{BRANCH}:{old}", "origin", f"HEAD:refs/heads/{BRANCH}")
    return run("git", "rev-parse", "HEAD")


def check_run(repo, head):
    runs = api(f"repos/{repo}/actions/workflows/ghostty-build.yml/runs?head_sha={head}&event=workflow_dispatch")
    return runs["workflow_runs"][0] if runs["workflow_runs"] else None


def dispatch(repo, head):
    check = check_run(repo, head)
    if check is None:
        # Explicit dispatch works with GITHUB_TOKEN; no PR creation permission needed.
        api(f"repos/{repo}/actions/workflows/ghostty-build.yml/dispatches", ref=BRANCH)
        return "Full CI dispatched. The active agent-queue session will create the review PR/card after validation."
    if check["status"] == "completed" and check["conclusion"] != "success":
        return f"Validation failed: {check['html_url']}. Master retains its tested pin. Inspect and rerun that workflow after resolving the failure."
    return f"Existing validation: {check['html_url']} ({check['status']})."


def graphql(query, **variables):
    return api("graphql", query=query, variables=variables)["data"]


def queue_issue(project, issue_id):
    result = graphql('''mutation($project:ID!,$issue:ID!){
      addProjectV2ItemById(input:{projectId:$project,contentId:$issue}){
        item{id fieldValueByName(name:"Status"){... on ProjectV2ItemFieldSingleSelectValue{name}}}}}''',
                     project=project, issue=issue_id)
    item = result["addProjectV2ItemById"]["item"]
    if item["fieldValueByName"]:
        return  # Never reset claimed, blocked, or deliberately planned work.
    fields = graphql('''query($project:ID!){node(id:$project){... on ProjectV2{
      fields(first:100){nodes{... on ProjectV2SingleSelectField{id name options{id name}}}}}}}''', project=project)
    status = next(field for field in fields["node"]["fields"]["nodes"] if field.get("name") == "Status")
    option = next(option["id"] for option in status["options"] if option["name"] == "Queued")
    graphql('''mutation($project:ID!,$item:ID!,$field:ID!,$option:String!){
      updateProjectV2ItemFieldValue(input:{projectId:$project,itemId:$item,fieldId:$field,
      value:{singleSelectOptionId:$option}}){projectV2Item{id}}}''',
            project=project, item=item["id"], field=status["id"], option=option)


def sync(repo, project):
    # Read-only until the prepared branch is a successful pin-only update.
    refs = api(f"repos/{repo}/git/matching-refs/heads/{BRANCH}")
    ref = next((ref for ref in refs if ref["ref"] == f"refs/heads/{BRANCH}"), None)
    if not ref:
        print("No prepared Rust update")
        return
    head = ref["object"]["sha"]
    comparison = api(f"repos/{repo}/compare/master...{head}")
    if [file["filename"] for file in comparison["files"]] != [PIN]:
        raise RuntimeError("Prepared update must only change the canonical pin")
    base_sha = comparison["merge_base_commit"]["sha"]
    before = base64.b64decode(api(f"repos/{repo}/contents/{PIN}?ref={base_sha}")["content"]).decode()
    after = base64.b64decode(api(f"repos/{repo}/contents/{PIN}?ref={head}")["content"]).decode()
    release = tomllib.loads(after)["toolchain"]["channel"]
    current = tomllib.loads(base64.b64decode(api(f"repos/{repo}/contents/{PIN}?ref=master")["content"]).decode())["toolchain"]["channel"]
    if version(release) <= version(current):
        print("Prepared update is already superseded by master")
        return
    if replace_pin(before, release) != after:
        raise RuntimeError("Prepared update contains edits beyond the canonical pin")
    check = check_run(repo, head)
    if not check or check["status"] != "completed" or check["conclusion"] != "success":
        print(f"Rust update is not validated: {check['html_url'] if check else 'CI not dispatched'}")
        return
    pr = open_pr(repo)
    if not pr:
        pr = api(f"repos/{repo}/pulls", title=f"Update tested Rust pin to {release}", head=BRANCH, base="master",
                 body=f"Prepared from {MANIFEST}. Validation: {check['html_url']}. Independent final-revision agent review and current protected CI are required before merge.")
    if pr["head"]["sha"] != head:
        raise RuntimeError("Prepared branch changed during synchronization; retry after its new CI")
    marker = f"<!-- rust-toolchain-update:{pr['number']} -->"
    pages = json.loads(gh("api", "--paginate", "--slurp", f"repos/{repo}/issues?state=open&per_page=100"))
    issue = next((issue for page in pages for issue in page
                  if "pull_request" not in issue and marker in (issue.get("body") or "")), None)
    if not issue:
        issue = api(f"repos/{repo}/issues", title="Review and merge the tested Rust pin update",
                    body=f"{marker}\nReview {pr['html_url']}. Keep scope limited to the canonical stable Rust pin. Confirm the official release, obtain independent agent review of the final SHA, update from master and pass every protected check before merging. The same authorized GitHub account may merge; no formal GitHub approval or separate reviewer account is required. Do not auto-approve or bypass protection. If validation fails, retain the tested master pin and report the blocker. This card is done only when that PR merges.")
    closing = f"Closes #{issue['number']}"
    if closing not in (pr.get("body") or ""):
        run("gh", "api", f"repos/{repo}/pulls/{pr['number']}", "--method", "PATCH", "--input", "-",
            input=json.dumps({"body": (pr.get("body") or "") + "\n\n" + closing}))
    queue_issue(project, issue["node_id"])
    print(f"Queued {issue['html_url']} for {pr['html_url']}")


def main():
    import argparse
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["propose", "sync", "report"])
    parser.add_argument("--project")
    args = parser.parse_args()
    repo = os.environ.get("GITHUB_REPOSITORY") or json.loads(gh("repo", "view", "--json", "nameWithOwner"))["nameWithOwner"]
    if args.mode == "sync":
        if not args.project:
            parser.error("sync requires --project")
        # Existing session gh authentication is used; no token or reviewer identity is invented.
        sync(repo, args.project)
        return
    if args.mode == "report":
        event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())["workflow_run"]
        message = (f"Rust update validation: {event['conclusion']}. {event['html_url']}. "
                   "Master retains its tested pin until independent agent review and protected merge. "
                   "If validation failed, inspect its logs and rerun after resolving the cause.")
        print(message)
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as summary:
            summary.write(message + "\n")
        return
    if not os.environ.get("GH_TOKEN"):
        raise RuntimeError("GH_TOKEN requires contents and actions write access; no PR approval permission is needed")
    with urllib.request.urlopen(MANIFEST, timeout=30) as response:
        manifest = response.read().decode()
    head = propose(repo, manifest)
    message = dispatch(repo, head) if head else "Canonical pin is current; no update."
    print(message)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as summary:
            summary.write(message + "\n")


if __name__ == "__main__":
    main()
