# Experimental Codex app-server backend

> **Status (2026-08-14): phase 0, connect-only backend with broker tools.** The
> wire and provider are implemented against `codex-cli 0.147.0`. Amy
> authorized a persistent sidecar as the target; systemd now owns its lifecycle
> and the context-session adapter is the next slice. The backend is deliberately
> not seeded or selected by default yet.

Kaijutsu can use a user-managed Codex app-server as an experimental LLM
backend without adding another kernel subprocess launcher. Start the sidecar
outside the kernel, then configure its WebSocket endpoint:

```sh
codex app-server --listen ws://127.0.0.1:4500
kj backend set codex --kind codex-app --base-url ws://127.0.0.1:4500
kj backend model set codex <model-id>
```

On a persistent Linux host, install the repo-owned systemd user unit instead
of keeping that command in a terminal:

```sh
./contrib/install-codex-app-server-systemd.sh
```

The generated unit resolves the currently active `codex` binary, listens only
on `127.0.0.1:4500`, and exports Codex OTel logs, traces, and metrics over
OTLP/gRPC to the local collector on port 4317. Raw user-prompt logging remains
disabled. The unit owns sidecar restart and reaping; the kernel remains a
client of a configured local service.

The provider opens a fresh connection per Kaijutsu completion, performs
`initialize` / `initialized`, starts a Codex thread and turn, and translates
agent-message and reasoning-summary deltas into Kaijutsu `StreamEvent`s.
Unknown notifications are tolerated. All context-visible broker tools are
registered through app-server's experimental `dynamicTools` API. An
`item/tool/call` pauses the Codex stream while the server writes the ordinary
ToolCall/ToolResult pair and dispatches through the context-bound broker; in
particular, the shell tool still reaches `EmbeddedKaish`. Other
server-initiated approval, permission, elicitation, and user-input requests
are declined. The thread is requested with `sandbox=read-only` and
`approvalPolicy=untrusted`, and its config forcibly disables Codex's native
shell, unified-exec, and freeform patch tools.

## Phase-0 semantics

This is intentionally a **stateless consultation backend**, not yet a native
Codex session projection:

- The hydrated Kaijutsu message history is flattened into one labeled text
  input for every completion. A fresh Codex thread is created each time.
- The Kaijutsu system/rc prompt maps to Codex `developerInstructions`; this augments
  Codex's own instruction stack and is not byte-identical provider semantics.
- Model, reasoning effort, and Kaijutsu tool definitions are forwarded.
  Temperature, top-p, maximum output tokens, cache breakpoints, and images are
  not.
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
all action escalation. A systemd user unit owns launch, restart, logs, and
reaping; providers do not grow arbitrary `Command::new` sites.

The implemented tool path is stronger than merely approving Codex's native
shell: Codex's built-in shell/unified-exec/patch tools are disabled through
its thread config, Kaijutsu's context-bound broker tools are advertised as
app-server `dynamicTools`, and `item/tool/call` is answered by invoking the
broker. This gives Codex the same shell implementation as every other
model—`builtin.shell` through EmbeddedKaish—without spawning a loopback
`kaijutsu-mcp` process.
`kaijutsu-mcp` remains the equivalent external-harness surface; dynamic tools
are the in-kernel adapter over the same definitions and calls.

## Next slices

1. Add durable `(context, backend) -> Codex thread` identity, then map context
   create/resume/fork boundaries to `thread/start` / `thread/resume` /
   `thread/fork` rather than flattening history.
2. Carry context identity, host cwd, and triggering block identity through the
   provider request seam; use the block id as Codex `clientUserMessageId`.
3. Add a live app-server integration test for the experimental dynamic-tool
   round trip, including interruption while a broker call is in flight.
4. Add usage projection from `thread/tokenUsage/updated` and asynchronous
   `turn/interrupt` cancellation.
5. Replace stateless history flattening with boundary-aware Codex thread
   start/resume/fork. The Kaijutsu block list remains the durable shared model;
   a Codex thread is a live projection, not a second authority over it.
