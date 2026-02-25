//! Context-based conversation tracking resources.

use bevy::prelude::*;
use kaijutsu_crdt::ContextId;

/// Ordered list of known contexts.
///
/// Populated by ContextJoined events. Maintains display order.
#[derive(Resource, Default)]
pub struct ContextOrder {
    ids: Vec<ContextId>,
}

impl ContextOrder {
    /// Add a context (no-op if already present).
    pub fn add(&mut self, id: ContextId) {
        if !self.ids.contains(&id) {
            self.ids.push(id);
        }
    }

    /// Get all context IDs in display order.
    pub fn ids(&self) -> &[ContextId] {
        &self.ids
    }

    /// Move a context to the front of the order (most recent).
    pub fn move_to_front(&mut self, id: ContextId) {
        if let Some(pos) = self.ids.iter().position(|oid| *oid == id) {
            let id = self.ids.remove(pos);
            self.ids.insert(0, id);
        }
    }
}

/// Resource tracking the currently active context.
///
/// The active context is what's displayed in the MainCell and
/// what receives messages from the prompt.
#[derive(Resource, Default)]
pub struct ActiveContext(pub Option<ContextId>);

impl ActiveContext {
    /// Get the current context ID, if any.
    pub fn id(&self) -> Option<ContextId> {
        self.0
    }
}
