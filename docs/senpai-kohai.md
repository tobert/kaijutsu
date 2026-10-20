# Senpai and kohai

A character at work in a context is a **context character**: the fusion of one character with one context, with
the senpai that context answers to and the model settings it runs on. Every context a character plays answers to
one other context: its **senpai**. It is that context's **kohai**. The senpai edge is the only thing that routes
asks, reports and escalations. It replaces the reviewer override, the director column, delegation grants and the
review walk over `forked_from`.

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

Decided by Amy on 2026-10-20, after step 5 was first built on character instances that spanned contexts:

- One context character per context. "why does an instance need to have a conversation? I get why a
  conversation has an instance." The name: "context_characters ... is a better name than 'character instance'
  anyways since it's the fusion of a character with a context."
- Casts melt into characters: "melt them, it's too complex having both layers."
- Point-in-time settings are what `kj kohai create` options set. A context character could be updated at
  runtime, "since they'd be isolated. If freezing simplifies things, we can freeze it."
- Rotation does not move kohai in the kernel: "A rotating senpai knows who its kohai are so we should figure out
  a handoff rather than trying to embed to much of this in kaijutsu." The successor adopts them ("successor
  adopts"). "asks that get caught in between should get queued. the new senpai could do a quick query for any
  lingering asks in rc".
- A fork takes the forking context as its senpai, so drift along the edge covers drift between a context and its
  forks ("yep").

Why: an instance that spanned contexts had to point back at its current conversation, and the kernel inferred
that pointer from the newest binding. A rotation that failed after create left the pointer on the unlabeled
successor, and whether an instance was active depended on whether that inferred conversation was archived, so
archiving one context ended an instance. One record per context removes the pointer and both inferences.

## The model

Two layers. Static settings are edited in place. A context character is written when its context is made; only
its `senpai_ctx` changes after that, through `kj senpai set`.

```
static     characters           principal_id, name, root, created_at, retired_at, handoff_ctx, root_ctx,
                                persona (step 6)
           character_slots      principal_id, role, backend_id, model, max_tokens, temperature, top_p, effort,
                                thinking_budget, thinking_style
dynamic    context_characters   context_id, principal_id, senpai_ctx, persona, created_at
           context_slots        context_id, role, backend_id, model, max_tokens, context_window, temperature,
                                top_p, effort, thinking_budget, thinking_style
contexts                        context_type, forked_from, label, archived_at, ...
turns                           turn_id, context_id, provider, model, raw tokens, ...
```

- **A character** is the persistent someone a name resolves to. Keys in `auth.db` bind to it. Its row holds
  static settings only: its name, the root flag, its persona (step 6) and its handoff log. Editing them changes
  no context.
- **A character slot** is the model a character uses for one role, where a role is a context type, or `judge`,
  `gate` or `distill`. A NULL parameter takes the model's `backend_models` value when a context character is
  written. There are no casts: each character holds its own slots.
- **A context character** is one character in one context: `context_characters`, keyed by `context_id`. It
  names the character (`principal_id`), the context's senpai (`senpai_ctx`, another context), and the persona,
  and its `context_slots` hold a copy of every character slot as it was when the context was made, with unset
  parameters filled from `backend_models` and the create options applied. A trigger refuses a change to
  `principal_id`, `persona` and the slots. `persona` is NULL until step 6 gives characters a persona.
- **A context with no context character** (scratch, the handoff log, the drift queue, lost+found, the beat score,
  cold-start discovery) runs no turns and routes nothing.
- **A context's performer** is its context character's `principal_id`. `contexts.played_by` stays gone.
- **The root console** of a root character is `characters.root_ctx`, a pointer like `handoff_ctx`. A context
  with no senpai must be its character's root console, so each root character has one. Rotating the root
  console moves the pointer at the commit, as it moves `handoff_ctx`. A root console cannot be forked, moved
  under a senpai, or archived: each would leave the root character performing a context other than its root
  console, or with none.
- **Live** means not archived. A concluded context is live; verbs that refuse a concluded target say so.
- **The kohai** of a context are the live contexts whose `senpai_ctx` is that context. The senpai chain never
  has a cycle. Setting an edge that would close one is refused.
- **The handoff log** belongs to the character: `characters.handoff_ctx`, as today. Every context of the
  character writes and reads the same log. A rotation reads the predecessor's handoff as it does today. For a
  fresh log, `kj context rotate` of the handoff context points `characters.handoff_ctx` at its successor.

What stays as it is:

- `context_type`: what a context does (its rc bundle and gate tier). The `director` type keeps its name until
  step 6.
- `forked_from` and fork kind: provenance only. `forked_from` is the only parent link (step 4).

## Where a model comes from

Every model call reads the context's own `context_slots` and nothing else. A model turn in context K runs on K's
slot for K's context type. A context with no context character runs no model.

- Writing a context character copies everything a call needs into `context_slots`: each character slot's
  backend, model and parameters, and for each parameter the slot leaves unset, the value in the model's
  `backend_models` row, context window included. After that no call reads `character_slots` or
  `backend_models`; they are catalogs read when a context character is written. Kaijutsu supplies no parameter
  value of its own.
- `max_tokens`, which every provider request carries, must come from the character slot, a create option or the
  model's row. A slot with none refuses when the context character is written, naming the character, the role
  and the model. A `gate` or `judge` slot names a council server and the model it serves, and needs no
  `max_tokens`. A `judge` or `gate` slot whose backend is not a council server refuses when it is written.
  `context_window` comes from the slot's model row in `backend_models`, or is unknown. An unset optional parameter
  (temperature, top_p, effort, thinking) is left out of the request. A context window none holds shows as
  unknown.
- A context with no slot for its type has no model. `kj kohai create` and `kj context rotate` say so in their
  output ("<context> has no model for <type>: <character> has no <type> slot"), and `kj context info` shows the
  same line. A model turn in K refuses before any provider work, naming the character, the type and the fix
  (`kj character slot set`, then `kj context rotate`).
- The judge shadow runs on its own `judge` slot and the gate on K's `gate` slot. The two are independent: a
  context with no `gate` slot has no council, and its gate decides without one; a context with a `gate` slot and
  no `judge` slot runs its council with no judge.
- A judge shadow belongs to one context. The gate makes it as a fork of that context, so it copies the context
  character and takes the context as its senpai. `kj senpai set` refuses to move a judge shadow. The coder's
  archive rc archives it when a rotation archives the predecessor, and the successor's first gate call makes a
  new one.
- A fork's distillation runs on the forking context's `distill` slot, and `kj drift pull|merge` on the caller's
  `distill` slot, so a cheap model can summarize for an
  expensive conversation. With no `distill` slot, a fork that asks for distillation refuses, naming the
  character and the fix. A one-shot prompt takes `max_tokens` from the slot it runs on.
- A backend's connection facts (`base_url`, keys, timeouts, `max_concurrent`) are read live. They are
  infrastructure, not a model choice.
- To move a context to another model or persona, change the character's slots and rotate it. To run two models
  side by side, create a context with a create option, or play a second character.
- Slots are frozen after create. No verb edits a running context's slots; one can be added later, since each
  context's copy is its own.

## Routing

Reviewer of an ask raised in context K by actor A:

1. K with no context character refuses, naming K.
2. Let S be K's `senpai_ctx`. If S is null (K is a root console): when A is K's live root character, the ask is a
   self-confirmation; otherwise refuse, naming A and K.
3. The reviewer is S's character when that character is not retired, is not A, and is not K's performer. S may
   be archived; asks route to a character, so an archived senpai's character still reviews.
4. When S's character is A or K's performer, climb: K := S, and repeat from 2. A fork's performer is its
   forking context's character, so an ask in a fork, by anyone, climbs past that character.
5. A retired character refuses by name. It never substitutes another character.

There is one resolver.

- **Escalation** runs the same climb from the ask's context, skipping senpai until one is held by a character
  other than the current reviewer, and refuses at a context with no senpai, naming it. `kj ledger escalate --to`
  and the lineage root's take-over stay as step 2 built them.
- **Lineage root**: the character at the top of the senpai chain. It must be a live root character, or refuse
  with the reason.

## Authority

One rule: **the character of a context's senpai, or its lineage root**, may:

- move it to another senpai (`kj senpai set`);
- escalate its asks;
- rotate it (its own character may also rotate it, as today);
- archive it;
- change its egress (`kj context set --egress-allow|--egress-deny`).

**Placing under a senpai.** `kj kohai create --senpai <ctx>` and `kj senpai set` share one rule over the new
senpai: the actor plays it, holds its senpai, or is its lineage root. A new edge refuses an archived senpai. A
copied edge (fork, rotate) keeps the senpai it copies, archived or not.

**A handoff log** has no context character. Its owner and any live root character may rotate it.

**Character slots** are written with `kj character slot set|unset`, which needs the `ConfigWrite` capability, as
`kj character set` does today.

A verb nested in an rc lifecycle uses a narrower rule in place of this one, with the context whose lifecycle runs
as the actor: it may act on that context's kohai, so the coder's archive rc archives its own judge shadows
(`coder/archive/S10-shadows.kai`). In a rotate lifecycle it may also act on the predecessor's kohai (see "Rotation
hands off its kohai"). The context's character lends the lifecycle nothing. A selection such as `--children`
that names a context the rule does not cover refuses, naming it.

The refusal names the rule: "only <context>'s senpai (<character>) or its lineage root (<root>) may <act>". A
context's character never changes, so there is no recast act; to put another character on the work, create a
context for it.

Unchanged: a root character never performs a context other than its root console; the performer can never review
its own work; a model turn refuses to start when its performer would review itself.

## Creating, forking, rotating

- `kj kohai create <label> --as <character> --type <t> [--senpai <ctx>] [--parent <ctx>] [--cwd <dir>]
  [--env K=V]... [--model <backend>/<model>] [--max-tokens N] [--effort E]` makes a context and its context
  character. `--as` is required. The senpai is the caller's context, or `--senpai <ctx>` under "Placing under a
  senpai". `--parent` sets `forked_from` only. The slots
  are copied from the character. `--model`, `--max-tokens` and `--effort` replace the slot for `<t>` in the copy
  and are the only create options; unset parameters of a `--model` override are filled from that model's
  `backend_models` row. It needs the Operator capability and a live character caller.
- `kj fork`: the child gets a copy of the forking context's context character, slots included, with the forking
  context as its senpai. Asks from a fork climb past its performer to the forking context's senpai (Routing,
  rule 4). A root console cannot be forked.
- `kj context rotate <ctx>`: a new context for the same character, with slots copied fresh from the character.
  It copies, as today, the type, env, cwd and shell, and the predecessor's senpai; its `forked_from` is the
  predecessor's parent, so the successor is a sibling. The label and ring position move at the commit. The
  kernel moves no kohai (see "Rotation hands off its kohai").
- `kj context rotate` of a character's handoff log points `characters.handoff_ctx` at the successor. A handoff
  log has no context character, so rotate skips the loadout check for it; any other context with no context
  character refuses rotation. `kj handoff note` refuses an archived log.
- `kj context archive` archives the context it names, and with `--children` the selection it names. The kernel
  moves no kohai: their asks still reach the archived senpai's character, and reports to it refuse until they
  are moved (`kj senpai set`) or the senpai is promoted. Unresolved asks raised *in* the archived context are
  swept with a note, as today; asks raised in its kohai are not touched. Contexts that should go with it go by
  their type's archive rc, as the coder's judge shadows do today (`coder/archive/S10-shadows.kai`).
- `kj senpai set <ctx> <senpai-ctx>`: moves a kohai. It needs the authority rule over the moved context and
  "Placing under a senpai" over the new one. It refuses a cycle, a root console, and a judge shadow. It refuses
  while a pending ask would change reviewer: it re-resolves every pending ask raised in the moved context and in
  the contexts below it whose climb passes through it, and refuses, naming the asks, when any reviewer would
  differ.
- `kj senpai show [<ctx>]`, `kj kohai list [<ctx>]`, `kj senpai report "<text>"` default to the caller's context.
  `kj kohai list` shows kohai whose senpai is archived, so they are easy to find.
- `kj ledger list --reviewer <character>` lists only the asks whose reviewer is that character.
- `kj context info` shows the context's character, its senpai, and the model for its type (or why it has none).
  `kj character show` lists the character's live contexts.

## Rotation hands off its kohai

The kernel does not move kohai on rotation. The rotating character knows its kohai, and the handoff carries them:

1. Rotate creates the successor and runs its create rc, runs the predecessor's archive rc, and commits: the
   predecessor is archived and the successor takes its label, ring position, `handoff_ctx` and `root_ctx`
   pointers, in one transaction. The kohai still name the predecessor.
2. After the commit, rotate runs the successor's **rotate** rc (`/config/rc/<type>/rotate/`), with
   `ROTATED_FROM` naming the predecessor. A director's rotate rc takes the kohai with
   `kj senpai set <kohai> <successor>` and then lists the asks waiting for its character with
   `kj ledger list --reviewer <character>`, so the asks raised during the rotation are in front of it. The
   character may also take its kohai by hand.
3. Until the kohai are taken, their asks reach the same character (the predecessor's) and wait in the ledger,
   and their reports refuse, naming the archived senpai.

A rotation that fails before the commit leaves the predecessor live and labeled, with its kohai, and the
successor unlabeled; the error names the successor, and rotate archives it before returning. A rotate rc that
fails after the commit undoes nothing: the rotation stands, the error names the kohai not yet taken, and
`kj kohai list` of the predecessor shows them.

## Reports and drift

- `kj senpai report "<text>"` puts the text in the senpai context as a drift block.
- `kj drift push` along the edge, one hop from a context to its senpai or to one of its kohai, runs with no ask,
  with one exception: a push up from a fork whose `created_by` is not its character keeps today's gate. That
  proxy treats a fork a person made, and a judge shadow (made by `system`), as not the context's own; a person
  connected with the fork's character's key reads as the character. A context forks itself the way Claude Code
  forks a subagent, and its pushes up run with no ask. A push two or more hops away keeps today's gate. A
  `gate.toml` key on `kj drift push` still wins over the edge.

## What a context sees

- The `<situation>` block names the context's character and its senpai: `<character name="<name>"/>` and
  `<senpai id="<short id>" context="<label>" character="<name>"/>`. As step 3 built it, `character` is named
  even when retired, `context` is left out when the senpai has no label, and a root console has no `<senpai>`.
- The rc persona text (director S00-stance) says "our senpai" and reads the name from the kernel fact.

## Removed (hard cut)

Steps 1-3 (context edge):

- Columns `contexts.reviewer_id` and `contexts.director_id`; tables `approval_reviewer_delegations` and its events.
- `kj context set --reviewer`, `--clear-reviewer` and `--director`; `kj ledger delegation grant|revoke|list`;
  `kj context create` (now `kj kohai create`).
- The `forked_from` review walk, `responsible_character_above`, and the duplicate resolver.

Step 5 (context characters):

- Columns `contexts.senpai_ctx` (moves to `context_characters`), `contexts.played_by`, `contexts.cast_id`,
  `contexts.provider`, `contexts.model`, and the character's cast column (`default_cast_id` on main, `cast_id`
  after the first build); `characters.root_ctx` stays, as the root console's pointer;
  `presets.cast_id` and `presets.system_prompt`; tables `casts`, `cast_slots` and `llm_defaults`; and the
  unreleased `turns.cast_label` and `turns.model_source`, which never reached a deployed database. The
  `senpai_migration_report` table stays: `kj senpai show` prints a context's notes until its senpai is set again.
- `kj cast` (every subcommand; `kj character slot set|unset|list` replaces `kj cast slot`) and
  `kj character set --cast`.
- `kj context set --as|--model|--cast` and `kj context unset --model|--cast`; `kj kohai create --cast`;
  `kj fork --model`; `kj model` (`kj context info` shows the model); `kj backend default`;
  `kj preset save --cast|--system-prompt`. `kj models` (discovery) stays.
- RPC `configureLlm`, `setDefaultProvider`, `setDefaultModel` and `getLlmConfig`; the client's
  `set_context_model`. The wire's `ContextHandleInfo.provider` and `.model` carry the context's slot for its
  type; `.castLabel` goes.
- The MCP `session.start` hook's model write; the live `GET /v1/models` lookup for a context window (a window that
  `backend_models` does not hold shows as unknown); the DriftRouter handle's provider/model copy and the turn's
  explicit `model` argument that outranks it, with the wire `prompt` method's `model` parameter
  (`kaijutsu-client` `prompt(content, model, ctx)`).
- `kj fork --distill-model`; `[council] server` and `[council] model` in `gate.toml`; every read of
  `backend_models` or `character_slots` that chooses a model outside writing a context character.
  `kj character slot set` reads the model row only to warn about the slot it writes.
- Every value kaijutsu supplies on its own: the 64000 `max_tokens` floor (`LlmRegistry::max_output_tokens`),
  `BuildOpts::new`'s 64000 (`BuildOpts` takes `max_tokens` from its caller), the openai-compatible client's 8192
  for a zero, and the one-shot prompts' 4096.
- Every resolver below the context's slots: `model_resolution.rs`'s override, registry-default rungs and
  `ModelSource`; `effective_cast` and its fall back to the performer's cast; the override's borrow of the
  current default backend; `LlmRegistry::prompt` and its first-advertised model; the tunable floor
  (`default_tunables`, `SlotTunables::over` onto it); `resolve_model_choice`'s bare name onto the default
  backend, and distillation's registry-default fall back.
- From the first build of step 5: `character_instances`, `instance_bindings`, `instance_slots`,
  `instance_migration_report`, `contexts.instance_id` and `.binding_seq`, `turns.instance_id` and
  `.binding_seq`, and the derivations of an instance's conversation, its activity and the root console.
- `cast_slots.loadout` and `.extra` are not carried into `character_slots`: they had no reader.

Presets keep only their fork filters (`kj fork --preset full|window|spawn`).

## Migration

The migration runs once, in one transaction, after step 1's. It records every value it drops and every context
it cannot map in `context_character_migration_report` (kinds: `dropped`, `unmapped`, `no_slot`, `no_gate_slot`,
`no_distill_slot`, `no_character_slot`), which `kj senpai show` reads. It never invents a value: each row it
writes records an answer the old code would have given. The one exception is a judge shadow's character, which
the old row did not have; the new model requires it, and it is the character of the context the shadow judges.

1. **Character slots.** Each character gets a copy of its cast's slots. A character with no cast gets none; the
   migration lists it with the roles its live contexts use (`no_character_slot`), because a rotation copies
   fresh from the character and would lose the model. We add those slots before deploy. A cast no character
   names is dropped and logged with its slots.
2. **Context characters.** Each context with a performer gets a context character of its performer, with its
   `senpai_ctx`. A judge shadow (type `judge` with no performer, found by `forked_from` with archived rows
   included) gets the character of the context it judges, with that context as its senpai. A context with no
   performer that is not a judge shadow gets none, whether or not it has a `senpai_ctx`, and is listed when it
   has one. No character's `handoff_ctx` gets one.
3. **Edges.** `senpai_ctx` is copied as step 1 left it. A self-edge or a cycle among live contexts refuses the
   migration, naming the contexts; one among archived contexts is broken at the archived context, which is
   listed. Each root character's `root_ctx` stays; a live context with no senpai that is not its character's
   `root_ctx` refuses the migration, naming it.
4. **Slots.** Each context's slots are a copy of the effective cast its next turn, its judge and its gate would
   have used
   under the old resolver (override, context cast, default cast and `llm_defaults` included), resolved from the
   rows as they stand at migration, so a migrated context holds that cast's roles, not every character slot.
   `context_window` is the model row's value, or unknown, since the live lookup is gone. `max_tokens` is what the
   old code sent: the slot's, the model row's,
   `llm_defaults`', or 64000 when all are unset, written into the slot. A context gets a `gate` or `distill` slot
   only from the cast's own slot. The migration cannot read `gate.toml`, so it lists each live context with no
   `gate` slot and each with no `distill` slot; we add them with `kj character slot set` and rotate before
   deploy. A context the old resolver could not resolve gets no slot for its type, and the migration lists it.
5. **Labels and handoff logs** stay where they are.
6. Archived contexts map like live ones; an archived context that cannot be mapped is left with no context
   character and listed. Only live contexts refuse.

## Out of scope

- A verify tier for reviewers (read-only builds and tests) is a gate tier change, done separately.
- `@<name>` drift addressing (documented in docs/character.md, not verified as implemented).
- A verb that edits a running context's slots.

## Tests (each falsified once)

- An ask in a kohai goes to its senpai's character; a fork's ask climbs past its performer, including an ask a
  person raises in the fork; a root console
  self-confirms; a top context held by a non-root refuses; a retired character refuses by name; a context with no
  context character refuses; an archived senpai's character still reviews.
- Escalation climbs one step and refuses at the top.
- The authority rule allows the senpai's character and the lineage root, and refuses a sibling and the kohai
  itself, for each act it governs; `kj senpai set` also checks the new senpai.
- `kj senpai set` refuses a cycle, an archived senpai, a root console, a judge shadow, and a pending ask in the
  moved context or in a fork below it whose reviewer would change; it allows pending asks when every reviewer
  stays the same. `kj kohai create --senpai` and `kj senpai set` use the same rule over the new senpai.
- A root console cannot be forked or archived; rotating it moves `root_ctx` at the commit; a live context with
  no senpai that is not a `root_ctx` cannot exist.
- Archiving a context archives no other context and moves no kohai; a report to an archived senpai refuses;
  `kj kohai list` shows kohai of an archived senpai.
- Create copies every character slot with unset parameters from `backend_models`; a create option replaces the
  slot for the context's type; a later edit to the character's slots or to `backend_models` moves no context; a
  trigger refuses a change to a context's slots or character; a turn sends no unset optional parameter.
- A fork copies the forking context's slots and takes it as senpai; a rotation copies fresh slots and keeps the
  predecessor's senpai.
- Rotation moves no kohai; the successor takes them with `kj senpai set`; a director's rotate rc takes them
  after the commit and lists the asks waiting for its character; an ask raised during the rotation waits in the
  ledger and is in that list; a rotate rc that fails after the commit leaves the rotation standing and names the
  kohai not taken; a rotation that fails before commit leaves the predecessor live, labeled and with its kohai, and the
  successor archived.
- A context with no slot for its type: create and rotate say so; a turn refuses and names the character, the
  type and the fix. A slot with no `max_tokens` anywhere refuses at create.
- A fork with distillation runs on the `distill` slot, and refuses with no `distill` slot; a context with no
  `gate` slot runs its gate with no council; a `gate` slot with no `judge` slot runs the council with no judge;
  a `judge` or `gate` slot on a backend that is not a council server refuses at write; a `[council] server` key in `gate.toml` is refused as unknown.
- The judge shadow and the gate use the `judge` and `gate` slots.
- A drift along the edge runs with no ask, both ways; up from a fork a person made asks; two hops away asks; a drift to a sibling
  still asks; a `gate.toml` ask key wins.
- Two contexts of one character read and write the same handoff log; rotating the handoff context repoints
  `characters.handoff_ctx`.
- The prompt's `<situation>` names the character and the senpai.
- The migration: a character gets its default cast's slots; a context keeps its resolved model, override and
  `llm_defaults` cases included; a judge shadow gets the character of the context it judges; an unresolvable
  context is listed; a cycle refuses; dropped values are logged.
- The removed verbs and RPCs are gone (`kj model`, `kj cast` and `kj context set --as` are unknown).

## Open (step 5)

Proposed by the lead after the kaibo review of this redesign (deepseek, 2026-10-20); each needs Amy's word:

- **A `rotate` rc lifecycle verb** (`/config/rc/<type>/rotate/`), run on the successor after the commit. The
  create rc runs before the commit, so it cannot be where a successor adopts its kohai. A new lifecycle verb
  touches rc orchestration (`docs/kaish-integration.md`).
- **`characters.root_ctx` comes back** as a pointer like `handoff_ctx`, moved by the rotation's commit.
  Deriving the root console from "the live context with no senpai" gives two answers while a root console
  rotates.
- **Root consoles cannot be forked, moved or archived.** Each would leave a root character performing another
  context, or with none.
- **Drift with no ask is one hop.** The first build allowed the whole `forked_from` chain within an instance.

The sections below record what each step built. Steps 1-3 built the context-edge version, and the first build
of step 5 built character instances that spanned contexts. Where they conflict with the sections above, the
sections above win.

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

## Decided while building step 5 (lead, within Amy's decisions)

- **Edge directions.** A push from any context of an instance to the other instance's conversation runs with no
  ask, up to the senpai or down to a kohai. A push to a context of the other instance that is not its
  conversation keeps the gate. Within one instance, "down" covers forks of forks (the `forked_from` chain).
- **Distillation** for `kj drift pull|merge` runs on the caller's instance's `distill` slot, as a fork's does.
- **A judge shadow's performer** reads as its instance's character, because the shadow joins that instance.
  Retiring the character archives its shadows with its other contexts.
- **Rotating a handoff log** is allowed for the character that owns it and for any live root character.
- **`kj senpai set`** checks for pending asks in every context of the instance, not only the one named.
- **A backend named by a binding cannot be deleted**: bindings are append-only and keep their backend.
- **`getLlmConfig`** went with the other model RPCs, and the ACP client's `cast.toml` preset went with the
  client's model choice. solo-acp writes its own `solo` cast and requires `--max-tokens` when the model's row has
  no output budget.
- **Creating a context from a root's session.** A root character performs only its root console, so a root's
  `kaijutsu-tui` or `kaijutsu-mcp` session names the character that plays what it creates: `--character <name>`
  on both, and `KAIJUTSU_CHARACTER` for MCP (the flag wins). The name goes to `kj kohai create --as`; there is
  no new wire method. Without it the session's own character plays, and a root refuses with an error that
  names the option. Both clients ask `ActorHandle::performer_for_new_contexts`, so the refusal reads the same.


## Open (step 1)

Choices made conservatively where the design was silent. None is a new rule; each refuses or maps exactly.

- **Create with no calling context** refuses. The design names the caller's context as the senpai; a caller with none (a bare `--parent` from a client that has not run from a context) has nothing to answer to, and the kernel does not pick one.
- **Cold-start discovery** (`bootstrap_discovered_context` in the server) mints contexts with no senpai, like the other parentless system contexts. The design's list of null-senpai system contexts does not name it. A non-root actor in one refuses at the top.
- **Judge shadows** (a fork child of the context they judge) take that context as senpai, per the fork rule.
- **Migration, performer-less contexts**: with no performer there is no actor, so the old resolver has no single answer. The migration maps them to the nearest strict ancestor that has a holder, which is what a create or fork from that ancestor would have set.
- **Migration, retired holder on the old walk**: mapped to the ancestor whose holder is retired. The old resolver refused by name there; the new one refuses by name at the same context, so behaviour is preserved and startup is not blocked. An override naming a retired character has no such context and refuses the migration.
- **Migration, archived contexts** map like live ones (a live context's chain passes through them). An archived context that cannot be mapped is left a top and reported instead of refusing; only live ones refuse.
- **Migration, exhausted old walk** (nobody above, no override): the context stays a top, which refuses or self-confirms at ask time as the old resolver did.
- **`kj context set --reviewer/--director` and `kj ledger delegation`** still exist in this step but no longer route anything; step 2 removes them.
- **Escalation** climbs past the current reviewer by holder (not "the context that yielded it"); the two agree unless one character holds several consecutive senpai contexts.
- **A context with no performer** has no reviewer to report (`resolve_context_review` errors "no performer"), as before.

- **Escalation** climbs past the context the current reviewer holds, as designed. An ask whose reviewer was set by hand (`escalate --to`) to a character holding no senpai on the chain has no context to climb from; it refuses and asks for an explicit `--to`.
- **Dead guard**: `insert_new_context_rows` still checks that a performer is not its own reviewer. Under the senpai climb that cannot happen (the climb passes the actor), so the check is unreachable; step 2 may remove it.
- **Test fixtures**: contexts registered under a parent (`register_context`, council `live_context_with`) now take the parent as senpai, mirroring create and fork. `KernelDb::assign_test_review` replaces the reviewer override in fixtures: the reviewer holds a context, and the context is placed under it. A held senpai is required, so fixtures that created a context from a context nobody performs now give that context a performer.
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
7. Fix container.md: "senpai" is the supervising session outside the container; "banto" is the director character.
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
  0`) is not checked again. The shipped coder archive bucket archives the coder's judge shadows, whose senpai the
  coder context holds, not whoever archived the coder; the verb that started the lifecycle was checked.
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
  context lacks operator for `kj context rotate`.

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
- **A nested archive in an rc lifecycle** (step 2 follow-up 1) passes when the context whose lifecycle runs is
  the target's senpai, and only then. A lifecycle has no more power than its context: neither that context's
  holder nor whoever started the lifecycle lends it anything (Amy, 2026-10-19). The shipped coder archive
  bucket needs no more, since the coder context is its judge shadows' senpai.
- **The council bench contexts** (`contrib/bench/rc-variants/council/coder/create/S04..S06`, step 2 follow-up 2)
  are made with `kj kohai create` inside the first coder's create lifecycle, so they answer to that coder context.
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
