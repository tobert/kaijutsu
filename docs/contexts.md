# Contexts, conversations, and the context graph

A **context** is durable metadata (`contexts` row) and a kernel-sequenced block
log (`Conversation` document) with edits and exclusions. A **conversation** is
the live append-only message sequence shipped to an LLM provider, hydrated from
a context at boundary events (create, fork, cold start, or attach).

## Context vs. conversation

- **Single sequencer.** The kernel sequences every accepted mutation and
  commits the semantic operation and materialized block state atomically before
  publishing on the `ChangeFeed` (`op_seq`). Block text is a plain `String`;
  streaming appends with `push_str`.
- **Hydration (`llm/hydrate.rs`).** At a boundary event, the kernel walks the
  context's block log, skips blocks marked `excluded` or `ephemeral`, applies
  snapshot repair to pair interleaved tool calls and tool results for the
  provider wire without altering durable block order, and builds the provider
  message list.
- **Mailbox cursor (`llm/mailbox.rs`).** While a conversation is live, async
  writers (background shell completions, incoming drifts, file-watch notices,
  sibling writes) append blocks to the durable log. `ConversationMailbox` holds
  a cursor (`HydrationState` + seen `BlockId` set) over the log and folds new
  blocks into the conversation between tool rounds (`catch_up`).
- **Exclusions (`kj stage exclude|include`).** Excluding a system instruction
  block takes effect on the next turn. Excluding conversation history blocks
  takes effect on the next hydration boundary:
  `kj stage exclude <id> && kj fork` hydrates a child without those blocks.

## The context graph: forks, senpai, and drift

Three distinct edges connect contexts:

| Edge | Column / table | Shape | Purpose |
|---|---|---|---|
| **Fork** | `contexts.forked_from` | Forest (acyclic) | Provenance: which parent context a child was copied from (`kj fork`). |
| **Senpai** | `contexts.senpai_ctx` | Tree rooted at `root_ctx` | Accountability: who reviews asks, receives `.senpai` drifts, and holds authority (`docs/character.md`). |
| **Drift** | `drift_edges` | Directed graph (cyclic) | Content transfer between contexts (`kj drift push\|pull\|merge`). |

### Forking (`kj fork`)
- `kj fork` copies selected blocks from the current context into a new child
  context with the same performer, setting `forked_from` and `senpai_ctx` to
  the parent context.
- `--include` and `--exclude` filter block ranges (`0:5`, `end-10:`) or block
  keys; `--preset <name>` applies a stored fork filter preset.
- `--drop images|thinking|tool_results|mcp:<server>` replaces heavy older blocks
  in the child with compact stubs naming the source block ID for
  `kj block read`, while keeping the last turn's thinking intact.
- `--compact` summarizes earlier turns on the context's `distill` model slot,
  retaining system instructions and the latest complete turn.
- `--stage` creates a staged fork whose block exclusions can be refined before
  `kj stage commit`.

### Drift and dead letters (`kj drift`)
- **Immediate by default.** `kj drift push <dst> <content...>` writes a
  `BlockKind::Drift` block directly into the destination context and runs the
  destination's `drift` rc lifecycle. `.senpai` addresses the caller's
  `senpai_ctx`; `.parent` addresses `forked_from`.
- **Staging.** `kj drift push --stage` queues a `StagedDrift` in the durable
  drift queue context (`ensure_drift_queue_context`) until `kj drift flush` or
  `kj drift cancel <id>`.
- **Distillation.** `kj drift push --summarize`, `kj drift pull <src>`, and
  `kj drift merge` distill a context using the caller's `distill` slot.
- **Turn boundary.** A drift never starts or extends a turn on its own unless
  the destination's `drift` rc script calls `kj drive`.
- **Console gate.** A drift into a person's console (`characters.console_ctx`)
  lands held (`Status::Pending` and `excluded = true`) and opens an ask;
  `kj ledger allow <id>` releases it.
- **Retries and `lost+found`.** Failed deliveries retry up to
  `MAX_DRIFT_RETRIES` (5), then move to the durable dead-letter log and drain
  into the well-known `lost+found` context (`ensure_lost_found_context`).

## Working directory

A shell `cd` changes the context's working directory. The kernel reads the
saved directory once at the start of each model tool batch. Call `cd` in a
separate batch before using relative paths in that directory.

## Rotation, handoffs, and archive

- **Rotation (`kj context rotate [<ctx>]`).** Creates a successor context
  played by the same character under the same senpai, with fresh `context_slots`
  copied from the character, copied environment, working directory, file
  watches, and `context_control_vectors`, and `ROTATED_FROM` set to the
  predecessor's ID. Runs `create` rc (`S16-handoff.kai` injects recent notes
  from `kj handoff tail`; `S17-predecessor.kai` injects the predecessor tail
  unless `--migrate` is passed). Moves `root_ctx`, `console_ctx`, or
  `handoff_ctx` pointers atomically and archives the predecessor.
- **Archive (`kj context archive <id> --confirm`) and promote
  (`kj context promote <id>`).** Contexts are never deleted. Archiving runs the
  performer's `archive` rc lifecycle, abandons pending asks for that context,
  and marks `archived_at` while retaining all blocks, lineage, execution
  receipts, and approval history. From the live senpai context it plays, a
  character can archive a direct kohai without `operator`. The default shell
  policy allows a plain command with one literal target. Neither exception
  applies to consoles or shadows. Fork provenance grants no archive
  authority, and `--children` still selects by fork provenance under the
  existing authority checks.

## Context shadows and renditions

A **context shadow** (`context_shadows` table, keyed by `(source_ctx, kind)`)
is a child context that follows a source context and holds renditions of its
blocks:

- **Judge shadows (`kind = 'judge'`).** Created by the council gate and
  performed by the `judge` character named in the source context's `gate.toml`
  layer. Holds the source context's outward command history in order, without
  tool outputs or thinking. Archived automatically when its source context is
  archived.
- **Media description shadows (`kind = 'description'`).** Created on demand by
  `kj shadow describe <block_id>` and performed by the source context's own
  character using its `image` (or `audio` / `video`) media slot copied as the
  shadow's `model` slot. Each generated description block is linked in
  `rendition_links` and cached in `rendition_index` by `(cas_hash, kind,
  backend_id, model, preset)` so repeated describes of the same media asset
  reuse the existing rendition.

## Orchestration and peers

- **Driving and waiting.** `kj drive <ctx> [prompt]` starts one autonomous turn
  on a target context. A seat that holds `drive` without `operator` (a kohai)
  may drive only itself and its senpai (`.senpai`); `operator` reaches any
  context. `--if-idle` starts the turn only when the target has none running,
  and a busy target is left alone with nothing written. `kj wait [<ctx>] [--since <cursor>] [--timeout <secs>]`
  waits for accepted turns, a background shell `--operation`, an `--ask`
  decision, or a kaish `--job`, returning the newest blocks and a `cursor` for
  the next wait (`status: idle | ask | running`).
- **Claude Code peers (`kj cc`).** `kaijutsu-mcp` sessions register in the peer
  registry (`mcp/<label>`) and listen on a Unix socket inbox (`cc_inbox`).
  `kj cc list` inspects live sessions and `kj cc send <session> <msg>` delivers
  messages through the drift router.
