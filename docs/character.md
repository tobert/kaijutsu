# Character — the persistent someone a name resolves to

> Design + rollout plan, 2026-09-05, revised 2026-09-06. Drawn from a
> morning's conversation with Amy about her restart-every-session routine
> and what kaijutsu needs so the morning is smooth, then revised through two
> model reviews the same afternoon ("Review", below) and a readiness pass
> the next day. **Slices 0a–4 are built; slices 5–8 remain planned.**
> Read "Current implementation" first; historical line references need rechecking.
> Amy's statements are guidance, not rulings.

## Current implementation

Read this section for current behavior; the design and original inventory below
also describe work that has not shipped. `AGENTS.md` links here so the roadmap
does not become an instruction to use nonexistent features.

| Part | Implemented contract |
|---|---|
| Identity and sheet | `PrincipalId`, kernel-owned name, creation/retirement timestamps, optional `handoff_ctx`, a `root` flag, and `root_ctx`; `kj character create [--root]\|list\|show\|set --root\|--no-root\|retire` |
| Bootstrap | `kaijutsu-server init --as <name> --key <pubkey-file>` creates the first root character and binds its key, with the service stopped. The server refuses to start without a live root character. There is no seeded character and no anonymous auth. See "Bootstrap: the person creates themself" |
| Root context | Each live root character has one: type `root` (a model-less admin bundle), labeled with the character's name, played by it, with no parent. `kj character create --root` and `set --root` create it; the server creates any missing one at start. `set --no-root` refuses while it is live |
| Credentials | `auth.db` binds fingerprints to principals; `add-key --as <character>` binds to an existing character |
| Performer | `kj context create --as <character>` needs the Operator capability and a live character caller, records `played_by` before create rc, rejects unknown, retired, root, or self-reviewing assignments, and preserves the requester's `created_by`. Without `--as`, this path leaves it unset. Fork copies it |
| Client creation | There is no create RPC. A client runs `kj context create` through `executeKj` from an existing context, which becomes the parent. A client with no context yet uses `--parent <label>`, or the kernel's only live root context, and refuses with several roots (`kaijutsu_client::choose_parent`; `kaijutsu-mcp`, `kaijutsu-tui`, and `kaijutsu-acp` all take `--parent`). Ordinary client contexts leave the performer unset. Creation records the acting caller as director when it has a character sheet, and leaves the director unset otherwise; it grants no approval authority. MCP session registration records the credential character as performer, except a root character, which cannot be cast |
| Review assignment | Explicit context override, then explicit director-wide delegation, then the walk up `forked_from`. There is no configured default. An exhausted walk is a self-confirmation for a live root character and an error for anyone else. The lineage root, the root character at the top of a context's `forked_from` chain, controls its reviewer and director overrides; any live root grants and revokes delegation. Fork records the forking actor as director and preserves the reviewer override. See `docs/approval-identity.md` |
| Model invocation | Resolve live, distinct performer/reviewer characters before starting the turn, and refuse a `root` performer. Provider output and tool calls carry the performer; the requester stays separate |
| Approval | Asks snapshot performer and reviewer. Only that reviewer may decide; the performer cannot approve from any context, except the self-confirmation whose reviewer IS its performer. See `docs/approval-identity.md` |
| Retirement | Concludes and archives live contexts linked by `played_by`; existing block authors stay unchanged. A retired character responsible for an ancestor context refuses reviewer resolution below it, by name |
| Handoff | Ordinary context referenced by `handoff_ctx`, created on the first note; `tail` never creates it. `note --for` keeps the caller as author |
| Rc | One context-type directory per lifecycle. Coder, mcp, and director include shared handoff injection; director names the performer from context metadata |
| Rotation | `kj context rotate [<context>]` creates a successor from the predecessor's parent that copies its configuration, env, and cwd, sets `ROTATED_FROM` to the predecessor's id, and runs `create` rc. Only a successor with a loadout takes over: one transaction archives the predecessor and moves its label, ring seat, and any `root_ctx` pointer. The performer or the lineage root may rotate. See `docs/prompts.md`, "Rotating a context" |
| Accountability | A relation between contexts: `forked_from`, set by fork and by `kj context create` from inside a context; the responsible character is the performer, or the director when unset. Reviewer resolution and `kj ledger escalate` both walk it (`KernelDb::responsible_character_above`; `docs/approval-identity.md`). The sheet carries a `root` flag ("Roots and rotation"); there is no `accountable_to` column |

Sources: `kernel_db.rs::CharacterRow`, `kernel_db.rs::effective_approval_reviewer`,
`kj/context.rs::context_create`,
`kj/character.rs`, `kj/handoff.rs`, `rc/mod.rs::load_scripts` in
`crates/kaijutsu-kernel/src/`; `crates/kaijutsu-server/src/rpc.rs::create_context_inner`;
`assets/defaults/rc/director/create/S00-stance.kai` and
`assets/defaults/rc/lib/create/S16-handoff.kai`.

Banto is a character using the `director` context type. It does not need a new
type to have its own identity and handoff. `KJ_CHARACTER` was a temporary rc
name/handoff selector; it never set `played_by`. New instructions read performer
metadata instead. Existing contexts that used the bridge remain unassigned:
set `--as banto` on the seat, or create a successor with `--as banto`, then
`kj context rotate` it. See `docs/prompts.md`, "Rotating a context".

Still planned: character rc composition (slice 5), roster grouping and character drift addressing (slices 6–7), and
scheduled janitor/proctor work (slice 8). The sheet still has no
`default_cast_id`, `rc_dir`, or `memory_root` fields. The handoff
is a context, not a transport track. Requester and performer remain separate;
setting `played_by` changes subsequent model invocation and output attribution;
it never changes credentials or rewrites existing asks and block authors.

The original inventory and gap analysis below describe the pre-implementation
state. Use this section and the rollout to distinguish them from current code.

## The problem, in one paragraph

Amy restarts every Claude Code session each morning and again whenever a
context's prompt cache has gone cold. Each restart is a cold hydrate, and
what makes it cheap is a handoff: the repo `signoff.md`, exomemory, and the
Claude Code memory index. Only the last two are injected by the kernel
(`S15-recall.kai` on `create`). The signoff, the one that matters most for
resuming, is a hand-rewritten file read by hand. And the name the routine
hangs on, `kaijutsu-lead`, has no home: it is a flag on `claude --name`, a
label on whichever context is live, and a principal on each act. The
question this document answers is **what the name resolves to**, and how to
thread that thing through a kernel that already has most of its parts.

## What a character is

A character is the collection of things that makes up **one continuous
lineage of interactions**. In kaijutsu that lineage is made of contexts, a
`context_type`, the models a cast resolves to, and the rc that customizes
them — and the character is the sheet that holds them together and points
outward at the rest.

Amy's word for the shape is a D&D character sheet (guidance, 2026-09-06):
a page of stats and pointers that is not itself the lore, but that
references out to deeper lore. Read the design that way. The sheet is
small and mostly foreign keys; what it names — the rc directory, the
handoff log, the root context, the memory root — is where the depth
lives. Adding a column is how the sheet reaches somewhere new.

Two things follow, and they are the ones to check a change against:

- **The sheet is pointers, not content.** If a field wants to hold prose,
  a policy, or a list, it wants to be a context or a directory the sheet
  points at instead.
- **The lineage is the point, not the row.** A character outlives any one
  context; contexts are performances of it. That is why it retires rather
  than being deleted, and why the handoff log is the part of the design
  that changes a morning.

## Decisions (guidance, Amy, 2026-09-05)

- **The connecting thing is a character.** Human or model. Amy is one.
  Opaque UUIDv7 id in records and `.data`; given name in the UI and as the
  drift address. The word fits the cast vocabulary kaijutsu already uses: a
  character is what an actor is cast *as*, and a context is one performance.
- **A character has a default cast.** A gig may pin another.
- **Accountability replaces any group concept.** Every model character
  answers to a character above it; a human is a root. No party. Superseded
  in shape by "Roots and rotation": the relation is between contexts, not
  between sheets.
- **rc is a union.** A context runs its `context_type` rc and its
  character's rc, merged by the `SXX-name` ordering rc already sorts on. A
  character script chooses what comes through per type in kaish code.
  `context_type` keeps its name; "chair" is not a term.
- **Presence is kept and derived; the roster inverts onto characters.** Many
  clients per character: Amy is connected 2–5 ways a day from several
  machines. Never associate one client with one character. "Seat" is not a
  term.
- **A context that wants a human raises an ask.** No availability field for
  now; an escalation-triage character will answer some asks by policy later.
- **The night shift is a janitor first.** Cleanup and archiving. The proctor
  sweep is its second job. Compaction watchers later.
- **Deferred, not dropped:** the saifu 財布 (a per-character purse with
  several currencies: tokens, dollars, megabytes, a house scrip) and an
  availability field.
- **Kaijutsu stays bespoke** for Amy and the fleet. Open source, so who knows.

## Roots and rotation (guidance, Amy, 2026-09-15)

- **Accountability is a relation between performances, not between
  sheets.** A context is accountable to the context it was forked or
  created from: `forked_from` on the context row, set by `kj fork` and by
  `kj context create` run from inside a context. The character responsible
  for a context is its performer, or its director when no performer is
  set. So a coder lane banto forks is accountable to that banto seat, not
  to "banto" in general, and two bantos never collide. There is no
  `accountable_to` on the character sheet; Amy: "accountable_to is a
  relation at runtime, so if banto forks a coder, that coder is accountable
  to the precise banto that forked it, not any banto."
- **A root context has no `forked_from`.** A context created over the wire
  with no parent, or `ROOT`, is a root of the forest. Its responsible
  character is a root character.
- **A root character has no model.** The sheet carries a `root` flag set at
  create; a root has no cast and turn identity refuses it as a performer. A
  root is a place with hands on it: Amy fools around there, and during
  bootstrap asks a Claude Code session to do something through the MCP
  bridge, where the model is outside the kernel and drives through `kj`.
  Autonomous operation restricts it further later.
- **`ROOT` is reserved and single.** One live holder per kernel, played by
  a root character, claimed the way the drift queue is claimed on the
  router. The reserved names live in one place: `ROOT`, the drift queue,
  and the factory preset labels.
- **Rotation replaces a context, not a character.** Amy, 2026-09-17:
  rotate a context, and move a character's `root_ctx` pointer only when it
  named that context. The successor is created from the predecessor's own
  parent, copies its whole configuration including env, sets
  `ROTATED_FROM`, and takes the label, ring seat, and pointer as the
  predecessor archives. The label follows the live holder. A root rotates
  too, when its history goes stale; a root confirms itself, so no one else
  is asked. The performer or the lineage root may rotate.
- **banto starts from `ROOT`.** Its home seat is created from the root
  context, so its lineage, and its accountability, begin there. The seat
  takes the character's name as its label. The old arrangement, where
  `ROOT` named banto's director seat, ends: `ROOT` is Amy's, banto's seat
  is `banto`.
- **The reviewer is the nearest responsible character that is not the
  actor.** Resolution takes the actor and the context: an explicit
  override on the context, then the director's delegation, then the walk
  up `forked_from` to the first context whose responsible character is
  live and is not the actor. There is no configured default. The walk
  starts at the context itself, which is how a context with no parent
  still resolves through its own director. Archived ancestors are walked
  through; what matters is who is responsible for them. When the walk
  ends without finding anyone else and the actor is a live root
  character, the actor is at its own root and the ask is a
  **self-confirmation**: the row is raised with the actor as its reviewer,
  the actor alone may answer it, and the ledger records the answer as a
  self-confirmation. A non-root actor with nobody above it has no
  reviewer, and the ask refuses. Every other ask has a reviewer who is not
  its actor, so an ask nobody can answer cannot exist. Escalation runs the
  same walk past the context that yielded the current reviewer, and
  refuses at a root. See `docs/approval-identity.md`.
- **A director casts its children.** `kj context set <ctx> --as <character>`
  is allowed when the caller is the context's resolved reviewer, or when
  the caller's actor directs the context, which a fork or create from the
  caller's own context records. Casting is what makes the cast character
  accountable to the caller there; no sheet relation is consulted. Roots
  cannot be cast, since they have no model.
- **A model rotates itself with `kj context rotate`.** Amy, 2026-09-17:
  the performer may rotate its own seat, so no ask is needed.
  `kj context create --as` follows `set --as` instead: Operator and a live
  character caller, who directs what it casts, with no ask.

### A session, inside kaijutsu

How a day like 2026-09-15 runs once the pieces above exist. Amy gives
prompts, banto directs lanes, and the lanes are contexts.

1. Amy sits in banto's seat, not `ROOT`. `ROOT` is where she runs `kj` by
   hand. She attaches the tui to `banto` and types; the draft is hers, submit
   authors a user block as amy, and the turn runs with banto as actor and amy
   as reviewer.
2. banto plans and forks lanes with filters, each played by a coder character
   forked from banto's seat, each with a label, a territory, and a worktree under
   `~/src/wt/`.
3. Lanes run as driven turns. banto drives and waits. A lane's gated
   statement walks the chain to banto; banto's own, such as a commit, walks to
   amy. Static allow tiers pass the routine ones; the rest reach Amy's asks
   pane.
4. Review is a tool call: kaibo mounts as an MCP server in banto's loadout,
   and the review is a tool-result block in the seat, with a fix lane forked
   from it.
5. Results come back as drift into banto's seat, distilled when casts differ.
   banto re-reads every citation against the tree, then commits path-scoped
   through the gated shell.
6. Every lane stays a context: open it in the picker, scroll, copy, or exclude
   a bad turn, fork, and re-drive. `kj context info` and `kj block list -c
   <lane>` answer what a lane did a week later.
7. `kj handoff note` replaces `signoff.md`. When the seat grows long, banto
   asks to rotate; the successor hydrates from `ROTATED_FROM` and the handoff
   tail, and the predecessor archives with its lineage intact.

Polish step 3 first: driving and waiting on lanes with disjoint territories
is where a session spends its care.
- **The principal is the character's key.** No distinct `CharacterId`.
  Permanence comes from the three rules in the design: kernel-owned name, no
  deletion while referenced, missing mapping is corruption. Revisit if a
  surface confuses performer with requester in review.
- **`auth.db` is a keyring; names melt into the character** (guidance, Amy,
  2026-09-06). *"Nicks were a quick idea"* — `auth.db` binds a fingerprint
  to a principal id and nothing else, `characters.name` becomes the only
  name in the system, `add-key` binds instead of minting, and bulk import
  is removed. `Principal` loses its name fields and `authenticate` returns
  a `PrincipalId`. Detail in "`auth.db` is a keyring".
- **The person creates themself before the first connection** (guidance,
  Amy, 2026-09-16, replacing the seeded `hajime` of 2026-09-06). `init`
  creates a root character with a minted id, not a well-known one:
  *"deterministic feels like a choice we'd regret."* Roots are equal: *"1
  will be typical, more than one just needs to be possible for now."*
  Adding keys stays host-only, and *"there should be no anonymous at
  all!"* Detail in "Bootstrap: the person creates themself".
- **Retire takes its contexts with it** (guidance, Amy, 2026-09-06). A
  retired character's live contexts are concluded and archived by the same
  act. There is no reassignment, no orphan performance, and no verb for
  moving a context to another character — simpler, and it means a turn can
  never start for a retired character because no unarchived context plays
  one.

The word "character" collides with the text unit. In prose, *character* is
the persistent someone; for text say code point, glyph, or `char`. The
collision is real in code (about 300 uses of the word as a text unit across
the crates), which is one reason the design below adds **no new Rust noun**
for identity.

## Original inventory — the parts

Read before designing; most of the character is already in the kernel under
other names.

| Part of a character | Exists today as | Where |
|---|---|---|
| id + given name | `Principal { id, username, display_name }` — the id half survives; the name half melts into the sheet ("`auth.db` is a keyring") | `kaijutsu-types/src/principal.rs:16`, `ids.rs:20` |
| handles, many per character | `credentials(fingerprint → principal_id)`; `principals.username UNIQUE`; `add-key --nick` chooses which principal a key joins (default: a new hash-named one) | server `auth_db.rs:37`, `:44`; default path `~/.local/share/kaijutsu/auth.db` (`:104`) |
| presence, derived | the roster: `RosterEntity::{Principal, Context}`, liveness `Bound`/`Recent`, self-reported `Availability {Active, Idle, Away, Dnd}`; four `roster_*` tables; `kj roster` | `kernel/src/roster.rs:103`, `:192`, `:250`; `kernel_db.rs:1189–1281`; `kj/roster.rs` |
| a role's rc bundle | `context_type` → `/config/rc/<type>/<verb>/`, loaded and sorted by `SXX-name` | `kaijutsu-types/src/paths.rs:144`; `rc/mod.rs` |
| who plays | casts: one slot per role, keyed `(cast_id, role)`; per-context `cast_id` and `provider`/`model` override; resolution ladder explicit override → cast slot on `context_type` → registry default | `kernel_db.rs:1094`, `:1118`; `contexts.cast_id`; `kj/context.rs:704–722` |
| a cadence that outlives contexts | a track: clock (`BeatPolicy`) + score context + attachments; `kj transport attach` creates the track stopped if absent; non-rotating attachments are first-class | `hyoushigi/mod.rs:45`, `:47`, `:125`; `kj/transport.rs:40–48`; `docs/tracks.md` §5 |
| a window over a long log | `kj context hydrate --window N`: `[0, marker] ∪ last-N`, persisted per context | `kj/context.rs:1498`; `kernel_db.rs:5443` |
| async notes reaching the next turn | the mailbox, a pull cursor over the block log | `llm/mailbox.rs:153` |
| addressed sends | `kj drift push <ctx>`, staged queue, flush, cancel | `kj/drift.rs:37–70` |
| idle detection | `contexts.last_activity_at` | `kernel_db.rs:612` |
| driving a session | `kj drive [ctx] --prompt`; archived contexts refuse | `kj/drive.rs:31–35`, `:265` |
| driving a Claude Code session | `kj cc send` over the socket protocol, with a liveness gate | `kj/cc.rs:55–59`; `docs/cc-peer.md` |
| a Claude Code session as a context | `register_session` makes a `cc-<repo>-…` bridge context | `kaijutsu-mcp/src/lib.rs:1821`; label test `main.rs:560` |

Two facts from that table drive the whole design:

1. **A principal is already most of a character.** It has the opaque id, the
   unique given name, a display name, and many credentials mapping into it.
   The schema allows Amy's keys from every machine to resolve to one
   principal; in practice `add-key` defaults the nick to a fingerprint hash,
   so on 2026-09-05 zorak's auth db held her as three (`amy`, `amy/moltar`,
   and a hash-named one for usagi). So the character is **a principal with
   a sheet**, not a new identity beside the principal. Consolidating her
   three is re-binding each key to one principal id — slice 2's
   `add-key --as`, since before the melt every way to do it mints.
2. **The kernel already has a roster that knows principals and contexts,
   presence, and self-reported availability.** Inverting it onto characters
   is grouping, not a new store.

## Original gap analysis

- **The sheet.** Nothing hangs off a principal but credentials. No
  accountable-to, no default cast, no pointers to rc, memory, handoff, root.
- **A model character's blocks are stamped `PrincipalId::system()`.** The
  turn path inserts every provider-emitted block, thinking, text and tool
  call alike, with the system principal (`kaijutsu-server/src/llm_stream.rs:1837`,
  `:1862`, `:1909`, `:1932`, `:1965`; 21 stamp sites in that file, not all of
  them provider output). The human's principal goes on the user's prompt block
  (`rpc.rs:4686`), which is right. There is no principal for kaijutsu-lead,
  so `system` is all there is to stamp. The consumer that shows this is the
  wire: a block's author on the wire *is* its principal id (`rpc.rs:10234`,
  `:10432`). Capabilities key on the caller's context (`kj/mod.rs:666`), the
  ledger stamps the caller who tripped the gate (`kj/gate.rs:372`), and the
  hydrator maps by role and kind (`llm/hydrate.rs:169`); none of them read a
  block's principal. One production path does, and it is the one that
  *wants* the change: the beat scheduler reads a model block's principal as
  `played_by` and records it as the attachment's producer so a cell failure
  routes back to the producing conversation (`kaijutsu-server/src/beat.rs:2213–2221`,
  matched in `producer_ctx_for` at `:1481–1494`). With every model block
  stamped `system` today, every producer records the same value and the
  match returns whichever attachment iterates first. Distinct character
  principals make it work as documented; the multi-producer path has no
  test that would have caught the collapse, so slice 3 adds one. Nothing in
  production compares a principal against `system()`; the only sentinel
  equality is against `beat()` (`beat.rs:2199`). Sequence lanes tolerate
  foreign principals by construction (`blocks/block_store.rs:353–362`).
- **A context does not know which character performs it.** `contexts` has
  `created_by`, `context_type`, `cast_id`, but no "played by".
- **The auth database does not honor the principal's documented permanence.**
  `Principal` says its id is permanent (`principal.rs:16–18`), but
  `set_username` renames (`auth_db.rs:224`) and `remove_principal` deletes,
  cascading credentials (`auth_db.rs:242`). A character that must retire
  and never be deleted cannot rest on that as it stands. The keyring melt
  removes both verbs along with the columns they mutate: there is no
  username to rename, and removal is a key's business, not an identity's.
- **rc reads one directory.** `load_scripts` takes `(context_type, verb)`
  and nothing else. rc scripts see `KJ_CONTEXT`, `KJ_VERB`, `KJ_CONTEXT_TYPE`
  (seeded 2026-09-05), `KJ_RC_DEPTH`, `KJ_PARENT_CONTEXT`, `KJ_FORK_INFO`,
  `KJ_PARENT_BLOCK_COUNT`, `KJ_DRIFT_INFO` (`rc/mod.rs`, `run_kai_script`),
  not a character.
- **Drift addresses contexts only.** Push resolves through
  `refs::resolve_context_arg` (`kj/refs.rs:80`) with a `DriftRouter`
  fallback (`kernel/src/drift.rs:527`) for archived contexts the router
  still holds; both grammars are one since `46878b28`. A character with no
  context has nowhere to receive a note.
- **The handoff is a file nothing reads.** Whole-file rewrite, one writer,
  no per-entry stamp or author, no window, melting by hand.
- ~~Distillation picks the source's cast silently.~~ Fixed 2026-09-05:
  `summarize_with_model_for_caller` (`kj/mod.rs`) refuses when the caller's
  and source's (provider, model) pairs differ and no `--distill-model` is
  named; `kj drift pull` and `merge` take the flag.

## The design

### Character = principal + sheet

No new identity type. `PrincipalId` stays on every block and on the wire.
The **sheet** is a new table in `kernel.db`, one row per principal that has
one, normalized (no JSON column; `feedback_sql_schema`). Slice 1 ships the
first four columns; the rest arrive with the slice that reads them.

```sql
CREATE TABLE IF NOT EXISTS characters (
    principal_id     BLOB NOT NULL PRIMARY KEY,   -- auth.db principals.id; immutable
    name             TEXT NOT NULL UNIQUE,        -- kernel-owned given name; immutable in v1
    created_at       INTEGER NOT NULL,
    retired_at       INTEGER,                     -- characters retire; never deleted
    -- arrive with later slices:
    root             INTEGER NOT NULL DEFAULT 0, -- no model; see "Roots and rotation"
    default_cast_id  BLOB REFERENCES casts(cast_id) ON DELETE SET NULL,
    rc_dir           TEXT,                        -- default /config/rc/character/<name>
    memory_root      TEXT,                        -- host path; NULL = no memory of its own
    handoff_ctx      BLOB REFERENCES contexts(context_id) ON DELETE SET NULL,
    root_ctx         BLOB REFERENCES contexts(context_id) ON DELETE SET NULL
);
```

Three rules make the principal a safe key:

- **`name` is the kernel's, not `auth.db`'s.** It is the drift address,
  and after the keyring melt there is no other name for it to disagree
  with — `principals.username` is gone, not demoted.
- **A principal with a character cannot be removed.** `remove_principal`
  refuses while a `characters` row references it (`auth_db.rs:242` deletes
  today, cascading credentials). Retirement sets `retired_at` and, in the
  same act, concludes and archives every live context the character plays.
  A retired character therefore has no live performance to start a turn
  in; `kj drive` already refuses an archived context (`kj/drive.rs:265`),
  so the loud failure is the one that exists.
- **A missing mapped principal is corruption**, never a fallback to `system`.

#### How principals line up today, and why the sheet is not a second truth

Read this before deciding where a character is created; the answer is not
the one the two-database split suggests (verified 2026-09-06).

**The join key is always the opaque `PrincipalId`. Every name is a
denormalized display label, cached where the identity was seen, and none of
them is authoritative.**

- `auth.db` holds two tables: `principals(id, username, display_name)` and
  `credentials(fingerprint → principal_id)` (`auth_db.rs:36–53`). It is a
  **credential map**, not an identity registry. `create_principal`
  (`auth_db.rs:160`) mints a `PrincipalId::new()` and names it; only
  `authenticate` joins through credentials.
- **The kernel never reads `auth.db`.** Zero reads in the crate. It stores
  bare principal ids with no foreign key — `contexts.created_by`, every
  `BlockId`, the roster's principal rows — and says so deliberately at
  `kernel_db.rs:1209–1211`: a `PrincipalId` is a bare, un-rowed identity
  here.
- The kernel already caches a name for one: `roster_entity.label`, whose
  schema comment is explicit that it is display only and never the join key
  (`kernel_db.rs:1195–1198`). It is fed from a peer's self-reported nick
  (`roster_sources.rs:92`), not from `auth.db`.
- The one live id → name resolution is `answerer_name` (`rpc.rs:474`), in
  the **server**, which can do it only because the server owns both
  databases. It reads `display_name`, else `username`, else the id's short
  form.

So `characters.name` does not compete with `auth.db` for a truth that
`auth.db` holds. It is **the first authoritative name the kernel owns**, and
every other name in the system is already a cache of something else. The
sheet referring to a principal id across databases is the same move
`created_by` has always made.

Two consequences:

- **A model character needs no `auth.db` row until it gets a key.** Nothing
  authenticates as it, so nothing looks it up by fingerprint.
- **A name rendered from `auth.db` would be a second name.** Rather than
  reconcile the two, the melt below removes one: names leave `auth.db`
  entirely.

Given name = `characters.name`, and it is the only name in the system.
Signature is rendered from the principal and its credentials, never stored.

Presence is a query over the roster: rows for the principal plus rows for
the contexts it plays, liveness the maximum across them. Never derive a
character's liveness by counting blocks it authored; human prompts, system
output and several contexts played at once would all distort it. The
roster's self-reported `Availability` is the seed of the deferred
availability field and needs no change now.

The alternative, a distinct kernel-owned `CharacterId` with a one-to-one
immutable principal mapping, is what the frontier review recommended. It
buys type safety at API boundaries: "who performs this context" can never
be confused with "who authenticated this request". It costs a second id on
every surface that names a character. The three rules above deliver the
permanence half of that argument, and the keyring melt takes most of the
type-safety half: with `Principal` reduced to an id, there is no longer a
named struct on the authentication path for a performer to be confused
with. Revisit only if a review finds a surface that still conflates them.

### `auth.db` is a keyring

Decided with Amy, 2026-09-06, after "How principals line up today" showed
that nothing joins on a username. **Names melt out of `auth.db` into the
character sheet, and `auth.db` answers exactly one question: which principal
does this fingerprint belong to?**

```sql
-- auth.db, after the melt
CREATE TABLE IF NOT EXISTS credentials (
    fingerprint   TEXT NOT NULL PRIMARY KEY,
    principal_id  BLOB NOT NULL,   -- bare identity, as everywhere else
    kind          TEXT NOT NULL DEFAULT 'ssh_key',
    key_type      TEXT NOT NULL,
    key_blob      BLOB NOT NULL,
    comment       TEXT,
    created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
    last_used_at  INTEGER
);
```

`principals` loses `username` and `display_name`, which is everything it
had beyond an id and a timestamp. What remains of it is a bare existence
row; whether to keep it as the `ON DELETE CASCADE` target or drop it and
let `principal_id` be un-rowed here the way it is in `kernel.db` is an
implementation call for the slice.

**Resolved (slice 2 implementation): kept, un-rowed relationship.**
`principals(id, created_at)` stays — `ensure_principal_row` upserts it
before every `add_key`/`rebind_key` write, which is what lets `add-key`
record a binding with no live `kernel.db` connection (the name was already
resolved to an id before this write). `credentials.principal_id` carries no
FK, matching the printed schema above ("bare identity, as everywhere
else") — `remove_principal` is gone, so there is no delete path for the
cascade to guard.

A pre-existing installation's `credentials` table keeps its OLD
`principal_id REFERENCES principals(id) ON DELETE CASCADE` (only
`principals` is rebuilt to shed `username`/`display_name`; `credentials` is
untouched) — harmless day to day, but load-bearing for the migration that
sheds those columns: SQLite's `ALTER TABLE ... DROP COLUMN` refuses a
column carrying a `UNIQUE` constraint (`username` was `UNIQUE`), so the
migration rebuilds `principals` via `CREATE` + `INSERT ... SELECT` +
`DROP TABLE` + `RENAME`. `DROP TABLE` on a table another table's row
references performs an *implicit* `DELETE FROM` first when foreign key
enforcement is on — which fires that legacy `ON DELETE CASCADE` and erases
every credential the instant `principals` is dropped. `PRAGMA foreign_keys`
has to go `OFF` for the rebuild and back `ON` after (it is a no-op inside an
open transaction, so this must bracket the `BEGIN`/`COMMIT`, not sit inside
it). Caught by a test that asserts a fixture's key still authenticates
after migrating — the crash would otherwise be silent (`unwrap()` all
succeed; the row is just gone).

**`Principal` loses its name, and with it its reason to exist.**
`authenticate` returns a `PrincipalId`. The struct is constructed in exactly
two production places today, both in `auth_db.rs` (`:136`, `:544`), so the
change has one small blast radius and no wire consequence: a `BlockId`
already carries `{contextId, principalId, seq}` and no name (`rpc.rs:10236`).

Three rules follow.

- **One name, one source.** `characters.name` is the only name a player
  reads. `Identity` on the wire keeps carrying it — a client cannot
  round-trip per block — and is the one sanctioned cache, the same pattern
  `roster_entity.label` already documents (`kernel_db.rs:1195–1198`).
- **A log line takes the id, not the name.** `id.short()` is greppable,
  unambiguous, and cannot go stale. The `ssh.rs` and `share.rs` sites that
  print `principal.username` today want that, not a resolver call. The
  surfaces that genuinely need a name are the `Identity` fill, the ledger's
  answerer column (`answerer_name`, `rpc.rs:474`), and the app chrome, which
  reads `Identity` and does not change.
- **`display_name` does not survive.** Its content is the key's comment
  (`atobey@zorak`), and `credentials.comment` is the column that already
  describes the key. A name for a credential belongs on the credential.

**The sentinels stay constants, not rows.** `PrincipalId::system()` and
`::beat()` derive from fixed `UUIDv5` seeds (`ids.rs:230`) and their names
are already compile-time constants (`principal.rs:41`). They get no
`characters` row: a character is a continuous lineage of interactions, and
the kernel's own hand is not one — rows for them would make the table a
junk drawer of every id that needs a label. One resolver,
`name_for(PrincipalId)`, checks the two sentinels and then the sheet.
`PrincipalId::for_agent_session` needs no answer at all: the bridge
connects with a real key now.

**Adding a key binds; it never mints.** `kj character create` mints the
principal id, so `add-key` takes an existing one:

```sh
kj character create kaijutsu-lead                       # mints the principal id + the sheet
kaijutsu-server add-key ~/.ssh/kaijutsu-lead.pub --as kaijutsu-lead
kaijutsu-server list-keys                               # fingerprint → character
```

`--nick` and `add_key_auto_principal` go away with it. Without this
inversion, `add-key --nick kaijutsu-lead` would quietly create a second
principal with the same name in the other database — the corruption case,
and a silent one.

**Bulk import is removed** (Amy, 2026-09-06). `import_authorized_keys`
minted a principal per imported key, which is exactly the minting this
design takes away, and there is no sensible character to bind a file of
keys to. First run becomes two deliberate steps, `create` then `add-key`,
which is the right shape for an act that establishes who someone is.

**There is no anonymous auth.** An unknown key is rejected. The ephemeral
test config runs `init` in its temporary directory and connects with the key
it bound, so tests and production take the same path.

**Three name reads break at compile time** when `Principal` loses its
fields, and all three land in the same change: `answerer_name`
(`rpc.rs:483`), `whoami`'s `Identity` fill (`rpc.rs:3077`), and
`materialize_context_shell_for`, which builds a shell name as
`"{kernel}-{username}-{session}"` (`rpc.rs:9307`). The first two read
`name_for`; the third wants `id.short()`, since it is naming a shell, not
a person.

**`add-key` gains a `kernel.db` read** it does not have today: it opens
`auth.db` only (`main.rs:359`), and resolving `--as <name>` means opening
`kernel.db` read-only as well. Writes stay in `auth.db`.

**`add-key` never rebinds silently.** `credentials.fingerprint` is the
primary key, so one key maps to one principal and the same key cannot be
bound to two characters. Adding a key that is already bound refuses and
names the current binding — `key SHA256:… is bound to banto; move it with
--rebind` — rather than issuing an UPDATE. A silent move would take a live
session's identity out from under it.

**`auth.db` moves to WAL.** It sets only `foreign_keys` and `busy_timeout`
today (`auth_db.rs:60`) while `kernel.db` has been WAL since it was written
(`kernel_db.rs:1951`). In rollback-journal mode a writer locks the whole
file, so `add-key` against a running server retries for five seconds and
then fails `SQLITE_BUSY`, blocking any connection authenticating meanwhile.
That is nearly invisible while add-key is a once-a-machine act and becomes
routine the moment binding a character's key is the normal path. There is
no cache to invalidate underneath it: `AuthDb` holds one long-lived
connection (`ssh.rs:320`) and every lookup is a fresh query, with no
`HashMap<PrincipalId, _>` anywhere, so a CLI side-write is visible to a
running server on its next read once the lock allows it.

### Bootstrap: the person creates themself

Before the first start, the person who runs the kernel creates their own
root character and binds their key to it:

```sh
kaijutsu-server init --as amy --key ~/.ssh/id_ed25519.pub
systemctl --user start kaijutsu-server   # creates the root context `amy`
ssh kaijutsu                             # you are amy, in your root context
```

`init` writes `kernel.db` and `auth.db` directly, so run it with the service
stopped. It checks before it writes: a different live root character, a
retired name, or a key bound to another character refuses with no change.
An existing live character of that name becomes the root and keeps its
principal id. Running the same `init` again changes nothing.

**The server refuses to start without a live root character**, and the
error names `init`. At start it creates the root context of each live root
character that has none.

**A root context is a model-less admin console.** Its type is `root`, its
label is the character's name, the character plays it, and it has no parent.
The `root` rc bundle grants the operator authority (`admin`, `config-write`,
`operator`, `exec`, the shell facades) and composes no instruction blocks.
Seats a root starts, such as banto's, are created from it, so their lineage
and accountability begin there.

**Roots are equal, and one is typical.** A second person joins through a
root: `kj character create bob --root` creates bob and bob's root context,
then `kaijutsu-server add-key bob.pub --as bob` on the host binds the key.
Every player is still inside one trust boundary (`docs/instrument-design.md`,
"Many hands, one trust boundary"); a person you would not give a shell
account belongs on a separate kernel.

**A kernel wipe orphans every binding.** The schema stance is that a
version bump wipes (`kernel_db.rs:1959`), which takes `characters` with it
while `auth.db` keeps every credential bound to a principal id that now has
no sheet. `name_for` renders those as `id.short()`, the loud failure the
"missing mapped principal is corruption" rule wants. Recovery is `init`
again, with `add-key --rebind` for any key `init` refuses to move. The keys
still authenticate; it is the names that vanish.

**Lockout recovery works with the service stopped.** Retiring the last live
root stops the next start. `kaijutsu-server list-characters` reads
`kernel.db` read-only, and `init` with a new name or `add-key` gets back in.

So the server CLI is four verbs: `init`, `add-key --as`, `list-keys`, and
`list-characters`. All of them work with the service stopped.

### A context is played by a character

```sql
ALTER TABLE contexts ADD COLUMN played_by BLOB;  -- principal_id; NULL = nobody (score ctx, file doc)
```

Set explicitly with `kj context create --as <character>`; omitting it leaves
`played_by` unset on the kj path. Fork copies it. Server client creation
defaults to the creating principal's character, when present. Automatic
inheritance from a calling model context remains a design question.

**Two identities ride every turn, and neither replaces the other.** The
**requester** is who caused the turn: the principal on the `KjCaller`, on the
user's prompt or `kj drive` seed (`kj/drive.rs:135–172`), on a gate's ask,
and on `TurnFlow` events. The **effective actor** is the character whose
performance this is, resolved once at turn start from `played_by`. Who gets
stamped where:

| Block or record | Principal |
|---|---|
| capability checks, drive origin, user prompt and seed, approval ask, `TurnFlow` | requester, unchanged |
| provider-emitted thinking, model text, model tool call | effective actor (today `system`) |
| tool result, structured tool error, kernel warning, error and interrupt markers | `system`, unchanged |
| rc output | the context's `created_by`, unchanged (`rc/mod.rs`) |

The criterion is **provenance, provider output versus kernel output, not
role**. The max-iterations halt is `Role::Model` and kernel-generated, so it
stays `system`; the tool result is kernel-generated and stays `system`
(`llm_stream.rs:2396`). Once a model block is created under the actor, its
streaming appends use that same principal. A NULL `played_by` keeps today's
behavior exactly.

Two seams the audit named that slice 3 must decide, not discover: builtin
tool servers author their blocks under the requester (`mcp/servers/shell.rs:345`,
`block.rs`, `tasks.rs`, `background.rs`), so after slice 3 a model's
`ToolCall` carries the character while the tool's own output block carries
the human who drove the turn; the matrix above says tool output is kernel
output and should stamp `system`, and those servers should follow it. And
the gate-resume seed passes no principal (`rpc.rs:1316`), falling through
to the store default `system` (`rpc.rs:2431`), where `kj drive`'s seed stamps
the caller (`kj/drive.rs:158`); the resume seed is a requester act and should
say so.

Two invariants to pin with tests, because both are already load-bearing:

- **Approval redemption keys on the requester.** Gate resume reuses the
  principal that raised the ask, never a fresh one (`rpc.rs:1131–1146`), and
  the anti-self-approval rule compares contexts, not principals
  (`approval-ledger/src/decide.rs:33–47`). `approval.context_id` = the
  raising context; `approval.principal_id` = the requester; the model block's
  author = the actor. If accountability must appear on an ask, add a column;
  do not change the redemption key.
- **The block id changes lane.** `BlockId` embeds `principal_id`
  (`kaijutsu-types/src/block.rs:38–42`), so a model block moves from the
  system principal's sequence lane to the character's. Anything that
  compares block or principal ids across that boundary sees new values.

Clients do not infer the performer from blocks. Context metadata gains an
optional `played_by` id and the character's name; the tui labels a model's
divider from the cast label or the model leaf today (`kaijutsu-tui/src/app.rs:285–298`)
and would read the name from there. The app keeps its authenticated session
principal for drafts and local blocks; a character playing a context is not
the app's compose identity.

### The bridge identity: a key per model character

Today a Claude Code session reaches the kernel through `kaijutsu-mcp
--connect`, configured once in `~/.claude.json` for the whole user. The MCP
takes no user or key argument; the client's default key source is the SSH
agent, trying every key it holds (`kaijutsu-client/src/ssh.rs:29–32`), so
the kernel sees Amy's fingerprint, finds it in `credentials`, and the
connection is principal `amy`. That principal is the `cc-*` context's
creator (`rpc.rs:5086`), what `whoami` answers (`rpc.rs:3076`), and the
roster's bound row. Blocks the bridge authors are a different story:
`authorBlock` takes the principal from the wire by design (`rpc.rs:8118–8139`),
and the hook listener supplies a **deterministic per-session id**,
`PrincipalId::for_agent_session(<Claude Code session id>)`
(`kaijutsu-mcp/src/hook_listener.rs:775–786`; rationale at
`kaijutsu-types/src/ids.rs:252–271`). So a bridge session already authors
under its own principal, one that exists in no table and changes with every
Claude Code session. Blocks written by builtin tool servers on a bridge
session's behalf carry the requester instead (`mcp/servers/shell.rs:345`).
The MCP's `session_principal` field was an earlier attempt and is dead code
(`kaijutsu-mcp/src/lib.rs:661–664`). The bridge identity below replaces an
anonymous per-session author and an `amy` connection with one named
principal for both.

**The fingerprint is the identity handle.** `credentials` is keyed by
fingerprint (`auth_db.rs:44`), so the string that selects a key out of the
agent is the string the kernel uses to find the principal. One value, two
lookups, nothing to keep in sync. Decided with Amy, 2026-09-05:

- `kaijutsu-mcp` gains `--key-fingerprint SHA256:…`, selecting exactly one
  agent identity, and `--key-file <path>`, reading an **unencrypted** key
  file through the client's existing file key source (`ssh.rs:34–37`). Each
  has an environment variable fallback so a per-repo `.mcp.json` can set it
  in its `env` block; a flag wins over its variable. Both given is an error.
  A fingerprint the agent does not hold, or an encrypted key file, **fails
  the connection loudly** and names what was asked for; it never falls back
  to trying every key, since that would silently reconnect as Amy. Neither
  given keeps today's behavior.
- Keys for model characters are unencrypted and live under `~/.ssh/`, so a
  login shell can `ssh-add` them without anyone thinking about it, or the
  MCP reads the file directly. Their public halves go in through the
  existing auth import. Inside one trust boundary an unencrypted local
  identity key is the same posture as the agent socket itself.
- The user-scope entry connects as a generic `kaijutsu-mcp` principal. Any
  session without a named lead shows up as that, which already beats showing
  up as Amy. Each repo with a lead adds a `.mcp.json` entry naming its own
  key, so the kaish repo connects as `kaish-lead` and this one as
  `kaijutsu-lead`. Project-scope entries override the user-scope entry of
  the same name; confirm that precedence at setup.
- The dead `session_principal` field is deleted in the same change. A real
  identity flows through the connection now.

**Setting one up** (the client side is built; the kernel needs nothing):

```sh
ssh-keygen -t ed25519 -N "" -C "kaijutsu-lead" -f ~/.ssh/kaijutsu-lead   # unencrypted, on purpose
kj character create kaijutsu-lead                                       # mints the principal id + the sheet
kaijutsu-server add-key ~/.ssh/kaijutsu-lead.pub --as kaijutsu-lead     # public half → a credentials row
ssh-add ~/.ssh/kaijutsu-lead        # or skip the agent and use --key-file below
ssh-add -l                          # copy the SHA256:… line for this key
```

Before the melt lands, the middle two lines are one
`kaijutsu-server add-key --nick kaijutsu-lead`, which mints the principal
itself. That is how the first live run below was set up.

Then in the repo's `.mcp.json`, on the `kaijutsu` server entry:

```json
"env": { "KAIJUTSU_KEY_FINGERPRINT": "SHA256:…" }
```

or `"KAIJUTSU_KEY_FILE": "/home/atobey/.ssh/kaijutsu-lead"` to read the
file directly. On a machine where the MCP entry is machine-specific (a
`target/debug` path), prefer `claude mcp add -s local kaijutsu -e
KAIJUTSU_KEY_FINGERPRINT=SHA256:… -- <path> --connect`: local scope is
per project and per user, is not committed, and overrides the user-scope
entry of the same name. `kaijutsu-server list-keys` confirms the row. The MCP warns
at connect when it is probably using a personal key: the default
try-every-agent-key mode, or a `--key-file` named like `~/.ssh/id_*`. It
still connects, so a first run stays easy.

What it buys, before any character table exists: blocks authored by the
character on the wire; asks raised by the lead's principal and answered from
Amy's, the cross-character shape the ledger already wants (its rule keys on
context, so nothing there changes); a bound roster row per connected lead,
which is the identity half of the roster inversion for free; and `kj whoami`
from the bridge saying who is speaking.

First live run, 2026-09-05 14:55, zorak: the bridge authenticated with the
`kaijutsu-lead` key while the SSH username stayed `atobey`, and `whoami`
answered `kaijutsu-lead`, so **the server resolves the principal from the
key alone**; the username is not compared. The client's log line
"Authenticated as atobey with key SHA256:…" names the SSH username, which
reads as the wrong identity now that keys carry the identity; it should say
"ssh user". No personal-key warning fired. The hook socket bound on the new
hosting pid without waiting. Still to read once: the `mcp` type's governance
script and the hook pipeline's dry-run path for anything keyed on the
username `amy`, and whether the kernel roster and the cc-peer roster agree
about who is in the room when one process is two names.

This is slice 1's first character row without the sheet, and the cheapest
possible test of "a model character is a principal", the question the
frontier review pushed on. It needs no kernel code. The fix to the hook
listener archiving the wrong context on `session.end` (`docs/issues.md`,
"The hook listener archives another session's context") should land first,
because a spurious archive re-runs the `mcp` create bundle and drifts the
bridge context's label.

### rc is a union

`load_scripts(context_type, verb)` becomes
`load_scripts(context_type, character, verb)`, following one rule in
order: enumerate `/config/rc/<type>/<verb>/` and `<rc_dir>/<verb>/`;
validate every name in both (the invalid-name rule already fails the whole
verb, `rc/mod.rs`); **reject a canonical filename present on
both sides, loudly**, no shadowing; combine and sort once as one `Vec` by
filename (`names.sort()` at `:401` is the existing sort; two pre-sorted lists
concatenated would put a character `S05` after a type `S10`); snapshot every
body; execute the snapshot. Collision is judged on the link's own filename,
since that is what governs ordering today (`:372–374`). One new rc
variable, `KJ_CHARACTER` (the name); `KJ_CONTEXT_TYPE` is already seeded. A character
script branches in kaish:

```kaish
set -e
case "$KJ_CONTEXT_TYPE" in
  coder)    kj env set REPO ~/src/kaish ;;
  reviewer) kj binding deny shell ;;
  *) ;;
esac
```

Type specializations compose the same way from the type side, with the
symlink pattern rc already has (`docs/rc-on-disk.md`). **Directives do not
compose up the accountability chain automatically.** A character script
that wants its accountable character's stance reads it explicitly; a
recursive merge would bring graph-ordering, cycle and provenance questions
before the two-directory case has proven itself. rc output keeps its
current author, the context's `created_by` (`rc/mod.rs`);
`played_by` does not replace it.

The frontier review argued for deferring the union entirely, since
symlinks already cover static reuse. Amy's stated need is dynamic: one
character's scripts choosing what comes through in each type. The union
stays in the plan, after the handoff, and ships when a real character needs
it.

### The handoff is an ordinary context

Each character gets a context of `context_type = "handoff"`, pointed at by
`characters.handoff_ctx`, with a persisted hydration policy so a reader
takes a window (`kj context hydrate --window N`, `kj/context.rs:1498`;
`kernel_db.rs:5443`, default `:5433`).

- `kj handoff note "…"` appends one block to the caller's character's
  handoff context, authored by the caller's principal. A note from another
  character lands in the same log with that author. **The message board is
  this log read by someone else.**
- `kj handoff tail [--window N] [<character>]` reads the last N notes.
- `create` rc gains `S16-handoff.kai`: inject `kj handoff tail --window 12`
  as a `Notification` block, the way `S15-recall.kai` injects the memory
  indexes today (same never-exit-nonzero contract, same cache-safe slot).
- Compaction is `kj stage exclude` on old notes plus a summary note at each
  rotate. A `conclude`/`rotate` verb asks the character for the summary.
- `signoff.md` retires when this lands; the durable parts melt into docs as
  they do now.

The first draft of this design put the handoff on a **track's score
context**, by analogy with the musician's page-turn. The frontier review
moved it, and the argument holds: a score context is minted with
`context_type = "score"` and reconstructed as ABC cells
(`kaijutsu-server/src/beat.rs:399–424`), a track's durable row is clock
state (period, phrase, playhead, wakeup, rotate; `kernel_db.rs:959–1005`),
and a track restarts stopped after a kernel restart (`docs/tracks.md`,
"as built"). None of that is a handoff log. What the analogy was reaching
for, write ahead of the boundary and re-read a window, the hydration policy
already provides on any context. A track remains the right **cadence
source** for the janitor (`docs/tracks.md` §5).

The block-store shape concern stands: blocks are one CBOR blob per document,
and a handoff log is that shape. It is small.

### Drift addresses a character, explicitly

`kj drift push @<name>` (or `character:<name>`) resolves to the character's
handoff context, always, whether or not a live context matches. A bare name
keeps today's meaning, a live context. The first draft proposed a bare name
that meant "live context if present, else the handoff"; that makes one
command change destination as liveness changes, which is a silent fallback,
so it is out. Push and pull share one resolver (`docs/drift-ux.md` gap 3).
Delivery does not wake anyone (gap 2, shape D). `kj cc send` stays: the
proctor drives Claude Code sessions through it.

### The janitor, then the proctor

A character (`yakin`, 夜勤) woken by a system-clock track, local cast,
accountable to kaijutsu-lead. Its `tick` rc:

1. **Janitor.** Archive contexts idle past a threshold; sweep throwaway
   probe contexts by label pattern; cancel asks from archived contexts once
   `kj ledger cancel` exists; prune `backups/` by a written rule.
2. **Proctor.** For each live context whose tail is newer than its
   character's last handoff note and idle past a threshold:
   1. raise an ask to the accountable human, through the ledger, as the
      janitor requester;
   2. if unanswered and the cache is still warm, `kj drive <ctx> --prompt`
      with published text asking the session to write its handoff note and
      sign off. Same model, cached input; the session's own model is both
      the best summarizer and the cheapest one. Works on a kaijutsu context
      and, through `kj cc send`, on a Claude Code session (liveness there
      needs work). The prompt is authored by the janitor; the output is
      authored by the target character; the janitor never masquerades as
      either the accountable human or the target;
   3. if the cache is cold, fork with filters (drop tool results and trace
      blocks, window the rest) and drive the child to summarize with the
      same model on a fraction of the input;
   4. if unreachable, archive and leave a note saying so.

The distillation refusal is already in: when the caller's cast and the
source's differ and no distill model is named, the kernel refuses and names
both casts and the flag; `kj drift pull` and `merge` take `--distill-model`.

The proctor is last on purpose. It depends on reliable liveness, consent to
external drive, the warm/cold cache policy, archival semantics, and the
requester/actor split above. `kj drive` already refuses inappropriate
context states and can target a context other than the authorizing caller
(`kj/drive.rs:60–121`); that is infrastructure, not the policy.

## Rollout, smallest first

Each slice is independently shippable and leaves the tree green.

0. **Terms and docs.** This file; Terms table rows; devlog chapter. Done
   with this commit.
0a. **Prework, no kernel code** (2026-09-05). **Landed; each item
   re-verified 2026-09-06.** The `PrincipalId` consumer audit; one
   `actor_principal` binding threaded through the turn path with no
   behavior change (`llm_stream.rs:1404`, pinned by a source test at
   `:4409` asserting exactly six provider-output sites); `--distill-model`
   on pull and merge with the caller-versus-source refusal (`kj/mod.rs:757`);
   push and pull on one resolver (`kj/drift.rs:316`, `:456`, `:582`); the
   hook-listener `session.end` guard (`hook_listener.rs:232`); the roster's
   periodic refresh wired into the server (`rpc.rs:2990`);
   `KJ_CONTEXT_TYPE` seeded for rc (`rc/mod.rs`); and the bridge
   identity above (`kaijutsu-mcp/src/main.rs:88–103`).
1. **The sheet. Shipped 2026-09-06.** `characters` with its first four columns;
   `contexts.played_by`, copied by fork; `kj character
   create|list|show|retire`, where `create` mints the principal id and
   `retire` concludes and archives every context the character plays;
   optional `played_by` and the name in context metadata. No attribution
   change — every block is stamped exactly as it is today. First rows by
   hand: `amy` (the existing principal) and `kaijutsu-lead`.
   Tests: NULL `played_by` preserves today's behavior; fork copies it;
   retiring archives the live contexts and leaves blocks and their authors
   intact; an unmapped character fails loudly.
2. **The keyring melt. Shipped 2026-09-06; the migration ran on zorak
   2026-09-07** after a full rehearsal on a snapshot: six principals became
   six characters, `hajime` was seeded as a seventh, and no key binding
   moved. Found while checking whether an unmigrated restart was safe: it
   is not, and it does not announce itself, because `CREATE TABLE IF NOT
   EXISTS` is a no-op against the old table and only a later principal
   mint hits the legacy `NOT NULL username`. The server now refuses to
   start on a pre-melt `auth.db` (`tests/auth_db_premelt_guard.rs`). The
   `principals`/FK-cascade findings above are from this pass. `auth.db`
   drops `username` and `display_name` and
   gains `PRAGMA journal_mode = WAL`; `authenticate` returns a
   `PrincipalId`; `Principal` loses its name fields; `add-key --as
   <character>` binds, never mints, and refuses an already-bound
   fingerprint without `--rebind`; `--nick`, `set-nick`,
   `set_display_name`, `add_key_auto_principal` and
   `import_authorized_keys` are removed; `kaijutsu-server list-characters`
   reads `kernel.db` read-only; a fresh kernel seeds `hajime` with a minted
   id and the empty-auth log line names it; anonymous auto-register
   (`ssh.rs:1111`) binds to `hajime` rather than minting;
   `name_for(PrincipalId)` resolves sentinels then the sheet; the three
   compile-time name reads — `answerer_name` (`rpc.rs:483`), `whoami`'s
   `Identity` fill (`:3077`), and `materialize_context_shell_for`
   (`:9307`, which wants `id.short()`) — land in this change; `ssh.rs`,
   `share.rs` and `sftp.rs` log lines and `tracing` fields take
   `id.short()`.
   **This slice follows the sheet immediately** because between the two
   there are two minting paths for one name, and `add-key --nick <name>`
   would quietly create a second principal beside the character.
   Tests: a bound key authenticates to the character's principal; a name
   renders identically from the wire before and after; a fingerprint with
   no character fails loudly rather than rendering blank; re-adding a bound
   fingerprint refuses and names the current binding; `--rebind` moves it;
   a fresh kernel seeds exactly one character and `list-characters` finds
   it with the service stopped; the migration
   (`kaijutsu-server::migrate_keyring`) turns pre-existing principals into
   characters carrying their old usernames, against a fixture mimicking a
   real `auth.db`, never against anything real; anonymous auto-register
   binds to `hajime` and mints nothing. (2026-09-16: `init` replaced
   `hajime` and anonymous auth; see "Bootstrap: the person creates
   themself".)
3. **Attribution. Shipped.** Create-time performer selection
   (`kj context create --as`) landed separately. Turn-start resolution of
   `played_by` to the effective actor, and provider-emitted blocks authored
   by it, is `turn_identity::resolve`
   (`crates/kaijutsu-kernel/src/runtime/turn_identity.rs`), stamped at the
   tool-call and tool-result sites in
   `crates/kaijutsu-kernel/src/runtime/llm_stream.rs`. This was the slice
   that changed `BlockId` lanes; it shipped alone.
   Tests: a model block's author is the character and differs from the
   requester; the prompt's author is still the requester; a tool call is
   actor-authored while its result stays `system`; appends use the inserted
   block's principal; `TurnFlow` still names the requester; approval
   redemption still uses the ask's original requester; the app's draft
   owner is still the session principal; the multi-producer beat path
   routing a cell failure to its own producer has no test covering it today
   (`beat.rs:1464` `producer_ctx_for`).
4. **Handoff context.** `handoff` type, `kj handoff note|tail`, hydration
   policy set at creation, `S16-handoff.kai` in the `coder` and `mcp` create
   bundles, `register_session` sets `played_by`. `characters.handoff_ctx`
   arrives here. This is the slice that changes tomorrow morning, and it
   needs only slice 1 — if the morning is worth more than closing the
   double-mint window early, it can swap with slice 2, as long as nobody
   runs `add-key --nick` in between.
   **Shipped 2026-09-07**, verified in two real create lifecycles. The
   decisions, all reversible: `note --for <character>` writes into another
   character's log under the caller's name, which makes "the message board
   is this log read by someone else" true before slice 7's addressing
   exists; `tail` never mints, because a read-only verb's whole flag
   surface must be incapable of a write (the verb declares `Effect::Read`
   and the class match is exhaustive, `kj/effect.rs`); `note` is not
   `ConfigWrite`-gated, since leaving a note is ordinary authoring and the
   gate would route every handoff through the lfm2d escalation; no
   character fails loudly and names `kj character create`; `played_by`
   is set by `register_session` with `kj context create --as`, except for a
   root character, which cannot be cast. The window
   is 50, set on the first note (a marker needs a block to anchor on).
   `S16-handoff.kai` guards `kj handoff tail` inside the substitution: a
   trailing `||` never fires in kaish, and under `set -e` a failing
   substitution aborts the script, so a principal with no character got an
   Error block on every create until the guard moved.
5. **rc union.** Two directories, one sorted list, collision is an error,
   `KJ_CONTEXT_TYPE` and `KJ_CHARACTER` seeded; `characters.rc_dir` arrives.
   Reseed leaves character dirs alone (they are not shipped defaults).
6. **Roster inversion.** `kj roster` groups rows by character, presence
   specified as the aggregation above. The periodic refresh is already wired:
   `create_shared_kernel` spawns it every 10 s under the server's shutdown
   token (`kaijutsu-server/src/rpc.rs:2984`; `roster_sources.rs:67`). A stale
   doc comment said otherwise until 2026-09-05.
7. **Drift to a character.** `@name` addressing; one resolver.
8. **Janitor, then proctor.** The `yakin` character, its track, its tick rc;
   `default_cast_id` arrives with the character rows that
   need them; the distill-cast refusal and `--distill-model` on pull.

Deferred: saifu, availability, and `memory_root` until a reader exists.
`root_ctx` has its reader, rotation ("Roots and rotation" above), and is
queued in `docs/issues.md`. Not doing: party, chair, seat, any reputation or trust score
(shared trust rules it out), a `Character` Rust type.

## What changes for a morning

Today: Amy restarts sessions; I read `signoff.md`, exomemory, and the memory
index by hand; the name lives in a flag.

After slice 4: Amy restarts sessions; `register_session` makes a bridge
context played by `kaijutsu-lead`; `create` rc injects the last twelve
handoff notes and the memory indexes; the notes carry who wrote them and
when, including anything another character left overnight. After slice 8:
a session that goes idle gets asked to write its own note before it goes
cold, so the morning window is current without anyone remembering to write
it.

## Open

- **A repository-wide audit of `PrincipalId` consumers** before slice 3
  lands: broker policy, hooks, telemetry, indexing, client caches. The
  reviews established the turn path, capabilities, ledger, roster, RPC, tui
  and app; they did not establish every consumer. Anything that treats a
  block's principal as the authenticated requester needs a separate field
  or durable turn provenance, not a second overload of the author field.
- **Does a character carry one default cast, or one per type?** Start with
  one.
- **A musician's memory.** Bass-san's `memory_root` is NULL by choice.
- **Socket-protocol liveness** for driving a Claude Code session
  (`docs/cc-peer.md`).
- **The desk character** (受付, Uketsuke): an automode context that answers
  some asks by policy and escalates fewer to Amy. `docs/issues.md` "The
  escalation seat" is the earlier sketch of the same idea.

## Review

- **kaibo, cast `crusoe` (GLM-5.2 synth, DeepSeek-V4-Flash explorer),
  2026-09-05, whole files attached.** Confirmed every citation but two.
  Corrected: the attribution gap was misread (model blocks carry `system`,
  not the driving human; the cited lines were `TurnFlow::Failed` publishes),
  and the track citation pointed at the attachment half only. Narrowed the
  blast radius of re-stamping model blocks to the wire author projection.
  Confirmed the rc union and the cross-database reference are consistent
  with the loader and the schema, and supplied the sort-once and
  link-name-collision notes.
- **kaibo `deliberate`, cast `gpt-deliberate` (GPT-5.6 synth on the batch
  lane, GPT-5.6 explorer), 2026-09-05, dossier
  `kaibo://cas/866c1b75…`.** Adopted: the requester versus effective-actor
  split and its attribution matrix; provenance not role as the criterion;
  the ledger redemption invariant; the block-id lane change; the
  kernel-owned immutable `name` and the no-deletion rule, after it showed
  `auth.db` renames and deletes principals; the handoff on an ordinary
  context instead of a track score; explicit `@name` drift addressing in
  place of a liveness-dependent fallback; no automatic rc inheritance up
  the accountability chain; rc output keeps its `created_by` author; the roster inversion was gated on wiring the refresh loop, which
  turned out to be wired already; slice 1 trimmed
  to identity and attribution with the sheet's other columns arriving with
  their readers; the `PrincipalId` consumer audit. Declined, for Amy:
  a distinct `CharacterId` (see "Character = principal + sheet"); deferring the rc union
  outright (kept, ordered after the handoff). Every citation acted on here
  was re-read in the source before it was written down.

- **PrincipalId consumer audit (Opus lane, read-only, 2026-09-05),
  `scratchpad/audit-principal-consumers.md`.** Every reader classified.
  Findings folded in above: the beat scheduler's `played_by` read, the
  tool-server authoring split, the gate-resume seed, the bridge's
  per-session author principal, and the absence of any `system()`
  comparison or principal-keyed cache. One headline was an artifact: it
  reported the `actor_principal` binding as pre-existing, but that binding
  is the refactor lane's in-progress edit to the same file (zero occurrences
  in HEAD at the time). A read-only lane running beside builders must read
  `git show HEAD:<path>`, not the working tree.

- **kaibo, cast `crusoe` (GLM-5.2 synth), 2026-09-05 afternoon, on the
  MCP bridge and SSH client changes, whole files attached.** Found no
  defect in the session-end guard, the ping hiding, the fingerprint parity
  with `auth_db`, or the flag/env precedence. Two follow-ups adopted: a
  `Stale` probe verdict is confirmed by a second probe before a socket is
  unlinked, because BSD-derived systems return ECONNREFUSED for a live
  listener with a full backlog where Linux returns EAGAIN; and an empty
  environment value counts as unset. Noted and accepted: a session whose
  end event carries no session id, or whose scraped id happened to be
  right with no earlier event, stays un-archived, and the warn line names
  why.
- **kaibo, cast `crusoe-ds4` (DeepSeek-V4-Flash), 2026-09-05 afternoon, on
  the kernel and server changes, whole files attached.** The authorship
  classification and `KJ_CONTEXT_TYPE` threading are clean. Two defects
  adopted: the distillation refusal compared `DriftRouter` pairs, which
  exist only for explicitly pinned contexts, so a default-configured caller
  pulling from a pinned expensive source, the very case it was built for,
  slipped through; both sides now resolve through the turn path's ladder.
  And `kj drift push` could deliver into an archived context through the
  router fallback, mutating retained work and firing its `drift` rc; push
  now refuses an archived target as `kj drive` does. Minor: the roster
  loop test's two-second deadlines were a flake vector under load.

- **kaibo, cast `crusoe` (GLM-5.2 synth, DeepSeek-V4-Flash explorer),
  2026-09-06, whole-file attach of this design plus `auth_db.rs`,
  `principal.rs`, `ids.rs`.** A readiness review of the keyring melt before
  any code. Confirmed the structural claim (nothing joins on a username;
  authentication resolves from the fingerprint alone and never compares the
  SSH login user), the no-cache claim, the WAL implication, the slice 1→2→3
  ordering and both of its stated reasons, the rebind mechanic against the
  primary key, and the lockout recovery. Found and fixed here: the
  anonymous auto-register path at `ssh.rs:1111`, a *runtime* minting path
  the design had missed entirely; `materialize_context_shell_for`
  (`rpc.rs:9307`) as a third compile-time name read; `add-key`'s new
  `kernel.db` dependency; the wipe-and-rebind scenario; and five stale
  citations — the hydrate window (`kernel_db.rs:5413` → `:5443`), three
  `llm_stream.rs` anchors that sat near their stamp sites rather than on
  them, and the tool-result stamp (`:2376` → `:2396`). Every correction was
  re-read in the source before it was applied, which is how the
  `allow_anonymous` finding sharpened: the mode is off in `production()`
  (`ssh.rs:241`) and on only in the ephemeral test config (`:226`), so it
  binds to `hajime` rather than needing to survive as a minting path.

## Records

- Design artifact, three passes with concept art:
  https://claude.ai/code/artifact/3cd90372-dc4c-4fa9-8255-a66d4c16d824
- Sheet sketches (superseded by the third pass above):
  https://claude.ai/code/artifact/7d9b47f7-b528-4306-8a77-241fce26dd93
- exomemory `daily/2026-09-05.md` carries the fleet-facing decision.
