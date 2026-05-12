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
- `message` — cache through a specific message (needs `--index <N>`,
  0-based). The natural target after a fork is the last message
  shared with the parent; see "rc env vars" below for the current
  caveat on computing that index.

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

## rc env vars

rc lifecycle scripts (`/etc/rc/<context_type>/<verb>/SXX-*.kai`)
receive these variables from the kernel (see
`kaijutsu-kernel/src/kj/lifecycle.rs`):

- `KJ_CONTEXT`            — hex ID of the context this script runs against
- `KJ_VERB`               — `create` | `fork` | `drift`
- `KJ_RC_DEPTH`           — recursion depth (capped at `MAX_RC_DEPTH = 4`)
- `KJ_PARENT_CONTEXT`     — hex ID of parent (fork only)
- `KJ_PARENT_BLOCK_COUNT` — parent's BlockStore size at fork time, as
                            a decimal string (fork only). Equals the
                            number of blocks copied into the child.
- `KJ_FORK_INFO`          — JSON `{"kind": "shallow|compact|full",
                                     "parent": "<hex>"}` (fork only)
- `KJ_DRIFT_INFO`         — JSON `{"kind": "...", "source": "<hex>",
                                     "target": "<hex>",
                                     "source_model": "..."}` (drift only)

### rc-on-create — durable per-session targets

```sh
# /etc/rc/default/create/S20-cache.kai
kj cache add --target tools  --ttl extended    # fixed toolset → 1h
kj cache add --target system --ttl ephemeral   # may drift → 5m
```

### rc-on-fork — cache the prefix shared with parent

```sh
# /etc/rc/default/fork/S30-cache.kai
# The last shared message with the parent is at index N-1, where
# N is the parent's block count at fork time.
kj cache add \
  --target message \
  --index $((KJ_PARENT_BLOCK_COUNT - 1)) \
  --ttl extended
```

### rc-on-drift — clear and rebuild

```sh
# /etc/rc/default/drift/S40-cache.kai
# Drift events (compact, model swap, doc inject) reshape the
# conversation — old MessageIndex breakpoints point at the wrong
# message now. Clear and re-seed the stable bits.
kj cache clear
kj cache add --target tools  --ttl extended
kj cache add --target system --ttl ephemeral
```
