//! The client half of the MIDI **exchange** — how a kernel question reaches
//! this process's MIDI hardware and comes back (`docs/midi-next.md` "SysEx:
//! the exchange pattern", slice 1 step 5).
//!
//! Every other server-push callback in [`crate::subscriptions`] is
//! fire-and-forget: it drops a [`ServerEvent`](crate::ServerEvent) into a
//! broadcast and returns. An exchange can't — the kernel is *awaiting a
//! promise*, so someone in this process has to actually run the dialogue and
//! answer.
//!
//! ## The slot, and why it isn't a constructor argument
//!
//! The callback capability is built during the connect handshake, long before
//! (and independently of) whatever owns the hardware. So the forwarder holds
//! an [`MidiExchangeSlot`] — a shared, late-bindable seat — and the sink
//! installs itself into it whenever it is ready ([`MidiExchangeSlot::install`]).
//! An empty slot is a *loud refusal*, not a silent success: a client with no
//! MIDI hardware (the MCP server, a headless tool) says so, and the kernel
//! reports that rather than reporting "no reply".
//!
//! The channel is deliberately the seam: the handler lives on the client's
//! `!Send` RPC LocalSet, and the real ALSA client lives on its own thread
//! (`alsa::Seq` is `!Send`, and a blocking hardware wait must never sit on the
//! RPC executor). A request crosses on an unbounded channel, the answer comes
//! back on a `oneshot`.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

/// One exchange for the local sink to run: send `payload` at
/// `port_or_device`, collect the first reply starting with `reply_match`,
/// give up after `timeout`.
#[derive(Debug)]
pub struct MidiExchangeRequest {
    /// A device profile name (resolved through this sink's own routing table,
    /// exactly as a device-addressed control cue is) or a backend port
    /// address (`"24:0"`).
    pub port_or_device: String,
    /// Complete MIDI bytes to send.
    pub payload: Vec<u8>,
    /// Leading bytes the reply must match. Empty = the first complete message
    /// that arrives.
    pub reply_match: Vec<u8>,
    /// The sink's own bound on the wait. The kernel allows itself a little
    /// longer, so THIS is the timeout that should normally fire — which is
    /// what makes "the device didn't answer" distinguishable from "the sink
    /// is wedged".
    pub timeout: Duration,
    /// Where the answer goes: the reply bytes, or a message saying why there
    /// aren't any. Dropping it (a panicking worker) is itself an answer the
    /// caller reports loudly.
    pub reply: oneshot::Sender<Result<Vec<u8>, String>>,
}

/// The sink end: an unbounded channel a hardware worker drains. Unbounded
/// because exchanges are rare, human/model-paced calls — a bound here would
/// only convert "busy" into a second, less informative error than the timeout
/// that already covers it.
pub type MidiExchangeSender = mpsc::UnboundedSender<MidiExchangeRequest>;
pub type MidiExchangeReceiver = mpsc::UnboundedReceiver<MidiExchangeRequest>;

/// The late-bindable seat a MIDI-capable client installs itself into.
///
/// Created by `spawn_actor`, reachable via
/// [`ActorHandle::midi_exchange`](crate::ActorHandle::midi_exchange), and read
/// by the block-events forwarder on every incoming exchange — so installing
/// (or replacing) a sink takes effect immediately, with no re-subscribe and
/// no reconnect.
#[derive(Debug, Default)]
pub struct MidiExchangeSlot {
    sink: RwLock<Option<MidiExchangeSender>>,
}

impl MidiExchangeSlot {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Seat a sink. Replaces whatever was there (an app that restarts its
    /// MIDI worker must end up with exactly one live sink, not two racing).
    pub fn install(&self, sender: MidiExchangeSender) {
        *self
            .sink
            .write()
            .expect("midi exchange slot poisoned (a writer panicked)") = Some(sender);
    }

    /// Empty the seat — this client can no longer run exchanges.
    pub fn clear(&self) {
        *self
            .sink
            .write()
            .expect("midi exchange slot poisoned (a writer panicked)") = None;
    }

    /// The seated sink, if any.
    pub fn get(&self) -> Option<MidiExchangeSender> {
        self.sink
            .read()
            .expect("midi exchange slot poisoned")
            .clone()
    }

    pub fn is_installed(&self) -> bool {
        self.get().is_some()
    }

    /// Run one exchange through the seated sink.
    ///
    /// Four refusals, each naming a different thing to go fix, and none of
    /// them a hang:
    ///
    /// 1. no sink installed — this client has no MIDI hardware at all,
    /// 2. the worker's channel is closed (its thread died),
    /// 3. the worker dropped the reply without answering (it panicked),
    /// 4. the worker outran its own timeout — bounded here by
    ///    `timeout + `[`WORKER_SLACK`] so a wedged worker surfaces as a wedged
    ///    worker rather than as the kernel's outer deadline.
    pub async fn run(
        &self,
        port_or_device: String,
        payload: Vec<u8>,
        reply_match: Vec<u8>,
        timeout: Duration,
    ) -> Result<Vec<u8>, String> {
        let sink = self.get().ok_or_else(|| {
            "this client has no MIDI exchange sink installed (it holds no MIDI hardware)"
                .to_string()
        })?;
        let (reply, rx) = oneshot::channel();
        sink.send(MidiExchangeRequest {
            port_or_device,
            payload,
            reply_match,
            timeout,
            reply,
        })
        .map_err(|_| "the MIDI exchange worker is gone (its thread exited)".to_string())?;

        // Bounded, not cancellable: nothing interrupts an exchange once the
        // worker has it. That is the right shape for a hardware dialogue —
        // the bytes are already on the wire, and the worker's own timeout is
        // what the device is actually racing.
        match tokio::time::timeout(timeout + WORKER_SLACK, rx).await {
            Err(_) => Err(format!(
                "the MIDI exchange worker did not answer within {:?} — its own \
                 {timeout:?} timeout should have fired first",
                timeout + WORKER_SLACK
            )),
            Ok(Err(_)) => {
                Err("the MIDI exchange worker dropped the request without answering".to_string())
            }
            Ok(Ok(answer)) => answer,
        }
    }
}

/// How much longer the forwarder waits than the worker was told to. Covers
/// the channel hop and the worker's own bookkeeping only — small enough that
/// it still fires well inside the kernel's outer deadline, so a wedged worker
/// is reported as such instead of timing the kernel out.
pub const WORKER_SLACK: Duration = Duration::from_millis(500);

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_empty_slot_refuses_loudly_rather_than_waiting() {
        let slot = MidiExchangeSlot::new();
        assert!(!slot.is_installed());
        let err = slot
            .run("keystep-pro".into(), vec![0xF0], vec![], Duration::from_millis(10))
            .await
            .unwrap_err();
        assert!(err.contains("no MIDI exchange sink"), "err: {err}");
    }

    #[tokio::test]
    async fn an_installed_sink_receives_the_request_and_its_answer_comes_back() {
        let slot = MidiExchangeSlot::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        slot.install(tx);
        tokio::spawn(async move {
            let req = rx.recv().await.expect("request");
            assert_eq!(req.port_or_device, "keystep-pro");
            assert_eq!(req.reply_match, vec![0xF0, 0x7E]);
            let _ = req.reply.send(Ok(vec![0xF0, 0x7E, 0xF7]));
        });
        let reply = slot
            .run(
                "keystep-pro".into(),
                vec![0xF0],
                vec![0xF0, 0x7E],
                Duration::from_millis(500),
            )
            .await
            .expect("answered");
        assert_eq!(reply, vec![0xF0, 0x7E, 0xF7]);
    }

    /// A dead worker thread is a named error, not a timeout — the difference
    /// between "restart the app" and "the device is silent".
    #[tokio::test]
    async fn a_closed_worker_channel_is_named_not_waited_out() {
        let slot = MidiExchangeSlot::new();
        let (tx, rx) = mpsc::unbounded_channel();
        slot.install(tx);
        drop(rx);
        let err = slot
            .run("d".into(), vec![0xF0], vec![], Duration::from_secs(30))
            .await
            .unwrap_err();
        assert!(err.contains("worker is gone"), "err: {err}");
    }

    #[tokio::test]
    async fn a_worker_that_drops_the_request_is_reported_not_hung() {
        let slot = MidiExchangeSlot::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        slot.install(tx);
        tokio::spawn(async move {
            drop(rx.recv().await.expect("request"));
        });
        let err = slot
            .run("d".into(), vec![0xF0], vec![], Duration::from_secs(30))
            .await
            .unwrap_err();
        assert!(err.contains("without answering"), "err: {err}");
    }

    /// A wedged worker (never answers, never dies) is bounded by our own
    /// slack — the kernel's outer deadline must not be the first thing to
    /// fire, or the error would blame the wrong layer.
    #[tokio::test(start_paused = true)]
    async fn a_wedged_worker_is_bounded_by_the_slack_not_by_the_kernel() {
        let slot = MidiExchangeSlot::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        slot.install(tx);
        let err = slot
            .run("d".into(), vec![0xF0], vec![], Duration::from_millis(2000))
            .await
            .unwrap_err();
        assert!(err.contains("did not answer within 2.5s"), "err: {err}");
    }

    #[tokio::test]
    async fn installing_replaces_the_previous_sink() {
        let slot = MidiExchangeSlot::new();
        let (first, mut first_rx) = mpsc::unbounded_channel();
        let (second, mut second_rx) = mpsc::unbounded_channel();
        slot.install(first);
        slot.install(second);
        tokio::spawn({
            let slot = slot.clone();
            async move {
                let _ = slot
                    .run("d".into(), vec![0xF0], vec![], Duration::from_millis(50))
                    .await;
            }
        });
        assert!(second_rx.recv().await.is_some(), "the newest sink receives");
        assert!(first_rx.try_recv().is_err(), "the replaced sink receives nothing");

        slot.clear();
        assert!(!slot.is_installed());
    }
}
