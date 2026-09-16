# Architecture debt scan — 2026-09-16

Amy: "prefer reading whole files so you can think holistically" and "this is
more about how things fit together than pedantics or efficiency."

Reviewed at `f7e46f8e`, with a clean working tree before this report. This is
a source review, not a runtime audit. No implementation changed and no tests
or live-kernel probes ran. Whole smaller files and complete responsibilities
within larger files supplied the evidence; the largest modules were not read
in full. Music scheduling, provider internals, rendering internals, and the
entire approval policy were outside this pass.

The recurring problem is divided ownership. A newer representation replaces
an older one, but callers retain the older representation's responsibilities.
The resulting fixes synchronize several owners instead of removing one.

## 1. Make an accepted document mutation one operation

**Observed.** `BlockStore::edit_text_as` mutates `BlockDocument` and advances
the document version under a DashMap guard. It releases that guard before
`journal_op`, which separately allocates an oplog sequence, writes SQLite,
and may compact. The caller publishes its event afterward. `set_status`,
`set_excluded`, and other setters follow the same pattern.

The SQLite part now correctly commits the op row and activity stamp together.
The old issue claiming those are two autocommit statements is obsolete. It
does not make the entire mutation atomic: the in-memory change precedes that
transaction, and publication follows a separate lock acquisition.

**Consequence inferred from the code.** Writer A can mutate before B but
journal or publish after B. A failed journal can leave the in-memory document
changed even though the caller receives an error. Compaction also snapshots
memory and reads the journal head separately from the database write. These
are structural possibilities; this pass did not reproduce an interleaving.

**Simplification.** Give the existing block store one operation that owns
validation, ordering, durable commit, in-memory acceptance, and publication.
Choose one authoritative sequence at that boundary. Keep the semantic log and
the client change feed distinct; replace the repeated mutation/journal/emit
protocol at every setter. A new distributed system or generic event framework
would add nothing here.

**First test.** Force two writers to interleave at the current guard-release
boundary, then compare live state, replayed state, and delivered event order.
Inject a commit failure and require either unchanged accepted state or a fatal
store failure. Use barriers or fault injection, not sleeps.

Evidence: `crates/kaijutsu-kernel/src/block_store.rs`, `journal_op` (1020),
`compact_document` (1116), `set_status` (1501), `edit_text_as` (1590);
`docs/change-feed.md`, "Classification happens at the mutation".

## 2. Separate conversation lifetime from cache eviction and turn exclusion

**Observed.** `ConversationMailbox` deliberately remembers the content it
first folded for each block. Later edits stay out of that live conversation.
`ConversationCache` owns both that state and the mutex used to serialize turns
for a context. LRU eviction discards the conversation and its mutex. Explicit
`evict` does the same, even while a turn retains an Arc to the old mutex.
Gate-resume uses eviction to make an in-place result fill visible.

**Consequence.** This cache holds semantic state. Whether an edit reaches the
next model request can depend on unrelated contexts displacing its entry.
Invalidation also replaces the synchronization object on which turn exclusion
depends. Calling this ordinary cache management hides both responsibilities.

**Simplification.** Keep a stable per-context turn owner. Make conversation
reset an explicit transition of that owner, deferred or refused while a turn
is active. Preserve the current distinction between the editable context and
the append-only conversation. Whether an idle conversation may be discarded
and rebuilt is a product decision; the implementation should name that
boundary instead of letting an LRU decide it incidentally.

**First tests.** Evict/reset while a turn holds the context and prove a second
turn cannot run beside it. Compare an edited conversation with and without
cache pressure and pin the intended reset semantics.

Evidence: `crates/kaijutsu-kernel/src/llm/mailbox.rs` (whole file);
`crates/kaijutsu-server/src/rpc.rs`, `ConversationCache` (172),
`get_or_create` (206), `evict` (256); `llm_stream.rs` (1778).
Related existing issue: "The cached mailbox never re-reads a block it has seen."

## 3. Put turn and shell execution under one runtime owner

**Observed.** `Kernel` owns liveness, shell operations, and event buses.
`SharedKernelState` in server `rpc.rs` owns conversation sessions, interrupts,
the dispatcher, and shutdown. That same RPC module hosts the headless turn
driver and approval resumption. Kernel code publishes a turn request for the
server to execute, even when no connection is involved.

Shell construction is already shared through `materialize_context_kaish_*`.
Execution settlement is not. Server `shell_run::run_into_blocks` performs
result projection, cwd/env persistence, hook processing, job completion, and
receipt completion. The kernel's MCP `ShellServer` has a separate asynchronous
completion path. `shell_run.rs` imports its execution helpers back from
`rpc.rs`. Results move between kaish results, blocks, and shell envelopes;
the server reconstructs a job result and receipt from settled blocks.

**Consequence.** A transport adapter has become part of the application
runtime, and each entry path has to know how to finish an operation. The
existing PostCall-output/exit-code discrepancy and unfinished-completion
issues are examples of the maintenance burden.

**Simplification.** Move the existing turn driver and shell settlement into
kernel runtime code with one owner for active turns and one settled shell
outcome. Let RPC, MCP, and approval resume submit to it. Project that outcome
into blocks, receipts, and transport results. Keep kaish's JobManager as the
execution mechanism and the receipt as durable bookkeeping; these have
different lifetimes and should not be collapsed into one object.

Preserve explicit differences: interactive cwd/env write-back, read-only
shells, internal versus model output, hook policy, and connection context
switching. Those differences need named choices at submission, not duplicate
settlement implementations or implicit flavor booleans spread across callers.

**First test.** Run the same command through interactive, model, and approved
resumption paths. Check the agreed outcome fields, cancellation, and exactly
one terminal settlement, including a PostCall substitution and a failure.

Evidence: `crates/kaijutsu-server/src/shell_run.rs` and
`crates/kaijutsu-kernel/src/shell_operations.rs` (whole files);
server `rpc.rs`, `SharedKernelState` (380), `spawn_turn_driver` (546),
`spawn_gate_resume_driver` (1268); kernel `mcp/servers/shell.rs` (446–737).

## 4. Remove the surviving draft filesystem route

**Observed.** Yesterday's decision says the compose draft must have no model
path. `InputFilesystem` still provides read, write, and clear at `/v/input`.
`EmbeddedKaish::with_identity_mode` mounts it in every shell. The read-only
shell wraps it against writes but still exposes reads. The model tool
description explicitly advertises it.

The mount receives `principal_id`, the authenticated requester. The model
shell's `KjBuiltin` separately receives `actor_id`, the performer. For a model
turn requested by Amy, the constructor chain therefore selects Amy's draft
for `/v/input`. This is source-path evidence; no live draft was accessed.

**Simplification.** Remove `InputFilesystem` and its mount, imports, help, and
tests that require that interface. Keep compose input in the client/RPC path.
Then review ordinary block mutation and `/v/docs` access to draft blocks as
part of the same invariant; removing one pathname alone does not prove the
broader invariant. Those additional routes were not audited here.

**First test.** With distinct requester and performer, prove neither model
shell flavor can read or change the requester's draft. Keep the app/TUI input
identity tests as positive coverage.

Evidence: `crates/kaijutsu-kernel/src/runtime/input_filesystem.rs` (whole file);
`runtime/embedded_kaish.rs` (336, 485); `kj/context_shell.rs` (241–397);
`mcp/servers/shell.rs` (145–158, 446–476).

## 5. Finish sharing client recovery and stop copying domain state to render it

**Observed.** `ContextMirror` shares event application. `DocumentStore`
shares some receiver bookkeeping. Subscribe/fetch, rehydrate tasks, failures,
and stale-response handling remain in the app, TUI, and ACP separately.
The app launches refetches from `view/sync.rs`; ACP awaits recovery in its
pump; the TUI has its own buffering, task, and release machinery.

Their failure policies already differ. App refetch failure logs and leaves
recovery unfinished; TUI releases the view and names the reattach gesture;
ACP logs that its stream is stale. Those are observable code paths, not
evidence that all clients must have identical presentation policy.

The app also clones `ContextMirror` blocks into `RenderBlockStore` on every
mirror-version change. That second store maintains an index, version,
generation, and local collapse values. Its rebuild has to copy collapse
choices out and put them back so a streamed delta does not undo user intent.

**Simplification.** Let `kaijutsu-client` own the followed context's feed,
mirror, recovery generation, and snapshot application together. Clients choose
retry/release presentation, but should not implement protocol correctness.
Have the app render the mirror with separate per-view collapse/selection state.
Keep layout and glyph caches; retire the second mutable domain-block store
for live conversations. Welcome/offline content still needs a local source.

**First tests.** A reconnect snapshot races a delta, a second reconnect, and
context release; no stale response can replace current state. A local collapse
choice survives streaming and context switching without rewriting snapshots.

Evidence: `crates/kaijutsu-client/src/document_store.rs`,
`crates/kaijutsu-app/src/view/sync.rs`, `view/render_store.rs`, and
`crates/kaijutsu-tui/src/bridge.rs` (whole files);
`kaijutsu-tui/src/run.rs` (2114–2312); `kaijutsu-acp/src/session.rs` (214–270).

## 6. Retire the old kernel shell-state API and resolve inert consent settings

**Observed.** `KernelState` still supplies a second kernel UUID, variables,
command history, and checkpoints. Workspace call searches found the variable,
history, and checkpoint facade used by its own implementation/tests rather
than the production context-shell paths. The real shell uses durable
`context_env`/`context_shell` and a fresh kaish scope. The kernel name remains
a separate concern; do not delete the entire container blindly.

There is also a real settings split. `kj context set --consent` persists
`ContextRow.consent_mode`; the model turn instead reads the kernel-wide
`Kernel::consent_mode()` to choose its iteration cap. No workspace caller of
`set_consent_mode` was found. Both kernel constructors initialize it to
Collaborative. The stored per-context choice therefore does not control this
consumer. `--system-prompt` being inert is already recorded separately.

**Simplification.** Delete the unused shell-state facade and its bookkeeping.
Decide whether consent still earns a setting: either resolve it from context
configuration at turn start or remove its published setting and stored field.
Do not repair the gap by copying context settings into a global variable.

**First test.** Two contexts with different configured consent modes produce
the intended distinct turn limits, or the retired option is rejected clearly.
Compilation and caller checks should cover the dead API deletion.

Evidence: `crates/kaijutsu-kernel/src/state.rs` (whole file); `kernel.rs`
(1340, 1982–2027, 2064–2071); `kj/context.rs` (512–544);
`crates/kaijutsu-server/src/llm_stream.rs` (684, 1898).

## 7. Revisit file documents as a requirement of ordinary reads

**Observed.** `FileDocumentCache` turns ordinary text files into durable
single-block documents. It then needs a second durable marker to distinguish
recoverable unsaved work from a stale clean copy. `MountBackend` routes file
reads and writes through that cache; editor pins and explicit invalidation
keep the representations coherent. The incident and guard fixes in
`docs/file-buffers.md` are consequences of this ownership arrangement.

**Simplification candidate, already proposed in that document.** Ordinary
reads should not require durable documents. Materialize an editor buffer when
editing begins, and persist unsaved work with its recovery metadata together.
Keep the explicit conflict and swap-recovery behavior. This is a design slice,
not permission to remove those protections. Its benefit is fewer states that
can disagree, rather than faster reads.

This pass read the cache's data model, load/reconcile, invalidation, and dirty
marker paths, not the whole cache or editor implementation. A full caller
review is needed before choosing the exact buffer representation. The already
decided MCP file-tool removals are a separate, smaller deletion.

## Smaller follow-ups from the second opinion

`Kernel::new` and `Kernel::with_flows` duplicate the full initialization of
the kernel's services. Their differing fields are `id` and `block_flows`.
Have `new` delegate with its existing defaults so new services and startup
recovery cannot drift between constructors. This is a bounded deletion,
independent of reorganizing runtime ownership.

The eight `materialize_context_kaish_*` entry points express four execution
profiles with default or explicit identity. Their construction body is already
shared. Consider an explicit invocation value when consolidating runtime
submission, but preserve explicit identity and structural read-only execution
policy. Replacing these functions with optional identity and several booleans
would merely hide the same choices.

The module contract in `kj/context_shell.rs` says durable shell changes happen
only through `kj context set`. Interactive `shell_run.rs` also writes cwd and
exported environment through `rpc::persist_shell_state`. That behavior is
intentional; correct the contract when naming submission policies.

## Order of work

The live [cleanup plan](../issues.md#architecture-cleanup-plan) breaks this
order into bounded deletions, verification requirements, and deferred design
choices. Source TODOs point to its issue entries.

1. Close the draft route and retire verified unused APIs as bounded deletions.
2. Pin and repair document mutation ordering before reorganizing its callers.
3. Establish a stable turn owner, then move transport-independent execution
   and settlement behind it.
4. Finish the shared client recovery lifecycle and remove the app's duplicate
   conversation representation.
5. Revisit file-buffer persistence as its own design change.

Keep the sound boundaries: requester/performer/reviewer are distinct; context
and conversation are distinct; the semantic journal and client projection are
distinct; batched document changes and unbatched musical timing directives
are distinct. Reducing ownership duplication should preserve those contracts.

Review attribution: Codex source review, with a Kaibo second opinion using
the `deepseek` cast (`deepseek-flash`) after Amy's explicit approval. The
review received five whole files: `kernel.rs`, `embedded_kaish.rs`,
`context_shell.rs`, `shell_run.rs`, and `shell_operations.rs`. Its response
is saved in `~/exomemory/kaijutsu/architecture-scan-2026-09-16-kaibo.json`.

The second opinion supports shared shell settlement and identified the
constructor duplication and inaccurate shell-state contract above. Its
speculation about process-global `shared_*` factories was checked and
discarded: the factories allocate fresh instances. Its proposed removal of
receipt exit-code narrowing was not accepted without checking every producer;
one path already storing an i32 does not establish the envelope's range.
Timeline retirement was not established and remains outside this pass.
