//! `kj embedding` — the model the semantic index embeds with.
//!
//! One setting serves the whole kernel: every context is searched in one
//! vector space, so one model makes every vector. That model is a
//! `backend_models` row on an `openai` or `council` backend, and the row holds
//! the model's prompt profile (`kj backend model set --embedding-profile`).
//! The `embedding_config` row points at it by `(backend_id, model)`, so the
//! backend and the model row cannot be removed while the index uses them.
//!
//! kaijutsu-server reads the setting when it starts; `kj embedding show` says
//! when the attached index differs from it.

use clap::{Parser, Subcommand};
use kaijutsu_types::ContentType;

use crate::kernel_db::EmbeddingConfigRow;

use super::{KjCaller, KjDispatcher, KjResult, clap_help_for};

#[derive(Parser, Debug)]
#[command(
    name = "embedding",
    about = "The model the semantic index embeds with: one for the whole kernel.",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct EmbeddingArgs {
    #[command(subcommand)]
    command: EmbeddingCommand,
}

#[derive(Subcommand, Debug)]
enum EmbeddingCommand {
    /// Show the embedding model, its settings, and whether the index is attached.
    Show,
    /// Set the embedding model. Applies when kaijutsu-server restarts.
    ///
    /// kj backend model set ollama embeddinggemma-2:270m-q8_0-text --embedding-profile embeddinggemma
    ///
    /// kj embedding set ollama/embeddinggemma-2:270m-q8_0-text --dimensions 768
    ///
    /// This declares the whole setting: --timeout-ms, --max-in-flight and
    /// --max-context-bytes take their defaults when omitted. A different
    /// model, width, profile or endpoint empties the index; `kj synth`
    /// fills it again.
    Set {
        /// The model as <backend>/<model>. The backend is of kind openai or
        /// council, and the model has an embedding profile
        #[arg(value_name = "BACKEND/MODEL")]
        model: String,
        /// Vector width to ask the backend for and to store
        #[arg(long)]
        dimensions: u32,
        /// Milliseconds one embedding request may take, queue time included
        #[arg(long = "timeout-ms", default_value_t = 30_000)]
        timeout_ms: u32,
        /// The most embedding requests in flight at once
        #[arg(long = "max-in-flight", default_value_t = 2)]
        max_in_flight: u32,
        /// Bytes of a context's text that are embedded for search
        #[arg(long = "max-context-bytes", default_value_t = 2048)]
        max_context_bytes: u32,
    },
    /// Turn the semantic index off when kaijutsu-server restarts. The setting
    /// stays; `kj embedding set` turns it on again.
    Off,
}

impl KjDispatcher {
    pub(crate) async fn dispatch_embedding(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<EmbeddingArgs>();
        }
        let parsed = match EmbeddingArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj embedding: {e}"));
            }
        };
        if !matches!(parsed.command, EmbeddingCommand::Show)
            && let Err(denied) = self.require_cap(caller, crate::mcp::Capability::ConfigWrite, "embedding")
        {
            return denied;
        }
        match parsed.command {
            EmbeddingCommand::Show => self.embedding_show(),
            EmbeddingCommand::Set { model, dimensions, timeout_ms, max_in_flight, max_context_bytes } => {
                self.embedding_set(&model, dimensions, timeout_ms, max_in_flight, max_context_bytes)
            }
            EmbeddingCommand::Off => self.embedding_off(),
        }
    }

    fn embedding_show(&self) -> KjResult {
        let (row, backend_name, checked) = {
            let db = self.kernel_db().lock();
            let row = match db.get_embedding_config() {
                Ok(Some(row)) => row,
                Ok(None) => {
                    return KjResult::ok_with_data(
                        "No embedding model is set, so the semantic index is off. Set one with \
                         `kj embedding set <backend>/<model> --dimensions <n>`."
                            .to_string(),
                        serde_json::Value::Null,
                    );
                }
                Err(e) => return KjResult::Err(format!("kj embedding show: {e}")),
            };
            let backend_name = match db.get_backend(row.backend_id) {
                Ok(Some(b)) => b.name,
                Ok(None) => return KjResult::Err("kj embedding show: the embedding backend row is missing".into()),
                Err(e) => return KjResult::Err(format!("kj embedding show: {e}")),
            };
            let checked = crate::llm::resolve_embedding(&db, &row);
            (row, backend_name, checked)
        };
        let mut lines = vec![
            format!("Model: {backend_name}/{}", row.model),
            format!("State: {}", if row.enabled { "on" } else { "off" }),
            format!("Dimensions: {}", row.dimensions),
            format!("Timeout: {} ms", row.timeout_ms),
            format!("Max in flight: {}", row.max_in_flight),
            format!("Max context bytes: {}", row.max_context_bytes),
        ];
        // An OpenAI-compatible embedder's revision names its profile and
        // endpoint; a council server names its own, so only model and width
        // are compared there.
        let expected_revision = checked.as_ref().ok()
            .filter(|config| config.backend.kind == crate::llm::BackendKind::OpenAi)
            .and_then(|config| config.backend.base_url.as_deref()
                .map(|url| kaijutsu_index::openai_revision(config.profile, url)));
        let profile = match &checked {
            Ok(config) => {
                lines.insert(1, format!("Profile: {}", config.profile.as_str()));
                Some(config.profile.as_str())
            }
            Err(e) => {
                lines.push(format!("Problem: {e}"));
                None
            }
        };
        let index = self.semantic_index();
        let attached = index.as_ref().map(|index| {
            let embedder = index.embedder();
            (embedder.model_name().to_string(), embedder.dimensions(), index.len(), embedder.revision().to_string())
        });
        lines.push(match &attached {
            None => "Index: not attached".to_string(),
            Some((model, dimensions, contexts, revision)) => {
                let matches = row.enabled && *model == row.model && *dimensions as i64 == row.dimensions
                    && expected_revision.as_ref().is_none_or(|expected| expected == revision);
                let mut line = format!("Index: attached, {model} at {dimensions} dimensions, {contexts} contexts");
                if !matches {
                    line.push_str("; differs from the setting until kaijutsu-server restarts");
                }
                line
            }
        });
        if attached.is_none() && row.enabled && checked.is_ok() {
            lines.push("  The server attaches the index after it starts and the backend answers.".into());
        }
        let data = serde_json::json!({
            "backend": backend_name,
            "model": row.model,
            "enabled": row.enabled,
            "profile": profile,
            "dimensions": row.dimensions,
            "timeout_ms": row.timeout_ms,
            "max_in_flight": row.max_in_flight,
            "max_context_bytes": row.max_context_bytes,
            "problem": checked.as_ref().err().map(|e| e.to_string()),
            "attached": attached.as_ref().map(|(model, dimensions, contexts, _)| serde_json::json!({
                "model": model, "dimensions": dimensions, "contexts": contexts,
            })),
        });
        KjResult::ok_with_data(lines.join("\n"), data)
    }

    fn embedding_set(
        &self,
        spec: &str,
        dimensions: u32,
        timeout_ms: u32,
        max_in_flight: u32,
        max_context_bytes: u32,
    ) -> KjResult {
        let (backend, model) = match super::slot::split_backend_model(spec) {
            Ok(pair) => pair,
            Err(e) => return KjResult::Err(format!("kj embedding set: {e}")),
        };
        let db = self.kernel_db().lock();
        let backend_id = match db.get_backend_by_name(&backend) {
            Ok(Some(b)) => b.backend_id,
            Ok(None) => {
                return KjResult::Err(format!(
                    "kj embedding set: backend '{backend}' not found; `kj backend list` names the backends"
                ));
            }
            Err(e) => return KjResult::Err(format!("kj embedding set: {e}")),
        };
        let row = EmbeddingConfigRow {
            enabled: true,
            backend_id,
            model: model.clone(),
            dimensions: dimensions.into(),
            timeout_ms: timeout_ms.into(),
            max_in_flight: max_in_flight.into(),
            max_context_bytes: max_context_bytes.into(),
        };
        // The checks startup runs, so a setting that cannot work is refused
        // here rather than leaving the index off after the next restart.
        let config = match crate::llm::resolve_embedding(&db, &row) {
            Ok(config) => config,
            Err(e) => return KjResult::Err(format!("kj embedding set: {e}")),
        };
        if let Err(e) = db.set_embedding_config(&row) {
            return KjResult::Err(format!("kj embedding set: {e}"));
        }
        KjResult::ok(format!(
            "set the embedding model to {backend}/{model} at {dimensions} dimensions, profile {}; \
             restart kaijutsu-server to apply",
            config.profile.as_str()
        ))
    }

    fn embedding_off(&self) -> KjResult {
        let db = self.kernel_db().lock();
        let row = match db.get_embedding_config() {
            Ok(Some(row)) => row,
            Ok(None) => return KjResult::Err("kj embedding off: no embedding model is set".into()),
            Err(e) => return KjResult::Err(format!("kj embedding off: {e}")),
        };
        match db.set_embedding_config(&EmbeddingConfigRow { enabled: false, ..row }) {
            Ok(()) => KjResult::ok(
                "the semantic index turns off when kaijutsu-server restarts; `kj embedding set` turns it on again"
                    .to_string(),
            ),
            Err(e) => KjResult::Err(format!("kj embedding off: {e}")),
        }
    }
}

// Verb class: kj/effect.rs
use super::effect::{Classify, Effect};

impl Classify for EmbeddingArgs {
    fn effect(&self) -> Effect {
        match self.command {
            EmbeddingCommand::Show => Effect::Read,
            EmbeddingCommand::Set { .. } | EmbeddingCommand::Off => Effect::Write,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{test_caller, test_dispatcher};
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    async fn run(d: &KjDispatcher, parts: &[&str]) -> KjResult {
        d.dispatch(&argv(parts), &test_caller()).await
    }

    async fn seeded() -> KjDispatcher {
        let d = test_dispatcher().await;
        {
            let db = d.kernel_db().lock();
            crate::test_backends::example_backends(&db);
            crate::test_backends::backend(&db, "spaceghost", "council", Some("http://100.65.47.5:8091"), None, None, true);
        }
        d.reload_llm_registry().await.unwrap();
        d
    }

    const MODEL: &str = "embeddinggemma-2:270m-q8_0-text";

    /// The profile the model row records, so the index adds the right prompts.
    async fn profile(d: &KjDispatcher, backend: &str, model: &str, profile: &str) {
        let r = run(d, &["backend", "model", "set", backend, model, "--embedding-profile", profile]).await;
        assert!(r.is_ok(), "{}", r.message());
    }

    /// The happy path: a model row with a profile, then the setting; `show`
    /// reports both and says the index is not attached yet.
    #[tokio::test]
    async fn set_then_show_reports_the_model_its_profile_and_the_index() {
        let d = seeded().await;
        profile(&d, "ollama", MODEL, "embeddinggemma").await;
        let r = run(&d, &["embedding", "set", &format!("ollama/{MODEL}"), "--dimensions", "768"]).await;
        assert!(r.is_ok(), "{}", r.message());
        assert!(r.message().contains("restart kaijutsu-server"), "says when it applies: {}", r.message());

        let row = d.kernel_db().lock().get_embedding_config().unwrap().expect("written");
        assert!(row.enabled);
        assert_eq!((row.model.as_str(), row.dimensions, row.timeout_ms, row.max_in_flight, row.max_context_bytes),
            (MODEL, 768, 30_000, 2, 2048), "omitted flags take their defaults");

        let show = run(&d, &["embedding", "show"]).await;
        let text = show.message();
        assert!(text.contains(&format!("Model: ollama/{MODEL}")), "{text}");
        assert!(text.contains("Profile: embeddinggemma"), "{text}");
        assert!(text.contains("Dimensions: 768"), "{text}");
        assert!(text.contains("Index: not attached"), "{text}");
        assert!(!text.contains("Problem"), "{text}");
    }

    /// A model with no recorded profile cannot be the embedding model: the
    /// refusal says how to record one, and nothing is written.
    #[tokio::test]
    async fn set_refuses_a_model_without_a_profile_and_names_the_command() {
        let d = seeded().await;
        let r = run(&d, &["embedding", "set", &format!("ollama/{MODEL}"), "--dimensions", "768"]).await;
        assert!(!r.is_ok());
        assert!(r.message().contains("--embedding-profile"), "{}", r.message());
        assert!(d.kernel_db().lock().get_embedding_config().unwrap().is_none(), "a refusal writes nothing");
    }

    /// Only an openai or council backend serves embeddings.
    #[tokio::test]
    async fn set_refuses_a_backend_that_cannot_embed() {
        let d = seeded().await;
        profile(&d, "anthropic", "claude-haiku-4-5", "plain").await;
        let r = run(&d, &["embedding", "set", "anthropic/claude-haiku-4-5", "--dimensions", "768"]).await;
        assert!(!r.is_ok());
        assert!(r.message().contains("openai") && r.message().contains("council"), "{}", r.message());
    }

    /// A council server adds its own task prompts, so its model takes `plain`.
    #[tokio::test]
    async fn set_refuses_a_council_model_with_a_prompt_profile() {
        let d = seeded().await;
        profile(&d, "spaceghost", "embed-1", "qwen3").await;
        let r = run(&d, &["embedding", "set", "spaceghost/embed-1", "--dimensions", "1024"]).await;
        assert!(!r.is_ok());
        assert!(r.message().contains("plain"), "{}", r.message());
        profile(&d, "spaceghost", "embed-1", "plain").await;
        let r = run(&d, &["embedding", "set", "spaceghost/embed-1", "--dimensions", "1024"]).await;
        assert!(r.is_ok(), "{}", r.message());
    }

    #[tokio::test]
    async fn set_refuses_an_unknown_backend_and_a_bare_model() {
        let d = seeded().await;
        let r = run(&d, &["embedding", "set", "nope/m", "--dimensions", "8"]).await;
        assert!(!r.is_ok() && r.message().contains("'nope' not found"), "{}", r.message());
        let r = run(&d, &["embedding", "set", MODEL, "--dimensions", "8"]).await;
        assert!(!r.is_ok() && r.message().contains("<backend>/<model>"), "{}", r.message());
    }

    /// `off` keeps the setting and marks it off; with nothing set it refuses.
    #[tokio::test]
    async fn off_keeps_the_setting_and_refuses_when_nothing_is_set() {
        let d = seeded().await;
        let r = run(&d, &["embedding", "off"]).await;
        assert!(!r.is_ok() && r.message().contains("no embedding model is set"), "{}", r.message());

        profile(&d, "ollama", MODEL, "embeddinggemma").await;
        assert!(run(&d, &["embedding", "set", &format!("ollama/{MODEL}"), "--dimensions", "768"]).await.is_ok());
        let r = run(&d, &["embedding", "off"]).await;
        assert!(r.is_ok(), "{}", r.message());
        let row = d.kernel_db().lock().get_embedding_config().unwrap().expect("the setting stays");
        assert!(!row.enabled);
        assert_eq!(row.model, MODEL);
        assert!(run(&d, &["embedding", "show"]).await.message().contains("State: off"));
    }

    /// The model row is what the setting points at: it cannot be removed or
    /// lose its profile while the index uses it.
    #[tokio::test]
    async fn the_embedding_model_row_cannot_be_removed_or_lose_its_profile() {
        let d = seeded().await;
        profile(&d, "ollama", MODEL, "embeddinggemma").await;
        assert!(run(&d, &["embedding", "set", &format!("ollama/{MODEL}"), "--dimensions", "768"]).await.is_ok());
        let r = run(&d, &["backend", "model", "remove", "ollama", MODEL]).await;
        assert!(!r.is_ok() && r.message().contains("kj embedding"), "{}", r.message());
        let r = run(&d, &["backend", "model", "set", "ollama", MODEL, "--clear", "embedding-profile"]).await;
        assert!(!r.is_ok() && r.message().contains("kj embedding"), "{}", r.message());
    }

    /// Whatever command changes the backend or the model row, the setting
    /// stays one that can run: a council model keeps `plain`, and the
    /// backend keeps a kind that embeds.
    #[tokio::test]
    async fn a_change_that_would_stop_the_embedding_model_is_refused() {
        let d = seeded().await;
        profile(&d, "spaceghost", "embed-1", "plain").await;
        assert!(run(&d, &["embedding", "set", "spaceghost/embed-1", "--dimensions", "1024"]).await.is_ok());
        let r = run(&d, &["backend", "model", "set", "spaceghost", "embed-1", "--embedding-profile", "qwen3"]).await;
        assert!(!r.is_ok() && r.message().contains("plain"), "{}", r.message());
        let r = run(&d, &["backend", "set", "spaceghost", "--kind", "anthropic"]).await;
        assert!(!r.is_ok() && r.message().contains("kj embedding"), "{}", r.message());
        let r = run(&d, &["backend", "set", "spaceghost", "--kind", "openai", "--base-url", "http://127.0.0.1:9/v1"]).await;
        assert!(r.is_ok(), "openai embeds too: {}", r.message());
    }

    /// An embedder that answers nothing, standing in for the one the server
    /// attached at start.
    struct Attached { model: String, revision: String, dimensions: usize }

    #[async_trait::async_trait]
    impl kaijutsu_index::Embedder for Attached {
        fn model_name(&self) -> &str { &self.model }
        fn revision(&self) -> &str { &self.revision }
        fn dimensions(&self) -> usize { self.dimensions }
        async fn embed_batch(&self, _: &[&str], _: kaijutsu_index::EmbeddingPurpose)
            -> Result<Vec<Vec<f32>>, kaijutsu_index::IndexError> {
            Err(kaijutsu_index::IndexError::Embedding("not in this test".into()))
        }
    }

    /// `show` compares the attached index with the setting by model, width,
    /// profile and endpoint: a changed profile puts vectors in another space,
    /// so it differs until a restart.
    #[tokio::test]
    async fn show_says_when_the_attached_index_differs_from_the_setting() {
        let d = seeded().await;
        let dir = tempfile::tempdir().unwrap();
        profile(&d, "ollama", MODEL, "embeddinggemma").await;
        assert!(run(&d, &["embedding", "set", &format!("ollama/{MODEL}"), "--dimensions", "768"]).await.is_ok());
        let revision = kaijutsu_index::openai_revision(
            kaijutsu_index::EmbeddingProfile::EmbeddingGemma, "http://localhost:11434/v1");
        let index = kaijutsu_index::SemanticIndex::new(
            kaijutsu_index::IndexConfig::new(768, 2048, dir.path()),
            Box::new(Attached { model: MODEL.into(), revision, dimensions: 768 }),
        ).unwrap();
        d.set_semantic_index(Some(std::sync::Arc::new(index)));

        let text = run(&d, &["embedding", "show"]).await.message().to_string();
        assert!(text.contains("Index: attached") && !text.contains("differs"), "{text}");

        profile(&d, "ollama", MODEL, "plain").await;
        let text = run(&d, &["embedding", "show"]).await.message().to_string();
        assert!(text.contains("differs from the setting"), "a profile change is a different space: {text}");
    }

    #[tokio::test]
    async fn help_shows_the_example_and_the_model_form() {
        let d = seeded().await;
        let help = run(&d, &["embedding", "set", "--help"]).await.message().to_string();
        assert!(help.contains("<BACKEND/MODEL>"), "{help}");
        assert!(help.contains("kj embedding set ollama/"), "{help}");
    }
}
