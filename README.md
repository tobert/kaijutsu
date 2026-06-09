# 会術 Kaijutsu

Kaijutsu is an AI agent system built around context forking and drifting, with some
experimental features for agentic music production. The core is the kaijutsu kernel,
which offers CRDT-based editing primitives to help users and multiple agents work
in parallel over unreliable networks. To make authentication simple and secure, kaijutsu
uses an embedded SSH server and ssh keys exclusively to identify users. 

The kaijutsu kernel maintains a DAG (directed acyclic graph) of contexts. Contexts can be forked
with different models, content redacted/repaired, and other changes that usually mean breaking
KV caches. Content can be sent across contexts with 'drifting'. Drifts are blocks of content
that a user or agent can send from one context to another, with the relationship tracked by
kaijutsu. This can be inspected and visualized in the app or over MCP.

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
cargo run -p kaijutsu-server -- add-key ~/.ssh/id_ed25519.pub --nick amy

# Terminal 1: Server
cargo run -p kaijutsu-server

# Terminal 2: Client
cargo run -p kaijutsu-app
```

## Crates

### kaijutsu-types

The relational foundation: typed IDs, principals, credentials, blocks, kernels,
and context metadata. Pure leaf crate with no internal kaijutsu dependencies —
read this first when learning the codebase.

### kaijutsu-kernel

The kernel wraps up your filesystem, models, MCPs, and contexts in a remote server
over ssh. It provides its own VFS, an MCP broker for tool dispatch, an LLM registry,
an agent registry, a drift router, and a pub/sub FlowBus. Contexts can be forked
any time, at which point the context can be edited and even switch models and tools.

### kaijutsu-crdt

Block-based CRDT document model built on [diamond-types-extended][dte]. Documents
are DAGs of blocks — each block is an independently-editable CRDT text buffer with
metadata (role, kind, status, parent). This is the shared state that all
participants (models, humans, scripts) edit concurrently.

### kaijutsu-server

SSH + Cap'n Proto RPC server. Handles authentication via SQLite-backed public
keys, runs EmbeddedKaish for shell command execution, and routes file I/O
through CRDT blocks via KaijutsuBackend.

### kaijutsu-client

RPC client library. `ActorHandle` provides a Send+Sync interface, broadcast
subscriptions for server events and connection status, and automatic
reconnection that re-registers subscriptions.

### kaijutsu-cas

Content-addressed blob store. Hash, stage, and seal binary content (images,
audio, attachments) by content hash, with metadata and references that point
into blocks.

### kaijutsu-agent-tools

Detects which AI coding tool (Claude Code, Gemini CLI, etc.) is hosting the
current process, by walking parent processes and reading session metadata.
Used to correlate kaijutsu sessions with their host agent.

### kaijutsu-abc

Parser and MIDI generator for ABC music notation. Produces a structured AST
plus SMF format 0 MIDI bytes. Used by `abc_block` so models can compose music
that renders as both standard staff notation and audio.

### kaijutsu-index

Semantic vector indexing — local ONNX embeddings, HNSW nearest-neighbor
search, and density-based clustering. No external API calls; runs fully
offline.

### kaijutsu-mcp

[MCP server][mcp] exposing the CRDT kernel to Claude Code, Gemini CLI, opencode,
and other MCP clients. Can run standalone (in-memory) or connected to
kaijutsu-server.

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
| [docs/design-notes.md](docs/design-notes.md) | Origin, terminology, design explorations |
| [docs/telemetry.md](docs/telemetry.md) | OpenTelemetry integration |
| [docs/abc-reference.md](docs/abc-reference.md) | ABC music notation reference |
| [docs/issues.md](docs/issues.md) | Live work items not yet in code |

## Forked Dependencies

| Fork | Why |
|------|-----|
| [diamond-types-extended][dte] | Completes Map/Set/Register types alongside Text CRDT |

[dte]: https://github.com/tobert/diamond-types-extended
