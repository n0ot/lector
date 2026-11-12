Independent reviewer2 approved head `07dd1794f92289c6e77e3dd833ca9386c00c78c8` on base `9fbe4ec85dd148700cde132d2f6b2da58e26a73a`, with no blockers after baseline and measurement-fence audit.

Rebased cleanly to `0493cddb9c2d61dd8cd3b9323014257b881177fd` on `bfa905cc9623b9ac0e2f57827e1292b7b8ae1f68`. `git range-diff 9fbe4ec..07dd179 bfa905c..0493cdd` marks all six patches equivalent. Intervening PR63 only encapsulates View text caches: the independent full/document and current/previous states retain lazy refresh, buffer capacity, snapshot finalization, and history-eviction alignment. This includes the output/history caches reachable from modeled tmux. No parser, fixture, receipt or measurement fence changed; source inspection found no new integration concern. Prior independent review is reused under AGENTS.md. Original measured-source artifacts retain their exact provenance.

Current-base validation on the rebased source passed the same commands and
verified Ghostty archive documented in `split-scenes-81dfdc6.md`: release locked
build, strict all-target/all-feature release clippy, benchmark self-test, and
release library667/App207/core45/live-PTY13/presentation111/terminal112/tmux198.
Native audio and Docker-only tests stayed ignored in ordinary runs. The full
release benchmark passed all old and additive thresholds in a coordinated quiet
window; raw report `split-scenes-0493cdd-gate.json`. No baseline values changed.

Docker: the initial private-terminfo check passed and 10/11 tmux cases passed;
the byte-echo/paste/mouse/flood case timed out after 10 seconds waiting for the
bracketed-paste state, with partial hex output visible. The unchanged affected
case passed one bounded rerun (0.120 s), followed by a successful full harness
(private-terminfo1 and tmux11). No code or timeout changed. The driver advances
PTY handling and ticks when receiving chunks, so this observation alone does
not distinguish transport progress from publication/scheduling; no load cause
is asserted. Initial and follow-up logs are retained at
`/private/tmp/lector49-cache-docker{,-focused,-confirm}.log`.
