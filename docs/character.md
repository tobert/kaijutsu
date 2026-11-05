# Character — the persistent someone a name resolves to

> Design and current behavior. Built: the sheet, the keyring, attribution, the handoff log, and the type melt
> (slices 0–5, "Rollout, smallest first"). Planned: slices 6–8. The review-routing and slot parts follow
> `docs/senpai-kohai.md`. Amy's statements are guidance, not rulings.

## Current implementation

Read this section and "The type melts into the character" for current behavior; the sections after them hold the
reasons and the planned work. `AGENTS.md` links here so the roadmap does not become an instruction to use
nonexistent features.

| Part | Contract |
|---|---|
| Identity and sheet | `PrincipalId`, kernel-owned name, creation/retirement timestamps, optional `handoff_ctx`, a `root` flag, `root_ctx`, `rc_dir` (the directory under `/config/rc/` its contexts run), and for a root the `console_ctx` and `mcp_hub_ctx` pointers; `kj character create [--root] [--rc <dir>]\|list\|show\|set --root\|--no-root\|--rc <dir>\|--console <ctx>\|--no-console\|retire`. A character has no type. `--rc` takes a top-level rc directory with a runnable `create` script, never `lib`, and `person` only for a root (`create` and `set --rc` refuse it otherwise, naming `--root`); the default is the character's name, or `person` for a root, and `create` and `set --root` check the default as they check `--rc`, before anything commits. `kaijutsu-server init` writes `person` without a check, since it runs with no kernel; the next start refuses if `person/create` cannot bind the root console. `--console` takes a live context performed by a live non-root character that answers to the root's root console; a rotation of that context moves the pointer with the label. `show` prints the directory and the pointers |
| Bootstrap | `kaijutsu-server bootstrap` runs `init`, creates the shipped characters that are missing ("Shipped characters"), and runs the `kj` verbs that give the console a model (`docs/server-cli.md`, "bootstrap"). `kaijutsu-server init --as <name> --key <pubkey-file>` alone creates the first root character and binds its key, with the service stopped. The server refuses to start without a live root character. There is no anonymous auth. See "Bootstrap: the person creates themself" |
| Root context | Each live root character has one, the context `root_ctx` names: labeled with the character's name, played by it, with no parent and no senpai (a root console). `kj character create --root` and `set --root` create it; the server creates any missing one at start, and archives a live context a live root performs with no senpai that no `root_ctx` names (a rotation that never committed); one it cannot archive is logged by name, and start goes on. Every archive refuses a root console, naming `kj context rotate`: `kj context archive` and the demote ladder's archive step check before they run anything, and the write checks again. A rotation moves `root_ctx` to the successor before it archives the predecessor, and a retirement clears it before it archives; a live root with no `root_ctx` gets a new root console at the next start, and a live console or MCP hub that answered to the archived one answers to the new one, with a warning naming both. `set --no-root` archives what `root_ctx`, `console_ctx` and `mcp_hub_ctx` name, with their judge shadows, and clears the three pointers and the flag in one transaction, after the archive lifecycles of the root console and the console run; it refuses, naming them, while the character performs any other live context or the hub has live sessions, for the last live person, for a caller who is not a live person or who may not archive the console, for a character whose rc directory is `person` or unset unless `--rc` names another in the same call, and when a pointer moved after the lifecycles ran, naming the move. The gate always asks before it runs. `set --root` refuses while the character performs live contexts, naming them. Beside it each live root has an MCP hub, labeled `mcp/<name>`, with no performer: the parent of the root's MCP sessions, made at the same three points and named by `mcp_hub_ctx`. With the pointer unset, a live context with no performer on the label is adopted; one with a performer refuses, naming it. A context is the hub of one sheet. Archiving the hub refuses while live sessions have it as their parent, naming them, checked again inside the write, and otherwise clears `mcp_hub_ctx` in the same transaction |
| Person | A person is a root character. Every context a person performs is alike: it runs no model turns (a turn refuses naming the person, and so do `kj drive` and `kj fork --prompt`, before they write anything), its compose draft runs as shell, and the person may confirm their own commands there, so `kj context create <person>` skips the self-review check. Only a live root performs a context with no senpai. A person (a live root character) holds human-root authority from any context; a context grants none because of what it is or its label (`kj/context.rs`, `require_placement_authority`). A person may also archive a context whose lineage has no live root; a broken lineage (a senpai cycle) refuses, naming it. Only a live person retires a person. Retiring a person clears their `root_ctx`, `console_ctx` and `mcp_hub_ctx` in the transaction that retires them; their console and hub stay live, since the person does not perform them |
| Console | The console is the context a root's `console_ctx` names, whatever its label. A caller in it may promote, demote and conclude any context, but not demote or conclude a root console or the console. Only the person whose console it is concludes, demotes or archives it while that person is a live root; a console whose sheet names someone else any person may conclude, demote or archive, and a drift into it refuses, naming `kj character set <person> --console <ctx>` and `kj context archive`. The archive write checks the owner again, for `kj context archive` and the demote ladder's archive step, and the gate always asks before `kj context archive` of a console and before the demote step that archives one. A context is the console of one sheet: `set --console` refuses a context another sheet names, naming it, and two sheets that name one refuse every check that reads the owner, naming both. Archiving it clears `console_ctx` in the same transaction; a rotation moves the pointer at the commit. A drift into it waits for the person. `kaijutsu-server bootstrap` finds it by the pointer and its performer; with no pointer it adopts a live context labeled `console` that the console's performer plays under the root console, and names any other |
| Wire facts | `ContextHandleInfo` carries `performer` (the name, empty for none), `performedByPerson`, `isRootConsole`, `isConsole` and `isMcpSession`, computed in the kernel (`KernelDb::context_facts`) and decoded into `kaijutsu_client::ContextInfo`. `contextType @12` stays declared and the kernel leaves it empty; `ContextInfo` has no type. Clients decide from the facts: `Ctrl+Z`, the start ranking (the console, else the only live root console), the tui's shell-draft handling, and the MCP self-archive. The tui divider, picker, ask cards and ledger, the time well's card accent, `kj system ps`, `whoami` and `kj ledger runs` show the performer |
| Credentials | `auth.db` binds fingerprints to principals; `add-key --as <character>` binds to an existing character |
| Context | A context carries its performer, senpai and persona (`contexts.performer`, `senpai_ctx`, `persona`) and a copy of the model slots it runs on (`docs/senpai-kohai.md`). `kj context create <character> [--name <label>]` needs the Operator capability and a live character caller, requires the character, takes the label from `--name` or the character's name (`<name>-2`, `-3` while a live context holds it), runs the character's rc directory, refuses an unknown or retired character, one that names no rc directory or whose directory has no `create` script, and one that would review itself, and preserves the requester's `created_by`. It copies the character's slots into the context. A context has no type. `kj context archive --children --performer <character>` narrows the archive to the children that character performs. A fork continues its source's performance; a rotation writes a successor with the same character and fresh slots. The performer of a context never changes; to put another character on the work, start a context of it |
| Client creation | There is no create RPC. A client runs `kj context create <character>` through `executeKj` from an existing context, which becomes the new context's senpai. A client with no context yet uses `--parent <label>`, or the kernel's only live root context, and refuses with several roots (`kaijutsu_client::choose_parent`). The performer is the connection's character; a person's connection names another with `--character` (`kaijutsu-tui`, `kaijutsu-acp`, `kaijutsu-mcp`, and `KAIJUTSU_CHARACTER`), and without it refuses. An MCP session created from a root console passes `--parent <hub id>`, the `mcp_hub_ctx` the person's sheet names (read with `kj character show`), so the hub is its `forked_from` and the gate raises no ask (`ActorHandle::mcp_hub_of`; `docs/senpai-kohai.md`, "MCP sessions"). `Ctrl+Z` in a root console or the console opens `kj context create <person>` from it (`docs/tui.md`) |
| Gate layers | `gate.toml` reads `[global]`, then `[rc.<dir>]` for the performer's `rc_dir`, then `[root]` when a root character performs the context, then `[character.<name>]` for the performer; each later one reads over the earlier, so the narrowest decides. A context with no performer reads `[global]` alone; a context id with no row refuses. `[context_type.<type>]` and a bare `[context_type]` fail the load naming their replacements; a section naming no rc directory or character is listed by `kj ledger rules` and warns when the set of such sections changes between loads. See "Gate" and `docs/gate-policy-tuning.md`, "Performer layers" |
| Senpai edge | Every context answers to one senpai context (`contexts.senpai_ctx`), except a root console; a context with no senpai must be performed by a live root character; the senpai's character reviews the context's asks, receives its reports, and holds authority over it. One resolver, one authority rule, no per-context override and no delegation. See `docs/senpai-kohai.md` and `docs/approval-identity.md` |
| Model | A character has one model (`character_models`), set with `kj character create <name> --model <backend>/<model>` or `kj character model set <name> <backend>/<model>`, `unset` or `show`; a root runs none. It may also hold a `distill` and a `gate` slot (`character_slots`, `kj character slot set\|unset\|list`); any other role refuses, and a `gate` slot must name an `mk` backend. A context copies them into `context_slots` when it is made: the model under `model`, and under `distill` when the character has no `distill` slot and the model is no council server; then the `distill` and `gate` slots. A character with no model gives a context no `model` slot. Unset parameters come from `backend_models`. Every model turn reads the context's own `model` slot, and its turn row records role `model`. No default stands in; a context with no `model` slot gives no model, and a turn refuses naming the fix (`kj character model set <name> <backend>/<model>`, then `kj context rotate`); an MCP session, whose parent is an MCP hub, prints no notice. A slot on an `mk` backend is a council server: it needs no `max_tokens` and is never a chat model. The gate uses the context's `gate` slot and distillation its `distill` slot. The judge is a character: `judge = "<character>"` in a council block of the seat's performer's gate layers (`[rc.<dir>.council]`, `[character.<name>.council]`) names it, its shadow is a child of the seat that character performs, and the shadow's `model` slot is the judge's server |
| Model invocation | Resolve live, distinct performer/reviewer characters before starting the turn, and refuse a context a person performs. Provider output and tool calls carry the performer; the requester stays separate |
| Approval | Asks snapshot performer and reviewer. Only that reviewer may decide; the performer cannot approve from any context, except a person's self-confirmation in a context they perform. See `docs/approval-identity.md` |
| Standing rules | `kj ledger allow <id> --remember character [--family]` learns a rule bound to the ask's performer that applies wherever it performs. An allow covers `kj drive`, `kj interrupt`, `kj context archive`, and `kj context create` only on contexts whose senpai that character holds. See `docs/approval-identity.md`, "Standing rules for a character" |
| Retirement | `kj character retire <name>` concludes and archives the live contexts the character performs, judge shadows included, retires it, and clears the sheet's `root_ctx`, `console_ctx` and `mcp_hub_ctx`, in one transaction: a refusal or a fault leaves everything as it was. Only a live person retires a person, and a person's retirement archives their root console. A character that performs a live person's console is retired only by that person; anyone else is refused, naming the console, `kj character set <person> --console <ctx>` or `--no-console`, and archiving the console first. A performed context that an archive refuses (an MCP hub with live sessions, a held drift) refuses the retirement before any archive lifecycle runs, and so does the retirement of the last live person, naming `kj character create <name> --root`. The gate always asks before the retirement of a person or of a character that performs a live console. Existing block authors stay unchanged. A console whose person is retired may be archived or re-owned (`kj character set <person> --console <ctx>`) by any live person. A retired senpai character refuses reviewer resolution below it, by name |
| Handoff | Ordinary context referenced by `handoff_ctx`, created on the first note; `tail` never creates it. `note --for` keeps the caller as author |
| Rc | A context runs its performer's directory, `/config/rc/<rc_dir>/<verb>/`; a context with no performer runs none, and a performer with no `rc_dir` fails the lifecycle naming `kj character set <name> --rc <dir>`. A judge shadow is performed by the judge character, so it runs that character's directory (`judge/` when shipped). Scripts read `KJ_CHARACTER` and `KJ_RC_DIR`. A person's Ctrl+Z context runs the person's directory, `person/` by default. Coder, mcp, banto, companion and storyteller include shared handoff injection; banto names the performer and the senpai from context metadata. See "rc" |
| Rotation | `kj context rotate [<context>]` creates a successor with the same character and the predecessor's senpai, slots read fresh from the character's own, copying env and cwd, setting `ROTATED_FROM` to the predecessor's id, and running `create` rc. Only a successor with a loadout takes over: the predecessor's `archive` rc runs, then one transaction archives the predecessor and moves its label and ring position. The kohai point at the context, so they do not move. Rotating a handoff log points `handoff_ctx` at the successor. The performer, the senpai's character, or the lineage root may rotate. See `docs/prompts.md`, "Rotating a context" |
| Accountability | The senpai edge, not `forked_from`. `forked_from` is provenance only. The sheet carries a `root` flag ("Roots and rotation"); there is no `accountable_to` column |

Sources: `kernel_db.rs::CharacterRow`, `kernel_db/context_characters.rs`, `kernel_db/contexts.rs`, `kernel_db/migrate_rc_dir.rs`,
`kernel_db/migrate_slot_roles.rs`, `kernel_db/migrate_type_melt.rs`, `model_resolution.rs`, `kj/context.rs`, `kj/character.rs`,
`kj/gate_policy.rs`, `kj/handoff.rs`, `rc/mod.rs::load_scripts` in `crates/kaijutsu-kernel/src/`;
`crates/kaijutsu-server/src/bootstrap.rs`; `assets/defaults/rc/banto/create/S00-stance.kai` and
`assets/defaults/rc/lib/create/S16-handoff.kai`.

Planned: roster grouping by character and drift addressed to a character (slices 6–7), and scheduled
janitor and proctor work (slice 8). The handoff is a context, not a transport track. Requester and
performer remain separate; a performer never changes credentials or rewrites existing asks and block authors.

## The type melts into the character

Decided by Amy, 2026-11-03 and 2026-11-04: "yeah A all the way". "having banto absorb director will be nice, no
more dichotomy". "yep, one coder character". A kind never brings a model and only characters do; a person is a
root character; a person's contexts are all alike; the judge is a character; a character names its rc directory.
Shipped character directories are templates that `kj character create` picks up, and bootstrap runs the creates:
"yeah that's perfect".

A context has no type. What a context does comes from the character that performs it: the character's rc shapes
the context at each lifecycle verb, the character's gate layers govern its statements, and the character's model
answers its turns. A context with no performer runs no rc and no turns.

### What a context and a character carry

- **A context**: `performer`, `senpai_ctx`, `persona`, `forked_from`, label, and its copied `context_slots`.
- **A character's sheet**: name, the `root` flag, `rc_dir`, its model, its `distill` and `gate` slots, and pointers:
  `root_ctx`, `console_ctx`, `mcp_hub_ctx` (root characters) and `handoff_ctx`.
- **`rc_dir`** names the directory under `/config/rc/` whose lifecycle scripts the character's contexts run. It
  defaults to the character's name, and a root character's to `person`. `kj character create <name> --rc <dir>`
  and `kj character set <name> --rc <dir>` point it at another directory, so two
  characters can share one: a second coder on another model is `kj character create coder-opus --rc coder --model
  anthropic/claude-opus-5-5`. A create whose directory has no runnable `create` script refuses, naming the
  directory. `lib/` is reserved. A character's stance is in its directory, such as `banto/create/S00-stance.md`.
- **The model** is the character's one model. `kj character create <name> --model <backend>/<model>` and `kj
  character model set <name> <backend>/<model>` take it as one value, the way `kj character show` prints it. The
  value splits at its first `/`, so a model id may hold more (`openrouter/deepseek/deepseek-chat`), and a backend
  name may not hold one. A root character has no model.
- **Slot roles** are `distill` (a cheaper model for `kj fork --compact` and `kj drift pull|merge`) and `gate` (the
  council server that judges the gate). A context copies the character's model under role `model`, and each slot
  under its role, when it is made. A model turn reads the context's `model` slot, and the durable turn row records
  role `model`. A slot whose backend is a council server (kind `mk`) needs no `max_tokens` and is never a chat
  model; that is decided by the backend's kind, not by the role's name.

### A person, the console, MCP sessions, the judge

- **A person** is a root character. Every context a person performs is alike: it runs no model turns, its draft
  runs as shell, and it runs the person's rc (`person/` by default). The person's root console is the one
  `characters.root_ctx` names; it has no senpai. Ctrl+Z opens another context the person performs, `kj context
  create <person>`, answering to the context it was opened from. A person may confirm their own commands in a
  context they perform. A context with no senpai must be performed by a root character, and it is that
  character's root console once the same transaction sets `root_ctx`.
- **Human-root authority** follows the human, as "Roots and rotation" records: a caller whose character is a live
  root character has it from any context. A context grants none because of what it is; the rule reads the
  caller's character only.
- **The console** is the context `characters.console_ctx` names on the person's sheet. banto performs it, and it
  answers to the person's root console. `kaijutsu-server bootstrap` creates banto and the console (`--console-as
  <name>` names another performer), and `kj character set <person> --console <ctx>` points at another. Rotating
  the console moves the pointer at the commit, as rotation moves `root_ctx` and `handoff_ctx`; archiving it clears
  the pointer in the same transaction. Only the person concludes, demotes or archives their console. A drift into
  the console waits for the person.
- **An MCP session** is performed by the connection's own character (`kaijutsu-lead`, or the one `--character`
  names): a character with no model whose rc directory is `mcp/`. The client brings its own model. The session
  answers to the person's root console, and its parent is the person's MCP hub, the performer-less context
  `characters.mcp_hub_ctx` names; archiving the hub clears the pointer. A session is a context whose parent is an
  MCP hub. Creating one under the caller's own hub needs no ask (`docs/senpai-kohai.md`, "MCP sessions").
- **The judge** is a character, `judge`, whose model is a council server (a backend of kind `mk`). The judge
  shadow of a context K is a context performed by the judge character named in K's gate layers
  (`[character.<name>.council] judge = "judge"`), with `forked_from` K and K as its senpai, and slots copied from
  the judge character. It is found as the live child of K performed by that character, and the council server is
  its `model` slot.
- **A context with no performer** (the handoff log, the MCP hub, a score, the drift queue, lost+found, cold-start
  discovery) runs no rc and no turns. Pointers find the ones the kernel needs: `handoff_ctx`, `mcp_hub_ctx`.

### rc

`load_scripts(rc_dir, verb)` reads `/config/rc/<rc_dir>/<verb>/`, where `rc_dir` is the performer's. A context
with no performer runs none. rc gets `KJ_CHARACTER` (the performer's name) and `KJ_RC_DIR`. Shared fragments stay
in `lib/` and are linked into a directory. A script that wants only some of a context's children names them by
performer: `kj context archive --children --performer "$KJ_CHARACTER"` (the companion's tick). The kernel archives
a context's judge shadows with it, in the same transaction, wherever it archives that context; no rc script does.

`person/` gives every context a person performs the person's whole loadout ("your contexts are all alike": yes,
2026-11-04); capabilities narrow focus, and a person narrows nothing for themself. `banto/` holds one stance,
`S00-stance.md`: banto's character and the console's system-administrator duties. A directory whose characters
fork or drift links the `lib/` cache scripts into its `fork/` and `drift/` buckets; `person/` runs no model and
needs none.

Shipped directories in `assets/defaults/rc/`:

| Directory | Played by |
|---|---|
| `person/` | every root character |
| `banto/` | banto |
| `coder/`, `companion/`, `musician/`, `storyteller/`, `toolie/`, `assistant/`, `council/` | the character of the same name |
| `judge/` | judge |
| `mcp/` | each MCP connection's character |

`kaijutsu-server rc reseed` names a host directory the seed no longer ships (`default/`, `director/`, `console/`,
`root/`, `shell/`), and `--force` moves it aside to `<dir>.retired-<UTC timestamp>` and deletes nothing.

### Gate

`gate.toml` layers are `[global]`, then `[rc.<dir>]` for the performer's rc directory, then `[root]` for a context
a root character performs, then `[character.<name>]` for the performer. Each later layer reads over the earlier
ones, so the narrowest section decides. A directory layer gives every character that shares a directory the same
posture, so the shipped file can cover each MCP connection's character with `[rc.mcp]` without naming it; `[root]`
covers every person; a character layer tunes one character ("Character + directory", Amy, 2026-11-04).
`[context_type.<type>]` fails the load, naming the section, as any section the kernel does not read does. A
council block moves with its layer: `[rc.coder.council]` or `[character.coder.council]`.

A `[rc.<dir>]` that names no directory in the rc tree, or a `[character.<name>]` that names no character on the
sheet, live or retired, is listed by `kj ledger rules` as applying to nobody and logged as a warning when the set of
such sections changes between loads. It does not fail the load: a failed load refuses every gated statement,
including the `kj character create` that would fix it. An rc tree that cannot be listed skips the `[rc.<dir>]` check
with a warning naming the tree. The council's case carries the performer's name, and the council contexts a context
consults are composed from its performer's layers. The console and a person's root console (the context a sheet's
`root_ctx` names) are never under a council, whatever their performer's layers say: the gate consults none for them,
raises no council-read ask for them and builds no seat for them, while the other contexts the same character plays,
banto's or a person's, are judged as their layers say, `[root.council]` included ("Never under a council", Amy,
2026-11-05, because "the root is where Amy goes when the council is failing").

Three acts always ask: archiving a console (a context a sheet's `console_ctx` names), with `kj context archive`
or the `kj context demote` step that archives it; `kj character set <name> --no-root`; and `kj character retire
<name>` when `<name>` is a person or performs a live console. The gate decides them by their target, as it decides
`kj context create` by its parent. An allow key, a learned rule or the council does not skip the ask, and a deny
key still refuses. The gate mirrors the verbs' authority refusals for the caller, not every refusal. It raises no
ask for `--no-root` or a person's retirement by a caller who is not a live person, for a console's archive by a caller who may not
archive it, or for the retirement of a character that performs any live person's console but the caller's. It
does not model the refusals that do not depend on the caller: retire's last-person refusal, a context that no
verb archives (`archive_refusal`), a held drift, a root console, or an MCP hub with live sessions. The gate can
therefore ask for an act the verb then refuses, which over-asks and is safe. `kj context archive --children`
archives the children in order and stops at the first it may not archive, so it asks naming the consoles it
archives before that child and raises no ask when another person's console comes before any console the caller
may archive; it models the console rule alone, so it can name a console the verb never reaches because it stops
first at a child it refuses for another reason. The ask is answered in the ledger like any other, and a person
answers one raised in a context they perform. Allowing it runs each `kj` statement the ask showed as confirmed,
because an approval is a stronger confirmation than `--confirm`. That includes a literal `$(kj ...)` in the
approved source, which the plan holds as its own command, so the reviewer saw it. A `kj` the plan did not hold as a
literal command (one in a file the source runs with `source`, or one with an argument that expands) is not
confirmed by the approval, and a permanent verb there latches; one that spells `--confirm` itself runs, as any
`kj` with `--confirm` does. `kj context
promote <console>` brings back an archived console. A promote whose senpai is archived refuses, naming `kj context
promote <senpai>` and `kj context set <ctx> --senpai <ctx>`. `kj character set <name> --root` makes the character
a person again, with a new root console and MCP hub; the old root console stays archived, and `kj context promote`
refuses it, naming that command. The console `--no-root` archived answers to that old root console, so the person
moves it under the new one with `kj context set <console> --senpai <root console>` and then promotes it. A
retirement is not undone. The ask catches an accident without blocking a choice (Amy,
2026-11-05). Archiving a root console still refuses, because `kj context rotate` replaces one, and retiring the
last live person refuses, as `--no-root` does.

`kj character set <name> --no-root` archives what the sheet's `root_ctx`, `console_ctx` and `mcp_hub_ctx` name and
clears the three pointers and the root flag, in one transaction. It refuses, naming them, while the character
performs any other live context or the hub has live sessions, and when the character is the last live person.
Only a live person runs it. It archives the contexts whose archive lifecycles ran: when a pointer moved after the
lifecycles, the transaction refuses, naming the move, and archives nothing. The character keeps a directory it
may run: one whose `rc_dir` is `person` or unset names another in the same call (`--rc <dir>`), because `person/`
belongs to roots: `create` and `set --rc` refuse `person` for a character that is not a root.

### Shipped characters

`kaijutsu-server bootstrap` creates each shipped character that does not exist, with its directory and the model
it was given: banto, coder, companion, musician, storyteller, toolie, assistant and council. judge is created with
no model; `kj character model set judge <council-backend>/<model>` gives it one before the council is enabled. An
existing character is left alone and named in the report, as `rc reseed` names a file it leaves alone. Bootstrap
is idempotent, so running it on an existing kernel creates only what is missing.

banto performs the console. Its other contexts are created with `kj context create banto`, and its stance reads
the performer and the senpai from context metadata.

### Composing senpai and kohai

`/config/rc/lib/recipes/` holds kaish scripts that compose characters and contexts. `pair.kai <senpai-character>
<kohai-character>` creates a context of the first under the caller and a context of the second under it. A recipe
is run on demand; no lifecycle runs one. See `docs/senpai-kohai.md`, "Composing senpai and kohai".

### Migrations

These boot migrations move an older database to this design, in this order. Each one runs once and records
itself, and copies the database aside first when it would change a row: `kernel.db.pre-rc-dir`,
`kernel.db.pre-rc-dir-rename`, `kernel.db.pre-slot-roles` and `kernel.db.pre-type-melt`.

1. **`rc-dir-backfill-v1`.** A root character gets `root`. A character with a `context_type` gets that directory.
   A character with neither gets the type most of its live contexts have, which gives an MCP lead `mcp` and a
   hand-made directory such as `bassist` the character that plays it. A character with no type and no live context
   keeps NULL, and a create for it refuses, naming `kj character set <name> --rc <dir>`. Boot never fails for this.
   The same pass sets `console_ctx` from a root's one live `console` context that answers to its root console, and
   leaves it NULL, with a warning naming them, when there are several. Characters it cannot place are listed in
   `context_character_migration_report` (kinds `rc_dir_unset` and `console_ambiguous`).
2. **`rc-dir-rename-v1`.** `rc_dir` values move to the directories that absorbed them (`director` and `console` to
   `banto`, `root` and `shell` to `person` for a root character, `default` to `coder`). `person/` belongs to roots,
   so a character that is not a root and is on `root` or `shell` gets NULL instead, its report says so and names
   `kj character set <name> --rc <dir>`. Until a directory is set, no rc lifecycle of a context it performs
   runs, so a create for it refuses and so does `kj context archive` of a context it already performs. Setting a
   directory clears the character's `rc_dir_unset` and `rc_dir_renamed` reports.
3. **`slot-roles-v1`.** In each `context_slots` row the role that is the context's type becomes `model`; `judge` and
   other type-keyed rows are dropped. `character_slots` keeps `distill` and `gate`, and a character with no model takes its own-type slot as
   its model. The migration drops and
   recreates the never-changed triggers inside its own transaction; no verb changes a slot. Live judge shadows are
   archived, and the next gate call makes one performed by judge; archived shadows stay archived as ordinary forks.
4. **`type-melt-v1`.** `contexts.context_type` and `characters.context_type` are dropped, with their
   schema defaults, so a fresh database never creates them. The approval ledger's `rc_runs.context_type` is
   renamed `rc_dir`; an old row keeps the name the directory had when it ran.

Earlier migrations that read `context_type` still run on an old database before these, and a fresh database
never needs them; both paths are tested.

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
lineage of interactions**. In kaijutsu that lineage is made of contexts, the
model they run on, and the rc directory that shapes them — and the character
is the sheet that holds them together and points outward at the rest.

Amy's word for the shape is a D&D character sheet (guidance, 2026-09-06):
a page of stats and pointers that is not itself the lore, but that
references out to deeper lore. Read the design that way. The sheet is
small and mostly foreign keys; what it names — the rc directory, the
handoff log, the root context — is where the depth
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
  drift address. The word fits the stage: a character is what an actor
  plays, and a context is one performance.
- **A character has one model and two slots.** `kj character model set <name> <backend>/<model>` sets the model,
  and `kj character slot set <name> distill|gate <backend>/<model>` a slot (`unset` removes one, `list` shows them,
  `kj character show` prints them). A context's model comes from its own `model` slot, copied from its performer's
  model when the context is made. A `gate` slot names the council server its gate decisions run on
  (`docs/council.md`, "Configuration"), and a `distill` slot runs its forks' and pulls' distillation. The judge is
  a character of its own (`docs/council.md`, "Shadow context").
- **Accountability replaces any group concept.** Every model character
  answers to a character above it; a human is a root. No party. The relation is
  between contexts, not between sheets: see "Roots and rotation".
- **A character names its rc directory** (Amy, 2026-11-04). Its contexts run that directory, and several
  characters may share one ("The type melts into the character"). "Chair" is not a term.
- **Presence is kept and derived; the roster inverts onto characters.** Many
  clients per character: Amy is connected 2–5 ways a day from several
  machines. Never associate one client with one character. "Seat" is the
  council's word only (`docs/writing.md`, Terms).
- **A context that wants a human raises an ask.** No availability field for
  now; an escalation-triage character will answer some asks by policy later.
- **The night shift is a janitor first.** Cleanup and archiving. The proctor
  sweep is its second job. Compaction watchers later.
- **Deferred, not dropped:** the saifu 財布 (a per-character purse with
  several currencies: tokens, dollars, megabytes, a house scrip) and an
  availability field.
- **Kaijutsu stays bespoke** for Amy and the fleet. Open source, so who knows.
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

## Roots and rotation (guidance, Amy, 2026-09-15; senpai edge 2026-10-18; persons 2026-11-04)

- **Accountability is a relation between performances, not between
  sheets.** A context answers to its **senpai**, another context
  (`contexts.senpai_ctx`); the senpai's character
  reviews its work. A coder lane banto creates answers to that banto
  context, not to "banto" in general, and two bantos never collide. There is no
  `accountable_to` on the character sheet; Amy: "accountable_to is a
  relation at runtime, so if banto forks a coder, that coder is accountable
  to the precise banto that forked it, not any banto."
- **A root context has no `forked_from`.** A root console is a root of the
  forest, and `kj context create` always names a parent
  (`docs/fork-and-create.md`, "Where a created context sits"). Its
  responsible character is a root character.
- **A root character has no model.** "A kind never brings a model; only
  characters do", and a person's contexts are all alike (Amy, 2026-11-04).
  A root is a person's hands: its contexts run no model turns, and a model
  reaches the person through the console or through an MCP session whose
  client brings its own model (`docs/senpai-kohai.md`, "MCP sessions").
- **The root console and its shell.** Amy, 2026-10-25 and 2026-11-04. A
  person's root console has no model and composes no stance; `person/` gives
  it the operator's authority set, minus `drive`. Nobody sits above a root
  console to ask, so a person's own confirmation is the human's act, there
  and in every other context the person performs. A turn still refuses a
  performer that is its own reviewer. A chat submit in a context a person
  performs is refused before the self-review check, with a reason that says
  its draft runs as shell and points to the console
  (`runtime/turn_identity.rs`, `person_refusal`); clients run that draft as
  shell. The model console is the context the person's `console_ctx` names,
  labeled `console` by bootstrap, answering to the root console. banto
  performs it. Its stance (`banto/create/S00-stance.md`)
  names its privileged position and asks for care. Its `kj` requests go
  through the regular approval policy, and its reviewer is the person above
  it. Ctrl-Z in the console, or in the root console, opens a context the
  person performs under it (`kj context create <person>`), with no model, and
  the person confirms their own commands there as in their root console. Its
  output stays in its own log and reaches a model only by fork or drift.
  Ctrl-Z there returns to the context it was opened from.
- **A drift into the console waits for the human.** Amy, 2026-10-28. A drift into
  the console lands held: a `Drift` block that is `Pending` and
  excluded, so no model reads it, and no running turn is offered it. Every
  drift kind holds except `Notification`. The drift's `rc` scripts run as
  for any drift. An ask with origin `drift` waits in the approval ledger:
  the sender is its performer, and its reviewer is whoever reviews the
  console's performer, which is the person above it. Its description
  names the sender's context, the drift kind, and the text. The ask card,
  the compose pause, the ledger view, and the desktop notice treat it like
  any other ask. `kj ledger allow <id>` releases the drift: the approval
  worker makes the block `Done` and included and offers it to a running
  turn, so it joins at the next tool round, or else it rides the next
  submit. `kj ledger deny <id>`, or the sender's `kj ledger cancel <id>`,
  dismisses it: `Done`, still excluded, kept in the log. A drift ask takes
  no `--remember`: the kernel refuses the answer and the ask stays open
  (`kj/drift.rs`, `settle_drift_answer`; `kj/ledger.rs`, `decide_ask`).
  - Only a human answers it. A drift is refused, before anything is
    written, when the console's reviewer is not a live root character.
  - A drift ask survives a restart; every other unresolved ask is retired.
    At start, a held drift with no open or unsettled ask is dismissed
    (`reconcile_root_held_drifts`).
  - The console is not archived, rotated, or demoted past the rim while a drift
    ask is open or its answer is unsettled. Each verb refuses before it
    runs anything, naming the asks.
  - `kj stage include` and `setBlockExcluded` refuse a held drift; only the
    ask's allow includes it. A held `pull` or `merge` tells its caller the
    ask id and shows no preview.
- **The console is a pointer, not a type or a label.** The drift hold, the
  placement authority and the client's start choice read the person's
  `console_ctx`. The label `console` is the name bootstrap gives it; a
  context labeled `console` that no sheet names is not the console.
- **The console places contexts.** Amy, 2026-10-28: "I think we are simplifying
  on having the console be the privileged context for getting stuff moving. A
  banto/director should have privileges to drive other contexts, and fall
  back to the console when it needs to do something on itself. This gives me
  one place for things to bubble down to me." `kj context promote`,
  `demote`, and `conclude` follow this rule (`kj/context.rs`,
  `require_placement_authority`). Asked where a person's own authority
  comes from, Amy chose "It follows the human": a live root character (a
  sheet with `root` set, not retired) is a human root wherever it acts, so
  its keys work in any client on any card, and it needs no capability. A
  context grants nothing because of what it is. A caller in the console, the
  context a sheet's `console_ctx` names, needs the operator capability.
  - The console may promote, demote and conclude any context, but may not
    demote or conclude a root console or the console.
  - Only a person demotes or concludes a root console, and only the person
    whose console it is demotes or concludes the console while that person is
    a live root. Nobody concludes a root console: conclusion has no undo, and
    a concluded context refuses drifts, which is how work reaches the human.
    A root console that wants fresh history rotates.
  - Any other caller with Operator may act on any other context in the house,
    in any lineage. It may not act on a root console, a console, or its own
    context. Its own context is the one the call runs in, or any context its
    character plays. The refusal says to ask the console, for example with
    `kj drift push console`.
  - A caller without Operator acts on nothing. The refusal names the
    capability and says to ask the console.

  `kj context archive` and `kj context rotate` follow the same principle:
  each needs Operator, or a live root character acting from any context,
  and archive always latches. Rotate then asks the performer or lineage
  root, as before. `kj drive` keeps its own rule: it needs the drive
  capability on any target, including the caller's own context.
- **Rotation replaces a conversation, not a character.** Amy, 2026-09-17,
  and `docs/senpai-kohai.md`, "Creating, forking, rotating": the successor is a new context
  with the same character and senpai, its slots read fresh from the
  character's own. It copies env and cwd, sets `ROTATED_FROM`, and takes the
  label and ring position as the predecessor archives. The kohai
  point at the context, so they do not move. A root console rotates too,
  when its history goes stale; a person confirms themself, so no one else is
  asked. The performer, the senpai's character, or the lineage root may
  rotate.
- **banto starts from the root console.** The console banto performs answers to
  Amy's root console, and so does a banto context created there, which takes the
  character's name as its label when no live context holds it.
- **The reviewer is the senpai's character, climbing past the actor.**
  Resolution takes the actor and the context: the character of the context's
  senpai, or, when that character is the actor, the character of
  the senpai's senpai, and so on. A root console self-confirms. There is no configured
  default and no override. See `docs/approval-identity.md`, "Routing".
- **A context's character never changes.** The senpai's
  character, or the lineage root, starts another context of the character that
  should do the work (`kj context create <character>`).
- **A model rotates itself with `kj context rotate`.** Amy, 2026-09-17:
  the performer may rotate its own context, so no ask is needed.
  `kj context create <character>` follows the same shape: Operator and a live
  character caller, with no ask.

### A session, inside kaijutsu

How a day like 2026-09-15 runs once the pieces above exist. Amy gives
prompts, banto supervises lanes, and the lanes are contexts.

1. Amy works in the console, which banto performs. She runs `kj` by hand in
   her root console or in a context `Ctrl+Z` opens, where the draft runs as
   shell. In the console the draft is hers, submit authors a user block as
   amy, and the turn runs with banto as actor and amy as reviewer.
2. banto creates lanes with `kj context create coder --name <lane>`,
   each with a label, a territory, and a worktree under `~/src/wt/`.
3. Lanes run as autonomous turns. banto drives and waits. A lane's gated
   statement goes to banto, its senpai's character; banto's own, such as a
   commit, goes to amy. Static allow tiers pass the routine ones; the rest reach Amy's asks
   pane.
4. Review is a tool call: kaibo mounts as an MCP server in banto's loadout,
   and the review is a tool-result block in banto's conversation, with a fix lane forked
   from it.
5. Results come back as drift into banto's conversation, distilled on its `distill` slot.
   banto re-reads every citation against the tree, then commits path-scoped
   through the gated shell.
6. Every lane stays a context: open it in the picker, scroll, copy, or exclude
   a bad turn, fork, and re-drive. `kj context info` and `kj block list -c
   <lane>` answer what a lane did a week later.
7. `kj handoff note` replaces `signoff.md`. When the conversation grows long, banto
   asks to rotate; the successor hydrates from `ROTATED_FROM` and the handoff
   tail, and the predecessor archives with its lineage intact.

Polish step 3 first: driving and waiting on lanes with disjoint territories
is where a session spends its care.

## The design

### Character = principal + sheet

No new identity type. `PrincipalId` stays on every block and on the wire.
The **sheet** is a new table in `kernel.db`, one row per principal that has
one, normalized (no JSON column; `feedback_sql_schema`).

```sql
CREATE TABLE IF NOT EXISTS characters (
    principal_id     BLOB NOT NULL PRIMARY KEY,   -- auth.db principals.id; immutable
    name             TEXT NOT NULL UNIQUE,        -- kernel-owned given name; immutable in v1
    created_at       INTEGER NOT NULL,
    retired_at       INTEGER,                     -- characters retire; never deleted
    handoff_ctx      BLOB REFERENCES contexts(context_id) ON DELETE SET NULL,
    root             INTEGER NOT NULL DEFAULT 0,  -- a person; see "Roots and rotation"
    root_ctx         BLOB REFERENCES contexts(context_id) ON DELETE SET NULL,
    rc_dir           TEXT,                        -- the directory under /config/rc/ its contexts run
    console_ctx      BLOB REFERENCES contexts(context_id) ON DELETE SET NULL,
    mcp_hub_ctx      BLOB REFERENCES contexts(context_id) ON DELETE SET NULL
);
```

Three rules make the principal a safe key:

- **`name` is the kernel's, not `auth.db`'s.** It is the drift address,
  and after the keyring melt there is no other name for it to disagree
  with — `principals.username` is gone, not demoted.
- **A principal with a character is never removed.** `auth.db` has no
  removal path ("`auth.db` is a keyring"). Retirement sets `retired_at` and,
  in the same act, concludes and archives every live context the character
  plays. A retired character therefore has no live performance to start a
  turn in; `kj drive` refuses an archived context, so the loud failure is
  the one that exists.
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
kj character create kaijutsu-lead --rc mcp              # mints the principal id + the sheet
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
character that has none, and rebinds — loudly, with a warning naming the
character and context — one that already exists but has no usable loadout,
the same repair `kj context rebind` performs. A character it cannot bind,
newly created or already existing, fails the start rather than serve an
inert root: the message names the context, the rc bucket it expected
(`/config/rc/<rc_dir>/create`, `person/create` by default), and the fix
(`kaijutsu-server rc reseed`). Every context parents under a root, so an
inert one would deadlock the whole tree beneath it.

**A root context is a model-less admin console.** Its label is the
character's name, the character plays it, and it has no parent. `person/`
grants the operator authority (`admin`, `config-write`, `operator`, `exec`,
the shell facades) and composes no instruction blocks. Contexts a root
starts, such as the console, are created from it, so their senpai is the
root console.

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

`contexts.performer` names the character that plays a context. `kj context
create <character>` sets it from its required positional, and a fork copies
it. A context with no performer (a handoff log, an MCP hub, a score) runs no
turns. The performer never changes (`docs/senpai-kohai.md`, "The model").

**Two identities ride every turn, and neither replaces the other.** The
**requester** is who caused the turn: the principal on the `KjCaller`, on the
user's prompt, on a gate's ask, and on `TurnFlow` events. `kj drive` makes
the caller's performer the driven turn's requester, and its seed carries
that performer. The **effective actor** is the character whose
performance this is, resolved once at turn start from `contexts.performer`. Who gets
stamped where:

| Block or record | Principal |
|---|---|
| capability checks, drive origin, user prompt, approval ask, `TurnFlow` | requester, unchanged |
| `kj drive` seed, and the requester of the driven turn | the driving caller's performer |
| provider-emitted thinking, model text, model tool call | effective actor |
| tool result, structured tool error, kernel warning, error and interrupt markers | `system`, unchanged |
| rc output | the context's `created_by`, unchanged (`rc/mod.rs`) |

The criterion is **provenance, provider output versus kernel output, not
role**. The max-iterations halt is `Role::Model` and kernel-generated, so it
stays `system`; the tool result is kernel-generated and stays `system`
(`llm_stream.rs:2396`). Once a model block is created under the actor, its
streaming appends use that same principal.

Two seams the audit named that slice 3 must decide, not discover: builtin
tool servers author their blocks under the requester (`mcp/servers/shell.rs:345`,
`block.rs`, `tasks.rs`, `background.rs`), so after slice 3 a model's
`ToolCall` carries the character while the tool's own output block carries
the human who drove the turn; the matrix above says tool output is kernel
output and should stamp `system`, and those servers should follow it. And
the gate-resume seed passes no principal (`rpc.rs:1316`), falling through
to the store default `system` (`rpc.rs:2431`), where `kj drive`'s seed stamps
the caller's performer; the resume seed is a requester act and should say so.

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

Clients do not infer the performer from blocks: `ContextHandleInfo.performer`
carries the character's name, and the tui divider shows it. The app keeps its authenticated session
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
  given keeps today's behavior. `kaijutsu-acp`, `kaijutsu-app`, and
  `kaijutsu-tui` accept the identical pair, resolved the identical way:
  `kaijutsu_client::key_select` owns `--key-fingerprint`/`--key-file` and
  their env fallbacks once (`KeyArgs`, `resolve_key_source`), flattened into
  each binary's own `clap::Parser` rather than declared per binary.
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
kj character create kaijutsu-lead --rc mcp                              # mints the principal id + the sheet
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
hosting pid without waiting. Still to read once: the `mcp/` directory's governance
script (`mcp/create/S15-governance.kai`) for anything keyed on the
username `amy`, and whether the kernel roster and the cc-peer roster agree
about who is in the room when one process is two names.

This is slice 1's first character row without the sheet, and the cheapest
possible test of "a model character is a principal", the question the
frontier review pushed on. It needs no kernel code. The fix to the hook
listener archiving the wrong context on `session.end` (`docs/issues.md`,
"The hook listener archives another session's context") should land first,
because a spurious archive re-runs the `mcp` create bundle and drifts the
bridge context's label.

### The handoff is an ordinary context

Each character gets a handoff log: a context with no performer, pointed at
by `characters.handoff_ctx`, created by the first `kj handoff note`, with a
persisted hydration policy so a reader takes a window (`kj context hydrate
--window N`; the window is 50, set on the first note).

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
- `note --for <character>` writes into another character's log under the
  caller's name; `tail` never creates the log, because a read-only verb
  must be incapable of a write; `note` needs no `ConfigWrite`, since leaving
  a note is ordinary authoring. `S16-handoff.kai` guards `kj handoff tail`
  inside the substitution; the kernel test
  `set_e_substitution_guards_behave_as_rc_scripts_place_them` pins the
  placement.

The first draft of this design put the handoff on a **track's score
context**, by analogy with the musician's page-turn. The frontier review
moved it, and the argument holds: a score context is reconstructed as ABC
cells (`kaijutsu-server/src/beat.rs`), a track's durable row is clock
state (period, phrase, playhead, wakeup, rotate),
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

A character (`yakin`, 夜勤) on a local model, woken by a system-clock track,
its context answering to a context kaijutsu-lead performs. Its `tick` rc:

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

Distillation runs on the caller's `distill` slot, and a context with none
refuses, naming the character and the fix (`docs/senpai-kohai.md`, "Where a
model comes from").

The proctor is last on purpose. It depends on reliable liveness, consent to
external drive, the warm/cold cache policy, archival semantics, and the
requester/actor split above. `kj drive` already refuses inappropriate
context states and can target a context other than the authorizing caller;
that is infrastructure, not the policy.

## Rollout, smallest first

Each slice is independently shippable and leaves the tree green. The story of
each shipped slice is in `docs/devlog.md` and git.

0. **Terms, docs and prework.** Done 2026-09-05: the `PrincipalId` consumer
   audit, one `actor_principal` binding through the turn path, push and pull
   on one resolver, the hook-listener `session.end` guard, the roster's
   periodic refresh, and the bridge identity above.
1. **The sheet.** Shipped 2026-09-06: `characters`, `kj character
   create|list|show|retire`, where `create` mints the principal id and
   `retire` concludes and archives every context the character plays.
2. **The keyring melt.** Shipped 2026-09-06, migrated on zorak 2026-09-07
   ("`auth.db` is a keyring"). The server refuses to start on a pre-melt
   `auth.db` (`tests/auth_db_premelt_guard.rs`). `init` replaced the seeded
   `hajime` and anonymous auth on 2026-09-16.
3. **Attribution.** Shipped: `turn_identity::resolve`
   (`crates/kaijutsu-kernel/src/runtime/turn_identity.rs`) resolves the
   effective actor, and provider-emitted blocks are authored by it
   (`runtime/llm_stream.rs`).
4. **The handoff log.** Shipped 2026-09-07 ("The handoff is an ordinary
   context").
5. **The type melts into the character.** Shipped 2026-11-05 ("The type melts
   into the character"). The zorak deploy is open in `docs/issues.md`.
6. **Roster inversion.** `kj roster` groups rows by character, presence
   specified as the aggregation above. The periodic refresh is already wired
   (`roster_sources.rs`).
7. **Drift to a character.** `@name` addressing; one resolver.
8. **Janitor, then proctor.** The `yakin` character, its track, its tick rc.

Deferred: saifu and availability until a reader exists. A character has no
memory field: memory is planned as tool calls and inference-side work.
Not doing: party, chair, seat, any reputation or trust score
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

- **The gate's own model setup.** A character's `gate` slot names the
  council server its contexts' gate decisions run on; a setup of the gate's
  own is open (`docs/gate-policy-tuning.md`, "Senpai, the gate's model, and
  the banto tier").
- **Socket-protocol liveness** for driving a Claude Code session
  (`docs/cc-peer.md`).
- **The desk character** (受付, Uketsuke): an automode context that answers
  some asks by policy and escalates fewer to Amy. `docs/issues.md` "The
  escalation seat" is the earlier sketch of the same idea.
