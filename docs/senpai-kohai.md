# Senpai and kohai

Every context answers to one other context: its **senpai**. The context is that senpai's **kohai**. The senpai edge is
the only thing that routes asks, reports and escalations. The one exception is a **root console**, the context
of a root character (`characters.root_ctx`), which answers to nobody: the human is at the top.

A context carries the character that performs it and the senpai it answers to, on its own row in `contexts`
(`performer`, `senpai_ctx`, `persona`), with a copy of the model settings it runs on in `context_slots`.

Decided by Amy:

- Delegation grants, the reviewer override and the `forked_from` review walk are replaced by the senpai edge,
  not kept beside it (2026-10-18).
- "I *hate* fallbacks. remove it." A model comes from one place, and nothing stands in when it is missing.
  "mid-context re-cast would be bad." A conversation keeps the model and persona it started with (2026-10-19).
- One record per context. "why does an instance need to have a conversation? I get why a conversation has an
  instance." Casts melt into characters: "melt them, it's too complex having both layers." Rotation does not move
  kohai in the kernel: "A rotating senpai knows who its kohai are so we should figure out a handoff rather than
  trying to embed to much of this in kaijutsu." A fork takes the forking context as its senpai (2026-10-20).
- A character has one model, and a role slot exists only where a role differs ("B seems like a nice improvement
  let's go that way"). MCP sessions keep the root console as their senpai, and a model-less hub is their parent
  ("go with i"). "senpai should not be a kj verb!" `kj context create` names the character, not the label
  (2026-11-01).
- Every context has a senpai, with no "no character" branch, and a context's own performer may archive it:
  "1. yes". The model console is renamed: "oh, I like console, let's do it." (2026-11-01).

A new install, end to end:

```
kaijutsu-server bootstrap --as amy --backend deepseek --model deepseek-flash
kj context create banto --name lead
kj context create coder --senpai lead
```

Bootstrap creates the shipped characters, banto and coder among them, each with the model it was given. Run from
the `lead` context, the last line is `kj context create coder`. The recipe in "Composing senpai and kohai" does
both lines at once.

Each `kj context create` is ready for a turn: the context copies the character's model under `model` and
`distill`. A Claude Code session connects with `kaijutsu-mcp --connect` and gets an `mcp` context under the hub,
with no ask.

## Names

- **Person**: a root character. Every context a person performs is alike: it runs no model
  turns, its draft runs as shell, and the person may confirm their own commands there.
- **Root console**: the context `characters.root_ctx` names on a root character's sheet. It is the person's own,
  performed by them, and has no senpai.
- **The console**: the model console a person talks to, the context `characters.console_ctx` names on the
  person's sheet; bootstrap labels it `console`. A non-root character performs it and the person's root console
  is its senpai. `kaijutsu-server bootstrap` makes it. Only its person archives it while that person is a live
  root, and the gate always asks first; the archive clears the pointer. Drifts into it wait for the person
  ("Reports and drift").
- **Performer**: the character that plays a context. **Senpai**: the context a context answers to. **Kohai**: a word for the
  relationship only. The kj surface says "the contexts that answer to X" and lists them with `kj context list
  --senpai X`.

## The model

Two layers. Static settings are edited in place. A context takes its performer, persona and slots when it is
made; only its `senpai_ctx` changes after that, through `kj context set --senpai`.

```
static     characters           principal_id, name, root, rc_dir, created_at, retired_at, handoff_ctx,
                                root_ctx, console_ctx, mcp_hub_ctx
           character_models     principal_id, backend_id, model, max_tokens, temperature, top_p, effort,
                                thinking_budget, thinking_style
           character_slots      principal_id, role, backend_id, model, max_tokens, temperature, top_p, effort,
                                thinking_budget, thinking_style
dynamic    contexts             context_id, forked_from, label, archived_at, performer, senpai_ctx,
                                persona, ...
           context_slots        context_id, role, backend_id, model, max_tokens, context_window, temperature,
                                top_p, effort, thinking_budget, thinking_style
turns                           turn_id, context_id, provider, model, raw tokens, ...
```

- **A character** is the persistent someone a name resolves to. Keys in `auth.db` bind to it. Its row holds
  static settings only: its name, the root flag, its rc directory (`characters.rc_dir`, the directory under
  `/config/rc/` its contexts run), a root's console and MCP hub pointers, and its handoff log. Editing them
  changes no context. `kj character create <name> --rc <dir>` and `kj character set <name> --rc <dir>` set the
  directory; it defaults to the character's name, and a root's to `person`.
- **A character's model** (`character_models`, at most one row) is the one model its contexts' turns run on. `kj
  character create <name> --model <backend>/<model>` sets it, and `kj character model set|unset` changes it. A
  root character has none: a root runs no model.
- **A character slot** is the model a character uses for one of two roles: `distill`, a cheaper model for `kj fork
  --compact` and `kj drift pull|merge`, and `gate`, the council server that judges the gate. `kj character slot
  set` refuses any other role, and a `gate` slot on a backend that is not kind `mk`. A NULL parameter takes the
  model's `backend_models` value when a context is made. There are no casts: each character holds its own slots.
- **A context's performer** (`contexts.performer`) is the character that plays it. A trigger refuses a change to
  `performer` and `persona` once set, and `context_slots` rows are never changed. `persona` is NULL until
  characters carry a persona. A context with no performer (scratch, the handoff log, the MCP hub, the drift queue,
  lost+found, the beat score, cold-start discovery) runs no turns, but still has a senpai.
- **A context's senpai** (`contexts.senpai_ctx`) is NULL only for a root console. The insert path refuses any
  other NULL, and a trigger refuses an update to NULL unless the context is a character's `root_ctx`. An existing
  row may name an archived senpai (see `docs/issues.md`). The senpai chain never has a cycle. Setting an edge
  that would close one is refused.
- **The root console** of a root character is `characters.root_ctx`, a pointer like `handoff_ctx`. Rotating the
  root console moves the pointer at the commit, as it moves `handoff_ctx`. A root console cannot be forked,
  moved under a senpai, or archived; `kj context rotate` replaces it, and `kj character set <person> --no-root`
  archives it with the console and the MCP hub when the person stops being one. A context with no senpai must be
  performed by a root character; boot archives a live one that no `root_ctx` names (a rotation
  that never committed), and when it mints a fresh root console for an archived one, the live console that
  answered to the old one answers to the new one.
- **Live** means not archived. A concluded context is live; verbs that refuse a concluded target say so.
- **The contexts that answer to** a context (its kohai) are the live contexts whose `senpai_ctx` is that context.
- **The handoff log** belongs to the character: `characters.handoff_ctx`. Every context of the character writes
  and reads the same log. A rotation reads the predecessor's handoff. For a fresh log, `kj context rotate` of the
  handoff context points `characters.handoff_ctx` at its successor.
- **The lineage root** is the character that performs the root console at the top of the senpai chain. It must
  be a live root character, or the rule that needs it refuses with the reason.

What stays as it is:

- A context has no type: its performer's rc directory, gate layers and model say what it does
  (`docs/character.md`, "The type melts into the character").
- `forked_from` and fork kind: provenance only. `forked_from` is the only parent link.

## Where a model comes from

Every model call reads the context's own `context_slots` and nothing else. A model turn in context K runs on K's
`model` slot, whoever performs K, and the durable turn row records role `model`. A context with no performer runs no
model.

- Making a context copies everything a call needs into `context_slots`:
  1. the character's model, under `model`, and under `distill` when the character has no `distill` slot and the
     model is no council server. A character with no model gives the context no `model` slot;
  2. the character's `distill` and `gate` slots, under their roles.

  Each copy takes the backend, model and parameters, and for each parameter left unset, the value in the model's
  `backend_models` row, context window included. After that no call reads `character_models`,
  `character_slots` or `backend_models`; they are catalogs read when a context is made. Kaijutsu supplies no
  parameter value of its own.
- `max_tokens`, which every provider request carries, must come from the character's model or slot, or from the
  model's row. A slot with none refuses when the context is made, naming the character, the role and the model.
  A slot whose backend is a council server (kind `mk`) names a served model, needs no `max_tokens`, and is never
  a chat model: that is decided by the backend's kind, not by the role's name. A `gate` slot whose backend is not
  a council server refuses when it is written. `context_window` comes from the slot's model row in
  `backend_models`, or is unknown. An unset optional parameter (temperature, top_p, effort, thinking) is left out of
  the request. A context window none holds shows as unknown.
- A context with no `model` slot has no model. `kj context create` and `kj context rotate` say so in their output
  ("<context> has no model: <character> has no model; `kj character model set <character> <backend>/<model>`, then
  `kj context rotate`"), and `kj context info` shows the same line. An MCP session, a context whose parent is an
  MCP hub, prints no such line: its client brings its own model ("MCP sessions"). A model turn in K refuses before
  any provider work, naming the character and the same fix. A judge shadow, whose `model` slot is on a council
  server, has no chat model: `kj context info` says it runs on a council server. A listing shows a context whose
  slot cannot be read with the error in its model column and lists the rest.
- The gate runs on K's `gate` slot and the judge on its shadow's `model` slot. The two are independent: a context
  with no `gate` slot has no council, and its gate decides without one; a council block with no `judge` key runs
  its council with no judge.
- A judge shadow belongs to one context. The gate makes it as a context performed by the judge character the
  council block names, with K as its parent and its senpai, and slots copied from the judge character. It is
  found as K's live child that character performs. A context is a judge shadow when its parent is its senpai and
  its performer is a judge: a character whose model is on a council server, or one some council block in a
  gate.toml that loaded names as `judge`. `kj context set --senpai` refuses to move one. The kernel archives K's
  judge shadows in the same transaction that archives K, at `kj context archive`, a rotation's commit, a demote
  past the rim and a character's retirement, and the successor's first gate call makes a new one.
- A fork's distillation runs on the forking context's `distill` slot, and `kj drift pull|merge` on the caller's
  `distill` slot, so a cheap model can summarize for an expensive conversation. A character with a model and no
  `distill` slot distills on its model. With no `distill` slot in the context, a fork that asks for distillation
  refuses, naming the character and the fix. A one-shot prompt takes `max_tokens` from the slot it runs on.
- A backend's connection facts (`base_url`, keys, timeouts, `max_concurrent`) are read live. They are
  infrastructure, not a model choice.
- To move a context to another model or persona, change the character's model or slots and rotate it. To run two
  models side by side, play a second character.
- Slots are frozen after create. No verb edits a running context's slots.

## Routing

Reviewer of an ask raised in context K by actor A:

1. Let S be K's `senpai_ctx`. If S is null (K is a root console): when A is K's live root character, the ask is a
   self-confirmation; when A is not a root and K's performer is a live root, that root reviews it (a model acting
   in a root's context); otherwise refuse, naming A and K.
2. A senpai with no performer is passed over: the climb continues to its senpai.
3. The reviewer is the performer of S when that character is not retired, is not A, and is not K's performer. S
   may be archived; asks route to a character, so an archived senpai's character still reviews.
4. When S's performer is A or K's performer, climb: K := S, and repeat from 1. A fork's performer is its forking
   context's character, so an ask in a fork, by anyone, climbs past that character.
5. A retired character refuses by name. It never substitutes another character.

A missing context row, a senpai with no row and a senpai cycle are corruption and fail loudly. There is one
resolver (`KernelDb::route_review`).

- **Escalation** runs the same climb from the ask's context, skipping senpai until one is held by a character
  other than the current reviewer, and refuses at a context with no senpai, naming it. `kj ledger escalate --to`
  and the lineage root's take-over are overrides held by the lineage root alone.

## Authority

One rule, with no branch for a context that has no performer: **the performer of a context's senpai (climbing
past senpais with no performer), or its lineage root**, may:

- move it to another senpai (`kj context set <ctx> --senpai <ctx>`);
- escalate its asks;
- rotate it (its own performer may also rotate it, and so may an rc lifecycle running for that context);
- archive it (its own performer may also archive it, so an MCP client archives its own session when the session
  ends);
- change its egress (`kj context set --egress-allow|--egress-deny`).

The context's own performer is refused for every other act.

**Placing under a senpai.** `kj context create --senpai <ctx>` and `kj context set --senpai` share one rule over the new
senpai: the actor performs it (or the nearest performer above it), or is its lineage root. A new edge refuses an
archived senpai. A copied edge (fork, rotate) keeps the senpai it copies, archived or not.

**A context made by a lifecycle** (a handoff log, a hub) with no senpai named answers to the root console of the
lineage root of the context that made it, else the console of the only live root. A kernel with no live root
console, or several and no usable origin, refuses (`KernelDb::default_console_for`).

**A character's model and slots** are written with `kj character model set|unset` and `kj character slot
set|unset`, which need the `ConfigWrite` capability, as `kj character set` does.

A verb nested in an rc lifecycle uses a narrower rule in place of this one, with the context whose lifecycle runs
as the actor: it may act on the contexts that answer to that context, so an archive rc may archive its own kohai. In a
rotate lifecycle it may also act on the contexts that answer to the predecessor (see
"Rotation hands off its contexts"). The context's character lends the lifecycle nothing. A selection such as
`--children` that names a context the rule does not cover refuses, naming it.

The refusal names the rule: "only <context>'s senpai (<character>) or its lineage root (<root>) may <act>". A
context's performer never changes, so there is no recast act; to put another character on the work, create a
context for it.

Unchanged: the performer can never review its own work, except a person in a context they perform, which runs no
model turns; a model turn refuses to start when its performer would review itself.

## Creating, forking, rotating

- `kj context create <character> [--name <label>] [--senpai <ctx>] [--parent <ctx>] [--cwd <dir>]
  [--env K=V]...` makes a context performed by the character. The positional names the live character; there is
  no `--as`.
  - **Label:** `--name`, else the character's name when no live context holds it, else `<name>-2`, `-3`, … (the
    first free one); the output names the label. A `--name` another live context holds refuses.
  - **rc directory:** the character's `rc_dir`. A character with none refuses, naming
    `kj character set <name> --rc <dir>`; no directory stands in.
  - **Senpai:** the caller's context, or `--senpai <ctx>` under "Placing under a senpai". `--parent` sets
    `forked_from` only. A create with no calling context and no `--senpai` refuses: the kernel does not pick one.
  - **Slots** are copied from the character, with unset parameters filled from each model's `backend_models`
    row. There is no per-create model option: to run another model, start a context of a character whose slot
    has it. The create needs the Operator capability and a live character caller.
  - A context a root character performs runs no model turns, so a root's `kaijutsu-tui` or
    `kaijutsu-mcp` session names the character that plays what it creates: `--character <name>` on both, and
    `KAIJUTSU_CHARACTER` for MCP (the flag wins). Without it a root refuses, with an error that names the option.
    The context `Ctrl+Z` opens is performed by the person at the client, whatever `--character` says.
- `kj fork`: the child copies the forking context's performer, persona and slots, with the forking context as its
  senpai. Asks from a fork climb past its performer to the forking context's senpai (Routing, step 4). A root
  console cannot be forked.
- `kj context rotate <ctx>`: a new context for the same character, with slots copied fresh from the character.
  It copies the env, cwd and shell, and the predecessor's senpai; its `forked_from` is the predecessor's
  parent, so the successor is a sibling. The label and ring position move at the commit. The kernel moves no
  context that answers to it (see "Rotation hands off its contexts").
- `kj context rotate` of a character's handoff log points `characters.handoff_ctx` at the successor. A context
  with no performer rotates without the model check. `kj handoff note` refuses an archived log.
- `kj context archive` archives the context it names, and with `--children` the selection it names. The kernel
  moves no context that answers to it: their asks still reach the archived senpai's character, and a push to it
  refuses until they are moved (`kj context set <ctx> --senpai <ctx>`) or the senpai is promoted. Unresolved asks raised *in* the archived context are
  swept with a note; asks raised in the contexts that answer to it are not touched. The kernel archives its judge shadows in the
  same transaction, as it does at a rotation's commit and a character's retirement (`docs/council.md`, "Shadow
  context"); other contexts that should go with it go by its archive rc.
- `kj context set [<ctx>] --senpai <senpai-ctx>`: moves a context under another senpai. It needs the authority rule over the moved context and
  "Placing under a senpai" over the new one. It refuses a cycle, a root console, and a judge shadow. It refuses
  while a pending ask would change reviewer: it re-resolves every pending ask raised in the moved context and in
  the contexts below it whose climb passes through it, and refuses, naming the asks, when any reviewer would
  differ.
- `kj context list --senpai <ctx>` lists the live contexts that answer to `<ctx>`, and finds them when `<ctx>` is
  archived. The context reference `.senpai` names the senpai of the current context (`.parent` names its fork
  parent; steps chain in any order: `.senpai.senpai`, `.parent.senpai`), so `kj drift push .senpai "<text>"`
  reaches the senpai with no id to type. There is no `kj senpai` verb.
- `kj ledger list --reviewer <character>` lists only the asks whose reviewer is that character.
- `kj context info` shows the context's performer, its senpai, the migration notes for the context, and its
  `model` slot (or why it has none). Its JSON keys are `performer` and `performer_name`. `kj character show`
  lists the character's live contexts.

## Rotation hands off its contexts

The kernel does not move the contexts that answer to a rotated context. The rotating character knows them, and the handoff carries them:

1. Rotate creates the successor and runs its create rc, runs the predecessor's archive rc, and commits: the
   predecessor is archived and the successor takes its label, ring position, `handoff_ctx` and `root_ctx`
   pointers, in one transaction. The contexts that answered to the predecessor still name it.
2. After the commit, the character takes them: `kj context list --senpai <predecessor>` lists them,
   `kj context set <ctx> --senpai <successor>` takes each, and `kj ledger list --reviewer <character>` shows the
   asks raised during the rotation. No lifecycle takes them.
3. Until they are taken, their asks reach the same character (the predecessor's) and wait in the ledger,
   and their pushes to the senpai refuse, naming the archived senpai.

A rotation that fails before the commit leaves the predecessor live and labeled, with the contexts that answer to it, and the
successor live and unlabeled; the error names the successor, which `kj context archive` retires.

The successor of a root console has no senpai until the commit moves `root_ctx` to it. A root-console rotation
that fails before the commit archives the successor and says so, and boot archives any live context with no
senpai that no `root_ctx` names. At the commit the archived console takes the successor as its senpai.

## Reports and drift

- `kj drift push .senpai "<text>"` puts the text in the senpai context as a drift block. It refuses a context with
  no senpai, and an archived senpai, naming `kj context promote` and `kj context set <ctx> --senpai <ctx>`.
- `kj drift push` along the edge, one hop from a context to its senpai or to a context that answers to it, runs with no ask,
  with one exception: a push up from a fork whose `created_by` is not its character keeps the gate. That proxy
  treats a fork a person made, and a judge shadow (made by `system`), as not the context's own; a person
  connected with the fork's character's key reads as the character. A context forks itself the way Claude Code
  forks a subagent, and its pushes up run with no ask. A push two or more hops away keeps the gate. A
  `gate.toml` key on `kj drift push` still wins over the edge.
- **A drift into the console waits for its person.** The hold keys on the person's `console_ctx` pointer, not on a
  type or a label.
  Every drift kind holds except `Notification` (`docs/character.md`, "A drift into the console waits for the
  human").

## What a context sees

- The `<situation>` block names the context's performer and its senpai: `<character name="<name>"/>` and
  `<senpai id="<short id>" context="<label>" character="<name>"/>`. `character` is named even when retired,
  `context` is left out when the senpai has no label, and a root console has no `<senpai>`.
- The rc persona text (banto's S00-stance) says "our senpai" and reads the name from the kernel fact.

## Composing senpai and kohai

```sh
PATH="/config/rc/lib/recipes:$PATH" pair banto coder
```

`/config/rc/lib/recipes/` holds kaish scripts that compose characters and contexts. `pair <senpai-character>
<kohai-character>` creates a context performed by the first character, answering to the context it runs in, then
a context performed by the second, answering to the first, and prints both context ids. It is two `kj context
create` calls, so each create is gated and refused as that verb is, and a refused second create leaves the first
context in place. A recipe runs on demand, by name from `PATH`; no lifecycle runs one, and `kj rc list` does not
list `lib/recipes/`. Test: `crates/kaijutsu-server/tests/pair_recipe_wire.rs`.

## MCP sessions

An MCP client (Claude Code, Codex) brings its own model. It needs a context to author into and a reviewer for its
asks, and no model.

- **The hub.** Each live root character has one MCP hub: the context `characters.mcp_hub_ctx` names, labeled
  `mcp/<root name>`, with no performer, answering to the root console, created by the root. The kernel makes it
  beside the root console, at `kj character create --root` and `set --root`, and at start for every live root
  that has none. Making it is idempotent: the pointer is trusted first, and with the pointer unset a live context
  with no performer on the label is adopted. A live context with a performer on the label refuses
  `create --root` and `set --root`, naming it and its performer, and at start is logged as an error and left
  alone, since the hub is a view and must not stop boot. Archiving the hub clears the pointer. Like a handoff log, the hub runs no turns and routes
  nothing. It exists so `kj context list --tree` shows every session of that root in one place.
- **Registration.** When the parent is a root console (none named and the kernel has one root, or an explicit
  parent that is a root console), the client runs, from the root console:
  `kj context create <character> --name <label> --parent <hub id>`, where the hub is the `mcp_hub_ctx` the
  person's sheet names (`kj character show <person>`). The root console is the session's senpai, so its asks go to
  the root character. The hub is its `forked_from` (`--parent` sets nothing else). `<character>` is the
  connection's own character, or the one `--character` (`KAIJUTSU_CHARACTER`) names for a root connection. A client
  that names a `--parent` (`KAIJUTSU_PARENT`) that is not a root console creates under that context instead. A
  root console whose sheet names no live hub refuses registration, naming the hub. `register_session` takes only
  a label.
- **No ask.** The gate's builtin layer allows `kj context create <character> --parent <hub>` when `--parent`
  resolves to the MCP hub of the caller's lineage root (`characters.mcp_hub_ctx`), the senpai the create sets
  (`--senpai`, else the calling context) is that root's console, and every argument is a literal word. A create from a kohai of the root console takes the ordinary layers and asks. The decision reads `builtin allows kj context create under mcp/<root
  name>, the MCP hub of its lineage root`. Creating a context runs no turn. A `gate.toml` ask or deny key on `kj
  context create`, in any of the caller's layers, still wins.
- **No model.** The client brings its own model, so the session's character has none and the session gets no
  `model` slot; a session prints no no-model notice. A `distill` slot on the character is copied as for any
  context, so `kj fork --compact` and `kj drift pull|merge` work from a session.
- **Session end.** The client runs `kj context archive <id> --confirm` from the session context itself, which its
  own performer may archive (Authority). The gate would ask for it from a context whose performer runs `mcp/`, so
  `gate.toml` ships `kj context archive` in `[rc.mcp] allow`.

## Removed

- Columns `contexts.reviewer_id`, `contexts.director_id`, `contexts.played_by`, `contexts.cast_id`,
  `contexts.provider` and `contexts.model`; the table `context_characters`; tables `approval_reviewer_delegations`,
  `casts`, `cast_slots` and `llm_defaults`; the character's cast column; `presets.cast_id` and
  `presets.system_prompt`. `characters.root_ctx` stays, as the root console's pointer. The
  `context_character_migration_report` table stays; `kj context info` prints a context's notes.
- The reviewer override, delegation grants and the `forked_from` review walk, with the duplicate resolvers.
- `kj senpai` (every subcommand: `show` is part of `kj context info`, `kohai` is `kj context list --senpai`, `set` is
  `kj context set --senpai`, `report` is `kj drift push .senpai`); `kj context kohai`, `kj context move` and
  `kj drift report` (the general verbs and the `.senpai` context reference do their work); `kj cast` (`kj character slot set|unset|list` replaces
  `kj cast slot`); `kj context set --as|--model|--cast|--reviewer|--clear-reviewer|--director`;
  `kj context unset --model|--cast`; `kj context create --as|--cast|--top`; `kj fork --model|--distill-model`;
  `kj model`; `kj backend default`; `kj preset save --cast|--system-prompt`; `kj ledger delegation`;
  `kj character set --cast`. `kj models` (discovery) stays.
- A context's type (`docs/character.md`, "The type melts into the character"): `kj context create --type`,
  `kj context set --type`, `kj context archive --children --type` (`--children --performer` narrows it), `kj
  character create|set --type` and `set --no-type`, `kj rc list --type` (`--dir` filters by directory),
  `KJ_CONTEXT_TYPE`, the tui's `--context-type`, the MCP `register_session` `context_type` field, and the
  `judge` slot role.
- The RPCs `configureLlm`, `setDefaultProvider`, `setDefaultModel` and `getLlmConfig`, the client's
  `set_context_model`, and the MCP `session.start` hook's model write. The wire's `ContextHandleInfo.provider` and
  `.model` carry the context's `model` slot.
- The live `GET /v1/models` lookup for a context window, the DriftRouter handle's provider/model copy, and the
  explicit `model` argument of `prompt`.
- `[council] server` and `[council] model` in `gate.toml`.
- Every value kaijutsu supplies on its own: the 64000 `max_tokens` floor, `BuildOpts::new`'s 64000, the
  openai-compatible client's 8192 for a zero, and the one-shot prompts' 4096.
- Every resolver below the context's slots: the override and registry-default rungs of `model_resolution.rs`,
  `effective_cast`, `LlmRegistry::prompt`'s first-advertised model, the tunable floor, bare-name resolution onto
  the default backend, and distillation's registry-default fall back.
- The structural rows of `context_edges`: `forked_from` is the only parent link, so every edge row is a drift
  edge. `fork_children` (live children, oldest first) answers `kj context archive --children`, the judge shadow
  lookup and the `Children:` count of `kj context info`.

Presets keep only their fork filters (`kj fork --preset full|window|spawn`).

## Migration

Each step runs once on open, in one transaction, and is idempotent. A database that has the earlier shape is
copied aside first (`kernel.db.pre-context-fold` for the fold). A step records every value it drops and every
context it cannot map in `context_character_migration_report`, which `kj context info` prints for the context.

1. **`senpai-edges-v1`**: maps the old reviewer override, director column and `forked_from` walk onto `senpai_ctx`.
2. **`character-slots-v1`**: each character gets a copy of its cast's slots. A character with no cast gets none;
   it is listed with the roles its live contexts use (`no_character_slot`), because a rotation copies fresh from
   the character and would lose the model. We add those slots before deploy.
3. **`context-characters-v1`**: each context with a performer takes a context character of it with its senpai; a
   judge shadow takes the character of the context it judges. Each context's slots are a copy of the effective
   cast its next turn, its judge and its gate would have used under the old resolver. It never invents a value.
   Kinds: `dropped`, `unmapped`, `no_slot`, `no_gate_slot`, `no_distill_slot`, `no_character_slot`.
4. **`context-fold-v1`**: copies each context character row into `contexts.performer`, `senpai_ctx` and
   `persona`, rebuilds `context_slots` to reference `contexts`, and drops `context_characters`. Every live
   context with no senpai that is not a root console gets one: the console of the only live root, else the root
   console at the top of the senpai chain above its nearest `forked_from` ancestor (kind `senpai_assigned`), an
   archived context by the same rule as a live one. A live context with neither refuses the whole migration, naming
   it. An archived context with neither keeps a NULL senpai and never refuses (kind `archived_no_senpai`). It does
   not check that an existing senpai is live. `kj context set --senpai` clears a context's senpai notes.
5. **`console-label-v1`**: the live context of type `console` labeled `ROOT` is relabeled `console`. When another
   live context already holds `console`, the migration refuses, naming both. A live `ROOT` of any other type keeps
   its label and is listed (kind `root_label_kept`).

## Out of scope

- A verify tier for reviewers (read-only builds and tests) is a gate tier change, done separately.
- `@<name>` drift addressing (documented in `docs/character.md`, not verified as implemented).
- A verb that edits a running context's slots.

## Tests (each falsified once)

- An ask in a context goes to its senpai's character; a fork's ask climbs past its performer, including an ask a
  person raises in the fork; a root console self-confirms; a senpai with no performer is passed over; a retired
  character refuses by name; an archived senpai's character still reviews.
- Escalation climbs one step and refuses at the top.
- The authority rule allows the senpai's performer and the lineage root, and refuses a sibling and the context
  itself, for each act it governs; a context's own performer archives it and is refused every other act.
- `kj context set --senpai` refuses a cycle, an archived senpai, a root console, a judge shadow, and a pending ask in the
  moved context or in a fork below it whose reviewer would change; it allows pending asks when every reviewer
  stays the same. `kj context create --senpai` and `kj context set --senpai` use the same rule over the new senpai.
- A context with no performer answers to a senpai; an insert or update that leaves a NULL senpai outside a root
  console is refused.
- A root console cannot be forked or archived; rotating it moves `root_ctx` at the commit, and `kj character set
  <person> --no-root` archives it with the console and the hub in one transaction.
- Archiving the console (with `kj context archive` or the demote step that archives it), `kj character set
  <person> --no-root`, and `kj character retire` of a person or of the console's performer always ask: an allow
  key, a learned rule or the uncovered tier does not skip the ask, and a deny key refuses. An allow runs the
  command as confirmed. Only the console's person archives it, checked again inside the write.
- Archiving a context archives no other context and moves no other context; a push to an archived senpai refuses;
  `kj context list --senpai` shows the contexts of an archived senpai. The `.senpai` reference chains with
  `.parent` in any order, and a step with nothing to follow names the context and the step.
- Create copies every character slot with unset parameters from `backend_models`; a later edit to the
  character's slots or to `backend_models` moves no context; a trigger refuses a change to a context's slots or
  performer; a turn sends no unset optional parameter.
- A fork copies the forking context's slots and takes it as senpai; a rotation copies fresh slots and keeps the
  predecessor's senpai.
- Rotation moves no context that answered to the predecessor; a rotation that fails before commit leaves the predecessor live,
  labeled and with the contexts that answer to it, and the
  successor archived.
- A context with no `model` slot: create and rotate say so, except for an MCP session; a turn refuses and names
  the character and the fix. A chat slot with no `max_tokens` anywhere refuses at create; a council server's is
  exempt by its backend's kind.
- A fork with distillation runs on the `distill` slot, and refuses with no `distill` slot; a context with no
  `gate` slot runs its gate with no council; a `gate` slot on a backend that is not a council server refuses at
  write, and so does a character slot of any role but `distill` and `gate`.
- A drift along the edge runs with no ask, both ways; up from a fork a person made asks; two hops away asks; a
  drift to a sibling still asks; a `gate.toml` ask key wins.
- The drift hold keys on `console_ctx`; a context labeled `console` that no sheet names is not held, and
  archiving the console clears the pointer.
- Two contexts of one character read and write the same handoff log; rotating the handoff context repoints
  `characters.handoff_ctx`.
- The prompt's `<situation>` names the performer and the senpai.
- The migrations: the fold keeps every performer, senpai, persona and slot, gives a context with no senpai the
  rule's console, and refuses naming a context it cannot place; `console-label-v1` relabels the console, keeps a
  `ROOT` of another type, and refuses when `console` is taken.
- A character created with `--model` gives every context it performs a `model` slot and a `distill` slot; a `distill`
  slot wins over the model for distillation; the model never fills `gate`, and a council server fills `model`
  alone; a root refuses a model; `kj character model set` moves no context until it rotates; `kj senpai` is
  unknown.
- The judge shadow is performed by the judge character the council block names, under its seat; a block with no
  `judge` makes none; a judge with no model is a miss naming the fix; `slot-roles-v1` moves type-keyed slots to
  `model`, drops and reports the rest, and archives live judge shadows.
- Bootstrap gives the console character a model, not slots, and a second run changes nothing.
- `kj context create kohai` runs the character's rc directory and takes the label `kohai`; a second one is
  `kohai-2`; `--name` wins; a taken `--name` refuses; a character with no rc directory refuses; `--type` and `--as`
  are unknown.
- A root's MCP hub exists after `kj character create --root` and after start; an MCP registration with no
  `--parent` lands under the hub with the root console as senpai and raises no ask; a `gate.toml` ask key on
  `kj context create` still asks; the session's own performer archives it at session end.
