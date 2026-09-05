# Character — the persistent someone a name resolves to

> Design + rollout plan, 2026-09-05. Drawn from a morning's conversation
> with Amy about her restart-every-session routine and what kaijutsu needs so
> the morning is smooth, then revised through two model reviews the same
> afternoon ("Review", below). Nothing here is built. Every code claim
> carries a `file:line` read that day; re-read it before relying on it.
> Amy's statements are guidance, not rulings.

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

## Decisions (guidance, Amy, 2026-09-05)

- **The connecting thing is a character.** Human or model. Amy is one.
  Opaque UUIDv7 id in records and `.data`; given name in the UI and as the
  drift address. The word fits the cast vocabulary kaijutsu already uses: a
  character is what an actor is cast *as*, and a context is one performance.
- **A character has a default cast.** A gig may pin another.
- **Accountability is a chain and replaces any group concept.** Every model
  character points at a character; a human is a root. No party.
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

The word "character" collides with the text unit. In prose, *character* is
the persistent someone; for text say code point, glyph, or `char`. The
collision is real in code (about 300 uses of the word as a text unit across
the crates), which is one reason the design below adds **no new Rust noun**
for identity.

## What already exists — the parts

Read before designing; most of the character is already in the kernel under
other names.

| Part of a character | Exists today as | Where |
|---|---|---|
| id + given name | `Principal { id, username, display_name }` | `kaijutsu-types/src/principal.rs:16`, `ids.rs:20` |
| handles, many per character | `credentials(fingerprint → principal_id)`; `principals.username UNIQUE` | server `auth_db.rs:37`, `:44`; default path `~/.local/share/kaijutsu/auth.db` (`:104`) |
| presence, derived | the roster: `RosterEntity::{Principal, Context}`, liveness `Bound`/`Recent`, self-reported `Availability {Active, Idle, Away, Dnd}`; four `roster_*` tables; `kj roster` | `kernel/src/roster.rs:103`, `:192`, `:250`; `kernel_db.rs:1189–1281`; `kj/roster.rs` |
| a role's rc bundle | `context_type` → `/config/rc/<type>/<verb>/`, loaded and sorted by `SXX-name` | `kaijutsu-types/src/paths.rs:144`; `kj/lifecycle.rs:352` |
| who plays | casts: one slot per role, keyed `(cast_id, role)`; per-context `cast_id` and `provider`/`model` override; resolution ladder explicit override → cast slot on `context_type` → registry default | `kernel_db.rs:1094`, `:1118`; `contexts.cast_id`; `kj/context.rs:704–722` |
| a cadence that outlives contexts | a track: clock (`BeatPolicy`) + score context + attachments; `kj transport attach` creates the track stopped if absent; non-rotating attachments are first-class | `hyoushigi/mod.rs:45`, `:47`, `:125`; `kj/transport.rs:40–48`; `docs/tracks.md` §5 |
| a window over a long log | `kj context hydrate --window N`: `[0, marker] ∪ last-N`, persisted per context | `kj/context.rs:1497`; `kernel_db.rs:5413` |
| async notes reaching the next turn | the mailbox, a pull cursor over the block log | `llm/mailbox.rs:153` |
| addressed sends | `kj drift push <ctx>`, staged queue, flush, cancel | `kj/drift.rs:37–70` |
| idle detection | `contexts.last_activity_at` | `kernel_db.rs:612` |
| driving a session | `kj drive [ctx] --prompt`; archived contexts refuse | `kj/drive.rs:31–35`, `:265` |
| driving a Claude Code session | `kj cc send` over the socket protocol, with a liveness gate | `kj/cc.rs:55–59`; `docs/cc-peer.md` |
| a Claude Code session as a context | `register_session` makes a `cc-<repo>-…` bridge context | `kaijutsu-mcp/src/lib.rs:1821`; label test `main.rs:560` |

Two facts from that table drive the whole design:

1. **A principal is already most of a character.** It has the opaque id, the
   unique given name, a display name, and many credentials mapping into it.
   Amy's keys from every machine already resolve to one principal. So the
   character is **a principal with a sheet**, not a new identity beside the
   principal.
2. **The kernel already has a roster that knows principals and contexts,
   presence, and self-reported availability.** Inverting it onto characters
   is grouping, not a new store.

## What is missing — the gaps

- **The sheet.** Nothing hangs off a principal but credentials. No
  accountable-to, no default cast, no pointers to rc, memory, handoff, root.
- **A model character's blocks are stamped `PrincipalId::system()`.** The
  turn path inserts every provider-emitted block, thinking, text and tool
  call alike, with the system principal (`kaijutsu-server/src/llm_stream.rs:1818`,
  `:1890`, `:1946`; 21 such sites in that file, not all of them provider
  output). The human's principal goes on the user's prompt block
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
  test that would have caught the collapse, so slice 1 adds one. Nothing in
  production compares a principal against `system()`; the only sentinel
  equality is against `beat()` (`beat.rs:2199`). Sequence lanes tolerate
  foreign principals by construction (`blocks/block_store.rs:353–362`).
- **A context does not know which character performs it.** `contexts` has
  `created_by`, `context_type`, `cast_id`, but no "played by".
- **The auth database does not honor the principal's documented permanence.**
  `Principal` says its id is permanent (`principal.rs:16–18`), but
  `set_username` renames (`auth_db.rs:224`) and `remove_principal` deletes,
  cascading credentials (`auth_db.rs:242`). A character that must retire
  and never be deleted cannot rest on that as it stands.
- **rc reads one directory.** `load_rc_scripts` takes `(context_type, verb)`
  and nothing else; rc scripts see `KJ_CONTEXT`, `KJ_VERB`, `KJ_RC_DEPTH`,
  `KJ_PARENT_CONTEXT`, `KJ_FORK_INFO`, `KJ_PARENT_BLOCK_COUNT`, `KJ_DRIFT_INFO`
  (`kj/lifecycle.rs:523–584`), not the type and not a character.
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
    accountable_to   BLOB REFERENCES characters(principal_id) ON DELETE RESTRICT,
    default_cast_id  BLOB REFERENCES casts(cast_id) ON DELETE SET NULL,
    rc_dir           TEXT,                        -- default /config/rc/character/<name>
    memory_root      TEXT,                        -- host path; NULL = no memory of its own
    handoff_ctx      BLOB REFERENCES contexts(context_id) ON DELETE SET NULL,
    root_ctx         BLOB REFERENCES contexts(context_id) ON DELETE SET NULL
);
```

Three rules make the principal a safe key:

- **`name` is the kernel's, not `auth.db`'s.** It does not follow a
  username rename, and it is the drift address. `principals.username` stays
  the login handle.
- **A principal with a character cannot be removed.** `remove_principal`
  refuses while a `characters` row references it. Retirement sets
  `retired_at`; starting a turn for a retired character fails loudly until
  the context is reassigned.
- **A missing mapped principal is corruption**, never a fallback to `system`.

`principal_id` refers into the server's `auth.db`, a second database. The
kernel already stores principal ids without a foreign key (`contexts.created_by`,
every block, the roster's principal rows; the schema says so at
`kernel_db.rs:1209–1211`: a `PrincipalId` is a bare, un-rowed identity here).
The sheet does the same. A model character is already a legal row in
`auth.db`: `create_principal` (`auth_db.rs:160`) needs no credential, and
only `authenticate` joins through credentials. Creating a character is one
idempotent server operation that makes or resolves the principal and writes
the sheet.

Given name = `characters.name`. Display = `principals.display_name`.
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
permanence half of that argument; the type-safety half is open, below.

### A context is played by a character

```sql
ALTER TABLE contexts ADD COLUMN played_by BLOB;  -- principal_id; NULL = nobody (score ctx, file doc)
```

Set at create: `kj context create --as <character>`; default is the caller's
own character when the caller is a model character's context, else none.
Fork copies it. `register_session` sets it from the session's `--name`.

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
| rc output | the context's `created_by`, unchanged (`kj/lifecycle.rs:196–199`) |

The criterion is **provenance, provider output versus kernel output, not
role**. The max-iterations halt is `Role::Model` and kernel-generated, so it
stays `system`; the tool result is kernel-generated and stays `system`
(`llm_stream.rs:2376`). Once a model block is created under the actor, its
streaming appends use that same principal. A NULL `played_by` keeps today's
behavior exactly.

Two seams the audit named that slice 1 must decide, not discover: builtin
tool servers author their blocks under the requester (`mcp/servers/shell.rs:345`,
`block.rs`, `tasks.rs`, `background.rs`), so after slice 1 a model's
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

What it buys, before any character table exists: blocks authored by the
character on the wire; asks raised by the lead's principal and answered from
Amy's, the cross-character shape the ledger already wants (its rule keys on
context, so nothing there changes); a bound roster row per connected lead,
which is the identity half of the roster inversion for free; and `kj whoami`
from the bridge saying who is speaking.

Things to read once with the new identity in mind on the first live run:
whether the SSH server compares the connection's username to the principal's
username or uses the key alone; the `mcp` type's governance script and the
hook pipeline's dry-run path for anything keyed on the username `amy`; and
whether the kernel roster and the cc-peer roster agree about who is in the
room when one process is two names.

This is slice 1's first character row without the sheet, and the cheapest
possible test of "a model character is a principal", the question the
frontier review pushed on. It needs no kernel code. The fix to the hook
listener archiving the wrong context on `session.end` (`docs/issues.md`,
"The hook listener archives another session's context") should land first,
because a spurious archive re-runs the `mcp` create bundle and drifts the
bridge context's label.

### rc is a union

`load_rc_scripts(context_type, verb)` becomes
`load_rc_scripts(context_type, character, verb)`, following one rule in
order: enumerate `/config/rc/<type>/<verb>/` and `<rc_dir>/<verb>/`;
validate every name in both (the invalid-name rule already fails the whole
verb, `kj/lifecycle.rs:389–396`); **reject a canonical filename present on
both sides, loudly**, no shadowing; combine and sort once as one `Vec` by
filename (`names.sort()` at `:400` is the existing sort; two pre-sorted lists
concatenated would put a character `S05` after a type `S10`); snapshot every
body; execute the snapshot. Collision is judged on the link's own filename,
since that is what governs ordering today (`:372–374`). Two new rc
variables: `KJ_CONTEXT_TYPE` and `KJ_CHARACTER` (the name). A character
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
current author, the context's `created_by` (`kj/lifecycle.rs:196–199`);
`played_by` does not replace it.

The frontier review argued for deferring the union entirely, since
symlinks already cover static reuse. Amy's stated need is dynamic: one
character's scripts choosing what comes through in each type. The union
stays in the plan, after the handoff, and ships when a real character needs
it.

### The handoff is an ordinary context

Each character gets a context of `context_type = "handoff"`, pointed at by
`characters.handoff_ctx`, with a persisted hydration policy so a reader
takes a window (`kj context hydrate --window N`, `kj/context.rs:1497`;
`kernel_db.rs:5413`).

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
0a. **Prework, no kernel code, running or queued as lanes** (2026-09-05):
   the `PrincipalId` consumer audit; one `actor_principal` binding threaded
   through the turn path with no behavior change; `--distill-model` on pull
   and merge with the caller-versus-source refusal; push and pull on one
   resolver; the hook-listener `session.end` guard; the roster's periodic
   refresh wired into the server; `KJ_CONTEXT_TYPE` seeded for rc; and the
   bridge identity above.
1. **Identity and attribution.** `characters` with its first four columns;
   `contexts.played_by`, copied by fork; one idempotent server operation to
   create a character and its principal; turn-start resolution of
   `played_by` to the actor; provider-emitted blocks authored by the actor;
   optional `played_by` and name in context metadata. `kj character
   create|list|show|retire`. First rows by hand: `amy` (the existing
   principal) and `kaijutsu-lead` (a new principal, no credential).
   Tests: NULL `played_by` preserves today's behavior; a model block's
   author is the character and differs from the requester; the prompt's
   author is still the requester; a tool call is actor-authored while its
   result stays `system`; appends use the inserted block's principal;
   `TurnFlow` still names the requester; approval redemption still uses the
   ask's original requester; the app's draft owner is still the session
   principal; a retired or unmapped character fails loudly.
2. **Handoff context.** `handoff` type, `kj handoff note|tail`, hydration
   policy set at creation, `S16-handoff.kai` in the `coder` and `mcp` create
   bundles, `register_session` sets `played_by`. `characters.handoff_ctx`
   arrives here. This is the slice that changes tomorrow morning.
3. **rc union.** Two directories, one sorted list, collision is an error,
   `KJ_CONTEXT_TYPE` and `KJ_CHARACTER` seeded; `characters.rc_dir` arrives.
   Reseed leaves character dirs alone (they are not shipped defaults).
4. **Roster inversion.** After the periodic refresh is wired into
   production (`roster_sources.rs:183` says it is not called from anywhere
   yet): `kj roster` groups rows by character, presence specified as the
   aggregation above.
5. **Drift to a character.** `@name` addressing; one resolver.
6. **Janitor, then proctor.** The `yakin` character, its track, its tick rc;
   `accountable_to` and `default_cast_id` arrive with the character rows that
   need them; the distill-cast refusal and `--distill-model` on pull.

Deferred: saifu, availability, `memory_root` and `root_ctx` until a reader
exists. Not doing: party, chair, seat, any reputation or trust score
(shared trust rules it out), a `Character` Rust type.

## What changes for a morning

Today: Amy restarts sessions; I read `signoff.md`, exomemory, and the memory
index by hand; the name lives in a flag.

After slice 2: Amy restarts sessions; `register_session` makes a bridge
context played by `kaijutsu-lead`; `create` rc injects the last twelve
handoff notes and the memory indexes; the notes carry who wrote them and
when, including anything another character left overnight. After slice 6:
a session that goes idle gets asked to write its own note before it goes
cold, so the morning window is current without anyone remembering to write
it.

## Open

- **Principal key, or a distinct `CharacterId`?** The frontier review wants
  the distinct id for type safety at API boundaries. This design keeps the
  principal as the key and gets permanence from three rules (kernel-owned
  name, no deletion while referenced, missing mapping is corruption). Amy's
  call; recommendation: principal key, and revisit if a surface confuses
  performer with requester in review.
- **Where do principals live?** In `auth.db` today, owned by the server. A
  sheet in `kernel.db` referencing across databases works the way
  `created_by` already does. A real foreign key would need `principals` in
  `kernel.db`. Decide when the second cross-database read appears.
- **A repository-wide audit of `PrincipalId` consumers** before slice 1
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
  the accountability chain; rc output keeps its `created_by` author; the
  roster inversion waits for the refresh loop to be wired; slice 1 trimmed
  to identity and attribution with the sheet's other columns arriving with
  their readers; the `PrincipalId` consumer audit. Declined, for Amy:
  a distinct `CharacterId` (recorded under Open); deferring the rc union
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

## Records

- Design artifact, three passes with concept art:
  https://claude.ai/code/artifact/3cd90372-dc4c-4fa9-8255-a66d4c16d824
- Sheet sketches (superseded by the third pass above):
  https://claude.ai/code/artifact/7d9b47f7-b528-4306-8a77-241fce26dd93
- exomemory `daily/2026-09-05.md` carries the fleet-facing decision.
