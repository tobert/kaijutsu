# Supporting crates

*Deep-dive companion to [README.md](README.md). Covers the eight smaller crates.
Code is truth; verified 2026-06-16.*

---

## `kaijutsu-mcp` — the stdio MCP server

Standalone binary + lib exposing the kernel to agent clients (Claude Code, Gemini
CLI, opencode), and a one-shot hook client. `KaijutsuMcp` (`src/lib.rs:303`) is the
`rmcp` `ServerHandler`. A `Backend` enum (`:134`) abstracts in-process vs SSH:
**`Local(SharedBlockStore)`** keeps the kernel store directly; **`Remote`** holds an
`ActorHandle` + a single `SyncedDocument` driven by a sole-writer event listener
on a `Notify` (the fix for the dropped-stdout bug — see memory
`project_mcp_synceddocument_sync`). Tools: `shell`, `context_shell`,
`register_session`, `whoami`, `invoke_peer`, `kaish_exec`, `list_kernel_tools`,
and the input tools (`read`/`write`/`edit`/`submit`). `HookListener`
(`hook_listener.rs:29`) is a Unix-socket server that turns Claude Code lifecycle
events into CRDT blocks and injects drift context into responses.

It is the **terminal consumer** — depends on `-kernel`, `-server`, `-client`,
`-crdt`, `-types`, `-agent-tools`, `-telemetry`. Smells: op-count estimated as
`bytes/50`; Phase-2 full-snapshot fetch per shell command (per-block RPC is a
TODO); the `KAIJUTSU_MCP_TOOLS` dedup list must be hand-synced and has stale names.

## `kaijutsu-cas` — content-addressed store

BLAKE3-truncated 128-bit `ContentHash` (`hash.rs:9`), 32 hex chars, 2-char
directory sharding. `ContentStore` trait with one impl, `FileStore`
(`store.rs:83`): `{base}/objects/{prefix}/{remainder}` + optional JSON metadata
sidecars, and a staging→`seal()` pattern (EXDEV-safe rename→copy) for streaming
writes. Leaf crate (no in-repo deps). Used by `-hyoushigi` (re-exports
`ContentHash`) and `-kernel` (block blobs, images). Smells: **no refcounting/GC**
(`remove` is unconditional); object+metadata write isn't atomic; missing metadata
silently yields `application/octet-stream`.

## `kaijutsu-index` — semantic index

Fully local: embeds block text via ONNX (`ort`), stores vectors in an HNSW graph
(`hnsw_rs`), maps slot↔context in SQLite, supports density clustering. No external
API calls. `SemanticIndex` (`lib.rs:117`) is the entry point; `BlockSource` /
`StatusReceiver` traits (`:73`/`:86`) are seams the server implements to avoid a
dep cycle. Depends only on `-types`; used by `-kernel` and `-server`. Smells:
`rebuild()` is a stub (dead HNSW slots accumulate forever); the metadata lock is
held across ONNX inference (serializes index calls); `SearchResult.label` is always
`None`; `ort` uses `download-binaries` (breaks air-gapped builds).

## `kaijutsu-agent-tools` — agent session detection

Detects the hosting AI tool by walking the parent process and extracting session
metadata. `AgentSession` trait + `ClaudeCodeSession` (`claude.rs:13`), which
encodes cwd the way Claude Code does (`/home/u/x → -home-u-x`) and scans
`~/.claude/projects/{encoded}/*.jsonl`. `detect()` is the sole entry. Leaf crate;
used by `-mcp`. Smells: only Claude Code detected; discovery silently falls back to
`minimal()` if the path convention changes; mtime-sorted selection is filesystem-
dependent.

## `kaijutsu-telemetry` — OpenTelemetry

Centralized OTel wiring: OTLP export (traces/logs/metrics), W3C trace-context
propagation across the SSH/capnp boundary, a tiered sampler, and GenAI-convention
LLM metrics. `OtelGuard` (`otel.rs:23`) is an RAII shutdown guard;
`inject_trace_context`/`extract_trace_context` bridge the wire; `record_llm_usage`
records token histograms. Leaf crate; used by nearly everything. Smells: the Bevy
path leaks a `tokio::runtime::Runtime` (never joined) and upcasts its
`EnterGuard` to `'static` — a soundness assumption that the leaked runtime
outlives the guard.

## `kaijutsu-hyoushigi` (拍子木) — the timing substrate

The shared-timeline-coordinate engine. A `Tick` (from `-types`) is a pure `i64`
logical coordinate (PPQ/beat counter), distinct from `order_key` (CRDT ordering)
and `BlockId` (CRDT identity). `Timeline` (`engine.rs:154`) holds a **playhead**
that only moves forward (`pump`/`advance_to`), a future schedule, committed cells,
an in-memory CAS, and misprediction ledgers. A `Cell` (`cell.rs:114`) is `{ span,
body, state, track, played_by }`; `Body` is `Concrete(ContentRef)` or
`Deferred(Recipe)` (data, not a closure — serializable); `Fallback`
(`Skip|UseLastGood|Literal`) is required. `CellState` transitions
(Pending→Speculating→…→Committed/Squashed/Failed) are enforced. `materialize(cell,
block_id)` stamps a caller-supplied `BlockId` onto the produced `BlockSnapshot` —
there is no `BlockId` inside this crate. Depends on `-cas` + `-types`; used by
`-kernel` + `-server` (the beat scheduler). Smells: resolve runs synchronously
inside `advance_to` (fine for ABC→MIDI, would block on an LLM resolver);
`content_before` is track-blind (the `$HEARD` consumer isn't wired); `TickClock`
collapses PPQ+tempo+epoch into one f64 (stub).

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
(`scales.rs`, `RadialBands` unused by the current layout path), `assign_idle_band`
(`layout.rs`, pure per-context classification, no stored state) over four
idle-age bands (`HotNow|ThisWeek|ThirtyDays|Horizon`) derived from
`now − last_activity_at`, and a `Join` keyed data-join reconciler
(`join.rs`) that structurally separates layout cadence from data cadence. Leaf
crate (proptest dev-only); used by `-app`. `CompactingBandLayout`/`LayoutPos`
(the earlier concrete radial-position layout, along with the `order_key: i64`
slot-assignment scheme it used) were deleted 2026-07-03 — the ring geometry
that replaced them lives directly in `kaijutsu-app`'s `view/time_well/card.rs`
now, not in this crate.
