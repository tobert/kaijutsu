//! Test-util context-character fixtures.

use super::*;

impl KernelDb {
    /// Test fixture: a live root character with a root console, made once.
    /// For tests that create a context with no performer (a handoff log,
    /// scratch, lost+found), which answers to the only live root's console.
    #[cfg(any(test, feature = "test-util"))]
    pub fn seed_test_root_console(&self) -> KernelDbResult<(PrincipalId, ContextId)> {
        if let Some(sheet) = self.list_characters(false)?.into_iter().find(|sheet| sheet.root && sheet.root_ctx.is_some()) {
            return Ok((sheet.principal_id, sheet.root_ctx.expect("filtered on root_ctx")));
        }
        let principal_id = PrincipalId::new();
        self.insert_character(&CharacterRow {
            principal_id,
            name: format!("root-fixture-{}", principal_id.short()),
            created_at: 0,
            retired_at: None,
            handoff_ctx: None,
            root: true,
            root_ctx: None,
            context_type: None,
        })?;
        let workspace = self.get_or_create_default_workspace(principal_id)?;
        let console = ContextId::new();
        self.insert_context_with_document(
            &ContextRow {
                context_id: console, label: Some(format!("root-fixture-{}", console.short())), system_prompt: None,
                context_state: ContextState::Live, context_type: kaijutsu_types::ROOT_CONTEXT_TYPE.into(), created_at: now_millis(),
                created_by: principal_id, forked_from: None, fork_kind: None, archived_at: None, workspace_id: None,
                preset_id: None, concluded_at: None, last_activity_at: None, promoted_at: None, demoted_at: None,
                paused_at: None, origin_host: None, performer: None, senpai_ctx: None, persona: None,
            },
            workspace,
        )?;
        self.write_context_character(console, principal_id, None, None)?;
        self.set_character_root_ctx(principal_id, Some(console))?;
        Ok((principal_id, console))
    }

    /// Test fixture: record that `context` should answer to `senpai`.
    /// [`Self::update_performer`] gives the context character this senpai when
    /// it writes it, and a context that already has one is moved at once, so a
    /// fixture may build a chain in either order. The note lives in a
    /// temporary table of this connection.
    #[cfg(any(test, feature = "test-util"))]
    pub fn note_senpai_intent(&self, context: ContextId, senpai: Option<ContextId>) -> KernelDbResult<()> {
        self.conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS test_senpai_intent (context_id BLOB NOT NULL PRIMARY KEY, senpai_ctx BLOB)",
        )?;
        self.conn.execute(
            "INSERT OR REPLACE INTO test_senpai_intent (context_id, senpai_ctx) VALUES (?1, ?2)",
            params![blob_param(context.as_bytes()), senpai.as_ref().map(|id| id.as_bytes().to_vec())],
        )?;
        if self.context_character(context)?.is_some() {
            // A `None` makes a top that is no root console, which the guard
            // refuses; a fixture builds one by lifting the guard for the write.
            self.conn.execute_batch("DROP TRIGGER IF EXISTS contexts_senpai_required")?;
            let written = self.conn.execute(
                "UPDATE contexts SET senpai_ctx = ?1 WHERE context_id = ?2",
                params![senpai.as_ref().map(|id| id.as_bytes().to_vec()), blob_param(context.as_bytes())],
            );
            Self::ensure_context_guards(&self.conn)?;
            written?;
        }
        Ok(())
    }

    #[cfg(any(test, feature = "test-util"))]
    pub(super) fn has_senpai_intent_table(&self) -> KernelDbResult<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_temp_master WHERE type = 'table' AND name = 'test_senpai_intent')",
            [], |r| r.get(0),
        )?)
    }

    #[cfg(any(test, feature = "test-util"))]
    pub(super) fn senpai_intent(&self, context: ContextId) -> KernelDbResult<Option<ContextId>> {
        if !self.has_senpai_intent_table()? {
            return Ok(None);
        }
        Ok(self.conn
            .query_row(
                "SELECT senpai_ctx FROM test_senpai_intent WHERE context_id = ?1",
                params![blob_param(context.as_bytes())],
                |r| read_opt_context_id(r, 0),
            )
            .optional()?
            .flatten())
    }

    /// Test fixture: give `context` a context character of `played_by`, the
    /// way creation does, under the senpai [`Self::note_senpai_intent`] named.
    /// A context that already has this character's context character is left
    /// alone; a different character rewrites it (see
    /// [`Self::recast_test_character`]) and `None` refuses.
    #[cfg(any(test, feature = "test-util"))]
    pub fn update_performer(
        &self,
        id: ContextId,
        played_by: Option<PrincipalId>,
    ) -> KernelDbResult<()> {
        let Some(played_by) = played_by else {
            return Err(KernelDbError::Validation(
                "a context character never changes its character, so a fixture cannot clear a performer".into(),
            ));
        };
        self.assign_context_character(id, played_by)?;
        Ok(())
    }

    /// Test fixture: rewrite the character of `id`'s context character to
    /// `played_by`. No kernel path does this: a context's character never
    /// changes. For wire tests that make a context through `kj context create`
    /// and then need another character to perform it. The trigger that fixes
    /// the performer is dropped for the update and put back.
    #[cfg(any(test, feature = "test-util"))]
    pub fn recast_test_character(&self, id: ContextId, played_by: PrincipalId) -> KernelDbResult<()> {
        self.context_character(id)?
            .ok_or_else(|| KernelDbError::Validation(format!("context {} has no context character to recast", id.short())))?;
        self.conn.execute_batch("DROP TRIGGER IF EXISTS contexts_performer_fixed")?;
        let updated = self.conn.execute(
            "UPDATE contexts SET performer = ?1 WHERE context_id = ?2",
            params![blob_param(played_by.as_bytes()), blob_param(id.as_bytes())],
        );
        Self::ensure_context_guards(&self.conn)?;
        updated?;
        Ok(())
    }

    /// [`Self::assign_test_review`] after recasting a context character of
    /// another character to `played_by` (see [`Self::recast_test_character`]).
    #[cfg(any(test, feature = "test-util"))]
    pub fn assign_test_review_recasting(
        &self,
        context: ContextId,
        played_by: Option<PrincipalId>,
        reviewer: Option<PrincipalId>,
    ) -> KernelDbResult<()> {
        if let (Some(played_by), Some(character)) = (played_by, self.context_character(context)?)
            && character.principal_id != played_by
        {
            self.recast_test_character(context, played_by)?;
        }
        self.assign_test_review(context, played_by, reviewer)
    }

    /// Test fixture: make `character` run `model` on `backend` for each of
    /// `roles`. See [`Self::seed_test_slot_entries`].
    #[cfg(any(test, feature = "test-util"))]
    pub fn seed_test_slots(
        &self,
        character: PrincipalId,
        backend: &str,
        model: &str,
        roles: &[&str],
    ) -> KernelDbResult<()> {
        let entries: Vec<(&str, &str, &str)> = roles.iter().map(|role| (*role, backend, model)).collect();
        self.seed_test_slot_entries(character, &entries)
    }

    /// Test fixture: give `character` exactly these slots, one per
    /// `(role, backend, model)`, replacing the slots it had. A backend row is
    /// created (kind `mock`, no key) when absent, and each slot sets
    /// `max_tokens`. A context character written after this copies the slots;
    /// one written before keeps the slots it has.
    #[cfg(any(test, feature = "test-util"))]
    pub fn seed_test_slot_entries(
        &self,
        character: PrincipalId,
        entries: &[(&str, &str, &str)],
    ) -> KernelDbResult<()> {
        for existing in self.list_character_slots(character)? {
            self.delete_character_slot(character, &existing.role)?;
        }
        for (role, backend, model) in entries {
            let backend_row = match self.get_backend_by_name(backend)? {
                Some(row) => row,
                None => {
                    let row = BackendRow {
                        backend_id: BackendId::new(),
                        name: backend.to_string(),
                        kind: "mock".to_string(),
                        base_url: None,
                        api_key_env: None,
                        api_key_file: None,
                        key_optional: true,
                        request_timeout_secs: None,
                        idle_timeout_secs: None,
                        max_concurrent: None,
                        created_at: 0,
                        created_by: PrincipalId::system(),
                    };
                    self.upsert_backend(&row)?;
                    row
                }
            };
            self.set_character_slot(&CharacterSlotRow {
                principal_id: character,
                role: role.to_string(),
                backend_id: backend_row.backend_id,
                model: model.to_string(),
                max_tokens: Some(4096),
                temperature: None,
                top_p: None,
                effort: None,
                thinking_budget: None,
                thinking_style: None,
            })?;
        }
        Ok(())
    }

    /// Test fixture: give `id` a stand-in context character of `played_by`,
    /// which a later [`Self::update_performer`] or [`Self::assign_test_review`]
    /// naming another character replaces. For helpers that register a context
    /// before a test says who runs it.
    #[cfg(any(test, feature = "test-util"))]
    pub fn assign_stand_in_character(&self, id: ContextId, played_by: PrincipalId) -> KernelDbResult<()> {
        let existed = self.context_character(id)?.is_some();
        self.assign_context_character(id, played_by)?;
        if !existed {
            self.conn.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS test_stand_in_character (context_id BLOB NOT NULL PRIMARY KEY)",
            )?;
            self.conn.execute(
                "INSERT OR IGNORE INTO test_stand_in_character (context_id) VALUES (?1)",
                params![blob_param(id.as_bytes())],
            )?;
        }
        Ok(())
    }

    /// Whether `id`'s context character is a stand-in a fixture may replace.
    #[cfg(any(test, feature = "test-util"))]
    pub(super) fn is_stand_in_character(&self, id: ContextId) -> KernelDbResult<bool> {
        let table: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_temp_master WHERE type = 'table' AND name = 'test_stand_in_character')",
            [], |r| r.get(0),
        )?;
        if !table {
            return Ok(false);
        }
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM test_stand_in_character WHERE context_id = ?1)",
            params![blob_param(id.as_bytes())],
            |r| r.get(0),
        )?)
    }

    /// Write `id`'s context character as creation does. A root character's
    /// first context with no senpai becomes its console. An existing context
    /// character of another character is rewritten.
    #[cfg(any(test, feature = "test-util"))]
    pub(super) fn assign_context_character(&self, id: ContextId, played_by: PrincipalId) -> KernelDbResult<ContextCharacterRow> {
        self.get_context(id)?
            .ok_or_else(|| KernelDbError::NotFound(format!("context {}", id.short())))?;
        if let Some(existing) = self.context_character(id)? {
            if existing.principal_id == played_by {
                return Ok(existing);
            }
            if !self.is_stand_in_character(id)? {
                return Err(KernelDbError::Validation(format!(
                    "context {} already has a context character of another character, which never changes",
                    id.short()
                )));
            }
            self.recast_test_character(id, played_by)?;
            self.conn.execute(
                "DELETE FROM test_stand_in_character WHERE context_id = ?1",
                params![blob_param(id.as_bytes())],
            )?;
            return Ok(self.require_context_character(id)?);
        }
        let senpai = self.senpai_intent(id)?;
        let row = self.write_context_character(id, played_by, senpai, None)?;
        if senpai.is_none()
            && let Some(sheet) = self.get_character(played_by)?
            && sheet.root
            && sheet.root_ctx.is_none()
        {
            self.set_character_root_ctx(played_by, Some(id))?;
        }
        Ok(row)
    }

    /// Test fixture: make `context` performed by `played_by` and reviewed by
    /// `reviewer`: the reviewer holds a seat context (created on first use)
    /// with its own context character, and the context is placed under the
    /// seat, so routing follows the senpai edge like production. With no
    /// reviewer only the performer is set; with no performer a stand-in
    /// character runs a context that has no context character yet.
    #[cfg(any(test, feature = "test-util"))]
    pub fn assign_test_review(
        &self,
        context: ContextId,
        played_by: Option<PrincipalId>,
        reviewer: Option<PrincipalId>,
    ) -> KernelDbResult<()> {
        // A context routes through its context character, so one with no
        // named performer is run by a stand-in character.
        match (played_by, self.context_character(context)?) {
            (Some(played_by), _) => { self.assign_context_character(context, played_by)?; }
            (None, None) => { self.assign_context_character(context, PrincipalId::new())?; }
            (None, Some(_)) => {}
        }
        let Some(reviewer) = reviewer else { return Ok(()) };
        let label = format!("review-seat-{}", reviewer.to_hex());
        let seat = match self.find_context_by_label(&label)? {
            Some(seat) => seat.context_id,
            None => {
                let seat = ContextId::new();
                let ws = self.get_or_create_default_workspace(reviewer)?;
                let row = ContextRow {
                    context_id: seat, label: Some(label), system_prompt: None,
                    context_state: ContextState::Live, context_type: "default".into(),
                    created_at: now_millis(), created_by: reviewer, forked_from: None, fork_kind: None,
                    archived_at: None, workspace_id: None, preset_id: None, concluded_at: None,
                    last_activity_at: None, promoted_at: None, demoted_at: None, paused_at: None,
                    origin_host: None, performer: None, senpai_ctx: None, persona: None,
                };
                // The reviewer's own senpai is where the context answered
                // before, so the climb continues where the old walk did.
                let previous = self.senpai_of(context)?;
                self.insert_context_with_document(&row, ws)?;
                self.assign_context_character(seat, reviewer)?;
                if let Some(previous) = previous {
                    self.set_context_senpai(seat, Some(previous))?;
                }
                seat
            }
        };
        self.set_context_senpai(context, Some(seat))
    }
}

