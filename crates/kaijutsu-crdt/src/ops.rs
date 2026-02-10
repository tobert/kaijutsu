//! CRDT operation serialization helpers.
//!
//! With the unified diamond_types_extended library, operations are handled internally via `SerializedOps`.
//! This module re-exports the relevant types for convenience.

// Re-export key types from diamond_types_extended for sync operations
pub use diamond_types_extended::{Frontier, SerializedOps, SerializedOpsOwned, LV};

#[cfg(test)]
mod tests {
    use diamond_types_extended::{Document, Frontier};

    #[test]
    fn test_ops_sync() {
        // Create two documents
        let mut doc_a = Document::new();
        let mut doc_b = Document::new();

        let alice = doc_a.get_or_create_agent("alice");
        let bob = doc_b.get_or_create_agent("bob");

        // Alice makes changes
        doc_a.transact(alice, |tx| {
            tx.root().set("key", 42i64);
        });

        // Sync to Bob - ops_since returns SerializedOps<'_> borrowed from doc_a
        let ops = doc_a.ops_since(&Frontier::root()).into();
        doc_b.merge_ops(ops).unwrap();

        // They should converge
        assert_eq!(
            doc_a.root().get("key").unwrap().as_int(),
            doc_b.root().get("key").unwrap().as_int()
        );

        // Bob makes changes
        doc_b.transact(bob, |tx| {
            tx.root().set("key2", "hello");
        });

        // Sync back to Alice
        let ops = doc_b.ops_since(&Frontier::root()).into();
        doc_a.merge_ops(ops).unwrap();

        // Should still converge
        assert_eq!(
            doc_a.root().get("key2").unwrap().as_str(),
            doc_b.root().get("key2").unwrap().as_str()
        );
    }
}
