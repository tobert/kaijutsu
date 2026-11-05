//! Migration `type-melt-v1`: `contexts.context_type` and
//! `characters.context_type` are dropped, with their schema defaults
//! (`docs/character.md`, "Migrations"). What a context does comes from its
//! performer's sheet.

use super::*;

/// The columns `type-melt-v1` drops, as (table, column).
const TYPE_COLUMNS: &[(&str, &str)] = &[("contexts", "context_type"), ("characters", "context_type")];

impl KernelDb {
    pub(super) const TYPE_MELT_MIGRATION: &'static str = "type-melt-v1";

    /// Where a database is copied before `type-melt-v1` changes it:
    /// `kernel.db.pre-type-melt`.
    pub(super) fn pre_type_melt_backup_path(path: &Path) -> std::path::PathBuf {
        let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(".pre-type-melt");
        path.with_file_name(name)
    }

    /// Copy a database that `type-melt-v1` is about to change aside first:
    /// the migration is not recorded and a column it drops is there, or the
    /// approval ledger's `rc_runs.context_type`, which the ledger's own
    /// migration renames in the same open. An existing copy stays as it is,
    /// and a copy that cannot be made fails the open.
    pub(super) fn back_up_before_type_melt(conn: &Connection, path: &Path) -> KernelDbResult<()> {
        if Self::has_table(conn, "kernel_migrations")? {
            let done: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
                [Self::TYPE_MELT_MIGRATION],
                |r| r.get(0),
            )?;
            if done {
                return Ok(());
            }
        }
        let mut changes = false;
        for (table, column) in TYPE_COLUMNS.iter().chain(&[("rc_runs", "context_type")]) {
            changes |= Self::has_table(conn, table)? && Self::has_column(conn, table, column)?;
        }
        if !changes {
            return Ok(());
        }
        Self::back_up_aside(conn, &Self::pre_type_melt_backup_path(path), "the context type columns are dropped")
    }

    /// Drop `contexts.context_type` and `characters.context_type`, once, in
    /// one transaction with the record. Runs after every migration that reads
    /// them. A fresh database has neither and records the migration with
    /// nothing to drop. The columns carry no index, trigger, view or foreign
    /// key, so `ALTER TABLE ... DROP COLUMN` drops each with its default.
    pub(super) fn migrate_type_melt(conn: &Connection) -> KernelDbResult<()> {
        let done: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
            [Self::TYPE_MELT_MIGRATION],
            |r| r.get(0),
        )?;
        if done {
            return Ok(());
        }
        let tx = conn.unchecked_transaction()?;
        let mut dropped = Vec::new();
        for (table, column) in TYPE_COLUMNS {
            if Self::has_column(&tx, table, column)? {
                tx.execute_batch(&format!("ALTER TABLE {table} DROP COLUMN {column}"))?;
                dropped.push(format!("{table}.{column}"));
            }
        }
        tx.execute("INSERT INTO kernel_migrations (name) VALUES (?1)", [Self::TYPE_MELT_MIGRATION])?;
        tx.commit()?;
        if !dropped.is_empty() {
            info!(columns = %dropped.join(", "), "dropped the context type columns; a context's performer says what it does");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
