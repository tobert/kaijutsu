# Drift and dead letters

Drift moves content between contexts as durable `BlockKind::Drift` blocks.

## Delivery and staging

- **Immediate by default.** `kj drift push <dst> <content...>` delivers a
  `BlockKind::Drift` block directly into the target context's document and
  runs the target's `drift` rc lifecycle.
- **Staging is opt-in.** `kj drift push --stage <dst> <content...>` queues a
  `StagedDrift` entry for batch delivery via `kj drift flush` or removal via
  `kj drift cancel <id>`.
- **Distillation.** `kj drift push --summarize`, `kj drift pull <src>`, and
  `kj drift merge` distill a context on the caller's `distill` model slot.
- **Turn boundary.** A drift never starts or extends a turn on its own unless
  the target context's `drift` rc lifecycle explicitly calls `kj drive`. An
  in-flight turn reads newly arrived drifts after its next tool round via
  `ConversationMailbox::catch_up`.
- **Console gate.** A drift into a person's console lands held (`Pending` and
  excluded from model hydration) and opens an ask; `kj ledger allow <id>`
  releases it.
- **Refusals.** `push` and `flush` refuse an archived or concluded target
  context.

## Durability and dead letters

- **Durable queue and dead-letter log.** Staged drifts and dead letters persist
  in the well-known drift queue context (`ensure_drift_queue_context`) and
  rehydrate on cold start (`attach_persistence`, `rehydrate_from_block_log`)
  before new items can stage.
- **Retry and `lost+found`.** Failed deliveries retry up to `MAX_DRIFT_RETRIES`
  (5), then move to the dead-letter queue and drain into the well-known
  `lost+found` context (`ensure_lost_found_context`). Well-known contexts are
  tracked by role in `KernelDb` rather than by magic label, so `lost+found` can
  rotate without losing history or minting duplicates.
- **Origins.** `StagedDrift.origin` (`DriftOrigin`) records either a Kaijutsu
  `Context` or an external `Peer` (such as a Claude Code session via
  `cc_inbox`).
