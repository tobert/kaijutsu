//! Kernel-owned local executor for accepted shell tools and model turns. Its runtime outlives
//! submitting transports and supports reentrant calls on the reserved kaish stack.

use std::future::Future;
use std::pin::Pin;
use futures::FutureExt;
use tokio_util::sync::CancellationToken;

type Work = Box<dyn FnOnce(CancellationToken) -> Pin<Box<dyn Future<Output = ()>>> + Send>;

pub(crate) struct CommandWorker {
    sender: tokio::sync::mpsc::UnboundedSender<Work>,
    shutdown: CancellationToken,
    thread_id: std::thread::ThreadId,
    joined: futures::future::Shared<futures::future::BoxFuture<'static, Result<(), String>>>,
}

impl CommandWorker {
    pub(crate) fn start() -> Result<Self, String> {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Work>();
        let shutdown = CancellationToken::new();
        let stopped = shutdown.clone();
        let (started, ready) = std::sync::mpsc::sync_channel(1);
        let thread = crate::spawn_kaish_thread("kernel-shell-tools", move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(error) => { let _ = started.send(Err(error.to_string())); return Err(error.to_string()); }
            };
            let _entered = runtime.enter();
            let local = tokio::task::LocalSet::new();
            if started.send(Ok(())).is_err() { return Err("shell worker startup receiver disappeared".into()); }
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
                                tracing::error!("shell command task failed: {error}");
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
                        tracing::error!("shell command task failed during shutdown: {error}");
                        failures.push(error.to_string());
                    }
                }
                if failures.is_empty() { Ok(()) } else { Err(format!("shell command worker failed: {}", failures.join("; "))) }
            })
        }).map_err(|e| e.to_string())?;
        ready.recv().map_err(|_| "shell worker stopped during startup".to_string())??;
        let thread_id = thread.thread().id();
        let joined = async move {
            tokio::task::spawn_blocking(move || thread.join().map_err(|_| "shell worker thread panicked".to_string())?)
                .await.map_err(|error| format!("shell worker join failed: {error}"))?
        }.boxed().shared();
        Ok(Self { sender, shutdown, thread_id, joined })
    }

    pub(crate) fn submit<F, W>(&self, work: W) -> Result<(), String>
    where F: Future<Output = ()> + 'static, W: FnOnce(CancellationToken) -> F + Send + 'static {
        if self.shutdown.is_cancelled() { return Err("shell worker is shut down".into()); }
        self.sender.send(Box::new(move |stop| Box::pin(work(stop))))
            .map_err(|_| "shell worker stopped before accepting work".into())
    }

    pub(crate) fn stop(&self) { self.shutdown.cancel(); }

    pub(crate) async fn join(&self) -> Result<(), String> {
        if std::thread::current().id() == self.thread_id {
            return Err("shell worker cannot wait for its own shutdown".into());
        }
        self.joined.clone().await
    }
}

impl Drop for CommandWorker {
    fn drop(&mut self) { self.stop(); }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn a_panicked_command_makes_worker_shutdown_fail() {
        let kernel = crate::Kernel::new_ephemeral("panic-worker").await;
        kernel.spawn_command(|_| async { panic!("worker command panic sentinel"); }).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if kernel.spawn_command(|_| async {}).is_err() { break; }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.expect("task failure must stop new admission without waiting for host shutdown");
        let result = kernel.shutdown_command_worker().await;
        assert!(result.is_err(), "worker must not report a clean shutdown after a task panic");
    }

    #[tokio::test]
    async fn shutdown_waits_for_work_even_when_a_waiter_is_dropped() {
        let kernel = crate::Kernel::new_ephemeral("worker-drain").await;
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (task_entered, task_release, task_finished) = (entered.clone(), release.clone(), finished.clone());
        kernel.spawn_command(move |stop| async move {
            stop.cancelled().await;
            task_entered.notify_one();
            task_release.notified().await;
            task_finished.store(true, std::sync::atomic::Ordering::SeqCst);
        }).unwrap();
        let mut shutdown = Box::pin(kernel.shutdown_command_worker());
        tokio::select! {
            result = &mut shutdown => panic!("shutdown returned before settlement: {result:?}"),
            _ = entered.notified() => {}
        }
        drop(shutdown);
        release.notify_one();
        let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            tokio::join!(kernel.shutdown_command_worker(), kernel.shutdown_command_worker())
        }).await.unwrap();
        first.unwrap();
        second.unwrap();
        assert!(finished.load(std::sync::atomic::Ordering::SeqCst));
        kernel.shutdown_command_worker().await.unwrap();
        assert!(kernel.spawn_command(|_| async {}).is_err());
    }

    #[tokio::test]
    async fn stopping_before_first_use_prevents_late_worker_startup() {
        let kernel = crate::Kernel::new_ephemeral("stopped-worker").await;
        kernel.stop_command_worker();
        assert!(kernel.spawn_command(|_| async {}).is_err(), "shutdown must forbid lazy startup");
    }
}
