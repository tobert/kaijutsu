//! Characters, their slots, and their models.

use super::*;

/// A character's sheet — the persistent someone `name` resolves to. The
/// principal is the character's key (`docs/character.md`, "Character =
/// principal + sheet"): no distinct id type, `principal_id` is immutable,
/// and `name` is kernel-owned and immutable in this slice. `handoff_ctx`
/// arrives with slice 4; the rest of the sheet arrives with the slice that
/// reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharacterRow {
    pub principal_id: PrincipalId,
    pub name: String,
    pub created_at: i64,
    /// `None` while live. Set once, never cleared — a character retires,
    /// it is never un-retired.
    pub retired_at: Option<i64>,
    /// The character's handoff-log context, or `None` before the first `kj
    /// handoff note`/`tail` mints one. Set exactly once, by
    /// `set_character_handoff_ctx` under the same `KernelDb` lock that
    /// checked it was still unset (`docs/character.md`, "The handoff is an
    /// ordinary context").
    pub handoff_ctx: Option<ContextId>,
    /// A root character's root context, where its person types `kj` and
    /// where the seats it starts begin their lineage. `None` for an
    /// ordinary character, and briefly for a root whose context the kernel
    /// has not created yet (`docs/character.md`, "Roots and rotation").
    pub root_ctx: Option<ContextId>,
    /// A root character is a model context: a model can play it, and its
    /// kj requests always need approval (`docs/character.md`, "Roots and
    /// rotation").
    pub root: bool,
    /// The directory under `/config/rc/` whose lifecycle scripts this
    /// character's contexts run. `None` when unset: a context create for the
    /// character refuses until `kj character set <name> --rc <dir>` names one
    /// (`docs/character.md`, "What a context and a character carry").
    pub rc_dir: Option<String>,
    /// A root character's console: the model console a person talks to,
    /// performed by a non-root character and answering to the root console.
    /// A rotation of the console moves the pointer in the commit, as it
    /// moves `root_ctx`. `None` for an ordinary character or before one is set.
    pub console_ctx: Option<ContextId>,
    /// A root character's MCP hub, the parent of its MCP sessions.
    /// `None` for an ordinary character or before the hub exists.
    pub mcp_hub_ctx: Option<ContextId>,
}

impl CharacterRow {
    /// A root character that has not retired: a person's hands on the
    /// instrument, the posture every "is this a person" check asks about.
    pub fn is_live_root(&self) -> bool {
        self.root && self.retired_at.is_none()
    }
}

/// One role's seat on a character. A NULL tunable takes the model's `backend_models` value when a context character copies the slot.
#[derive(Debug, Clone, PartialEq)]
pub struct CharacterSlotRow {
    pub principal_id: PrincipalId,
    pub role: String,
    pub backend_id: BackendId,
    pub model: String,
    pub max_tokens: Option<i64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub effort: Option<String>,
    pub thinking_budget: Option<i64>,
    pub thinking_style: Option<String>,
}

/// A character's one model. A NULL tunable takes the model's `backend_models` value when a context character copies it.
#[derive(Debug, Clone, PartialEq)]
pub struct CharacterModelRow {
    pub principal_id: PrincipalId,
    pub backend_id: BackendId,
    pub model: String,
    pub max_tokens: Option<i64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub effort: Option<String>,
    pub thinking_budget: Option<i64>,
    pub thinking_style: Option<String>,
}

pub(super) const CHARACTER_SLOT_COLS: &str = "principal_id, role, backend_id, model, max_tokens, temperature, \
                                   top_p, effort, thinking_budget, thinking_style";

pub(super) fn row_to_character_slot(row: &rusqlite::Row<'_>) -> SqliteResult<CharacterSlotRow> {
    Ok(CharacterSlotRow {
        principal_id: read_principal_id(row, 0)?,
        role: row.get(1)?,
        backend_id: read_backend_id(row, 2)?,
        model: row.get(3)?,
        max_tokens: row.get(4)?,
        temperature: row.get(5)?,
        top_p: row.get(6)?,
        effort: row.get(7)?,
        thinking_budget: row.get(8)?,
        thinking_style: row.get(9)?,
    })
}

pub(super) const CHARACTER_MODEL_COLS: &str = "principal_id, backend_id, model, max_tokens, temperature, \
                                    top_p, effort, thinking_budget, thinking_style";

pub(super) fn row_to_character_model(row: &rusqlite::Row<'_>) -> SqliteResult<CharacterModelRow> {
    Ok(CharacterModelRow {
        principal_id: read_principal_id(row, 0)?,
        backend_id: read_backend_id(row, 1)?,
        model: row.get(2)?,
        max_tokens: row.get(3)?,
        temperature: row.get(4)?,
        top_p: row.get(5)?,
        effort: row.get(6)?,
        thinking_budget: row.get(7)?,
        thinking_style: row.get(8)?,
    })
}

impl KernelDb {
    /// Insert or replace one slot of a character. The role is `distill` or
    /// `gate`. A `gate` slot names a council server (a backend of kind `mk`),
    /// and a `distill` slot names a chat model (any other kind); anything
    /// else refuses, naming the fix.
    pub fn set_character_slot(&self, row: &CharacterSlotRow) -> KernelDbResult<()> {
        if row.role.trim().is_empty() {
            return Err(KernelDbError::Validation("role must not be empty".into()));
        }
        crate::model_resolution::refuse_character_slot_role(&row.role, &self.name_for(row.principal_id))
            .map_err(KernelDbError::Validation)?;
        let backend = self
            .get_backend(row.backend_id)?
            .ok_or_else(|| KernelDbError::NotFound(format!("backend {}", row.backend_id)))?;
        let council_server = crate::model_resolution::is_council_kind(&backend.kind);
        if row.role == crate::council::gate::GATE && !council_server {
            return Err(KernelDbError::Validation(format!(
                "a `gate` slot names a council server, and backend {} is kind {}, not mk; \
                 `kj backend list` names the mk backends",
                backend.name, backend.kind
            )));
        }
        if row.role == crate::model_resolution::DISTILL && council_server {
            return Err(KernelDbError::Validation(format!(
                "a `distill` slot names a chat model, and backend {} is kind mk, a council server, which \
                 distills nothing; name a backend of another kind (`kj backend list`)",
                backend.name
            )));
        }
        if row.model.trim().is_empty() {
            return Err(KernelDbError::Validation("model must not be empty".into()));
        }
        validate_tunables(
            row.max_tokens,
            row.temperature,
            row.top_p,
            row.thinking_budget,
            row.thinking_style.as_deref(),
        )?;
        self.conn.execute(
            &format!(
                "INSERT INTO character_slots ({CHARACTER_SLOT_COLS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(principal_id, role) DO UPDATE SET
                     backend_id = excluded.backend_id,
                     model = excluded.model,
                     max_tokens = excluded.max_tokens,
                     temperature = excluded.temperature,
                     top_p = excluded.top_p,
                     effort = excluded.effort,
                     thinking_budget = excluded.thinking_budget,
                     thinking_style = excluded.thinking_style"
            ),
            params![
                blob_param(row.principal_id.as_bytes()),
                row.role,
                blob_param(row.backend_id.as_bytes()),
                row.model,
                row.max_tokens,
                row.temperature,
                row.top_p,
                row.effort,
                row.thinking_budget,
                row.thinking_style,
            ],
        )?;
        Ok(())
    }

    /// Remove one slot. Returns whether a row went.
    pub fn delete_character_slot(&self, principal_id: PrincipalId, role: &str) -> KernelDbResult<bool> {
        let n = self.conn.execute(
            "DELETE FROM character_slots WHERE principal_id = ?1 AND role = ?2",
            params![blob_param(principal_id.as_bytes()), role],
        )?;
        Ok(n > 0)
    }

    /// One character's slots, ordered by role.
    pub fn list_character_slots(&self, principal_id: PrincipalId) -> KernelDbResult<Vec<CharacterSlotRow>> {
        let sql = format!("SELECT {CHARACTER_SLOT_COLS} FROM character_slots WHERE principal_id = ?1 ORDER BY role");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![blob_param(principal_id.as_bytes())], row_to_character_slot)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// One character's slot for `role`, if it has one.
    pub fn get_character_slot(&self, principal_id: PrincipalId, role: &str) -> KernelDbResult<Option<CharacterSlotRow>> {
        let sql = format!("SELECT {CHARACTER_SLOT_COLS} FROM character_slots WHERE principal_id = ?1 AND role = ?2");
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt
            .query_row(params![blob_param(principal_id.as_bytes()), role], row_to_character_slot)
            .optional()?)
    }

    /// Insert or replace a character's model. A root character runs no model,
    /// so it is refused.
    pub fn set_character_model(&self, row: &CharacterModelRow) -> KernelDbResult<()> {
        let Some(sheet) = self.get_character(row.principal_id)? else {
            return Err(KernelDbError::NotFound(format!("character {}", row.principal_id.short())));
        };
        if sheet.root {
            return Err(KernelDbError::Validation(format!(
                "{} is a root character, and a root runs no model",
                sheet.name
            )));
        }
        if row.model.trim().is_empty() {
            return Err(KernelDbError::Validation("model must not be empty".into()));
        }
        validate_tunables(
            row.max_tokens,
            row.temperature,
            row.top_p,
            row.thinking_budget,
            row.thinking_style.as_deref(),
        )?;
        self.conn.execute(
            &format!(
                "INSERT INTO character_models ({CHARACTER_MODEL_COLS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(principal_id) DO UPDATE SET
                     backend_id = excluded.backend_id,
                     model = excluded.model,
                     max_tokens = excluded.max_tokens,
                     temperature = excluded.temperature,
                     top_p = excluded.top_p,
                     effort = excluded.effort,
                     thinking_budget = excluded.thinking_budget,
                     thinking_style = excluded.thinking_style"
            ),
            params![
                blob_param(row.principal_id.as_bytes()),
                blob_param(row.backend_id.as_bytes()),
                row.model,
                row.max_tokens,
                row.temperature,
                row.top_p,
                row.effort,
                row.thinking_budget,
                row.thinking_style,
            ],
        )?;
        Ok(())
    }

    /// Remove a character's model. Returns whether a row went.
    pub fn delete_character_model(&self, principal_id: PrincipalId) -> KernelDbResult<bool> {
        let n = self.conn.execute(
            "DELETE FROM character_models WHERE principal_id = ?1",
            params![blob_param(principal_id.as_bytes())],
        )?;
        Ok(n > 0)
    }

    /// A character's model, if it has one.
    pub fn get_character_model(&self, principal_id: PrincipalId) -> KernelDbResult<Option<CharacterModelRow>> {
        let sql = format!("SELECT {CHARACTER_MODEL_COLS} FROM character_models WHERE principal_id = ?1");
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt
            .query_row(params![blob_param(principal_id.as_bytes())], row_to_character_model)
            .optional()?)
    }

    /// Create a character's sheet row. Fails loudly on a duplicate name —
    /// the caller (`kj character create`) checks for an existing name first
    /// and returns that row unchanged rather than calling this twice; this
    /// method itself does not dedupe.
    pub fn insert_character(&self, row: &CharacterRow) -> KernelDbResult<()> {
        validate_label(&row.name)?;
        self.conn
            .execute(
                "INSERT INTO characters (principal_id, name, created_at, retired_at, handoff_ctx, root, root_ctx, rc_dir, console_ctx, mcp_hub_ctx)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    blob_param(row.principal_id.as_bytes()),
                    row.name,
                    row.created_at,
                    row.retired_at,
                    row.handoff_ctx.as_ref().map(|c| c.as_bytes().to_vec()),
                    row.root,
                    row.root_ctx.as_ref().map(|c| c.as_bytes().to_vec()),
                    row.rc_dir,
                    row.console_ctx.as_ref().map(|c| c.as_bytes().to_vec()),
                    row.mcp_hub_ctx.as_ref().map(|c| c.as_bytes().to_vec()),
                ],
            )
            .map_err(|e| {
                map_unique_violation(e, format!("character name '{}' already in use", row.name))
            })?;
        Ok(())
    }

    /// Fetch a character by its kernel-owned name.
    pub fn get_character_by_name(&self, name: &str) -> KernelDbResult<Option<CharacterRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT principal_id, name, created_at, retired_at, handoff_ctx, root, root_ctx, rc_dir, console_ctx, mcp_hub_ctx
             FROM characters WHERE name = ?1",
        )?;
        Ok(stmt.query_row(params![name], row_to_character_row).optional()?)
    }

    /// Fetch a character by the principal id that plays it — the counterpart
    /// to `get_character_by_name` for callers that only have a context row's
    /// `played_by` (e.g. resolving the name to display in `kj context info`).
    pub fn get_character(&self, principal_id: PrincipalId) -> KernelDbResult<Option<CharacterRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT principal_id, name, created_at, retired_at, handoff_ctx, root, root_ctx, rc_dir, console_ctx, mcp_hub_ctx
             FROM characters WHERE principal_id = ?1",
        )?;
        Ok(stmt
            .query_row(params![blob_param(principal_id.as_bytes())], row_to_character_row)
            .optional()?)
    }

    /// Every character, ordered by name. `include_retired` controls whether
    /// a retired row is included — `kj character list` wants only the live
    /// ones by default.
    pub fn list_characters(&self, include_retired: bool) -> KernelDbResult<Vec<CharacterRow>> {
        let sql = if include_retired {
            "SELECT principal_id, name, created_at, retired_at, handoff_ctx, root, root_ctx, rc_dir, console_ctx, mcp_hub_ctx \
             FROM characters ORDER BY name"
        } else {
            "SELECT principal_id, name, created_at, retired_at, handoff_ctx, root, root_ctx, rc_dir, console_ctx, mcp_hub_ctx \
             FROM characters WHERE retired_at IS NULL ORDER BY name"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], row_to_character_row)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Set (or clear) a character's `rc_dir`. Naming a directory clears the
    /// character's `rc_dir_unset` migration note.
    pub fn set_character_rc_dir(&self, principal_id: PrincipalId, rc_dir: Option<&str>) -> KernelDbResult<()> {
        self.set_character_column("rc_dir", principal_id, rc_dir.map(str::to_string))?;
        if rc_dir.is_some_and(|dir| !dir.is_empty()) {
            self.conn.execute(
                "DELETE FROM context_character_migration_report WHERE kind = 'rc_dir_unset' AND principal_id = ?1",
                params![blob_param(principal_id.as_bytes())],
            )?;
        }
        Ok(())
    }

    /// Set (or clear) a root character's console pointer. A context is the
    /// console of one sheet: a context another sheet's `console_ctx` names
    /// refuses, naming that sheet.
    pub fn set_character_console_ctx(&self, principal_id: PrincipalId, ctx: Option<ContextId>) -> KernelDbResult<()> {
        self.refuse_pointer_held_elsewhere("console_ctx", "console", principal_id, ctx)?;
        self.set_character_column("console_ctx", principal_id, ctx.map(|c| c.as_bytes().to_vec()))
    }

    /// Set (or clear) a root character's MCP hub pointer. A context is the
    /// hub of one sheet, as for [`Self::set_character_console_ctx`].
    pub fn set_character_mcp_hub_ctx(&self, principal_id: PrincipalId, ctx: Option<ContextId>) -> KernelDbResult<()> {
        self.refuse_pointer_held_elsewhere("mcp_hub_ctx", "MCP hub", principal_id, ctx)?;
        self.set_character_column("mcp_hub_ctx", principal_id, ctx.map(|c| c.as_bytes().to_vec()))
    }

    /// Refuse to point `principal_id`'s `column` at `ctx` while another
    /// sheet's `column` names it, naming the sheets (`what` names the
    /// pointer in the refusal).
    fn refuse_pointer_held_elsewhere(&self, column: &str, what: &str, principal_id: PrincipalId, ctx: Option<ContextId>) -> KernelDbResult<()> {
        let Some(ctx) = ctx else { return Ok(()) };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT name FROM characters WHERE {column} = ?1 AND principal_id != ?2 ORDER BY name"
        ))?;
        let holders = stmt
            .query_map(params![blob_param(ctx.as_bytes()), blob_param(principal_id.as_bytes())], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if holders.is_empty() {
            return Ok(());
        }
        let fix = match column {
            "console_ctx" => format!("; `kj character set {} --no-console` frees it", holders[0]),
            _ => String::new(),
        };
        Err(KernelDbError::Validation(format!(
            "context {} is already the {what} of {}, and a context is the {what} of one character{fix}",
            self.context_name(ctx),
            holders.join(", ")
        )))
    }

    fn set_character_column<V: rusqlite::ToSql>(&self, column: &str, principal_id: PrincipalId, value: Option<V>) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            &format!("UPDATE characters SET {column} = ?1 WHERE principal_id = ?2"),
            params![value, blob_param(principal_id.as_bytes())],
        )?;
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!("character {}", principal_id.short())));
        }
        Ok(())
    }

    /// Set (or clear) a character's handoff-log context. Called exactly
    /// once per character in the ordinary case, by `kj handoff`'s
    /// get-or-create path, under the same lock that just confirmed the
    /// column was still NULL — see that call site for the race this
    /// guards. `None` is accepted (not just produced) so a future repair
    /// verb can disown a context without a bespoke clear method, mirroring
    /// `update_performer`.
    pub fn set_character_handoff_ctx(
        &self,
        principal_id: PrincipalId,
        ctx: Option<ContextId>,
    ) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE characters SET handoff_ctx = ?1 WHERE principal_id = ?2",
            params![
                ctx.as_ref().map(|c| c.as_bytes().to_vec()),
                blob_param(principal_id.as_bytes())
            ],
        )?;
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!(
                "character {}",
                principal_id.short()
            )));
        }
        Ok(())
    }

    /// Set or clear a character's `root` flag, which marks a model context
    /// whose kj requests always need approval (`docs/character.md`,
    /// "Roots and rotation").
    pub fn update_character_root(&self, principal_id: PrincipalId, root: bool) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE characters SET root = ?1 WHERE principal_id = ?2",
            params![root, blob_param(principal_id.as_bytes())],
        )?;
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!("character {}", principal_id.short())));
        }
        Ok(())
    }

    /// Retire a character: stamp `retired_at`, first time only. Returns
    /// `true` if this call newly retired it, `false` if it was unknown or
    /// already retired — idempotent re-retire is a no-op success, never an
    /// error, since "already retired" and "just retired" leave the same
    /// row behind.
    ///
    /// Does not touch the character's contexts — `kj character retire`
    /// concludes and archives them separately, through the same
    /// `archive_context`/`conclude_context` every other archival path uses.
    /// A context whose senpai's holder has retired refuses reviewer
    /// resolution by name rather than being skipped ([`Self::route_review`]).
    pub fn retire_character(&self, principal_id: PrincipalId, at: i64) -> KernelDbResult<bool> {
        let updated = self.conn.execute(
            "UPDATE characters SET retired_at = ?1
             WHERE principal_id = ?2 AND retired_at IS NULL",
            params![at, blob_param(principal_id.as_bytes())],
        )?;
        Ok(updated > 0)
    }

    /// Resolve a principal to the name a player reads: the two sentinels
    /// first, then the sheet. A principal with a credential but no
    /// `characters` row — the wipe-and-rebind case, or a stranger nobody
    /// minted a sheet for — renders as its id's short form: loud, never
    /// blank, and never a fabricated name (`docs/character.md`, "`auth.db`
    /// is a keyring").
    pub fn name_for(&self, principal_id: PrincipalId) -> String {
        if principal_id == PrincipalId::system() {
            return "system".to_string();
        }
        if principal_id == PrincipalId::beat() {
            return "beat".to_string();
        }
        match self.get_character(principal_id) {
            Ok(Some(row)) => row.name,
            _ => principal_id.short(),
        }
    }

    /// Every live (non-archived) context `principal_id` performs — the set
    /// `kj character retire` archives.
    pub fn contexts_played_by(&self, principal_id: PrincipalId) -> KernelDbResult<Vec<ContextRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, label,
                    system_prompt, context_state,
                    created_at, created_by, forked_from, fork_kind,
                    archived_at, workspace_id, preset_id, concluded_at,
                    last_activity_at, promoted_at, demoted_at, paused_at,
             origin_host, performer, senpai_ctx, persona
             FROM contexts
             WHERE performer = ?1 AND archived_at IS NULL",
        )?;
        let rows = stmt.query_map(params![blob_param(principal_id.as_bytes())], row_to_context_row)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }
}

#[cfg(test)]
mod tests;
