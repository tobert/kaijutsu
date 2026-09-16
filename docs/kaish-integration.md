# Kaish integration and rc lifecycle

Status: migration in progress. Contextual construction belongs to runtime and
lifecycle orchestration belongs to rc. Instruction scripts now use explicit
block authoring; command settlement and turn ownership remain open. The inventory distinguishes completed and pending work.

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

These are source observations, not promises that all paths behave alike.

- `src/lib.rs::spawn_kaish_thread` reserves the 16 MiB stack for dedicated
  threads that can enter kaish. The server's Tokio runtime also reserves it.
  The helper sets thread name and stack; it does not construct a shell or own
  its Tokio runtime. Stack tests currently live in server `rpc.rs`.
- `runtime/embedded_kaish.rs::EmbeddedKaish` wraps the kaish kernel. Its common
  constructor wires mounts, the kernel's file cache, per-context JobManager,
  output limits, host execution policy, HOME/PATH, and trace propagation.
- `runtime/context_shell.rs` implements `EmbeddedKaish::for_context` with
  explicit `ShellIdentity` and `ShellPolicy`. It supplies builtins, environment,
  and cwd for every production caller. The eight dispatcher factory methods
  are deleted. Rc policy requires `RcAuthority`, constructed only by lifecycle
  orchestration. Missing dispatcher registration fails construction.
- `runtime/synthesis.rs` owns the block-source adapters used by contextual
  shells; hooks no longer depend on rc for synthesis wiring.
- `rc::run` accepts `RcInvocation` and owns loading, ordering, variables,
  recursion, run records, and `.kai` execution. Markdown is ordinary data. All
  lifecycle callers use this entry point; dispatcher lifecycle methods are
  deleted. `rc/script_path.rs` owns the shared filename/path grammar, and
  `kj rc` remains a command adapter. Unknown lifecycle verbs fail explicitly.
- `runtime/command.rs` owns execution into a block pair for interactive
  commands, authored structured `kj`, and approval resume. Server `shell_run.rs`
  is deleted. `runtime/structured.rs` owns addressed `kj` construction, quoting,
  pair/receipt creation, and response projection. RPC only decodes, supplies
  identity, owns its response lifetime, and encodes the reply. Result
  projections live in `runtime/command_result.rs`, used by command execution,
  structured/streaming RPC, and MCP shell envelopes. The duplicate text-replace
  helper is deleted; callers use the block store's atomic replacement operation.
- `runtime/shell_state.rs` snapshots cwd/exports and commits their changed
  values in one transaction. Failed writes roll back and reach the caller.
  Interactive and structured `kj` block completion now waits for result hooks;
  paused-hook regressions cover runtime execution and the SSH/RPC surface.
- `runtime/command_outcome.rs` retains raw execution, a hook replacement or
  refusal, elapsed time, and shell-state write failures. Interactive and approved
  commands project blocks, receipts, and jobs from it; block reconstruction is
  deleted. Terminal outcomes are retained before projection; receipts commit
  against that record before terminal block publication. Startup finishes pending
  projections without executing code or hooks. Raw records are read separately
  from ordinary receipt polls, so a poll does not duplicate captured output.
- Structured `executeKj` now shares command outcomes and settlement. Hook
  replacements preserve structured data and clear obsolete stderr/exits. Authored
  calls register durable receipts and release the RPC on a pending result review;
  their task retains execution and continues after approval. Quiet calls share
  execution, result review, and projection without a transcript pair or ordinary
  operation receipt.
- Settlement remains incomplete: streaming RPC and MCP completion still have
  separate projection paths. Streaming RPC cannot honor all hook verdicts; live
  reporting/retry of persistence failures also remains open.
- `runtime/result_review.rs` checkpoints executed outcomes before waiting on a
  PostCall or OnError ask. Approval continues the same ordered hook snapshot;
  neither the command nor earlier hooks run again. Result-review asks have
  `hook_result` origin, no executable source, and a digest binding the phase,
  call, and captured result. The generic execution/resume driver excludes them;
  retry redemption cannot consume another execution owner's answer.
  Cancellation or dropping the wait abandons the ask and retains the outcome;
  restart reports interrupted review without re-entering the lost hook snapshot.
  The same review store retains quiet results without transcript blocks. Every
  sequential ask links to its invocation and optional operation receipt.
  `kj ledger show <request-id>` exposes captured and terminal results.
- `background_exec.rs` was removed in `8ea04fdf`. Asynchronous shell programs
  use kaish's job system with durable Kaijutsu receipts. Earlier notes claiming
  that a temporary shell cannot host work that outlives it are obsolete.

## Caller migration inventory

Each row must move to the shared integration or have a specific, documented
reason to use a lower-level kaish API. Update this table as changes land;
remove the obsolete API in the same change as its final caller.

| State | Caller or mechanism | Current location | Contract to retain |
|---|---|---|---|
| Migrated | Shell construction and builtin wiring | `runtime/embedded_kaish.rs`, `runtime/context_shell.rs` | One construction owner; structural read-only policy; explicit requester, performer, reviewer, session, and context |
| Pending | Dedicated threads and startup runtime | kernel `lib.rs`; server `main.rs`, `ssh.rs`, `beat.rs`, turn/resume drivers | Stack reservation, cancellation/shutdown, re-entry, and `!Send` RPC placement |
| Partial | Interactive shell submission | server `rpc.rs::execute_shell_command`; kernel `runtime/command.rs` | Draft revision consumption, command/output pair, identity, hooks, cwd/export write-back, context-switch notification |
| Pending | Streaming execute RPC | server `rpc.rs::execute` | Execution IDs, connection cancellation and concurrency rules, output subscriptions; resolve its unsupported hook substitution explicitly |
| Migrated | Structured `executeKj` | kernel `runtime/structured.rs`, `runtime/command.rs`; RPC response lifetime in server | Shared execution/settlement, addressed context, literal argv, typed refusals/latches, quiet review, data, and state write-back; worker placement remains in the dedicated-thread audit |
| Partial | Approval resume | server resume drivers; kernel `runtime/command.rs` | Original actor/reviewer, captured cwd/env, existing block pair, exactly one execution and terminal settlement |
| Pending | Model/MCP foreground and background shells | kernel `mcp/servers/shell.rs` | Read-only/writable distinction, stdin, typed rejection, job ownership, receipts, cancellation, and async completion |
| Migrated | Rc lifecycle | kernel `rc/mod.rs`; create/fork/attach/drift/tick/rotate/submit callers | Discovery, ordering, lifecycle facts, run records, failure visibility, recursion, and explicit rc authority |
| Pending | Hook bodies | kernel `mcp/broker.rs` | Inline snapshot versus path-read semantics, internal output profile, hook timeout, exact verdict interpretation, and no recursive command-hook application |
| Pending | Editor shell reads | kernel `kernel.rs::fetch_editor_io` | Opener identity/context, full text, and fail-before-splice behavior |
| Partial | Environment setup and approved environment restore | `EmbeddedKaish::apply_context_config`, `apply_ask_env`, `runtime/shell_state.rs`, `kj/env_snapshot.rs` | Scoped variables, exact approved inputs, shared serialization, and explicit write-back policy |
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

Structured `kj` argv is rendered as one quoted kaish invocation. Quoting follows
kaish's parser, including literal backticks inside double quotes, and an execution
test checks authored text. The pinned kaish `execute_argv` API has no per-call
`ExecuteOptions`; keep cancellation/stdin policy intact when considering it.

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
outcome by reading a partially settled block. Kaish's job control requires an
integer: unmodified execution retains its complete `ExecResult`, including its
spill control code. A synthetic job result uses 0/1 and `kaijutsu.synthetic`
baggage; its public envelope has no physical exit code. Real command exits 2
and 3 are errors, just like other nonzero exits; an output-limit remap is judged
by the retained original exit.

Interactive settlement retains its terminal outcome and a pending-projection
marker atomically before writing blocks. It commits the receipt against that
immutable outcome before publishing terminal statuses, then clears the marker.
A failed write returns an error. Startup hydrates the affected documents and
finishes retained projections without invoking kaish or hooks. It repairs only
unfinished publication: a committed receipt plus terminal output proves that
output projection finished, so later edits are preserved. A terminal block alone
is insufficient; it may predate this execution. Startup fails explicitly if a
retained projection cannot be recovered.

A command whose outcome was never retained remains subject to interrupted-run
recovery: the kernel reports an unknown result and never reruns its source.
Failure before the initial retention write still requires live reporting/retry;
background interactive callers currently log that failure, and approval resumes
report it to the model.

Result-hook asks retain what already ran and resume hook processing without
permission to rerun. Interactive/approved and authored structured commands
checkpoint the outcome and current ask before publishing Waiting blocks.
Cwd/exports persist before review.
Approval consumes the answer and continues the ordered hook snapshot; every
sequential ask retains its link to that invocation. Captured execution remains
inspectable after completion. Tracked review completion commits atomically with
terminal outcome preparation; quiet review completion stores an immutable result
without a transcript pair. Ordinary quiet calls create no review record. The
checkpoint schema migration preserves interrupted tracked reviews. A dropped
wait or cancellation abandons an unanswered ask;
restart retains the execution and reports interrupted review even if the answer
arrived before shutdown. It cannot reconstruct the in-memory hook snapshot.

`kj ledger show <request-id>` includes `result_review.captured` and
`result_review.settled` in its structured data; the latter is `null` while hooks
are still processing. The output shows both results, so a quiet caller can read
its completed result without a transcript block. An earlier ask in a sequence
still resolves the same invocation and optional operation receipt.

Streaming RPC and generic MCP calls do not yet supply a result-review owner.
Their result-phase Ask or kaish escalation returns GateUnavailable before
creating an ask. Migrate those consumers to retained outcomes; do not restore
executable asks as a fallback. Streaming RPC's unhandled verdicts remain open.

Preserve the distinction between parse/validation rejection (nothing ran) and
an execution fault. Persistence failure must be visible and must not report a
successful terminal result. Define recovery for an accepted operation whose
completion cannot be persisted. JobManager remains kaish's execution mechanism;
the durable operation receipt remains Kaijutsu's restart/recovery record.

## Rc Markdown: explicit instruction authoring

Automatic Markdown loading is removed. Only canonical `SXX-name.kai` entries
execute. Other files, including noncanonical or dangling `.md` entries, are
ignored before executable-name validation. Invalid `.kai` names still fail
before any script executes. `kj rc` administers canonical `.kai` scripts and
`.md` data, labeling data separately in list/show output.

Shipped instruction scripts use the ordinary `kj` write path:

```sh
kj block create --role system --kind text --content-type text/markdown < "$(dirname "$0")/$(basename "$0" .kai).md"
```

`$0` is the invoked VFS entry path, including its context-type symlink name.
Companion lookup is relative to that entry. Composed seeds include both a
script symlink and a data symlink. Existing Markdown paths remain unchanged,
so non-forced reseeding can install wrappers while preserving edited text.
See `docs/rc-on-disk.md`, "Migrating existing rc trees" for custom entries.

Amy chose invoking-performer authorship: "Use the invoking performer
consistently with kj." New instructions are `System` / `Text` / `Done` /
`Markdown` blocks authored by `caller.actor_id`; creator and requester may
differ. Existing durable blocks retain their authors. Required missing or
invalid UTF-8 input fails visibly without authoring a block. Redirection
preserves trailing newlines and does not route instruction bytes through
stdout limits. Successful script stdout is still bounded diagnostic trace.

Executable bodies snapshot at lifecycle start. Companion data is read when
the script runs. The script digest records only the executable body; it does
not claim to include transitive data. The authored block preserves the text
read. This differs deliberately from the former Markdown loader's snapshot
of every instruction body.

Tests cover regular and symlinked invocation paths, distinct creator/requester/
performer identities, more than 4 MiB of exact UTF-8 input, missing and invalid
input, executable snapshots versus live data reads, reseeding with custom
Markdown, and rendered prompts for every migrated context type. The locked
kaish API supplies positional parameters directly; no upstream API change was
needed. Its unsupported `${0%.kai}` expansion is recorded in `docs/issues.md`;
the shipped scripts use supported `dirname` and `basename` builtins.

## Implementation sequence

Each step includes tests and comment cleanup, then a focused commit. The shared
API is allowed to evolve as the next consumer supplies evidence; temporary
adapters must have a named deletion step and must not become permanent aliases.

1. **Document the boundary and inventory.** This document, architecture
   summaries, rc/prompt contracts, and the live issue plan agree about current
   behavior and the intended destination.
2. **Consolidate construction (implemented).** Contextual construction belongs
   to runtime, with explicit invocation identity and named policies.
   All factory callers now use `EmbeddedKaish::for_context`; the
   `materialize_context_kaish_*` family is deleted. Backend and builtin
   interfaces are preserved. Rc authority belongs to its lifecycle caller.
3. **Separate rc orchestration (implemented).** `rc::run` owns
   discovery, execution records, recursion, and diagnostics adjacent to runtime.
   `kj rc` is an administration adapter; every lifecycle caller is migrated.
   Markdown loading is replaced by the explicit instruction scripts above.
4. **Consolidate command settlement (owner migration in progress).** Runtime
   owns block-pair execution, result projections, and atomic shell-state
   write-back; server helper dependencies are removed. Introduce one outcome and projection
   path, then migrate interactive, streaming, structured `kj`, model foreground,
   model background, and approved-resume callers. Remove server-to-`rpc.rs`
   helper dependencies (removed) and duplicate MCP completion logic. Preserve caller
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
lines. The construction migration corrected lifecycle verb/body-loading docs,
invocation-local session tracking, retired tool names, removed mounts, and
output-limit descriptions. Continue auditing each owner as it moves. Verify
the surrounding contract before rewriting comments. Do not run a formatting
sweep.

The migration is complete when all inventory rows are resolved, superseded
APIs have no remaining callers and are deleted, supported entry paths use the
shared owners, relevant end-to-end checks and workspace/all-target checks
pass, and docs/help/comments describe the result. Report deployment separately;
a passing repository migration does not mean host rc was reseeded.
