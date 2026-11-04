//! SQLite persistence for kernel context metadata, edges, presets, and workspaces.
//!
//! Pattern follows `auth_db.rs` and `db.rs`: single `Connection`, WAL mode,
//! BLOB-encoded typed IDs, in-memory constructor for tests.
//!
//! All timestamps are Unix milliseconds (matching `now_millis()`).
//!
//! ## Layout
//!
//! This file holds the handle (`KernelDb`, its error type, open, transactions,
//! maintenance), the id readers and row mappers several tables share, and the
//! module list. Each table group lives in its own child module as an
//! `impl KernelDb` block, with its tests in `<module>/tests.rs`: `contexts`,
//! `context_ring`, `context_fork`, `context_characters`, `review`, `approvals`,
//! `documents`, `lineage`, `llm_backends`, `llm_characters`, `usage`, and the
//! rest below. `schema` holds the DDL; `migrate`, `migrate_senpai`,
//! `migrate_cast`, `migrate_context_characters` and `migrate_fold` hold the
//! migration ladder. Shared test fixtures live in
//! `test_support`.
//!
//! The live handle is a single `Arc<Mutex<KernelDb>>`, so every write
//! serializes on one lock. See `docs/architecture/kernel.md` for the
//! connection-pool angle; revisit it when write contention is measured.

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

mod context_characters;
mod model_prices;
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
mod migrate_context_characters;
mod migrate_fold;
mod migrate_console;
mod migrate_rc_dir;
mod migrate_slot_roles;
mod migrate_cast;
mod migrate;
#[cfg(test)]
mod test_support;
#[cfg(test)]
use test_support::*;
pub use model_prices::{CostGroup, ModelPriceRow, NewModelPrice};
pub use context_characters::{ContextCharacterMigrationNote, ContextCharacterRow, ContextFacts, ContextSlotRow};
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
pub use context_ring::{ACTIVE_RING_CAPACITY, Archived, DemoteOutcome, PromoteOutcome, RotationCommit};
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
    // Connection and open
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
        Self::back_up_before_context_fold(&conn, path)?;
        Self::back_up_before_rc_dir_backfill(&conn, path)?;
        Self::back_up_before_slot_roles(&conn, path)?;
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
    // Transactions and maintenance
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
    // Test hooks
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

}

// ============================================================================
// LLM configuration helpers shared by llm_backends and llm_characters
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
        performer: read_opt_principal_id(row, 18)?,
        senpai_ctx: read_opt_context_id(row, 19)?,
        persona: row.get(20)?,
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
        rc_dir: row.get(8)?,
        console_ctx: read_opt_context_id(row, 9)?,
        mcp_hub_ctx: read_opt_context_id(row, 10)?,
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
// Tests
// ============================================================================

#[cfg(test)]
mod tests;

