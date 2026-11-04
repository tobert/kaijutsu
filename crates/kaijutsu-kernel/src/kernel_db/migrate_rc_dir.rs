//! Migration `rc-dir-backfill-v1`: every character gets the rc directory its
//! contexts run, and every root its console pointer, where the database can
//! say which (`docs/character.md`, "Migrations").

use super::*;

/// A character as the backfill reads it.
struct BackfillCharacter {
    id: PrincipalId,
    name: String,
    root: bool,
    context_type: Option<String>,
    rc_dir: Option<String>,
    root_ctx: Option<ContextId>,
    console_ctx: Option<ContextId>,
}

impl KernelDb {
    pub(super) const RC_DIR_BACKFILL_MIGRATION: &'static str = "rc-dir-backfill-v1";

    /// Where a database is copied before `rc-dir-backfill-v1` changes it:
    /// `kernel.db.pre-rc-dir`.
    pub(super) fn pre_rc_dir_backup_path(path: &Path) -> std::path::PathBuf {
        let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(".pre-rc-dir");
        path.with_file_name(name)
    }

    /// Copy a database that `rc-dir-backfill-v1` is about to change aside
    /// first. A database needs the copy when the migration is not recorded and
    /// would change a row: a character with no `rc_dir`, or a root with no
    /// `console_ctx` that has a live console to point at. The copy is a
    /// consistent snapshot; an existing copy stays as it is, and a copy that
    /// cannot be made fails the open.
    pub(super) fn back_up_before_rc_dir_backfill(conn: &Connection, path: &Path) -> KernelDbResult<()> {
        if !Self::has_table(conn, "characters")? {
            return Ok(());
        }
        if Self::has_table(conn, "kernel_migrations")? {
            let done: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
                [Self::RC_DIR_BACKFILL_MIGRATION],
                |r| r.get(0),
            )?;
            if done {
                return Ok(());
            }
        }
        // An older shape lacks the columns the checks read; it has rows the
        // backfill will change, so it is copied.
        let any: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM characters)", [], |r| r.get(0))?;
        if !any {
            return Ok(());
        }
        let current_shape = Self::has_column(conn, "characters", "rc_dir")?
            && Self::has_column(conn, "characters", "console_ctx")?
            && Self::has_column(conn, "characters", "root_ctx")?
            && Self::has_column(conn, "contexts", "context_type")?
            && Self::has_column(conn, "contexts", "senpai_ctx")?
            && Self::has_column(conn, "contexts", "archived_at")?;
        if current_shape {
            let changes: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM characters WHERE rc_dir IS NULL OR rc_dir = '')
                    OR EXISTS(SELECT 1 FROM characters c
                              WHERE c.root AND c.console_ctx IS NULL AND c.root_ctx IS NOT NULL
                                AND EXISTS(SELECT 1 FROM contexts x
                                           WHERE x.senpai_ctx = c.root_ctx AND x.context_type = ?1
                                             AND x.archived_at IS NULL))",
                [kaijutsu_types::CONSOLE_CONTEXT_TYPE],
                |r| r.get(0),
            )?;
            if !changes {
                return Ok(());
            }
        }
        Self::back_up_aside(conn, &Self::pre_rc_dir_backup_path(path), "the characters take their rc directories")
    }

    /// Give each character with no `rc_dir` one, once, in one transaction.
    ///
    /// A root character gets `root`. A character with a `context_type` gets
    /// that value. Any other gets the `context_type` most of its live
    /// contexts have, not counting `judge` contexts: before `slot-roles-v1`
    /// a seat's character performed its judge shadows, which ran `judge/`.
    /// A character with no live vote takes the vote of
    /// all its contexts, archived included (an MCP lead's sessions archive
    /// when their process ends). A tie or no vote leaves `rc_dir` NULL; each
    /// such character is listed in `context_character_migration_report`
    /// (kind `rc_dir_unset`, naming the character by `principal_id`) and
    /// logged, and a create for it refuses until `kj character set <name>
    /// --rc <dir>` names one. An empty `rc_dir` counts as unset.
    ///
    /// A database without `contexts.context_type` or `characters.context_type`
    /// has nothing to vote with: the migration records itself and changes
    /// nothing.
    ///
    /// The same pass points a root with no `console_ctx` at its console when
    /// exactly one live `console` context answers to the root's root console.
    /// Several leave the pointer NULL, and each is listed (kind
    /// `console_ambiguous`) and logged. The migration never refuses.
    pub(super) fn migrate_rc_dir_backfill(conn: &Connection) -> KernelDbResult<()> {
        let done: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
            [Self::RC_DIR_BACKFILL_MIGRATION],
            |r| r.get(0),
        )?;
        if done {
            return Ok(());
        }
        if !Self::has_column(conn, "contexts", "context_type")? || !Self::has_column(conn, "characters", "context_type")? {
            conn.execute("INSERT INTO kernel_migrations (name) VALUES (?1)", [Self::RC_DIR_BACKFILL_MIGRATION])?;
            return Ok(());
        }
        let tx = conn.unchecked_transaction()?;
        let characters: Vec<BackfillCharacter> = {
            let mut stmt = tx.prepare(
                "SELECT principal_id, name, root, context_type, rc_dir, root_ctx, console_ctx
                 FROM characters ORDER BY name",
            )?;
            stmt.query_map([], |r| {
                Ok(BackfillCharacter {
                    id: read_principal_id(r, 0)?,
                    name: r.get(1)?,
                    root: r.get(2)?,
                    context_type: r.get(3)?,
                    rc_dir: r.get(4)?,
                    root_ctx: read_opt_context_id(r, 5)?,
                    console_ctx: read_opt_context_id(r, 6)?,
                })
            })?
            .collect::<SqliteResult<_>>()?
        };
        let recorded_at = now_millis();
        let report = |kind: &str, context: Option<ContextId>, principal: Option<PrincipalId>, note: String| -> KernelDbResult<()> {
            tx.execute(
                "INSERT INTO context_character_migration_report (kind, context_id, principal_id, note, recorded_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    kind,
                    context.map(|c| c.as_bytes().to_vec()),
                    principal.map(|p| p.as_bytes().to_vec()),
                    note,
                    recorded_at
                ],
            )?;
            Ok(())
        };
        let mut assigned = 0usize;
        let mut unset: Vec<String> = Vec::new();
        for c in &characters {
            if c.rc_dir.as_deref().is_some_and(|dir| !dir.is_empty()) {
                continue;
            }
            let chosen = if c.root {
                Ok(kaijutsu_types::ROOT_CONTEXT_TYPE.to_string())
            } else if let Some(context_type) = &c.context_type {
                Ok(context_type.clone())
            } else {
                // Live contexts vote; with none, every context does.
                let votes = |live_only: bool| -> KernelDbResult<Vec<(String, i64)>> {
                    let archived = if live_only { "AND archived_at IS NULL" } else { "" };
                    let mut stmt = tx.prepare(&format!(
                        "SELECT context_type, COUNT(*) FROM contexts
                         WHERE performer = ?1 {archived} AND context_type != ?2
                         GROUP BY context_type ORDER BY COUNT(*) DESC, context_type"
                    ))?;
                    let rows = stmt
                        .query_map(params![blob_param(c.id.as_bytes()), crate::council::shadow::JUDGE], |r| {
                            Ok((r.get(0)?, r.get(1)?))
                        })?
                        .collect::<SqliteResult<_>>()?;
                    Ok(rows)
                };
                let (counts, which) = match votes(true)? {
                    live if !live.is_empty() => (live, "live contexts"),
                    _ => (votes(false)?, "contexts (none is live)"),
                };
                match counts.as_slice() {
                    [] => {
                        let judges: bool = tx.query_row(
                            "SELECT EXISTS(SELECT 1 FROM contexts WHERE performer = ?1)",
                            params![blob_param(c.id.as_bytes())],
                            |r| r.get(0),
                        )?;
                        if judges {
                            Err(format!(
                                "{} has no context type and plays only judge contexts, which cast no vote; \
                                 `kj character set {} --rc <dir>` names its rc directory",
                                c.name, c.name
                            ))
                        } else {
                            Err(format!(
                                "{} has no context type and plays no context; `kj character set {} --rc <dir>` names its rc directory",
                                c.name, c.name
                            ))
                        }
                    }
                    [(top, n), (next, m), ..] if n == m => Err(format!(
                        "{}'s {which} are as often '{top}' as '{next}' ({n} each), so the migration did not choose; \
                         `kj character set {} --rc <dir>` names its rc directory",
                        c.name, c.name
                    )),
                    [(top, _), ..] => Ok(top.clone()),
                }
            };
            match chosen {
                Ok(dir) => {
                    tx.execute(
                        "UPDATE characters SET rc_dir = ?1 WHERE principal_id = ?2",
                        params![dir, blob_param(c.id.as_bytes())],
                    )?;
                    assigned += 1;
                }
                Err(note) => {
                    warn!(character = %c.name, "{note}");
                    report("rc_dir_unset", None, Some(c.id), note)?;
                    unset.push(c.name.clone());
                }
            }
        }

        let mut consoles_set = 0usize;
        for c in characters.iter().filter(|c| c.root && c.console_ctx.is_none()) {
            let Some(root_ctx) = c.root_ctx else { continue };
            let consoles: Vec<(ContextId, Option<String>)> = {
                let mut stmt = tx.prepare(
                    "SELECT context_id, label FROM contexts
                     WHERE senpai_ctx = ?1 AND context_type = ?2 AND archived_at IS NULL
                     ORDER BY created_at, context_id",
                )?;
                stmt.query_map(
                    params![blob_param(root_ctx.as_bytes()), kaijutsu_types::CONSOLE_CONTEXT_TYPE],
                    |r| Ok((read_context_id(r, 0)?, r.get(1)?)),
                )?
                .collect::<SqliteResult<_>>()?
            };
            match consoles.as_slice() {
                [] => {}
                [(only, _)] => {
                    tx.execute(
                        "UPDATE characters SET console_ctx = ?1 WHERE principal_id = ?2",
                        params![blob_param(only.as_bytes()), blob_param(c.id.as_bytes())],
                    )?;
                    consoles_set += 1;
                }
                several => {
                    let named: Vec<String> = several
                        .iter()
                        .map(|(id, label)| label.as_ref().map_or_else(|| id.short(), |l| format!("{l} ({})", id.short())))
                        .collect();
                    let note = format!(
                        "{} live console contexts answer to {}'s root console ({}), so its console pointer stays unset; \
                         `kj character set {} --console <ctx>` points it at one",
                        several.len(),
                        c.name,
                        named.join(", "),
                        c.name
                    );
                    warn!(character = %c.name, "{note}");
                    for (id, _) in several {
                        report("console_ambiguous", Some(*id), None, note.clone())?;
                    }
                }
            }
        }

        tx.execute("INSERT INTO kernel_migrations (name) VALUES (?1)", [Self::RC_DIR_BACKFILL_MIGRATION])?;
        tx.commit()?;
        if assigned > 0 || consoles_set > 0 || !unset.is_empty() {
            info!(
                assigned,
                consoles_set,
                unset = unset.len(),
                "backfilled the characters' rc directories and console pointers"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
