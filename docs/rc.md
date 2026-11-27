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
- `senpai/`, `kohai/`: the universal core characters — `senpai` orchestrates
  and keeps state, `kohai` executes bounded work. Both split character
  activation (`S00-stance.md`) from Kaijutsu mechanics (`S05-env.md`) and link
  the shared inline model-tuning check (`S08-model-tuning.kai`).
- `banto/`, `coder/`, `companion/`, `storyteller/`, `musician/`, `toolie/`,
  `assistant/`, `council/`: other shipped model characters (`banto` and `coder`
  remain during the transition to `senpai` and `kohai`).
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
| `done` | After an autonomous turn ends | Tell the senpai how it ended and wake it (kohai `S10-notify.kai`). |

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
  facts, `KJ_DONE_STATUS`, `KJ_DONE_FEEDBACK`, `KJ_TURN_ID`, `KJ_INPUT_BLOCK`,
  and `KJ_OUTPUT_BLOCK` on `done`). The input names the block the turn started
  from. An autonomous drive with a prompt anchors its turn at that prompt.

### The `done` verb
The kernel fires `done` once for each autonomous turn that ends, with
`KJ_DONE_STATUS` set to how it ended: the model's own report from the `done`
tool (`done`, `blocked`, `gave_up`, `refused`, with its feedback in
`KJ_DONE_FEEDBACK`), or `ended`, `max_tokens`, `max_iterations`, `declined` (the
provider refused the request; no feedback, so its words stay out of the
context that hears of it), or `failed` when the turn stopped without one. A turn that never started because startup
failed reports `failed`. A cancelled turn and a turn a person prompted fire
nothing. The context stays live, so a senpai that finds the work going well
drives it again with `kj drive <kohai> --prompt …`. The kohai seed pushes the
report to `.senpai` and runs `kj drive .senpai --if-idle`: a senpai in a turn
reads the drift after its next tool round, and an idle senpai takes a turn.

## System prompt assembly (`llm/system_prompt.rs`)

On every model turn, the kernel assembles the system prompt fresh from two
parts:

1. **Runtime `<situation>` block:** Rendered from live kernel state
   (`SituationalContext::to_xml_addendum`), never stored as a block:
   ```xml
   <situation>
     <context id="019abc..." label="lead" state="live"/>
     <model provider="deepseek" name="deepseek-chat"/>
     <performer id="019def..." name="senpai"/>
     <senpai id="019000..." context="amy" character="amy"/>
   </situation>
   ```
2. **Stored system instruction blocks:** Every non-excluded, non-ephemeral,
   non-draft block in the context with `Role::System` and `BlockKind::Text`,
   joined with `\n\n` in document order.

### Stance register and live edits
- Shipped stances (`S00-stance.md`) use direct second-person (`you`) focused on
  activating the character's voice and judgment rather than crystallizing domain
  rules. Kaijutsu-specific runtime contracts live in a separate delimited block
  (`S05-env.md`) when needed, and `lib/create/S08-model-tuning.kai` provides an
  optional inline model-specific nudge at context creation.
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
- **Shell file operations.** Add `kaijutsu_kernel::shell_file=info` to the
  server's `RUST_LOG` filter to follow successful operations on mounted
  files as they finish. Records carry the operation, path, session, context,
  requester, and performer. A rename includes both paths. A redirect can
  open a file and write its output in separate operations. A program that
  opens a host file itself bypasses the shell backend.
- **Workbench diffs.** An agent's shell tool call or a recorded interactive
  command can create and update a diff while it runs. Kaish captures
  successful text writes, appends, and patches under the writable
  `~/src` mount and directories configured with `--rw-mount` or
  `[workspace].rw`. The first pending change starts a 100 ms window.
  The kernel drains pending changes when the command finishes, including
  after cancellation. Each command owns one diff block per changed path.
  Another writer's edit starts a separate section if the next write sees
  different file contents. The context change feed delivers updates and a
  reconnecting client recovers the current patch from the context snapshot.
  Each text snapshot can contain up to 256 KiB. A later uncaptured write to
  a tracked file marks the patch incomplete. Scratch files on the default
  `/tmp` mount, symlink aliases, and files opened directly by host programs
  fall outside this capture path. File renames and removals appear in the
  server's shell file trace.
  Commands keep up to 8 MiB of recent file snapshots in memory. Older
  snapshots go in a private directory under `workbench-spill` beside the
  kernel's `cas` directory. Command completion removes it. A server crash
  can leave a directory there.
- **Host execution policy (`runtime/context_shell.rs`):** Controlled by the
  context's loadout. Contexts with the `Exec` capability run with
  `ExternalExec::Allow`; read-only contexts (`toolie`, `musician`) run with
  `ExternalExec::LookupOnly` and a builtin read-only `git` tool.
- **Shell escape guard (`S45-shell-guard.kai`):** Every shipped character that
  grants `Exec` installs the `shell-escape-guard` `pre_call` hook on
  `shell`. Its hook body lives at `/config/rc/lib/hooks/shell-guard.kai`
  and is read fresh from `/config` on each call, inspecting `KJ_TOOL_PLAN` to
  refuse unnecessary `sh -c` / `bash -c` wrappers and suggest native kaish or
  `kj` commands. If the context's working directory no longer exists, the
  shell call returns an error instead of opening an ask. The kernel moves the
  context to the nearest existing parent below `/`, or clears the working
  directory if no such parent exists. The refused command does not run.
