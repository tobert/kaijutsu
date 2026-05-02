//! Context management engine for kaish shell.
//!
//! Provides the `context` command for creating and switching contexts from
//! the shell interface. Delegates all context state to the kernel's DriftRouter
//! (the single source of truth for context labels and metadata).
//!
//! # Usage
//!
//! ```kaish
//! # Switch to an existing context
//! context switch default
//!
//! # List contexts
//! context list
//! ```

use dashmap::DashMap;
use std::sync::Arc;

use kaijutsu_types::{ContextId, SessionId};


// ============================================================================
// Shell Context State - per-session "current context" tracking
// ============================================================================

/// Per-session current-context map. Each SSH session gets independent state
/// so that one session switching context doesn't affect others.
///
/// Uses DashMap for synchronous, concurrent access.
pub type SessionContextMap = Arc<DashMap<SessionId, ContextId>>;

/// Extension trait for SessionContextMap to provide convenient accessors.
pub trait SessionContextExt {
    /// Get the current context for a session.
    fn current(&self, session_id: &SessionId) -> Option<ContextId>;
}

impl SessionContextExt for SessionContextMap {
    fn current(&self, session_id: &SessionId) -> Option<ContextId> {
        self.get(session_id).map(|r| *r)
    }
}

/// Create a new session-context map.
pub fn session_context_map() -> SessionContextMap {
    Arc::new(DashMap::new())
}
