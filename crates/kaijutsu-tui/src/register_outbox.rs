//! The register outbox: the vi register writes a key made reach the kernel
//! in the order they were made, and the key path never waits on one
//! (`docs/tui.md`, "Compose").
//!
//! One writer task sends each batch and waits for its answer before sending
//! the next, because the kernel applies writes in the order it receives
//! them. A batch the kernel refuses, or one the writer never sent, comes
//! back as a [`Failed`] for the status line; the local registers keep the
//! text either way.

use std::future::Future;

use kaijutsu_client::ActorHandle;
use kaijutsu_types::editor::RegisterWrite;
use tokio::sync::mpsc;

/// Where the writer sends batches: the kernel, or a test's stand-in.
pub trait RegisterSink: 'static {
    /// Send one batch to the kernel's copy of this player's registers.
    fn put(&self, writes: Vec<RegisterWrite>) -> impl Future<Output = Result<(), String>>;
}

impl RegisterSink for ActorHandle {
    fn put(&self, writes: Vec<RegisterWrite>) -> impl Future<Output = Result<(), String>> {
        let actor = self.clone();
        async move { actor.registers_put(writes).await.map_err(|e| e.to_string()) }
    }
}

/// A batch that did not reach the kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failed {
    /// How many writes the batch held.
    pub writes: usize,
    pub error: String,
}

/// The sending half, held by the client. Sending never waits.
#[derive(Clone, Default)]
pub struct RegisterOutbox {
    /// `None` on a `default()` outbox: a client built for a test has no
    /// writer, and a send there fails loudly.
    tx: Option<mpsc::UnboundedSender<Vec<RegisterWrite>>>,
}

impl RegisterOutbox {
    /// Queue `writes` behind everything already sent.
    pub fn send(&self, writes: Vec<RegisterWrite>) -> Result<(), String> {
        self.tx
            .as_ref()
            .ok_or_else(|| "no register writer is attached".to_string())?
            .send(writes)
            .map_err(|_| "the register writer stopped".to_string())
    }
}

/// Start the writer on the current `LocalSet`. Failures come back on the
/// receiver.
pub fn spawn_local(sink: impl RegisterSink) -> (RegisterOutbox, mpsc::UnboundedReceiver<Failed>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<RegisterWrite>>();
    let (failed_tx, failed_rx) = mpsc::unbounded_channel();
    tokio::task::spawn_local(async move {
        while let Some(writes) = rx.recv().await {
            let count = writes.len();
            if let Err(error) = sink.put(writes).await {
                let _ = failed_tx.send(Failed { writes: count, error });
            }
        }
    });
    (RegisterOutbox { tx: Some(tx) }, failed_rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::editor::{RegisterShape, RegisterWriteKind};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn delete(text: &str) -> RegisterWrite {
        RegisterWrite { kind: RegisterWriteKind::Delete, text: text.into(), shape: RegisterShape::Linewise }
    }

    #[derive(Clone, Default)]
    struct Recorder {
        seen: Rc<RefCell<Vec<Vec<RegisterWrite>>>>,
        refuse: bool,
    }

    impl RegisterSink for Recorder {
        fn put(&self, writes: Vec<RegisterWrite>) -> impl Future<Output = Result<(), String>> {
            self.seen.borrow_mut().push(writes);
            let refuse = self.refuse;
            async move {
                tokio::task::yield_now().await;
                if refuse { Err("refused".to_string()) } else { Ok(()) }
            }
        }
    }

    fn on_local_set<T>(body: impl AsyncFnOnce() -> T) -> T {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        tokio::task::LocalSet::new().block_on(&rt, body())
    }

    /// Batches reach the sink in the order they were queued.
    #[test]
    fn batches_arrive_in_order() {
        let seen = on_local_set(async || {
            let sink = Recorder::default();
            let seen = sink.seen.clone();
            let (outbox, mut failed) = spawn_local(sink);
            outbox.send(vec![delete("one\n")]).expect("queued");
            outbox.send(vec![delete("two\n"), delete("three\n")]).expect("queued");
            drop(outbox);
            assert_eq!(failed.recv().await, None, "no failure, and the writer ended with its outbox");
            seen.take()
        });
        assert_eq!(seen, vec![vec![delete("one\n")], vec![delete("two\n"), delete("three\n")]]);
    }

    /// A refused batch comes back with its size and the reason.
    #[test]
    fn a_refused_batch_comes_back_failed() {
        let failed = on_local_set(async || {
            let (outbox, mut failed) = spawn_local(Recorder { refuse: true, ..Recorder::default() });
            outbox.send(vec![delete("one\n"), delete("two\n")]).expect("queued");
            failed.recv().await
        });
        assert_eq!(failed, Some(Failed { writes: 2, error: "refused".into() }));
    }

    /// A client with no writer says so rather than dropping the writes.
    #[test]
    fn an_unattached_outbox_refuses() {
        assert!(RegisterOutbox::default().send(vec![delete("x\n")]).is_err());
    }
}
