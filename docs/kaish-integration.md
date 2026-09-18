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

Contexts are retained. `kj context archive <id> --confirm` removes a context
from the active set while preserving lineage, blocks, execution receipts and
approval history. `kj context promote <id>` restores it. There is no context
removal command, and document deletion refuses documents owned by contexts.
Already accepted commands retain their settlement destination across archive.
The migrated execution entry points order admission against archive under the kernel database lock:
archive first refuses new work before user input, drafts, receipts or lifecycle
scripts change; admission first lets that request prepare and settle. Command
hooks, approval and cancellation still apply. Index eligibility
is separate pending policy; see `docs/issues.md`, "Context retention and index
eligibility".

## Current implementation

These are source observations, not promises that all paths behave alike.

- `runtime/admission.rs::ContextAdmission` records an accepted context request.
  Interactive, structured, quiet, streaming, editor, MCP shell and model entry
  points obtain it before preparation. Prompt and chat submission obtain it
  before authoring input or promoting a draft. Rc borrows the triggering
  request's proof; lifecycle scripts do not recheck archive between scripts.
  Approval resume retains admission with its claim. Interpreter construction
  and result publication stay available to accepted requests after archive.
  Missing or unreadable context state refuses new admission explicitly.
  Archive and unresolved-ask cleanup commit atomically, including nested
  rotation transactions; retry also cleans asks on an already archived row.
  Gate ask creation and redemption repeat the canonical context check under
  their database guard, so archive during input capture cannot leave a new ask.
  Interactive model preparation shares the headless startup owner below.
  The duplicate `executeTool` RPC and MCP `kaish_exec` are retired. MCP command
  execution uses `shell` and its retained operation receipt. `callMcpTool` is
  also retired; native broker tests call the cancellable kernel boundary, and
  client tests use retained execution.
  Backend discovery and invocation share the broker's context-visible names,
  bindings, and ListTools filters. Qualified collision names retain their schema
  for positional arguments. Kaish builtins still take precedence.
- MCP exposes `shell`, session/peer tools, and `list_kernel_tools`. Visible
  broker tools with unshadowed names accept ordinary kaish arguments. The
  backend preserves stdout, stderr, exit code and structured output, including
  an error body on stdout. There is no generic exact-name override: kaish
  builtins win collisions such as `read`, `write`, `glob` and `grep`. Native
  model tool calls still use broker name resolution. No `kj mcp call` command
  is added; `kj mcp` continues to administer external servers.
- `src/lib.rs::spawn_kaish_thread` reserves the 16 MiB stack for dedicated
  threads that can enter kaish. The server's Tokio runtime also reserves it.
  The helper sets thread name and stack; it does not construct a shell or own
  its Tokio runtime. Stack tests currently live in server `rpc.rs`.
- `runtime/embedded_kaish.rs::EmbeddedKaish` wraps the kaish kernel. Its common
  constructor wires mounts, the kernel's file cache, per-context JobManager,
  output limits, host execution policy, HOME/PATH, and trace propagation.
- Embedded shells enable kaish's Linux parent-death signal for external
  children. Accepted commands execute on the kernel worker and survive a client
  disconnect; direct external children are killed if the server dies abruptly.
  Process-tree cancellation and hard server death are tested in a PID-isolated
  container. Ephemeral server fixtures deny host exec by default; subprocess
  cases opt in. Native filesystem tests use the production kernel builder and
  real mounts with host exec denied.
- `runtime/context_shell.rs` implements `EmbeddedKaish::for_context` with
  explicit `ShellIdentity` and `ShellPolicy`. It supplies builtins, environment,
  and cwd for every production caller. The eight dispatcher factory methods
  are deleted. Rc policy requires `RcAuthority`, constructed only by lifecycle
  orchestration. Missing dispatcher registration fails construction.
- Context construction refuses a failed loadout/cwd read or an unavailable
  persisted cwd. `ShellCwd` selects current context state or captured approval
  state; a captured unset cwd stays unset. Approval paths no longer restore a
  newer context cwd before applying their pin. Gate outcomes preserve no pin
  versus captured-unset state; rule decisions use current context state. RPC
  cwd validation can repair a removed old directory.
  `ContextShellInputs` is the shared construction/capture value: cwd, durable
  exports, and external execution policy. HOME/PATH defaults have one provider;
  initial PWD follows selected cwd. The old database restore methods are deleted,
  and their tests use the contextual constructor, including VFS-only cwd.
- Durable cwd reads share `shell_state::read_context_cwd`; storage errors are
  distinct from an unset value through model startup and every RPC caller.
  Stored, captured, and persisted cwd values must be absolute. A context switch
  validates target cwd and exports, then saves the outgoing cwd, before changing
  live cwd, exports, or the session's context. Missing directories, read errors,
  invalid exports, and failed write-back refuse the switch without partial live
  changes. Unset target cwd retains the live cwd; target exports overlay the
  current scope.
- `runtime/editor_read.rs` owns `:r !cmd` on the existing kernel worker. Caller
  drop and shutdown cancel the read and allow kaish to finish cleanup; nested
  `kj editor keys` can re-enter without blocking that worker. The editor receives
  complete UTF-8 text or an error before splicing. Truncated output is refused
  explicitly; shell scope changes remain local. No command hooks or transcript
  pair are added to this consumer.
- The kaish backend forwards its invocation cancellation token into MCP calls,
  so a pending tool observes the same cancellation and timeout as its shell.
- Interpreter construction and MCP dispatch carry the complete `ShellIdentity`.
  Editor opens retain requester, performer, reviewer, session, and the context
  at open; later shell navigation does not retarget a read. Block and task tools
  attribute authored content and edits to the performer. The lower-level
  identity constructors are crate-private; engine fixtures use them explicitly.
- Read-only shells classify resolved `kj` argv using the verb's declared
  effect before dispatch. Mutating verbs, editor entry/input, network calls,
  and generic MCP calls are refused. MCP metadata has no effect contract;
  inspection uses filesystem builtins and `kj` reads, while `shell_write`
  owns arbitrary tool calls. Policy comes from contextual construction, not
  the aggregate read-only flag on kaish's temporary filesystem overlays.
  `kj synth` now shares its argument declaration, effect classification, and
  help rendering with the rest of `kj`; the runtime still owns its index/source.
- `runtime/synthesis.rs` owns the block-source adapters used by contextual
  shells; hooks no longer depend on rc for synthesis wiring.
  Hydration errors stop synthesis before embedding work. Image imports read
  the kernel's mounted filesystem before storing bytes in CAS. The docs mount
  resolves parent components within its root and refuses writes to document
  directories; only block paths accept text replacement.
- `rc::run` accepts `RcInvocation` and owns loading, ordering, variables,
  recursion, run records, and `.kai` execution. Markdown is ordinary data. All
  lifecycle callers use this entry point; dispatcher lifecycle methods are
  deleted. `rc/script_path.rs` owns the shared filename/path grammar, and
  `kj rc` remains a command adapter. Unknown lifecycle verbs fail explicitly.
  `RcInvocation` requires an owner token. Inline `kj` re-entry carries the
  interpreter's token through `KjCaller`; cancellation signals child execution
  and waits for cleanup. Committed contexts, drift deliveries, and separately
  admitted turns remain retained. A cancelled lifecycle does not admit a later
  fork turn or commit a rotation. Character-root callers preserve session,
  recursion depth, and owner while selecting the new root performer.
- `rc/settlement.rs` retains each script's complete returned `ExecResult` before
  projecting its Trace or Error block. The run log records the source and start
  before execution. A bookkeeping fault stops later scripts; it never retries
  source. Output bytes, structured data, MIME type, physical exit, spill state,
  and cancellation remain distinguishable. Diagnostic blocks are plain text;
  their ANSI spans, original bytes, and projection marker commit atomically.
  `kj ledger runs <run-id>` exposes retained results and pending settlement.
  In-memory ownership bridges failed result writes; the existing kernel retries
  settlement before later rc work and at shutdown. Unresolved writes make
  shutdown fail. Restart restores pending projections without executing source;
  a begun script with no retained result records that effects may have occurred
  and output is unavailable. Interrupted runs without a retained finish intent
  become `abandoned`; recorded intent remains authoritative. Historical
  completed script records retain their known exit when output is unavailable.
- `runtime/rc_lifecycle.rs` queues scheduled tick/rotate work on the existing
  kernel runtime with the scheduler's original admission and transport snapshot.
  The beat thread runs only clock/transport work. Archive and client disconnect
  do not cancel accepted lifecycles; runtime shutdown signals and joins them.
- `runtime/command.rs` owns execution into a block pair for interactive
  commands, authored structured `kj`, and approval resume. Server `shell_run.rs`
  is deleted. `runtime/structured.rs` owns addressed `kj` construction, quoting,
  pair/receipt creation, response projection, and pending/result channels. It
  admits accepted commands to the kernel worker; RPC only supplies identity and
  translates the immediate outcome. Result
  projections live in `runtime/command_result.rs`, used by command execution,
  structured/streaming RPC, and MCP shell envelopes. The duplicate text-replace
  helper is deleted; callers use the block store's atomic replacement operation.
- Durable-export seeding and approval environment restoration share one helper.
  Values cross as typed kaish overlays; temporary names cannot collide with any
  target, including an unset name. Invalid or duplicate names refuse before any
  value changes.
- `runtime/shell_state.rs` snapshots cwd/exports and commits their changed
  values in one transaction. Failed writes roll back and reach the caller.
  Interactive and structured `kj` block completion now waits for result hooks;
  paused-hook regressions cover runtime execution and the SSH/RPC surface.
- `runtime/command_outcome.rs` retains raw execution, a hook replacement or
  refusal, elapsed time, and shell-state write failures. Interactive and approved
  commands project blocks, receipts, and jobs from it; block reconstruction is
  deleted. Terminal outcomes are retained before projection. Stdout, stderr,
  structured output, content type, exit, ephemeral flags, ANSI spans/original
  bytes, both statuses and the receipt commit in one block-journal transaction.
  Startup finishes pending projections without executing code or hooks. An
  unfinished receipt without a captured outcome settles its original pair and
  receipt in one journal acceptance. Recovery preserves recorded output and ANSI
  provenance, reports the interruption, and leaves the exit code unknown.
  Recovery failure aborts startup before writers are admitted. Raw records are read separately
  from ordinary receipt polls, so a poll does not duplicate captured output.
  Approval resumes that need a new pair use the same atomic pair/receipt/ask
  setup. Their command retains the model role and performer author; the receipt
  records the requester. A claimed ask starts Running, while a pending ask
  starts Waiting. Setup failure reports that the approval is spent and nothing
  ran. Retained outcomes recover without executing the approved source again;
  completion delivery retains its own durable owner after redemption.
  A job reports its captured command outcome even if durable projection fails;
  that persistence error is returned separately. `kj wait --job` may finish while
  `kj wait --operation` still reports an unfinished projection. Recovery keeps
  the original outcome rather than turning publication failure into a new exit.
  Failed terminal-outcome writes keep an immutable live retention owner in the
  receipt registry. The existing worker retries at most four oldest attempts
  per scan without running source or hooks. Final receipt-free review writes
  use the same mechanism. Shutdown retries after joining execution and reports
  results still not durable. `state.retention_error` on operation waits and
  `result_review.retention_error` on ledger reads expose the remaining write
  fault; both clear after retention succeeds. A poisoned document still requires
  restart for projection, with its outcome preserved durably.
  Interactive, structured, tool and approval execution carry their admission
  receipt through preparation, refusal, capture and result review. Execution
  into blocks requires that receipt; active invocations do not reconstruct it
  from the output block. Setup failure settles NotRun and closes the job.
  Recovery may discover a receipt when no live owner remains.
  The command owner also keeps the first interrupted outcome. Dropped review waits, cancellation and panic recovery share that
  exact result without reading SQLite. Settlement faults still reach retention;
  jobs finish with the captured result before an original panic is resumed.
  Terminal retention also closes the invocation's unanswered review asks in
  the same transaction. Ask-update or audit-event failure rolls back the
  result and closure; the existing retry owner keeps both pending. Successful
  retry announces closure once. Already decided asks keep their decisions.
  Linking an executable ask to an existing model pair now adopts that pair into
  the same receipt registry. Model Waiting content, statuses, ask link and
  receipt commit together. A non-executable ask only links its pair. Links
  preserve their context, performer and owner; another pair cannot replace
  them. Original execution asks remain usable for receipt lookup when a later
  result-review ask becomes current. Session pre-call settlement includes its
  ask link and Waiting receipt in the same acceptance; a failed link publishes
  no partial result. Approval admission re-reads linkage with context state
  under the redemption guard, so a link completed after the delivery scan
  controls owner and performer checks. The gate records a paired caller's
  publication contract with the ask. Waiting publication releases delivery in
  the result transaction; bare linkage and terminal failures do not. Publication
  wakes previously deferred answers. Gate retries cannot consume executable
  answers owned by another paired invocation. Terminal publication retires a
  held invocation atomically; startup retires remaining holds after recovering
  captured results. Decisions remain auditable through `kj ledger show`.
  Registered operations recover their original pair even when an ask never
  linked. After retiring unresolved asks, server startup closes receiptless
  writers in one acceptance per context. Statuses, appended stderr and one
  explanation commit together; stored output and provenance survive. Forks use
  the same operation on copied open blocks. Either caller propagates failure.
  Abrupt live failure and continuation admission remain in the ownership audit.
- `runtime/completion_notice.rs` owns execution-completion delivery. An allowed
  approval reserves a notification with its claim; an asynchronous shell reserves
  one with its pair and receipt. Source rows retain identity and continuation
  epoch. A small shared record retains the prepared message, delivered block or
  explicit suppression reason. Block insertion and the delivery marker share a
  journal acceptance. Retrying delivery never authorizes execution.
  Startup resolves interrupted notification owners after command recovery and
  disables automatic provider wakes for those notices. The existing delivery
  worker drains ready notices and new answers in scans of up to four, also
  polling each second so lost events and larger backlogs do not strand work.
  `kj wait --operation` reports notification disposition; `kj ledger show`
  includes an approval's completion delivery. Provider admission remains separate
  from durable delivery and still requires the original continuation window.
- `runtime/interactive.rs` admits shell submissions, constructs the addressed
  context's shell, authors the pair/receipt, and applies PreCall on the kernel
  worker. Accepted work survives RPC teardown. Runtime consumes the captured
  compose draft revision after acceptance; later edits and refused drafts remain.
  Shutdown cancels preparation, execution and review, then joins settlement.
  Preparation failure after receipt registration settles its pair before
  propagating. Context switches
  await the RPC adapter's acknowledgement before publication; a departed adapter
  or kernel shutdown releases that wait. Connection state stays on the RPC thread.
- Structured `executeKj` now shares command outcomes and settlement. Hook
  replacements preserve structured data and clear obsolete stderr/exits. Authored
  calls register durable receipts and release the RPC on a pending result review;
  their kernel task retains execution through caller disconnect and continues
  after approval. Shutdown cancels preparation, pre-call hooks, execution and
  result review, then joins settlement. Pre-call panic settles unrun pairs before
  the original unwind reaches the worker. Hook recursion depth crosses admission.
  Quiet calls share this owner without a transcript pair or ordinary receipt.
- Streaming RPC uses the same execution, state write-back, and retained review
  owner without transcript blocks. Its adapter owns execution IDs, the single
  active-execution slot, interrupts, history, and output subscriptions. All hook
  replacements and denials affect delivered output. Result review holds the slot
  until settled; approval continues hooks without executing source again.
  Stream exit events preserve physical exits through output truncation; synthetic
  replacements use 0/1 completion codes because the wire requires an integer.
  `runtime/streaming.rs` owns preparation and execution on the kernel worker.
  The RPC adapter reserves its single execution slot before preparation, records
  accepted history, applies acknowledged context switches, and dispatches output.
  A dropped adapter releases the slot and cancels its token. Disconnect abandons
  pending review while the kernel retains settlement and captured output; joined
  shutdown waits for that settlement too. Each SSH RPC channel keeps a dedicated
  LocalSet for connection callbacks, and teardown removes its session binding.
- MCP shell execution uses `runtime/tool_command.rs` and the shared command
  owner. Its server declares execution-owned result hooks; other MCP servers
  keep broker-owned hooks. PreCall stays in the broker. PostCall/OnError observe
  actual execution using the original tool name and arguments, not an admission
  receipt. Background calls return only after job attachment; foreground result
  reviews return a typed Pending refusal while retaining execution.
  Background job streams expose raw output after each completed statement;
  final job results and receipts include result-hook effects. A hook replacement
  never fills an otherwise empty raw stream.
- `runtime/worker.rs::RuntimeWorker` supplies one lazy kernel-owned LocalSet on
  a reserved kaish stack. `Kernel::spawn_runtime_task` admits work;
  `stop_runtime_worker` signals cancellation and `shutdown_runtime_worker`
  joins settlement. The executor thread is named `kernel-runtime`. Accepted shell
  tools and model turns survive the submitting runtime or transport. Hook
  recursion depth crosses the handoff. Host shutdown cancels accepted commands,
  drains their settlement, and forbids late startup. Already-cancelled admissions
  do not enter kaish. Read-only commands discard local cwd/export changes;
  writable commands persist them. Async completion notification reads the settled
  receipt instead of writing a second outcome.
- Model streaming and identity resolution live in `runtime/llm_stream.rs` and
  `runtime/turn_identity.rs`. Kernel-owned `TurnState` holds conversation locks,
  cached mailboxes, images, and per-turn leases. Each lease owns its `TurnId`,
  liveness and interrupt registration through startup, queuing and inference. Headless
  admission returns that ID (`kj drive` includes it in structured data), and
  Requested/Completed/Failed retain it through the TurnEvents client callbacks.
  Missing or malformed callback IDs are errors; clients do not invent identity.
  Ending one cannot hide another turn; a context interrupt signals all accepted
  turns. RPC translates startup errors into wire errors. Accepted turns run on
  the kernel worker and survive the caller's LocalSet. Rejected admission drops
  its lease. Every running exit drops its lease and records continuation yield
  before publishing its terminal event. A panic publishes Failed, then resumes
  unwinding so worker shutdown reports failure. Each lease tracks its created
  text, thinking, tool-call, tool-result and receipt-setup blocks. Before terminal
  publication, the runtime atomically marks only that turn's remaining Running
  blocks Error; completed blocks, Waiting approvals and other writers remain
  untouched. Provider error, incomplete EOF, cancellation and panic use this
  cleanup. Panic signals its calls before publication, then reaches worker
  supervision. Cleanup faults publish Failed even when a diagnostic cannot be
  written; cleanup panics also reach supervision. Shutdown cancels queued turns
  and provider work and joins cleanup. Required text/thinking inserts, appends,
  signatures and completion statuses propagate write errors to this owner;
  failure interrupts the turn and prevents successful completion. Thinking
  summaries are display metadata and remain best-effort. Ordinary and inline
  tool calls share durable result creation, shell/ANSI projection and settlement.
  Results start Running in their insertion acceptance; dispatch stops if either
  block could not persist. Content, style, error flag and both pair statuses
  commit together before provider continuation. A persistence fault cancels and
  joins concurrent siblings; cleanup marks failed result flags for hydration.
  Content events must match the open text/thinking block; tools and Done
  require it to be closed. EOF before Done fails the turn, even after complete
  text. Done ends consumption immediately. Hard cancellation preserves only
  accepted content and drains terminal usage under one absolute idle deadline.
  A requested hard cancel takes priority over a ready chunk, error or EOF.
  Generic status writes also synchronize the tool-result error flag, including
  command outcomes and client completion; metadata precedes terminal status.
  Waiting results link the ask in the same acceptance as their content and
  statuses. Initial shell pairs, receipts and optional ask links also commit
  together, including interactive, structured, background and pending-model
  setup. Retrying an ask reuses its receipt; conflicting source or identity
  fails before mutation. Denied/cancelled pair settlement must succeed before
  consuming an answer. Model refusal notifications and redemption commit together;
  repeated notification delivery creates no second block. Session refusal linkage
  remains in the ownership audit; approved completion delivery has a separate durable owner.
- Timed `kj drive --track <name> --score-at <tick>` pairs a turn lease with
  timeline-owned preparation before model work starts. Rc chooses the target
  and fallback; runtime validates complete performer-authored ABC and retains
  durable rejection feedback; the timeline validates its seed/score basis and
  commits or falls back at the original target. Turn and work IDs correlate
  through the command result. Dropping the handoff interrupts only its turn.
  The completion-bus scheduler and `schedule_abc_cell` are deleted. Untimed
  drives no longer schedule notation. See `docs/hyoushigi.md`, "Model turns with
  an intended score tick" for the basis scope and controlled SSH scenario.
- `runtime/turn_request.rs` admits headless requests directly to the kernel
  worker. Fork/drive, approval continuation, and async shell completion all call
  it; FlowBus subscriber counts never authorize execution. Requested publishes
  after admission and before any terminal event. Automatic continuation only
  reserves an idle context. Performer reassignment atomically closes the window
  and invalidates previous epochs. Startup claims and later inference admission
  reject invalidated work; signoff and newer explicit drives still allow accepted
  turns to finish without refreshing those windows. The dedicated request thread
  is deleted.
  `runtime/prompt.rs` admits interactive text and compose drafts before changing
  input. It captures submit facts before registering its own turn, then gives
  submit rc and model preparation to the same startup owner as headless work.
  Every path owns a turn lease before its first await. A departed RPC caller
  cannot discard preparation; shutdown or hard interruption cancels it and
  publishes the admitted turn's cancelled outcome. Preparation faults publish
  Failed. Rc cancellation stops discovery or construction, signals an active
  interpreter and awaits cleanup, records the stopped script, and skips later
  scripts. The provider starts only after submit rc finishes. The caller-owned
  `spawn_llm_for_prompt` API and late lease creation are deleted.
  `runtime/approval_resume.rs` owns answer delivery, claims, captured cwd/env,
  approved execution, and follow-up seeds on the same kernel worker. Startup
  installs one subscription and snapshots old answers before returning; failure
  refuses host startup. Approval input capture rejects unreadable cwd/env before
  creating an ask; the two values share one database lock. Linked receipt reads,
  context validation and execution claims share one database lock; read faults
  leave approvals available for retry. A spent claim cannot
  overwrite completed output after performer reassignment. Shutdown stops delivery,
  cancels preparation and running
  commands, and joins their settlement. Idle delivery holds only a weak kernel
  reference. A preparation unwind settles only its owned pair, or records a
  no-run error when no pair exists, before the original panic reaches the worker.
  Shutdown retains the claimed action's delivery seed without requesting another
  turn. Shared cwd reads live in
  `runtime/shell_state.rs::context_cwd`.
- SIGTERM/SIGINT await the runtime worker's thread before checkpointing and
  exiting. Joining is shared across callers and survives a cancelled waiter;
  a worker cannot join itself. Host Drop signals cancellation without waiting.
  Interactive, structured, streaming, model and approval callers use this owner.
- Shared capture/review catches unwinding panics only to settle before resuming
  the original panic. Execution without a captured result records a fault with
  unknown side effects. State-publication and result-hook panics retain captured
  execution; completed statement output drains before streams close. The worker
  stops admission after a task failure and returns an error from shutdown.
  Task factories also run inside their spawned task, so construction panics
  reach that failure path instead of unwinding the supervisor. Accepted sibling
  commands and queued shutdown work keep their settlement owners. Construction
  remains on the kaish thread and may produce a non-`Send` future.
- Live reporting/retry of persistence failures remains open. Abrupt task
  destruction before capture still needs a durable terminal outcome; cooperative worker shutdown settles execution, paused hooks,
  and review with matching job/receipt results and closed streams.
- `runtime/result_review.rs` checkpoints executed outcomes and links them in
  the PostCall or OnError ask transaction, before notification or waiting. Approval continues the same ordered hook snapshot;
  neither the command nor earlier hooks run again. Result-review asks have
  `hook_result` origin, no executable source, and a digest binding the phase,
  call, and captured result. The generic execution/resume driver excludes them;
  retry redemption cannot consume another execution owner's answer.
  The command owner handles cancellation. Dropping its wait retains the
  outcome and closes unanswered asks together; restart uses the same retention
  transaction and reports interruption without re-entering the lost hook snapshot.
  The same review store retains quiet results without transcript blocks. Every
  sequential ask links to its invocation and optional operation receipt.
  `kj ledger show <request-id>` exposes captured and terminal results.
- `background_exec.rs` was removed in `8ea04fdf`. Asynchronous shell programs
  use kaish's job system with durable Kaijutsu receipts. Earlier notes claiming
  that a temporary shell cannot host work that outlives it are obsolete.

Context-level `kj wait` requires no accepted turns left in flight on both its
event and polling paths. It retains terminal details while other turns remain;
a terminated subscription falls back to paced log/liveness polling. Explicit
job waits report process-local status and exit code, while operation waits
observe durable receipts. Numeric job IDs are not durable operation handles.

Hook evaluation carries its execution owner's cancellation through builtin
bodies, kaish execution, and result review. Once a body starts, the evaluator
awaits its cleanup; it does not drop the running body to settle a command.
Cancellation closes unanswered result reviews and retains previously captured
execution. A script's explicit exit 130 remains a denial when its owner is
live; cancellation is determined by the owner token. Timeout 124 and internal
output spill 3 still escalate. Diagnostics retain the last 512 stderr
characters with terminal controls escaped.

Advisory shell hooks run through `runtime/dry_run` on the existing kernel
worker. They never execute the proposed command, but their own effects require
context admission, disconnect survival, and joined shutdown. Inline and stored
script bodies remain snapshots; path bodies read at invocation. Hook shells
retain the invoking session as well as requester, performer, and reviewer.
Notification hook evaluation and its final block insertion share one runtime
owner; dropping a notification producer does not discard accepted evaluation.
Cancellation suppresses that emission after cleanup.

Adapter writes also carry the performing identity. `/v/docs` uses the shell's
performer and the block store's atomic whole-text replacement. Editor input
uses its current actor through keys, paste, asynchronous read insertion, and
rollback. The opener remains the owner of `:r !` execution; the read result's
insertion belongs to whoever submitted the keys. RPC input uses the connection
principal and `kj editor` uses its invoking performer. Original block IDs and
authors remain unchanged. Durable per-edit provenance remains separate work.

## Caller migration inventory

Each row must move to the shared integration or have a specific, documented
reason to use a lower-level kaish API. Update this table as changes land;
remove the obsolete API in the same change as its final caller.

| State | Caller or mechanism | Current location | Contract to retain |
|---|---|---|---|
| Migrated | Shell construction and builtin wiring | `runtime/embedded_kaish.rs`, `runtime/context_shell.rs` | One construction owner; structural read-only policy; explicit requester, performer, reviewer, session, and context |
| Partial | Dedicated threads and startup runtime | kernel `lib.rs`, `runtime/worker.rs`; server `main.rs`, `ssh.rs`, `beat.rs` | Stack reservation, cancellation/shutdown, re-entry, and `!Send` RPC placement |
| Migrated | Interactive shell submission | kernel `runtime/interactive.rs`, `runtime/command.rs`; server RPC adapter | Kernel admission, draft revision consumption, addressed identity/context, command/output pair, hooks, write-back, acknowledged context switches, disconnect survival, and joined shutdown |
| Migrated | Streaming execute RPC | kernel `runtime/streaming.rs`, `runtime/command.rs`; server RPC adapter | Kernel-owned preparation/execution/settlement; connection-owned IDs, admission slot, history, cancellation and callbacks; hooks, review, physical exit, context switches, disconnect and joined shutdown |
| Migrated | Structured `executeKj` | kernel `runtime/structured.rs`, `runtime/command.rs`; server RPC adapter | Kernel admission, shared execution/settlement, addressed context, literal argv, typed refusals/latches, quiet review, data, state write-back, disconnect survival, and joined shutdown |
| Retired | Generic `executeTool` and MCP `kaish_exec` | Reserved wire ordinal 18; MCP uses retained `shell` | Unshadowed broker calls preserve both streams and structured output; no exact-name override |
| Retired | Generic `callMcpTool` | Reserved wire ordinal 58 | Client tests use retained shell submissions; native broker fixtures preserve tool-specific behavior; the uncancellable dispatch wrapper is deleted |
| Partial | Model turns and conversation state | kernel `runtime/llm_stream.rs`, `runtime/turn_state.rs`, `runtime/interrupt.rs`, `runtime/turn_identity.rs` | Shared identity/provider selection, conversation exclusion, hydration, terminal events, per-turn leases, worker placement, headless admission, shutdown, and selective open-block cleanup; approval ownership transfer remains open |
| Partial | Approval resume | kernel `runtime/approval_resume.rs`, `runtime/command.rs` | Original actor/reviewer, captured cwd/env, retained pair/receipt, single-use claim, runtime ownership, startup readiness, cancellation, joined settlement, preparation unwind cleanup; explicit publication handoff; terminal/restart retirement; registered and receiptless original-pair recovery; durable completion delivery; abrupt live failure and continuation admission remain open |
| Partial | Model/MCP foreground and background shells | kernel `mcp/servers/shell.rs`, `runtime/tool_command.rs`, `runtime/worker.rs` | Shared execution/hooks, structural read-only policy, stdin, typed review, job/receipt settlement, state, cooperative shutdown, and unwind settlement migrated; durable completion delivery migrated; pre-admission drop leaves no receipt; post-admission drop settles; job results retain captured outcomes through projection failure; terminal retention retries without execution; ask/checkpoint/link admission is atomic; interruption shares the original outcome without storage reads; result retention and unanswered-ask closure are atomic; admission receipts survive preparation/refusal and execution-entry read faults; abrupt worker destruction and remaining job/controller lifetimes remain open |
| Migrated | Rc lifecycle | kernel `rc/mod.rs`; create/fork/attach/drift/tick/rotate/submit callers | Discovery, ordering, lifecycle facts, run records, failure visibility, recursion, and explicit rc authority migrated; inline re-entry carries cancellation and joins kaish; scheduled tick/rotate retains admission on the joined runtime; durable script admission, full returned results, atomic projections, retry/restart without replay, and truthful committed-state failures |
| Migrated | Hook bodies | kernel `mcp/broker.rs`, `runtime/dry_run.rs`, command result review | Contextual identity/session, snapshot versus path-read semantics, internal output/verdict protocol, real timeout, owner cancellation with joined cleanup, recursion-depth propagation, retained result review, admitted advisory work, and owned notification emission |
| Migrated | Editor shell reads | kernel `runtime/editor_read.rs`, `kernel.rs::fetch_editor_io` | Kernel ownership, caller/shutdown cancellation, re-entry, complete UTF-8, fail-before-splice, full opener identity, context captured at open, and refusal of editor entry/input through read-only shells |
| Migrated | Environment setup and approved environment restore | `ContextShellInputs`, `apply_ask_env`, `runtime/shell_state.rs`, `kj/env_snapshot.rs` | One atomic cwd/export snapshot and shared defaults; validated typed restore preserves unset values and avoids overlay collisions; approval uses original identities and captured inputs; transactional write-back stays explicit |
| Pending | Job/receipt readers and controllers | kernel `shell_operations.rs`, `kj/wait.rs`, `kj/context.rs`, runtime job builtins | In-memory jobs and durable receipts keep their distinct lifetimes |
| Migrated | Integration backends and builtins | `runtime/*_backend.rs`, filesystem adapters, `kj_builtin`, `vi_builtin`, `curl_tool`, `ps_builtin`, synthesis, file tools/cache | Direct kaish backend/tool implementations retain contextual construction and policy. Docs/image/synthesis adapters and file-tool/cache mutation carry the invoking performer; hydration keeps the loader principal |
| Migrated | Gate planning and parsing | `kj/gate*`, `hook_gate`, `shell_gate`, `plan_clauses`, `readonly` | Direct syntax consumers: kaish owns parsing/plans, shared clause rendering feeds review and broker scoring, and the retained plan supplies approval environment capture without a second interpreter |
| Migrated | Tests and fixtures | kernel and server unit/integration tests | SSH, kernel rc, and direct interpreter fixtures deny host execution by default; explicit subprocess cases opt in. Direct engine tests cover settlement/cancellation without contextual dispatch; builtin fixtures test their kaish interfaces; gate/parser fixtures only parse or plan. Real kernel/SSH and container tests cover contextual behavior, resolution, and lifecycle. |

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
While the process is alive, the receipt registry retains failed terminal writes
for retry without execution. Process loss before SQLite accepts that outcome
leaves only the durable observations available to interrupted-run recovery.

Result-hook asks retain what already ran and resume hook processing without
permission to rerun. Ask creation, the captured outcome, and the invocation link
commit together before notifying reviewers. This includes quiet calls and
automatic decisions. A checkpoint or link failure rolls back the ask; the caller
receives a hook refusal, and tracked commands retain their captured execution
through terminal settlement. Interactive/approved and authored structured
commands publish Waiting blocks after this admission. Cwd/exports persist before
review.
Approval consumes the answer and continues the ordered hook snapshot; every
sequential ask retains its link to that invocation. Captured execution remains
inspectable after completion. Tracked review completion commits atomically with
terminal outcome preparation; quiet review completion stores an immutable result
without a transcript pair. Ordinary quiet calls create no review record. The
checkpoint schema migration preserves interrupted tracked reviews. Terminal
retention abandons linked pending/claimed asks atomically with the outcome;
storage failure keeps the immutable result for retry. Existing decisions remain
unchanged. Restart retains execution and reports interrupted review even if the answer
arrived before shutdown. It cannot reconstruct the in-memory hook snapshot.

`kj ledger show <request-id>` includes `result_review.captured` and
`result_review.settled` in its structured data; the latter is `null` while hooks
are still processing. The output shows both results, so a quiet caller can read
its completed result without a transcript block. An earlier ask in a sequence
still resolves the same invocation and optional operation receipt.

Non-shell MCP calls do not yet supply a result-review owner. Their result-phase
Ask or kaish escalation returns GateUnavailable before creating an ask. Migrate
those consumers to retained outcomes; do not restore executable asks as a fallback.

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
   helper dependencies and duplicate MCP completion logic (both removed). Preserve caller
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
