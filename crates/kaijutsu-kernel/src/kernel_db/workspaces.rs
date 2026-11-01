//! Workspaces and their paths.

use super::*;

/// A workspace row.
#[derive(Debug, Clone)]
pub struct WorkspaceRow {
    pub workspace_id: WorkspaceId,
    pub label: String,
    pub description: Option<String>,
    pub created_at: i64,
    pub created_by: PrincipalId,
    pub archived_at: Option<i64>,
}

/// A workspace path row.
#[derive(Debug, Clone)]
pub struct WorkspacePathRow {
    pub workspace_id: WorkspaceId,
    pub path: String,
    pub read_only: bool,
    pub created_at: i64,
}

impl KernelDb {
    /// Get or create the `__system` workspace (for config docs).
    /// Returns the workspace ID.
    pub fn get_or_create_system_workspace(
        &self,
        created_by: PrincipalId,
    ) -> KernelDbResult<WorkspaceId> {
        self.get_or_create_builtin_workspace(
            "__system",
            "System configuration documents",
            created_by,
        )
    }

    /// Get or create the `__default` workspace (for conversations and file cache).
    /// Returns the workspace ID.
    pub fn get_or_create_default_workspace(
        &self,
        created_by: PrincipalId,
    ) -> KernelDbResult<WorkspaceId> {
        self.get_or_create_builtin_workspace("__default", "Default workspace", created_by)
    }

    /// Get or create a built-in workspace by label.
    pub(super) fn get_or_create_builtin_workspace(
        &self,
        label: &str,
        description: &str,
        created_by: PrincipalId,
    ) -> KernelDbResult<WorkspaceId> {
        if let Some(ws) = self.get_workspace_by_label(label)? {
            return Ok(ws.workspace_id);
        }
        let ws_id = WorkspaceId::new();
        let row = WorkspaceRow {
            workspace_id: ws_id,
            label: label.to_string(),
            description: Some(description.to_string()),
            created_at: now_millis(),
            created_by,
            archived_at: None,
        };
        self.insert_workspace(&row)?;
        info!(workspace_id = %ws_id.to_hex(), label, "Created built-in workspace");
        Ok(ws_id)
    }

    /// Insert a new workspace.
    pub fn insert_workspace(&self, row: &WorkspaceRow) -> KernelDbResult<()> {
        validate_label(&row.label)?;

        self.conn
            .execute(
                "INSERT INTO workspaces (
                workspace_id, label, description, created_at,
                created_by, archived_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    blob_param(row.workspace_id.as_bytes()),
                    row.label,
                    row.description,
                    row.created_at,
                    blob_param(row.created_by.as_bytes()),
                    row.archived_at,
                ],
            )
            .map_err(|e| {
                map_unique_violation(e, format!("workspace label '{}' already in use", row.label))
            })?;
        Ok(())
    }

    /// Get a workspace by ID.
    pub fn get_workspace(&self, id: WorkspaceId) -> KernelDbResult<Option<WorkspaceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT workspace_id, label, description, created_at,
                    created_by, archived_at
             FROM workspaces WHERE workspace_id = ?1",
        )?;

        let mut rows = stmt.query(params![blob_param(id.as_bytes())])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_workspace_row(row)?))
        } else {
            Ok(None)
        }
    }

    /// Get a workspace by label.
    pub fn get_workspace_by_label(&self, label: &str) -> KernelDbResult<Option<WorkspaceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT workspace_id, label, description, created_at,
                    created_by, archived_at
             FROM workspaces WHERE label = ?1",
        )?;

        let mut rows = stmt.query(params![label])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_workspace_row(row)?))
        } else {
            Ok(None)
        }
    }

    /// List active (non-archived) workspaces.
    pub fn list_workspaces(&self) -> KernelDbResult<Vec<WorkspaceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT workspace_id, label, description, created_at,
                    created_by, archived_at
             FROM workspaces
             WHERE archived_at IS NULL
             ORDER BY label",
        )?;

        let rows = stmt.query_map([], row_to_workspace_row)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// List all workspaces (including archived).
    pub fn list_all_workspaces(&self) -> KernelDbResult<Vec<WorkspaceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT workspace_id, label, description, created_at,
                    created_by, archived_at
             FROM workspaces
             ORDER BY label",
        )?;

        let rows = stmt.query_map([], row_to_workspace_row)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Archive a workspace (soft delete). Returns true if it was active.
    pub fn archive_workspace(&self, id: WorkspaceId) -> KernelDbResult<bool> {
        let now = now_millis();
        let updated = self.conn.execute(
            "UPDATE workspaces SET archived_at = ?1
             WHERE workspace_id = ?2 AND archived_at IS NULL",
            params![now, blob_param(id.as_bytes())],
        )?;
        Ok(updated > 0)
    }

    /// Insert a workspace path.
    pub fn insert_workspace_path(&self, row: &WorkspacePathRow) -> KernelDbResult<()> {
        self.conn
            .execute(
                "INSERT INTO workspace_paths (workspace_id, path, read_only, created_at)
             VALUES (?1, ?2, ?3, ?4)",
                params![
                    blob_param(row.workspace_id.as_bytes()),
                    row.path,
                    row.read_only as i64,
                    row.created_at,
                ],
            )
            .map_err(|e| {
                map_unique_violation(e, format!("workspace path '{}' already exists", row.path))
            })?;
        Ok(())
    }

    /// List paths for a workspace.
    pub fn list_workspace_paths(
        &self,
        workspace_id: WorkspaceId,
    ) -> KernelDbResult<Vec<WorkspacePathRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT workspace_id, path, read_only, created_at
             FROM workspace_paths WHERE workspace_id = ?1
             ORDER BY path",
        )?;

        let rows = stmt.query_map(params![blob_param(workspace_id.as_bytes())], |row| {
            let ro: i64 = row.get(2)?;
            Ok(WorkspacePathRow {
                workspace_id: read_workspace_id(row, 0)?,
                path: row.get(1)?,
                read_only: ro != 0,
                created_at: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Delete a workspace path. Returns true if it existed.
    pub fn delete_workspace_path(
        &self,
        workspace_id: WorkspaceId,
        path: &str,
    ) -> KernelDbResult<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM workspace_paths WHERE workspace_id = ?1 AND path = ?2",
            params![blob_param(workspace_id.as_bytes()), path],
        )?;
        Ok(deleted > 0)
    }

    /// Get workspace paths for a context (via contexts.workspace_id FK).
    /// Returns None if context has no workspace bound.
    pub fn context_workspace_paths(
        &self,
        context_id: ContextId,
    ) -> KernelDbResult<Option<Vec<WorkspacePathRow>>> {
        let ctx = self
            .get_context(context_id)?
            .ok_or_else(|| KernelDbError::NotFound(format!("context {}", context_id.short())))?;
        match ctx.workspace_id {
            Some(ws_id) => {
                let paths = self.list_workspace_paths(ws_id)?;
                Ok(Some(paths))
            }
            None => Ok(None),
        }
    }

    /// Check whether a path is allowed by a context's workspace.
    ///
    /// Returns `None` if the context has no workspace (unbound = kernel perimeter
    /// defaults, no restriction). Returns `Some(read_only)` if the path falls
    /// under a workspace path. Returns an error if the path is outside all
    /// workspace paths (bound context, path not in scope).
    pub fn check_workspace_path(
        &self,
        context_id: ContextId,
        path: &str,
    ) -> KernelDbResult<Option<bool>> {
        let ws_paths = match self.context_workspace_paths(context_id)? {
            None => return Ok(None), // unbound context — no workspace restriction
            Some(paths) => paths,
        };

        // Find longest-prefix match among workspace paths
        let mut best: Option<&WorkspacePathRow> = None;
        for wp in &ws_paths {
            if path == wp.path || path.starts_with(&format!("{}/", wp.path)) {
                match best {
                    None => best = Some(wp),
                    Some(prev) if wp.path.len() > prev.path.len() => best = Some(wp),
                    _ => {}
                }
            }
        }

        match best {
            Some(wp) => Ok(Some(wp.read_only)),
            None => Err(KernelDbError::Validation(format!(
                "path '{}' is outside workspace scope",
                path,
            ))),
        }
    }

    /// Count contexts using a specific workspace.
    pub fn contexts_using_workspace(&self, workspace_id: WorkspaceId) -> KernelDbResult<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM contexts
             WHERE workspace_id = ?1 AND archived_at IS NULL",
            params![blob_param(workspace_id.as_bytes())],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }
}

#[cfg(test)]
mod tests;
