//! Kernel-owned local executor for accepted shell tools. Its runtime outlives
//! submitting transports and supports reentrant calls on the reserved kaish stack.

use std::future::Future;
use std::pin::Pin;
use tokio_util::sync::CancellationToken;

type Work = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()>>> + Send>;

pub(crate) struct CommandWorker {
    sender: tokio::sync::mpsc::UnboundedSender<Work>,
    shutdown: CancellationToken,
}

impl CommandWorker {
    pub(crate) fn start() -> Result<Self, String> {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Work>();
        let shutdown = CancellationToken::new();
        let stopped = shutdown.clone();
        let (started, ready) = std::sync::mpsc::sync_channel(1);
        crate::spawn_kaish_thread("kernel-shell-tools", move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(error) => { let _ = started.send(Err(error.to_string())); return; }
            };
            let _entered = runtime.enter();
            let local = tokio::task::LocalSet::new();
            if started.send(Ok(())).is_err() { return; }
            local.block_on(&runtime, async move {
                loop {
                    tokio::select! {
                        biased;
                        _ = stopped.cancelled() => break,
                        work = receiver.recv() => match work {
                            Some(work) => { tokio::task::spawn_local(work()); }
                            None => break,
                        }
                    }
                }
            });
        }).map_err(|e| e.to_string())?;
        ready.recv().map_err(|_| "shell worker stopped during startup".to_string())??;
        Ok(Self { sender, shutdown })
    }

    pub(crate) fn submit<F, W>(&self, work: W) -> Result<(), String>
    where F: Future<Output = ()> + 'static, W: FnOnce() -> F + Send + 'static {
        if self.shutdown.is_cancelled() { return Err("shell worker is shut down".into()); }
        self.sender.send(Box::new(move || Box::pin(work())))
            .map_err(|_| "shell worker stopped before accepting work".into())
    }

    pub(crate) fn stop(&self) { self.shutdown.cancel(); }
}

impl Drop for CommandWorker {
    fn drop(&mut self) { self.stop(); }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn stopping_before_first_use_prevents_late_worker_startup() {
        let kernel = crate::Kernel::new_ephemeral("stopped-worker").await;
        kernel.stop_command_worker();
        assert!(kernel.spawn_command(|| async {}).is_err(), "shutdown must forbid lazy startup");
    }
}
