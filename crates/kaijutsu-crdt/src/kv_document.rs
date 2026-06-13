//! Flat key→string Map CRDT for the kernel key–value store.
//!
//! Where [`BlockDocument`](crate::BlockDocument) is a block-*log* document (an
//! OR-Set of ordered block ids plus per-block sub-maps), `KvDocument` is the
//! degenerate case: a single root [`diamond_types_extended`] Map of flat
//! `key → String` registers with last-write-wins semantics. It carries no
//! schema — values are opaque UTF-8 strings; the kernel KV layer is what wraps
//! them in a versioned envelope.
//!
//! It reuses the same sync primitives as `BlockDocument` ([`ops_since`],
//! [`merge_ops_owned`], snapshot/restore) so the kernel can journal it to the
//! KernelDb oplog and rebuild it on cold start with the exact machinery the
//! block store already uses.
//!
//! [`ops_since`]: KvDocument::ops_since
//! [`merge_ops_owned`]: KvDocument::merge_ops_owned

use diamond_types_extended::{AgentId, Document, Frontier, SerializedOpsOwned, Uuid};

use crate::{CrdtError, PrincipalId, Result};

/// A flat `key → String` LWW Map backed by a single DTE `Document`.
///
/// Deletion is a Nil tombstone: the register lingers in the underlying Map (so
/// the op converges and the key can be resurrected by a later `set`), but
/// [`get`](KvDocument::get) reports it absent and [`keys`](KvDocument::keys)
/// omits it.
pub struct KvDocument {
    /// Facet Document holding the root Map.
    doc: Document,
    /// Diamond-types actor id for ops authored through this handle.
    agent: AgentId,
}

impl KvDocument {
    /// Create an empty store stamped with `principal_id` as the CRDT actor.
    pub fn new(principal_id: PrincipalId) -> Self {
        let mut doc = Document::new();
        let agent = doc.create_agent(Uuid::from_bytes(*principal_id.as_bytes()));
        Self { doc, agent }
    }

    /// Set `key` to `value` (last-write-wins). Resurrects a deleted key.
    pub fn set(&mut self, key: &str, value: &str) {
        self.doc.transact(self.agent, |tx| {
            tx.root().set(key, value);
        });
    }

    /// Read the live string value for `key`, or `None` if absent or deleted.
    pub fn get(&self, key: &str) -> Option<String> {
        self.doc
            .root()
            .get(key)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
    }

    /// Tombstone `key`. Returns whether a live value existed before the delete.
    pub fn delete(&mut self, key: &str) -> bool {
        let existed = self.get(key).is_some();
        self.doc.transact(self.agent, |tx| {
            tx.root().set_nil(key);
        });
        existed
    }

    /// All live keys (Nil tombstones excluded), unordered.
    ///
    /// `map_keys` returns an owned `Vec`, so the per-key `get` below does not
    /// re-borrow the document mid-iteration — sidestepping the DTE v0.2
    /// re-entrant-lock hazard that `block_ids_ordered` documents.
    pub fn keys(&self) -> Vec<String> {
        self.doc
            .map_keys(&[])
            .unwrap_or_default()
            .into_iter()
            .filter(|k| self.get(k).is_some())
            .collect()
    }

    /// Number of live keys.
    pub fn len(&self) -> usize {
        self.keys().len()
    }

    /// Whether the store holds no live keys.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    // ── Sync ────────────────────────────────────────────────────────────────

    /// Current CRDT frontier (version), for incremental journaling.
    pub fn frontier(&self) -> Frontier {
        self.doc.version().clone()
    }

    /// Operations since `frontier`, owned for journaling / cross-thread use.
    pub fn ops_since(&self, frontier: &Frontier) -> SerializedOpsOwned {
        self.doc.ops_since_owned(frontier)
    }

    /// Merge remote operations, converging this store toward the sender.
    ///
    /// Wrapped in `catch_unwind` to turn a DTE causalgraph panic into an error
    /// rather than tearing down the kernel — the same defensive posture
    /// `BlockDocument::merge_ops_owned` takes.
    pub fn merge_ops_owned(&mut self, ops: SerializedOpsOwned) -> Result<()> {
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.doc.merge_ops(ops)));
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(CrdtError::Internal(format!("kv merge error: {e:?}"))),
            Err(_) => Err(CrdtError::Internal(
                "kv CRDT merge panicked — likely concurrent causalgraph bug in DTE".into(),
            )),
        }
    }

    // ── Snapshot / restore ────────────────────────────────────────────────────

    /// Materialize live `(key, value)` pairs for a compaction snapshot, sorted
    /// by key.
    ///
    /// The sort matters: [`from_snapshot`](Self::from_snapshot) re-authors fresh
    /// CRDT ops in iteration order, so a snapshot rebuilt at compaction time and
    /// the same snapshot rebuilt on cold start must see identical input order to
    /// assign identical local versions — otherwise post-snapshot ops journaled
    /// against the live (compacted) doc would reference versions a differently
    /// ordered rebuild never produced (`DataMissing` on replay).
    pub fn snapshot(&self) -> Vec<(String, String)> {
        let mut entries: Vec<(String, String)> = self
            .keys()
            .into_iter()
            .filter_map(|k| self.get(&k).map(|v| (k, v)))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    /// Rebuild a store from a snapshot's live pairs.
    ///
    /// Tombstones are not carried: a snapshot is the live materialization, and
    /// the oplog that referenced the deleted keys has been truncated past them.
    pub fn from_snapshot(entries: Vec<(String, String)>, principal_id: PrincipalId) -> Self {
        let mut doc = Self::new(principal_id);
        for (k, v) in entries {
            doc.set(&k, &v);
        }
        doc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> KvDocument {
        KvDocument::new(PrincipalId::new())
    }

    #[test]
    fn set_get_roundtrip() {
        let mut kv = doc();
        kv.set("a.b", "hello");
        assert_eq!(kv.get("a.b").as_deref(), Some("hello"));
        assert_eq!(kv.get("missing"), None);
    }

    #[test]
    fn overwrite_is_last_write_wins() {
        let mut kv = doc();
        kv.set("k", "one");
        kv.set("k", "two");
        assert_eq!(kv.get("k").as_deref(), Some("two"));
        assert_eq!(kv.len(), 1);
    }

    #[test]
    fn delete_hides_key_but_reports_prior_existence() {
        let mut kv = doc();
        kv.set("k", "v");
        assert!(kv.delete("k"));
        assert_eq!(kv.get("k"), None);
        assert!(!kv.keys().contains(&"k".to_string()));
        // second delete sees nothing live
        assert!(!kv.delete("k"));
    }

    #[test]
    fn deleted_key_can_be_resurrected() {
        let mut kv = doc();
        kv.set("k", "v1");
        kv.delete("k");
        kv.set("k", "v2");
        assert_eq!(kv.get("k").as_deref(), Some("v2"));
        assert!(kv.keys().contains(&"k".to_string()));
    }

    #[test]
    fn keys_lists_only_live() {
        let mut kv = doc();
        kv.set("a", "1");
        kv.set("b", "2");
        kv.set("c", "3");
        kv.delete("b");
        let mut keys = kv.keys();
        keys.sort();
        assert_eq!(keys, vec!["a".to_string(), "c".to_string()]);
    }

    #[test]
    fn concurrent_writes_converge() {
        let p1 = PrincipalId::new();
        let p2 = PrincipalId::new();
        let mut a = KvDocument::new(p1);
        let mut b = KvDocument::new(p2);

        // Disjoint keys authored concurrently on each replica.
        a.set("from.a", "alpha");
        b.set("from.b", "beta");

        let a_ops = a.ops_since(&Frontier::root());
        let b_ops = b.ops_since(&Frontier::root());
        a.merge_ops_owned(b_ops).unwrap();
        b.merge_ops_owned(a_ops).unwrap();

        for kv in [&a, &b] {
            assert_eq!(kv.get("from.a").as_deref(), Some("alpha"));
            assert_eq!(kv.get("from.b").as_deref(), Some("beta"));
        }
    }

    #[test]
    fn incremental_ops_since_frontier() {
        let mut kv = doc();
        kv.set("first", "1");
        let f = kv.frontier();

        // Follower catches up to the frontier with the full history so far.
        let mut follower = KvDocument::new(PrincipalId::new());
        follower
            .merge_ops_owned(kv.ops_since(&Frontier::root()))
            .unwrap();
        assert_eq!(follower.get("first").as_deref(), Some("1"));

        // Author past the frontier; ship only the delta.
        kv.set("second", "2");
        follower.merge_ops_owned(kv.ops_since(&f)).unwrap();

        assert_eq!(follower.get("first").as_deref(), Some("1"));
        assert_eq!(follower.get("second").as_deref(), Some("2"));
    }

    #[test]
    fn snapshot_roundtrip_preserves_live_drops_deleted() {
        let mut kv = doc();
        kv.set("keep", "v");
        kv.set("gone", "x");
        kv.delete("gone");

        let snap = kv.snapshot();
        assert_eq!(snap, vec![("keep".to_string(), "v".to_string())]);

        let restored = KvDocument::from_snapshot(snap, PrincipalId::new());
        assert_eq!(restored.get("keep").as_deref(), Some("v"));
        assert_eq!(restored.get("gone"), None);
        assert_eq!(restored.len(), 1);
    }
}
