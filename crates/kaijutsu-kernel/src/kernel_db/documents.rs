//! Documents, oplog, block provenance, and snapshots.

use super::*;

/// A document row — the document content layer.
#[derive(Debug, Clone)]
pub struct DocumentRow {
    pub document_id: ContextId,
    pub workspace_id: WorkspaceId,
    pub doc_kind: DocKind,
    pub language: Option<String>,
    pub path: Option<String>,
    pub created_at: i64,
    pub created_by: PrincipalId,
}

/// A doc_snapshots row — compaction checkpoint.
///
/// `state` is the whole document, so there is no separate text column: the
/// text is what decoding `state` produces. A reader that wants only the text
/// still decodes, which is the cost of having one copy instead of two that
/// can disagree.
#[derive(Debug, Clone)]
pub struct DocSnapshotRow {
    pub document_id: ContextId,
    pub seq: i64,
    pub version: i64,
    pub state: Vec<u8>,
    pub created_at: i64,
}

impl KernelDb {
    /// Insert a new document.
    pub fn insert_document(&self, row: &DocumentRow) -> KernelDbResult<()> {
        Self::write_document(&self.conn, row)
            .map_err(|e| self.classify_document_insert_error(e, row))
    }

    /// Insert a document row against `conn` (a `&Connection` or a
    /// `&Transaction` via deref). Shared by `insert_document` and the
    /// transactional `insert_document_with_op`. Returns the raw SQLite error
    /// so the caller can classify a constraint violation by reading the
    /// database back. Does NOT commit.
    pub(super) fn write_document(conn: &Connection, row: &DocumentRow) -> SqliteResult<()> {
        conn.execute(
            "INSERT INTO documents (
                document_id, workspace_id, doc_kind,
                language, path, created_at, created_by
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                blob_param(row.document_id.as_bytes()),
                blob_param(row.workspace_id.as_bytes()),
                row.doc_kind.as_str(),
                row.language,
                row.path,
                row.created_at,
                blob_param(row.created_by.as_bytes()),
            ],
        )?;
        Ok(())
    }

    /// Insert a document row and its first op in ONE transaction: both commit
    /// or neither does.
    ///
    /// A document row that commits without its first op is unreadable. The
    /// boot replay finds no snapshot and no oplog, builds an empty document,
    /// and every read of it answers "exists but has no blocks" — the shape
    /// `BlockStore::load_from_db`'s boot repair now sweeps. Writing the two
    /// rows in separate transactions is what leaves that gap;
    /// `BlockStore::create_document_with_block` closes it by coming here.
    ///
    /// `seq` is the op's oplog sequence, 1 for a document's first op.
    pub fn insert_document_with_op(
        &mut self,
        row: &DocumentRow,
        seq: i64,
        payload: &[u8],
    ) -> KernelDbResult<()> {
        let tx = self.conn.transaction()?;
        let doc_err = Self::write_document(&tx, row).err();
        let op_err = match doc_err {
            None => Self::write_op(&tx, row.document_id, seq, payload).err(),
            Some(_) => None,
        };
        if doc_err.is_none() && op_err.is_none() {
            tx.commit()?;
            return Ok(());
        }
        tx.rollback()?;
        match doc_err {
            Some(e) => Err(self.classify_document_insert_error(e, row)),
            None => Err(op_err.expect("one of the two writes failed").into()),
        }
    }

    /// Classify an `insert_document` constraint-violation failure by READING
    /// THE DB BACK, never by inspecting the SQLite/kaijutsu error message
    /// text — `map_unique_violation` flattens every UNIQUE conflict into the
    /// same `LabelConflict` string, which makes a benign PRIMARY KEY
    /// duplicate indistinguishable from other constraint violations by
    /// message text alone.
    ///
    /// FK violations (extended code 787) and anything that isn't a
    /// constraint violation delegate straight to `map_unique_violation`,
    /// preserving today's `Validation` mapping exactly.
    pub(super) fn classify_document_insert_error(
        &self,
        e: rusqlite::Error,
        row: &DocumentRow,
    ) -> KernelDbError {
        let is_constraint_violation = matches!(
            &e,
            rusqlite::Error::SqliteFailure(err, _)
                if err.code == rusqlite::ErrorCode::ConstraintViolation
                    && err.extended_code != 787
        );
        if !is_constraint_violation {
            return map_unique_violation(e, "document already exists or path conflict");
        }

        // Which constraint fired? Read back, don't guess from the message.
        match self.get_document(row.document_id) {
            Ok(Some(_)) => return KernelDbError::DuplicateDocument(row.document_id),
            Ok(None) => {}
            Err(read_err) => return read_err,
        }
        // The read-back doesn't explain the failure — don't fabricate a
        // classification, fall through to the existing generic mapping.
        map_unique_violation(e, "document already exists or path conflict")
    }

    /// Insert a document, ignoring if it already exists (idempotent).
    pub fn insert_document_or_ignore(&self, row: &DocumentRow) -> KernelDbResult<()> {
        Self::write_document_or_ignore(&self.conn, row)
    }

    /// Insert-or-ignore a document row against `conn` (a `&Connection` or a
    /// `&Transaction` via deref). Shared by `insert_document_or_ignore` and
    /// the transactional `insert_forked_context`. Does NOT commit.
    pub(super) fn write_document_or_ignore(conn: &Connection, row: &DocumentRow) -> KernelDbResult<()> {
        conn.execute(
            "INSERT OR IGNORE INTO documents (
                document_id, workspace_id, doc_kind,
                language, path, created_at, created_by
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                blob_param(row.document_id.as_bytes()),
                blob_param(row.workspace_id.as_bytes()),
                row.doc_kind.as_str(),
                row.language,
                row.path,
                row.created_at,
                blob_param(row.created_by.as_bytes()),
            ],
        )?;
        Ok(())
    }

    /// Get a document by ID.
    pub fn get_document(&self, id: ContextId) -> KernelDbResult<Option<DocumentRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT document_id, workspace_id, doc_kind,
                    language, path, created_at, created_by
             FROM documents WHERE document_id = ?1",
        )?;
        let mut rows = stmt.query(params![blob_param(id.as_bytes())])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_document_row(row)?))
        } else {
            Ok(None)
        }
    }

    /// List all documents in this kernel.
    pub fn list_documents(&self) -> KernelDbResult<Vec<DocumentRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT document_id, workspace_id, doc_kind,
                    language, path, created_at, created_by
             FROM documents
             ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], row_to_document_row)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// List documents filtered by kind.
    pub fn list_documents_by_kind(&self, kind: DocKind) -> KernelDbResult<Vec<DocumentRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT document_id, workspace_id, doc_kind,
                    language, path, created_at, created_by
             FROM documents WHERE doc_kind = ?1
             ORDER BY created_at",
        )?;
        let rows = stmt.query_map(params![kind.as_str()], row_to_document_row)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Delete a document that has no context. Context history is retained.
    pub fn delete_document(&self, id: ContextId) -> KernelDbResult<bool> {
        let tx = self.conn.unchecked_transaction()?;
        if self.get_context(id)?.is_some() {
            return Err(KernelDbError::Validation(format!(
                "context history is retained; use `kj context archive {id}`"
            )));
        }
        let deleted = tx.execute(
            "DELETE FROM documents WHERE document_id = ?1",
            params![blob_param(id.as_bytes())],
        )?;
        tx.commit()?;
        Ok(deleted > 0)
    }

    /// Append an op to the journal for a document.
    pub fn append_op(
        &self,
        document_id: ContextId,
        seq: i64,
        payload: &[u8],
    ) -> KernelDbResult<()> {
        Self::write_op(&self.conn, document_id, seq, payload)?;
        Ok(())
    }

    /// Append an op row against `conn` (a `&Connection` or a `&Transaction`
    /// via deref). Shared by `append_op` and the transactional
    /// `insert_document_with_op`. Does NOT commit.
    pub(super) fn write_op(
        conn: &Connection,
        document_id: ContextId,
        seq: i64,
        payload: &[u8],
    ) -> SqliteResult<()> {
        conn.execute(
            "INSERT INTO oplog (document_id, seq, payload) VALUES (?1, ?2, ?3)",
            params![blob_param(document_id.as_bytes()), seq, payload],
        )?;
        Ok(())
    }

    /// Store the byte-exact original an ingest transform consumed for this
    /// block (docs/ansi-and-beyond.md). `OR REPLACE`: originals are
    /// write-once in practice, but re-running an ingest hook on the same
    /// block must not fail — last write wins, and both writes carry the same
    /// bytes by construction (the hook writes what it just stripped).
    pub fn insert_block_provenance(
        &self,
        block_id: &BlockId,
        transform: &str,
        version: u32,
        original: &[u8],
    ) -> KernelDbResult<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO block_provenance
                 (context_id, principal_id, block_seq, transform, version, original)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                blob_param(block_id.context_id.as_bytes()),
                blob_param(block_id.principal_id.as_bytes()),
                block_id.seq as i64,
                transform,
                version,
                original,
            ],
        )?;
        Ok(())
    }

    /// Fetch the stored original for one (block, transform), or `None` — a
    /// block whose `provenance` tag names a transform with no row here is a
    /// detectable gap (crash between the block write and this row), and
    /// callers report it rather than inventing bytes.
    pub fn get_block_provenance(
        &self,
        block_id: &BlockId,
        transform: &str,
    ) -> KernelDbResult<Option<(u32, Vec<u8>)>> {
        let row = self
            .conn
            .query_row(
                "SELECT version, original FROM block_provenance
                 WHERE context_id = ?1 AND principal_id = ?2
                   AND block_seq = ?3 AND transform = ?4",
                params![
                    blob_param(block_id.context_id.as_bytes()),
                    blob_param(block_id.principal_id.as_bytes()),
                    block_id.seq as i64,
                    transform,
                ],
                |row| Ok((row.get::<_, u32>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// List the transforms holding an original for this block:
    /// `(transform, version, original_len)`. The bytes stay in the store —
    /// this is the cheap "what could reproject?" question.
    pub fn list_block_provenance(
        &self,
        block_id: &BlockId,
    ) -> KernelDbResult<Vec<(String, u32, usize)>> {
        let mut stmt = self.conn.prepare(
            "SELECT transform, version, length(original) FROM block_provenance
             WHERE context_id = ?1 AND principal_id = ?2 AND block_seq = ?3
             ORDER BY transform",
        )?;
        let rows = stmt
            .query_map(
                params![
                    blob_param(block_id.context_id.as_bytes()),
                    blob_param(block_id.principal_id.as_bytes()),
                    block_id.seq as i64,
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, i64>(2)? as usize,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Load oplog entries after a given seq (for replay after snapshot restore).
    pub fn load_oplog_since(
        &self,
        document_id: ContextId,
        after_seq: i64,
    ) -> KernelDbResult<Vec<(i64, Vec<u8>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, payload FROM oplog
             WHERE document_id = ?1 AND seq > ?2
             ORDER BY seq",
        )?;
        let rows = stmt.query_map(
            params![blob_param(document_id.as_bytes()), after_seq],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Load the latest compaction snapshot for a document.
    pub fn load_latest_snapshot(
        &self,
        document_id: ContextId,
    ) -> KernelDbResult<Option<DocSnapshotRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT document_id, seq, version, state, created_at
             FROM doc_snapshots WHERE document_id = ?1",
        )?;
        let mut rows = stmt.query(params![blob_param(document_id.as_bytes())])?;
        if let Some(row) = rows.next()? {
            Ok(Some(DocSnapshotRow {
                document_id: read_context_id(row, 0)?,
                seq: row.get(1)?,
                version: row.get(2)?,
                state: row.get(3)?,
                created_at: row.get(4)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// Write a compaction snapshot and truncate the oplog up to that seq.
    /// Joins an existing transaction, or commits both writes in its own.
    pub fn write_snapshot_and_truncate(
        &self,
        document_id: ContextId,
        seq: i64,
        version: i64,
        state: &[u8],
    ) -> KernelDbResult<()> {
        if self.conn.is_autocommit() {
            return self.in_transaction(|db| db.write_snapshot_and_truncate(document_id, seq, version, state));
        }
        self.conn.execute(
            "INSERT OR REPLACE INTO doc_snapshots (document_id, seq, version, state)
             VALUES (?1, ?2, ?3, ?4)",
            params![blob_param(document_id.as_bytes()), seq, version, state],
        )?;
        self.conn.execute(
            "DELETE FROM oplog WHERE document_id = ?1 AND seq <= ?2",
            params![blob_param(document_id.as_bytes()), seq],
        )?;
        Ok(())
    }

    /// Delete one oplog row, simulating the two states `block_content_at_seq`
    /// must refuse to paper over: a journal write that has claimed its seq but
    /// not yet committed (the TOCTOU window in `journal_op`), and a row lost
    /// to a failed `append_op`. There is no public API that produces a gapped
    /// oplog on purpose — that's the point. Never compiled outside test builds.
    #[cfg(test)]
    pub(crate) fn delete_oplog_row_for_test(
        &self,
        document_id: ContextId,
        seq: i64,
    ) -> KernelDbResult<()> {
        self.conn.execute(
            "DELETE FROM oplog WHERE document_id = ?1 AND seq = ?2",
            params![blob_param(document_id.as_bytes()), seq],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
