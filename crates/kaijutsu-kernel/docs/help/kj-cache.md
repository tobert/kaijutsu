# kj cache — Claude prompt-cache breakpoints

Manage `cache_control` breakpoints on the active context. Populated by
rc lifecycle scripts at create/fork/drift; read at LLM stream build
time.

```bash
kj cache list
kj cache add --target tools  --ttl ephemeral
kj cache add --target system --ttl ephemeral
kj cache add --target message --index 42 --ttl extended
kj cache clear
```

## Targets

- `tools`  — cache the tools array (stable per session)
- `system` — cache the system prompt block
- `message` — cache through a specific message (needs `--index <N>`).
  The natural target after a fork: `--index $((KAI_FORK_AT - 1))`
  covers the prefix the child shares with its parent.

## TTLs

- `ephemeral` (default) — 5-minute Anthropic cache
- `extended` — 1-hour Anthropic cache (use for stable per-session
  targets and fork points where multiple follow-up calls amortize
  the higher write cost)

## Cap and dedupe

Anthropic allows up to 4 breakpoints per request. Storage is liberal —
`add` always succeeds. The Claude wire layer applies in declaration
order, dedupes `tools`/`system`/`message[N]`, and drops anything past
the cap with a `tracing::warn` line.
