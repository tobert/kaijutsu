# Drift UI Convergence Plan

*Converging the Bevy client onto ActorHandle and building the multi-context drifting experience.*

## Vision

Kaijutsu's drift system enables **cognitive enhancement through multi-context collaboration** — pushing insights between contexts, pulling distilled summaries, merging work across parallel explorations. The server-side implementation is complete (DriftRouter, DriftEngine, all 9 drift RPC methods). But the client tells a different story:

- **ActorHandle** exists with 11 methods, Send+Sync, concurrent dispatch — but the app doesn't use it
- **ConnectionBridge** (1,557 lines) duplicates RPC dispatch with its own command/event enums
- Drift blocks render passively with a `🚂` prefix and dim color — no interactive drift UI exists
- The constellation shows context nodes but has no drift connection lines or fork-from-UI

This plan bridges that gap in four phases.

## Current vs Target Architecture

```
CURRENT                                    TARGET
┌─────────────────────┐                    ┌─────────────────────┐
│     Bevy App        │                    │     Bevy App        │
│                     │                    │                     │
│  ConnectionBridge   │                    │  ActorPlugin        │
│  ├─ 17 commands     │                    │  ├─ ActorHandle     │
│  ├─ 26 events       │                    │  ├─ ServerEvent     │
│  ├─ manual thread   │     ────────►      │  │  broadcast       │
│  └─ 1557 lines      │                    │  ├─ ConnectionState │
│                     │                    │  └─ ~300 lines      │
│  (ActorHandle       │                    │                     │
│   unused by app)    │                    │  Drift UI           │
│                     │                    │  ├─ context list    │
│  No drift UI        │                    │  ├─ drift queue     │
│                     │                    │  ├─ enhanced blocks │
│  Single context     │                    │  └─ constellation   │
│                     │                    │     drift lines     │
│                     │                    │                     │
│                     │                    │  Multi-context      │
│                     │                    │  ├─ constellation   │
│                     │                    │  │  as navigation   │
│                     │                    │  ├─ fork-from-UI    │
│                     │                    │  └─ per-ctx LLM     │
└─────────────────────┘                    └─────────────────────┘
```

## Phases

| Phase | Doc | Goal | Depends On |
|-------|-----|------|------------|
| 1 | [phase1-actor.md](phase1-actor.md) | Extend ActorHandle with macro + subscriptions + full coverage | — |
| 2 | [phase2-bridge.md](phase2-bridge.md) | Replace ConnectionBridge with ActorPlugin | Phase 1 |
| 3 | [phase3-drift-ui.md](phase3-drift-ui.md) | Drift commands, context list, enhanced rendering | Phase 2 |
| 4 | [phase4-multi-ctx.md](phase4-multi-ctx.md) | Constellation navigation, fork, per-context LLM | Phase 2, 3 |

```
Phase 1 ──► Phase 2 ──┬──► Phase 3
                       └──► Phase 4
                       (3 and 4 are partially parallel)
```

## ActorHandle Coverage Strategy

The Cap'n Proto schema defines **88 Kernel ordinals + 6 World ordinals**. ActorHandle does NOT wrap them 1:1 — it wraps what the app needs. Server-side operations (VFS, git, blob, config, MCP management, agents) stay behind kaish.

### Tier 1 — App Needs Now (current: 11, target: ~27)

| Category | Methods | Status |
|----------|---------|--------|
| Drift (6) | drift_push, drift_flush, drift_queue, drift_cancel, drift_pull, drift_merge | ✅ Done |
| Context (2) | list_all_contexts, get_context_id | ✅ Done |
| CRDT sync (2) | push_ops, get_document_state | ✅ Done |
| Tool exec (1) | execute_tool | ✅ Done |
| LLM (2) | prompt, shell_execute | ❌ Phase 1 |
| MCP tools (1) | call_mcp_tool | ❌ Phase 1 |
| Timeline (2) | fork_from_version, cherry_pick_block | ❌ Phase 1 |
| Context mgmt (4) | list_contexts, join_context, create_context, leave_seat | ❌ Phase 1 |
| World-level (2) | whoami, list_kernels | ❌ Phase 1 |
| Subscriptions (3) | subscribe_blocks, subscribe_mcp_resources, subscribe_mcp_elicitations | ❌ Phase 1 (broadcast pattern) |

### Tier 2 — Nice to Have

| Category | Methods | When |
|----------|---------|------|
| LLM config (3) | get_llm_config, set_default_provider, set_default_model | Phase 4 (early — Step 2) |
| Tool filter (2) | get_tool_filter, set_tool_filter | Phase 4 |
| Info (1) | get_info | Phase 2 (dashboard) |
| History (2) | get_document_history, get_command_history | Later |

### Tier 3 — Kaish-Only (~50 ordinals)

These stay server-side, accessed via kaish commands or MCP:
- VFS (@12-15): vfs, listMounts, mount, unmount
- Blob (@35-38): writeBlob, readBlob, deleteBlob, listBlobs
- Git (@39-46): registerRepo through setAttribution
- Config (@69-73): listConfigs through ensureSeatConfig
- MCP management (@27-29, @48-58): register/unregister/list MCP servers, prompts, roots, progress, logging, completion, cancellation
- Agents (@60-65): attach/list/detach/setCapabilities/invoke/subscribe
- Legacy/unused (@6-8): listEquipment, equip, unequip
- Lifecycle (@9-10): fork, thread (use fork_from_version instead)

## Status

| Phase | Status | Notes |
|-------|--------|-------|
| Phase 1 — ActorHandle | ✅ Complete | 36 methods, broadcast subscriptions, auto-reconnect |
| Phase 2 — Bridge replacement | ✅ Complete | ActorPlugin (~340 lines) replaces ConnectionBridge (1,302 lines) |
| Phase 3 — Drift UI | 🔄 In progress | Enhanced rendering, DriftState, context widget, constellation lines |
| Phase 4 — Multi-context | 🔲 Not started | |
