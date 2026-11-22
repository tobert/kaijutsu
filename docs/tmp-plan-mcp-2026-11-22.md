# MCP 2026-07-28 & Service Mode Plan

Date: 2026-11-22
Status: Complete (all 4 slices merged to `main`)
Target SDK: `rmcp 3.11.0` (MCP `2026-07-28`, backward-compatible with `2025-11-25`)

## Goals and decisions

1. **Architecture (Option A):** `kaijutsu-mcp` remains the standalone MCP bridge over SSH to `kaijutsu-server`. SSH stays Kaijutsu's boundary.
2. **Subscriptions first, hooks as fallback:** Implement MCP `2026-07-28` `subscriptions/listen` with coalesced drift and event delivery for capable clients (including future cross-kernel MCP communication). Keep lifecycle hooks as the fallback when a client (Claude Code, Antigravity) does not maintain a `subscriptions/listen` stream, and suppress hook drift injection when a live subscription already covers that context's drift.
3. **Service mode (UDS + optional loopback HTTP):** Allow `kaijutsu-mcp` to run as a long-lived service listening on a Unix domain socket (`--listen-unix`) and/or loopback HTTP (`--listen-http`), while keeping stdio as the default when neither flag is set. Mount `POST /hook` on the same router so hooks talk to the service instead of per-PID sockets.
4. **Antigravity (`agy`) support:** `agy` supports Stdio + Streamable HTTP (`url`) and `hooks.json` lifecycle hooks (camelCase JSON). Add `NativeHookSource::Antigravity` so `agy` gets hook mirroring and drift injection too.
5. **Elicitation & MRTR:** Support protocol-aware elicitation (`InputRequiredResult` on `2026-07-28+`, `peer.elicit` on `2025-11-25`), and wire `create_elicitation` on the `kaijutsu-kernel` broker client.
6. **Tasks & cancellation:** Wire `rmcp::task_manager::TaskManager` (SEP-2663) and real `on_cancelled` handling.
7. **Deferred:** Skills extension (SEP-2640).

## Slices

### Slice 1 — Kernel MCP Client: UDS Transport & `subscriptions/listen`
- Status: **Merged** (`mcp-slice1-kernel-client`, `97ac0240`)
- Scope (`crates/kaijutsu-kernel`, `Cargo.toml`):
  - [x] Enable `transport-streamable-http-client-unix-socket` on workspace `rmcp`.
  - [x] Add `McpTransport::UnixSocket` (`transport = "unix_socket"`, `socket = "..."`, optional `url`) in `mcp/servers/external.rs` and `mcp/toml.rs`.
  - [x] Upgrade `ExternalMcpServer::subscribe` / `unsubscribe` to use `subscriptions/listen` when negotiated protocol `>= 2026-07-28`, keeping legacy `resources/subscribe` for `2025-11-25`.
  - [x] TDD tests: `toml` parsing/validation, UDS handshake + tool call, `subscriptions/listen` delivering `ResourceUpdated` to `notif_tx`.

### Slice 2 — `kaijutsu-mcp` Server: Resource Templates, Coalescer, & `subscriptions/listen`
- Status: **Merged** (`mcp-slice2-subscriptions`, `66f065fe`)
- Scope (`crates/kaijutsu-mcp`):
  - [x] Implement `list_resource_templates` (`kaijutsu://docs/{id}`, `kaijutsu://blocks/{id}/{key}`, `kaijutsu://contexts/{id}/drift`, `kaijutsu://contexts/{id}/turns`, `kaijutsu://contexts/{id}/events`).
  - [x] Slim `list_resources` so it no longer fetches all blocks of all contexts.
  - [x] Add `src/subscriptions.rs`: per-URI debounce coalescer, bounded per-context turn/event ring buffer, and `has_drift_subscriber(context_id)` tracking.
  - [x] Upgrade `spawn_pulse_task` to route `ServerEvent`s into the coalescer and wake `listen()` subscribers.
  - [x] Implement `read_resource` for `/drift`, `/turns`, and `/events`.
  - [x] Implement `accepted_subscription_filter` and `listen` on `KaijutsuMcp`.
  - [x] TDD tests: in-memory `rmcp` `2026-07-28` client subscribing via `subscriptions/listen`, receiving coalesced `notifications/resources/updated`, and reading `/events`, `/turns`, and `/drift`.

### Slice 3 — `kaijutsu-mcp` Service Mode (UDS + Loopback HTTP), Unified `/hook`, & `agy` Hook Adapter
- Status: **Merged** (`mcp-slice3-service-mode`, `5d29e5f0`)
- Scope (`crates/kaijutsu-mcp`):
  - [x] Enable `transport-streamable-http-server` on `rmcp` in `kaijutsu-mcp`.
  - [x] Add `--listen-unix <path>` and `--listen-http <addr>` flags (stdio remains default).
  - [x] Serve `StreamableHttpService<KaijutsuMcp, LocalSessionManager>` at `/mcp` and `POST /hook` over UDS (`hyper` HTTP/1.1 + `UnixListener`) and optional loopback TCP.
  - [x] Share kernel `ActorHandle` across sessions in service mode with per-session context binding and `session_id` routing for `/hook`.
  - [x] Suppress hook drift injection when `has_drift_subscriber(context_id)` is true.
  - [x] Update `kaijutsu-mcp hook` client to POST to UDS `/hook` (with fallback to legacy `hook-{pid}.sock`).
  - [x] Add `NativeHookSource::Antigravity` in `hook_adapter.rs` / `main.rs`.
  - [x] TDD tests: end-to-end UDS `UnixSocketHttpClient` + `subscriptions/listen` + `/hook` + `Antigravity` adapter tests.

### Slice 4 — Elicitation / MRTR, Tasks (SEP-2663), & Cancellation
- Status: **Merged** (4a `mcp-slice4a-server-mrtr-tasks` `e19f6c5b` + 4b `mcp-slice4b-kernel-elicitation-tasks` `0a3033b3`)
- Scope (`crates/kaijutsu-mcp`, `crates/kaijutsu-kernel`):
  - [x] Server (`kaijutsu-mcp`): enable `request-state` on `rmcp`, add protocol-aware elicitation helper (MRTR `InputRequiredResult` on `2026-07-28+`, `peer.elicit` on `2025-11-25`, plain-text fallback).
  - [x] Server (`kaijutsu-mcp`): wire `TaskManager` (`tasks/get`, `tasks/update`, `tasks/cancel`) for `shell` when client declares `tasks` capability, and wire `on_cancelled` to interrupt in-flight shell execution.
  - [x] Client (`kaijutsu-kernel`): enable `elicitation` and `tasks` capabilities in `BrokerClientHandler` and route `create_elicitation` cleanly (`ElicitationResponder` + MRTR + SEP-2663 task polling/update/cancel).
  - [x] TDD tests for MRTR, tasks, and cancellation.

## Progress Log

- 2026-11-22: Audited `rmcp 3.11.0`, cloned upstream repo to `~/src/research/rmcp`, confirmed `agy` MCP/hook capabilities, wrote plan, launched Slices 1 & 2 in parallel worktrees.
- 2026-11-22: Merged Slice 2 (`66f065fe`), Slice 1 (`97ac0240`), and Slice 3 (`5d29e5f0`) into `main`. Launched Slices 4a and 4b in parallel worktrees.
- 2026-11-22: Merged Slice 4a (`e19f6c5b`) and Slice 4b (`0a3033b3`) into `main`. All 4 slices verified together on `main`.
- Signoff reminder: Remind Amy to have Gemini 4 Neon take a shot at `kaish` stack usage reductions.
