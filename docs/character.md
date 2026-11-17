# Character — the persistent someone a name resolves to

Design and current behavior. Built: the sheet, the keyring, attribution, the handoff log, and the type melt. `AGENTS.md` links here so documentation remains accurate.

## Current implementation

| Part | Contract |
|---|---|
| Identity and sheet | `PrincipalId`, kernel-owned name, creation/retirement timestamps, optional `handoff_ctx`, a `root` flag, `root_ctx`, `rc_dir` (directory under `/config/rc/`), and root pointers `console_ctx` and `mcp_hub_ctx`. `kj character create [--root] [--rc <dir>]|list|show|set --root|--no-root|--rc <dir>|--console <ctx>|--no-console|retire`. |
| Bootstrap | `kaijutsu-server bootstrap` runs `init`, creates missing shipped characters, and binds console models. `kaijutsu-server init --as <name> --key <pubkey-file>` creates the first root character. |
| Root context | Each live root character has one `root_ctx`: labeled with the character's name, played by it, with no parent and no senpai (root console). |
| Person | A person is a root character. Contexts performed by a person run no model turns, draft as shell, and skip self-review checks. |
| Console | Context `console_ctx` names on a person's sheet. Performed by a non-root character, answering to the person's root console. |
| Wire facts | `ContextHandleInfo` carries `performer`, `performedByPerson`, `isRootConsole`, `isConsole`, and `isMcpSession`, computed in kernel. |
| Credentials | `auth.db` binds fingerprints to principals; `add-key --as <character>` binds to an existing character. |
| Context | Carries `performer`, `senpai_ctx`, `persona`, and copied `context_slots`. Created via `kj character create <character>`. |
| Gate layers | `gate.toml` layers: `[global]`, `[rc.<dir>]`, `[root]`, and `[character.<name>]`. Narrowest decides. |
| Senpai edge | Every context answers to one senpai context (`contexts.senpai_ctx`), except a root console. |
| Model | One model (`character_models`) plus `distill`, `gate`, `image`, `audio`, `video`, `speech` slots (`character_slots`). Copied to `context_slots` at creation. |
| Approval | Asks snapshot performer and reviewer. Reviewer alone decides; performer cannot approve except self-confirmation. |
| Standing rules | `kj ledger allow <id> --remember character [--family]` binds a rule to the performer. |
| Retirement | `kj character retire <name>` concludes and archives live contexts performed by the character in one transaction. |
| Handoff | Ordinary context referenced by `handoff_ctx`, created on first note. |
| Rc | Runs `/config/rc/<rc_dir>/<verb>/`. Scripts read `KJ_CHARACTER` and `KJ_RC_DIR`. |
| Rotation | `kj context rotate [<context>]` creates a successor with the predecessor's character, senpai, slots, env, and cwd. |

## The type melts into the character

A context has no type; what it does comes from its character: the rc directory shapes its lifecycle, gate layers govern its statements, and model slots answer its turns.

- **A context**: `performer`, `senpai_ctx`, `persona`, `forked_from`, label, and copied `context_slots`.
- **A character's sheet**: name, `root` flag, `rc_dir`, model, slots (`distill`, `gate`, `image`, `audio`, `video`, `speech`), and pointers (`root_ctx`, `console_ctx`, `mcp_hub_ctx`, `handoff_ctx`).
- **rc directory**: `/config/rc/<rc_dir>/`. Defaults to character name, or `person` for roots.
- **The judge**: character `judge` whose model is a council server. Judge shadows are recorded in `context_shadows` (kind `judge`).

## What a character is

A character is one continuous lineage of interactions made of contexts, models, and rc directories. The sheet holds them together as pointers.

- The sheet is pointers, not content. Prose, policy, and lists belong in contexts or directories the sheet points at.
- The lineage is the point, not the row. A character outlives any single context; contexts are performances of it.

## Senpai and kohai

Every context answers to one senpai context (`contexts.senpai_ctx`), except a root console (`characters.root_ctx`).

- **Setting and moving the senpai edge**: Set at context creation or fork. Moved with `kj context set <ctx> --senpai <target>`. The kernel refuses cycles and archived targets.
- **What the senpai edge governs**:
  - **Review routing**: Asks climb `senpai_ctx` to the first live character reviewer.
  - **Drift permissions**: `kj drift push .senpai` is allowed in `gate.toml`.
  - **Authority**: The context's performer or any character along its senpai chain up to the lineage root may move, conclude, promote/demote, or archive the context.
- **Model slots**: `model`, `distill`, `gate`, `image`, `audio`, `video`, and `speech` live on the character (`character_models`, `character_slots`) and are copied into `context_slots` at context creation. There are no fallbacks and no mid-context recasts.

### The bridge identity: a key per model character
`credentials` is keyed by fingerprint (`auth_db.rs:44`), so the string selecting a key from the agent finds the principal. `kaijutsu-mcp` gains `--key-fingerprint` and `--key-file` to connect as a named model character rather than defaulting to `amy`.

```sh
ssh-keygen -t ed25519 -N "" -C "kaijutsu-lead" -f ~/.ssh/kaijutsu-lead
kj character create kaijutsu-lead --rc mcp
kaijutsu-server add-key ~/.ssh/kaijutsu-lead.pub --as kaijutsu-lead
ssh-add ~/.ssh/kaijutsu-lead
```

### The handoff is an ordinary context
Each character gets a handoff log: a context with no performer, pointed at by `handoff_ctx`, created by the first `kj handoff note`.
- `kj handoff note "…"` appends one block to the handoff context.
- `kj handoff tail [--window N] [<character>]` reads the last N notes.

### Bootstrap: the person creates themself
`kaijutsu-server init --as <name> --key <pubkey-file>` creates a root character with a minted id. Roots are equal: 1 is typical, more than one possible. Adding keys stays host-only, with no anonymous auth.
