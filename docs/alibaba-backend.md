# Alibaba Model Studio backend (design)

Status: design. Today Alibaba runs as `--kind openai` backends (`alibaba`, the
us-east-1 workspace endpoint, and `alibaba-token`, the Token Plan). This
document is the plan for a dedicated `alibaba` kind.

## Why a kind and not `openai` with a base URL

The generic `openai` client (`llm/openai/build.rs`) treats every non-OpenAI,
non-DeepSeek server the same: `effort` is inert and warned, thinking cannot be
turned on or off, `cache_breakpoints` are ignored, and the key env defaults to
`OPENAI_API_KEY`. Alibaba differs on each:

- **Thinking is per model.** qwen3.8-max and qwen3.8-flash take
  `reasoning_effort` (`low`, `medium`, `high`, `xhigh`, `max`; `none`
  disables). qwen3.7, qwen3.6, glm and kimi take only `enable_thinking`
  (true or false). glm-5.3 and kimi-k2.7-code always think and refuse
  `enable_thinking: false`. DeepSeek V4 on Alibaba takes `low`, `high`, `max`.
  qwen3.8 refuses `reasoning_effort` together with `thinking_budget`.
- **Thinking carries across turns** with `preserve_thinking: true` and the
  earlier turn's `reasoning_content` sent back on the assistant message.
- **Explicit cache.** A `cache_control: {"type": "ephemeral"}` marker on a
  message's text content caches the prefix up to it: at least 1,024 tokens,
  not on tools, a miss when the marker is more than 20 content blocks behind
  the match. Hits are billed at 10% of input. Usage reports
  `prompt_tokens_details.cached_tokens` and `cache_creation_input_tokens`.
  The `X-DashScope-CacheControl: enable` header turns caching on.
- **GLM drops array content on a request with no tools.** A tool-less GLM
  request must send each message's text parts joined into one string.
- **Two key families.** The Token Plan key and a workspace key are different
  keys for different hosts; neither is an OpenAI key, and an `openai`-kind
  backend that names no `api_key_env` reads `OPENAI_API_KEY`.

Sources: Alibaba's docs ("Deep thinking", "Context cache", "Explicit cache
best practices", the model pages) and Qwen Code (`QwenLM/qwen-code`,
`packages/core/src/providers/presets/alibaba-token-plan.ts` and
`packages/core/src/core/openaiContentGenerator/provider/dashscope.ts`), which
encodes the same rules.

## Shape

`BackendKind::Alibaba` (`llm/config.rs`), a preset over `openai::Client` in
`llm/alibaba/mod.rs`, as `llm/deepseek/` is:

- `base_url` required: the Token Plan
  (`https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1`)
  or a workspace endpoint. The kind does not guess which.
- Key env default `ALIBABA_API_KEY`, never `OPENAI_API_KEY`.
- The request builder takes a dialect instead of the two booleans it has now
  (`reasoning_required`, `hosted_openai`): `Generic`, `HostedOpenAi`,
  `DeepSeek`, `Alibaba`. The Alibaba arm maps effort and thinking per model,
  sends `preserve_thinking`, places `cache_control` from `cache_breakpoints`,
  sends the cache header, and flattens tool-less GLM content.
- The client names itself as kaijutsu in `User-Agent`. It does not send Qwen
  Code's identity.

## Which knob a model takes: two options

**A. A table in code** (`llm/alibaba/models.rs`), keyed by model id prefix:
the thinking control (`effort` ladder, toggle, or always-on), the effort
values it accepts, and its context window. Qwen Code works this way. It is a
fact about the wire, it is tested beside the builder, and a new model id that
matches no prefix falls back to toggle-only with a warning. Cost: a new model
family needs a code change.

**B. Per-model rows** (`kj backend model set … --extra '{"thinking": …}'`):
the same facts as data the operator edits. No code change for a new model,
but the knob a model takes is protocol, not a tunable, and a wrong row sends
a request the server refuses.

Recommendation: A for which knob a model takes (protocol), and model rows
for what the operator tunes (`effort` level, `max_tokens`, context window).
Each value has one owner.

## Server-side tools (open)

Alibaba runs `web_search` and `web_extractor` on its side (billed to the
Token Plan, reported under `usage.x_tools`); Anthropic has `server_tool_use`
blocks for web search, which `llm/claude/types.rs` parses and drops. A tool
the provider runs is neither a kernel tool call (nothing to execute or gate)
nor plain text (it has inputs, results, and cost). Representing it needs a
design conversation before either backend enables one; see docs/issues.md.

## Not in scope here

The Token Plan's usage rule allows interactive use inside coding and agent
tools and forbids automation scripts, application backends, and
non-interactive batch calls. Which kaijutsu callers fit is Amy's call; the
backend kind does not enforce it.
