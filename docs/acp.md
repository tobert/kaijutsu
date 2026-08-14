# ACP in kaijutsu — living doc

All things Agent Client Protocol for kaijutsu. Seeded 2026-08-04 from the
meadow-lab research session (full harness comparison:
`~/src/meadow-lab/docs/kaijutsu-gap-analysis.md`). Amy: *"ACP 1 for now.
color me curious."*

## Why we care

ACP is doing the LSP trajectory: v1 stable (2026-06-24, schema 1.20.0),
governance moved off Zed to a neutral `agentclientprotocol` GitHub org with a
multi-stakeholder working group. Client ecosystem already includes **mobile
apps** (Happy, Agmente, Ferngeist, Mobvibe) and messaging bridges. A
`kaijutsu-acp` adapter puts kj on existing frontends — including Amy's phone —
before any custom app exists, and reframes the mobile side quest from "build
an app" to "build a bridge, then decide if the app is still worth it."

## The adapter shape

Same architecture as `kaijutsu-mcp`: thin bridge binary, protocol outward,
kernel connection inward.

```
ACP client (Zed / Happy / neovim / …)
   │  JSON-RPC 2.0 over stdio (ACP v1)
kaijutsu-acp
   │  kaijutsu-client (SSH + Cap'n Proto), --connect like kaijutsu-mcp
kaijutsu-server / kernel
```

Concept mapping — **as built** (`crates/kaijutsu-acp`, prototype 2026-08-05):

| ACP v1 | kaijutsu | status |
|---|---|---|
| ACP `SessionId` | `ContextId` hex (`rank::session_id_of`) | durable; survives restarts, no side table |
| `session/new` | `resolve_context_label` upsert → `create_context_typed(label, "coder")` → `join_context` | built |
| `session/load` | parse session id → `join_context` + replay transcript as updates | built |
| `session/list` | **the rank** — `list_contexts` → `assign_ring_seats`, ring 0 then ring 1 | built (ring stamps are on the wire — see below) |
| `session/prompt` | `get_input_state` → `edit_input(0, text, len)` → `submit_input(ctx, false)` | built; text-only |
| turn end → `stopReason` | `ServerEvent::TurnCompleted{stop_reason}` (1:1 by construction) | built |
| turn broke | `ServerEvent::TurnFailed` → JSON-RPC error, **not** a stop reason | built |
| `session/update` text/thought | `BlockKind::Text`/`Thinking` char deltas off the CRDT mirror | built |
| `session/update` tool_call / tool_call_update | `BlockKind::ToolCall` create, then patch; `ToolResult` patches the call it links to | built |
| `session/cancel` | `interrupt_context(ctx, immediate: false)` — soft | built |
| `session/request_permission` | `ActorHandle::take_permission_asks` → `permission::run_permission_pump` → `contextId` → ACP session (`rank::session_id_of`) → real round trip; fail-closed on no session/client error/timeout | built |
| `mcpServers` declared into session | external MCP wiring (`external.rs`, no caller) | ignored + warned |
| `session/update` `plan` | `BlockKind::Task` blocks rebuilt whole-context off the CRDT mirror, one `PlanEntry` per non-cancelled task | built — see "Task → plan" below |
| `fs/*`, `terminal/*` client methods | — | not used; kj runs its own tools |

## Session picker = the rank

Amy: *"I suppose we could use the ranks we have in the app rings already?"*
Yes — built. Ring membership is **explicit placement** (2026-07-05 rebuild,
narrowed 2026-08-01 to two rings + a floor, 10 seats each): ring 0 ACTIVE is
hand-curated, append-ordered by `promoted_at`, **kernel-capped at 10**; the
`0–9` rank landed *as* ring 0 (`docs/timewell.md` "Ring membership becomes
explicit"; seating logic `kaijutsu-viz/src/layout.rs`). Because the
promoted/demoted stamps are kernel-side, the ACP adapter can serve the same
rank as its session list — a phone shows the identical ten seats, same order,
as the app's ring 0. Muscle memory transfers: `ctrl-a 2` on the desk is seat 2
on the phone. Still open per timewell.md: table-edge rail rendering, pinning
(deferred until the itch is real).

**Verified on the wire (2026-08-05).** No schema change was needed:
`ContextHandleInfo.promotedAt/@17`, `.demotedAt/@18`, `.pausedAt/@19`
(`kaijutsu.capnp:637-639`) ride `listContexts` and are decoded into
`ContextInfo` (`kaijutsu-client/src/rpc.rs:2845`). *Seat order* is not on the
wire and does not need to be: `kaijutsu_viz::layout::assign_ring_seats` is a
zero-dep pure function, so the bridge computes the identical seating the app
does from the identical inputs. `kaijutsu-acp` adds `kaijutsu-viz` as a
dependency and calls it (`rank.rs`) — one seating engine, two frontends.

Note what is *not* exposed: `kj context info --json` omits the three stamps
(human text only, `kj/format.rs:229`), and no VFS path carries them. A
consumer must use `list_contexts()`, not the `kj` surface.

`ranked_sessions` serves ring 0 in seat order, then ring 1 by recency. Ring 0
alone is empty on a fresh kernel and a picker showing nothing is useless;
appending ring 1 keeps ring 0's seats stable at the front where the muscle
memory lives.

## Foundational work before the adapter

Sonnet assessment done 2026-08-04 (kaijutsu-client/RPC vs. ACP v1 checklist).

**Maps cleanly already — no redesign needed:**
- Session CRUD: `RpcClient::{list,create,join}_context`/`conclude`/`archive`
  (`kaijutsu-client/src/rpc.rs:432,560,587,1421,1534`).
- Turn submit + cancel: `submit_input` (`rpc.rs:1923`) and — better than ACP
  needs — `interrupt_context` with `immediate: Bool` soft/hard semantics
  (`rpc.rs:1659`, `kaijutsu.capnp:1308-1311`). `session/cancel` is covered.
- Tool-call reporting: `BlockKind::ToolCall/ToolResult` with Status + engine
  (`kaijutsu-types/src/block.rs:1011-1020`) streaming as
  `ServerEvent::BlockInserted/BlockStatusChanged/BlockOutputChanged/...`
  (`subscriptions.rs:37-169`) — exactly ACP's `tool_call`/`tool_call_update`
  create+patch shape. Text/Thinking chunks ride the same events.
- kaijutsu-client is proven reusable: kaijutsu-mcp consumes
  `ActorHandle`/`connect_ssh` with zero forked RPC code (`kaijutsu-mcp/src/
  lib.rs:80,451`).

**The gaps, prioritized:**
1. ~~**Turn-completion event is not on the wire**~~ **SHIPPED, merged to main**
   (branch `turn-events`, 2026-08-04, Opus agent; 4966 tests green; merge commit
   `9554602c`). Step-0 finding: interactive turns published NOTHING (an
   `announce_completion: bool` consumer-filter living in the producer);
   replaced with `TurnOrigin { Interactive, Autonomous }` on every event —
   beat.rs filters, everyone publishes. Stop reason:
   `TurnStopReason { EndTurn, Cancelled{immediate}, MaxTokens,
   MaxIterations }` on Completed (`Failed` = only "the turn broke");
   `output_is_complete()` false only for hard cancel. Kernel-wide
   subscription (event names its contextId), mandatory in the actor's
   subscription set. Catch-up for late/reconnecting subscribers deliberately
   deferred (needs a catch-up story, not a journal — noted on the
   TurnFlow-durability issue).
2. ~~**No Ask pathway for `session/request_permission`**~~ **SHIPPED, merged to
   main** (branch `hook-ask`, 2026-08-05, Sonnet agent; 1949 kaijutsu-kernel +
   304 kaijutsu-server + 139 kaijutsu-client tests green, `cargo build
   --workspace` clean; merge commit `232c99c9`). `HookAction::Ask(AskSpec)`
   joins Invoke/Deny/Log/ShortCircuit (`hook_table.rs`), with an
   `HookActionWire::Ask { description }` admin-wire surface and DB
   persistence (`hooks.action_ask_description`) alongside the others.
   `PermissionEvents::onAsk` (`kaijutsu.capnp`, ordinal `@103` on `Kernel`
   for `subscribePermissionEvents`) is modeled on `ElicitationEvents::
   onRequest`'s blocking call/response shape but is deliberately
   **kernel-wide, not per-connection**: `HookAction::Ask` can fire from any
   call path (autonomous turn, sibling context, kaish script), so there's no
   "the connection that asked" the way MCP elicitation has. That forced a
   real correction mid-implementation — a first pass tried to hold
   `permission_events::Client`s directly in `SharedKernelState` and hit a
   wall: capnp `Client`s are `Rc`-based (`!Send`), but `SharedKernelState`
   must stay `Send + Sync` (each connection can run on its own OS thread).
   Fixed by copying the `peers::InvokeRequest` cross-thread pattern: the
   shared registry holds only `Send`-safe `mpsc::Sender`s; the actual
   capability stays on its owning connection's `spawn_local` bridge task,
   which drains the channel and drives `onAsk` locally, replying via a
   paired `oneshot`. `Broker::run_permission_ask` wraps the whole ask in
   `tokio::time::timeout` (30s default, `DEFAULT_PERMISSION_ASK_TIMEOUT`,
   test-overridable) — **no subscriber attached, and timeout, both fail
   closed** (`McpError::Denied`, logged at `warn!`, not the quieter `debug!`
   other hook denials use — a stuck Ask usually means a misconfigured
   session, worth an operator's attention). `kaijutsu-client` gained
   `permission_events_channel()` (mirrors `turn_events_channel`, but
   request/response instead of push-only) and `KernelHandle::
   subscribe_permission_events`; an ACP adapter calls the former to build a
   callback + envelope receiver, passes the callback to the latter, then
   drains `PermissionAskEnvelope`s and answers each via its paired
   `oneshot::Sender<PermissionAskAnswer>` — exactly `session/
   request_permission`'s shape. Tests: kernel-side hook parse/dispatch +
   allow/deny/no-subscriber/timeout round trips against a fake
   `PermissionAsker` (`broker.rs`), DB round-trip (`kernel_db.rs`),
   admin-wire install/reject (`hooks_builtin.rs`); client-side capnp
   encode/decode round trips including a dropped-receiver failure mode
   (`subscriptions.rs`); a real SSH+capnp e2e proving the cross-thread
   bridge itself — not just each side's fakes — actually joins
   (`kaijutsu-server/tests/permission_ask_wire.rs`).
3. ~~**`register_session` reconnect/label-conflict**~~ **SHIPPED, merged to
   main** (branch `register-session-upsert`, 2026-08-04, Sonnet agent; 2440
   tests green; merge commit `7670d72e`). Upsert semantics: DB-driven `resolveContextLabel`
   RPC → attach-if-live (`resumed: true` + timestamps for the stale-id
   hazard), suffix-fresh if concluded (`previous_context` in reply). Bonus
   finding: boot-time recovery already re-registers non-archived contexts,
   so the "invisible after restart" divergence only genuinely bit ARCHIVED
   contexts — `join_context` now heals from the durable row; issues.md entry
   corrected with verified findings.
4. **External MCP wiring — MEDIUM.** `external.rs` implemented, no caller
   (issues.md MCP audit). Blocks ACP's optional client-declared `mcpServers`;
   also v2-proofing (v2 drops `fs/*`/`terminal/*` for client-provided MCP).
5. **Delegation join — substrate SHIPPED with #1; `kj wait` itself still
   unbuilt, deliberately.** The bus is now subscribable; what remains is
   semantics, not plumbing: timeout policy, the turn-that-ended-before-the-
   waiter-subscribed race (bus is lossy + un-journaled), and multi-child
   waits. Documented seam on `request_child_turn`; issues.md entry rewritten.

Suggested order: #1 (+#5 riding along) → #3 → #2 → adapter prototype can
start with `request_permission` stubbed to auto-allow → #4 and real
permissions before any untrusted frontend. #1–#3 are now all shipped and
merged to main; #2's `HookAction::Ask` + `PermissionEvents` is real
permission plumbing, not the stub the original order assumed — an adapter
can wire `session/request_permission` straight to `subscribe_permission_events`
/ `permission_events_channel` from day one instead of auto-allowing.

## The adapter, as built (2026-08-05)

`crates/kaijutsu-acp` — one binary, no forked RPC code, `kaijutsu-client`
consumed exactly as `kaijutsu-mcp` consumes it.

```
src/bridge.rs      kernel side: connect, resolve/create/join, prompt, interrupt
src/session.rs     SessionRegistry + the per-session event pump + the turn wait
src/update.rs      PURE: BlockSnapshot → SessionUpdate (the mapping layer)
src/rank.rs        PURE: ContextInfo[] → the rank → SessionInfo[]
src/permission.rs  the live Ask pathway: pump, option shaping, answer mapping
src/lib.rs         the six ACP handlers + version negotiation
src/main.rs        clap, stderr tracing, LocalSet, --connect
tests/dispatch.rs  the six-handler chain actually dispatches by type
```

**Which way agency points.** In MCP kaijutsu is a *tool* another agent calls;
in ACP kaijutsu *is* the agent and the client is the human's seat. So new
sessions take `context_type=coder` (the model-facing stance bundle), not
`mcp`. `--context-type` overrides.

**Decisions worth remembering:**

- *Snapshots, not events.* `BlockTextOps` carries opaque CRDT ops, so decoding
  a delta means owning a `SyncedDocument` anyway. The pump applies the event
  and hands the resulting `BlockSnapshot` to a mapper that keeps per-block
  high-water marks. That makes translation idempotent, which is also how
  `session/load` replays history through the same code path.
- *Char deltas, never byte.* A chunk boundary mid-codepoint is a corrupt frame.
  Same for the input-doc write: `edit_input`'s delete count is characters.
- *Kernel-wide block events* (`scope_blocks_to_context: false`). An ACP client
  can hold several sessions — that is what `session/list` is for — and a
  context-scoped subscription would starve every session but the last joined.
  Each pump filters by context id. If the firehose bites, the fix is an actor
  per session, not a narrower filter.
- *Echo suppression is armed, not addressed.* `submit_input` only returns the
  block id after the fact, by which time the pump may have already streamed
  the echo. The mapper arms before the write and claims the first unseen user
  block. A sibling's message landing in that window would be eaten instead —
  accepted for the prototype.
- *Crosstalk reaches the phone.* User-role blocks from other principals are
  forwarded as `user_message_chunk`. Your neighbour typing at the desk shows
  up in the ACP transcript. That is the stance, not a leak.
- *A failed turn is a JSON-RPC error*, not `stopReason: end_turn`. ACP has no
  "failed" stop and reporting a clean end for a crashed turn is the silent
  fallback we refuse.
- *Soft cancel by default.* `interrupt_context(immediate: false)` — the
  in-flight model call finishes, so the transcript keeps a complete phrase.
- *Bounded connect.* The actor's FSM retries a failed handshake forever and
  never reaches Terminal, so an unbounded wait hangs the bridge with the ACP
  client seeing `initialize` go unanswered. `--connect-timeout` (30s) turns
  that into a message naming the likely cause. Found by the first live smoke
  test, against a kernel too old to serve `subscribeTurnEvents`.

**Permission asks, wired live.** `permission.rs`'s Ask pathway is real: the
`.with_spawned` task registered in `serve_stdio`
(`permission::start_permission_pump`) drains
`ActorHandle::take_permission_asks()` — a kernel-wide envelope stream
`kaijutsu-client` re-arms best-effort on every reconnect
(`actor.rs::connect_handshake` step 3.7) — resolves each envelope's
`contextId` to an ACP session (`rank::session_id_of`, no side table), and
drives a real `session/request_permission` round trip if (and only if) a
session is live for that context. Every failure mode denies: no live
session, a client error, a client timeout
(`permission::PERMISSION_ASK_TIMEOUT`, mirrors the kernel's own 30s default
as defense in depth — the kernel's own timeout is the actual authority), or
a selected option this bridge can't place. `AutoAllow`/`PermissionPolicy`
(the old always-allow stub) are gone — there is no configured opt-in bypass
to keep. `PermissionOption.kind` is free text on the wire
(`kaijutsu.capnp`); `permission::map_kind` places the four strings the
kernel's doc comment promises (`allow_once`/`allow_always`/`reject_once`/
`reject_always`) and treats anything else as `RejectOnce` — the safest
reading, not the most permissive. `AskSpec` carries no options today (v1's
rule syntax is a plain description), so the empty-options synthesis path
(`build_options`, a plain Allow/Deny pair) is what every real ask exercises
in practice.

**Gaps found while building** (also in issues.md):

- **No catch-up after a resync.** On `SyncReset`/lag/reconnect the pump
  rebuilds the mirror and re-pegs the mapper *silently* — anything that
  changed during the gap never reaches that client. Replaying instead would
  duplicate the whole transcript. Logged at `warn`; the real fix is the same
  catch-up story the turn-events work deferred.
- **`TurnCompleted` has no turn id.** The prompt wait matches on
  `context_id` + `TurnOrigin::Interactive`, which is correlation by ordering.
  Two interactive turns racing in one context would confuse it. Already noted
  P3 in issues.md ("no turnId/endedAt … revisit with the adapter") — the
  adapter now says: yes, we want it.
- **`kaijutsu-mcp`'s `write_input` deletes by byte length** (`lib.rs:1434`,
  `state.content.len()`), so a non-ASCII input doc is corrupted or truncated.
  `kaijutsu-acp` uses `chars().count()`. mcp should too.
- **`session/delete`, `session/set_mode`, `session/set_config_option`** are
  stable v1 and unimplemented. `set_mode` maps naturally onto `context_type`
  / cast roles; `delete` onto `conclude`/`archive`. Not advertised, so a
  client will not call them.
- **Prompt content is text-only.** Image/audio/embedded-resource blocks are
  turned into a `[… omitted]` marker rather than dropped in silence, and the
  capabilities say `image: false, audio: false, embedded_context: false`.

## Task → plan

`BlockKind::Task` (`docs/tasks.md`) wired to ACP v1's `plan` session update,
2026-08-05. `crates/kaijutsu-acp/src/update.rs`.

**Why it can't be `observe()` alone.** Every other block kind maps to a
per-block update (`observe()` is deliberately pure: one `BlockSnapshot` in,
the deltas that block hasn't shown the client yet out). A `plan` is not
per-block — ACP v1 has no `plan_operations` feature in this build, so a plan
update **replaces the whole list**; there is no "here's one new entry"
shape. So the mapper splits the job:

- `UpdateMapper::note_task(&BlockSnapshot) -> bool` — a cheap per-block
  high-water mark (`content`, `task_status`), the same idempotence
  pattern as `emitted`/`tool_status` for every other kind. `false` for
  any non-`Task` block (harmless no-op — the pump calls it
  unconditionally rather than gating on `block.kind` itself) and for an
  unchanged Task.
- `UpdateMapper::build_plan(&[BlockSnapshot]) -> Option<SessionUpdate>` —
  the actual rebuild: walks the **whole** document's Task blocks
  (`task_plan_entries`), and returns `None` if the result is
  byte-for-byte the plan already emitted (`last_plan`). This is the ONE
  rebuild-and-emit path, called from three places:
  - **Live pump** (`session::run_pump`): after `observe()` on any block,
    `note_task` gates whether `build_plan(&doc.blocks())` is even
    attempted.
  - **`session/load` replay**: `observe()` every block first (Task
    always empty per-block, same as live), then exactly **one**
    `build_plan` call at the end — not one per task touched during
    replay.
  - **`session/new` bootstrap** and **resync**: `mark_seen`/
    `baseline_plan` silently establish `last_plan` from the current
    doc without emitting (an rc-seeded task shouldn't be narrated at a
    client that just opened the session — same reasoning as
    `mark_seen` for text/tool-call marks); the resync sweep then calls
    `build_plan` unconditionally alongside its existing `observe()`
    sweep, so a gap with no task activity stays silent (the same
    "second sweep must be silent" contract `resync_sweep_emits_
    exactly_the_gap` already pins for every other kind).

**Status mapping** (`TaskStatus` → `PlanEntryStatus`):

| kaijutsu `TaskStatus` | ACP `PlanEntryStatus` |
|---|---|
| `Open` | `Pending` |
| `InProgress` | `InProgress` |
| `Done` | `Completed` |
| `Cancelled` | *(omitted — see below)* |

**Cancelled tasks are omitted from the plan entirely**, not mapped onto any
of the three ACP states. ACP's `plan` is framed as "what the agent intends
to do"; a cancelled task is a deliberate groom decision to do nothing
further with it, and `PlanEntryStatus` has no fourth state to say that
honestly in-line (`Pending` would be a lie — nothing is queued —  and
`Completed` would be a different lie). Cancelling an already-shown task is
an ordinary status write, not a delete: it round-trips through the same
`note_task`/`build_plan` path and the entry simply drops out of the next
rebuilt plan.

**Subtasks flatten.** ACP plans are flat (`Vec<PlanEntry>`, no hierarchy
field); kaijutsu subtasks nest via the ordinary `parent_id` DAG edge
(`docs/tasks.md` decision 3). `task_plan_entries` does a pre-order DFS over
that edge — a parent, then each child and its own descendants, before the
next sibling — and renders the nesting as a `"↳ "` prefix on `content`,
repeated once per ancestor level, since there's nowhere else to put it. A
task whose `parent_id` doesn't resolve to another *Task* block in the
current snapshot (`None`, or pointing at a non-Task block — nothing in
`builtin.tasks` promises otherwise) falls back to being treated as a root,
so it can never silently vanish from the plan.

**Priority defaults to `Medium`** for every entry. `docs/tasks.md`
"Deferred" is explicit that Task blocks carry no priority field yet
(nothing asked for it); this is ACP's required-field filler, not a
kernel-side signal — do not read anything into it, and do not invent a
priority field on the kernel side just to feed this mapping.

**Ordering** is document order (`SyncedDocument::blocks()` →
`block_ids_ordered()`), not `BlockId`/`BTreeMap` iteration, which is
principal-major and would scramble both plan order and subtask nesting
(`gotcha_blockid_vs_document_order` — the same trap this crate's other
block-ordered reads already avoid).

**Known gap, not fixed here**: `session::run_pump`'s `BlockDeleted` arm
calls `mapper.forget(block_id)` and `continue`s *without* calling
`doc.apply_event(&event)` — so the live `SyncedDocument` mirror never
actually drops a deleted block; it lingers until the next resync rebuilds
the mirror from scratch. This predates the Task work and affects every
block kind identically, not just tasks — a deleted Task block (`kj block
delete` on the underlying generic block, since `builtin.tasks` itself
has no delete verb) will not disappear from a live plan until the next
resync/reconnect. Logged in `docs/issues.md`.

## Manual smoke test (live kernel)

Prerequisite: the running `kaijutsu-server` must post-date the turn-events
merge — an older one answers `subscribeTurnEvents` with "Method not
implemented", the actor never reaches Connected, and the bridge now exits with
a message saying exactly that. `./contrib/kj rebuild && ./contrib/kj restart`.

```bash
cargo build -p kaijutsu-acp
# sanity: should print the rank, then exit
./target/debug/kaijutsu-acp --connect --host localhost --port 2222
```

Driving it from toad (the desk-side client this was built for):

```bash
# toad's custom-agent entry point is the `acp` SUBCOMMAND (there is no
# --agent flag — bare `toad` opens the agent-store picker, which only lists
# registered agents). `toad acp "COMMAND" [PATH]` wraps the command in an
# ad-hoc agent definition and launches straight into it; -t titles the
# status bar. Quote the whole command; use an absolute path — toad runs it
# from the project dir.
toad acp '/home/atobey/src/kaijutsu/target/debug/kaijutsu-acp --connect --host zorak --port 2222' \
  ~/src/kaijutsu -t kaijutsu
```

What to check, in order:

1. **initialize** — toad connects and shows the agent as `kaijutsu-acp`.
2. **session picker** — the sessions listed are ring 0 in seat order, then
   ring 1. Compare against `kj context list` (ring-0 rows carry `[ring0]`).
3. **session/new** — a fresh `acp-<dir>-<secs>` context appears in the app's
   time well. It should get the `coder` rc stance.
4. **prompt** — text streams in as it generates; thinking shows in the thought
   lane; a tool call appears once and then updates in place rather than
   repeating.
5. **crosstalk** — type into the same context from `kaijutsu-app`; it should
   appear in toad as a user message.
6. **cancel** — interrupt a long turn; toad should get `stopReason: cancelled`
   and the block should end on a complete phrase (soft cancel).
7. **session/load** — reconnect toad and reopen the same session from the
   picker; the transcript should replay.

`RUST_LOG=kaijutsu_acp=debug` on stderr for the play-by-play. stdout is the
wire — never print to it.

## Version stance

**ACP v1 only.** v2 is draft (announced 2026-07-20, alpha schemas, no GA
date); the Rust crate's `2.0.0` on crates.io is crate SemVer, NOT protocol v2
— protocol v2 sits behind the `unstable_protocol_v2` feature flag. Build v1,
leave the flag off, negotiate versions, revisit when the draft label drops.
Remote transport (HTTP/WS) is an Active RFD — irrelevant to us; SSH
`--connect` already covers remote, the ACP hop stays local to the client.

## Reference material

- Official: agentclientprotocol.com (v1 spec, RFDs, updates), the
  `agentclientprotocol` GitHub org (spec 3.9k★, rust-sdk, registry).
- Rust crates: `agent-client-protocol` 2.0.0, `-rmcp` (MCP integration —
  we already use rmcp), `-http` (draft transport, ignore), `-conductor`
  (MCP-over-ACP experiments), plus a docs.rs cookbook crate.
- Local reference clones (`~/src/research/`): **happy** (MIT, Expo/React
  Native mobile client + self-hostable E2E-encrypted relay server — the
  mobile UX to study), **toad** (AGPL-3.0, Will McGugan/batrachian.ai,
  released 2026-01, Python/Textual — "unified interface for AI in your
  terminal", integrates agents as an ACP client; the desk-side frontend a
  kaijutsu-acp adapter would light up first — the Claude session working in
  ~/src/kaibo glowed about it), **hermes-agent** (ships an ACP adapter,
  `acp_adapter/entry.py`),
  **QwenPaw** (whole Textual TUI is an ACP client:
  `cli/tui/transport/acp.py`; ACP server `agents/acp/server.py` — the
  "frontend parity via ACP subprocess" pattern).
- Adjacent, don't confuse: IBM's dead "ACP" merged into A2A (Aug 2025);
  A2A v1.0 (Mar 2026) is agent↔agent, different layer; AG-UI has an
  ACP→AG-UI bridge (complementary, not competing).

## Open questions

- One ACP session per kj context is the obvious mapping — but does a session
  want to *follow* a context through fork rolls (EvictionIndex generations),
  or bind to a single context id? Leaning: follow the chain, it's what a
  phone user means by "the conversation." **The prototype binds a single
  context id** and makes the session id *be* that context id, which is what
  made `session/list` → `session/load` work with no side table. Following the
  chain means the session id can no longer be the context id — it would have
  to name a chain, and the picker would have to show chains. Worth doing, but
  it is a data-model change, not an adapter change.
- Permission UX on mobile: ACP `session/request_permission` vs. kj's
  shared-trust stance — the adapter may be where graduated trust first gets
  real teeth.
- Happy's relay architecture (E2E-encrypted sync server) vs. plain
  ACP-over-local-bridge: if we want push notifications to the phone, we may
  end up wanting a relay too. Study the clone before deciding.
