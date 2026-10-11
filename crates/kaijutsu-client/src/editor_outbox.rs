//! The editor outbox: keys and pastes for a kernel editor session reach the
//! kernel in the order they were pressed, and the key path never waits on
//! one (`docs/tui.md`, "Editor and diff").
//!
//! The kernel owns the editor's mode and buffer (`docs/vi.md`, "Decisions"),
//! so nothing is drawn ahead of it: the screen draws each state the kernel
//! answers with. One writer task keeps one call in flight. Keys that queue
//! up behind it go out together as one `editor_keys` batch, since vim
//! notation concatenates. A paste is its own `editor_insert` and keeps its
//! place in the order. Answers come back to the caller as [`Landed`]. If the
//! writer is dropped with calls queued or in flight, each lands as a failure.
//!
//! The writer is a future, so each client runs it on its own executor, as
//! `draft_outbox` describes: [`spawn_local`] for a tokio `LocalSet`,
//! [`channel`] for any other.

use std::future::Future;

use tokio::sync::mpsc;

use crate::actor::ActorHandle;
use crate::outbox_error::OutboxError;
use crate::rpc::EditorState;

/// Where the writer sends editor input: the kernel, or a test's stand-in.
pub trait EditorSink: 'static {
    /// Feed `keys`, in the kernel's vim notation, to `session`.
    fn keys(&self, session: u64, keys: &str) -> impl Future<Output = Result<EditorState, String>>;
    /// Insert `text` at `session`'s cursor, as a paste.
    fn insert(&self, session: u64, text: &str) -> impl Future<Output = Result<EditorState, String>>;
}

impl EditorSink for ActorHandle {
    fn keys(&self, session: u64, keys: &str) -> impl Future<Output = Result<EditorState, String>> {
        let actor = self.clone();
        let keys = keys.to_string();
        async move { actor.editor_keys(session, &keys).await.map_err(|e| e.to_string()) }
    }

    fn insert(&self, session: u64, text: &str) -> impl Future<Output = Result<EditorState, String>> {
        let actor = self.clone();
        let text = text.to_string();
        async move { actor.editor_insert(session, &text).await.map_err(|e| e.to_string()) }
    }
}

/// One call's answer.
#[derive(Debug)]
pub struct Landed {
    pub session: u64,
    /// What was sent, for the log when it fails: the key batch, or `paste`.
    pub sent: String,
    /// The session's state after the call, or why it failed.
    pub result: Result<EditorState, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Outgoing {
    Keys { session: u64, keys: String },
    Insert { session: u64, text: String },
}

/// The sending half, held by the client. Sending never waits.
#[derive(Clone, Default)]
pub struct EditorOutbox {
    /// `None` on a `default()` outbox: a client built for a test has no
    /// writer, and a key sent there fails loudly.
    tx: Option<mpsc::UnboundedSender<Outgoing>>,
}

/// The writer's state. Dropping it, whether the writer ended or its executor
/// dropped the future, lands every call still queued or in flight as a
/// failure, so an acknowledged enqueue is never silently lost.
struct Writer {
    rx: mpsc::UnboundedReceiver<Outgoing>,
    landed_tx: mpsc::UnboundedSender<Landed>,
    /// A call taken off the queue that did not join the batch before it.
    next: Option<Outgoing>,
    /// The call awaiting the kernel: its session and what was sent.
    in_flight: Option<(u64, String)>,
}

impl Writer {
    fn lost(&self, session: u64, sent: String) {
        let result = Err("the editor writer stopped before this call was answered".to_string());
        let _ = self.landed_tx.send(Landed { session, sent, result });
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if let Some((session, sent)) = self.in_flight.take() {
            self.lost(session, sent);
        }
        self.rx.close();
        let queued = self.next.take().into_iter().chain(std::iter::from_fn(|| self.rx.try_recv().ok()));
        let queued: Vec<Outgoing> = queued.collect();
        for outgoing in queued {
            match outgoing {
                Outgoing::Keys { session, keys } => self.lost(session, keys),
                Outgoing::Insert { session, .. } => self.lost(session, "paste".to_string()),
            }
        }
    }
}

/// Build the outbox, its writer, and the receiver its answers land on. The
/// caller runs the writer to completion on any executor; it ends when every
/// [`EditorOutbox`] clone is dropped, or when the [`Landed`] receiver is.
pub fn channel<S: EditorSink>(sink: S) -> (EditorOutbox, impl Future<Output = ()>, mpsc::UnboundedReceiver<Landed>) {
    let (tx, rx) = mpsc::unbounded_channel::<Outgoing>();
    let (landed_tx, landed_rx) = mpsc::unbounded_channel();
    let mut writer = Writer { rx, landed_tx, next: None, in_flight: None };
    let run = async move {
        loop {
            let outgoing = match writer.next.take() {
                Some(outgoing) => outgoing,
                None => match writer.rx.recv().await {
                    Some(outgoing) => outgoing,
                    None => return,
                },
            };
            let landed = match outgoing {
                Outgoing::Keys { session, mut keys } => {
                    // Everything already queued for the same session joins
                    // this batch; anything else waits its turn.
                    while let Ok(queued) = writer.rx.try_recv() {
                        match queued {
                            Outgoing::Keys { session: s, keys: more } if s == session => keys.push_str(&more),
                            other => {
                                writer.next = Some(other);
                                break;
                            }
                        }
                    }
                    writer.in_flight = Some((session, keys.clone()));
                    let result = sink.keys(session, &keys).await;
                    Landed { session, sent: keys, result }
                }
                Outgoing::Insert { session, text } => {
                    writer.in_flight = Some((session, "paste".to_string()));
                    let result = sink.insert(session, &text).await;
                    Landed { session, sent: "paste".to_string(), result }
                }
            };
            writer.in_flight = None;
            // The loop is gone; nothing is left to tell.
            if writer.landed_tx.send(landed).is_err() {
                return;
            }
        }
    };
    (EditorOutbox { tx: Some(tx) }, run, landed_rx)
}

/// Start the writer on the current tokio `LocalSet`. Returns the outbox and
/// the receiver its answers land on.
pub fn spawn_local(sink: impl EditorSink) -> (EditorOutbox, mpsc::UnboundedReceiver<Landed>) {
    let (outbox, writer, landed) = channel(sink);
    tokio::task::spawn_local(writer);
    (outbox, landed)
}

impl EditorOutbox {
    /// Queue keys, in the kernel's vim notation, behind everything already
    /// sent.
    pub fn keys(&self, session: u64, keys: &str) -> Result<(), OutboxError> {
        self.send(Outgoing::Keys { session, keys: keys.to_string() })
    }

    /// Queue a paste behind everything already sent.
    pub fn insert(&self, session: u64, text: &str) -> Result<(), OutboxError> {
        self.send(Outgoing::Insert { session, text: text.to_string() })
    }

    fn send(&self, outgoing: Outgoing) -> Result<(), OutboxError> {
        self.tx
            .as_ref()
            .ok_or(OutboxError::NotAttached("editor"))?
            .send(outgoing)
            .map_err(|_| OutboxError::Stopped("editor"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    /// A sink that records each call and answers only when the test lets it.
    #[derive(Clone)]
    struct GatedSink {
        calls: Rc<RefCell<Vec<Outgoing>>>,
        gate: Rc<tokio::sync::Semaphore>,
    }

    impl GatedSink {
        fn closed() -> Self {
            Self { calls: Rc::default(), gate: Rc::new(tokio::sync::Semaphore::new(0)) }
        }

        fn answer(&self, call: Outgoing) -> impl Future<Output = Result<EditorState, String>> + use<> {
            let this = self.clone();
            async move {
                this.gate.acquire().await.expect("gate open").forget();
                let session = match &call {
                    Outgoing::Keys { session, keys } if keys.contains("FAIL") => {
                        return Err(format!("refused session {session}"))
                    }
                    Outgoing::Keys { session, .. } | Outgoing::Insert { session, .. } => *session,
                };
                this.calls.borrow_mut().push(call);
                Ok(EditorState {
                    session, text: String::new(), cursor: 0, mode: None, dirty: false,
                    command_line: None, message: None, selection: None,
                })
            }
        }
    }

    impl EditorSink for GatedSink {
        fn keys(&self, session: u64, keys: &str) -> impl Future<Output = Result<EditorState, String>> {
            self.answer(Outgoing::Keys { session, keys: keys.to_string() })
        }

        fn insert(&self, session: u64, text: &str) -> impl Future<Output = Result<EditorState, String>> {
            self.answer(Outgoing::Insert { session, text: text.to_string() })
        }
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    fn local<F: Future>(f: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        tokio::task::LocalSet::new().block_on(&rt, f)
    }

    /// The next answer, or a failure rather than a hang when it never comes.
    async fn next(landed: &mut mpsc::UnboundedReceiver<Landed>) -> Landed {
        tokio::time::timeout(Duration::from_secs(2), landed.recv())
            .await
            .expect("an answer within 2 s")
            .expect("the writer is running")
    }

    fn keys(session: u64, keys: &str) -> Outgoing {
        Outgoing::Keys { session, keys: keys.into() }
    }

    /// Sending never waits on the kernel, and keys that queue behind a call
    /// in flight go out as one batch, in order.
    #[test]
    fn keys_queued_behind_a_call_go_out_as_one_batch() {
        local(async {
            let sink = GatedSink::closed();
            let (outbox, mut landed) = spawn_local(sink.clone());
            outbox.keys(7, "o").unwrap();
            settle().await;
            for k in ["a", "b", "<Esc>"] {
                outbox.keys(7, k).unwrap();
            }
            settle().await;
            assert!(landed.try_recv().is_err(), "nothing answered yet");

            sink.gate.add_permits(2);
            assert_eq!(next(&mut landed).await.sent, "o");
            assert_eq!(next(&mut landed).await.sent, "ab<Esc>");
            assert_eq!(*sink.calls.borrow(), [keys(7, "o"), keys(7, "ab<Esc>")]);
        });
    }

    /// A paste keeps its place: keys before it go first, keys after it wait
    /// for it, and the batch never reaches across it.
    #[test]
    fn a_paste_keeps_its_place_between_keys() {
        local(async {
            let sink = GatedSink::closed();
            let (outbox, mut landed) = spawn_local(sink.clone());
            outbox.keys(7, "i").unwrap();
            settle().await;
            outbox.keys(7, "x").unwrap();
            outbox.insert(7, "PASTE").unwrap();
            outbox.keys(7, "y").unwrap();
            outbox.keys(7, "z").unwrap();
            sink.gate.add_permits(4);
            for _ in 0..4 {
                next(&mut landed).await;
            }
            assert_eq!(
                *sink.calls.borrow(),
                [keys(7, "i"), keys(7, "x"), Outgoing::Insert { session: 7, text: "PASTE".into() }, keys(7, "yz")]
            );
        });
    }

    /// Keys for two sessions never share a batch.
    #[test]
    fn a_batch_holds_one_session() {
        local(async {
            let sink = GatedSink::closed();
            let (outbox, mut landed) = spawn_local(sink.clone());
            outbox.keys(1, "a").unwrap();
            settle().await;
            outbox.keys(1, "b").unwrap();
            outbox.keys(2, "c").unwrap();
            outbox.keys(2, "d").unwrap();
            sink.gate.add_permits(3);
            for _ in 0..3 {
                next(&mut landed).await;
            }
            assert_eq!(*sink.calls.borrow(), [keys(1, "a"), keys(1, "b"), keys(2, "cd")]);
        });
    }

    /// A refused call lands as a failure naming what was sent, and the
    /// writer keeps going.
    #[test]
    fn a_refused_call_lands_as_a_failure() {
        local(async {
            let sink = GatedSink::closed();
            sink.gate.add_permits(2);
            let (outbox, mut landed) = spawn_local(sink.clone());
            outbox.keys(3, "FAIL").unwrap();
            let failed = next(&mut landed).await;
            assert_eq!(failed.sent, "FAIL");
            assert_eq!(failed.result.unwrap_err(), "refused session 3");
            outbox.keys(3, "j").unwrap();
            assert!(next(&mut landed).await.result.is_ok());
        });
    }

    #[test]
    fn an_outbox_with_no_writer_refuses_loudly() {
        let error = EditorOutbox::default().keys(1, "j").unwrap_err();
        assert!(error.to_string().contains("no editor writer"), "{error}");
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

    /// A writer dropped with a call in flight and keys queued lands each as
    /// a failure naming what was sent.
    #[test]
    fn a_dropped_writer_lands_its_queued_calls_as_failures() {
        local(async {
            let sink = GatedSink::closed();
            let (outbox, writer, mut landed) = channel(sink.clone());
            let task = tokio::task::spawn_local(writer);
            outbox.keys(5, "i").unwrap();
            settle().await; // "i" is in flight, waiting on the gate
            outbox.keys(5, "a").unwrap();
            outbox.insert(5, "PASTE").unwrap();
            task.abort();
            let _ = task.await;
            let mut sent = Vec::new();
            for _ in 0..3 {
                let got = next(&mut landed).await;
                assert!(got.result.unwrap_err().contains("writer stopped"));
                sent.push(got.sent);
            }
            assert_eq!(sent, ["i", "a", "paste"]);
        });
    }

    /// A call of another session, stashed behind the batch in flight, lands
    /// as a failure when the writer is dropped.
    #[test]
    fn a_stashed_call_of_another_session_lands_at_drop() {
        local(async {
            let sink = GatedSink::closed();
            let (outbox, writer, mut landed) = channel(sink.clone());
            let task = tokio::task::spawn_local(writer);
            outbox.keys(1, "a").unwrap();
            settle().await; // session 1's "a" is in flight
            outbox.keys(1, "b").unwrap();
            outbox.keys(2, "c").unwrap();
            // The writer only moves "c" into its stash once "a" has landed
            // and it batches "b": release exactly that call, then stop it.
            sink.gate.add_permits(1);
            assert_eq!(next(&mut landed).await.sent, "a");
            settle().await; // "b" is in flight; "c" was stashed while batching
            task.abort();
            let _ = task.await;
            let mut sent = Vec::new();
            for _ in 0..2 {
                let got = next(&mut landed).await;
                assert!(got.result.unwrap_err().contains("writer stopped"));
                sent.push((got.session, got.sent));
            }
            assert_eq!(sent, [(1, "b".to_string()), (2, "c".to_string())]);
        });
    }

    /// With the `Landed` receiver dropped, the writer ends at its next ack.
    #[test]
    fn dropping_the_landed_receiver_ends_the_writer() {
        local(async {
            let sink = GatedSink::closed();
            let (outbox, writer, landed) = channel(sink.clone());
            let task = tokio::task::spawn_local(writer);
            outbox.keys(1, "a").unwrap();
            settle().await;
            drop(landed);
            sink.gate.add_permits(1);
            tokio::time::timeout(Duration::from_secs(2), task).await.expect("the writer ended").unwrap();
            assert!(outbox.keys(1, "b").is_err());
        });
    }
}
