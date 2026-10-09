//! The draft outbox: compose edits reach the kernel in the order they were
//! typed, and the key path never waits on one (`docs/tui.md`, "Compose").
//!
//! One writer task sends each edit and waits for its ack before sending the
//! next, because every `EditOp` is addressed against the text the edits
//! before it left. Acks come back to the event loop as [`Landed`], where
//! `Compose::landed` folds them in. A submit, or a switch that reads the
//! kernel's draft, first awaits [`DraftOutbox::flush`], so the kernel holds
//! every keystroke the player saw.

use std::future::Future;

use anyhow::{Context, Result, anyhow};
use kaijutsu_editor::EditOp;
use kaijutsu_types::ContextId;
use tokio::sync::{mpsc, oneshot};

use crate::bridge::KernelBridge;

/// Where the writer sends edits: the kernel, or a test's stand-in.
pub trait DraftSink: 'static {
    /// Apply one edit to `context_id`'s draft and return the context
    /// version that acknowledged it.
    fn edit(&self, context_id: ContextId, op: &EditOp) -> impl Future<Output = Result<u64>>;
}

impl DraftSink for KernelBridge {
    fn edit(&self, context_id: ContextId, op: &EditOp) -> impl Future<Output = Result<u64>> {
        self.edit_input(context_id, op.offset as u64, &op.insert, op.delete as u64)
    }
}

/// One edit back from the kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landed {
    /// The draft generation the edit was sent for (`Compose::sent`).
    pub generation: u64,
    /// The acknowledging context version, or why the edit failed.
    pub result: Result<u64, String>,
}

enum Outgoing {
    Edit { context_id: ContextId, generation: u64, op: EditOp },
    Flush(oneshot::Sender<()>),
}

/// The sending half, held by the app. Sending never waits.
#[derive(Clone, Default)]
pub struct DraftOutbox {
    /// `None` until [`spawn`] attaches a writer: an `App` built for a test
    /// has none, and an edit sent there fails loudly.
    tx: Option<mpsc::UnboundedSender<Outgoing>>,
}

/// Start the writer on the current `LocalSet`. Returns the outbox and the
/// receiver its acks land on.
pub fn spawn(sink: impl DraftSink) -> (DraftOutbox, mpsc::UnboundedReceiver<Landed>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Outgoing>();
    let (landed_tx, landed_rx) = mpsc::unbounded_channel();
    tokio::task::spawn_local(async move {
        while let Some(outgoing) = rx.recv().await {
            match outgoing {
                Outgoing::Edit { context_id, generation, op } => {
                    let result = sink.edit(context_id, &op).await.map_err(|e| format!("{e:#}"));
                    // The loop is gone; nothing is left to tell.
                    if landed_tx.send(Landed { generation, result }).is_err() {
                        return;
                    }
                }
                Outgoing::Flush(done) => {
                    let _ = done.send(());
                }
            }
        }
    });
    (DraftOutbox { tx: Some(tx) }, landed_rx)
}

impl DraftOutbox {
    /// Queue one edit behind every edit already sent.
    pub fn edit(&self, context_id: ContextId, generation: u64, op: EditOp) -> Result<()> {
        self.sender()?
            .send(Outgoing::Edit { context_id, generation, op })
            .map_err(|_| anyhow!("the draft writer has stopped"))
    }

    /// Resolve once every edit queued before this call has reached the
    /// kernel and been answered.
    pub async fn flush(&self) -> Result<()> {
        let (done, wait) = oneshot::channel();
        self.sender()?
            .send(Outgoing::Flush(done))
            .map_err(|_| anyhow!("the draft writer has stopped"))?;
        wait.await.context("the draft writer stopped before the flush")
    }

    fn sender(&self) -> Result<&mpsc::UnboundedSender<Outgoing>> {
        self.tx.as_ref().ok_or_else(|| anyhow!("no draft writer is attached"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    /// A sink that records each edit and answers only when the test lets it.
    #[derive(Clone)]
    struct GatedSink {
        seen: Rc<RefCell<Vec<String>>>,
        gate: Rc<tokio::sync::Semaphore>,
        fail_on: Option<&'static str>,
    }

    impl GatedSink {
        fn closed() -> Self {
            Self { seen: Rc::default(), gate: Rc::new(tokio::sync::Semaphore::new(0)), fail_on: None }
        }
    }

    impl DraftSink for GatedSink {
        fn edit(&self, _context_id: ContextId, op: &EditOp) -> impl Future<Output = Result<u64>> {
            let this = self.clone();
            let insert = op.insert.clone();
            async move {
                this.gate.acquire().await.expect("gate open").forget();
                if this.fail_on == Some(insert.as_str()) {
                    anyhow::bail!("refused {insert}");
                }
                this.seen.borrow_mut().push(insert);
                Ok(this.seen.borrow().len() as u64)
            }
        }
    }

    /// The next ack, or a failure rather than a hang when it never comes.
    async fn next(landed: &mut mpsc::UnboundedReceiver<Landed>) -> Landed {
        tokio::time::timeout(Duration::from_secs(2), landed.recv())
            .await
            .expect("an ack within 2 s")
            .expect("the writer is running")
    }

    fn op(offset: usize, insert: &str) -> EditOp {
        EditOp { offset, insert: insert.into(), delete: 0 }
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    fn local<F: Future>(f: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        tokio::task::LocalSet::new().block_on(&rt, f)
    }

    /// Sending is synchronous and never waits on the kernel: three edits
    /// queue while the sink answers none of them.
    #[test]
    fn edits_queue_without_waiting_for_the_kernel() {
        local(async {
            let sink = GatedSink::closed();
            let (outbox, mut landed) = spawn(sink.clone());
            let ctx = ContextId::new();
            for (i, c) in ["a", "b", "c"].into_iter().enumerate() {
                outbox.edit(ctx, 1, op(i, c)).unwrap();
            }
            settle().await;
            assert!(landed.try_recv().is_err(), "nothing answered yet");
            assert!(sink.seen.borrow().is_empty());

            sink.gate.add_permits(3);
            for version in 1..=3 {
                let got = next(&mut landed).await;
                assert_eq!(got, Landed { generation: 1, result: Ok(version) });
            }
            assert_eq!(*sink.seen.borrow(), ["a", "b", "c"], "the kernel saw them in typed order");
        });
    }

    /// Each edit waits for the one before it: the sink never has two at
    /// once, so a later offset is never applied before the text it counts.
    #[test]
    fn one_edit_is_on_the_wire_at_a_time() {
        local(async {
            let sink = GatedSink::closed();
            let (outbox, mut landed) = spawn(sink.clone());
            let ctx = ContextId::new();
            outbox.edit(ctx, 1, op(0, "a")).unwrap();
            outbox.edit(ctx, 1, op(1, "b")).unwrap();
            settle().await;
            sink.gate.add_permits(1);
            next(&mut landed).await;
            settle().await;
            assert_eq!(*sink.seen.borrow(), ["a"], "b waits for its own permit");
            assert_eq!(sink.gate.available_permits(), 0);
            sink.gate.add_permits(1);
            next(&mut landed).await;
            assert_eq!(*sink.seen.borrow(), ["a", "b"]);
        });
    }

    /// A flush resolves only after every edit queued before it has been
    /// answered, which is what a submit waits on.
    #[test]
    fn a_flush_waits_for_every_earlier_edit() {
        local(async {
            let sink = GatedSink::closed();
            let (outbox, _landed) = spawn(sink.clone());
            let ctx = ContextId::new();
            outbox.edit(ctx, 1, op(0, "a")).unwrap();
            outbox.edit(ctx, 1, op(1, "b")).unwrap();
            let flushed = Rc::new(std::cell::Cell::new(false));
            let flag = flushed.clone();
            let flusher = outbox.clone();
            let waiter = tokio::task::spawn_local(async move {
                flusher.flush().await.unwrap();
                flag.set(true);
            });
            sink.gate.add_permits(1);
            settle().await;
            assert!(!flushed.get(), "b has not been answered");
            sink.gate.add_permits(1);
            waiter.await.unwrap();
            assert_eq!(*sink.seen.borrow(), ["a", "b"]);
        });
    }

    /// A refused edit lands as a failure with the kernel's reason, and the
    /// edits behind it still go.
    #[test]
    fn a_refused_edit_lands_as_a_failure() {
        local(async {
            let sink = GatedSink { fail_on: Some("b"), ..GatedSink::closed() };
            sink.gate.add_permits(3);
            let (outbox, mut landed) = spawn(sink.clone());
            let ctx = ContextId::new();
            for (i, c) in ["a", "b", "c"].into_iter().enumerate() {
                outbox.edit(ctx, 7, op(i, c)).unwrap();
            }
            assert_eq!(next(&mut landed).await.result, Ok(1));
            let failed = next(&mut landed).await;
            assert_eq!(failed.generation, 7);
            assert_eq!(failed.result, Err("refused b".to_string()));
            assert_eq!(next(&mut landed).await.result, Ok(2));
        });
    }

    #[test]
    fn an_outbox_with_no_writer_refuses_loudly() {
        let outbox = DraftOutbox::default();
        let error = outbox.edit(ContextId::new(), 0, op(0, "a")).unwrap_err();
        assert!(error.to_string().contains("no draft writer"), "{error}");
    }
}
