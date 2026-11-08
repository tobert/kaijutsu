//! Context watches: what a context watches and the coalesced status of each
//! since its last delivery (`docs/watch.md`).

use super::*;

/// One row of `context_watches`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchRow {
    pub watch_id: i64,
    pub context_id: ContextId,
    pub kind: String,
    pub target: String,
    pub host_path: String,
    pub created_by: PrincipalId,
    pub created_at: i64,
    pub changes: u64,
    pub first_change_at: Option<i64>,
    pub last_change_at: Option<i64>,
    pub present: Option<bool>,
    pub delivered_at: Option<i64>,
    /// When promotion stopped the watch; `kj watch add` starts it again.
    pub stopped_at: Option<i64>,
}

const WATCH_COLUMNS: &str = "w.watch_id, w.context_id, w.kind, w.target, w.host_path, w.created_by, w.created_at, \
     w.changes, w.first_change_at, w.last_change_at, w.present, w.delivered_at, w.stopped_at";

/// Only a running watch on a live context accrues or delivers.
const LIVE: &str = "JOIN contexts c ON c.context_id = w.context_id AND c.archived_at IS NULL AND c.context_state = 'live' AND w.stopped_at IS NULL";

fn read_watch(row: &rusqlite::Row<'_>) -> SqliteResult<WatchRow> {
    Ok(WatchRow {
        watch_id: row.get(0)?,
        context_id: read_context_id(row, 1)?,
        kind: row.get(2)?,
        target: row.get(3)?,
        host_path: row.get(4)?,
        created_by: read_principal_id(row, 5)?,
        created_at: row.get(6)?,
        changes: row.get::<_, i64>(7)? as u64,
        first_change_at: row.get(8)?,
        last_change_at: row.get(9)?,
        present: row.get::<_, Option<i64>>(10)?.map(|p| p != 0),
        delivered_at: row.get(11)?,
        stopped_at: row.get(12)?,
    })
}

impl KernelDb {
    /// Add a path watch to `context`. A second watch on the same target is a
    /// validation error naming the watch that holds it; a stopped one starts
    /// again with an empty status.
    pub fn add_path_watch(
        &self, context: ContextId, target: &str, host_path: &str, created_by: PrincipalId,
    ) -> KernelDbResult<WatchRow> {
        if let Some(held) = self.list_watches(context)?.into_iter().find(|w| w.kind == "path" && w.target == target) {
            if held.stopped_at.is_none() {
                return Err(KernelDbError::Validation(format!("{target} is already watched by watch {}", held.watch_id)));
            }
            self.conn.execute(
                "UPDATE context_watches SET stopped_at = NULL, host_path = ?2, changes = 0, first_change_at = NULL,
                    last_change_at = NULL, present = NULL WHERE watch_id = ?1",
                params![held.watch_id, host_path],
            )?;
            return self.get_watch(context, held.watch_id)?
                .ok_or_else(|| KernelDbError::Validation("the restarted watch was not stored".into()));
        }
        self.conn.execute(
            "INSERT INTO context_watches (context_id, kind, target, host_path, created_by, created_at)
             VALUES (?1, 'path', ?2, ?3, ?4, ?5)",
            params![blob_param(context.as_bytes()), target, host_path, blob_param(created_by.as_bytes()), now_millis()],
        )?;
        let id = self.conn.last_insert_rowid();
        self.get_watch(context, id)?.ok_or_else(|| KernelDbError::Validation("the new watch was not stored".into()))
    }

    /// `context`'s watches, oldest first.
    pub fn list_watches(&self, context: ContextId) -> KernelDbResult<Vec<WatchRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {WATCH_COLUMNS} FROM context_watches w WHERE w.context_id = ?1 ORDER BY w.watch_id"
        ))?;
        let rows = stmt.query_map(params![blob_param(context.as_bytes())], read_watch)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// One of `context`'s watches; another context's watch is `None`.
    pub fn get_watch(&self, context: ContextId, watch_id: i64) -> KernelDbResult<Option<WatchRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {WATCH_COLUMNS} FROM context_watches w WHERE w.context_id = ?1 AND w.watch_id = ?2"
        ))?;
        let mut rows = stmt.query_map(params![blob_param(context.as_bytes()), watch_id], read_watch)?;
        Ok(rows.next().transpose()?)
    }

    /// Remove one of `context`'s watches. Returns whether it existed.
    pub fn remove_watch(&self, context: ContextId, watch_id: i64) -> KernelDbResult<bool> {
        let removed = self.conn.execute(
            "DELETE FROM context_watches WHERE context_id = ?1 AND watch_id = ?2",
            params![blob_param(context.as_bytes()), watch_id],
        )?;
        Ok(removed == 1)
    }

    /// Record an event on `host_path` for every live context watching it.
    /// `new_burst` counts the event as a change; an event inside a burst
    /// moves only `last_change_at` and `present`, unless the watch was
    /// delivered since the burst began, when it counts again. Returns the
    /// rows touched.
    pub fn record_path_event(&self, host_path: &str, present: bool, at: i64, new_burst: bool) -> KernelDbResult<usize> {
        let touched = self.conn.execute(
            "UPDATE context_watches SET
                changes = changes + (CASE WHEN ?4 = 1 OR changes = 0 THEN 1 ELSE 0 END),
                first_change_at = COALESCE(first_change_at, ?3),
                last_change_at = ?3,
                present = ?2
             WHERE kind = 'path' AND host_path = ?1 AND stopped_at IS NULL AND context_id IN
                (SELECT context_id FROM contexts WHERE archived_at IS NULL AND context_state = 'live')",
            params![host_path, present as i64, at, new_burst as i64],
        )?;
        Ok(touched)
    }

    /// `context`'s watches with changes waiting, if the context is live.
    pub fn pending_watches(&self, context: ContextId) -> KernelDbResult<Vec<WatchRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {WATCH_COLUMNS} FROM context_watches w {LIVE} WHERE w.context_id = ?1 AND w.changes > 0 ORDER BY w.watch_id"
        ))?;
        let rows = stmt.query_map(params![blob_param(context.as_bytes())], read_watch)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Reset the status of the delivered watches. Each must still hold the
    /// change count that was rendered, or the delivery fails: an event that
    /// landed between render and reset would otherwise be lost.
    pub fn mark_watches_delivered(&self, delivered: &[(i64, u64)], at: i64) -> KernelDbResult<()> {
        for (watch_id, changes) in delivered {
            let reset = self.conn.execute(
                "UPDATE context_watches SET changes = 0, first_change_at = NULL, last_change_at = NULL, delivered_at = ?3
                 WHERE watch_id = ?1 AND changes = ?2",
                params![watch_id, *changes as i64, at],
            )?;
            if reset != 1 {
                return Err(KernelDbError::Validation(format!("watch {watch_id} changed during its delivery")));
            }
        }
        Ok(())
    }

    /// Stop every watch of `context`. Promotion calls this, so a context
    /// brought back from the archive does not re-arm. Does not commit.
    pub(crate) fn stop_watches(&self, context: ContextId, at: i64) -> KernelDbResult<()> {
        self.conn.execute(
            "UPDATE context_watches SET stopped_at = ?2 WHERE context_id = ?1 AND stopped_at IS NULL",
            params![blob_param(context.as_bytes()), at],
        )?;
        Ok(())
    }

    /// Give `successor` a fresh copy of each of `predecessor`'s watches: the
    /// same targets, an empty status. The predecessor keeps its rows.
    /// Does not commit.
    pub(crate) fn copy_watches(&self, predecessor: ContextId, successor: ContextId) -> KernelDbResult<()> {
        self.conn.execute(
            "INSERT INTO context_watches (context_id, kind, target, host_path, created_by, created_at)
             SELECT ?2, kind, target, host_path, created_by, ?3 FROM context_watches
             WHERE context_id = ?1 AND stopped_at IS NULL ORDER BY watch_id",
            params![blob_param(predecessor.as_bytes()), blob_param(successor.as_bytes()), now_millis()],
        )?;
        Ok(())
    }

    /// The host paths some live context watches, for the path watcher.
    pub fn watched_host_paths(&self) -> KernelDbResult<Vec<String>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT DISTINCT w.host_path FROM context_watches w {LIVE} WHERE w.kind = 'path' ORDER BY w.host_path"
        ))?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }
}

#[cfg(test)]
mod tests;
