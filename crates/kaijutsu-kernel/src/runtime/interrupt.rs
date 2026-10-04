//! Per-turn cancellation for provider streams and their tool calls.
//!
//! Each accepted turn owns fresh cancellation state in `TurnState`. Context
//! interruption signals every outstanding turn, including queued requests.
//!
//! # Soft vs Hard
//! - **Soft** (`immediate=false`): sets `stop_after_turn` flag → agentic loop
//!   checks it before each LLM call and breaks cleanly.
//! - **Hard** (`immediate=true`): cancels the `CancellationToken` → the stream
//!   event loop aborts immediately via `tokio::select!`.
//!
//! Both end a tool call's wait on its own ask (`stop_waiting`), and the turn
//! abandons that ask. A soft interrupt that keeps held asks
//! ([`ContextInterruptState::soft_keeping_asks`]) ends the wait too, but
//! leaves each held ask pending for the approval worker: an allow runs its
//! stored command once and settles the call's result in place, as for an ask
//! whose turn ended at the gate.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio_util::sync::CancellationToken;

/// Cancellation state owned by one accepted turn's lease.
pub struct ContextInterruptState {
    /// Soft interrupt: stop the agentic loop before the NEXT LLM call.
    pub stop_after_turn: AtomicBool,
    /// Hard interrupt: abort the current LLM stream immediately.
    pub cancel: CancellationToken,
    /// Either interrupt: a tool call holding on its ask stops waiting. A
    /// result that arrives after either could not reach the model in this
    /// turn.
    pub stop_waiting: CancellationToken,
    /// Set before `stop_waiting` fires by [`Self::soft_keeping_asks`]: a call
    /// that stops waiting leaves its ask pending instead of abandoning it.
    keep_held_asks: AtomicBool,
}

impl ContextInterruptState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            stop_after_turn: AtomicBool::new(false),
            cancel: CancellationToken::new(),
            stop_waiting: CancellationToken::new(),
            keep_held_asks: AtomicBool::new(false),
        })
    }

    /// Soft interrupt — stop the agentic loop after the current tool turn. A
    /// call waiting on its ask stops waiting.
    pub fn soft(&self) {
        self.stop_after_turn.store(true, Ordering::Relaxed);
        self.stop_waiting.cancel();
    }

    /// Soft interrupt that leaves every held ask pending: the loop stops
    /// before its next LLM call and a call waiting on its ask stops waiting,
    /// but the ask stays open for the approval worker.
    pub fn soft_keeping_asks(&self) {
        self.keep_held_asks.store(true, Ordering::SeqCst);
        self.soft();
    }

    /// Whether a call that stopped waiting leaves its ask to the approval
    /// worker instead of abandoning it.
    pub fn keeps_held_asks(&self) -> bool {
        self.keep_held_asks.load(Ordering::SeqCst)
    }

    /// Hard interrupt — abort the current LLM stream immediately.
    pub fn hard(&self) {
        self.cancel.cancel();
        self.stop_waiting.cancel();
    }
}
