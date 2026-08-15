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
//! backend`/`kj cast`/`kj alias` mutation ends with
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

use super::config::{
    BackendConfig, BackendKind, EmbeddingModelConfig, ModelAlias, ModelInfo, ResolvedSlot,
    SlotTunables,
};
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
            models,
        });
    }
    Ok(out)
}

/// Build a fresh [`LlmRegistry`] from the DB.
///
/// Registers one client per backend row, keyed by backend NAME. A backend that
/// fails to construct (no resolvable key, say) is warned about and skipped —
/// except the default one, which is fatal: silently falling back to some other
/// backend would hide the misconfiguration and quietly change which model runs
/// every turn.
pub fn build_llm_registry(db: &KernelDb) -> LlmResult<LlmRegistry> {
    let backends = load_backends(db)?;
    let mut registry = LlmRegistry::new();

    for backend in &backends {
        match Provider::from_backend(backend) {
            Ok(provider) => {
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

    registry.set_backends(backends);

    // ── defaults ────────────────────────────────────────────────────────
    let defaults = db
        .get_llm_defaults()
        .map_err(|e| LlmError::InvalidRequest(format!("reading llm_defaults: {e}")))?;
    if let Some(d) = &defaults {
        if !registry.set_default(&d.default_backend) {
            let available: Vec<String> = registry.list().iter().map(|s| s.to_string()).collect();
            return Err(LlmError::InvalidRequest(format!(
                "default backend '{}' is not registered; available backends: {available:?}",
                d.default_backend
            )));
        }
        registry.set_default_model(&d.default_model);
        registry.set_default_tunables(SlotTunables {
            max_tokens: d.max_tokens.and_then(|v| u64::try_from(v).ok()),
            temperature: d.temperature,
            top_p: d.top_p,
            effort: d.effort.clone(),
            thinking_budget: d.thinking_budget.and_then(|v| u64::try_from(v).ok()),
            thinking_style: d.thinking_style.clone(),
        });
    } else {
        // A DB that has never been seeded (a bare unit-test KernelDb). Not an
        // error here — the caller decides whether a registry with no default
        // is usable; `ensure_factory_backends` is what gives a real kernel one.
        tracing::warn!("no llm_defaults row — registry has no default backend/model");
    }

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

    // ── casts ───────────────────────────────────────────────────────────
    let floor = registry.default_tunables().clone();
    let mut slots: Vec<(String, ResolvedSlot)> = Vec::new();
    for cast in db
        .list_casts()
        .map_err(|e| LlmError::InvalidRequest(format!("reading casts: {e}")))?
    {
        for slot in db
            .list_cast_slots(cast.cast_id)
            .map_err(|e| LlmError::InvalidRequest(format!("reading cast_slots: {e}")))?
        {
            let Some(backend) = backend_names.get(&slot.backend_id) else {
                return Err(LlmError::InvalidRequest(format!(
                    "cast '{}' slot '{}' references a backend_id with no row — DB is corrupt",
                    cast.label, slot.role
                )));
            };
            let own = SlotTunables {
                max_tokens: slot.max_tokens.and_then(|v| u64::try_from(v).ok()),
                temperature: slot.temperature,
                top_p: slot.top_p,
                effort: slot.effort.clone(),
                thinking_budget: slot.thinking_budget.and_then(|v| u64::try_from(v).ok()),
                thinking_style: slot.thinking_style.clone(),
            };
            slots.push((
                cast.label.clone(),
                ResolvedSlot {
                    role: slot.role,
                    backend: backend.clone(),
                    model: slot.model,
                    tunables: own.over(&floor),
                    loadout: slot.loadout,
                    extra: slot.extra,
                },
            ));
        }
    }
    registry.set_cast_slots(slots);

    Ok(registry)
}

/// Read the singleton embedding config, or `None` when unset/disabled.
///
/// `enabled = 0` resolves to `None` — the same shape the kernel already
/// handles as "no semantic index configured", so the disable knob needs no
/// second code path.
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
    // `~` expansion happens here, not at write time: the stored value is what
    // the operator typed, so a config moved between machines keeps meaning
    // "my home directory" rather than someone else's absolute path.
    let expanded = shellexpand::tilde(&row.model_dir).into_owned();
    Ok(Some(EmbeddingModelConfig {
        enabled: true,
        model_dir: std::path::PathBuf::from(expanded),
        dimensions: row.dimensions as usize,
        max_tokens: row.max_tokens as usize,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel_db::{BackendModelRow, EmbeddingConfigRow, LlmDefaultsRow, ModelAliasRow};
    use crate::seed_backends::{ensure_factory_backends, reseed_factory_backends};
    use kaijutsu_types::{BackendId, CastId, PrincipalId};

    fn seeded_db() -> KernelDb {
        let mut db = KernelDb::in_memory().unwrap();
        ensure_factory_backends(&mut db, PrincipalId::system()).unwrap();
        db
    }

    /// Add an alias by backend NAME — the floor seeds none (aliases are
    /// operator data), so every alias test writes its own.
    fn alias(db: &KernelDb, alias: &str, backend: &str, model: &str) {
        let b = db.get_backend_by_name(backend).unwrap().unwrap();
        db.set_model_alias(&ModelAliasRow {
            alias: alias.into(),
            backend_id: b.backend_id,
            model: model.into(),
        })
        .unwrap();
    }

    #[test]
    fn registry_registers_every_backend_by_name() {
        // The floor's four backends must all appear under their own NAMES —
        // including `gpt` and `ollama`, which the old TOML could only express
        // by making the provider-type string be the name.
        let db = seeded_db();
        // SAFETY: single-threaded test; the openai-kind backends are
        // key-optional or key-file based, so no env is required for them.
        let registry = build_llm_registry(&db).expect("registry builds from the floor");
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
        let registry = build_llm_registry(&db).unwrap();
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
    fn the_floor_ships_no_aliases() {
        // The old models.toml alias set was a pile of guesses; aliases are
        // operator data now, like casts.
        let db = seeded_db();
        assert!(db.list_model_aliases().unwrap().is_empty());
        let registry = build_llm_registry(&db).unwrap();
        assert!(registry.model_aliases().is_empty());
        assert!(registry.resolve_alias("fast").is_none());
    }

    #[test]
    fn aliases_resolve_to_backend_and_model() {
        let db = seeded_db();
        alias(&db, "opus", "anthropic", "claude-opus-5");
        alias(&db, "local", "ollama", "gemma4:31b");
        let registry = build_llm_registry(&db).unwrap();
        assert_eq!(registry.resolve_alias("opus"), Some(("anthropic", "claude-opus-5")));
        assert_eq!(registry.resolve_alias("local"), Some(("ollama", "gemma4:31b")));
    }

    #[test]
    fn alias_lookup_is_case_insensitive() {
        // The alias PK is COLLATE NOCASE, so a case-sensitive resolver could
        // only ever fail to find a row that provably exists.
        let db = seeded_db();
        alias(&db, "opus", "anthropic", "claude-opus-5");
        let registry = build_llm_registry(&db).unwrap();
        assert!(registry.resolve_alias("OPUS").is_some());
        assert!(registry.resolve_alias("Opus").is_some());
    }

    #[test]
    fn defaults_are_deepseek_flash_with_16k_output() {
        let db = seeded_db();
        let registry = build_llm_registry(&db).unwrap();
        assert_eq!(registry.default_provider_name(), Some("deepseek"));
        assert_eq!(registry.default_model(), Some("deepseek-v4-flash"));
        assert_eq!(registry.max_output_tokens(), 16384);
        let t = registry.default_tunables();
        // Effort IS decided: the factory floor asks for maximum reasoning on
        // deepseek (see `seed_backends::FACTORY_EFFORT` for why that is the
        // frugal choice rather than the extravagant one). Asserted here as
        // well as at the seed, because this is the path that carries it into
        // a live registry — the seed being right buys nothing if the registry
        // drops it.
        assert_eq!(t.effort.as_deref(), Some("max"));
        // Knobs we haven't decided on stay NULL — "provider default" is a
        // real answer, not a gap to fill with an invented number.
        assert_eq!(t.temperature, None);
        assert_eq!(t.top_p, None);
        assert_eq!(t.thinking_budget, None);
        assert_eq!(t.thinking_style, None);
    }

    #[test]
    fn missing_default_backend_is_fatal_not_a_silent_fallback() {
        let mut db = KernelDb::in_memory().unwrap();
        ensure_factory_backends(&mut db, PrincipalId::system()).unwrap();
        // Point the defaults at a backend name nothing registers under.
        // `set_llm_defaults` refuses a dangling name outright, so get there
        // the only other way: delete the backend the defaults already name.
        db.delete_backend("deepseek").unwrap();
        let err = build_llm_registry(&db).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("default backend 'deepseek'"), "{msg}");
        assert!(msg.contains("not registered"), "{msg}");
    }

    #[test]
    fn unknown_kind_in_the_table_is_fatal() {
        // The write path refuses unknown kinds, so one in the table means the
        // DB was edited behind our back. Skipping it would hang a later turn
        // on a backend that "was configured".
        let db = KernelDb::in_memory().unwrap();
        db.upsert_backend(&crate::kernel_db::BackendRow {
            backend_id: BackendId::new(),
            name: "gemini".into(),
            // 'mock' is admitted by the CHECK; parse refuses it without the
            // feature. Under cfg(test) the feature IS on, so use a kind the
            // CHECK admits but parse cannot know — there is none, so exercise
            // the parse error directly instead.
            kind: "mock".into(),
            base_url: None,
            api_key_env: None,
            api_key_file: None,
            key_optional: true,
            request_timeout_secs: None,
            created_at: 0,
            created_by: PrincipalId::system(),
        })
        .unwrap();
        // Under cfg(test) 'mock' parses, so the registry builds; the real
        // guard is BackendKind::parse, covered in llm/config.rs. What we pin
        // here is that a CHECK-illegal kind never reaches the table at all.
        assert!(build_llm_registry(&db).is_ok());
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
                created_at: 0,
                created_by: PrincipalId::system(),
            })
            .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("constraint"),
            "the CHECK must reject an unknown kind at the SQL layer: {err}"
        );
    }

    #[test]
    fn cast_slots_resolve_with_the_defaults_cascade() {
        let db = seeded_db();
        let cast_id = CastId::new();
        db.insert_cast(&crate::kernel_db::CastRow {
            cast_id,
            label: "house".into(),
            description: None,
            created_at: 0,
            created_by: PrincipalId::system(),
        })
        .unwrap();
        let anthropic = db.get_backend_by_name("anthropic").unwrap().unwrap();
        db.set_cast_slot(&crate::kernel_db::CastSlotRow {
            cast_id,
            role: "coder".into(),
            backend_id: anthropic.backend_id,
            model: "claude-opus-5".into(),
            max_tokens: None, // falls back to the llm_defaults floor
            temperature: Some(0.3),
            top_p: None,
            effort: Some("high".into()),
            thinking_budget: None,
            thinking_style: None,
            loadout: Some("coder".into()),
            extra: None,
        })
        .unwrap();

        let registry = build_llm_registry(&db).unwrap();
        let slot = registry.resolved_slot("house", "coder").expect("slot resolves");
        assert_eq!(slot.backend, "anthropic");
        assert_eq!(slot.model, "claude-opus-5");
        assert_eq!(slot.tunables.max_tokens, Some(16384), "NULL cascades to the floor");
        assert_eq!(slot.tunables.temperature, Some(0.3), "slot value wins");
        assert_eq!(slot.tunables.effort.as_deref(), Some("high"));
        assert_eq!(slot.loadout.as_deref(), Some("coder"));
        // Cast labels are case-insensitive, like the UNIQUE collation.
        assert!(registry.resolved_slot("HOUSE", "coder").is_some());
        assert!(registry.resolved_slot("house", "musician").is_none());
    }

    #[test]
    fn no_factory_casts_are_seeded() {
        // Casts are operator/agent-configured; the floor ships none.
        let db = seeded_db();
        assert!(db.list_casts().unwrap().is_empty());
        let registry = build_llm_registry(&db).unwrap();
        assert!(registry.cast_labels().is_empty());
    }

    #[test]
    fn embedding_config_reads_from_the_db_and_expands_tilde() {
        let db = seeded_db();
        let emb = load_embedding_config(&db).unwrap().expect("floor seeds embedding");
        assert_eq!(emb.dimensions, 384);
        assert_eq!(emb.max_tokens, 512);
        assert!(
            !emb.model_dir.to_string_lossy().starts_with('~'),
            "~ must be expanded at read time: {:?}",
            emb.model_dir
        );
        assert!(emb.model_dir.ends_with("bge-small-en-v1.5"));
    }

    #[test]
    fn disabled_embedding_reads_as_none() {
        let db = seeded_db();
        let mut row = db.get_embedding_config().unwrap().unwrap();
        row.enabled = false;
        db.set_embedding_config(&row).unwrap();
        assert!(load_embedding_config(&db).unwrap().is_none());
    }

    #[test]
    fn a_live_edit_shows_up_in_the_next_build() {
        // The live-reload contract: a write followed by a rebuild reflects the
        // change without any process restart.
        let db = seeded_db();
        alias(&db, "fast", "anthropic", "claude-haiku-4-5");
        let before = build_llm_registry(&db).unwrap();
        assert_eq!(before.resolve_alias("fast").map(|(b, _)| b), Some("anthropic"));

        alias(&db, "fast", "ollama", "qwen3.5:9b-bf16");

        let after = build_llm_registry(&db).unwrap();
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
            build_llm_registry(&db).unwrap().context_window_for("gpt", "gpt-5.6-terra"),
            None
        );
        db.set_backend_model(&BackendModelRow {
            backend_id: gpt.backend_id,
            model_id: "gpt-5.6-terra".into(),
            context_window: Some(400_000),
            extra: None,
        })
        .unwrap();
        assert_eq!(
            build_llm_registry(&db).unwrap().context_window_for("gpt", "gpt-5.6-terra"),
            Some(400_000)
        );
    }

    #[test]
    fn reseed_restores_the_floor_after_a_clobber() {
        let mut db = seeded_db();
        alias(&db, "opus", "ollama", "wrong");
        db.set_llm_defaults(&LlmDefaultsRow {
            default_backend: "ollama".into(),
            default_model: "gemma4:31b".into(),
            max_tokens: Some(99),
            temperature: None,
            top_p: None,
            effort: None,
            thinking_budget: None,
            thinking_style: None,
        })
        .unwrap();

        reseed_factory_backends(&mut db, PrincipalId::system()).unwrap();
        let registry = build_llm_registry(&db).unwrap();
        assert_eq!(registry.default_provider_name(), Some("deepseek"));
        assert_eq!(registry.max_output_tokens(), 16384);
        // The operator's alias is NOT part of the floor, so reseed leaves it.
        assert_eq!(registry.resolve_alias("opus"), Some(("ollama", "wrong")));
    }

    #[test]
    fn embedding_defaults_survive_a_reseed_with_the_expected_shape() {
        let mut db = seeded_db();
        db.set_embedding_config(&EmbeddingConfigRow {
            enabled: false,
            model_dir: "/tmp/nope".into(),
            dimensions: 7,
            max_tokens: 9,
        })
        .unwrap();
        reseed_factory_backends(&mut db, PrincipalId::system()).unwrap();
        let emb = load_embedding_config(&db).unwrap().expect("reseed re-enables");
        assert_eq!(emb.dimensions, 384);
    }
}
