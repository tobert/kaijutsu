//! SQLite persistence for kernel context metadata, edges, presets, and workspaces.
//!
//! Pattern follows `auth_db.rs` and `db.rs`: single `Connection`, WAL mode,
//! BLOB-encoded typed IDs, in-memory constructor for tests.
//!
//! All timestamps are Unix milliseconds (matching `now_millis()`).
//!
//! ## Known shape, deferred on purpose
//!
//! This file is large (~20 tables, many methods) and the live handle is a single
//! `Arc<Mutex<KernelDb>>`, so every write serializes on one lock. That is a
//! recognized "god-table + single-mutex" smell — and we are **deliberately not
//! splitting it yet**. The pressure that would justify the churn (measured
//! write-contention under concurrent contexts) is not expected any time soon;
//! revisit only when it's an actual, observed problem. See
//! `docs/architecture/kernel.md` for the connection-pool angle. Don't
//! pre-emptively refactor.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;

use rusqlite::{Connection, OpenFlags, OptionalExtension, Result as SqliteResult, params};
use tracing::{info, warn};

use kaijutsu_types::{
    BackendId, BlockId, CastId, ContextId, ContextState, DocKind, ForkKind,
    KernelId, PresetId, PrincipalId, TurnId, WorkspaceId,
};

use crate::llm::stream::{CacheTarget, CacheTtl};
use crate::mcp::binding::ContextToolBinding;
use crate::mcp::types::InstanceId;
use crate::roster::{Availability, LivenessKind, PresenceSnapshotRow, RosterEntity, RosterRow};

/// SQL for the performer of the `contexts` row aliased `$t`: the principal of
/// its context character, `NULL` for a context with none.
macro_rules! performer_sql {
    ($t:literal) => {
        concat!("(SELECT cc.principal_id FROM context_characters cc WHERE cc.context_id = ", $t, ".context_id)")
    };
}

mod context_characters;
mod model_prices;
#[cfg(test)]
mod test_support;
mod kernel_meta;
mod presets;
mod hooks;
mod client_state;
mod well_known;
mod workspaces;
mod roster;
mod usage;
mod context_bindings;
mod context_env;
mod cache_hydration;
mod tracks;
mod documents;
mod lineage;
mod llm_backends;
mod llm_characters;
mod approvals;
mod review;
mod contexts;
mod context_ring;
#[cfg(any(test, feature = "test-util"))]
mod fixtures;
mod context_fork;
mod schema;
mod migrate_senpai;
mod migrate_cast;
mod migrate;
#[cfg(test)]
use test_support::*;
pub use model_prices::{CostGroup, ModelPriceRow, NewModelPrice};
pub use context_characters::{ContextCharacterMigrationNote, ContextCharacterRow, ContextSlotRow};
pub use kernel_meta::QuiesceState;
pub use presets::{PresetRow, PresetArg};
pub use hooks::{HookRow, HookScriptRow};
pub use client_state::DirtyFileBuffer;
pub use well_known::WellKnownRole;
pub use workspaces::{WorkspaceRow, WorkspacePathRow};
pub use usage::{ContextUsageRow, TurnStart, RawUsage, TurnRow, TurnTotals, context_used_pct};
pub use context_env::{ContextShellRow, ContextEnvRow};
pub use cache_hydration::DEFAULT_HYDRATION_WINDOW;
pub use tracks::{PersistedTrack, PersistedAttachment};
pub use documents::{DocumentRow, DocSnapshotRow};
pub use lineage::ContextEdgeRow;
pub use llm_backends::{BackendRow, BackendModelRow, ModelTunablesRow, ModelAliasRow, EmbeddingConfigRow};
pub use llm_characters::{CharacterRow, CharacterSlotRow, CharacterModelRow};
pub use approvals::ContinuationOrigin;
pub(crate) use approvals::{HeldAskEnd, record_approval_tool_call, record_approval_drift_block};
pub use review::{ReviewFrom, GovernedAct, Authority, ReviewQuery, EffectiveReviewer};
pub use contexts::ContextRow;
pub use context_ring::{ACTIVE_RING_CAPACITY, DemoteOutcome, PromoteOutcome};
use schema::SCHEMA;

// ============================================================================
// Error type
// ============================================================================

/// Errors from KernelDb operations.
#[derive(Debug, thiserror::Error)]
pub enum KernelDbError {
    /// Underlying SQLite error.
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    /// An `approval-ledger` operation failed. The ledger is DB-injected into
    /// this same connection, so its errors surface during kernel-DB work —
    /// but they are not bare SQLite faults, and flattening them into [`Self::Db`]
    /// would throw away which ledger invariant was actually violated.
    #[error("approval ledger error: {0}")]
    Ledger(#[from] approval_ledger::error::LedgerError),

    /// Entity not found.
    #[error("not found: {0}")]
    NotFound(String),

    /// The senpai climb reached a context with no senpai and nobody above it
    /// to land on. Distinct from a fault: an escalation refuses on it, and the
    /// reviewer-context chain ends on it.
    #[error("{0}")]
    NoReviewerAbove(String),

    /// Stored state contradicts an invariant, such as a senpai cycle or a
    /// character with no sheet. A fault to report, never a refusal the caller
    /// can repair by changing its request.
    #[error("{0}")]
    Corrupt(String),

    /// Label already in use for this kernel.
    #[error("label conflict: {0}")]
    LabelConflict(String),

    /// Label contains forbidden characters.
    #[error("invalid label: {0}")]
    InvalidLabel(String),

    /// A `context_env` key is not a legal shell identifier — refused at the
    /// durable write so a bad key is never discovered only when a shell
    /// later tries to materialize it.
    #[error("invalid env key: {0}")]
    InvalidEnvKey(String),

    /// A `context_egress` host is not a bare DNS name, a loopback literal,
    /// or `*` — refused at the durable write, `docs/egress.md`, "The rule".
    #[error("invalid egress host: {0}")]
    InvalidEgressHost(String),

    /// General validation failure.
    #[error("validation error: {0}")]
    Validation(String),

    /// A document with this `document_id` already exists (PRIMARY KEY
    /// conflict on `documents.document_id`) — read back from the DB, never
    /// inferred from message text. See `classify_document_insert_error`.
    #[error("document already exists: {0}")]
    DuplicateDocument(ContextId),
}

pub type KernelDbResult<T> = Result<T, KernelDbError>;

// ============================================================================
// Row types
// ============================================================================

// ============================================================================
// Schema
// ============================================================================

// ============================================================================
// BLOB helpers
// ============================================================================

pub(crate) fn blob_param(id: &[u8; 16]) -> &[u8] {
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

fn read_opt_principal_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<Option<PrincipalId>> {
    let bytes: Option<Vec<u8>> = row.get(idx)?;
    match bytes {
        Some(b) => PrincipalId::try_from_slice(&b).map(Some).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                idx,
                rusqlite::types::Type::Blob,
                "invalid PrincipalId bytes".into(),
            )
        }),
        None => Ok(None),
    }
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

fn read_opt_cast_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<Option<CastId>> {
    let bytes: Option<Vec<u8>> = row.get(idx)?;
    match bytes {
        Some(b) => CastId::try_from_slice(&b).map(Some).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                idx,
                rusqlite::types::Type::Blob,
                "invalid CastId bytes".into(),
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

/// Parse ContextState from TEXT column.
fn context_state_from_sql(s: &str) -> ContextState {
    ContextState::from_str(s).unwrap_or_else(|_| {
        warn!(state = %s, "unknown ContextState in DB, defaulting to Live");
        ContextState::Live
    })
}

/// Parse ForkKind from a TEXT column. NULL → `None`. An unknown non-null value
/// is a HARD error — a corrupt or forward-incompatible row must crash, never
/// silently degrade to `None` (which would erase fork provenance). Crash over
/// corruption.
fn fork_kind_from_sql(s: Option<String>) -> SqliteResult<Option<ForkKind>> {
    match s {
        None => Ok(None),
        Some(v) => ForkKind::from_str(&v).map(Some).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                11,
                rusqlite::types::Type::Text,
                format!("unknown ForkKind '{v}'").into(),
            )
        }),
    }
}

/// Parse DocKind from TEXT column.
fn doc_kind_from_sql(s: &str) -> DocKind {
    DocKind::from_str(s).unwrap_or_else(|_| {
        warn!(kind = %s, "unknown DocKind in DB, defaulting to Conversation");
        DocKind::Conversation
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

/// Validate a durable export name before storing it. Contextual construction
/// and switching use the same kaish identifier rule: ASCII letter/underscore
/// first, then ASCII alphanumeric/underscore.
fn validate_env_key(key: &str) -> KernelDbResult<()> {
    let mut chars = key.chars();
    let starts_ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_');
    if !starts_ok || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(KernelDbError::InvalidEnvKey(format!(
            "env key {key:?} is not a valid shell identifier — must start with an ASCII \
             letter or underscore, and contain only ASCII letters, digits, or underscores"
        )));
    }
    Ok(())
}

/// Validate and normalize a `context_egress` host (`docs/egress.md`, "The
/// rule"): a bare DNS name, a loopback literal (`localhost`, `127.0.0.1`,
/// `::1`, or any IP address that parses as loopback), or exactly `*` for
/// every host. No scheme, path, port, whitespace, or wildcard other than
/// the single literal `*`. Lowercases the result, so storage and lookup
/// never disagree on case.
pub(crate) fn validate_egress_host(host: &str) -> KernelDbResult<String> {
    let bad = || {
        KernelDbError::InvalidEgressHost(format!(
            "egress host {host:?} is not valid — use a bare DNS name (example: \"crates.io\"), \
             a loopback literal (\"localhost\", \"127.0.0.1\", \"::1\"), or \"*\" for every host; \
             no scheme, path, port, whitespace, or wildcard other than \"*\""
        ))
    };
    if host.is_empty() || host.chars().any(|c| c.is_whitespace()) {
        return Err(bad());
    }
    if host == "*" {
        return Ok(host.to_string());
    }
    let lower = host.to_ascii_lowercase();
    // An IP literal is accepted only as a loopback address — any other
    // address is not a form this table understands today.
    if let Ok(ip) = lower.parse::<std::net::IpAddr>() {
        return if ip.is_loopback() { Ok(ip.to_string()) } else { Err(bad()) };
    }
    if lower == "localhost" {
        return Ok(lower);
    }
    // A bare DNS name: no scheme separator, no path, no port, and no
    // wildcard other than the literal `*` already handled above.
    if lower.contains("://") || lower.contains('/') || lower.contains(':') || lower.contains('*') {
        return Err(bad());
    }
    Ok(lower)
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

/// True when `e` is SQLite's "duplicate column name" error — the expected,
/// ignorable outcome of `apply_additive_migrations` re-running an `ALTER
/// TABLE ... ADD COLUMN` against a DB that already has the column (either a
/// fresh DB created from the current `SCHEMA`, or an already-migrated one).
/// SQLite has no dedicated extended code for this (it's a generic
/// `SQLITE_ERROR`, surfaced by rusqlite as `ErrorCode::Unknown`), so we match
/// on the message text — the same shape SQLite itself uses across versions.
fn is_duplicate_column_error(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(err, Some(msg))
            if err.code == rusqlite::ErrorCode::Unknown
                && msg.starts_with("duplicate column name")
    )
}

// ============================================================================
// KernelDb
// ============================================================================

/// SQLite database for kernel context metadata.
pub struct KernelDb {
    conn: Connection,
    /// Owns the throwaway directory a `temporary()` database was opened
    /// under; `None` for every `open()`. Held only for its `Drop` — removing
    /// the directory (and the `.db` file inside it) once nothing can reach
    /// this handle anymore.
    #[cfg(any(test, feature = "test-util"))]
    _temp_dir: Option<tempfile::TempDir>,
}

impl KernelDb {
    // ========================================================================
    // Quiesce
    // ========================================================================

    pub(crate) fn conn_for_ledger(&self) -> &Connection {
        &self.conn
    }

    /// The connection, mutably, for tests that install a SQLite authorizer
    /// to inject a read or write fault (`Connection::authorizer` takes
    /// `&mut self`).
    #[cfg(test)]
    pub(crate) fn conn_for_test_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Open or create at the given path.
    ///
    /// The DB schema (`SCHEMA`) is the source of truth for a new database;
    /// [`Self::apply_additive_migrations`] brings an older one up to it with
    /// guarded steps (additive columns, and the named migrations and drops
    /// that each guard on what is still there). A database still on casts is
    /// copied aside first ([`Self::back_up_before_character_slots`]). rc
    /// lifecycle scripts are no longer table rows — they live as files under
    /// `/config/rc` (see `seed_scripts` and `rc/mod.rs`), seeded to disk
    /// at server boot.
    pub fn open<P: AsRef<Path>>(path: P) -> KernelDbResult<Self> {
        if let Some(parent) = path.as_ref().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let path = path.as_ref();
        let conn = Connection::open(path)?;
        Self::init_connection(&conn)?;
        Self::back_up_before_character_slots(&conn, path)?;
        conn.execute_batch(SCHEMA)?;
        Self::apply_additive_migrations(&conn)?;
        Self::migrate_ledger(&conn)?;
        Self::ensure_singleton_kernel(&conn)?;
        Ok(Self {
            conn,
            #[cfg(any(test, feature = "test-util"))]
            _temp_dir: None,
        })
    }

    /// Open an existing `kernel.db` as a genuine read-only SQLite connection:
    /// no `SCHEMA` execution, no migration. `open()` always runs both, which
    /// needs write access even when nothing changes — unusable for a reader
    /// that must never contend with a live server's write connection.
    ///
    /// The lockout-recovery CLI (`kaijutsu-server list-characters`) and
    /// `add-key --as`'s name lookup: both read `kernel.db` while a server
    /// may already be holding it open, and both must work with the service
    /// stopped. Fails loudly if the file does not exist or is not a
    /// kaijutsu `kernel.db` — there is nothing sensible to fall back to.
    pub fn open_read_only<P: AsRef<Path>>(path: P) -> KernelDbResult<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(Self {
            conn,
            #[cfg(any(test, feature = "test-util"))]
            _temp_dir: None,
        })
    }

    /// Create a real, file-backed database under a fresh throwaway directory,
    /// owned by the returned handle: the directory (and the `.db` file inside
    /// it) is removed when this `KernelDb` drops.
    ///
    /// Never `:memory:`. A `:memory:` connection is per-connection state that
    /// never touches a file, so two `in_memory()` handles used to be two
    /// separate, unrelated databases even though the name suggested shared
    /// state — and none of it exercised WAL mode, file locking, or reopening
    /// across a restart, all of which durability work needs to hit. `open()`
    /// and `temporary()` run the identical setup sequence (`SCHEMA`,
    /// migrations); the only difference is a real file under a directory
    /// nobody but this handle knows about.
    ///
    /// Test-only: gated behind `cfg(test)` for this crate's own tests and
    /// `feature = "test-util"` for integration tests and other crates
    /// (`kaijutsu-server`) that opt in — never reachable from production code.
    #[cfg(any(test, feature = "test-util"))]
    pub fn temporary() -> KernelDbResult<Self> {
        let dir = tempfile::tempdir().expect("create temporary kernel db directory");
        let mut db = Self::open(dir.path().join("kernel.db"))?;
        db._temp_dir = Some(dir);
        Ok(db)
    }

    // ========================================================================
    // Kernel identity
    // ========================================================================

    // ========================================================================
    // Auto-Workspaces
    // ========================================================================

    // ========================================================================
    // Document CRUD
    // ========================================================================

    // ========================================================================
    // Op-Log Persistence
    // ========================================================================

    /// Run `f` as one SQLite transaction on this connection. Commits when
    /// `f` returns `Ok`; an `Err` rolls back every write `f` made. Fails if a
    /// transaction is already open on the connection.
    pub fn in_transaction<T>(
        &self,
        f: impl FnOnce(&Self) -> KernelDbResult<T>,
    ) -> KernelDbResult<T> {
        let tx = self.conn.unchecked_transaction()?;
        let out = f(self)?;
        tx.commit()?;
        Ok(out)
    }

    /// Run a WAL checkpoint in TRUNCATE mode: flush committed WAL frames into
    /// the main database file and shrink the `-wal` file back to zero.
    ///
    /// Without proactive checkpoints the WAL grows between SQLite's automatic
    /// 1000-page checkpoints, and a bare-file read of the main `.db` lags
    /// behind committed history — the mechanism behind the 2026-06-11
    /// journaling-forensics misread (no data was lost; the main file was
    /// simply stale while ~4 MB of newer ops sat only in `kernel.db-wal`).
    /// Compaction calls this so the main file tracks the oplog.
    ///
    /// Returns `(busy, log_frames, checkpointed_frames)` as SQLite reports
    /// them. `busy == 1` means a concurrent reader/writer on another
    /// connection prevented a full truncate; the checkpoint is best-effort
    /// and the caller treats busy as non-fatal.
    pub fn checkpoint(&self) -> KernelDbResult<(i64, i64, i64)> {
        let row = self.conn.query_row(
            "PRAGMA wal_checkpoint(TRUNCATE)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        Ok(row)
    }

    /// Hot-backup this database into a fresh file at `path` via SQLite's
    /// native `VACUUM INTO` — a single consistent, already-compacted
    /// snapshot that's safe to run against a live writer (unlike a bare `cp`
    /// of `kernel.db`, which can race the `-wal` file and produce a torn or
    /// stale copy). No prior `checkpoint()` call is required — `VACUUM INTO`
    /// reads a consistent snapshot regardless of WAL state.
    ///
    /// The path is bound as a query parameter (`VACUUM INTO ?1`), not
    /// string-interpolated, so a path containing `'` or other SQL
    /// metacharacters can't break out of the statement.
    ///
    /// SQLite refuses to overwrite an existing target file — that error
    /// surfaces as-is (`KernelDbError::Db`) rather than being papered over by
    /// pre-deleting the target, so a caller can never silently clobber an
    /// existing backup by name collision.
    pub fn vacuum_into<P: AsRef<Path>>(&self, path: P) -> KernelDbResult<()> {
        let path_str = path.as_ref().to_str().ok_or_else(|| {
            KernelDbError::Validation(format!(
                "backup path is not valid UTF-8: {}",
                path.as_ref().display()
            ))
        })?;
        self.conn.execute("VACUUM INTO ?1", params![path_str])?;
        Ok(())
    }

    // ========================================================================
    // Context CRUD
    // ========================================================================

    // ========================================================================
    // Context Edges
    // ========================================================================

    // ========================================================================
    // Presets
    // ========================================================================

    // ========================================================================
    // Workspaces
    // ========================================================================

    // ========================================================================
    // Workspace Paths
    // ========================================================================

    // ========================================================================
    // Context Shell Configuration
    // ========================================================================

    // ========================================================================
    // Context Token Usage (token-usage gauge)
    // ========================================================================

    // ========================================================================
    // Turns (per-turn provenance and spend)
    // ========================================================================

    // ========================================================================
    // Context Tool Bindings (Phase 5, D-54)
    // ========================================================================

    /// Force every subsequent write on this connection to fail with
    /// SQLite's read-only error, without touching schema or data — used to
    /// prove a write path leaves prior state untouched when the write
    /// itself fails partway. Never compiled outside test builds.
    #[cfg(test)]
    pub(crate) fn set_query_only_for_test(&self, on: bool) -> KernelDbResult<()> {
        let pragma = if on { "PRAGMA query_only = ON;" } else { "PRAGMA query_only = OFF;" };
        self.conn.execute_batch(pragma)?;
        Ok(())
    }

    // ========================================================================
    // Hook Persistence
    // ========================================================================

    // ========================================================================
    // Hook scripts (kernel-global shared kaish bodies)
    // ========================================================================

    // ========================================================================
    // Context Environment Variables
    // ========================================================================

    // ========================================================================
    // Context egress (docs/egress.md)
    // ========================================================================

    // ========================================================================
    // Claude cache breakpoints (per-context policy)
    // ========================================================================

}

impl KernelDb {
    // ========================================================================
    // Tracks (clock domains — docs/tracks.md Stage 1)
    // ========================================================================

    // ========================================================================
    // Attachments (context → track binding — docs/tracks.md §2+3)
    // ========================================================================

    // ========================================================================
    // Context Config Fork + Workspace Query
    // ========================================================================

    // ========================================================================
    // Workspace Permission Checking
    // ========================================================================

    // ========================================================================
    // Phase 4A: Additional methods for kj commands
    // ========================================================================

    // ========================================================================
    // Well-Known Context Registry (docs/drifting-dead-letters.md, slice 1)
    // ========================================================================

    // ========================================================================
    // Client Views (docs/shared-state.md "Retiring KV")
    // ========================================================================

    // ========================================================================
    // Dirty File Buffers (docs/file-buffers.md — swap-file marker)
    // ========================================================================

}

// ============================================================================
// LLM configuration (backends / models / casts / aliases / defaults)
// ============================================================================
//
// SQL is the source of truth for model configuration — there is no TOML in
// this path. Every write validates loudly and every failure bubbles: a
// swallowed config write means the kernel talks to the wrong model, which is
// exactly the silent-wrong-answer this project refuses.

fn read_backend_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<BackendId> {
    let bytes: Vec<u8> = row.get(idx)?;
    BackendId::try_from_slice(&bytes).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Blob,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "backend_id blob is not 16 bytes",
            )),
        )
    })
}


impl KernelDb {
    // ── backends ────────────────────────────────────────────────────────

    // ── backend_models ──────────────────────────────────────────────────

    // ── character slots ─────────────────────────────────────────────────

    // ── character models ────────────────────────────────────────────────

    // ── characters ──────────────────────────────────────────────────────

    // ── model_aliases ───────────────────────────────────────────────────

    // ── embedding_config ────────────────────────────────────────────────

}

/// The `thinking_style` tokens the providers accept; `llm/claude/build.rs`
/// refuses any other at turn build.
pub const THINKING_STYLES: [&str; 4] = ["off", "auto", "adaptive", "budget"];

/// Range-check the shared tunable knobs. Loud rejection, never a clamp — the
/// SQL CHECKs say the same thing, but a Rust-side error names the knob and
/// the legal range instead of surfacing `SQLITE_CONSTRAINT`.
fn validate_tunables(
    max_tokens: Option<i64>,
    temperature: Option<f64>,
    top_p: Option<f64>,
    thinking_budget: Option<i64>,
    thinking_style: Option<&str>,
) -> KernelDbResult<()> {
    if let Some(v) = max_tokens
        && v <= 0
    {
        return Err(KernelDbError::Validation(format!(
            "max_tokens must be a positive integer (got {v})"
        )));
    }
    if let Some(v) = temperature
        && !(0.0..=2.0).contains(&v)
    {
        return Err(KernelDbError::Validation(format!(
            "temperature must be in 0.0..=2.0 (got {v})"
        )));
    }
    if let Some(v) = top_p
        && !(v > 0.0 && v <= 1.0)
    {
        return Err(KernelDbError::Validation(format!(
            "top_p must be in (0.0, 1.0] (got {v})"
        )));
    }
    if let Some(v) = thinking_budget
        && v <= 0
    {
        return Err(KernelDbError::Validation(format!(
            "thinking_budget must be a positive integer (got {v})"
        )));
    }
    if let Some(v) = thinking_style
        && !THINKING_STYLES.contains(&v)
    {
        return Err(KernelDbError::Validation(format!(
            "thinking_style must be one of {} (got {v:?})",
            THINKING_STYLES.join(", ")
        )));
    }
    Ok(())
}

// ============================================================================
// Row parsers
// ============================================================================

fn row_to_document_row(row: &rusqlite::Row<'_>) -> SqliteResult<DocumentRow> {
    let kind_str: String = row.get(2)?;
    Ok(DocumentRow {
        document_id: read_context_id(row, 0)?,
        workspace_id: read_workspace_id(row, 1)?,
        doc_kind: doc_kind_from_sql(&kind_str),
        language: row.get(3)?,
        path: row.get(4)?,
        created_at: row.get(5)?,
        created_by: read_principal_id(row, 6)?,
    })
}

fn row_to_context_row(row: &rusqlite::Row<'_>) -> SqliteResult<ContextRow> {
    let state_str: String = row.get(3)?;
    let fork_kind_str: Option<String> = row.get(8)?;

    Ok(ContextRow {
        context_id: read_context_id(row, 0)?,
        label: row.get(1)?,
        system_prompt: row.get(2)?,
        context_state: context_state_from_sql(&state_str),
        context_type: row.get(4)?,
        created_at: row.get(5)?,
        created_by: read_principal_id(row, 6)?,
        forked_from: read_opt_context_id(row, 7)?,
        fork_kind: fork_kind_from_sql(fork_kind_str)?,
        archived_at: row.get(9)?,
        workspace_id: read_opt_workspace_id(row, 10)?,
        preset_id: read_opt_preset_id(row, 11)?,
        concluded_at: row.get(12)?,
        last_activity_at: row.get(13)?,
        promoted_at: row.get(14)?,
        demoted_at: row.get(15)?,
        paused_at: row.get(16)?,
        origin_host: row.get(17)?,
        played_by: read_opt_principal_id(row, 18)?,
    })
}

fn row_to_edge_row(row: &rusqlite::Row<'_>) -> SqliteResult<ContextEdgeRow> {
    Ok(ContextEdgeRow {
        edge_id: read_edge_id(row, 0)?,
        source_id: read_context_id(row, 1)?,
        target_id: read_context_id(row, 2)?,
        metadata: row.get(3)?,
        created_at: row.get(4)?,
    })
}

fn row_to_preset_row(row: &rusqlite::Row<'_>) -> SqliteResult<PresetRow> {
    Ok(PresetRow {
        preset_id: read_preset_id(row, 0)?,
        label: row.get(1)?,
        description: row.get(2)?,
        created_at: row.get(3)?,
        created_by: read_principal_id(row, 4)?,
    })
}

fn row_to_character_row(row: &rusqlite::Row<'_>) -> SqliteResult<CharacterRow> {
    Ok(CharacterRow {
        principal_id: read_principal_id(row, 0)?,
        name: row.get(1)?,
        created_at: row.get(2)?,
        retired_at: row.get(3)?,
        handoff_ctx: read_opt_context_id(row, 4)?,
        root: row.get(5)?,
        root_ctx: read_opt_context_id(row, 6)?,
        context_type: row.get(7)?,
    })
}

fn row_to_hook_script_row(row: &rusqlite::Row<'_>) -> SqliteResult<HookScriptRow> {
    Ok(HookScriptRow {
        script_id: row.get(0)?,
        body: row.get(1)?,
        description: row.get(2)?,
        created_at: row.get(3)?,
        created_by: read_principal_id(row, 4)?,
        updated_at: row.get(5)?,
    })
}

fn row_to_workspace_row(row: &rusqlite::Row<'_>) -> SqliteResult<WorkspaceRow> {
    Ok(WorkspaceRow {
        workspace_id: read_workspace_id(row, 0)?,
        label: row.get(1)?,
        description: row.get(2)?,
        created_at: row.get(3)?,
        created_by: read_principal_id(row, 4)?,
        archived_at: row.get(5)?,
    })
}

// ============================================================================
// Roster (crate::roster) — storage half; types/invariants live in roster.rs
// ============================================================================

impl KernelDb {
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fork_kind_from_sql_fails_loud_on_unknown() {
        // NULL column → None; known value parses.
        assert!(fork_kind_from_sql(None).unwrap().is_none());
        assert_eq!(
            fork_kind_from_sql(Some("filtered".into())).unwrap(),
            Some(ForkKind::Filtered)
        );
        // A retired ('shallow', 'subtree') or corrupt value must ERROR, never
        // silently degrade to None — that would erase fork provenance.
        assert!(fork_kind_from_sql(Some("shallow".into())).is_err());
        assert!(fork_kind_from_sql(Some("subtree".into())).is_err());
        assert!(fork_kind_from_sql(Some("garbage".into())).is_err());
    }

    // ── 1. Schema idempotent ────────────────────────────────────────────

    #[test]
    fn schema_idempotent() {
        let db = KernelDb::temporary().unwrap();
        // Apply schema again — should not error.
        db.conn.execute_batch(SCHEMA).unwrap();
    }

    /// `is_duplicate_column_error` is the guard that lets
    /// `apply_additive_migrations` re-run safely against an already-migrated
    /// DB — it must match SQLite's actual "duplicate column name" wording and
    /// nothing else (a different message must not be misclassified as the
    /// ignorable case and swallowed).
    #[test]
    fn duplicate_column_error_detection_is_specific() {
        let db = KernelDb::temporary().unwrap();
        let dup = db
            .conn
            .execute(
                "ALTER TABLE tracks ADD COLUMN deleted_at INTEGER",
                [],
            )
            .unwrap_err();
        assert!(is_duplicate_column_error(&dup), "expected duplicate-column: {dup}");

        let other = db
            .conn
            .execute("ALTER TABLE no_such_table ADD COLUMN x INTEGER", [])
            .unwrap_err();
        assert!(
            !is_duplicate_column_error(&other),
            "a no-such-table failure must not be misclassified as duplicate-column: {other}"
        );
    }

    // ── approval-ledger migration ──────────────────────────────────────

    // ── synchronous pragma ────────────────────────────────────────────

    /// `init_connection` must set `synchronous = NORMAL` (1), not SQLite's
    /// default `FULL` (2). Under WAL, NORMAL fsyncs at checkpoint instead of
    /// on every commit, and a streaming turn commits once per delta.
    #[test]
    fn init_connection_sets_synchronous_normal() {
        let db = KernelDb::temporary().unwrap();
        let mode: i64 = db
            .conn_for_ledger()
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, 1, "synchronous should be NORMAL (1), got {mode}");
    }

    // ── journal size limit ───────────────────────────────────────────────

    /// `init_connection` must cap the WAL's high-water mark with
    /// `journal_size_limit`, or SQLite's default of -1 (no limit) lets a busy
    /// kernel's `-wal` file grow without bound and never shrink back down,
    /// since a checkpoint only truncates a WAL larger than this limit.
    #[test]
    fn init_connection_sets_journal_size_limit() {
        let db = KernelDb::temporary().unwrap();
        let limit: i64 = db
            .conn_for_ledger()
            .query_row("PRAGMA journal_size_limit", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            limit, 67_108_864,
            "journal_size_limit should be 67108864 (64 MiB), got {limit}"
        );
    }

    // ── WAL checkpoint ──────────────────────────────────────────────────

    /// `checkpoint()` (TRUNCATE) must flush committed frames into the main
    /// file and shrink the `-wal` file back to zero. Guards the proactive
    /// checkpoint that compaction relies on so a bare-file read of the main
    /// `.db` stops lagging committed history (2026-06-11 forensics). On-disk
    /// db required — a `:memory:` connection has no WAL file.
    #[test]
    fn checkpoint_truncates_wal() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ckpt.db");
        let wal_path = dir.path().join("ckpt.db-wal");

        let db = KernelDb::open(&db_path).unwrap();
        let ws_id = db
            .get_or_create_default_workspace(PrincipalId::system())
            .unwrap();
        let doc = ContextId::new();
        db.insert_document(&DocumentRow {
            document_id: doc,
            workspace_id: ws_id,
            doc_kind: DocKind::Conversation,
            language: None,
            path: None,
            created_at: now_millis(),
            created_by: PrincipalId::system(),
        })
        .unwrap();

        // 200 × 4 KiB ops ≈ 800 KiB — grows the WAL but stays under SQLite's
        // 1000-page (~4 MiB) auto-checkpoint, so the WAL is non-empty and
        // un-truncated when we reach the manual checkpoint below.
        let payload = vec![0xABu8; 4096];
        for seq in 1..=200 {
            db.append_op(doc, seq, &payload).unwrap();
        }

        let wal_before = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
        assert!(
            wal_before > 0,
            "WAL should have grown after 200 appends, got {wal_before} bytes",
        );

        let (busy, _log, _checkpointed) = db.checkpoint().unwrap();
        assert_eq!(busy, 0, "single-connection checkpoint must not be busy");

        // The behavioral proof: TRUNCATE shrinks the -wal file back to zero,
        // so a bare-file read of the main .db now reflects all 200 ops. (The
        // log/checkpointed frame counts are version-dependent for TRUNCATE and
        // not asserted.)
        let wal_after = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
        assert_eq!(
            wal_after, 0,
            "TRUNCATE checkpoint should zero the -wal file, got {wal_after} bytes",
        );
    }

    // ── VACUUM INTO backup ──────────────────────────────────────────────

    /// `vacuum_into` must produce a real, non-empty file — the behavioral
    /// guard against a silent no-op (e.g. a typo'd pragma or a swallowed
    /// error that still returns `Ok`). A bug that made `vacuum_into` write
    /// nothing would still pass a test that only checked `is_ok()`; this one
    /// also asserts the file exists and has SQLite's magic header size.
    #[test]
    fn vacuum_into_produces_nonempty_file() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("source.db");
        let backup_path = dir.path().join("backup.db");

        let db = KernelDb::open(&db_path).unwrap();
        db.get_or_create_default_workspace(PrincipalId::system())
            .unwrap();

        assert!(!backup_path.exists(), "precondition: backup must not exist yet");
        db.vacuum_into(&backup_path).unwrap();

        let meta = std::fs::metadata(&backup_path)
            .expect("vacuum_into must create the backup file");
        assert!(
            meta.len() >= 100,
            "backup file too small to be a real SQLite db ({} bytes) — vacuum_into may have \
             written nothing",
            meta.len()
        );
    }

    /// The backup must be a fully valid, queryable SQLite database containing
    /// the same rows as the source — not just a non-empty file. Opens the
    /// backup as an independent `KernelDb` and checks it has the same
    /// singleton kernel id and the workspace/document rows written before
    /// the backup was taken.
    #[test]
    fn vacuum_into_backup_is_valid_and_contains_expected_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("source.db");
        let backup_path = dir.path().join("backup.db");

        let db = KernelDb::open(&db_path).unwrap();
        let ws_id = db
            .get_or_create_default_workspace(PrincipalId::system())
            .unwrap();
        let doc = ContextId::new();
        db.insert_document(&DocumentRow {
            document_id: doc,
            workspace_id: ws_id,
            doc_kind: DocKind::Conversation,
            language: None,
            path: None,
            created_at: now_millis(),
            created_by: PrincipalId::system(),
        })
        .unwrap();
        let source_kernel_id = db.kernel_id().unwrap();

        db.vacuum_into(&backup_path).unwrap();

        // Open the backup as an independent connection — proves it's a
        // structurally valid SQLite file with the full schema, not a
        // truncated or corrupt copy.
        let backup = KernelDb::open(&backup_path).unwrap();
        assert_eq!(
            backup.kernel_id().unwrap(),
            source_kernel_id,
            "backup must carry the same singleton kernel identity"
        );
        let restored_doc = backup
            .get_document(doc)
            .expect("query backup db")
            .expect("document present in backup");
        assert_eq!(restored_doc.doc_kind, DocKind::Conversation);
    }

    /// SQLite refuses to `VACUUM INTO` an existing file. The error must
    /// surface cleanly (not panic, not silently succeed by overwriting) —
    /// `kj db backup` relies on this to avoid clobbering a same-named backup.
    #[test]
    fn vacuum_into_target_exists_errors_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("source.db");
        let backup_path = dir.path().join("backup.db");
        std::fs::write(&backup_path, b"not a database, just occupying the name").unwrap();

        let db = KernelDb::open(&db_path).unwrap();
        let result = db.vacuum_into(&backup_path);

        assert!(
            result.is_err(),
            "vacuum_into must error when the target file already exists"
        );
        // The pre-existing (bogus) file content must be left untouched —
        // vacuum_into must not have deleted-then-recreated it.
        let contents = std::fs::read(&backup_path).unwrap();
        assert_eq!(contents, b"not a database, just occupying the name");
    }

    /// A path with a `'` — the SQL string-literal delimiter — must not be
    /// able to break out of the `VACUUM INTO` statement. Guards the bound-
    /// parameter choice (`?1`) over string interpolation.
    #[test]
    fn vacuum_into_path_with_quote_is_bound_not_interpolated() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("source.db");
        let backup_path = dir.path().join("back'up.db");

        let db = KernelDb::open(&db_path).unwrap();
        db.vacuum_into(&backup_path).unwrap();

        assert!(
            backup_path.exists(),
            "a bound parameter should handle a quote in the filename literally"
        );
    }

    // ── 2. Context lifecycle ────────────────────────────────────────────

    // ── 3. Label validation: colon ──────────────────────────────────────

    #[test]
    fn label_validation_colon() {
        let db = KernelDb::temporary().unwrap();
        let row = make_context_row(Some("my:label"));

        let err = db.insert_context(&row).unwrap_err();
        assert!(matches!(err, KernelDbError::InvalidLabel(_)));
    }

    // ── 4. Label uniqueness ────────────────────────────────────────────

    // ── 5. Fork lineage 3 deep ─────────────────────────────────────────

    // ── 8. Drift edge allows dupes ─────────────────────────────────────

    // ── 10. Preset lifecycle ───────────────────────────────────────────

    // ── 11. Workspace lifecycle ────────────────────────────────────────

    // ── 12. Workspace soft delete ──────────────────────────────────────

    // ── 13. Drift push creates edge ────────────────────────────────────

    // ── 14. Context DAG recursive ──────────────────────────────────────

    // ── 15. Tag resolution (label + prefix) ────────────────────────────

    // ── 16. Resolve context basic ──────────────────────────────────────

    // ── 17. Null labels coexist ────────────────────────────────────────

    // ── 18. Archive excludes from active + fork_children ─────────

    // ── 19. Context edge cascade ───────────────────────────────────────

    // ── 20. Context row to_context conversion ──────────────────────────

    // ── 21. Roundtrip: create context, read back, verify all 15 fields ──

    // ── 22. FK violation produces Validation, not LabelConflict ──────────

    #[test]
    fn fk_violation_is_validation_error() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);

        // Reference a workspace_id that doesn't exist on the context
        let ctx_id = ContextId::new();
        // Insert document with valid workspace first
        db.insert_document(&DocumentRow {
            document_id: ctx_id,
                        workspace_id: ws_id,
            doc_kind: DocKind::Conversation,
            language: None,
            path: None,
            created_at: now_millis(),
            created_by: PrincipalId::new(),
        })
        .unwrap();

        let row = ContextRow {
            context_id: ctx_id,
            label: Some("fk-test".into()),
            system_prompt: None,
            context_state: ContextState::Live,
            context_type: "default".to_string(),
            created_at: now_millis(),
            created_by: PrincipalId::new(),
            forked_from: None,
            fork_kind: None,
            archived_at: None,
            workspace_id: Some(WorkspaceId::new()), // doesn't exist
            preset_id: None,
            concluded_at: None,
            last_activity_at: None,
            promoted_at: None,
            demoted_at: None,
            paused_at: None,
            origin_host: None,
            played_by: None,
        };
        let err = db.insert_context(&row).unwrap_err();
        assert!(
            matches!(err, KernelDbError::Validation(_)),
            "expected Validation for FK violation, got: {err}"
        );
    }

    // ── 22b. Characters CRUD ──────────────────────────────────────────

    // ── 22c. the `root` flag ────────────────────────────────────────────

    // ── 22e. the senpai migration ───────────────────────────────────────

    /// `open_read_only` reads an existing `kernel.db` without running
    /// `SCHEMA` or migrations — the lockout-recovery path
    /// (`kaijutsu-server list-characters`), which must work with the
    /// service stopped and must never contend with a live server's write
    /// connection.
    #[test]
    fn open_read_only_reads_an_existing_db_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kernel.db");
        let character = PrincipalId::new();
        {
            let db = KernelDb::open(&path).unwrap();
            db.insert_character(&CharacterRow { context_type: None,
                principal_id: character,
                name: "hajime".to_string(),
                created_at: 1000,
                retired_at: None,
                handoff_ctx: None, root_ctx: None, root: false,
            })
            .unwrap();
        }

        let ro = KernelDb::open_read_only(&path).unwrap();
        let row = ro.get_character_by_name("hajime").unwrap().unwrap();
        assert_eq!(row.principal_id, character);

        // A write attempt through the read-only connection must fail loudly
        // rather than silently succeeding — the whole point of the mode.
        let err = ro
            .insert_character(&CharacterRow { context_type: None,
                principal_id: PrincipalId::new(),
                name: "intruder".to_string(),
                created_at: 2000,
                retired_at: None,
                handoff_ctx: None, root_ctx: None, root: false,
            })
            .unwrap_err();
        assert!(
            matches!(err, KernelDbError::Db(_)),
            "a write on a read-only connection must fail, got: {err}"
        );
    }

    /// `open_read_only` against a path with no database must fail loudly —
    /// there is nothing sensible to fall back to (`docs/character.md`'s
    /// "corruption, never a fallback" rule applies here too).
    #[test]
    fn open_read_only_fails_loudly_on_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-kernel.db");
        assert!(KernelDb::open_read_only(&missing).is_err());
    }

    // ── 23. Context shell CRUD ────────────────────────────────────────

    // ── 23b. Context tool bindings CRUD (Phase 5, D-54) ──────────────
    //
    // Normalized schema: parent `context_bindings` + `_instances` (ordered)
    // + `_names` (sticky map). The get path reconstructs `ContextToolBinding`
    // by joining; the upsert path writes all three tables transactionally.

    // ── Context egress (docs/egress.md) ─────────────────────────────

    #[test]
    fn validate_egress_host_accepts_dns_names_loopback_literals_and_star() {
        assert_eq!(validate_egress_host("crates.io").unwrap(), "crates.io");
        assert_eq!(validate_egress_host("LOCALHOST").unwrap(), "localhost");
        assert_eq!(validate_egress_host("127.0.0.1").unwrap(), "127.0.0.1");
        assert_eq!(validate_egress_host("::1").unwrap(), "::1");
        assert_eq!(validate_egress_host("0:0:0:0:0:0:0:1").unwrap(), "::1");
        assert_eq!(validate_egress_host("*").unwrap(), "*");
    }

    #[test]
    fn validate_egress_host_rejects_scheme_path_port_wildcard_empty_and_space() {
        for bad in ["https://x", "x/path", "x:443", "*.x.io", "", "x y"] {
            let err = validate_egress_host(bad).unwrap_err();
            assert!(matches!(err, KernelDbError::InvalidEgressHost(_)), "{bad}: {err}");
        }
    }

    // ── Fork inheritance ──────────────────────────────────────────────

    // ── 25. Workspace paths with read_only ────────────────────────────

    // ── 27. Context workspace paths query ─────────────────────────────

    // ── 28. Workspace path permission checking ────────────────────────

    // ── 29. Time-well ring placement (promote/demote/pause) ────────────

    // ── Roster: dropping the legacy value-enum CHECKs ───────────────────
    // Rust-API coverage (upsert/reconcile/restart-shaped/generation/clock
    // skew) lives in `roster.rs`'s own test module. These exercise the
    // schema directly, the same split approval-ledger's `schema.rs` uses —
    // but where that split used to mean "the CHECK rejects what the Rust
    // API never sends," these four tables no longer carry that CHECK at
    // all (`drop_legacy_value_enum_checks`'s doc comment states the
    // ruling). `RosterEntity::from_parts`, `LivenessKind::parse`, and
    // `Availability::parse` (`roster.rs`) are the only gates left.

    fn put_roster_entity_back_on_its_legacy_shape(conn: &Connection) {
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE roster_entity_legacy (
                 entity_kind   TEXT NOT NULL CHECK (entity_kind IN ('principal', 'context')),
                 entity_id     BLOB NOT NULL,
                 label         TEXT,
                 first_seen_at INTEGER NOT NULL
                     DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
                 PRIMARY KEY (entity_kind, entity_id)
             );
             INSERT INTO roster_entity_legacy (entity_kind, entity_id, label, first_seen_at)
                 SELECT entity_kind, entity_id, label, first_seen_at FROM roster_entity;
             DROP TABLE roster_entity;
             ALTER TABLE roster_entity_legacy RENAME TO roster_entity;
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
    }

    fn put_roster_presence_back_on_its_legacy_shape(conn: &Connection) {
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE roster_presence_legacy (
                 source          TEXT NOT NULL,
                 source_local_id TEXT NOT NULL,
                 entity_kind     TEXT NOT NULL,
                 entity_id       BLOB NOT NULL,
                 liveness_kind   TEXT NOT NULL
                     CHECK (liveness_kind IN ('bound', 'recent', 'attested')),
                 host            TEXT,
                 pid             INTEGER,
                 proc_start      INTEGER,
                 first_seen_at   INTEGER NOT NULL,
                 PRIMARY KEY (source, source_local_id),
                 FOREIGN KEY (entity_kind, entity_id)
                     REFERENCES roster_entity(entity_kind, entity_id) ON DELETE CASCADE
             );
             INSERT INTO roster_presence_legacy
                 (source, source_local_id, entity_kind, entity_id, liveness_kind,
                  host, pid, proc_start, first_seen_at)
                 SELECT source, source_local_id, entity_kind, entity_id, liveness_kind,
                        host, pid, proc_start, first_seen_at
                   FROM roster_presence;
             DROP TABLE roster_presence;
             ALTER TABLE roster_presence_legacy RENAME TO roster_presence;
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
    }

    fn put_roster_activity_back_on_its_legacy_shape(conn: &Connection) {
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE roster_activity_legacy (
                 source          TEXT    NOT NULL,
                 source_local_id TEXT    NOT NULL,
                 live            INTEGER NOT NULL CHECK (live IN (0, 1)),
                 observed_at     INTEGER,
                 recorded_at     INTEGER NOT NULL,
                 PRIMARY KEY (source, source_local_id),
                 FOREIGN KEY (source, source_local_id)
                     REFERENCES roster_presence(source, source_local_id) ON DELETE CASCADE
             );
             INSERT INTO roster_activity_legacy (source, source_local_id, live, observed_at, recorded_at)
                 SELECT source, source_local_id, live, observed_at, recorded_at FROM roster_activity;
             DROP TABLE roster_activity;
             ALTER TABLE roster_activity_legacy RENAME TO roster_activity;
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
    }

    fn put_roster_status_back_on_its_legacy_shape(conn: &Connection) {
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE roster_status_legacy (
                 entity_kind  TEXT NOT NULL,
                 entity_id    BLOB NOT NULL,
                 status_text  TEXT,
                 availability TEXT NOT NULL CHECK (availability IN ('active', 'idle', 'away', 'dnd')),
                 observed_at  INTEGER,
                 recorded_at  INTEGER NOT NULL,
                 PRIMARY KEY (entity_kind, entity_id),
                 FOREIGN KEY (entity_kind, entity_id)
                     REFERENCES roster_entity(entity_kind, entity_id) ON DELETE CASCADE
             );
             INSERT INTO roster_status_legacy
                 (entity_kind, entity_id, status_text, availability, observed_at, recorded_at)
                 SELECT entity_kind, entity_id, status_text, availability, observed_at, recorded_at
                   FROM roster_status;
             DROP TABLE roster_status;
             ALTER TABLE roster_status_legacy RENAME TO roster_status;
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
    }

    /// A database carrying `roster_entity`'s legacy `entity_kind` CHECK must
    /// lose it on the next open (`apply_additive_migrations`), keep the row
    /// it already had, and accept a kind the old CHECK rejected. Falsified
    /// by reverting `drop_legacy_value_enum_checks` to a no-op: the final
    /// INSERT below then failed with a CHECK violation.
    #[test]
    fn drop_legacy_value_enum_checks_drops_the_roster_entity_check_and_keeps_rows() {
        let db = KernelDb::temporary().unwrap();
        db.conn
            .execute(
                "INSERT INTO roster_entity (entity_kind, entity_id, label, first_seen_at)
                 VALUES ('principal', ?1, 'kept', 1)",
                params![blob_param(PrincipalId::new().as_bytes())],
            )
            .unwrap();
        put_roster_entity_back_on_its_legacy_shape(&db.conn);

        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        let sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'roster_entity'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!sql.contains("CHECK (entity_kind IN ("), "the value-enum CHECK must be gone, got: {sql}");
        let label: String = db
            .conn
            .query_row("SELECT label FROM roster_entity WHERE entity_kind = 'principal'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(label, "kept");

        // Neither 'principal' nor 'context' — never admitted by the retired
        // CHECK. `RosterEntity::from_parts` (roster.rs) is the sole gate now.
        db.conn
            .execute(
                "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
                 VALUES ('service', ?1, 1)",
                params![blob_param(&[7u8; 16])],
            )
            .unwrap();
    }

    /// Same contract as the `roster_entity` test above, for
    /// `roster_presence.liveness_kind`.
    #[test]
    fn drop_legacy_value_enum_checks_drops_the_roster_presence_check_and_keeps_rows() {
        let db = KernelDb::temporary().unwrap();
        let principal = PrincipalId::new();
        db.conn
            .execute(
                "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
                 VALUES ('principal', ?1, 1)",
                params![blob_param(principal.as_bytes())],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO roster_presence
                    (source, source_local_id, entity_kind, entity_id, liveness_kind, first_seen_at)
                 VALUES ('kept-source', 'kept-id', 'principal', ?1, 'bound', 1)",
                params![blob_param(principal.as_bytes())],
            )
            .unwrap();
        put_roster_presence_back_on_its_legacy_shape(&db.conn);

        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        let sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'roster_presence'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!sql.contains("CHECK (liveness_kind IN ("), "the value-enum CHECK must be gone, got: {sql}");
        let kept: String = db
            .conn
            .query_row("SELECT liveness_kind FROM roster_presence WHERE source = 'kept-source'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(kept, "bound");

        // 'heard' is the design table's fourth kind, deliberately never
        // admitted by the retired CHECK (no v1 producer). It now inserts
        // cleanly — `LivenessKind::parse` (roster.rs) is the sole gate.
        db.conn
            .execute(
                "INSERT INTO roster_presence
                    (source, source_local_id, entity_kind, entity_id, liveness_kind, first_seen_at)
                 VALUES ('new-source', 'new-id', 'principal', ?1, 'heard', 1)",
                params![blob_param(principal.as_bytes())],
            )
            .unwrap();
    }

    /// Same contract as the `roster_entity` test above, for
    /// `roster_activity.live`.
    #[test]
    fn drop_legacy_value_enum_checks_drops_the_roster_activity_check_and_keeps_rows() {
        let db = KernelDb::temporary().unwrap();
        let principal = PrincipalId::new();
        db.conn
            .execute(
                "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
                 VALUES ('principal', ?1, 1)",
                params![blob_param(principal.as_bytes())],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO roster_presence
                    (source, source_local_id, entity_kind, entity_id, liveness_kind, first_seen_at)
                 VALUES ('kept-source', 'kept-id', 'principal', ?1, 'bound', 1)",
                params![blob_param(principal.as_bytes())],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO roster_activity (source, source_local_id, live, recorded_at)
                 VALUES ('kept-source', 'kept-id', 1, 1)",
                [],
            )
            .unwrap();
        put_roster_activity_back_on_its_legacy_shape(&db.conn);

        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        let sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'roster_activity'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!sql.contains("CHECK (live IN ("), "the value-enum CHECK must be gone, got: {sql}");
        let kept: i64 = db
            .conn
            .query_row("SELECT live FROM roster_activity WHERE source = 'kept-source'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(kept, 1);

        // `live` was a strict 0/1 boolean under the retired CHECK. The only
        // writer (`roster_reconcile_presence`) always writes 0 or 1; this
        // pins that the schema itself no longer enforces it.
        db.conn
            .execute(
                "INSERT INTO roster_presence
                    (source, source_local_id, entity_kind, entity_id, liveness_kind, first_seen_at)
                 VALUES ('new-source', 'new-id', 'principal', ?1, 'bound', 1)",
                params![blob_param(principal.as_bytes())],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO roster_activity (source, source_local_id, live, recorded_at)
                 VALUES ('new-source', 'new-id', 2, 1)",
                [],
            )
            .unwrap();
    }

    /// Same contract as the `roster_entity` test above, for
    /// `roster_status.availability`.
    #[test]
    fn drop_legacy_value_enum_checks_drops_the_roster_status_check_and_keeps_rows() {
        let db = KernelDb::temporary().unwrap();
        let principal = PrincipalId::new();
        db.conn
            .execute(
                "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
                 VALUES ('principal', ?1, 1)",
                params![blob_param(principal.as_bytes())],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO roster_status (entity_kind, entity_id, availability, recorded_at)
                 VALUES ('principal', ?1, 'active', 1)",
                params![blob_param(principal.as_bytes())],
            )
            .unwrap();
        put_roster_status_back_on_its_legacy_shape(&db.conn);

        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        let sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'roster_status'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!sql.contains("CHECK (availability IN ("), "the value-enum CHECK must be gone, got: {sql}");
        let kept: String = db
            .conn
            .query_row("SELECT availability FROM roster_status WHERE entity_kind = 'principal'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(kept, "active");

        // 'sideways' was never one of the retired CHECK's four values. It
        // now inserts cleanly — `Availability::parse` (roster.rs) is the
        // sole gate.
        let second = PrincipalId::new();
        db.conn
            .execute(
                "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
                 VALUES ('principal', ?1, 1)",
                params![blob_param(second.as_bytes())],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO roster_status (entity_kind, entity_id, availability, recorded_at)
                 VALUES ('principal', ?1, 'sideways', 1)",
                params![blob_param(second.as_bytes())],
            )
            .unwrap();
    }

    /// The hazard `drop_legacy_value_enum_checks` exists to avoid:
    /// `roster_entity` is the FK parent of `roster_presence` and
    /// `roster_status` (`ON DELETE CASCADE`), and `roster_presence` is in
    /// turn the parent of `roster_activity`. With foreign keys enforced —
    /// the connection's normal state, set by `init_connection` — a naive
    /// rebuild of `roster_entity` performs an implicit `DELETE FROM` under
    /// `DROP TABLE` and fires every one of those cascades, emptying the
    /// live roster. Falsified by removing the `PRAGMA foreign_keys = OFF`
    /// window from `drop_legacy_value_enum_checks`: every child row seeded
    /// below was gone after `apply_additive_migrations`.
    #[test]
    fn drop_legacy_value_enum_checks_does_not_cascade_delete_the_live_roster() {
        let mut db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("roster-cascade"));
        insert_context_with_doc(&db, &row, ws_id);
        let entity = RosterEntity::Context(row.context_id);

        db.roster_reconcile_presence(
            "cascade-source",
            LivenessKind::Bound,
            &[PresenceSnapshotRow {
                source_local_id: "cascade-peer".into(),
                entity,
                entity_label: Some("cascade".into()),
                host: Some("localhost".into()),
                pid: Some(4321),
                proc_start: Some(1000),
                observed_at: Some(1000),
                live: true,
            }],
            1000,
        )
        .unwrap();
        db.roster_write_status(entity, Some("cascade"), Some("hi"), Availability::Active, Some(1000), 1000)
            .unwrap();

        let count = |table: &str| -> i64 {
            db.conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
        };
        // Confirm the fixture actually populated every child table before
        // touching anything, so a later count of 0 unambiguously means the
        // migration deleted rows rather than never having seen any.
        assert_eq!(count("roster_presence"), 1);
        assert_eq!(count("roster_activity"), 1);
        assert_eq!(count("roster_status"), 1);

        put_roster_entity_back_on_its_legacy_shape(&db.conn);

        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        assert_eq!(count("roster_entity"), 1, "the entity row must survive its own rebuild");
        assert_eq!(
            count("roster_presence"),
            1,
            "roster_entity's rebuild must not cascade into roster_presence"
        );
        assert_eq!(
            count("roster_activity"),
            1,
            "the cascade must not reach two levels down into roster_activity"
        );
        assert_eq!(count("roster_status"), 1, "roster_entity's rebuild must not cascade into roster_status");
    }

    /// `DROP TABLE` takes a table's triggers with it. After a
    /// `roster_entity` rebuild, `roster_entity_context_must_exist` must
    /// both still be listed in `sqlite_master` and still fire — checking
    /// only the listing would miss a trigger body silently changed by a
    /// bad `SCHEMA` edit. Falsified by skipping the post-rebuild `SCHEMA`
    /// re-run in `drop_legacy_value_enum_checks`: the trigger disappeared
    /// and the phantom insert below succeeded instead of erroring.
    #[test]
    fn drop_legacy_value_enum_checks_restores_the_roster_entity_trigger() {
        let db = KernelDb::temporary().unwrap();
        put_roster_entity_back_on_its_legacy_shape(&db.conn);

        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        let trigger_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                  WHERE type = 'trigger' AND name = 'roster_entity_context_must_exist'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(trigger_count, 1, "the joinability trigger must survive the rebuild");

        let phantom = ContextId::new();
        let err = db
            .conn
            .execute(
                "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
                 VALUES ('context', ?1, 1)",
                params![blob_param(phantom.as_bytes())],
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("never minted"),
            "expected the joinability trigger's message, got: {err}"
        );
    }

    /// `DROP TABLE` takes a table's indexes with it too. After a
    /// `roster_presence` rebuild, `idx_roster_presence_entity` must still
    /// exist. Falsified the same way as the trigger test above.
    #[test]
    fn drop_legacy_value_enum_checks_restores_the_roster_presence_index() {
        let db = KernelDb::temporary().unwrap();
        put_roster_presence_back_on_its_legacy_shape(&db.conn);

        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        let index_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                  WHERE type = 'index' AND name = 'idx_roster_presence_entity'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 1, "idx_roster_presence_entity must survive the rebuild");
    }

    /// A real `documents` row, because `doc_snapshots.document_id` is a
    /// foreign key and a snapshot for a document that does not exist would
    /// not be a case this migration ever meets.
    fn setup_snapshot_document(db: &KernelDb) -> ContextId {
        let ws_id = setup_test_db(db);
        let doc_id = ContextId::new();
        db.insert_document(&DocumentRow {
            document_id: doc_id,
            workspace_id: ws_id,
            doc_kind: DocKind::Conversation,
            language: None,
            path: None,
            created_at: now_millis(),
            created_by: PrincipalId::system(),
        })
        .unwrap();
        doc_id
    }

    /// Bring `doc_snapshots` back to the shape that carried `content`, and
    /// seed a row through it, so the next open has something real to migrate.
    fn put_doc_snapshots_back_with_its_content_column(conn: &Connection, doc: ContextId) {
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             DROP TABLE doc_snapshots;
             CREATE TABLE doc_snapshots (
                 document_id BLOB    NOT NULL PRIMARY KEY,
                 seq         INTEGER NOT NULL,
                 version     INTEGER NOT NULL,
                 state       BLOB    NOT NULL,
                 content     TEXT    NOT NULL,
                 created_at  INTEGER NOT NULL
                     DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
                 FOREIGN KEY (document_id) REFERENCES documents(document_id) ON DELETE CASCADE
             );
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO doc_snapshots (document_id, seq, version, state, content, created_at)
             VALUES (?1, 7, 3, X'DEADBEEF', 'the derived copy', 1)",
            params![blob_param(doc.as_bytes())],
        )
        .unwrap();
    }

    /// A database still carrying `doc_snapshots.content` loses the column on
    /// the next open, and the row it held keeps every other value. The text
    /// was recoverable from `state` all along, so dropping it loses nothing
    /// a reader could have wanted.
    #[test]
    fn the_doc_snapshots_content_column_is_dropped_and_the_row_survives() {
        let db = KernelDb::temporary().unwrap();
        let doc = setup_snapshot_document(&db);
        put_doc_snapshots_back_with_its_content_column(&db.conn, doc);

        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        let sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'doc_snapshots'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!sql.contains("content"), "the content column must be gone, got: {sql}");

        let row = db.load_latest_snapshot(doc).unwrap().expect("the row must survive the drop");
        assert_eq!(row.seq, 7, "seq must survive");
        assert_eq!(row.version, 3, "version must survive");
        assert_eq!(row.state, vec![0xDE, 0xAD, 0xBE, 0xEF], "state must survive");
        assert_eq!(row.created_at, 1, "created_at must survive, and must not shift a column left");
    }

    /// The drop is guarded, so a second open does not re-run it — an
    /// unguarded `ALTER TABLE ... DROP COLUMN` on a column that is already
    /// gone is an error, not a no-op.
    #[test]
    fn dropping_the_doc_snapshots_content_column_is_idempotent() {
        let db = KernelDb::temporary().unwrap();
        let doc = setup_snapshot_document(&db);
        put_doc_snapshots_back_with_its_content_column(&db.conn, doc);

        KernelDb::apply_additive_migrations(&db.conn).unwrap();
        KernelDb::apply_additive_migrations(&db.conn).unwrap();
        KernelDb::apply_additive_migrations(&db.conn).unwrap();
    }

    /// Bring `contexts.consent_mode` and `presets.consent_mode` back, with a
    /// non-default value on each row, so the next open has something real to
    /// migrate.
    fn put_consent_mode_columns_back(conn: &Connection, ctx: ContextId, preset: PresetId) {
        conn.execute(
            "ALTER TABLE contexts ADD COLUMN consent_mode TEXT NOT NULL DEFAULT 'collaborative'",
            [],
        )
        .unwrap();
        conn.execute(
            "ALTER TABLE presets ADD COLUMN consent_mode TEXT NOT NULL DEFAULT 'collaborative'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE contexts SET consent_mode = 'autonomous' WHERE context_id = ?1",
            params![blob_param(ctx.as_bytes())],
        )
        .unwrap();
        conn.execute(
            "UPDATE presets SET consent_mode = 'autonomous' WHERE preset_id = ?1",
            params![blob_param(preset.as_bytes())],
        )
        .unwrap();
    }

    /// A database still carrying `contexts.consent_mode` and
    /// `presets.consent_mode` loses both columns on the next open, and the
    /// rows they held keep every other value. Nothing reads the field any
    /// more — the migration exists only to leave an old database loadable.
    #[test]
    fn the_consent_mode_columns_are_dropped_and_rows_survive() {
        let db = KernelDb::temporary().unwrap();
        let ws = setup_test_db(&db);
        let row = make_context_row(Some("consent-drop-ctx"));
        let ctx = row.context_id;
        insert_context_with_doc(&db, &row, ws);
        let preset = insert_test_preset(&db, "consent-drop-preset");

        put_consent_mode_columns_back(&db.conn, ctx, preset);

        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        let contexts_sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'contexts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !contexts_sql.contains("consent_mode"),
            "contexts.consent_mode must be gone, got: {contexts_sql}"
        );
        let presets_sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'presets'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !presets_sql.contains("consent_mode"),
            "presets.consent_mode must be gone, got: {presets_sql}"
        );

        let loaded = db.get_context(ctx).unwrap().expect("the context row must survive the drop");
        assert_eq!(loaded.label, Some("consent-drop-ctx".to_string()));
        let loaded_preset =
            db.get_preset(preset).unwrap().expect("the preset row must survive the drop");
        assert_eq!(loaded_preset.label, "consent-drop-preset");
    }

    /// The drop is guarded, so a second open does not re-run it — an
    /// unguarded `ALTER TABLE ... DROP COLUMN` on a column that is already
    /// gone is an error, not a no-op.
    #[test]
    fn dropping_the_consent_mode_columns_is_idempotent() {
        let db = KernelDb::temporary().unwrap();
        let ws = setup_test_db(&db);
        let row = make_context_row(Some("consent-drop-idempotent"));
        let ctx = row.context_id;
        insert_context_with_doc(&db, &row, ws);
        let preset = insert_test_preset(&db, "consent-drop-idempotent-preset");
        put_consent_mode_columns_back(&db.conn, ctx, preset);

        KernelDb::apply_additive_migrations(&db.conn).unwrap();
        KernelDb::apply_additive_migrations(&db.conn).unwrap();
        KernelDb::apply_additive_migrations(&db.conn).unwrap();
    }

    /// Every rebuild in `rebuild_value_enum_tables` carries its table's
    /// shape a second time, beside `SCHEMA`'s own copy of it. A rebuilt
    /// table must therefore end up with the shape a fresh database gets:
    /// the same columns, types, nullability, defaults and primary key, and
    /// the same foreign keys. An edit to `SCHEMA` that forgets its rebuild
    /// spec fails here, instead of silently dropping a column the next time
    /// an older kernel opens its database.
    #[test]
    fn a_rebuilt_table_has_the_shape_a_fresh_database_gets() {
        fn columns(conn: &Connection, table: &str) -> Vec<String> {
            conn.prepare(&format!("PRAGMA table_info({table})"))
                .unwrap()
                .query_map([], |row| {
                    Ok(format!(
                        "{}|{}|{}|{}|{}",
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                        row.get::<_, i64>(5)?,
                    ))
                })
                .unwrap()
                .collect::<SqliteResult<Vec<_>>>()
                .unwrap()
        }
        fn foreign_keys(conn: &Connection, table: &str) -> Vec<String> {
            conn.prepare(&format!("PRAGMA foreign_key_list({table})"))
                .unwrap()
                .query_map([], |row| {
                    Ok(format!(
                        "{}|{}|{}|{}",
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                        row.get::<_, String>(6)?,
                    ))
                })
                .unwrap()
                .collect::<SqliteResult<Vec<_>>>()
                .unwrap()
        }

        let fresh = KernelDb::temporary().unwrap();
        let rebuilt = KernelDb::temporary().unwrap();
        put_backends_back_on_its_pre_codex_shape(&rebuilt.conn);
        put_roster_entity_back_on_its_legacy_shape(&rebuilt.conn);
        put_roster_presence_back_on_its_legacy_shape(&rebuilt.conn);
        put_roster_activity_back_on_its_legacy_shape(&rebuilt.conn);
        put_roster_status_back_on_its_legacy_shape(&rebuilt.conn);

        KernelDb::apply_additive_migrations(&rebuilt.conn).unwrap();

        for table in
            ["backends", "roster_entity", "roster_presence", "roster_activity", "roster_status"]
        {
            assert_eq!(
                columns(&rebuilt.conn, table),
                columns(&fresh.conn, table),
                "{table}'s rebuild spec has drifted from SCHEMA"
            );
            assert_eq!(
                foreign_keys(&rebuilt.conn, table),
                foreign_keys(&fresh.conn, table),
                "{table}'s rebuild spec lost or changed a foreign key"
            );
        }
    }

    /// The joinability guarantee at the schema level: an `entity_kind =
    /// 'context'` row must name a context this kernel actually minted. A
    /// phantom id (never inserted into `contexts`) is refused outright.
    #[test]
    fn roster_entity_context_must_exist_trigger_refuses_a_phantom_context() {
        let db = KernelDb::temporary().unwrap();
        let _ws_id = setup_test_db(&db);
        let phantom = ContextId::new();
        let err = db
            .conn
            .execute(
                "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
                 VALUES ('context', ?1, 1)",
                params![blob_param(phantom.as_bytes())],
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("never minted"),
            "expected the joinability trigger's message, got: {err}"
        );
    }

    /// A `principal` entity has no backing table to check against — the
    /// trigger's `WHEN` is scoped to `entity_kind = 'context'` only, so an
    /// arbitrary principal id must insert cleanly (documented in
    /// `roster.rs`'s module doc as Rust-API-only discipline for that half).
    #[test]
    fn roster_entity_principal_kind_is_not_checked_against_any_table() {
        let db = KernelDb::temporary().unwrap();
        db.conn
            .execute(
                "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
                 VALUES ('principal', ?1, 1)",
                params![blob_param(PrincipalId::new().as_bytes())],
            )
            .unwrap();
    }

    // ── 31. Well-known context registry (docs/drifting-dead-letters.md, slice 1) ──

    /// `temporary()` must open a real file on disk, not `:memory:` — and that
    /// file (and the directory holding it) must be gone once nothing holds
    /// the `KernelDb` anymore. Asserted against the filesystem directly, not
    /// against any in-process bookkeeping.
    #[test]
    fn temporary_writes_a_real_file_and_removes_it_on_drop() {
        let db = KernelDb::temporary().unwrap();
        let dir = db._temp_dir.as_ref().unwrap().path().to_path_buf();
        let db_path = dir.join("kernel.db");
        assert!(db_path.is_file(), "temporary() must back its handle with a real file");

        drop(db);

        assert!(
            !dir.exists(),
            "the temp directory must be removed once the KernelDb handle drops"
        );
    }

    // ========================================================================
    // insert_document_with_op — the row and the first op commit together or
    // not at all. A row that commits alone is a document with no snapshot and
    // no oplog: it loads empty and every read of it fails.
    // ========================================================================

    // ========================================================================
    // `context_state` and `archived_at` are written together. Before this,
    // archiving stamped only the timestamp, so every archived context read
    // back as `live` and the enum lied to anyone who trusted it alone.
    // ========================================================================

}

