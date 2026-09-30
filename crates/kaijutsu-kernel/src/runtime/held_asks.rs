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
}

/// The asks model turns are holding on, keyed by request id.
#[derive(Default)]
pub struct HeldAsks {
    holders: parking_lot::Mutex<HashMap<String, Holder>>,
}

impl HeldAsks {
    /// Register a hold before the pair is published as `Waiting`, so the
    /// worker, which acts only on a released pair, always finds it.
    pub(crate) fn hold<'a>(&'a self, request_id: &str) -> HeldAsk<'a> {
        let (settled, rx) = tokio::sync::oneshot::channel();
        let stopped = CancellationToken::new();
        let previous = self.holders.lock().insert(request_id.to_owned(), Holder { settled, stopped: stopped.clone() });
        assert!(previous.is_none(), "ask {request_id} already has a holding turn");
        HeldAsk { asks: self, request_id: request_id.to_owned(), settled: Some(rx), stopped }
    }

    /// The token an approved command runs under while a turn holds it, or
    /// `None` when no turn is holding this ask.
    pub(crate) fn holder(&self, request_id: &str) -> Option<CancellationToken> {
        self.holders.lock().get(request_id).map(|holder| holder.stopped.clone())
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

/// One turn's hold on one ask. Dropping it ends the hold and stops a command
/// the worker is running for it.
pub(crate) struct HeldAsk<'a> {
    asks: &'a HeldAsks,
    request_id: String,
    settled: Option<tokio::sync::oneshot::Receiver<()>>,
    stopped: CancellationToken,
}

impl HeldAsk<'_> {
    /// Wait until the worker settles the pair (`true`) or `stop` fires
    /// (`false`). A dropped sender without a release reads as settled; the
    /// caller reads the pair's state either way.
    pub(crate) async fn wait(&mut self, stop: &CancellationToken) -> bool {
        let settled = self.settled.as_mut().expect("a hold is waited once");
        tokio::select! {
            biased;
            _ = settled => true,
            _ = stop.cancelled() => false,
        }
    }
}

impl Drop for HeldAsk<'_> {
    fn drop(&mut self) {
        self.stopped.cancel();
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
        let mut hold = asks.hold("ask-1");
        assert!(asks.holder("ask-1").is_some());
        assert!(asks.release("ask-1"));
        assert!(hold.wait(&CancellationToken::new()).await);
        assert!(asks.holder("ask-1").is_none());
        assert!(!asks.release("ask-1"), "a second release finds no holder");
    }

    #[tokio::test]
    async fn stopping_returns_false_and_dropping_stops_the_command() {
        let asks = HeldAsks::default();
        let mut hold = asks.hold("ask-2");
        let command = asks.holder("ask-2").unwrap();
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
