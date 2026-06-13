//! Kernel key–value store — persistent, CRDT-backed shell variables.
//!
//! A small, durable, collaborative `key → String` store: flat UTF-8 keys, string
//! values, multi-writer, real-time-synced, SQLite-backed. Think of it as the
//! kernel's `env`, but persistent across restarts and shared across every peer
//! and rc script attached to the kernel. See `docs/kernel-kv.md`.
//!
//! ## Shape
//!
//! - The live source of truth is a [`KvDocument`] — a diamond-types-extended
//!   `Map` CRDT (last-write-wins per key).
//! - Each value rides in a small **versioned envelope** ([`Envelope`]) so future
//!   capabilities (CAS, real TTL, structured values) land with no data
//!   migration. The envelope is stored as JSON in the CRDT register because DTE
//!   registers are typed and hold no raw bytes — JSON keeps the value
//!   inspectable and diffable, which the design wants anyway.
//! - Writes mutate the Map in memory, **journal to the KernelDb oplog** in real
//!   time, broadcast to `watch` subscribers, and **compact to a SQLite
//!   snapshot** on a churn-tuned op-count cadence — the exact machinery the
//!   block store uses.
//! - Persistence is **fail-loud**: a store that declares persistence with no DB
//!   handle returns [`KvError::NoDatabaseConfigured`] rather than dropping a
//!   write.
//!
//! ## What this is not
//!
//! There is no per-key ACL (single-user, shared-trust kernel — caps are
//! ergonomic nudges), no hierarchy in the type (dotted keys are convention), and
//! no sweeper for TTL (v1 `expires_at` is advisory, checked best-effort on read).

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use kaijutsu_crdt::{KvDocument, SerializedOpsOwned};
use kaijutsu_types::{ContextId, DocKind, PrincipalId, codec, now_millis};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::block_store::DbHandle;
use crate::kernel_db::DocumentRow;

/// Hard per-value size cap, enforced at `set`. Without it the KV becomes a
/// backdoor blob store that bypasses the content-addressed DAG and bloats the
/// oplog. Large/structured blobs belong in the CAS or a document.
pub const MAX_VALUE_BYTES: usize = 64 * 1024;

/// Current envelope schema version. Readers dispatch on this; bumping it lets
/// new keys take a new shape while old keys stay readable — no migration.
const ENVELOPE_V1: u8 = 1;

/// Ops journaled since the last snapshot before we compact. KV keys are
/// overwrite-heavy (a client rewrites `current_context` on every switch), so
/// this is tuned lower than the append-only block log's 500 — at ~100 small
/// keys a snapshot is a few KB of CBOR, cheap to take often.
const COMPACTION_OP_THRESHOLD: u64 = 200;

/// The reserved well-known document id the KV store journals under, derived
/// deterministically so every kernel agrees on it without coordination.
pub fn root_context_id() -> ContextId {
    let uuid = Uuid::new_v5(&Uuid::NAMESPACE_URL, b"kaijutsu:kv:root");
    ContextId::from_bytes(*uuid.as_bytes())
}

/// Structured error for KV operations.
#[derive(Debug, thiserror::Error)]
pub enum KvError {
    #[error("value exceeds {MAX_VALUE_BYTES} byte cap: {0} bytes")]
    ValueTooLarge(usize),

    #[error("no database configured")]
    NoDatabaseConfigured,

    #[error("corrupt envelope for key {key:?}: {source}")]
    Corrupt {
        key: String,
        source: serde_json::Error,
    },

    #[error("crdt error: {0}")]
    Crdt(#[from] kaijutsu_crdt::CrdtError),

    #[error("database error: {0}")]
    Db(String),

    #[error("codec error: {0}")]
    Codec(String),
}

pub type KvResult<T> = Result<T, KvError>;

/// The versioned value envelope. Every value is stored as one of these, JSON in
/// the CRDT register. v1 uses almost none of the forward-proofing — `cas_token`
/// rides along as 0, `expires_at` is advisory — but shipping them now means
/// compare-and-swap and leases land later with zero envelope migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Envelope {
    /// Envelope schema version — the evolution hatch.
    v: u8,
    /// The caller's value (structured data is the caller's JSON).
    value: String,
    /// Writer's wall clock at `set` (ms epoch), informational.
    written_at: i64,
    /// Advisory absolute expiry on the writer's clock (ms). Best-effort: no
    /// sweeper, readers MAY treat as gone. Omitted from JSON when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<i64>,
    /// 0 = unconditional. Non-zero enables compare-and-swap (reserved for v1).
    #[serde(default)]
    cas_token: u64,
}

/// A single observed change, delivered to `watch` subscribers.
#[derive(Debug, Clone)]
pub struct KvChange {
    pub key: String,
    /// The new value, or `None` for a delete.
    pub value: Option<String>,
}

/// The kernel key–value store.
pub struct Kv {
    /// Live CRDT state.
    doc: RwLock<KvDocument>,
    /// Reserved document id this store journals under.
    context_id: ContextId,
    /// CRDT actor / row author for this store.
    principal: PrincipalId,
    /// Backing database, if persistent.
    db: Option<DbHandle>,
    /// When true, a missing `db` is a hard error rather than a silent skip.
    persistent: bool,
    /// Next oplog sequence number (monotonic, 1-based).
    next_seq: AtomicI64,
    /// Ops journaled since the last snapshot.
    uncompacted: AtomicU64,
    /// Whole-store change broadcast. Prefix filtering is the watcher's job in
    /// v1 (watch granularity is deliberately deferred — see docs/kernel-kv.md).
    tx: broadcast::Sender<KvChange>,
}

impl Kv {
    /// An in-memory store with no persistence — writes never journal. For tests
    /// and ephemeral kernels.
    pub fn ephemeral(principal: PrincipalId) -> Self {
        let (tx, _rx) = broadcast::channel(1024);
        Self {
            doc: RwLock::new(KvDocument::new(principal)),
            context_id: root_context_id(),
            principal,
            db: None,
            persistent: false,
            next_seq: AtomicI64::new(0),
            uncompacted: AtomicU64::new(0),
            tx,
        }
    }

    /// A persistent store backed by `db`. Ensures the reserved document row
    /// exists (the handle-implies-row invariant) and rebuilds live state from
    /// the latest snapshot + oplog replay.
    pub fn persistent(db: DbHandle, principal: PrincipalId) -> KvResult<Self> {
        let (tx, _rx) = broadcast::channel(1024);
        let kv = Self {
            doc: RwLock::new(KvDocument::new(principal)),
            context_id: root_context_id(),
            principal,
            db: Some(db),
            persistent: true,
            next_seq: AtomicI64::new(0),
            uncompacted: AtomicU64::new(0),
            tx,
        };
        kv.ensure_document_row()?;
        kv.load_from_db()?;
        Ok(kv)
    }

    /// Subscribe to whole-store changes. Callers filter by prefix.
    pub fn subscribe(&self) -> broadcast::Receiver<KvChange> {
        self.tx.subscribe()
    }

    // ── Reads ────────────────────────────────────────────────────────────────

    /// Read the live value for `key`. Returns `Ok(None)` if absent, deleted, or
    /// advisory-expired. Returns `Err(Corrupt)` if the stored envelope cannot be
    /// decoded — we surface corruption rather than swallow it.
    pub fn get(&self, key: &str) -> KvResult<Option<String>> {
        let Some(raw) = self.doc.read().get(key) else {
            return Ok(None);
        };
        let env: Envelope =
            serde_json::from_str(&raw).map_err(|source| KvError::Corrupt {
                key: key.to_string(),
                source,
            })?;
        if let Some(exp) = env.expires_at
            && now_millis() as i64 >= exp
        {
            return Ok(None); // advisory expiry — best-effort, no sweep
        }
        Ok(Some(env.value))
    }

    /// Live keys matching `prefix` (or all keys when `prefix` is empty),
    /// unordered. `limit`/`cursor` are accepted for forward-compatibility; v1
    /// returns everything with `next_cursor: None`.
    pub fn keys(&self, prefix: Option<&str>, _limit: Option<usize>, _cursor: Option<&str>) -> KeysPage {
        let mut keys: Vec<String> = self
            .doc
            .read()
            .keys()
            .into_iter()
            .filter(|k| prefix.is_none_or(|p| k.starts_with(p)))
            .collect();
        keys.sort();
        KeysPage {
            keys,
            next_cursor: None,
        }
    }

    // ── Writes ───────────────────────────────────────────────────────────────

    /// Set `key` to `value`, optionally with an advisory absolute `expires_at`
    /// (writer-clock ms). Enforces the 64 KB value cap, journals, and broadcasts.
    pub fn set(&self, key: &str, value: &str, expires_at: Option<i64>) -> KvResult<()> {
        if value.len() > MAX_VALUE_BYTES {
            return Err(KvError::ValueTooLarge(value.len()));
        }
        let env = Envelope {
            v: ENVELOPE_V1,
            value: value.to_string(),
            written_at: now_millis() as i64,
            expires_at,
            cas_token: 0,
        };
        let json = serde_json::to_string(&env)
            .map_err(|e| KvError::Codec(format!("envelope encode: {e}")))?;

        let ops = {
            let mut doc = self.doc.write();
            let before = doc.frontier();
            doc.set(key, &json);
            doc.ops_since(&before)
        };
        self.journal(ops)?;
        let _ = self.tx.send(KvChange {
            key: key.to_string(),
            value: Some(value.to_string()),
        });
        Ok(())
    }

    /// Delete `key`. Returns whether a live value existed. Idempotent.
    pub fn delete(&self, key: &str) -> KvResult<bool> {
        let (existed, ops) = {
            let mut doc = self.doc.write();
            let before = doc.frontier();
            let existed = doc.delete(key);
            (existed, doc.ops_since(&before))
        };
        if existed {
            self.journal(ops)?;
            let _ = self.tx.send(KvChange {
                key: key.to_string(),
                value: None,
            });
        }
        Ok(existed)
    }

    // ── Persistence internals ─────────────────────────────────────────────────

    /// `Ok(Some(db))` when journaling can proceed, `Ok(None)` for an ephemeral
    /// store, `Err(NoDatabaseConfigured)` for a persistent store missing its DB.
    fn journaling_db(&self) -> KvResult<Option<&DbHandle>> {
        match self.db.as_ref() {
            Some(db) => Ok(Some(db)),
            None if self.persistent => Err(KvError::NoDatabaseConfigured),
            None => Ok(None),
        }
    }

    /// Append a delta to the oplog and compact past the churn threshold.
    fn journal(&self, ops: SerializedOpsOwned) -> KvResult<()> {
        let Some(db) = self.journaling_db()? else {
            return Ok(());
        };
        let bytes = codec::encode(&ops).map_err(|e| KvError::Codec(e.to_string()))?;
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst) + 1;
        db.lock()
            .append_op(self.context_id, seq, &bytes)
            .map_err(|e| KvError::Db(e.to_string()))?;
        let count = self.uncompacted.fetch_add(1, Ordering::SeqCst) + 1;
        if count >= COMPACTION_OP_THRESHOLD {
            self.compact()?;
        }
        Ok(())
    }

    /// Snapshot live state to SQLite, truncate the oplog up to it, checkpoint
    /// the WAL, and **rebuild the in-memory doc from the same snapshot**.
    ///
    /// The rebuild is load-bearing, not housekeeping: it makes the live doc's
    /// CRDT history identical to what `load_from_db` will deterministically
    /// reproduce from this snapshot on cold start. Without it, ops journaled
    /// after compaction reference the pre-snapshot history and fail replay with
    /// `DataMissing` (the block store rebuilds for the same reason).
    fn compact(&self) -> KvResult<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        let seq = self.next_seq.load(Ordering::SeqCst);
        let snapshot = {
            let mut doc = self.doc.write();
            let snapshot = doc.snapshot();
            *doc = KvDocument::from_snapshot(snapshot.clone(), self.principal);
            snapshot
        };
        let state = codec::encode(&snapshot).map_err(|e| KvError::Codec(e.to_string()))?;
        {
            let mut g = db.lock();
            g.write_snapshot_and_truncate(self.context_id, seq, seq, &state, "")
                .map_err(|e| KvError::Db(e.to_string()))?;
            // Best-effort: a busy checkpoint is non-fatal (see KernelDb::checkpoint).
            let _ = g.checkpoint();
        }
        self.uncompacted.store(0, Ordering::SeqCst);
        Ok(())
    }

    /// Rebuild live state from the latest snapshot + oplog replay. Mirrors the
    /// block store's `load_from_db`: snapshot restore, then replay only the ops
    /// the snapshot didn't already fold in (`seq > base_seq`).
    fn load_from_db(&self) -> KvResult<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        let g = db.lock();
        let (mut doc, base_seq) = match g
            .load_latest_snapshot(self.context_id)
            .map_err(|e| KvError::Db(e.to_string()))?
        {
            Some(row) => {
                let entries: Vec<(String, String)> =
                    codec::decode(&row.state).map_err(|e| KvError::Codec(e.to_string()))?;
                (KvDocument::from_snapshot(entries, self.principal), row.seq)
            }
            None => (KvDocument::new(self.principal), 0),
        };
        let oplog = g
            .load_oplog_since(self.context_id, base_seq)
            .map_err(|e| KvError::Db(e.to_string()))?;
        drop(g);

        let mut max_seq = base_seq;
        for (seq, bytes) in oplog {
            let ops: SerializedOpsOwned =
                codec::decode(&bytes).map_err(|e| KvError::Codec(e.to_string()))?;
            doc.merge_ops_owned(ops)?;
            max_seq = seq;
        }

        *self.doc.write() = doc;
        self.next_seq.store(max_seq, Ordering::SeqCst);
        self.uncompacted.store(0, Ordering::SeqCst);
        Ok(())
    }

    /// Write the reserved document row if absent. Upholds the handle-implies-row
    /// invariant: the registry row exists before the store serves writes.
    fn ensure_document_row(&self) -> KvResult<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        let g = db.lock();
        let workspace_id = g
            .get_or_create_default_workspace(self.principal)
            .map_err(|e| KvError::Db(e.to_string()))?;
        g.insert_document_or_ignore(&DocumentRow {
            document_id: self.context_id,
            workspace_id,
            doc_kind: DocKind::Kv,
            language: None,
            path: None,
            created_at: now_millis() as i64,
            created_by: self.principal,
        })
        .map_err(|e| KvError::Db(e.to_string()))?;
        Ok(())
    }
}

/// A page of keys, cursor-shaped from day one even though v1 never paginates.
#[derive(Debug, Clone)]
pub struct KeysPage {
    pub keys: Vec<String>,
    pub next_cursor: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel_db::KernelDb;
    use std::sync::Arc;

    fn temp_db() -> (DbHandle, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = KernelDb::open(dir.path().join("kernel.db")).expect("open db");
        (Arc::new(parking_lot::Mutex::new(db)), dir)
    }

    #[test]
    fn ephemeral_set_get() {
        let kv = Kv::ephemeral(PrincipalId::system());
        kv.set("client.current_context", "abc", None).unwrap();
        assert_eq!(kv.get("client.current_context").unwrap().as_deref(), Some("abc"));
        assert_eq!(kv.get("nope").unwrap(), None);
    }

    #[test]
    fn value_cap_enforced() {
        let kv = Kv::ephemeral(PrincipalId::system());
        let big = "x".repeat(MAX_VALUE_BYTES + 1);
        assert!(matches!(kv.set("k", &big, None), Err(KvError::ValueTooLarge(_))));
        // Exactly at the cap is allowed.
        let ok = "x".repeat(MAX_VALUE_BYTES);
        assert!(kv.set("k", &ok, None).is_ok());
    }

    #[test]
    fn persistent_store_with_no_db_is_a_hard_error() {
        // Construct a persistent-flagged store by hand with no DB to prove the
        // fail-loud guard (the public constructor always supplies a DB).
        let (tx, _rx) = broadcast::channel(8);
        let kv = Kv {
            doc: RwLock::new(KvDocument::new(PrincipalId::system())),
            context_id: root_context_id(),
            principal: PrincipalId::system(),
            db: None,
            persistent: true,
            next_seq: AtomicI64::new(0),
            uncompacted: AtomicU64::new(0),
            tx,
        };
        assert!(matches!(
            kv.set("k", "v", None),
            Err(KvError::NoDatabaseConfigured)
        ));
    }

    #[test]
    fn delete_is_idempotent_and_reports_prior_existence() {
        let kv = Kv::ephemeral(PrincipalId::system());
        kv.set("k", "v", None).unwrap();
        assert!(kv.delete("k").unwrap());
        assert_eq!(kv.get("k").unwrap(), None);
        assert!(!kv.delete("k").unwrap());
    }

    #[test]
    fn keys_filters_by_prefix_and_sorts() {
        let kv = Kv::ephemeral(PrincipalId::system());
        kv.set("app.a", "1", None).unwrap();
        kv.set("app.b", "2", None).unwrap();
        kv.set("other.c", "3", None).unwrap();
        let page = kv.keys(Some("app."), None, None);
        assert_eq!(page.keys, vec!["app.a".to_string(), "app.b".to_string()]);
        assert!(page.next_cursor.is_none());
        assert_eq!(kv.keys(None, None, None).keys.len(), 3);
    }

    #[test]
    fn advisory_ttl_hides_expired_on_read() {
        let kv = Kv::ephemeral(PrincipalId::system());
        kv.set("soon", "gone", Some(now_millis() as i64 - 1)).unwrap();
        assert_eq!(kv.get("soon").unwrap(), None);
        // But the key still lingers (no sweep) — visible via keys().
        assert!(kv.keys(None, None, None).keys.contains(&"soon".to_string()));
    }

    #[test]
    fn corrupt_envelope_surfaces_not_swallowed() {
        let kv = Kv::ephemeral(PrincipalId::system());
        // Write raw non-envelope JSON directly into the underlying document.
        kv.doc.write().set("bad", "{not valid envelope");
        assert!(matches!(kv.get("bad"), Err(KvError::Corrupt { .. })));
    }

    #[test]
    fn persists_across_reopen() {
        let (db, _dir) = temp_db();
        {
            let kv = Kv::persistent(db.clone(), PrincipalId::system()).unwrap();
            kv.set("survive", "yes", None).unwrap();
            kv.set("temp", "x", None).unwrap();
            kv.delete("temp").unwrap();
        }
        // Reopen against the same DB — state rebuilds from oplog.
        let kv2 = Kv::persistent(db, PrincipalId::system()).unwrap();
        assert_eq!(kv2.get("survive").unwrap().as_deref(), Some("yes"));
        assert_eq!(kv2.get("temp").unwrap(), None);
    }

    #[test]
    fn survives_compaction_and_reopen() {
        let (db, _dir) = temp_db();
        {
            let kv = Kv::persistent(db.clone(), PrincipalId::system()).unwrap();
            // Churn one key past the compaction threshold, then write a survivor.
            for i in 0..(COMPACTION_OP_THRESHOLD + 20) {
                kv.set("churn", &i.to_string(), None).unwrap();
            }
            kv.set("survivor", "kept", None).unwrap();
            assert_eq!(kv.get("churn").unwrap().as_deref(), Some(&(COMPACTION_OP_THRESHOLD + 19).to_string()[..]));
        }
        let kv2 = Kv::persistent(db, PrincipalId::system()).unwrap();
        assert_eq!(kv2.get("survivor").unwrap().as_deref(), Some("kept"));
        assert_eq!(
            kv2.get("churn").unwrap().as_deref(),
            Some(&(COMPACTION_OP_THRESHOLD + 19).to_string()[..])
        );
    }

    #[test]
    fn watch_observes_set_and_delete() {
        let kv = Kv::ephemeral(PrincipalId::system());
        let mut rx = kv.subscribe();
        kv.set("k", "v", None).unwrap();
        kv.delete("k").unwrap();
        let first = rx.try_recv().unwrap();
        assert_eq!(first.key, "k");
        assert_eq!(first.value.as_deref(), Some("v"));
        let second = rx.try_recv().unwrap();
        assert_eq!(second.value, None);
    }
}
