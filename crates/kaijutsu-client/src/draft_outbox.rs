//! The draft outbox: compose edits reach the kernel in the order they were
//! typed, and the key path never waits on one (`docs/tui.md`, "Compose").
//!
//! One writer sends each call and waits for its answer before sending the
//! next, because every edit is addressed against the text the edits before it
//! left. Edit acks come back to the caller as [`Landed`], where the compose
//! state folds them in. A submit, or a read of the kernel's draft, goes
//! through the same queue ([`DraftOutbox::submit`], [`DraftOutbox::read`]), so
//! the kernel holds every keystroke queued before it, and none queued after
//! it overtakes it. [`DraftOutbox::flush`] waits for the queue to drain.
//!
//! The writer is a future, so each client runs it on its own executor:
//! [`spawn_local`] puts it on a tokio `LocalSet`, and a client with another
//! executor (the Bevy app's task pools) spawns the writer from [`channel`]
//! itself. The writer is `Send` when its sink is, as [`ActorHandle`] is. It
//! uses only executor-agnostic channels. If the writer is dropped with edits
//! queued or in flight, each lands as a failure; none vanishes.

use std::future::Future;

use kaijutsu_types::{BlockId, ContextId, InputEdge};
use tokio::sync::{mpsc, oneshot};

use crate::actor::ActorHandle;
use crate::outbox_error::OutboxError;

/// One draft edit, char-indexed: at `offset`, remove `delete` chars and
/// insert `insert`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftEdit {
    pub offset: u64,
    pub insert: String,
    pub delete: u64,
}

/// Where the writer sends calls: the kernel, or a test's stand-in. Each
/// failure is the text the caller shows.
pub trait DraftSink: 'static {
    /// Apply one edit to `context_id`'s draft and return the context
    /// version that acknowledged it.
    fn edit(&self, context_id: ContextId, edit: &DraftEdit) -> impl Future<Output = Result<u64, String>>;
    /// Submit `context_id`'s draft as a turn, with the player's `edge` of
    /// context when known, and return the block it became.
    fn submit(&self, context_id: ContextId, edge: Option<InputEdge>) -> impl Future<Output = Result<BlockId, String>>;
    /// Read `context_id`'s draft text.
    fn read(&self, context_id: ContextId) -> impl Future<Output = Result<String, String>>;
}

impl DraftSink for ActorHandle {
    fn edit(&self, context_id: ContextId, edit: &DraftEdit) -> impl Future<Output = Result<u64, String>> {
        let actor = self.clone();
        let insert = edit.insert.clone();
        let (offset, delete) = (edit.offset, edit.delete);
        async move {
            actor
                .edit_input(context_id, offset, &insert, delete)
                .await
                .map_err(|e| format!("edit input doc: {e}"))
        }
    }

    fn submit(&self, context_id: ContextId, edge: Option<InputEdge>) -> impl Future<Output = Result<BlockId, String>> {
        let actor = self.clone();
        async move {
            actor
                .submit_input_with_edge(context_id, false, edge)
                .await
                .map(|result| result.block_id)
                .map_err(|e| format!("submit input: {e}"))
        }
    }

    fn read(&self, context_id: ContextId) -> impl Future<Output = Result<String, String>> {
        let actor = self.clone();
        async move {
            actor
                .get_input_state(context_id)
                .await
                .map(|state| state.content)
                .map_err(|e| format!("read input doc: {e}"))
        }
    }
}

/// One edit back from the kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landed {
    /// The context whose draft the edit was for.
    pub context_id: ContextId,
    /// The draft generation the edit was sent for (the caller's own
    /// counter; the writer only hands it back).
    pub generation: u64,
    /// The acknowledging context version, or why the edit failed.
    pub result: Result<u64, String>,
}

enum Outgoing {
    Edit { context_id: ContextId, generation: u64, edit: DraftEdit },
    Flush(oneshot::Sender<()>),
    Submit { context_id: ContextId, edge: Option<InputEdge>, done: oneshot::Sender<Result<BlockId, String>> },
    Read { context_id: ContextId, done: oneshot::Sender<Result<String, String>> },
}

/// The sending half, held by the client. Sending never waits.
#[derive(Clone, Default)]
pub struct DraftOutbox {
    /// `None` on a `default()` outbox: a client built for a test has no
    /// writer, and an edit sent there fails loudly.
    tx: Option<mpsc::UnboundedSender<Outgoing>>,
}

/// The writer's state. Dropping it, whether the writer ended or its executor
/// dropped the future, lands every edit still queued or in flight as a
/// failure, so an acknowledged enqueue is never silently lost.
struct Writer {
    rx: mpsc::UnboundedReceiver<Outgoing>,
    landed_tx: mpsc::UnboundedSender<Landed>,
    in_flight: Option<(ContextId, u64)>,
}

impl Writer {
    fn lost(&self, context_id: ContextId, generation: u64) {
        let result = Err("the draft writer stopped before this edit was acknowledged".to_string());
        let _ = self.landed_tx.send(Landed { context_id, generation, result });
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if let Some((context_id, generation)) = self.in_flight.take() {
            self.lost(context_id, generation);
        }
        self.rx.close();
        while let Ok(outgoing) = self.rx.try_recv() {
            if let Outgoing::Edit { context_id, generation, .. } = outgoing {
                self.lost(context_id, generation);
            }
            // A dropped `done` sender tells a waiting submit or read.
        }
    }
}

/// Build the outbox, its writer, and the receiver its acks land on. The
/// caller runs the writer to completion on any executor; it ends when every
/// [`DraftOutbox`] clone is dropped, or when the [`Landed`] receiver is.
pub fn channel<S: DraftSink>(sink: S) -> (DraftOutbox, impl Future<Output = ()>, mpsc::UnboundedReceiver<Landed>) {
    let (tx, rx) = mpsc::unbounded_channel::<Outgoing>();
    let (landed_tx, landed_rx) = mpsc::unbounded_channel();
    let mut writer = Writer { rx, landed_tx, in_flight: None };
    let run = async move {
        while let Some(outgoing) = writer.rx.recv().await {
            match outgoing {
                Outgoing::Edit { context_id, generation, edit } => {
                    writer.in_flight = Some((context_id, generation));
                    let result = sink.edit(context_id, &edit).await;
                    writer.in_flight = None;
                    // The loop is gone; nothing is left to tell.
                    if writer.landed_tx.send(Landed { context_id, generation, result }).is_err() {
                        return;
                    }
                }
                Outgoing::Flush(done) => {
                    let _ = done.send(());
                }
                Outgoing::Submit { context_id, edge, done } => {
                    let _ = done.send(sink.submit(context_id, edge).await);
                }
                Outgoing::Read { context_id, done } => {
                    let _ = done.send(sink.read(context_id).await);
                }
            }
        }
    };
    (DraftOutbox { tx: Some(tx) }, run, landed_rx)
}

/// Start the writer on the current tokio `LocalSet`. Returns the outbox and
/// the receiver its acks land on.
pub fn spawn_local(sink: impl DraftSink) -> (DraftOutbox, mpsc::UnboundedReceiver<Landed>) {
    let (outbox, writer, landed) = channel(sink);
    tokio::task::spawn_local(writer);
    (outbox, landed)
}

impl DraftOutbox {
    /// Queue one edit behind every call already sent.
    pub fn edit(&self, context_id: ContextId, generation: u64, edit: DraftEdit) -> Result<(), OutboxError> {
        self.sender()?
            .send(Outgoing::Edit { context_id, generation, edit })
            .map_err(|_| OutboxError::Stopped("draft"))
    }

    /// Resolve once every edit queued before this call has reached the
    /// kernel and been answered.
    pub async fn flush(&self) -> Result<(), OutboxError> {
        let (done, wait) = oneshot::channel();
        self.sender()?.send(Outgoing::Flush(done)).map_err(|_| OutboxError::Stopped("draft"))?;
        wait.await.map_err(|_| OutboxError::StoppedBeforeReply("draft"))
    }

    /// Submit `context_id`'s draft as a turn, behind every call already
    /// queued. An edit queued after this call goes out after the submit.
    /// `edge` is the newest block the player had shown.
    pub async fn submit(&self, context_id: ContextId, edge: Option<InputEdge>) -> Result<BlockId, OutboxError> {
        let (done, wait) = oneshot::channel();
        self.sender()?
            .send(Outgoing::Submit { context_id, edge, done })
            .map_err(|_| OutboxError::Stopped("draft"))?;
        wait.await.map_err(|_| OutboxError::StoppedBeforeReply("draft"))?.map_err(OutboxError::Refused)
    }

    /// Read `context_id`'s draft text from the kernel, behind every call
    /// already queued, so it holds every edit queued before this call.
    pub async fn read(&self, context_id: ContextId) -> Result<String, OutboxError> {
        let (done, wait) = oneshot::channel();
        self.sender()?
            .send(Outgoing::Read { context_id, done })
            .map_err(|_| OutboxError::Stopped("draft"))?;
        wait.await.map_err(|_| OutboxError::StoppedBeforeReply("draft"))?.map_err(OutboxError::Refused)
    }

    fn sender(&self) -> Result<&mpsc::UnboundedSender<Outgoing>, OutboxError> {
        self.tx.as_ref().ok_or(OutboxError::NotAttached("draft"))
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
        fn edit(&self, _context_id: ContextId, op: &DraftEdit) -> impl Future<Output = Result<u64, String>> {
            let this = self.clone();
            let insert = op.insert.clone();
            async move {
                this.gate.acquire().await.expect("gate open").forget();
                if this.fail_on == Some(insert.as_str()) {
                    return Err(format!("refused {insert}"));
                }
                this.seen.borrow_mut().push(insert);
                Ok(this.seen.borrow().len() as u64)
            }
        }

        async fn submit(&self, _: ContextId, _: Option<InputEdge>) -> Result<BlockId, String> {
            unreachable!("this sink is not asked to submit")
        }

        async fn read(&self, _: ContextId) -> Result<String, String> {
            unreachable!("this sink is not asked to read")
        }
    }

    /// The next ack, or a failure rather than a hang when it never comes.
    async fn next(landed: &mut mpsc::UnboundedReceiver<Landed>) -> Landed {
        tokio::time::timeout(Duration::from_secs(2), landed.recv())
            .await
            .expect("an ack within 2 s")
            .expect("the writer is running")
    }

    fn op(offset: u64, insert: &str) -> DraftEdit {
        DraftEdit { offset, insert: insert.into(), delete: 0 }
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
            let (outbox, mut landed) = spawn_local(sink.clone());
            let ctx = ContextId::new();
            for (i, c) in ["a", "b", "c"].into_iter().enumerate() {
                outbox.edit(ctx, 1, op(i as u64, c)).unwrap();
            }
            settle().await;
            assert!(landed.try_recv().is_err(), "nothing answered yet");
            assert!(sink.seen.borrow().is_empty());

            sink.gate.add_permits(3);
            for version in 1..=3 {
                let got = next(&mut landed).await;
                assert_eq!(got, Landed { context_id: ctx, generation: 1, result: Ok(version) });
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
            let (outbox, mut landed) = spawn_local(sink.clone());
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
            let (outbox, _landed) = spawn_local(sink.clone());
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
            let (outbox, mut landed) = spawn_local(sink.clone());
            let ctx = ContextId::new();
            for (i, c) in ["a", "b", "c"].into_iter().enumerate() {
                outbox.edit(ctx, 7, op(i as u64, c)).unwrap();
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
        assert_eq!(error, OutboxError::NotAttached("draft"));
        assert!(error.to_string().contains("no draft writer"), "{error}");
    }

    /// A sink that answers its first call late, as a slow link would. The
    /// order the kernel sees is the order the sink was called in, so a
    /// second edit that overtook the first would show up in `seen` first.
    #[derive(Clone)]
    struct SlowFirstSink {
        seen: Rc<RefCell<Vec<String>>>,
    }

    impl DraftSink for SlowFirstSink {
        fn edit(&self, _context_id: ContextId, op: &DraftEdit) -> impl Future<Output = Result<u64, String>> {
            let seen = self.seen.clone();
            let insert = op.insert.clone();
            async move {
                if insert == "a" {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                seen.borrow_mut().push(insert);
                Ok(seen.borrow().len() as u64)
            }
        }

        async fn submit(&self, _: ContextId, _: Option<InputEdge>) -> Result<BlockId, String> {
            unreachable!("this sink is not asked to submit")
        }

        async fn read(&self, _: ContextId) -> Result<String, String> {
            unreachable!("this sink is not asked to read")
        }
    }

    /// A submit follows `flush`: with the first edit slow, the flush still
    /// resolves only after both edits have reached the sink, in typed order,
    /// and every ack has landed before it resolves.
    #[test]
    fn a_flush_does_not_overtake_a_slow_first_edit() {
        local(async {
            let sink = SlowFirstSink { seen: Rc::default() };
            let (outbox, mut landed) = spawn_local(sink.clone());
            let ctx = ContextId::new();
            outbox.edit(ctx, 1, op(0, "a")).unwrap();
            outbox.edit(ctx, 1, op(1, "b")).unwrap();
            outbox.edit(ctx, 1, op(2, "c")).unwrap();
            outbox.flush().await.unwrap();
            assert_eq!(*sink.seen.borrow(), ["a", "b", "c"], "the flush returned before the edits did");
            for version in 1..=3 {
                let got = landed.try_recv().expect("acked before the flush resolved");
                assert_eq!(got.result, Ok(version));
            }
        });
    }

    /// The writer of an `ActorHandle` sink can be handed to a multi-threaded
    /// executor, which is how the Bevy app runs it.
    #[test]
    fn the_writer_for_an_actor_handle_is_send() {
        fn assert_send<T: Send>(_: &T) {}
        let (actor, _unanswered) = ActorHandle::never_answers_for_test();
        let (outbox, writer, _landed) = channel(actor);
        assert_send(&writer);
        assert_send(&outbox);
    }

    fn block(ctx: ContextId) -> BlockId {
        BlockId::new(ctx, kaijutsu_types::PrincipalId::system(), 1)
    }

    /// A sink that logs the start and end of each call, with a slow submit.
    #[derive(Clone)]
    struct LogSink {
        log: Rc<RefCell<Vec<String>>>,
        text: Rc<RefCell<String>>,
    }

    impl LogSink {
        fn new() -> Self {
            Self { log: Rc::default(), text: Rc::default() }
        }
    }

    impl DraftSink for LogSink {
        fn edit(&self, _context_id: ContextId, op: &DraftEdit) -> impl Future<Output = Result<u64, String>> {
            let this = self.clone();
            let insert = op.insert.clone();
            async move {
                this.log.borrow_mut().push(format!("edit:{insert}"));
                this.text.borrow_mut().push_str(&insert);
                Ok(this.log.borrow().len() as u64)
            }
        }

        fn submit(&self, context_id: ContextId, _edge: Option<InputEdge>) -> impl Future<Output = Result<BlockId, String>> {
            let this = self.clone();
            async move {
                this.log.borrow_mut().push("submit:start".into());
                tokio::time::sleep(Duration::from_millis(50)).await;
                this.log.borrow_mut().push("submit:end".into());
                Ok(block(context_id))
            }
        }

        fn read(&self, _context_id: ContextId) -> impl Future<Output = Result<String, String>> {
            let this = self.clone();
            async move {
                this.log.borrow_mut().push("read".into());
                Ok(this.text.borrow().clone())
            }
        }
    }

    /// An edit queued while a submit is in flight reaches the sink after the
    /// submit finishes, never during it: the submit snapshots the draft the
    /// edits before it built.
    #[test]
    fn an_edit_queued_during_a_submit_goes_after_it() {
        local(async {
            let sink = LogSink::new();
            let (outbox, _landed) = spawn_local(sink.clone());
            let ctx = ContextId::new();
            outbox.edit(ctx, 1, op(0, "a")).unwrap();
            let submitter = outbox.clone();
            let submit = tokio::task::spawn_local(async move { submitter.submit(ctx, None).await });
            settle().await;
            assert_eq!(*sink.log.borrow(), ["edit:a", "submit:start"], "the submit is in flight");
            outbox.edit(ctx, 2, op(1, "b")).unwrap();
            assert_eq!(submit.await.unwrap().unwrap(), block(ctx));
            outbox.flush().await.unwrap();
            assert_eq!(*sink.log.borrow(), ["edit:a", "submit:start", "submit:end", "edit:b"]);
        });
    }

    /// A read queued behind edits sees them, even when it is called before
    /// the edits have reached the kernel.
    #[test]
    fn a_read_holds_every_edit_queued_before_it() {
        local(async {
            let sink = LogSink::new();
            let (outbox, _landed) = spawn_local(sink.clone());
            let ctx = ContextId::new();
            outbox.edit(ctx, 1, op(0, "a")).unwrap();
            outbox.edit(ctx, 1, op(1, "b")).unwrap();
            assert_eq!(outbox.read(ctx).await.unwrap(), "ab");
        });
    }

    /// A kernel refusal of a submit comes back with its reason.
    #[test]
    fn a_refused_submit_carries_the_kernels_reason() {
        #[derive(Clone)]
        struct Refuses;
        impl DraftSink for Refuses {
            async fn edit(&self, _: ContextId, _: &DraftEdit) -> Result<u64, String> {
                Ok(1)
            }
            async fn submit(&self, _: ContextId, _: Option<InputEdge>) -> Result<BlockId, String> {
                Err("submit input: no performer".to_string())
            }
            async fn read(&self, _: ContextId) -> Result<String, String> {
                Err("read input doc: gone".to_string())
            }
        }
        local(async {
            let (outbox, _landed) = spawn_local(Refuses);
            let ctx = ContextId::new();
            let error = outbox.submit(ctx, None).await.unwrap_err();
            assert_eq!(error, OutboxError::Refused("submit input: no performer".into()));
            assert_eq!(error.to_string(), "submit input: no performer");
            assert_eq!(outbox.read(ctx).await.unwrap_err().to_string(), "read input doc: gone");
        });
    }

    /// A writer dropped with one edit in flight and two queued lands all
    /// three as failures, and a waiting submit learns the writer stopped.
    #[test]
    fn a_dropped_writer_lands_its_queued_edits_as_failures() {
        local(async {
            let sink = GatedSink::closed();
            let (outbox, writer, mut landed) = channel(sink.clone());
            let task = tokio::task::spawn_local(writer);
            let ctx = ContextId::new();
            for (i, generation) in [10u64, 11, 12].into_iter().enumerate() {
                outbox.edit(ctx, generation, op(i as u64, "x")).unwrap();
            }
            let submitter = outbox.clone();
            let submit = tokio::task::spawn_local(async move { submitter.submit(ctx, None).await });
            settle().await; // the first edit is in flight, waiting on the gate
            task.abort();
            let _ = task.await;
            for generation in [10u64, 11, 12] {
                let got = next(&mut landed).await;
                assert_eq!((got.context_id, got.generation), (ctx, generation));
                assert!(got.result.unwrap_err().contains("writer stopped"));
            }
            assert_eq!(submit.await.unwrap().unwrap_err(), OutboxError::StoppedBeforeReply("draft"));
            assert!(outbox.edit(ctx, 13, op(0, "y")).is_err(), "a send after the stop is refused");
        });
    }

    /// The writer needs no tokio runtime: a sink that never touches one runs
    /// to completion under a bare `futures` executor, so a tokio timer or
    /// spawn in the writer's poll path would panic here.
    #[test]
    fn the_writer_runs_without_a_tokio_runtime() {
        #[derive(Clone)]
        struct Plain(Rc<RefCell<Vec<String>>>);
        impl DraftSink for Plain {
            fn edit(&self, _: ContextId, op: &DraftEdit) -> impl Future<Output = Result<u64, String>> {
                self.0.borrow_mut().push(format!("edit:{}", op.insert));
                let n = self.0.borrow().len() as u64;
                async move { Ok(n) }
            }
            fn submit(&self, context_id: ContextId, _: Option<InputEdge>) -> impl Future<Output = Result<BlockId, String>> {
                self.0.borrow_mut().push("submit".into());
                async move { Ok(block(context_id)) }
            }
            fn read(&self, _: ContextId) -> impl Future<Output = Result<String, String>> {
                self.0.borrow_mut().push("read".into());
                async { Ok("draft".to_string()) }
            }
        }
        let log = Rc::new(RefCell::new(Vec::new()));
        let (outbox, writer, mut landed) = channel(Plain(log.clone()));
        let ctx = ContextId::new();
        let driver = async move {
            outbox.edit(ctx, 1, op(0, "a")).unwrap();
            outbox.flush().await.unwrap();
            assert_eq!(outbox.submit(ctx, None).await.unwrap(), block(ctx));
            assert_eq!(outbox.read(ctx).await.unwrap(), "draft");
        };
        futures::executor::block_on(futures::future::join(writer, driver));
        assert_eq!(*log.borrow(), ["edit:a", "submit", "read"]);
        let got = landed.try_recv().unwrap();
        assert_eq!((got.context_id, got.generation, got.result), (ctx, 1, Ok(1)));
    }

    /// A flush waiting when the writer is dropped learns it stopped.
    #[test]
    fn a_flush_waiting_at_drop_gets_stopped_before_reply() {
        local(async {
            let sink = GatedSink::closed();
            let (outbox, writer, _landed) = channel(sink.clone());
            let task = tokio::task::spawn_local(writer);
            let ctx = ContextId::new();
            outbox.edit(ctx, 1, op(0, "a")).unwrap();
            let flusher = outbox.clone();
            let flush = tokio::task::spawn_local(async move { flusher.flush().await });
            settle().await; // the edit is in flight, the flush queued behind it
            task.abort();
            let _ = task.await;
            assert_eq!(flush.await.unwrap().unwrap_err(), OutboxError::StoppedBeforeReply("draft"));
        });
    }

    /// With the `Landed` receiver dropped, the writer ends at its next edit
    /// ack, and a submit queued behind that edit learns it stopped.
    #[test]
    fn dropping_the_landed_receiver_ends_the_writer() {
        local(async {
            let sink = GatedSink::closed();
            let (outbox, writer, landed) = channel(sink.clone());
            let task = tokio::task::spawn_local(writer);
            let ctx = ContextId::new();
            outbox.edit(ctx, 1, op(0, "a")).unwrap();
            let submitter = outbox.clone();
            let submit = tokio::task::spawn_local(async move { submitter.submit(ctx, None).await });
            settle().await;
            drop(landed);
            sink.gate.add_permits(1);
            tokio::time::timeout(Duration::from_secs(2), task).await.expect("the writer ended").unwrap();
            assert_eq!(submit.await.unwrap().unwrap_err(), OutboxError::StoppedBeforeReply("draft"));
            assert!(outbox.edit(ctx, 2, op(1, "b")).is_err());
        });
    }
}
