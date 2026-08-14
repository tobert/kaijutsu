# Experimental Codex app-server backend

> **Status (2026-08-14): phase 0, connect-only transport checkpoint.** The
> wire and provider are implemented against `codex-cli 0.147.0`. Amy
> authorized a kernel-managed sidecar as the target; lifecycle ownership and
> the context-session adapter are the next slice. The backend is deliberately
> not seeded or selected by default yet.

Kaijutsu can use a user-managed Codex app-server as an experimental LLM
backend without adding another kernel subprocess launcher. Start the sidecar
outside the kernel, then configure its WebSocket endpoint:

```sh
codex app-server --listen ws://127.0.0.1:4500
kj backend set codex --kind codex-app --base-url ws://127.0.0.1:4500
kj backend model set codex <model-id>
```

The provider opens a fresh connection per Kaijutsu completion, performs
`initialize` / `initialized`, starts a Codex thread and turn, and translates
agent-message and reasoning-summary deltas into Kaijutsu `StreamEvent`s.
Unknown notifications are tolerated. All server-initiated approval,
permission, elicitation, and user-input requests are declined; the thread is
explicitly requested with `sandbox=read-only` and
`approvalPolicy=untrusted`, rather than inheriting sidecar defaults.

## Phase-0 semantics

This is intentionally a **stateless consultation backend**, not yet a native
Codex session projection:

- The hydrated Kaijutsu message history is flattened into one labeled text
  input for every completion. A fresh Codex thread is created each time.
- The Kaijutsu system/rc prompt maps to Codex `developerInstructions`; this augments
  Codex's own instruction stack and is not byte-identical provider semantics.
- Model and reasoning effort are forwarded. Temperature, top-p, maximum output
  tokens, cache breakpoints, images, and Kaijutsu tool definitions are not.
- Raw Codex reasoning is ignored; only reasoning-summary deltas are projected.
- Cancellation currently drops the connection when the outer turn is torn
  down; `ProviderStream::cancel()` cannot yet send the asynchronous
  `turn/interrupt` request.

## Why phase 0 started connect-only

Codex app-server owns its built-in shell/file/MCP loop. It does not hand an
ordinary command back to the host for execution; it only asks the client for
approval. Spawning it from the provider would also create a second ad-hoc host
exec site. Both conflict with Kaijutsu's rule that kaish owns host execution,
whose only existing process-launch exception is config-driven MCP stdio
servers. Phase 0 therefore connects to a separately owned sidecar and refuses
all action escalation. The authorized target is one kernel-owned Codex
sidecar. That is a deliberate expansion of the process-owner design, not
permission for providers to grow arbitrary `Command::new` sites: one sidecar
supervisor must own launch, readiness, restart, stderr, and reaping.

The target tool path is stronger than merely approving Codex's native shell:
disable Codex's built-in shell/unified-exec/patch tools through its thread
config, advertise Kaijutsu's context-bound broker tools as app-server
`dynamicTools`, and answer `item/tool/call` by invoking the broker. This gives
Codex the same shell implementation as every other model—`builtin.shell`
through EmbeddedKaish—without spawning a loopback `kaijutsu-mcp` process.
`kaijutsu-mcp` remains the equivalent external-harness surface; dynamic tools
are the in-kernel adapter over the same definitions and calls.

## Next slices

1. Add one kernel sidecar supervisor, enabled only when a `codex-app` backend
   exists: launch/reap app-server, wait for readiness, restart with bounded
   backoff, and expose one authenticated loopback endpoint.
2. Add durable `(context, backend) -> Codex thread` identity, then map context
   create/resume/fork boundaries to `thread/start` / `thread/resume` /
   `thread/fork` rather than flattening history.
3. Carry context identity, host cwd, and triggering block identity through the
   provider request seam; use the block id as Codex `clientUserMessageId`.
4. Project context-bound broker tools as Codex dynamic tools, disabling its
   native shell/unified-exec/patch surfaces. Route calls through the broker so
   shell execution remains kaish-owned.
5. Add usage projection from `thread/tokenUsage/updated` and asynchronous
   `turn/interrupt` cancellation.
6. Replace stateless history flattening with boundary-aware Codex thread
   start/resume/fork. The Kaijutsu block list remains the durable shared model;
   a Codex thread is a live projection, not a second authority over it.
