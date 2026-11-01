//! Client views and dirty file buffers.

use super::*;

/// One row of `dirty_file_buffers`: an unflushed edit's swap-file marker.
/// See `docs/file-buffers.md` for what a row means and who clears it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyFileBuffer {
    pub path: String,
    pub context_id: ContextId,
    pub loaded_generation: Option<u64>,
    pub dirtied_at: i64,
}

pub(super) fn read_dirty_file_buffer(row: &rusqlite::Row<'_>) -> SqliteResult<DirtyFileBuffer> {
    let path: String = row.get(0)?;
    let context_id = read_context_id(row, 1)?;
    // The CHECK constraint on `loaded_generation` rejects a negative value at
    // write time, so this cast is safe for any row that made it into the table.
    let loaded_generation: Option<i64> = row.get(2)?;
    let dirtied_at: i64 = row.get(3)?;
    Ok(DirtyFileBuffer {
        path,
        context_id,
        loaded_generation: loaded_generation.map(|g| g as u64),
        dirtied_at,
    })
}

impl KernelDb {
    /// Record `context_id` as the last-viewed context for `client_id`
    /// (upsert). Called on every context switch, including the restore
    /// itself — an idempotent re-write of the same value is harmless.
    pub fn set_client_view(&self, client_id: &str, context_id: ContextId) -> KernelDbResult<()> {
        self.conn.execute(
            "INSERT INTO client_views (client_id, context_id, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(client_id) DO UPDATE SET
                context_id = excluded.context_id,
                updated_at = excluded.updated_at",
            params![client_id, blob_param(context_id.as_bytes()), now_millis()],
        )?;
        Ok(())
    }

    /// Read back the last-viewed context for `client_id`, or `None` if this
    /// client has never recorded one. No data-migration from the retired KV
    /// key (`<client_id>.current_context`) — losing the restore value once,
    /// for a single-user early-dev install, is acceptable.
    pub fn get_client_view(&self, client_id: &str) -> KernelDbResult<Option<ContextId>> {
        let mut stmt = self
            .conn
            .prepare("SELECT context_id FROM client_views WHERE client_id = ?1")?;
        let mut rows = stmt.query_map(params![client_id], |row| read_context_id(row, 0))?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    /// Record `path` as dirty (upsert) — called every time a buffer edit
    /// makes it dirty, including a re-edit while it is already dirty. A
    /// re-edit updates `context_id`/`loaded_generation` in place but leaves
    /// `dirtied_at` alone: the row remembers when this dirty spell began,
    /// not the timestamp of the latest edit inside it.
    pub fn record_dirty_file_buffer(
        &self,
        path: &str,
        context_id: ContextId,
        loaded_generation: Option<u64>,
    ) -> KernelDbResult<()> {
        self.conn.execute(
            "INSERT INTO dirty_file_buffers (path, context_id, loaded_generation)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(path) DO UPDATE SET
                context_id = excluded.context_id,
                loaded_generation = excluded.loaded_generation",
            params![
                path,
                blob_param(context_id.as_bytes()),
                loaded_generation.map(|g| g as i64),
            ],
        )?;
        Ok(())
    }

    /// Clear `path`'s dirty-buffer row — called when the buffer is flushed
    /// (`:w`) or the edit is discarded. Returns whether a row existed to
    /// clear, so a caller can tell a real flush from a no-op on a clean path.
    pub fn clear_dirty_file_buffer(&self, path: &str) -> KernelDbResult<bool> {
        let deleted = self
            .conn
            .execute("DELETE FROM dirty_file_buffers WHERE path = ?1", params![path])?;
        Ok(deleted > 0)
    }

    /// Fetch `path`'s dirty-buffer row, or `None` if the path has no unsaved
    /// work. This is the cold-start "is this a swap or a stale cache?" check.
    pub fn get_dirty_file_buffer(&self, path: &str) -> KernelDbResult<Option<DirtyFileBuffer>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, context_id, loaded_generation, dirtied_at
             FROM dirty_file_buffers WHERE path = ?1",
        )?;
        let mut rows = stmt.query_map(params![path], read_dirty_file_buffer)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    /// List every recovered swap, for the cold-start sweep that must
    /// announce each one. Ordered by path so the announcement a player sees
    /// is stable across restarts.
    pub fn list_dirty_file_buffers(&self) -> KernelDbResult<Vec<DirtyFileBuffer>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, context_id, loaded_generation, dirtied_at
             FROM dirty_file_buffers ORDER BY path",
        )?;
        let rows = stmt
            .query_map([], read_dirty_file_buffer)?
            .collect::<SqliteResult<Vec<_>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests;
