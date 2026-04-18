//! SQLite persistence for kernel context metadata, edges, presets, and workspaces.
//!
//! Pattern follows `auth_db.rs` and `db.rs`: single `Connection`, WAL mode,
//! BLOB-encoded typed IDs, in-memory constructor for tests.
//!
//! All timestamps are Unix milliseconds (matching `now_millis()`).

use std::collections::HashSet;
use std::path::Path;
use std::str::FromStr;

use rusqlite::{Connection, Result as SqliteResult, params};
use tracing::{info, warn};

use kaijutsu_types::{
    ConsentMode, ContextId, ContextState, DocKind, EdgeKind, ForkKind, KernelId, PresetId,
    PrincipalId, WorkspaceId,
};

use crate::mcp::binding::ContextToolBinding;
use crate::mcp::types::InstanceId;

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
    pub consent_mode: ConsentMode,
    pub context_state: ContextState,
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
    pub action_kaish_script_id: Option<String>,
    pub action_result_text: Option<String>,
    pub action_is_error: Option<bool>,
    pub action_deny_reason: Option<String>,
    pub action_log_target: Option<String>,
    pub action_log_level: Option<String>,
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

/// A document row — CRDT content layer.
#[derive(Debug, Clone)]
pub struct DocumentRow {
    pub document_id: ContextId,
    pub kernel_id: KernelId,
    pub workspace_id: WorkspaceId,
    pub doc_kind: DocKind,
    pub language: Option<String>,
    pub path: Option<String>,
    pub created_at: i64,
    pub created_by: PrincipalId,
}

/// A doc_snapshots row — compaction checkpoint.
#[derive(Debug, Clone)]
pub struct DocSnapshotRow {
    pub document_id: ContextId,
    pub seq: i64,
    pub version: i64,
    pub state: Vec<u8>,
    pub content: String,
    pub created_at: i64,
}

/// An input_doc_snapshots row — compaction checkpoint for input docs.
#[derive(Debug, Clone)]
pub struct InputDocSnapshotRow {
    pub document_id: ContextId,
    pub seq: i64,
    pub state: Vec<u8>,
    pub content: String,
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
-- ── Kernel Identity ─────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS kernel (
    kernel_id  BLOB NOT NULL PRIMARY KEY,
    created_at INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER))
);

-- ── Workspaces ──────────────────────────────────────────────────
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

-- ── Presets ─────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS presets (
    preset_id    BLOB NOT NULL PRIMARY KEY,
    kernel_id    BLOB NOT NULL,
    label        TEXT NOT NULL,
    description  TEXT,
    provider     TEXT,
    model        TEXT,
    system_prompt TEXT,
    consent_mode TEXT NOT NULL DEFAULT 'collaborative',
    created_at   INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    created_by   BLOB NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_presets_label
    ON presets(kernel_id, label);

-- ── Documents (CRDT content layer) ─────────────────────────────
CREATE TABLE IF NOT EXISTS documents (
    document_id  BLOB NOT NULL PRIMARY KEY,
    kernel_id    BLOB NOT NULL,
    workspace_id BLOB NOT NULL REFERENCES workspaces(workspace_id) ON DELETE RESTRICT,
    doc_kind     TEXT NOT NULL DEFAULT 'conversation',
    language     TEXT,
    path         TEXT,
    created_at   INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    created_by   BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_documents_kernel
    ON documents(kernel_id);
CREATE INDEX IF NOT EXISTS idx_documents_workspace
    ON documents(workspace_id);
CREATE INDEX IF NOT EXISTS idx_documents_kind
    ON documents(kernel_id, doc_kind);
CREATE UNIQUE INDEX IF NOT EXISTS idx_documents_path
    ON documents(workspace_id, path) WHERE path IS NOT NULL;

-- ── Contexts (conversation metadata, extends documents) ────────
CREATE TABLE IF NOT EXISTS contexts (
    context_id   BLOB NOT NULL PRIMARY KEY
        REFERENCES documents(document_id) ON DELETE CASCADE,
    kernel_id    BLOB NOT NULL,
    label        TEXT,
    provider     TEXT,
    model        TEXT,
    system_prompt TEXT,
    consent_mode TEXT NOT NULL DEFAULT 'collaborative',
    context_state TEXT NOT NULL DEFAULT 'live',
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

-- ── Context Edges ───────────────────────────────────────────────
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
CREATE INDEX IF NOT EXISTS idx_edges_source ON context_edges(source_id);
CREATE INDEX IF NOT EXISTS idx_edges_target ON context_edges(target_id);

-- ── Op-Log Persistence ──────────────────────────────────────────
-- Append-only journal: each mutation writes one row with the delta.
CREATE TABLE IF NOT EXISTS oplog (
    document_id BLOB    NOT NULL,
    seq         INTEGER NOT NULL,
    payload     BLOB    NOT NULL,
    created_at  INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    PRIMARY KEY (document_id, seq),
    FOREIGN KEY (document_id) REFERENCES documents(document_id) ON DELETE CASCADE
) WITHOUT ROWID;

-- Compaction checkpoints: latest snapshot per document.
CREATE TABLE IF NOT EXISTS doc_snapshots (
    document_id BLOB    NOT NULL PRIMARY KEY,
    seq         INTEGER NOT NULL,
    version     INTEGER NOT NULL,
    state       BLOB    NOT NULL,
    content     TEXT    NOT NULL,
    created_at  INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    FOREIGN KEY (document_id) REFERENCES documents(document_id) ON DELETE CASCADE
);

-- ── Input Document Op-Log ───────────────────────────────────────
CREATE TABLE IF NOT EXISTS input_oplog (
    document_id BLOB    NOT NULL,
    seq         INTEGER NOT NULL,
    payload     BLOB    NOT NULL,
    created_at  INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    PRIMARY KEY (document_id, seq),
    FOREIGN KEY (document_id) REFERENCES documents(document_id) ON DELETE CASCADE
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS input_doc_snapshots (
    document_id BLOB    NOT NULL PRIMARY KEY,
    seq         INTEGER NOT NULL,
    state       BLOB    NOT NULL,
    content     TEXT    NOT NULL,
    created_at  INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    FOREIGN KEY (document_id) REFERENCES documents(document_id) ON DELETE CASCADE
);

-- ── Context Shell / Env ─────────────────────────────────────────
CREATE TABLE IF NOT EXISTS context_shell (
    context_id  BLOB NOT NULL PRIMARY KEY REFERENCES contexts(context_id) ON DELETE CASCADE,
    cwd         TEXT,
    init_script TEXT,
    updated_at  INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER))
);

-- ── Context Tool Bindings (Phase 5, D-54) ───────────────────────
-- Per-context instance visibility + sticky name resolution, persisted so
-- curation survives kernel restart. First-touch loads from this set of
-- tables; fall-back to "bind all registered" only when the parent row is
-- absent. Normalized per feedback_sql_schema.md: the Rust struct
-- ContextToolBinding reconstructs by joining.
CREATE TABLE IF NOT EXISTS context_bindings (
    context_id BLOB    NOT NULL PRIMARY KEY REFERENCES contexts(context_id) ON DELETE CASCADE,
    updated_at INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER))
);

-- Ordered list of allowed instances (Vec<InstanceId>). order_idx preserves
-- the tiebreak semantic for Auto-mode resolution (D-20 / §4.2).
CREATE TABLE IF NOT EXISTS context_binding_instances (
    context_id  BLOB    NOT NULL REFERENCES context_bindings(context_id) ON DELETE CASCADE,
    instance_id TEXT    NOT NULL,
    order_idx   INTEGER NOT NULL,
    PRIMARY KEY (context_id, instance_id)
);
CREATE INDEX IF NOT EXISTS idx_binding_instances_lookup
    ON context_binding_instances(instance_id);

-- Sticky name resolution map (HashMap<visible_name, (instance, tool)>).
-- visible_name is the LLM-observable name; (instance_id, original_tool) is
-- the resolved target. Preserving this across restart is exit criterion #2.
CREATE TABLE IF NOT EXISTS context_binding_names (
    context_id    BLOB NOT NULL REFERENCES context_bindings(context_id) ON DELETE CASCADE,
    visible_name  TEXT NOT NULL,
    instance_id   TEXT NOT NULL,
    original_tool TEXT NOT NULL,
    PRIMARY KEY (context_id, visible_name)
);
CREATE INDEX IF NOT EXISTS idx_binding_names_target
    ON context_binding_names(context_id, instance_id, original_tool);

CREATE TABLE IF NOT EXISTS context_env (
    context_id BLOB NOT NULL REFERENCES contexts(context_id) ON DELETE CASCADE,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    PRIMARY KEY (context_id, key)
);
CREATE INDEX IF NOT EXISTS idx_ctx_env ON context_env(context_id);

-- ── Hooks (hook persistence follow-up) ──────────────────────────
-- Global match-action hook entries. One row per entry; load ordered by
-- (phase, priority ASC, insertion_idx ASC) so HookTables.entries Vec is
-- reconstructed in insertion order. `insertion_idx` is a monotonic
-- counter per phase computed inside the INSERT statement so callers
-- don't have to track it. `action_kind` is the tagged-union
-- discriminator matching HookActionWire; variant-specific columns are
-- nullable and set only for the matching kind. Not FK-linked — hooks
-- are global, not per-context.
CREATE TABLE IF NOT EXISTS hooks (
    hook_id                TEXT    NOT NULL PRIMARY KEY,
    phase                  TEXT    NOT NULL,
    priority               INTEGER NOT NULL,
    insertion_idx          INTEGER NOT NULL,
    match_instance         TEXT,
    match_tool             TEXT,
    match_context          BLOB,
    match_principal        TEXT,
    action_kind            TEXT    NOT NULL,
    action_builtin_name    TEXT,
    action_kaish_script_id TEXT,
    action_result_text     TEXT,
    action_is_error        INTEGER,
    action_deny_reason     TEXT,
    action_log_target      TEXT,
    action_log_level       TEXT,
    updated_at             INTEGER NOT NULL
        DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    UNIQUE (phase, insertion_idx)
);
CREATE INDEX IF NOT EXISTS idx_hooks_phase_priority
    ON hooks(phase, priority, insertion_idx);
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
        Some(b) => ContextId::try_from_slice(&b).map(Some).ok_or_else(|| {
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
        Some(b) => WorkspaceId::try_from_slice(&b).map(Some).ok_or_else(|| {
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
        Some(b) => PresetId::try_from_slice(&b).map(Some).ok_or_else(|| {
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

/// Parse ConsentMode from TEXT column.
fn consent_mode_from_sql(s: &str) -> ConsentMode {
    ConsentMode::from_str(s).unwrap_or_else(|_| {
        warn!(mode = %s, "unknown ConsentMode in DB, defaulting to Collaborative");
        ConsentMode::Collaborative
    })
}

/// Parse ContextState from TEXT column.
fn context_state_from_sql(s: &str) -> ContextState {
    ContextState::from_str(s).unwrap_or_else(|_| {
        warn!(state = %s, "unknown ContextState in DB, defaulting to Live");
        ContextState::Live
    })
}

/// Parse ForkKind from TEXT column.
fn fork_kind_from_sql(s: Option<String>) -> Option<ForkKind> {
    s.and_then(|v| ForkKind::from_str(&v).ok())
}

/// Parse DocKind from TEXT column.
fn doc_kind_from_sql(s: &str) -> DocKind {
    DocKind::from_str(s).unwrap_or_else(|_| {
        warn!(kind = %s, "unknown DocKind in DB, defaulting to Conversation");
        DocKind::Conversation
    })
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
        return Err(KernelDbError::InvalidLabel(format!(
            "label '{}' must not contain ':'",
            label
        )));
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
    if let rusqlite::Error::SqliteFailure(err, ref detail) = e
        && err.code == rusqlite::ErrorCode::ConstraintViolation
    {
        // SQLITE_CONSTRAINT_FOREIGNKEY = 787
        if err.extended_code == 787 {
            let detail_str = detail.as_deref().unwrap_or("foreign key constraint failed");
            return KernelDbError::Validation(detail_str.to_string());
        }
        // SQLITE_CONSTRAINT_UNIQUE = 2067, or any other constraint
        return KernelDbError::LabelConflict(msg.into());
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

    /// Idempotent column migrations for existing databases.
    ///
    /// `CREATE TABLE IF NOT EXISTS` won't add columns to an existing table,
    /// so new columns must be added via ALTER TABLE. Each migration checks
    /// whether the column already exists before altering.
    fn run_migrations(conn: &Connection) -> KernelDbResult<()> {
        // 2026-03-27: add context_state column (ContextState enum)
        let has_context_state: bool = conn
            .prepare("SELECT context_state FROM contexts LIMIT 0")
            .is_ok();
        if !has_context_state {
            info!("Migration: adding context_state column to contexts table");
            conn.execute_batch(
                "ALTER TABLE contexts ADD COLUMN context_state TEXT NOT NULL DEFAULT 'live';",
            )?;
        }
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
        Self::run_migrations(&conn)?;
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
        let existing: Option<Vec<u8>> = self
            .conn
            .query_row("SELECT kernel_id FROM kernel LIMIT 1", [], |row| row.get(0))
            .ok();

        if let Some(bytes) = existing {
            if let Some(id) = KernelId::try_from_slice(&bytes) {
                return Ok(id);
            }
            // Corrupt row — fall through to create fresh
            warn!("Corrupt kernel_id in kernel table, creating fresh");
        }

        // No kernel row yet. Check if contexts exist from a previous run
        // (before the kernel table was added) and adopt their kernel_id.
        let adopted: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT kernel_id FROM contexts ORDER BY created_at DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .ok();

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
        let distinct_count: u32 = self
            .conn
            .query_row(
                "SELECT COUNT(DISTINCT kernel_id) FROM contexts",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
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
    // Auto-Workspaces
    // ========================================================================

    /// Get or create the `__system` workspace (for config docs).
    /// Returns the workspace ID.
    pub fn get_or_create_system_workspace(
        &self,
        kernel_id: KernelId,
        created_by: PrincipalId,
    ) -> KernelDbResult<WorkspaceId> {
        self.get_or_create_builtin_workspace(
            kernel_id,
            "__system",
            "System configuration documents",
            created_by,
        )
    }

    /// Get or create the `__default` workspace (for conversations and file cache).
    /// Returns the workspace ID.
    pub fn get_or_create_default_workspace(
        &self,
        kernel_id: KernelId,
        created_by: PrincipalId,
    ) -> KernelDbResult<WorkspaceId> {
        self.get_or_create_builtin_workspace(
            kernel_id,
            "__default",
            "Default workspace",
            created_by,
        )
    }

    /// Get or create a built-in workspace by label.
    fn get_or_create_builtin_workspace(
        &self,
        kernel_id: KernelId,
        label: &str,
        description: &str,
        created_by: PrincipalId,
    ) -> KernelDbResult<WorkspaceId> {
        if let Some(ws) = self.get_workspace_by_label(kernel_id, label)? {
            return Ok(ws.workspace_id);
        }
        let ws_id = WorkspaceId::new();
        let row = WorkspaceRow {
            workspace_id: ws_id,
            kernel_id,
            label: label.to_string(),
            description: Some(description.to_string()),
            created_at: now_millis(),
            created_by,
            archived_at: None,
        };
        self.insert_workspace(&row)?;
        info!(workspace_id = %ws_id.to_hex(), label, "Created built-in workspace");
        Ok(ws_id)
    }

    // ========================================================================
    // Document CRUD
    // ========================================================================

    /// Insert a new document.
    pub fn insert_document(&self, row: &DocumentRow) -> KernelDbResult<()> {
        self.conn
            .execute(
                "INSERT INTO documents (
                document_id, kernel_id, workspace_id, doc_kind,
                language, path, created_at, created_by
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    blob_param(row.document_id.as_bytes()),
                    blob_param(row.kernel_id.as_bytes()),
                    blob_param(row.workspace_id.as_bytes()),
                    row.doc_kind.as_str(),
                    row.language,
                    row.path,
                    row.created_at,
                    blob_param(row.created_by.as_bytes()),
                ],
            )
            .map_err(|e| map_unique_violation(e, "document already exists or path conflict"))?;
        Ok(())
    }

    /// Insert a document, ignoring if it already exists (idempotent).
    pub fn insert_document_or_ignore(&self, row: &DocumentRow) -> KernelDbResult<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO documents (
                document_id, kernel_id, workspace_id, doc_kind,
                language, path, created_at, created_by
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                blob_param(row.document_id.as_bytes()),
                blob_param(row.kernel_id.as_bytes()),
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
            "SELECT document_id, kernel_id, workspace_id, doc_kind,
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

    /// List all documents for a kernel.
    pub fn list_documents(&self, kernel_id: KernelId) -> KernelDbResult<Vec<DocumentRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT document_id, kernel_id, workspace_id, doc_kind,
                    language, path, created_at, created_by
             FROM documents WHERE kernel_id = ?1
             ORDER BY created_at",
        )?;
        let rows = stmt.query_map(params![blob_param(kernel_id.as_bytes())], |row| {
            row_to_document_row(row)
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// List documents filtered by kind for a kernel.
    pub fn list_documents_by_kind(
        &self,
        kernel_id: KernelId,
        kind: DocKind,
    ) -> KernelDbResult<Vec<DocumentRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT document_id, kernel_id, workspace_id, doc_kind,
                    language, path, created_at, created_by
             FROM documents WHERE kernel_id = ?1 AND doc_kind = ?2
             ORDER BY created_at",
        )?;
        let rows = stmt.query_map(
            params![blob_param(kernel_id.as_bytes()), kind.as_str()],
            row_to_document_row,
        )?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Delete a document (CASCADE deletes snapshots, input_docs, and context).
    pub fn delete_document(&self, id: ContextId) -> KernelDbResult<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM documents WHERE document_id = ?1",
            params![blob_param(id.as_bytes())],
        )?;
        Ok(deleted > 0)
    }

    // ========================================================================
    // Op-Log Persistence
    // ========================================================================

    /// Append an op to the journal for a document.
    pub fn append_op(
        &self,
        document_id: ContextId,
        seq: i64,
        payload: &[u8],
    ) -> KernelDbResult<()> {
        self.conn.execute(
            "INSERT INTO oplog (document_id, seq, payload) VALUES (?1, ?2, ?3)",
            params![blob_param(document_id.as_bytes()), seq, payload],
        )?;
        Ok(())
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
            "SELECT document_id, seq, version, state, content, created_at
             FROM doc_snapshots WHERE document_id = ?1",
        )?;
        let mut rows = stmt.query(params![blob_param(document_id.as_bytes())])?;
        if let Some(row) = rows.next()? {
            Ok(Some(DocSnapshotRow {
                document_id: read_context_id(row, 0)?,
                seq: row.get(1)?,
                version: row.get(2)?,
                state: row.get(3)?,
                content: row.get(4)?,
                created_at: row.get(5)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// Write a compaction snapshot and truncate the oplog up to that seq.
    /// Must be called with exclusive access (the Mutex guarantees this).
    pub fn write_snapshot_and_truncate(
        &mut self,
        document_id: ContextId,
        seq: i64,
        version: i64,
        state: &[u8],
        content: &str,
    ) -> KernelDbResult<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO doc_snapshots (document_id, seq, version, state, content)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                blob_param(document_id.as_bytes()),
                seq,
                version,
                state,
                content,
            ],
        )?;
        tx.execute(
            "DELETE FROM oplog WHERE document_id = ?1 AND seq <= ?2",
            params![blob_param(document_id.as_bytes()), seq],
        )?;
        tx.commit()?;
        Ok(())
    }

    // ========================================================================
    // Input Document Op-Log
    // ========================================================================

    /// Append an input doc op to the journal.
    pub fn append_input_op(
        &self,
        document_id: ContextId,
        seq: i64,
        payload: &[u8],
    ) -> KernelDbResult<()> {
        self.conn.execute(
            "INSERT INTO input_oplog (document_id, seq, payload) VALUES (?1, ?2, ?3)",
            params![blob_param(document_id.as_bytes()), seq, payload],
        )?;
        Ok(())
    }

    /// Load input oplog entries after a given seq.
    pub fn load_input_oplog_since(
        &self,
        document_id: ContextId,
        after_seq: i64,
    ) -> KernelDbResult<Vec<(i64, Vec<u8>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, payload FROM input_oplog
             WHERE document_id = ?1 AND seq > ?2
             ORDER BY seq",
        )?;
        let rows = stmt.query_map(
            params![blob_param(document_id.as_bytes()), after_seq],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Load the latest input doc compaction snapshot.
    pub fn load_latest_input_snapshot(
        &self,
        document_id: ContextId,
    ) -> KernelDbResult<Option<InputDocSnapshotRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT document_id, seq, state, content, created_at
             FROM input_doc_snapshots WHERE document_id = ?1",
        )?;
        let mut rows = stmt.query(params![blob_param(document_id.as_bytes())])?;
        if let Some(row) = rows.next()? {
            Ok(Some(InputDocSnapshotRow {
                document_id: read_context_id(row, 0)?,
                seq: row.get(1)?,
                state: row.get(2)?,
                content: row.get(3)?,
                created_at: row.get(4)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// Write an input doc compaction snapshot and truncate its oplog.
    pub fn write_input_snapshot_and_truncate(
        &mut self,
        document_id: ContextId,
        seq: i64,
        state: &[u8],
        content: &str,
    ) -> KernelDbResult<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO input_doc_snapshots (document_id, seq, state, content)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                blob_param(document_id.as_bytes()),
                seq,
                state,
                content,
            ],
        )?;
        tx.execute(
            "DELETE FROM input_oplog WHERE document_id = ?1 AND seq <= ?2",
            params![blob_param(document_id.as_bytes()), seq],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// List document IDs that have input oplog entries or snapshots.
    pub fn list_input_doc_ids(
        &self,
        kernel_id: KernelId,
    ) -> KernelDbResult<Vec<ContextId>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT d.document_id FROM documents d
             WHERE d.kernel_id = ?1
               AND (EXISTS (SELECT 1 FROM input_oplog o WHERE o.document_id = d.document_id)
                 OR EXISTS (SELECT 1 FROM input_doc_snapshots s WHERE s.document_id = d.document_id))",
        )?;
        let rows = stmt.query_map(
            params![blob_param(kernel_id.as_bytes())],
            |row| read_context_id(row, 0),
        )?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    // ========================================================================
    // Context CRUD
    // ========================================================================

    /// Insert a document + context pair in one call (convenience for conversations).
    ///
    /// Creates the document row first, then the context row. Uses the
    /// default workspace if no workspace_id is set on the context.
    pub fn insert_context_with_document(
        &self,
        row: &ContextRow,
        default_workspace_id: WorkspaceId,
    ) -> KernelDbResult<()> {
        let ws_id = row.workspace_id.unwrap_or(default_workspace_id);
        self.insert_document_or_ignore(&DocumentRow {
            document_id: row.context_id,
            kernel_id: row.kernel_id,
            workspace_id: ws_id,
            doc_kind: DocKind::Conversation,
            language: None,
            path: None,
            created_at: row.created_at,
            created_by: row.created_by,
        })?;
        self.insert_context(row)
    }

    /// Insert a new context.
    ///
    /// The corresponding document row must already exist (FK enforced).
    pub fn insert_context(&self, row: &ContextRow) -> KernelDbResult<()> {
        if let Some(ref label) = row.label {
            validate_label(label)?;
        }

        self.conn
            .execute(
                "INSERT INTO contexts (
                context_id, kernel_id, label, provider, model,
                system_prompt, consent_mode, context_state,
                created_at, created_by, forked_from, fork_kind,
                archived_at, workspace_id, preset_id
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7, ?8, ?9,
                ?10, ?11, ?12,
                ?13, ?14, ?15
            )",
                params![
                    blob_param(row.context_id.as_bytes()),
                    blob_param(row.kernel_id.as_bytes()),
                    row.label,
                    row.provider,
                    row.model,
                    row.system_prompt,
                    row.consent_mode.as_str(),
                    row.context_state.as_str(),
                    row.created_at,
                    blob_param(row.created_by.as_bytes()),
                    row.forked_from.as_ref().map(|id| id.as_bytes().to_vec()),
                    row.fork_kind.map(|fk| fk.as_str().to_string()),
                    row.archived_at,
                    row.workspace_id.as_ref().map(|id| id.as_bytes().to_vec()),
                    row.preset_id.as_ref().map(|id| id.as_bytes().to_vec()),
                ],
            )
            .map_err(|e| {
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
                    system_prompt, consent_mode, context_state,
                    created_at, created_by, forked_from, fork_kind,
                    archived_at, workspace_id, preset_id
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

        let updated = self
            .conn
            .execute(
                "UPDATE contexts SET label = ?1 WHERE context_id = ?2",
                params![label, blob_param(id.as_bytes())],
            )
            .map_err(|e| {
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
        consent_mode: ConsentMode,
    ) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE contexts SET system_prompt = ?1, consent_mode = ?2
             WHERE context_id = ?3",
            params![
                system_prompt,
                consent_mode.as_str(),
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
        let now = now_millis();
        let updated = self.conn.execute(
            "UPDATE contexts SET archived_at = ?1
             WHERE context_id = ?2 AND archived_at IS NULL",
            params![now, blob_param(id.as_bytes())],
        )?;
        Ok(updated > 0)
    }

    /// Update the lifecycle state of a context.
    pub fn update_context_state(
        &self,
        id: ContextId,
        state: ContextState,
    ) -> KernelDbResult<bool> {
        let updated = self.conn.execute(
            "UPDATE contexts SET context_state = ?1 WHERE context_id = ?2",
            params![state.as_str(), blob_param(id.as_bytes())],
        )?;
        Ok(updated > 0)
    }

    /// List active (non-archived) contexts for a kernel.
    pub fn list_active_contexts(&self, kernel_id: KernelId) -> KernelDbResult<Vec<ContextRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, kernel_id, label, provider, model,
                    system_prompt, consent_mode, context_state,
                    created_at, created_by, forked_from, fork_kind,
                    archived_at, workspace_id, preset_id
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
                    system_prompt, consent_mode, context_state,
                    created_at, created_by, forked_from, fork_kind,
                    archived_at, workspace_id, preset_id
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
    pub fn resolve_context(&self, kernel_id: KernelId, query: &str) -> KernelDbResult<ContextId> {
        // Load active contexts for this kernel (set is small, <100)
        let contexts = self.list_active_contexts(kernel_id)?;
        let items = contexts.iter().map(|c| (c.context_id, c.label.as_deref()));

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
                    c.system_prompt, c.consent_mode, c.context_state,
                    c.created_at, c.created_by, c.forked_from, c.fork_kind,
                    c.archived_at, c.workspace_id, c.preset_id
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
                    c.system_prompt, c.consent_mode, c.context_state,
                    c.created_at, c.created_by, c.forked_from, c.fork_kind,
                    c.archived_at, c.workspace_id, c.preset_id
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
                   c.system_prompt, c.consent_mode, c.context_state,
                   c.created_at, c.created_by, c.forked_from, c.fork_kind,
                   c.archived_at, c.workspace_id, c.preset_id,
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
                   c.system_prompt, c.consent_mode, c.context_state,
                   c.created_at, c.created_by, c.forked_from, c.fork_kind,
                   c.archived_at, c.workspace_id, c.preset_id,
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
    pub fn subtree_snapshot(&self, root_id: ContextId) -> KernelDbResult<Vec<(ContextRow, i64)>> {
        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE subtree(ctx_id, depth) AS (
                SELECT ?1, 0
                UNION ALL
                SELECT e.target_id, subtree.depth + 1
                FROM subtree
                JOIN context_edges e ON e.source_id = subtree.ctx_id AND e.kind = 'structural'
            )
            SELECT c.context_id, c.kernel_id, c.label, c.provider, c.model,
                   c.system_prompt, c.consent_mode, c.context_state,
                   c.created_at, c.created_by, c.forked_from, c.fork_kind,
                   c.archived_at, c.workspace_id, c.preset_id,
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
    fn would_create_cycle(&self, source: ContextId, target: ContextId) -> KernelDbResult<bool> {
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

        self.conn
            .execute(
                "INSERT INTO presets (
                preset_id, kernel_id, label, description, provider, model,
                system_prompt, consent_mode, created_at, created_by
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    blob_param(row.preset_id.as_bytes()),
                    blob_param(row.kernel_id.as_bytes()),
                    row.label,
                    row.description,
                    row.provider,
                    row.model,
                    row.system_prompt,
                    row.consent_mode.as_str(),
                    row.created_at,
                    blob_param(row.created_by.as_bytes()),
                ],
            )
            .map_err(|e| {
                map_unique_violation(e, format!("preset label '{}' already in use", row.label))
            })?;
        Ok(())
    }

    /// Get a preset by ID.
    pub fn get_preset(&self, id: PresetId) -> KernelDbResult<Option<PresetRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT preset_id, kernel_id, label, description, provider, model,
                    system_prompt, consent_mode, created_at, created_by
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
                    system_prompt, consent_mode, created_at, created_by
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
                    system_prompt, consent_mode, created_at, created_by
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

        let updated = self
            .conn
            .execute(
                "UPDATE presets SET
                label = ?1, description = ?2, provider = ?3, model = ?4,
                system_prompt = ?5, consent_mode = ?6
             WHERE preset_id = ?7",
                params![
                    row.label,
                    row.description,
                    row.provider,
                    row.model,
                    row.system_prompt,
                    row.consent_mode.as_str(),
                    blob_param(row.preset_id.as_bytes()),
                ],
            )
            .map_err(|e| {
                map_unique_violation(e, format!("preset label '{}' already in use", row.label))
            })?;

        if updated == 0 {
            return Err(KernelDbError::NotFound(format!("preset {}", row.preset_id)));
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

        self.conn
            .execute(
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
            )
            .map_err(|e| {
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
        let now = now_millis();
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
        self.conn
            .execute(
                "INSERT INTO workspace_paths (workspace_id, path, read_only, created_at)
             VALUES (?1, ?2, ?3, ?4)",
                params![
                    blob_param(row.workspace_id.as_bytes()),
                    row.path,
                    row.read_only as i64,
                    row.created_at,
                ],
            )
            .map_err(|e| {
                map_unique_violation(e, format!("workspace path '{}' already exists", row.path))
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

        let rows = stmt.query_map(params![blob_param(workspace_id.as_bytes())], |row| {
            let ro: i64 = row.get(2)?;
            Ok(WorkspacePathRow {
                workspace_id: read_workspace_id(row, 0)?,
                path: row.get(1)?,
                read_only: ro != 0,
                created_at: row.get(3)?,
            })
        })?;
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
        let mut rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
            Ok(ContextShellRow {
                context_id: read_context_id(row, 0)?,
                cwd: row.get(1)?,
                init_script: row.get(2)?,
                updated_at: row.get(3)?,
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
            init_script: src.init_script,
            updated_at: now_millis(),
        };
        self.upsert_context_shell(&row)?;
        Ok(true)
    }

    // ========================================================================
    // Context Tool Bindings (Phase 5, D-54)
    // ========================================================================

    /// Upsert a full `ContextToolBinding` for `context_id`.
    ///
    /// Transactional: bumps the parent row's `updated_at`, then wholesale
    /// replaces the child `context_binding_instances` / `context_binding_names`
    /// rows. Callers treat the binding as the unit of write. This matches the
    /// in-memory model (`Broker::set_binding` is also wholesale) and keeps
    /// restoration trivial (read parent → read children → fold).
    pub fn upsert_context_binding(
        &mut self,
        context_id: ContextId,
        binding: &ContextToolBinding,
    ) -> KernelDbResult<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO context_bindings (context_id, updated_at)
             VALUES (?1, ?2)
             ON CONFLICT(context_id) DO UPDATE SET updated_at = excluded.updated_at",
            params![blob_param(context_id.as_bytes()), now_millis()],
        )?;
        tx.execute(
            "DELETE FROM context_binding_instances WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
        )?;
        tx.execute(
            "DELETE FROM context_binding_names WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
        )?;
        for (idx, instance) in binding.allowed_instances.iter().enumerate() {
            tx.execute(
                "INSERT INTO context_binding_instances (context_id, instance_id, order_idx)
                 VALUES (?1, ?2, ?3)",
                params![
                    blob_param(context_id.as_bytes()),
                    instance.as_str(),
                    idx as i64,
                ],
            )?;
        }
        for (visible_name, (instance, tool)) in &binding.name_map {
            tx.execute(
                "INSERT INTO context_binding_names
                     (context_id, visible_name, instance_id, original_tool)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    blob_param(context_id.as_bytes()),
                    visible_name,
                    instance.as_str(),
                    tool,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Load a `ContextToolBinding` for `context_id`, or `None` if the context
    /// has never been bound. Callers fall back to "bind all registered"
    /// (first-touch auto-populate) when this returns `None`.
    pub fn get_context_binding(
        &self,
        context_id: ContextId,
    ) -> KernelDbResult<Option<ContextToolBinding>> {
        let exists: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM context_bindings WHERE context_id = ?1",
                params![blob_param(context_id.as_bytes())],
                |row| row.get(0),
            )
            .ok();
        if exists.is_none() {
            return Ok(None);
        }

        let mut allowed_instances: Vec<InstanceId> = {
            let mut stmt = self.conn.prepare(
                "SELECT instance_id FROM context_binding_instances
                 WHERE context_id = ?1
                 ORDER BY order_idx ASC",
            )?;
            let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
                let s: String = row.get(0)?;
                Ok(InstanceId::new(s))
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            out
        };
        allowed_instances.shrink_to_fit();

        let mut name_map = std::collections::HashMap::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT visible_name, instance_id, original_tool
                 FROM context_binding_names
                 WHERE context_id = ?1",
            )?;
            let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
                let visible: String = row.get(0)?;
                let inst: String = row.get(1)?;
                let tool: String = row.get(2)?;
                Ok((visible, (InstanceId::new(inst), tool)))
            })?;
            for r in rows {
                let (visible, pair) = r?;
                name_map.insert(visible, pair);
            }
        }

        Ok(Some(ContextToolBinding {
            allowed_instances,
            name_map,
        }))
    }

    /// Delete the full binding for a context (cascades to instances + names).
    /// Returns true if a parent row existed.
    pub fn delete_context_binding(&self, context_id: ContextId) -> KernelDbResult<bool> {
        let rows = self.conn.execute(
            "DELETE FROM context_bindings WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
        )?;
        Ok(rows > 0)
    }

    // ========================================================================
    // Hook Persistence
    // ========================================================================

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
                action_builtin_name, action_kaish_script_id,
                action_result_text, action_is_error,
                action_deny_reason,
                action_log_target, action_log_level
             ) VALUES (
                ?1, ?2, ?3,
                (SELECT COALESCE(MAX(insertion_idx), -1) + 1 FROM hooks WHERE phase = ?2),
                ?4, ?5, ?6, ?7,
                ?8,
                ?9, ?10,
                ?11, ?12,
                ?13,
                ?14, ?15
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
                row.action_kaish_script_id,
                row.action_result_text,
                is_error,
                row.action_deny_reason,
                row.action_log_target,
                row.action_log_level,
            ],
        )?;
        Ok(())
    }

    /// Delete a hook by id. Returns true if a row existed.
    pub fn delete_hook(&self, hook_id: &str) -> KernelDbResult<bool> {
        let rows = self
            .conn
            .execute("DELETE FROM hooks WHERE hook_id = ?1", params![hook_id])?;
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
                    action_builtin_name, action_kaish_script_id,
                    action_result_text, action_is_error,
                    action_deny_reason,
                    action_log_target, action_log_level
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
            let is_error_int: Option<i64> = row.get(11)?;
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
                action_kaish_script_id: row.get(9)?,
                action_result_text: row.get(10)?,
                action_is_error,
                action_deny_reason: row.get(12)?,
                action_log_target: row.get(13)?,
                action_log_level: row.get(14)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
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

    // ========================================================================
    // Context Config Fork + Workspace Query
    // ========================================================================

    /// Copy shell config + env vars from source context to target.
    /// Called during all fork operations.
    pub fn fork_context_config(&self, source: ContextId, target: ContextId) -> KernelDbResult<()> {
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
        let ctx = self
            .get_context(context_id)?
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
    // Workspace Permission Checking
    // ========================================================================

    /// Check whether a path is allowed by a context's workspace.
    ///
    /// Returns `None` if the context has no workspace (unbound = kernel perimeter
    /// defaults, no restriction). Returns `Some(read_only)` if the path falls
    /// under a workspace path. Returns an error if the path is outside all
    /// workspace paths (bound context, path not in scope).
    pub fn check_workspace_path(
        &self,
        context_id: ContextId,
        path: &str,
    ) -> KernelDbResult<Option<bool>> {
        let ws_paths = match self.context_workspace_paths(context_id)? {
            None => return Ok(None), // unbound context — no workspace restriction
            Some(paths) => paths,
        };

        // Find longest-prefix match among workspace paths
        let mut best: Option<&WorkspacePathRow> = None;
        for wp in &ws_paths {
            if path == wp.path || path.starts_with(&format!("{}/", wp.path)) {
                match best {
                    None => best = Some(wp),
                    Some(prev) if wp.path.len() > prev.path.len() => best = Some(wp),
                    _ => {}
                }
            }
        }

        match best {
            Some(wp) => Ok(Some(wp.read_only)),
            None => Err(KernelDbError::Validation(format!(
                "path '{}' is outside workspace scope",
                path,
            ))),
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
            params![
                blob_param(kernel_id.as_bytes()),
                blob_param(preset_id.as_bytes())
            ],
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
            params![
                blob_param(kernel_id.as_bytes()),
                blob_param(workspace_id.as_bytes())
            ],
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
                    system_prompt, consent_mode, context_state,
                    created_at, created_by, forked_from, fork_kind,
                    archived_at, workspace_id, preset_id
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

fn row_to_document_row(row: &rusqlite::Row<'_>) -> SqliteResult<DocumentRow> {
    let kind_str: String = row.get(3)?;
    Ok(DocumentRow {
        document_id: read_context_id(row, 0)?,
        kernel_id: read_kernel_id(row, 1)?,
        workspace_id: read_workspace_id(row, 2)?,
        doc_kind: doc_kind_from_sql(&kind_str),
        language: row.get(4)?,
        path: row.get(5)?,
        created_at: row.get(6)?,
        created_by: read_principal_id(row, 7)?,
    })
}

fn row_to_context_row(row: &rusqlite::Row<'_>) -> SqliteResult<ContextRow> {
    let consent_str: String = row.get(6)?;
    let state_str: String = row.get(7)?;
    let fork_kind_str: Option<String> = row.get(11)?;

    Ok(ContextRow {
        context_id: read_context_id(row, 0)?,
        kernel_id: read_kernel_id(row, 1)?,
        label: row.get(2)?,
        provider: row.get(3)?,
        model: row.get(4)?,
        system_prompt: row.get(5)?,
        consent_mode: consent_mode_from_sql(&consent_str),
        context_state: context_state_from_sql(&state_str),
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
    let consent_str: String = row.get(7)?;

    Ok(PresetRow {
        preset_id: read_preset_id(row, 0)?,
        kernel_id: read_kernel_id(row, 1)?,
        label: row.get(2)?,
        description: row.get(3)?,
        provider: row.get(4)?,
        model: row.get(5)?,
        system_prompt: row.get(6)?,
        consent_mode: consent_mode_from_sql(&consent_str),
        created_at: row.get(8)?,
        created_by: read_principal_id(row, 9)?,
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
        consent_mode: ConsentMode::default(),
        context_state: ContextState::Live,
        created_at: now_millis() as i64,
        created_by: PrincipalId::new(),
        forked_from: None,
        fork_kind: None,
        archived_at: None,
        workspace_id: None,
        preset_id: None,
    }
}

/// Insert both a document row and context row for a context.
/// Tests need this because contexts FK to documents.
#[cfg(test)]
fn insert_context_with_doc(db: &KernelDb, row: &ContextRow, ws_id: WorkspaceId) {
    db.insert_document(&DocumentRow {
        document_id: row.context_id,
        kernel_id: row.kernel_id,
        workspace_id: ws_id,
        doc_kind: DocKind::Conversation,
        language: None,
        path: None,
        created_at: row.created_at,
        created_by: row.created_by,
    })
    .unwrap();
    db.insert_context(row).unwrap();
}

/// Set up a test DB with kernel + default workspace. Returns (KernelId, WorkspaceId).
#[cfg(test)]
fn setup_test_db(db: &KernelDb) -> (KernelId, WorkspaceId) {
    let kid = KernelId::new();
    let creator = PrincipalId::system();
    let ws_id = db.get_or_create_default_workspace(kid, creator).unwrap();
    (kid, ws_id)
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
        let (kid, ws_id) = setup_test_db(&db);
        let row = make_context_row(kid, Some("main"));
        let cid = row.context_id;

        insert_context_with_doc(&db, &row, ws_id);

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
        let (kid, ws_id) = setup_test_db(&db);

        let row1 = make_context_row(kid, Some("shared"));
        insert_context_with_doc(&db, &row1, ws_id);

        // Same kernel, same label → conflict (doc insert ok, context label conflicts)
        let row2 = make_context_row(kid, Some("shared"));
        db.insert_document(&DocumentRow {
            document_id: row2.context_id,
            kernel_id: kid,
            workspace_id: ws_id,
            doc_kind: DocKind::Conversation,
            language: None,
            path: None,
            created_at: row2.created_at,
            created_by: row2.created_by,
        })
        .unwrap();
        let err = db.insert_context(&row2).unwrap_err();
        assert!(matches!(err, KernelDbError::LabelConflict(_)));

        // Different kernel, same label → OK (needs its own workspace)
        let kid2 = KernelId::new();
        let ws_id2 = db
            .get_or_create_default_workspace(kid2, PrincipalId::system())
            .unwrap();
        let row3 = make_context_row(kid2, Some("shared"));
        insert_context_with_doc(&db, &row3, ws_id2);

        // NULL + NULL → OK (multiple)
        let n1 = make_context_row(kid, None);
        let n2 = make_context_row(kid, None);
        insert_context_with_doc(&db, &n1, ws_id);
        insert_context_with_doc(&db, &n2, ws_id);
    }

    // ── 5. Fork lineage 3 deep ─────────────────────────────────────────

    #[test]
    fn fork_lineage_3_deep() {
        let db = KernelDb::in_memory().unwrap();
        let (kid, ws_id) = setup_test_db(&db);

        let root = make_context_row(kid, Some("root"));
        insert_context_with_doc(&db, &root, ws_id);

        let mut child = make_context_row(kid, Some("child"));
        child.forked_from = Some(root.context_id);
        child.fork_kind = Some(ForkKind::Full);
        insert_context_with_doc(&db, &child, ws_id);

        let mut grandchild = make_context_row(kid, Some("grandchild"));
        grandchild.forked_from = Some(child.context_id);
        grandchild.fork_kind = Some(ForkKind::Shallow);
        insert_context_with_doc(&db, &grandchild, ws_id);

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
        let (kid, ws_id) = setup_test_db(&db);

        let parent = make_context_row(kid, Some("template"));
        insert_context_with_doc(&db, &parent, ws_id);

        let c1 = make_context_row(kid, Some("child1"));
        insert_context_with_doc(&db, &c1, ws_id);
        db.insert_edge(&make_edge(
            parent.context_id,
            c1.context_id,
            EdgeKind::Structural,
        ))
        .unwrap();

        let c2 = make_context_row(kid, Some("child2"));
        insert_context_with_doc(&db, &c2, ws_id);
        db.insert_edge(&make_edge(
            parent.context_id,
            c2.context_id,
            EdgeKind::Structural,
        ))
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
        let (kid, ws_id) = setup_test_db(&db);

        let a = make_context_row(kid, Some("a"));
        let b = make_context_row(kid, Some("b"));
        insert_context_with_doc(&db, &a, ws_id);
        insert_context_with_doc(&db, &b, ws_id);

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
        let (kid, ws_id) = setup_test_db(&db);

        let a = make_context_row(kid, None);
        let b = make_context_row(kid, None);
        insert_context_with_doc(&db, &a, ws_id);
        insert_context_with_doc(&db, &b, ws_id);

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
        let (kid, ws_id) = setup_test_db(&db);

        let a = make_context_row(kid, Some("cyc-a"));
        let b = make_context_row(kid, Some("cyc-b"));
        insert_context_with_doc(&db, &a, ws_id);
        insert_context_with_doc(&db, &b, ws_id);

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
        insert_context_with_doc(&db, &c, ws_id);
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
        assert!(
            db.delete_workspace_path(ws.workspace_id, "/home/user/src/kaish")
                .unwrap()
        );
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
        insert_context_with_doc(&db, &ctx, ws.workspace_id);

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
        let (kid, ws_id) = setup_test_db(&db);

        let a = make_context_row(kid, Some("source"));
        let b = make_context_row(kid, Some("target"));
        insert_context_with_doc(&db, &a, ws_id);
        insert_context_with_doc(&db, &b, ws_id);

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
        let (kid, ws_id) = setup_test_db(&db);

        // Create a 5-node tree: root → [a, b], a → [c, d]
        let root = make_context_row(kid, Some("root"));
        let a = make_context_row(kid, Some("a"));
        let b = make_context_row(kid, Some("b"));
        let c = make_context_row(kid, Some("c"));
        let d = make_context_row(kid, Some("d"));

        for ctx in [&root, &a, &b, &c, &d] {
            insert_context_with_doc(&db, ctx, ws_id);
        }

        db.insert_edge(&make_edge(
            root.context_id,
            a.context_id,
            EdgeKind::Structural,
        ))
        .unwrap();
        db.insert_edge(&make_edge(
            root.context_id,
            b.context_id,
            EdgeKind::Structural,
        ))
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
        let (kid, ws_id) = setup_test_db(&db);

        let ctx1 = make_context_row(kid, Some("opusplan"));
        let ctx2 = make_context_row(kid, Some("sonnet"));
        insert_context_with_doc(&db, &ctx1, ws_id);
        insert_context_with_doc(&db, &ctx2, ws_id);

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
        let (kid, ws_id) = setup_test_db(&db);

        let ctx = make_context_row(kid, Some("unique-label"));
        insert_context_with_doc(&db, &ctx, ws_id);

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
        let (kid, ws_id) = setup_test_db(&db);

        // Insert 5 contexts with NULL label — all succeed
        for _ in 0..5 {
            let row = make_context_row(kid, None);
            insert_context_with_doc(&db, &row, ws_id);
        }

        let all = db.list_all_contexts(kid).unwrap();
        assert_eq!(all.len(), 5);
    }

    // ── 18. Archive excludes from active + structural_children ─────────

    #[test]
    fn archive_excludes_from_active() {
        let db = KernelDb::in_memory().unwrap();
        let (kid, ws_id) = setup_test_db(&db);

        let parent = make_context_row(kid, Some("parent"));
        let child = make_context_row(kid, Some("child"));
        insert_context_with_doc(&db, &parent, ws_id);
        insert_context_with_doc(&db, &child, ws_id);
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
        let (kid, ws_id) = setup_test_db(&db);

        let a = make_context_row(kid, Some("edge-a"));
        let b = make_context_row(kid, Some("edge-b"));
        insert_context_with_doc(&db, &a, ws_id);
        insert_context_with_doc(&db, &b, ws_id);

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
            consent_mode: ConsentMode::Autonomous,
            context_state: ContextState::Live,
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
        let (kid, ws_id) = setup_test_db(&db);
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
            consent_mode: ConsentMode::Collaborative,
            context_state: ContextState::Live,
            created_at: 1000,
            created_by: creator,
            forked_from: None,
            fork_kind: None,
            archived_at: None,
            workspace_id: None,
            preset_id: None,
        };
        insert_context_with_doc(&db, &parent, ws_id);

        // Insert child forked from parent
        let child_id = ContextId::new();
        let child = ContextRow {
            context_id: child_id,
            kernel_id: kid,
            label: Some("child-fork".into()),
            provider: Some("google".into()),
            model: Some("gemini-2.0-flash".into()),
            system_prompt: Some("Be concise.".into()),
            consent_mode: ConsentMode::Autonomous,
            context_state: ContextState::Live,
            created_at: 2000,
            created_by: creator,
            forked_from: Some(parent_id),
            fork_kind: Some(ForkKind::Full),
            archived_at: None,
            workspace_id: None,
            preset_id: None,
        };
        insert_context_with_doc(&db, &child, ws_id);

        // Read back and verify all 15 fields
        let recovered = db.get_context(child_id).unwrap().expect("child not found");
        assert_eq!(recovered.context_id, child_id);
        assert_eq!(recovered.kernel_id, kid);
        assert_eq!(recovered.label, Some("child-fork".into()));
        assert_eq!(recovered.provider, Some("google".into()));
        assert_eq!(recovered.model, Some("gemini-2.0-flash".into()));
        assert_eq!(recovered.system_prompt, Some("Be concise.".into()));
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

        // Verify update_model roundtrip
        db.update_model(child_id, Some("deepseek"), Some("deepseek-r1"))
            .unwrap();
        let updated = db.get_context(child_id).unwrap().unwrap();
        assert_eq!(updated.provider, Some("deepseek".into()));
        assert_eq!(updated.model, Some("deepseek-r1".into()));
    }

    // ── 22. FK violation produces Validation, not LabelConflict ──────────

    #[test]
    fn fk_violation_is_validation_error() {
        let db = KernelDb::in_memory().unwrap();
        let (kid, ws_id) = setup_test_db(&db);

        // Reference a workspace_id that doesn't exist on the context
        let ctx_id = ContextId::new();
        // Insert document with valid workspace first
        db.insert_document(&DocumentRow {
            document_id: ctx_id,
            kernel_id: kid,
            workspace_id: ws_id,
            doc_kind: DocKind::Conversation,
            language: None,
            path: None,
            created_at: now_millis() as i64,
            created_by: PrincipalId::new(),
        })
        .unwrap();

        let row = ContextRow {
            context_id: ctx_id,
            kernel_id: kid,
            label: Some("fk-test".into()),
            provider: None,
            model: None,
            system_prompt: None,
            consent_mode: ConsentMode::default(),
            context_state: ContextState::Live,
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
        let (kid, ws_id) = setup_test_db(&db);
        let ctx = make_context_row(kid, Some("shell-test"));
        insert_context_with_doc(&db, &ctx, ws_id);

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
        let (kid, ws_id) = setup_test_db(&db);
        let src = make_context_row(kid, Some("src"));
        let tgt = make_context_row(kid, Some("tgt"));
        insert_context_with_doc(&db, &src, ws_id);
        insert_context_with_doc(&db, &tgt, ws_id);

        // Copy from context with shell config
        let row = ContextShellRow {
            context_id: src.context_id,
            cwd: Some("/home/user/project".into()),
            init_script: Some("alias ll='ls -la'".into()),
            updated_at: now_millis() as i64,
        };
        db.upsert_context_shell(&row).unwrap();

        assert!(
            db.copy_context_shell(src.context_id, tgt.context_id)
                .unwrap()
        );

        let copied = db.get_context_shell(tgt.context_id).unwrap().unwrap();
        assert_eq!(copied.cwd, Some("/home/user/project".into()));
        assert_eq!(copied.init_script, Some("alias ll='ls -la'".into()));
    }

    #[test]
    fn context_shell_copy_empty() {
        let db = KernelDb::in_memory().unwrap();
        let (kid, ws_id) = setup_test_db(&db);
        let src = make_context_row(kid, Some("src"));
        let tgt = make_context_row(kid, Some("tgt"));
        insert_context_with_doc(&db, &src, ws_id);
        insert_context_with_doc(&db, &tgt, ws_id);

        // Copy from context with no shell config → returns false
        assert!(
            !db.copy_context_shell(src.context_id, tgt.context_id)
                .unwrap()
        );
        assert!(db.get_context_shell(tgt.context_id).unwrap().is_none());
    }

    #[test]
    fn context_shell_cascade_delete() {
        let db = KernelDb::in_memory().unwrap();
        let (kid, ws_id) = setup_test_db(&db);
        let ctx = make_context_row(kid, Some("cascade"));
        insert_context_with_doc(&db, &ctx, ws_id);

        db.upsert_context_shell(&ContextShellRow {
            context_id: ctx.context_id,
            cwd: Some("/tmp".into()),
            init_script: None,
            updated_at: now_millis() as i64,
        })
        .unwrap();
        assert!(db.get_context_shell(ctx.context_id).unwrap().is_some());

        // Delete context → shell row should cascade
        db.delete_context(ctx.context_id).unwrap();
        assert!(db.get_context_shell(ctx.context_id).unwrap().is_none());
    }

    // ── 23b. Context tool bindings CRUD (Phase 5, D-54) ──────────────
    //
    // Normalized schema: parent `context_bindings` + `_instances` (ordered)
    // + `_names` (sticky map). The get path reconstructs `ContextToolBinding`
    // by joining; the upsert path writes all three tables transactionally.

    fn binding_with(
        instances: &[&str],
        names: &[(&str, &str, &str)], // (visible, instance, original)
    ) -> ContextToolBinding {
        let mut b = ContextToolBinding::new();
        b.allowed_instances = instances.iter().map(|s| InstanceId::new(*s)).collect();
        for (visible, inst, tool) in names {
            b.name_map
                .insert((*visible).into(), (InstanceId::new(*inst), (*tool).into()));
        }
        b
    }

    #[test]
    fn context_binding_roundtrip_preserves_order_and_sticky_names() {
        let mut db = KernelDb::in_memory().unwrap();
        let (kid, ws_id) = setup_test_db(&db);
        let ctx = make_context_row(kid, Some("binding-roundtrip"));
        insert_context_with_doc(&db, &ctx, ws_id);

        let original = binding_with(
            &["builtin.block", "builtin.file", "external.gpal"],
            &[
                ("read", "builtin.file", "file_read"),
                ("write", "builtin.file", "file_write"),
                ("consult", "external.gpal", "consult_gemini"),
            ],
        );
        db.upsert_context_binding(ctx.context_id, &original).unwrap();

        let loaded = db
            .get_context_binding(ctx.context_id)
            .unwrap()
            .expect("binding should exist after upsert");

        // Order of `allowed_instances` must survive the roundtrip (D-20:
        // order is the tiebreaker for Auto resolution).
        assert_eq!(loaded.allowed_instances.len(), 3);
        assert_eq!(loaded.allowed_instances[0].as_str(), "builtin.block");
        assert_eq!(loaded.allowed_instances[1].as_str(), "builtin.file");
        assert_eq!(loaded.allowed_instances[2].as_str(), "external.gpal");

        // Name map content must match exactly — sticky resolution is the
        // whole point of persisting name_map across restart.
        assert_eq!(loaded.name_map.len(), 3);
        assert_eq!(
            loaded.name_map.get("read").unwrap(),
            &(InstanceId::new("builtin.file"), "file_read".into())
        );
        assert_eq!(
            loaded.name_map.get("consult").unwrap(),
            &(InstanceId::new("external.gpal"), "consult_gemini".into())
        );
    }

    #[test]
    fn context_binding_get_absent_returns_none() {
        let db = KernelDb::in_memory().unwrap();
        // No context, no upsert: get must return None so the broker can
        // fall back to "bind all registered" on first touch.
        assert!(db.get_context_binding(ContextId::new()).unwrap().is_none());
    }

    #[test]
    fn context_binding_upsert_replaces_wholesale() {
        // Phase 5 writes bindings as whole units; a second upsert must
        // wholly replace the children, not accumulate. Regression guard
        // against "leftover rows from a previous binding leak through."
        let mut db = KernelDb::in_memory().unwrap();
        let (kid, ws_id) = setup_test_db(&db);
        let ctx = make_context_row(kid, Some("binding-replace"));
        insert_context_with_doc(&db, &ctx, ws_id);

        let first = binding_with(
            &["builtin.block", "builtin.file"],
            &[("read", "builtin.file", "file_read")],
        );
        db.upsert_context_binding(ctx.context_id, &first).unwrap();

        let second = binding_with(
            &["external.gpal"],
            &[("consult", "external.gpal", "consult_gemini")],
        );
        db.upsert_context_binding(ctx.context_id, &second).unwrap();

        let loaded = db.get_context_binding(ctx.context_id).unwrap().unwrap();
        assert_eq!(loaded.allowed_instances.len(), 1, "stale instances leaked");
        assert_eq!(loaded.allowed_instances[0].as_str(), "external.gpal");
        assert_eq!(loaded.name_map.len(), 1, "stale name_map entries leaked");
        assert!(loaded.name_map.contains_key("consult"));
        assert!(!loaded.name_map.contains_key("read"));
    }

    #[test]
    fn context_binding_delete_returns_whether_existed() {
        let mut db = KernelDb::in_memory().unwrap();
        let (kid, ws_id) = setup_test_db(&db);
        let ctx = make_context_row(kid, Some("binding-delete"));
        insert_context_with_doc(&db, &ctx, ws_id);

        // Delete of absent row returns false.
        assert!(!db.delete_context_binding(ctx.context_id).unwrap());

        db.upsert_context_binding(
            ctx.context_id,
            &binding_with(&["builtin.file"], &[]),
        )
        .unwrap();

        // Delete of present row returns true and clears children via CASCADE.
        assert!(db.delete_context_binding(ctx.context_id).unwrap());
        assert!(db.get_context_binding(ctx.context_id).unwrap().is_none());
    }

    #[test]
    fn context_binding_cascades_on_context_delete() {
        // Parent context gone → all three binding tables cascade-clear.
        // Guards against orphaned rows in binding_instances / binding_names.
        let mut db = KernelDb::in_memory().unwrap();
        let (kid, ws_id) = setup_test_db(&db);
        let ctx = make_context_row(kid, Some("binding-cascade"));
        insert_context_with_doc(&db, &ctx, ws_id);

        db.upsert_context_binding(
            ctx.context_id,
            &binding_with(
                &["builtin.file", "builtin.block"],
                &[("read", "builtin.file", "file_read")],
            ),
        )
        .unwrap();
        assert!(db.get_context_binding(ctx.context_id).unwrap().is_some());

        db.delete_context(ctx.context_id).unwrap();
        assert!(db.get_context_binding(ctx.context_id).unwrap().is_none());

        // Children rows must also be gone (belt-and-suspenders on the
        // CASCADE — a future FK misstep would be caught here).
        let inst_rows: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM context_binding_instances WHERE context_id = ?1",
                params![blob_param(ctx.context_id.as_bytes())],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(inst_rows, 0);
        let name_rows: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM context_binding_names WHERE context_id = ?1",
                params![blob_param(ctx.context_id.as_bytes())],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(name_rows, 0);
    }

    // ── 23b. Hook persistence (hook persistence follow-up) ────────────
    //
    // Hooks are global (not per-context), so these tests don't need a
    // context row at all. Insert writes one row per entry with the DB
    // computing `insertion_idx` internally; load returns rows ordered
    // by `(phase, priority, insertion_idx)` so the broker can rebuild
    // `HookTable.entries` in the same Vec order as before restart.

    fn minimal_hook_row(id: &str, phase: &str, priority: i32) -> HookRow {
        HookRow {
            hook_id: id.into(),
            phase: phase.into(),
            priority,
            match_instance: None,
            match_tool: None,
            match_context: None,
            match_principal: None,
            action_kind: "log".into(),
            action_builtin_name: None,
            action_kaish_script_id: None,
            action_result_text: None,
            action_is_error: None,
            action_deny_reason: None,
            action_log_target: Some("kaijutsu::hooks".into()),
            action_log_level: Some("info".into()),
        }
    }

    #[test]
    fn hook_insert_roundtrip_preserves_all_action_variants() {
        // One insert per action_kind. Round-trip must preserve every
        // variant-specific column and every match field so the broker's
        // reconstruction loses no information.
        let db = KernelDb::in_memory().unwrap();

        // Builtin-invoke with full match fields.
        let ctx_id = ContextId::new();
        let principal = PrincipalId::new();
        let builtin = HookRow {
            hook_id: "h-builtin".into(),
            phase: "pre_call".into(),
            priority: 10,
            match_instance: Some("builtin.*".into()),
            match_tool: Some("file_*".into()),
            match_context: Some(ctx_id),
            match_principal: Some(principal),
            action_kind: "builtin_invoke".into(),
            action_builtin_name: Some("tracing_audit".into()),
            action_kaish_script_id: None,
            action_result_text: None,
            action_is_error: None,
            action_deny_reason: None,
            action_log_target: None,
            action_log_level: None,
        };
        db.insert_hook(&builtin).unwrap();

        // ShortCircuit with is_error=true and no matches.
        let sc = HookRow {
            hook_id: "h-sc".into(),
            phase: "post_call".into(),
            priority: 0,
            match_instance: None,
            match_tool: None,
            match_context: None,
            match_principal: None,
            action_kind: "shortcircuit".into(),
            action_builtin_name: None,
            action_kaish_script_id: None,
            action_result_text: Some("synthetic".into()),
            action_is_error: Some(true),
            action_deny_reason: None,
            action_log_target: None,
            action_log_level: None,
        };
        db.insert_hook(&sc).unwrap();

        // Deny.
        let deny = HookRow {
            hook_id: "h-deny".into(),
            phase: "pre_call".into(),
            priority: -5,
            match_instance: Some("builtin.file".into()),
            match_tool: None,
            match_context: None,
            match_principal: None,
            action_kind: "deny".into(),
            action_builtin_name: None,
            action_kaish_script_id: None,
            action_result_text: None,
            action_is_error: None,
            action_deny_reason: Some("no writes".into()),
            action_log_target: None,
            action_log_level: None,
        };
        db.insert_hook(&deny).unwrap();

        // Log.
        let log = minimal_hook_row("h-log", "on_notification", 0);
        db.insert_hook(&log).unwrap();

        // Kaish (reserved; rejected at admin time, but the row shape
        // must still round-trip so a manually-inserted row hydrates
        // predictably and the skip-with-warn path works).
        let kaish = HookRow {
            hook_id: "h-kaish".into(),
            phase: "list_tools".into(),
            priority: 0,
            match_instance: None,
            match_tool: None,
            match_context: None,
            match_principal: None,
            action_kind: "kaish_invoke".into(),
            action_builtin_name: None,
            action_kaish_script_id: Some("script-42".into()),
            action_result_text: None,
            action_is_error: None,
            action_deny_reason: None,
            action_log_target: None,
            action_log_level: None,
        };
        db.insert_hook(&kaish).unwrap();

        let loaded = db.load_all_hooks().unwrap();
        assert_eq!(loaded.len(), 5);

        let by_id: std::collections::HashMap<String, HookRow> = loaded
            .into_iter()
            .map(|r| (r.hook_id.clone(), r))
            .collect();

        let b = by_id.get("h-builtin").unwrap();
        assert_eq!(b.phase, "pre_call");
        assert_eq!(b.priority, 10);
        assert_eq!(b.match_instance.as_deref(), Some("builtin.*"));
        assert_eq!(b.match_tool.as_deref(), Some("file_*"));
        assert_eq!(b.match_context, Some(ctx_id));
        assert_eq!(b.match_principal, Some(principal));
        assert_eq!(b.action_kind, "builtin_invoke");
        assert_eq!(b.action_builtin_name.as_deref(), Some("tracing_audit"));

        let s = by_id.get("h-sc").unwrap();
        assert_eq!(s.action_kind, "shortcircuit");
        assert_eq!(s.action_result_text.as_deref(), Some("synthetic"));
        assert_eq!(s.action_is_error, Some(true));

        let d = by_id.get("h-deny").unwrap();
        assert_eq!(d.action_kind, "deny");
        assert_eq!(d.action_deny_reason.as_deref(), Some("no writes"));
        assert_eq!(d.priority, -5);

        let l = by_id.get("h-log").unwrap();
        assert_eq!(l.action_kind, "log");
        assert_eq!(l.action_log_level.as_deref(), Some("info"));

        let k = by_id.get("h-kaish").unwrap();
        assert_eq!(k.action_kind, "kaish_invoke");
        assert_eq!(k.action_kaish_script_id.as_deref(), Some("script-42"));
    }

    #[test]
    fn hook_delete_returns_whether_existed() {
        let db = KernelDb::in_memory().unwrap();
        // Delete of absent id is false (idempotent cleanup).
        assert!(!db.delete_hook("nope").unwrap());

        db.insert_hook(&minimal_hook_row("keep", "pre_call", 0))
            .unwrap();
        assert!(db.delete_hook("keep").unwrap());
        // Second delete of same id is false.
        assert!(!db.delete_hook("keep").unwrap());
    }

    #[test]
    fn load_all_hooks_orders_by_phase_priority_then_insertion_idx() {
        // Tests the evaluation-law tiebreak (§4.3): priority ascending,
        // insertion-order tiebreak. Insert three hooks in pre_call with
        // identical priority; load must return them in insertion order.
        // Cross-phase, ordering within each phase must be preserved.
        let db = KernelDb::in_memory().unwrap();
        db.insert_hook(&minimal_hook_row("a", "pre_call", 5))
            .unwrap();
        db.insert_hook(&minimal_hook_row("b", "pre_call", 5))
            .unwrap();
        db.insert_hook(&minimal_hook_row("c", "pre_call", 1))
            .unwrap();
        db.insert_hook(&minimal_hook_row("x", "post_call", 0))
            .unwrap();
        db.insert_hook(&minimal_hook_row("y", "post_call", 0))
            .unwrap();

        let loaded = db.load_all_hooks().unwrap();
        let pre: Vec<&str> = loaded
            .iter()
            .filter(|r| r.phase == "pre_call")
            .map(|r| r.hook_id.as_str())
            .collect();
        // Priority 1 first, then priority 5 with insertion order (a, b).
        assert_eq!(pre, vec!["c", "a", "b"]);

        let post: Vec<&str> = loaded
            .iter()
            .filter(|r| r.phase == "post_call")
            .map(|r| r.hook_id.as_str())
            .collect();
        assert_eq!(post, vec!["x", "y"]);
    }

    #[test]
    fn hook_insert_same_id_errors() {
        // Primary-key collision is a load-bearing signal: the broker
        // should call `delete_hook` first for replace semantics rather
        // than relying on an implicit upsert. Lock that contract in.
        let db = KernelDb::in_memory().unwrap();
        db.insert_hook(&minimal_hook_row("dup", "pre_call", 0))
            .unwrap();
        let err = db
            .insert_hook(&minimal_hook_row("dup", "pre_call", 0))
            .unwrap_err();
        match err {
            KernelDbError::Db(_) => {}
            other => panic!("expected DB error on PK collision, got {other:?}"),
        }
    }

    // ── 24. Context env CRUD ──────────────────────────────────────────

    #[test]
    fn context_env_set_and_get() {
        let db = KernelDb::in_memory().unwrap();
        let (kid, ws_id) = setup_test_db(&db);
        let ctx = make_context_row(kid, Some("env-test"));
        insert_context_with_doc(&db, &ctx, ws_id);

        // Initially empty
        let vars = db.get_context_env(ctx.context_id).unwrap();
        assert!(vars.is_empty());

        // Set vars
        db.set_context_env(ctx.context_id, "RUST_LOG", "debug")
            .unwrap();
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
        let (kid, ws_id) = setup_test_db(&db);
        let ctx = make_context_row(kid, Some("env-upsert"));
        insert_context_with_doc(&db, &ctx, ws_id);

        db.set_context_env(ctx.context_id, "RUST_LOG", "debug")
            .unwrap();
        db.set_context_env(ctx.context_id, "RUST_LOG", "trace")
            .unwrap();

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
        let (kid, ws_id) = setup_test_db(&db);
        let ctx = make_context_row(kid, Some("env-del"));
        insert_context_with_doc(&db, &ctx, ws_id);

        db.set_context_env(ctx.context_id, "FOO", "bar").unwrap();
        assert!(db.delete_context_env(ctx.context_id, "FOO").unwrap());
        assert!(!db.delete_context_env(ctx.context_id, "FOO").unwrap()); // already gone
        assert!(!db.delete_context_env(ctx.context_id, "NEVER_SET").unwrap());
    }

    #[test]
    fn context_env_clear() {
        let db = KernelDb::in_memory().unwrap();
        let (kid, ws_id) = setup_test_db(&db);
        let ctx = make_context_row(kid, Some("env-clear"));
        insert_context_with_doc(&db, &ctx, ws_id);

        db.set_context_env(ctx.context_id, "A", "1").unwrap();
        db.set_context_env(ctx.context_id, "B", "2").unwrap();
        db.set_context_env(ctx.context_id, "C", "3").unwrap();

        assert_eq!(db.clear_context_env(ctx.context_id).unwrap(), 3);
        assert!(db.get_context_env(ctx.context_id).unwrap().is_empty());
    }

    #[test]
    fn context_env_copy() {
        let db = KernelDb::in_memory().unwrap();
        let (kid, ws_id) = setup_test_db(&db);
        let src = make_context_row(kid, Some("env-src"));
        let tgt = make_context_row(kid, Some("env-tgt"));
        insert_context_with_doc(&db, &src, ws_id);
        insert_context_with_doc(&db, &tgt, ws_id);

        db.set_context_env(src.context_id, "RUST_LOG", "debug")
            .unwrap();
        db.set_context_env(src.context_id, "EDITOR", "vim").unwrap();
        db.set_context_env(src.context_id, "SHELL", "/bin/bash")
            .unwrap();

        let count = db.copy_context_env(src.context_id, tgt.context_id).unwrap();
        assert_eq!(count, 3);

        let vars = db.get_context_env(tgt.context_id).unwrap();
        assert_eq!(vars.len(), 3);
    }

    #[test]
    fn context_env_cascade_delete() {
        let db = KernelDb::in_memory().unwrap();
        let (kid, ws_id) = setup_test_db(&db);
        let ctx = make_context_row(kid, Some("env-cascade"));
        insert_context_with_doc(&db, &ctx, ws_id);

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
        let (kid, ws_id) = setup_test_db(&db);
        let src = make_context_row(kid, Some("fork-src"));
        let tgt = make_context_row(kid, Some("fork-tgt"));
        insert_context_with_doc(&db, &src, ws_id);
        insert_context_with_doc(&db, &tgt, ws_id);

        // Set up source with shell config + env vars
        db.upsert_context_shell(&ContextShellRow {
            context_id: src.context_id,
            cwd: Some("/home/user/src/kaijutsu".into()),
            init_script: None,
            updated_at: now_millis() as i64,
        })
        .unwrap();
        db.set_context_env(src.context_id, "RUST_LOG", "debug")
            .unwrap();
        db.set_context_env(src.context_id, "EDITOR", "vim").unwrap();
        db.set_context_env(src.context_id, "TERM", "xterm-256color")
            .unwrap();

        db.fork_context_config(src.context_id, tgt.context_id)
            .unwrap();

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
        let (kid, ws_id) = setup_test_db(&db);
        let src = make_context_row(kid, Some("empty-src"));
        let tgt = make_context_row(kid, Some("empty-tgt"));
        insert_context_with_doc(&db, &src, ws_id);
        insert_context_with_doc(&db, &tgt, ws_id);

        // Fork from context with no config → no error, no data on target
        db.fork_context_config(src.context_id, tgt.context_id)
            .unwrap();

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
        })
        .unwrap();
        db.insert_workspace_path(&WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: "/home/user/docs".into(),
            read_only: true,
            created_at: now,
        })
        .unwrap();

        // Create context with workspace bound
        let mut ctx = make_context_row(kid, Some("bound"));
        ctx.workspace_id = Some(ws.workspace_id);
        insert_context_with_doc(&db, &ctx, ws.workspace_id);

        let paths = db.context_workspace_paths(ctx.context_id).unwrap();
        let paths = paths.unwrap();
        assert_eq!(paths.len(), 2);
    }

    #[test]
    fn context_workspace_paths_unbound() {
        let db = KernelDb::in_memory().unwrap();
        let (kid, ws_id) = setup_test_db(&db);
        let ctx = make_context_row(kid, Some("unbound"));
        insert_context_with_doc(&db, &ctx, ws_id);

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
        let ws_id = db
            .get_or_create_default_workspace(old_kid, PrincipalId::system())
            .unwrap();
        let ctx = make_context_row(old_kid, Some("legacy"));
        insert_context_with_doc(&db, &ctx, ws_id);

        // kernel table is empty — should adopt the context's kernel_id
        let id = db.get_or_create_kernel_id().unwrap();
        assert_eq!(id, old_kid, "should adopt kernel_id from existing contexts");

        // Subsequent call returns the same
        let id2 = db.get_or_create_kernel_id().unwrap();
        assert_eq!(id2, old_kid);
    }

    // ── 28. Workspace path permission checking ────────────────────────

    #[test]
    fn check_workspace_path_unbound_context() {
        let db = KernelDb::in_memory().unwrap();
        let (kid, ws_id) = setup_test_db(&db);
        let ctx = make_context_row(kid, Some("unbound"));
        insert_context_with_doc(&db, &ctx, ws_id);

        // Unbound context → None (no restriction)
        let result = db
            .check_workspace_path(ctx.context_id, "/anywhere")
            .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn check_workspace_path_allowed_rw() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let creator = PrincipalId::new();
        let now = now_millis() as i64;

        let ws = WorkspaceRow {
            workspace_id: WorkspaceId::new(),
            kernel_id: kid,
            label: "proj".into(),
            description: None,
            created_at: now,
            created_by: creator,
            archived_at: None,
        };
        db.insert_workspace(&ws).unwrap();
        db.insert_workspace_path(&WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: "/home/user/src/kaijutsu".into(),
            read_only: false,
            created_at: now,
        })
        .unwrap();

        let mut ctx = make_context_row(kid, Some("bound"));
        ctx.workspace_id = Some(ws.workspace_id);
        insert_context_with_doc(&db, &ctx, ws.workspace_id);

        // Path inside workspace rw path → Some(false)
        let result = db
            .check_workspace_path(ctx.context_id, "/home/user/src/kaijutsu/src/main.rs")
            .unwrap();
        assert_eq!(result, Some(false));
    }

    #[test]
    fn check_workspace_path_allowed_ro() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let creator = PrincipalId::new();
        let now = now_millis() as i64;

        let ws = WorkspaceRow {
            workspace_id: WorkspaceId::new(),
            kernel_id: kid,
            label: "proj".into(),
            description: None,
            created_at: now,
            created_by: creator,
            archived_at: None,
        };
        db.insert_workspace(&ws).unwrap();
        db.insert_workspace_path(&WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: "/home/user/docs".into(),
            read_only: true,
            created_at: now,
        })
        .unwrap();

        let mut ctx = make_context_row(kid, Some("bound"));
        ctx.workspace_id = Some(ws.workspace_id);
        insert_context_with_doc(&db, &ctx, ws.workspace_id);

        // Path inside workspace ro path → Some(true)
        let result = db
            .check_workspace_path(ctx.context_id, "/home/user/docs/README.md")
            .unwrap();
        assert_eq!(result, Some(true));
    }

    #[test]
    fn check_workspace_path_outside_scope() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let creator = PrincipalId::new();
        let now = now_millis() as i64;

        let ws = WorkspaceRow {
            workspace_id: WorkspaceId::new(),
            kernel_id: kid,
            label: "proj".into(),
            description: None,
            created_at: now,
            created_by: creator,
            archived_at: None,
        };
        db.insert_workspace(&ws).unwrap();
        db.insert_workspace_path(&WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: "/home/user/src/kaijutsu".into(),
            read_only: false,
            created_at: now,
        })
        .unwrap();

        let mut ctx = make_context_row(kid, Some("bound"));
        ctx.workspace_id = Some(ws.workspace_id);
        insert_context_with_doc(&db, &ctx, ws.workspace_id);

        // Path outside all workspace paths → Validation error
        let err = db
            .check_workspace_path(ctx.context_id, "/etc/passwd")
            .unwrap_err();
        assert!(matches!(err, KernelDbError::Validation(_)));
    }

    #[test]
    fn check_workspace_path_longest_prefix() {
        let db = KernelDb::in_memory().unwrap();
        let kid = KernelId::new();
        let creator = PrincipalId::new();
        let now = now_millis() as i64;

        let ws = WorkspaceRow {
            workspace_id: WorkspaceId::new(),
            kernel_id: kid,
            label: "proj".into(),
            description: None,
            created_at: now,
            created_by: creator,
            archived_at: None,
        };
        db.insert_workspace(&ws).unwrap();
        db.insert_workspace_path(&WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: "/home/user/src".into(),
            read_only: false,
            created_at: now,
        })
        .unwrap();
        db.insert_workspace_path(&WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: "/home/user/src/kaijutsu/docs".into(),
            read_only: true,
            created_at: now,
        })
        .unwrap();

        let mut ctx = make_context_row(kid, Some("bound"));
        ctx.workspace_id = Some(ws.workspace_id);
        insert_context_with_doc(&db, &ctx, ws.workspace_id);

        // /home/user/src/kaijutsu/src → matches /home/user/src (rw)
        assert_eq!(
            db.check_workspace_path(ctx.context_id, "/home/user/src/kaijutsu/src/main.rs")
                .unwrap(),
            Some(false),
        );

        // /home/user/src/kaijutsu/docs/README.md → matches /home/user/src/kaijutsu/docs (ro, longer prefix)
        assert_eq!(
            db.check_workspace_path(ctx.context_id, "/home/user/src/kaijutsu/docs/README.md")
                .unwrap(),
            Some(true),
        );
    }
}
