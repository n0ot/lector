# Project guidance

## Bug fixes

Identify the general invariant or state transition exposed by a reported bug, and encode that invariant once at the lowest appropriate layer. Prefer evidence-based state models over checks for the particular command, application, key sequence, or protocol encoding in the report. Do not accumulate special cases merely because they reproduce the desired result.

When the available observations cannot distinguish two outcomes, choose a documented conservative behavior rather than guessing.

## Test isolation

Regression tests should exercise abstract scenarios with simulated inputs and state. Translate reproduction material supplied to agents into minimal, neutral dummy data that preserves only the relevant structure; do not copy user data, environment details, command output, configuration, or other incidental reproduction content into committed tests.

Prefer explicit byte sequences and modeled state over launching real programs. This keeps each test specific to the behavior under test and avoids external dependencies. Use project-built fixtures when process or platform behavior itself is the subject.

Use an external program only when Lector intentionally implements compatibility with that program or one of its concrete protocols, and make the test exercise that compatibility boundary directly. Run indispensable external integrations only inside Docker containers, never directly on the host, and keep ordinary test commands inert with respect to them. Platform-specific host tests may run only Lector and project-built fixtures. Express this policy through test structure rather than checks for particular names, paths, commands, or applications.
