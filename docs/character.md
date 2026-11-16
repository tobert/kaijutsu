# Character — the persistent someone a name resolves to

Design and current behavior. Built: the sheet, the keyring, attribution, the handoff log, and the type melt (slices 0–5). Planned: slices 6–8. The review-routing and slot parts follow `docs/senpai-kohai.md`. Amy's statements are guidance, not rulings.

## Current implementation

Read this section and "The type melts into the character" for current behavior; the sections after them hold the reasons and the design invariants. `AGENTS.md` links here so the documentation remains accurate.

| Part | Contract |
|---|---|
| Identity and sheet | `PrincipalId`, kernel-owned name, creation/retirement timestamps, optional `handoff_ctx`, a `root` flag, `root_ctx`, `rc_dir` (the directory under `/config/rc/` its contexts run), and for a root the `console_ctx` and `mcp_hub_ctx` pointers; `kj character create [--root] [--rc <dir>]\|list\|show\|set --root\|--no-root\|--rc <dir>\|--console <ctx>\|--no-console\|retire`. A character has no type. `--rc` takes a top-level rc directory with a runnable `create` script, never `lib`, and `person` only for a root (`create` and `set --rc` refuse it otherwise, naming `--root`); the default is the character's name, or `person` for a root. |
| Bootstrap | `kaijutsu-server bootstrap` runs `init`, creates the shipped characters that are missing, and runs the `kj` verbs that give the console a model (`docs/server-cli.md`, "bootstrap"). `kaijutsu-server init --as <name> --key <pubkey-file>` alone creates the first root character and binds its key, with the service stopped. The server refuses to start without a live root character. There is no anonymous auth. |
| Root context | Each live root character has one, the context `root_ctx` names: labeled with the character's name, played by it, with no parent and no senpai (a root console). `kj character create --root` and `set --root` create it; the server creates any missing one at start, and archives a live context a live root performs with no senpai that no `root_ctx` names (a rotation that never committed). A rotation moves `root_ctx` to the successor before it archives the predecessor, and a retirement clears it before it archives. |
| Person | A person is a root character. Every context a person performs is alike: it runs no model turns (a turn refuses naming the person, and so do `kj drive` and `kj fork --prompt`), its compose draft runs as shell, and the person may confirm their own commands there, so `kj context create <person>` skips the self-review check. Only a live root performs a context with no senpai. A person holds human-root authority from any context. Only a live person retires a person. |
| Console | The console is the context a root's `console_ctx` names, whatever its label. A caller in it may promote, demote and conclude any context, but not demote or conclude a root console or the console. Only the person whose console it is concludes, demotes or archives it while that person is a live root. Archiving clears `console_ctx` in the same transaction; a rotation moves the pointer at the commit. |
| Wire facts | `ContextHandleInfo` carries `performer`, `performedByPerson`, `isRootConsole`, `isConsole` and `isMcpSession`, computed in the kernel (`KernelDb::context_facts`) and decoded into `kaijutsu_client::ContextInfo`. `contextType` stays declared and the kernel leaves it empty; `ContextInfo` has no type. Clients decide from the facts: `Ctrl+Z`, start ranking, tui shell-draft handling, and MCP self-archive. |
| Credentials | `auth.db` binds fingerprints to principals; `add-key --as <character>` binds to an existing character. |
| Context | A context carries its performer, senpai and persona (`contexts.performer`, `senpai_ctx`, `persona`) and a copy of the model slots it runs on (`docs/senpai-kohai.md`). `kj context create <character> [--name <label>]` needs the Operator capability and a live character caller, requires the character, takes the label from `--name` or the character's name, runs the character's rc directory, refuses an unknown or retired character, and preserves the requester's `created_by`. |
| Gate layers | `gate.toml` reads `[global]`, then `[rc.<dir>]` for the performer's `rc_dir`, then `[root]` when a root character performs the context, then `[character.<name>]` for the performer; each later one reads over the earlier, so the narrowest decides. |
| Senpai edge | Every context answers to one senpai context (`contexts.senpai_ctx`), except a root console; a context with no senpai must be performed by a live root character; the senpai's character reviews the context's asks, receives its reports, and holds authority over it. |
| Model | A character has one model (`character_models`), set with `kj character create <name> --model <backend>/<model>` or `kj character model set <name> <backend>/<model>`, `unset` or `show`. It may also hold a `distill`, a `gate`, and an `image` slot (`character_slots`, `kj character slot set\|unset\|list`). A context copies them into `context_slots` when made. A root runs no model. A slot whose backend is a council server is a council server: it needs no `max_tokens`. |
| Approval | Asks snapshot performer and reviewer. Only that reviewer may decide; the performer cannot approve from any context, except a person's self-confirmation in a context they perform. |
| Standing rules | `kj ledger allow <id> --remember character [--family]` learns a rule bound to the ask's performer that applies wherever it performs. |
| Retirement | `kj character retire <name>` concludes and archives the live contexts the character performs, judge shadows included, retires it, and clears the sheet's `root_ctx`, `console_ctx` and `mcp_hub_ctx`, in one transaction. |
| Handoff | Ordinary context referenced by `handoff_ctx`, created on the first note; `tail` never creates it. |
| Rc | A context runs its performer's directory, `/config/rc/<rc_dir>/<verb>/`; a context with no performer runs none, and a performer with no `rc_dir` fails the lifecycle naming `kj character set <name> --rc <dir>`. Scripts read `KJ_CHARACTER` and `KJ_RC_DIR`. |
| Rotation | `kj context rotate [<context>]` creates a successor with the same character and the predecessor's senpai, slots read fresh from the character's own, copying env and cwd, setting `ROTATED_FROM` to the predecessor's id, and running `create` rc. |

## The type melts into the character

Decided by Amy, 2026-11-03 and 2026-11-04: "yeah A all the way". "having banto absorb director will be nice, no more dichotomy". "yep, one coder character". A kind never brings a model and only characters do; a person is a root character; a person's contexts are all alike; the judge is a character; a character names its rc directory. Shipped character directories are templates that `kj character create` picks up, and bootstrap runs the creates.

A context has no type. What a context does comes from the character that performs it: the character's rc shapes the context at each lifecycle verb, the character's gate layers govern its statements, and the character's model answers its turns. A context with no performer runs no rc and no turns.

### What a context and a character carry
- **A context**: `performer`, `senpai_ctx`, `persona`, `forked_from`, label, and its copied `context_slots`.
- **A character's sheet**: name, the `root` flag, `rc_dir`, model, its `distill` and `gate` slots, and pointers: `root_ctx`, `console_ctx`, `mcp_hub_ctx` (root characters) and `handoff_ctx`.
- **`rc_dir`**: names the directory under `/config/rc/` whose lifecycle scripts the character's contexts run. It defaults to the character's name, and a root character's to `person`. `kj character create <name> --rc <dir>` and `kj character set <name> --rc <dir>` point it at another directory.
- **The model and slots**: one model plus `distill`, `gate`, and `image` slots.

### A person, the console, MCP sessions, the judge
- **A person**: root character. Every context a person performs is alike: it runs no model turns, its draft runs as shell, and it runs the person's rc (`person/` by default). The person's root console is the one `root_ctx` names; it has no senpai.
- **The console**: context `console_ctx` names on the person's sheet. banto performs it, answering to the person's root console.
- **An MCP session**: performed by the connection's own character (`kaijutsu-lead`, or `--character` names): a character with no model whose rc directory is `mcp/`. The client brings its own model.
- **The judge**: character, `judge`, whose model is a council server. The judge shadow of a context K is a context the gate makes, performed by the judge character named in K's gate layers, recorded in `context_shadows` (kind `judge`).

### rc
`load_scripts(rc_dir, verb)` reads `/config/rc/<rc_dir>/<verb>/`. rc gets `KJ_CHARACTER` (the performer's name) and `KJ_RC_DIR`. Shared fragments stay in `lib/` and are linked into a directory.

Shipped directories in `assets/defaults/rc/`:
- `person/`: every root character
- `banto/`: banto
- `coder/`, `companion/`, `musician/`, `storyteller/`, `toolie/`, `assistant/`, `council/`: the character of the same name
- `judge/`: judge
- `mcp/`: each MCP connection's character

### Gate
`gate.toml` layers are `[global]`, then `[rc.<dir>]` for the performer's rc directory, then `[root]` for a root character, then `[character.<name>]` for the performer. Each later layer reads over the earlier ones, so the narrowest section decides.

### Migrations
Boot migrations move an older database to this design: `rc-dir-backfill-v1`, `rc-dir-rename-v1`, `slot-roles-v1`, `type-melt-v1`, `council-backends-v1`, and `context-shadows-v1`. Each runs once and records itself, copying the database aside first.

## What a character is

A character is the collection of things that makes up **one continuous lineage of interactions**. In kaijutsu that lineage is made of contexts, the model they run on, and the rc directory that shapes them — and the character is the sheet that holds them together and points outward at the rest.

Amy's word for the sheet is a D&D character sheet: a page of stats and pointers that is not itself the lore, but references out to deeper lore.
- **The sheet is pointers, not content.** If a field wants to hold prose, policy, or a list, it wants to be a context or a directory the sheet points at instead.
- **The lineage is the point, not the row.** A character outlives any one context; contexts are performances of it.

### The bridge identity: a key per model character
`credentials` is keyed by fingerprint (`auth_db.rs:44`), so the string that selects a key out of the agent is the string the kernel uses to find the principal. `kaijutsu-mcp` gains `--key-fingerprint` and `--key-file` to connect as a named model character rather than defaulting to `amy`.

```sh
ssh-keygen -t ed25519 -N "" -C "kaijutsu-lead" -f ~/.ssh/kaijutsu-lead
kj character create kaijutsu-lead --rc mcp
kaijutsu-server add-key ~/.ssh/kaijutsu-lead.pub --as kaijutsu-lead
ssh-add ~/.ssh/kaijutsu-lead
```

### The handoff is an ordinary context
Each character gets a handoff log: a context with no performer, pointed at by `handoff_ctx`, created by the first `kj handoff note`.
- `kj handoff note "…"` appends one block to the caller's character's handoff context.
- `kj handoff tail [--window N] [<character>]` reads the last N notes.
- `create` rc gains `S16-handoff.kai`: inject `kj handoff tail --window 12` as a `Notification` block.

### Bootstrap: the person creates themself
`kaijutsu-server init --as <name> --key <pubkey-file>` creates a root character with a minted id. Roots are equal: 1 will be typical, more than one possible. Adding keys stays host-only, and there is no anonymous auth.

## Roots and rotation

- **Accountability is a relation between performances, not between sheets.** A context answers to its **senpai**, another context (`contexts.senpai_ctx`); the senpai's character reviews its work.
- **A root context has no `forked_from`.** A root console is a root of the forest, and `kj context create` always names a parent.
- **A root character has no model.** A root is a person's hands: its contexts run no model turns, and a model reaches the person through the console or through an MCP session.
- **The root console and its shell.** A person's root console has no model and composes no stance; `person/` gives it the operator's authority set minus `drive`. A chat submit in a context a person performs is refused before self-review with a reason saying its draft runs as shell and pointing to the console (`runtime/turn_identity.rs`, `person_refusal`).
- **A drift into the console waits for the human.** Lands held: a `Drift` block that is `Pending` and excluded, so no model reads it. `kj ledger allow <id>` releases the drift.
- **Two identities ride every turn:**
  - **Requester**: who caused the turn (`KjCaller`, user prompt, approval ask).
  - **Effective actor**: character whose performance this is (`contexts.performer`).
  - Provider output and tool calls carry the performer; tool results, kernel warnings, and error markers carry `system`.
