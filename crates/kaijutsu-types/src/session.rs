//! Session metadata types.
//!
//! A `Session` records the fact that a principal connected to a kernel.
//! Like `Kernel` and `Context`, this is a birth certificate — runtime
//! state (active context, connection health) lives elsewhere.

use serde::{Deserialize, Serialize};

use crate::ids::{KernelId, PrincipalId, SessionId};

/// Birth certificate for a connection session.
///
/// Created when a principal connects to a kernel. Immutable after creation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// Globally unique session identifier (UUIDv7, time-ordered).
    pub id: SessionId,
    /// Who connected.
    pub principal_id: PrincipalId,
    /// Which kernel they connected to.
    pub kernel_id: KernelId,
    /// When this session was created (Unix millis).
    pub created_at: u64,
}

impl Session {
    /// Create a new session record.
    pub fn new(principal_id: PrincipalId, kernel_id: KernelId) -> Self {
        Self {
            id: SessionId::new(),
            principal_id,
            kernel_id,
            created_at: crate::now_millis(),
        }
    }

    /// Display string: short hex ID (sessions don't have labels).
    pub fn display_name(&self) -> String {
        self.id.short()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_construction() {
        let principal = PrincipalId::new();
        let kernel = KernelId::new();
        let s = Session::new(principal, kernel);

        assert_eq!(s.principal_id, principal);
        assert_eq!(s.kernel_id, kernel);
        assert!(s.created_at > 0);
    }

    #[test]
    fn test_display_name() {
        let s = Session::new(PrincipalId::new(), KernelId::new());
        assert_eq!(s.display_name().len(), 8); // short hex
    }

    #[test]
    fn test_json_roundtrip() {
        let s = Session::new(PrincipalId::new(), KernelId::new());
        let json = serde_json::to_string(&s).unwrap();
        let parsed: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(s, parsed);
    }

    #[test]
    fn test_postcard_roundtrip() {
        let s = Session::new(PrincipalId::new(), KernelId::new());
        let bytes = postcard::to_stdvec(&s).unwrap();
        let parsed: Session = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(s, parsed);
    }

    #[test]
    fn test_unique_ids() {
        let principal = PrincipalId::new();
        let kernel = KernelId::new();
        let a = Session::new(principal, kernel);
        let b = Session::new(principal, kernel);
        assert_ne!(a.id, b.id);
    }
}
