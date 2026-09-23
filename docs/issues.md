# Open Issues

Live work items distilled from prior design and TODO docs, plus architectural observations from code reviews. Code is truth; this exists to track what's *not* in the code yet.

Organized by area. Keep entries terse — link to file:line when a pointer makes the work concrete. When an item ships, delete the entry — if the "how we got here" is worth keeping, move the narrative to [`devlog.md`](devlog.md) (the landed-work story). See `AGENTS.md`, "Writing, memory, and git".

---

## Anticipation and commitment — iteration order

The kernel coordinates anticipation and commitment on the shared pulse.
Iterate through one observable performance with producers at different
speeds; each change should improve that flow, remove a competing mechanism,
or establish a testable contract. Keep model placement independent of this
work. `docs/audio-inference.md` records the workload and measured costs.

1. **Keep interaction dependable.** Continue the complete kaish/rc caller
   migration alongside the following steps, using `docs/kaish-integration.md`'s
   inventory. Each migrated caller must retain identity, cancellation,
   complete output, and terminal
   settlement, with the old path removed. These are the means of playing
   the instrument, so verify them through the actual client as well as unit
   tests.
2. **Prove different producer speeds on one pulse.** Build a deterministic
   integration scenario using controlled producers and a controllable clock:
   fast, delayed, failed, and completed after their basis changes. Assert
   that the shared timeline advances while work is pending, valid results
   commit once, stale or superseded completions cannot overwrite accepted
   work, and a missed commitment uses its declared fallback. Use barriers
   and explicit completion delivery, not wall-clock sleeps or hosted models.
3. **Align execution with that proof.** Audit admission through completion
   and commitment against exact work ownership and intended musical time.
   Keep slow preparation outside timeline locks; give attempts bounded
   resource admission (`docs/resource-admission.md`) and explicit
   cancellation/shutdown behavior. Preserve
   a small synchronous commit step. Use existing runtime and resolver seams;
   delete superseded paths as the scenario starts passing.
4. **Expose the feedback needed to play.** Record intended musical time,
   queue/compute/transfer duration, readiness, basis validity, and final
   disposition. A player should be able to distinguish committed work,
   obsolete work, a failure, and a declared fallback. Measure under overlap
   before choosing lead-time targets; avoid turning unmeasured percentiles
   into guarantees.
5. **Exercise replacement, then extract.** Run the same scenario with one
   real producer and CAS/SFTP media delivery, observing it through a client
   and audiod when hardware is available. Use the evidence to choose which
   model-specific behavior becomes an optional tool or resolver adapter.
   `kj audio beats` is a candidate; its new home remains undecided. Remove
   the kernel dependency and obsolete verb only with a verified replacement
   or an explicit decision to drop the feature.

The first day's useful checkpoint is a repaired interaction path and a
repeatable scenario that exposes the next scheduling gap. A controlled
failure is useful evidence; a new model integration is not required to
prove the coordination contract. Update the checklist as each step lands,
and correct adjacent docs/comments in the same change.

### Measure and recover admitted timeline work

Live model score work now binds an absolute target and a seed/score basis at
admission, with turn-owned delivery and timeline-owned cancellation. The old
completion listener is deleted. Controlled SSH tests exercise both the generic
producer scenario and timed `kj drive`, including the shipped musician tick
script. See `docs/hyoushigi.md`, "Model turns with an intended score tick".
The basis deliberately excludes the changing conversation, tool inputs and
cross-track heard view. Broader dependencies need an explicit projection;
current model handoffs do not automatically replay their producer on invalidity.

CAS preparation has a process-wide limit of four operations. An uninterruptible
host read retains its slot even after its owner is cancelled; four stuck reads
would stop further CAS preparation until a read returns or the process restarts.
Timeline deadlines still settle waiting work and the pulse keeps advancing. The
20 ms cost estimate remains an initial guess, not a measured size/queue model.
The open-work bound counts operations, not bytes; ready-source and committed CAS
residency still need measurement before admitting large rendered artifacts through
this adapter (placed clips carry small records referring to media in CAS).

Feedback retains its original source anchor when available and retries missing
anchors or failed writes on subsequent pulses. The delivery cursor and bounded
work history remain live state, not restart recovery or durable work provenance.
A persistent write fault also holds later feedback behind the failed event.
Measure and design recovery before promising durable admission or delivery.

## Architecture cleanup plan

Source review at `f7e46f8e`, September 16:
[evidence and proposed tests](audits/2026-09-16-architecture-debt.md).
This is the live plan; the audit records the review. Planning added source
markers and corrected misleading comments, without changing behavior.

The bounded deletions are recorded in `docs/devlog.md`, "Retiring duplicated
state". The remaining work below needs separate reviewable changes. Start
behavior changes with red/green regressions; remove each issue and its source
marker in the change that satisfies it.

### Idle conversation reset policy

Idle LRU eviction still resets semantic conversation history, so cache
pressure can make stored edits visible on the next turn. Active and waiting
turns retain one lock across reset; concurrent first lookups share it.
Decide when idle conversations may reset before changing the LRU policy;
preserve context/conversation separation. See `docs/conversation-session.md`.

### Turn execution and shell settlement

`docs/kaish-integration.md` owns the caller inventory and its Migrated,
Partial and Pending rows. `docs/resource-admission.md` owns the worker pool
slices. The story of what landed is in `docs/devlog.md`, "Retiring duplicated
state", and in git. Review evidence:
`~/exomemory/kaijutsu/reviews/2026-09-17-execution/`. What stays open:

Provenance and storage
- **No durable per-edit audit record.** Writes carry their current performer
  as live state only: `TextEdit`/`SyncPayload` and persisted snapshots do not
  retain an edit actor.
- **Boot skips a corrupt document with only a log line.** `load_one_from_db`
  refuses with `CorruptSnapshot` or `CorruptOplog`; the bulk path
  `load_from_db` logs and skips. Return the skipped contexts and their errors
  so startup can report them.
- **A failed document acceptance poisons that context until restart.**
  Structured inspection from that context also refuses; inspect the operation
  from a healthy context.
- **Pair creation and receipt registration are separate writes.** A failure
  between them leaves blocks without a receipt.

Block tools

File cache
- Generation metadata errors are swallowed, and comparison detects only an
  increasing generation.
- A dirty symlink buffer does not detect a changed target in the guarded-write
  check. Preserve dirty work while fixing it.
- Stale-read error branches remove cache entries; check that they preserve
  editor pins.

Settlement and lifetimes
- **Non-shell MCP calls have no retained result-review owner.** A result-phase
  Ask or escalation returns `GateUnavailable` before minting an ask.
- **Abrupt worker-task destruction before capture has no live terminal
  settlement.** Startup reports the interruption without replaying source.
  The job/controller lifetime work is the Pending row in
  `docs/kaish-integration.md`.
- Host `Drop` is a cancellation signal without a wait; SIGTERM and SIGINT do
  wait for the runtime pool.
- The live retention copy of a terminal result is lost if the process dies
  before SQLite accepts it. Retries are four per scan, so a long storage fault
  accumulates retained results: `docs/resource-admission.md`, slice 5.
- A session refusal settles before a separate redemption, so a retry can
  re-emit the same pair's metadata and status updates. Startup suppresses old
  denied pairs rather than settling them.
- The four-item delivery cap counts deliveries, not provider requests. Model
  spend needs its own admission count. A changed-performer completion keeps a
  suppressed disposition; conversations already running need their own audit.
- `jobs --json` does not expose the nested external process group under the
  outer kaish job, so the receipt does not identify it. Parent-death cleanup
  covers direct children; descendant trees after SIGKILL have no evidence.

`kj wait` and turn outcomes
- Turn events carry a `TurnId`, but clients clear context activity on any
  terminal event.
- A context wait has no durable per-turn outcome: a failure before any model
  block plus a missed terminal event can time out as running. Stored event
  detail is the latest observed outcome only.
- The global ledger wake re-reads an ask on unrelated changes:
  `docs/resource-admission.md`, slice 6.

### Shared client recovery

Give `kaijutsu-client` ownership of subscription, mirror, snapshot recovery,
and rejection of obsolete responses. Start with tests for delta/snapshot
ordering, overlapping reconnects, and release during recovery. Migrate app,
TUI, and ACP one at a time, removing their duplicate lifecycle code as they
adopt the shared owner. Clients retain retry/release presentation choices.

### Render the live mirror

Separate collapse/selection state from live `BlockSnapshot` copies, then let
the app render its mirror without rebuilding `RenderBlockStore` per version.
Preserve welcome/offline sources and geometry/glyph caches. Reuse collapse
regressions and check streaming plus context switching through the GUI runner
and BRP. Keep this separate from the shared recovery migration.

### Consent setting ownership

`kj context set --consent` writes `ContextRow.consent_mode`, while the model
loop reads the kernel-wide value, whose setter has no workspace callers.
Choose context resolution or removal before implementation. Do not copy
context configuration into shared kernel state. If retained, test two contexts
with distinct limits through the real turn path. If removed, check CLI help,
schema/persistence migration, and rejection of the retired option. This is a
behavior decision, not part of the dead-API deletion.

### Lazy file documents

Audit readers, editors, and recovery callers before choosing a buffer
representation. Ordinary reads should not require durable documents; persist
unsaved editor content with its recovery metadata. Preserve external-change
checks, pinning, and swap acknowledgment. Start with restart/recovery and
external-edit regressions from `docs/file-buffers.md`; only then remove clean
read materialization. This remains a separate design change.

**Order:** kaish/rc migration (construction, rc, settlement, then turn
ownership), shared recovery, rendering, then file-buffer persistence. Each
requires its own reviewable change; the source TODOs point to these entries.

### Kernel architecture overview needs a refresh

Finish the symbol and schema inventory review alongside the runtime migration.
Keep the architecture overview's current implementation separate from the
planned destination in `docs/kaish-integration.md`; update server ownership,
lifecycle callers, and diagrams when the code moves.

## What banto driving one cleanup to a coder showed (2026-09-22)

Banto (director, qwen3.8-flash) created a coder lane and drove the
TimelineVisibility cleanup. The coder's edits were right; the path around them
stalled. Ask rows: `kj ledger list --history --origin shell_gate --since`
covering 2026-09-22. Open, most costly first:

- **An ask to a model reviewer notifies nobody.** The coder's asks went to
  banto, whose turn had ended; nothing woke it, and the coder spun to the
  50-iteration cap. Design direction to discuss: a model reviewer passes the
  ask up the walk with its opinion attached rather than holding it.
- **One inert statement escalates a whole program.** Banto ran
  `kj ledger allow <id>; echo "allow_exit=$?"`. The ledger answer has a
  builtin allow; `echo` has no builtin key, so the program escalated and the
  answer became an ask to banto's own reviewer. Banto was already the coder's
  assigned reviewer; nothing else stood in the way. The director stance now says to run the answer
  as a command of its own. Whether `echo`/`printf` without a redirect earn a
  builtin key is open.
- **lfm2d escalates a coder's `cargo check` and `git diff`.** Scored
  situation-normal, escalated anyway; `2>/dev/null` on a read also became an
  ask. Tune through `docs/gate-policy-tuning.md`, as for `kj handoff note`.
- **Offline `kj` takes 44 s to boot.** Inferred, unprofiled:
  `BlockStore::load_from_db` decodes every document snapshot serially under
  the `KernelDb` mutex. Profile before changing it.
- **Approval resume and completion notices write before reserving.**
  `runtime/approval_resume.rs` and `runtime/completion_notice.rs` write blocks
  ahead of a runtime reservation, against rule 1 of
  `docs/resource-admission.md`.
- **Approval delivery holds a top-level reservation for the process
  lifetime.** Harmless at `ADMISSION_CAPACITY` 64; slice 4's occupant
  exemption removes it.
- **A shutdown between a caller's write and its spawn strands the write**
  (kaibo, 2026-09-23). `reserve` checks the shutdown token once; the
  supervisor closes the queue on shutdown, so `RuntimeSlot::spawn` fails after
  `prompt::submit` inserted its block or consumed the draft, or after
  `kj drive` wrote its seed. `ToolCommand::execute` settles its receipt on
  that failure; these two do not. Shutdown-only, so no live harm yet.
- **`RuntimeSlot::Local` is not pinned to its thread.** Spent on another
  thread, it would send work to the originating thread's channel and skip
  admission. No caller does that today; a `!Send` marker or a thread check in
  `spawn` would make it impossible.
- **kaish bare `echo` prints nothing.** POSIX prints a newline. Upstream kaish
  issue; Amy decides whether and how it is posted.

## From the kaibo review of the scripted mock and the session scenario (2026-09-15)

Read by the lead; each line re-checked before it went here.

- **A registry rebuild rewinds every mock queue.** `Provider::from_backend`
  re-reads `KJ_MOCK_SCRIPT_DIR` on every `build_llm_registry`, and `kj cast`,
  `kj backend`, and `kj alias` writes rebuild it, so a mid-scenario write
  replays consumed turns instead of panicking. `session_scenario.rs` survives
  by ordering; say so in the file, or hold the queues outside the provider.
- **`kj handoff tail <other>` refuses a reader with no sheet** because the
  caller is resolved before the target is chosen (`kj/handoff.rs:250-253`).
  Resolve the caller only on the no-target branch.
- Stale comments: `llm/mod.rs:470-476` says the mock refuses streaming;
  `kj/handoff.rs:17,274` point at a `READ_ONLY_TABLE` that no longer exists.

## Accountability at the handoff log (Amy, 2026-09-15)

Reviewer resolution walks `forked_from`, so every path that mints a context
decides who reviews. Fork and `kj context create` from inside a context are
covered; nothing re-parents a context. This is not:

1. **Handoff logs are parentless.** `kj handoff note` mints the
   character's log with `forked_from: None` (`kj/handoff.rs`), a
   parentless context played by a model. The kernel must not guess a root
   (Amy, 2026-09-16), so it needs an explicit parent. Since the default
   reviewer went away, an ask raised there refuses (nobody is above its
   model performer) and it has no lineage root. Amy decides its parent.

Open question from the lane: the walk starts at the ask's own context,
so an actor who is not that context's performer, a human typing in a
lane coder plays, resolves to coder. Starting at the parent would give
banto. Both are models. Neither the docs nor a test pins it; Amy decides
whether a human's command in a model's lane is reviewed by that model,
by its parent, or walks to the nearest root.

## Roots, bootstrap, and rotation: what stays open (2026-09-17)

The seven-slice bootstrap redesign shipped 2026-09-16 and 2026-09-17
(`docs/devlog.md`, "The kernel with no one to answer to"). Open:

- Deploy: each host needs its kernel wiped and `kaijutsu-server init` run,
  every client binary rebuilt (wire version 2), rc reseeded, and the stale
  `/config/kernel/approval.toml` deleted by hand. Migrate zorak by hand or
  in downtime; keep it simple.
- A model rotating its own seat from inside a live turn archives the
  context that turn is still writing to. Untested.
- `Ctrl+A r` (rotate by prefilled prompt, client follows the successor) is
  TUI only. The app has the prefilled-`kj` pattern but no rotate chord and no
  follow on a rotate result (`docs/input.md`, "The prefix table").
- A pinned structured `kj context switch` still runs the builtin's
  `switch_context`: it validates and saves the addressed context's cwd in a
  throwaway shell, so a context whose cwd has gone missing refuses a switch
  that would never have moved that shell
  (`runtime/kj_builtin.rs`, `KjResult::Switch`). The client follows
  `switched_to` from the result data instead.
- An upgrade can land a kernel on a `gate.toml` the new binary rejects, and
  then every gated shell submission is refused. On moltar 2026-09-21 the host
  file was the older shipped default, naming `[context_type.explorer]` after
  the type became `toolie`. `rc reseed` does not look at `/config/kernel`, and
  nothing at boot says the gate is unusable; the refusal shows only per call
  in the log. Reseed also never removes a retired bucket: the host still
  has `/config/rc/bassist`.
- Amy's 2026-09-21 report that `Ctrl+A` failed from the inline draft after
  `Esc` does not reproduce: `the_prefix_switches_seats_from_normal_mode_mid_draft`
  passes, and every path from the draft reaches `Keys::interpret`. A pty
  cannot stand in for her terminal and mux. `RUST_LOG=kaijutsu_tui::keys=trace`
  records each key and the intent it became in
  `~/.local/state/kaijutsu-tui/tui.log`; run with it and read the log at the
  next occurrence.
- Over a full-screen surface only seat chords and `Ctrl+A a` act; the
  picker, ledger, and prefilled prompts are held because nothing draws them
  there. The hold notice is only visible after leaving the surface.
- Whether seats write their handoff note before stopping is prompt guidance
  (`S00-base.md`), unmeasured. Tune it from what the morning rotation finds.
- `kj::ledger::tests::decision_span_keeps_the_ask_and_deciding_actor_separate`
  is flaky under the parallel test runner and passes single-threaded.
- `mcp::broker::tests::tool_call_spans_keep_requester_actor_and_reviewer_distinct`
  missed both spans in a parallel `tool_` test run; its isolated rerun passed
  (2026-09-17, /tmp/kaijutsu-tool-write-tests.log). Audit tracing subscriber and
  callsite ownership alongside the ledger test; the contributing factors are
  not established.

## What running under a benchmark showed (2026-09-18)

Harbor's ACP runner drove a coder context on a throwaway kernel with
deepseek-v4-flash. Evidence, event logs and the code audit:
`~/exomemory/kaijutsu/coder-early-stop-2026-09-18.md`. Recipe:
`contrib/bench/README.md`. Amy's observation that prompted it: contexts "stop
more readily than other agents". Open, most costly first:

- **An approved command's output never reaches the model.** On an ask the tool
  result says nothing was run; after approval the same ACP tool call completes
  with the real output, but the model only gets the notice built in
  `runtime/approval_resume.rs` ("It has run"), with no output. The next
  inference still believed it was blocked and spent about twenty calls looking
  for its own result: 35 inferences and 1.09M input tokens against 9 and 173K
  for the same task with the commands allowed. Fold the settled output into
  the notice, or make the settled pair visible to the next hydration.
- **A turn can end before its ask is offered.** A mock turn raised an ask and
  ended about 70 ms later; the ask-to-decision round trip measured 337 ms. The
  ask stayed pending, the command never ran, and the ACP client saw no
  permission request. Nothing holds the turn, or `session/prompt`, open for an
  ask the turn raised. `contrib/bench/analysis/classify_run.py` reports these
  as `asks_orphaned`.
- **The waiting receipt is ambiguous.** `runtime/llm_stream.rs` returns "the
  command has not run" with `is_error=false` and `Status::Done`; the ACP
  status is `failed` in one phrasing and `completed` in another. A model
  reading the text stops; one reading the status assumes it ran. The refusal
  remedy (`kaijutsu-types/src/refusal.rs`) tells the reader to run
  `kj ledger allow`, which a model cannot do for itself.
- **Shell is asynchronous by default, and foreground is not a free fix.**
  `mcp/servers/shell.rs` defaults `foreground` to false, so every command
  costs a second `kj wait` call to read. The broker call timeout is 120 s for
  `shell` and 315 s for `shell_write` (`mcp/policy.rs`), and on expiry the
  model gets plain text with no partial output. A foreground default for the
  coder type has to move with a larger `call_timeout` and partial output on
  timeout.
  Evidence 2026-09-22, qwen3.8-flash in both seats: banto and a coder each
  called `read_shell_operation` with operation ids they made up (three
  "no shell operation ... was ever created" errors in one 130 s coder turn),
  and the coder lane a director made with `kj context create` started with
  no working directory ("glob: this context has no working directory set"),
  so its first command ran in `/home/atobey`. Banto's `kj wait <lane>
  --timeout 240` through the `shell` tool hit that tool's 120 s ceiling
  ("Tool 'shell' timed out after 120.0s") and had to re-poll.
- **Any text with no tool call ends the turn** (`runtime/llm_stream.rs`, "no
  tool calls this iteration"). There is no completion command, no check of
  unfinished plan items and no continuation nudge. Direction from Amy: "a done
  signal sounds right"; coder "will almost always be a subagent being driven
  by a banto", so the coder rc can carry the delegated-worker contract and the
  done signal reports to the driving context.
- **The coder stance names yielding as a normal step**
  (`assets/defaults/rc/coder/create/S00-stance.kai`, "Before yielding or
  signing off…"), against one weaker persistence sentence in
  `lib/create/S00-base.md`. A fresh coder seat is about 46,000 input tokens
  before any work.
- **Shell output is capped at 8 KiB**, keeping 1024 B of head and 512 B of
  tail, with the exit code remapped to 3 on spill
  (`runtime/embedded_kaish.rs`, `runtime/command_result.rs`). A failing test
  suite is mostly unreadable to the model.
- **A turn can still spend several output ceilings.** A ceiling stop now
  continues the turn with a notice instead of ending it, bounded by
  `MAX_OUTPUT_CEILING_CONTINUATIONS` (`runtime/llm_stream.rs`); see
  `docs/conversation-session.md`, "When an inference stops at the output
  ceiling". A model that reasons past the ceiling every time therefore costs
  up to that many ceilings before the turn ends anyway. Measure continuations
  per turn and how many of them recovered in the next benchmark arm before
  tuning the bound, and decide there whether the notice should also lower
  `effort` for the continuation.
- **No cumulative token count.** `context_usage` is a last-call snapshot, no
  real run emitted an ACP `usage_update`, and `PromptResponse.usage` is unset,
  so Harbor's token columns are empty. Totals come from the kernel log's
  `LLM stream completed` lines today.
- **ACP has no model selection.** Harbor's runner raises when `--model` is
  passed and the agent advertises none; the model is chosen through the
  agent's own flags meanwhile.
- **ACP `mcpServers` are ignored** with a warning
  (`crates/kaijutsu-acp/src/lib.rs`, `warn_ignored_mcp_servers`), which blocks
  MCPMark and any task that ships MCP servers.
- **File tools:** `read` truncates a line at 2000 characters with no way to
  page within it; `grep` stops at 200 matches without saying how many remain
  (`mcp/servers/file.rs`).

From the first Terminal-Bench 2.0 runs in containers (jobs under
`~/src/bench-work/harbor/jobs/kj-calib-1`):

- **A mid-stream transport failure still fails the turn.** The recorded runs
  carry "error decoding response body" and "Connection closed" after content
  had already streamed into blocks. Retrying there means deciding what happens
  to the partial blocks the first attempt wrote, which the stream-start retry
  policy does not cover; `runtime/llm_stream.rs` deliberately does not retry
  mid-stream to avoid duplicate kernel blocks. Bring partial-block handling to
  the design before changing it.
- **The factory output ceiling is small for effort max.** `max_tokens` 16384
  with effort max means reasoning spends the same budget, which is what
  truncated the `write` call on `regex-log`. `kaijutsu-solo-acp --max-tokens
  <N>` (2026-09-18, `docs/solo-acp.md`) lets a benchmark operator raise it;
  every other caller of the factory default is unchanged.
- **The iteration cap assumes a human is present.** `sqlite-with-gcov` stopped
  at "Paused after 50 agentic iteration(s) (consent: collaborative). Send a
  follow-up to continue". A driven worker has nobody to send one. The cap and
  consent mode want a per-context or per-type setting that a driver can choose;
  today consent is kernel-wide. `kaijutsu-solo-acp --consent autonomous`
  (2026-09-18, `docs/solo-acp.md`) lets a solo kernel's own operator choose the
  wider cap at boot, through the same kernel-wide setting
  (`Kernel::set_consent_mode`) — the per-context row `kj context set --consent`
  writes still is not what `runtime/llm_stream.rs` reads (see its own
  "Consent setting ownership" TODO there), so this remains open for anyone
  who needs it per-context or mid-run.
- **Boot spends about 1.8 s probing an unreachable embedding host**
  (`kaijutsu-server/src/rpc.rs`, "Embedding service unavailable"; the endpoint
  comes from `seed_backends.rs`). In a sandbox that is most of the boot. Give
  the probe a short connect timeout or let a caller skip the semantic index.
- **A process the model spawns can read the kernel's environment through
  `/proc/<pid>/environ`** when it runs as root, which is usual in task
  containers. kaish clears the child environment and `kaijutsu-solo-acp` clears
  its dumpable flag, which stops a same-uid reader but not root. A provider key
  that reaches the kernel by environment is readable there; use a run-scoped
  key in a disposable container.
- **`--consent` on a context is accepted and does nothing.** `kj context
  create --consent` and `kj context set --consent` both write
  `ContextRow.consent_mode` through the shared `ContextConfigArgs`
  (`kj/context.rs:38-40`, flattened into `Create` at `:125` and `Set` at
  `:156`; the write is `db.update_settings` at `:551`). The turn loop never
  reads that row: `runtime/llm_stream.rs:1755` takes
  `kernel.consent_mode().await`, the kernel-wide value, under a TODO at
  `:1751-1754` that names the split and points here. So a per-context consent
  mode is stored, reported back by `kj context info` (`:1011`), and ignored by
  the iteration cap it appears to set. `kaijutsu-solo-acp --consent` is the
  available workaround and sets the kernel-wide value
  (`Kernel::set_consent_mode`, `kernel.rs:1904`), which is why the benchmark
  arms carry a consent mode at all. Decide per-context resolution or retire
  the field; see "Consent setting ownership" above for the constraints.

## Leftovers from the solo-acp state work (2026-09-18)

Smaller, from the same work:

- `kaijutsu-mcp` and `kaijutsu-acp` each carry a copy of the key-flag
  resolution and the personal-key warning. Both are pure and belong beside
  `KeySource` in `kaijutsu-client`.
- `kaijutsu-mcp` names its context flag `--context-name`; `kaijutsu-acp` uses
  `--context-type` and `--character`. There is still no way to run one `kj`
  command against a kernel from a shell without speaking MCP or ACP;
  `contrib/bench/kjmcp.py` fills that gap for the harness.
- Provider HTTPS verifies through `rustls-platform-verifier`, which reads the
  on-disk CA bundle with no bundled fallback. A static binary in an image
  without `ca-certificates` fails every model call.
- Static builds: `contrib/bench/Containerfile.static` duplicates the root
  `Containerfile`'s build stage. Amy prefers static binaries for deployment.
  Put the per-target link flags in `.cargo/config.toml`
  (`[target.x86_64-unknown-linux-musl]`; a blanket `RUSTFLAGS` breaks
  proc-macro crates), build the root image static, export the binaries from
  it, and delete the second Containerfile. Measure musl's allocator under load
  before a static kernel serves real work.

## The uncovered tier does not reach a `KjVerb` ask (2026-09-18)

`gate.toml`'s `uncovered = "allow"` (`docs/gate-policy-tuning.md`, "The
uncovered tier: a sandbox posture") rides the config layers, and an
`Origin::KjVerb` ask never meets them: its one live caller passes
`gate_policy::no_config()` (`kj/cc.rs:112`, `kj cc send`), and
`lower_layers_per_gated_statement` (`kj/gate_policy.rs:810`) returns
`Uncovered` for that origin because there is no planned program to key on.
So a sandboxed kernel still asks on those. Either those callers should load
the file and the evaluator should let the tier decide a plan-less ask, or
the boundary stays and the `kj cc` help says so. No benchmark has hit it
yet.

## The uncovered tier: three seams left open (2026-09-18)

`docs/gate-policy-tuning.md`, "The uncovered tier: a sandbox posture".

- **No broker-level PreCall test loads the tier.** The evaluator is pinned
  in `kj/gate_policy.rs` and the shell gate in `kj/ledger.rs`, but nothing
  asserts that `evaluate_phase_with_mode` (`mcp/broker.rs`) skips hooks for
  a program the tier allows, which is the behavior an operator feels most.
  It needs a test in broker.rs's own test module.
- **A commandless statement carries no tier stamp on `KJ_TOOL_PLAN`.** The
  stamping loop zips `stmt.plan.commands` with the JSON twin
  (`mcp/broker.rs`, `run_kaish_hook`), so a statement with no command — an
  assignment, an exit, a `[[ ]]` test — gets no `tier` field. The evaluator
  still decides it, and a hook that reads tiers per command finds nothing to
  read, so this is cosmetic today. It becomes real if a hook ever scores
  statements rather than commands.
- **No startup line says the posture is on.** `kj ledger rules` states it,
  and every auto-decision names it in its durable row, but a kernel booted
  with a copied-in sandbox `gate.toml` says nothing. The place for one is
  `create_shared_kernel` in `kaijutsu-server/src/rpc.rs`, in the loop that
  seeds and mounts each config tree and already logs where a non-default
  tree came from; the line would load `gate.toml` there and name the
  sections setting `uncovered = "allow"`. A boot-time load also has to
  decide what an unloadable file does at boot, where today nothing reads it
  until the first shell submission.

## The isotest harness could use the uncovered tier (2026-09-18)

`crates/kaijutsu-isotest/tests/common/mod.rs:65` keeps `HARNESS_ROOT_ALLOW`,
a hand-listed allow tier for the harness's own setup commands, because an
unanswered ask hung the run. The harness tests process isolation and VFS
protection, not the gate, and it throws its kernel away per run — a
`[context_type.root] uncovered = "allow"` section would replace the list and
stop it drifting as setup commands change. Left alone here: that crate is
another lane's this week.

## Split admin grants between `root` and `director` (2026-09-16)

`director` is banto's model seat and still carries the whole operator grant
set (`assets/defaults/rc/director/create/S10-binding.kai`), which `root`
now also holds. Decide which grants banto keeps (likely drive, fork, drift,
operator) and which belong to roots only (likely `admin`, `config-write`,
`system`). Amy chose the split on 2026-09-16 and left the grant list open.

## Identity audit: what stays open (2026-09-15)

Amy: *"are all the gate sites using the right identifiers to check who it
is?"* Scan by the lead plus a kaibo (deepseek) audit against
`docs/approval-identity.md`; every line below was re-read by the lead. Full
notes: `~/exomemory/kaijutsu/identity-gate-audit-2026-09-15.md`.

The sheet-level `accountable_to` that shipped at noon is gone, replaced by
the walk up the context forest ("Roots, accountability as a runtime
relation"). Reviewer resolution, delegation, answering, cancel/escalate,
redemption, turn identity, `require_cap`, the facade gate, and every
draft/shell RPC read the identifier the doc names. Open:

1. `authorBlock` takes `principalId` from the request (`rpc.rs:8687`), the
   only RPC that does; documented as shared-trust in `kaijutsu.capnp:2232`.
2. The hook listener authors under `for_agent_session` with a `system()`
   fallback (`hook_listener.rs:949`); the bridge-identity design in
   `docs/character.md` replaces it.

Verified by `crates/kaijutsu-server/tests/user_input_identity.rs`: a human
with nobody responsible above her runs a gated shell command, the ask
snapshots actor == reviewer, and her own `kj ledger allow` confirms it and
executes the command. That is the self-confirmation the walk settles on;
the earlier unanswerable Pending row, and the in-band refusal that briefly
replaced it, are both gone.

Seen while writing that test: the test's blanket Ask hook re-gated the
`kj ledger allow` typed into the same context, so the test answers from a
hook-free context. The shipped evaluator exempts `kj ledger` as a whole verb
(`docs/gate-policy-tuning.md`, "Builtin tier"), so this is likely a test
artifact; a test that pins the exemption under a custom hook would settle it.

## The compose draft is the player's alone (Amy, 2026-09-15)

Amy: *"the draft should never have a path for the model to reach it. we may
add things to the apps to make testing easier but that draft has to be user
only for some of our assumptions about safety to hold up."* And: *"we'll
allow the mcp for now, we use it a lot for testing, but we should mark it for
removal later, y'all have drive and drift for talking to each other."*

The draft is the per-principal input block written by `edit_draft`
(`crates/kaijutsu-kernel/src/block_store.rs`). The kernel editor cannot open
it: `resolve_editor_target` (`crates/kaijutsu-kernel/src/editor.rs`) binds to
file-backed blocks only, and no dedicated `kj input` verb exists. The intended
contract is no model VFS path for the draft and no editor session over it.

The dedicated `/v/input` mount and generic model routes (`/v/docs`, `kj`
block commands, search/synthesis, and MCP block reads) now exclude drafts.
Generic status commands cannot create or promote them; historical reads
refuse draft-era content even after submission. Client compose and feed
queries retain draft access. Regression coverage includes distinct requester
and performer identities in both model-shell flavors.

The MCP bridge's `read_input`/`write_input`/`edit_input`/`submit_input` tools
are gone as of today. A model that wants another player's attention uses `kj
drive` and drift instead. What remains open: the facade gate at the wire
(`facade:edit_input`/`facade:submit_input`) reads the context binding and
cannot tell a human connection from a model principal holding its own
credential, so the assumption that "a user block was typed by a human" is
not yet enforced at the wire. That is the next design conversation.

## Thinking folds to a summary line once the player has moved on (Amy, 2026-09-12)

Amy: *"I'm watching y'all work, and you're thinking, I often read/scan it
because I'm curious, but it takes up a lot of space, so I'd like to collapse
it but keep *something* visible. so I want it to collapse in the app on
hydrate or a second or two after the thinking ends and I've likely moved on.
I think in the tui, the thinking preview thing would collapse to the
summarized line on the terminal history."*

**Shipped 2026-09-12, kernel/wire/tui half.** `kaijutsu_types::summarize_thinking`
(sentence boundary needs trailing whitespace, 120-char cap, markdown
markers stripped); `summary` on `BlockSnapshot`/`BlockMetadata` and the
kernel's `BlockContent`, journaled as a full snapshot like `stderr`;
capnp `BlockSnapshot.summary @45`, `BlockMetadata.summary @9`; set in the
server's `ThinkingEnd` arm before the status flips to Done; the tui stub
shows it; `kj block inspect --json` prints it. **Remaining: the app's
per-viewer fold**, handed to the moltar session through the exomemory
daily (fold on hydrate, fold a second or two after `onTurnCompleted`, an
explicit toggle pins open, never `set_collapsed`).

**Before.** The kernel keeps a durable `collapsed` flag per block, flipped
only by an explicit toggle (`set_collapsed`, `CollapsedChanged`); nothing
collapses on its own. The app shows thinking expanded until toggled. The tui
holds the turn's thinking in its pane and, when the block completes, prints
one stub into scrollback: `▸ thinking · N lines · <first line>`
(`present::thinking_stub_line`). The first line is a poor stand-in for a
summary. No block carries a summary today; `kaijutsu_index::synthesis`
has an extractive `best_sentence` that could produce one without a model.

**The plan.**

- **The kernel derives the summary, once, at completion.** When a
  `Thinking` block settles, the kernel computes one line and stores it on
  the block, published on `BlockSnapshot` as a new field and through the
  change feed. Start extractive (`best_sentence` over the block, capped),
  which is synchronous and free; a model-written summary, lfm2d or a
  flash-tier cloud model, is a later swap behind the same field and can be
  driven by rc. Display only: the summary never enters hydration.
- **The app folds locally.** A completed thinking block reads as collapsed
  on hydrate, and a block that completes while watched folds a second or two
  after it settles, showing the summary line. This is per-viewer
  presentation state, not the durable `collapsed` flag: one player folding
  must not fold a sibling's screen. An explicit toggle still works and pins
  the block open. Timing constants live in one place and are relative to
  settle, not to wall clock at hydrate.
- **The tui's stub uses the summary.** Same stub, summary in place of the
  first line, first line as the fallback when no summary exists. The stub
  prints when the block completes, as now; an extractive summary is ready
  by then. A model-written summary would arrive later, and the stub cannot
  be redrawn once printed, so that path either delays the stub (risking
  document order against a fast answer) or lands the summary elsewhere. Do
  not ship a slow summary source without answering this.

**Decided** (Amy, 2026-09-12): *"yes extractive now, model later. agreed no
on durable flag, for now anyways. from turn end I think."* So: the summary
is extractive in the first cut with a model source as a later swap; the fold
never writes the durable `collapsed` flag; the app's fold delay counts from
the turn ending, not from the block settling, which also matches the tui
pane's lifetime.

## The player's edge: what is still open (2026-09-13)

The kernel, wire, `submit` rc verb, shipped example, and the tui landed on
2026-09-13 (`docs/prompts.md`, "The submit verb"; `docs/devlog.md`, "The
message that knew where the player was looking"). Left:

- **The Bevy app sends no edge.** `submit_input` still compiles and sends
  none. The app's edge is the newest block its viewport had shown at Enter,
  older than the log tail when scrolled up, with the character count when
  that block was still streaming. `ActorHandle::submit_input_with_edge` is
  the call. Handed to the moltar session through the exomemory daily.
- **No type links the example.** `lib/submit/S10-edge.kai` renders the
  excerpt; a type opts in by symlink into its `submit/` directory. Decide
  which shipped types should link it once it has been watched live; the
  ordinal and percentage experiments Amy named are further scripts against
  the same variables, not kernel work.
- **The notification lands after the message.** Hydration reads the user
  message, then "the player wrote the message above while looking at…".
  If a model reads the reference late, move the script's block before the
  input block with `--after`; the facts already carry both ids.
- **Every chat submit writes a ledger run row**, even for a type with no
  `submit/` directory: `start_run` fires before the script list is loaded
  (`rc/mod.rs`). Consistent with the other verbs, but submit is a
  hotter path than create or fork. Skip the row when no script exists, or
  accept it once measured.
- **The edge never rides the change feed.** The promotion emits
  `StatusChanged` and `MetadataChanged`, and `BlockMetadata` has no edge
  fields, so a live replica's copy of a promoted block reads no edge until
  it refetches. Nothing reads a stored edge from a mirror yet. From the
  kaibo review (deepseek, 2026-09-13).

## Async completion recovery follow-ups

- Completion during a model's final inference can reach the durable mailbox
  after that request was sent. The current automatic wake check skips an
  in-flight turn. Reconcile unread completion notifications when the turn
  yields, using its mailbox cursor, so a late result within the continuation
  window does not wait for the next explicit drive. Do not refresh the window
  or replay already consumed results.
- RPC PostCall hooks can replace output/status while the durable exit code
  still records the executed command. Define separate command outcome and hook
  outcome before changing job summaries to infer a synthetic exit code.
- Shell operation inspection currently uses bounded result/block output.
  Integrate kaish job streams and spill references for live, complete output
  retrieval without invalidating pagination offsets.
## Config defaults ARE the config; the VFS path proxies XDG (Amy, 2026-09-12)

Design direction for `theme.toml` and the other `/config/kernel` singletons
(`mcp.toml`, `gate.toml`, and the client configs). Today a file is seeded from
an embedded snippet on first boot, and there are three copies of the theme:
the app's compiled fallback (`ui/theme.rs`), the kernel's embedded seed
(`config_seed.rs` `DEFAULT_THEME` from `assets/defaults`), and the live host
file. Amy's shape:

- The compiled defaults become Amy's actual settings, so the common case needs
  no file on disk ("for now" — fine in a no-outside-users learning space).
- Stop seeding a snippet. A host file exists only when someone writes one to
  override; `seed_entries_into_dir` no longer plants theme et al.
- The defaults are printable, not shipped: a `kj config` verb emits the
  effective defaults as TOML so a human can capture a starting point to edit.
- The VFS path `/config/kernel/theme.toml` stays the interface characters and
  tools use (the raw XDG path is never exposed to them) and proxies to the XDG
  host file. Copy-on-write from defaults: an absent file reads as the
  synthesized defaults, a write creates the override.
- The kernel's defaults become the one source; the app's pre-connect fallback
  shrinks to a minimal neutral default instead of mirroring the whole theme.

Interacts with `docs/color.md` (theme ownership — this keeps the kernel the
owner but makes the default the body), the "theme changes never reach a
running app" entry (the read is already live per connect; only the write side
and the seed change), and the theme-lane / client-local question (whether the
theme should move to the `/config/client` cascade since it is per-display
presentation). Sequence AFTER the pending origin/main pull, which touches the
approval/config area.

## The approval sheet and ribbon have no dim behind them (2026-09-12)

The ask sheet and ledger ribbon float over the conversation or room at full
surrounding brightness, so they compete with the transcript instead of
reading as a popup. A first attempt at a full-frame scrim node (a childless
`Node` with a translucent `BackgroundColor` at a `GlobalZIndex` just under
the panel) did not render: the entity existed at full viewport size with the
right color, but its `ViewVisibility` stayed 0 while every content panel at a
`GlobalZIndex` drew normally. A childless background-only UI node behaves
differently here; the working panels all carry a child surface. Reproduce
with `world.get_components` on the scrim entity (Visibility Inherited,
InheritedVisibility true, ViewVisibility 0). Removed rather than shipped
non-functional. When revisited, either give the scrim a drawn child or use
the same surface machinery the panels use; the conversation-walls work may
restructure this layer anyway.

## The app runs as Amy, so she cannot answer her own gated asks (2026-09-12)

**Re-check on the live kernel before acting.** This predates "a root confirms
itself" (`3d69e765`, 2026-09-15): `can_review` no longer requires
`principal != actor`, and `a_root_actor_confirms_its_own_ask` in `kj/gate.rs`
covers the path. If Amy's character is a root with no reviewer, the ask below
may now be answerable. The open question is then whether the app still gets a
distinct performer character for a clearer audit record.

Verified live on 2026-09-12. A `shell_write` in the app that trips the `lfm2d-advisory`
gate raises an ask whose requester, performer, and reviewer are all `amy`
(the app authenticates as Amy). `AskDetail::can_review` requires
`principal == reviewer && principal != actor`, so it returns false: the ask
sheet correctly shows "not yours to answer — the reviewer runs: kj ledger
allow <id>" and offers no decision keys, and `kj ledger allow|deny` from the
app is refused for the same reason (self-approval is barred by the
2026-09-12 approval-identity work). The ask is then stuck pending until it
expires. So the app's own shell cannot run any gated command today. The fix
is an identity decision, not app UI: the app should attach as a distinct
performer character (`kj context create --as`, or a dedicated app
character) with Amy as the reviewer, or a non-Amy reviewer must be
configured for the app's context. Coordinate with the approval-identity
work (commits dcb5fc8b, fd27b822). Probe ask `01a096d2-358d-…` was left
pending by this check; it is fail-closed and will expire.

## BRP-injected input lands one request late (2026-09-12)

A key sent with `brp_extras/send_keys`, or a state change through
`world.insert_resources`, takes effect only when the next BRP request
arrives: a screenshot batched with the key shows the frame before it. The
app's winit loop is reactive (100 ms focused, 500 ms unfocused,
`main.rs`), and the injected event evidently does not wake it. Drivers
work around it by following every input with a cheap read. Look at
whether `bevy_brp_extras` should request a redraw after injecting, or
whether the app should run continuous updates while BRP is enabled.

## `kj block list --json` loses its command-specific metadata (2026-09-12)

`BlockCommand::List` declares `--json` and builds a `{context_id, count,
total, blocks}` object, but `KjBuiltin::execute` treats every bare `--json`
as kaish's global output flag and removes it before block dispatch. The
dispatcher therefore receives `json = false`, returns its for-loop block-id
array as `.data`, and kaish renders that array. A live `kj block list
--context approval-identity-probe --json` consequently prints block ids
instead of the declared metadata object. The direct `kj/block.rs` tests call
the dispatcher and do not cover the embedded-kaish path; add that boundary
test when separating global output formatting from command-specific JSON
shapes.

## A streaming turn still writes ~2 MB/s and logs ~160 lines/s (2026-09-11, perf)

The fsync storm is fixed and measured (`docs/devlog.md`, "The kernel that
fsynced every word"): `synchronous = NORMAL` and one transaction per
journaled op took a streamed delta from about 200 KB of block-layer writes
to 13 to 15 KB, with `wchar` equal to `write_bytes`. What remains is
payload and logging, in the order to try:

- The DEBUG `llm` spans log every `TextDelta`/`ThinkingDelta` event, about
  160 lines/s during a deepseek-v4-flash turn, and journald writes each.
  The unit runs `RUST_LOG=info` since 2026-09-11 11:18 EDT (matches
  `contrib/install-systemd.sh`), and INFO is a turn-level log now
  (`docs/devlog.md`, "The kernel that fsynced every word").
- Each delta was about 14 write syscalls and 13 KB: WAL pages for the
  `oplog` row and the `contexts` activity touch. The touch is throttled to
  once a second per context since 2026-09-12 (`ACTIVITY_STAMP_INTERVAL_MS`,
  `block_store.rs`); measure the per-delta figure again on the next bounce.
  Batching deltas into fewer op rows is a design conversation, since the
  op row is the durable unit the change feed replays.
- The "idle" 50–100 KB/s measured today is unverified: the audio
  inventory store is in-memory (`audio_inventory.rs`, no db), and every
  sample so far overlapped either Amy's turns or this session's own hook
  mirror (each Claude Code tool call becomes blocks in the session
  context). Measure again with no client active; the per-thread `io`
  attribution (`/proc/<pid>/task/*/io`, threads named `kjutsu-rpc-<id>`)
  says which connection writes.
- Untested from the diagnosis: `PASSIVE` instead of `TRUNCATE` for
  compaction's checkpoint, and `chattr +C` on a rebuilt db. The 913 MB db
  has 16,147 extents.

## The cached mailbox never re-reads a block it has seen (2026-09-11)

`ConversationMailbox::catch_up` (`llm/mailbox.rs`) folds unseen block ids
and keeps a `seen` set; nothing invalidates an entry when a seen block
changes. The gate-resume fill evicts the cache (`ConversationCache::evict`,
rpc.rs), which covers approvals. Same class, not covered (Kaibo, deepseek
cast, source reading only):

- `kj block edit` / `kj block append` on a block a cached mailbox already
  folded: the next turn reads the old text. The kj verbs live in the
  kernel crate and cannot reach the server's cache.
- `kj stage exclude` on a warm cache: `excluded` is read only at fold time
  (`hydrate.rs`). Narrow, since the documented flow is exclude then fork.
- Draft promotion: `submit_draft` turns a `Draft` block `Done` under the
  same id; `catch_up` adds every block to `seen` even when the fold skips
  it as ineligible, so a draft that was open during another principal's
  turn stays invisible to that mailbox for good. Not reproduced.
- Overlapping prompts on one context: a second turn that called
  `get_or_create` before the fill holds the old `Arc`, waits on the mutex,
  then folds against the stale `seen` set. Needs two prompts in flight on
  one context; the interactive spawn sites do not check `turn_in_flight`.

One mechanism would cover all four: a change feed the mailbox subscribes
to, or a per-block version the fold compares. Both are design
conversations under `docs/conversation-session.md`.

## Monochrome TUI copy-mode indicators need a contract

With inherited `NO_COLOR=1`, terminal probes lose the marked-row background
and reader inverse styling. Clearing that variable makes the Space/copy-mode
probe pass. Crossterm 0.29.0 formats disabled colors as empty strings inside
`ESC[...m`, yielding an attribute reset that can erase inverse as well as
color. Decide the monochrome presentation and preserve a visible reader and
selection without overriding the user's color preference. The normal
color-dependent PTY fixtures now remove inherited `NO_COLOR`; dedicated
monochrome fixtures can set it explicitly. No upstream posting was made.

## transcript_plan clones every block per frame (2026-09-13)

`render::transcript_plan` clones each block of the current context and
builds a speaker string on every frame; the wrap cache keeps the wrap
itself to a hash and a lookup. Measured 2.4 ms per frame in release over
5,000 blocks (`render::a_warm_frame_over_five_thousand_blocks_costs_a_screenful_not_a_context`,
50 ms tripwire), so no rendered-lines cap was added. If a long context
ever feels slow, stop cloning in the plan pass before adding a cap.

## Arrows cannot scroll from inside a wrapped one-line draft (2026-09-13)

`Up` leaves the tail only from the draft's first visual row and `Down`
is inert only on its last (`run::tail_key`, `Compose::cursor_visual_row`),
so inside a long wrapped draft the arrows belong to the draft. But
modalkit's `Up` is a logical-line motion with nowhere to go on a
one-line draft, so on a long wrapped draft the arrows cannot scroll the
transcript at all; `PageUp` and `Ctrl+A [` still can. The fix is vim's
`gk`/`gj` display-line motion for `Up`/`Down` in the draft, which
`kaijutsu-editor` does not expose yet. Pinned by
`run::a_wrapped_one_line_draft_keeps_its_own_arrows`.

## The open picker's cursor moved under a refresh in a probe (2026-09-13)

`a_placement_verb_moves_the_row_while_the_picker_stays_open` failed in 3
of 8 full-suite runs during slice 2 and never alone. The captured screen
showed the verb landing on the filtered, empty ACTIVE section instead of
the fork's row in RECENT, so no placement ran. The probe now waits for
the `›` cursor to sit on the fork's row before each verb and has been
clean since. The cause is not proven: `PickerModel::refreshed` is meant
to keep the cursor on its context across a rebuild (`follow`), so either
the Tab count computed from an earlier screen was stale by the time the
keys landed, or `follow` loses the row when the filtered section it is
in becomes empty for one round. Reproduce under load with the wait
removed before touching `follow`.

## A pty probe flakes under load: the scrolled place after a switch (2026-09-14)

`a_scrolled_context_comes_back_scrolled_after_a_switch` in
`crates/kaijutsu-tui/tests/terminal_fit.rs` fails its `readout == place`
assertion ("the reader landed somewhere else") when the full pty suite
runs on a loaded box: twice now under the whole suite, and clean every
time alone (3/3 on 2026-09-13, 3/3 on 2026-09-14). The scrolled position
is restored by block and content line, so the suspect is a timing window
between the switch back and the transcript re-anchoring while a late
hydrate delivery is still landing, not a wrong anchor. Reproduce under
load before changing the anchor logic; the probe passes alone, so a fix
that only changes the probe's waits is suspect too.

## An editor edit publishes after it releases the lock (2026-09-14, inherited)

`Kernel::editor_keys_checked` and `Kernel::editor_insert` compute the new
state under the sessions lock, release it, run `mark_dirty` (a database
write), and only then `publish_editor_state`. A reconciler merge landing in
that window publishes the merged state first and the older snapshot second,
so a renderer ends on a buffer missing the peer's edit until the next push.
Not new with the paste slice; `editor_keys` has had the same window. Fix
is to publish the state read under the lock before the database write, or
re-read after it. Found by the kaibo review of the paste slice.

## The tui takes the kernel-wide firehose and blocks on one RPC per keystroke (2026-09-10)

`crates/kaijutsu-tui/src/bridge.rs:78` spawns the actor with
`scope_blocks_to_context: false` on purpose (the tui is a mux), so with
500+ live contexts it receives every block event, including
`report_audio_inventory` bumping a revision every 10 s. `run.rs:782`
(`mirror_ops`) awaits one `edit_input` per vi op with no per-RPC timeout —
the only timeout is `connect_timeout` (`main.rs`) — so under IO stall
typing blocks rather than timing out. Fix is the same as the app's entry
above: `watch_contexts` for the set the mux shows, and one `edit_input` per
keystroke batch. The "kernel events lost" notices are in the tui's log file
(`docs/tui.md`, "Every way out restores the terminal") since 2026-09-13.

## kaish children inherit the kernel's priority (2026-09-10)

A host command run from a context (`cargo test` in a coder, `git` from
the operator's seat) is spawned by kaish inside `kaijutsu-server.service`
at the kernel's own nice (kaish-kernel 0.17.2 `spawn.rs:205` builds the
`Command` with no priority), so a coder's build competes with the kernel's
own threads head-on, and raising the service's `CPUWeight` raises the
build with it. `docs/operating.md`, "Builds beside a live kernel" carries
what can be done from outside today.

The policy is ours; the seam is kaish's. Kaijutsu never sees the
`Command`, a pre-exec point, or the child pid — the embedder sets only
`allow_external_commands` and `PATH` — so the request for a `nice` field
or a `before_exec` hook on the spawn request is filed in
`~/exomemory/issues/kaish.md`, "A pre-exec seam on the spawn request".
Once it lands, `ExternalExec` in `runtime/embedded_kaish.rs` (the one
exec authority, `runtime/context_shell.rs`) grows the policy, and it should be
pluggable by host rather than a table in kaish: nice is the portable
floor; Linux can add cgroup placement (write the pid into a prepared
cgroup, or `systemd-run --scope` with `CPUWeight`/`IOWeight`) for IO and
memory control nice cannot give; macOS has no cgroup and marks a process
background with `setpriority(PRIO_DARWIN_PROCESS, …)` / `PRIO_DARWIN_BG`,
what `taskpolicy -b` does. Start with nice 10 for exec-granting seats and
measure before adding a second mechanism.

## A queued startup failure records two error blocks

`spawn_admitted_turn` (`runtime/llm_stream.rs`) inserts a pre-stream error
block for identity, review, provider, tool-definition, and system-prompt
failures and returns `Err`; `queue_startup` (`runtime/turn_request.rs`) then
calls `report_failure`, which inserts a second block ("turn failed to run for
this context: ...") under the requester. Both `request_turn` and
`prompt::submit` go through the queue, so one refusal shows twice after the
prompt. Decide which side owns the block; a test that drives `request_turn`
with a missing performer and counts `Status::Error` blocks pins it. Found by
kaibo (DeepSeek) reviewing the performer-first change, 2026-09-22.

## `decision_span_keeps_the_ask_and_deciding_actor_separate` flakes under thread contention

`kj/ledger.rs`, the test installs a custom `tracing::Dispatch` with
`.with_subscriber(...)` for one future; run multi-threaded beside other
ledger and context tests it panicked twice at "decision span", and passed
in isolation and under `--test-threads=1`. Global or thread-local tracing
state races with the other tests' subscribers. Seen 2026-09-22 by a lane
running the kernel suite under host load.

## Restart tests stop the server task, not the first kernel

`tests/common/mod.rs` `StateDirServer::stop` aborts the `spawn_local` task
and awaits it, which releases the data-directory lock, but the first
kernel lives on: the editor reconciler thread (`rpc.rs`, spawned in
`ssh.rs` after the kernel) owns an `Arc<ServerRegistry>`, and the beat
scheduler thread owns an `Arc<Kernel>`, so `SharedKernelState::drop`
never runs and the runtime pool, roster loop, SQLite connection, and beat
thread of the first kernel stay live while the second boots over the same
files. The restart tests in `context_label_resolve.rs` and
`context_origin_host.rs` therefore still run two kernels in one process.
A fix takes the kernel handle through `run_on_listener_with_kernel_sink`
and runs the settle steps in `stop()`; the two OS threads still cannot be
joined in-process. Found by kaibo (DeepSeek) reviewing 730a9ed8, 2026-09-22.

## Parentless contexts outside the root consoles

`kj context create` and `kj fork` always name a parent (fe187fe5), but
these paths still insert `forked_from: None` rows: `kj context scratch`
(`kj/context.rs`, `context_scratch`), the handoff log (`kj/handoff.rs`,
already filed under "Handoff logs are parentless"), the beat's score
context (`kaijutsu-server/src/beat.rs`), the cold-start document bootstrap
(`kaijutsu-server/src/rpc.rs`, `bootstrap_discovered_context`), the drift
queue and lost+found (`drift.rs`, `kj/drift.rs`, via
`insert_well_known_context`). Each is its own tree in the forest with no
root character above it, so a reviewer walk from inside one ends at nobody.
Decide per path: parent it under the caller's lineage root, or state that
it is a registry-owned well-known context that never hosts a gated turn.
Found by kaibo (DeepSeek) reviewing fe187fe5, 2026-09-22.

## A `--env KEY=VALUE` argument drops `kj context create` out of its allow tier (2026-09-10)

From an `mcp` seat with `[context_type.mcp] allow = ["kj context create"]`
live, `kj context create x --type director` auto-allowed and
`kj context create x --type director --env KJ_CHARACTER=banto` escalated
(ask `01a08d9a-6514`). A `gate_policy` unit test of the same
statement against the same config returns Allow from the context_type
layer, so the evaluator is not where it drops: look between the broker's
PreCall (`mcp/broker.rs`, `evaluate_planned` over its own plan of the
command) and the hook path that raised the ask, on the running binary
(`04538700`).

## Optional rc reads collapse failures into missing-history text

`assets/defaults/rc/lib/create/S16-handoff.kai` and `S17-predecessor.kai`
guard reads with `|| true`. A missing sheet or predecessor is optional, but a
storage/read failure takes the same path and may be reported as absent history.
The scripts also swallow notification-write failures and then print "injected".
Distinguish legitimate absence from failed reads/writes and report the actual
outcome without making optional memory a prerequisite for context creation.
`context info` also discards character lookup errors with `.ok().flatten()`,
so rc can mistake an unreadable performer for an unassigned context.
The performer-selection tests cover successful handoff injection; failure
classification needs its own fixtures.

## A delegated coder inherits the lead's morning window (2026-09-10)

`coder/create/S15-recall.kai` and `S16-handoff.kai` inject the fleet memory
index and the calling character's last twelve handoff notes into every
coder. A coder driven on a two-file fixture read the lead's note "no ask
expected" and reported the lfm2d escalations it met as a possible
regression — a concern the note caused, not the task. The broad loadout
also lists 96 tools in `<situation>`, 45 of them `bevy_brp`. Both are
per-turn cost on a delegated lane that needs neither; whether a delegated
coder should run the same create lifecycle as a character's own seat is a
design question (`docs/delegated-coder.md`, `docs/character.md`), not a
patch to the scripts.

## A failed approval-executed `shell_write` leaves an empty tool result (2026-09-10)

`gate-resume` ran an approved `python -m unittest -v .` (exit 1,
`err_len=1174`) and the resulting block (`2d25fb02#37` in
`rc-review-coder-ds`) reads as an empty `[error]` tool result under
`kj block read`; the model still saw the stderr, so the text rides in the
tool envelope and not the block body. Same family as "A tool result's
shape still depends on whether its body is empty" below, and the tui item
"a ToolResult block must show command OUTPUT" in `signoff.md`.

## Context `--system-prompt` settings are inert (2026-09-10)

`ContextRow.system_prompt` is still stored by `kj context --system-prompt`,
`kj preset`, and one fork copy path. LLM assembly and `kj context prompt` now
use only accepted `Role::System`/`BlockKind::Text` sections plus runtime facts,
so the stored value never reaches a model. Decide and implement removal or an
explicit migration; do not leave a second apparent prompt owner.

## Fork consistency across metadata and document commits (2026-09-10)

Kaibo's `crusoe` review of the prompt changes identified two adjacent fork
issues. Code inspection confirmed the ordering; no DB fault was injected.

- Compact forks capture context type, cast, workspace, and player before the
  model call, while `insert_forked_context` copies environment and bindings
  afterward. The source block-version guard cannot detect concurrent metadata
  changes. Choose one configuration snapshot or reject a change before child
  publication; test a source metadata update during a delayed mock response.
- Fork initialization spans document copy, context/env transaction, cwd override,
  edge insert, and router registration. Failure before the context transaction
  can leave an orphan document; failure afterward can leave a partially
  initialized context. The upfront label check does not cover a label claimed
  concurrently during summarization. Define atomic initialization or cleanup,
  with fault-injection coverage across full, filtered, and compact forks.
  Interrupted-writer cleanup now returns an error naming the copied child;
  its statuses and explanation are atomic, but the earlier document copy
  remains. A fault test pins the caller's refusal and unchanged parent.

`abandon_open_blocks` closes Running/Waiting, leaving Pending untouched. Normal
tool calls start Running, but `kj block status` can set Pending explicitly.
Define what a copied Pending tool call means before adding queued execution to
forks; the model's repaired wire pair does not change that durable status.

## The lfm2d gate escalates `kj handoff note` from the MCP shell (2026-09-07)

`kj handoff note` and `kj context create --type coder` from an `mcp` seat
raise an lfm2d advisory ask scored `escalate` even at 0.76-0.82
`situation-normal`, because the seat's `LFM2D_BENIGN_LABEL=informative` is the
only passing label, and same-seat answer is refused — every note needs a
second seat.

**Decided (Amy, 2026-09-08): fix it through the general gate-policy
mechanism**, not a one-off exemption. `docs/gate-policy-tuning.md` (designed
2026-09-08, unbuilt) lists `kj handoff note` as its first tuning-pass entry in
the global allow tier and names this issue by title to close when slice 5
ships. Delete this entry then.

## A client can resume into an archived context (2026-09-09)

Sequence from the journal: an MCP session's context was archived by another
process while the session was attached; a second MCP instance created a new
context under the same label; the first session kept working in the archived
one, because archiving does not evict a context from the in-memory registry,
and it re-joined the same archived id after the next bounce. The heal now
registers an archived row as archived, so the re-join no longer collides
with the live holder of the label (`register_with_state`, `drift.rs`). What
remains open is whether `joinContext` on an archived context should redirect
the client to the live holder of its label, or refuse so the MCP re-registers
a fresh context. Joining still permits history inspection; new shell and model
requests now refuse archive at admission. Choose reconnect behavior without
silently retargeting requests intended for a retained context.

## An ask's `exec_source` shows `none` for a positional the executor ran correctly (2026-09-09)

`kj ledger show 01a0864f-a5c1-…` for a hook-origin ask on `kj preset
remove nonexistent-zz` prints `statement: … {"command":"kj preset remove
nonexistent-zz"}` and `exec_source: kj preset remove none`. The journal
shows gate-resume executed `"kj preset remove nonexistent-zz"`, so what
ran is right and only the stored text is wrong. `kaish_kernel::plan_program`
renders the command faithfully (checked in a throwaway test);
`hook_gate.rs`'s `shell_source` stores `command.trim()`; the ledger crate
stores and reads the column verbatim. Where `nonexistent-zz` becomes
`none` is not yet found. Same probe: the lfm2d advisory both auto-allowed
the command (ask `…a5a5…`, `decided: auto_allow`) and escalated it as
data-critical 0.539 (ask `…a5c1…`); two ledger rows for one statement.

## Character: two hand tasks for Amy (rollout in `docs/character.md`)

The slice rollout itself is `docs/character.md`'s to carry (currently: 0a/1/2
built, 4 shipped 2026-09-06/07, 3/5-8 remain — read the doc, not this entry).
Two operational tasks that doc's mechanism supports but nobody has done:

- Bind a spare key (`kaijutsu-server add-key <pub> --as amy`) as lockout
  insurance.
- Decide what to do with seven characters: `hajime` plays no contexts and is
  safe to retire; consolidating the other six means rebinding keys onto one,
  and retire takes a character's contexts with it.

## The MCP `shell` path applies no size limit to its envelope (2026-09-04)

The in-kernel `shell` result is bounded by the broker's per-instance
`max_result_bytes` (`mcp/broker.rs:1745`). The external stdio path has no
equivalent: `ShellCompletion::to_tool_result` (`kaijutsu-mcp/src/lib.rs`)
hands `CallToolResult::structured` the whole envelope with no truncation, so a
command with 10 MB of stdout ships 10 MB. Whether the MCP path should carry
the same budget — and where it would read one from, having no per-instance
policy — is open.

## A tool result's shape still depends on whether its body is empty (2026-09-04, latent)

`Kernel::call_tool` (`kernel.rs:677`) still substitutes the pretty-printed
`structured` payload when the flattened text body is empty. Correct for a
structured-only tool; a hazard only for a tool that returns both a text body
and a structured payload where the body can be empty. No tool has that
property today (`bindings_builtin`/`hooks_builtin`/`resources_builtin` all
return `ToolContent::Json` unconditionally, re-verified) — latent, not live.
Fix if one ever grows the property: decide the shape from the tool's declared
output, not from emptiness.

## A recovered document's first op races for seq 1 (2026-09-04)

`create_document_with_block` (`block_store.rs:479`) sets `next_journal_seq` to
1 inside the DashMap guard on the fresh-row path — no window. Its recovery
branch (`DuplicateDocument`, line 555-561) cannot: it journals the op after
the guard releases (line 575, `self.journal_op(context_id, payload)?`) with
`next_journal_seq` still 0, since only the `Ok(())` branch (line 549) sets it.
A concurrent writer reaching the document in that window claims seq 1 and the
block-creation op lands behind it at seq 2 — a replay that applies an edit
before the block it edits exists. Fix: reserve the seq at entry-construction
time; `journal_op` uses the pre-reserved number instead of deriving one — a
change to the journaling contract.

## The scorer cannot see a redirect, only the exemption can (2026-09-01)

Closing the `--help`/`ledger` redirect hole (`9f426c9c`) made those reach the
classifier instead of skipping it — but the classifier never sees the
redirect: `clause` (built in `items_filter`) deliberately excludes redirects,
so `kj block list > ~/.bashrc` still auto-allows on a clause reading
`kj block list`. `docs/gate-policy-tuning.md` names this out of scope for the
evaluator work ("the evaluator changes who asks, never what the classifier
sees") and cites this entry by title — still open. Three ways to close it, in
increasing cost: append redirects to the scored clause (cheapest, moves the
corpus and escalation rates — re-run `contrib/kj-corpus.json` first); a
standing rule on redirect targets; or treat `has_redirect` as escalate-worthy
on its own (safest, blunt). Not urgent — `is_read_only_kj` already refuses a
redirect for the table it governs, so exposure is `--help`/`kj ledger` only.

## Codex app-server: attaching to a shared daemon over stdio (2026-09-01)

From a sibling session's bridge work, not yet folded into
`docs/codex-app-backend.md`:

- `codex app-server proxy` pipes stdio to a managed daemon's control socket —
  shared-daemon access over plain stdio, so `StdioJsonl`/`JsonlTransport`
  takes it unmodified with no new WebSocket client, and the spawn stays
  outside the protocol module. Only exists for a daemon started as
  `codex app-server daemon start`.
- Our client always calls `thread/start` and only sees the thread it created
  — it cannot attach to a thread a human is driving.
- `codex-codes` (types-only) pulls in nothing past serde/thiserror and models
  `thread/list`, `turn/steer`, `turn/started`, `turn/completed` — a real
  answer to the protocol-churn objection, if the "locally-owned JSON shapes"
  choice is revisited.
- `AdditionalContextEntry.kind` is `untrusted | application` — the right slot
  for peer or tool text entering a Codex turn.
- The installed sidecar unit (`contrib/codex-app-server.service.in`) now
  listens on `unix://$XDG_RUNTIME_DIR/codex-app-server.sock` so the Codex
  TUI can share it. The kernel backend dials `ws://` only, so it cannot
  reach that unit; a unix-socket `JsonlTransport` is the small fix, the
  stdio proxy above the general one.

## Checked-in gate probe corpus is stale

`contrib/kj-corpus.json` predates current prompt help and some context commands.
The removed context-deletion row is deleted, but the remaining snapshot needs
regeneration and policy review against the live reflection in
`examples/lfm2d-probe`. Treat it as a probe artifact, not current command help.

## Context retention and index eligibility (Amy, 2026-09-18)

Amy: "archive should be good enough for everyone. perhaps we can mark some as
don't-index or something so they don't get picked up by classifiers and so on
building search indices."

Context history and audit records are retained; archiving removes a context
from the active set and preserves its parent edges. Index eligibility is a
separate policy still to implement. `kj search --all` uses active contexts,
but the semantic-index watcher consumes terminal block events through
`BlockStoreSource`, with no context-state lookup. Archiving alone neither
prevents future embedding/classification nor evicts existing vectors.

Design an explicit opt-out with consistent selection for automatic indexing,
manual synthesis/index refresh, classifiers, and search results. Preserve
explicit history reads. Handle existing vectors, in-flight refresh publication,
and later opt-in together; filtering only the watcher leaves other producers
and stale search entries. Do not present archive as a don't-index guarantee.

## Synthesis re-embeds the whole context on every block write (2026-09-01)

**Automatic synthesis remains disabled** (`spawn_index_watcher` receives
`None` for `on_indexed`; keep this title stable for its re-enable condition).
Unchanged manual synthesis now reuses a hash of its exact input and embedding
profile, survives restart, and accepts `--force`. lfm2d replaces builtin
embedding inference; see `docs/synthesis.md` for its configuration and contract.

Changed contexts still embed every selected block plus gist/keyword candidates.
Next: per-block vector reuse and coalescing changed refreshes before enabling
automatic synthesis. Manual refreshes currently serialize across the index.
A final snapshot check refuses changes during inference, but publication is
not atomic with later context mutations. Index refresh checks competing
commits, but its supplied snapshot has no authoritative revision; a stale
snapshot submitted after a newer commit can still overwrite it. Both paths
want revision-aware publication through the kernel sequencer.

Selection also needs a separate decision: synthesis preserves its old
non-File/length filter, whereas indexing requires terminal Text/Thinking
blocks. Neither filter explicitly accounts for excluded blocks. Review that
policy before automatic synthesis returns. An empty index projection also
leaves its prior search vector intact; synthesis now clears its own preview,
but search needs an explicit removal policy.

Bulk synthesis also undercounts completed indexing when a context's index
write succeeds but its later synthesis fails: `refresh_synthesis` returns one
error and loses the intermediate `was_indexed` counter. Preserve partial
progress in that result when refining the bulk status output.

## Synthesis publication can race index eviction

With optional `IndexConfig::max_contexts`, an in-flight synthesis refresh can
finish after its context was evicted and repopulate that context's synthesis
rows. `runtime/synthesis.rs::run_synthesis_and_cache` serializes refreshes,
but indexing/eviction does not share its guard. There is also a narrower
window in `SemanticIndex::store_synthesis`: the metadata guard drops before
the memory-cache insert, so eviction can remove the DB row before that insert
reintroduces the memory entry. These storage paths predate the service move.

Make publication conditional on an eviction generation and keep DB/cache
publication ordered with eviction. Preserve intentional synthesis-only caches;
requiring an index entry unconditionally would break them. Add deterministic
race tests covering eviction during inference and between DB/cache publication.
Found in kaibo's synthesis review (GLM-5.3 via Crusoe).

## Embedding and classifier service configuration should share backend rows

The existing `embedding_config` singleton is the kernel's sole embedding
configuration source (`llm/db_config.rs::load_embedding_config`); the
classifier hook uses its own lfm2d URL. Move both into backend rows with an
embedding/classifier kind and shared endpoint ownership. This depends on
the active kj verb-class lane because it changes the registry and backend
administration. It does not block the synthesis service branch.

## The terminal client — `kaijutsu-tui` follow-ups (first cut shipped 2026-09-02)

Design: `docs/tui.md`. What the shipped skeleton left open, re-verified still
true:

1. **`inputTokens` on the wire.** `kaijutsu.capnp` still carries only
   `cacheReadTokens`; the status line's `⟳` divides by `contextUsedTokens`
   instead.
2. **Editor wire gaps.** `EditorFlow` (`flows.rs:1624`) still has only
   `StateChanged`/`Closed`, no `Opened` — `editor_open_as` publishes nothing a
   sibling renderer can watch for; the `open_editor` peer invocation is still
   the only open detection.
3. **Picker tails see only whole-block events** — the kernel-wide
   `ServerEvent` stream has no partial-block granularity, shared with the
   app's tails.
4. `theme.toml` → ratatui palette, `bindings.toml` keyed by vim notation (no
   file exists yet at either client), then porting the app to the same file.
5. **Images (additive).** `Abc` needs no new emitter (`engrave::engrave_to_svg`
   is complete); `Svg`/`Image` rasterization is unbuilt.
6. Bar/beat assumes 4/4; the wire carries no time signature.

The seat-digit disagreement between the picker and the status line, and
the ask-card/ledger follow-ups, are their own entries below.

## Telemetry method inventory has not had a full audit

`docs/telemetry.md` no longer lists `push_ops` or the `sync.*` span and
sampling rows, and `get_info`, `interrupt` and `complete` are no longer called
uninstrumented. The rest of the method inventory and its counts have not been
compared with `kaijutsu.capnp` and the `extract_rpc_trace` call sites.

## App draft edits need an ordered submission contract (2026-09-12)

`input/systems.rs::handle_compose_input` spawns each `edit_input` separately,
logs edit failures, and submits the server draft with another task. Local
overlay text can therefore differ from the draft submitted after a failed or
late edit. Visible submit failures do not repair that earlier divergence.
Design batching, acknowledgement, and recovery with the TUI input work;
Amy asked to discuss text input design before changing that contract.

Kaibo's review also found two recovery UX edges. A retained failed submission
can reappear after newer text is submitted and the overlay becomes empty;
it does not overwrite text or submit itself, but needs an explicit recovery
affordance. Text typed with no active or target context has no context to
retain under an identity transition. Define ownership for that unassigned
text alongside the draft-edit contract. Do not solve either case by silently
dropping saved text.

## App bootstrap results need consistent connection scoping (2026-09-12)

Identity replies now carry actor generation and transport epoch. Other
asynchronous bootstrap replies, including context lists/restoration and theme
configuration, still do not. A reply queued before actor replacement can
arrive after the new actor is installed. Audit those consumers together and
discard or refetch results from the old connection. Peer reattachment is
remembered by the actor only after its first `attach_peer` call; a cold-start
failure before that call also needs a complete bootstrap retry.

## Bevy approval review: the surfaces shipped, two gaps left (2026-09-12)

The ask sheet (`ui/ask_sheet.rs`) and the ledger ribbon
(`ui/ledger_ribbon.rs`, `Ctrl+A l`) read `connection::ledger::LedgerMirror`
and write decisions back through it, with `InputContext::AskSheet` and
`InputContext::LedgerRibbon` taking the keyboard while either is up. What is
still missing:

- **A waiting block still gets a generic status label.** The conversation
  does not say "this turn is waiting on an ask", only the hints line's `!n`
  and the sheet do.
- **Context creation and review assignment have no app UI.** Use
  `kj context create --as` so the performer exists before rc. The connected
  character is the director; reviewer assignment follows explicit delegation
  or the Amy default.
- **The PLAN block renders a flat statement list**, which is all
  `kj ledger show` carries. A plan tree and per-statement verdicts would
  need kernel fields first; the block is shaped to take them.
- **Neither surface's state is BRP-visible**, so a live check can only read
  `ActiveInputContexts` to tell whether the sheet or the ribbon has the keys.
  `AskSheetState` would need `Reflect` on its `HashSet<String>` and
  `Option<ContextId>`; that plus a way to raise a test ask is what a
  pixel-level check of the sheet needs.

## The tui and the app disagree on a few chords (2026-09-03)

Survey against `docs/input.md`'s prefix table and `docs/tui.md` "Keys".
Direction is on record (`docs/tui.md`, guidance 4: a shared `bindings.toml`
the app inherits) but the file doesn't exist yet, so the specific gaps still
stand:

- **App ahead, not yet in the tui:** `Ctrl+A '` (switch by prompt),
  `Ctrl+A A` (rename), `Ctrl+A q` (close+demote), `Ctrl+A d` (detach). The
  tui names each one's future meaning on the status line.
- **Tui ahead, not claimed in the app's table (no conflict rolling them in):**
  `Ctrl+A [` (copy mode), `Ctrl+A ]` (tui's own yank buffer, not the OS
  clipboard). `Ctrl+A l` is now in both.
- **`Ctrl+Z`** is suspend in the tui, `ToggleSurface` (chat/shell) in the app
  — the tui retired the shell surface for `:!`, the app still has the
  toggle. Name this in `docs/input.md` or retire the app's toggle.

## The /config melt's leftovers (2026-08-29/30)

Resolved: `invalidate_config_file_cache`'s doc comment (`kernel.rs:1875`) now
states the rc-specific rationale directly (a path predicate, not a parameter,
because `kj rc`/`kj config` callers have no session in scope). Only the
`Add` call's redundancy is left undecided — trivial, rediscoverable by
reading `kj/rc.rs`'s `write_path` match.

## Per-client config write-target defaulting has no owner (2026-08-30)

`kj config` is still only `list`/`show`/`reset` (confirmed,
`kj/config.rs`'s `ConfigCommand`) — the old default-to-caller's-own
`/config/client/<id>/<name>` behavior has no successor. Either the file tools
need to reproduce it from the caller's client-id, or the policy is gone and
every per-client write names its full path by hand. Still undecided.

## kaish `ln -s` across two /config mounts still creates a dangling link (2026-08-30)

The mount table rewrites an absolute target on the link's own mount relative
to the link (`vfs/mount.rs`, `symlink`), so the documented same-tree idiom
resolves. A target on another mount — for example,
`ln -s /config/midi/devices/x /config/rc/x/create/S20-device.md` — is stored
as given and resolves to nothing on the host, because a backend does not
know another mount's host directory. Either the mount table resolves both
sides to host paths when both are `LocalBackend`, or the surface fails
loudly on a cross-mount absolute target.

## Remaining approval ergonomics

- Shell/hook asks deliberately have no default expiry. A future janitor must
  identify obsolete asks and unfinished operations, record explicit cleanup,
  and preserve any re-ask linkage; absence of a TTL or sweeper is not itself a
  defect.
- Rotation remains manual. Design how a successor retains the predecessor's
  unfinished operation and ask references without moving authority or
  automatically resuming a different model context.
- A gated `:kj` command still wraps the refusal in an error on some client
  paths; show the durable ask reference as a waiting result consistently.
- Reviewer characters can have several live contexts. The ledger change feed
  informs connected clients; there is no dedicated context mailbox routing or
  automatic wake for a kernel model assigned to review a coder. A directing
  model currently reads the coder's response and uses `kj ledger list|show`.

## The scorer and the snapshot (2026-09-02)

**Shipped:** the lfm2d hook now reads `$s.plan.rendered` for `clause`
(`assets/defaults/rc/lib/hooks/lfm2d.kai:66`) instead of rebuilding it in jq.

**Still open:** substituting values into the scored clause before scoring.
Measured 2026-09-02: an unexpanded variable scores as a middle guess
(`chmod -R 777 ${DIR}` 26% vs `/` 97% vs a tmp path 3%). Proposed: score a
second, kaish-rendered *expanded* view beside the unexpanded one, take max
severity — parse-time substitution with a supplied map, not execution. An ask
to the kaish lead, not ours to build.

## The Claude Code advisory hook forwards to the kernel (2026-09-02, shipped; open follow-ups)

`PreToolUse` Bash → `kaijutsu-mcp hook claude` → `shellDryRun` → PreCall in
dry-run mode → abandoned ask row, always allow (`docs/gate-and-shell-split.md`,
"Dry-run mode"). Still open: count a day of `kj ledger list --status
abandoned --since 24h` against the Python hook's verdicts before retiring it;
the `( … ) &` subshell planning gap (kaish cannot plan it, S45 denies "no
execution plan") is an ask to the kaish lead; an export verb for corpus
builders was ruled "later" — they read the ledger directly for now.

## A secret source that runs a command has no home yet (2026-08-31)

`env.TOKEN = { command = "..." }` is still rejected —
`a_command_source_is_deferred_with_an_instruction` (`mcp/toml.rs:465`)
confirms the message says "not implemented" and names `file`/`env` as the
alternatives. Deliberate: host exec has one owner, and `external.rs`'s
sanctioned exception is about launching a config-declared server, not running
arbitrary programs for config values. Three ways in, whichever wins keeps the
existing fail-loud contract (never launches blank, never quotes the value into
a log line): route through `EmbeddedKaish`'s exec policy (needs the kaish
runtime up at secret-fetch time); widen the `external.rs` exception
deliberately (fastest, a second bare `Command::new`); or leave it — `pass show
… > ~/.token` is one hop away.

## The `attach` verb fires and no type ships a script (2026-08-28)

Confirmed still true: no `attach/` directory exists under any
`assets/defaults/rc/<type>/` despite `kj/attach.rs` running the lifecycle
(`VERB_ATTACH` wired, `attach.rs:75` rejects on `Err`). Either it earns a
script (re-stating the seat's stance to a model that arrived in a context it
did not boot) or `attach` should retire from `RC_VERBS`.

## Ctrl+Z lands in the wrong input on the second toggle (Amy, 2026-08-26)

Amy: *"sometimes when I hit ctrl-z it goes to conversation input... usually
first time is fine, second time goes to the other input."* No fix found in
the app (`ToggleSurface` still the same shape, `input/systems.rs:105`). Queued
for the app/UI session — start at `ActiveSurface`/`FocusArea` duplication
(128 references across `kaijutsu-app/src/`, still two pieces of state that
must agree and are set in different places).

## Binding review: unfixed items (2026-08-23)

- **A sticky `name_map` may defeat the collision resolver on sequential
  grants.** Not re-verified; no related commit found.
- **Narrowing a capability does not stop running kaish jobs.** The
  `kill_all_for_context` path this was filed against is gone; asynchronous
  shell work runs as kaish jobs with durable receipts. Check what a
  capability revoke does to a running job before deciding. Killing on narrow
  would destroy work.

## The wire drops kaish's output line anchor (2026-08-23)

`OutputNode` (`kaijutsu.capnp:1535`) still has no `line` field, confirmed —
`name`/`entryType`/`text`/`hasText`/`cells`/`children` only. kaish 0.16+'s own
`OutputNode.line` cannot be populated on our wire, so any client reading
structured output loses the anchor. Adding it is a schema change plus all five
artifacts rebuilt — its own decision. Worth doing when something wants it:
`grep -n`, an editor jump-to-match, and the vi surface are all line-anchored
already.

## An Error block is shown twice after a fork (2026-08-22)

`llm/hydrate.rs`'s Error-block fold (`:366-410`) still unconditionally
appends the envelope onto a parent `ToolResult` that may already carry the
same message — confirmed no dedup check. Costs tokens on every hydrate after
a fork and teaches nothing the first copy didn't. Fix has to keep the
standalone-error path (when the parent's tool result already flushed) and
skip only the duplicate — a judgment call, not mechanical.

## Approval transfer after operation setup

Initial command/output pairs, shell receipts and an optional ask link now commit
together. Model Waiting results also link their ask in the result acceptance;
failed registration or linkage publishes no partial Waiting pair. Repeated
setup for the same ask reuses the receipt; conflicting source or identity is
rejected before document mutation.

Session pre-call settlement now includes its ask link and Waiting receipt in
one acceptance. Command output fields, ANSI originals/spans, both statuses and
terminal receipt also commit together; failed transactions retain the previous
projection and a captured terminal outcome for restart recovery.
Approval resumes that need a new pair now use atomic pair/receipt/ask setup;
the separate `author_pair_for_ask` writer is deleted. Linking an executable ask
now retains a receipt for its existing pair too. Model Waiting acceptance
commits that receipt with the content, statuses and ask link. Conflicting pairs,
owners, contexts and performers are rejected. Original executable asks retain
receipt lookup through their pair when a result-review ask becomes current.
Close the remaining ownership and delivery handoffs before declaring approval
settlement migrated.
Admission now re-reads pair linkage under the same database guard as context
validation and redemption. A pair linked after the delivery scan keeps its
Session/Turn owner and participates in the performer-change check. Regression
coverage reproduces both stale-scan failures.

The gate now records whether its caller will publish a pair. Waiting publication
releases that handoff atomically with the link and result; bare linkage and
terminal failure do not authorize execution. Driver admission and matching gate
retries honor that ownership. A publication event wakes answers deferred before
the pair existed. Quiet, streaming and direct foreground calls declare no pair.

Terminal publication now retires an unreleased invocation with its result.
Startup recovers captured projections, then retires remaining unpublished asks
without execution. Pending asks become Abandoned; terminal reviewer decisions
remain intact. `kj ledger show` distinguishes publication abandonment from the
answer. Linked pairs settle to Error; retirement faults roll back and retry on
restart.

A live caller that fails without publishing a terminal result still leaves a
hold until restart. Registered operations now recover their original pair and
receipt atomically even when an ask never linked. Output and ANSI provenance
survive; recovery records an interruption without inventing an exit code or
rerunning source. Receiptless model pairs now settle through one atomic
acceptance per context during server startup. The sweep appends the reason to
stored stderr and retains other output; startup refuses approval or block
recovery failures. Historical error receipts
already completed by the old receipt-only sweep are not rewritten by the new
unfinished-operation recovery. Finish that ownership and compatibility audit.
Claimed execution now has durable notification retry. Continue the live-failure
and continuation-admission audit in docs/gate-resume.md,
"Still open".

## The app can stop taking the kernel-wide firehose (2026-08-22)

`ActorHandle::watch_contexts` lets a block-event subscription name a *set* of
contexts rather than one-or-all. The app still sets
`scope_blocks_to_context = false` (`connection/bootstrap.rs:130`, confirmed
unchanged) and takes every context's block events. It could watch exactly the
contexts it renders and re-issue as that set changes. Worth doing only if
event volume shows up in a profile — the firehose is a known cost, not a known
problem.

## Tech-debt audits, 2026-08-20 — what is still open

Full reports: `docs/audits/`. Re-verified against the current tree:

- **`dirty_file_buffers.context_id` is loaded and never used.**
  `get_dirty_file_buffer` and `list_dirty_file_buffers` select it, and
  `kj/swap.rs` reads only the path and the dirtied time. S.
- **The MIDI ear still logs a WARN on every refused capture batch**
  (`kaijutsu-audio-runtime/src/runtime.rs:398`), not once per state change —
  confirmed unchanged. An expected idle state, not a fault. S.
- **Escalate in PostCall/OnError/OnNotification still blocks the path up to
  the gate wait** — whether escalate is meaningful outside PreCall is still
  undecided. M, design.
- **rc softening for interactive seats is still missing** — that a human's
  interactive shell takes the hook path is written down
  (`docs/gate-and-shell-split.md`, "The three rpc.rs shell paths take the
  hook path"); the softening itself is not built.
- **The rc bootstrap gate still seeds a tree only when the whole directory is
  empty** (`rpc.rs:2585`, `if dir_is_empty(&host_dir)`), not path-by-path —
  confirmed still true, so a script added to the embedded set after a kernel
  was first seeded still never lands on its own; recovery is still a manual
  `kaijutsu-server rc reseed`. (`ensure_rc_seed_files` itself is install-if-
  absent per path and well tested — `rpc.rs` gaining `#[cfg(test)] mod
  context_bootstrap_tests` this pass is unrelated coverage, not this gap.)
- **`OutputProfile::Internal`** (`runtime/embedded_kaish.rs:127,1282`) is
  still present, still waiting on a kaish spill knob that doesn't remap the
  exit code.

## Older app and broker debt, carried out of auto-memory (2026-09-08)

Re-verified against the tree the same day:

- **Tall blocks lose Y resolution** past `max_texture_dimension_2d`
  (`GpuTextureLimits`, `view/block_render.rs`) — confirmed present. Fix is
  tiled rendering of the visible portion.
- **Role-group borders still draw through Vello**, missed in the Vello→MSDF
  migration.
- **Provider cache expiry is not a hydrate boundary** — a long-idle session
  carries messages the provider no longer has cached; nothing observes it.
- **`ActiveSurface`/`FocusArea`/paired overlay queries** are threaded as
  separate params through compose/interrupt/toggle systems (128 references) —
  a bundle component or resolver would collapse them.

## File buffers: reduce the MCP file tools to kaish (low priority)

Slices 1-3 of `docs/file-buffers.md` shipped. `mcp/servers/file.rs` still
registers `read`, `edit`, `write`, `glob` and `grep`. Amy, 2026-08-21: remove
them outright, "It's ok if we don't have them for a short period while we
finish the kaish upgrade." Amy, 2026-09-20: low priority, and "a focused
session where we think through the reduction to kaish."

Bring these to that session:

- kaish emits a `line` anchor under `--json` (kaish `aafc0ee4`), and the wire
  drops it: see "The wire drops kaish's output line anchor".
- Hashline stays on our side. kaish has no hashline code; the kaish builtin
  `edit` and the MCP tool `edit` are two mechanisms: see "`edit` still names
  two different things on two surfaces".
- `write` has no staleness guard: see "`write` has no staleness guard".
- `docs/file-buffers.md` describes a different slice 4 (remove `write` and
  `grep`, make `edit` hashline-only). Rewrite it from the session's outcome.

Slice 5 (`swapRecovered`/`diskChangedSinceLoad` on `EditorState`) is also
open; neither field is in `kaijutsu.capnp`.

## File documents should be created lazily, not on every read (2026-08-19, deferred)

Every file the kernel reads still leaves a durable block-store document
forever (`block_id` still not optional in `get_or_load`, confirmed). The
eventual model — a clean buffer stays a `String`, a document materializes only
on first edit, so a document existing *means* unsaved work — is deferred for
cost and cross-referenced from `docs/file-buffers.md` itself ("filed in
docs/issues.md; the row goes away if it lands"). Revisit once swap semantics
are proven.

## The swap marker and the content it marks are two writes (2026-08-19)

`record_dirty_file_buffer` (`kernel_db.rs:6337`, one `INSERT ... ON
CONFLICT`) and `edit_text`'s oplog journal write are still separate
statements, not one transaction — confirmed. A crash between them either loses
the marker (cold path reconciles the unsaved work away) or leaves a marker
pointing at content never written.

## `edit` still names two different things on two surfaces (2026-08-18)

Confirmed unchanged: kaish builtin `edit <path>` (`context_shell.rs:217`,
registered alongside `vi`) opens an interactive vi session; MCP tool `edit`
(`mcp/servers/file.rs:163`) is a surgical, non-interactive hashline/string
edit. Same name, same coder, opposite mechanism. `vi` is already the
documented front door (`docs/vi.md`), so dropping the kaish `edit` alias is
the cheap fix — check rc scripts and help text for callers first.

## `write` has no staleness guard (2026-08-18)

`write_file` (`mcp/servers/file.rs:512`) still calls `cache.create_or_replace`
with no precondition, then `flush_one` — never `flush_one_guarded` — so the
W12 disk-moved-under-you guard that protects `:w` still does not apply to
`write`. Whether an existing-file `write` should require a
generation/hash precondition or explicit overwrite intent (while a new path
stays a plain create) is still undecided. This is the asymmetry that let a
stale-context overwrite drop 115 backlog entries from this file on
2026-06-29 (recovered from `3f8b54d3`) while `edit`'s hashline mode would have
refused.

## Tool-pair atomicity at insert time remains unbuilt

Conversation snapshots repair pairing, and `Provider::stream` refuses an
invalid message sequence before backend dispatch. Neither holds unrelated
writers while a tool call is open. Durable blocks can still interleave,
and hydration preserves the existing interruption policy: synthesize an
error for a missing adjacent result and drop late results with warnings.
A writer-side queue needs a separate design with drift and peer tool calls
as concrete consumers. See `docs/conversation-session.md`, "Out of scope
for Slice A". `AGENTS.md` already describes the absence of this queue.

## Duplicate tool calls can execute before the next send refuses them

`process_llm_stream` collects `StreamEvent::ToolUse` events in a vector but
indexes their durable block ids by tool-use id. Repeated ids overwrite that
index and both calls still reach concurrent dispatch. The provider pairing
check refuses the next request, after those tools have run. Validate the
incoming call batch before execution; the send-time check cannot prevent
those duplicate side effects. The hydration lane's live-loop refusal test
uses two calls to an unknown tool to exercise this safely.

## Flaky: `test_ordering_stress_100_bisections` put a Middle block first, once (2026-08-17)

`test_ordering_stress_100_bisections`
(`crates/kaijutsu-kernel/src/blocks/block_store.rs:2796`) asserts only
`blocks_ordered()[0].content == "First"` — it still checks the sorted
output, not the generated order keys, so a rare ordering inversion (seen
once: `Middle-5` sorted ahead of `First`) would fail the same way again
without naming the mechanism. Cause unknown: not reproducible on demand
(8/8 and 10/10 clean reruns), and not touched by `89d90ccc`'s `merge_ops`
work. Fix the test to assert on order keys, not the sort result, so a
recurrence names order-key precision or tie-break rather than a content
string.

Recurred during the September 17 effective-environment validation: `Middle-9`
sorted before `First` (3117 passed, one failed,6 ignored). The change does not
edit block ordering. Evidence: `/tmp/kaijutsu-effective-env-kernel-final.log`.
Keep this occurrence with the existing ordering audit; a passing rerun does not
explain it.

---

## A `;` chain reports only its last command's status

`is_error` for a `;`-separated chain is the last command's exit status
(`env.is_error()`, `runtime/command_result.rs`). A chain whose last command
fails reports `Error:` when every earlier command succeeded, and a passing
last command hides an earlier failure. Decide what a chain's status means
before changing it.

---

## The file write/edit tools are not gated by the approval ledger (Amy, 2026-08-16)

`builtin.file:write`/`:edit` still route as plain capability tokens, not
through `approval_ledger` — confirmed still true, and
`docs/gate-and-shell-split.md` ("What this does NOT do") names this exact
gap as unsolved by that design. Not a security boundary (every player is
already inside the trust boundary); the ask is an ergonomic nudge so a
large destructive edit is visible and undoable rather than only
forensically reconstructable afterwards. A cheap partial worth keeping on
the table: gate on a size-delta threshold (N lines or X% of a file)
rather than every write.

---

## `docs/architecture/` needs re-certification, and two diagrams are missing (2026-08-16)

The two deleted diagrams (`01-system-topology.svg`, `06-crate-deps.svg`)
are now tracked in `docs/architecture/diagrams/README.md` itself — that
doc is canonical for the diagram gap, not this file.

Still open, not in any docs/*.md: `docs/architecture/README.md`,
`foundation.md`, and `client.md` were swept for vocabulary and for the
deleted last-write-wins machinery, but not re-verified line-by-line
against current code the way the 2026-06-16 sweep did originally — treat
as improved, not re-certified. And: `test_task_status_lww_tiebreak_order`
(`kaijutsu-types/src/block.rs:5566`, confirmed present) is the only thing
still pinning `TaskStatus` LWW order — decide whether that order needs
pinning at all now that concurrent merge into a kernel document is
structurally impossible.

## vi input editor stopped repainting after a small in-place edit (found 2026-08-16, live on moltar)

Amy was editing a typo ("rost" → "rest") in the compose-block vi input.
`i e <Esc>` (insert `e`, leave insert mode) changed the buffer but the
on-screen render did not update; `dw` + retype (a full replace) rendered
correctly. Not root-caused; likely a missed redraw/dirty flag on the
insert-then-escape path rather than a buffer-state bug. Vi input handling
lives in `crates/kaijutsu-app/src/input/vim/` (`mod.rs`, `dispatch.rs`) —
no dirty/repaint marker found there by name, so check whatever marks the
input view dirty against the insert-mode commit path.

---

## The roster index has no kernel-now reference, so client-rendered ages mix two clocks (2026-08-16)

Still true: `/run/roster/index` carries each row's `recorded_at` on the
kernel's clock (`kaijutsu-kernel/src/roster.rs`) but no kernel-now value,
so a client renders age by subtracting the kernel's stamp from its own
clock. Accepted for now (NTP-disciplined LAN, skew below display
resolution); wrong the moment a client's clock isn't disciplined. Fix:
add a kernel-now value to the index, or give `FileAttr` a `generation`
(see the entry below) so a client can conditional-fetch instead.

---

## Drift peer origins are stageable but not deliverable, and the wire can't name one (2026-08-17)

This, "Ambient command center" and "Seats-at-the-table follow-ups" all wait on
one missing piece: the wire cannot tie a principal to a peer. Build that once,
for the first consumer that needs it.

Still accurate and still unreachable in production (only tests construct
`DriftOrigin::Peer`) — confirmed at
`kaijutsu-server/src/rpc.rs:10839-10866`, `origin_ctx_bytes` reports a
peer origin as absent because the wire's `sourceCtx @1 :Data` means "a
ContextId" and nothing else. Durable, never lost (drains to dead-letter
like any delivery failure), just can't be delivered or displayed.
**Whichever lane adds the first real peer-origin producer (the cc inbox,
`docs/drifting-dead-letters.md` slice 4) must give the wire an honest
origin representation in the same change** — appending origin fields is
ordinal-safe, do it then, not before.

## The rc lifecycle shell has a narrower tool set than the interactive one (2026-08-22)

An rc `create` script calling `fmt` fails with `command not found: fmt`,
while the same command in the MCP `shell` tool succeeds — a `.kai`
verified interactively can still fail at context create. Exec authority
gates on the context's binding holding `Capability::Exec`
(`runtime/context_shell.rs`), so this is plausibly an ordering artifact of
when a create-time binding takes effect, not yet confirmed either way.

Two consequences. Verify rc scripts by *creating a context*, not by
running the command in a shell. And a create-path script under `set -e`
should degrade rather than abort: a failed helper takes down the whole
context create, and a context with no stance is worse than a stance that
reads a little ragged.

## `connection/drift.rs` still reads block events off the kernel-wide stream

Still true, two sites: `connection::drift`'s
`ServerEvent::BlockInserted { kind: Drift }` detector
(`kaijutsu-app/src/connection/drift.rs:181`), and
`time_well::live::ingest_live_events`'s `ContextTails` build
(`live.rs:289`) — both read the kernel-wide stream rather than the
per-context change feed, deliberately kept because the feed can't serve
contexts nobody follows. Decide the scope question (does drift into an
unfollowed context deserve a notification?) once for both sites, not per
site, before moving either.

## Model names via hooks — the plumbing exists, the data mostly does not arrive (2026-08-15, Amy)

Plumbing is built (`HookEvent::model: Option<String>`,
`kaijutsu-mcp/src/hook_types.rs:30`; set on `SessionStart` in
`hook_listener.rs`) — the gap is source data, and it's three separate
problems: Claude Code's hook payload has no model field at all (would
need `transcript_path` JSONL sniffing); Crush/qwen doesn't send it either
(unconfirmed what it does send); `SessionStart`-only is stale the moment
`/model` changes mid-session. Do the per-source capability survey (who
sends it, what's the honest fallback) before writing more code — and if a
source can't supply it, the roster should say "unreported", not render as
unconfigured.

---

## `/v/docs` block filenames do not sort into document order (2026-08-15, Amy)

Still true: the `/v/docs` backend lists block filenames as
`BlockId::to_key()`
(`kaijutsu-kernel/src/runtime/kaish_backend.rs:443`), the
`<ctx>_<principal>_<seq>` string, so `ls` sorts principal-major and seq as
text (`_2` after `_15`). `order_key`
(`kaijutsu-kernel/src/blocks/content.rs`) is already a base-62
lexicographic fractional index built for exactly this — rename to
`<order_key>__<short_block_id>` and `ls` sorts correctly with no kaish
change. Unblocks the sketched netrw-style subscription view Amy wants;
open question is whether `order_key` (which moves when a block moves) is
stable enough for whatever consumes the name.

---

## Principal plumbing — a holistic sweep, not a per-lane patch (2026-08-15, Amy)

Not started (no `PrincipalSweep` or equivalent found). Ruling stands:
this does NOT gate the git-auto-commit work — that ships with
service-authored commits and principal fidelity retrofits later. When it
happens, survey first: inventory every mutation path that reaches durable
state (VFS/config writes, rc edits, block mutations, MCP tool calls,
kaish builtins, drift, `kj` verbs) and classify what each knows about its
actor — real `Principal`, synthetic/service, inferred, or genuinely none
(legitimate for kernel-internal timers). Not an authorization mechanism:
attribution and recovery only (`docs/instrument-design.md`, "Many hands,
one trust boundary").

---

## `poll_connection_status` has an arm that cannot fire (2026-09-21)

`poll_connection_status` (`kaijutsu-app/src/connection/actor_plugin.rs`) removes
the `RpcActor` resource when the status broadcast reports `Closed`. The receiver
comes from `actor.handle.subscribe_status()`, and `ActorHandle` holds its own
`status_tx`, so the channel stays open for as long as the resource exists. A
panicked actor now publishes `Terminal` through `supervise_actor`
(`kaijutsu-client/src/actor.rs`) and is not respawned; the app shows it as
`last_error`. Delete the arm's body, or decide what removes a `Terminal`
actor's resource.

---

## Managing roots — the concept kaijutsu is missing (Amy, 2026-08-15)

Settled design: a forest of one-parent trees; drift is a separate,
deliberately cyclic overlay, never part of structure
(`KernelDb::insert_edge`'s cycle check already applies only to
`EdgeKind::Structural` — confirmed). Archive is one row, never a cascade,
and archived contexts keep their label out of the live index — both
**shipped 2026-09-05**. Anchors are seats, not workspaces (fork copies
history by default, so an unused anchor is cheap and a used one taxes
every descendant forever).

**Not built**: the `anchored_at` column (slice 2 — parentless by
construction, never swept by age; no such column exists yet), so there is
still no `--detached` flag on `kj context create` and no enforcement of
"anchors stay unused." Recommendation stands: build slice 2 (the anchor
bit) before slice 3 (enforcement) or slice 4 (`kj root` verbs). Slice 1's
guardrail is in: `context_move` is one transaction, so a refused move no
longer orphans the context.

---

## Live roster — push-on-attach is the remaining unwired half (2026-08-14)

Slices 1-4 and periodic refresh shipped (`crates/kaijutsu-kernel/src/roster.rs`,
`roster_sources.rs`, `kj/roster.rs`, `vfs/backends/roster.rs`,
`tests/roster_refresh_boot.rs`). Still open:

- **Push-based refresh on peer attach/detach isn't wired.**
  `roster_sources.rs:43` still names this a TODO —
  `kaijutsu-server`'s RPC attach/detach handlers should call `refresh_once`
  (or a narrower single-peer reconcile) instead of waiting for the ~10s pull
  tick. `SharedKernelState::roster` already holds the handle needed.
- `RECENT_LIVE_WINDOW_MS` (`roster_sources.rs:73`, 15 minutes) is an untuned
  v1 guess — a config knob if it needs adjusting.

## Theme changes never reach a running app — there is no live config push (2026-08-13)

Still true, verified: `ThemeReceived` (`actor_plugin.rs:551`) has exactly one
send site, the connect-time bootstrap fetch, and no `ServerEvent`
config/theme variant exists. `kj config set` on the theme file updates the
kernel document and nothing tells a running app — a next-connect console,
not the live one `docs/color.md` sells.

Remaining work: a config-changed server event (or subscription) that
re-fires `ThemeReceived` on theme writes; the app-side repaint already works
once `Theme` is replaced. Also open: `ThemeData` (the TOML wire format) has
no fields for block text colors (`block_assistant`/`block_thinking`/…; user
text follows `fg`), so no
theme file can change conversation text colors; `Theme` derives neither
`Reflect` nor registers with BRP, so it cannot be poked remotely for testing.

## Dock RTT sizes skip physical-px rounding (2026-08-12, kaibo find)

Still open, verified: `render_north_dock`/`render_south_dock` still stamp
`rtt.built_width = logical.x` raw (`ui/dock.rs:585,844`), while block cells
round via `round_to_physical_px` (`view/block_render.rs:318`). At fractional
DPI that makes `msdf_item_scale` a hair off exact, giving sub-pixel glyph
drift on the dock. Cosmetic, pre-existing; fold into the tier-2 "unify RTT
resize" cleanup.

---

## Summaries drift stronger than what they summarise (2026-08-11)

Writing failure mode, not a code bug: a summary reads stronger — or, in two
recorded cases, weaker — than what was actually pinned, and the next reader
inherits the drifted version without re-deriving it.

**Still open — Amy's call, never ruled on:** should a claim about what is
*proven* carry the assertion name or a `file:line`, so the next reader can
tell pinned from observed at a glance? Not adopted anywhere in CLAUDE.md or
docs/ as of this sweep.

---

## MCP 2026-07-28 adoption — two slices shipped, three items open (2026-08-11)

Claude Code cannot list kaijutsu-mcp's tools on rmcp 3.1.2 (2026-09-22).
Its MCP log: the version probe hard-closes the server ("rmcp-class pre-init
hard close; respawning pinned legacy"); the respawn negotiates 2025-11-25;
then tools, resources, and prompts lists fail with "request _meta is missing
or has malformed required fields: io.modelcontextprotocol/protocolVersion,
io.modelcontextprotocol/clientCapabilities". A hand-driven stdio
`initialize` + `tools/list` succeeds. rmcp 3.2.0 lists "keep initialize on
legacy protocol versions (#1228)"; 3.4.0 is current. Amy: "let's plan to get
on latest rmcp" and "fully adopt the latest spec where we can". Plan:

1. Capture Claude Code's exact stdio traffic (a tee wrapper around the
   binary) and pin it as a failing kaijutsu-mcp test: probe, then legacy
   `initialize`, then each list call.
2. `cargo update -p rmcp` to 3.4.0; fix the build in `kaijutsu-mcp` and the
   kernel broker (`mcp/servers/external.rs`); re-read both hand-pinned
   `V_2026_07_28` comments against the new `KNOWN_VERSIONS`. The test goes
   green; `/mcp` lists tools in a real Claude Code session.
3. Serve the 2026-07-28 inline lifecycle (SEP-2575 `server/discover`,
   per-request `_meta`) instead of forcing clients to the legacy respawn.
   Consider 3.3.0's `ServerHandler::negotiate_initialize`.
4. The open items below, in order of payoff: `structuredContent` +
   `outputSchema` on `shell`; tasks (SEP-2663) for long calls; reconnect;
   elicitation 1b. Drop deprecated roots/logging once kaibo and bevy_brp no
   longer need them.

Shipped since filing: **elicitation slice 1a** — `create_elicitation`
(`mcp/servers/external.rs:200`) now emits `ServerNotification::Elicitation`
instead of rmcp's silent auto-decline. **`on_progress`** (`external.rs:167`)
is no longer a no-op — it forwards `ServerNotification::Progress`.

Still open:
- **Elicitation slice 1b** — nothing yet *answers* an elicitation
  (human/sibling-context routing, timeout policy). Design pass, not a patch.
- **`structuredContent`/`outputSchema` on `shell`** — `ShellCompletion::to_json`
  still hand-rolls its envelope into a `TextContent` string; no
  `Tool::with_output_schema` usage found in `mcp/servers/shell.rs`.
- **Tasks (SEP-2663)** for outbound long calls — not built; `enable_tasks`
  doesn't appear anywhere.
- **No automatic reconnect** — `reconnect()` (`external.rs:468`) still has no
  caller; a dead server stays Down until `kj mcp reload`.

## Ambient command center — trace packets, switchboard follow-ups (2026-08-10)

Still unbuilt: the trace-packet/comet system (concepted, not built — no
`RouteRegistry`/mode-2 pulse slots in `TraceGlowMaterial`); switchboard
placement (south wall still sits behind the default room camera); switchboard
polish (recency dynamic range still narrow); an ambience agent (idea only).

Verified changed since filing:
- **Switchboard slow leak — now intentional, not a bug.**
  `SwitchboardState::retain_relevant` (`view/room/switchboard.rs:333`) keeps
  an off-roster context's sticky ember on purpose, pinned by
  `retain_relevant_keeps_offroster_embers_and_drops_idle_signals`.
- **Turn-comet/seat-flare enabler** (principal↔peer correlation) is still
  missing — same open item as "Seats-at-the-table" below; don't duplicate
  the fix.
- Runner still runs `cargo watch` (`contrib/kaijutsu-runner.sh`), not
  watchexec — moltar-deaf status unverified this sweep.

---

## Peer-registry doctrine (2026-08-10, peers-plumbing)

**Resolved since filing:** the nick-goes-stale-on-relabel bug is fixed —
`PeerRegistry::attach` now keys on `instance` alone when present
(`peers.rs:45` `peer_key`), so a re-join under a stabilized label replaces
the old entry instead of duplicating it.

Still open: the wire `PeerInfo` struct (`kaijutsu.capnp:1622`) still carries
only `nick`+`attachedAt` — no `instance`/kind field — even though
`PeerConfig` gained `instance` (`:1617`). `listPeers` still can't
distinguish two peers sharing a nick from each other.

---

## Seats-at-the-table follow-ups: nameplate LOD, turn-event flare (2026-08-10, seats)

Both still open:
- **LOD-gated nameplate on well-zoom** — needs a nearest/under-cursor wisp
  pick, not built (`view/room/seats.rs:8` still points back to this file).
- **Turn-event flare needs principal↔peer correlation** —
  `ServerEvent::TurnCompleted` still carries only `principal_id`, no peer
  nick/instance; no correlation exists in kernel or app (grepped, no hits).
  Same enabler needed by "Ambient command center" above — don't duplicate.

---

## FlowBus backpressure — what the 2026-08-05 rework left open

Per-subscription bounded queues, `subSeq` and the lag kick shipped. Open:

- **No catch-up for a subscriber that was not there.** `flows.rs` has no
  catch-up path; losslessness is a promise to live subscribers only.
- **Only `slowSubscriber` is ever sent.** `serverShutdown`/`superseded`
  exist on the wire (`kaijutsu.capnp:774`) but nothing emits them
  (`context_feed.rs:286`, `rpc.rs:11425`), so a clean shutdown looks like an
  ordinary disconnect.
- The ACP adapter's defensive sweeps (`kaijutsu-acp/src/session.rs:495`,
  `update.rs:1586`) are dormant defence-in-depth; remove once real flights
  show them firing zero times.

## lfm2d escalation: the shell gate that scores `shell_write` (wired 2026-08-24)

The hook body (`assets/defaults/rc/lib/hooks/lfm2d.kai`) re-derives each
secondary signal's verdict, exempts read-only `kj` and `kj ledger`, and
requires ladder position 0 plus a label match before auto-allowing. No
self-approval and approval-executes are canonical in
`docs/gate-and-shell-split.md` and `docs/gate-resume.md`; the layered policy
tiers are `docs/gate-policy-tuning.md`. Old measurements in this entry's
history must not be quoted. Open:

- **Our `kj` verbs never reach the classifier as reads.** Every verb
  declares `Effect::Read | Write | Destroy` in code (`kj/effect.rs`); Read
  skips scoring by construction
  and the classifier is not expected to learn kj vocabulary. Writes still
  score until the gate-policy tiers land.
- **Widen the probe with real traffic** (`LFM2D_MODE=log` for an interval)
  and **whether to enable an auto-allow band at all** — both Amy's call.
- **A reformulated command does not carry its pending ask forward.** A
  `retry-after-ask` ledger row is the minimum (a measurement, not a
  control). Unbuilt.
- **The hook's own `kj ledger` exemption is a second statement of
  `is_gate_exempt_kj`** (`assets/defaults/rc/lib/hooks/lfm2d.kai`,
  exemption 2) and reads the second word, so a root flag ahead of the verb
  defeats it; the evaluator already skips PreCall for an exempt program.
  Amy, 2026-09-21: "anything that doesn't precisely match the allowlist
  should go to the classifier whole, no modification. if a model bundles kj
  with other stuff, it has to go through full classification." and "raw
  command goes to the classifier. we'll dial in the classifier over time."
  So the hook sends the raw `shell_write` command as one clause: the
  per-command split, the allow-tier drop and all three jq exemptions go, with
  the `contrib/lfm2d-ladder-check.kai` copy. The ask tier stays firm. Two
  tests already run the shipped hook body (`mcp/broker.rs`, the
  `..._real_lfm2d_hook...` tests); the mixed-program test reads the
  `/v1/cascade` body from a loopback mock, which waits on per-context egress
  (`docs/egress.md`). Known cost: the classifier reads a plain `kj` read as
  risky, so a mixed program asks a human until the classifier improves.
- **The hook's `n_clauses -eq 0` exit cannot be reached.** The broker skips
  hooks for an all-allow program using the same evaluator that stamps the
  hook's tiers, so the allow-tier drop never empties the list.
- **A kaish lexer rejection degrades the gate to the no-plan fallback**
  (~16x noisier). `contrib/kai-parse-check.sh` guards our own corpus; the
  lexer bug is kaish's (`gotcha_kaish` in memory has the shape).

## The worker pool dropped a queued task's settlement once (2026-09-22)

`runtime::worker::tests::a_factory_panic_during_shutdown_still_drains_accepted_work`
failed once in a full `cargo test -p kaijutsu-kernel` run and passed three
times alone. The third task's `finished` sender was dropped without the task
running: `settled.await` failed with "factory failure must not discard queued
settlement" (`runtime/worker.rs`), and no panic from the task's own asserts
was printed. Same pool (2d829c9d) and the same class of cross-thread
placement as the ignored same-context ordering test. Queued work that
vanishes at shutdown loses a settlement, so treat it as a bug until a run
shows otherwise.

## Egress: what stays open (2026-09-21)

`docs/egress.md` owns the rule: per-context rows, a reviewer-held list, refuse
on a miss. Amy: "git and any builtin contacting outside resources should go
through the same path over time so we can monitor/classify/constrain." Open:

- **Only `curl` reads the list.** `git` and host programs run under
  `ExternalExec::Allow` reach the network with no egress check.
- **A miss is refused, never adjudicated.** Amy: "we may add a special network
  classifier later but for now we'll rely on allowlist or yolo for curl." The
  plan evaluator sees `curl <url>` as a command; an approval has no way to
  reach the tool at connect time. Needs a design conversation.
- **A context created before this change still carries an unused
  `LFM2D_URL` env row.** `lib/create/S50-lfm2d.kai` no longer writes it, and
  the hook body reads `KJ_GATE_CLASSIFIER_URL` instead, so the leftover row
  is inert — nothing reads it — but nothing clears it either.
- **Nothing records a request.** Monitoring is part of the goal; no span or
  ledger row names the host a context reached or was refused.

## Cast follow-ups (seeded 2026-08-03)

- Nothing reads `cast_slots.loadout`. Remove the column or give it a reader
  in the capability redesign.
- `available_models()` is hand-maintained per provider
  (`llm/mod.rs:899`) though a live Models API lookup exists for the context
  window (`llm/claude/models_api.rs`).

## An unreadable `api_key_file` falls through to env (seeded 2026-08-04)

Four backends warn `api_key_file configured but unreadable`
(`llm/config.rs`, `resolve_api_key`) for a `~/.openai-key` that does not
exist, then read the env var anyway. The env rule next to it already says
naming a source is a statement about where the key lives; the file rule
should match — return `None` and let the registry skip the backend — but
that changes which of Amy's backends load, so it waits for her to fix the
rows or agree.

## MCP subsystem — audit follow-ups (2026-07-29)

- **`InstancePolicy` does not persist across restart** —
  `Broker.policies` (`mcp/broker.rs:125`) is a bare map, so a live-tuned
  `call_timeout_ms`/`max_result_bytes` reverts on restart.
- **No project-instructions discovery** (CLAUDE.md/AGENTS.md analog).
  `build_system_prompt` (`llm/system_prompt.rs`) assembles stored instruction
  sections plus `<situation>`, with no filesystem crawl.

## MIDI device profiles: routing does not consume port roles (`docs/midi-next.md` slice 2)

`PortRoleMatch` exists (`kaijutsu-audio-runtime/src/midi_match.rs:88`), but
`kj midi send`/`identify` still route to a device's FIRST matched port
(`midi_exchange.rs:86,524`, `dj/midi.rs:92,114`). Demonstrated wrong on the
MiniBrute, which answers identity only on port 1. Also unfilled: USB
`vendor:product` enrichment (`midi_in.rs:194`, `usb_id` left `None`), so
matching is name-substring only.

## `rich_json` is unbounded on the wire (seeded 2026-07-18)

`block_output_data` (`kaijutsu-server/src/rpc.rs:9080`) persists `.data`
whole with no size check, bypassing kaish's text-only output limiter. A size
ceiling that fails loud, or CAS routing like `RenderCue`'s `casHash`.

## SFTP over the VFS: appends can clobber each other (`docs/sftp.md`)

`write` (`sftp.rs:566-576`) does one `getattr` for both the generation guard
and the APPEND offset, so two cross-session appenders can both read gen=N
and lose an update. Wants an atomic append: `VfsOps` has no append
primitive (`write_all` is truncate+rewrite, `vfs/ops.rs:158`), which would
also make `>>` and jsonl logs cheap. `opendir` materializes a whole
`readdir` per handle (no pagination); the post-write re-getattr race is
accepted in code (`sftp.rs:585`).

## Offline audio analysis has no resource admission or cancellation contract

`kj/audio.rs::audio_beats` starts a blocking task for each request, without
an analysis-specific concurrency limit, decoded sample bound, or inference
cancellation mechanism. Dropping its waiter cannot stop started inference;
runtime shutdown waits for the blocking work. Full-file decoding and mel
extraction precede chunked prediction. A 180 s synthetic track peaked near
564 MiB; two concurrent analyses peaked near 1.37 GiB on the measured host.
See `docs/audio-inference.md` for methodology and placement tradeoffs.

Define bounded admission, input limits, and cancellation semantics before
expanding this workload. Results currently name a mutable host path and
model family, without audio or weight hashes; caching or durable musical
use needs immutable provenance. Executor placement, including possible
lfm2d ownership, remains undecided. Hardware timing stays with audiod.

Amy's workload is near-term musical decisions on a shared pulse, with
seconds available for models and media transfer. Evaluate readiness across
queueing, computation, and transfer, and use the existing resolver contract
for basis validation and commit-time fallback. Beat analysis is a candidate
for an optional tool/resolver adapter; retaining a core `kj` verb is not a
requirement. Packaging and relocation remain undecided.

The latency doctrine in `docs/midi.md` also needs measured targets: its
"99.99%" readiness claim has no supporting measurement here, and "a few
seconds — 16–32 bars" conflates durations (at 120 BPM in 4/4, that is
32–64 seconds). Derive horizons from tempo and measured end-to-end costs.

## Audio nodes — follow-up after daemon extraction

- **Keep jobs are in-memory on both sides** — `KeepJobs`
  (`crates/kaijutsu-kernel/src/kj/audio_capture.rs:18`, still a bare
  `Mutex<BTreeMap>`) and `takes.rs::Pool`: after a kernel restart nothing
  can rediscover a job's `/tmp/kaijutsu-audio-<uuid>` staging path. Persist
  the job row (id, node, instance, path, phase) before a recovery verb is
  worth building.
- Profile edits load only on daemon connection/reconnection; raw port
  reconciliation does not refresh profile files.
- Distinguish ingress data loss from lost topology notifications — the
  conservative reset replaces generations on every startup burst, not only
  real loss. See `docs/audio-daemon.md` "Evolution" for the execution
  checklist (node inventory, retained windows, capture export).
- A timed-out RPC (10s) flips `connected=false` and re-sends
  `MetronomeConfig` to the DJ mid-play; should reload config on reconnect
  only, treat a timeout as a retry.
- `--context` is validated once at startup; an archived capture context
  just makes `commit_capture` warn every four seconds forever instead of
  stopping.
- Named render destinations: playback broadcasts to every attached render
  client; multiple machines need an explicit destination contract.
- Kept-take recovery after kernel restart, and explicit handling of a
  permanently lost daemon instance — do not add a broad `/tmp` sweep;
  ownership recovery must name exact job paths.
- Generic `kj` help still understates MIDI verbs; the live subcommand help
  is more complete.

## Architecture & System Design

- **`rpc.rs` is one file of about 10,600 lines.** It shrinks as execution
  moves into the kernel crate. Split the Cap'n Proto trait impl by domain
  (`rpc/vfs.rs`, `rpc/llm.rs`, `rpc/mcp.rs`).
- A broadened role loadout reaches a live context only on re-create or
  restart.

## Drift UX — cross-session ergonomics (2026-08-12)

Design record: `docs/drift-ux.md`; the newer `docs/drifting-dead-letters.md`
(2026-08-16/17) now owns most of drift's architecture backlog. Still open
and not covered by either doc:

- **A received drift is a dead end for reply.** Hydration surfaces the short
  id only (`llm/hydrate.rs`), never the sender's label or a reply hint.
  Not the cheap fix it looks like: `translate_block` has no DB access, and
  a stamped label would go stale when `stabilize_context_label` renames
  `cc-*` contexts. Wants a label snapshot passed in, not a DB handle.
- **Drift edge metadata is inconsistent across delivery paths** — still true:
  immediate push stamps `drift_kind.to_string()` (`kj/drift.rs:406`,
  `"push"`), flush stamps `format!("{}#{}", drift.drift_kind, drift.id)`
  (`drift.rs:793`, `"push#1"`). `kj drift history` cannot uniformly trace an
  edge back to a staging event.
- **MCP connections that never receive hook traffic mint a context that
  never stabilizes/archives** (found 2026-08-12 during the cc-* sweep) — a
  `kaijutsu-mcp --connect` probe with no session gets no `session.end`, so
  nothing reclaims it. Options: lazy registration on first hook event, or
  archive-on-drop for a connection that never stabilized.
- **Arriving drift honoring `--drive`** — the receiver-side per-context
  "honor drive requests" setting (default off) described in the 2026-08-12
  ruling is not built; no code found for it. Its blocker (the rc-lifecycle
  identity smear) is fixed — see Drive gates below — so this is now
  buildable, just not built.

## Drive gates — external drive still ungated (2026-08-12)

Self-drive is gated (`Capability::Drive`, `kj/drive.rs:61-64`). The archived
check shipped: `kj drive` refuses `Concluded`/`Staging`/archived targets,
tested (`drive_refuses_an_archived_context` and siblings,
`kj/drive.rs:274-320`). Still open:

- **External-drive gate (caller != target)** — the genuinely new per-context
  gate, default off with `context_type` defaults via rc. No
  `ExternalDrive`/honor-drive code found; not built.
- **Cold-cache suppression** — computable with no new schema
  (`context_usage.updated_at` vs. the shortest `cache_breakpoints` TTL,
  `kj/cache.rs:138-141`) but not implemented. Refusals must be loud, with a
  way to insist (cold-cache is a cost signal, not a correctness one).

## Musician create cannot name its track; dock sparklines have no meaning yet

The musician create rc attaches to a label-derived track before an explicit
`--track` can move it: `kj context create` has no `--track` passthrough. The
dock sparklines' data source is a placeholder (events/sec, running-block
count); decide what they measure before polishing.

## Control plane (kj): two gaps

- **`--out` writes bypass the VFS.** `kj cas get` (`cas_get`), `kj block cat`
  and `kj block original` `std::fs::write` relative to the server cwd, never
  through mounts. The fix needs `dispatch_cas` and `dispatch_block` to become
  `async fn`, because `VfsOps::write_all` is async and those dispatchers are
  not: that is two `.await`s in `kj/mod.rs`'s dispatch match plus every test
  that calls either dispatcher. Do not bridge it with `block_in_place` and
  `Handle::block_on` instead — `kj` runs under the current-thread runtime each
  SSH session thread builds (`kaijutsu-server/src/ssh.rs`, where rc lifecycles
  re-enter kaish), and `block_in_place` panics there. Attempted and reverted
  2026-09-20 for exactly that reason.

## Index and ABC: two schema-shaped debts

- **The synthesis tables lack `ON DELETE CASCADE`.** `synthesis`,
  `synthesis_keywords` and `synthesis_top_blocks` live in
  `crates/kaijutsu-index/src/metadata.rs`; deletes are manual across the
  three. Do it at the next schema change.
- **ABC MIDI pitch/velocity are unmasked.** The `kaijutsu-abc` MidiWriter
  leaves pitch/velocity unmasked (`midi.rs:970-995`), safe while the one
  caller uses velocity 80.

## `docs/abc-reference.md`'s support matrix is four months stale

The ABC v2.1 reference maps notation to `kaijutsu-abc` support status as of
2026-05-25; 27 commits have touched the crate since, including the June 30
conformance push. The crate's tests are truth; re-derive the matrix from
them or drop the status columns.

## Tracks do not re-arm after a kernel restart

A restart resets every track to stopped, and nothing re-arms the tracks that
were playing. Tick counters also do not survive a restart. `docs/tracks.md`
records the current behavior; the re-arm sweep and counter durability are
unbuilt.

## Hyoushigi / Musician — open remainder

`docs/midi.md`, `docs/pcm.md`, `docs/chameleon.md` and `docs/fork-filters.md`
carry the mechanism; these are what none of them cover:

- **No CAS write surface (client→kernel put).** `/v/cas` is read-only
  (`vfs/backends/cas.rs`) and `commitCapture`'s `Cas(hash)` arm refuses
  (`rpc.rs:8546`). Needed at the first heavy payload.
- **Perception is notation-only.** `KJ_HEARD` is ABC; no `MidiToAbcDeriver`,
  so a captured MIDI window is invisible to a model.
- **No chart is seeded into a player's context**, and the OODA Act is
  hardwired to ABC (`kj drive --score-at`, `hyoushigi/model.rs`).
- **Players get no tools at all.** A read-only kaish (kaibo's posture) would
  remove the tool-palette-hangs-small-models cliff by construction and give
  bar math an escape hatch. Decide which RO builtins.
- **Rotate chains pollute the director's tree** (`kj context list --tree`
  shows a 17-deep chain per song); no `--hide-archived` or chain folding.

## kaijutsu-mcp Remote backend collapses multi-context ops to one context

`context_ids()` (`kaijutsu-mcp/src/lib.rs:844`) returns only the joined
context for `Backend::Remote`, so a global search silently skips every other
context; resource/prompt handlers hardcode `kind: "Conversation"` for Remote
(`lib.rs:2871,2918`).

## The stream-start retry loop's use of `retry_disposition` has no test (2026-09-20)

`retry_disposition` (`runtime/llm_stream.rs`) is pinned exhaustively per
`LlmError` variant, but nothing drives the retry loop itself with a real
`AuthError` or a transient error: `MockClient` (`llm/mod.rs`) cannot make
`Provider::stream()` return an `Err(LlmError)`, so only the synchronous
`validate_tool_pairing` path can produce one without a live provider — which
is what `invalid_live_tool_pairing_fails_once_without_retry` already uses. An
error-injection builder on `MockClient` would let that test's shape cover the
permanent and transient cases, and would prove the attempt count.

## OpenAI's 402 becomes a retried `ApiError` (2026-09-20)

`llm/openai/mod.rs:255` maps HTTP 402 to `LlmError::ApiError("insufficient
balance: ...")`, and `retry_disposition` (`runtime/llm_stream.rs`) classifies
every `ApiError` as transient, so an exhausted balance spends the full backoff
reproducing itself. Claude routes the same status through `400..=499 =>
InvalidRequest` (`llm/claude/mod.rs:331`), so the two providers disagree. The
catch-all `_` arm in both also lands a non-4xx/5xx status on `ApiError`, and a
JSON-parse failure of a successful response does too
(`openai/mod.rs:192`, `claude/mod.rs:246`). Decide whether 402 is a permanent
variant or whether `ApiError` needs splitting, and make both providers agree.
Found by kaibo reviewing the retry-classification commit.

## A Claude provider comment promises an error it panics instead (2026-09-20)

`llm/claude/mod.rs:85-89` says a reqwest client-builder failure surfaces as
`LlmError::Unavailable`; the code `.expect()`s and panics
(`claude/mod.rs:108-109`). Either return the error or say that a builder
failure is unrecoverable at construction. Found by kaibo.

## `cargo test -p kaijutsu-solo-acp` fails 21 tests for a missing feature (2026-09-20)

`solo_acp_stdio.rs` drives real turns through `BackendKind::Mock`, which
exists only under the `test-mock` feature, so a plain
`cargo test -p kaijutsu-solo-acp` fails 21 of 23 with
`invalid value 'mock' for '--backend-kind'` rather than skipping them. Declare
the target in `Cargo.toml` with `required-features = ["test-mock"]` so cargo
leaves it alone without the feature, and say in the file which invocation runs
it.

## The theme mirror test only spot-checks fields (2026-09-20)

`theme::tests::default_theme_toml_deserializes` (`kaijutsu-types/src/theme.rs`)
`include_str!`s the shipped `assets/defaults/theme.toml` and asserts about a
hand-picked dozen fields, so a key present on one side and absent on the other
passes. Proven: removing three `SceneGainsData` fields from the struct while
leaving them in the file produced no new failure. Compare key sets instead —
round-trip both sides through `toml::Value` and diff.

## `vfs_getattr` does not reach the app (2026-09-20)

`Vfs.getattr` and `KernelHandle::vfs_getattr` carry `FileAttr.generation` to a
client now, but `ActorHandle` has no command for it, so
`connection/roster.rs`'s poll still reads the whole index every time. Wiring is
an `RpcCommand` variant and a `RpcActor::dispatch` arm, then the feed compares
generations and skips the read.

## `kj cas get --out` and `kj block cat --out` need async dispatchers (2026-09-20)

See "Control plane (kj): two gaps" for the shape. Recorded separately because
the attempt is instructive: a `block_in_place` bridge is not the way around it.

## The editor's terminator reconcile is two block-store calls (2026-09-20)

`mirror_ops` (`kernel/src/editor.rs`) reads the block and then, conditionally,
deletes one terminator at a fixed character offset. The two calls each take and
release the block-store entry guard, so a writer that is not an editor session —
`kj block append`, a model turn, another client — landing between them would put
the delete on changed text. Editor sessions serialize through the registry and
the equality guard makes a spurious fire unlikely, so this is a shape to close,
not an observed loss. Found by kaibo.

## `EditorCore::insert_at` round-trips through lossy text (2026-09-20)

`insert_at` (`crates/kaijutsu-editor/src/lib.rs`) rebuilds the buffer from
`self.text()`, which strips one trailing newline, so a paste into a block ending
in a blank line leaves the buffer one newline short of the block. The kernel
side no longer mistakes that for a doubled terminator, but the editor's own view
is still wrong until the next hydrate. The contained fix is for `insert_at` to
splice the rope rather than its normalized text; the larger one is the
terminator byte-fidelity work in `docs/vi.md`.

## Testing & Tooling

- A failed SSH integration assertion can also panic in russh 0.61.1
  `channels/io/mod.rs:37` during teardown, filling the log with backtraces.
  Reproduced by the shell-draft red tests; investigate harness shutdown before
  attributing the secondary panic to the behavior under test.
- `vfs::backends::local::tests::test_normal_paths_succeed` is flaky under
  full-workspace parallelism (found 2026-08-02).
- `contrib/kaijutsu-runner.sh` rebuilds only `kaijutsu-app`; a wire change
  still needs `kaijutsu-server` and `kaijutsu-mcp` rebuilt by hand
  (`docs/operating.md`).

## Conversation-view latent costs (surface slice 0, 2026-08-18)

- **`ConversationGeometry::reconcile` still never retries a skipped block.**
  On a `None` seed it `continue`s without adding the id to `rows`/
  `block_index`, but `self.block_ids = ids.to_vec()` (`view/geometry.rs:415`)
  still records the full incoming list — so `ids_match` reports no change
  next frame and `sync_conversation_geometry` never calls `reconcile` again
  for that id. The block gets no row until the doc version moves for some
  unrelated reason. Real bug, still open.
- `sync_conversation_geometry` still calls `recompute_offsets()`
  unconditionally through `Mut<_>`, marking `ConversationGeometry` changed
  every frame. Nothing consumes `Changed<ConversationGeometry>` today
  (verified) — the first consumer that tries will be silently defeated.

## `BlockContentCache` is still unbounded (surface slice 3, 2026-08-18)

`view/surface/content.rs:253`'s `BlockContentCache` still only evicts blocks
that leave the document — it grows with scroll depth, a second copy of the
conversation's rendered text alongside the block store's own. Not urgent
(strings, not glyphs; no frame-cost impact — every consumer iterates a
geometry band). Fix is the same pinned-window LRU `shape_cache` already
uses, at a different band (content ±2 screens vs. shape ±1).

## A streaming rich block still re-parses and re-draws whole, every tick (surface slice 4, 2026-08-18)

Confirmed still true: `view/surface/content.rs:9` documents "no streaming
debounce here" by design for the incremental-prefix path, but drawn rich
kinds (ABC, diff, sparkline, SVG, image; `RichKindInfo::is_drawn()`) still
shape as one chunk, whole, on the main thread every content bump — no
debounce, `rich.rs`. Bounded (budgeted parsers), not free; nobody has
measured a streamed diff on this path yet. Fix if it bites: a debounce
scoped to `is_drawn()` blocks in `Running` status only.

## ANSI rendering, pass 1: four corners left open (2026-08-19)

Stage 3.2/3.3 (`StyleSpan` → parley ranged brushes → `PositionedGlyph`) is
shipped. Still open, all verified against current code:

- **Italic (SGR 3) is parsed but never rendered** — no `FontStyle`/`Italic`
  handling found anywhere in `kaijutsu-app`'s text/view code. It's the one
  attribute that changes shaping, so it needs a column-alignment decision
  first.
- **ANSI spans are dropped on a block detected as a drawn rich kind** (ABC,
  SVG, sparkline, diff, image) — those shape through `RichShaper`, which
  never sees the styled-span list. Doesn't arise from today's ingest hooks
  (shell output is plain).
- **`INVERSE` is resolved CPU-side and stale until re-shape** — an inverted
  span bakes both colors, carries `style_index = 0`; the theme epoch forces
  a re-shape within a frame or two.
- **Backgrounds/underlines bake color into vertices** — `ShapeKey::
  baked_theme_epoch` exists for exactly this reason.

## Capability names and layout need a redesign sweep (Amy, 2026-08-20)

Still unaddressed: `Capability` (`kaijutsu-kernel/src/mcp/binding.rs:133`)
still mixes `Instance`/`Tool{instance,tool}`/`Facade` (granular), `Admin`/
`AllInstances`/`AllFacades` (broad), and bare-word verb authorities
(`Drive`/`Fork`/`Drift`/`Transport`/`Operator`/`ConfigWrite`/`Exec`/
`Editor`) — three different shapes, grown ad-hoc as gaps were found. Amy:
"Director should only get `shell` as long as it has `kj`" — director's
broad facade/exec grants (`assets/defaults/rc/director/create/S10-binding.kai`)
are worth revisiting once `kj` itself can reach what a shell used to be
for. "We'll do a cap redesign sweep soon so it's a good time to
experiment" — treat `Editor` as provisional until that sweep.

## Shell settlement follow-ups

- **`did_spill` misses nested spills (kaish, open upstream).** Only a
  top-level statement's spill reaches the program-level flag.
  `x=$(seq 1 100000)` truncates the captured value and reports
  `did_spill: false`; the same holds inside function bodies and `if`/`while`
  conditions. The shell envelope presents `did_spill` as "output was capped",
  so a caller can believe it received everything. Also upstream:
  `seq 1 100000 && echo YES` never runs the right side, because the spilled
  left operand is remapped to exit 3 before `&&` reads it, while
  `if seq 1 100000` is true. The kaish lead has a follow-up PR approved; check
  `docs/shell-envelope.md` wording when it lands.
- **No timeout knob on the server kernel**, so the wire timeout test proxies
  with `exit 124`; a real one needs an injectable `TimeoutPolicy`.
- **Interruption elsewhere still reads as a plain failure.** An interrupted
  result review now projects exit 130. A structured `kj` call returns the
  refusal reason as an RPC error before `exec_result` is read
  (`runtime/structured.rs`, the `Refused` early return), and an unpublished
  approval pair recovered at restart (`approval_resume.rs`,
  `recover_unpublished_pairs`) projects 1. Decide whether either should carry
  the interruption marker.
- **A failed wake after a completion notice is never retried.**
  `completion_notice::deliver` commits the notice block, which stamps
  `block_id` (`block_store.rs`, `insert_completion_notice`), then calls
  `request_turn`. If that fails (context archived during the
  `gate_resume_window` await, or the worker shutting down), every later scan and
  startup recovery skips the row because they select `block_id IS NULL`. The
  notice block survives; the owed continuation is lost with no durable record.
  Approval delivery loses its wake the same way by design, but keeps a
  redeemable answer. Decide between recording an owed wake durably and
  reserving turn admission before the notice insert; see the resource admission
  design, `docs/resource-admission.md`, "Known hazards".
- **Uncovered:** scheduler call sites of tick/rotate (`beat.rs` ~2225, ~2230);
  the mailbox notice block for a paused PostCall; wire-level interactive cancel
  (no wire cancel exists for durable interactive commands).

## Waiting on upstream

Each of these needs a change in another project before anything here can
move. Re-check when that dependency's version changes.

### Kaish positional suffix expansion

The locked kaish 0.17.2 expands `${0%.kai}` to an empty value, rather than
stripping the suffix or rejecting unsupported syntax. The rc migration exposed
this as a failed companion-file read. Shipped scripts use `dirname "$0"` and
`basename "$0" .kai` instead. Audit unsupported parameter expansion in kaish;
keep it explicit rather than silently accepting a different expression.
No upstream issue has been posted.

### OSC 8 hyperlinks wait on a ratatui span attribute (2026-09-13)

`present::links` detects the paths and URLs a hyperlink would target, with
tests, but nothing emits one: ratatui 0.30.2 and ratatui-core 0.1.2 carry
no hyperlink attribute on a `Style` or a `Span`, and the backend diffs
cells — escape bytes smuggled into a cell's symbol would be miscounted as
width and overwritten by the next diff. The exit is a ratatui feature that
adds the attribute, or a custom backend that writes OSC 8 around a cell's
own bytes; neither is built.

### Hi-res wheel (v120) blocked at winit/sctk — slow drags are a compositor dead zone (2026-08-16)

Root cause confirmed, not app-side: MX Master emits sub-detent v120 →
sctk 0.19.2 has no `AxisValue120` handler (verified in the cargo cache) →
winit 0.30.13 pins that sctk and has zero value120 references even on its
own master branch, so the block is winit, not sctk or the compositor —
switching KWin→Mutter would not fix it.

**Lane PARKED by Amy's call** ("fix kaijutsu-app for what already works
… experiment later with the HID++ device"). Carried forks exist and were
protocol-verified live (`~/src/research/{client-toolkit,winit}`, branches
`tobert/axis-value120-0.19` and `tobert/wayland-axis-value120-0.30`) but
`[patch.crates-io]` was removed from `Cargo.toml` per the no-committed-
path-deps rule; re-wire via the `tobert/*` GitHub forks when resumed. Root
probe also found the Bolt receiver (046d:c548) runs on `hid-generic`, not
`hid_logitech_dj` — hidpp never manages the mouse, a separate,
possibly-upstreamable one-line kernel fix
(`~/src/research/bolt-dj-bind-test.sh`). Do not re-add smoothing hacks
meanwhile; pipeline stays fraction-ready.

### `ExecResult.output` cannot carry structured data past kaish's output limiter (found 2026-07-18)

Still true on kaish 0.17.1: `materialize()` (`kaish-types/src/result.rs:504`)
clears `.output` unconditionally even when `.out` never consumed it. `kj`
works around it by writing only `.data`, bridged at `block_output_data`
(`kaijutsu-server/src/rpc.rs`), regression-pinned in `kj_builtin.rs`. The
clean fix is upstream: clear `.output` only inside the `if .out.is_empty()`
branch.

### vte 0.15.0 drops a control byte after a chunked partial UTF-8 codepoint (2026-08-19)

Upstream bug in `vte::Parser::advance_partial_utf8` (not `kaijutsu-ansi`):
`strip(&[0xCD, 0xAE, 0x1B, 0xFF])` differs when fed as one chunk vs. two —
chunked feed leaks an extra replacement character and silently drops the
`0x1B` (ESC) that followed a resumed multi-byte codepoint. Narrow trigger
(incomplete UTF-8 lead byte as the last byte of a `feed` call, continuation
immediately followed by a control byte in the next) — exactly the shape of
chunked kaish output mixing multibyte text and escapes. Pinned as a
regression test asserting the *current* buggy divergence:
`crates/kaijutsu-ansi/tests/vte_partial_utf8_regression.rs` — a `vte`
upgrade that fixes it fails this test loudly. Not worked around in
`kaijutsu-ansi` (would mean reimplementing vte's UTF-8 resumption). Fix:
file upstream against `alacritty/vte`, or vendor-patch if needed sooner.
