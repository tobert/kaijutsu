# Supporting crates

*Deep-dive companion to [README.md](README.md). Covers the smaller crates.
Code is truth: every pointer below names a symbol — `grep` it.*

---

## `kaijutsu-mcp` — the stdio MCP server

Standalone binary + lib exposing the kernel to agent clients (Claude Code, Gemini
CLI, opencode, Codex), and a one-shot hook client. `KaijutsuMcp` (`src/lib.rs:652`)
is the `rmcp` `ServerHandler`. A `Backend` enum (`:166`) abstracts in-process vs
SSH: **`Local(SharedBlockStore)`** keeps the kernel store directly; **`Remote`**
(`RemoteState`, `:337`) holds an `ActorHandle` and nothing else document-shaped —
there is no `SyncedDocument` and no mirror here. A `spawn_pulse_task` subscribes
the actor's event broadcast and bumps a `watch::Sender<u64>` generation counter
on every event; every reader (cold prompts/resources/completions, and the shell
completion poll) treats a bump only as "look again" and reads the server
directly over RPC. The prior design fetched a server snapshot to seed a
now-deleted `RemoteState.synced` mirror; nothing here builds or holds a document
at all today (`docs/crdt-position-2026-08.md`, "The mirror that stopped being a
mirror"). Tools: `shell`, `register_session`, `whoami`, `invoke_peer`,
`kaish_exec`, `list_kernel_tools`, and the input tools
(`read_input`/`write_input`/`edit_input`/`submit_input`). `HookListener`
(`hook_listener.rs:29`) is a Unix-socket server that turns Claude Code lifecycle
events into blocks and injects drift context into responses.

It is the **terminal consumer** — depends on `-kernel`, `-client`, `-types`,
`-agent-tools`, `-telemetry` (`-server` is a dev-only test dependency). Smells:
the `KAIJUTSU_MCP_TOOLS` (`hook_types.rs:176`) hook-dedup list must be
hand-synced against every real tool name or PostToolUse double-creates blocks
for a tool the MCP server already recorded.

## `kaijutsu-cas` — content-addressed store

BLAKE3-truncated 128-bit `ContentHash` (`hash.rs:15`), 32 hex chars, 2-char
directory sharding. `ContentStore` trait with one impl, `FileStore`
(`store.rs:83`): `{base}/objects/{prefix}/{remainder}` + optional JSON metadata
sidecars, and a staging→`seal()` pattern (EXDEV-safe rename→copy) for streaming
writes. Leaf crate (no in-repo deps). Used by `-hyoushigi` (re-exports
`ContentHash`) and `-kernel` (block blobs, images). Smells: **no refcounting/GC**
(`remove` is unconditional); object+metadata write isn't atomic; missing metadata
silently yields `application/octet-stream`.

## `kaijutsu-index` — semantic index

Fully local: embeds block text via pure-Rust ONNX inference (`rten`/`rten-tensor`
— there is no `ort` dependency, so no `download-binaries` air-gapped-build
hazard), stores vectors in an HNSW graph (`hnsw_rs`), maps slot↔context in
SQLite, supports density clustering. No external API calls. `SemanticIndex`
(`lib.rs:178`) is the entry point; `BlockSource` / `StatusReceiver` traits
(`:73`/`:86`) are seams the server implements to avoid a dep cycle. Depends only
on `-types`; used by `-kernel` and `-server`. Smells: the metadata lock is held
across ONNX inference (`lib.rs:347`, serializes index calls); `SearchResult.label`
is always `None` (`lib.rs:510`, `:544`). `rebuild()` (`:436`) is a real
slot-stable, never-reuse rebuild now, not a stub.

## `kaijutsu-agent-tools` — agent session detection

Detects the hosting AI tool. `AgentSession` trait (`lib.rs:26`) has two impls:
**`CodexSession`** (`codex.rs:14`), discovered from a nonempty `CODEX_THREAD_ID`
env var forwarded by kaijutsu's Codex integration (no process-walking — Codex
does not expose its active thread through a transcript file); **`ClaudeCodeSession`**
(`claude.rs:13`), discovered by walking the parent process for `CLAUDECODE=1`
and extracting session metadata, encoding cwd the way Claude Code does
(`/home/u/x → -home-u-x`) to scan `~/.claude/projects/{encoded}/*.jsonl`.
`detect()` (`lib.rs:50`) prefers Codex when both hosts' markers are present —
its thread id directly identifies the conversation, unlike `CLAUDECODE`. Leaf
crate; used by `-mcp`. Smells: Claude Code discovery silently falls back to
`minimal()` if the path convention changes; mtime-sorted transcript selection
is filesystem-dependent; no Gemini CLI or opencode detection despite `-mcp`
serving them.

## `kaijutsu-telemetry` — OpenTelemetry

Centralized OTel wiring: OTLP export (traces/logs/metrics), W3C trace-context
propagation across the SSH/capnp boundary, a tiered sampler, and GenAI-convention
LLM metrics. `OtelGuard` (`otel.rs:22`) is an RAII shutdown guard;
`inject_trace_context`/`extract_trace_context` (`lib.rs:68`, `:73`) bridge the
wire; `record_llm_usage` (`metrics.rs:443`) records token histograms. Leaf
crate; used by nearly everything. Smells: the Bevy path `Box::leak`s a
`tokio::runtime::Runtime` (`otel.rs:75`, never joined) and upcasts its
`EnterGuard` to `'static` — a soundness assumption that the leaked runtime
outlives the guard.

## `kaijutsu-hyoushigi` (拍子木) — the timing substrate

The shared-timeline-coordinate engine. A `Tick` (from `-types`) is a pure `i64`
logical coordinate (PPQ/beat counter), distinct from `order_key` (block ordering)
and `BlockId` (block identity). `Timeline` (`engine.rs:158`) holds a **playhead**
that only moves forward (`pump`/`advance_to`, `:400`), a future schedule, committed
cells, an in-memory CAS, and misprediction ledgers. A `Cell` (`cell.rs:114`) is
`{ span, body, state, track, played_by }`; `Body` (`:57`) is `Concrete(ContentRef)`
or `Deferred(Recipe)` (data, not a closure — serializable); `Fallback` (`:36`,
`Skip|UseLastGood|Literal`) is required. `CellState` (`:69`) transitions
(Pending→Speculating→…→Committed/Squashed/Failed) are enforced. `materialize(cell,
block_id)` (`materialize.rs:28`) stamps a caller-supplied `BlockId` onto the
produced `BlockSnapshot` — there is no `BlockId` inside this crate. Depends on
`-cas` + `-types`; used by `-kernel` + `-server` (the beat scheduler). Smells:
resolve runs synchronously inside `advance_to` (fine for ABC→MIDI, would block
on an LLM resolver); `content_before` (`resolver.rs:25`) is track-blind (the
`$HEARD` consumer isn't wired); `TickClock` (`:33`) collapses PPQ+tempo+epoch
into a single `ticks_per_sec: f64`.

## `kaijutsu-abc` — music notation

ABC parser → typed AST → MIDI (SMF format 0), SVG engraving, and round-trip
serialization. `parse`/`parse_with_mode` (Strict/Generous/Fragment), `to_midi`,
`to_abc`, `transpose`. Self-contained (no in-repo deps, no external deps); used by
`-kernel` (ABC→MIDI resolver) and `-app` (rendering ABC blocks). Smells: `to_abc`
silently drops `InlineField`/`Decoration`/`VoiceSwitch`; the tuplet writer omits
the optional `:r` count; the engrave subtree has no feature gate.

## `kaijutsu-viz` — visualization substrate

Pure, dependency-free D3-style toolkit; first/sole consumer is the time-well
browser in `-app`. `ScaleLinear`/`ScaleTime`/`ScaleThreshold`/`RadialBands`
(`scales.rs`, `RadialBands` unused by the current layout path — the terracing
in `kaijutsu-app`'s `view/time_well/card.rs` divides its own radius/depth
envelope directly instead), and a `Join` keyed data-join reconciler
(`join.rs:131`) that structurally separates layout cadence from data cadence.
Leaf crate (proptest dev-only); used by `-app`.

Ring placement is **explicit seating, not derived idle-age banding**:
`assign_ring_seats` (`layout.rs:177`) seats up to `RING_SLOTS` (10) contexts per
ring from each context's kernel-stamped `promoted_at`/`demoted_at`/`concluded_at`,
producing `Band::Active` (ring 0, stable seats ordered by `promoted_at`),
`Band::Recent` (ring 1, the ten most-recently-active non-concluded contexts of
the rest), and an unseated `horizon` list rendered as a "+N" count — demoted,
concluded, and overflow contexts all land there alike. There is no `now`
parameter and no age math anywhere in this module (`docs/timewell.md`, "Ring
membership becomes explicit").
