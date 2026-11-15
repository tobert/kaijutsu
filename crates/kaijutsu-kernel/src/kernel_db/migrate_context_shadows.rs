//! Migration `context-shadows-v1`: the `judge_shadows` marker table becomes
//! `context_shadows`, and each of its rows becomes a shadow of kind `judge`
//! (`docs/shadows.md`).

use super::*;

impl KernelDb {
    pub(super) const CONTEXT_SHADOWS_MIGRATION: &'static str = "context-shadows-v1";

    /// Copy the rows of an old `judge_shadows` table into `context_shadows`
    /// as kind `judge` and drop the old table, once, in one transaction with
    /// the record. A database with no old table records the migration with
    /// nothing to copy.
    pub(super) fn migrate_context_shadows(conn: &Connection) -> KernelDbResult<()> {
        let done: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
            [Self::CONTEXT_SHADOWS_MIGRATION],
            |r| r.get(0),
        )?;
        if done {
            return Ok(());
        }
        let tx = conn.unchecked_transaction()?;
        let mut copied = 0;
        if Self::has_table(&tx, "judge_shadows")? {
            copied = tx.execute(
                "INSERT OR IGNORE INTO context_shadows (context_id, kind)
                 SELECT context_id, 'judge' FROM judge_shadows",
                [],
            )?;
            tx.execute("DROP TABLE judge_shadows", [])?;
        }
        tx.execute("INSERT INTO kernel_migrations (name) VALUES (?1)", [Self::CONTEXT_SHADOWS_MIGRATION])?;
        tx.commit()?;
        if copied > 0 {
            info!(shadows = copied, "moved the judge shadow markers into context_shadows");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
