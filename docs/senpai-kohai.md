# Senpai and kohai

Every context that does work answers to one other context: its **senpai**. It is that context's **kohai**.
The senpai edge is the only thing that routes a context's asks, its reports and its escalations. It replaces
the reviewer override, the director column, delegation grants and the review walk over `forked_from`.

Decided by Amy on 2026-10-18 (interview with senpai, the supervising Claude session of the nagaya run):

- The edge points at the senpai's **context**, not at a character.
- Delegation grants, the reviewer override and the `forked_from` review walk are **replaced**, not kept beside it.
- The kj surface is a **hard cut**: the old routing verbs and flags go away in the same change.
- It is built in a worktree and lands as one branch.

Why: on 2026-10-18 the nagaya run needed four mechanisms to say "this banto answers to senpai". A banto's asks
went to Amy, so it could not even drift its supervisor. A delegation grant was refused ("only a live root
character may grant"). A reviewer change was refused ("only the lineage root may change approval routing"). The
working fix was to rotate the banto into a context that senpai created, and to teach a one-off `--family` ledger
rule so its drifts would not ask. The survey of the code found the same relationship stored in `reviewer_id`,
`director_id`, `approval_reviewer_delegations` and the `forked_from` walk, resolved by two near-identical
functions (`KernelDb::effective_approval_reviewer` and `Kernel::resolve_review_assignment`).

## The model

- `contexts.senpai_ctx`: the id of the senpai context. It is null only for a root context (a root character's
  console) and for the system contexts that have no parent today (scratch, the handoff log, the drift queue,
  lost+found, the beat score).
- The **holder** of a context is the character that answers for it: its performer (`played_by`); for a root
  context, the root character that owns it (`characters.root_ctx`). A context with neither has no holder, and an
  edge that points at it is refused.
- The **kohai** of a context are the live contexts whose `senpai_ctx` is that context.
- The senpai chain never has a cycle. Setting an edge that would close one is refused.

What stays as it is:

- `character`: who performs a context (persona, keys, default cast).
- `cast`: the model team.
- `context_type`: what a context does (its rc bundle and gate tier). The `director` type keeps its name.
- `forked_from` and fork kind: provenance only. They no longer route anything. (`context_edges` 'structural'
  rows duplicate `forked_from`; removing that duplication is out of scope here.)

## Routing

Reviewer of an ask raised in context K by actor A:

1. Let S be `senpai_ctx(K)`. If S is null: when A is a live root character, the ask is a self-confirmation (as
   today); otherwise refuse, naming A and K.
2. The reviewer is `holder(S)` when it is live and is not A.
3. When `holder(S)` is A, climb: K := S, and repeat from 1. (A banto that forks itself answers to the banto's
   own senpai.)
4. A retired holder refuses by name. It never substitutes another character.

There is one resolver. The two existing functions are replaced by it.

- **Escalation** runs the same climb, past the context that yielded the current reviewer, and refuses at a
  context with no senpai, naming it.
- **Lineage root**: the holder at the top of the senpai chain. It must be a live root character, or refuse with
  the reason.

## Authority

One rule: **the holder of a context's senpai, or its lineage root**, may:

- cast or recast its performer (`kj context set <ctx> --as <character>`);
- move it to another senpai (`kj senpai set`);
- escalate its asks;
- rotate it (its own performer may also rotate it, as today);
- archive it.

The six rules this replaces each had their own error text. The new refusal names the rule: "only <ctx>'s senpai
(<holder>) or its lineage root (<root>) may <act>".

Unchanged: a root character can never be cast; the performer can never review its own work; a model turn
refuses to start when its performer would review itself; an assignment change refuses while the context has
pending asks.

## Creating and moving

- `kj kohai create <label> --type <t> [--as <c>] [--cast <x>] [--cwd <dir>] [--env K=V]... [--parent <ctx>]
  [--senpai <ctx>]` replaces `kj context create`. The new context's senpai is the caller's context, or
  `--senpai <ctx>` when the caller has authority over `<ctx>` (it holds it, or it is that context's lineage
  root). `--parent` sets provenance only. Creating needs the Operator capability and a live character caller,
  as `create --as` does today.
- `kj fork`: the fork's senpai is the forking context.
- `kj context rotate`: the successor keeps the predecessor's senpai, and the kohai of the predecessor move to
  the successor.
- `kj senpai set <ctx> <senpai-ctx>`: moves a kohai. It needs the authority rule above, refuses with pending
  asks, refuses a cycle, and refuses a senpai context with no live holder.
- `kj senpai show [<ctx>]`: the senpai context (label and id), its holder, the lineage root, and how the
  reviewer of an ask by the performer would resolve.
- `kj kohai list [<ctx>]`: the live kohai of a context, with type and performer.

## Reports and drift

- `kj senpai report "<text>"` puts the text in the senpai's log as a drift block from this context.
- `kj drift push` along the edge, from a kohai to its senpai or from a senpai to its own kohai, runs with no
  ask. That replaces today's per-context `--family` standing rule. Any other drift target keeps today's gate.

## What a seat sees

- The `<situation>` block names the senpai: `<senpai context="<label>" id="<id>" character="<name>"/>`, beside the
  performer. It replaces `<reviewer>`; the reviewer of the seat's own asks is the senpai's holder, or the climb
  above, and `kj senpai show` prints it.
- The rc persona text (director S00-stance) says "our senpai" and reads the name from the kernel fact. The
  contrib S12-environment files stop hardcoding "senpai is a Claude session outside the container".

## Removed (hard cut)

- Columns `contexts.reviewer_id` and `contexts.director_id`.
- Tables `approval_reviewer_delegations` and its events table.
- `kj context set --reviewer`, `--clear-reviewer` and `--director`; `kj ledger delegation grant|revoke|list`;
  `kj context create` (now `kj kohai create`).
- The `forked_from` review walk, `responsible_character_above`, and the duplicate resolver.
- gate.toml keys that name `kj context create` become `kj kohai create`. rc scripts, contrib (bootstrap.sh, the
  agent-dev rc), docs and tests move in the same branch. `docs/approval-identity.md` and `docs/character.md`
  are rewritten to this model.

## Migration

A schema migration sets `senpai_ctx` for every live context from what routes it today, then drops the old
columns and tables:

- The senpai is the nearest context at or above it on `forked_from` whose holder is the context's effective
  reviewer today (the old resolver, run once).
- When no such ancestor exists (an override or delegation named a character outside the lineage), the senpai is
  the reviewer character's root context if it is a root, or else that character's most recent live context.
- When neither exists, the migration refuses to run and names each such context. It never guesses.
- Every context whose mapping was not the nearest-ancestor case is listed in a migration report table, and
  `kj senpai show` prints a note for it until its senpai is set again.

## Out of scope

- A verify tier for reviewers (read-only builds and tests) is a gate tier change, done separately.
- Removing the 'structural' `context_edges` duplicate of `forked_from`.
- `@<name>` drift addressing (documented in docs/character.md, not verified as implemented).

## Tests (each falsified once)

- An ask in a kohai goes to the senpai's holder; a self-held senpai climbs; a root console self-confirms; a
  top context held by a non-root refuses; a retired holder refuses by name.
- Escalation climbs one step and refuses at the top.
- The authority rule allows the senpai's holder and the lineage root, and refuses a sibling and the kohai
  itself, for each act it governs.
- `kj senpai set` refuses a cycle, pending asks, and a senpai with no live holder.
- Fork and rotate set the senpai as above; rotate moves the predecessor's kohai.
- A drift along the edge runs with no ask, both ways; a drift to a sibling still asks.
- The prompt's `<situation>` names the senpai.
- The migration maps nearest-ancestor cases exactly, reports the others, and refuses an unmappable context.

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
5. **Model provenance per turn; no cast on the context.** Today a turn records nothing durable about its model:
   `context_usage` keeps only the last request per context. First, each turn writes a durable record: turn id,
   context, performer, cast, role (context type), provider, model, and the token counts, which also gives spend
   totals. Then `contexts.cast_id` and the per-context provider/model override go. A context's model is its
   performer's cast slot for the context's type. To run another model, cast a character whose cast has it.
6. **Persona on the character.** The character sheet carries the persona text; the context type says what the
   seat does. The `director` type, which predates characters, is reworked or replaced, and its persona file
   (S00-stance) moves to the character that plays the seat.
7. **The gate's own cast**, as in kaibo's config: the reserved `gate` role leaves `cast_slots`. In parallel with
   4-6 where it does not collide; otherwise after them.

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
  runs, is the target's senpai, or when the seat's holder passes the authority rule. The first case covers a
  seat nobody performs (`archiving_a_coder_archives_its_judge_shadow`). Whoever started the lifecycle lends it
  nothing.
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
