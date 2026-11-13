# Control transitions

Handling a tmux control event happens in two steps, in two places:

1. `TmuxConnectionState::step_control` consumes one event and returns a
   result. For command events it also consumes exactly one expected reply,
   except for the initial command. It owns state local to the connection:
   inventory, topology, and flow control.
2. `App::process_tmux_control` applies that result. It owns command queueing,
   the connection hierarchy, views, recovery, and presentation.

This page is the reference for what one step can produce, and the order in
which App must apply it. Read it before changing either function.

## What can happen together

One step can carry several effects. This table lists, for each input, the
work it defers to App and what else can occur alongside. A step with nothing
to do still runs App's bookkeeping. The initial command is a separate early
return.

| Input | Deferred work | Work that can occur together |
| --- | --- | --- |
| Initial command | None; return before App effects | Must not pop an expected reply |
| Inventory reply | Drain, retry, terminal failure, or publish | Publication carries removed panes/windows, native copy-mode panes, topology synchronization/rendering, location/session changes, and possibly a flow-control warning |
| Flow policy / verification | None | Connection acceptance/retention state only |
| Bootstrap reply | Portable retry or bootstrap application | Application can later request render, location announcement and resume |
| Resync probe | Capture batch or failure | Probe stores capture metadata; failure discards it |
| Resync continue / capture / pending escape | None | Mutate the pending capture; failed continue poisons the later snapshot |
| Resync verify | Success, raced, or failure (exclusive) | Success updates topology metadata; application can later request rendering and resume |
| Pause command reply | Optional resync request | Flow state/backoff changes |
| Continue command reply | Optional resync request | Earlier capture instead schedules recapture; failures retain retry policy |
| Carrier command reply | Exactly one create/verify/unlink/switch/move/move-verify result | No other reply outcome from this command |
| User command reply | Optional result popup | Key configuration changes can also request inventory |
| Ignored / unsolicited command | None | Unsolicited replies do not fabricate an expected reply |
| Output / extended output | Pane transport | Flow entry/age bookkeeping; App transport can itself change topology/presentation |
| Pause notification | Lost carrier or optional resume | Ordinary pane capture-required state; carrier loss must not resume a broken nested protocol |
| Continue notification | Optional resync request | Final continue/capture batch keeps recovery active; prior captures schedule recapture |
| Message / config-error notification | Popup | No topology reconciliation |
| Other notification | Reconcile topology | Inventory invalidation, removed panes/windows, sync/render and location/session changes can coexist |
| Other control event | None | App bookkeeping still runs |

Two things follow from the table:

- Effects are mostly independent fields, not one "outcome" enum. Topology
  synchronization and rendering can both happen. So can a command result and
  an inventory request. Removed identities and native copy-mode panes carry
  data, not just a flag.
- Only carrier reply kinds and capture verification results are enums. Those
  really are exclusive, and their match arms prove it.

## Ordered App effects

App applies a result in these phases. Keep the order, including when an
earlier phase creates work for a later one.

1. Carrier reply; inventory lease reconciliation; attached-session observation;
   lost-carrier failure. Then preferred location and bell acknowledgment.
2. Bootstrap retry, inventory request, native copy-mode exit commands, resync
   capture batch, inventory-failure UI, resume command, resync request. Commands
   retain their paired expected replies and existing bounded queue handling.
3. Resolve destroyed gateways, synchronize panes, optionally open review.
4. Apply bootstrap, successful capture, raced capture or failed capture. Keep
   mutable render/location flags and collect any resume discovered here.
5. Queue that late resume, then present **pane output, else chooser update, else
   topology render**. Do not freeze presentation decisions at transition time.
6. User command result, then notification popup (including inventory failure).

Errors propagate through App; missing connections and the initial reply
skip the apply phases. Presentation context and time are sampled by App before
transition. Payload ownership moves from the connection-local result into these
ordered effects.
