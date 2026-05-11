# Unrig

*Removing `rig-core` and replacing it with bespoke per-model systems.*

The promise of kaijutsu is a bespoke experience for everyone — and that includes
the models themselves. Each model family has its own surface: Claude has prompt
caching, extended thinking, beta headers, citations; Gemini has search grounding,
code execution, the File API; local models have their own constraints around
throughput, batching, and quantization. A lowest-common-denominator wrapper
asks each model to wear someone else's clothes.

This is the one thing we want to get right before we start relying on kaijutsu
daily.

---

## Decision

Drop `rig-core` from `kaijutsu-kernel`. Replace it with hand-rolled provider
modules — starting with Claude and Gemini together so a second voice forces the
contract to stay honest. Local inference (Ollama or otherwise) is deferred —
we want to think through what local model work actually needs before we
commit to a target.

The `StreamEvent` enum stays — kaijutsu-shaped events flowing upward into CRDT
block writes is the right shape. The `LlmStream` trait does *not* stay: dispatch
becomes an explicit `Provider` enum `match` at call sites, so provider-specific
knobs can be typed and visible rather than hidden behind a uniform interface.

---

## Contributing Factors

Things we've already noticed that pushed us here:

- **JSON-hack escape hatches in our code.** `llm/stream.rs:349` injects
  extended-thinking config as a raw JSON blob through rig's `additional_params`
  passthrough. That escape hatch existing in our code is rig telling us "this
  doesn't fit."
- **Features we want and can't have.** No `cache_control` for Claude, no beta
  headers, no citations, no native handling of structured thinking blocks. The
  cost difference of prompt caching alone justifies the surgery on Claude.
- **Lossy conversion.** Our `Message` / `ContentBlock` types are richer than
  rig's — `Reasoning` blocks get flattened on the way down, and conversion is
  unidirectional. We translate *out* of our types and never get back what we
  put in.
- **Narrow attack surface.** Only `kaijutsu-kernel` depends on rig, concentrated
  in `llm/mod.rs` and `llm/stream.rs`. The surgery is contained.
- **Maintenance.** The official Anthropic Rust SDK was unmaintained last time
  we checked. Hand-rolling REST+SSE against the Messages API is well-trodden
  ground — `reqwest` + `eventsource-stream` and we own the surface.

---

## Architecture

Per-provider modules live inside `kaijutsu-kernel/src/llm/`:

```
crates/kaijutsu-kernel/src/llm/
├── mod.rs           # LlmStream trait, LlmRegistry
├── stream.rs        # StreamEvent enum, StreamRequest
├── claude/
│   ├── mod.rs       # ClaudeProvider, LlmStream impl
│   ├── http.rs      # hand-rolled HTTP client
│   ├── sse.rs       # event-stream parsing
│   └── types.rs     # request/response types
├── gemini/
│   └── …            # mirror structure
└── (local later)
```

Each provider:

- Lives behind a `Provider` enum variant; no shared trait.
- Owns its own native request/response types. Provider-specific knobs are
  typed builder methods on the native request (`with_thinking`, `cache_system`,
  `with_google_search`, `with_cached_content`).
- Translates kaijutsu's `Message` / `ContentBlock` into its own native shape
  in a dedicated `Client::build(&conv, opts)` function — explicit and visible,
  no `From` impls.
- Emits the shared `StreamEvent` enum (shrunk to what CRDT block writing
  needs); provider-specific richness rides on typed `UsageExtra::Claude` /
  `UsageExtra::Gemini` variants and a typed `Provider(_)` escape hatch on
  `FinishReason` and `StreamError`.

Server-side dispatch in `kaijutsu-server/src/llm_stream.rs` is a thin
`match Provider` that delegates to per-provider helpers in
`kaijutsu-kernel::llm::{claude,gemini}` — the server stays small as knobs
accumulate.

---

## What We Keep / What We Replace

| Layer | Status |
|-------|--------|
| `StreamEvent` enum | Keep, shrunk to CRDT-write essentials |
| `LlmStream` trait | Delete — replaced by `Provider` enum `match` |
| `StreamRequest` (kernel-side) | Delete — replaced by per-provider native requests + thin `BuildOpts` for shared knobs |
| CRDT block writing (`kaijutsu-server/src/llm_stream.rs`) | Keep — provider-agnostic already; gains a per-provider `match` |
| Tool definition source (MCP broker) | Keep — single source of truth, including Gemini built-ins as virtual MCP entries |
| `RigProvider` enum, `RigStreamAdapter` | Delete |
| rig's `Message` / `Content` / `CompletionRequest` translation | Delete |
| `additional_params: {"thinking": …}` JSON hack | Delete — becomes a typed field |
| `models.toml` provider config | Keep, add per-provider feature sections |

---

## Contract Decisions (Phase 0)

Locked during initial design conversation:

- **No `LlmProvider` trait.** Dispatch is `match` on a `Provider` enum
  (`Provider::Claude(Client) | Provider::Gemini(Client)`). Closed set; adding a
  provider = new variant + new call-site branch. Closed-set dispatch beats
  trait-objects-pretending-to-be-open for our use case.
- **Per-provider native request/response types.** No shared "extensions"
  struct, no `Option<ProviderXKnob>` fields on a common request. Knobs are
  typed builder methods on the native request (`MessagesRequest::with_thinking`,
  `GenerateContentRequest::with_google_search`, etc.). Call-site branch pulls
  knobs from context/config and applies them.
- **Conversation type stays kaijutsu-native.** Kernel-side `Message` /
  `ContentBlock` remains canonical. Each provider's `Client::build(&conv, opts)`
  decides how to translate into its native conversation shape.
- **Per-provider tool definitions.** No internal "common ToolDef." Each
  provider's `build()` translates from MCP broker tool entries into its own
  native format.
- **Gemini built-ins as virtual MCP tools.** `googleSearch`, `codeExecution`,
  `urlContext` (and friends) register as MCP-flavored entries in the kaijutsu
  tool surface — visible, toggleable, per-context, same shape as `kj` builtin
  tools. The Gemini provider's `build()` recognizes them and emits native
  built-in tool config instead of function declarations. Other providers ignore
  these entries.
- **Thin server-side dispatch.** The call-site `match Provider` in
  `kaijutsu-server/src/llm_stream.rs` delegates to per-provider helper
  functions in `kaijutsu-kernel::llm::{claude,gemini}`. Server stays small
  as knobs accumulate.
- **`StreamEvent` is minimal.** Just what CRDT block writing needs: text
  delta, thinking delta, tool use lifecycle, usage, finish, error. Provider
  richness rides on typed `UsageExtra` enum variants.
- **Errors: common variants + provider-typed escape hatch.** `StreamError`
  has `RateLimit | Auth | Server | Overloaded | Provider(typed)`. Kaijutsu
  surfaces errors as JSON to users — homogenization isn't load-bearing.
- **Cache breakpoints as side-channel map.** Claude's prompt caching is a
  *policy* (we get 4 cacheable spots per request), not a *property* of any
  content block. Shape: `HashMap<BlockId, CacheBreakpoint>` on `BuildOpts`,
  kernel-side (lives in `kaijutsu-kernel::llm`, not `kaijutsu-types` — Claude
  is the only consumer). Claude's `build()` reads it; Gemini's `build()`
  ignores it. *How* the map gets populated (static config, heuristic, manual
  annotation, drift router) is a separate decision, deferred.

## Phases

### Phase 0 — Contract design

Sketch the `LlmStream` trait extensions, `StreamEvent` additions, and the
`kaijutsu-types` shapes that need to grow (cache-control, thinking, grounding
hooks). Validate the contract against both Claude *and* Gemini on paper before
writing the first provider — having two voices at the design table is the
point.

### Phase 1 — Drop rig

Rip `rig-core` out of `Cargo.toml`, delete `RigProvider` / `RigStreamAdapter`
and the rig-shaped `LlmStream` trait. Stand up the new shapes:

- `Provider` enum (`Claude(Client) | Gemini(Client)`)
- Minimal `StreamEvent`, `Usage`, `UsageExtra`, `FinishReason`, `StreamError`
- `BuildOpts` (shared knobs only)
- Skeleton `kaijutsu-kernel/src/llm/{claude,gemini}/` modules with `Client`
  structs whose `stream()` returns a loud "not yet implemented" error

Update the server-side dispatch in `kaijutsu-server/src/llm_stream.rs` to a
`match Provider` over the new enum.

Silent fallbacks are mistakes — crashing loudly is preferred while the system
is offline. We can do this because we're not relying on kaijutsu daily yet;
the "no model access" window is acceptable.

### Phase 2 — Claude

Hand-rolled Anthropic SDK in `crates/kaijutsu-kernel/src/llm/claude/`.
Messages API, SSE streaming, tool use, extended thinking as typed fields,
prompt caching with explicit `cache_control` on selectable content blocks.

TDD: SSE parser tests against recorded fixtures so we have tests that can and
will fail when the wire format shifts.

### Phase 3 — Gemini

Hand-rolled Gemini SDK in `crates/kaijutsu-kernel/src/llm/gemini/`. Streaming,
tool use, the bits that don't map onto Claude's shape (search grounding, code
execution, file refs). The friction of bringing Gemini up against the
contract we designed in Phase 0 is the design-validation signal — if Phase 0
got it wrong, Phase 3 finds out.

### Phase 4 — Local model story (deferred)

Ollama is the obvious target but not committed. Open question: does kaijutsu
want HTTP-to-Ollama, direct llama.cpp/candle integration, or both? Defer
until Claude + Gemini are in and we know what shape the contract actually
took.

---

## Progress

- [x] Phase 0 — Contract design
- [x] Phase 1 — Drop rig
- [x] Phase 2 — Claude (incl. 2.5 signature plumbing)
- [ ] Phase 3 — Gemini
- [ ] Phase 4 — Local models (deferred)

---

## Open Questions

- **System prompt as block list.** *Resolved in Phase 2:* `BuildOpts.system`
  stays `Option<String>`. Claude's `build()` promotes it to a single
  `SystemPrompt::Blocks([SystemBlock::text(_).with_cache_control(...)])`
  when a `CacheTarget::System` breakpoint is set, and keeps the plain
  string form otherwise. Per-segment caching of a multi-block system prompt
  is deferred — no current consumer needs it.
- **Cache breakpoint policy.** *Shape revised in Phase 2:* the doc's
  `HashMap<BlockId, CacheBreakpoint>` was unimplementable without
  `LlmMessage` carrying block identity past hydration. Phase 2 uses
  `Vec<CacheTarget>` with symbolic variants (`Tools`, `System`,
  `MessageIndex(usize)`) instead. *Policy:* user-picked **carrier only,
  no defaults** — `BuildOpts.cache_breakpoints` defaults to empty;
  nothing applies `cache_control` until a future PR wires a populator
  (heuristic / drift-router / `models.toml`).
- **Extended thinking config source.** *Open, deferred:* Phase 2 lays the
  typed builder (`build::with_thinking(req, budget_tokens)`) but no
  caller populates it. Open: per-context (DriftRouter), per-provider
  (`models.toml`), or per-call.
- **Thinking signature plumbing.** *Resolved in Phase 2.5:*
  `StreamEvent::ThinkingEnd` became a struct variant carrying
  `signature: Option<String>`. Claude state machine accumulates
  `signature_delta` payloads inside the per-block state and emits the
  combined signature at `content_block_stop`. Server-side
  `process_llm_stream` captures it into a per-iteration
  `assistant_thinking_signature` accumulator and threads it into
  `Message::with_reasoning_text_and_tool_uses`, so the next agentic
  loop iteration echoes the reasoning chain back to Anthropic with
  its verifier — required when extended thinking is enabled and
  `tool_use` is in the same turn.
- **Ollama vs llama.cpp/candle.** Don't commit yet. Local model phase starts
  with a design conversation, not code.
- **Embeddings.** rig also gave us embedding clients. What's the current
  embedding usage, and does it belong in the same unrig pass or split out?

---

## Decision Log

*Append entries here as we go — date, what changed, why.*

- **2026-05-11** — Decision recorded. Unrig committed; Claude + Gemini in
  initial scope, local models deferred. Modules live inside `kaijutsu-kernel`,
  not separate crates (compile time + close-to-consumers trumps reusability
  we don't yet need).
- **2026-05-11** — Phase 0 contract decisions locked (see "Contract Decisions"
  section). Key shape: `Provider` enum + `match` dispatch, per-provider native
  types with typed builder knobs, kaijutsu-native conversation translated by
  `Client::build()`, per-provider tool definitions, Gemini built-ins exposed as
  virtual MCP tools, thin server dispatch via per-provider helpers, minimal
  `StreamEvent`, typed `UsageExtra` enum, errors with typed provider escape
  hatch.
- **2026-05-11** — Phase 1 landed. `rig-core` removed from `Cargo.toml`
  (workspace + kernel). New shapes live in `crates/kaijutsu-kernel/src/llm/`:
  `Provider` enum (`Claude | Gemini | Mock`), `ProviderStream` dispatcher,
  minimal `StreamEvent` / `BuildOpts` / `Usage` / `UsageExtra` /
  `FinishReason` / `StreamError`. Skeleton modules
  `crates/kaijutsu-kernel/src/llm/{claude,gemini}/mod.rs` have `Client`
  structs whose `stream()` and `prompt()` return loud `LlmError::Unavailable`
  ("not yet implemented (unrig phase 2|3)"). Server-side dispatch in
  `crates/kaijutsu-server/src/llm_stream.rs` builds a `BuildOpts` and calls
  `Provider::stream(opts, messages)`. OpenAI/Ollama variants dropped per
  decision; `from_config` rejects them. `LlmStream` trait + `StreamRequest` +
  rig `From` impls deleted. Workspace builds clean, all 604 kernel + 15
  server unit tests + 63 LLM tests pass. No working model access (expected
  per Phase 1 scope).
- **2026-05-11** — Phase 2 landed. Claude wire layer live under
  `crates/kaijutsu-kernel/src/llm/claude/`:
  - `types.rs` — Anthropic Messages API native shapes
    (`MessagesRequest`, `RequestContent` with `#[serde(tag = "type")]`,
    `CacheControl`, `Thinking`, `ResponseUsage` with cache stats).
  - `build.rs` — `build_request(opts, messages, streaming)` translates
    kaijutsu `Message`/`ContentBlock` into Anthropic shapes. Cache
    breakpoints applied to last tool + system block when present.
  - `sse.rs` — Typed `ClaudeSseEvent` over `eventsource-stream`. Unknown
    events and JSON parse errors surface as typed `SseDecodeError`,
    never silently dropped.
  - `stream.rs` — `StateMachine` translates SSE events to kaijutsu
    `StreamEvent`s. Tool input assembled across `input_json_delta`
    partials, emitted atomically on `content_block_stop`. Signature
    bytes from `signature_delta` accumulate inside per-block state and
    emit on `content_block_stop` (see Phase 2.5 entry below).
  - `mod.rs` — `Client` wraps `reqwest::Client` with auth headers in
    `default_headers`. `stream()` POSTs `/v1/messages` and wraps the
    SSE byte stream. `cancel()` fires a `tokio_util::CancellationToken`
    that the next `next_event()` poll observes; emits
    `Done { stop_reason: None }` matching the server's
    cancel-confirm contract.
  - 41 new tests (types serde, build translation, SSE parser including
    chunked transport, state machine including malformed tool input
    surface-error, end-to-end stream-from-bytes, cancel-confirm).
    Plus a live smoke test gated on `ANTHROPIC_API_KEY` (run with
    `cargo test --ignored claude_live`).
  - Dependencies added: `reqwest` (json + stream features, rustls via
    defaults), `eventsource-stream = "0.2"`, `bytes`.
  - Cache breakpoint shape diverged from the Phase 0 sketch:
    `HashMap<BlockId, CacheBreakpoint>` → `Vec<CacheTarget>` with
    symbolic variants. Reason: `LlmMessage` doesn't carry `BlockId`
    past hydration.
  - 645 kernel + 15 server unit tests pass (up from 604/15 in Phase 1).
- **2026-05-11** — Phase 2.5 landed: thinking signature plumbing.
  `StreamEvent::ThinkingEnd` became a struct variant carrying
  `signature: Option<String>` (breaking change, small surface — one
  server match arm + one test). Claude state machine accumulates
  `signature_delta` payload (defensively, since the wire-event name
  reserves room to split it across multiple deltas) and emits at
  `content_block_stop`. Server's `process_llm_stream` gained an
  `assistant_thinking_signature` per-iteration accumulator that feeds
  `Message::with_reasoning_text_and_tool_uses`, so reasoning chains
  round-trip back to Anthropic with their verifier — required for
  correctness when extended thinking + `tool_use` share a turn.
  Verified end-to-end: live smoke test against api.anthropic.com
  ("hi there" → "Hey! 👋 How's it going? What can I help you with?",
  17 input / 21 output tokens, natural `end_turn`). 646 kernel + 15
  server unit tests pass.
