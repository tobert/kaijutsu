//! Build an [`LlmRegistry`] from the kernel database.
//!
//! This replaces the demolished `toml_config.rs` (models.toml → `LlmConfig` →
//! `initialize_llm_registry`). SQL is the source of truth; there is no TOML in
//! the model-config path at all.
//!
//! ## Live reload
//!
//! The registry is a **snapshot**, rebuilt and swapped wholesale. The kernel
//! holds it as `RwLock<LlmRegistry>` (`Kernel::llm()`), so every `kj
//! backend`/`kj character slot`/`kj alias` mutation ends with
//! `KjDispatcher::reload_llm_registry()`, which calls [`build_llm_registry`]
//! and assigns through the write guard. A config change therefore takes effect
//! on the next turn, with no kernel restart — and readers on the turn path
//! keep taking a cheap read lock over plain maps rather than reaching into
//! SQLite per request.
//!
//! Chose swap-the-snapshot over read-through because the alternative would put
//! a `Mutex<KernelDb>` acquisition inside every `resolve_model` /
//! `context_window_for` call (both of which run inside the async LLM path
//! while a `parking_lot` mutex is decidedly not async-friendly), and because
//! a torn read across several config tables mid-turn is a class of bug that
//! simply cannot happen when the whole thing is built at once.

use std::collections::HashMap;
use std::sync::Arc;

use crate::kernel_db::KernelDb;
use crate::llm::endpoint::Endpoints;

use kaijutsu_index::EmbeddingProfile;
use super::config::{BackendConfig, BackendKind, EmbeddingModelConfig, ModelAlias, ModelInfo, SlotTunables};
use super::{LlmError, LlmRegistry, LlmResult, Provider};

/// Read every `backends` row (plus its `backend_models`) into
/// [`BackendConfig`]s.
///
/// A row whose `kind` is not a known [`BackendKind`] is a hard error, not a
/// skip: the write path refuses unknown kinds, so one in the table means the
/// DB was edited behind our back or the code lost a variant — either way,
/// silently dropping the backend is how a turn ends up hanging on a provider
/// that "was configured".
pub fn load_backends(db: &KernelDb) -> LlmResult<Vec<BackendConfig>> {
    let rows = db
        .list_backends()
        .map_err(|e| LlmError::InvalidRequest(format!("reading backends: {e}")))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let kind = BackendKind::parse(&row.kind).map_err(|msg| {
            LlmError::InvalidRequest(format!("backend '{}': {msg}", row.name))
        })?;
        let model_rows = db
            .list_backend_models(row.backend_id)
            .map_err(|e| LlmError::InvalidRequest(format!("reading backend_models: {e}")))?;
        let mut models = HashMap::new();
        for m in model_rows {
            models.insert(
                m.model_id,
                ModelInfo {
                    // The schema CHECK and the write path both refuse a
                    // non-positive window, so a negative here would be
                    // corruption; treat it as unknown rather than casting it
                    // into a nonsense u64.
                    context_window: m.context_window.and_then(|w| u64::try_from(w).ok()),
                    extra: m.extra,
                    tunables: SlotTunables {
                        max_tokens: m.tunables.max_tokens.and_then(|v| u64::try_from(v).ok()),
                        temperature: m.tunables.temperature,
                        top_p: m.tunables.top_p,
                        effort: m.tunables.effort,
                        thinking_budget: m.tunables.thinking_budget.and_then(|v| u64::try_from(v).ok()),
                        thinking_style: m.tunables.thinking_style,
                    },
                },
            );
        }
        out.push(BackendConfig {
            name: row.name,
            kind,
            base_url: row.base_url,
            api_key_env: row.api_key_env,
            api_key_file: row.api_key_file,
            key_optional: row.key_optional,
            request_timeout_secs: row.request_timeout_secs.and_then(|s| u64::try_from(s).ok()),
            idle_timeout_secs: row.idle_timeout_secs.and_then(|s| u64::try_from(s).ok()),
            max_concurrent: row.max_concurrent.and_then(|n| u32::try_from(n).ok()),
            models,
        });
    }
    Ok(out)
}

/// Build a fresh [`LlmRegistry`] from the DB.
///
/// Registers one client per backend row, keyed by backend NAME. A backend that
/// fails to construct (no resolvable key, say) is warned about and skipped; a
/// turn bound to it refuses, naming the backend. Nothing here chooses a
/// model: a context character's slots do.
///
/// Each client takes its slots from `endpoints`, the kernel's one set, whose
/// limits this rebuild sets from the backends that built. A skipped backend
/// sets no limit.
pub fn build_llm_registry(db: &KernelDb, endpoints: &Endpoints) -> LlmResult<LlmRegistry> {
    let backends = load_backends(db)?;
    let mut registry = LlmRegistry::new();
    let mut built = Vec::new();

    for backend in &backends {
        match Provider::from_backend(backend, endpoints) {
            Ok(provider) => {
                built.push(backend.clone());
                tracing::info!(
                    backend = %backend.name,
                    kind = %backend.kind.as_str(),
                    "registered LLM backend"
                );
                registry.register(&backend.name, Arc::new(provider));
            }
            // Only an actual credential failure should guess "missing API
            // key?" — anything else names its own cause.
            Err(e @ LlmError::AuthError(_)) => {
                tracing::warn!(
                    backend = %backend.name,
                    error = %e,
                    "failed to initialize backend (missing API key?)"
                );
            }
            Err(e) => {
                tracing::warn!(backend = %backend.name, error = %e, "failed to initialize backend");
            }
        }
    }

    endpoints.configure(&built);
    registry.set_backends(backends);

    // ── aliases ─────────────────────────────────────────────────────────
    let backend_names: HashMap<_, _> = db
        .list_backends()
        .map_err(|e| LlmError::InvalidRequest(format!("reading backends: {e}")))?
        .into_iter()
        .map(|b| (b.backend_id, b.name))
        .collect();

    let mut aliases = HashMap::new();
    for row in db
        .list_model_aliases()
        .map_err(|e| LlmError::InvalidRequest(format!("reading model_aliases: {e}")))?
    {
        // The FK guarantees the backend row exists; if the join fails the DB
        // is corrupt, and an alias pointing nowhere would resolve to the
        // default backend with someone else's model id — a silent wrong answer.
        let Some(backend) = backend_names.get(&row.backend_id) else {
            return Err(LlmError::InvalidRequest(format!(
                "model alias '{}' references a backend_id with no row — DB is corrupt",
                row.alias
            )));
        };
        aliases.insert(
            row.alias,
            ModelAlias {
                backend: backend.clone(),
                model: row.model,
            },
        );
    }
    registry.set_model_aliases(aliases);

    Ok(registry)
}

/// Read the singleton embedding config, or `None` when unset/disabled.
///
/// `enabled = 0` resolves to `None` — the same shape the kernel already
/// handles as "no semantic index configured", so the disable knob needs no
/// second code path. An enabled row whose backend is missing or cannot embed
/// is an error: the operator asked for an index and would otherwise get
/// silence.
pub fn load_embedding_config(db: &KernelDb) -> LlmResult<Option<EmbeddingModelConfig>> {
    let Some(row) = db
        .get_embedding_config()
        .map_err(|e| LlmError::InvalidRequest(format!("reading embedding_config: {e}")))?
    else {
        return Ok(None);
    };
    if !row.enabled {
        return Ok(None);
    }
    let backend = load_backends(db)?
        .into_iter()
        .find(|backend| backend.name.eq_ignore_ascii_case(&row.backend))
        .ok_or_else(|| LlmError::InvalidRequest(format!(
            "embedding_config names backend `{}`, which is not configured; add it with `kj backend set`",
            row.backend
        )))?;
    if !matches!(backend.kind, BackendKind::OpenAi | BackendKind::Mk) {
        return Err(LlmError::InvalidRequest(format!(
            "embedding_config names backend `{}` of kind `{}`; embeddings need an `openai` kind backend \
             whose base_url has an /embeddings route, or an `mk` kind backend serving /council/v2/embeddings",
            backend.name, backend.kind.as_str()
        )));
    }
    let profile: EmbeddingProfile = row.profile.parse().map_err(LlmError::InvalidRequest)?;
    if backend.kind == BackendKind::Mk && profile != EmbeddingProfile::Plain {
        return Err(LlmError::InvalidRequest(format!(
            "embedding_config names backend `{}` of kind `mk` with profile `{}`; a council server renders \
             its own task prompts, so use profile `plain`",
            backend.name, profile.as_str()
        )));
    }
    Ok(Some(EmbeddingModelConfig {
        enabled: true, backend, model: row.model,
        dimensions: usize::try_from(row.dimensions)
            .map_err(|_| LlmError::InvalidRequest("embedding_config dimensions is not positive".into()))?,
        profile,
        timeout_ms: row.timeout_ms as u64, max_in_flight: row.max_in_flight as usize,
        max_context_bytes: row.max_context_bytes as usize,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel_db::{BackendModelRow, EmbeddingConfigRow, ModelAliasRow};
    use kaijutsu_types::{BackendId, PrincipalId};

    fn seeded_db() -> KernelDb {
        let db = KernelDb::temporary().unwrap();
        crate::test_backends::example_backends(&db);
        db
    }

    /// Add an alias by backend NAME; every alias test writes its own.
    fn alias(db: &KernelDb, alias: &str, backend: &str, model: &str) {
        let b = db.get_backend_by_name(backend).unwrap().unwrap();
        db.set_model_alias(&ModelAliasRow {
            alias: alias.into(),
            backend_id: b.backend_id,
            model: model.into(),
        })
        .unwrap();
    }

    /// A backend that fails to build sets no limit on its endpoint, so it
    /// cannot cap the backends beside it that did build.
    ///
    /// Falsified by a limit taken from every backend row.
    #[test]
    fn a_backend_that_does_not_build_sets_no_limit() {
        let db = seeded_db();
        for (name, kind, key_env, max) in [("mk-zorak", "mk", None, None), ("broken", "anthropic", Some("KJ_TEST_NO_SUCH_KEY"), Some(1))] {
            db.upsert_backend(&crate::kernel_db::BackendRow {
                backend_id: BackendId::new(),
                name: name.into(),
                kind: kind.into(),
                base_url: Some("http://zorak:8090".into()),
                api_key_env: key_env.map(str::to_string),
                api_key_file: None,
                key_optional: false,
                request_timeout_secs: None,
                idle_timeout_secs: None,
                max_concurrent: max,
                created_at: 0,
                created_by: PrincipalId::system(),
            })
            .unwrap();
        }
        let endpoints = Endpoints::default();
        let registry = build_llm_registry(&db, &endpoints).unwrap();
        assert!(registry.get("broken").is_none(), "the anthropic row has no key");
        let status = endpoints.get("http://zorak:8090").status();
        assert_eq!(status.limit, None);
        assert_eq!(status.backends, vec![("mk-zorak".to_string(), None)]);
    }

    /// A kernel ships no backends: a new DB has no backend, model, alias or
    /// character slot, and its registry has no provider.
    #[test]
    fn a_new_kernel_db_has_no_llm_configuration() {
        let db = KernelDb::temporary().unwrap();
        assert!(db.list_backends().unwrap().is_empty());
        assert!(db.list_model_aliases().unwrap().is_empty());
        let registry = build_llm_registry(&db, &Endpoints::default()).unwrap();
        assert!(registry.list().is_empty());
    }

    #[test]
    fn registry_registers_every_backend_by_name() {
        // Backends appear under their own NAMES, including `gpt` and
        // `ollama`, which the old TOML could only express by making the
        // provider-type string be the name.
        let db = seeded_db();
        let registry = build_llm_registry(&db, &Endpoints::default()).expect("registry builds");
        let mut names = registry.list();
        names.sort();
        assert!(names.contains(&"ollama"), "names: {names:?}");
        assert!(names.contains(&"gpt"), "names: {names:?}");
    }

    #[test]
    fn two_backends_may_share_one_kind() {
        // The whole point of the name/kind split: `gpt` and `ollama` are both
        // kind=openai. Under models.toml this was impossible — the table name
        // WAS the provider type.
        let db = seeded_db();
        let backends = load_backends(&db).unwrap();
        let openai_named: Vec<&str> = backends
            .iter()
            .filter(|b| b.kind == BackendKind::OpenAi)
            .map(|b| b.name.as_str())
            .collect();
        assert!(openai_named.len() >= 2, "openai-kind backends: {openai_named:?}");
    }

    #[test]
    fn context_windows_come_from_the_db() {
        let db = seeded_db();
        let registry = build_llm_registry(&db, &Endpoints::default()).unwrap();
        assert_eq!(
            registry.context_window_for("anthropic", "claude-opus-4-8"),
            Some(1_000_000)
        );
        assert_eq!(
            registry.context_window_for("anthropic", "claude-haiku-4-5"),
            Some(200_000)
        );
        // Unpinned models stay honestly unknown — never a guessed denominator.
        assert_eq!(registry.context_window_for("gpt", "gpt-5.6-terra"), None);
        assert_eq!(registry.context_window_for("ollama", "gemma4:31b"), None);
    }

    #[test]
    fn aliases_resolve_to_backend_and_model() {
        let db = seeded_db();
        alias(&db, "opus", "anthropic", "claude-opus-5");
        alias(&db, "local", "ollama", "gemma4:31b");
        let registry = build_llm_registry(&db, &Endpoints::default()).unwrap();
        assert_eq!(registry.resolve_alias("opus"), Some(("anthropic", "claude-opus-5")));
        assert_eq!(registry.resolve_alias("local"), Some(("ollama", "gemma4:31b")));
    }

    #[test]
    fn alias_lookup_is_case_insensitive() {
        // The alias PK is COLLATE NOCASE, so a case-sensitive resolver could
        // only ever fail to find a row that provably exists.
        let db = seeded_db();
        alias(&db, "opus", "anthropic", "claude-opus-5");
        let registry = build_llm_registry(&db, &Endpoints::default()).unwrap();
        assert!(registry.resolve_alias("OPUS").is_some());
        assert!(registry.resolve_alias("Opus").is_some());
    }

    #[test]
    fn unknown_kind_in_the_table_is_fatal() {
        // The write path refuses unknown kinds, so one in the table means the
        // DB was edited behind our back. Skipping it would hang a later turn
        // on a backend that "was configured".
        let db = KernelDb::temporary().unwrap();
        db.upsert_backend(&crate::kernel_db::BackendRow {
            backend_id: BackendId::new(),
            name: "gemini".into(),
            // `mock` parses only with the test-mock feature, which cfg(test)
            // turns on — so this row is accepted here and would not be in a
            // production build.
            kind: "mock".into(),
            base_url: None,
            api_key_env: None,
            api_key_file: None,
            key_optional: true,
            request_timeout_secs: None,
            idle_timeout_secs: None,
            max_concurrent: None,
            created_at: 0,
            created_by: PrincipalId::system(),
        })
        .unwrap();
        // Under cfg(test) 'mock' parses, so the registry builds. What we pin
        // here is that an unparseable kind never reaches the table at all.
        assert!(build_llm_registry(&db, &Endpoints::default()).is_ok());
        let err = db
            .upsert_backend(&crate::kernel_db::BackendRow {
                backend_id: BackendId::new(),
                name: "gemini2".into(),
                kind: "gemini".into(),
                base_url: None,
                api_key_env: None,
                api_key_file: None,
                key_optional: true,
                request_timeout_secs: None,
                idle_timeout_secs: None,
                max_concurrent: None,
                created_at: 0,
                created_by: PrincipalId::system(),
            })
            .unwrap_err();
        assert!(
            err.to_string().contains("gemini"),
            "the write path must reject an unknown kind, naming it: {err}"
        );
    }

    #[test]
    fn a_new_kernel_db_configures_no_embedding_service() {
        let db = KernelDb::temporary().unwrap();
        assert_eq!(db.get_embedding_config().unwrap(), None);
        assert!(load_embedding_config(&db).unwrap().is_none());
    }

    fn embedding_row(backend: &str) -> EmbeddingConfigRow {
        EmbeddingConfigRow {
            enabled: true, backend: backend.into(), model: "qwen/qwen3-embedding-8b".into(),
            dimensions: 1024, profile: "qwen3".into(),
            timeout_ms: 30_000, max_in_flight: 2, max_context_bytes: 2048,
        }
    }

    #[test]
    fn a_configured_embedding_service_reads_back_with_its_backend() {
        let db = seeded_db();
        crate::test_backends::backend(
            &db, "openrouter", "openai", Some("https://openrouter.ai/api/v1"),
            Some("OPENROUTER_API_KEY"), None, false,
        );
        db.set_embedding_config(&embedding_row("openrouter")).unwrap();
        let emb = load_embedding_config(&db).unwrap().expect("configured and enabled");
        assert_eq!(emb.backend.name, "openrouter");
        assert_eq!(emb.backend.base_url.as_deref(), Some("https://openrouter.ai/api/v1"));
        assert_eq!(emb.backend.api_key_env.as_deref(), Some("OPENROUTER_API_KEY"));
        assert_eq!(emb.model, "qwen/qwen3-embedding-8b");
        assert_eq!(emb.dimensions, 1024);
        assert_eq!(emb.profile, kaijutsu_index::EmbeddingProfile::Qwen3);
        assert_eq!(emb.timeout_ms, 30_000);
        assert_eq!(emb.max_in_flight, 2);
        assert_eq!(emb.max_context_bytes, 2048);
    }

    #[test]
    fn disabled_embedding_reads_as_none() {
        let db = seeded_db();
        db.set_embedding_config(&EmbeddingConfigRow { enabled: false, ..embedding_row("openrouter") }).unwrap();
        assert!(load_embedding_config(&db).unwrap().is_none(), "a disabled row needs no backend");
    }

    #[test]
    fn an_embedding_row_naming_no_backend_is_an_error_not_silence() {
        let db = seeded_db();
        db.set_embedding_config(&embedding_row("openrouter")).unwrap();
        let error = load_embedding_config(&db).unwrap_err().to_string();
        assert!(error.contains("openrouter") && error.contains("kj backend"), "{error}");
    }

    #[test]
    fn only_an_openai_or_mk_backend_can_embed() {
        let db = seeded_db();
        for backend in ["anthropic", "deepseek"] {
            db.set_embedding_config(&embedding_row(backend)).unwrap();
            let error = load_embedding_config(&db).unwrap_err().to_string();
            assert!(error.contains(backend) && error.contains("openai") && error.contains("mk"), "{backend}: {error}");
        }
    }

    /// A council server renders each task's prompt itself, so an `mk`
    /// backend takes the `plain` profile; any other would prefix twice.
    #[test]
    fn an_mk_backend_embeds_with_the_plain_profile_only() {
        let db = seeded_db();
        crate::test_backends::backend(&db, "spaceghost", "mk", Some("http://100.65.47.5:8091"), None, None, true);
        db.set_embedding_config(&EmbeddingConfigRow { profile: "plain".into(), ..embedding_row("spaceghost") }).unwrap();
        let emb = load_embedding_config(&db).unwrap().expect("configured and enabled");
        assert_eq!(emb.backend.kind, BackendKind::Mk);
        assert_eq!(emb.profile, kaijutsu_index::EmbeddingProfile::Plain);

        db.set_embedding_config(&embedding_row("spaceghost")).unwrap();
        let error = load_embedding_config(&db).unwrap_err().to_string();
        assert!(error.contains("spaceghost") && error.contains("plain"), "{error}");
    }

    #[test]
    fn a_live_edit_shows_up_in_the_next_build() {
        // The live-reload contract: a write followed by a rebuild reflects the
        // change without any process restart.
        let db = seeded_db();
        alias(&db, "fast", "anthropic", "claude-haiku-4-5");
        let before = build_llm_registry(&db, &Endpoints::default()).unwrap();
        assert_eq!(before.resolve_alias("fast").map(|(b, _)| b), Some("anthropic"));

        alias(&db, "fast", "ollama", "qwen3.5:9b-bf16");

        let after = build_llm_registry(&db, &Endpoints::default()).unwrap();
        assert_eq!(
            after.resolve_alias("fast"),
            Some(("ollama", "qwen3.5:9b-bf16"))
        );
    }

    #[test]
    fn context_window_edit_takes_effect_on_rebuild() {
        let db = seeded_db();
        let gpt = db.get_backend_by_name("gpt").unwrap().unwrap();
        assert_eq!(
            build_llm_registry(&db, &Endpoints::default()).unwrap().context_window_for("gpt", "gpt-5.6-terra"),
            None
        );
        db.set_backend_model(&BackendModelRow {
            backend_id: gpt.backend_id,
            model_id: "gpt-5.6-terra".into(),
            context_window: Some(400_000),
            extra: None,
            tunables: Default::default(),
        })
        .unwrap();
        assert_eq!(
            build_llm_registry(&db, &Endpoints::default()).unwrap().context_window_for("gpt", "gpt-5.6-terra"),
            Some(400_000)
        );
    }
}
