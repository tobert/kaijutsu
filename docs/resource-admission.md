# Resource admission for the kernel worker

Status: planned. No code exists. The assumptions were checked against the
source; "What the source check found" records the result.

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
- **One bounded submission queue.** `Kernel::spawn_runtime_task` takes an owned
  slot with `try_reserve_owned()` and sends the work through that slot. A full
  queue is the refusal. The caller holds the slot while it writes its input, so
  rule 1 needs no second mechanism.
- **The supervisor dispatches.** It receives from the queue and hands each
  piece of work to the thread with the fewest running tasks, while that thread
  is under its running limit. Thread choice lives in one function,
  `pick_thread`.
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
- **No context affinity.** Work for one context may run on any thread. If
  same-context ordering turns out to be a promise, `pick_thread` hashes the
  context to a thread and nothing else changes.
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

- **Callers that write before asking.** `prompt::submit` (user block or draft),
  `kj drive` (seed block), and the background MCP shell tool
  (`create_operation`) create durable state before they reach the worker.
  Each moves its reservation ahead of that write.
- **Blocking re-entry paths**, all of which must reach `spawn_runtime_task` on
  a worker thread for rule 3 to hold: the editor read from `kj editor keys`;
  a foreground tool call inside a turn; `Broker::emit_notification_block` from
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
  submitting to one context can land their block pairs in either order. Each
  pair is still written atomically under the document guard. The slice 1
  ordering test decides whether `pick_thread` hashes the context.
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
   pool full; two back-to-back submissions to one context, held at a barrier,
   keep their block order (this test decides whether affinity is needed); a
   panic in one thread stops admission and is reported by shutdown.
2. **The bounded queue and reserve-first call sites.** `try_reserve_owned` in
   the funnel; move the reservation ahead of the writes listed above and ahead
   of `beat::fire_lifecycle`'s work. Test through a stood-up kernel and its
   client: with the pool full, shell, structured `kj`, streaming, prompt and
   draft submission are each refused, and blocks, receipts, rc runs, flow
   events and the draft revision are unchanged. The clock-thread call returns
   while the pool is still full.
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
