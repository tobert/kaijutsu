# Resource admission for the kernel worker

Status: slices 1 and 2 are built (`runtime/worker.rs`, `RuntimePool`): the
pool, the supervisor, `pick_thread`, local re-entry, eager start and
whole-pool shutdown, and now a bounded reservation ahead of every top-level
submission, with the reserve-first call sites listed under "Known hazards"
moved. There is still no per-thread running limit — slice 4 owns that, along
with configured limits and refusal shapes. Slices 3 to 6 are planned.

The kernel worker runs every prompt turn, `kj drive`, shell command, structured
`kj` call, MCP shell call, rc lifecycle run, approval resume, and scheduled
tick or rotate. Today it is one thread with an unbounded queue and an unbounded
set of running tasks. One slow command delays every other context, and nothing
bounds the memory held by accepted work.

Terms: a **reservation** is the resource decision, taken before anything is
written. **Admission** is the existing `ContextAdmission` proof
(`runtime/admission.rs`); a reservation comes before it and does not replace it.

## Rules

1. **Reserve first, write second.** A caller takes a reservation before it
   creates an input block, consumes a draft, registers a receipt, or mints a
   `ContextAdmission`. A refused prompt, drive, shell command, or rc run leaves
   no block, receipt, run record, or event.
2. **A reservation never waits.** Refusal is immediate. The beat's clock thread
   and any caller that already holds work can be refused; they cannot be parked.
3. **Nested work does not take a reservation.** Work admitted from a task that
   is already running on a worker thread runs on that same thread. A parent
   waiting for its child therefore cannot wait on a slot the pool has given to
   someone else.
4. **Model startup continues into inference in one task.** There is no second
   admission between them.
5. **Settlement is never refused.** A terminal outcome that SQLite will not
   accept stays in memory and retries. New top-level work is refused while the
   retained results are above a high-water mark; retries and nested work
   continue, so recovery can make progress.
6. **The pulse does not depend on the pool.** Nothing on the beat path awaits a
   reservation, a worker thread, or a queue.

## Shape

- **A pool of worker threads.** Each thread is what the single worker is today:
  a current-thread runtime with a `LocalSet`, started with the kaish thread
  stack. Task futures stay on the thread that started them.
- **One bounded admission, held for the admitted task's whole run.**
  `Kernel::reserve_runtime_slot` takes an `OwnedPermit` with `try_reserve_owned()`
  on a bounded counting channel and returns it as a `RuntimeSlot`; `spawn`
  moves the permit into the task, so it is dropped — freeing the slot — only
  when the task finishes, not when it is merely dispatched to a thread. A full
  admission is the refusal, and it is immediate: nothing here awaits a permit.
  `Kernel::spawn_runtime_task` reserves and spawns in one call, for a caller
  with no durable write of its own to order ahead of the reservation. The
  caller holds the slot across its own write when it has one, so rule 1 needs
  no second mechanism. Capacity is `ADMISSION_CAPACITY`, 64 today, and does
  not follow the thread count: it bounds accepted work, not parallelism.
  Approval delivery holds one reservation for the process lifetime. A turn's
  reservation covers startup only until slice 3: `spawn_admitted_turn` queues
  the stream as local re-entry, which takes no permit, so the cap bounds turn
  startups, not running streams. Slice 4 makes the capacity a configured
  value.
- **The supervisor dispatches.** A separate, unbounded relay from
  `reserve`/`spawn` to the chosen thread's own channel; capacity above governs
  admission, not this hand-off. It hands each piece of work to the thread with
  the fewest running tasks. Thread choice lives in one function, `pick_thread`.
- **Re-entry stays local.** A thread-local holds the current worker thread's
  own work sender. When it is set, `spawn_runtime_task` sends to that thread
  and skips the bounded queue. It does not call the free `spawn_local`: that
  panics outside a `LocalSet` poll, and `tokio::spawn`ed tasks such as broker
  pump loops run on a worker thread's runtime but outside its `LocalSet`.
  Nesting depth is bounded by the existing hook depth limit and, if
  measurement asks for it, a per-thread nested count.
- **The pool starts at kernel boot.** The worker starts lazily today, and the
  start blocks while a thread comes up. The clock thread reaches the funnel
  through `beat::fire_lifecycle`, so a lazy start would make the pulse wait.
- **All threads share one cancellation token.** A panic in any thread's task
  then stops admission everywhere and is reported by shutdown, as it is today.
  Shutdown closes the queue, dispatches what it holds, and joins every thread.
- **No context affinity.** Work for one context may run on any thread. Two
  submissions to one context that are in flight together can land their block
  pairs in either order: measured at 4 runs in 30 with both sent on one
  connection before either reply, and 0 in 15 with every task on one thread.
  `interactive::prepare` awaits `EmbeddedKaish::for_context` before
  `start_shell_operation`, and on two threads those preparations run in
  parallel. A caller that awaits each reply before sending the next is
  unaffected. Order is not a promise; pairing is: each output block names its
  own command block as parent, and each pair is written atomically (one
  insert version, captured inside the document guard). The test is
  `two_in_flight_submissions_to_one_context_each_pair_its_own_command_atomically`
  (`kaijutsu-server/tests/command_settlement_wire.rs`). If context order ever
  becomes a promise, `pick_thread` hashes the context to a thread and nothing
  else changes.
- **Refusal is a fault, not a `Refusal`.** RPC returns a capnp `Overloaded`
  error; the MCP shell tool returns a `Rejected` shell envelope a model can
  read and retry; `kj` returns an error naming the limit. `Refusal` stays
  reserved for the caller's standing (`kaijutsu-types/src/refusal.rs`).
  Shutdown and capacity produce different messages.
- **Limits are `/config/kernel` values** with generous defaults and a test
  override on `Kernel`. No latency or capacity figure is promised until the
  controlled-producer scenario has run under overlap.

Existing bounds stay: the per-MCP-instance semaphore, the four CAS preparation
slots, 64 open items per timeline, `MAX_HOOK_DEPTH`. `DELIVERY_CAP_PER_SCAN`
stays the bound on delivery throughput; the pool bounds accepted work. Neither
is a bound on model spend, which needs its own count of provider requests.

## Known hazards

- **Callers that write before asking — fixed.** `prompt::submit` (user block
  or draft), `kj drive` (seed block), and the background MCP shell tool
  (`create_operation`) used to create durable state before they reached the
  worker; each now reserves first. The MCP shell tool reserves before its
  admission mint and before the gate writes an ask; a pending call drops the
  slot unspent. `kj drive` reserves and admits before its seed-block write and
  hands both to `Kernel::request_turn_admitted`, so archive cannot refuse the
  turn after the seed lands. A pool shut down between a caller's write and
  its spawn still strands that write; see `docs/issues.md`. Approval delivery and completion delivery (`approval_resume.rs`,
  `completion_notice.rs`) still write their seed block or notice before
  calling `request_turn`, which only reserves for its own admission mint;
  covering their writes too is out of this slice.
- **Blocking re-entry paths**, all of which must reach `spawn_runtime_task` on
  a worker thread for rule 3 to hold: the editor read from `kj editor keys`;
  a shell call that waits inside a turn; `Broker::emit_notification_block` from
  the bindings tool and the `kj binding` and `kj mcp` verbs; an inline kaish
  hook body; an approved command run inline by the delivery task.
- **Long-lived occupants.** The approval delivery task never ends, and
  `kj wait` holds its task for the whole timeout while computing nothing.
  A parked-occupant guard around the delivery loop's idle wait and `kj wait`'s
  park releases the thread's running count while held. The delivery scan
  itself, which runs approved commands inline, stays counted.
- **Exactly one delivery task.** Its `woken` set and its check of a turn in
  flight before seeding are unlocked, and correct only because
  `Kernel::approval_delivery` is a `OnceLock`. The pool must not start a second.
- **Background tasks follow the thread that spawned them.** Broker pump loops,
  flush timers and `kj audio keep` jobs are `tokio::spawn`ed from worker tasks,
  so with N runtimes they scatter across threads. Spawn them on one designated
  thread or the host runtime.
- **Same-context order across transports.** Each entry point does its first
  synchronous step on the caller's thread and then awaits a reply, so one
  caller's submissions stay ordered. `start_shell_operation` and
  `consume_draft` run inside the task after an await, so two transports
  submitting to one context can land their block pairs in either order. Order
  is not promised; each pair is still written atomically under the document
  guard, and each output names its own command as parent — see "No context
  affinity" above. `pick_thread` would need to hash the context for order to
  become a promise; nothing here requires that.
- **One blocking pool per worker runtime.** `spawn_blocking` work now has N
  pools of tokio's default size, so its ceiling is N times higher.
- **Work outside the count.** `tokio::spawn` tasks started from a worker task
  (broker pump loops, flush timers, `kj audio keep`) and `spawn_blocking` work
  (`kj audio beats`, CAS preparation) are not bounded by the pool.
- **A failed wake is not retried** after a completion notice commits
  (`docs/issues.md`, "Shell settlement follow-ups"). Refusals make this more
  likely, so it is fixed before real limits ship.

## Slices

1. **Pool and dispatch, limits generous.** N threads, the supervisor,
   `pick_thread`, the worker-thread marker, local re-entry, shutdown that drains
   and joins every thread. Tests: a chain of nested work completes with the
   pool full; re-entry from a `tokio::spawn`ed task; a panic in one thread
   stops admission and is reported by shutdown; shutdown drains every thread.
   A test that needs the pool to hold work back parks every thread
   (`kj::test_helpers::park_runtime_pool`); parking one no longer holds
   anything.
2. **Built.** The bounded queue and reserve-first call sites. `try_reserve_owned`
   in the funnel (`RuntimePool::reserve`, returning a `RuntimeSlot` that
   `Kernel::reserve_runtime_slot` exposes); the reservation moved ahead of the
   writes listed above and ahead of `beat::fire_lifecycle`'s work.
   `Kernel::spawn_context_task` (the shared admission point behind shell,
   structured `kj`, and streaming submission) and `Kernel::request_turn` also
   reserve ahead of their own `ContextAdmission` mint, so every caller through
   those funnels is covered without a change at each call site. Tested through
   a stood-up kernel and its client: with the pool full, shell, structured
   `kj`, streaming, prompt and draft submission are each refused, and blocks,
   receipts, rc runs, flow events and the draft revision are unchanged. The
   clock-thread call returns while the pool is still full.
   `Kernel::fill_runtime_pool_for_test` (behind the `test-util` feature) is
   the cross-crate equivalent of `kj::test_helpers::park_runtime_pool` for a
   `kaijutsu-server` integration test: it parks every thread, then queues
   parked tasks until admission refuses, so admission is fully exhausted, not
   merely busy.
3. **One task from startup through inference.** Remove the second spawn in
   `llm_stream::spawn_admitted_turn`.
4. **Refusal shapes and config.** `Overloaded`, the `Rejected` envelope, `kj`
   text, `/config/kernel` limits; occupants exempted from the running limit.
5. **Retained-result pressure.** Byte accounting for `ShellOperationRegistry`
   and `RcSettlements`; the high-water refusal of rule 5.
6. **Delivery fairness.** Coalesce ledger wakes into the existing interval so
   scan rate follows time, not event traffic.

Tests use barriers and explicit completion delivery, no sleeps and no hosted
models.

## What the source check found

- **The task futures are `!Send`, for a shallow reason.** `ContextSwitchSink`
  (`runtime/command.rs`) is a borrowed `dyn Fn` returning a `LocalBoxFuture`,
  and `CommandRunOptions` carries it through every command future. There is no
  `Rc`, no production `thread_local!`, and no lock guard held across an await.
  Making the sink `Send` could open the way to a multi-thread runtime, but the
  rest of the tree (kaish execution, `rc::run`, provider streams) is unchecked
  and needs a compile to find out. The thread-per-`LocalSet` pool does not
  depend on it.
- **Nothing relies on the single thread for mutual exclusion.** `Kernel` is
  already shared with RPC threads and the clock thread; its state is behind
  locks. Per-context turn exclusion is an async mutex held for the turn, and
  turn admission is ordered by the database guard.
- **Every blocking re-entry reaches the funnel from a worker task.** No
  `spawn_blocking` closure reaches it. Callers on other threads hold no worker
  slot, so a full pool refuses them; it cannot deadlock them. A refused
  `emit_notification_block` drops its notification with a warning.
- **`try_reserve_owned` fits.** The pinned tokio has it, the slot is
  `Send + 'static`, and no call site holds it across an await. `request_turn`
  needs a form that takes a slot its caller already holds, because `kj drive`
  writes its seed block first.

Evidence (entry-point inventory, review findings, file and line references):
`~/exomemory/kaijutsu/resource-admission-evidence-2026-09-19.md`.
