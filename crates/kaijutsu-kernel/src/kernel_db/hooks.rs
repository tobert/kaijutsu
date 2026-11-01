//! Hook entries and hook scripts.

use super::*;

/// A persisted hook entry. Flat shape so the rusqlite row readers map
/// directly; the broker reconstructs the live `HookEntry` (including
/// resolving `action_builtin_name` against `BuiltinHookRegistry`) at
/// hydrate time. `insertion_idx` is not exposed — the DB manages it
/// internally so load order within a phase matches insertion order.
#[derive(Debug, Clone)]
pub struct HookRow {
    pub hook_id: String,
    pub phase: String,
    pub priority: i32,
    pub match_instance: Option<String>,
    pub match_tool: Option<String>,
    pub match_context: Option<ContextId>,
    pub match_principal: Option<PrincipalId>,
    pub action_kind: String,
    pub action_builtin_name: Option<String>,
    /// Snapshot of the kaish body that fires at evaluation. Always
    /// set when `action_kind = "kaish_invoke"`. Even hooks installed
    /// via a shared `hook_scripts` reference snapshot the body here
    /// at install time per
    /// `feedback_script_snapshot_on_instantiation`.
    pub action_kaish_body: Option<String>,
    /// Provenance: if installed from a shared script, the originating
    /// `script_id`. Not re-resolved at hydrate; metadata only.
    pub action_kaish_script_id: Option<String>,
    /// `action_kind = "kaish_path"`: a VFS path read fresh at every
    /// fire, never snapshotted. Mutually exclusive with
    /// `action_kaish_body` — a `kaish_path` row carries no body at all.
    pub action_kaish_path: Option<String>,
    pub action_result_text: Option<String>,
    pub action_is_error: Option<bool>,
    pub action_deny_reason: Option<String>,
    pub action_log_target: Option<String>,
    pub action_log_level: Option<String>,
    /// `action_kind = "ask"` (D-57): the `AskSpec::description` override,
    /// if the hook set one. `None` means "auto-generate `{instance}.{tool}`
    /// at fire time" — that fallback is NOT persisted, so a rename of
    /// either doesn't stale the description.
    pub action_ask_description: Option<String>,
}

/// A shared kaish script body, referenced by zero or more hooks via
/// `hooks.action_kaish_script_id`. DB-global (matches the `hooks`
/// table); `script_id` is caller-supplied (defaults to UUIDv4 at the
/// admin surface) so common scripts can be addressed by stable name.
#[derive(Debug, Clone)]
pub struct HookScriptRow {
    pub script_id: String,
    pub body: String,
    pub description: Option<String>,
    pub created_at: i64,
    pub created_by: PrincipalId,
    pub updated_at: i64,
}

impl KernelDb {
    /// Persist one hook entry. `insertion_idx` is computed inside the
    /// INSERT as `MAX(insertion_idx) WHERE phase = ... + 1` so callers
    /// don't have to track it; load order within a phase uses that same
    /// column for tiebreak after `priority`. Fails with the SQLite
    /// UNIQUE-violation if `hook_id` already exists — callers should
    /// `delete_hook` first for replace semantics.
    pub fn insert_hook(&self, row: &HookRow) -> KernelDbResult<()> {
        let is_error: Option<i64> = row.action_is_error.map(|b| if b { 1 } else { 0 });
        let match_context_bytes: Option<Vec<u8>> =
            row.match_context.map(|c| c.as_bytes().to_vec());
        let match_principal_str: Option<String> =
            row.match_principal.map(|p| p.to_string());
        self.conn.execute(
            "INSERT INTO hooks (
                hook_id, phase, priority, insertion_idx,
                match_instance, match_tool, match_context, match_principal,
                action_kind,
                action_builtin_name, action_kaish_body, action_kaish_script_id,
                action_kaish_path,
                action_result_text, action_is_error,
                action_deny_reason,
                action_log_target, action_log_level,
                action_ask_description
             ) VALUES (
                ?1, ?2, ?3,
                (SELECT COALESCE(MAX(insertion_idx), -1) + 1 FROM hooks WHERE phase = ?2),
                ?4, ?5, ?6, ?7,
                ?8,
                ?9, ?10, ?11,
                ?12,
                ?13, ?14,
                ?15,
                ?16, ?17,
                ?18
             )",
            params![
                row.hook_id,
                row.phase,
                row.priority,
                row.match_instance,
                row.match_tool,
                match_context_bytes,
                match_principal_str,
                row.action_kind,
                row.action_builtin_name,
                row.action_kaish_body,
                row.action_kaish_script_id,
                row.action_kaish_path,
                row.action_result_text,
                is_error,
                row.action_deny_reason,
                row.action_log_target,
                row.action_log_level,
                row.action_ask_description,
            ],
        )?;
        Ok(())
    }

    /// Insert a hook entry, or replace it in place if `hook_id` already
    /// exists anywhere in the table (including under a different phase).
    /// One SQL statement, so the write either lands completely or not at
    /// all — unlike a caller-driven delete followed by an insert, a
    /// mid-write failure can never leave a previously persisted hook gone.
    /// Returns whether an existing row was replaced.
    pub fn upsert_hook(&self, row: &HookRow) -> KernelDbResult<bool> {
        let existed: i64 = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM hooks WHERE hook_id = ?1)",
            params![row.hook_id],
            |r| r.get(0),
        )?;
        let is_error: Option<i64> = row.action_is_error.map(|b| if b { 1 } else { 0 });
        let match_context_bytes: Option<Vec<u8>> =
            row.match_context.map(|c| c.as_bytes().to_vec());
        let match_principal_str: Option<String> =
            row.match_principal.map(|p| p.to_string());
        self.conn.execute(
            "INSERT INTO hooks (
                hook_id, phase, priority, insertion_idx,
                match_instance, match_tool, match_context, match_principal,
                action_kind,
                action_builtin_name, action_kaish_body, action_kaish_script_id,
                action_kaish_path,
                action_result_text, action_is_error,
                action_deny_reason,
                action_log_target, action_log_level,
                action_ask_description
             ) VALUES (
                ?1, ?2, ?3,
                (SELECT COALESCE(MAX(insertion_idx), -1) + 1 FROM hooks WHERE phase = ?2),
                ?4, ?5, ?6, ?7,
                ?8,
                ?9, ?10, ?11,
                ?12,
                ?13, ?14,
                ?15,
                ?16, ?17,
                ?18
             )
             ON CONFLICT(hook_id) DO UPDATE SET
                phase = excluded.phase,
                priority = excluded.priority,
                insertion_idx = excluded.insertion_idx,
                match_instance = excluded.match_instance,
                match_tool = excluded.match_tool,
                match_context = excluded.match_context,
                match_principal = excluded.match_principal,
                action_kind = excluded.action_kind,
                action_builtin_name = excluded.action_builtin_name,
                action_kaish_body = excluded.action_kaish_body,
                action_kaish_script_id = excluded.action_kaish_script_id,
                action_kaish_path = excluded.action_kaish_path,
                action_result_text = excluded.action_result_text,
                action_is_error = excluded.action_is_error,
                action_deny_reason = excluded.action_deny_reason,
                action_log_target = excluded.action_log_target,
                action_log_level = excluded.action_log_level,
                action_ask_description = excluded.action_ask_description",
            params![
                row.hook_id,
                row.phase,
                row.priority,
                row.match_instance,
                row.match_tool,
                match_context_bytes,
                match_principal_str,
                row.action_kind,
                row.action_builtin_name,
                row.action_kaish_body,
                row.action_kaish_script_id,
                row.action_kaish_path,
                row.action_result_text,
                is_error,
                row.action_deny_reason,
                row.action_log_target,
                row.action_log_level,
                row.action_ask_description,
            ],
        )?;
        Ok(existed != 0)
    }

    /// Delete a hook by id. Returns true if a row existed.
    pub fn delete_hook(&self, hook_id: &str) -> KernelDbResult<bool> {
        let rows = self
            .conn
            .execute("DELETE FROM hooks WHERE hook_id = ?1", params![hook_id])?;
        Ok(rows > 0)
    }

    /// Insert a new hook script. Duplicate `script_id` returns
    /// `KernelDbError::AlreadyExists` so callers can distinguish create
    /// vs update.
    pub fn insert_hook_script(&self, row: &HookScriptRow) -> KernelDbResult<()> {
        self.conn
            .execute(
                "INSERT INTO hook_scripts (
                    script_id, body, description,
                    created_at, created_by, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    row.script_id,
                    row.body,
                    row.description,
                    row.created_at,
                    blob_param(row.created_by.as_bytes()),
                    row.updated_at,
                ],
            )
            .map_err(|e| {
                map_unique_violation(e, format!("hook script '{}' already exists", row.script_id))
            })?;
        Ok(())
    }

    /// Replace the body (and optional description) of an existing
    /// script, bumping `updated_at`. Returns `Ok(false)` if no row
    /// exists with the given `script_id`.
    pub fn update_hook_script(
        &self,
        script_id: &str,
        body: &str,
        description: Option<&str>,
    ) -> KernelDbResult<bool> {
        let now = kaijutsu_types::now_millis() as i64;
        let rows = self.conn.execute(
            "UPDATE hook_scripts
             SET body = ?2, description = ?3, updated_at = ?4
             WHERE script_id = ?1",
            params![script_id, body, description, now],
        )?;
        Ok(rows > 0)
    }

    /// Look up a single script by id.
    pub fn get_hook_script(&self, script_id: &str) -> KernelDbResult<Option<HookScriptRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT script_id, body, description,
                    created_at, created_by, updated_at
             FROM hook_scripts
             WHERE script_id = ?1",
        )?;
        let mut rows = stmt.query(params![script_id])?;
        if let Some(r) = rows.next()? {
            Ok(Some(row_to_hook_script_row(r)?))
        } else {
            Ok(None)
        }
    }

    /// Load every hook script in the DB, ordered by `script_id`.
    /// Used by both the admin surface (`hook_script_list`) and
    /// `Broker::hydrate_hooks_from_db` (to build the script lookup
    /// table for resolving `action_kaish_script_id` references).
    pub fn list_all_hook_scripts(&self) -> KernelDbResult<Vec<HookScriptRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT script_id, body, description,
                    created_at, created_by, updated_at
             FROM hook_scripts
             ORDER BY script_id ASC",
        )?;
        let rows = stmt.query_map([], row_to_hook_script_row)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Count hooks referencing this script. Callers use this to enforce
    /// the "fail deletion when referenced" guard rather than cascading.
    pub fn count_hooks_referencing_script(&self, script_id: &str) -> KernelDbResult<usize> {
        let mut stmt = self
            .conn
            .prepare("SELECT COUNT(*) FROM hooks WHERE action_kaish_script_id = ?1")?;
        let n: i64 = stmt.query_row(params![script_id], |r| r.get(0))?;
        Ok(n as usize)
    }

    /// Delete a hook script. Caller must verify
    /// `count_hooks_referencing_script == 0` first to avoid orphaning
    /// `hooks.action_kaish_script_id` references — the schema has no FK
    /// constraint by design (FKs across SQLite ATTACH boundaries hurt;
    /// the guard lives in the application layer).
    pub fn delete_hook_script(&self, script_id: &str) -> KernelDbResult<bool> {
        let rows = self
            .conn
            .execute("DELETE FROM hook_scripts WHERE script_id = ?1", params![script_id])?;
        Ok(rows > 0)
    }

    /// Load every persisted hook row, ordered by
    /// `(phase ASC, priority ASC, insertion_idx ASC)`. The broker walks
    /// these in order and pushes each onto the matching `HookTable`,
    /// reconstructing the Vec insertion order from before the restart.
    pub fn load_all_hooks(&self) -> KernelDbResult<Vec<HookRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT hook_id, phase, priority,
                    match_instance, match_tool, match_context, match_principal,
                    action_kind,
                    action_builtin_name, action_kaish_body, action_kaish_script_id,
                    action_kaish_path,
                    action_result_text, action_is_error,
                    action_deny_reason,
                    action_log_target, action_log_level,
                    action_ask_description
             FROM hooks
             ORDER BY phase ASC, priority ASC, insertion_idx ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let match_context_bytes: Option<Vec<u8>> = row.get(5)?;
            let match_context = match match_context_bytes {
                Some(bytes) => Some(ContextId::try_from_slice(&bytes).ok_or_else(|| {
                    rusqlite::Error::FromSqlConversionFailure(
                        5,
                        rusqlite::types::Type::Blob,
                        "invalid ContextId bytes in hooks.match_context".into(),
                    )
                })?),
                None => None,
            };
            let match_principal_str: Option<String> = row.get(6)?;
            let match_principal = match match_principal_str {
                Some(s) => Some(PrincipalId::parse(&s).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        6,
                        rusqlite::types::Type::Text,
                        format!("invalid PrincipalId in hooks.match_principal: {e}").into(),
                    )
                })?),
                None => None,
            };
            let is_error_int: Option<i64> = row.get(13)?;
            let action_is_error = is_error_int.map(|i| i != 0);
            Ok(HookRow {
                hook_id: row.get(0)?,
                phase: row.get(1)?,
                priority: row.get(2)?,
                match_instance: row.get(3)?,
                match_tool: row.get(4)?,
                match_context,
                match_principal,
                action_kind: row.get(7)?,
                action_builtin_name: row.get(8)?,
                action_kaish_body: row.get(9)?,
                action_kaish_script_id: row.get(10)?,
                action_kaish_path: row.get(11)?,
                action_result_text: row.get(12)?,
                action_is_error,
                action_deny_reason: row.get(14)?,
                action_log_target: row.get(15)?,
                action_log_level: row.get(16)?,
                action_ask_description: row.get(17)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
