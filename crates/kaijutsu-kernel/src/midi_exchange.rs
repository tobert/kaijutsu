//! The kernel half of the MIDI **exchange** — a bounded call at ONE sink
//! (`docs/midi-next.md` "SysEx: the exchange pattern", slice 1 step 5).
//!
//! Every MIDI path before this one was fire-and-forget fan-out: a `RenderCue`
//! goes to every attached client and the one holding the gear plays it. An
//! exchange is the opposite shape in both respects — it is a **call** (send
//! bytes, collect a matching reply, bounded by a timeout) and it is
//! **addressed** (to the single connection whose sink reported the device
//! present). Broadcasting a device dialogue would mean several sinks
//! answering a question only one of them can hear.
//!
//! ## Why a registry and not a capability
//!
//! The sink's callback capability is a Cap'n Proto client pinned to the
//! server's per-connection `LocalSet` — `!Send`, unreachable from the kernel.
//! So the server registers a plain channel here, keyed by the connection's
//! `SessionId` (the SAME key presence attribution uses —
//! [`crate::midi_presence::SinkAttribution::connection`] — which is what lets
//! `kj midi identify` go from "who reported this device" straight to "who can
//! I ask"), and owns a task that turns each request into the capnp call. The
//! kernel stays free of capnp types and of hardware, exactly as
//! `docs/midi.md`'s sink-owns-hardware law requires.
//!
//! ## Loud, always
//!
//! Every failure mode is an error the caller can name: no sink registered for
//! that connection, the sink's task gone, the sink refusing (unroutable
//! device, no reply), or our own deadline. **Never** an empty reply, and
//! never a hang: the kernel's deadline is deliberately larger than the sink's
//! own timeout ([`KERNEL_DEADLINE_SLACK`]) so a healthy sink always gets to
//! answer first and a wedged one still can't pin the `kj` verb.

use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kaijutsu_types::SessionId;
use tokio::sync::{mpsc, oneshot};

/// Default bound on one exchange, as `kj midi identify` sends it. Identity
/// replies are ~15 bytes and arrive in milliseconds on healthy gear; two
/// seconds is generous for a device on a busy DIN chain and short enough that
/// a silent device doesn't feel like a hang.
pub const DEFAULT_EXCHANGE_TIMEOUT_MS: u32 = 2000;

/// How much longer the kernel waits than the sink was told to. The sink owns
/// the real timeout (it is the one holding the wire); this slack only covers
/// the round-trip and the sink's own bookkeeping, so a healthy sink's honest
/// "no reply" always beats our deadline and produces the *specific* error
/// instead of the generic one.
pub const KERNEL_DEADLINE_SLACK: Duration = Duration::from_millis(1000);

/// One exchange, on its way to a sink. The `reply` channel is how the sink's
/// answer (or its refusal) comes back.
#[derive(Debug)]
pub struct ExchangeRequest {
    /// Device profile name (resolved through the sink's own routing table) or
    /// a backend port address. The kernel sends device names; it never learns
    /// ALSA client numbers.
    pub port_or_device: String,
    /// Complete MIDI bytes to send (one whole F0…F7 SysEx today).
    pub payload: Vec<u8>,
    /// Leading bytes a reply must match to count as the answer.
    pub reply_match: Vec<u8>,
    /// The sink's own bound on the wait.
    pub timeout_ms: u32,
    /// Where the sink puts the answer: the reply bytes, or a message
    /// describing why there aren't any.
    pub reply: oneshot::Sender<Result<Vec<u8>, String>>,
}

/// The sink end of the registry: an unbounded channel the server's
/// per-connection task drains. Unbounded because exchanges are rare,
/// human/model-paced calls — a bound here would only convert "the sink is
/// busy" into a *different* error than the timeout that already covers it.
pub type ExchangeSender = mpsc::UnboundedSender<ExchangeRequest>;
pub type ExchangeReceiver = mpsc::UnboundedReceiver<ExchangeRequest>;

/// Why an exchange didn't produce bytes. Every variant names a different
/// thing to go fix, which is the whole point of not collapsing them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExchangeError {
    /// No sink is registered for that connection. The device was reported by
    /// a connection that has since gone, or by a peer too old to serve
    /// exchanges at all.
    NoSink(SessionId),
    /// The sink's task dropped the request (its connection tore down
    /// mid-call).
    SinkGone(SessionId),
    /// The sink answered, and the answer was "no": unroutable device, no ALSA
    /// sequencer, no reply within its timeout.
    Refused(String),
    /// Our own deadline fired — the sink never answered at all, which its own
    /// timeout should have prevented. A wedged sink, not a silent device.
    DeadlineExceeded(Duration),
}

impl std::fmt::Display for ExchangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExchangeError::NoSink(id) => write!(
                f,
                "the sink that reported this device (connection {}) is not \
                 serving exchanges — it disconnected, or it is an older peer \
                 that can't answer one",
                id.short()
            ),
            ExchangeError::SinkGone(id) => write!(
                f,
                "the sink (connection {}) went away mid-exchange",
                id.short()
            ),
            ExchangeError::Refused(msg) => write!(f, "the sink refused: {msg}"),
            ExchangeError::DeadlineExceeded(d) => write!(
                f,
                "no answer from the sink within {d:?} (its own timeout should \
                 have fired first — the sink is wedged, not the device silent)"
            ),
        }
    }
}

/// A live registration: the channel to drain, plus the epoch that identifies
/// *this* registration among a connection's successive ones.
#[derive(Debug)]
pub struct ExchangeRegistration {
    /// Which registration this is. A re-subscribe on the same connection
    /// mints a fresh epoch, and the task it replaced can only ever
    /// [`MidiExchangeRegistry::unregister_epoch`] its own — otherwise the
    /// loser of that race would tear down the winner's sink on its way out.
    pub epoch: u64,
    pub receiver: ExchangeReceiver,
}

/// Connection → its exchange channel. One per kernel, held behind an `Arc` on
/// [`crate::Kernel`].
#[derive(Debug, Default)]
pub struct MidiExchangeRegistry {
    sinks: RwLock<HashMap<SessionId, (u64, ExchangeSender)>>,
    /// Monotonic registration counter — see [`ExchangeRegistration::epoch`].
    epochs: AtomicU64,
}

impl MidiExchangeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) the channel for a connection, returning the
    /// receiver the caller's task drains. Replacing is deliberate: a client
    /// that re-subscribes on the same connection gets one live channel, not
    /// two racing ones.
    pub fn register(&self, connection: SessionId) -> ExchangeRegistration {
        let (tx, rx) = mpsc::unbounded_channel();
        let epoch = self.epochs.fetch_add(1, Ordering::Relaxed) + 1;
        self.sinks
            .write()
            .expect("midi exchange registry poisoned (a writer panicked)")
            .insert(connection, (epoch, tx));
        ExchangeRegistration { epoch, receiver: rx }
    }

    /// Forget a connection's channel. Idempotent — both the bridge task's
    /// exit and the connection's `Drop` call it, and neither may assume it
    /// ran first.
    pub fn unregister(&self, connection: SessionId) -> bool {
        self.sinks
            .write()
            .expect("midi exchange registry poisoned (a writer panicked)")
            .remove(&connection)
            .is_some()
    }

    /// Forget a connection's channel **only if** it is still the registration
    /// identified by `epoch`. This is what a bridge task calls on its way
    /// out: a task whose registration was already replaced must leave the
    /// replacement alone.
    pub fn unregister_epoch(&self, connection: SessionId, epoch: u64) -> bool {
        let mut sinks = self
            .sinks
            .write()
            .expect("midi exchange registry poisoned (a writer panicked)");
        match sinks.get(&connection) {
            Some((current, _)) if *current == epoch => {
                sinks.remove(&connection);
                true
            }
            _ => false,
        }
    }

    /// Is this connection serving exchanges right now?
    pub fn has(&self, connection: SessionId) -> bool {
        self.sinks
            .read()
            .expect("midi exchange registry poisoned")
            .contains_key(&connection)
    }

    pub fn len(&self) -> usize {
        self.sinks
            .read()
            .expect("midi exchange registry poisoned")
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Ask ONE sink to run an exchange and wait for its answer, bounded by
    /// `timeout_ms + `[`KERNEL_DEADLINE_SLACK`].
    ///
    /// The sender is cloned out of the map before awaiting — the lock is
    /// never held across an `.await`, so a slow device can't block a
    /// concurrent registration or a connection teardown.
    pub async fn exchange(
        &self,
        connection: SessionId,
        port_or_device: impl Into<String>,
        payload: Vec<u8>,
        reply_match: Vec<u8>,
        timeout_ms: u32,
    ) -> Result<Vec<u8>, ExchangeError> {
        let sender = {
            let sinks = self.sinks.read().expect("midi exchange registry poisoned");
            sinks
                .get(&connection)
                .map(|(_, tx)| tx.clone())
                .ok_or(ExchangeError::NoSink(connection))?
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        sender
            .send(ExchangeRequest {
                port_or_device: port_or_device.into(),
                payload,
                reply_match,
                timeout_ms,
                reply: reply_tx,
            })
            .map_err(|_| ExchangeError::SinkGone(connection))?;

        let deadline = Duration::from_millis(u64::from(timeout_ms)) + KERNEL_DEADLINE_SLACK;
        match tokio::time::timeout(deadline, reply_rx).await {
            Err(_) => Err(ExchangeError::DeadlineExceeded(deadline)),
            Ok(Err(_)) => Err(ExchangeError::SinkGone(connection)),
            Ok(Ok(Err(msg))) => Err(ExchangeError::Refused(msg)),
            Ok(Ok(Ok(bytes))) => Ok(bytes),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Spawn a stand-in sink on `connection` that answers every request with
    /// `answer`, recording what it was asked.
    fn spawn_sink(
        registry: Arc<MidiExchangeRegistry>,
        connection: SessionId,
        answer: Result<Vec<u8>, String>,
    ) -> tokio::task::JoinHandle<Option<ExchangeRequest>> {
        let mut rx = registry.register(connection).receiver;
        tokio::spawn(async move {
            let req = rx.recv().await?;
            let seen = ExchangeRequest {
                port_or_device: req.port_or_device.clone(),
                payload: req.payload.clone(),
                reply_match: req.reply_match.clone(),
                timeout_ms: req.timeout_ms,
                reply: oneshot::channel().0,
            };
            let _ = req.reply.send(answer);
            Some(seen)
        })
    }

    #[tokio::test]
    async fn a_registered_sink_answers_with_its_bytes() {
        let registry = Arc::new(MidiExchangeRegistry::new());
        let conn = SessionId::new();
        let sink = spawn_sink(registry.clone(), conn, Ok(vec![0xF0, 0x7E, 0xF7]));

        let reply = registry
            .exchange(conn, "keystep-pro", vec![0xF0, 0x7E, 0x7F, 0xF7], vec![0xF0, 0x7E], 50)
            .await
            .expect("the sink answered");
        assert_eq!(reply, vec![0xF0, 0x7E, 0xF7]);

        let asked = sink.await.unwrap().expect("the sink saw a request");
        assert_eq!(asked.port_or_device, "keystep-pro", "the DEVICE name travels, not a port");
        assert_eq!(asked.reply_match, vec![0xF0, 0x7E]);
        assert_eq!(asked.timeout_ms, 50);
    }

    /// The headline refusal: nobody is registered for that connection. This
    /// is what an exchange addressed at a sink that has since disconnected
    /// must produce — an error naming the connection, never a wait.
    #[tokio::test]
    async fn an_unregistered_connection_is_a_loud_no_sink() {
        let registry = MidiExchangeRegistry::new();
        let conn = SessionId::new();
        let err = registry
            .exchange(conn, "keystep-pro", vec![0xF0], vec![], 10)
            .await
            .unwrap_err();
        assert_eq!(err, ExchangeError::NoSink(conn));
        assert!(
            err.to_string().contains("serving exchanges"),
            "the message must name what is missing: {err}"
        );
    }

    /// A sink's own "no" (unroutable device, no reply) reaches the caller
    /// verbatim — the sink knows why, and paraphrasing would lose it.
    #[tokio::test]
    async fn a_sink_refusal_travels_verbatim() {
        let registry = Arc::new(MidiExchangeRegistry::new());
        let conn = SessionId::new();
        let _sink = spawn_sink(
            registry.clone(),
            conn,
            Err("no port matched 'keystep-pro' on this sink".into()),
        );
        let err = registry
            .exchange(conn, "keystep-pro", vec![0xF0], vec![], 10)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ExchangeError::Refused("no port matched 'keystep-pro' on this sink".into())
        );
    }

    /// A sink that never answers hits OUR deadline — bounded, not a hang.
    /// (`start_paused` auto-advances the clock, so the test is instant.)
    #[tokio::test(start_paused = true)]
    async fn a_silent_sink_hits_the_kernel_deadline() {
        let registry = Arc::new(MidiExchangeRegistry::new());
        let conn = SessionId::new();
        // Register, but hold the receiver without ever replying.
        let _rx = registry.register(conn);
        let err = registry
            .exchange(conn, "keystep-pro", vec![0xF0], vec![], 2000)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ExchangeError::DeadlineExceeded(Duration::from_millis(2000) + KERNEL_DEADLINE_SLACK),
            "the kernel waits a slack longer than the sink was told to"
        );
    }

    /// A sink whose task vanished mid-call is a distinct, named failure.
    #[tokio::test]
    async fn a_dropped_sink_task_is_sink_gone_not_a_timeout() {
        let registry = Arc::new(MidiExchangeRegistry::new());
        let conn = SessionId::new();
        let mut rx = registry.register(conn).receiver;
        let handle = tokio::spawn(async move {
            let req = rx.recv().await.expect("request");
            drop(req); // the connection tore down: the reply sender dies with it
        });
        let err = registry
            .exchange(conn, "keystep-pro", vec![0xF0], vec![], 10)
            .await
            .unwrap_err();
        assert_eq!(err, ExchangeError::SinkGone(conn));
        handle.await.unwrap();
    }

    /// Unregistering is idempotent and only touches its own connection —
    /// both the bridge task's exit and the connection Drop call it.
    #[tokio::test]
    async fn unregister_is_idempotent_and_scoped_to_one_connection() {
        let registry = MidiExchangeRegistry::new();
        let a = SessionId::new();
        let b = SessionId::new();
        let _ra = registry.register(a);
        let _rb = registry.register(b);
        assert_eq!(registry.len(), 2);
        assert!(registry.unregister(a));
        assert!(!registry.unregister(a), "a second unregister is a no-op, not a panic");
        assert!(registry.has(b), "the other connection's sink is untouched");
        assert_eq!(registry.len(), 1);
    }

    /// Re-registering the same connection replaces the channel rather than
    /// stacking a second one: a re-subscribe must leave exactly one live sink.
    #[tokio::test]
    async fn re_registering_a_connection_replaces_its_channel() {
        let registry = Arc::new(MidiExchangeRegistry::new());
        let conn = SessionId::new();
        let first = registry.register(conn);
        let mut first_rx = first.receiver;
        let second = registry.register(conn);
        let mut second_rx = second.receiver;
        assert_eq!(registry.len(), 1);

        let reg = registry.clone();
        tokio::spawn(async move {
            let _ = reg.exchange(conn, "d", vec![0xF0], vec![], 10).await;
        });
        let got = second_rx.recv().await;
        assert!(got.is_some(), "the newest channel receives");
        assert!(
            first_rx.try_recv().is_err(),
            "the replaced channel receives nothing"
        );

        // The replaced registration must not be able to tear down its
        // successor on the way out — the exact race a bridge task's exit
        // would otherwise lose.
        assert!(!registry.unregister_epoch(conn, first.epoch));
        assert!(registry.has(conn), "the live sink survived its predecessor's exit");
        assert!(registry.unregister_epoch(conn, second.epoch));
        assert!(!registry.has(conn));
    }
}
