//! Backends for unit tests. A kernel ships no backends, so each test that
//! needs one writes its own rows.
//!
//! [`example_backends`] writes `anthropic`, `deepseek`, `gpt`, and `ollama`
//! with a few model rows, and makes `deepseek/deepseek-flash` the default
//! with a `max_tokens` of 16384.

use kaijutsu_types::{BackendId, PrincipalId};

use crate::kernel_db::{BackendModelRow, BackendRow, KernelDb, LlmDefaultsRow, ModelTunablesRow};

/// Write one backend row and return it.
pub(crate) fn backend(
    db: &KernelDb,
    name: &str,
    kind: &str,
    base_url: Option<&str>,
    api_key_env: Option<&str>,
    api_key_file: Option<&str>,
    key_optional: bool,
) -> BackendRow {
    db.upsert_backend(&BackendRow {
        backend_id: BackendId::new(),
        name: name.to_string(),
        kind: kind.to_string(),
        base_url: base_url.map(str::to_string),
        api_key_env: api_key_env.map(str::to_string),
        api_key_file: api_key_file.map(str::to_string),
        key_optional,
        request_timeout_secs: None,
        idle_timeout_secs: None,
        max_concurrent: None,
        created_at: 0,
        created_by: PrincipalId::system(),
    })
    .expect("write a test backend row")
}

fn model(db: &KernelDb, backend: &BackendRow, model_id: &str, window: u64, tunables: ModelTunablesRow) {
    db.set_backend_model(&BackendModelRow {
        backend_id: backend.backend_id,
        model_id: model_id.to_string(),
        context_window: Some(window as i64),
        extra: None,
        tunables,
    })
    .expect("write a test model row");
}

/// Write the four example backends, their model rows, and the defaults.
pub(crate) fn example_backends(db: &KernelDb) {
    let anthropic = backend(
        db, "anthropic", "anthropic", None,
        Some("ANTHROPIC_API_KEY"), Some("~/.anthropic-key.txt"), false,
    );
    model(db, &anthropic, "claude-haiku-4-5", 200_000, ModelTunablesRow::default());
    for id in ["claude-opus-4-8", "claude-opus-5"] {
        model(db, &anthropic, id, 1_000_000, ModelTunablesRow::default());
    }

    let deepseek = backend(
        db, "deepseek", "deepseek", None,
        Some("DEEPSEEK_API_KEY"), Some("~/.deepseek-key"), false,
    );
    for id in ["deepseek-flash", "deepseek-v4-pro"] {
        model(db, &deepseek, id, 1_000_000, ModelTunablesRow {
            max_tokens: Some(65536),
            effort: Some("high".to_string()),
            ..Default::default()
        });
    }

    backend(
        db, "gpt", "openai", Some("https://api.openai.com/v1"),
        Some("OPENAI_API_KEY"), Some("~/.openai-key.txt"), false,
    );
    backend(db, "ollama", "openai", Some("http://localhost:11434/v1"), None, None, true);

    db.set_llm_defaults(&LlmDefaultsRow {
        default_backend: "deepseek".to_string(),
        default_model: "deepseek-flash".to_string(),
        max_tokens: Some(16384),
        temperature: None,
        top_p: None,
        effort: None,
        thinking_budget: None,
        thinking_style: None,
    })
    .expect("write the test defaults");
}
