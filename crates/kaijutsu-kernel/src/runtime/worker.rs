//! Kernel-owned executor for commands, model turns, and approval delivery.
//! It outlives submitting transports and supports reentrant calls on the kaish stack.
//!
//! A pool of threads runs the work. Each thread is a current-thread runtime
//! with a `LocalSet`, so task futures stay on the thread that started them; a
//! supervisor thread receives submitted work and hands each piece to a thread.
//! See `docs/resource-admission.md`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use futures::FutureExt;
use tokio_util::sync::CancellationToken;

type Work = Box<dyn FnOnce(CancellationToken) -> Pin<Box<dyn Future<Output = ()>>> + Send>;

/// Threads in a pool whose size no caller states. Four threads spread the work
/// a kernel runs at once without reserving a core per thread on a large host.
pub(crate) fn default_pool_threads() -> usize {
    std::thread::available_parallelism().map(|count| count.get()).unwrap_or(1).min(4)
}

/// Top-level reservations the pool holds at once, running or queued. It bounds
/// accepted work, not parallelism, so it does not follow the thread count: a
/// streaming turn holds its reservation for its whole run, and the approval
/// delivery task holds one for the process lifetime. Slice 4 of
/// `docs/resource-admission.md` makes it a configured value.
pub(crate) const ADMISSION_CAPACITY: usize = 64;

/// One pool thread's work channel and the count of work assigned to it.
/// The count rises when work is sent and falls when the task finishes, so
/// [`pick_thread`] reads assigned work rather than only started work.
struct ThreadHandle {
    work: tokio::sync::mpsc::UnboundedSender<Work>,
    assigned: Arc<AtomicUsize>,
}

impl ThreadHandle {
    /// Count the work against its thread before sending it, so a burst of
    /// dispatches spreads instead of piling onto one unread queue.
    fn send(&self, work: Work) -> Result<(), String> {
        self.assigned.fetch_add(1, Ordering::SeqCst);
        if self.work.send(work).is_err() {
            self.assigned.fetch_sub(1, Ordering::SeqCst);
            return Err("runtime worker stopped before accepting work".into());
        }
        Ok(())
    }
}

/// A pool thread's own work channel, held weakly. The supervisor owns the one
/// strong sender per thread, so dropping it still closes the channel and ends
/// the thread's drain.
struct WeakThreadHandle {
    work: tokio::sync::mpsc::WeakUnboundedSender<Work>,
    assigned: Arc<AtomicUsize>,
}

thread_local! {
    /// The work channel of the pool thread running this code, set before the
    /// thread's runtime starts. Reentrant submission reads it and sends to its
    /// own thread, so a task waiting for nested work never waits for a slot
    /// the pool gave to another caller.
    ///
    /// A thread-local, not a task-local: `tokio::spawn`ed tasks such as broker
    /// pump loops and flush timers run on a pool thread's runtime but outside
    /// its `LocalSet`, and the free `tokio::task::spawn_local` panics there.
    /// This is the crate's only production thread-local; nothing else may read
    /// or write it, because a value left on a thread that is not a pool thread
    /// would send work to a thread that is not draining it.
    static POOL_THREAD: std::cell::OnceCell<WeakThreadHandle> = const { std::cell::OnceCell::new() };
}

/// The pool thread this code runs on, or `None` off the pool and once the
/// supervisor has dropped this thread's channel.
fn current_pool_thread() -> Option<ThreadHandle> {
    POOL_THREAD
        .try_with(|cell| {
            cell.get().and_then(|handle| {
                handle.work.upgrade().map(|work| ThreadHandle { work, assigned: handle.assigned.clone() })
            })
        })
        .ok()
        .flatten()
}

enum ThreadEvent {
    Completed(Result<(), tokio::task::JoinError>),
    Work(Option<Work>),
}

// Reap completed tasks before admitting more queued work so a busy producer
// cannot postpone failure detection and retain finished tasks indefinitely.
async fn next_thread_event(
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<Work>,
    tasks: &mut tokio::task::JoinSet<()>,
) -> ThreadEvent {
    tokio::select! {
        biased;
        result = tasks.join_next(), if !tasks.is_empty() => {
            ThreadEvent::Completed(result.expect("a nonempty JoinSet has a next task"))
        }
        work = receiver.recv() => ThreadEvent::Work(work),
    }
}

/// One pool thread. Its loop ends when the supervisor drops its channel, so
/// nothing the supervisor dispatched can arrive after the thread has stopped
/// reading. Failures are returned for the supervisor to aggregate.
fn run_pool_thread(
    stopped: CancellationToken,
    mut receiver: tokio::sync::mpsc::UnboundedReceiver<Work>,
    assigned: Arc<AtomicUsize>,
    own: tokio::sync::mpsc::WeakUnboundedSender<Work>,
    started: std::sync::mpsc::SyncSender<Result<(), String>>,
) -> Result<(), String> {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => { let _ = started.send(Err(error.to_string())); return Err(error.to_string()); }
    };
    let _entered = runtime.enter();
    let local = tokio::task::LocalSet::new();
    let _ = POOL_THREAD.with(|cell| cell.set(WeakThreadHandle { work: own, assigned: assigned.clone() }));
    if started.send(Ok(())).is_err() { return Err("runtime worker startup receiver disappeared".into()); }
    local.block_on(&runtime, async move {
        let mut tasks = tokio::task::JoinSet::new();
        let mut failures = Vec::new();
        loop {
            match next_thread_event(&mut receiver, &mut tasks).await {
                ThreadEvent::Work(Some(work)) => { tasks.spawn_local(work(stopped.child_token())); }
                ThreadEvent::Work(None) => break,
                ThreadEvent::Completed(result) => {
                    assigned.fetch_sub(1, Ordering::SeqCst);
                    if let Err(error) = result {
                        tracing::error!("runtime task failed: {error}");
                        failures.push(error.to_string());
                        // The pool shares one token, so a failed task stops
                        // admission on every thread rather than only this one.
                        stopped.cancel();
                    }
                }
            }
        }
        // Keep the runtime alive through command settlement and turn
        // finalization; the supervisor has already stopped sending.
        while let Some(result) = tasks.join_next().await {
            assigned.fetch_sub(1, Ordering::SeqCst);
            if let Err(error) = result {
                tracing::error!("runtime task failed during shutdown: {error}");
                failures.push(error.to_string());
            }
        }
        if failures.is_empty() { Ok(()) } else { Err(failures.join("; ")) }
    })
}

struct PoolThread {
    handle: ThreadHandle,
    thread: std::thread::JoinHandle<Result<(), String>>,
}

fn start_pool_thread(index: usize, shutdown: &CancellationToken) -> Result<PoolThread, String> {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<Work>();
    let assigned = Arc::new(AtomicUsize::new(0));
    let (started, ready) = std::sync::mpsc::sync_channel(1);
    let (stopped, own, counter) = (shutdown.clone(), sender.downgrade(), assigned.clone());
    let thread = crate::spawn_kaish_thread(format!("kernel-runtime-{index}"), move || {
        run_pool_thread(stopped, receiver, counter, own, started)
    }).map_err(|error| error.to_string())?;
    ready.recv().map_err(|_| "runtime worker stopped during startup".to_string())??;
    Ok(PoolThread { handle: ThreadHandle { work: sender, assigned }, thread })
}

/// Choose the thread for one piece of top-level work: the fewest pieces
/// already assigned, ties to the lowest index. Thread choice is made here and
/// nowhere else, so context affinity is a change to this function alone.
fn pick_thread(threads: &[PoolThread]) -> usize {
    threads.iter().enumerate()
        .min_by_key(|(_, thread)| thread.handle.assigned.load(Ordering::SeqCst))
        .map(|(index, _)| index)
        .expect("a runtime pool has at least one thread")
}

fn dispatch(threads: &[PoolThread], work: Work) {
    let index = pick_thread(threads);
    if let Err(error) = threads[index].handle.send(work) {
        tracing::error!("runtime worker thread {index} refused dispatched work: {error}");
    }
}

/// One reservation, taken before a caller writes anything durable
/// (`docs/resource-admission.md`, rule 1). Local re-entry (rule 3) needs no
/// reservation and carries the parent thread's handle instead; a top-level
/// caller carries an [`tokio::sync::mpsc::OwnedPermit`] that stays alive for
/// the admitted task's whole run, so capacity is released on completion, not
/// on dequeue — a submission still queued for its thread when the pool is
/// "full" must count against the limit exactly like one already running.
///
/// The permit is never sent through its channel; it is simply held and
/// dropped. The channel exists only as a bounded counter reachable from
/// `try_reserve_owned()`, matching the doc's own naming for this mechanism.
enum RuntimeSlotInner {
    Local(ThreadHandle),
    Reserved { permit: tokio::sync::mpsc::OwnedPermit<()>, queue: tokio::sync::mpsc::UnboundedSender<Work> },
}

/// A held place in the runtime pool, reserved before its caller's durable
/// write and spent afterward with [`RuntimeSlot::spawn`]. The type is public
/// so a caller in another crate (the beat scheduler) can hold one across its
/// own write and hand it to the kaijutsu-kernel call that spends it; its
/// construction and spending stay crate-private.
pub struct RuntimeSlot(RuntimeSlotInner);

impl RuntimeSlot {
    /// Spend the reservation on `work`. Local re-entry sends directly to the
    /// owning thread; a top-level reservation moves its permit into the task
    /// so it drops — releasing capacity — only when the task finishes.
    pub(crate) fn spawn<F, W>(self, work: W) -> Result<(), String>
    where F: Future<Output = ()> + 'static, W: FnOnce(CancellationToken) -> F + Send + 'static {
        match self.0 {
            RuntimeSlotInner::Local(thread) => {
                let work: Work = Box::new(move |stop| Box::pin(async move { work(stop).await }));
                thread.send(work)
            }
            RuntimeSlotInner::Reserved { permit, queue } => {
                let work: Work = Box::new(move |stop| Box::pin(async move {
                    let _reservation = permit;
                    work(stop).await;
                }));
                queue.send(work).map_err(|_| "runtime worker stopped before accepting work".to_string())
            }
        }
    }
}

/// Receive submitted work and hand each piece to a thread. The supervisor runs
/// on its own thread so a task that blocks its thread cannot stall dispatch for
/// the rest of the pool.
fn run_supervisor(
    stopped: CancellationToken,
    mut queue: tokio::sync::mpsc::UnboundedReceiver<Work>,
    threads: Vec<PoolThread>,
) -> Result<(), String> {
    let mut failures = Vec::new();
    match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime.block_on(async {
            loop {
                tokio::select! {
                    biased;
                    _ = stopped.cancelled() => break,
                    work = queue.recv() => match work {
                        Some(work) => dispatch(&threads, work),
                        None => break,
                    },
                }
            }
            // Refuse further submissions, hand the accepted ones to their
            // threads, then let each thread drain by dropping its channel.
            queue.close();
            stopped.cancel();
            while let Some(work) = queue.recv().await { dispatch(&threads, work); }
        }),
        Err(error) => { stopped.cancel(); failures.push(error.to_string()); }
    }
    let (handles, joins): (Vec<_>, Vec<_>) =
        threads.into_iter().map(|thread| (thread.handle, thread.thread)).unzip();
    drop(handles);
    for (index, join) in joins.into_iter().enumerate() {
        match join.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failures.push(error),
            Err(_) => failures.push(format!("runtime worker thread {index} panicked")),
        }
    }
    if failures.is_empty() { Ok(()) } else { Err(format!("runtime worker failed: {}", failures.join("; "))) }
}

pub(crate) struct RuntimePool {
    queue: tokio::sync::mpsc::UnboundedSender<Work>,
    /// The bounded admission channel `reserve()` takes a permit from, sized
    /// [`ADMISSION_CAPACITY`]. Never sent through — see [`RuntimeSlotInner`] —
    /// so its only job is the counting `try_reserve_owned()` gives us.
    admission: tokio::sync::mpsc::Sender<()>,
    /// Kept alive so `admission`'s reservations never see the channel as
    /// closed. Never read: nothing is ever sent, so there is nothing to
    /// receive.
    #[allow(dead_code)]
    admission_receiver: tokio::sync::mpsc::Receiver<()>,
    admission_capacity: usize,
    shutdown: CancellationToken,
    worker_threads: usize,
    /// Every thread the pool owns, the supervisor included, so [`Self::join`]
    /// refuses a caller that would be waiting for itself.
    threads: Vec<std::thread::ThreadId>,
    joined: futures::future::Shared<futures::future::BoxFuture<'static, Result<(), String>>>,
}

impl RuntimePool {
    /// Start `threads` worker threads and the supervisor that feeds them.
    /// `shutdown` is the pool's one token: every task holds a child of it, and
    /// a task failure on any thread cancels it for all of them.
    pub(crate) fn start(threads: usize, shutdown: CancellationToken) -> Result<Self, String> {
        let threads = threads.max(1);
        let admission_capacity = ADMISSION_CAPACITY;
        let (queue, receiver) = tokio::sync::mpsc::unbounded_channel::<Work>();
        let (admission, admission_receiver) = tokio::sync::mpsc::channel::<()>(admission_capacity);
        let mut pool: Vec<PoolThread> = Vec::with_capacity(threads);
        let mut ids = Vec::with_capacity(threads + 1);
        for index in 0..threads {
            match start_pool_thread(index, &shutdown) {
                Ok(thread) => { ids.push(thread.thread.thread().id()); pool.push(thread); }
                Err(error) => {
                    shutdown.cancel();
                    let (handles, joins): (Vec<_>, Vec<_>) =
                        pool.into_iter().map(|thread| (thread.handle, thread.thread)).unzip();
                    drop(handles);
                    for join in joins { let _ = join.join(); }
                    return Err(error);
                }
            }
        }
        let stopped = shutdown.clone();
        let supervisor = crate::spawn_kaish_thread("kernel-runtime-pool", move || run_supervisor(stopped, receiver, pool))
            .map_err(|error| { shutdown.cancel(); error.to_string() })?;
        ids.push(supervisor.thread().id());
        let joined = async move {
            tokio::task::spawn_blocking(move || supervisor.join().map_err(|_| "runtime worker supervisor panicked".to_string())?)
                .await.map_err(|error| format!("runtime worker join failed: {error}"))?
        }.boxed().shared();
        Ok(Self { queue, admission, admission_receiver, admission_capacity, shutdown, worker_threads: threads, threads: ids, joined })
    }

    pub(crate) fn worker_threads(&self) -> usize { self.worker_threads }

    /// Take one reservation, before the caller writes anything durable
    /// (`docs/resource-admission.md`, rule 1). Local re-entry (rule 3) needs
    /// none: work submitted from a pool thread runs on that same thread, so a
    /// task waiting for its child never waits on a slot the pool gave to
    /// someone else. A refusal here is immediate — never a wait (rule 2).
    pub(crate) fn reserve(&self) -> Result<RuntimeSlot, String> {
        if self.shutdown.is_cancelled() { return Err("runtime worker is shut down".into()); }
        match current_pool_thread() {
            Some(thread) => Ok(RuntimeSlot(RuntimeSlotInner::Local(thread))),
            None => {
                let permit = self.admission.clone().try_reserve_owned().map_err(|_| format!(
                    "kernel runtime worker is at capacity ({} admitted reservations already running or \
                     queued); refused — the limit is generous today and not yet configurable, see \
                     docs/resource-admission.md",
                    self.admission_capacity,
                ))?;
                Ok(RuntimeSlot(RuntimeSlotInner::Reserved { permit, queue: self.queue.clone() }))
            }
        }
    }

    /// Reserve and spend a slot in one call, for callers with no durable
    /// write of their own to order ahead of the reservation.
    pub(crate) fn submit<F, W>(&self, work: W) -> Result<(), String>
    where F: Future<Output = ()> + 'static, W: FnOnce(CancellationToken) -> F + Send + 'static {
        self.reserve()?.spawn(work)
    }

    pub(crate) fn stop(&self) { self.shutdown.cancel(); }

    pub(crate) async fn join(&self) -> Result<(), String> {
        if self.threads.contains(&std::thread::current().id()) {
            return Err("runtime worker cannot wait for its own shutdown".into());
        }
        self.joined.clone().await
    }
}

impl Drop for RuntimePool {
    fn drop(&mut self) { self.stop(); }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread::ThreadId;
    use tokio::sync::oneshot;

    /// Every wait a pool test makes is bounded, so a pool that never dispatches
    /// fails the test instead of hanging the suite.
    const WAIT: std::time::Duration = std::time::Duration::from_secs(10);

    /// Occupy one task slot on some pool thread: the task reports the thread it
    /// runs on and then waits for its release, holding the slot without
    /// blocking the thread.
    async fn occupy(kernel: &crate::Kernel) -> (ThreadId, oneshot::Sender<()>) {
        let (entered, running) = oneshot::channel();
        let (release, held) = oneshot::channel();
        kernel.spawn_runtime_task(move |_| async move {
            entered.send(std::thread::current().id()).unwrap();
            let _ = held.await;
        }).unwrap();
        let thread = tokio::time::timeout(WAIT, running).await.expect("the pool must run submitted work").unwrap();
        (thread, release)
    }

    /// Occupy a whole thread: the task blocks the thread until released, so
    /// work dispatched to it stays queued.
    async fn block_thread(kernel: &crate::Kernel) -> (ThreadId, std::sync::mpsc::Sender<()>) {
        let (entered, running) = oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        kernel.spawn_runtime_task(move |_| async move {
            entered.send(std::thread::current().id()).unwrap();
            held.recv().unwrap();
        }).unwrap();
        let thread = tokio::time::timeout(WAIT, running).await.expect("the pool must run submitted work").unwrap();
        (thread, release)
    }

    /// Admission bounds accepted work, not parallelism: a one-thread pool
    /// still admits [`super::ADMISSION_CAPACITY`] held reservations, and
    /// refuses the next one immediately.
    #[tokio::test]
    async fn admission_capacity_does_not_follow_the_thread_count() {
        let kernel = crate::Kernel::new_ephemeral_with_threads("pool-admission", 1).await;
        let held: Vec<_> = (0..super::ADMISSION_CAPACITY)
            .map(|index| kernel.reserve_runtime_slot().unwrap_or_else(|error| panic!("reservation {index}: {error}")))
            .collect();
        let refused = kernel.reserve_runtime_slot().err().expect("one past capacity must be refused");
        assert!(refused.contains("at capacity (64 "), "{refused}");
        drop(held);
        assert!(kernel.reserve_runtime_slot().is_ok(), "dropped reservations must free admission");
        kernel.shutdown_runtime_worker().await.unwrap();
    }

    /// Submit a chain of `depth` nested tasks, each from inside the task above
    /// it and each awaiting its child, and report the threads they ran on from
    /// the innermost outward.
    fn nested_chain(
        kernel: Arc<crate::Kernel>, depth: usize, report: oneshot::Sender<Vec<ThreadId>>,
    ) -> Result<(), String> {
        let owner = kernel.clone();
        kernel.spawn_runtime_task(move |_| async move {
            let here = std::thread::current().id();
            if depth == 0 { report.send(vec![here]).unwrap(); return; }
            let (child_report, child) = oneshot::channel();
            nested_chain(owner, depth - 1, child_report).expect("nested work must be accepted");
            let mut threads = child.await.expect("nested work must complete");
            threads.push(here);
            report.send(threads).unwrap();
        })
    }

    #[tokio::test]
    async fn a_ready_task_failure_preempts_the_ready_admission_queue() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<super::Work>();
        sender.send(Box::new(|_| Box::pin(async {}))).unwrap();
        let mut tasks = tokio::task::JoinSet::new();
        let (finished, ready) = tokio::sync::oneshot::channel();
        tasks.spawn(async move {
            let _ = finished.send(());
            panic!("ready supervisor failure sentinel");
        });
        ready.await.unwrap();
        tokio::task::yield_now().await;

        match super::next_thread_event(&mut receiver, &mut tasks).await {
            super::ThreadEvent::Completed(Err(error)) => assert!(error.is_panic()),
            _ => panic!("a ready admission queue must not starve a ready task failure"),
        }
    }

    #[tokio::test]
    async fn two_occupied_tasks_run_on_different_pool_threads() {
        let kernel = crate::Kernel::new_ephemeral_with_threads("pool-spread", 2).await;
        let (first, release_first) = occupy(&kernel).await;
        let (second, release_second) = occupy(&kernel).await;
        assert_ne!(first, second, "the pool must spread work across its threads");
        drop((release_first, release_second));
        kernel.shutdown_runtime_worker().await.unwrap();
    }

    #[tokio::test]
    async fn nested_work_runs_on_its_parent_thread_with_every_thread_occupied() {
        const THREADS: usize = 3;
        let kernel = Arc::new(crate::Kernel::new_ephemeral_with_threads("pool-nested", THREADS).await);
        let mut releases = Vec::new();
        for _ in 0..THREADS {
            let (_, release) = occupy(&kernel).await;
            releases.push(release);
        }
        let (report, chain) = oneshot::channel();
        nested_chain(kernel.clone(), 4, report).unwrap();
        let threads = tokio::time::timeout(WAIT, chain).await
            .expect("nested work must not wait for a thread the pool gave to someone else").unwrap();
        assert_eq!(threads.len(), 5, "a chain of depth four reports five threads");
        assert!(threads.windows(2).all(|pair| pair[0] == pair[1]), "nested work runs on its parent's thread: {threads:?}");
        for release in &releases {
            assert!(!release.is_closed(), "the occupying tasks must still be parked when nested work completes");
        }
        drop(releases);
        kernel.shutdown_runtime_worker().await.unwrap();
    }

    #[tokio::test]
    async fn reentry_from_a_spawned_task_outside_the_localset_is_accepted() {
        let kernel = Arc::new(crate::Kernel::new_ephemeral_with_threads("pool-reentry-spawn", 2).await);
        let (report, ran) = oneshot::channel();
        let owner = kernel.clone();
        kernel.spawn_runtime_task(move |_| async move {
            let parent = std::thread::current().id();
            // `tokio::spawn` runs on this thread's runtime but outside its
            // LocalSet, where the free `spawn_local` panics.
            tokio::spawn(async move {
                owner.spawn_runtime_task(move |_| async move {
                    report.send((parent, std::thread::current().id())).unwrap();
                }).expect("a spawned task on a pool thread must reach the funnel");
            }).await.unwrap();
        }).unwrap();
        let (parent, nested) = tokio::time::timeout(WAIT, ran).await
            .expect("work submitted outside the LocalSet must run").unwrap();
        assert_eq!(parent, nested, "reentry outside the LocalSet stays on its own thread");
        kernel.shutdown_runtime_worker().await.unwrap();
    }

    #[tokio::test]
    async fn a_panicked_task_makes_worker_shutdown_fail() {
        let kernel = crate::Kernel::new_ephemeral_with_threads("panic-worker", 3).await;
        let (cancelled, observed) = oneshot::channel();
        kernel.spawn_runtime_task(move |stop| async move {
            stop.cancelled().await;
            cancelled.send(std::thread::current().id()).unwrap();
        }).unwrap();
        let settled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (release, held) = oneshot::channel();
        let (entered, parked) = oneshot::channel();
        let flag = settled.clone();
        kernel.spawn_runtime_task(move |_| async move {
            entered.send(std::thread::current().id()).unwrap();
            let _ = held.await;
            flag.store(true, Ordering::SeqCst);
        }).unwrap();
        let parked = tokio::time::timeout(WAIT, parked).await.expect("the pool must run submitted work").unwrap();
        let (panicked, running) = oneshot::channel();
        kernel.spawn_runtime_task(move |_| async {
            panicked.send(std::thread::current().id()).unwrap();
            panic!("runtime task panic sentinel");
        }).unwrap();
        let panicked = tokio::time::timeout(WAIT, running).await.expect("the pool must run submitted work").unwrap();
        let watcher = tokio::time::timeout(WAIT, observed).await
            .expect("a failed task must stop admission on every thread").unwrap();
        assert_ne!(watcher, panicked, "the watching task must run on another thread");
        assert_ne!(parked, panicked, "the settling task must run on another thread");
        assert!(kernel.spawn_runtime_task(|_| async {}).is_err(), "a failed task must stop new admission");
        release.send(()).unwrap();
        let result = tokio::time::timeout(WAIT, kernel.shutdown_runtime_worker()).await.unwrap();
        assert!(settled.load(Ordering::SeqCst), "a task on another thread must still settle");
        let error = result.expect_err("worker must not report a clean shutdown after a task panic");
        assert!(error.contains("runtime task panic sentinel"), "{error}");
    }

    #[tokio::test]
    async fn shutdown_drains_work_queued_for_every_thread() {
        const THREADS: usize = 3;
        let kernel = crate::Kernel::new_ephemeral_with_threads("pool-drain", THREADS).await;
        let mut releases = Vec::new();
        for _ in 0..THREADS {
            let (_, release) = block_thread(&kernel).await;
            releases.push(release);
        }
        let drained = Arc::new(AtomicUsize::new(0));
        for _ in 0..THREADS {
            let counter = drained.clone();
            kernel.spawn_runtime_task(move |_| async move { counter.fetch_add(1, Ordering::SeqCst); }).unwrap();
        }
        kernel.stop_runtime_worker();
        for release in &releases { release.send(()).unwrap(); }
        tokio::time::timeout(WAIT, kernel.shutdown_runtime_worker()).await.unwrap().unwrap();
        assert_eq!(drained.load(Ordering::SeqCst), THREADS, "shutdown must drain the work queued for every thread");
    }

    #[tokio::test]
    async fn a_factory_panic_during_shutdown_still_drains_accepted_work() {
        let kernel = crate::Kernel::new_ephemeral("factory-panic-drain").await;
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        kernel.spawn_runtime_task(move |_| async move {
            entered.send(()).unwrap();
            blocked.recv().unwrap();
        }).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), ready).await.unwrap().unwrap();
        kernel.spawn_runtime_task(|_| -> std::future::Ready<()> { panic!("queued factory panic sentinel"); }).unwrap();
        let (finished, settled) = tokio::sync::oneshot::channel();
        let caller = std::thread::current().id();
        kernel.spawn_runtime_task(move |stop| {
            assert_ne!(std::thread::current().id(), caller, "construction stays on the kaish thread");
            let local = std::rc::Rc::new("settled");
            async move {
                assert!(stop.is_cancelled(), "queued work must receive the stopped token");
                tokio::task::yield_now().await;
                finished.send(*local).unwrap();
            }
        }).unwrap();
        kernel.stop_runtime_worker();
        release.send(()).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), kernel.shutdown_runtime_worker()).await.unwrap();
        assert_eq!(settled.await.expect("factory failure must not discard queued settlement"), "settled");
        assert!(result.unwrap_err().contains("queued factory panic sentinel"));
        assert!(kernel.spawn_runtime_task(|_| async {}).is_err());
    }

    #[tokio::test]
    async fn shutdown_waits_for_work_even_when_a_waiter_is_dropped() {
        let kernel = crate::Kernel::new_ephemeral("worker-drain").await;
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (task_entered, task_release, task_finished) = (entered.clone(), release.clone(), finished.clone());
        kernel.spawn_runtime_task(move |stop| async move {
            stop.cancelled().await;
            task_entered.notify_one();
            task_release.notified().await;
            task_finished.store(true, std::sync::atomic::Ordering::SeqCst);
        }).unwrap();
        let mut shutdown = Box::pin(kernel.shutdown_runtime_worker());
        tokio::select! {
            result = &mut shutdown => panic!("shutdown returned before settlement: {result:?}"),
            _ = entered.notified() => {}
        }
        drop(shutdown);
        release.notify_one();
        let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            tokio::join!(kernel.shutdown_runtime_worker(), kernel.shutdown_runtime_worker())
        }).await.unwrap();
        first.unwrap();
        second.unwrap();
        assert!(finished.load(std::sync::atomic::Ordering::SeqCst));
        kernel.shutdown_runtime_worker().await.unwrap();
        assert!(kernel.spawn_runtime_task(|_| async {}).is_err());
    }

    #[tokio::test]
    async fn stopping_before_first_use_prevents_late_worker_startup() {
        let kernel = crate::Kernel::new_ephemeral("stopped-worker").await;
        kernel.stop_runtime_worker();
        assert!(kernel.spawn_runtime_task(|_| async {}).is_err(), "shutdown must forbid new admission");
    }
}
