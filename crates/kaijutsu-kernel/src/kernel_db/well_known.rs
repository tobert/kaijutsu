//! Well-known context roles and their registry rows.

use super::*;

/// A kernel role that exactly one context fills at a time. Identity for these
/// contexts is this role, NOT their label — which is what makes rotation an
/// UPDATE instead of a rename. See `well_known_contexts` in `SCHEMA` and
/// `docs/drifting-dead-letters.md` (slices 1 and 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WellKnownRole {
    /// The lost+found dead-letter sink.
    LostFound,
    /// The durable drift queue (slice 3): staged and dead-lettered
    /// `StagedDrift` items are persisted as blocks here so a kernel restart
    /// cannot discard them. See `DriftRouter::attach_persistence` in
    /// `drift.rs`.
    DriftQueue,
}

impl WellKnownRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::LostFound => "lost_found",
            Self::DriftQueue => "drift_queue",
        }
    }
}

impl KernelDb {
    /// The context currently filling `role`, or `None` if the role is vacant
    /// (never assigned, or its holder was destroyed and cascaded away).
    pub fn well_known_context(&self, role: WellKnownRole) -> KernelDbResult<Option<ContextId>> {
        let mut stmt = self
            .conn
            .prepare("SELECT context_id FROM well_known_contexts WHERE role = ?1")?;
        let mut rows = stmt.query(params![role.as_str()])?;
        match rows.next()? {
            Some(row) => Ok(Some(read_context_id(row, 0)?)),
            None => Ok(None),
        }
    }

    /// Publish a context and its well-known role atomically. Failed publication
    /// leaves no context, so its unregistered document can be discarded.
    pub fn insert_well_known_context(
        &self,
        row: &ContextRow,
        workspace: WorkspaceId,
        role: WellKnownRole,
    ) -> KernelDbResult<()> {
        let tx = self.conn.unchecked_transaction()?;
        self.insert_placed_context(row, workspace)?;
        self.set_well_known_context(role, row.context_id)?;
        tx.commit()?;
        Ok(())
    }

    /// Assign `id` to `role` — the rotation primitive. Upserts: a role that
    /// is already held is reassigned to `id` rather than erroring, so this is
    /// also how a rotation happens (the incumbent is untouched by this call —
    /// it keeps its row and its blocks as an ordinary context, it just stops
    /// being pointed at by the registry).
    pub fn set_well_known_context(&self, role: WellKnownRole, id: ContextId) -> KernelDbResult<()> {
        self.conn.execute(
            "INSERT INTO well_known_contexts (role, context_id, assigned_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(role) DO UPDATE SET
                 context_id = excluded.context_id,
                 assigned_at = excluded.assigned_at",
            params![role.as_str(), blob_param(id.as_bytes()), now_millis()],
        )?;
        Ok(())
    }

    /// Vacate `role`. Not currently called by slice 1 (rotation reassigns
    /// rather than clears), but a role the registry can hand out must also be
    /// one it can take back — e.g. if a holder is deliberately retired with
    /// no successor yet chosen.
    pub fn clear_well_known_context(&self, role: WellKnownRole) -> KernelDbResult<()> {
        self.conn
            .execute("DELETE FROM well_known_contexts WHERE role = ?1", params![role.as_str()])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
