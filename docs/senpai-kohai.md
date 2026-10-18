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
