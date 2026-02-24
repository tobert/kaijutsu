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
        let conn = Connection::open(&db_path)
            .map_err(|e| IndexError::Database(format!("open: {}", e)))?;

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

        Ok(Self { conn })
    }

    /// Get the HNSW slot for a context.
    pub fn get_slot(&self, ctx: ContextId) -> Result<Option<u32>, IndexError> {
        let mut stmt = self
            .conn
            .prepare("SELECT hnsw_slot FROM index_entries WHERE context_id = ?1")
            .map_err(|e| IndexError::Database(e.to_string()))?;

        let result = stmt
            .query_row([ctx.as_bytes().as_slice()], |row| {
                row.get::<_, u32>(0)
            })
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
            .query_row([ctx.as_bytes().as_slice()], |row| {
                row.get::<_, String>(0)
            })
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
    /// Returns the slot number.
    pub fn assign_slot(
        &mut self,
        ctx: ContextId,
        content_hash: &str,
        model_name: &str,
        dimensions: usize,
    ) -> Result<u32, IndexError> {
        // Check if we already have a slot
        if let Some(existing_slot) = self.get_slot(ctx)? {
            // Update the hash
            self.conn
                .execute(
                    "UPDATE index_entries SET content_hash = ?1, embedded_at = ?2 WHERE context_id = ?3",
                    rusqlite::params![
                        content_hash,
                        now_millis(),
                        ctx.as_bytes().as_slice(),
                    ],
                )
                .map_err(|e| IndexError::Database(e.to_string()))?;
            return Ok(existing_slot);
        }

        // Allocate next slot
        let next_slot = self.next_slot()?;

        self.conn
            .execute(
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

    /// Get the next available slot number.
    fn next_slot(&self) -> Result<u32, IndexError> {
        let max: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(hnsw_slot) FROM index_entries",
                [],
                |row| row.get(0),
            )
            .map_err(|e| IndexError::Database(e.to_string()))?;

        Ok(max.map(|m| m as u32 + 1).unwrap_or(0))
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

        assert_eq!(store.get_content_hash(ctx).unwrap(), Some("hash1".to_string()));

        // Update
        store.assign_slot(ctx, "hash2", "model", 384).unwrap();
        assert_eq!(store.get_content_hash(ctx).unwrap(), Some("hash2".to_string()));
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

        store.assign_slot(ContextId::new(), "h1", "model", 384).unwrap();
        store.assign_slot(ContextId::new(), "h2", "model", 384).unwrap();

        assert_eq!(store.count().unwrap(), 2);
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
