//! Context tracking for Kaijutsu client.
//!
//! This module provides Bevy resources for tracking active contexts:
//! - `ContextOrder`: Ordered list of known context IDs
//! - `ActiveContext`: Which context is currently displayed
//!
//! Context list is populated by ContextJoined events (in `ui/state.rs`).

mod registry;

pub use registry::{ActiveContext, ContextOrder};

use bevy::prelude::*;

/// Plugin for context tracking.
pub struct ConversationPlugin;

impl Plugin for ConversationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ContextOrder>()
            .init_resource::<ActiveContext>();
    }
}
