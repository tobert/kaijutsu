# Senpai and kohai

A character at work is a **character instance**: one run of one character, with its own conversation, its own
senpai, and the settings it runs on. Every character instance that does work answers to one other character
instance: its **senpai**. It is that instance's **kohai**. The senpai edge is the only thing that routes asks,
reports and escalations. It replaces the reviewer override, the director column, delegation grants and the review
walk over `forked_from`.

Decided by Amy on 2026-10-18 (interview with senpai, the supervising Claude session of the nagaya run):

- Delegation grants, the reviewer override and the `forked_from` review walk are **replaced**, not kept beside it.
- The kj surface is a **hard cut**: the old routing verbs and flags go away in the same change.
- It is built in a worktree and lands as one branch.

Decided by Amy on 2026-10-19:

- "I *hate* fallbacks. remove it." A model comes from one place, and nothing stands in when it is missing.
- "mid-context re-cast would be bad." A conversation keeps the model and persona it started with.
- "I think the tables should look like character/cast static settings, then those get instantiated into a record
  in a more dynamic table, that is used for the character in operation." The name is `character_instance`.
- The senpai edge belongs to the character instance. This replaces the 2026-10-18 choice of an edge between
  contexts: steps 1-3 built that version, and step 5 moves it.
- A fork stays in the forking context's character instance.

Why: on 2026-10-18 the nagaya run needed four mechanisms to say "this banto answers to senpai". A banto's asks
went to Amy, so it could not even drift its supervisor. A delegation grant was refused ("only a live root
character may grant"). A reviewer change was refused ("only the lineage root may change approval routing"). The
working fix was to rotate the banto into a context that senpai created, and to teach a one-off `--family` ledger
rule so its drifts would not ask. The survey of the code found the same relationship stored in `reviewer_id`,
`director_id`, `approval_reviewer_delegations` and the `forked_from` walk, resolved by two near-identical
functions (`KernelDb::effective_approval_reviewer` and `Kernel::resolve_review_assignment`).

On 2026-10-19 the edge between contexts showed its cost: a rotation had to move the kohai of the old context to
the new one, because the thing that answers to a senpai outlives any one conversation. And a turn's model could
come from six places (a per-context override, the context's cast, the performer's default cast, `llm_defaults`,
a preset's cast at fork time, and hidden `max_tokens` floors), so a character change could recast a live
conversation, and a missing model fell back to a default.

## The model

Two layers. Static settings are edited in place. The dynamic layer is appended to and never modified.

```
static     characters           principal_id, name, root, created_at, retired_at, persona (step 6), cast_id,
                                handoff_ctx
           casts, cast_slots    a reusable model team: one slot per role
dynamic    character_instances  instance_id, principal_id, senpai_instance, started_at,
                                created_by
           instance_bindings    instance_id, seq, conversation_ctx, bound_at, cast_label, persona
           instance_slots       instance_id, seq, role, backend_id, model, max_tokens, temperature, top_p,
                                effort, thinking_budget, thinking_style
contexts                        instance_id, binding_seq, context_type, forked_from, ...
turns                           turn_id, context_id, instance_id, binding_seq, provider, model, raw tokens, ...
```

- **A character** is the persistent someone a name resolves to. Keys in `auth.db` bind to it. Its row holds
  static settings only: its name, the root flag, its persona (step 6) and the cast it uses. Editing them changes
  no running instance.
- **A cast** is a reusable model team: one slot per role, where a role is a context type, or `judge` or `gate`.
  Many characters may name one cast.
- **A character instance** is one run of one character. `kj kohai create` starts it. Its senpai is another
  character instance. Its character never changes. It is live while its conversation is live.
- **A binding** is the settings an instance runs on: the character's persona and a copy of its cast's slots, as
  they were when the binding was written. Binding `seq` 1 is written when the instance starts. A rotation appends
  the next `seq`, read fresh from the static settings, together with the instance's new conversation. Nothing
  else writes a binding, and no binding row is ever changed.
- **A context** belongs to at most one character instance and runs on one binding `seq`, both fixed when the
  context is made. The instance's **conversation** is the `conversation_ctx` of its newest binding. Its other
  contexts are forks, its judge shadow, and contexts its rc lifecycle makes. A context with no instance (scratch,
  the handoff log, the drift queue, lost+found, the beat score, cold-start discovery) runs no turns and routes
  nothing.
- **The root console** of a root character is a character instance with no senpai. Each root character has at
  most one live root instance. `characters.root_ctx` goes; the root console is that instance's conversation.
- **The kohai** of an instance are the live instances whose `senpai_instance` is that instance. The senpai chain
  never has a cycle. Setting an edge that would close one is refused.
- **The handoff log** belongs to the character: `characters.handoff_ctx`, as today. Every instance of the
  character writes and reads the same log, so a new instance sees the old one's notes. A rotation reads the
  predecessor conversation's handoff as it does today. For a fresh log, `kj context rotate` of the handoff context
  points `characters.handoff_ctx` at its successor.

What stays as it is:

- `context_type`: what a context does (its rc bundle and gate tier). The `director` type keeps its name until
  step 6.
- `forked_from` and fork kind: provenance only. `forked_from` is the only parent link (step 4).

## Where a model comes from

A model turn in context K runs on the slot for K's context type in the binding K was made with. That is the one
place.

- The slot gives the backend, the model and the sampling parameters. A parameter the slot leaves unset comes from
  the model's own row in `backend_models`. Nothing below them: kaijutsu supplies no parameter value of its own.
- `max_tokens`, which every provider request carries, must come from the slot or the model's row. A binding
  whose slot has neither refuses when it is written, naming the cast, the role and the model. An unset optional
  parameter (temperature, top_p, effort, thinking) is left out of the request.
- A binding with no slot for K's type gives K no model. `kj kohai create` and `kj context rotate` say so in their
  output ("<instance> has no model for <type>: cast <cast> has no <type> slot", or "<character> has no cast"),
  and `kj context info` shows the same line. A model turn in K refuses before any provider work, naming the
  character, the cast, the type and the fix (`kj cast slot set` or `kj character set <name> --cast`, then
  `kj context rotate`).
- The judge shadow runs on the `judge` slot of its instance's binding, and the gate on the `gate` slot, so a cast
  change moves neither in a running instance. Both read the binding, not the live cast: `seat_slot_backend`'s
  `effective_cast` lookup goes. Until step 7, an instance with no `gate` slot decides on `[council] server`;
  step 7 removes that fallback.
- A judge shadow belongs to one conversation, as today: the coder's archive rc archives it when a rotation
  archives the predecessor, and the successor's first gate call makes a new one on the successor's binding.
- Other model callers resolve the same way. A fork's distillation runs on the forking context's slot, or on
  `--distill-model <backend>/<model>`; a bare model name refuses. A one-shot prompt (summaries) takes
  `max_tokens` from the slot it runs on.
- A backend's connection facts (`base_url`, keys, timeouts, `max_concurrent`) are read live. They are
  infrastructure, not a model choice.
- To move a running instance to another model or persona, change the static settings and rotate it. To run two
  models side by side, start a second instance of a character whose cast has the other model.

## Routing

Reviewer of an ask raised in context K by actor A:

1. Let I be K's instance. A context with no instance refuses, naming K.
2. Let S be `senpai_instance(I)`. If S is null (I is a root console): when A is I's live root character, the ask
   is a self-confirmation; otherwise refuse, naming A and K.
3. The reviewer is S's character when it is live and is not A.
4. When S's character is A, climb: I := S, and repeat from 2.
5. A retired character refuses by name. It never substitutes another character.

There is one resolver.

- **Escalation** runs the same climb, past the instance that yielded the current reviewer, and refuses at an
  instance with no senpai, naming it.
- **Lineage root**: the character at the top of the senpai chain. It must be a live root character, or refuse
  with the reason.

## Authority

One rule: **the character of an instance's senpai, or its lineage root**, may:

- move it to another senpai (`kj senpai set`);
- escalate its asks;
- rotate it (its own character may also rotate it, as today);
- archive it.

The refusal names the rule: "only <instance>'s senpai (<character>) or its lineage root (<root>) may <act>".
An instance's character never changes, so there is no recast act; to put another character on the work, start an
instance of it.

Unchanged: a root character never performs an instance other than its root console; the performer can never
review its own work; a model turn refuses to start when its performer would review itself; a move refuses while
the instance has pending asks.

## Creating, forking, rotating

- `kj kohai create <label> --as <character> --type <t> [--senpai <instance>] [--parent <ctx>] [--cwd <dir>]
  [--env K=V]...` starts a character instance and its conversation. `--as` is required. The senpai is the
  caller's instance, or `--senpai <instance>` when the caller's character holds that instance or is its lineage
  root. `--parent` sets the conversation's `forked_from` only. It writes binding `seq` 1 from the character's
  persona and cast. It needs the Operator capability and a live character caller.
- `kj fork`: the child joins the forking context's instance and runs on the same binding `seq`.
- `kj context rotate <instance-or-ctx>`: a new conversation in the same instance, with the next binding `seq`
  read fresh from the static settings. The kohai and the senpai do not move: they point at the instance.
- `kj context rotate` of a character's handoff log points `characters.handoff_ctx` at the successor. A context
  with no instance has no loadout to check, so rotate skips that check for it; a context with no instance that
  is not a handoff log refuses rotation. `kj handoff note` refuses an archived log.
- `kj context archive` archives only the context it names. Archiving an instance's conversation makes the
  instance inactive: reports to it and `kj senpai set` onto it refuse, and its forks stay live. Contexts that
  should go with it go by their type's archive rc, as the coder's judge shadows do today
  (`coder/archive/S10-shadows.kai`). `kj context promote` of the conversation makes the instance live again.
- `kj senpai set <instance> <senpai-instance>`: moves a kohai. It needs the authority rule, refuses with pending
  asks, refuses a cycle, and refuses an inactive senpai instance.
- `kj senpai show [<instance-or-ctx>]`, `kj kohai list [<instance-or-ctx>]`, `kj senpai report "<text>"` work on
  instances; given a context, they use its instance.
- `kj context info` shows the context's instance, its binding `seq`, and the model for its type (or why it has
  none). `kj character show` lists the character's live instances.
- **Addresses.** An instance has no label of its own. It is named by its conversation's label, which a rotation
  carries to the new conversation as it does today. A verb that takes an instance accepts any of the instance's
  contexts and uses that context's instance.

## Reports and drift

- `kj senpai report "<text>"` puts the text in the senpai instance's conversation as a drift block.
- `kj drift push` along the edge, from a context of a kohai instance to its senpai's conversation or back, runs
  with no ask. Within one instance, a push from the conversation down to one of its forks runs with no ask. A
  push from a fork up to the conversation runs with no ask when the instance's own character made the fork (the
  fork's `created_by`), as a context forks itself the way Claude Code forks a subagent; a fork a person made
  keeps today's gate on the way up. Any other drift target keeps today's gate. A `gate.toml` key on `kj drift push` still wins over the
  edge (Amy, 2026-10-19).

## What a context sees

- The `<situation>` block names the instance and its senpai:
  `<instance label="<label>" character="<name>"/>` and `<senpai instance="<label>" character="<name>"/>`.
- The rc persona text (director S00-stance) says "our senpai" and reads the name from the kernel fact.

## Removed (hard cut)

Steps 1-3 (context edge):

- Columns `contexts.reviewer_id` and `contexts.director_id`; tables `approval_reviewer_delegations` and its events.
- `kj context set --reviewer`, `--clear-reviewer` and `--director`; `kj ledger delegation grant|revoke|list`;
  `kj context create` (now `kj kohai create`).
- The `forked_from` review walk, `responsible_character_above`, and the duplicate resolver.

Step 5 (instances and bindings):

- Columns `contexts.senpai_ctx`, `contexts.played_by`, `contexts.cast_id`, `contexts.provider`, `contexts.model`,
  `characters.root_ctx`, `presets.cast_id`, `presets.system_prompt`, `turns.cast_label`
  and `turns.model_source`; table `llm_defaults`; the `senpai_migration_report` table once its rows map.
- `kj context set --as|--model|--cast` and `kj context unset --model|--cast`; `kj kohai create -m|--model|--cast`;
  `kj fork --model`; `kj model` (`kj context info` shows the model); `kj backend default`;
  `kj preset save --cast|--system-prompt`. `kj models` (discovery) stays.
- RPC `configureLlm`, `setDefaultProvider` and `setDefaultModel`; the client's `set_context_model`. The wire's
  `ContextHandleInfo.provider` and `.model` carry the binding's model for the context's type; `.castLabel` carries
  the binding's cast label.
- The MCP `session.start` hook's model write; the live `GET /v1/models` lookup for a context window (a window that
  `backend_models` does not hold shows as unknown); the DriftRouter handle's provider/model copy and the turn's
  explicit `model` argument that outranks it.
- Every value kaijutsu supplies on its own: the 64000 `max_tokens` floor (`LlmRegistry::max_output_tokens`),
  `BuildOpts::new`'s 64000 (`BuildOpts` takes `max_tokens` from its caller), the openai-compatible client's 8192
  for a zero, and the one-shot prompts' 4096.
- Every resolver below the binding: `model_resolution.rs`'s override, registry-default rungs and
  `ModelSource`; `effective_cast` and its fall back to the performer's cast; the override's borrow of the
  current default backend; `LlmRegistry::prompt` and its first-advertised model; the tunable floor
  (`default_tunables`, `SlotTunables::over` onto it); `resolve_model_choice`'s bare name onto the default
  backend, and distillation's registry-default fall back.
- `characters.default_cast_id` becomes `characters.cast_id`, read only when a binding is written.

Presets keep only their fork filters (`kj fork --preset full|window|spawn`).

## Migration

The migration from the context-edge schema runs once, in one transaction, and logs every value it drops, as
`migrate_preset_cast_narrowing` does. It never invents a value: each instance and binding it writes records an
answer the old code would have given.

1. **Root consoles.** Each root character's `root_ctx` becomes its root instance, with no senpai.
2. **Instances.** Contexts are grouped into instances in creation order. A context with a performer joins the
   instance of its `forked_from` source when the source has the same performer and the context was forked or
   rotated from it; otherwise it starts an instance of its performer. "Same performer" means the current
   `played_by`: `kj context set --as` rewrote it in place, and no history of it exists. A rotation's link is
   its `ROTATED_FROM` env value, because a successor's `forked_from` is its predecessor's parent; a value that
   does not parse as a context id is logged and ignored. A context with no performer joins the instance of its
   `senpai_ctx`. A judge shadow (type `judge` with no performer, found by `forked_from` with archived rows
   included) joins the instance of the context it judges. A context with neither a performer nor a
   `senpai_ctx` has no instance, and neither has any character's `handoff_ctx`, which has a performer.
3. **Edges.** An instance's senpai is the instance of its first context's `senpai_ctx`. An instance whose mapped
   senpai would be itself or would close a cycle refuses the migration, naming the contexts.
4. **Bindings.** Each context gets the binding its next turn, its judge and its gate would have used under the old
   resolver (override, context cast, default cast and `llm_defaults` included), resolved from the rows as they
   stand at migration. `max_tokens` is what the old code sent: the slot's, the model row's, `llm_defaults`', or
   64000 when all are unset, written into the slot. A binding gets a `gate` slot only from a cast `gate` slot;
   without one, `[council] server` in `gate.toml` decides until step 7, since the migration cannot read the VFS.
   Contexts of one instance with equal resolutions share a `seq`; a different resolution starts the
   next `seq`, in creation order. A context the old resolver could not resolve gets a binding with no slot for its
   type, and the migration lists it.
5. **Conversation.** An instance's conversation is its newest live context that is not a fork child of another of
   its contexts or a judge shadow. No stored marker says an rc lifecycle made a context; one made as a fork
   child is caught by the fork clause. With none, its newest context of those kinds
   is its conversation, and the instance is inactive when that context is archived.
6. **Labels.** Labels stay on contexts; the migration moves none.
7. **Handoff.** `characters.handoff_ctx` stays where it is.
8. Archived contexts map like live ones; an archived context that cannot be mapped is left with no instance and
   listed, as step 1 did. Only live contexts refuse.

## Out of scope

- A verify tier for reviewers (read-only builds and tests) is a gate tier change, done separately.
- `@<name>` drift addressing (documented in docs/character.md, not verified as implemented).

## Tests (each falsified once)

- An ask in a kohai instance goes to its senpai's character; a self-held senpai climbs; a root console
  self-confirms; a top instance held by a non-root refuses; a retired character refuses by name; a context with no
  instance refuses.
- Escalation climbs one step and refuses at the top.
- The authority rule allows the senpai's character and the lineage root, and refuses a sibling and the kohai
  itself, for each act it governs.
- `kj senpai set` refuses a cycle, pending asks, and an inactive senpai.
- Archiving a conversation archives no other context; the instance is inactive until `kj context promote`
  restores the conversation; a report to an inactive instance refuses.
- Create writes binding `seq` 1 from the character's cast and persona; a fork joins the instance on the same
  `seq`; a rotation appends the next `seq` read fresh; a later cast or persona change moves no running context;
  the kohai of a rotated instance stay where they are.
- A binding with no slot for the type: create and rotate say so; a turn refuses and names the character, the cast,
  the type and the fix. A slot with no `max_tokens` in it or in `backend_models` refuses when bound.
- A turn uses the slot's parameters, then `backend_models`, and sends no unset optional parameter.
- The judge shadow and the gate use the instance binding's `judge` and `gate` slots.
- A drift along the edge runs with no ask, both ways; a drift from a conversation to its own fork runs with no
  ask; a drift up from a fork the character made runs with no ask, and up from a fork a person made asks; a
  drift to a sibling still asks; a `gate.toml` ask key wins.
- Two instances of one character read and write the same handoff log; rotating the handoff context repoints
  `characters.handoff_ctx`.
- The prompt's `<situation>` names the instance and the senpai.
- The migration: a rotation chain becomes one instance; a fork child joins its source's instance; a judge shadow
  joins the instance of the context it judges; a shared cast copies into each binding; a context with an
  override keeps its model; one resolved through `llm_defaults` keeps that model; an unresolvable one is listed;
  a cycle refuses; dropped values are logged.
- Each turn row names its instance and binding `seq`; the removed verbs and RPCs are gone (`kj model` and
  `kj context set --as` are unknown).

## Decided (step 5, Amy, 2026-10-19)

- **The handoff log belongs to the character.** "it should be by character for handoffs. If I want a fresh
  handoff I could rotate the handoff context perhaps."
- **Drift within one instance**: "downwards, yes, upwards, it depends." It depends on who made the fork: "Forks
  come from 2 places, me and the character/context. A context can fork for the same reasons you might in claude
  code using its fork ability."
- **A judge shadow belongs to one conversation**, not to the instance: "correct on the judge shadows". A rotation
  archives it with the predecessor, and the successor gets its own.
- **The `mcp` type prints "no model"** at create when its cast has no `mcp` slot, and we keep the line. It is
  noise, since the MCP client brings its own model and the context is more of a log, and it hurts nothing. A
  later direction: MCP offers a way to chat with kaijutsu's characters instead.

The sections below record what each step built. Steps 1-3 built the context-edge version that step 5 replaces;
where they conflict with the sections above, the sections above win.

## Open (step 1)

Choices made conservatively where the design was silent. None is a new rule; each refuses or maps exactly.

- **Create with no calling context** refuses. The design names the caller's context as the senpai; a caller with none (a bare `--parent` from a client that has not run from a context) has nothing to answer to, and the kernel does not pick one.
- **Cold-start discovery** (`bootstrap_discovered_context` in the server) mints contexts with no senpai, like the other parentless system contexts. The design's list of null-senpai system contexts does not name it. A non-root actor in one refuses at the top.
- **Judge shadows** (a fork child of a seat) take the seat as senpai, per the fork rule.
- **Migration, performer-less contexts**: with no performer there is no actor, so the old resolver has no single answer. The migration maps them to the nearest strict ancestor that has a holder, which is what a create or fork from that ancestor would have set.
- **Migration, retired holder on the old walk**: mapped to the ancestor whose holder is retired. The old resolver refused by name there; the new one refuses by name at the same context, so behaviour is preserved and startup is not blocked. An override naming a retired character has no such context and refuses the migration.
- **Migration, archived contexts** map like live ones (a live context's chain passes through them). An archived context that cannot be mapped is left a top and reported instead of refusing; only live ones refuse.
- **Migration, exhausted old walk** (nobody above, no override): the context stays a top, which refuses or self-confirms at ask time as the old resolver did.
- **`kj context set --reviewer/--director` and `kj ledger delegation`** still exist in this step but no longer route anything; step 2 removes them.
- **Escalation** climbs past the current reviewer by holder (not "the context that yielded it"); the two agree unless one character holds several consecutive senpai contexts.
- **A context with no performer** has no reviewer to report (`resolve_context_review` errors "no performer"), as before.

- **Escalation** climbs past the context the current reviewer holds, as designed. An ask whose reviewer was set by hand (`escalate --to`) to a character holding no senpai on the chain has no context to climb from; it refuses and asks for an explicit `--to`.
- **Dead guard**: `insert_new_context_rows` still checks that a performer is not its own reviewer. Under the senpai climb that cannot happen (the climb passes the actor), so the check is unreachable; step 2 may remove it.
- **Test fixtures**: contexts registered under a parent (`register_context`, council `live_context_with`) now take the parent as senpai, mirroring create and fork. `KernelDb::assign_test_review` replaces the reviewer override in fixtures: the reviewer holds a seat, and the context is placed under it. A held senpai is required, so fixtures that created a context from a context nobody performs now give that context a performer.
- **Tests rewritten for the new model** (old to new): `lineage_root_repairs_a_retired_explicit_reviewer` removed (covers `--reviewer`, step 2); `explicit_reviewer_can_assign_a_performer_when_the_lineage_root_is_retired` is now `the_senpai_holder_can_assign_a_performer_when_the_lineage_root_is_retired`; `context_info_keeps_metadata_when_explicit_reviewer_is_retired` is now `..._when_the_senpai_holder_is_retired`; `final_assignment_rejects_reviewer_revoked_after_precheck` and the egress twin move the senpai instead of revoking a delegation; `final_context_insert_rejects_a_reviewer_changed_to_the_performer` is now `..._a_performer_nobody_can_review`; `a_forked_from_cycle_fails_the_listing_and_the_escalation_loudly` is now a senpai cycle; the escalate tests expect "at the top" and a held root console; `context_review_assignment_belongs_to_the_director` drops the self-review step (unreachable); the server wire test `amy_default_and_director_delegation_route_approval_over_the_wire` is now `..._and_escalation_...` (delegation steps removed).
- **Verified**: `cargo test -p kaijutsu-kernel` 4107 passed, 0 failed, 19 ignored; `kaijutsu-server` and `approval-ledger` pass; fmt clean; clippy on kaijutsu-kernel lib 77 warnings (baseline ~78), none in new code.

## Decided after the docs pass (senpai, 2026-10-18, within Amy's decisions)

The docs rewrite (`4a5c8be3`..`ff20a31c`) found these gaps in the spec. Steps 2 and 3 build to these answers:

1. Character standing rules on `kj drive`, `interrupt`, `archive` and `kohai create` that tested
   `director_id == actor` now test "the actor holds the target's senpai" (the first half of the authority rule).
2. `--top` goes. Only root consoles and the listed system contexts have no senpai; `--senpai <ctx>` covers the rest.
3. `--parent` (provenance) and `--senpai` (routing) both default to the caller's context and may differ.
4. Parentless system contexts keep no senpai and raise no asks; if one does, it refuses loudly.
5. Egress authority follows the one authority rule.
6. `kj ledger escalate --to <character>` and the lineage root's take-over stay, as lineage-root-only overrides on
   top of the climb.
7. Fix container.md: "senpai" is the supervising session outside the container; "banto" is the director seat.
8. Step 5's replacement for a one-off `kj context set --model`, and step 7's home for `[council] server` and the
   per-slot backend choice, go to Amy when those steps start.
9. Step 2 also removes the stale doc comment naming `responsible_character_above` (kernel_db.rs ~8562).

## Follow-ups in the same stack (decided by Amy, 2026-10-18)

Amy, on the cleanup after senpai/kohai: "1. gates would have a cast too, sorta like we did in kaibo's config. but
yes let's plan on that later or in parallel. 2. yes simplify. maybe a context doesn't need the cast attached? each
turn will have an id we can trace back to cast and model right? 3. I'm cool with moving persona to the character
and clarifying. 'director' predates the character even existing, and was an early experiment we can rework/remove/
replace and just didn't finish doing it. 4. good. do it."

These are later steps on this branch, after steps 1-3:

4. **One parent link.** `forked_from` is the only parent link. The 'structural' rows in `context_edges` that
   duplicate it go; drift edges stay.
5. **Character instances and bindings.** Each turn writes a durable record (done, `959b6dac`). Then the senpai
   edge moves to the character instance, and a context runs on the binding its instance had when the context was
   made: "The model" and "Where a model comes from" above. This replaces the first version of this item, a
   context with no cast (Amy, 2026-10-19).
6. **Persona on the character.** The character sheet carries the persona text, and a binding copies it; the
   context type says what the context does. The `director` type, which predates characters, is reworked or
   replaced, and its persona file (S00-stance) moves to the character that plays it.
7. **The gate's own cast**, as in kaibo's config: the reserved `gate` role leaves `cast_slots`. After step 5.
   Until step 7, an instance with no `gate` slot decides on `[council] server`; step 7 removes that fallback.

## Open (step 2)

Choices made where the design was silent, the smaller shape each time.

- **Escalation**: the ask's assigned reviewer may climb as well as the rule's two (the senpai's holder, the lineage
  root), so a reviewer reached by one climb can pass the ask on again. `--to` and taking an ask over are the
  lineage root's alone; with no live lineage root nobody may use `--to`.
- **Archive inside an rc lifecycle**: a nested `kj context archive` from an rc lifecycle (privileged, `rc_depth >
  0`) is not checked again. The shipped coder archive bucket archives the seat's judge shadows, whose senpai the
  seat holds, not whoever archived the seat; the verb that started the lifecycle was checked.
- **Archiving a holderless top**: a parentless system context with no holder has no senpai and no lineage root, so
  `kj context archive` refuses it for everyone. Nothing in the shipped rc or bootstrap archives one.
- **The old columns** are on `contexts` (`contexts.reviewer_id`, `contexts.director_id`), not on `characters`.
- **`kj kohai create` flags**: it keeps `kj context create`'s whole flag set (`--name`, `-m/--model`,
  `--system-prompt`, an optional `--type` defaulting to `default`) minus `--top`, plus `--senpai`. The spec's
  synopsis lists fewer; step 5 removes the per-context model.
- **`kj senpai set`** also needs the caller to hold the new senpai or be its lineage root, the same rule as
  `--senpai` at create, and the operator capability, as other changes to an existing context do. It refuses an
  archived senpai. A successful set clears the migration report row, so `kj senpai show` stops printing the note.
- **Character standing rules on `kj kohai create`**: a bare create is covered; `--senpai <ctx>` is covered only when
  the actor holds `<ctx>` itself; `--parent` is provenance and does not narrow the rule.
- **A characterless creator refuses**: a key in `auth.db` whose principal has no character sheet can no longer
  create a context (the server wire test now expects the refusal).
- **`kj context info`** shows `Senpai:` and its record has `senpai_ctx` in place of `reviewer_override_id` and
  `director_id`. `llm.turn` no longer records `director.id`.
- **Kept**: the dead self-review guard in `insert_new_context_rows` (step 1 note) stays.
- **contrib/agent-dev/bootstrap.sh** moved to `kj kohai create` in this step: the coder is created with
  `--senpai "$project-banto"` instead of `kj context set --reviewer banto`, and the root's gate allow names
  `kj kohai create`. The council seed scripts, `headless-app.sh` and the demo storyboard moved too.
- **Left for step 3**: `contrib/bench/rc-variants/council/coder/create/S04..S06` still run `kj context create --top`.
  They run inside a coder's create lifecycle, where `--top` has no equivalent until we choose which context the
  council contexts answer to.
- **Pre-existing failures**: `kaijutsu-tui` `terminal_fit` tests `ctrl_a_r_rotates_the_seat_and_follows_its_successor`
  and `the_prefix_leaves_the_editor_parked_and_the_way_back_restores_it` fail on `3260b9a6` as well: the coder probe
  seat lacks operator for `kj context rotate`.

## Open (step 3)

Choices made where the design was silent, the smaller shape each time.

- **Where the edge allow sits.** A drift along the senpai edge is allowed by a `senpai edge` layer of the gate's
  own evaluation (`gate_policy::evaluate`). It decides only a command that no user rule, family rule or
  `gate.toml` key decided, ahead of the uncovered tier, so an `ask` or `deny` key on `kj drift push` stays firm
  and a learned deny still wins. Broker PreCall reads no database and sees no edge, so the pre_call hooks still
  run before the gate allows. A `KjVerb`-origin ask never meets the layers and is unchanged.
- **What the edge reads.** `kj drift push <dst>` whose target, resolved as the verb resolves it (`.parent`,
  labels, ids), is the caller's senpai or a context whose senpai is the caller's; and `kj senpai report` from a
  context with a senpai. Every argument must be a literal word and the command plain (no redirect, background
  flag or heredoc), since the target is read from the arguments; `--` refuses, as the character condition does.
  An archived or concluded kohai passes the edge and is refused by the verb itself.
- **`kj senpai report`** needs the drift capability, as `kj drift push` does, and delivers at once (no
  `--stage`). It reuses push's delivery by the senpai's full id, so staging on a failed delivery and the target's
  drift lifecycle are the same. It refuses a context with no senpai, an archived senpai, and an empty report.
- **The `<senpai>` element** uses the short id, like `<context>`. `character` is the holder's name, named even
  when retired (the ask refuses by name later); it is left out when nobody holds the senpai, and `context` is
  left out when the senpai has no label. A context with no senpai has no element. A senpai with no row stops
  the turn. `kj context prompt` reads the same fact and no longer resolves the reviewer, so it no longer refuses
  where review routing would; the turn still does.
- **A nested archive in an rc lifecycle** (step 2 follow-up 1) passes when the seat, the context whose lifecycle
  runs, is the target's senpai, and only then. A lifecycle has no more power than its seat: neither the seat's
  holder nor whoever started the lifecycle lends it anything (Amy, 2026-10-19). The shipped coder archive
  bucket needs no more, since the seat is its judge shadows' senpai.
- **The council bench contexts** (`contrib/bench/rc-variants/council/coder/create/S04..S06`, step 2 follow-up 2)
  are made with `kj kohai create` inside the first coder's create lifecycle, so they answer to that coder seat.
  They raise no asks, so the edge routes nothing for them today.
- **bootstrap.sh** cannot put banto under senpai's context: that context is made later, when senpai's MCP session
  registers. banto starts under Amy's root console, and the script prints the `kj senpai set` Amy runs once
  senpai's context exists. Automating that move is left for the deploy.
- **The director stance** reads the senpai from `kj senpai show --json` (the holder's name and the senpai's label)
  and writes "Our senpai is <holder>, in <context>." The contrib S12-environment files call the session outside
  the container "the supervisor" where they mean the `/exchange` directories, and "our senpai" for the edge.
- **Tests rewritten**: `performer_and_reviewer_are_distinct_runtime_facts` is now
  `performer_and_senpai_are_distinct_runtime_facts`; it pinned the `<reviewer>` element the spec replaces.

## Open (step 4)

Choices made where the design was silent, the smaller shape each time.

- **No `EdgeKind`, no `kind` column.** With the structural rows gone every row of `context_edges` is a drift edge,
  so the enum had one variant and the column one value. Both are deleted, along with the `idx_edges_structural_unique`
  index, `KernelDb::structural_parents`, `structural_children`, `would_create_cycle`, `KernelDbError::CycleDetected`
  and the unknown-kind fallback that defaulted to Structural. `insert_edge`, `edges_from` and `edges_to` lose their
  kind arguments. The table keeps the name `context_edges`; renaming it would touch every drift query for no gain.
- **The migration** (`KernelDb::drop_structural_edges`) runs on open. A database without the `kind` column is done,
  so it needs no `kernel_migrations` marker. It compares each structural row with `contexts.forked_from` and
  refuses, writing nothing, for: a target whose `forked_from` is not the row's source (including `forked_from`
  null), a target with two or more structural parents, and any kind other than `structural` or `drift`. The error
  names every such context by short id and repeats on each open until the rows are corrected by hand. A context with
  `forked_from` set and no structural row is fine: `forked_from` alone is the link.
- **Children come from `forked_from`.** `KernelDb::fork_children` (live children, oldest first) replaces
  `structural_children` for `kj context archive --children`, the judge shadow lookup and `kj context info`. The
  info `Children:` count now counts live children only; it used to count every edge, archived children included.
- **`kj context list --tree`** is depth-first: each context is followed by its whole subtree, siblings by creation
  time. The old walk ordered by depth then creation time, so a context created after a sibling that already had
  children was listed between that sibling and its children, and the children looked like its own. A live context
  whose parent is archived is listed as a root of its own, where the old walk dropped it from the tree. A cycle in
  `forked_from` among live contexts fails the listing instead of omitting its members.
- **`insert_new_context_checked` and `insert_new_context_rows`** lose their `parent_id` argument. The row's
  `forked_from` is the parent.
- **`drift_edge_rm_refuses_structural`** is deleted with the kind it guarded. `delete_drift_edge` keeps its name;
  no other kind of row exists to protect.
- **The `forked_from` immutability** is not enforced by a trigger. No code path updates it after insert; a trigger is
  a second mechanism for a rule nothing breaks.
