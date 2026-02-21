//! Kernel metadata types.
//!
//! A kernel is 会場 (kaijou) — the meeting place. It doesn't fork; the founder
//! starts it, and participants join as peers. `Kernel` is the birth certificate:
//! who founded it, when, and what it's called. Runtime state (VFS mounts,
//! tool registry, connected sessions) lives elsewhere.

use serde::{Deserialize, Serialize};

use crate::ids::{KernelId, PrincipalId};

/// Birth certificate for a kernel instance.
///
/// Immutable after creation. The founder is whoever started the kernel —
/// not an "owner" with special privileges. All participants are peers once
/// they've joined.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kernel {
    /// Globally unique kernel identifier (UUIDv7, time-ordered).
    pub id: KernelId,
    /// Who started this kernel.
    pub founder: PrincipalId,
    /// Human-friendly label (e.g., "kaijutsu-dev", "pair-session").
    pub label: Option<String>,
    /// When this kernel was created (Unix millis).
    pub created_at: u64,
}

impl Kernel {
    /// Create a new kernel, founded by the given principal.
    pub fn new(founder: PrincipalId, label: Option<String>) -> Self {
        Self {
            id: KernelId::new(),
            founder,
            label,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        }
    }

    /// Display string: label if present, otherwise short hex ID.
    pub fn display_name(&self) -> String {
        self.id.display_or(self.label.as_deref())
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
        let founder = PrincipalId::new();
        let k = Kernel::new(founder, Some("dev".into()));

        assert_eq!(k.founder, founder);
        assert_eq!(k.label, Some("dev".to_string()));
        assert!(k.created_at > 0);
    }

    #[test]
    fn test_no_label() {
        let founder = PrincipalId::new();
        let k = Kernel::new(founder, None);
        assert!(k.label.is_none());
    }

    #[test]
    fn test_display_name_prefers_label() {
        let k = Kernel::new(PrincipalId::new(), Some("pair-session".into()));
        assert_eq!(k.display_name(), "pair-session");
    }

    #[test]
    fn test_display_name_falls_back_to_hex() {
        let k = Kernel::new(PrincipalId::new(), None);
        assert_eq!(k.display_name().len(), 8); // short hex
    }

    #[test]
    fn test_json_roundtrip() {
        let k = Kernel::new(PrincipalId::new(), Some("test".into()));
        let json = serde_json::to_string(&k).unwrap();
        let parsed: Kernel = serde_json::from_str(&json).unwrap();
        assert_eq!(k, parsed);
    }

    #[test]
    fn test_postcard_roundtrip() {
        let k = Kernel::new(PrincipalId::new(), Some("test".into()));
        let bytes = postcard::to_stdvec(&k).unwrap();
        let parsed: Kernel = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(k, parsed);
    }

    #[test]
    fn test_system_founder() {
        let k = Kernel::new(PrincipalId::system(), Some("system-kernel".into()));
        assert_eq!(k.founder, PrincipalId::system());
    }

    #[test]
    fn test_unique_ids() {
        let founder = PrincipalId::new();
        let a = Kernel::new(founder, None);
        let b = Kernel::new(founder, None);
        assert_ne!(a.id, b.id);
    }
}
