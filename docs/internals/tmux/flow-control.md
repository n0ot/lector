# Flow control and recovery

A pane can produce output faster than anything can display it. This page
explains how Lector stays responsive when that happens, and how it rebuilds a
pane after output is lost.

The key fact: **when tmux pauses a pane, it throws output away.** It does not
buffer it. A paused pane's terminal state in Lector is therefore out of date,
or *stale*, and must be rebuilt from a capture.

## Asking tmux to pause

When a control client starts, before its inventory, Lector sends:

```text
refresh-client -f pause-after=1
```

This asks tmux to pause any pane whose output has gone unread for one second,
instead of letting an unlimited backlog build up.

Lector then reads `#{client_flags}` and checks that `pause-after=1` is really
set. A successful reply to the command is not enough proof.

If the installed tmux does not support `pause-after`, Lector consumes both
replies cleanly, announces that automatic recovery is unavailable, and
carries on. Ordinary output still works.

## When a pane is paused

`%pause %N` marks pane `N` stale.

- **If the pane is in the window being shown,** Lector queues exactly one
  resume, however many times tmux repeats the notification:

  ```text
  refresh-client -A '%N:continue'
  ```

- **If it is in the background,** it stays paused until its window or
  connection is shown.

`%continue %N`, or a successful resume reply, means only that delivery has
restarted. It cannot bring back the discarded bytes. Lector follows every
resume with a [resynchronization](#resynchronization) and ignores incremental
output for the pane until that completes.

`%extended-output` carries an age. Lector records it for latency diagnostics
only. A large age can simply mean intact output waited in a queue, so
recovery starts from `%pause` or Lector's own limits, never from an age
threshold.

### Output that arrives during recovery

If output arrives while a capture is in flight, Lector schedules one more
capture after a short quiet period. Continuous output could postpone that
forever, so there is a hard deadline of one second from the first such byte.

At the deadline, Lector pauses the pane itself, probes it while paused, and
sends `continue` and the final capture together as one tmux command sequence.
tmux resets its output offset on `continue`, so that capture includes the
paused interval, and everything after the sequence arrives as normal
incremental output. This is the last round for that recovery, not another
retry.

### Failures

A failed resume or capture leaves the pane stale. Lector retries with a
bounded delay for as long as the pane is visible, so a visible pane cannot
stay paused forever.

## Fairness

Lector also limits its own work, below tmux's flow control.

- One event-loop turn reads at most 32 KiB from the root PTY, then lets
  input, signals, replies, and queued commands run.
- Visible changes from one drain are composed and drawn once, before waiting
  input is handled.
- Background panes get 4 KiB of immediate work, then 4 KiB per turn in
  round-robin order.

This is one bounded scheduler on one thread, not a thread per pane.

**Coalescing.** Neighboring `%output` records for the same pane in one read
are combined, up to 64 KiB. Control records, extended output, and a change of
pane end a run. The combined run is always applied before the read handler
returns, so it never delays a model update or a protocol reply.

**Per-pane backlog.** When one pane's deferred output reaches 16 KiB, Lector
asks tmux to pause that pane right away, without waiting for the one-second
rule. Selecting the pane later queues a `continue` and then the capture.

**Total backlog.** Deferred background output is capped at 64 KiB in all. If
a pane would push past that, its queued output is dropped and it is marked as
needing resynchronization. Its bytes are ignored until then. The capture
waits until the pane is shown.

The effect is that typing and commands never queue behind a hidden pane's
flood.

## Resynchronization

### Why everything is discarded

While a pane is stale, Lector discards all of its output without parsing it.

Once any bytes are missing, the next bytes might begin in the middle of an
escape sequence, a UTF-8 character, or an image transfer. Even something that
looks like a complete control code cannot be trusted. So bells, terminal
queries, clipboard requests, notifications, and every other side effect are
dropped along with screen changes.

### The capture transaction

Recovery sends four commands, in order:

```text
display-message -p -t %N -F <pane capture metadata>
capture-pane -p -e -F -J -S - -t %N
capture-pane -p -P -t %N
display-message -p -t %N -F <pane capture metadata>
```

| Step | Purpose |
| --- | --- |
| First metadata sample | Tells Lector which screen to capture: the pane's display, or a tmux mode screen (`-M`). |
| Main capture | `-e` keeps styles. `-F` keeps the line flags tmux has for prompts, output, and links. `-S -` includes history on the primary screen. History is left out on the alternate screen. |
| `-P` capture | Fetches an escape sequence that is only partly received. |
| Second metadata sample | Must match the first in screen, size, and mode. If not, the snapshot is inconsistent and Lector retries. |

As in [bootstrap](panes.md#the-capture), `-a` is never used.

On success, the pane's terminal is replaced with the captured text, history,
styles, links, prompt boundaries, size, and cursor, followed by the partial
escape sequence. Output after the transaction is processed normally.

If the pane disappears while replies are in flight, the transaction is
dropped harmlessly.

### What cannot be recovered

tmux's capture is not a saved terminal. Recovery cannot restore:

- Kitty images and their placements;
- semantic details tmux does not keep in its line flags;
- exact historical wrap positions, and saved-mode stacks; or
- the partial escape sequence, if the installed tmux cannot do the `-P`
  capture.

Lector drops images during replacement, records which limitations applied,
and announces them afterward.

### Coalescing

Repeated `%pause` events lead to one resume and one capture. Resize storms
collapse to the latest size.

## Nested connections

A pane that carries a nested `tmux -CC` stream is different. Its bytes are a
protocol, not a screen, and a protocol cannot be rebuilt from a capture.

- Lector always drains such a pane, even while something else is shown.
- Before the outer client switches session, Lector links the pane's window
  into the destination session so that its output keeps flowing. See
  [carrier windows](prefix.md#carrier-windows).
- If tmux pauses the pane anyway, protocol bytes are gone for good. Lector
  drops the nested connection. It does not send `continue` and guess where
  the stream resumes.

## What the user hears

The user-facing messages are described in the
[tmux guide](../../guide/tmux.md#when-something-goes-wrong).

- A resynchronization message means tmux paused a pane, or Lector capped an
  overloaded one. Text and history were rebuilt.
- A resynchronization failure means `capture-pane` failed. The pane stays
  stale and retries while visible. Lasting failures usually mean the control
  PTY is not accepting writes, or the tmux client has gone.
