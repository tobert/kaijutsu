//! Forked contexts: config, binding, and character copies.

use super::*;

impl KernelDb {
    /// Atomically create a forked context: the document row, the context row,
    /// the child's `cwd`, and the env + egress + capability-binding config
    /// copied from `source`, all in ONE transaction.
    ///
    /// `cwd` is the caller's: `kj fork` passes the source's cwd or `--pwd`
    /// after checking that `cd` enters it. `None` leaves the child without
    /// a cwd.
    ///
    /// This folds `insert_context_with_document` + `fork_context_config` into a
    /// single all-or-nothing write. Calling them separately left a gap: the
    /// context insert committed first, so a failure copying the config stranded
    /// a committed-but-misconfigured context (under deny-by-default, one with no
    /// loadout — locked out). Here a failure on any write rolls the whole fork
    /// back, leaving no context row behind.
    ///
    /// The source config is read up front (SELECTs) so the transaction holds
    /// only the writes, matching `fork_context_config`.
    pub fn insert_forked_context(
        &mut self,
        row: &ContextRow,
        default_workspace_id: WorkspaceId,
        source: ContextId,
        cwd: Option<&str>,
    ) -> KernelDbResult<()> {
        let env = self.get_context_env(source)?;
        let egress = self.list_context_egress(source)?;
        // A parent's context-scoped MCP grants name servers declared for the
        // parent alone; the child keeps the rest of the loadout.
        let binding = self
            .get_context_binding(source)?
            .map(crate::mcp::ContextToolBinding::without_context_scoped);

        let ws_id = row.workspace_id.unwrap_or(default_workspace_id);
        let doc = DocumentRow {
            document_id: row.context_id,
            workspace_id: ws_id,
            doc_kind: DocKind::Conversation,
            language: None,
            path: None,
            created_at: row.created_at,
            created_by: row.created_by,
        };

        let tx = self.conn.transaction()?;
        Self::write_document_or_ignore(&tx, &doc)?;
        Self::write_context(&tx, row)?;
        if let Some(cwd) = cwd {
            Self::write_context_shell(
                &tx,
                &ContextShellRow {
                    context_id: row.context_id,
                    cwd: Some(cwd.to_string()),
                    updated_at: now_millis(),
                },
            )?;
        }
        for var in &env {
            Self::write_context_env(&tx, row.context_id, &var.key, &var.value)?;
        }
        for host in &egress {
            Self::write_context_egress(&tx, row.context_id, host)?;
        }
        if let Some(binding) = binding {
            Self::write_binding(&tx, row.context_id, &binding)?;
        }
        // Attachments travel with the fork: the child joins the SAME tracks as the
        // parent (docs/tracks.md §3 — "The context binds; the child inherits the
        // bind at fork"). The child re-binds via create/fork rc on the way up so
        // the track never has to watch for forks. A non-musician parent has no
        // attachments — the copy is a clean no-op.
        Self::copy_attachments_for_fork(&tx, source, row.context_id)?;
        // The child is the same character as its source, answering to it.
        super::context_characters::copy_character_into(&tx, source, row.context_id)?;
        tx.commit()?;
        Ok(())
    }

    /// Copy shell config + env vars + egress hosts + capability binding from
    /// source context to target. Called during all fork operations. The
    /// binding copy makes permissions follow the fork — under deny-by-default
    /// a fork would otherwise start with no loadout and be locked out. Grants
    /// for MCP servers declared for the source alone are not copied.
    ///
    /// Atomic: the four copies land in ONE transaction. A fork that fails
    /// partway must leave NO partial config behind — a half-copied loadout
    /// would lock the fork out (or grant a stale subset), and the caller has
    /// already committed the context row, so a silent partial here would
    /// strand a misconfigured context. The source is read up front so the
    /// transaction holds only the writes.
    pub fn fork_context_config(&mut self, source: ContextId, target: ContextId) -> KernelDbResult<()> {
        let shell = self.get_context_shell(source)?;
        let env = self.get_context_env(source)?;
        let egress = self.list_context_egress(source)?;
        let binding = self
            .get_context_binding(source)?
            .map(crate::mcp::ContextToolBinding::without_context_scoped);

        let tx = self.conn.transaction()?;
        if let Some(src) = shell {
            Self::write_context_shell(
                &tx,
                &ContextShellRow {
                    context_id: target,
                    cwd: src.cwd,
                    updated_at: now_millis(),
                },
            )?;
        }
        for var in &env {
            Self::write_context_env(&tx, target, &var.key, &var.value)?;
        }
        for host in &egress {
            Self::write_context_egress(&tx, target, host)?;
        }
        if let Some(binding) = binding {
            Self::write_binding(&tx, target, &binding)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Copy the capability binding (flags + instances + tools + facades +
    /// sticky names) from `source` to `target`. No-op if the source has no
    /// binding. The child can later attenuate (self-narrow) but inherits the
    /// parent's loadout as its starting point, without the parent's
    /// context-scoped MCP server grants.
    pub fn copy_context_binding(
        &mut self,
        source: ContextId,
        target: ContextId,
    ) -> KernelDbResult<bool> {
        match self.get_context_binding(source)? {
            Some(binding) => {
                self.upsert_context_binding(target, &binding.without_context_scoped())?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests;
