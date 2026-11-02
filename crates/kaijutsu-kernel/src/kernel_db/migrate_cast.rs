//! Legacy cast tables and their migrations to character slots and narrowed presets.

use super::*;

impl KernelDb {
    /// The pre-character-slot cast tables, created empty when a database old
    /// enough to lack them is about to be read by the migrations that carry
    /// casts forward. Only called on a database that still has
    /// `contexts.provider`.
    pub(super) fn ensure_legacy_cast_tables(conn: &Connection) -> KernelDbResult<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS casts (
                cast_id     BLOB NOT NULL PRIMARY KEY,
                label       TEXT NOT NULL UNIQUE COLLATE NOCASE,
                description TEXT,
                created_at  INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
                created_by  BLOB NOT NULL
            );
            CREATE TABLE IF NOT EXISTS cast_slots (
                cast_id         BLOB NOT NULL REFERENCES casts(cast_id) ON DELETE CASCADE,
                role            TEXT NOT NULL,
                backend_id      BLOB NOT NULL REFERENCES backends(backend_id),
                model           TEXT NOT NULL CHECK (model != ''),
                max_tokens      INTEGER CHECK (max_tokens IS NULL OR max_tokens > 0),
                temperature     REAL    CHECK (temperature IS NULL OR (temperature >= 0.0 AND temperature <= 2.0)),
                top_p           REAL    CHECK (top_p IS NULL OR (top_p > 0.0 AND top_p <= 1.0)),
                effort          TEXT,
                thinking_budget INTEGER CHECK (thinking_budget IS NULL OR thinking_budget > 0),
                thinking_style  TEXT,
                loadout         TEXT,
                extra           TEXT,
                PRIMARY KEY (cast_id, role)
            );",
        )?;
        Ok(())
    }

    /// Move each character's cast into `character_slots` and drop the casts
    /// (`docs/senpai-kohai.md`, "Migration", step 1), in one transaction.
    ///
    /// A character gets a copy of the slots of the cast it names; a character
    /// with no cast gets none. Every value that does not carry over is logged
    /// before it goes: a cast no character names, with its slots, and a
    /// slot's `loadout` and `extra`, which had no reader. Then
    /// `characters.cast_id`, `cast_slots` and `casts` are dropped. A database
    /// with neither cast table is on the new shape and does nothing, and the
    /// presence of the tables is the whole state: the move writes no marker. One that
    /// still has `characters.default_cast_id` has not been through
    /// `context-characters-v1` and refuses, because that migration reads the
    /// casts.
    pub(super) fn migrate_character_slots(conn: &Connection) -> KernelDbResult<()> {
        if !Self::has_table(conn, "casts")? && !Self::has_table(conn, "cast_slots")? {
            return Ok(());
        }
        if Self::has_column(conn, "characters", "default_cast_id")? || Self::has_column(conn, "contexts", "cast_id")? {
            return Err(KernelDbError::Validation(
                "the casts cannot move to character slots before the context character migration has read them".into(),
            ));
        }
        let tx = conn.unchecked_transaction()?;
        let has_cast_column = Self::has_column(&tx, "characters", "cast_id")?;
        if has_cast_column && Self::has_table(&tx, "cast_slots")? {
            tx.execute(
                "INSERT INTO character_slots
                     (principal_id, role, backend_id, model, max_tokens, temperature, top_p, effort,
                      thinking_budget, thinking_style)
                 SELECT c.principal_id, s.role, s.backend_id, s.model, s.max_tokens, s.temperature, s.top_p,
                        s.effort, s.thinking_budget, s.thinking_style
                 FROM characters c JOIN cast_slots s ON s.cast_id = c.cast_id
                 WHERE true
                 ON CONFLICT(principal_id, role) DO NOTHING",
                [],
            )?;
        }
        if Self::has_table(&tx, "casts")? {
            let named = if has_cast_column {
                "EXISTS (SELECT 1 FROM characters ch WHERE ch.cast_id = c.cast_id)"
            } else {
                "0"
            };
            let mut stmt = tx.prepare(&format!(
                "SELECT c.label, (SELECT COUNT(*) FROM cast_slots s WHERE s.cast_id = c.cast_id), {named} FROM casts c ORDER BY c.label"
            ))?;
            let casts: Vec<(String, i64, bool)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0)))?
                .collect::<SqliteResult<_>>()?;
            for (label, slots, named) in casts {
                if !named {
                    warn!(cast = %label, slots, "dropping a cast no character names, with its slots");
                }
            }
        }
        if Self::has_table(&tx, "cast_slots")? {
            let mut stmt = tx.prepare(
                "SELECT c.label, s.role FROM cast_slots s JOIN casts c ON c.cast_id = s.cast_id
                 WHERE s.loadout IS NOT NULL OR s.extra IS NOT NULL ORDER BY c.label, s.role",
            )?;
            let unread: Vec<(String, String)> =
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<SqliteResult<_>>()?;
            for (label, role) in unread {
                warn!(cast = %label, role = %role, "dropping a slot's loadout and extra, which nothing read");
            }
        }
        if has_cast_column {
            // A character that plays a live context with nothing to copy now
            // has no slots; the first create or rotate for it will refuse
            // naming the role, so say so here, where the deploy happens.
            // The performers are in `context_characters` until
            // `context-fold-v1` moves them onto `contexts`.
            let performs = if Self::has_table(&tx, "context_characters")? {
                "JOIN context_characters cc ON cc.principal_id = ch.principal_id
                 JOIN contexts c ON c.context_id = cc.context_id AND c.archived_at IS NULL"
            } else {
                "JOIN contexts c ON c.performer = ch.principal_id AND c.archived_at IS NULL"
            };
            let mut stmt = tx.prepare(&format!(
                "SELECT DISTINCT ch.name FROM characters ch
                 {performs}
                 WHERE NOT EXISTS (SELECT 1 FROM character_slots s WHERE s.principal_id = ch.principal_id)
                 ORDER BY ch.name"
            ))?;
            let bare: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<SqliteResult<_>>()?;
            drop(stmt);
            for name in bare {
                warn!(character = %name, "plays live contexts and has no slot after the casts moved; `kj character slot set` gives it one before the next create or rotate");
            }
            tx.execute("ALTER TABLE characters DROP COLUMN cast_id", [])?;
        }
        tx.execute("DROP TABLE IF EXISTS cast_slots", [])?;
        tx.execute("DROP TABLE IF EXISTS casts", [])?;
        tx.commit()?;
        info!("moved each character's cast into its own slots and dropped the casts");
        Ok(())
    }

    /// One-shot preset narrowing for Track D (2026-08-03): a preset's old
    /// `provider`/`model` columns are dropped and replaced with a nullable
    /// `cast_id` reference (a preset now assigns a cast rather than pinning a
    /// provider/model pair itself). No attempt is made to guess a cast from
    /// the old provider/model — Amy: "a preset whose provider/model can't map
    /// just loses them (cast_id stays NULL)" — but every preset that's about
    /// to lose a non-null value logs it at `info` first, so the drop is
    /// never silent.
    ///
    /// Column presence is checked via `PRAGMA table_info` rather than the
    /// duplicate-column-error idiom `apply_additive_migrations` uses for
    /// ADD-only alters: this function does three independent operations
    /// (drop, drop, add) and needs to resume correctly even if a prior run
    /// crashed between them, which a single try-and-ignore-the-error pass
    /// over all three wouldn't detect per-column.
    pub(super) fn migrate_preset_cast_narrowing(conn: &Connection) -> KernelDbResult<()> {
        let columns: Vec<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(presets)")?;
            stmt.query_map([], |r| r.get::<_, String>(1))?
                .collect::<SqliteResult<_>>()?
        };
        let has_provider = columns.iter().any(|c| c == "provider");
        let has_model = columns.iter().any(|c| c == "model");

        if has_provider || has_model {
            let provider_expr = if has_provider { "provider" } else { "NULL" };
            let model_expr = if has_model { "model" } else { "NULL" };
            let sql = format!(
                "SELECT label, {provider_expr}, {model_expr} FROM presets \
                 WHERE {provider_expr} IS NOT NULL OR {model_expr} IS NOT NULL"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?;
            for row in rows {
                let (label, provider, model) = row?;
                info!(
                    "preset '{label}': dropping provider={provider:?} model={model:?} \
                     (cast_id left NULL — no automatic mapping)"
                );
            }
        }

        if has_provider {
            conn.execute("ALTER TABLE presets DROP COLUMN provider", [])?;
        }
        if has_model {
            conn.execute("ALTER TABLE presets DROP COLUMN model", [])?;
        }
        Ok(())
    }

    /// Where a database still on casts is copied before `character-slots-v1`
    /// drops them: `kernel.db.pre-character-slots`, beside the database.
    pub(super) fn pre_character_slots_backup_path(path: &Path) -> std::path::PathBuf {
        let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(".pre-character-slots");
        path.with_file_name(name)
    }

    /// Copy a database that still has its casts aside before the migration
    /// that drops them runs. The copy is a consistent snapshot
    /// (`VACUUM INTO`). A copy that already exists is the state before an
    /// earlier attempt and stays as it is. When the copy cannot be made the
    /// open fails and says why, and nothing is migrated.
    pub(super) fn back_up_before_character_slots(conn: &Connection, path: &Path) -> KernelDbResult<()> {
        if !Self::has_table(conn, "casts")? && !Self::has_table(conn, "cast_slots")? {
            return Ok(());
        }
        Self::back_up_aside(
            conn,
            &Self::pre_character_slots_backup_path(path),
            "the casts become character slots",
        )
    }

    /// Copy the database to `backup` (`VACUUM INTO`, a consistent snapshot)
    /// before a migration that drops data runs. A copy that already exists is
    /// the state before an earlier attempt and stays as it is. When the copy
    /// cannot be made the open fails and says why, and nothing is migrated.
    pub(super) fn back_up_aside(conn: &Connection, backup: &Path, because: &str) -> KernelDbResult<()> {
        if backup.exists() {
            info!(backup = %backup.display(), "a pre-migration backup already exists; keeping it");
            return Ok(());
        }
        conn.execute("VACUUM INTO ?1", [backup.to_string_lossy().as_ref()]).map_err(|e| {
            KernelDbError::Validation(format!(
                "{because} on this open, and the copy of the database at {} could not be made: {e}; \
                 free the space or the path, or stop the server that holds the database, and start again",
                backup.display()
            ))
        })?;
        info!(backup = %backup.display(), "copied the database aside before {because}");
        Ok(())
    }
}

#[cfg(test)]
mod tests;
