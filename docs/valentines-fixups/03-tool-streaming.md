# 03 — Tool Streaming (ToolCallDelta Ignored)

**Priority:** P0 | **Found by:** Gemini 3 Pro code review

## Problem

`RigStreamAdapter` in `llm/stream.rs:422` emits `ToolUse` immediately on the first `ToolCall` event (often with empty/partial args) and ignores all subsequent `ToolCallDelta` events. Most LLM providers (Anthropic, OpenAI) stream tool calls incrementally.

## Impact

Tool calls arrive with empty or truncated arguments. Tools either fail or execute with wrong parameters. Affects all providers that stream tool calls.

## Fix

1. On first `ToolCall` event, create a buffer (tool name + partial args)
2. On each `ToolCallDelta`, append to the args buffer
3. On stream end or next tool call, emit the complete `ToolUse` with full args
4. Handle the case where provider sends complete tool call in one event (no deltas)

## Files to Modify

- `crates/kaijutsu-kernel/src/llm/stream.rs` — `RigStreamAdapter`

## Verification

- Unit test: feed sequence of ToolCall + ToolCallDelta events, verify single complete ToolUse emitted
- Integration test with real provider if possible

## Completion

- [ ] Fix applied and compiles
- [ ] Tests pass
- [ ] Update [README.md](README.md) checklist
