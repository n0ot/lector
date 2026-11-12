# Lua-dispatch-base integration validation

Source `ee447ae59d2ece613b23fc2519131abb41cdb28a`, base
`7d3567ae19db52ddebf4e12d92a851f4630d6148`. All seven patches from
`3077cb0113c595b3398a9660f2b21155f84ccb4e` on `bfa905c` rebased unchanged
(`git range-diff` reports `=` throughout). Original reviewer2 approval of
`07dd179` on `9fbe4ec` remains applicable: intervening PR71 moves Lua option
conversion/dispatch while retaining reader defaults and invariant-enforcing
setters. These no-hooks benchmark readers invoke no Lua option callbacks.
No benchmark fixture, measurement fence or parser path changed. Statistical
baseline provenance remains source372454f/basefda95bd; prior validation reports
retain their original revisions.

Same commands and verified Ghostty archive as `split-scenes-81dfdc6.md`:
release locked build, strict release clippy, self-test and scoped suites passed.
The first live-PTY suite observed the existing startup-fixture cross-channel
race tracked in #67: only the first asynchronous speech log entry was present
after the synchronous hook marker. No code or timeout changed. The affected
test passed its bounded rerun (0.79s), then full live-PTY13, presentation111,
terminal112 and modeledtmux198 passed. Initial evidence is retained in
`/private/tmp/lector49-dispatch-tests.log`; follow-ups in
`/private/tmp/lector49-dispatch-pty-focused.log` and
`/private/tmp/lector49-dispatch-confirm.log`. Corroboration was added to #67.
Ordinary native audio/external integrations remained ignored.

The full release benchmark passed all old/additive gates in a coordinated quiet
window; raw report `split-scenes-ee447ae-gate.json`. No baseline values changed.

Library670/App207/core45 passed before the fixture observation race. The final
Docker harness passed private-terminfo1 and real-tmux11 on its first attempt for
this base (`/private/tmp/lector49-dispatch-docker.log`). The following commit
only adds this validation record and raw benchmark report.
