//! Kernel-owned executor for commands, model turns, and approval delivery.
//! It outlives submitting transports and supports reentrant calls on the kaish stack.

use std::future::Future;
use std::pin::Pin;
use futures::FutureExt;
use tokio_util::sync::CancellationToken;

type Work = Box<dyn FnOnce(CancellationToken) -> Pin<Box<dyn Future<Output = ()>>> + Send>;

pub(crate) struct RuntimeWorker {
    sender: tokio::sync::mpsc::UnboundedSender<Work>,
    shutdown: CancellationToken,
    thread_id: std::thread::ThreadId,
    joined: futures::future::Shared<futures::future::BoxFuture<'static, Result<(), String>>>,
}

impl RuntimeWorker {
    pub(crate) fn start() -> Result<Self, String> {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Work>();
        let shutdown = CancellationToken::new();
        let stopped = shutdown.clone();
        let (started, ready) = std::sync::mpsc::sync_channel(1);
        let thread = crate::spawn_kaish_thread("kernel-runtime", move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(error) => { let _ = started.send(Err(error.to_string())); return Err(error.to_string()); }
            };
            let _entered = runtime.enter();
            let local = tokio::task::LocalSet::new();
            if started.send(Ok(())).is_err() { return Err("runtime worker startup receiver disappeared".into()); }
            local.block_on(&runtime, async move {
                let mut tasks = tokio::task::JoinSet::new();
                let mut failures = Vec::new();
                loop {
                    tokio::select! {
                        biased;
                        _ = stopped.cancelled() => break,
                        work = receiver.recv() => match work {
                            Some(work) => { tasks.spawn_local(work(stopped.child_token())); }
                            None => break,
                        },
                        Some(result) = tasks.join_next() => {
                            if let Err(error) = result {
                                tracing::error!("runtime task failed: {error}");
                                failures.push(error.to_string());
                                stopped.cancel();
                            }
                        }
                    }
                }
                // Cancel queued and running work, then keep the runtime alive
                // through command settlement and turn finalization.
                receiver.close();
                stopped.cancel();
                while let Some(work) = receiver.recv().await { tasks.spawn_local(work(stopped.child_token())); }
                while let Some(result) = tasks.join_next().await {
                    if let Err(error) = result {
                        tracing::error!("runtime task failed during shutdown: {error}");
                        failures.push(error.to_string());
                    }
                }
                if failures.is_empty() { Ok(()) } else { Err(format!("runtime worker failed: {}", failures.join("; "))) }
            })
        }).map_err(|e| e.to_string())?;
        ready.recv().map_err(|_| "runtime worker stopped during startup".to_string())??;
        let thread_id = thread.thread().id();
        let joined = async move {
            tokio::task::spawn_blocking(move || thread.join().map_err(|_| "runtime worker thread panicked".to_string())?)
                .await.map_err(|error| format!("runtime worker join failed: {error}"))?
        }.boxed().shared();
        Ok(Self { sender, shutdown, thread_id, joined })
    }

    pub(crate) fn submit<F, W>(&self, work: W) -> Result<(), String>
    where F: Future<Output = ()> + 'static, W: FnOnce(CancellationToken) -> F + Send + 'static {
        if self.shutdown.is_cancelled() { return Err("runtime worker is shut down".into()); }
        // Construct the future inside its task so a factory panic reaches the
        // JoinSet failure path and cannot unwind the supervisor or its siblings.
        self.sender.send(Box::new(move |stop| Box::pin(async move { work(stop).await })))
            .map_err(|_| "runtime worker stopped before accepting work".into())
    }

    pub(crate) fn stop(&self) { self.shutdown.cancel(); }

    pub(crate) async fn join(&self) -> Result<(), String> {
        if std::thread::current().id() == self.thread_id {
            return Err("runtime worker cannot wait for its own shutdown".into());
        }
        self.joined.clone().await
    }
}

impl Drop for RuntimeWorker {
    fn drop(&mut self) { self.stop(); }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn a_panicked_task_makes_worker_shutdown_fail() {
        let kernel = crate::Kernel::new_ephemeral("panic-worker").await;
        kernel.spawn_runtime_task(|_| async { panic!("runtime task panic sentinel"); }).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if kernel.spawn_runtime_task(|_| async {}).is_err() { break; }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.expect("task failure must stop new admission without waiting for host shutdown");
        let result = kernel.shutdown_runtime_worker().await;
        assert!(result.is_err(), "worker must not report a clean shutdown after a task panic");
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
        assert!(kernel.spawn_runtime_task(|_| async {}).is_err(), "shutdown must forbid lazy startup");
    }
}
