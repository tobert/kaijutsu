//! Sequence state — tracks multi-key sequences like g→t, g→T, g→g.

use bevy::prelude::*;

use super::binding::InputSource;

/// Tracks an in-progress multi-key sequence.
///
/// When a key is pressed that could be a sequence prefix (e.g. `g`),
/// the dispatcher stores it here. On the next key press within the timeout,
/// it checks for sequence bindings. If the timeout expires, the prefix
/// is discarded.
#[derive(Resource, Default, Reflect)]
#[reflect(Resource)]
pub struct SequenceState {
    /// The pending prefix key (None = no sequence in progress)
    #[reflect(ignore)]
    pub pending: Option<InputSource>,
    /// When the prefix was pressed (for timeout detection)
    #[reflect(ignore)]
    pub started_at: Option<std::time::Instant>,
}

impl SequenceState {
    /// Start a new sequence with the given prefix.
    pub fn start(&mut self, prefix: InputSource) {
        self.pending = Some(prefix);
        self.started_at = Some(std::time::Instant::now());
    }

    /// Clear the pending sequence.
    pub fn clear(&mut self) {
        self.pending = None;
        self.started_at = None;
    }

    /// Check if the sequence has timed out.
    pub fn is_expired(&self, timeout_ms: u64) -> bool {
        self.started_at
            .map(|t| t.elapsed().as_millis() > timeout_ms as u128)
            .unwrap_or(false)
    }

    /// Check if a given prefix matches the pending sequence.
    pub fn matches_prefix(&self, prefix: &InputSource) -> bool {
        self.pending.as_ref() == Some(prefix)
    }
}
