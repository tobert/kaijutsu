# 会術 Kaijutsu

## Developer Notes from Amy 2026-06-26

Kaijutsu started as a "more serious" version of an [SSH MUD](https://github.com/tobert/sshwarma)
that nerdsniped me as I built out an equipment system for models. That led to having rooms have
tools in them too. The system felt cool but then I had to face the context problem: how do I
bound context for each model? How should I compact for them? Could it be customized by role, room,
and other dimensions?

I was also working on variations of [hootenanny](https://github.com/tobert/hootenanny)
which was a big pile of ideas and experiments while I learned about music models, real-time
sound, and a few other things. I've retired it. I learned a lot but it was time to start over.

Kaijutsu is a maximal project, where I spend my leisure coding time building something
ambitious and occasionally whimsical. Which is to say, it's turned into an operating
system that lives in a process. It has its own shell, coreutils, and default
assumptions around concurrent change by multiple agents and users, making it more like
a shared game world than a typical developer tool. Also unlike developer tools, contexts
can have a beat, and features exist for sliding window contexts with KV cache optimizations.

As I write this developer note, it's been about 6 months since the project started with "what
if my agent had a bevy frontend and its own shell" has turned into kaish maturing rapidly as
part of [kaibo](https://github.com/tobert/kaibo). In a lot of ways kaibo is a more pragmatic
take on a lot of what I've explored in kaijutsu so far.

The curious are welcome to give it a try, but I wouldn't call this ready for consumption yet.
The idea blender is still whirring and only the fast and the foolish should put their hands in
at this point. If that's your jam, welcome, find me on Bluesky as @renice.bsky.social
or open an issue on Github.

## Introduction

Kaijutsu is an AI agent system built around context forking and drifting, with some
experimental features for agentic music production. The core is the kaijutsu kernel,
which offers CRDT-based editing primitives to help users and multiple agents work
in parallel over unreliable networks. To make authentication simple and secure, kaijutsu
uses an embedded SSH server and ssh keys exclusively to identify users. 

The stance behind all of it: kaijutsu is an instrument, not a harness. You play
it, a model plays it, and if you hand someone a connected app they play it too —
many hands on one keyboard. The kernel is the instrument's body: it supplies what
a turn needs and doesn't play the turn itself.

The kaijutsu kernel maintains a DAG (directed acyclic graph) of contexts. Contexts can be forked
with different models, content redacted/repaired, and other changes that usually mean breaking
KV caches. Content can be sent across contexts with 'drifting'. Drifts are blocks of content
that a user or agent can send from one context to another, with the relationship tracked by
kaijutsu. This can be inspected and visualized in the app or over MCP.

## Status

**Kaijutsu is not released yet**. The kernel feels solid and reliable, and
diamond-types-extended seems to be stable. The UI is coming along.

You may need my branch of kaish for this to build. Kaish will go back to
cargo versions soon.

-Amy

## Quick Start

```bash
# First time: add your SSH key
cargo run -p kaijutsu-server -- add-key ~/.ssh/id_ed25519.pub --nick amy

# ...or bulk-import an existing authorized_keys file
cargo run -p kaijutsu-server -- import ~/.ssh/authorized_keys

# Check what's registered
cargo run -p kaijutsu-server -- list-users
cargo run -p kaijutsu-server -- list-keys amy

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

The kernel holds your filesystem, models, MCPs, and contexts behind a remote SSH
server — the shared body everyone plays. It offers its own VFS, an MCP broker for
tool dispatch, an LLM registry, a drift router, and a pub/sub FlowBus. Contexts can
be forked any time, at which point the context can be edited and even switch models
and tools.

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
| [docs/instrument-design.md](docs/instrument-design.md) | The instrument stance — principles for system-message design |
| [docs/telemetry.md](docs/telemetry.md) | OpenTelemetry integration |
| [docs/abc-reference.md](docs/abc-reference.md) | ABC music notation reference |
| [docs/issues.md](docs/issues.md) | Live work items not yet in code |

## Forked Dependencies

| Fork | Why |
|------|-----|
| [diamond-types-extended][dte] | Completes Map/Set/Register types alongside Text CRDT |

[dte]: https://github.com/tobert/diamond-types-extended
