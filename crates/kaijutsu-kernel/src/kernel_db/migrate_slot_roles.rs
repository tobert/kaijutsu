//! Migration `slot-roles-v1`: a context's chat model moves to role `model`,
//! a character keeps only its `distill` and `gate` slots, and live judge
//! shadows are archived so the next gate call makes one performed by the
//! judge character (`docs/character.md`, "Migrations").

use super::*;

/// A character slot the migration reads.
struct OldCharacterSlot {
    principal_id: PrincipalId,
    character: String,
    role: String,
    backend: String,
    model: String,
}

impl KernelDb {
    pub(super) const SLOT_ROLES_MIGRATION: &'static str = "slot-roles-v1";

    /// Where a database is copied before `slot-roles-v1` changes it:
    /// `kernel.db.pre-slot-roles`.
    pub(super) fn pre_slot_roles_backup_path(path: &Path) -> std::path::PathBuf {
        let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(".pre-slot-roles");
        path.with_file_name(name)
    }

    /// Copy a database that `slot-roles-v1` is about to change aside first. A
    /// database needs the copy when the migration is not recorded and would
    /// change a row: a context slot whose role is not `model`, `distill` or
    /// `gate`, a character slot whose role is not `distill` or `gate`, or a
    /// live judge shadow. An older shape, which lacks the tables or columns
    /// the checks read and has contexts the earlier migrations will give
    /// type-keyed slots, is copied. The copy is a consistent snapshot; an
    /// existing copy stays as it is, and a copy that cannot be made fails the
    /// open.
    pub(super) fn back_up_before_slot_roles(conn: &Connection, path: &Path) -> KernelDbResult<()> {
        if !Self::has_table(conn, "contexts")? {
            return Ok(());
        }
        if Self::has_table(conn, "kernel_migrations")? {
            let done: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
                [Self::SLOT_ROLES_MIGRATION],
                |r| r.get(0),
            )?;
            if done {
                return Ok(());
            }
        }
        let current_shape = Self::has_table(conn, "context_slots")?
            && Self::has_table(conn, "character_slots")?
            && Self::has_column(conn, "contexts", "archived_at")?;
        let changes: bool = if current_shape {
            let judges = if Self::has_column(conn, "contexts", "context_type")? {
                "OR EXISTS(SELECT 1 FROM contexts WHERE context_type = 'judge' AND archived_at IS NULL)"
            } else {
                ""
            };
            conn.query_row(
                &format!(
                    "SELECT EXISTS(SELECT 1 FROM context_slots WHERE role NOT IN ('model', 'distill', 'gate'))
                        OR EXISTS(SELECT 1 FROM character_slots WHERE role NOT IN ('distill', 'gate'))
                        {judges}"
                ),
                [],
                |r| r.get(0),
            )?
        } else {
            conn.query_row("SELECT EXISTS(SELECT 1 FROM contexts)", [], |r| r.get(0))?
        };
        if !changes {
            return Ok(());
        }
        Self::back_up_aside(conn, &Self::pre_slot_roles_backup_path(path), "the slots move to their roles")
    }

    /// Move every slot to its role, once, in one transaction.
    ///
    /// - In `context_slots`, the row whose role is its context's
    ///   `context_type` becomes role `model`. Every row whose role is not
    ///   `model`, `distill` or `gate` is then deleted, `judge` included. The
    ///   never-changed triggers are dropped and recreated inside the
    ///   transaction.
    /// - In `character_slots`, a character with no `character_models` row
    ///   and a slot whose role is its own `characters.context_type` takes
    ///   that slot as its model (a root character runs none). Every role
    ///   other than `distill` and `gate` is then deleted, and each is listed
    ///   in `context_character_migration_report` (kind `slot_dropped`, naming
    ///   the character by `principal_id`) and logged.
    /// - Live contexts of type `judge` are archived; the next gate call makes
    ///   a shadow performed by the judge character.
    ///
    /// A database without `contexts.context_type` or
    /// `characters.context_type` skips the steps that read it. The migration
    /// never refuses over data.
    pub(super) fn migrate_slot_roles(conn: &Connection) -> KernelDbResult<()> {
        let done: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
            [Self::SLOT_ROLES_MIGRATION],
            |r| r.get(0),
        )?;
        if done {
            return Ok(());
        }
        let contexts_typed = Self::has_column(conn, "contexts", "context_type")?;
        let characters_typed = Self::has_column(conn, "characters", "context_type")?;
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(
            "DROP TRIGGER IF EXISTS context_slots_never_changed_u;
             DROP TRIGGER IF EXISTS context_slots_never_changed_d;",
        )?;

        let renamed = if contexts_typed {
            tx.execute(
                "UPDATE context_slots SET role = 'model'
                 WHERE role = (SELECT c.context_type FROM contexts c WHERE c.context_id = context_slots.context_id)
                   AND role NOT IN ('model', 'distill', 'gate')
                   AND NOT EXISTS (SELECT 1 FROM context_slots m
                                   WHERE m.context_id = context_slots.context_id AND m.role = 'model')",
                [],
            )?
        } else {
            0
        };
        let context_dropped =
            tx.execute("DELETE FROM context_slots WHERE role NOT IN ('model', 'distill', 'gate')", [])?;

        // A character's slot for its own type becomes its model and leaves
        // the slots, so it is not reported as dropped.
        let adopt: Vec<(PrincipalId, String)> = if characters_typed {
            let mut stmt = tx.prepare(
                "SELECT s.principal_id, s.role
                 FROM character_slots s JOIN characters c ON c.principal_id = s.principal_id
                 WHERE s.role = c.context_type AND NOT c.root
                   AND s.role NOT IN ('distill', 'gate')
                   AND NOT EXISTS (SELECT 1 FROM character_models m WHERE m.principal_id = s.principal_id)",
            )?;
            stmt.query_map([], |r| Ok((read_principal_id(r, 0)?, r.get(1)?)))?.collect::<SqliteResult<_>>()?
        } else {
            Vec::new()
        };
        for (principal_id, role) in &adopt {
            let who = blob_param(principal_id.as_bytes());
            tx.execute(
                "INSERT INTO character_models (principal_id, backend_id, model, max_tokens, temperature, top_p,
                                               effort, thinking_budget, thinking_style)
                 SELECT principal_id, backend_id, model, max_tokens, temperature, top_p,
                        effort, thinking_budget, thinking_style
                 FROM character_slots WHERE principal_id = ?1 AND role = ?2",
                params![who, role],
            )?;
            tx.execute("DELETE FROM character_slots WHERE principal_id = ?1 AND role = ?2", params![who, role])?;
        }
        let adopted = adopt.len();
        let stray: Vec<OldCharacterSlot> = {
            let mut stmt = tx.prepare(
                "SELECT s.principal_id, COALESCE(c.name, ''), s.role, COALESCE(b.name, '(missing backend)'), s.model
                 FROM character_slots s
                 LEFT JOIN characters c ON c.principal_id = s.principal_id
                 LEFT JOIN backends b ON b.backend_id = s.backend_id
                 WHERE s.role NOT IN ('distill', 'gate')
                 ORDER BY c.name, s.role",
            )?;
            stmt.query_map([], |r| {
                Ok(OldCharacterSlot {
                    principal_id: read_principal_id(r, 0)?,
                    character: r.get(1)?,
                    role: r.get(2)?,
                    backend: r.get(3)?,
                    model: r.get(4)?,
                })
            })?
            .collect::<SqliteResult<_>>()?
        };
        let recorded_at = now_millis();
        for slot in &stray {
            let note = format!(
                "dropped {}'s `{}` slot ({}/{}): a character has one model and the slots `distill` and `gate`; \
                 `kj character model set {} <backend>/<model>` sets its model",
                slot.character, slot.role, slot.backend, slot.model, slot.character
            );
            warn!(character = %slot.character, role = %slot.role, "{note}");
            tx.execute(
                "INSERT INTO context_character_migration_report (kind, context_id, principal_id, note, recorded_at)
                 VALUES ('slot_dropped', NULL, ?1, ?2, ?3)",
                params![blob_param(slot.principal_id.as_bytes()), note, recorded_at],
            )?;
        }
        tx.execute("DELETE FROM character_slots WHERE role NOT IN ('distill', 'gate')", [])?;

        // A judge shadow raises no asks and holds no drifts, so archiving it
        // here sweeps nothing.
        let shadows = if contexts_typed {
            tx.execute(
                "UPDATE contexts SET archived_at = ?1, context_state = 'archived', promoted_at = NULL, demoted_at = NULL
                 WHERE context_type = 'judge' AND archived_at IS NULL",
                params![recorded_at],
            )?
        } else {
            0
        };

        tx.execute_batch(super::context_characters::CONTEXT_SLOTS_NEVER_CHANGED)?;
        tx.execute("INSERT INTO kernel_migrations (name) VALUES (?1)", [Self::SLOT_ROLES_MIGRATION])?;
        tx.commit()?;
        if renamed > 0 || context_dropped > 0 || adopted > 0 || !stray.is_empty() || shadows > 0 {
            info!(
                renamed,
                context_dropped,
                adopted,
                character_dropped = stray.len(),
                shadows_archived = shadows,
                "moved the slots to their roles"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
