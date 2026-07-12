//! SQLite metadata for the semantic index.
//!
//! Maps ContextId <-> HNSW slot, tracks content hashes for change detection.

use std::path::Path;

use kaijutsu_types::ContextId;
use rusqlite::Connection;

use crate::IndexError;

/// SQLite-backed metadata store for the semantic index.
pub struct MetadataStore {
    conn: Connection,
}

impl MetadataStore {
    /// Open or create the metadata database.
    pub fn open(data_dir: &Path) -> Result<Self, IndexError> {
        let db_path = data_dir.join("index_meta.db");
        let conn =
            Connection::open(&db_path).map_err(|e| IndexError::Database(format!("open: {}", e)))?;

        // WAL mode allows concurrent readers + writer without SQLITE_BUSY on reads.
        // busy_timeout retries on lock contention instead of failing immediately.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;",
        )
        .map_err(|e| IndexError::Database(format!("pragmas: {}", e)))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS index_entries (
                context_id BLOB PRIMARY KEY,
                hnsw_slot INTEGER NOT NULL UNIQUE,
                content_hash TEXT NOT NULL,
                dimensions INTEGER NOT NULL,
                model_name TEXT NOT NULL,
                embedded_at INTEGER NOT NULL
            );",
        )
        .map_err(|e| IndexError::Database(format!("create table: {}", e)))?;

        // Monotonic slot allocator. Slots must NEVER be reused: hnsw_rs can't
        // delete points, so an evicted slot's vector stays in the graph until
        // rebuild — reallocating its number would put two graph points behind
        // one DataId and let search attribute the dead vector to the new
        // context. MAX(hnsw_slot)+1 breaks exactly when the highest slot is
        // evicted; a persistent high-water mark can't. Seeded from existing
        // rows on first open after upgrade (INSERT OR IGNORE = migration).
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS slot_allocator (
                id INTEGER PRIMARY KEY CHECK (id = 0),
                next_slot INTEGER NOT NULL
            );
            INSERT OR IGNORE INTO slot_allocator (id, next_slot)
                VALUES (0, COALESCE((SELECT MAX(hnsw_slot) + 1 FROM index_entries), 0));",
        )
        .map_err(|e| IndexError::Database(format!("create slot_allocator: {}", e)))?;

        Ok(Self { conn })
    }

    /// Get the HNSW slot for a context.
    pub fn get_slot(&self, ctx: ContextId) -> Result<Option<u32>, IndexError> {
        let mut stmt = self
            .conn
            .prepare("SELECT hnsw_slot FROM index_entries WHERE context_id = ?1")
            .map_err(|e| IndexError::Database(e.to_string()))?;

        let result = stmt
            .query_row([ctx.as_bytes().as_slice()], |row| row.get::<_, u32>(0))
            .optional()
            .map_err(|e| IndexError::Database(e.to_string()))?;

        Ok(result)
    }

    /// Get the content hash for a context.
    pub fn get_content_hash(&self, ctx: ContextId) -> Result<Option<String>, IndexError> {
        let mut stmt = self
            .conn
            .prepare("SELECT content_hash FROM index_entries WHERE context_id = ?1")
            .map_err(|e| IndexError::Database(e.to_string()))?;

        let result = stmt
            .query_row([ctx.as_bytes().as_slice()], |row| row.get::<_, String>(0))
            .optional()
            .map_err(|e| IndexError::Database(e.to_string()))?;

        Ok(result)
    }

    /// Get the context ID for an HNSW slot.
    pub fn get_context_id(&self, slot: u32) -> Result<Option<ContextId>, IndexError> {
        let mut stmt = self
            .conn
            .prepare("SELECT context_id FROM index_entries WHERE hnsw_slot = ?1")
            .map_err(|e| IndexError::Database(e.to_string()))?;

        let result = stmt
            .query_row([slot], |row| {
                let bytes: Vec<u8> = row.get(0)?;
                Ok(bytes)
            })
            .optional()
            .map_err(|e| IndexError::Database(e.to_string()))?;

        match result {
            Some(bytes) => {
                if bytes.len() == 16 {
                    let arr: [u8; 16] = bytes.try_into().unwrap();
                    Ok(Some(ContextId::from_bytes(arr)))
                } else {
                    Ok(None)
                }
            }
            None => Ok(None),
        }
    }

    /// Assign a slot for a context (create or update).
    ///
    /// Returns the slot number. The check-then-insert is wrapped in a
    /// transaction so the slot allocation is atomic.
    pub fn assign_slot(
        &mut self,
        ctx: ContextId,
        content_hash: &str,
        model_name: &str,
        dimensions: usize,
    ) -> Result<u32, IndexError> {
        let tx = self
            .conn
            .transaction()
            .map_err(|e| IndexError::Database(format!("begin transaction: {}", e)))?;

        // Check if we already have a slot
        let existing_slot: Option<u32> = tx
            .prepare("SELECT hnsw_slot FROM index_entries WHERE context_id = ?1")
            .and_then(|mut stmt| {
                stmt.query_row([ctx.as_bytes().as_slice()], |row| row.get::<_, u32>(0))
                    .optional()
            })
            .map_err(|e| IndexError::Database(e.to_string()))?;

        if let Some(slot) = existing_slot {
            // Update the hash
            tx.execute(
                "UPDATE index_entries SET content_hash = ?1, embedded_at = ?2 WHERE context_id = ?3",
                rusqlite::params![
                    content_hash,
                    now_millis(),
                    ctx.as_bytes().as_slice(),
                ],
            )
            .map_err(|e| IndexError::Database(e.to_string()))?;
            tx.commit()
                .map_err(|e| IndexError::Database(format!("commit: {}", e)))?;
            return Ok(slot);
        }

        // Allocate the next slot from the monotonic high-water mark — never
        // from MAX(hnsw_slot), which regresses when the highest slot is
        // evicted (see the slot_allocator comment in `open`).
        let next_slot: u32 = tx
            .query_row("SELECT next_slot FROM slot_allocator WHERE id = 0", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(|e| IndexError::Database(e.to_string()))? as u32;
        tx.execute(
            "UPDATE slot_allocator SET next_slot = next_slot + 1 WHERE id = 0",
            [],
        )
        .map_err(|e| IndexError::Database(e.to_string()))?;

        tx.execute(
            "INSERT INTO index_entries (context_id, hnsw_slot, content_hash, dimensions, model_name, embedded_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                ctx.as_bytes().as_slice(),
                next_slot,
                content_hash,
                dimensions as i64,
                model_name,
                now_millis(),
            ],
        )
        .map_err(|e| IndexError::Database(e.to_string()))?;

        tx.commit()
            .map_err(|e| IndexError::Database(format!("commit: {}", e)))?;
        Ok(next_slot)
    }

    /// Remove a context from the index metadata.
    pub fn remove(&mut self, ctx: ContextId) -> Result<(), IndexError> {
        self.conn
            .execute(
                "DELETE FROM index_entries WHERE context_id = ?1",
                [ctx.as_bytes().as_slice()],
            )
            .map_err(|e| IndexError::Database(e.to_string()))?;
        Ok(())
    }

    /// List all (hnsw_slot, context_id) pairs, ordered by slot.
    ///
    /// Used by `rebuild()` to know which slots are still live in metadata —
    /// the source of truth for what should survive into the fresh graph.
    pub fn all_slots(&self) -> Result<Vec<(u32, ContextId)>, IndexError> {
        let mut stmt = self
            .conn
            .prepare("SELECT hnsw_slot, context_id FROM index_entries ORDER BY hnsw_slot")
            .map_err(|e| IndexError::Database(e.to_string()))?;

        let rows = stmt
            .query_map([], |row| {
                let slot: u32 = row.get(0)?;
                let bytes: Vec<u8> = row.get(1)?;
                Ok((slot, bytes))
            })
            .map_err(|e| IndexError::Database(e.to_string()))?;

        let mut result = Vec::new();
        for row in rows {
            let (slot, bytes) = row.map_err(|e| IndexError::Database(e.to_string()))?;
            if bytes.len() == 16 {
                let arr: [u8; 16] = bytes.try_into().unwrap();
                result.push((slot, ContextId::from_bytes(arr)));
            }
        }

        Ok(result)
    }

    /// List all indexed context IDs.
    pub fn all_context_ids(&self) -> Result<Vec<ContextId>, IndexError> {
        let mut stmt = self
            .conn
            .prepare("SELECT context_id FROM index_entries ORDER BY hnsw_slot")
            .map_err(|e| IndexError::Database(e.to_string()))?;

        let rows = stmt
            .query_map([], |row| {
                let bytes: Vec<u8> = row.get(0)?;
                Ok(bytes)
            })
            .map_err(|e| IndexError::Database(e.to_string()))?;

        let mut ids = Vec::new();
        for row in rows {
            let bytes = row.map_err(|e| IndexError::Database(e.to_string()))?;
            if bytes.len() == 16 {
                let arr: [u8; 16] = bytes.try_into().unwrap();
                ids.push(ContextId::from_bytes(arr));
            }
        }

        Ok(ids)
    }

    /// Number of indexed entries.
    pub fn count(&self) -> Result<usize, IndexError> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM index_entries", [], |row| row.get(0))
            .map_err(|e| IndexError::Database(e.to_string()))?;
        Ok(count as usize)
    }

    /// Evict the `n` oldest entries by `embedded_at` timestamp.
    ///
    /// Returns the HNSW slot numbers of the evicted rows (ascending by
    /// `embedded_at`, i.e. oldest first) so the caller can clear them from the
    /// HNSW embeddings cache (`HnswIndex::clear_slot`). Select-then-delete
    /// inside one transaction, rather than relying on `RETURNING`, so the set
    /// of rows selected is exactly the set deleted.
    ///
    /// Note: HNSW does not support point deletion — evicted slots become dead
    /// weight in the graph until a full `rebuild()` is performed.
    pub fn evict_oldest(&mut self, n: usize) -> Result<Vec<u32>, IndexError> {
        let tx = self
            .conn
            .transaction()
            .map_err(|e| IndexError::Database(format!("begin transaction: {}", e)))?;

        let slots: Vec<u32> = {
            let mut stmt = tx
                .prepare("SELECT hnsw_slot FROM index_entries ORDER BY embedded_at ASC LIMIT ?1")
                .map_err(|e| IndexError::Database(e.to_string()))?;
            let rows = stmt
                .query_map([n as i64], |row| row.get::<_, u32>(0))
                .map_err(|e| IndexError::Database(e.to_string()))?;
            rows.collect::<Result<Vec<u32>, _>>()
                .map_err(|e| IndexError::Database(e.to_string()))?
        };

        // Delete exactly the slots selected above — re-running the ORDER BY
        // subquery could pick a different set when embedded_at ties (same
        // millisecond), and the returned slots must match the deleted rows.
        // Slot numbers are integers we just read back, so inlining is safe.
        if !slots.is_empty() {
            let in_list = slots
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(",");
            tx.execute(
                &format!("DELETE FROM index_entries WHERE hnsw_slot IN ({in_list})"),
                [],
            )
            .map_err(|e| IndexError::Database(format!("evict_oldest: {}", e)))?;
        }

        tx.commit()
            .map_err(|e| IndexError::Database(format!("commit: {}", e)))?;

        Ok(slots)
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// Bring in rusqlite Optional extension
use rusqlite::OptionalExtension;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_assign_and_get_slot() {
        let dir = TempDir::new().unwrap();
        let mut store = MetadataStore::open(dir.path()).unwrap();

        let ctx = ContextId::new();
        let slot = store.assign_slot(ctx, "abc123", "test-model", 384).unwrap();
        assert_eq!(slot, 0);

        let retrieved = store.get_slot(ctx).unwrap();
        assert_eq!(retrieved, Some(0));
    }

    #[test]
    fn test_sequential_slots() {
        let dir = TempDir::new().unwrap();
        let mut store = MetadataStore::open(dir.path()).unwrap();

        let ctx1 = ContextId::new();
        let ctx2 = ContextId::new();
        let ctx3 = ContextId::new();

        let s1 = store.assign_slot(ctx1, "h1", "model", 384).unwrap();
        let s2 = store.assign_slot(ctx2, "h2", "model", 384).unwrap();
        let s3 = store.assign_slot(ctx3, "h3", "model", 384).unwrap();

        assert_eq!(s1, 0);
        assert_eq!(s2, 1);
        assert_eq!(s3, 2);
    }

    #[test]
    fn test_content_hash_check() {
        let dir = TempDir::new().unwrap();
        let mut store = MetadataStore::open(dir.path()).unwrap();

        let ctx = ContextId::new();
        store.assign_slot(ctx, "hash1", "model", 384).unwrap();

        assert_eq!(
            store.get_content_hash(ctx).unwrap(),
            Some("hash1".to_string())
        );

        // Update
        store.assign_slot(ctx, "hash2", "model", 384).unwrap();
        assert_eq!(
            store.get_content_hash(ctx).unwrap(),
            Some("hash2".to_string())
        );
    }

    #[test]
    fn test_reverse_lookup() {
        let dir = TempDir::new().unwrap();
        let mut store = MetadataStore::open(dir.path()).unwrap();

        let ctx = ContextId::new();
        let slot = store.assign_slot(ctx, "h", "model", 384).unwrap();

        let retrieved_ctx = store.get_context_id(slot).unwrap();
        assert_eq!(retrieved_ctx, Some(ctx));
    }

    #[test]
    fn test_count() {
        let dir = TempDir::new().unwrap();
        let mut store = MetadataStore::open(dir.path()).unwrap();

        assert_eq!(store.count().unwrap(), 0);

        store
            .assign_slot(ContextId::new(), "h1", "model", 384)
            .unwrap();
        store
            .assign_slot(ContextId::new(), "h2", "model", 384)
            .unwrap();

        assert_eq!(store.count().unwrap(), 2);
    }

    #[test]
    fn test_evict_oldest() {
        let dir = TempDir::new().unwrap();
        let mut store = MetadataStore::open(dir.path()).unwrap();

        let ctx1 = ContextId::new();
        let ctx2 = ContextId::new();
        let ctx3 = ContextId::new();

        // Insert with increasing timestamps (assign_slot uses now_millis)
        let s1 = store.assign_slot(ctx1, "h1", "model", 384).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let s2 = store.assign_slot(ctx2, "h2", "model", 384).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        store.assign_slot(ctx3, "h3", "model", 384).unwrap();

        assert_eq!(store.count().unwrap(), 3);

        // Evict the 2 oldest — must return their slot numbers, oldest first.
        let evicted = store.evict_oldest(2).unwrap();
        assert_eq!(evicted, vec![s1, s2]);
        assert_eq!(store.count().unwrap(), 1);

        // ctx3 (newest) should remain
        assert!(store.get_slot(ctx3).unwrap().is_some());
        assert!(store.get_slot(ctx1).unwrap().is_none());
        assert!(store.get_slot(ctx2).unwrap().is_none());
    }

    #[test]
    fn test_evict_oldest_more_than_available() {
        let dir = TempDir::new().unwrap();
        let mut store = MetadataStore::open(dir.path()).unwrap();

        let slot = store
            .assign_slot(ContextId::new(), "h1", "model", 384)
            .unwrap();
        assert_eq!(store.count().unwrap(), 1);

        let evicted = store.evict_oldest(10).unwrap();
        assert_eq!(evicted, vec![slot]);
        assert_eq!(store.count().unwrap(), 0);
    }

    /// Slot numbers must never be reused, even after the highest slot is
    /// evicted. Reuse would collide with the dead point still in the HNSW
    /// graph (hnsw_rs can't delete): two graph points sharing a DataId, and
    /// search attributing the dead vector to the new context.
    #[test]
    fn test_evicted_max_slot_is_not_reused() {
        let dir = TempDir::new().unwrap();
        let mut store = MetadataStore::open(dir.path()).unwrap();

        let ctx_old_max = ContextId::new();
        let ctx_newer = ContextId::new();

        // ctx_old_max gets the highest slot but the OLDEST embedded_at
        // (indexed once, long ago); ctx_newer holds a lower slot with a
        // fresher timestamp (re-embedded since).
        store.assign_slot(ctx_newer, "h1", "model", 384).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let max_slot = store.assign_slot(ctx_old_max, "h2", "model", 384).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        // Re-embed ctx_newer: keeps its slot, refreshes embedded_at.
        store.assign_slot(ctx_newer, "h1b", "model", 384).unwrap();

        // Evicting one drops ctx_old_max — the max slot leaves metadata.
        let evicted = store.evict_oldest(1).unwrap();
        assert_eq!(evicted, vec![max_slot]);

        let fresh = store
            .assign_slot(ContextId::new(), "h3", "model", 384)
            .unwrap();
        assert!(
            fresh > max_slot,
            "slot {fresh} reuses evicted slot {max_slot} — collides with the dead graph point"
        );
    }

    /// The allocator's high-water mark must survive reopen (it lives in
    /// SQLite, not memory).
    #[test]
    fn test_slot_allocator_survives_reopen() {
        let dir = TempDir::new().unwrap();
        let max_slot;
        {
            let mut store = MetadataStore::open(dir.path()).unwrap();
            store
                .assign_slot(ContextId::new(), "h1", "model", 384)
                .unwrap();
            max_slot = store
                .assign_slot(ContextId::new(), "h2", "model", 384)
                .unwrap();
            // Empty the table entirely — MAX(hnsw_slot) has nothing to say.
            store.evict_oldest(10).unwrap();
        }

        let mut store = MetadataStore::open(dir.path()).unwrap();
        let fresh = store
            .assign_slot(ContextId::new(), "h3", "model", 384)
            .unwrap();
        assert!(
            fresh > max_slot,
            "slot {fresh} reuses slot {max_slot} after reopen of an emptied table"
        );
    }

    #[test]
    fn test_all_slots_ordering() {
        let dir = TempDir::new().unwrap();
        let mut store = MetadataStore::open(dir.path()).unwrap();

        let ctx1 = ContextId::new();
        let ctx2 = ContextId::new();
        let ctx3 = ContextId::new();

        let s1 = store.assign_slot(ctx1, "h1", "model", 384).unwrap();
        let s2 = store.assign_slot(ctx2, "h2", "model", 384).unwrap();
        let s3 = store.assign_slot(ctx3, "h3", "model", 384).unwrap();

        let all = store.all_slots().unwrap();
        assert_eq!(all, vec![(s1, ctx1), (s2, ctx2), (s3, ctx3)]);
    }

    #[test]
    fn test_all_slots_empty() {
        let dir = TempDir::new().unwrap();
        let store = MetadataStore::open(dir.path()).unwrap();
        assert_eq!(store.all_slots().unwrap(), vec![]);
    }

    #[test]
    fn test_remove() {
        let dir = TempDir::new().unwrap();
        let mut store = MetadataStore::open(dir.path()).unwrap();

        let ctx = ContextId::new();
        store.assign_slot(ctx, "h", "model", 384).unwrap();
        assert_eq!(store.count().unwrap(), 1);

        store.remove(ctx).unwrap();
        assert_eq!(store.count().unwrap(), 0);
        assert_eq!(store.get_slot(ctx).unwrap(), None);
    }
}
