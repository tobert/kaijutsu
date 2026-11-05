//! Forked-from tree and drift edges.

use super::*;

/// A drift edge row: content moved from `source_id` to `target_id`.
#[derive(Debug, Clone)]
pub struct ContextEdgeRow {
    pub edge_id: uuid::Uuid,
    pub source_id: ContextId,
    pub target_id: ContextId,
    pub metadata: Option<String>,
    pub created_at: i64,
}

impl KernelDb {
    /// Get the live children of a context: the contexts forked from it.
    pub fn fork_children(&self, id: ContextId) -> KernelDbResult<Vec<ContextRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.context_id, c.label,
                    c.system_prompt, c.context_state,
                    c.created_at, c.created_by, c.forked_from, c.fork_kind,
                    c.archived_at, c.workspace_id, c.preset_id, c.concluded_at,
                    c.last_activity_at, c.promoted_at, c.demoted_at, c.paused_at, c.origin_host, c.performer, c.senpai_ctx, c.persona
             FROM contexts c
             WHERE c.forked_from = ?1 AND c.archived_at IS NULL
             ORDER BY c.created_at, c.context_id",
        )?;

        let rows = stmt.query_map(params![blob_param(id.as_bytes())], |row| {
            row_to_context_row(row)
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Every live context in depth-first order over `forked_from`, each
    /// parent followed by its subtree, siblings by creation time. Returns
    /// `(ContextRow, depth)` pairs. A live context whose parent is archived
    /// (or absent) is a root of its own, so it still appears.
    pub fn context_dag(&self) -> KernelDbResult<Vec<(ContextRow, i64)>> {
        let live = self.list_active_contexts()?;
        let live_ids: HashSet<ContextId> = live.iter().map(|c| c.context_id).collect();
        let mut children: HashMap<ContextId, Vec<&ContextRow>> = HashMap::new();
        let mut roots: Vec<&ContextRow> = Vec::new();
        for ctx in &live {
            match ctx.forked_from.filter(|p| live_ids.contains(p)) {
                Some(parent) => children.entry(parent).or_default().push(ctx),
                None => roots.push(ctx),
            }
        }
        let order = |a: &&ContextRow, b: &&ContextRow| {
            (a.created_at, a.context_id).cmp(&(b.created_at, b.context_id))
        };
        roots.sort_by(order);
        for siblings in children.values_mut() {
            siblings.sort_by(order);
        }

        let mut out = Vec::with_capacity(live.len());
        let mut stack: Vec<(&ContextRow, i64)> = roots.into_iter().rev().map(|c| (c, 0)).collect();
        while let Some((ctx, depth)) = stack.pop() {
            out.push((ctx.clone(), depth));
            if let Some(kids) = children.get(&ctx.context_id) {
                stack.extend(kids.iter().rev().map(|c| (*c, depth + 1)));
            }
        }
        if out.len() != live.len() {
            return Err(KernelDbError::Validation(format!(
                "forked_from forms a cycle among live contexts: {} of {} reachable from a root",
                out.len(),
                live.len()
            )));
        }
        Ok(out)
    }

    /// Walk fork lineage from a context upward via `forked_from`.
    /// Returns `(ContextRow, depth)` where depth 0 = the starting context.
    pub fn fork_lineage(&self, context_id: ContextId) -> KernelDbResult<Vec<(ContextRow, i64)>> {
        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE lineage(ctx_id, depth) AS (
                SELECT ?1, 0
                UNION ALL
                SELECT c.forked_from, lineage.depth + 1
                FROM lineage
                JOIN contexts c ON c.context_id = lineage.ctx_id
                WHERE c.forked_from IS NOT NULL
            )
            SELECT c.context_id, c.label,
                   c.system_prompt, c.context_state,
                   c.created_at, c.created_by, c.forked_from, c.fork_kind,
                   c.archived_at, c.workspace_id, c.preset_id, c.concluded_at,
                   c.last_activity_at, c.promoted_at, c.demoted_at, c.paused_at,
                   c.origin_host, c.performer, c.senpai_ctx, c.persona, lineage.depth
            FROM lineage
            JOIN contexts c ON c.context_id = lineage.ctx_id
            ORDER BY lineage.depth",
        )?;

        let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
            let ctx = row_to_context_row(row)?;
            let depth: i64 = row.get(20)?;
            Ok((ctx, depth))
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Insert a drift edge.
    pub fn insert_edge(&self, row: &ContextEdgeRow) -> KernelDbResult<()> {
        self.conn.execute(
            "INSERT INTO context_edges (edge_id, source_id, target_id, metadata, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                row.edge_id.as_bytes().as_slice(),
                blob_param(row.source_id.as_bytes()),
                blob_param(row.target_id.as_bytes()),
                row.metadata,
                row.created_at,
            ],
        )?;
        Ok(())
    }

    /// List edges from a source, oldest first.
    pub fn edges_from(&self, source: ContextId) -> KernelDbResult<Vec<ContextEdgeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT edge_id, source_id, target_id, metadata, created_at
             FROM context_edges
             WHERE source_id = ?1
             ORDER BY created_at",
        )?;
        let rows = stmt
            .query_map(params![blob_param(source.as_bytes())], row_to_edge_row)?
            .collect::<SqliteResult<Vec<_>>>()?;
        Ok(rows)
    }

    /// List edges to a target, oldest first.
    pub fn edges_to(&self, target: ContextId) -> KernelDbResult<Vec<ContextEdgeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT edge_id, source_id, target_id, metadata, created_at
             FROM context_edges
             WHERE target_id = ?1
             ORDER BY created_at",
        )?;
        let rows = stmt
            .query_map(params![blob_param(target.as_bytes())], row_to_edge_row)?
            .collect::<SqliteResult<Vec<_>>>()?;
        Ok(rows)
    }

    /// List drift edges originating from a context, newest first.
    pub fn drift_provenance(&self, context_id: ContextId) -> KernelDbResult<Vec<ContextEdgeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT edge_id, source_id, target_id, metadata, created_at
             FROM context_edges
             WHERE source_id = ?1
             ORDER BY created_at DESC",
        )?;

        let rows = stmt
            .query_map(params![blob_param(context_id.as_bytes())], row_to_edge_row)?
            .collect::<SqliteResult<Vec<_>>>()?;
        Ok(rows)
    }

    /// Delete a single drift edge by its UUID.
    ///
    /// Companion to `kj drift edge rm` — `kj drift history` emits these
    /// UUIDs as iteration handles. A context's parent link is
    /// `contexts.forked_from`, so no edge row holds one.
    pub fn delete_drift_edge(&self, edge_id: uuid::Uuid) -> KernelDbResult<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM context_edges
             WHERE edge_id = ?1",
            params![edge_id.as_bytes().as_slice()],
        )?;
        Ok(deleted > 0)
    }
}

#[cfg(test)]
mod tests;
