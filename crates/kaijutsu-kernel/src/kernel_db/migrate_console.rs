//! Migration `console-label-v1`: the model console's label `ROOT` becomes
//! `console` (`docs/senpai-kohai.md`, "Names").

use super::*;

impl KernelDb {
    pub(super) const CONSOLE_LABEL_MIGRATION: &'static str = "console-label-v1";

    /// Relabel the live `console` context labeled `ROOT` to `console`, once.
    ///
    /// A live `ROOT` of another type is a different context that happens to
    /// carry the word: it keeps its label and is listed in
    /// `context_character_migration_report` (kind `root_label_kept`). When a
    /// live context already holds `console`, the relabel would be ambiguous,
    /// so the open refuses, names both, and changes nothing.
    pub(super) fn migrate_console_label(conn: &Connection) -> KernelDbResult<()> {
        let done: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
            [Self::CONSOLE_LABEL_MIGRATION],
            |r| r.get(0),
        )?;
        if done {
            return Ok(());
        }
        let tx = conn.unchecked_transaction()?;
        let live_root: Vec<(ContextId, String)> = {
            let mut stmt = tx.prepare(
                "SELECT context_id, context_type FROM contexts WHERE label = 'ROOT' AND archived_at IS NULL ORDER BY created_at, context_id",
            )?;
            stmt.query_map([], |r| Ok((read_context_id(r, 0)?, r.get::<_, String>(1)?)))?
                .collect::<SqliteResult<_>>()?
        };
        let (consoles, others): (Vec<_>, Vec<_>) =
            live_root.into_iter().partition(|(_, t)| t == kaijutsu_types::CONSOLE_CONTEXT_TYPE);
        if let Some((console, _)) = consoles.first() {
            let holder: Option<ContextId> = tx
                .query_row(
                    "SELECT context_id FROM contexts WHERE label = 'console' AND archived_at IS NULL",
                    [],
                    |r| read_context_id(r, 0),
                )
                .optional()?;
            if let Some(holder) = holder {
                return Err(KernelDbError::Validation(format!(
                    "console-label-v1 cannot relabel the console ROOT ({}) to console: live context {} already has that label. \
                     Archive or relabel one of them, then start again; nothing was changed",
                    console.short(),
                    holder.short()
                )));
            }
            tx.execute(
                "UPDATE contexts SET label = 'console' WHERE context_id = ?1",
                params![blob_param(console.as_bytes())],
            )?;
        }
        let recorded_at = now_millis();
        for (ctx, context_type) in &others {
            tx.execute(
                "INSERT INTO context_character_migration_report (kind, context_id, note, recorded_at) VALUES (?1, ?2, ?3, ?4)",
                params![
                    "root_label_kept",
                    blob_param(ctx.as_bytes()),
                    format!("labeled ROOT but a '{context_type}' context, not the console; the label stays"),
                    recorded_at
                ],
            )?;
        }
        tx.execute("INSERT INTO kernel_migrations (name) VALUES (?1)", [Self::CONSOLE_LABEL_MIGRATION])?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
