//! CRDT operation serialization helpers.
//!
//! With the unified diamond-types, operations are handled internally via `SerializedOps`.
//! This module re-exports the relevant types for convenience.

// Re-export key types from diamond-types for sync operations
pub use diamond_types::{SerializedOps, SerializedOpsOwned, LV};

/// Frontier type alias for clarity.
/// A frontier represents a version vector - the set of latest operations seen.
pub type Frontier = Vec<LV>;

#[cfg(test)]
mod tests {
    use diamond_types::{OpLog, CreateValue, Primitive, ROOT_CRDT_ID};

    #[test]
    fn test_ops_sync() {
        // Create two documents
        let mut oplog1 = OpLog::new();
        let mut oplog2 = OpLog::new();

        let alice = oplog1.cg.get_or_create_agent_id("alice");
        let bob = oplog2.cg.get_or_create_agent_id("bob");

        // Alice makes changes
        oplog1.local_map_set(alice, ROOT_CRDT_ID, "key",
            CreateValue::Primitive(Primitive::I64(42)));

        // Sync to Bob - ops_since returns SerializedOps<'_> borrowed from oplog1
        let ops = oplog1.ops_since(&[]);
        oplog2.merge_ops(ops).unwrap();

        // They should converge
        assert_eq!(oplog1.checkout(), oplog2.checkout());

        // Bob makes changes
        oplog2.local_map_set(bob, ROOT_CRDT_ID, "key2",
            CreateValue::Primitive(Primitive::Str("hello".into())));

        // Sync back to Alice
        let ops = oplog2.ops_since(&[]);
        oplog1.merge_ops(ops).unwrap();

        // Should still converge
        assert_eq!(oplog1.checkout(), oplog2.checkout());
    }
}
