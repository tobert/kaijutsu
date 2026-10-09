//! The editor outbox: keys and pastes for a kernel editor session reach the
//! kernel in the order they were pressed, and the key path never waits on
//! one (`docs/tui.md`, "Editor and diff").
//!
//! The kernel owns the editor's mode and buffer (`docs/vi.md`, "Decisions"),
//! so nothing is drawn ahead of it: the screen draws each state the kernel
//! answers with. One writer task keeps one call in flight. Keys that queue
//! up behind it go out together as one `editor_keys` batch, since vim
//! notation concatenates. A paste is its own `editor_insert` and keeps its
//! place in the order. Answers come back to the event loop as [`Landed`].

use std::future::Future;

use anyhow::{Result, anyhow};
use kaijutsu_client::EditorState;
use tokio::sync::mpsc;

use crate::bridge::KernelBridge;

/// Where the writer sends editor input: the kernel, or a test's stand-in.
pub trait EditorSink: 'static {
    /// Feed `keys`, in the kernel's vim notation, to `session`.
    fn keys(&self, session: u64, keys: &str) -> impl Future<Output = Result<EditorState>>;
    /// Insert `text` at `session`'s cursor, as a paste.
    fn insert(&self, session: u64, text: &str) -> impl Future<Output = Result<EditorState>>;
}

impl EditorSink for KernelBridge {
    fn keys(&self, session: u64, keys: &str) -> impl Future<Output = Result<EditorState>> {
        let actor = self.actor().clone();
        let keys = keys.to_string();
        async move { Ok(actor.editor_keys(session, &keys).await?) }
    }

    fn insert(&self, session: u64, text: &str) -> impl Future<Output = Result<EditorState>> {
        let actor = self.actor().clone();
        let text = text.to_string();
        async move { Ok(actor.editor_insert(session, &text).await?) }
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

/// The sending half, held by the app. Sending never waits.
#[derive(Clone, Default)]
pub struct EditorOutbox {
    /// `None` until [`spawn`] attaches a writer: an `App` built for a test
    /// has none, and a key sent there fails loudly.
    tx: Option<mpsc::UnboundedSender<Outgoing>>,
}

/// Start the writer on the current `LocalSet`. Returns the outbox and the
/// receiver its answers land on.
pub fn spawn(sink: impl EditorSink) -> (EditorOutbox, mpsc::UnboundedReceiver<Landed>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Outgoing>();
    let (landed_tx, landed_rx) = mpsc::unbounded_channel();
    tokio::task::spawn_local(async move {
        let mut next: Option<Outgoing> = None;
        loop {
            let outgoing = match next.take() {
                Some(outgoing) => outgoing,
                None => match rx.recv().await {
                    Some(outgoing) => outgoing,
                    None => return,
                },
            };
            let landed = match outgoing {
                Outgoing::Keys { session, mut keys } => {
                    // Everything already queued for the same session joins
                    // this batch; anything else waits its turn.
                    while let Ok(queued) = rx.try_recv() {
                        match queued {
                            Outgoing::Keys { session: s, keys: more } if s == session => keys.push_str(&more),
                            other => {
                                next = Some(other);
                                break;
                            }
                        }
                    }
                    let result = sink.keys(session, &keys).await.map_err(|e| format!("{e:#}"));
                    Landed { session, sent: keys, result }
                }
                Outgoing::Insert { session, text } => {
                    let result = sink.insert(session, &text).await.map_err(|e| format!("{e:#}"));
                    Landed { session, sent: "paste".to_string(), result }
                }
            };
            // The loop is gone; nothing is left to tell.
            if landed_tx.send(landed).is_err() {
                return;
            }
        }
    });
    (EditorOutbox { tx: Some(tx) }, landed_rx)
}

impl EditorOutbox {
    /// Queue keys, in the kernel's vim notation, behind everything already
    /// sent.
    pub fn keys(&self, session: u64, keys: &str) -> Result<()> {
        self.send(Outgoing::Keys { session, keys: keys.to_string() })
    }

    /// Queue a paste behind everything already sent.
    pub fn insert(&self, session: u64, text: &str) -> Result<()> {
        self.send(Outgoing::Insert { session, text: text.to_string() })
    }

    fn send(&self, outgoing: Outgoing) -> Result<()> {
        self.tx
            .as_ref()
            .ok_or_else(|| anyhow!("no editor writer is attached"))?
            .send(outgoing)
            .map_err(|_| anyhow!("the editor writer has stopped"))
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

        fn answer(&self, call: Outgoing) -> impl Future<Output = Result<EditorState>> + use<> {
            let this = self.clone();
            async move {
                this.gate.acquire().await.expect("gate open").forget();
                let session = match &call {
                    Outgoing::Keys { session, keys } if keys.contains("FAIL") => {
                        anyhow::bail!("refused session {session}")
                    }
                    Outgoing::Keys { session, .. } | Outgoing::Insert { session, .. } => *session,
                };
                this.calls.borrow_mut().push(call);
                Ok(EditorState {
                    session, text: String::new(), cursor: 0, mode: None, dirty: false,
                    command_line: None, message: None,
                })
            }
        }
    }

    impl EditorSink for GatedSink {
        fn keys(&self, session: u64, keys: &str) -> impl Future<Output = Result<EditorState>> {
            self.answer(Outgoing::Keys { session, keys: keys.to_string() })
        }

        fn insert(&self, session: u64, text: &str) -> impl Future<Output = Result<EditorState>> {
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
            let (outbox, mut landed) = spawn(sink.clone());
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
            let (outbox, mut landed) = spawn(sink.clone());
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
            let (outbox, mut landed) = spawn(sink.clone());
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
            let (outbox, mut landed) = spawn(sink.clone());
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
}
