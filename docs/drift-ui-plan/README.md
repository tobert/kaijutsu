# Drift UI Convergence Plan

*Converging the Bevy client onto ActorHandle and building the multi-context drifting experience.*

## Vision

Kaijutsu's drift system enables **cognitive enhancement through multi-context collaboration** — pushing insights between contexts, pulling distilled summaries, merging work across parallel explorations. The server-side implementation is complete (DriftRouter, DriftEngine, all 9 drift RPC methods).

Phases 1–3 are complete. The client now uses ActorHandle exclusively, drift blocks render with variant-specific formatting, and the constellation shows drift-aware connections. Phase 4 (multi-context navigation) remains.

## Architecture (Post Phase 3)

```
┌─────────────────────────────────────────────────┐
│                   Bevy App                       │
│                                                  │
│  ActorPlugin (~340 lines)                        │
│  ├─ ActorHandle (36 methods, Send+Sync)          │
│  ├─ ServerEvent broadcast                        │
│  └─ ConnectionStatus broadcast                   │
│                                                  │
│  DriftPlugin                                     │
│  ├─ DriftState (contexts, staged, notifications) │
│  ├─ 5s periodic polling via ActorHandle           │
│  └─ Drift arrival detection from ServerEvents    │
│                                                  │
│  Drift UI                                        │
│  ├─ Variant-specific block rendering             │
│  │   (Push ←/→, Pull/Distill boxed,             │
│  │    Merge ⇄, Commit 📝)                        │
│  ├─ Context list widget (south dock)             │
│  ├─ Drift notification flash (5s auto-dismiss)   │
│  └─ Constellation drift-aware connections        │
│      (ancestry lines, staged drift lines)        │
│                                                  │
│  TODO: Multi-context (Phase 4)                   │
│  ├─ Constellation as navigation                  │
│  ├─ Fork-from-UI                                 │
│  └─ Per-context LLM config                       │
└─────────────────────────────────────────────────┘
```

## Phases

| Phase | Doc | Goal | Depends On |
|-------|-----|------|------------|
| 1 | [phase1-actor.md](phase1-actor.md) | Extend ActorHandle with macro + subscriptions + full coverage | — |
| 2 | [phase2-bridge.md](phase2-bridge.md) | Replace ConnectionBridge with ActorPlugin | Phase 1 |
| 3 | [phase3-drift-ui.md](phase3-drift-ui.md) | Drift rendering, context widget, constellation lines | Phase 2 |
| 4 | [phase4-multi-ctx.md](phase4-multi-ctx.md) | Constellation navigation, fork, per-context LLM | Phase 2, 3 |

```
Phase 1 ──► Phase 2 ──┬──► Phase 3
                       └──► Phase 4
                       (3 and 4 are partially parallel)
```

## ActorHandle Coverage

The Cap'n Proto schema defines **88 Kernel ordinals + 6 World ordinals**. ActorHandle wraps what the app needs (36 methods). Server-side operations (VFS, git, blob, config, MCP management, agents) stay behind kaish.

### Tier 1 — App Needs (36 methods, all complete)

| Category | Methods | Status |
|----------|---------|--------|
| Drift (6) | drift_push, drift_flush, drift_queue, drift_cancel, drift_pull, drift_merge | ✅ |
| Context (2) | list_all_contexts, get_context_id | ✅ |
| CRDT sync (2) | push_ops, get_document_state | ✅ |
| Tool exec (1) | execute_tool | ✅ |
| LLM (2) | prompt, shell_execute | ✅ |
| MCP tools (1) | call_mcp_tool | ✅ |
| Timeline (2) | fork_from_version, cherry_pick_block | ✅ |
| Context mgmt (4) | list_contexts, join_context, create_context, leave_seat | ✅ |
| World-level (2) | whoami, list_kernels | ✅ |
| Subscriptions (3) | subscribe_blocks, subscribe_mcp_resources, subscribe_mcp_elicitations | ✅ |
| LLM config (3) | get_llm_config, set_default_provider, set_default_model | ✅ |
| Tool filter (2) | get_tool_filter, set_tool_filter | ✅ |
| Info + history (4) | get_info, get_document_history, get_command_history, list_my_seats | ✅ |

### Tier 2 — Kaish-Only (~50 ordinals)

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
| Phase 3 — Drift UI | ✅ Complete | Variant rendering, DriftState polling, context widget, constellation drift lines, notifications |
| Phase 4 — Multi-context | 🔲 Not started | Constellation navigation, fork-from-UI, per-context LLM |
