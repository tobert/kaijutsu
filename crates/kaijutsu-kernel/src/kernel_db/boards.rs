//! Boards and topics (`docs/board.md`).
//!
//! A board is a shared container of topics owned by either a character
//! (`owner_character IS NOT NULL`, `slug IS NOT NULL`) or a context
//! (`owner_character IS NULL`, `slug IS NULL`, referenced by `contexts.board_id`).
//! Each topic is a `DocKind::Topic` document in `documents` paired with a
//! `board_topics` row.

use rusqlite::types::Value as SqlValue;

use super::*;

/// A row in `boards`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardRow {
    pub board_id: BoardId,
    /// Owning character for a character board, or `None` for a context board.
    pub owner_character: Option<PrincipalId>,
    /// URL-safe slug for a character board, or `None` for a context board.
    pub slug: Option<String>,
    pub description: String,
    pub write_policy: BoardWritePolicy,
    pub created_by: PrincipalId,
    pub created_at: i64,
    pub archived_at: Option<i64>,
}

/// A row in `board_topics`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardTopicRow {
    pub document_id: ContextId,
    pub board_id: BoardId,
    pub slug: String,
    pub title: String,
    pub status: TopicStatus,
    pub locked: bool,
    pub tags: Vec<String>,
    pub promoted_from: Option<ContextId>,
    pub created_by: PrincipalId,
    pub created_at: i64,
    pub last_post_at: i64,
    pub post_count: i64,
}

const BOARD_SELECT: &str =
    "SELECT board_id, owner_character, slug, description, write_policy, created_by, created_at, archived_at
     FROM boards";

const TOPIC_SELECT: &str =
    "SELECT document_id, board_id, slug, title, status, locked, tags, promoted_from,
            created_by, created_at, last_post_at, post_count
     FROM board_topics";

fn board_write_policy_from_sql(s: &str, idx: usize) -> SqliteResult<BoardWritePolicy> {
    BoardWritePolicy::from_str(s).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Text,
            format!("unknown BoardWritePolicy '{s}'").into(),
        )
    })
}

fn topic_status_from_sql(s: &str, idx: usize) -> SqliteResult<TopicStatus> {
    TopicStatus::from_str(s).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Text,
            format!("unknown TopicStatus '{s}'").into(),
        )
    })
}

fn row_to_board_row(row: &rusqlite::Row<'_>) -> SqliteResult<BoardRow> {
    let policy_str: String = row.get(4)?;
    Ok(BoardRow {
        board_id: read_board_id(row, 0)?,
        owner_character: read_opt_principal_id(row, 1)?,
        slug: row.get(2)?,
        description: row.get(3)?,
        write_policy: board_write_policy_from_sql(&policy_str, 4)?,
        created_by: read_principal_id(row, 5)?,
        created_at: row.get(6)?,
        archived_at: row.get(7)?,
    })
}

fn row_to_topic_row(row: &rusqlite::Row<'_>) -> SqliteResult<BoardTopicRow> {
    let status_str: String = row.get(4)?;
    let locked_int: i64 = row.get(5)?;
    let tags_json: String = row.get(6)?;
    let tags: Vec<String> = serde_json::from_str(&tags_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            6,
            rusqlite::types::Type::Text,
            Box::new(e),
        )
    })?;
    Ok(BoardTopicRow {
        document_id: read_context_id(row, 0)?,
        board_id: read_board_id(row, 1)?,
        slug: row.get(2)?,
        title: row.get(3)?,
        status: topic_status_from_sql(&status_str, 4)?,
        locked: locked_int != 0,
        tags,
        promoted_from: read_opt_context_id(row, 7)?,
        created_by: read_principal_id(row, 8)?,
        created_at: row.get(9)?,
        last_post_at: row.get(10)?,
        post_count: row.get(11)?,
    })
}

/// Map SQLite constraint failures on `boards` / `board_topics` to typed errors.
/// UNIQUE (2067) -> `LabelConflict`; FOREIGN KEY (787) and CHECK (275) -> `Validation`.
fn map_board_constraint(e: rusqlite::Error, unique_msg: impl Into<String>) -> KernelDbError {
    if let rusqlite::Error::SqliteFailure(err, ref detail) = e
        && err.code == rusqlite::ErrorCode::ConstraintViolation
    {
        match err.extended_code {
            // SQLITE_CONSTRAINT_UNIQUE = 2067
            2067 => return KernelDbError::LabelConflict(unique_msg.into()),
            // SQLITE_CONSTRAINT_FOREIGNKEY = 787, SQLITE_CONSTRAINT_CHECK = 275
            787 | 275 => {
                let detail_str = detail.as_deref().unwrap_or("board constraint violation");
                return KernelDbError::Validation(detail_str.to_string());
            }
            _ => {}
        }
    }
    KernelDbError::Db(e)
}

impl KernelDb {
    /// Insert a new board row.
    pub fn insert_board(&self, row: &BoardRow) -> KernelDbResult<()> {
        self.conn
            .execute(
                "INSERT INTO boards (
                    board_id, owner_character, slug, description,
                    write_policy, created_by, created_at, archived_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    blob_param(row.board_id.as_bytes()),
                    row.owner_character.as_ref().map(|id| id.as_bytes().to_vec()),
                    row.slug,
                    row.description,
                    row.write_policy.as_str(),
                    blob_param(row.created_by.as_bytes()),
                    row.created_at,
                    row.archived_at,
                ],
            )
            .map_err(|e| {
                let msg = match &row.slug {
                    Some(slug) => format!("board slug '{slug}' already in use for this character"),
                    None => format!("board {} already exists", row.board_id.short()),
                };
                map_board_constraint(e, msg)
            })?;
        Ok(())
    }

    /// Fetch a board by ID (live or archived).
    pub fn get_board(&self, board_id: BoardId) -> KernelDbResult<Option<BoardRow>> {
        let sql = format!("{BOARD_SELECT} WHERE board_id = ?1");
        self.conn
            .query_row(&sql, params![blob_param(board_id.as_bytes())], row_to_board_row)
            .optional()
            .map_err(KernelDbError::from)
    }

    /// Fetch a live (non-archived) character board by owner and slug.
    pub fn get_character_board_by_slug(
        &self,
        owner_character: PrincipalId,
        slug: &str,
    ) -> KernelDbResult<Option<BoardRow>> {
        let sql = format!(
            "{BOARD_SELECT} WHERE owner_character = ?1 AND slug = ?2 AND archived_at IS NULL"
        );
        self.conn
            .query_row(
                &sql,
                params![blob_param(owner_character.as_bytes()), slug],
                row_to_board_row,
            )
            .optional()
            .map_err(KernelDbError::from)
    }

    /// List boards owned by a character.
    pub fn list_character_boards(
        &self,
        owner_character: PrincipalId,
        include_archived: bool,
    ) -> KernelDbResult<Vec<BoardRow>> {
        let sql = if include_archived {
            format!("{BOARD_SELECT} WHERE owner_character = ?1 ORDER BY slug, created_at")
        } else {
            format!(
                "{BOARD_SELECT} WHERE owner_character = ?1 AND archived_at IS NULL ORDER BY slug, created_at"
            )
        };
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![blob_param(owner_character.as_bytes())],
            row_to_board_row,
        )?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Return the working board for `context_id`, lazily creating one and
    /// attaching `contexts.board_id` atomically if it is currently `NULL`.
    pub fn ensure_context_board(
        &self,
        context_id: ContextId,
        created_by: PrincipalId,
    ) -> KernelDbResult<BoardRow> {
        let run = |db: &KernelDb| -> KernelDbResult<BoardRow> {
            let ctx = db.get_context(context_id)?.ok_or_else(|| {
                KernelDbError::NotFound(format!("context {}", context_id.short()))
            })?;
            if let Some(board_id) = ctx.board_id {
                return db.get_board(board_id)?.ok_or_else(|| {
                    KernelDbError::Corrupt(format!(
                        "context {} references missing board {}",
                        context_id.short(),
                        board_id.short()
                    ))
                });
            }
            let board = BoardRow {
                board_id: BoardId::new(),
                owner_character: None,
                slug: None,
                description: String::new(),
                write_policy: BoardWritePolicy::Open,
                created_by,
                created_at: now_millis(),
                archived_at: None,
            };
            db.insert_board(&board)?;
            db.set_context_board_id(context_id, board.board_id)?;
            Ok(board)
        };

        if self.conn.is_autocommit() {
            self.in_transaction(run)
        } else {
            self.conn.execute_batch("SAVEPOINT ensure_context_board")?;
            match run(self) {
                Ok(board) => {
                    self.conn
                        .execute_batch("RELEASE SAVEPOINT ensure_context_board")?;
                    Ok(board)
                }
                Err(err) => {
                    self.conn.execute_batch(
                        "ROLLBACK TO SAVEPOINT ensure_context_board; RELEASE SAVEPOINT ensure_context_board",
                    )?;
                    Err(err)
                }
            }
        }
    }

    /// Update a board's write policy and/or description.
    pub fn update_board(
        &self,
        board_id: BoardId,
        policy: Option<BoardWritePolicy>,
        description: Option<&str>,
    ) -> KernelDbResult<()> {
        let updated = match (policy, description) {
            (Some(p), Some(d)) => self.conn.execute(
                "UPDATE boards SET write_policy = ?1, description = ?2 WHERE board_id = ?3",
                params![p.as_str(), d, blob_param(board_id.as_bytes())],
            )?,
            (Some(p), None) => self.conn.execute(
                "UPDATE boards SET write_policy = ?1 WHERE board_id = ?2",
                params![p.as_str(), blob_param(board_id.as_bytes())],
            )?,
            (None, Some(d)) => self.conn.execute(
                "UPDATE boards SET description = ?1 WHERE board_id = ?2",
                params![d, blob_param(board_id.as_bytes())],
            )?,
            (None, None) => {
                if self.get_board(board_id)?.is_none() {
                    return Err(KernelDbError::NotFound(format!(
                        "board {}",
                        board_id.short()
                    )));
                }
                return Ok(());
            }
        };
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!(
                "board {}",
                board_id.short()
            )));
        }
        Ok(())
    }

    /// Archive a board (idempotent: returns `true` if newly archived).
    pub fn archive_board(&self, board_id: BoardId) -> KernelDbResult<bool> {
        let updated = self.conn.execute(
            "UPDATE boards SET archived_at = ?1 WHERE board_id = ?2 AND archived_at IS NULL",
            params![now_millis(), blob_param(board_id.as_bytes())],
        )?;
        Ok(updated > 0)
    }

    /// Archive all live boards owned by `principal_id`. Returns count archived.
    pub fn archive_character_boards(&self, principal_id: PrincipalId) -> KernelDbResult<usize> {
        let updated = self.conn.execute(
            "UPDATE boards SET archived_at = ?1 WHERE owner_character = ?2 AND archived_at IS NULL",
            params![now_millis(), blob_param(principal_id.as_bytes())],
        )?;
        Ok(updated)
    }

    /// Insert a new topic row. The corresponding `DocKind::Topic` document row
    /// must already exist in `documents` (FK enforced).
    pub fn insert_topic(&self, row: &BoardTopicRow) -> KernelDbResult<()> {
        let tags_json = serde_json::to_string(&row.tags)
            .map_err(|e| KernelDbError::Validation(format!("invalid topic tags: {e}")))?;
        self.conn
            .execute(
                "INSERT INTO board_topics (
                    document_id, board_id, slug, title, status, locked,
                    tags, promoted_from, created_by, created_at, last_post_at, post_count
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    blob_param(row.document_id.as_bytes()),
                    blob_param(row.board_id.as_bytes()),
                    row.slug,
                    row.title,
                    row.status.as_str(),
                    i64::from(row.locked),
                    tags_json,
                    row.promoted_from.as_ref().map(|id| id.as_bytes().to_vec()),
                    blob_param(row.created_by.as_bytes()),
                    row.created_at,
                    row.last_post_at,
                    row.post_count,
                ],
            )
            .map_err(|e| {
                map_board_constraint(
                    e,
                    format!(
                        "topic slug '{}' already exists on board {}",
                        row.slug,
                        row.board_id.short()
                    ),
                )
            })?;
        Ok(())
    }

    /// Fetch a topic by `document_id`.
    pub fn get_topic(&self, document_id: ContextId) -> KernelDbResult<Option<BoardTopicRow>> {
        let sql = format!("{TOPIC_SELECT} WHERE document_id = ?1");
        self.conn
            .query_row(
                &sql,
                params![blob_param(document_id.as_bytes())],
                row_to_topic_row,
            )
            .optional()
            .map_err(KernelDbError::from)
    }

    /// Fetch a non-archived topic by board and slug.
    pub fn get_topic_by_slug(
        &self,
        board_id: BoardId,
        slug: &str,
    ) -> KernelDbResult<Option<BoardTopicRow>> {
        let sql = format!(
            "{TOPIC_SELECT} WHERE board_id = ?1 AND slug = ?2 AND status != 'archived'"
        );
        self.conn
            .query_row(
                &sql,
                params![blob_param(board_id.as_bytes()), slug],
                row_to_topic_row,
            )
            .optional()
            .map_err(KernelDbError::from)
    }

    /// Resolve a topic by full `document_id` (32-char hex or hyphenated UUID)
    /// or a prefix of its entropy-tail `short()` form. Non-archived topics are
    /// searched first; if none match, archived topics are checked.
    pub fn resolve_topic_prefix(&self, hex_prefix: &str) -> KernelDbResult<BoardTopicRow> {
        if let Ok(full) = ContextId::parse(hex_prefix)
            && let Some(row) = self.get_topic(full)?
        {
            return Ok(row);
        }

        let map_err = |e: kaijutsu_types::PrefixError| match e {
            kaijutsu_types::PrefixError::NoMatch(q) => {
                KernelDbError::NotFound(format!("no topic matches '{q}'"))
            }
            kaijutsu_types::PrefixError::Ambiguous { prefix, candidates } => {
                KernelDbError::Validation(format!(
                    "ambiguous topic prefix '{prefix}': matches {candidates:?}"
                ))
            }
        };

        let mut stmt = self.conn.prepare(
            "SELECT document_id, status FROM board_topics ORDER BY last_post_at DESC",
        )?;
        let all: Vec<(ContextId, TopicStatus)> = stmt
            .query_map([], |row| {
                let status_str: String = row.get(1)?;
                Ok((
                    read_context_id(row, 0)?,
                    topic_status_from_sql(&status_str, 1)?,
                ))
            })?
            .collect::<SqliteResult<Vec<_>>>()?;

        let active_items = all
            .iter()
            .filter(|(_, s)| *s != TopicStatus::Archived)
            .map(|(id, _)| (*id, None));
        let matched = match kaijutsu_types::resolve_prefix(active_items, hex_prefix) {
            Ok(id) => id,
            Err(kaijutsu_types::PrefixError::NoMatch(_)) => {
                let archived_items = all
                    .iter()
                    .filter(|(_, s)| *s == TopicStatus::Archived)
                    .map(|(id, _)| (*id, None));
                kaijutsu_types::resolve_prefix(archived_items, hex_prefix).map_err(map_err)?
            }
            Err(e) => return Err(map_err(e)),
        };

        self.get_topic(matched)?
            .ok_or_else(|| KernelDbError::NotFound(format!("topic {}", matched.short())))
    }

    /// List topics on a board with optional status, tag, and limit filters.
    /// Pinned topics sort first, then most recently posted (`last_post_at DESC`).
    pub fn list_topics(
        &self,
        board_id: BoardId,
        status_filter: Option<TopicStatus>,
        tag_filter: Option<&str>,
        limit: Option<usize>,
    ) -> KernelDbResult<Vec<BoardTopicRow>> {
        let mut sql = format!("{TOPIC_SELECT} WHERE board_id = ?1");
        let mut bound: Vec<SqlValue> = vec![SqlValue::Blob(board_id.as_bytes().to_vec())];

        if let Some(status) = status_filter {
            bound.push(SqlValue::Text(status.as_str().to_string()));
            sql.push_str(&format!(" AND status = ?{}", bound.len()));
        }
        if let Some(tag) = tag_filter {
            bound.push(SqlValue::Text(tag.to_string()));
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM json_each(tags) WHERE value = ?{})",
                bound.len()
            ));
        }
        sql.push_str(
            " ORDER BY CASE WHEN status = 'pinned' THEN 0 ELSE 1 END, last_post_at DESC, document_id DESC",
        );
        if let Some(lim) = limit {
            bound.push(SqlValue::Integer(lim as i64));
            sql.push_str(&format!(" LIMIT ?{}", bound.len()));
        }

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(bound), row_to_topic_row)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// List recent non-archived topics across all boards or one board.
    pub fn list_recent_topics(
        &self,
        board_filter: Option<BoardId>,
        since_ms: Option<i64>,
        limit: Option<usize>,
    ) -> KernelDbResult<Vec<BoardTopicRow>> {
        let mut sql = format!("{TOPIC_SELECT} WHERE status != 'archived'");
        let mut bound: Vec<SqlValue> = Vec::new();

        if let Some(board_id) = board_filter {
            bound.push(SqlValue::Blob(board_id.as_bytes().to_vec()));
            sql.push_str(&format!(" AND board_id = ?{}", bound.len()));
        }
        if let Some(since) = since_ms {
            bound.push(SqlValue::Integer(since));
            sql.push_str(&format!(" AND last_post_at >= ?{}", bound.len()));
        }
        sql.push_str(" ORDER BY last_post_at DESC, document_id DESC");
        if let Some(lim) = limit {
            bound.push(SqlValue::Integer(lim as i64));
            sql.push_str(&format!(" LIMIT ?{}", bound.len()));
        }

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(bound), row_to_topic_row)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// List `document_id`s of non-archived topics (for semantic search filtering),
    /// optionally scoped to a single board.
    pub fn list_topic_ids(&self, board_filter: Option<BoardId>) -> KernelDbResult<Vec<ContextId>> {
        match board_filter {
            Some(board_id) => {
                let mut stmt = self.conn.prepare(
                    "SELECT document_id FROM board_topics
                     WHERE board_id = ?1 AND status != 'archived'
                     ORDER BY last_post_at DESC, document_id DESC",
                )?;
                let rows = stmt.query_map(params![blob_param(board_id.as_bytes())], |row| {
                    read_context_id(row, 0)
                })?;
                Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT document_id FROM board_topics
                     WHERE status != 'archived'
                     ORDER BY last_post_at DESC, document_id DESC",
                )?;
                let rows = stmt.query_map([], |row| read_context_id(row, 0))?;
                Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
            }
        }
    }

    /// Increment `post_count` and advance `last_post_at` when a reply lands.
    pub fn bump_topic_post_stats(
        &self,
        document_id: ContextId,
        now_ms: i64,
    ) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE board_topics
                SET post_count = post_count + 1,
                    last_post_at = MAX(last_post_at, ?1)
             WHERE document_id = ?2",
            params![now_ms, blob_param(document_id.as_bytes())],
        )?;
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!(
                "topic {}",
                document_id.short()
            )));
        }
        Ok(())
    }

    /// Update `post_count` and advance `last_post_at` when a held post is released.
    pub fn update_topic_on_post_release(
        &self,
        document_id: ContextId,
        visible_post_count: i64,
        now_ms: i64,
    ) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE board_topics
                SET post_count = MAX(?1, 1),
                    last_post_at = MAX(last_post_at, ?2)
             WHERE document_id = ?3",
            params![visible_post_count, now_ms, blob_param(document_id.as_bytes())],
        )?;
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!(
                "topic {}",
                document_id.short()
            )));
        }
        Ok(())
    }

    /// Update a topic's status.
    pub fn set_topic_status(
        &self,
        document_id: ContextId,
        status: TopicStatus,
    ) -> KernelDbResult<()> {
        let updated = self
            .conn
            .execute(
                "UPDATE board_topics SET status = ?1 WHERE document_id = ?2",
                params![status.as_str(), blob_param(document_id.as_bytes())],
            )
            .map_err(|e| {
                map_board_constraint(
                    e,
                    format!(
                        "another non-archived topic on this board already uses the same slug as {}",
                        document_id.short()
                    ),
                )
            })?;
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!(
                "topic {}",
                document_id.short()
            )));
        }
        Ok(())
    }

    /// Lock or unlock a topic.
    pub fn set_topic_locked(
        &self,
        document_id: ContextId,
        locked: bool,
    ) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE board_topics SET locked = ?1 WHERE document_id = ?2",
            params![i64::from(locked), blob_param(document_id.as_bytes())],
        )?;
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!(
                "topic {}",
                document_id.short()
            )));
        }
        Ok(())
    }

    /// Return all contexts pointing to `board_id`, live contexts first.
    pub fn contexts_for_board(&self, board_id: BoardId) -> KernelDbResult<Vec<ContextRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, label,
                    system_prompt, context_state,
                    created_at, created_by, forked_from, fork_kind,
                    archived_at, workspace_id, preset_id, concluded_at,
                    last_activity_at, promoted_at, demoted_at, paused_at,
                    origin_host, performer, senpai_ctx, persona, board_id
             FROM contexts
             WHERE board_id = ?1
             ORDER BY CASE WHEN archived_at IS NULL THEN 0 ELSE 1 END, created_at DESC",
        )?;
        let rows = stmt.query_map(params![blob_param(board_id.as_bytes())], row_to_context_row)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// True if `candidate_principal` or `candidate_ctx` appears strictly above
    /// `target_context` on its `senpai_ctx` chain.
    pub fn is_on_senpai_chain_of_context(
        &self,
        candidate_principal: PrincipalId,
        candidate_ctx: Option<ContextId>,
        target_context: ContextId,
    ) -> KernelDbResult<bool> {
        let mut visited = HashSet::from([target_context]);
        let mut at = self.senpai_of(target_context)?;
        while let Some(senpai_id) = at {
            if !visited.insert(senpai_id) {
                break;
            }
            if candidate_ctx == Some(senpai_id) {
                return Ok(true);
            }
            let Some(row) = self.get_context(senpai_id)? else {
                break;
            };
            if row.performer == Some(candidate_principal) {
                return Ok(true);
            }
            at = row.senpai_ctx;
        }
        Ok(false)
    }

    /// True if `candidate_principal` or `candidate_ctx` appears on the
    /// `senpai_ctx` chain above any live context performed by `target_character`.
    pub fn is_on_senpai_chain_of_character(
        &self,
        candidate_principal: PrincipalId,
        candidate_ctx: Option<ContextId>,
        target_character: PrincipalId,
    ) -> KernelDbResult<bool> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id FROM contexts WHERE performer = ?1 AND archived_at IS NULL",
        )?;
        let ctx_ids: Vec<ContextId> = stmt
            .query_map(params![blob_param(target_character.as_bytes())], |row| {
                read_context_id(row, 0)
            })?
            .collect::<SqliteResult<Vec<_>>>()?;
        for ctx_id in ctx_ids {
            if self.is_on_senpai_chain_of_context(candidate_principal, candidate_ctx, ctx_id)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests;
