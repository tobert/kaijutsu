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

Rough concept mapping (to be validated by the client-readiness assessment):

| ACP v1 | kaijutsu |
|---|---|
| `session/new` / `session/load` | context create / attach (`register_session`) |
| `session/prompt` turn | `write_input` + `submit_input` |
| `session/update` notifications | turn event stream (tokens, tool calls, blocks) |
| `session/cancel` | ⚠ needs a turn-interrupt primitive — check |
| `session/request_permission` | ⚠ hook engine has Deny/Log/Invoke, no Ask — check |
| `mcpServers` declared into session | external MCP wiring (issues.md — unplumbed) |
| session list ordering | **the rank** (ring 0) — see below |

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
1. ~~**Turn-completion event is not on the wire**~~ **SHIPPED on branch
   `turn-events`** (2026-08-04, Opus agent; 4966 tests green, awaiting
   review/merge — squash the `cf93e339`+`3cc8fcc4` pair for bisectability).
   Step-0 finding: interactive turns published NOTHING (an
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
2. **No Ask pathway for `session/request_permission` — MEDIUM, genuinely
   new.** `HookAction` is Invoke/Deny/Log only (`hook_table.rs:118-122`).
   Template exists: `ElicitationEvents::onRequest` (`kaijutsu.capnp:
   1125-1127`) already does blocking server→client call/response, just scoped
   to MCP elicitation. Needed: `HookAction::Ask` + a `PermissionEvents`
   callback. This is also where graduated trust gets teeth.
3. ~~**`register_session` reconnect/label-conflict**~~ **SHIPPED on branch
   `register-session-upsert`** (2026-08-04, Sonnet agent; 2440 tests green,
   awaiting review/merge). Upsert semantics: DB-driven `resolveContextLabel`
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
permissions before any untrusted frontend.

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
  phone user means by "the conversation."
- Permission UX on mobile: ACP `session/request_permission` vs. kj's
  shared-trust stance — the adapter may be where graduated trust first gets
  real teeth.
- Happy's relay architecture (E2E-encrypted sync server) vs. plain
  ACP-over-local-bridge: if we want push notifications to the phone, we may
  end up wanting a relay too. Study the clone before deciding.
