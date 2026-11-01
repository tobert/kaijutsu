//! Kernel identity and quiesce state.

use super::*;

/// The kernel is quiesced: it accepts writes but starts no turns.
///
/// Held only while the flag is set — the running kernel has no
/// `QuiesceState`, which is why every read returns an `Option`.
#[derive(Debug, Clone)]
pub struct QuiesceState {
    pub since_unix_ms: u64,
    pub quiesced_by: PrincipalId,
    /// What the operator said they were stopping. Optional, and worth
    /// asking for: whoever finds the kernel quiesced is usually not the
    /// person who quiesced it.
    pub reason: Option<String>,
}

impl KernelDb {
    /// The quiesce flag as it is on disk right now, or `None` when the kernel
    /// is running.
    ///
    /// Every caller reads it fresh. There is no cached copy on purpose: the
    /// flag is set precisely when something has gone wrong, and a stale
    /// "running" would start the turn the operator was stopping.
    pub fn quiesce_state(&self) -> KernelDbResult<Option<QuiesceState>> {
        let mut stmt = self
            .conn
            .prepare("SELECT since, quiesced_by, reason FROM system_quiesce WHERE singleton = 1")?;
        let mut rows = stmt.query([])?;
        match rows.next()? {
            Some(row) => Ok(Some(QuiesceState {
                since_unix_ms: row.get::<_, i64>(0)? as u64,
                quiesced_by: read_principal_id(row, 1)?,
                reason: row.get(2)?,
            })),
            None => Ok(None),
        }
    }

    /// Set the flag. Re-quiescing an already-quiesced kernel updates the
    /// principal and reason but **keeps the original `since`**: "how long has
    /// this kernel been stopped" is an incident-response question, and
    /// correcting the reason must not erase the answer.
    pub fn set_quiesced(&self, by: PrincipalId, reason: Option<&str>) -> KernelDbResult<()> {
        self.conn.execute(
            "INSERT INTO system_quiesce (singleton, since, quiesced_by, reason)
             VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(singleton) DO UPDATE SET
                 quiesced_by = excluded.quiesced_by,
                 reason      = excluded.reason",
            params![now_millis(), blob_param(by.as_bytes()), reason],
        )?;
        Ok(())
    }

    /// Clear the flag. Returns true when the kernel was quiesced, so a caller
    /// can tell "resumed" from "was already running" without a second read.
    pub fn clear_quiesced(&self) -> KernelDbResult<bool> {
        let rows = self
            .conn
            .execute("DELETE FROM system_quiesce WHERE singleton = 1", [])?;
        Ok(rows > 0)
    }

    /// Read any legacy `rc_scripts` rows from a pre-files DB, as
    /// `(canonical_path, content)` pairs. Used once at boot to migrate a
    /// user's customizations onto the `/config/rc` file tree. New DBs never
    /// create the table, so a missing table yields an empty vec (not an
    /// error) — the migration simply no-ops.
    pub fn legacy_rc_scripts(&self) -> Vec<(String, String)> {
        let mut stmt = match self
            .conn
            .prepare("SELECT path, content FROM rc_scripts")
        {
            Ok(s) => s,
            Err(_) => return Vec::new(), // table absent on new DBs
        };
        let rows = match stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        rows.filter_map(|r| r.ok()).collect()
    }

    /// Create the singleton kernel row on first open. Subsequent opens
    /// are no-ops because the row's `singleton` value is fixed at 1 and
    /// `INSERT OR IGNORE` short-circuits on the PK conflict.
    pub(super) fn ensure_singleton_kernel(conn: &Connection) -> KernelDbResult<()> {
        let id = KernelId::new();
        let founder = PrincipalId::system();
        conn.execute(
            "INSERT OR IGNORE INTO kernel (singleton, id, founder, label)
             VALUES (1, ?1, ?2, NULL)",
            params![blob_param(id.as_bytes()), blob_param(founder.as_bytes())],
        )?;
        Ok(())
    }

    /// The stable kernel ID for this database — written on first open,
    /// immutable thereafter. Used by the wire `bind_kernel` / `ping`
    /// handshake so clients can detect when they're talking to a
    /// different kernel than the one they bound to.
    pub fn kernel_id(&self) -> KernelDbResult<KernelId> {
        let bytes: Vec<u8> = self
            .conn
            .query_row("SELECT id FROM kernel WHERE singleton = 1", [], |row| {
                row.get(0)
            })?;
        KernelId::try_from_slice(&bytes).ok_or_else(|| {
            KernelDbError::Validation("kernel row holds invalid id bytes".to_string())
        })
    }

    /// Set or clear the kernel's human-friendly label.
    pub fn set_kernel_label(&self, label: Option<&str>) -> KernelDbResult<()> {
        if let Some(l) = label {
            validate_label(l)?;
        }
        self.conn.execute(
            "UPDATE kernel SET label = ?1 WHERE singleton = 1",
            params![label],
        )?;
        Ok(())
    }

    /// True when the named one-time migration has already been recorded in
    /// `kernel_migrations` on this database.
    pub fn migration_applied(&self, name: &str) -> KernelDbResult<bool> {
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM kernel_migrations WHERE name = ?1",
                params![name],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Drop the quiesce table, so `quiesce_state` returns an error rather than
    /// "not quiesced". The turn path fails closed on that error, and this is
    /// the only way to reach that branch. Never compiled outside test builds.
    #[cfg(test)]
    pub(crate) fn poison_quiesce_table_for_test(&self) -> KernelDbResult<()> {
        self.conn.execute_batch("DROP TABLE system_quiesce")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
