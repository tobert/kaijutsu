//! Model turns holding on their own asks.
//!
//! A model's gated tool call leaves a durable ask and a `Waiting` pair, and
//! its turn waits here for the answer instead of ending at the gate. The
//! approval worker stays the only executor: it settles the pair, then
//! releases the holder, and the turn reads the settled result as its tool
//! result. Only the in-kernel model turn holds; the MCP and RPC shell paths
//! stay non-blocking. See `docs/gate-resume.md`, "The turn holds".
//!
//! Nothing here is durable. A restart drops every holder; boot abandons the
//! pending asks and closes the `Waiting` pairs, so a held call fails.

use std::collections::HashMap;

use tokio_util::sync::CancellationToken;

struct Holder {
    settled: tokio::sync::oneshot::Sender<()>,
    /// Cancelled when the holding turn stops waiting, so an approved command
    /// already running for it stops too.
    stopped: CancellationToken,
    until: HoldUntil,
}

/// When the worker releases a hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HoldUntil {
    /// A foreground call: once the pair is settled. The turn reads the
    /// result, so the worker sends no completion notice.
    Settled,
    /// A background call: once the approved command starts, or once the
    /// pair settles without running. Completion reaches the model through
    /// its notice.
    Started,
}

/// A holding turn, as the worker sees it.
pub(crate) struct HolderView {
    pub stopped: CancellationToken,
    pub until: HoldUntil,
}

/// The asks model turns are holding on, keyed by request id.
#[derive(Default)]
pub struct HeldAsks {
    holders: parking_lot::Mutex<HashMap<String, Holder>>,
    /// Set when no answer can arrive, such as on an offline kernel: a call
    /// fails with this reason instead of holding.
    refusal: std::sync::OnceLock<&'static str>,
}

impl HeldAsks {
    /// Refuse every hold from now on, with `reason` as the failed call's
    /// result.
    pub(crate) fn refuse(&self, reason: &'static str) {
        let _ = self.refusal.set(reason);
    }

    /// Why a call must not hold, when it must not.
    pub(crate) fn refusal(&self) -> Option<&'static str> {
        self.refusal.get().copied()
    }

    /// Register a hold before the pair is published as `Waiting`, so the
    /// worker, which acts only on a released pair, always finds it.
    pub(crate) fn hold<'a>(&'a self, request_id: &str, until: HoldUntil) -> HeldAsk<'a> {
        let (settled, rx) = tokio::sync::oneshot::channel();
        let stopped = CancellationToken::new();
        let previous = self.holders.lock().insert(request_id.to_owned(), Holder { settled, stopped: stopped.clone(), until });
        assert!(previous.is_none(), "ask {request_id} already has a holding turn");
        HeldAsk { asks: self, request_id: request_id.to_owned(), settled: Some(rx), stopped, released: false }
    }

    /// The turn holding this ask, or `None` when no turn is holding it.
    pub(crate) fn holder(&self, request_id: &str) -> Option<HolderView> {
        self.holders.lock().get(request_id).map(|holder| HolderView { stopped: holder.stopped.clone(), until: holder.until })
    }

    /// Tell the holding turn its pair is settled. Returns whether a turn was
    /// holding and received it.
    pub(crate) fn release(&self, request_id: &str) -> bool {
        match self.holders.lock().remove(request_id) {
            Some(holder) => holder.settled.send(()).is_ok(),
            None => false,
        }
    }
}

/// One turn's hold on one ask. Dropping it before the worker releases it
/// ends the hold and stops a command the worker is running for it.
pub(crate) struct HeldAsk<'a> {
    asks: &'a HeldAsks,
    request_id: String,
    settled: Option<tokio::sync::oneshot::Receiver<()>>,
    stopped: CancellationToken,
    /// The worker released this hold; a background command it started
    /// keeps running after the hold is dropped.
    released: bool,
}

impl HeldAsk<'_> {
    /// Wait until the worker settles the pair (`true`) or `stop` fires
    /// (`false`). A dropped sender without a release reads as settled; the
    /// caller reads the pair's state either way.
    pub(crate) async fn wait(&mut self, stop: &CancellationToken) -> bool {
        let settled = self.settled.as_mut().expect("a hold is waited once");
        self.released = tokio::select! {
            biased;
            _ = settled => true,
            _ = stop.cancelled() => false,
        };
        self.released
    }

    /// End the hold and leave the ask to the approval worker: an answer
    /// that arrives later is acted on as for an ask whose turn ended at the
    /// gate. A command the worker already started for this hold keeps
    /// running.
    pub(crate) fn leave_to_worker(mut self) {
        self.released = true;
    }
}

impl Drop for HeldAsk<'_> {
    fn drop(&mut self) {
        if !self.released { self.stopped.cancel(); }
        // Ask ids are unique and `hold` refuses a second holder, so this
        // entry can only be this hold's own.
        self.asks.holders.lock().remove(&self.request_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_release_wakes_the_holder_and_ends_the_hold() {
        let asks = HeldAsks::default();
        let mut hold = asks.hold("ask-1", HoldUntil::Settled);
        assert!(asks.holder("ask-1").is_some());
        assert!(asks.release("ask-1"));
        assert!(hold.wait(&CancellationToken::new()).await);
        assert!(asks.holder("ask-1").is_none());
        let command = hold.stopped.clone();
        drop(hold);
        assert!(!command.is_cancelled(), "a released hold leaves a started command running");
        assert!(!asks.release("ask-1"), "a second release finds no holder");
    }

    #[tokio::test]
    async fn stopping_returns_false_and_dropping_stops_the_command() {
        let asks = HeldAsks::default();
        let mut hold = asks.hold("ask-2", HoldUntil::Settled);
        let command = asks.holder("ask-2").unwrap().stopped;
        let stop = CancellationToken::new();
        stop.cancel();
        assert!(!hold.wait(&stop).await);
        assert!(!command.is_cancelled());
        drop(hold);
        assert!(command.is_cancelled(), "ending the hold stops a command run for it");
        assert!(asks.holder("ask-2").is_none());
        assert!(!asks.release("ask-2"));
    }
}
