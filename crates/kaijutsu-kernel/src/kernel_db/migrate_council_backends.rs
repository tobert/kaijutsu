//! Migration `council-backends-v1`: backend kind `mk` becomes `council`,
//! and the judge shadows it told by their judge's backend get the
//! `judge_shadows` rows that now tell them (`docs/character.md`,
//! "Migrations").
//!
//! Before it, a character whose model was on an `mk` backend was a judge,
//! and its child that answered to its parent was a judge shadow, because a
//! council server was never a chat model. A council model now runs chat
//! turns, so the kernel records a shadow when the council makes it. The
//! backfill applies the old rule once, while it still held: no chat context
//! could run on an `mk` backend before this migration.

use super::*;

impl KernelDb {
    pub(super) const COUNCIL_BACKENDS_MIGRATION: &'static str = "council-backends-v1";

    /// Mark the shadows the old rule told, then rename kind `mk` to
    /// `council`, once, in one transaction with the record. A fresh database
    /// records the migration with nothing to change.
    pub(super) fn migrate_council_backends(conn: &Connection) -> KernelDbResult<()> {
        let done: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
            [Self::COUNCIL_BACKENDS_MIGRATION],
            |r| r.get(0),
        )?;
        if done {
            return Ok(());
        }
        let tx = conn.unchecked_transaction()?;
        let shadows = tx.execute(
            "INSERT OR IGNORE INTO judge_shadows (context_id)
             SELECT c.context_id FROM contexts c
             JOIN character_models m ON m.principal_id = c.performer
             JOIN backends b ON b.backend_id = m.backend_id
             WHERE b.kind = 'mk' AND c.forked_from IS NOT NULL AND c.senpai_ctx = c.forked_from",
            [],
        )?;
        let backends = tx.execute("UPDATE backends SET kind = 'council' WHERE kind = 'mk'", [])?;
        tx.execute("INSERT INTO kernel_migrations (name) VALUES (?1)", [Self::COUNCIL_BACKENDS_MIGRATION])?;
        tx.commit()?;
        if shadows > 0 || backends > 0 {
            info!(backends, shadows, "renamed backend kind mk to council and recorded the judge shadows it told");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
