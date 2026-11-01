//! Context shell, env and egress storage.

use super::*;

/// Per-context shell configuration.
#[derive(Debug, Clone)]
pub struct ContextShellRow {
    pub context_id: ContextId,
    pub cwd: Option<String>,
    pub updated_at: i64,
}

/// Per-context environment variable.
#[derive(Debug, Clone)]
pub struct ContextEnvRow {
    pub context_id: ContextId,
    pub key: String,
    pub value: String,
}

impl KernelDb {
    /// Write per-context shell config against `conn` — a bare `Connection`
    /// or an open `Transaction` (which derefs to one). Shared by
    /// `upsert_context_shell` and the transactional `fork_context_config` so
    /// the SQL has a single home.
    pub(super) fn write_context_shell(conn: &Connection, row: &ContextShellRow) -> KernelDbResult<()> {
        conn.execute(
            "INSERT INTO context_shell (context_id, cwd, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(context_id) DO UPDATE SET
                cwd = excluded.cwd,
                updated_at = excluded.updated_at",
            params![
                blob_param(row.context_id.as_bytes()),
                row.cwd,
                row.updated_at,
            ],
        )?;
        Ok(())
    }

    /// Upsert per-context shell configuration (cwd).
    pub fn upsert_context_shell(&self, row: &ContextShellRow) -> KernelDbResult<()> {
        Self::write_context_shell(&self.conn, row)
    }

    /// Get per-context shell configuration. Returns None if not set.
    pub fn get_context_shell(
        &self,
        context_id: ContextId,
    ) -> KernelDbResult<Option<ContextShellRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, cwd, updated_at
             FROM context_shell WHERE context_id = ?1",
        )?;
        let mut rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
            Ok(ContextShellRow {
                context_id: read_context_id(row, 0)?,
                cwd: row.get(1)?,
                updated_at: row.get(2)?,
            })
        })?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    /// Copy shell config from source to target. Returns true if source had config.
    pub fn copy_context_shell(&self, source: ContextId, target: ContextId) -> KernelDbResult<bool> {
        let src = match self.get_context_shell(source)? {
            Some(s) => s,
            None => return Ok(false),
        };
        let row = ContextShellRow {
            context_id: target,
            cwd: src.cwd,
            updated_at: now_millis(),
        };
        self.upsert_context_shell(&row)?;
        Ok(true)
    }

    /// Write one env var against `conn` (a `Connection` or an open
    /// `Transaction`). Shared by `set_context_env` and `fork_context_config`.
    pub(super) fn write_context_env(
        conn: &Connection,
        context_id: ContextId,
        key: &str,
        value: &str,
    ) -> KernelDbResult<()> {
        validate_env_key(key)?;
        conn.execute(
            "INSERT INTO context_env (context_id, key, value)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(context_id, key) DO UPDATE SET value = excluded.value",
            params![blob_param(context_id.as_bytes()), key, value],
        )?;
        Ok(())
    }

    /// Set a single environment variable for a context (upsert).
    pub fn set_context_env(
        &self,
        context_id: ContextId,
        key: &str,
        value: &str,
    ) -> KernelDbResult<()> {
        Self::write_context_env(&self.conn, context_id, key, value)
    }

    /// Get all environment variables for a context.
    pub fn get_context_env(&self, context_id: ContextId) -> KernelDbResult<Vec<ContextEnvRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, key, value FROM context_env
             WHERE context_id = ?1 ORDER BY key",
        )?;
        let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
            Ok(ContextEnvRow {
                context_id: read_context_id(row, 0)?,
                key: row.get(1)?,
                value: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Delete a single environment variable. Returns true if it existed.
    pub fn delete_context_env(&self, context_id: ContextId, key: &str) -> KernelDbResult<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM context_env WHERE context_id = ?1 AND key = ?2",
            params![blob_param(context_id.as_bytes()), key],
        )?;
        Ok(deleted > 0)
    }

    /// Delete all environment variables for a context. Returns count deleted.
    pub fn clear_context_env(&self, context_id: ContextId) -> KernelDbResult<u64> {
        let deleted = self.conn.execute(
            "DELETE FROM context_env WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
        )?;
        Ok(deleted as u64)
    }

    /// Copy all env vars from source to target. Returns count copied.
    pub fn copy_context_env(&self, source: ContextId, target: ContextId) -> KernelDbResult<u64> {
        let vars = self.get_context_env(source)?;
        for var in &vars {
            self.set_context_env(target, &var.key, &var.value)?;
        }
        Ok(vars.len() as u64)
    }

    /// Add one host against `conn` (a `Connection` or an open `Transaction`).
    /// Shared by `add_context_egress` and the fork copy paths. Idempotent —
    /// adding an already-present host is a no-op, not a conflict.
    pub(super) fn write_context_egress(conn: &Connection, context_id: ContextId, host: &str) -> KernelDbResult<()> {
        let host = validate_egress_host(host)?;
        conn.execute(
            "INSERT INTO context_egress (context_id, host)
             VALUES (?1, ?2)
             ON CONFLICT(context_id, host) DO NOTHING",
            params![blob_param(context_id.as_bytes()), host],
        )?;
        Ok(())
    }

    /// Add a host to a context's egress list (idempotent).
    pub fn add_context_egress(&self, context_id: ContextId, host: &str) -> KernelDbResult<()> {
        Self::write_context_egress(&self.conn, context_id, host)
    }

    /// Remove a host from a context's egress list. Returns true if it existed.
    /// Validates and normalizes `host` first, so `--egress-deny LOCALHOST`
    /// removes the `localhost` row it actually stored.
    pub fn remove_context_egress(&self, context_id: ContextId, host: &str) -> KernelDbResult<bool> {
        let host = validate_egress_host(host)?;
        let deleted = self.conn.execute(
            "DELETE FROM context_egress WHERE context_id = ?1 AND host = ?2",
            params![blob_param(context_id.as_bytes()), host],
        )?;
        Ok(deleted > 0)
    }

    /// List a context's egress hosts, sorted.
    pub fn list_context_egress(&self, context_id: ContextId) -> KernelDbResult<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT host FROM context_egress WHERE context_id = ?1 ORDER BY host",
        )?;
        let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
            row.get::<_, String>(0)
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Copy all egress hosts from source to target. Returns count copied.
    pub fn copy_context_egress(&self, source: ContextId, target: ContextId) -> KernelDbResult<u64> {
        let hosts = self.list_context_egress(source)?;
        for host in &hosts {
            self.add_context_egress(target, host)?;
        }
        Ok(hosts.len() as u64)
    }
}

#[cfg(test)]
mod tests;
