//! Per-context interrupt state for cancelling LLM streams and shell jobs.
//!
//! `ContextInterruptState` is created fresh at the start of each prompt and
//! stored in `SharedKernelState.context_interrupts`. The `interruptContext`
//! RPC method uses it to signal soft or hard interrupts.
//!
//! # Soft vs Hard
//! - **Soft** (`immediate=false`): sets `stop_after_turn` flag → agentic loop
//!   checks it before each LLM call and breaks cleanly.
//! - **Hard** (`immediate=true`): cancels the `CancellationToken` → the stream
//!   event loop aborts immediately via `tokio::select!`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio_util::sync::CancellationToken;

/// Per-context cancellation state.
///
/// A fresh instance is created at the start of every `process_llm_stream`
/// call (via `create_interrupt`). The `CancellationToken` cannot be
/// reset, so re-creating on each prompt is the correct approach.
///
/// Each instance carries a `generation` counter to prevent a race where
/// stream A's cleanup removes stream B's interrupt state. The cleanup
/// path compares generations before removing.
pub struct ContextInterruptState {
    /// Soft interrupt: stop the agentic loop before the NEXT LLM call.
    pub stop_after_turn: AtomicBool,
    /// Hard interrupt: abort the current LLM stream immediately.
    pub cancel: CancellationToken,
    /// Monotonically increasing generation counter. Assigned by
    /// `SharedKernelState::create_interrupt` from a per-map atomic.
    pub generation: u64,
}

impl ContextInterruptState {
    pub fn new(generation: u64) -> Arc<Self> {
        Arc::new(Self {
            stop_after_turn: AtomicBool::new(false),
            cancel: CancellationToken::new(),
            generation,
        })
    }

    /// Soft interrupt — stop the agentic loop after the current tool turn.
    pub fn soft(&self) {
        self.stop_after_turn.store(true, Ordering::Relaxed);
    }

    /// Hard interrupt — abort the current LLM stream immediately.
    pub fn hard(&self) {
        self.cancel.cancel();
    }
}
