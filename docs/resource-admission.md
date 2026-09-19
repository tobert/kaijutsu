# Resource admission for the kernel worker

Status: planned. No code exists. The assumptions under "Checks before slice 1"
are being verified against the source.

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
- **Re-entry stays local.** A thread-local marks a worker thread. When it is
  set, `spawn_runtime_task` calls `spawn_local` on the current thread and skips
  the queue. Nesting depth is bounded by the existing hook depth limit and, if
  measurement asks for it, a per-thread nested count.
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
  Neither may count against a thread's running limit.
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

## Checks before slice 1

- The task futures are `!Send`, and what makes them so. If they are `Send`, a
  multi-thread runtime replaces the thread-per-`LocalSet` pool.
- Nothing relies on the single worker thread for mutual exclusion or ordering.
- Every blocking re-entry path reaches the funnel on a worker thread.
- `try_reserve_owned` fits each call site without holding the slot across a
  database guard in a way that can deadlock.

Evidence (entry-point inventory, review findings, file and line references):
`~/exomemory/kaijutsu/resource-admission-evidence-2026-09-19.md`.
