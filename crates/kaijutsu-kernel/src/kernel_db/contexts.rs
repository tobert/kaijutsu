//! The contexts table.

use super::*;

/// A context row — superset of `Context` + `ContextHandle`.
#[derive(Debug, Clone)]
pub struct ContextRow {
    pub context_id: ContextId,
    pub label: Option<String>,
    pub system_prompt: Option<String>,
    pub context_state: ContextState,
    pub context_type: String,
    pub created_at: i64,
    pub created_by: PrincipalId,
    pub forked_from: Option<ContextId>,
    pub fork_kind: Option<ForkKind>,
    pub archived_at: Option<i64>,
    /// Unix-millis of the explicit `conclude` act, or `None` if still open.
    pub concluded_at: Option<i64>,
    pub workspace_id: Option<WorkspaceId>,
    pub preset_id: Option<PresetId>,
    /// Unix-millis of the most recent block append/mutation on this context,
    /// or `None` if never touched since creation (or a pre-migration row).
    /// Stamped by `touch_context_activity`, called from `BlockStore::journal_op`
    /// — the one chokepoint every mutating block op funnels through.
    pub last_activity_at: Option<i64>,
    /// Unix-millis of the explicit ring-0 ("active") promote, or `None` if
    /// never promoted. First-write-wins: re-promoting an already-promoted
    /// context is a no-op that preserves this timestamp, so ring-0 order
    /// stays append-stable. Mutually exclusive with `demoted_at` — setting
    /// one clears the other. Cleared by `conclude_context` (gives up the
    /// ring-0 seat) and by `archive_context`.
    pub promoted_at: Option<i64>,
    /// Unix-millis of the explicit push to the demoted ring, or `None` if
    /// never demoted. Mutually exclusive with `promoted_at`. Cleared by
    /// `archive_context`. See the demote ladder in `demote_context`.
    pub demoted_at: Option<i64>,
    /// Unix-millis of the explicit "suspend activity" flag, or `None` if not
    /// paused. Design-only for now: persisted and exposed on the wire, but
    /// not yet wired to any behavioral gating. Intended semantics once wired:
    /// a paused context receives no beat/OODA wakeups (seam: hyoushigi
    /// attachment wakeup fire) and rejects turn-starts loudly (seam: kernel
    /// turn-start).
    pub paused_at: Option<i64>,
    /// Advisory hostname the registering client self-reported (`gethostname`
    /// on the kaijutsu-mcp side today), or `None` when unknown — an old
    /// client that never called `setContextOriginHost`, a creation path with
    /// no client to ask (genesis bootstrap, fork), or a pre-migration row.
    /// Set ONCE, right after creation, via `KernelDb::set_origin_host` (never
    /// baked into `write_context`'s own INSERT) — mirrors `created_by`: an
    /// origin fact, not a "last seen from" one, so a later reconnect from a
    /// different machine does NOT overwrite it. Shared-trust model: this is
    /// observability, not auth — the server trusts the client's self-report.
    /// Rendered as `"-"` when `None` (`kj context info`,
    /// `kj context list`).
    pub origin_host: Option<String>,
    /// The character whose performance this context is, or `None` for a
    /// context that runs no turns (a score context, a file document, the
    /// handoff log, the MCP hub). Fixed once set. See
    /// `docs/senpai-kohai.md`, "The model".
    pub performer: Option<PrincipalId>,
    /// The context this one answers to. `None` only for a character's root
    /// console (`docs/senpai-kohai.md`, "The model").
    pub senpai_ctx: Option<ContextId>,
    /// The performer's persona for this context; `None` until characters have one.
    pub persona: Option<String>,
}

impl ContextRow {
    /// Whether this context is archived, and therefore inert: it runs
    /// nothing and answers nothing.
    ///
    /// `archived_at` is authoritative per `ContextState`'s own doc comment,
    /// so it is checked ahead of the enum. The kernel writes both together —
    /// `archive_context` stamps the timestamp and the state, and resurrection
    /// clears both — so this reads two fields that agree. It stays one
    /// predicate anyway: a caller that checks a single field is a caller that
    /// treats a dead context as live if they ever part again.
    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some() || self.context_state == ContextState::Archived
    }

    /// Whether this is a model console (type `console`). Not [`KernelDb::is_root_console`], which asks
    /// whether a context is a character's root console (type `root`).
    pub fn is_console(&self) -> bool {
        self.context_type == kaijutsu_types::CONSOLE_CONTEXT_TYPE
    }

    /// Convert to the lightweight `kaijutsu_types::Context`.
    pub fn to_context(&self) -> kaijutsu_types::Context {
        kaijutsu_types::Context {
            id: self.context_id,
            label: self.label.clone(),
            forked_from: self.forked_from,
            created_by: self.created_by,
            created_at: self.created_at as u64,
        }
    }
}

impl KernelDb {
    /// Insert a document + context pair in one call (convenience for conversations).
    ///
    /// Creates the document row first, then the context row. Uses the
    /// default workspace if no workspace_id is set on the context.
    pub fn insert_context_with_document(
        &self,
        row: &ContextRow,
        default_workspace_id: WorkspaceId,
    ) -> KernelDbResult<()> {
        let ws_id = row.workspace_id.unwrap_or(default_workspace_id);
        self.insert_document_or_ignore(&DocumentRow {
            document_id: row.context_id,
            workspace_id: ws_id,
            doc_kind: DocKind::Conversation,
            language: None,
            path: None,
            created_at: row.created_at,
            created_by: row.created_by,
        })?;
        self.insert_context(row)
    }

    /// [`Self::insert_context_with_document`] for a creator that places the
    /// context as it is made: the row carries its `senpai_ctx`, and a row with
    /// none is refused unless its performer is a root character (a root
    /// console; `characters.root_ctx` is pointed at it after the insert).
    /// Every production creator of a context goes through this check or
    /// through `insert_new_context_rows`, which applies it too
    /// ([`Self::check_senpai_edge`]).
    pub fn insert_placed_context(&self, row: &ContextRow, default_workspace_id: WorkspaceId) -> KernelDbResult<()> {
        self.check_senpai_edge(row.context_id, &row.context_type, row.performer, row.senpai_ctx)?;
        self.insert_context_with_document(row, default_workspace_id)
    }

    /// Insert a new context.
    ///
    /// The corresponding document row must already exist (FK enforced).
    pub fn insert_context(&self, row: &ContextRow) -> KernelDbResult<()> {
        Self::write_context(&self.conn, row)
    }

    /// Insert a context row against `conn` (a `&Connection` or a
    /// `&Transaction` via deref). Shared by `insert_context` and the
    /// transactional `insert_forked_context`. Does NOT commit.
    pub(super) fn write_context(conn: &Connection, row: &ContextRow) -> KernelDbResult<()> {
        if let Some(ref label) = row.label {
            validate_label(label)?;
        }
        conn.execute(
                "INSERT INTO contexts (
                context_id, label,
                system_prompt, context_state, context_type,
                created_at, created_by, forked_from, fork_kind,
                archived_at, workspace_id, preset_id, concluded_at,
                last_activity_at, promoted_at, demoted_at, paused_at,
                origin_host, performer, senpai_ctx, persona
            ) VALUES (
                ?1, ?2,
                ?3, ?4, ?5,
                ?6, ?7, ?8, ?9,
                ?10, ?11, ?12, ?13,
                ?14, ?15, ?16, ?17,
                ?18, ?19, ?20, ?21
            )",
                params![
                    blob_param(row.context_id.as_bytes()),
                    row.label,
                    row.system_prompt,
                    row.context_state.as_str(),
                    row.context_type,
                    row.created_at,
                    blob_param(row.created_by.as_bytes()),
                    row.forked_from.as_ref().map(|id| id.as_bytes().to_vec()),
                    row.fork_kind.map(|fk| fk.as_str().to_string()),
                    row.archived_at,
                    row.workspace_id.as_ref().map(|id| id.as_bytes().to_vec()),
                    row.preset_id.as_ref().map(|id| id.as_bytes().to_vec()),
                    row.concluded_at,
                    row.last_activity_at,
                    row.promoted_at,
                    row.demoted_at,
                    row.paused_at,
                    row.origin_host,
                    row.performer.as_ref().map(|id| id.as_bytes().to_vec()),
                    row.senpai_ctx.as_ref().map(|id| id.as_bytes().to_vec()),
                    row.persona,
                ],
            )
            .map_err(|e| {
                if let Some(ref label) = row.label {
                    map_unique_violation(e, format!("label '{}' already in use", label))
                } else {
                    KernelDbError::Db(e)
                }
            })?;
        Ok(())
    }

    /// Get a context by ID.
    pub fn get_context(&self, id: ContextId) -> KernelDbResult<Option<ContextRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, label,
                    system_prompt, context_state, context_type,
                    created_at, created_by, forked_from, fork_kind,
                    archived_at, workspace_id, preset_id, concluded_at,
                    last_activity_at, promoted_at, demoted_at, paused_at, origin_host, performer, senpai_ctx, persona
             FROM contexts WHERE context_id = ?1",
        )?;

        let mut rows = stmt.query(params![blob_param(id.as_bytes())])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_context_row(row)?))
        } else {
            Ok(None)
        }
    }

    /// Update a context's label.
    pub fn update_label(&self, id: ContextId, label: Option<&str>) -> KernelDbResult<()> {
        if let Some(l) = label {
            validate_label(l)?;
        }

        let updated = self
            .conn
            .execute(
                "UPDATE contexts SET label = ?1 WHERE context_id = ?2",
                params![label, blob_param(id.as_bytes())],
            )
            .map_err(|e| {
                if let Some(l) = label {
                    map_unique_violation(e, format!("label '{}' already in use", l))
                } else {
                    KernelDbError::Db(e)
                }
            })?;

        if updated == 0 {
            return Err(KernelDbError::NotFound(format!("context {}", id.short())));
        }
        Ok(())
    }

    /// Update a context's settings.
    pub fn update_settings(
        &self,
        id: ContextId,
        system_prompt: Option<&str>,
    ) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE contexts SET system_prompt = ?1
             WHERE context_id = ?2",
            params![system_prompt, blob_param(id.as_bytes())],
        )?;
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!("context {}", id.short())));
        }
        Ok(())
    }

    /// Update a context's `context_type` — selects which rc lifecycle
    /// scripts run for this context's create / fork / attach / drift
    /// moments.
    pub fn update_context_type(&self, id: ContextId, context_type: &str) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE contexts SET context_type = ?1 WHERE context_id = ?2",
            params![context_type, blob_param(id.as_bytes())],
        )?;
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!("context {}", id.short())));
        }
        Ok(())
    }

    /// Set (or clear) a context's advisory `origin_host` — the hostname the
    /// registering client self-reported. Deliberately NOT part of
    /// `write_context`'s own INSERT: it is set once, right after creation,
    /// by the `setContextOriginHost` RPC handler, so creation paths with no
    /// client to ask (genesis bootstrap, fork, tests) simply never call this
    /// and the row keeps `origin_host = NULL` ("-" on display). `host =
    /// None` clears an existing value. Unknown context id is a loud
    /// `NotFound`, same convention as `update_label`/`update_context_type`.
    pub fn set_origin_host(&self, id: ContextId, host: Option<&str>) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE contexts SET origin_host = ?1 WHERE context_id = ?2",
            params![host, blob_param(id.as_bytes())],
        )?;
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!("context {}", id.short())));
        }
        Ok(())
    }

    /// Update a context's workspace assignment.
    pub fn update_workspace(
        &self,
        id: ContextId,
        ws_id: Option<WorkspaceId>,
    ) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE contexts SET workspace_id = ?1 WHERE context_id = ?2",
            params![
                ws_id.as_ref().map(|id| id.as_bytes().to_vec()),
                blob_param(id.as_bytes()),
            ],
        )?;
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!("context {}", id.short())));
        }
        Ok(())
    }

    /// Stamp `last_activity_at` on a context — called from
    /// `BlockStore::journal_op`, the one chokepoint every mutating block op
    /// (insert, status change, text edit...) funnels through. Unlike
    /// `concluded_at` (first-write-wins), this re-stamps on every call: it is
    /// the kernel-truth "recency" fact the time-well vortex orders by.
    ///
    /// `ts` must be the same clock/epoch as `created_at` (Unix millis via
    /// `now_millis()` / `kaijutsu_types::now_millis()`) — callers compute
    /// `now − last_activity_at` directly against it, so a unit mismatch
    /// corrupts idle-age math.
    pub fn touch_context_activity(&self, context_id: ContextId, ts: i64) -> KernelDbResult<()> {
        self.conn.execute(
            "UPDATE contexts SET last_activity_at = ?2 WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes()), ts],
        )?;
        Ok(())
    }

    /// List active (non-archived) contexts.
    pub fn list_active_contexts(&self) -> KernelDbResult<Vec<ContextRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, label,
                    system_prompt, context_state, context_type,
                    created_at, created_by, forked_from, fork_kind,
                    archived_at, workspace_id, preset_id, concluded_at,
                    last_activity_at, promoted_at, demoted_at, paused_at, origin_host, performer, senpai_ctx, persona
             FROM contexts
             WHERE archived_at IS NULL
             ORDER BY COALESCE(last_activity_at, created_at)",
        )?;

        let rows = stmt.query_map([], row_to_context_row)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// List all contexts (including archived).
    pub fn list_all_contexts(&self) -> KernelDbResult<Vec<ContextRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, label,
                    system_prompt, context_state, context_type,
                    created_at, created_by, forked_from, fork_kind,
                    archived_at, workspace_id, preset_id, concluded_at,
                    last_activity_at, promoted_at, demoted_at, paused_at, origin_host, performer, senpai_ctx, persona
             FROM contexts
             ORDER BY COALESCE(last_activity_at, created_at)",
        )?;

        let rows = stmt.query_map([], row_to_context_row)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Resolve a context query string.
    ///
    /// Supports exact label, label prefix, hex prefix. For tag:prefix syntax
    /// (future), walks lineage via CTE.
    ///
    /// Fuzzy forms (label, prefix) resolve over the ACTIVE set only — an
    /// archived context never shadows a live label or shows up in prefix
    /// ambiguity. A FULL id (32-char hex or hyphenated UUID) resolves
    /// against the whole table, archived included: the exact id is the
    /// resurrection handle (`kj context promote <full-id>` must be able to
    /// reach an archived context; promote is the resurrection door).
    pub fn resolve_context(&self, query: &str) -> KernelDbResult<ContextId> {
        if let Ok(full) = ContextId::parse(query)
            && self.get_context(full)?.is_some()
        {
            return Ok(full);
        }

        // Load active contexts (set is small, <100)
        let contexts = self.list_active_contexts()?;
        let items = contexts.iter().map(|c| (c.context_id, c.label.as_deref()));

        kaijutsu_types::resolve_prefix(items, query).map_err(|e| match e {
            kaijutsu_types::PrefixError::NoMatch(q) => {
                KernelDbError::NotFound(format!("no context matches '{}'", q))
            }
            kaijutsu_types::PrefixError::Ambiguous { prefix, candidates } => {
                KernelDbError::Validation(format!(
                    "ambiguous prefix '{}': matches {:?}",
                    prefix, candidates
                ))
            }
        })
    }

    /// Find the live context that currently holds a given label. Archived
    /// contexts keep their label but never hold it: `None` when only
    /// archived rows carry it, so the label is free to give again.
    pub fn find_context_by_label(&self, label: &str) -> KernelDbResult<Option<ContextRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, label,
                    system_prompt, context_state, context_type,
                    created_at, created_by, forked_from, fork_kind,
                    archived_at, workspace_id, preset_id, concluded_at,
                    last_activity_at, promoted_at, demoted_at, paused_at, origin_host, performer, senpai_ctx, persona
             FROM contexts WHERE label = ?1 AND archived_at IS NULL",
        )?;

        let mut rows = stmt.query(params![label])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_context_row(row)?))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests;
