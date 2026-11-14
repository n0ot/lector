"""Decide when builds are needed and validate the complete CI result."""

import json
import os
import subprocess
import sys

BUILD_JOBS = ("macos", "linux-musl", "alpine-static")


def requires_build(paths):
    # An empty or unknown diff must not exempt a revision from testing.
    return not paths or any(not path.endswith(b".md") for path in paths)


def changed_paths(base):
    if not base or set(base) == {"0"}:
        return []
    try:
        result = subprocess.run(
            ["git", "diff", "--no-renames", "--name-only", "-z", base, "HEAD", "--"],
            check=True, capture_output=True,
        )
    except subprocess.CalledProcessError:
        return []
    return [path for path in result.stdout.split(b"\0") if path]


def ci_passed(needs):
    changes = needs.get("changes", {})
    if changes.get("result") != "success":
        return False
    build = changes.get("outputs", {}).get("build")
    if build not in ("true", "false"):
        return False
    return all(needs.get(job, {}).get("result") == "success" for job in BUILD_JOBS)


def main():
    if sys.argv[1] == "changes":
        build = requires_build(changed_paths(os.environ.get("BASE_SHA", "")))
        value = str(build).lower()
        with open(os.environ["GITHUB_OUTPUT"], "a") as output:
            output.write(f"build={value}\n")
        print("Full build required" if build else "Markdown-only change: builds unnecessary")
    elif sys.argv[1] == "gate":
        needs = json.loads(os.environ["NEEDS_JSON"])
        if not ci_passed(needs):
            print(json.dumps(needs, indent=2))
            sys.exit("Required CI jobs did not complete as expected")
        print("All required CI jobs passed (or were unnecessary for Markdown-only changes)")
    else:
        sys.exit("usage: ci_policy.py changes|gate")


if __name__ == "__main__":
    main()
