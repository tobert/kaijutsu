# Alibaba Model Studio backend

Status: built. `BackendKind::Alibaba` (`llm/config.rs`) is a preset over
`openai::Client` in `llm/alibaba/`, as `llm/deepseek/` is. The request
builder (`llm/openai/build.rs`) speaks it as `Dialect::Alibaba`.

```sh
kj backend set alibaba-token --kind alibaba \
  --base-url https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1 \
  --api-key-file ~/.alibaba-token-key
```

`--base-url` is required: the Token Plan and a workspace endpoint are
different hosts with different keys, and the kind does not guess which. With
no `--api-key-env` or `--api-key-file`, the key is `ALIBABA_API_KEY`, never
`OPENAI_API_KEY`. The provider name is the backend name, so two Alibaba
backends keep separate costs. Requests carry kaijutsu's `User-Agent`.

## Why a kind and not `openai` with a base URL

The generic `openai` dialect has no thinking switch, ignores
`cache_breakpoints`, and reads no key at a host other than api.openai.com.
Alibaba differs on each, per model. Sources: Alibaba's docs ("Deep thinking",
"Context cache", "Explicit cache best practices", the model pages) and Qwen
Code (`packages/core/src/providers/presets/alibaba-token-plan.ts`,
`packages/core/src/core/openaiContentGenerator/provider/dashscope.ts`).

## Thinking

Which control a model takes is protocol, so it is a table in code,
`llm/alibaba/models.rs`, keyed by model id prefix (case-insensitive). What the
operator tunes (`effort`, `max_tokens`, the context window) stays on model rows
and slots.

| Models | Control | Thinking off |
|---|---|---|
| qwen3.8-max, qwen3.8-flash | `reasoning_effort`: low, medium, high, xhigh, max | `reasoning_effort: "none"` |
| deepseek-v4* | `reasoning_effort`: low, high, max | `enable_thinking: false` |
| qwen3.7, qwen3.6, glm-5.2, glm-5.1, kimi-k2.5, kimi-k2.6 | `enable_thinking: true` | `enable_thinking: false` |
| glm-5.3, kimi-k2.7-code, MiniMax-M2.5 | always on | refused |

A model that matches no prefix is toggle-only, with one warning per model.

Thinking is off for `effort = "none"` or `thinking_style = "off"`. A request
the model cannot take is refused before it is sent, naming the model:
thinking off for an always-on model, an effort the model does not list
(never clamped to a neighbor), an effort beside `thinking_budget` (qwen3.8
refuses the pair), and a thinking style outside auto, adaptive, budget and
off. `thinking_budget` alone rides as `thinking_budget`.

A one-shot prompt turns thinking off where the model allows it and leaves an
always-on model thinking; a slot's explicit effort or thinking style wins.

Thinking carries across turns: with thinking on, the request sends
`preserve_thinking: true`, and each assistant message's reasoning goes back
as `reasoning_content`, whichever provider wrote it (`docs/fork-filters.md`).
A turn with no reasoning sends none; nothing is synthesized.

## Explicit cache

Every request sends `X-DashScope-CacheControl: enable`. A context's
`cache_breakpoints` become `cache_control: {"type": "ephemeral"}` markers on
message text (`llm/alibaba/cache.rs`); Alibaba takes none on tools.

- `System` marks the system message. `Tools` does too: tools render first,
  so that marker covers them.
- `MessageIndex(i)` marks the last text part of the last wire message that
  history message `i` produced. A message with no text passes the marker back
  to the nearest earlier message with text.
- A request with any breakpoint also marks its last message. Alibaba finds an
  earlier cache only within 20 content blocks before a marker, so a moving
  tail marker keeps each request a turn or so past the previous one's. A
  turn that adds more than 20 blocks misses once and writes a new cache.
- At most 4 markers, explicit ones first in declaration order. Alibaba caches
  for 5 minutes; an `extended` breakpoint is sent as ephemeral, with a
  warning. Each dropped breakpoint is warned.

Usage reads `prompt_tokens_details.cached_tokens` as the cache read and
`prompt_tokens_details.cache_creation_input_tokens` as the cache write. Both
are inside `prompt_tokens`, so pricing uses `InputStyle::IncludesCache`: the
rest of the prompt at the input rate, the read and the write each once at
their own rate. The fixture follows Alibaba's documented shape; no live
capture is in the repo yet.

## GLM with no tools

A glm request with no tools drops array content on the server. Such a
request sends each message's text-only content as one string, parts joined
with a blank line; content with an image keeps its parts. A string carries
no marker, so the request has no cache markers, with a warning.

## Server-side tools (open)

Alibaba runs `web_search` and `web_extractor` on its side (billed to the
Token Plan, reported under `usage.x_tools`); Anthropic has `server_tool_use`
blocks for web search, which `llm/claude/types.rs` parses and drops. A tool
the provider runs is neither a kernel tool call nor plain text. Representing
it needs a design conversation before either backend enables one; see
docs/issues.md.

`usage.x_tools` is not captured. It would be parsed on `openai::types::Usage`,
carried on `OpenAiCompatUsageExtra`, and recorded with
`KernelDb::add_turn_tool_uses` from `record_turn_spend`
(`runtime/llm_stream.rs`).

## Not in scope here

The Token Plan's usage rule allows interactive use inside coding and agent
tools and forbids automation scripts, application backends, and
non-interactive batch calls. Which kaijutsu callers fit is Amy's call; the
backend kind does not enforce it.
