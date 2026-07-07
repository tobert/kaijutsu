//! Ordered outbound keystroke pipe — the fix for two input-path hazards the
//! vi health review found in the old ship-a-detached-task-per-keystroke shape:
//!
//! - **Reordering.** A detached `IoTaskPool` task per keystroke plus the RPC
//!   actor's concurrent dispatch meant nothing guaranteed two keystrokes issued
//!   in one frame arrived in order (burst input — BRP `send_keys`, key repeat —
//!   was the concrete trigger). The pipe serializes: one batch on the wire at a
//!   time, everything else queues behind it in keyboard order.
//! - **Silent drops.** A transient RPC failure (timeout, connection blip) was
//!   logged and forgotten — the keystroke vanished. The pipe retries a failed
//!   batch once before dropping it, and the drop is reported to the caller so
//!   it never disappears without a trace.
//!
//! Coalescing falls out for free: while a batch is in flight, every new
//! keystroke queues, and the next ship concatenates them into one notation
//! string (`EditorCore::apply_keys` natively parses sequences) — burst typing
//! becomes one RPC instead of N.
//!
//! [`KeyPipe`] is pure (no Bevy, no RPC) so the ordering/retry contract is
//! unit-tested headless; `view::editor` wraps it in a resource and does the
//! actual shipping.

use std::collections::VecDeque;

/// What to do after a batch failed on the wire — [`KeyPipe::on_failure`]'s
/// verdict.
#[derive(Debug, PartialEq, Eq)]
pub enum FailureVerdict {
    /// First failure: resend this exact batch (newer keystrokes stay queued
    /// behind it, so order holds).
    Retry(String),
    /// Second consecutive failure: the batch is dropped. The caller should
    /// surface the loss (never a silent drop); queued keystrokes proceed.
    Dropped(String),
}

/// Pure ordering/retry core for outbound editor keystrokes.
#[derive(Debug, Default)]
pub struct KeyPipe {
    /// Keystrokes waiting to ship, oldest first, in keyboard order.
    queue: VecDeque<String>,
    /// The batch currently on the wire, if any.
    in_flight: Option<String>,
    /// The in-flight batch is already its second attempt.
    retried: bool,
}

impl KeyPipe {
    /// Queue a keystroke (vi notation) behind everything already waiting.
    pub fn push(&mut self, notation: &str) {
        self.queue.push_back(notation.to_string());
    }

    /// The next batch to ship: every queued keystroke coalesced into one
    /// notation string, in order. `None` while a batch is on the wire (ship
    /// after its outcome lands) or when nothing waits.
    pub fn take_batch(&mut self) -> Option<String> {
        if self.in_flight.is_some() || self.queue.is_empty() {
            return None;
        }
        let batch: String = self.queue.drain(..).collect();
        self.in_flight = Some(batch.clone());
        self.retried = false;
        Some(batch)
    }

    /// The on-wire batch landed. Queued keystrokes (typed while it flew) are
    /// released for the next [`take_batch`](Self::take_batch).
    pub fn on_success(&mut self) {
        self.in_flight = None;
        self.retried = false;
    }

    /// The on-wire batch failed transiently. First failure → retry the same
    /// batch; second → drop it (the verdict carries the lost keys so the
    /// caller can report them). A spurious call with nothing in flight drops
    /// an empty batch — harmless, but callers shouldn't do it.
    pub fn on_failure(&mut self) -> FailureVerdict {
        let Some(batch) = self.in_flight.clone() else {
            return FailureVerdict::Dropped(String::new());
        };
        if self.retried {
            self.in_flight = None;
            self.retried = false;
            FailureVerdict::Dropped(batch)
        } else {
            self.retried = true;
            FailureVerdict::Retry(batch)
        }
    }

    /// Abandon everything — the session closed, was lost, or was replaced.
    pub fn clear(&mut self) {
        self.queue.clear();
        self.in_flight = None;
        self.retried = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keystrokes_coalesce_in_keyboard_order() {
        let mut pipe = KeyPipe::default();
        pipe.push("i");
        pipe.push("a");
        pipe.push("<Esc>");
        assert_eq!(pipe.take_batch().as_deref(), Some("ia<Esc>"));
    }

    #[test]
    fn nothing_ships_while_a_batch_is_in_flight() {
        // The ordering guarantee: keys typed during a round-trip wait for it.
        let mut pipe = KeyPipe::default();
        pipe.push("d");
        assert_eq!(pipe.take_batch().as_deref(), Some("d"));
        pipe.push("w");
        assert_eq!(pipe.take_batch(), None, "in-flight batch blocks the next");
        pipe.on_success();
        assert_eq!(pipe.take_batch().as_deref(), Some("w"), "released after the outcome");
    }

    #[test]
    fn empty_queue_ships_nothing() {
        let mut pipe = KeyPipe::default();
        assert_eq!(pipe.take_batch(), None);
    }

    #[test]
    fn first_failure_retries_the_same_batch_ahead_of_newer_keys() {
        let mut pipe = KeyPipe::default();
        pipe.push("x");
        let batch = pipe.take_batch().unwrap();
        pipe.push("u"); // typed while the batch flew
        assert_eq!(
            pipe.on_failure(),
            FailureVerdict::Retry(batch),
            "first failure resends the exact batch"
        );
        assert_eq!(pipe.take_batch(), None, "the retry still occupies the wire");
        pipe.on_success();
        assert_eq!(pipe.take_batch().as_deref(), Some("u"), "newer keys follow, in order");
    }

    #[test]
    fn second_failure_drops_the_batch_and_releases_the_queue() {
        let mut pipe = KeyPipe::default();
        pipe.push("x");
        pipe.take_batch().unwrap();
        pipe.push("j");
        assert!(matches!(pipe.on_failure(), FailureVerdict::Retry(_)));
        // The retry fails too: the batch is dropped (reported, not silent) and
        // the keys typed since then proceed.
        assert_eq!(
            pipe.on_failure(),
            FailureVerdict::Dropped("x".to_string()),
            "second consecutive failure drops the batch"
        );
        assert_eq!(pipe.take_batch().as_deref(), Some("j"));
    }

    #[test]
    fn success_resets_the_failure_streak() {
        let mut pipe = KeyPipe::default();
        pipe.push("a");
        pipe.take_batch().unwrap();
        assert!(matches!(pipe.on_failure(), FailureVerdict::Retry(_)));
        pipe.on_success(); // the retry landed

        // A later batch gets its own fresh retry budget.
        pipe.push("b");
        pipe.take_batch().unwrap();
        assert!(
            matches!(pipe.on_failure(), FailureVerdict::Retry(_)),
            "the earlier failure doesn't count against a new batch"
        );
    }

    #[test]
    fn clear_abandons_queue_and_in_flight() {
        let mut pipe = KeyPipe::default();
        pipe.push("i");
        pipe.take_batch().unwrap();
        pipe.push("x");
        pipe.clear();
        assert_eq!(pipe.take_batch(), None, "nothing survives a clear");
        // A stale outcome after the clear is a harmless no-op.
        assert_eq!(pipe.on_failure(), FailureVerdict::Dropped(String::new()));
    }
}
