# 会術 Kaijutsu

*"The Art of Meeting"*

Kaijutsu is an agent system with a graphical UI that manages contexts in a directed
acyclic graph (DAG). The DAG supports `fork` and `drift` operations that simplify
breaking down work as a context develops, and then merging the results, while keeping
provenance intact.

## Features

- Cybernetics-focused design for users and models, aiming to make the flow between
system components as smooth as possible rather than minimizing or trying to eliminate it.
- *Multi-Context Multi-Model Drifting*: a context can be forked to explore
an idea then a summary drifted back to the parent.
- Conflict-Free Replicated Data Types (CRDT) via [dte](https://github.com/tobert/diamond-types-extended)
allows safe concurrent edits across workspaces, network interruptions, and *multiple users*.
- Transport is SSH with ssh key authentication. Works with ssh-agent.
- Binary Cap'n Proto protocol over SSH
- User interface built on Bevy and Vello for high performance and beautiful vectors.

## Status

**Kaijutsu is not released yet**. The kernel feels solid and reliable, and
diamond-types-extended seems to be stable. The UI is coming along.

You may need my branches of bevy_vello and kaish for this to build. Kaish
will go back to cargo versions soon, and I'm working to upstream the bevy_vello
patches.

-Amy

## Quick Start

```bash
# First time: add your SSH key
cargo run -p kaijutsu-server -- add-key ~/.ssh/id_ed25519.pub --nick amy --admin

# Terminal 1: Server
cargo run -p kaijutsu-server

# Terminal 2: Client
cargo run -p kaijutsu-app
```

## Crates

### kaijutsu-kernel

The kernel wraps up your filesystem, models, MCPs, and contexts in a remove server
over ssh. It provides its own VFS, a tool registry, an LLM registry, an agent registry,
a drift router, and a pub/sub FlowBus. Contexts can be forked any time, at which point
the context can be edited and even switch models and tools.

### kaijutsu-crdt

Block-based CRDT document model built on [diamond-types-extended][dte]. Documents
are DAGs of blocks — each block is an independently-editable CRDT text buffer with
metadata (role, kind, status, parent). This is the shared state that all
participants (models, humans, scripts) edit concurrently.

### kaijutsu-server

SSH + Cap'n Proto RPC server (87 Kernel methods + 4 World methods). Handles
authentication via SQLite-backed public keys, runs EmbeddedKaish for shell
command execution, and routes file I/O through CRDT blocks via KaijutsuBackend.

### kaijutsu-client

RPC client library. `ActorHandle` provides a Send+Sync interface with 34 methods,
broadcast subscriptions for server events and connection status, and automatic
reconnection that re-registers subscriptions.

### kaijutsu-mcp

[MCP server][mcp] exposing the CRDT kernel to Claude Code, Gemini CLI, opencode,
and other MCP clients. 25 tools across documents, blocks, drift, execution, and
identity. Can run standalone (in-memory) or connected to kaijutsu-server.

```bash
cargo run -p kaijutsu-mcp
```

See [crates/kaijutsu-mcp/README.md](crates/kaijutsu-mcp/README.md) for tool
documentation and configuration.

[mcp]: https://modelcontextprotocol.io/

### kaijutsu-telemetry

OpenTelemetry integration behind a `telemetry` feature flag. W3C TraceContext
propagation through Cap'n Proto RPC, differentiated sampling rates via
`KaijutsuSampler`, standard OTel envvars (`OTEL_EXPORTER_OTLP_ENDPOINT`).

### kaijutsu-app

Bevy 0.18 GUI client with custom MSDF text rendering, vim-style focus-based
input, a tiling window manager, and a constellation view for navigating
contexts as a radial tree graph. See
[crates/kaijutsu-app/README.md](crates/kaijutsu-app/README.md) for details on
text rendering, theming, and the UI architecture.

<!-- TODO: screenshot -->

## Documentation

| Doc | Purpose |
|-----|---------|
| [docs/kernel-model.md](docs/kernel-model.md) | **Authoritative kernel model — start here** |
| [docs/drift.md](docs/drift.md) | Cross-context communication design |
| [docs/block-tools.md](docs/block-tools.md) | CRDT block interface |
| [docs/diamond-types-fork.md](docs/diamond-types-fork.md) | Why we forked diamond-types |
| [docs/telemetry.md](docs/telemetry.md) | OpenTelemetry integration |
| [docs/design-notes.md](docs/design-notes.md) | Collected design explorations |

## Forked Dependencies

| Fork | Why |
|------|-----|
| [diamond-types-extended](https://github.com/tobert/diamond-types-extended) | Completes Map/Set/Register types alongside Text CRDT |
