# RC, `/config`, and system prompt assembly

Every context is shaped by its performer's rc directory (`/config/rc/<rc_dir>/`)
and the kernel configuration mounted under `/config`.

## `/config` and mounts

Config is ordinary host files reached through `LocalBackend`:

- **Four sibling mounts:** `/config/rc`, `/config/kernel`, `/config/client`,
  and `/config/midi` are sibling mount points in the kernel mount table
  (`mounts.toml`, `--config-root`, or `--mount`). `/config` itself has no
  backing store, and the host's `/etc` remains a plain read-only host path.
- **Seeds and reseeding (`assets/defaults/`):** Shipped defaults are embedded
  at compile time from `assets/defaults/`. On a fresh tree, boot seeds missing
  files. When deploying changes to `assets/defaults/rc/`, materialize them onto
  the host with `kaijutsu-server rc reseed [--force]`. Without `--force`,
  reseed installs new files and reports any host files that differ from the
  seed without overwriting them.
- **Local kernel config (`/config/kernel`):** Holds `gate.toml`, `mcp.toml`,
  `theme.toml`, `distillation.md`, and `continuation.md`. Because local config
  points at host secrets and local endpoints, `rc reseed` never overwrites
  `/config/kernel`. Use `kj config list|show|reset` to inspect or explicitly
  restore embedded defaults.

## RC directories and lifecycle verbs

A context has no type; it runs the lifecycle scripts in its performer's
`rc_dir` (`/config/rc/<rc_dir>/<verb>/`). A context with no performer runs no
rc.

### Shipped directories (`assets/defaults/rc/`)
Each top-level directory except `lib/`, `mcp/`, and `person/` ships a
character of the same name, which `kaijutsu-server bootstrap` creates when it
is missing (`seed_scripts::shipped_characters`). Adding a directory ships a
character.

- `person/`: root characters (persons). Grants operator and house capabilities
  without `drive`; runs no model turns.
- `banto/`, `coder/`, `companion/`, `storyteller/`, `musician/`, `toolie/`,
  `assistant/`, `council/`, `sol/`: model characters of the same name. Sol's
  stance is written for GPT-6.2, and bootstrap gives it the same model as the
  others. On any other model, sol's `S12-model-check.kai` leaves a
  notification naming the commands that set up a `chatgpt` backend, give sol
  `chatgpt/gpt-6.2-sol`, and rotate the context onto it.
- `judge/`: council judge shadow character (runs no chat turns and binds no
  tool loadout).
- `mcp/`: characters for external MCP bridge sessions (`kaijutsu-lead`).
- `lib/`: shared script bodies (`create/`, `fork/`, `drift/`, `submit/`,
  `archive/`, `hooks/`). Character directories link to `/config/rc/lib/...` via
  symlinks so a single edit in `lib/` reaches every character that shares it.

### Lifecycle verbs and execution order
The kernel runs `/config/rc/<rc_dir>/<verb>/SXX-name.kai` in lexical order
(`S00` through `S99`) on these lifecycle events:

| Verb | When it runs | Typical work |
|---|---|---|
| `create` | `kj context create` or bootstrap | Author stance (`S00-stance.kai`), bind loadout (`S10-binding.kai`), seed handoff/predecessor/datetime/git orientation (`S16`–`S36`), install shell guard (`S45`), build council seats (`S50`). |
| `fork` | `kj fork` | Bind loadout (`S10`), refresh datetime (`S20`), mark cache breakpoint (`S40`), install shell guard (`S45`), build council seats (`S50`). |
| `attach` | `kj attach` | Per-attach hooks on the target context. |
| `drift` | `kj drift push` / `flush` / `pull` / `merge` | Prompt-cache maintenance (`S40-cache.kai`) or opt-in turn wake. |
| `submit` | Before a user prompt turn | Per-turn notifications (e.g., companion one-turn handoff view, storyteller reminders). |
| `tick` | Beat scheduler tick on an attached track | Musician phrase drive (`S10-drive.kai`) or companion check-in/rotation. |
| `rotate` | Beat scheduler page-turn | Periodic context rotation. |
| `archive` | Before `kj context archive` or `kj character retire` commits | Conclude and archive child contexts answered to this context (`S90-children.kai`). |

### `.kai` scripts vs. `.md` companion data
- Only `SXX-name.kai` files are executed. Before running a verb, the kernel
  snapshots the `.kai` programs and records a durable run in `rc_runs` and
  `rc_script_runs`.
- `.md` files are data, never executed directly. Stance and primer scripts read
  their companion `.md` file at execution time:
  ```sh
  cat "${0%.kai}.md" | kj context instruction add
  ```
- Scripts receive `KJ_CHARACTER` (performer name), `KJ_RC_DIR`, context
  environment variables (`kj context env`), `ROTATED_FROM` (on rotation), and
  verb-specific variables (`KJ_PARENT_BLOCK_COUNT` on `fork`, submit/tick
  facts).

## System prompt assembly (`llm/system_prompt.rs`)

On every model turn, the kernel assembles the system prompt fresh from two
parts:

1. **Runtime `<situation>` block:** Rendered from live kernel state
   (`SituationalContext::to_xml_addendum`), never stored as a block:
   ```xml
   <situation>
     <context id="019abc..." label="lead" state="live"/>
     <performer id="019def..." name="banto"/>
     <senpai id="019000..." label="amy" character="amy"/>
     <cwd>/home/atobey/src/kaijutsu</cwd>
     <model provider="deepseek" id="deepseek-chat"/>
   </situation>
   ```
2. **Stored system instruction blocks:** Every non-excluded, non-ephemeral,
   non-draft block in the context with `Role::System` and `BlockKind::Text`,
   joined with `\n\n` in document order.

### Stance register and live edits
- Shipped stances (`S00-stance.md`) are written in plain ASCII STE using
  inclusive `we`, naming accountability to the person we work with, and closing
  with `頑張って` (for `coder`, `assistant`, and `companion`).
- `kj context instruction list|show|add|edit` and `kj stage exclude <id>`
  modify the stored instruction blocks of a live context and take effect on its
  next turn.
- Editing a file under `/config/rc/` changes future lifecycle runs and newly
  created or rotated contexts; it does not rewrite instruction blocks already
  stored in existing contexts.

## Kaish execution and hooks

- **`EmbeddedKaish` (`runtime/embedded_kaish.rs`):** Wraps the kaish kernel per
  context, wiring VFS mounts, file buffer cache, per-context `JobManager`,
  output preview limits (`PREVIEW_LIMIT_BYTES`), `HOME`/`PATH`, and W3C trace
  context.
- **Host execution policy (`runtime/context_shell.rs`):** Controlled by the
  context's loadout. Contexts with the `Exec` capability run with
  `ExternalExec::Allow`; read-only contexts (`toolie`, `musician`) run with
  `ExternalExec::LookupOnly` and a builtin read-only `git` tool.
- **Shell escape guard (`S45-shell-guard.kai`):** Every shipped character that
  grants `Exec` installs the `shell-escape-guard` `pre_call` hook on
  `shell_write`. Its hook body lives at `/config/rc/lib/hooks/shell-guard.kai`
  and is read fresh from `/config` on each call, inspecting `KJ_TOOL_PLAN` to
  refuse unnecessary `sh -c` / `bash -c` wrappers and suggest native kaish or
  `kj` commands. If the context's working directory no longer exists, the
  shell call returns an error instead of opening an ask. The kernel moves the
  context to the nearest existing parent below `/`, or clears the working
  directory if no such parent exists. The refused command does not run.
