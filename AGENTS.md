# Project guidance

## Pull requests and completion

Make changes on a branch and submit a pull request targeting `master`. Do not push directly to `master` or bypass branch protection, including with administrator privileges.

Before independent review, successfully run `cargo build -r --locked` locally and the relevant end-to-end tests, including host Lector/project-fixture tests and Docker-only external integrations where applicable. Resolve local failures and rerun affected checks before review. Blocked local validation means the work is incomplete; passing CI is not a substitute for this local gate. Preserve the test isolation policy below.

Work is complete only when all required GitHub CI checks pass for the latest pull request revision and the branch is up to date with `master`. Local checks alone are not sufficient. After pushing changes, monitor CI and address failures before considering the work done or merging. If CI cannot finish or a failure cannot be resolved, report the work as incomplete and explain the blocker.

The required `CI` check validates all build jobs. Markdown-only changes run the CI policy checks and skip compilation; mixed changes and uncertain diffs require the full matrix. CI runs for pull requests and pushes to `master`, and reuses verified Ghostty and Zig artifacts when their inputs match.

When changing CI job names or coverage, keep the `CI` gate and GitHub branch protection aligned with the workflow so every required job remains enforced.

## Bug fixes

Identify the general invariant or state transition exposed by a reported bug, and encode that invariant once at the lowest appropriate layer. Prefer evidence-based state models over checks for the particular command, application, key sequence, or protocol encoding in the report. Do not accumulate special cases merely because they reproduce the desired result.

When the available observations cannot distinguish two outcomes, choose a documented conservative behavior rather than guessing.

## Test isolation

Regression tests should exercise abstract scenarios with simulated inputs and state. Translate reproduction material supplied to agents into minimal, neutral dummy data that preserves only the relevant structure; do not copy user data, environment details, command output, configuration, or other incidental reproduction content into committed tests.

Prefer explicit byte sequences and modeled state over launching real programs. This keeps each test specific to the behavior under test and avoids external dependencies. Use project-built fixtures when process or platform behavior itself is the subject.

Use an external program only when Lector intentionally implements compatibility with that program or one of its concrete protocols, and make the test exercise that compatibility boundary directly. Run indispensable external integrations only inside Docker containers, never directly on the host, and keep ordinary test commands inert with respect to them. Platform-specific host tests may run only Lector and project-built fixtures. Express this policy through test structure rather than checks for particular names, paths, commands, or applications.

## Agent queue

When orchestrating this repository's queue, run `python3 .github/scripts/rust_updates.py sync --project <PROJECT_NODE_ID>` before dispatch and at the existing 30-minute checkpoints. This imports only validated canonical Rust updates as review/merge cards. Use the normal independent-agent final-revision review and protected merge process; a separate GitHub reviewer account is not required. A stopped session leaves prepared updates waiting.
