//! Migration `shell-facade-swap-v1`: `shell` becomes the read-write shell and
//! `shell_readonly` becomes the read-only shell.
//!
//! Persisted contexts that held only `facade:shell` (read-only) move to
//! `facade:shell_readonly`; contexts that held `facade:shell_write` (writable)
//! move to `facade:shell`; contexts that held both drop the redundant
//! read-only grant and keep `facade:shell`. Explicit instance/tool rows and
//! hook matchers follow the same swap, and sticky `context_binding_names`
//! rows for those two instances are cleared so the next tool listing rebinds
//! them cleanly.

use super::*;

impl KernelDb {
    pub(super) const SHELL_FACADE_SWAP_MIGRATION: &'static str = "shell-facade-swap-v1";

    pub(super) fn migrate_shell_facade_swap(conn: &Connection) -> KernelDbResult<()> {
        let done: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
            [Self::SHELL_FACADE_SWAP_MIGRATION],
            |r| r.get(0),
        )?;
        if done {
            return Ok(());
        }
        let tx = conn.unchecked_transaction()?;

        // Facade grants: drop redundant read-only `shell` when `shell_write`
        // is also held, then swap remaining `shell` -> `shell_readonly` and
        // `shell_write` -> `shell`.
        let dropped_dup_facades = tx.execute(
            "DELETE FROM context_binding_facades
             WHERE facade_id = 'shell'
               AND context_id IN (
                   SELECT context_id FROM context_binding_facades WHERE facade_id = 'shell_write'
               )",
            [],
        )?;
        let facades_to_ro = tx.execute(
            "UPDATE context_binding_facades SET facade_id = 'shell_readonly' WHERE facade_id = 'shell'",
            [],
        )?;
        let facades_to_rw = tx.execute(
            "UPDATE context_binding_facades SET facade_id = 'shell' WHERE facade_id = 'shell_write'",
            [],
        )?;

        // Explicit instance grants (uncommon, projected via facade normally).
        tx.execute(
            "DELETE FROM context_binding_instances
             WHERE instance_id = 'builtin.shell'
               AND context_id IN (
                   SELECT context_id FROM context_binding_instances WHERE instance_id = 'builtin.shell_write'
               )",
            [],
        )?;
        tx.execute(
            "UPDATE context_binding_instances SET instance_id = 'builtin.shell_readonly'
             WHERE instance_id = 'builtin.shell'",
            [],
        )?;
        tx.execute(
            "UPDATE context_binding_instances SET instance_id = 'builtin.shell'
             WHERE instance_id = 'builtin.shell_write'",
            [],
        )?;

        // Explicit tool grants.
        tx.execute(
            "DELETE FROM context_binding_tools
             WHERE instance_id = 'builtin.shell' AND original_tool = 'shell'
               AND context_id IN (
                   SELECT context_id FROM context_binding_tools
                   WHERE instance_id = 'builtin.shell_write' AND original_tool = 'shell_write'
               )",
            [],
        )?;
        tx.execute(
            "UPDATE context_binding_tools
             SET instance_id = 'builtin.shell_readonly', original_tool = 'shell_readonly'
             WHERE instance_id = 'builtin.shell' AND original_tool = 'shell'",
            [],
        )?;
        tx.execute(
            "UPDATE context_binding_tools
             SET instance_id = 'builtin.shell', original_tool = 'shell'
             WHERE instance_id = 'builtin.shell_write' AND original_tool = 'shell_write'",
            [],
        )?;

        // Clear sticky visible-name cache entries for the two swapped instances
        // so the next listing rebinds them without collisions.
        tx.execute(
            "DELETE FROM context_binding_names
             WHERE instance_id IN ('builtin.shell', 'builtin.shell_write')",
            [],
        )?;

        // Hook matchers (e.g. `shell-escape-guard` matching `shell_write`).
        tx.execute(
            "UPDATE hooks SET match_instance = 'builtin.shell_readonly'
             WHERE match_instance = 'builtin.shell'",
            [],
        )?;
        tx.execute(
            "UPDATE hooks SET match_instance = 'builtin.shell'
             WHERE match_instance = 'builtin.shell_write'",
            [],
        )?;
        let hooks_to_ro = tx.execute(
            "UPDATE hooks SET match_tool = 'shell_readonly' WHERE match_tool = 'shell'",
            [],
        )?;
        let hooks_to_rw = tx.execute(
            "UPDATE hooks SET match_tool = 'shell' WHERE match_tool = 'shell_write'",
            [],
        )?;

        tx.execute(
            "INSERT INTO kernel_migrations (name) VALUES (?1)",
            [Self::SHELL_FACADE_SWAP_MIGRATION],
        )?;
        tx.commit()?;

        let changed = dropped_dup_facades + facades_to_ro + facades_to_rw + hooks_to_ro + hooks_to_rw;
        if changed > 0 {
            info!(
                dropped_dup_facades,
                facades_to_ro,
                facades_to_rw,
                hooks_to_ro,
                hooks_to_rw,
                "migrated shell facades and hooks to shell / shell_readonly"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
