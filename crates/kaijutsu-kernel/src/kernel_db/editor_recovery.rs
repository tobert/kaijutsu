//! Editor recovery records: the argument list, index and cursors of a vi
//! session that has an opener, kept until the session closes
//! (`docs/vi.md`, "Recovering an argument list").

use super::*;

/// The handle of one recovery record (UUIDv7). It is not the live session's
/// handle: a session numbers itself per kernel run, and the record outlives
/// the run.
#[derive(Clone, Copy, Hash, Eq, PartialEq, Ord, PartialOrd)]
pub struct RecoveryId(uuid::Uuid);

impl RecoveryId {
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7())
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }

    pub fn try_from_slice(b: &[u8]) -> Option<Self> {
        uuid::Uuid::from_slice(b).ok().map(Self)
    }

    /// The 8 hex characters of the random tail: what a list shows and what
    /// a pick types. The front of a UUIDv7 is a clock shared by every id
    /// minted in the same minute.
    pub fn short(&self) -> String {
        let hex = self.0.as_simple().to_string();
        hex[hex.len() - 8..].to_string()
    }

    pub fn to_hex(&self) -> String {
        self.0.as_simple().to_string()
    }

    /// A full id matches exactly; otherwise `query` is a prefix of
    /// [`short`](Self::short).
    pub fn matches_short(&self, query: &str) -> bool {
        match uuid::Uuid::parse_str(query) {
            Ok(full) => self.0 == full,
            Err(_) => !query.is_empty() && self.short().starts_with(query),
        }
    }
}

impl Default for RecoveryId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for RecoveryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RecoveryId({})", self.short())
    }
}

/// One file of a recorded argument list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryFile {
    /// The path as the opener named it.
    pub path: String,
    /// Whether the session had loaded the file.
    pub visited: bool,
    /// 0-based line and column of the file's cursor at the latest write.
    pub cursor_line: usize,
    pub cursor_col: usize,
}

/// One recovery record: a session's list and where it stood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverySession {
    pub recovery_id: RecoveryId,
    pub opener_principal: PrincipalId,
    pub opener_context_id: ContextId,
    pub current_index: usize,
    pub opened_at: i64,
    pub updated_at: i64,
    /// In list order.
    pub files: Vec<RecoveryFile>,
}

impl KernelDb {
    /// Write a record: the first call for `id` inserts it, later calls replace
    /// its index and files and keep `opened_at`. One transaction, so a failed
    /// write leaves the previous record whole.
    pub fn write_editor_recovery(
        &self,
        id: RecoveryId,
        opener_principal: PrincipalId,
        opener_context_id: ContextId,
        current_index: usize,
        files: &[RecoveryFile],
    ) -> KernelDbResult<()> {
        if files.is_empty() {
            return Err(KernelDbError::Validation("an editor recovery record needs at least one file".into()));
        }
        if current_index >= files.len() {
            return Err(KernelDbError::Validation(format!(
                "editor recovery index {current_index} is outside a list of {} files",
                files.len()
            )));
        }
        self.in_transaction(|db| {
            let now = now_millis();
            db.conn.execute(
                "INSERT INTO editor_recovery_sessions
                    (recovery_id, opener_principal, opener_context_id, current_index, opened_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                 ON CONFLICT(recovery_id) DO UPDATE SET
                    opener_principal = excluded.opener_principal,
                    opener_context_id = excluded.opener_context_id,
                    current_index = excluded.current_index,
                    updated_at = excluded.updated_at",
                params![
                    id.as_bytes().as_slice(),
                    blob_param(opener_principal.as_bytes()),
                    blob_param(opener_context_id.as_bytes()),
                    current_index as i64,
                    now,
                ],
            )?;
            db.conn.execute(
                "DELETE FROM editor_recovery_files WHERE recovery_id = ?1",
                params![id.as_bytes().as_slice()],
            )?;
            for (position, file) in files.iter().enumerate() {
                db.conn.execute(
                    "INSERT INTO editor_recovery_files
                        (recovery_id, position, path, visited, cursor_line, cursor_col)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        id.as_bytes().as_slice(),
                        position as i64,
                        file.path,
                        file.visited,
                        file.cursor_line as i64,
                        file.cursor_col as i64,
                    ],
                )?;
            }
            Ok(())
        })
    }

    /// Delete a record and its files. Returns whether it existed.
    pub fn delete_editor_recovery(&self, id: RecoveryId) -> KernelDbResult<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM editor_recovery_sessions WHERE recovery_id = ?1",
            params![id.as_bytes().as_slice()],
        )?;
        Ok(deleted > 0)
    }

    /// Every record, newest write first.
    pub fn list_editor_recoveries(&self) -> KernelDbResult<Vec<RecoverySession>> {
        let mut stmt = self.conn.prepare(
            "SELECT recovery_id, opener_principal, opener_context_id, current_index, opened_at, updated_at
             FROM editor_recovery_sessions
             ORDER BY updated_at DESC, opened_at DESC, recovery_id DESC",
        )?;
        let heads = stmt
            .query_map([], read_recovery_head)?
            .collect::<SqliteResult<Vec<_>>>()?;
        heads.into_iter().map(|head| self.with_recovery_files(head)).collect()
    }

    /// One record, or `None`.
    pub fn get_editor_recovery(&self, id: RecoveryId) -> KernelDbResult<Option<RecoverySession>> {
        let mut stmt = self.conn.prepare(
            "SELECT recovery_id, opener_principal, opener_context_id, current_index, opened_at, updated_at
             FROM editor_recovery_sessions WHERE recovery_id = ?1",
        )?;
        let head = stmt
            .query_map(params![id.as_bytes().as_slice()], read_recovery_head)?
            .next()
            .transpose()?;
        head.map(|head| self.with_recovery_files(head)).transpose()
    }

    fn with_recovery_files(&self, mut session: RecoverySession) -> KernelDbResult<RecoverySession> {
        let mut stmt = self.conn.prepare(
            "SELECT position, path, visited, cursor_line, cursor_col
             FROM editor_recovery_files WHERE recovery_id = ?1 ORDER BY position",
        )?;
        let rows = stmt
            .query_map(params![session.recovery_id.as_bytes().as_slice()], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    RecoveryFile {
                        path: row.get(1)?,
                        visited: row.get(2)?,
                        cursor_line: row.get::<_, i64>(3)? as usize,
                        cursor_col: row.get::<_, i64>(4)? as usize,
                    },
                ))
            })?
            .collect::<SqliteResult<Vec<_>>>()?;
        for (expected, (position, _)) in rows.iter().enumerate() {
            if *position != expected as i64 {
                return Err(KernelDbError::Validation(format!(
                    "editor recovery {} has no file at position {expected}",
                    session.recovery_id.short()
                )));
            }
        }
        if rows.is_empty() {
            return Err(KernelDbError::Validation(format!(
                "editor recovery {} has no file rows",
                session.recovery_id.short()
            )));
        }
        if session.current_index >= rows.len() {
            return Err(KernelDbError::Validation(format!(
                "editor recovery {} has index {} in a list of {} files",
                session.recovery_id.short(),
                session.current_index,
                rows.len()
            )));
        }
        session.files = rows.into_iter().map(|(_, file)| file).collect();
        Ok(session)
    }
}

fn read_recovery_head(row: &rusqlite::Row<'_>) -> SqliteResult<RecoverySession> {
    let id: Vec<u8> = row.get(0)?;
    let recovery_id = RecoveryId::try_from_slice(&id).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Blob,
            "invalid RecoveryId bytes".into(),
        )
    })?;
    Ok(RecoverySession {
        recovery_id,
        opener_principal: read_principal_id(row, 1)?,
        opener_context_id: read_context_id(row, 2)?,
        current_index: row.get::<_, i64>(3)? as usize,
        opened_at: row.get(4)?,
        updated_at: row.get(5)?,
        files: Vec::new(),
    })
}

#[cfg(test)]
mod tests;
