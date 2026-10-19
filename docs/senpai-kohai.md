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
