//! Factory LLM configuration — the backends, their known context windows, and
//! the defaults a fresh kernel is born with.
//!
//! The backend analogue of [`crate::seed_presets`]: an idempotent floor
//! ([`ensure_factory_backends`], insert-only-if-absent, so an operator's edits
//! survive) plus a force-restore ([`reseed_factory_backends`], the `kj backend
//! reseed` path). The definitions are **Rust-embedded**, not TOML — SQL is the
//! source of truth for model config and `assets/defaults/models.toml` was
//! demolished along with the loader that read it.
//!
//! What the floor deliberately does NOT ship:
//!
//! - **No casts.** A cast is an operator/agent decision about who plays which
//!   seat; shipping one would be the kernel guessing at an ensemble.
//! - **No model aliases.** The old `models.toml` `[model_aliases]` set
//!   (`fast`/`smart`/`best`/`ds-*`/`local`/…) was a pile of guesses about which
//!   model deserves which adjective, and they were wrong often enough to be
//!   worse than absent — `default` pointed at a retired id for a while. Aliases
//!   are now purely operator/agent-configured, exactly like casts: the table
//!   and `kj alias` exist, the floor writes zero rows.
//! - **No context windows for `gpt`/`ollama`.** We have no verified number for
//!   the gpt-5.6 family, and a local server's effective window is a runtime
//!   setting (Ollama's `num_ctx`, a Modelfile PARAMETER, a llama.cpp launch
//!   flag) rather than a property of the model id. Unknown ≠ guessed: a
//!   fabricated denominator makes a "% of context used" gauge confidently
//!   wrong, which is worse than the gauge saying "unknown".
//! - **No tunables we haven't decided on.** `max_tokens` is real (16384, sized
//!   for V4 reasoning tokens counting against the output budget); temperature
//!   / top_p / effort / thinking_* stay NULL, meaning "provider default".
//!
//! Model ids are **undated on purpose**. Dated ids get retired out from under
//! us — `claude-sonnet-4-20250514` and `claude-opus-4-20250514` both 404 today,
//! and the first of those used to back the old `default` alias, so a fresh
//! kernel shipped pointing at a model Anthropic no longer serves. Undated ids
//! roll forward instead.

use kaijutsu_types::{BackendId, PrincipalId};

use crate::kernel_db::{
    BackendModelRow, BackendRow, EmbeddingConfigRow, KernelDb, KernelDbResult, LlmDefaultsRow,
};

/// A factory backend definition.
struct FactoryBackend {
    name: &'static str,
    kind: &'static str,
    base_url: Option<&'static str>,
    api_key_env: Option<&'static str>,
    api_key_file: Option<&'static str>,
    key_optional: bool,
    /// `(model_id, context_window)` — `None` means "we do not know", and that
    /// is the honest answer we ship rather than a number we made up.
    models: &'static [(&'static str, Option<u64>)],
}

/// Context windows read live from Anthropic's `GET /v1/models/{id}`
/// (`max_input_tokens`) on 2026-08-02 and from api-docs.deepseek.com on
/// 2026-07-29 — measured, not extrapolated.
const FACTORY_BACKENDS: &[FactoryBackend] = &[
    FactoryBackend {
        name: "anthropic",
        kind: "anthropic",
        base_url: None,
        api_key_env: Some("ANTHROPIC_API_KEY"),
        api_key_file: Some("~/.anthropic-key.txt"),
        key_optional: false,
        models: &[
            // Haiku 4.5 is still the latest Haiku (there is no Haiku 5) and is
            // the one model here supporting NO effort levels, while opus-5 /
            // sonnet-5 / fable-5 support low→max.
            ("claude-haiku-4-5", Some(200_000)),
            ("claude-opus-4-5", Some(200_000)),
            ("claude-opus-4-7", Some(1_000_000)),
            ("claude-opus-4-8", Some(1_000_000)),
            ("claude-opus-5", Some(1_000_000)),
            ("claude-sonnet-4-6", Some(1_000_000)),
            ("claude-sonnet-5", Some(1_000_000)),
            ("claude-fable-5", Some(1_000_000)),
        ],
    },
    FactoryBackend {
        name: "deepseek",
        kind: "deepseek",
        base_url: None,
        api_key_env: Some("DEEPSEEK_API_KEY"),
        api_key_file: Some("~/.deepseek-key"),
        key_optional: false,
        models: &[
            ("deepseek-v4-flash", Some(1_000_000)),
            ("deepseek-v4-pro", Some(1_000_000)),
        ],
    },
    FactoryBackend {
        name: "gpt",
        kind: "openai",
        base_url: Some("https://api.openai.com/v1"),
        api_key_env: Some("OPENAI_API_KEY"),
        api_key_file: Some("~/.openai-key.txt"),
        key_optional: false,
        // No windows: we have no verified number for the gpt-5.6 family.
        models: &[],
    },
    FactoryBackend {
        name: "ollama",
        kind: "openai",
        base_url: Some("http://localhost:11434/v1"),
        api_key_env: None,
        api_key_file: None,
        // A local server needs no bearer token.
        key_optional: true,
        // No windows: `num_ctx` is a runtime setting, not a model property.
        models: &[],
    },
];

/// The kernel-wide default: the affordable lane.
const FACTORY_DEFAULT_BACKEND: &str = "deepseek";
const FACTORY_DEFAULT_MODEL: &str = "deepseek-v4-flash";
/// V4 models think by default and reasoning tokens count toward the output
/// budget (max 64K), so leave headroom above a plain-answer ceiling.
const FACTORY_MAX_TOKENS: i64 = 16384;

/// The local ONNX embedding model for semantic indexing. Directory should hold
/// `model.onnx` + `tokenizer.json`; the model's identity is its directory
/// basename, so renaming the dir invalidates the on-disk index.
const FACTORY_EMBEDDING_DIR: &str = "~/.local/share/kaijutsu/models/bge-small-en-v1.5";
const FACTORY_EMBEDDING_DIMS: i64 = 384;
const FACTORY_EMBEDDING_MAX_TOKENS: i64 = 512;

/// True when `name` is a factory backend name. Not a hard reservation — an
/// operator may absolutely re-point `anthropic` at a gateway — but
/// `reseed_factory_backends` will restore these to the embedded definitions.
pub fn is_factory_backend_name(name: &str) -> bool {
    FACTORY_BACKENDS.iter().any(|b| b.name == name)
}

/// Idempotently seed the factory floor: a backend, model row, or singleton is
/// created only when absent, so operator edits survive. Returns how many
/// backends were newly created.
///
/// Every failure bubbles. A kernel that silently came up with no backends
/// would hang the first turn on "no provider configured", which is a much
/// worse diagnostic than the insert error that caused it.
pub fn ensure_factory_backends(db: &mut KernelDb, created_by: PrincipalId) -> KernelDbResult<usize> {
    let mut created = 0;
    for fb in FACTORY_BACKENDS {
        let existing = db.get_backend_by_name(fb.name)?;
        let backend_id = match existing {
            Some(row) => row.backend_id,
            None => {
                let row = insert_factory_backend(db, fb, created_by)?;
                created += 1;
                row.backend_id
            }
        };
        // Model rows are part of the floor too: absent-only, so a hand-tuned
        // context_window is never clobbered by a restart.
        let have: std::collections::HashSet<String> = db
            .list_backend_models(backend_id)?
            .into_iter()
            .map(|m| m.model_id)
            .collect();
        for (model_id, window) in fb.models {
            if have.contains(*model_id) {
                continue;
            }
            db.set_backend_model(&BackendModelRow {
                backend_id,
                model_id: (*model_id).to_string(),
                context_window: window.map(|w| w as i64),
                extra: None,
            })?;
        }
    }

    if db.get_llm_defaults()?.is_none() {
        db.set_llm_defaults(&factory_defaults())?;
    }
    if db.get_embedding_config()?.is_none() {
        db.set_embedding_config(&factory_embedding())?;
    }

    Ok(created)
}

/// Force-restore the factory floor from the embedded definitions: every
/// factory backend, its model rows, the defaults row, and the embedding row
/// are overwritten. Backends the operator added are left alone — this restores
/// the floor, it does not wipe the room. Aliases and casts are untouched
/// because the floor never had any: they are operator data end to end.
///
/// Returns how many factory backends were restored.
pub fn reseed_factory_backends(
    db: &mut KernelDb,
    created_by: PrincipalId,
) -> KernelDbResult<usize> {
    for fb in FACTORY_BACKENDS {
        // Upsert rather than delete-and-reinsert: deleting would cascade the
        // backend's model rows away and be refused outright while any cast
        // slot or alias references it (the FK guard in `delete_backend`).
        // Keeping the id means every referent survives the restore.
        let row = insert_factory_backend(db, fb, created_by)?;
        for (model_id, window) in fb.models {
            db.set_backend_model(&BackendModelRow {
                backend_id: row.backend_id,
                model_id: (*model_id).to_string(),
                context_window: window.map(|w| w as i64),
                extra: None,
            })?;
        }
    }
    db.set_llm_defaults(&factory_defaults())?;
    db.set_embedding_config(&factory_embedding())?;
    Ok(FACTORY_BACKENDS.len())
}

fn factory_defaults() -> LlmDefaultsRow {
    LlmDefaultsRow {
        default_backend: FACTORY_DEFAULT_BACKEND.to_string(),
        default_model: FACTORY_DEFAULT_MODEL.to_string(),
        max_tokens: Some(FACTORY_MAX_TOKENS),
        // Undecided knobs stay NULL — "provider default" is a real answer.
        temperature: None,
        top_p: None,
        effort: None,
        thinking_budget: None,
        thinking_style: None,
    }
}

fn factory_embedding() -> EmbeddingConfigRow {
    EmbeddingConfigRow {
        enabled: true,
        model_dir: FACTORY_EMBEDDING_DIR.to_string(),
        dimensions: FACTORY_EMBEDDING_DIMS,
        max_tokens: FACTORY_EMBEDDING_MAX_TOKENS,
    }
}

fn insert_factory_backend(
    db: &KernelDb,
    fb: &FactoryBackend,
    created_by: PrincipalId,
) -> KernelDbResult<BackendRow> {
    db.upsert_backend(&BackendRow {
        backend_id: BackendId::new(),
        name: fb.name.to_string(),
        kind: fb.kind.to_string(),
        base_url: fb.base_url.map(str::to_string),
        api_key_env: fb.api_key_env.map(str::to_string),
        api_key_file: fb.api_key_file.map(str::to_string),
        key_optional: fb.key_optional,
        request_timeout_secs: None,
        created_at: kaijutsu_types::now_millis() as i64,
        created_by,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_is_idempotent_and_seeds_four_backends() {
        let mut db = KernelDb::in_memory().unwrap();
        let who = PrincipalId::system();
        assert_eq!(ensure_factory_backends(&mut db, who).unwrap(), 4, "first run seeds all four");
        assert_eq!(ensure_factory_backends(&mut db, who).unwrap(), 0, "second run is a no-op");

        let names: Vec<String> = db.list_backends().unwrap().into_iter().map(|b| b.name).collect();
        assert_eq!(names, vec!["anthropic", "deepseek", "gpt", "ollama"]);
    }

    #[test]
    fn the_floor_seeds_no_aliases_and_no_casts() {
        // Both are operator/agent data. The old alias set was a pile of
        // guesses about which model deserves which adjective; shipping none
        // is the honest floor.
        let mut db = KernelDb::in_memory().unwrap();
        let who = PrincipalId::system();
        ensure_factory_backends(&mut db, who).unwrap();
        assert!(db.list_model_aliases().unwrap().is_empty());
        assert!(db.list_casts().unwrap().is_empty());
        reseed_factory_backends(&mut db, who).unwrap();
        assert!(db.list_model_aliases().unwrap().is_empty());
        assert!(db.list_casts().unwrap().is_empty());
    }

    #[test]
    fn reseed_leaves_operator_aliases_alone() {
        // The floor owns no aliases, so a reseed must not touch one.
        let mut db = KernelDb::in_memory().unwrap();
        let who = PrincipalId::system();
        ensure_factory_backends(&mut db, who).unwrap();
        let anthropic = db.get_backend_by_name("anthropic").unwrap().unwrap();
        db.set_model_alias(&crate::kernel_db::ModelAliasRow {
            alias: "opus".into(),
            backend_id: anthropic.backend_id,
            model: "claude-opus-5".into(),
        })
        .unwrap();
        reseed_factory_backends(&mut db, who).unwrap();
        let aliases = db.list_model_aliases().unwrap();
        assert_eq!(aliases.len(), 1);
        assert_eq!(aliases[0].model, "claude-opus-5");
    }

    #[test]
    fn no_backend_row_carries_key_material() {
        // The whole point of dropping the inline `api_key` field: config
        // stores WHERE a key lives, never the key.
        let mut db = KernelDb::in_memory().unwrap();
        ensure_factory_backends(&mut db, PrincipalId::system()).unwrap();
        for b in db.list_backends().unwrap() {
            if let Some(f) = &b.api_key_file {
                assert!(f.starts_with('~') || f.starts_with('/'), "{f} is a path");
            }
            if let Some(e) = &b.api_key_env {
                assert!(e.ends_with("_API_KEY"), "{e} is an env var NAME");
            }
        }
    }

    #[test]
    fn ollama_is_key_optional_and_gpt_is_not() {
        let mut db = KernelDb::in_memory().unwrap();
        ensure_factory_backends(&mut db, PrincipalId::system()).unwrap();
        assert!(db.get_backend_by_name("ollama").unwrap().unwrap().key_optional);
        assert!(!db.get_backend_by_name("gpt").unwrap().unwrap().key_optional);
    }

    #[test]
    fn openai_kind_backends_all_carry_a_base_url() {
        // kind=openai means "some OpenAI-compatible server"; without the URL
        // the row does not say which one.
        let mut db = KernelDb::in_memory().unwrap();
        ensure_factory_backends(&mut db, PrincipalId::system()).unwrap();
        for b in db.list_backends().unwrap().into_iter().filter(|b| b.kind == "openai") {
            assert!(b.base_url.is_some(), "{} has kind=openai but no base_url", b.name);
        }
    }

    #[test]
    fn unknown_windows_stay_unset_not_guessed() {
        let mut db = KernelDb::in_memory().unwrap();
        ensure_factory_backends(&mut db, PrincipalId::system()).unwrap();
        for name in ["gpt", "ollama"] {
            let b = db.get_backend_by_name(name).unwrap().unwrap();
            assert!(
                db.list_backend_models(b.backend_id).unwrap().is_empty(),
                "{name} must ship no invented context windows"
            );
        }
    }

    #[test]
    fn ensure_preserves_an_operator_edit_but_reseed_restores_it() {
        let mut db = KernelDb::in_memory().unwrap();
        let who = PrincipalId::system();
        ensure_factory_backends(&mut db, who).unwrap();

        // Operator re-points a factory backend at a gateway and re-pins a window.
        let mut anthropic = db.get_backend_by_name("anthropic").unwrap().unwrap();
        anthropic.base_url = Some("https://gateway.internal/v1".into());
        db.upsert_backend(&anthropic).unwrap();
        db.set_backend_model(&BackendModelRow {
            backend_id: anthropic.backend_id,
            model_id: "claude-opus-5".into(),
            context_window: Some(123_456),
            extra: None,
        })
        .unwrap();

        // The floor pass leaves both alone.
        ensure_factory_backends(&mut db, who).unwrap();
        let after_ensure = db.get_backend_by_name("anthropic").unwrap().unwrap();
        assert_eq!(after_ensure.base_url.as_deref(), Some("https://gateway.internal/v1"));

        // Reseed force-restores them — and keeps the same backend_id, so
        // aliases pointing at it survive.
        reseed_factory_backends(&mut db, who).unwrap();
        let after_reseed = db.get_backend_by_name("anthropic").unwrap().unwrap();
        assert_eq!(after_reseed.backend_id, anthropic.backend_id, "id is stable across reseed");
        assert_eq!(after_reseed.base_url, None);
        let window = db
            .list_backend_models(after_reseed.backend_id)
            .unwrap()
            .into_iter()
            .find(|m| m.model_id == "claude-opus-5")
            .unwrap()
            .context_window;
        assert_eq!(window, Some(1_000_000));
    }

    #[test]
    fn reseed_leaves_operator_added_backends_alone() {
        let mut db = KernelDb::in_memory().unwrap();
        let who = PrincipalId::system();
        ensure_factory_backends(&mut db, who).unwrap();
        db.upsert_backend(&BackendRow {
            backend_id: BackendId::new(),
            name: "zorak".into(),
            kind: "openai".into(),
            base_url: Some("http://zorak:8080/v1".into()),
            api_key_env: None,
            api_key_file: None,
            key_optional: true,
            request_timeout_secs: Some(600),
            created_at: 0,
            created_by: who,
        })
        .unwrap();
        reseed_factory_backends(&mut db, who).unwrap();
        assert!(db.get_backend_by_name("zorak").unwrap().is_some());
    }

    #[test]
    fn factory_name_predicate_matches_the_table() {
        for b in FACTORY_BACKENDS {
            assert!(is_factory_backend_name(b.name));
        }
        assert!(!is_factory_backend_name("zorak"));
    }

    #[test]
    fn factory_defaults_name_a_factory_backend() {
        assert!(is_factory_backend_name(FACTORY_DEFAULT_BACKEND));
        assert!(!FACTORY_DEFAULT_MODEL.is_empty());
    }

    #[test]
    fn no_dated_model_ids_in_the_floor() {
        // Dated ids get retired out from under us (claude-sonnet-4-20250514
        // and claude-opus-4-20250514 both 404 today, and the first backed the
        // `default` alias). Undated ids roll forward.
        for fb in FACTORY_BACKENDS {
            for (model, _) in fb.models {
                assert!(
                    !model.contains("-2025") && !model.contains("-2026"),
                    "{model} looks like a dated id"
                );
            }
        }
    }
}
