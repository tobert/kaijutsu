# Kaish integration and rc lifecycle

Status: migration plan. The current implementation is described below; the
migration steps are not implemented by this document.

Amy's objective: "*completely* migrate kaijutsu and clean up all the call
sites." Completion includes deleting superseded entry points and correcting
comments as each caller moves. Adding a shared API without moving its callers
is an intermediate state, not completion.

Kaijutsu owns contextual execution policy and durable outcomes. Kaish owns the
language, interpreter, subprocess execution, and in-memory job machinery. Rc
owns lifecycle orchestration and uses the same kaish integration as other
consumers. A hook body stored under `/config/rc/lib/hooks/` still follows the
broker's hook protocol; its location does not make it an rc lifecycle script.

## Ownership

Keep the integration in `kaijutsu-kernel`. Establish the module boundaries
before considering a separate crate. Names below describe responsibilities;
they do not prescribe a public Rust type for every row.

| Owner | Responsibility |
|---|---|
| Kaish integration, under `runtime` | Contextual construction, VFS adapters, builtin registration, identity propagation, execution/output policy, environment seeding, tracing, timeout/cancellation, and kaish job access |
| Rc lifecycle | Discover and snapshot executable bodies, order scripts, supply lifecycle facts, enforce recursion limits, record runs, and report lifecycle failures |
| Command execution | Apply command hooks and approval state, run or resume a command, settle its outcome once, and project it into blocks, receipts, and job results |
| Broker hooks | Select hooks and interpret their protocol; use shared execution for kaish bodies without granting rc authority |
| Editor command consumer | Execute in the opener's context and consume complete stdout for `:r !cmd`; report failure without splicing a preview |
| Transport adapters | Decode requests, identify the connected player, deliver responses/events, and apply connection-local context switches |
| Kaish | Parse and evaluate programs, perform backend operations and permitted host execution, and manage execution jobs |

The runtime must support re-entry: a command can call `kj fork`, whose rc script
can call another `kj` command. Do not hold a global execution lock across that
chain or dispatch nested work onto a worker that is waiting for itself.

## Current implementation

The baseline is `19c4deac`. These are source observations, not promises that
all paths already behave alike.

- `src/lib.rs::spawn_kaish_thread` reserves the 16 MiB stack for dedicated
  threads that can enter kaish. The server's Tokio runtime also reserves it.
  The helper sets thread name and stack; it does not construct a shell or own
  its Tokio runtime. Stack tests currently live in server `rpc.rs`.
- `runtime/embedded_kaish.rs::EmbeddedKaish` wraps the kaish kernel. Its common
  constructor wires mounts, the kernel's file cache, per-context JobManager,
  output limits, host execution policy, HOME/PATH, and trace propagation.
- `kj/context_shell.rs::materialize_context_kaish_*` supplies context identity,
  builtins, environment, and cwd. Eight public variants select four profiles
  with default or explicit performer identity; they share one construction
  body. The factory's placement under `kj` hides its wider set of consumers.
- `kj/lifecycle.rs` owns rc loading, ordering, variables, recursion, run
  records, `.md` block creation, and `.kai` execution.
- Server `shell_run.rs` shares execution between interactive commands and
  approval resume, but imports result and state helpers from `rpc.rs`.
  It reconstructs job results and receipts from the output block.
- Kernel `mcp/servers/shell.rs` separately converts foreground results and
  projects asynchronous completion. Shared construction has not yet produced
  shared command settlement.
- `background_exec.rs` was removed in `8ea04fdf`. Asynchronous shell programs
  use kaish's job system with durable Kaijutsu receipts. Earlier notes claiming
  that a temporary shell cannot host work that outlives it are obsolete.

## Caller migration inventory

Each row must move to the shared integration or have a specific, documented
reason to use a lower-level kaish API. Update this table as changes land;
remove the obsolete API in the same change as its final caller.

| State | Caller or mechanism | Current location | Contract to retain |
|---|---|---|---|
| Pending | Shell construction and builtin wiring | `runtime/embedded_kaish.rs`, `kj/context_shell.rs` | One construction owner; structural read-only policy; explicit requester, performer, reviewer, session, and context |
| Pending | Dedicated threads and startup runtime | kernel `lib.rs`; server `main.rs`, `ssh.rs`, `beat.rs`, turn/resume drivers | Stack reservation, cancellation/shutdown, re-entry, and `!Send` RPC placement |
| Pending | Interactive shell submission | server `rpc.rs::execute_shell_command`, `shell_run.rs` | Draft revision consumption, command/output pair, identity, hooks, cwd/export write-back, context-switch notification |
| Pending | Streaming execute RPC | server `rpc.rs::execute` | Execution IDs, connection cancellation and concurrency rules, output subscriptions; resolve its unsupported hook substitution explicitly |
| Pending | Structured `executeKj` | server `rpc.rs::execute_kj_command` | Addressed context, structured argv, gates/latches, quiet mode, data, and shell-state write-back |
| Pending | Approval resume | server `rpc.rs` resume drivers and helpers | Original actor/reviewer, captured cwd/env, existing block pair, exactly one execution and terminal settlement |
| Pending | Model/MCP foreground and background shells | kernel `mcp/servers/shell.rs` | Read-only/writable distinction, stdin, typed rejection, job ownership, receipts, cancellation, and async completion |
| Pending | Rc lifecycle | kernel `kj/lifecycle.rs`; create/fork/attach/drift/tick/rotate/submit callers | Discovery, ordering, lifecycle facts, run records, failure visibility, recursion, and explicit rc authority |
| Pending | Hook bodies | kernel `mcp/broker.rs` | Inline snapshot versus path-read semantics, internal output profile, hook timeout, exact verdict interpretation, and no recursive command-hook application |
| Pending | Editor shell reads | kernel `kernel.rs::fetch_editor_io` | Opener identity/context, full text, and fail-before-splice behavior |
| Pending | Environment setup and approved environment restore | `EmbeddedKaish::apply_context_config`, `apply_ask_env`, server state helpers, `kj/env_snapshot.rs` | Scoped variables, exact approved inputs, shared serialization, and explicit write-back policy |
| Pending | Job/receipt readers and controllers | kernel `shell_operations.rs`, `kj/wait.rs`, `kj/context.rs`, runtime job builtins | In-memory jobs and durable receipts keep their distinct lifetimes |
| Pending | Integration backends and builtins | `runtime/*_backend.rs`, filesystem adapters, `kj_builtin`, `vi_builtin`, `curl_tool`, `ps_builtin`, synthesis | Use kaish's backend/tool interfaces directly where they implement those interfaces |
| Pending | Gate planning and parsing | `kj/gate*`, `hook_gate`, `shell_gate`, `plan_clauses`, `readonly` | Kaish remains the syntax authority; preserve clause plans and approval semantics |
| Pending | Tests and fixtures | kernel and server unit/integration tests | Production constructors for integration tests; direct engine construction only in tests of that lower-level contract |

This inventory includes support code as well as execution calls. A search for
`kaish_kernel::` will still find legitimate backend implementations, builtin
tool adapters, and syntax consumers after migration. Completion means every
remaining use has an owner; it does not require wrapping every kaish type.

## Shared execution contract

Carry identity as an explicit value: requester, performer, reviewer, session,
and addressed context remain distinct. Do not recover identity from telemetry,
an environment variable, or a connection's current context after an await.

Use named policy choices for filesystem/host execution, output consumer, and
cwd/export write-back. Read-only execution must remain structural. Rc authority
belongs to the lifecycle entry point, not a general caller-settable flag.
Preserve intentional differences between a model command, a hook body, an rc
script, and an editor read. The ordinary execution API must not automatically
apply command hooks to the hook scripts that implement those hooks.

A submitted program supplies its source, optional source path and arguments,
stdin, variable overlay, timeout, and cancellation. Use kaish's types where
they already express the required meaning. Source-path support should use its
positional-parameter API rather than synthesize `$0` through string rewriting.

Command settlement records both the executed command outcome and any hook
replacement or refusal. Keep exit status, rejection, timeout, cancellation,
truncation, structured data, content type, and spill references distinguishable.
Do not invent a command exit code for a synthetic hook result. Publish terminal
completion only after required hook processing and durable state write-back.
Build block, receipt, and job projections from that outcome; do not rebuild the
outcome by reading a partially settled block.

Preserve the distinction between parse/validation rejection (nothing ran) and
an execution fault. Persistence failure must be visible and must not report a
successful terminal result. Define recovery for an accepted operation whose
completion cannot be persisted. JobManager remains kaish's execution mechanism;
the durable operation receipt remains Kaijutsu's restart/recovery record.

## Rc Markdown: explicit instruction authoring

Replace automatic Markdown loading with `.kai` scripts that use `$0` to read
companion data and `kj` to author instruction blocks. Amy suggested removing
the `.md` feature and confirmed: "that's fine if the script uses kj." Prove
this path against the acceptance conditions below before deleting the handler.
This is the intended migration, not current behavior.

Today `load_rc_scripts` selects both `.kai` and `.md` before validating names,
snapshots their bodies before the first script runs, and sorts by the entry
filename. A symlink's name determines its order and handler. `run_md_script`
authors a `System` / `Text` / `Done` / `Markdown` block as the lifecycle owner.
Successful `.kai` stdout becomes bounded diagnostic trace text, excluded from
hydration. Printing a file therefore does not replace `.md` block creation.

The locked kaish dependency is 0.17.2 at `a9807a64078f136b7a559e8fbde562086149889f`.
Its public `Kernel::set_positional(script_name, args)` supplies `$0` and
arguments. `EmbeddedKaish` does not expose that operation, and rc currently
executes the captured body without setting the script name. No upstream API
addition is established as necessary.

The proposed replacement has these acceptance conditions:

1. Only canonical `SXX-name.kai` entries execute. Markdown files become data,
   ignored by discovery, including before script-name validation. An invalid
   `.kai` name still fails visibly.
2. `$0` is the invoked VFS entry path, including its context-type symlink name,
   not a host path or resolved target. Companion lookup is relative to that
   entry. If a shared script needs data beside its target instead, it names
   that shared path explicitly. Test both regular and symlinked entries.
3. Scripts explicitly create instruction blocks. Prove role, kind, author,
   status, content type, order, and content fidelity through the real `kj`
   path. `kj block create` currently defaults to plain text and has no content
   type flag; `--content` with command substitution also needs a trailing-
   newline and output-limit check. Choose an existing adequate block-authoring
   path or fix its missing operation before deleting `run_md_script`.
4. Executable bodies still snapshot at lifecycle start. Ordinary companion
   reads happen when the script runs. Document that change from automatic
   Markdown snapshotting; do not quietly recreate a dependency loader to hide
   it. The script digest records the executable body, while the authored block
   preserves the text read. Do not claim the digest includes transitive data.
5. Missing or unreadable required data fails the script visibly. Oversized
   input cannot become a truncated instruction block reported as successful.
   Preserve intentional optional-data behavior explicitly in each script.
6. Convert every shipped Markdown instruction and symlink, all relevant
   fixtures, filename validators, seed inspection/reseed behavior, `kj rc`
   help, and prompt-composition documentation together. Existing durable
   instruction blocks stay untouched. Host rc migration must account for
   custom files; no reseed or deployment is part of this documentation step.
7. Check rendered system instructions from newly created contexts, including
   optional shared base composition. Running the interpreter tests alone does
   not prove the model receives the same instructions.

Implement this in a separate change. Delete the handler and its special-case
tests after migrating their behavioral assertions to the explicit scripts.
If a required guarantee cannot be expressed through `kj`, identify that gap
before changing the loader.

## Implementation sequence

Each step includes tests and comment cleanup, then a focused commit. The shared
API is allowed to evolve as the next consumer supplies evidence; temporary
adapters must have a named deletion step and must not become permanent aliases.

1. **Document the boundary and inventory.** This document, architecture
   summaries, rc/prompt contracts, and the live issue plan agree about current
   behavior and the intended destination.
2. **Consolidate construction.** Move contextual construction into runtime
   ownership; introduce explicit invocation identity and named policies.
   Migrate all factory callers and remove the `materialize_context_kaish_*`
   family after the final caller moves. Preserve existing backend and builtin
   interfaces. Keep rc authority restricted to its lifecycle caller.
3. **Separate rc orchestration.** Give lifecycle discovery, execution records,
   recursion, and error/trace emission a clear module owner adjacent to runtime.
   `kj rc` remains an administration adapter. Migrate every lifecycle caller.
   Implement the Markdown replacement above in its own tested change.
4. **Consolidate command settlement.** Introduce one outcome and projection
   path, then migrate interactive, streaming, structured `kj`, model foreground,
   model background, and approved-resume callers. Remove server-to-`rpc.rs`
   helper dependencies and duplicate MCP completion logic. Preserve caller
   policies rather than erasing their differences to make parity tests pass.
5. **Finish runtime ownership.** Move headless turn execution, interruption,
   and resumption ownership into the kernel as a separate change. Keep
   connection subscriptions, Cap'n Proto capabilities, and connection-local
   notifications in the server. Migrate stack/thread setup and its tests with
   the execution owners; retain re-entry and shutdown guarantees.
6. **Delete and audit.** Remove dead constructors, forwarding aliases,
   duplicate state/result converters, obsolete fixtures, and retired comments.
   Audit every remaining kaish reference against the inventory. Update the
   architecture overview and close the issue only after all rows are resolved.

## Verification and completion

Start behavior changes with regressions that fail for the intended reason.
Cover requester/performer separation, read-only mutation/exec denial, internal
versus model output, variable isolation, cwd/export persistence, context
switching, and approved cwd/env pins. Exercise shell calls and lifecycle entry
points where users reach them.

Command parity tests cover success, rejection, execution failure, timeout,
cancellation, output spill, structured output, and PreCall/PostCall/OnError
substitution. Pin the distinction between raw command and hook outcomes. A
paused PostCall test must prove observers cannot consume a terminal result
before the final outcome is known. Include async work outliving its shell,
restart recovery, and one terminal settlement per accepted operation.

Rc tests cover every wired verb, lexical order, symlinks, source identity,
nested `kj`/rc calls, failure continuation, durable run records, and the chosen
instruction-loading contract. Hooks and editor reads have their own protocol
checks. Read emitted CLI help and tool schemas for any changed interfaces.

As each file moves, replace historical narratives and stale rollout comments
with the current invariant, responsible owner, and a contract link where
needed. Move useful decisions into `docs/devlog.md`; delete comments about
removed mechanisms. Check adjacent comments and module docs, not only changed
lines. Known examples include `lifecycle.rs` claiming attach/drift are reserved
and bodies use `FileDocumentCache`, and `embedded_kaish.rs` describing a global
session map, the retired `read_only_shell` name, and an `input_fs` mount that no
longer exists. Verify the surrounding contract before rewriting those comments.
Do not run a formatting sweep.

The migration is complete when all inventory rows are resolved, superseded
APIs have no remaining callers and are deleted, supported entry paths use the
shared owners, relevant end-to-end checks and workspace/all-target checks
pass, and docs/help/comments describe the result. Report deployment separately;
a passing repository migration does not mean host rc was reseeded.
