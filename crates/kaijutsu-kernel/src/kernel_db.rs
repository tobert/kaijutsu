//! SQLite persistence for kernel context metadata, edges, presets, and workspaces.
//!
//! Pattern follows `auth_db.rs` and `db.rs`: single `Connection`, WAL mode,
//! BLOB-encoded typed IDs, in-memory constructor for tests.
//!
//! All timestamps are Unix milliseconds (matching `now_millis()`).

use std::collections::HashSet;
use std::path::Path;
use std::str::FromStr;

use rusqlite::{params, Connection, Result as SqliteResult};
use tracing::{info, warn};

use kaijutsu_types::{
    ConsentMode, ContextId, EdgeKind, ForkKind, KernelId, PresetId, PrincipalId, ToolFilter,
    WorkspaceId,
};

// ============================================================================
// Error type
// ============================================================================

/// Errors from KernelDb operations.
#[derive(Debug, thiserror::Error)]
pub enum KernelDbError {
    /// Underlying SQLite error.
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    /// Entity not found.
    #[error("not found: {0}")]
    NotFound(String),

    /// Label already in use for this kernel.
    #[error("label conflict: {0}")]
    LabelConflict(String),

    /// Label contains forbidden characters.
    #[error("invalid label: {0}")]
    InvalidLabel(String),

    /// Structural edge would create a cycle.
    #[error("cycle detected: adding this edge would create a cycle")]
    CycleDetected,

    /// General validation failure.
    #[error("validation error: {0}")]
    Validation(String),
}

pub type KernelDbResult<T> = Result<T, KernelDbError>;

// ============================================================================
// Row types
// ============================================================================

/// A context row — superset of `Context` + `ContextHandle`.
#[derive(Debug, Clone)]
pub struct ContextRow {
    pub context_id: ContextId,
    pub kernel_id: KernelId,
    pub label: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub tool_filter: Option<ToolFilter>,
    pub consent_mode: ConsentMode,
    pub created_at: i64,
    pub created_by: PrincipalId,
    pub forked_from: Option<ContextId>,
    pub fork_kind: Option<ForkKind>,
    pub archived_at: Option<i64>,
    pub workspace_id: Option<WorkspaceId>,
    pub preset_id: Option<PresetId>,
}

impl ContextRow {
    /// Convert to the lightweight `kaijutsu_types::Context`.
    pub fn to_context(&self) -> kaijutsu_types::Context {
        kaijutsu_types::Context {
            id: self.context_id,
            kernel_id: self.kernel_id,
            label: self.label.clone(),
            forked_from: self.forked_from,
            created_by: self.created_by,
            created_at: self.created_at as u64,
        }
    }
}

/// A preset template row.
#[derive(Debug, Clone)]
pub struct PresetRow {
    pub preset_id: PresetId,
    pub kernel_id: KernelId,
    pub label: String,
    pub description: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub tool_filter: Option<ToolFilter>,
    pub consent_mode: ConsentMode,
    pub created_at: i64,
    pub created_by: PrincipalId,
}

/// A workspace row.
#[derive(Debug, Clone)]
pub struct WorkspaceRow {
    pub workspace_id: WorkspaceId,
    pub kernel_id: KernelId,
    pub label: String,
    pub description: Option<String>,
    pub created_at: i64,
    pub created_by: PrincipalId,
    pub archived_at: Option<i64>,
}

/// A workspace path row.
#[derive(Debug, Clone)]
pub struct WorkspacePathRow {
    pub workspace_id: WorkspaceId,
    pub path: String,
    pub read_only: bool,
    pub created_at: i64,
}

/// Per-context shell configuration.
#[derive(Debug, Clone)]
pub struct ContextShellRow {
    pub context_id: ContextId,
    pub cwd: Option<String>,
    pub init_script: Option<String>,
    pub updated_at: i64,
}

/// Per-context environment variable.
#[derive(Debug, Clone)]
pub struct ContextEnvRow {
    pub context_id: ContextId,
    pub key: String,
    pub value: String,
}

/// A context edge row (structural or drift).
#[derive(Debug, Clone)]
pub struct ContextEdgeRow {
    pub edge_id: uuid::Uuid,
    pub source_id: ContextId,
    pub target_id: ContextId,
    pub kind: EdgeKind,
    pub metadata: Option<String>,
    pub created_at: i64,
}

// ============================================================================
// Schema
// ============================================================================

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS contexts (
    context_id   BLOB NOT NULL PRIMARY KEY,
    kernel_id    BLOB NOT NULL,
    label        TEXT,
    provider     TEXT,
    model        TEXT,
    system_prompt TEXT,
    tool_filter  TEXT,
    consent_mode TEXT NOT NULL DEFAULT 'collaborative',
    created_at   INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    created_by   BLOB NOT NULL,
    forked_from  BLOB REFERENCES contexts(context_id) ON DELETE SET NULL,
    fork_kind    TEXT,
    archived_at  INTEGER,
    workspace_id BLOB REFERENCES workspaces(workspace_id) ON DELETE SET NULL,
    preset_id    BLOB REFERENCES presets(preset_id) ON DELETE SET NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_contexts_label
    ON contexts(kernel_id, label) WHERE label IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_contexts_kernel
    ON contexts(kernel_id);

CREATE INDEX IF NOT EXISTS idx_contexts_workspace
    ON contexts(workspace_id) WHERE workspace_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS context_edges (
    edge_id    BLOB NOT NULL PRIMARY KEY,
    source_id  BLOB NOT NULL REFERENCES contexts(context_id) ON DELETE CASCADE,
    target_id  BLOB NOT NULL REFERENCES contexts(context_id) ON DELETE CASCADE,
    kind       TEXT NOT NULL DEFAULT 'structural',
    metadata   TEXT,
    created_at INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER))
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_edges_structural_unique
    ON context_edges(source_id, target_id) WHERE kind = 'structural';

CREATE INDEX IF NOT EXISTS idx_edges_source
    ON context_edges(source_id);

CREATE INDEX IF NOT EXISTS idx_edges_target
    ON context_edges(target_id);

CREATE TABLE IF NOT EXISTS presets (
    preset_id    BLOB NOT NULL PRIMARY KEY,
    kernel_id    BLOB NOT NULL,
    label        TEXT NOT NULL,
    description  TEXT,
    provider     TEXT,
    model        TEXT,
    system_prompt TEXT,
    tool_filter  TEXT,
    consent_mode TEXT NOT NULL DEFAULT 'collaborative',
    created_at   INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    created_by   BLOB NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_presets_label
    ON presets(kernel_id, label);

CREATE TABLE IF NOT EXISTS workspaces (
    workspace_id BLOB NOT NULL PRIMARY KEY,
    kernel_id    BLOB NOT NULL,
    label        TEXT NOT NULL,
    description  TEXT,
    created_at   INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    created_by   BLOB NOT NULL,
    archived_at  INTEGER
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_workspaces_label
    ON workspaces(kernel_id, label);

CREATE TABLE IF NOT EXISTS workspace_paths (
    workspace_id BLOB NOT NULL REFERENCES workspaces(workspace_id) ON DELETE CASCADE,
    path         TEXT NOT NULL,
    read_only    INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    PRIMARY KEY (workspace_id, path)
);

CREATE TABLE IF NOT EXISTS context_shell (
    context_id  BLOB NOT NULL PRIMARY KEY REFERENCES contexts(context_id) ON DELETE CASCADE,
    cwd         TEXT,
    init_script TEXT,
    updated_at  INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER))
);

CREATE TABLE IF NOT EXISTS context_env (
    context_id BLOB NOT NULL REFERENCES contexts(context_id) ON DELETE CASCADE,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    PRIMARY KEY (context_id, key)
);

CREATE INDEX IF NOT EXISTS idx_ctx_env ON context_env(context_id);

CREATE TABLE IF NOT EXISTS kernel (
    kernel_id  BLOB NOT NULL PRIMARY KEY,
    created_at INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER))
);
"#;

// ============================================================================
// BLOB helpers
// ============================================================================

fn blob_param(id: &[u8; 16]) -> &[u8] {
    id.as_slice()
}

fn read_context_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<ContextId> {
    let bytes: Vec<u8> = row.get(idx)?;
    ContextId::try_from_slice(&bytes).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Blob,
            "invalid ContextId bytes".into(),
        )
    })
}

fn read_opt_context_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<Option<ContextId>> {
    let bytes: Option<Vec<u8>> = row.get(idx)?;
    match bytes {
        Some(b) => ContextId::try_from_slice(&b)
            .map(Some)
            .ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    idx,
                    rusqlite::types::Type::Blob,
                    "invalid ContextId bytes".into(),
                )
            }),
        None => Ok(None),
    }
}

fn read_kernel_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<KernelId> {
    let bytes: Vec<u8> = row.get(idx)?;
    KernelId::try_from_slice(&bytes).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Blob,
            "invalid KernelId bytes".into(),
        )
    })
}

fn read_principal_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<PrincipalId> {
    let bytes: Vec<u8> = row.get(idx)?;
    PrincipalId::try_from_slice(&bytes).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Blob,
            "invalid PrincipalId bytes".into(),
        )
    })
}

fn read_opt_workspace_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<Option<WorkspaceId>> {
    let bytes: Option<Vec<u8>> = row.get(idx)?;
    match bytes {
        Some(b) => WorkspaceId::try_from_slice(&b)
            .map(Some)
            .ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    idx,
                    rusqlite::types::Type::Blob,
                    "invalid WorkspaceId bytes".into(),
                )
            }),
        None => Ok(None),
    }
}

fn read_opt_preset_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<Option<PresetId>> {
    let bytes: Option<Vec<u8>> = row.get(idx)?;
    match bytes {
        Some(b) => PresetId::try_from_slice(&b)
            .map(Some)
            .ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    idx,
                    rusqlite::types::Type::Blob,
                    "invalid PresetId bytes".into(),
                )
            }),
        None => Ok(None),
    }
}

fn read_workspace_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<WorkspaceId> {
    let bytes: Vec<u8> = row.get(idx)?;
    WorkspaceId::try_from_slice(&bytes).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Blob,
            "invalid WorkspaceId bytes".into(),
        )
    })
}

fn read_preset_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<PresetId> {
    let bytes: Vec<u8> = row.get(idx)?;
    PresetId::try_from_slice(&bytes).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Blob,
            "invalid PresetId bytes".into(),
        )
    })
}

fn read_edge_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<uuid::Uuid> {
    let bytes: Vec<u8> = row.get(idx)?;
    if bytes.len() == 16 {
        let mut arr = [0u8; 16];
        arr.copy_from_slice(&bytes);
        Ok(uuid::Uuid::from_bytes(arr))
    } else {
        Err(rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Blob,
            "invalid UUID bytes".into(),
        ))
    }
}

/// Serialize ToolFilter to JSON TEXT for SQLite.
fn tool_filter_to_sql(tf: &Option<ToolFilter>) -> Option<String> {
    tf.as_ref().map(|f| serde_json::to_string(f).unwrap_or_default())
}

/// Deserialize ToolFilter from JSON TEXT.
fn tool_filter_from_sql(s: Option<String>) -> Option<ToolFilter> {
    s.and_then(|json| serde_json::from_str(&json).ok())
}

/// Parse ConsentMode from TEXT column.
fn consent_mode_from_sql(s: &str) -> ConsentMode {
    ConsentMode::from_str(s).unwrap_or_else(|_| {
        warn!(mode = %s, "unknown ConsentMode in DB, defaulting to Collaborative");
        ConsentMode::Collaborative
    })
}

/// Parse ForkKind from TEXT column.
fn fork_kind_from_sql(s: Option<String>) -> Option<ForkKind> {
    s.and_then(|v| ForkKind::from_str(&v).ok())
}

/// Parse EdgeKind from TEXT column.
fn edge_kind_from_sql(s: &str) -> EdgeKind {
    EdgeKind::from_str(s).unwrap_or_else(|_| {
        warn!(kind = %s, "unknown EdgeKind in DB, defaulting to Structural");
        EdgeKind::Structural
    })
}

/// Current time as Unix milliseconds.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Validate a label: no colons allowed (reserved for tag:prefix syntax).
fn validate_label(label: &str) -> KernelDbResult<()> {
    if label.contains(':') {
        return Err(KernelDbError::InvalidLabel(
            format!("label '{}' must not contain ':'", label),
        ));
    }
    if label.is_empty() {
        return Err(KernelDbError::InvalidLabel(
            "label must not be empty".to_string(),
        ));
    }
    Ok(())
}

/// Map constraint violations to typed errors.
///
/// SQLite extended error codes distinguish UNIQUE (2067) from FOREIGN KEY (787).
/// We use `ConstraintViolation` as the primary code for both, so we check the
/// extended code to produce the right error variant.
fn map_unique_violation(e: rusqlite::Error, msg: impl Into<String>) -> KernelDbError {
    if let rusqlite::Error::SqliteFailure(err, ref detail) = e {
        if err.code == rusqlite::ErrorCode::ConstraintViolation {
            // SQLITE_CONSTRAINT_FOREIGNKEY = 787
            if err.extended_code == 787 {
                let detail_str = detail.as_deref().unwrap_or("foreign key constraint failed");
                return KernelDbError::Validation(detail_str.to_string());
            }
            // SQLITE_CONSTRAINT_UNIQUE = 2067, or any other constraint
            return KernelDbError::LabelConflict(msg.into());
        }
    }
    KernelDbError::Db(e)
}

// ============================================================================
// KernelDb
// ============================================================================

/// SQLite database for kernel context metadata.
pub struct KernelDb {
    conn: Connection,
}

impl KernelDb {
    fn init_connection(conn: &Connection) -> SqliteResult<()> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )?;
        Ok(())
    }

    /// Open or create at the given path.
    pub fn open<P: AsRef<Path>>(path: P) -> KernelDbResult<Self> {
        if let Some(parent) = path.as_ref().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        Self::init_connection(&conn)?;
        // Create workspaces/presets before contexts (FK refs).
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Create an in-memory database (for testing).
    pub fn in_memory() -> KernelDbResult<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init_connection(&conn)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    // ========================================================================
    // Kernel identity
    // ========================================================================

    /// Get the persisted kernel ID, or create one if this is the first run.
    ///
    /// The kernel table holds a single row. On first open it's empty and we
    /// insert a fresh KernelId. On subsequent startups we return the existing
    /// one so that context rows (which reference kernel_id) remain joinable.
    ///
    /// **Migration:** If the kernel table is empty but contexts already exist
    /// (pre-stable-ID era), adopts the most recent context's kernel_id so
    /// existing context rows remain joinable without data loss.
    pub fn get_or_create_kernel_id(&self) -> KernelDbResult<KernelId> {
        let existing: Option<Vec<u8>> = self.conn.query_row(
            "SELECT kernel_id FROM kernel LIMIT 1",
            [],
            |row| row.get(0),
        ).ok();

        if let Some(bytes) = existing {
            if let Some(id) = KernelId::try_from_slice(&bytes) {
                return Ok(id);
            }
            // Corrupt row — fall through to create fresh
            warn!("Corrupt kernel_id in kernel table, creating fresh");
        }

        // No kernel row yet. Check if contexts exist from a previous run
        // (before the kernel table was added) and adopt their kernel_id.
        let adopted: Option<Vec<u8>> = self.conn.query_row(
            "SELECT kernel_id FROM contexts ORDER BY created_at DESC LIMIT 1",
            [],
            |row| row.get(0),
        ).ok();

        let id = if let Some(bytes) = adopted {
            if let Some(kid) = KernelId::try_from_slice(&bytes) {
                info!("Adopted kernel_id {} from existing contexts", kid.to_hex());
                kid
            } else {
                KernelId::new()
            }
        } else {
            KernelId::new()
        };

        self.conn.execute(
            "INSERT INTO kernel (kernel_id) VALUES (?1)",
            params![blob_param(id.as_bytes())],
        )?;

        // Warn if contexts reference multiple kernel_ids — needs manual cleanup
        let distinct_count: u32 = self.conn.query_row(
            "SELECT COUNT(DISTINCT kernel_id) FROM contexts",
            [],
            |row| row.get(0),
        ).unwrap_or(0);
        if distinct_count > 1 {
            warn!(
                "contexts table has {} distinct kernel_ids — run \
                 UPDATE contexts SET kernel_id = X'{}' WHERE kernel_id != X'{}' \
                 to consolidate",
                distinct_count,
                id.to_hex(),
                id.to_hex(),
            );
        }

        Ok(id)
    }

    // ========================================================================
    // Context CRUD
    // ========================================================================

    /// Insert a new context.
    pub fn insert_context(&self, row: &ContextRow) -> KernelDbResult<()> {
        if let Some(ref label) = row.label {
            validate_label(label)?;
        }

        self.conn.execute(
            "INSERT INTO contexts (
                context_id, kernel_id, label, provider, model,
                system_prompt, tool_filter, consent_mode, created_at,
                created_by, forked_from, fork_kind, archived_at,
                workspace_id, preset_id
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7, ?8, ?9,
                ?10, ?11, ?12, ?13,
                ?14, ?15
            )",
            params![
                blob_param(row.context_id.as_bytes()),
                blob_param(row.kernel_id.as_bytes()),
                row.label,
                row.provider,
                row.model,
                row.system_prompt,
                tool_filter_to_sql(&row.tool_filter),
                row.consent_mode.as_str(),
                row.created_at,
                blob_param(row.created_by.as_bytes()),
                row.forked_from.as_ref().map(|id| id.as_bytes().to_vec()),
                row.fork_kind.map(|fk| fk.as_str().to_string()),
                row.archived_at,
                row.workspace_id.as_ref().map(|id| id.as_bytes().to_vec()),
                row.preset_id.as_ref().map(|id| id.as_bytes().to_vec()),
            ],
        ).map_err(|e| {
            if let Some(ref label) = row.label {
                map_unique_violation(e, format!("label '{}' already in use", label))
            } else {
                KernelDbError::Db(e)
            }
        })?;
        Ok(())
    }

    /// Get a context by ID.
    pub fn get_context(&self, id: ContextId) -> KernelDbResult<Option<ContextRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, kernel_id, label, provider, model,
                    system_prompt, tool_filter, consent_mode, created_at,
                    created_by, forked_from, fork_kind, archived_at,
                    workspace_id, preset_id
             FROM contexts WHERE context_id = ?1",
        )?;

        let mut rows = stmt.query(params![blob_param(id.as_bytes())])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_context_row(row)?))
        } else {
            Ok(None)
        }
    }

    /// Update a context's label.
    pub fn update_label(&self, id: ContextId, label: Option<&str>) -> KernelDbResult<()> {
        if let Some(l) = label {
            validate_label(l)?;
        }

        let updated = self.conn.execute(
            "UPDATE contexts SET label = ?1 WHERE context_id = ?2",
            params![label, blob_param(id.as_bytes())],
        ).map_err(|e| {
            if let Some(l) = label {
                map_unique_violation(e, format!("label '{}' already in use", l))
            } else {
                KernelDbError::Db(e)
            }
        })?;

        if updated == 0 {
            return Err(KernelDbError::NotFound(format!("context {}", id.short())));
        }
        Ok(())
    }

    /// Update a context's model assignment.
    pub fn update_model(
        &self,
        id: ContextId,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE contexts SET provider = ?1, model = ?2 WHERE context_id = ?3",
            params![provider, model, blob_param(id.as_bytes())],
        )?;
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!("context {}", id.short())));
        }
        Ok(())
    }

    /// Update a context's settings.
    pub fn update_settings(
        &self,
        id: ContextId,
        system_prompt: Option<&str>,
        tool_filter: &Option<ToolFilter>,
        consent_mode: ConsentMode,
    ) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE contexts SET system_prompt = ?1, tool_filter = ?2, consent_mode = ?3
             WHERE context_id = ?4",
            params![
                system_prompt,
                tool_filter_to_sql(tool_filter),
                consent_mode.as_str(),
                blob_param(id.as_bytes()),
            ],
        )?;
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!("context {}", id.short())));
        }
        Ok(())
    }

    /// Update a context's tool filter.
    pub fn update_tool_filter(
        &self,
        id: ContextId,
        tool_filter: &Option<ToolFilter>,
    ) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE contexts SET tool_filter = ?1 WHERE context_id = ?2",
            params![
                tool_filter_to_sql(tool_filter),
                blob_param(id.as_bytes()),
            ],
        )?;
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!("context {}", id.short())));
        }
        Ok(())
    }

    /// Update a context's workspace assignment.
    pub fn update_workspace(
        &self,
        id: ContextId,
        ws_id: Option<WorkspaceId>,
    ) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE contexts SET workspace_id = ?1 WHERE context_id = ?2",
            params![
                ws_id.as_ref().map(|id| id.as_bytes().to_vec()),
                blob_param(id.as_bytes()),
            ],
        )?;
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!("context {}", id.short())));
        }
        Ok(())
    }

    /// Archive a context (soft delete). Returns true if it was active.
    pub fn archive_context(&self, id: ContextId) -> KernelDbResult<bool> {
        let now = now_millis() as i64;
        let updated = self.conn.execute(
            "UPDATE contexts SET archived_at = ?1
             WHERE context_id = ?2 AND archived_at IS NULL",
            params![now, blob_param(id.as_bytes())],
        )?;
        Ok(updated > 0)
    }

    /// List active (non-archived) contexts for a kernel.
    pub fn list_active_contexts(&self, kernel_id: KernelId) -> KernelDbResult<Vec<ContextRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, kernel_id, label, provider, model,
                    system_prompt, tool_filter, consent_mode, created_at,
                    created_by, forked_from, fork_kind, archived_at,
                    workspace_id, preset_id
             FROM contexts
             WHERE kernel_id = ?1 AND archived_at IS NULL
             ORDER BY created_at",
        )?;

        let rows = stmt.query_map(params![blob_param(kernel_id.as_bytes())], |row| {
            row_to_context_row(row)
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// List all contexts for a kernel (including archived).
    pub fn list_all_contexts(&self, kernel_id: KernelId) -> KernelDbResult<Vec<ContextRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, kernel_id, label, provider, model,
                    system_prompt, tool_filter, consent_mode, created_at,
                    created_by, forked_from, fork_kind, archived_at,
                    workspace_id, preset_id
             FROM contexts
             WHERE kernel_id = ?1
             ORDER BY created_at",
        )?;

        let rows = stmt.query_map(params![blob_param(kernel_id.as_bytes())], |row| {
            row_to_context_row(row)
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Resolve a context query string within a kernel.
    ///
    /// Supports exact label, label prefix, hex prefix. For tag:prefix syntax
    /// (future), walks lineage via CTE.
    pub fn resolve_context(
        &self,
        kernel_id: KernelId,
        query: &str,
    ) -> KernelDbResult<ContextId> {
        // Load active contexts for this kernel (set is small, <100)
        let contexts = self.list_active_contexts(kernel_id)?;
        let items = contexts
            .iter()
            .map(|c| (c.context_id, c.label.as_deref()));

        kaijutsu_types::resolve_prefix(items, query).map_err(|e| match e {
            kaijutsu_types::PrefixError::NoMatch(q) => {
                KernelDbError::NotFound(format!("no context matches '{}'", q))
            }
            kaijutsu_types::PrefixError::Ambiguous { prefix, candidates } => {
                KernelDbError::Validation(format!(
                    "ambiguous prefix '{}': matches {:?}",
                    prefix, candidates
                ))
            }
        })
    }

    /// Get structural parents of a context.
    pub fn structural_parents(&self, id: ContextId) -> KernelDbResult<Vec<ContextRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.context_id, c.kernel_id, c.label, c.provider, c.model,
                    c.system_prompt, c.tool_filter, c.consent_mode, c.created_at,
                    c.created_by, c.forked_from, c.fork_kind, c.archived_at,
                    c.workspace_id, c.preset_id
             FROM contexts c
             JOIN context_edges e ON e.source_id = c.context_id
             WHERE e.target_id = ?1 AND e.kind = 'structural'",
        )?;

        let rows = stmt.query_map(params![blob_param(id.as_bytes())], |row| {
            row_to_context_row(row)
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Get active structural children of a context.
    pub fn structural_children(&self, id: ContextId) -> KernelDbResult<Vec<ContextRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.context_id, c.kernel_id, c.label, c.provider, c.model,
                    c.system_prompt, c.tool_filter, c.consent_mode, c.created_at,
                    c.created_by, c.forked_from, c.fork_kind, c.archived_at,
                    c.workspace_id, c.preset_id
             FROM contexts c
             JOIN context_edges e ON e.target_id = c.context_id
             WHERE e.source_id = ?1 AND e.kind = 'structural'
               AND c.archived_at IS NULL",
        )?;

        let rows = stmt.query_map(params![blob_param(id.as_bytes())], |row| {
            row_to_context_row(row)
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Walk the full context DAG for a kernel via recursive CTE on structural edges.
    /// Returns `(ContextRow, depth)` pairs.
    pub fn context_dag(&self, kernel_id: KernelId) -> KernelDbResult<Vec<(ContextRow, i64)>> {
        // Find roots: contexts in this kernel with no incoming structural edges.
        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE dag(ctx_id, depth) AS (
                -- Roots: contexts with no incoming structural edges in this kernel
                SELECT c.context_id, 0
                FROM contexts c
                WHERE c.kernel_id = ?1
                  AND c.archived_at IS NULL
                  AND NOT EXISTS (
                      SELECT 1 FROM context_edges e
                      WHERE e.target_id = c.context_id AND e.kind = 'structural'
                  )
                UNION ALL
                -- Children via structural edges
                SELECT e.target_id, dag.depth + 1
                FROM dag
                JOIN context_edges e ON e.source_id = dag.ctx_id AND e.kind = 'structural'
                JOIN contexts c2 ON c2.context_id = e.target_id AND c2.archived_at IS NULL
            )
            SELECT c.context_id, c.kernel_id, c.label, c.provider, c.model,
                   c.system_prompt, c.tool_filter, c.consent_mode, c.created_at,
                   c.created_by, c.forked_from, c.fork_kind, c.archived_at,
                   c.workspace_id, c.preset_id,
                   dag.depth
            FROM dag
            JOIN contexts c ON c.context_id = dag.ctx_id
            ORDER BY dag.depth, c.created_at",
        )?;

        let rows = stmt.query_map(params![blob_param(kernel_id.as_bytes())], |row| {
            let ctx = row_to_context_row(row)?;
            let depth: i64 = row.get(15)?;
            Ok((ctx, depth))
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Walk fork lineage from a context upward via `forked_from`.
    /// Returns `(ContextRow, depth)` where depth 0 = the starting context.
    pub fn fork_lineage(&self, context_id: ContextId) -> KernelDbResult<Vec<(ContextRow, i64)>> {
        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE lineage(ctx_id, depth) AS (
                SELECT ?1, 0
                UNION ALL
                SELECT c.forked_from, lineage.depth + 1
                FROM lineage
                JOIN contexts c ON c.context_id = lineage.ctx_id
                WHERE c.forked_from IS NOT NULL
            )
            SELECT c.context_id, c.kernel_id, c.label, c.provider, c.model,
                   c.system_prompt, c.tool_filter, c.consent_mode, c.created_at,
                   c.created_by, c.forked_from, c.fork_kind, c.archived_at,
                   c.workspace_id, c.preset_id,
                   lineage.depth
            FROM lineage
            JOIN contexts c ON c.context_id = lineage.ctx_id
            ORDER BY lineage.depth",
        )?;

        let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
            let ctx = row_to_context_row(row)?;
            let depth: i64 = row.get(15)?;
            Ok((ctx, depth))
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Snapshot a subtree rooted at `root_id` via structural edges.
    /// Returns `(ContextRow, depth)`.
    pub fn subtree_snapshot(
        &self,
        root_id: ContextId,
    ) -> KernelDbResult<Vec<(ContextRow, i64)>> {
        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE subtree(ctx_id, depth) AS (
                SELECT ?1, 0
                UNION ALL
                SELECT e.target_id, subtree.depth + 1
                FROM subtree
                JOIN context_edges e ON e.source_id = subtree.ctx_id AND e.kind = 'structural'
            )
            SELECT c.context_id, c.kernel_id, c.label, c.provider, c.model,
                   c.system_prompt, c.tool_filter, c.consent_mode, c.created_at,
                   c.created_by, c.forked_from, c.fork_kind, c.archived_at,
                   c.workspace_id, c.preset_id,
                   subtree.depth
            FROM subtree
            JOIN contexts c ON c.context_id = subtree.ctx_id
            ORDER BY subtree.depth, c.created_at",
        )?;

        let rows = stmt.query_map(params![blob_param(root_id.as_bytes())], |row| {
            let ctx = row_to_context_row(row)?;
            let depth: i64 = row.get(15)?;
            Ok((ctx, depth))
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    // ========================================================================
    // Context Edges
    // ========================================================================

    /// Insert an edge. Structural edges get cycle detection.
    pub fn insert_edge(&self, row: &ContextEdgeRow) -> KernelDbResult<()> {
        if row.kind == EdgeKind::Structural {
            // Check for cycle: walk descendants of target — if source appears, reject.
            if self.would_create_cycle(row.source_id, row.target_id)? {
                return Err(KernelDbError::CycleDetected);
            }
        }

        self.conn.execute(
            "INSERT INTO context_edges (edge_id, source_id, target_id, kind, metadata, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                row.edge_id.as_bytes().as_slice(),
                blob_param(row.source_id.as_bytes()),
                blob_param(row.target_id.as_bytes()),
                row.kind.as_str(),
                row.metadata,
                row.created_at,
            ],
        ).map_err(|e| {
            map_unique_violation(e, format!(
                "structural edge {:?} → {:?} already exists",
                row.source_id.short(), row.target_id.short()
            ))
        })?;
        Ok(())
    }

    /// Check if adding source→target structural edge would create a cycle.
    fn would_create_cycle(
        &self,
        source: ContextId,
        target: ContextId,
    ) -> KernelDbResult<bool> {
        if source == target {
            return Ok(true);
        }

        // Walk descendants of target via structural edges.
        let mut visited = HashSet::new();
        let mut stack = vec![target];

        while let Some(node) = stack.pop() {
            if node == source {
                return Ok(true);
            }
            if !visited.insert(node) {
                continue;
            }

            let mut stmt = self.conn.prepare(
                "SELECT target_id FROM context_edges
                 WHERE source_id = ?1 AND kind = 'structural'",
            )?;
            let children: Vec<ContextId> = stmt
                .query_map(params![blob_param(node.as_bytes())], |row| {
                    read_context_id(row, 0)
                })?
                .collect::<SqliteResult<Vec<_>>>()?;

            stack.extend(children);
        }

        Ok(false)
    }

    /// List edges from a source, optionally filtered by kind.
    pub fn edges_from(
        &self,
        source: ContextId,
        kind: Option<EdgeKind>,
    ) -> KernelDbResult<Vec<ContextEdgeRow>> {
        let rows = match kind {
            Some(k) => {
                let mut stmt = self.conn.prepare(
                    "SELECT edge_id, source_id, target_id, kind, metadata, created_at
                     FROM context_edges
                     WHERE source_id = ?1 AND kind = ?2
                     ORDER BY created_at",
                )?;
                stmt.query_map(
                    params![blob_param(source.as_bytes()), k.as_str()],
                    row_to_edge_row,
                )?
                .collect::<SqliteResult<Vec<_>>>()?
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT edge_id, source_id, target_id, kind, metadata, created_at
                     FROM context_edges
                     WHERE source_id = ?1
                     ORDER BY created_at",
                )?;
                stmt.query_map(params![blob_param(source.as_bytes())], row_to_edge_row)?
                    .collect::<SqliteResult<Vec<_>>>()?
            }
        };
        Ok(rows)
    }

    /// List edges to a target, optionally filtered by kind.
    pub fn edges_to(
        &self,
        target: ContextId,
        kind: Option<EdgeKind>,
    ) -> KernelDbResult<Vec<ContextEdgeRow>> {
        let rows = match kind {
            Some(k) => {
                let mut stmt = self.conn.prepare(
                    "SELECT edge_id, source_id, target_id, kind, metadata, created_at
                     FROM context_edges
                     WHERE target_id = ?1 AND kind = ?2
                     ORDER BY created_at",
                )?;
                stmt.query_map(
                    params![blob_param(target.as_bytes()), k.as_str()],
                    row_to_edge_row,
                )?
                .collect::<SqliteResult<Vec<_>>>()?
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT edge_id, source_id, target_id, kind, metadata, created_at
                     FROM context_edges
                     WHERE target_id = ?1
                     ORDER BY created_at",
                )?;
                stmt.query_map(params![blob_param(target.as_bytes())], row_to_edge_row)?
                    .collect::<SqliteResult<Vec<_>>>()?
            }
        };
        Ok(rows)
    }

    /// List drift edges originating from a context, newest first.
    pub fn drift_provenance(&self, context_id: ContextId) -> KernelDbResult<Vec<ContextEdgeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT edge_id, source_id, target_id, kind, metadata, created_at
             FROM context_edges
             WHERE source_id = ?1 AND kind = 'drift'
             ORDER BY created_at DESC",
        )?;

        let rows = stmt
            .query_map(params![blob_param(context_id.as_bytes())], row_to_edge_row)?
            .collect::<SqliteResult<Vec<_>>>()?;
        Ok(rows)
    }

    // ========================================================================
    // Presets
    // ========================================================================

    /// Insert a new preset.
    pub fn insert_preset(&self, row: &PresetRow) -> KernelDbResult<()> {
        validate_label(&row.label)?;

        self.conn.execute(
            "INSERT INTO presets (
                preset_id, kernel_id, label, description, provider, model,
                system_prompt, tool_filter, consent_mode, created_at, created_by
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                blob_param(row.preset_id.as_bytes()),
                blob_param(row.kernel_id.as_bytes()),
                row.label,
                row.description,
                row.provider,
                row.model,
                row.system_prompt,
                tool_filter_to_sql(&row.tool_filter),
                row.consent_mode.as_str(),
                row.created_at,
                blob_param(row.created_by.as_bytes()),
            ],
        ).map_err(|e| {
            map_unique_violation(e, format!("preset label '{}' already in use", row.label))
        })?;
        Ok(())
    }

    /// Get a preset by ID.
    pub fn get_preset(&self, id: PresetId) -> KernelDbResult<Option<PresetRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT preset_id, kernel_id, label, description, provider, model,
                    system_prompt, tool_filter, consent_mode, created_at, created_by
             FROM presets WHERE preset_id = ?1",
        )?;

        let mut rows = stmt.query(params![blob_param(id.as_bytes())])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_preset_row(row)?))
        } else {
            Ok(None)
        }
    }

    /// Get a preset by kernel + label.
    pub fn get_preset_by_label(
        &self,
        kernel_id: KernelId,
        label: &str,
    ) -> KernelDbResult<Option<PresetRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT preset_id, kernel_id, label, description, provider, model,
                    system_prompt, tool_filter, consent_mode, created_at, created_by
             FROM presets WHERE kernel_id = ?1 AND label = ?2",
        )?;

        let mut rows = stmt.query(params![blob_param(kernel_id.as_bytes()), label])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_preset_row(row)?))
        } else {
            Ok(None)
        }
    }

    /// List all presets for a kernel.
    pub fn list_presets(&self, kernel_id: KernelId) -> KernelDbResult<Vec<PresetRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT preset_id, kernel_id, label, description, provider, model,
                    system_prompt, tool_filter, consent_mode, created_at, created_by
             FROM presets WHERE kernel_id = ?1
             ORDER BY label",
        )?;

        let rows = stmt.query_map(params![blob_param(kernel_id.as_bytes())], |row| {
            row_to_preset_row(row)
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Update a preset.
    pub fn update_preset(&self, row: &PresetRow) -> KernelDbResult<()> {
        validate_label(&row.label)?;

        let updated = self.conn.execute(
            "UPDATE presets SET
                label = ?1, description = ?2, provider = ?3, model = ?4,
                system_prompt = ?5, tool_filter = ?6, consent_mode = ?7
             WHERE preset_id = ?8",
            params![
                row.label,
                row.description,
                row.provider,
                row.model,
                row.system_prompt,
                tool_filter_to_sql(&row.tool_filter),
                row.consent_mode.as_str(),
                blob_param(row.preset_id.as_bytes()),
            ],
        ).map_err(|e| {
            map_unique_violation(e, format!("preset label '{}' already in use", row.label))
        })?;

        if updated == 0 {
            return Err(KernelDbError::NotFound(format!(
                "preset {}",
                row.preset_id
            )));
        }
        Ok(())
    }

    /// Delete a preset. Returns true if it existed.
    pub fn delete_preset(&self, id: PresetId) -> KernelDbResult<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM presets WHERE preset_id = ?1",
            params![blob_param(id.as_bytes())],
        )?;
        Ok(deleted > 0)
    }

    // ========================================================================
    // Workspaces
    // ========================================================================

    /// Insert a new workspace.
    pub fn insert_workspace(&self, row: &WorkspaceRow) -> KernelDbResult<()> {
        validate_label(&row.label)?;

        self.conn.execute(
            "INSERT INTO workspaces (
                workspace_id, kernel_id, label, description, created_at,
                created_by, archived_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                blob_param(row.workspace_id.as_bytes()),
                blob_param(row.kernel_id.as_bytes()),
                row.label,
                row.description,
                row.created_at,
                blob_param(row.created_by.as_bytes()),
                row.archived_at,
            ],
        ).map_err(|e| {
            map_unique_violation(e, format!("workspace label '{}' already in use", row.label))
        })?;
        Ok(())
    }

    /// Get a workspace by ID.
    pub fn get_workspace(&self, id: WorkspaceId) -> KernelDbResult<Option<WorkspaceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT workspace_id, kernel_id, label, description, created_at,
                    created_by, archived_at
             FROM workspaces WHERE workspace_id = ?1",
        )?;

        let mut rows = stmt.query(params![blob_param(id.as_bytes())])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_workspace_row(row)?))
        } else {
            Ok(None)
        }
    }

    /// Get a workspace by kernel + label.
    pub fn get_workspace_by_label(
        &self,
        kernel_id: KernelId,
        label: &str,
    ) -> KernelDbResult<Option<WorkspaceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT workspace_id, kernel_id, label, description, created_at,
                    created_by, archived_at
             FROM workspaces WHERE kernel_id = ?1 AND label = ?2",
        )?;

        let mut rows = stmt.query(params![blob_param(kernel_id.as_bytes()), label])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_workspace_row(row)?))
        } else {
            Ok(None)
        }
    }

    /// List active (non-archived) workspaces for a kernel.
    pub fn list_workspaces(&self, kernel_id: KernelId) -> KernelDbResult<Vec<WorkspaceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT workspace_id, kernel_id, label, description, created_at,
                    created_by, archived_at
             FROM workspaces
             WHERE kernel_id = ?1 AND archived_at IS NULL
             ORDER BY label",
        )?;

        let rows = stmt.query_map(params![blob_param(kernel_id.as_bytes())], |row| {
            row_to_workspace_row(row)
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// List all workspaces for a kernel (including archived).
    pub fn list_all_workspaces(&self, kernel_id: KernelId) -> KernelDbResult<Vec<WorkspaceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT workspace_id, kernel_id, label, description, created_at,
                    created_by, archived_at
             FROM workspaces
             WHERE kernel_id = ?1
             ORDER BY label",
        )?;

        let rows = stmt.query_map(params![blob_param(kernel_id.as_bytes())], |row| {
            row_to_workspace_row(row)
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Archive a workspace (soft delete). Returns true if it was active.
    pub fn archive_workspace(&self, id: WorkspaceId) -> KernelDbResult<bool> {
        let now = now_millis() as i64;
        let updated = self.conn.execute(
            "UPDATE workspaces SET archived_at = ?1
             WHERE workspace_id = ?2 AND archived_at IS NULL",
            params![now, blob_param(id.as_bytes())],
        )?;
        Ok(updated > 0)
    }

    // ========================================================================
    // Workspace Paths
    // ========================================================================

    /// Insert a workspace path.
    pub fn insert_workspace_path(&self, row: &WorkspacePathRow) -> KernelDbResult<()> {
        self.conn.execute(
            "INSERT INTO workspace_paths (workspace_id, path, read_only, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                blob_param(row.workspace_id.as_bytes()),
                row.path,
                row.read_only as i64,
                row.created_at,
            ],
        ).map_err(|e| {
            map_unique_violation(
                e,
                format!("workspace path '{}' already exists", row.path),
            )
        })?;
        Ok(())
    }

    /// List paths for a workspace.
    pub fn list_workspace_paths(
        &self,
        workspace_id: WorkspaceId,
    ) -> KernelDbResult<Vec<WorkspacePathRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT workspace_id, path, read_only, created_at
             FROM workspace_paths WHERE workspace_id = ?1
             ORDER BY path",
        )?;

        let rows = stmt.query_map(
            params![blob_param(workspace_id.as_bytes())],
            |row| {
                let ro: i64 = row.get(2)?;
                Ok(WorkspacePathRow {
                    workspace_id: read_workspace_id(row, 0)?,
                    path: row.get(1)?,
                    read_only: ro != 0,
                    created_at: row.get(3)?,
                })
            },
        )?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Delete a workspace path. Returns true if it existed.
    pub fn delete_workspace_path(
        &self,
        workspace_id: WorkspaceId,
        path: &str,
    ) -> KernelDbResult<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM workspace_paths WHERE workspace_id = ?1 AND path = ?2",
            params![blob_param(workspace_id.as_bytes()), path],
        )?;
        Ok(deleted > 0)
    }

    // ========================================================================
    // Context Shell Configuration
    // ========================================================================

    /// Upsert per-context shell configuration (cwd, init_script).
    pub fn upsert_context_shell(&self, row: &ContextShellRow) -> KernelDbResult<()> {
        self.conn.execute(
            "INSERT INTO context_shell (context_id, cwd, init_script, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(context_id) DO UPDATE SET
                cwd = excluded.cwd,
                init_script = excluded.init_script,
                updated_at = excluded.updated_at",
            params![
                blob_param(row.context_id.as_bytes()),
                row.cwd,
                row.init_script,
                row.updated_at,
            ],
        )?;
        Ok(())
    }

    /// Get per-context shell configuration. Returns None if not set.
    pub fn get_context_shell(
        &self,
        context_id: ContextId,
    ) -> KernelDbResult<Option<ContextShellRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, cwd, init_script, updated_at
             FROM context_shell WHERE context_id = ?1",
        )?;
        let mut rows = stmt.query_map(
            params![blob_param(context_id.as_bytes())],
            |row| {
                Ok(ContextShellRow {
                    context_id: read_context_id(row, 0)?,
                    cwd: row.get(1)?,
                    init_script: row.get(2)?,
                    updated_at: row.get(3)?,
                })
            },
        )?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    /// Copy shell config from source to target. Returns true if source had config.
    pub fn copy_context_shell(
        &self,
        source: ContextId,
        target: ContextId,
    ) -> KernelDbResult<bool> {
        let src = match self.get_context_shell(source)? {
            Some(s) => s,
            None => return Ok(false),
        };
        let row = ContextShellRow {
            context_id: target,
            cwd: src.cwd,
            init_script: src.init_script,
            updated_at: now_millis() as i64,
        };
        self.upsert_context_shell(&row)?;
        Ok(true)
    }

    // ========================================================================
    // Context Environment Variables
    // ========================================================================

    /// Set a single environment variable for a context (upsert).
    pub fn set_context_env(
        &self,
        context_id: ContextId,
        key: &str,
        value: &str,
    ) -> KernelDbResult<()> {
        self.conn.execute(
            "INSERT INTO context_env (context_id, key, value)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(context_id, key) DO UPDATE SET value = excluded.value",
            params![blob_param(context_id.as_bytes()), key, value],
        )?;
        Ok(())
    }

    /// Get all environment variables for a context.
    pub fn get_context_env(
        &self,
        context_id: ContextId,
    ) -> KernelDbResult<Vec<ContextEnvRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, key, value FROM context_env
             WHERE context_id = ?1 ORDER BY key",
        )?;
        let rows = stmt.query_map(
            params![blob_param(context_id.as_bytes())],
            |row| {
                Ok(ContextEnvRow {
                    context_id: read_context_id(row, 0)?,
                    key: row.get(1)?,
                    value: row.get(2)?,
                })
            },
        )?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Delete a single environment variable. Returns true if it existed.
    pub fn delete_context_env(
        &self,
        context_id: ContextId,
        key: &str,
    ) -> KernelDbResult<bool> {
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
    pub fn copy_context_env(
        &self,
        source: ContextId,
        target: ContextId,
    ) -> KernelDbResult<u64> {
        let vars = self.get_context_env(source)?;
        for var in &vars {
            self.set_context_env(target, &var.key, &var.value)?;
        }
        Ok(vars.len() as u64)
    }

    // ========================================================================
    // Context Config Fork + Workspace Query
    // ========================================================================

    /// Copy shell config + env vars from source context to target.
    /// Called during all fork operations.
    pub fn fork_context_config(
        &self,
        source: ContextId,
        target: ContextId,
    ) -> KernelDbResult<()> {
        self.copy_context_shell(source, target)?;
        self.copy_context_env(source, target)?;
        Ok(())
    }

    /// Get workspace paths for a context (via contexts.workspace_id FK).
    /// Returns None if context has no workspace bound.
    pub fn context_workspace_paths(
        &self,
        context_id: ContextId,
    ) -> KernelDbResult<Option<Vec<WorkspacePathRow>>> {
        let ctx = self.get_context(context_id)?
            .ok_or_else(|| KernelDbError::NotFound(format!("context {}", context_id.short())))?;
        match ctx.workspace_id {
            Some(ws_id) => {
                let paths = self.list_workspace_paths(ws_id)?;
                Ok(Some(paths))
            }
            None => Ok(None),
        }
    }

    // ========================================================================
    // Phase 4A: Additional methods for kj commands
    // ========================================================================

    /// Delete a structural edge between source and target.
    ///
    /// Used by `kj context move` to reparent a context.
    pub fn delete_structural_edge(
        &self,
        source: ContextId,
        target: ContextId,
    ) -> KernelDbResult<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM context_edges
             WHERE source_id = ?1 AND target_id = ?2 AND kind = 'structural'",
            params![blob_param(source.as_bytes()), blob_param(target.as_bytes())],
        )?;
        Ok(deleted > 0)
    }

    /// Hard-delete a context and all its edges (CASCADE).
    ///
    /// Used by `kj context remove`. This is permanent — use `archive_context`
    /// for soft delete.
    pub fn delete_context(&self, id: ContextId) -> KernelDbResult<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM contexts WHERE context_id = ?1",
            params![blob_param(id.as_bytes())],
        )?;
        Ok(deleted > 0)
    }

    /// Count contexts using a specific preset.
    pub fn contexts_using_preset(
        &self,
        kernel_id: KernelId,
        preset_id: PresetId,
    ) -> KernelDbResult<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM contexts
             WHERE kernel_id = ?1 AND preset_id = ?2 AND archived_at IS NULL",
            params![blob_param(kernel_id.as_bytes()), blob_param(preset_id.as_bytes())],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Count contexts using a specific workspace.
    pub fn contexts_using_workspace(
        &self,
        kernel_id: KernelId,
        workspace_id: WorkspaceId,
    ) -> KernelDbResult<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM contexts
             WHERE kernel_id = ?1 AND workspace_id = ?2 AND archived_at IS NULL",
            params![blob_param(kernel_id.as_bytes()), blob_param(workspace_id.as_bytes())],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Find the context that currently holds a given label.
    pub fn find_context_by_label(
        &self,
        kernel_id: KernelId,
        label: &str,
    ) -> KernelDbResult<Option<ContextRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, kernel_id, label, provider, model,
                    system_prompt, tool_filter, consent_mode, created_at,
                    created_by, forked_from, fork_kind, archived_at,
                    workspace_id, preset_id
             FROM contexts WHERE kernel_id = ?1 AND label = ?2",
        )?;

        let mut rows = stmt.query(params![blob_param(kernel_id.as_bytes()), label])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_context_row(row)?))
        } else {
            Ok(None)
        }
    }
}

// ============================================================================
// Row parsers
// ============================================================================

fn row_to_context_row(row: &rusqlite::Row<'_>) -> SqliteResult<ContextRow> {
    let consent_str: String = row.get(7)?;
    let fork_kind_str: Option<String> = row.get(11)?;
    let tool_filter_str: Option<String> = row.get(6)?;

    Ok(ContextRow {
        context_id: read_context_id(row, 0)?,
        kernel_id: read_kernel_id(row, 1)?,
        label: row.get(2)?,
        provider: row.get(3)?,
        model: row.get(4)?,
        system_prompt: row.get(5)?,
        tool_filter: tool_filter_from_sql(tool_filter_str),
        consent_mode: consent_mode_from_sql(&consent_str),
        created_at: row.get(8)?,
        created_by: read_principal_id(row, 9)?,
        forked_from: read_opt_context_id(row, 10)?,
        fork_kind: fork_kind_from_sql(fork_kind_str),
        archived_at: row.get(12)?,
        workspace_id: read_opt_workspace_id(row, 13)?,
        preset_id: read_opt_preset_id(row, 14)?,
    })
}

fn row_to_edge_row(row: &rusqlite::Row<'_>) -> SqliteResult<ContextEdgeRow> {
    let kind_str: String = row.get(3)?;
    Ok(ContextEdgeRow {
        edge_id: read_edge_id(row, 0)?,
        source_id: read_context_id(row, 1)?,
        target_id: read_context_id(row, 2)?,
        kind: edge_kind_from_sql(&kind_str),
        metadata: row.get(4)?,
        created_at: row.get(5)?,
    })
}

fn row_to_preset_row(row: &rusqlite::Row<'_>) -> SqliteResult<PresetRow> {
    let consent_str: String = row.get(8)?;
    let tool_filter_str: Option<String> = row.get(7)?;

    Ok(PresetRow {
        preset_id: read_preset_id(row, 0)?,
        kernel_id: read_kernel_id(row, 1)?,
        label: row.get(2)?,
        description: row.get(3)?,
        provider: row.get(4)?,
        model: row.get(5)?,
        system_prompt: row.get(6)?,
        tool_filter: tool_filter_from_sql(tool_filter_str),
        consent_mode: consent_mode_from_sql(&consent_str),
        created_at: row.get(9)?,
        created_by: read_principal_id(row, 10)?,
    })
}

fn row_to_workspace_row(row: &rusqlite::Row<'_>) -> SqliteResult<WorkspaceRow> {
    Ok(WorkspaceRow {
        workspace_id: read_workspace_id(row, 0)?,
        kernel_id: read_kernel_id(row, 1)?,
        label: row.get(2)?,
        description: row.get(3)?,
        created_at: row.get(4)?,
        created_by: read_principal_id(row, 5)?,
        archived_at: row.get(6)?,
    })
}

// ============================================================================
// Test helpers
// ============================================================================

#[cfg(test)]
fn make_context_row(kernel_id: KernelId, label: Option<&str>) -> ContextRow {
    ContextRow {
        context_id: ContextId::new(),
        kernel_id,
        label: label.map(String::from),
        provider: None,
        model: None,
        system_prompt: None,
        tool_filter: None,
        consent_mode: ConsentMode::default(),
        created_at: now_millis() as i64,
        created_by: PrincipalId::new(),
        forked_from: None,
        fork_kind: None,
        archived_at: None,
        workspace_id: None,
        preset_id: None,
    }
}

#[cfg(test)]
fn make_edge(source: ContextId, target: ContextId, kind: EdgeKind) -> ContextEdgeRow {
    ContextEdgeRow {
        edge_id: uuid::Uuid::now_v7(),
        source_id: source,
        target_id: target,
        kind,
        metadata: None,
        created_at: now_millis() as i64,
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── 1. Schema idempotent ────────────────────────────────────────────

    #[test]
    fn schema_idempotent() {
        let db = KernelDb::in_memory().unwrap();
        // Apply schema again — should not error.
        db.conn.execute_batch(SCHEMA).unwrap();
    }

    // ── 2. Context lifecycle ────────────────────────────────────────────

    #[test]
    fn context_lifecycle() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let row = make_context_row(kid, Some("main"));
        let cid = row.context_id;

        db.insert_context(&row).unwrap();

        // Get
        let loaded = db.get_context(cid).unwrap().unwrap();
        assert_eq!(loaded.label, Some("main".into()));

        // Update label
        db.update_label(cid, Some("primary")).unwrap();
        let loaded = db.get_context(cid).unwrap().unwrap();
        assert_eq!(loaded.label, Some("primary".into()));

        // Archive
        assert!(db.archive_context(cid).unwrap());

        // list_active excludes
        let active = db.list_active_contexts(kid).unwrap();
        assert!(active.is_empty());

        // list_all includes
        let all = db.list_all_contexts(kid).unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].archived_at.is_some());
    }

    // ── 3. Label validation: colon ──────────────────────────────────────

    #[test]
    fn label_validation_colon() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let row = make_context_row(kid, Some("my:label"));

        let err = db.insert_context(&row).unwrap_err();
        assert!(matches!(err, KernelDbError::InvalidLabel(_)));
    }

    // ── 4. Label uniqueness ────────────────────────────────────────────

    #[test]
    fn label_uniqueness() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();

        let row1 = make_context_row(kid, Some("shared"));
        db.insert_context(&row1).unwrap();

        // Same kernel, same label → conflict
        let row2 = make_context_row(kid, Some("shared"));
        let err = db.insert_context(&row2).unwrap_err();
        assert!(matches!(err, KernelDbError::LabelConflict(_)));

        // Different kernel, same label → OK
        let kid2 = KernelId::new();
        let row3 = make_context_row(kid2, Some("shared"));
        db.insert_context(&row3).unwrap();

        // NULL + NULL → OK (multiple)
        let n1 = make_context_row(kid, None);
        let n2 = make_context_row(kid, None);
        db.insert_context(&n1).unwrap();
        db.insert_context(&n2).unwrap();
    }

    // ── 5. Fork lineage 3 deep ─────────────────────────────────────────

    #[test]
    fn fork_lineage_3_deep() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();

        let root = make_context_row(kid, Some("root"));
        db.insert_context(&root).unwrap();

        let mut child = make_context_row(kid, Some("child"));
        child.forked_from = Some(root.context_id);
        child.fork_kind = Some(ForkKind::Full);
        db.insert_context(&child).unwrap();

        let mut grandchild = make_context_row(kid, Some("grandchild"));
        grandchild.forked_from = Some(child.context_id);
        grandchild.fork_kind = Some(ForkKind::Shallow);
        db.insert_context(&grandchild).unwrap();

        let lineage = db.fork_lineage(grandchild.context_id).unwrap();
        assert_eq!(lineage.len(), 3);
        assert_eq!(lineage[0].0.context_id, grandchild.context_id);
        assert_eq!(lineage[0].1, 0);
        assert_eq!(lineage[1].0.context_id, child.context_id);
        assert_eq!(lineage[1].1, 1);
        assert_eq!(lineage[2].0.context_id, root.context_id);
        assert_eq!(lineage[2].1, 2);
    }

    // ── 6. Subtree snapshot ────────────────────────────────────────────

    #[test]
    fn subtree_snapshot() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();

        let parent = make_context_row(kid, Some("template"));
        db.insert_context(&parent).unwrap();

        let c1 = make_context_row(kid, Some("child1"));
        db.insert_context(&c1).unwrap();
        db.insert_edge(&make_edge(parent.context_id, c1.context_id, EdgeKind::Structural))
            .unwrap();

        let c2 = make_context_row(kid, Some("child2"));
        db.insert_context(&c2).unwrap();
        db.insert_edge(&make_edge(parent.context_id, c2.context_id, EdgeKind::Structural))
            .unwrap();

        let snapshot = db.subtree_snapshot(parent.context_id).unwrap();
        assert_eq!(snapshot.len(), 3);
        assert_eq!(snapshot[0].1, 0); // parent at depth 0
        assert_eq!(snapshot[1].1, 1); // child at depth 1
        assert_eq!(snapshot[2].1, 1); // child at depth 1
    }

    // ── 7. Structural edge unique ──────────────────────────────────────

    #[test]
    fn structural_edge_unique() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();

        let a = make_context_row(kid, Some("a"));
        let b = make_context_row(kid, Some("b"));
        db.insert_context(&a).unwrap();
        db.insert_context(&b).unwrap();

        // First structural edge OK
        db.insert_edge(&make_edge(a.context_id, b.context_id, EdgeKind::Structural))
            .unwrap();

        // Duplicate structural → error
        let err = db
            .insert_edge(&make_edge(a.context_id, b.context_id, EdgeKind::Structural))
            .unwrap_err();
        assert!(matches!(err, KernelDbError::LabelConflict(_)));

        // Same pair as drift → OK
        db.insert_edge(&make_edge(a.context_id, b.context_id, EdgeKind::Drift))
            .unwrap();
    }

    // ── 8. Drift edge allows dupes ─────────────────────────────────────

    #[test]
    fn drift_edge_allows_dupes() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();

        let a = make_context_row(kid, None);
        let b = make_context_row(kid, None);
        db.insert_context(&a).unwrap();
        db.insert_context(&b).unwrap();

        // Multiple drift edges same pair → all stored
        for _ in 0..3 {
            db.insert_edge(&make_edge(a.context_id, b.context_id, EdgeKind::Drift))
                .unwrap();
        }

        let edges = db.edges_from(a.context_id, Some(EdgeKind::Drift)).unwrap();
        assert_eq!(edges.len(), 3);
    }

    // ── 9. Cycle detection ─────────────────────────────────────────────

    #[test]
    fn cycle_detection() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();

        let a = make_context_row(kid, Some("cyc-a"));
        let b = make_context_row(kid, Some("cyc-b"));
        db.insert_context(&a).unwrap();
        db.insert_context(&b).unwrap();

        // A → B structural OK
        db.insert_edge(&make_edge(a.context_id, b.context_id, EdgeKind::Structural))
            .unwrap();

        // B → A structural → CycleDetected
        let err = db
            .insert_edge(&make_edge(b.context_id, a.context_id, EdgeKind::Structural))
            .unwrap_err();
        assert!(matches!(err, KernelDbError::CycleDetected));

        // Self-loop also detected
        let c = make_context_row(kid, Some("cyc-c"));
        db.insert_context(&c).unwrap();
        let err = db
            .insert_edge(&make_edge(c.context_id, c.context_id, EdgeKind::Structural))
            .unwrap_err();
        assert!(matches!(err, KernelDbError::CycleDetected));
    }

    // ── 10. Preset lifecycle ───────────────────────────────────────────

    #[test]
    fn preset_lifecycle() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let creator = PrincipalId::new();
        let now = now_millis() as i64;

        let mut preset = PresetRow {
            preset_id: PresetId::new(),
            kernel_id: kid,
            label: "opus-research".into(),
            description: Some("Deep research preset".into()),
            provider: Some("anthropic".into()),
            model: Some("claude-opus-4-6".into()),
            system_prompt: None,
            tool_filter: None,
            consent_mode: ConsentMode::Autonomous,
            created_at: now,
            created_by: creator,
        };

        db.insert_preset(&preset).unwrap();

        // Get by label
        let loaded = db
            .get_preset_by_label(kid, "opus-research")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.model, Some("claude-opus-4-6".into()));

        // Update
        preset.model = Some("claude-sonnet-4-6".into());
        db.update_preset(&preset).unwrap();
        let loaded = db.get_preset(preset.preset_id).unwrap().unwrap();
        assert_eq!(loaded.model, Some("claude-sonnet-4-6".into()));

        // Delete
        assert!(db.delete_preset(preset.preset_id).unwrap());
        assert!(db.get_preset(preset.preset_id).unwrap().is_none());
    }

    // ── 11. Workspace lifecycle ────────────────────────────────────────

    #[test]
    fn workspace_lifecycle() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let creator = PrincipalId::new();
        let now = now_millis() as i64;

        let ws = WorkspaceRow {
            workspace_id: WorkspaceId::new(),
            kernel_id: kid,
            label: "kaijutsu".into(),
            description: Some("Main project".into()),
            created_at: now,
            created_by: creator,
            archived_at: None,
        };

        db.insert_workspace(&ws).unwrap();

        // Add paths
        let p1 = WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: "/home/user/src/kaijutsu".into(),
            read_only: false,
            created_at: now,
        };
        let p2 = WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: "/home/user/src/kaish".into(),
            read_only: false,
            created_at: now,
        };
        db.insert_workspace_path(&p1).unwrap();
        db.insert_workspace_path(&p2).unwrap();

        // List paths
        let paths = db.list_workspace_paths(ws.workspace_id).unwrap();
        assert_eq!(paths.len(), 2);

        // Delete one path
        assert!(db
            .delete_workspace_path(ws.workspace_id, "/home/user/src/kaish")
            .unwrap());
        let paths = db.list_workspace_paths(ws.workspace_id).unwrap();
        assert_eq!(paths.len(), 1);

        // Archive workspace
        assert!(db.archive_workspace(ws.workspace_id).unwrap());
        let active = db.list_workspaces(kid).unwrap();
        assert!(active.is_empty());
    }

    // ── 12. Workspace soft delete ──────────────────────────────────────

    #[test]
    fn workspace_soft_delete() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let creator = PrincipalId::new();
        let now = now_millis() as i64;

        let ws = WorkspaceRow {
            workspace_id: WorkspaceId::new(),
            kernel_id: kid,
            label: "project".into(),
            description: None,
            created_at: now,
            created_by: creator,
            archived_at: None,
        };
        db.insert_workspace(&ws).unwrap();

        // Create context referencing this workspace
        let mut ctx = make_context_row(kid, Some("ctx-ws"));
        ctx.workspace_id = Some(ws.workspace_id);
        db.insert_context(&ctx).unwrap();

        // Archive workspace
        db.archive_workspace(ws.workspace_id).unwrap();

        // Context still references the workspace
        let loaded = db.get_context(ctx.context_id).unwrap().unwrap();
        assert_eq!(loaded.workspace_id, Some(ws.workspace_id));

        // get_workspace still returns it (it's archived, not deleted)
        let loaded_ws = db.get_workspace(ws.workspace_id).unwrap().unwrap();
        assert!(loaded_ws.archived_at.is_some());
    }

    // ── 13. Drift push creates edge ────────────────────────────────────

    #[test]
    fn drift_push_creates_edge() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();

        let a = make_context_row(kid, Some("source"));
        let b = make_context_row(kid, Some("target"));
        db.insert_context(&a).unwrap();
        db.insert_context(&b).unwrap();

        // Simulate drift push → create drift edge
        let edge1 = make_edge(a.context_id, b.context_id, EdgeKind::Drift);
        db.insert_edge(&edge1).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1));

        let edge2 = make_edge(a.context_id, b.context_id, EdgeKind::Drift);
        db.insert_edge(&edge2).unwrap();

        // edges_from returns both
        let edges = db.edges_from(a.context_id, Some(EdgeKind::Drift)).unwrap();
        assert_eq!(edges.len(), 2);

        // drift_provenance returns newest first
        let prov = db.drift_provenance(a.context_id).unwrap();
        assert_eq!(prov.len(), 2);
        assert!(prov[0].created_at >= prov[1].created_at);
    }

    // ── 14. Context DAG recursive ──────────────────────────────────────

    #[test]
    fn context_dag_recursive() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();

        // Create a 5-node tree: root → [a, b], a → [c, d]
        let root = make_context_row(kid, Some("root"));
        let a = make_context_row(kid, Some("a"));
        let b = make_context_row(kid, Some("b"));
        let c = make_context_row(kid, Some("c"));
        let d = make_context_row(kid, Some("d"));

        for ctx in [&root, &a, &b, &c, &d] {
            db.insert_context(ctx).unwrap();
        }

        db.insert_edge(&make_edge(root.context_id, a.context_id, EdgeKind::Structural))
            .unwrap();
        db.insert_edge(&make_edge(root.context_id, b.context_id, EdgeKind::Structural))
            .unwrap();
        db.insert_edge(&make_edge(a.context_id, c.context_id, EdgeKind::Structural))
            .unwrap();
        db.insert_edge(&make_edge(a.context_id, d.context_id, EdgeKind::Structural))
            .unwrap();

        let dag = db.context_dag(kid).unwrap();
        assert_eq!(dag.len(), 5);

        // Root at depth 0
        assert_eq!(dag[0].0.context_id, root.context_id);
        assert_eq!(dag[0].1, 0);

        // a, b at depth 1
        let depth1: Vec<_> = dag.iter().filter(|(_, d)| *d == 1).collect();
        assert_eq!(depth1.len(), 2);

        // c, d at depth 2
        let depth2: Vec<_> = dag.iter().filter(|(_, d)| *d == 2).collect();
        assert_eq!(depth2.len(), 2);
    }

    // ── 15. Tag resolution (label + prefix) ────────────────────────────

    #[test]
    fn tag_resolution() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();

        let ctx1 = make_context_row(kid, Some("opusplan"));
        let ctx2 = make_context_row(kid, Some("sonnet"));
        db.insert_context(&ctx1).unwrap();
        db.insert_context(&ctx2).unwrap();

        // Exact label match
        let resolved = db.resolve_context(kid, "opusplan").unwrap();
        assert_eq!(resolved, ctx1.context_id);

        // Prefix match
        let resolved = db.resolve_context(kid, "opus").unwrap();
        assert_eq!(resolved, ctx1.context_id);
    }

    // ── 16. Resolve context basic ──────────────────────────────────────

    #[test]
    fn resolve_context_basic() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();

        let ctx = make_context_row(kid, Some("unique-label"));
        db.insert_context(&ctx).unwrap();

        // Exact label
        let r = db.resolve_context(kid, "unique-label").unwrap();
        assert_eq!(r, ctx.context_id);

        // Label prefix
        let r = db.resolve_context(kid, "unique").unwrap();
        assert_eq!(r, ctx.context_id);

        // Hex prefix
        let hex = ctx.context_id.to_hex();
        let r = db.resolve_context(kid, &hex[..8]).unwrap();
        assert_eq!(r, ctx.context_id);

        // Not found
        let err = db.resolve_context(kid, "nonexistent").unwrap_err();
        assert!(matches!(err, KernelDbError::NotFound(_)));
    }

    // ── 17. Null labels coexist ────────────────────────────────────────

    #[test]
    fn null_labels_coexist() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();

        // Insert 5 contexts with NULL label — all succeed
        for _ in 0..5 {
            let row = make_context_row(kid, None);
            db.insert_context(&row).unwrap();
        }

        let all = db.list_all_contexts(kid).unwrap();
        assert_eq!(all.len(), 5);
    }

    // ── 18. Archive excludes from active + structural_children ─────────

    #[test]
    fn archive_excludes_from_active() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();

        let parent = make_context_row(kid, Some("parent"));
        let child = make_context_row(kid, Some("child"));
        db.insert_context(&parent).unwrap();
        db.insert_context(&child).unwrap();
        db.insert_edge(&make_edge(
            parent.context_id,
            child.context_id,
            EdgeKind::Structural,
        ))
        .unwrap();

        // Before archive
        let children = db.structural_children(parent.context_id).unwrap();
        assert_eq!(children.len(), 1);

        // Archive child
        db.archive_context(child.context_id).unwrap();

        // list_active excludes
        let active = db.list_active_contexts(kid).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].context_id, parent.context_id);

        // structural_children excludes archived
        let children = db.structural_children(parent.context_id).unwrap();
        assert!(children.is_empty());
    }

    // ── 19. Context edge cascade ───────────────────────────────────────

    #[test]
    fn context_edge_cascade() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();

        let a = make_context_row(kid, Some("edge-a"));
        let b = make_context_row(kid, Some("edge-b"));
        db.insert_context(&a).unwrap();
        db.insert_context(&b).unwrap();

        db.insert_edge(&make_edge(a.context_id, b.context_id, EdgeKind::Structural))
            .unwrap();
        db.insert_edge(&make_edge(a.context_id, b.context_id, EdgeKind::Drift))
            .unwrap();

        // Both edges exist
        let all = db.edges_from(a.context_id, None).unwrap();
        assert_eq!(all.len(), 2);

        // Delete context a → edges cascade
        db.conn
            .execute(
                "DELETE FROM contexts WHERE context_id = ?1",
                params![blob_param(a.context_id.as_bytes())],
            )
            .unwrap();

        // Edges gone
        let all = db.edges_from(a.context_id, None).unwrap();
        assert!(all.is_empty());
    }

    // ── 20. Context row to_context conversion ──────────────────────────

    #[test]
    fn context_row_to_context() {
        let kid = KernelId::new();
        let creator = PrincipalId::new();
        let parent = ContextId::new();
        let now = now_millis() as i64;

        let row = ContextRow {
            context_id: ContextId::new(),
            kernel_id: kid,
            label: Some("test".into()),
            provider: Some("anthropic".into()),
            model: Some("opus".into()),
            system_prompt: None,
            tool_filter: Some(ToolFilter::All),
            consent_mode: ConsentMode::Autonomous,
            created_at: now,
            created_by: creator,
            forked_from: Some(parent),
            fork_kind: Some(ForkKind::Full),
            archived_at: None,
            workspace_id: None,
            preset_id: None,
        };

        let ctx = row.to_context();
        assert_eq!(ctx.id, row.context_id);
        assert_eq!(ctx.kernel_id, kid);
        assert_eq!(ctx.label, Some("test".into()));
        assert_eq!(ctx.forked_from, Some(parent));
        assert_eq!(ctx.created_by, creator);
        assert_eq!(ctx.created_at, now as u64);
    }

    // ── 21. Roundtrip: create context, read back, verify all 15 fields ──

    #[test]
    fn roundtrip_create_and_recover() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let creator = PrincipalId::new();
        let parent_id = ContextId::new();

        // Insert parent first (forked_from FK requires it to exist)
        let parent = ContextRow {
            context_id: parent_id,
            kernel_id: kid,
            label: Some("parent".into()),
            provider: Some("anthropic".into()),
            model: Some("claude-opus-4-6".into()),
            system_prompt: Some("You are helpful.".into()),
            tool_filter: Some(ToolFilter::DenyList(["dangerous".to_string()].into())),
            consent_mode: ConsentMode::Collaborative,
            created_at: 1000,
            created_by: creator,
            forked_from: None,
            fork_kind: None,
            archived_at: None,
            workspace_id: None,
            preset_id: None,
        };
        db.insert_context(&parent).unwrap();

        // Insert child forked from parent
        let child_id = ContextId::new();
        let child = ContextRow {
            context_id: child_id,
            kernel_id: kid,
            label: Some("child-fork".into()),
            provider: Some("google".into()),
            model: Some("gemini-2.0-flash".into()),
            system_prompt: Some("Be concise.".into()),
            tool_filter: Some(ToolFilter::AllowList(["read".to_string(), "write".to_string()].into())),
            consent_mode: ConsentMode::Autonomous,
            created_at: 2000,
            created_by: creator,
            forked_from: Some(parent_id),
            fork_kind: Some(ForkKind::Full),
            archived_at: None,
            workspace_id: None,
            preset_id: None,
        };
        db.insert_context(&child).unwrap();

        // Read back and verify all 15 fields
        let recovered = db.get_context(child_id).unwrap().expect("child not found");
        assert_eq!(recovered.context_id, child_id);
        assert_eq!(recovered.kernel_id, kid);
        assert_eq!(recovered.label, Some("child-fork".into()));
        assert_eq!(recovered.provider, Some("google".into()));
        assert_eq!(recovered.model, Some("gemini-2.0-flash".into()));
        assert_eq!(recovered.system_prompt, Some("Be concise.".into()));
        assert!(matches!(recovered.tool_filter, Some(ToolFilter::AllowList(ref s)) if s.len() == 2));
        assert_eq!(recovered.consent_mode, ConsentMode::Autonomous);
        assert_eq!(recovered.created_at, 2000);
        assert_eq!(recovered.created_by, creator);
        assert_eq!(recovered.forked_from, Some(parent_id));
        assert_eq!(recovered.fork_kind, Some(ForkKind::Full));
        assert!(recovered.archived_at.is_none());
        assert!(recovered.workspace_id.is_none());
        assert!(recovered.preset_id.is_none());

        // Verify list_active_contexts returns both
        let active = db.list_active_contexts(kid).unwrap();
        assert_eq!(active.len(), 2);

        // Verify to_context() preserves forked_from
        let ctx = recovered.to_context();
        assert_eq!(ctx.forked_from, Some(parent_id));
        assert_eq!(ctx.created_by, creator);

        // Verify update_tool_filter roundtrip
        let new_filter = Some(ToolFilter::All);
        db.update_tool_filter(child_id, &new_filter).unwrap();
        let updated = db.get_context(child_id).unwrap().unwrap();
        assert_eq!(updated.tool_filter, Some(ToolFilter::All));

        // Verify update_model roundtrip
        db.update_model(child_id, Some("deepseek"), Some("deepseek-r1")).unwrap();
        let updated = db.get_context(child_id).unwrap().unwrap();
        assert_eq!(updated.provider, Some("deepseek".into()));
        assert_eq!(updated.model, Some("deepseek-r1".into()));
    }

    // ── 22. FK violation produces Validation, not LabelConflict ──────────

    #[test]
    fn fk_violation_is_validation_error() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();

        // Reference a workspace_id that doesn't exist
        let row = ContextRow {
            context_id: ContextId::new(),
            kernel_id: kid,
            label: Some("fk-test".into()),
            provider: None,
            model: None,
            system_prompt: None,
            tool_filter: None,
            consent_mode: ConsentMode::default(),
            created_at: now_millis() as i64,
            created_by: PrincipalId::new(),
            forked_from: None,
            fork_kind: None,
            archived_at: None,
            workspace_id: Some(WorkspaceId::new()), // doesn't exist
            preset_id: None,
        };
        let err = db.insert_context(&row).unwrap_err();
        assert!(
            matches!(err, KernelDbError::Validation(_)),
            "expected Validation for FK violation, got: {err}"
        );
    }

    // ── 23. Context shell CRUD ────────────────────────────────────────

    #[test]
    fn context_shell_upsert_and_get() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let ctx = make_context_row(kid, Some("shell-test"));
        db.insert_context(&ctx).unwrap();

        // Initially none
        assert!(db.get_context_shell(ctx.context_id).unwrap().is_none());

        // Insert
        let row = ContextShellRow {
            context_id: ctx.context_id,
            cwd: Some("/home/user/src/kaijutsu".into()),
            init_script: None,
            updated_at: now_millis() as i64,
        };
        db.upsert_context_shell(&row).unwrap();

        let loaded = db.get_context_shell(ctx.context_id).unwrap().unwrap();
        assert_eq!(loaded.cwd, Some("/home/user/src/kaijutsu".into()));
        assert!(loaded.init_script.is_none());

        // Update (upsert changes cwd)
        let row2 = ContextShellRow {
            context_id: ctx.context_id,
            cwd: Some("/tmp/work".into()),
            init_script: Some("set -o strict".into()),
            updated_at: now_millis() as i64,
        };
        db.upsert_context_shell(&row2).unwrap();

        let loaded = db.get_context_shell(ctx.context_id).unwrap().unwrap();
        assert_eq!(loaded.cwd, Some("/tmp/work".into()));
        assert_eq!(loaded.init_script, Some("set -o strict".into()));
    }

    #[test]
    fn context_shell_get_unknown() {
        let db = KernelDb::in_memory().unwrap();
        assert!(db.get_context_shell(ContextId::new()).unwrap().is_none());
    }

    #[test]
    fn context_shell_copy() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let src = make_context_row(kid, Some("src"));
        let tgt = make_context_row(kid, Some("tgt"));
        db.insert_context(&src).unwrap();
        db.insert_context(&tgt).unwrap();

        // Copy from context with shell config
        let row = ContextShellRow {
            context_id: src.context_id,
            cwd: Some("/home/user/project".into()),
            init_script: Some("alias ll='ls -la'".into()),
            updated_at: now_millis() as i64,
        };
        db.upsert_context_shell(&row).unwrap();

        assert!(db.copy_context_shell(src.context_id, tgt.context_id).unwrap());

        let copied = db.get_context_shell(tgt.context_id).unwrap().unwrap();
        assert_eq!(copied.cwd, Some("/home/user/project".into()));
        assert_eq!(copied.init_script, Some("alias ll='ls -la'".into()));
    }

    #[test]
    fn context_shell_copy_empty() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let src = make_context_row(kid, Some("src"));
        let tgt = make_context_row(kid, Some("tgt"));
        db.insert_context(&src).unwrap();
        db.insert_context(&tgt).unwrap();

        // Copy from context with no shell config → returns false
        assert!(!db.copy_context_shell(src.context_id, tgt.context_id).unwrap());
        assert!(db.get_context_shell(tgt.context_id).unwrap().is_none());
    }

    #[test]
    fn context_shell_cascade_delete() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let ctx = make_context_row(kid, Some("cascade"));
        db.insert_context(&ctx).unwrap();

        db.upsert_context_shell(&ContextShellRow {
            context_id: ctx.context_id,
            cwd: Some("/tmp".into()),
            init_script: None,
            updated_at: now_millis() as i64,
        }).unwrap();
        assert!(db.get_context_shell(ctx.context_id).unwrap().is_some());

        // Delete context → shell row should cascade
        db.delete_context(ctx.context_id).unwrap();
        assert!(db.get_context_shell(ctx.context_id).unwrap().is_none());
    }

    // ── 24. Context env CRUD ──────────────────────────────────────────

    #[test]
    fn context_env_set_and_get() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let ctx = make_context_row(kid, Some("env-test"));
        db.insert_context(&ctx).unwrap();

        // Initially empty
        let vars = db.get_context_env(ctx.context_id).unwrap();
        assert!(vars.is_empty());

        // Set vars
        db.set_context_env(ctx.context_id, "RUST_LOG", "debug").unwrap();
        db.set_context_env(ctx.context_id, "EDITOR", "vim").unwrap();

        let vars = db.get_context_env(ctx.context_id).unwrap();
        assert_eq!(vars.len(), 2);
        // Ordered by key
        assert_eq!(vars[0].key, "EDITOR");
        assert_eq!(vars[0].value, "vim");
        assert_eq!(vars[1].key, "RUST_LOG");
        assert_eq!(vars[1].value, "debug");
    }

    #[test]
    fn context_env_upsert() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let ctx = make_context_row(kid, Some("env-upsert"));
        db.insert_context(&ctx).unwrap();

        db.set_context_env(ctx.context_id, "RUST_LOG", "debug").unwrap();
        db.set_context_env(ctx.context_id, "RUST_LOG", "trace").unwrap();

        let vars = db.get_context_env(ctx.context_id).unwrap();
        assert_eq!(vars.len(), 1);
        assert_eq!(vars[0].value, "trace");
    }

    #[test]
    fn context_env_get_empty() {
        let db = KernelDb::in_memory().unwrap();
        let vars = db.get_context_env(ContextId::new()).unwrap();
        assert!(vars.is_empty());
    }

    #[test]
    fn context_env_delete() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let ctx = make_context_row(kid, Some("env-del"));
        db.insert_context(&ctx).unwrap();

        db.set_context_env(ctx.context_id, "FOO", "bar").unwrap();
        assert!(db.delete_context_env(ctx.context_id, "FOO").unwrap());
        assert!(!db.delete_context_env(ctx.context_id, "FOO").unwrap()); // already gone
        assert!(!db.delete_context_env(ctx.context_id, "NEVER_SET").unwrap());
    }

    #[test]
    fn context_env_clear() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let ctx = make_context_row(kid, Some("env-clear"));
        db.insert_context(&ctx).unwrap();

        db.set_context_env(ctx.context_id, "A", "1").unwrap();
        db.set_context_env(ctx.context_id, "B", "2").unwrap();
        db.set_context_env(ctx.context_id, "C", "3").unwrap();

        assert_eq!(db.clear_context_env(ctx.context_id).unwrap(), 3);
        assert!(db.get_context_env(ctx.context_id).unwrap().is_empty());
    }

    #[test]
    fn context_env_copy() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let src = make_context_row(kid, Some("env-src"));
        let tgt = make_context_row(kid, Some("env-tgt"));
        db.insert_context(&src).unwrap();
        db.insert_context(&tgt).unwrap();

        db.set_context_env(src.context_id, "RUST_LOG", "debug").unwrap();
        db.set_context_env(src.context_id, "EDITOR", "vim").unwrap();
        db.set_context_env(src.context_id, "SHELL", "/bin/bash").unwrap();

        let count = db.copy_context_env(src.context_id, tgt.context_id).unwrap();
        assert_eq!(count, 3);

        let vars = db.get_context_env(tgt.context_id).unwrap();
        assert_eq!(vars.len(), 3);
    }

    #[test]
    fn context_env_cascade_delete() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let ctx = make_context_row(kid, Some("env-cascade"));
        db.insert_context(&ctx).unwrap();

        db.set_context_env(ctx.context_id, "FOO", "bar").unwrap();
        db.set_context_env(ctx.context_id, "BAZ", "qux").unwrap();

        db.delete_context(ctx.context_id).unwrap();
        let vars = db.get_context_env(ctx.context_id).unwrap();
        assert!(vars.is_empty());
    }

    // ── 25. Workspace paths with read_only ────────────────────────────

    #[test]
    fn workspace_path_read_only_roundtrip() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let creator = PrincipalId::new();
        let now = now_millis() as i64;

        let ws = WorkspaceRow {
            workspace_id: WorkspaceId::new(),
            kernel_id: kid,
            label: "ro-test".into(),
            description: None,
            created_at: now,
            created_by: creator,
            archived_at: None,
        };
        db.insert_workspace(&ws).unwrap();

        let rw = WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: "/home/user/src".into(),
            read_only: false,
            created_at: now,
        };
        let ro = WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: "/home/user/docs".into(),
            read_only: true,
            created_at: now,
        };
        db.insert_workspace_path(&rw).unwrap();
        db.insert_workspace_path(&ro).unwrap();

        let paths = db.list_workspace_paths(ws.workspace_id).unwrap();
        assert_eq!(paths.len(), 2);
        // Ordered by path
        assert_eq!(paths[0].path, "/home/user/docs");
        assert!(paths[0].read_only);
        assert_eq!(paths[1].path, "/home/user/src");
        assert!(!paths[1].read_only);
    }

    // ── 26. Fork context config (composite) ──────────────────────────

    #[test]
    fn fork_context_config_full() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let src = make_context_row(kid, Some("fork-src"));
        let tgt = make_context_row(kid, Some("fork-tgt"));
        db.insert_context(&src).unwrap();
        db.insert_context(&tgt).unwrap();

        // Set up source with shell config + env vars
        db.upsert_context_shell(&ContextShellRow {
            context_id: src.context_id,
            cwd: Some("/home/user/src/kaijutsu".into()),
            init_script: None,
            updated_at: now_millis() as i64,
        }).unwrap();
        db.set_context_env(src.context_id, "RUST_LOG", "debug").unwrap();
        db.set_context_env(src.context_id, "EDITOR", "vim").unwrap();
        db.set_context_env(src.context_id, "TERM", "xterm-256color").unwrap();

        db.fork_context_config(src.context_id, tgt.context_id).unwrap();

        // Shell config copied
        let shell = db.get_context_shell(tgt.context_id).unwrap().unwrap();
        assert_eq!(shell.cwd, Some("/home/user/src/kaijutsu".into()));

        // Env vars copied
        let vars = db.get_context_env(tgt.context_id).unwrap();
        assert_eq!(vars.len(), 3);
    }

    #[test]
    fn fork_context_config_empty_source() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let src = make_context_row(kid, Some("empty-src"));
        let tgt = make_context_row(kid, Some("empty-tgt"));
        db.insert_context(&src).unwrap();
        db.insert_context(&tgt).unwrap();

        // Fork from context with no config → no error, no data on target
        db.fork_context_config(src.context_id, tgt.context_id).unwrap();

        assert!(db.get_context_shell(tgt.context_id).unwrap().is_none());
        assert!(db.get_context_env(tgt.context_id).unwrap().is_empty());
    }

    // ── 27. Context workspace paths query ─────────────────────────────

    #[test]
    fn context_workspace_paths_bound() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let creator = PrincipalId::new();
        let now = now_millis() as i64;

        let ws = WorkspaceRow {
            workspace_id: WorkspaceId::new(),
            kernel_id: kid,
            label: "project".into(),
            description: None,
            created_at: now,
            created_by: creator,
            archived_at: None,
        };
        db.insert_workspace(&ws).unwrap();

        db.insert_workspace_path(&WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: "/home/user/src/project".into(),
            read_only: false,
            created_at: now,
        }).unwrap();
        db.insert_workspace_path(&WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: "/home/user/docs".into(),
            read_only: true,
            created_at: now,
        }).unwrap();

        // Create context with workspace bound
        let mut ctx = make_context_row(kid, Some("bound"));
        ctx.workspace_id = Some(ws.workspace_id);
        db.insert_context(&ctx).unwrap();

        let paths = db.context_workspace_paths(ctx.context_id).unwrap();
        let paths = paths.unwrap();
        assert_eq!(paths.len(), 2);
    }

    #[test]
    fn context_workspace_paths_unbound() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let ctx = make_context_row(kid, Some("unbound"));
        db.insert_context(&ctx).unwrap();

        let paths = db.context_workspace_paths(ctx.context_id).unwrap();
        assert!(paths.is_none());
    }

    #[test]
    fn get_or_create_kernel_id_stable_across_calls() {
        let db = KernelDb::in_memory().unwrap();
        let id1 = db.get_or_create_kernel_id().unwrap();
        let id2 = db.get_or_create_kernel_id().unwrap();
        assert_eq!(id1, id2, "should return same ID on second call");
    }

    #[test]
    fn get_or_create_kernel_id_fresh_on_empty_db() {
        let db = KernelDb::in_memory().unwrap();
        let id = db.get_or_create_kernel_id().unwrap();
        // Should be a valid UUIDv7 (non-zero)
        assert_ne!(id.as_bytes(), &[0u8; 16]);
    }

    #[test]
    fn get_or_create_kernel_id_adopts_from_existing_contexts() {
        let db = KernelDb::in_memory().unwrap();

        // Simulate pre-stable-ID era: contexts exist with an old kernel_id
        let old_kid = KernelId::new();
        let ctx = make_context_row(old_kid, Some("legacy"));
        db.insert_context(&ctx).unwrap();

        // kernel table is empty — should adopt the context's kernel_id
        let id = db.get_or_create_kernel_id().unwrap();
        assert_eq!(id, old_kid, "should adopt kernel_id from existing contexts");

        // Subsequent call returns the same
        let id2 = db.get_or_create_kernel_id().unwrap();
        assert_eq!(id2, old_kid);
    }
}
