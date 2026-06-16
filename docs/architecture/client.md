# The client

*Deep-dive companion to [README.md](README.md). Covers `kaijutsu-client` — the
RPC client library all apps/CLIs use to reach the kernel. Code is truth; verified
2026-06-16.*

`kaijutsu-client` provides four things: an SSH transport (three multiplexed
channels), a typed Cap'n Proto facade over `World`/`Kernel`, a `Send+Sync`
`ActorHandle` that bridges multithreaded callers into the `!Send` Cap'n Proto
`LocalSet`, and client-side CRDT mirrors that apply server-pushed events
incrementally.

---

## The actor bridge

### `ActorHandle` (`src/actor.rs:617`)

`Clone + Send + Sync`. Holds an `mpsc::Sender<ChannelCmd>` (the only path in),
plus `broadcast` senders for `ServerEvent` and `ConnectionStatus`. Every public
method builds a `oneshot` reply channel, sends a `ChannelCmd` (command + caller
tracing span) over the bounded mpsc (cap 32), and awaits the reply. The mpsc is
the only shared mutable state, so the handle is trivially `Send+Sync` while the
real Cap'n Proto work runs inside `RpcActor` on a `LocalSet`. `spawn_actor`
(`:2388`) wires the channels, builds the actor, and `spawn_local`s `actor.run()`.

### `RpcActor` (internal, `!Send`, `src/actor.rs:1257`)

Runs the connection FSM: `Idle → Connecting → Connected → Closing → Cooldown →
(retry) | Terminal`. Owns the live `RpcClient` + `KernelHandle`, the ping task,
and the handshake task. Cap'n Proto calls dispatch as `spawn_local` children so
the main loop stays reactive; a `biased` select prioritizes close over command
intake.

### `RpcClient` / `SshClient`

`RpcClient` (`src/rpc.rs:43`, `!Send`) wraps a `world::Client` bootstrapped from
the `RpcSystem`. An `RpcSystemGuard` (`Rc<AbortHandle>`) aborts the
`spawn_local(rpc_system)` task on last-drop, closing the SSH stream so the server
detects disconnect. `bind_kernel` (`:147`) returns a `(KernelHandle, KernelId)`;
`KernelHandle` (`:256`) exposes the per-context RPC methods. `SshClient`
(`src/ssh.rs:181`) wraps `russh`, opens the three channels (control, rpc, events),
supports agent/file/in-memory keys, and does TOFU host-key checking via
`known_hosts` (mismatch is a non-retryable error).

---

## Client-side CRDT mirror

### `SyncManager` (`src/sync.rs:99`)

A pure state machine tracking `frontier: Option<HashMap<BlockId, Frontier>>` +
`context_id`. `frontier = None` (or a context change) means a full sync is needed.
`apply_initial_state` (`:267`) replaces the store from a CBOR `StoreSnapshot`;
`apply_block_inserted` (`:329`) tries incremental merge, falls back to full
snapshot, buffers on double failure; `apply_text_ops` (`:424`) is incremental-only
and resets the frontier on deserialize error. A `pending_ops` buffer (bounded at
200; overflow → reset) holds ops that can't yet apply.

### `SyncedDocument` (`src/synced_document.rs:40`)

Bundles a `CrdtBlockStore` + `SyncManager` + a `pending_events` map. The map
solves the **cross-topic FlowBus ordering problem**: non-insert events (status,
text ops, metadata) that arrive before their `BlockInserted` are buffered per
block (≤64 blocks, ≤128 events each); on insert the block syncs first, then
`replay_pending` drains the buffer. `apply_event` (`:211`) is the single consumer
API; `SyncReset` resets the frontier and signals `NeedsResync` (the consumer then
calls `get_context_sync` and feeds `apply_sync_state`).

### `SyncedInput` (`src/synced_input.rs:14`)

Much simpler — one DTE doc with an `"input"` text key, no blocks/buffer.
Pre-registers `PrincipalId::system()` as a known DTE agent so server ops merge
without `DataMissing`. `edit` returns ops to push upstream; `apply_remote_ops`
merges; `clear` deletes the full range.

### `subscriptions.rs`

Defines `ServerEvent` (`:30`, the typed enum of all server-push callbacks) and
`ConnectionStatus` (`:123`). `BlockEventsForwarder` (`:165`) implements the
`block_events::Server` capnp trait, deserializes each callback, and emits onto the
`broadcast` channel. `ResourceEventsForwarder` (`:706`) does the same for MCP
resource events.

---

## Data flow

**Outbound:** caller (any thread) → `ActorHandle` method → bounded mpsc → actor
loop → `spawn_local(run_rpc_call(...))` → `KernelHandle` method → capnp
`request.send()` → SSH rpc channel → server → reply via oneshot. Per-call timeout
30 s; disconnect-class errors trigger the `Closing` transition.

**Inbound:** server emits an event → SSH events channel → capnp callback (in the
`LocalSet`) → `BlockEventsForwarder` → `broadcast` (cap 256) → consumer
(`subscribe_events`) → `SyncedDocument::apply_event` (buffer-if-unknown, else
merge, then `replay_pending`) → `CrdtBlockStore` mutated → consumer reads
`blocks()`.

**Handshake** (`connect_handshake`, `:1852`): SSH dial+auth (5 s) → `bind_kernel`
(5 s) → `join_context` if set (5 s) → `attach_peer` if remembered (best-effort,
non-fatal) → `subscribe_blocks_filtered` + `subscribe_mcp_resources` in parallel
(5 s). Total budget 25 s.

---

## Smells (not fixed — see [issues](../issues.md))

- **Peer re-attach residual gap** — the reconnect path re-sends `attach_peer`
  (`actor.rs:1933`, the `tech_debt_peer_reattach_on_reconnect` fix), but the
  *initial* registration isn't remembered until the first successful user
  `attach_peer`; a restart before that leaves the peer un-reattached.
- **Backoff reset bug** — `finish_closing` reads `self.state` *after*
  `mem::replace` already moved it to `Idle` (`actor.rs:1451`), so the attempt
  counter isn't preserved through `Closing → Cooldown`; backoff always resets to
  1 s after a post-connect failure.
- **String-matched disconnect classification** — `is_disconnect_error` matches on
  the capnp error `Display` text (`actor.rs:1214`); fragile if capnp changes
  formatting (would stop triggering reconnect).
- **Escape hatches** — `doc_mut()`/`sync_mut()` (`synced_document.rs:465`) hand out
  `&mut` past all invariant maintenance.
- **Wrong-context events buffered** — events for another context with an unknown
  block id are buffered and never replayed (bounded, so not a leak, just wasted
  slots).
- **Dead `SyncReset` path** — wired and tested client-side but the server never
  emits it.
