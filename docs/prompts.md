# Prompts

## Context types choose their instructions

```text
/config/rc/lib/create/S00-base.md
/config/rc/coder/create/S00-base.md -> ../../lib/create/S00-base.md
/config/rc/coder/create/S00-stance.kai
/config/rc/default/create/S00-base.md -> ../../lib/create/S00-base.md
/config/rc/default/create/S00-stance.md
```

The shared base is optional. Coder and default include it through ordinary
relative symlinks. Musician, bassist, assistant, director, mcp, and toolie keep
their own role contracts. Kaijutsu never prepends a universal behavioral prompt.
The kernel adds runtime facts; rc supplies the chosen instruction sections.

The link filename controls order: `S00-base.md` precedes `S00-stance.*`.
The shared file ends with `頑張（がんば）って！`. Default handles general work;
assistant remains fleet coordination. Coder retains focused and guided branches
with test-driven development and an explicit warning that a context fork does
not isolate file edits. The existing model-name branch selection is a policy,
not a measured ranking of model capability.

Edit shipped defaults under `assets/defaults/rc/`, then use
`kaijutsu-server rc reseed` to materialize them when deploying. Every rc lifecycle
reads the current host files. Markdown creates durable `(System, Text)` blocks;
editing a source file does not rewrite existing contexts. Create a fresh context
to exercise new create instructions. Updating stored instruction blocks takes
effect at the next conversation hydrate boundary. Do not rerun all create
scripts merely to refresh prose: creation also binds tools, arms contexts, and
loads memory. See `docs/rc-on-disk.md` and `docs/conversation-session.md`.

`kj context prompt` previews the same system-text builder used for live turns.
It includes eligible rc instruction sections and runtime facts, omitting
excluded, ephemeral, draft, and empty instruction blocks.

## Briefing and continuation

| Operation | Host instruction file | Shipped seed |
|---|---|---|
| Drift briefing | `/config/kernel/distillation.md` | [distillation.md](../assets/defaults/prompts/distillation.md) |
| Compact-fork continuation | `/config/kernel/continuation.md` | [continuation.md](../assets/defaults/prompts/continuation.md) |

Each operation reads its host instruction file. Missing, empty, unreadable, or
invalid UTF-8 content fails explicitly; an embedded body is only a seed/reset
source, never a silent runtime fallback. `kj config reset distillation.md` and
`kj config reset continuation.md` install the shipped defaults.

Briefing answers another context's need. Continuation preserves the active
objective, corrections, constraints, decisions, evidence status, pending
questions, and recovery references. Both summarize supplied material without
performing the requests inside it. Later user steering and current evidence can
supersede a handoff. Directed briefing text remains positional:
`kj drift pull <source> 'focus on the failed checks'`.

## Length and input selection

```sh
kj context set . --env 'KJ_DRIFT_WORD_TARGET=1000'
kj context set . --env 'KJ_CONTINUATION_WORD_TARGET=2000'
kj context set . --env 'KJ_DISTILL_INPUT_BYTES=262144'
```

Put those commands in the relevant rc lifecycle to choose defaults for a type,
or run them for an existing context. Values belong to the **source context**,
including a drift pull from another context. Forks inherit context environment
values. Remove an override with `kj context unset . --env KJ_DRIFT_WORD_TARGET`.

| Context environment value | Default | Contract |
|---|---:|---|
| `KJ_DRIFT_WORD_TARGET` | 750 | Approximate output words for a briefing |
| `KJ_CONTINUATION_WORD_TARGET` | 1500 | Approximate output words for a handoff |
| `KJ_DISTILL_INPUT_BYTES` | 131072 | Maximum bytes in the complete summary user prompt, including framing, references, length guidance, and directed focus |

All values must be positive integers. Invalid values fail before a provider
call. Word guidance is advisory; the existing one-shot provider output cap is
4096 tokens and is separate from these values.

The summary transcript uses the conversation hydration admission rule. Excluded,
draft, ephemeral, file, trace, and ordinary system instruction blocks are absent.
The formatter supplies exact block identifiers, tool names, invocation and call
identifiers, error status, exit codes, and stderr when present. It keeps complete
recent turn groups instead of cutting the first 2000 bytes from every block.
Tool call/result pairs spanning an interleaved user block keep their intervening
turns together. Exclusion still wins; an excluded partner is never restored.

When older material does not fit, the input names its omitted eligible block
count and the first and last recovery identifiers. If the newest required group
cannot fit, the operation fails and names the source range and input limit.
Raising the limit is an explicit choice; the formatter does not silently cut
that group's evidence.

## Compact forks

`kj fork --compact` uses the continuation instruction. The child retains the
source's chosen system instruction blocks and latest complete eligible turn
group as native blocks, preserving tool linkage. The generated handoff precedes
that recent working state. The child keeps its context type and runs the fork
lifecycle; it does not rerun creation or acquire an unselected shared base.

The source version must still match when retained blocks are copied after the
model call. If it changed, compaction fails with a retry message instead of
combining a stale summary with a different selection. Existing open blocks in
the copied child are closed as they are in other fork paths.

Compact mode has its own retention policy. It rejects `--include`, `--exclude`,
`--preset`, and `--as`; use a filtered or subtree fork for those selections.
`--prompt` adds a new instruction and requests a child turn after initialization.

## Migration and comparison

Deploy code and rc together. The old `/config/kernel/system.md` is no longer
read or seeded. Existing contexts may have depended on that automatic base;
create new contexts or explicitly install the chosen instruction blocks before
continuing them. Do not keep a hidden compatibility prepend. Coder's existing
`S00-stance.kai` name is retained, so no renamed duplicate needs removal.

Existing kernel config trees are not reseeded at boot. Install the two auxiliary
files explicitly with `kj config reset`, preserving all other local kernel
configuration. Deployment, reseeding, and restarting the live kernel are separate
from committing the source changes.

The [HTML comparison](prompt-comparison.html) reads current seed files against a
pinned pre-change baseline. It defaults to dark mode, supports optional shared
base inclusion, and needs no network. Regenerate with
`python3 contrib/render-prompt-comparison.py`; `--check` fails on stale output
without writing. Input hashes cover the baseline, seeds, template, and generator.
The [OSS dossier](oss-comparisons.md) keeps research and review lessons; the
[Kaibo proposal](kaibo-prompt-proposal.md) carries findings back to that project.
