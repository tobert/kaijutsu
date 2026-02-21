//! Context metadata types.
//!
//! A `Context` captures the metadata of a context within a kernel —
//! its identity, lineage, and who created it. This is the birth certificate,
//! not the full runtime state.

use serde::{Deserialize, Serialize};

use crate::ids::{ContextId, KernelId, PrincipalId};

/// Metadata for a context within a kernel.
///
/// Used for listing, display, and constellation rendering. The actual CRDT
/// document lives in the kernel; this is the lightweight summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    /// Globally unique context identifier (also serves as document_id).
    pub id: ContextId,
    /// The kernel this context belongs to.
    pub kernel_id: KernelId,
    /// Human-friendly label (e.g., "default", "debug-auth"). Mutable.
    pub label: Option<String>,
    /// Parent context for fork/thread lineage. None for root contexts.
    pub parent_id: Option<ContextId>,
    /// Who created this context.
    pub created_by: PrincipalId,
    /// When this context was created (Unix millis).
    pub created_at: u64,
}

impl Context {
    /// Create a new context info for a freshly created context.
    pub fn new(
        kernel_id: KernelId,
        label: Option<String>,
        parent_id: Option<ContextId>,
        created_by: PrincipalId,
    ) -> Self {
        Self {
            id: ContextId::new(),
            kernel_id,
            label,
            parent_id,
            created_by,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        }
    }

    /// Whether this is a root context (no parent).
    pub fn is_root(&self) -> bool {
        self.parent_id.is_none()
    }

    /// Whether this context is a fork/thread of another.
    pub fn is_child(&self) -> bool {
        self.parent_id.is_some()
    }

    /// Display string: label if present, otherwise short hex ID.
    pub fn display_name(&self) -> String {
        self.id.display_or(self.label.as_deref())
    }
}

/// Walk a parent chain to collect fork lineage.
///
/// Given a set of contexts and a starting ID, returns the chain from
/// the starting context up to the root (inclusive).
pub fn fork_lineage(
    contexts: &[Context],
    start: ContextId,
) -> Vec<&Context> {
    let mut chain = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut current = Some(start);
    while let Some(id) = current {
        if !seen.insert(id) {
            break; // cycle detected
        }
        if let Some(ctx) = contexts.iter().find(|c| c.id == id) {
            chain.push(ctx);
            current = ctx.parent_id;
        } else {
            break;
        }
    }
    chain
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_info_construction() {
        let kernel = KernelId::new();
        let creator = PrincipalId::new();
        let info = Context::new(kernel, Some("default".into()), None, creator);

        assert_eq!(info.kernel_id, kernel);
        assert_eq!(info.label, Some("default".to_string()));
        assert!(info.parent_id.is_none());
        assert_eq!(info.created_by, creator);
        assert!(info.is_root());
        assert!(!info.is_child());
    }

    #[test]
    fn test_context_info_child() {
        let kernel = KernelId::new();
        let creator = PrincipalId::new();
        let parent = ContextId::new();
        let info = Context::new(kernel, Some("debug".into()), Some(parent), creator);

        assert!(info.is_child());
        assert!(!info.is_root());
        assert_eq!(info.parent_id, Some(parent));
    }

    #[test]
    fn test_display_name_prefers_label() {
        let kernel = KernelId::new();
        let creator = PrincipalId::new();
        let info = Context::new(kernel, Some("main".into()), None, creator);
        assert_eq!(info.display_name(), "main");
    }

    #[test]
    fn test_display_name_falls_back_to_short_hex() {
        let kernel = KernelId::new();
        let creator = PrincipalId::new();
        let info = Context::new(kernel, None, None, creator);
        assert_eq!(info.display_name().len(), 8); // short hex
    }

    #[test]
    fn test_fork_lineage() {
        let kernel = KernelId::new();
        let creator = PrincipalId::new();

        let root = Context::new(kernel, Some("root".into()), None, creator);
        let child = Context::new(kernel, Some("child".into()), Some(root.id), creator);
        let grandchild =
            Context::new(kernel, Some("grandchild".into()), Some(child.id), creator);

        let contexts = vec![root.clone(), child.clone(), grandchild.clone()];
        let chain = fork_lineage(&contexts, grandchild.id);

        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].id, grandchild.id);
        assert_eq!(chain[1].id, child.id);
        assert_eq!(chain[2].id, root.id);
    }

    #[test]
    fn test_fork_lineage_root_only() {
        let kernel = KernelId::new();
        let creator = PrincipalId::new();
        let root = Context::new(kernel, None, None, creator);
        let contexts = vec![root.clone()];
        let chain = fork_lineage(&contexts, root.id);
        assert_eq!(chain.len(), 1);
    }

    #[test]
    fn test_fork_lineage_missing_parent() {
        let kernel = KernelId::new();
        let creator = PrincipalId::new();
        // Child references a parent that doesn't exist in the list
        let orphan = Context::new(kernel, None, Some(ContextId::new()), creator);
        let contexts = vec![orphan.clone()];
        let chain = fork_lineage(&contexts, orphan.id);
        // Stops at the orphan since parent isn't found
        assert_eq!(chain.len(), 1);
    }

    #[test]
    fn test_context_info_serde_roundtrip() {
        let kernel = KernelId::new();
        let creator = PrincipalId::new();
        let info = Context::new(kernel, Some("test".into()), None, creator);
        let json = serde_json::to_string(&info).unwrap();
        let parsed: Context = serde_json::from_str(&json).unwrap();
        assert_eq!(info, parsed);
    }

    #[test]
    fn test_context_info_postcard_roundtrip() {
        let kernel = KernelId::new();
        let creator = PrincipalId::new();
        let info = Context::new(kernel, Some("test".into()), None, creator);
        let bytes = postcard::to_stdvec(&info).unwrap();
        let parsed: Context = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(info, parsed);
    }

    #[test]
    fn test_fork_lineage_cycle_terminates() {
        let kernel = KernelId::new();
        let creator = PrincipalId::new();

        // Manually construct a cycle: A → B → A
        let mut a = Context::new(kernel, Some("a".into()), None, creator);
        let b = Context::new(kernel, Some("b".into()), Some(a.id), creator);
        // Create the cycle
        a.parent_id = Some(b.id);

        let contexts = vec![a.clone(), b.clone()];
        let chain = fork_lineage(&contexts, a.id);
        // Should terminate without infinite loop
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].id, a.id);
        assert_eq!(chain[1].id, b.id);
    }
}
