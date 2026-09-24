# Prompts

This document describes current behavior. The complete kaish/rc migration is
tracked in [Kaish integration and rc lifecycle](kaish-integration.md).
Only `.kai` lifecycle entries execute; Markdown is data read by scripts.

## Context types choose their instructions

```text
/config/rc/lib/create/S00-base.kai
/config/rc/lib/create/S00-base.md
/config/rc/coder/create/S00-base.kai -> ../../lib/create/S00-base.kai
/config/rc/coder/create/S00-base.md -> ../../lib/create/S00-base.md
/config/rc/coder/create/S00-stance.kai
/config/rc/default/create/S00-base.kai -> ../../lib/create/S00-base.kai
/config/rc/default/create/S00-base.md -> ../../lib/create/S00-base.md
/config/rc/default/create/S00-stance.kai
/config/rc/default/create/S00-stance.md
/config/rc/director/create/S00-base.kai -> ../../lib/create/S00-base.kai
/config/rc/director/create/S00-base.md -> ../../lib/create/S00-base.md
/config/rc/director/create/S00-stance.kai
/config/rc/director/create/S06-kj-help.kai
```

The shared base is optional. Coder, default and director include it through
ordinary relative symlinks. Musician, assistant, mcp, and toolie keep
their own role contracts. Director is the operator's seat: its stance names
the character recorded in `played_by` (below) and `S06-kj-help.kai`
composes `kj help` plus a selected set of eighteen top-level verb help pages
into one durable instruction block. Leaf command help remains available on
demand. This reference was about 28 KB when introduced; that is a source size,
not a token measurement. The system cache breakpoint permits reuse where the
provider supports it; the initial request still pays for the reference.
Kaijutsu never prepends a universal behavioral prompt.
The kernel adds runtime facts, including the performing character and its
assigned reviewer (stable IDs and names); rc supplies the chosen instruction
sections. Provider/model selection remains a separate fact.

The executable filename controls order: `S00-base.kai` precedes `S00-stance.kai`.
The shared file ends with `頑張（がんば）って！`. Default handles general work;
assistant remains fleet coordination. Coder retains focused and guided branches
with test-driven development and an explicit warning that a context fork does
not isolate file edits. The existing model-name branch selection is a policy,
not a measured ranking of model capability.

`kj context create --type <type>` refuses before committing a row when
`<type>`'s `create` bucket (`/config/rc/<type>/create`) is missing or holds no
runnable `.kai` script — naming the type, the expected bucket path, and `rc
reseed` as the fix. A missing or empty bucket for any other verb (`fork`,
`attach`, `drift`, `tick`, `rotate`, `submit`) stays a legitimate no-op: a
type with no work to do at that verb is ordinary, and rc runs zero scripts
without error. Rebinding a context whose loadout is missing (`kj context
rebind`, boot's root-character repair, `kj context rotate`'s successor) runs
the same `create` lifecycle and reports through `has_usable_loadout` after the
fact instead, since those paths repair or replace a context that already
exists rather than deciding whether to create one.

Edit shipped defaults under `assets/defaults/rc/`, then use
`kaijutsu-server rc reseed` to materialize them when deploying. Every rc lifecycle
snapshots current executable bodies. Scripts create durable `(System, Text)` blocks;
editing a source file does not rewrite existing contexts. Create a fresh context
to exercise new create instructions. Stored instruction blocks are read again
before each turn: edits and exclusions affect the next turn without a fork.
Conversation-history edits take effect at the next hydrate boundary. Do not
rerun all create scripts merely to refresh prose: creation also binds tools,
arms contexts, and loads memory. See `docs/rc-on-disk.md` and
`docs/conversation-session.md`.

`kj context prompt` previews the same system-text builder used for live turns.
It includes eligible rc instruction sections and runtime facts, omitting
excluded, ephemeral, draft, and empty instruction blocks. A failed block read
stops both preview and live turn preparation. An existing context with no
instruction sections remains valid.

## Explicit instruction scripts

```sh
kj block create --role system --kind text --content-type text/markdown < "$(dirname "$0")/$(basename "$0" .kai).md"
```

An rc script's `$0` is its invoked VFS path, including a symlink's name.
The example reads a companion named `S00-instructions.md` when invoked
as `S00-instructions.kai`. The file is ordinary data, read when the script
runs. Missing files or invalid UTF-8 fail visibly. Redirection preserves
trailing newlines and does not route instruction text through stdout limits.
`--content` takes precedence over stdin when explicitly supplied.

New blocks are Done and authored by the invoking performer, consistently
with other `kj` writes. The context creator and requester may differ.
`--content-type` defaults to `text/plain`; use `text/markdown` for Markdown.
Existing instruction blocks retain their authors and content.

Markdown files never execute on their own. A composed script locates data
beside its invoked entry; include both script and data symlinks when composing
shared instructions. Executable bodies are captured before the run, while
companion reads happen during execution. The run's script digest covers the
executable only. See `docs/rc-on-disk.md`, "Migrating existing rc trees".

## Characters and lifecycle observations

`kj context create banto --type director --cast ops --as banto` records
Banto's principal in `played_by`, while `created_by` remains the requester.
The character must exist and be live; unknown or retired names fail before
creating a context. Omitting `--as` leaves `played_by` unset on this `kj` path.
The caller's acting character becomes its director. The reviewer follows
explicit context assignment, explicit director-wide delegation, then the walk
up `forked_from` (`docs/approval-identity.md`). Model output and tools use the performer;
the model turn refuses missing or self-reviewing identities. This does not
load a character rc bundle. See `docs/character.md`, "Current implementation".

Director's stance and the shared `S16-handoff.kai` read `played_by_name` from
context metadata. The handoff script reads that character's log, or the caller's
log when no performer is recorded. `KJ_CHARACTER` was an environment bridge;
new create instructions no longer use it. Existing stored instructions are
unchanged until edited or replaced with a new context.

Handoff, memory recall, datetime, and predecessor text are changing observations.
Their scripts emit `(System, Notification)` blocks, which hydrate into the
conversation without joining the cached system instruction sections. Handoff
reads the last twelve notes. A missing handoff log or predecessor is reported
as fallback text; these optional scripts do not abort creation. This differs
from the required instruction-file reads described below.

## Rotating a context

```sh
kj handoff note --for banto 'what happened, what is next'
kj context rotate banto
kj context prompt banto
```

A seat writes its handoff note before it stops; the shared base asks for it
and the handoff block carries the command. Rotating a cold seat therefore
needs no turn from it. In the TUI, `Ctrl+A r` prefills `kj context rotate `
and follows the successor. Rotation never prompts: the successor runs
`create` rc and waits for its first prompt. When the predecessor left no
note, the successor still has the predecessor excerpt below, and a director
may read the archived seat's blocks.

`kj context rotate` creates a successor from the predecessor's own parent. It
copies the type, cast, performer, director, reviewer override, model, system
prompt, workspace, env, and cwd, sets `ROTATED_FROM` to the
predecessor's id, and runs the `create` lifecycle. The hydration window is not
copied; a musician's create lifecycle sets its own. When the successor has a
usable loadout, one transaction archives the predecessor and gives the
successor its label, ring seat, and any character's `root_ctx` pointer. When
it has none, the predecessor stays live, and the error names the unlabeled
successor so you can read its Error blocks and remove it. The character that
plays the context or its lineage root may rotate it. A root context rotates
the same way.

Rotation copies a pinned model, not a resolved one. A context with no
`provider` and `model` of its own resolves the cast or the global default each
time, and so does its successor. Pin one with
`kj context set banto --model tenchi/qwen3.8-27b`; every later successor keeps
it. A context made with `kj context create` starts unpinned.

`ROTATED_FROM` names the predecessor. `S17-predecessor.kai` reads
up to twelve text blocks with `kj wait --timeout 1 --max-blocks 12 --max-bytes
400 --include text`, then emits a notification. This is a bounded excerpt,
not a complete continuation; it omits tool results and can cut prose. An idle
predecessor resolves from its log without waiting for another turn.

`kj fork --compact` is the alternative when retaining recent working state
and a generated continuation is useful. It keeps the source's type and
performer metadata and runs the fork lifecycle, not creation.

## Maintaining instructions

Keep `AGENTS.md` to repository work rules and essential invariants. Put task
procedure in the context type, shared collaboration guidance in the optional
base, and syntax in emitted help and tool schemas. A character's enduring
commitments and memory sources belong with that character; the planned rc
union is not yet implemented. Detailed writing rules and terms live in
`docs/writing.md`.

Write the rule before its reason and show correct examples. Preserve the user's
objective, corrections, and unresolved work. Report observations, inferences,
and unknowns separately. Verify the complete rendered instructions and available
tools, including lifecycle selection and hydration, before attributing a result
to prompt wording.

The research in `docs/oss-comparisons.md`, "Direction for base, coder, and
general-purpose contexts" motivates this division of responsibilities. It is
source review and design evidence, not a measured model ranking or proof that
fewer words improve performance. Coder and director choose a stance tier during
creation; changing the model later updates runtime facts but does not rerun the
stance. Compare behavior on the same tasks before expanding model-name rules.
Measure system text and tool schemas separately with the target tokenizer or
provider usage; bytes alone do not measure token cost or effectiveness.

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

## Coder continuation and signoff

The coder stance tells a model to keep a task checkpoint while it works and to
record every unfinished operation ID and ask ID before it yields or signs off.
Shell work uses `foreground: false` by default. Its stable receipt names the
operation and any ask; completion arrives as a separate fact.

`kj wait` observes an operation, ask, or kaish job. It does not control that
work or resume a model. `kj interrupt <target>` stops a context's accepted
turn: soft by default, or `--immediate` to cancel the model stream and its
tool calls right away. `kj handoff signoff <note>` closes the continuation
window immediately. Otherwise, the window lasts 30 minutes from the last
actual provider inference request, including one made by a tool-loop iteration;
a yield does not extend it. The policy controls automatic model resumption and
does not expire an ask. Rotation remains manual and is not part of this
continuation mechanism. See `docs/approval-identity.md`, "Continuation windows
and async work".

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
call. Word guidance is advisory. The Claude, DeepSeek, and OpenAI-compatible
one-shot adapters request a 4096-token output cap separately from these values;
the CodexApp one-shot path does not apply that cap.

The summary transcript uses the conversation hydration admission rule. Excluded,
draft, ephemeral, file, trace, and ordinary system instruction blocks are absent.
The formatter supplies exact block identifiers, tool names, invocation and call
identifiers, error status, exit codes, and stderr when present. It keeps complete
recent turn groups instead of cutting the first 2000 bytes from every block.
Tool call/result pairs spanning an interleaved user block keep their intervening
turns together. Exclusion still wins; an excluded partner is never restored.
A retained tool result whose call is absent stays in the durable child, but
hydration repair omits that orphan result from the conversation. Distillation
can still summarize its eligible, labeled source text.

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

The source block version must still match when retained blocks are copied after the
model call. If it changed, compaction fails with a retry message instead of
combining a stale summary with a different selection. This guard does not
cover concurrent context metadata or environment changes; see `docs/issues.md`,
"Fork consistency across metadata and document commits". Copied blocks in
Running or Waiting become Error, as in other fork paths. Pending blocks remain
Pending; queued execution is not transferred to the child.

Compact mode has its own retention policy. It rejects `--include`, `--exclude`,
and `--preset`; use a filtered fork for those selections.
`--prompt` adds a new instruction and requests a child turn after initialization.

## The submit verb

`submit` fires after the server promotes a player's chat submission to a
durable user block, the way `drift` fires after a drift block lands
(`rc/mod.rs`, `rc::run`). It runs awaited inline, so
anything a script writes is durable before `submitInput` returns.

Its scripts read the submit facts as `KJ_*` variables. Every name is always
set, empty rather than absent when the fact does not apply:

| Variable | Value |
|---|---|
| `KJ_INPUT_BLOCK` | The user block the draft became (a block key) |
| `KJ_EDGE_BLOCK` | The newest block the client had shown when the player submitted (a block key), empty when the client said none |
| `KJ_EDGE_SHOWN` | Characters of `KJ_EDGE_BLOCK` the client held when the player submitted, empty unless that block was still streaming |
| `KJ_LOG_TAIL` | The newest block in the log other than `KJ_INPUT_BLOCK` and any unsent draft (a block key), ephemeral or not, empty when there is none |
| `KJ_TURN_LIVE` | `true` or `false` — whether a model turn was running when the submit arrived |

No context type links a `submit` script by default; a type opts in by
symlink, the way any rc verb does. `assets/defaults/rc/lib/submit/S10-edge.kai`
is the shipped example: it emits a `(System, Notification)` excerpt of the
edge block, but only when the player was looking at an older point in the
conversation than the log tail. See `docs/prompts.md`, "The submit verb" for the design this verb implements.

## Migration and comparison

Deploy the `--as` implementation and metadata-reading director/handoff scripts
together. A kernel that lacks the flag cannot create the intended performer
relationship. Replace contexts using the old `KJ_CHARACTER` bridge with verified
successors as described above; changing the seed does not rewrite their stored
instructions or assign their performer retroactively.

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
The [OSS dossier](oss-comparisons.md) keeps research and review lessons.
