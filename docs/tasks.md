# Task Blocks & Grooming

The enabler for kaijutsu as a daily-task-grooming household agent (Amy:
*"Task BlockKind and tool is a great idea"* — `docs/issues.md` "Household-agent
arc"). Neither reference harness we surveyed (hermes-agent, QwenPaw) beats a
todo tool writing JSON to a file on disk; a `BlockKind::Task` gets
multi-frontend task sync from the CRDT for free — the model, the app, and a
sibling context all see the same task state converge, the same way every
other block already does.

## Status

**Designed + SHIPPED 2026-08-04.** `BlockKind::Task`, the `TaskStatus`
CRDT-synced field, `builtin.tasks` (create/update/complete/cancel/list),
hydration. **Deferred, on purpose** (see "Deferred" below): `kaijutsu-app`
rendering beyond a placeholder line, a `task_reparent` verb, and the
out-of-band-change notification companion described in "Hydration" below.

## Decisions

1. **Task is Text-shaped plus one new CRDT field, not a payload struct.**
   Compare `Notification`/`Resource`, which carry a dedicated payload struct
   (`NotificationPayload`/`ResourcePayload`) because they're broker-emitted,
   write-once records of something that already happened. A task is the
   opposite: ordinary authored content (a title/description, same as any
   `Text` block's `content`) plus ONE thing that mutates —
   its lifecycle status. So `content` carries the title, and `task_status`
   (+ its own LWW clock `task_status_at`) rides directly on `BlockHeader`/
   `BlockSnapshot` as a `Copy` field, mirroring `content_type`/
   `content_type_at` field-for-field (same per-field-clock mechanism in
   `kaijutsu-crdt/src/content.rs`: `set_task_status`, `merge_header`'s
   `field_wins` tiebreak). No new payload struct, no new mutation plumbing
   to invent — the exact machinery `content_type` already proved out.

2. **`TaskStatus` is a dedicated enum — `Open`/`InProgress`/`Done`/
   `Cancelled` — not a reuse of the existing block `Status`
   (Pending/Running/Done/Error).** `Status` is tool-execution shaped: its
   `Error` variant means "the tool crashed," and its LWW tie-break order
   (`Error > Done > Running > Pending`) exists specifically so a concurrent
   completion report can never mask a real failure. Neither half fits a
   task. A task never "errors" — and there is no honest way to fold
   `Cancelled` (an intentional groom decision) into `Status` without either
   silently dropping it or lying and calling it `Error`. Given the explicit
   ask for a cancel verb, and the house rule that silent fallbacks are a
   mistake, a small dedicated enum was the only honest option.
   `TaskStatus`'s own tie-break order (`Cancelled > Done > InProgress >
   Open`, pinned by `test_task_status_lww_tiebreak_order` in
   `kaijutsu-types` and exercised end-to-end by
   `test_per_field_lww_tiebreaker_task_status` in `kaijutsu-crdt`) makes
   both terminal states dominate the non-terminal ones, and treats a
   same-timestamp cancel as the more deliberate of two concurrent terminal
   writes.

3. **Subtasks reuse the ordinary DAG `parent_id` edge — no bespoke
   hierarchy.** `task_create` accepts an optional `parent_id`; `task_list`
   filters by it. There is no separate "task tree" structure to keep in
   sync with the block DAG, because there isn't a second structure at all.

4. **`builtin.tasks` (`crates/kaijutsu-kernel/src/mcp/servers/tasks.rs`) is
   a sibling of `builtin.block`, not an extension of it.** `block.rs`
   already has a generic `block_status`/`block_list`, so extending it was
   the obvious first instinct — rejected because task grooming wants a
   *curated* surface (5 verbs, `TaskStatus` semantics, open/done bucket
   filtering) that a household-agent MCP session should be grantable
   without also handing it `block.rs`'s generic block-editing tools
   (`block_edit`, `block_splice`, arbitrary `block_create` of any kind).
   Same reasoning `builtin.shell`/`builtin.shell_readonly` already split
   on for an analogous "narrower, purpose-shaped grant" reason. The
   `BlockKind::Task` block underneath is an ordinary block — `tasks.rs`
   delegates to the same `SharedBlockStore` `block.rs` uses, it's just a
   different curated verb set on top.
   - Verbs: `task_create` (content, optional `parent_id`/`status`),
     `task_update` (content and/or status — at least one required, fails
     loud otherwise), `task_complete`/`task_cancel` (status shorthands),
     `task_list` (optional `parent_id` filter; `filter: "open"` = Open ∪
     InProgress, `"done"` = Done ∪ Cancelled, or an exact status name).
   - Registered in `Kernel::register_builtin_mcp_servers` under
     `InstancePolicy::for_kernel`, same as every other builtin. No
     `kaijutsu-mcp` (the external stdio server) changes were needed to
     reach Claude Code / other MCP-attached models: an earlier MCP
     slim-down (see the `#[tool_router] impl KaijutsuMcp` doc comment in
     `crates/kaijutsu-mcp/src/lib.rs`) already replaced per-tool wrappers
     there with generic `kaish_exec`/`list_kernel_tools` escape hatches
     gated by the calling context's broker-level capability grant. The
     `mcp`/`coder`/`director` rc loadouts
     (`assets/defaults/rc/lib/create/S10-binding.kai`)
     grant `*` (`Capability::AllInstances`), so `builtin.tasks` is visible
     the moment it's registered — the "curate the external subset"
     framing in the original task brief predates that slim-down.
   - Task's `role` is `Role::Tool` (created via a tool call) — deliberately
     NOT forced to `Role::System` the way `notification_payload`/
     `resource_payload`/`error_payload` force it on `BlockSnapshotBuilder`.
     Those are broker-emitted events; a task is ordinary authored content,
     so it follows the `svg_block`/`abc_block`/`diff_block` precedent
     (tool-created rich content stays `Role::Tool`) rather than the
     broker-event one.
   - No `task_reparent`/reorder verb: `kaijutsu_crdt::BlockStore` has no
     cheap "move to a different parent" primitive today (`move_block` only
     reorders siblings under the SAME parent) — building one just for this
     slice would be exactly the "bespoke mechanism" decision 3 above
     avoided. Deferred to `docs/issues.md` rather than built.

## Hydration

**This was the one genuinely nuanced call**, flagged explicitly in the task
brief because tasks — unlike almost everything else already in the block
model — are edited frequently *mid-conversation*, and the kernel already has
an established (if informal — see the "cache-placement rule" comment in
`kj/lifecycle.rs`'s datetime-injection test) convention against anything
that would silently invalidate an LLM provider's prompt-cache prefix
(`--target=system` breakpoint, Claude `cache_control`). The load-bearing
precedent is `BlockKind::Notification`
(D-34, `docs/issues.md` "Context time awareness" — SHIPPED 2026-07-04): it
hydrates as an **appended** user-role message and is **never swept into the
assembled system prompt**, specifically so a frequently-emitted block type
can't daily-invalidate a cache breakpoint the way a `(Role::System,
BlockKind::Text)` block would (folded fresh into `extract_system_prompt_sections`
on every call).

Two mechanisms make that precedent transfer cleanly to Task:

1. **`extract_system_prompt_sections` filters on `kind == BlockKind::Text`.**
   A Task block — whatever its `role` — can never enter the assembled
   system prompt, full stop, regardless of anything else in this document.
   That's the hard safety property; everything below is about what the
   model actually *sees* mid-conversation, not about cache safety, which
   is already settled.

2. **`HydrationState::translate_block` (`llm/hydrate.rs`) gets a `(_,
   BlockKind::Task)` arm**, wildcard on role (same shape as Drift/Error/
   Notification/Resource), that formats the block's *current* snapshot
   fields (`format_task_for_llm`: status + content + parent, as an
   `<task ...>` XML envelope) and appends it as a fresh user message via
   `self.flush_all(); self.messages.push(...)`. Critically, this runs
   through the SAME dispatch both hydration paths share:
   - **Bootstrap** (`hydrate_from_blocks`, full re-hydrate at a boundary
     event — fork, new context, cold start, attach) walks the block log and
     calls `translate_block` once per block, using whatever the block's
     CRDT-merged fields currently say. So a boundary re-hydrate always
     shows the *true current* task state — an edit made an hour ago and a
     status flip made a second ago both show up identically, because
     bootstrap has no memory of a "previous" render to be stale relative
     to.
   - **Live** (`ConversationMailbox::feed`/`catch_up`, called every turn
     with the current `block_snapshots(context_id)` result) keys its
     `seen: HashSet<BlockId>` by block id and **only ever translates an id
     once** — `feed`/`catch_up` silently skip a block already in `seen`.
     This is the mechanism, not a side effect of it: a task's `content`/
     `task_status` mutating in place (exactly what `task_update`/
     `task_complete`/`task_cancel` do — same `BlockId`, LWW-merged fields)
     is invisible to `catch_up` from the second call onward, because the
     id was already folded in. **The already-hydrated message's bytes
     never change, so whatever cache breakpoint it landed under never
     invalidates.** This is proven directly by
     `task_status_mutation_after_hydration_does_not_alter_cached_prefix`
     in `llm/mailbox.rs`: feed a task block once, mutate `task_status`/
     `content` on the same in-memory snapshot, `catch_up` again (asserts
     `new_blocks == 0`), and assert the mailbox's `snapshot()` output is
     byte-identical (compared via serialized JSON — `Message` has no
     `PartialEq`) before and after. A companion assertion proves this
     isn't vacuous: a **fresh** mailbox fed the same (mutated) block DOES
     produce a different envelope — the staleness is real and scoped to
     "within a live, already-hydrated conversation," not "task edits are
     invisible forever."

**What this means in practice** for the household-agent's own grooming
loop: when the MODEL itself calls `task_update`/`task_complete`/
`task_cancel`, the change reaches the SAME turn's conversation for free —
not through any task-specific mechanism, but because the tool_call/
tool_result pair those MCP calls produce are themselves new, never-before-
seen blocks that `catch_up` folds in normally (ordinary `(Role::Model,
BlockKind::ToolCall)`/`(Role::Tool, BlockKind::ToolResult)` hydration, no
special-casing needed). The model reads its own tool result and knows the
task changed. **What does NOT reach a live conversation**: a task changing
out from under the model — Amy toggling a task done from the app, or a
sibling context completing it — mutates the block in place, which (per the
mechanism above) an already-materialized `ConversationMailbox` will not
surface until the next boundary re-hydrate (fork/new/cold start/attach).

**Deferred, not solved, by this slice:** the fix for that last gap doesn't
need new machinery — it needs the SAME one `Notification` already provides.
A future slice can have `set_task_status`'s kernel-level wrapper (or the
`kj task` surface, when one exists) emit a companion `BlockKind::Notification`
block ("task \<id\> marked done") whenever the write's principal differs from
the task's own author, reusing the existing `NotificationKind`/broker-event
convention verbatim rather than inventing a "TaskChanged" hydration path.
Not built here because it's speculative (household-agent multi-principal
grooming isn't live yet) and the CRDT-sync half of the promise — every
frontend converging on the same `task_status` — already works today via the
`BlockFlow::MetadataChanged` event any subscriber (the app, a sibling
context) can observe directly, with or without an LLM in the loop. See
`task_groom_is_visible_on_the_block_flow_bus` in `mcp/servers/tasks.rs` for
the test proving that half.

## Deferred

- **`kaijutsu-app` rendering.** `view/format.rs` has a minimal placeholder
  (`[status] content`, role-based color reusing the Text/File palette) so a
  Task block isn't invisible, but no dedicated status glyph, subtask
  indent, or in-app groom actions. Explicitly out of scope per the
  originating issues.md entry ("app/kj rendering can trail").
- **`kj` CLI surface.** `kj block create --kind task` and `kj search --kind
  task` work (the generic kind-parsers gained a `"task"` arm), but there is
  no dedicated `kj task ...` subcommand family. Falls out for free from the
  generic surface; a dedicated one is future work if the MCP tool surface
  turns out to be insufficient for kj-driven grooming.
- **`task_reparent`** — see Decision 4.
- **Out-of-band-change notification** — see "Hydration" above.
- **Priority/due-date/tags** — not asked for by the originating issues.md
  entry (create/update/complete/cancel/list only); adding fields to a
  `Copy` `BlockHeader` field is cheap if a real need shows up, but wasn't
  invented speculatively here.
