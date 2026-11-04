//! LLM backends, models, aliases, and embedding config.

use super::*;

/// One configured LLM endpoint. `name` is the free-form handle; `kind` is the
/// closed code-owned enum. See the `backends` table comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendRow {
    pub backend_id: BackendId,
    pub name: String,
    pub kind: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub api_key_file: Option<String>,
    pub key_optional: bool,
    pub request_timeout_secs: Option<i64>,
    pub idle_timeout_secs: Option<i64>,
    pub max_concurrent: Option<i64>,
    pub created_at: i64,
    pub created_by: PrincipalId,
}

/// Per-model metadata for one backend (a `backend_models` row).
#[derive(Debug, Clone, PartialEq)]
pub struct BackendModelRow {
    pub backend_id: BackendId,
    pub model_id: String,
    /// Total context window (input + output) in tokens. `None` = unknown,
    /// never a guess.
    pub context_window: Option<i64>,
    /// Sparse provider-specific JSON.
    pub extra: Option<String>,
    /// This model's own tunables on this backend.
    pub tunables: ModelTunablesRow,
}

/// A model's tunables on its backend. A binding copies each into a slot whose
/// own value is `None`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelTunablesRow {
    pub max_tokens: Option<i64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub effort: Option<String>,
    pub thinking_budget: Option<i64>,
    pub thinking_style: Option<String>,
}

/// A `--model` shorthand: alias → (backend, model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelAliasRow {
    pub alias: String,
    pub backend_id: BackendId,
    pub model: String,
}

/// The singleton `embedding_config` row. `backend` names a `backends` row of
/// kind `openai`, which owns the base URL and the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingConfigRow {
    pub enabled: bool,
    pub backend: String,
    pub model: String,
    pub dimensions: i64,
    /// An [`kaijutsu_index::EmbeddingProfile`] token.
    pub profile: String,
    pub timeout_ms: i64,
    pub max_in_flight: i64,
    pub max_context_bytes: i64,
}

pub(super) const BACKEND_COLS: &str = "backend_id, name, kind, base_url, api_key_env, api_key_file, \
                            key_optional, request_timeout_secs, idle_timeout_secs, created_at, \
                            created_by, max_concurrent";

pub(super) fn row_to_backend(row: &rusqlite::Row<'_>) -> SqliteResult<BackendRow> {
    Ok(BackendRow {
        backend_id: read_backend_id(row, 0)?,
        name: row.get(1)?,
        kind: row.get(2)?,
        base_url: row.get(3)?,
        api_key_env: row.get(4)?,
        api_key_file: row.get(5)?,
        key_optional: row.get::<_, i64>(6)? != 0,
        request_timeout_secs: row.get(7)?,
        idle_timeout_secs: row.get(8)?,
        created_at: row.get(9)?,
        created_by: read_principal_id(row, 10)?,
        max_concurrent: row.get(11)?,
    })
}

impl KernelDb {
    /// Insert or update a backend, keyed on its unique `name`. Returns the
    /// row as stored (with the id that was kept or minted).
    ///
    /// Upsert semantics: an existing row with the same name keeps its
    /// `backend_id` — so cast slots and aliases pointing at it survive a
    /// re-`set` — and every other column is replaced by what the caller
    /// passed. There is no partial merge: `kj backend set` is a declaration
    /// of the whole row, and a half-applied config is worse than a loud
    /// re-type.
    pub fn upsert_backend(&self, row: &BackendRow) -> KernelDbResult<BackendRow> {
        validate_label(&row.name)?;
        // `kind` is a closed set and this is the one write path, so the set is
        // enforced here. `BackendRow.kind` is a `String` because the column is,
        // and a kind the code cannot dispatch on must fail at the write that
        // introduces it rather than at the next registry build — by then the
        // row is durable and the caller who typo'd it is gone.
        crate::llm::config::BackendKind::parse(&row.kind).map_err(KernelDbError::Validation)?;
        let existing = self.get_backend_by_name(&row.name)?;
        let backend_id = existing.as_ref().map(|b| b.backend_id).unwrap_or(row.backend_id);
        let created_at = existing.as_ref().map(|b| b.created_at).unwrap_or(row.created_at);
        self.conn.execute(
            "INSERT INTO backends (backend_id, name, kind, base_url, api_key_env, api_key_file,
                                   key_optional, request_timeout_secs, idle_timeout_secs,
                                   created_at, created_by, max_concurrent)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(backend_id) DO UPDATE SET
                 name = excluded.name,
                 kind = excluded.kind,
                 base_url = excluded.base_url,
                 api_key_env = excluded.api_key_env,
                 api_key_file = excluded.api_key_file,
                 key_optional = excluded.key_optional,
                 request_timeout_secs = excluded.request_timeout_secs,
                 idle_timeout_secs = excluded.idle_timeout_secs,
                 max_concurrent = excluded.max_concurrent",
            params![
                blob_param(backend_id.as_bytes()),
                row.name,
                row.kind,
                row.base_url,
                row.api_key_env,
                row.api_key_file,
                row.key_optional as i64,
                row.request_timeout_secs,
                row.idle_timeout_secs,
                created_at,
                blob_param(row.created_by.as_bytes()),
                row.max_concurrent,
            ],
        )?;
        Ok(BackendRow {
            backend_id,
            created_at,
            ..row.clone()
        })
    }

    /// Fetch a backend by its unique name.
    pub fn get_backend_by_name(&self, name: &str) -> KernelDbResult<Option<BackendRow>> {
        let sql = format!("SELECT {BACKEND_COLS} FROM backends WHERE name = ?1");
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt.query_row(params![name], row_to_backend).optional()?)
    }

    /// Fetch a backend by its id.
    pub fn get_backend(&self, backend_id: BackendId) -> KernelDbResult<Option<BackendRow>> {
        let sql = format!("SELECT {BACKEND_COLS} FROM backends WHERE backend_id = ?1");
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt.query_row(params![blob_param(backend_id.as_bytes())], row_to_backend).optional()?)
    }

    /// Whether `backend_id` names a council server (a backend of kind `mk`).
    /// A backend that does not exist is an error.
    pub fn backend_is_council_server(&self, backend_id: BackendId) -> KernelDbResult<bool> {
        let backend = self
            .get_backend(backend_id)?
            .ok_or_else(|| KernelDbError::NotFound(format!("backend {backend_id}")))?;
        Ok(crate::model_resolution::is_council_kind(&backend.kind))
    }

    /// Every backend, ordered by name.
    pub fn list_backends(&self) -> KernelDbResult<Vec<BackendRow>> {
        let sql = format!("SELECT {BACKEND_COLS} FROM backends ORDER BY name");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_backend)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Delete a backend by name.
    ///
    /// Refuses loudly while a character slot, a context's slot or a
    /// model alias still points at it. The FK would reject the DELETE anyway,
    /// but a named list of the referents is the difference between "fix this"
    /// and "SQLITE_CONSTRAINT".
    pub fn delete_backend(&self, name: &str) -> KernelDbResult<()> {
        let Some(backend) = self.get_backend_by_name(name)? else {
            return Err(KernelDbError::NotFound(format!("backend '{name}'")));
        };
        let id = blob_param(backend.backend_id.as_bytes());
        let slots: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT c.name || '/' || s.role FROM character_slots s
                 JOIN characters c ON c.principal_id = s.principal_id
                 WHERE s.backend_id = ?1 ORDER BY 1",
            )?;
            stmt.query_map(params![id], |r| r.get::<_, String>(0))?
                .collect::<SqliteResult<Vec<_>>>()?
        };
        let models: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT c.name FROM character_models m
                 JOIN characters c ON c.principal_id = m.principal_id
                 WHERE m.backend_id = ?1 ORDER BY 1",
            )?;
            stmt.query_map(params![id], |r| r.get::<_, String>(0))?
                .collect::<SqliteResult<Vec<_>>>()?
        };
        let bindings: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT DISTINCT cs.context_id FROM context_slots cs
                 WHERE cs.backend_id = ?1 ORDER BY 1",
            )?;
            stmt.query_map(params![id], |r| Ok(read_context_id(r, 0)?.short()))?
                .collect::<SqliteResult<Vec<_>>>()?
        };
        let aliases: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT alias FROM model_aliases WHERE backend_id = ?1 ORDER BY alias")?;
            stmt.query_map(params![id], |r| r.get::<_, String>(0))?
                .collect::<SqliteResult<Vec<_>>>()?
        };
        if !slots.is_empty() || !models.is_empty() || !aliases.is_empty() || !bindings.is_empty() {
            let mut parts = Vec::new();
            if !slots.is_empty() {
                parts.push(format!("character slots [{}]", slots.join(", ")));
            }
            if !models.is_empty() {
                parts.push(format!("the models of characters [{}]", models.join(", ")));
            }
            if !bindings.is_empty() {
                parts.push(format!("context slots of [{}]", bindings.join(", ")));
            }
            if !aliases.is_empty() {
                parts.push(format!("aliases [{}]", aliases.join(", ")));
            }
            return Err(KernelDbError::Validation(format!(
                "backend '{name}' is still referenced by {} — remove those first",
                parts.join(" and ")
            )));
        }
        self.conn
            .execute("DELETE FROM backends WHERE backend_id = ?1", params![id])?;
        Ok(())
    }

    /// Insert or replace one backend's per-model metadata row.
    pub fn set_backend_model(&self, row: &BackendModelRow) -> KernelDbResult<()> {
        if row.model_id.trim().is_empty() {
            return Err(KernelDbError::Validation(
                "model id must not be empty".to_string(),
            ));
        }
        if let Some(w) = row.context_window
            && w <= 0
        {
            return Err(KernelDbError::Validation(format!(
                "context_window must be a positive integer (got {w}) — 0 is a typo, \
                 and a zero denominator makes every usage gauge garbage"
            )));
        }
        let t = &row.tunables;
        validate_tunables(t.max_tokens, t.temperature, t.top_p, t.thinking_budget, t.thinking_style.as_deref())?;
        self.conn.execute(
            "INSERT INTO backend_models (backend_id, model_id, context_window, extra,
                 max_tokens, temperature, top_p, effort, thinking_budget, thinking_style)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(backend_id, model_id) DO UPDATE SET
                 context_window = excluded.context_window,
                 extra = excluded.extra,
                 max_tokens = excluded.max_tokens,
                 temperature = excluded.temperature,
                 top_p = excluded.top_p,
                 effort = excluded.effort,
                 thinking_budget = excluded.thinking_budget,
                 thinking_style = excluded.thinking_style",
            params![
                blob_param(row.backend_id.as_bytes()),
                row.model_id,
                row.context_window,
                row.extra,
                t.max_tokens,
                t.temperature,
                t.top_p,
                t.effort,
                t.thinking_budget,
                t.thinking_style,
            ],
        )?;
        Ok(())
    }

    /// Remove one backend's per-model metadata row. Returns whether a row went.
    pub fn delete_backend_model(
        &self,
        backend_id: BackendId,
        model_id: &str,
    ) -> KernelDbResult<bool> {
        let n = self.conn.execute(
            "DELETE FROM backend_models WHERE backend_id = ?1 AND model_id = ?2",
            params![blob_param(backend_id.as_bytes()), model_id],
        )?;
        Ok(n > 0)
    }

    /// One backend's per-model metadata rows, ordered by model id.
    pub fn list_backend_models(
        &self,
        backend_id: BackendId,
    ) -> KernelDbResult<Vec<BackendModelRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT model_id, context_window, extra, max_tokens, temperature, top_p,
                    effort, thinking_budget, thinking_style
             FROM backend_models WHERE backend_id = ?1 ORDER BY model_id",
        )?;
        let rows = stmt.query_map(params![blob_param(backend_id.as_bytes())], |row| {
            Ok(BackendModelRow {
                backend_id,
                model_id: row.get(0)?,
                context_window: row.get(1)?,
                extra: row.get(2)?,
                tunables: ModelTunablesRow {
                    max_tokens: row.get(3)?,
                    temperature: row.get(4)?,
                    top_p: row.get(5)?,
                    effort: row.get(6)?,
                    thinking_budget: row.get(7)?,
                    thinking_style: row.get(8)?,
                },
            })
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Insert or replace a `--model` alias.
    pub fn set_model_alias(&self, row: &ModelAliasRow) -> KernelDbResult<()> {
        if row.alias.trim().is_empty() {
            return Err(KernelDbError::Validation("alias must not be empty".into()));
        }
        if row.model.trim().is_empty() {
            return Err(KernelDbError::Validation("model must not be empty".into()));
        }
        self.conn.execute(
            "INSERT INTO model_aliases (alias, backend_id, model) VALUES (?1, ?2, ?3)
             ON CONFLICT(alias) DO UPDATE SET
                 backend_id = excluded.backend_id,
                 model = excluded.model",
            params![
                row.alias,
                blob_param(row.backend_id.as_bytes()),
                row.model
            ],
        )?;
        Ok(())
    }

    /// Remove an alias. Returns whether a row went.
    pub fn delete_model_alias(&self, alias: &str) -> KernelDbResult<bool> {
        let n = self
            .conn
            .execute("DELETE FROM model_aliases WHERE alias = ?1", params![alias])?;
        Ok(n > 0)
    }

    /// Every alias, ordered by name.
    pub fn list_model_aliases(&self) -> KernelDbResult<Vec<ModelAliasRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT alias, backend_id, model FROM model_aliases ORDER BY alias")?;
        let rows = stmt.query_map([], |row| {
            Ok(ModelAliasRow {
                alias: row.get(0)?,
                backend_id: read_backend_id(row, 1)?,
                model: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// The singleton embedding row, or `None` when never seeded.
    pub fn get_embedding_config(&self) -> KernelDbResult<Option<EmbeddingConfigRow>> {
        Ok(self.conn.query_row(
            "SELECT enabled, backend, model, dimensions, profile, timeout_ms, max_in_flight, max_context_bytes
             FROM embedding_config WHERE id = 1",
            [], |row| Ok(EmbeddingConfigRow {
                enabled: row.get::<_, i64>(0)? != 0, backend: row.get(1)?, model: row.get(2)?,
                dimensions: row.get(3)?, profile: row.get(4)?,
                timeout_ms: row.get(5)?, max_in_flight: row.get(6)?, max_context_bytes: row.get(7)?,
            }),
        ).optional()?)
    }

    /// Replace the singleton embedding service configuration.
    pub fn set_embedding_config(&self, row: &EmbeddingConfigRow) -> KernelDbResult<()> {
        if row.backend.trim().is_empty() || row.model.trim().is_empty() {
            return Err(KernelDbError::Validation("embedding backend and model must be nonempty".into()));
        }
        if row.dimensions <= 0 || row.timeout_ms <= 0 || row.max_in_flight <= 0 || row.max_context_bytes <= 0 {
            return Err(KernelDbError::Validation("embedding dimensions, timeout_ms, max_in_flight and max_context_bytes must be positive".into()));
        }
        row.profile.parse::<kaijutsu_index::EmbeddingProfile>().map_err(KernelDbError::Validation)?;
        self.conn.execute(
            "INSERT INTO embedding_config (id, enabled, backend, model, dimensions, profile, timeout_ms, max_in_flight, max_context_bytes)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET enabled = excluded.enabled, backend = excluded.backend,
                 model = excluded.model, dimensions = excluded.dimensions, profile = excluded.profile,
                 timeout_ms = excluded.timeout_ms, max_in_flight = excluded.max_in_flight,
                 max_context_bytes = excluded.max_context_bytes",
            params![row.enabled as i64, row.backend, row.model, row.dimensions, row.profile,
                row.timeout_ms, row.max_in_flight, row.max_context_bytes],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
