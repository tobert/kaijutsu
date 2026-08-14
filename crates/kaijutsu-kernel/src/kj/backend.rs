//! `kj backend` — the SQL-native LLM backend surface.
//!
//! A **backend** is one configured endpoint: a free-form unique `name` (the
//! handle you type) plus a closed `kind` (`anthropic` | `deepseek` | `openai` |
//! `codex-app`)
//! that picks which client speaks for it. That split is the point of this
//! renovation — the demolished `models.toml` made the `[providers.<name>]`
//! table name BE the provider type, so two Anthropic gateways were
//! inexpressible and a local server had to be called `ollama`/`lemonade`/
//! `local` because those strings were hardcoded provider types.
//!
//! Rows live in `kernel_db` (`backends`, `backend_models`, `llm_defaults`).
//! There is no TOML in this path and no host file to edit. **No key material
//! is ever stored** — only an env-var NAME (`--api-key-env`) or a file path
//! (`--api-key-file`).
//!
//! Every mutation ends with [`KjDispatcher::reload_llm_registry`], so a
//! backend change takes effect on the next turn with no kernel restart.

use clap::{Parser, Subcommand};
use kaijutsu_types::{BackendId, ContentType};

use crate::kernel_db::{BackendModelRow, BackendRow, LlmDefaultsRow};
use crate::llm::BackendKind;

use super::{KjCaller, KjDispatcher, KjResult, clap_help_for};

#[derive(Parser, Debug)]
#[command(
    name = "backend",
    about = "LLM backends: one row per endpoint (name + kind). SQL-native — no models.toml.",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct BackendArgs {
    #[command(subcommand)]
    command: BackendCommand,
}

#[derive(Subcommand, Debug)]
enum BackendCommand {
    /// List configured backends.
    #[command(alias = "ls")]
    List,
    /// Show one backend, its models, and its resolved key source.
    Show {
        /// Backend name
        name: String,
    },
    /// Create or update a backend (upsert on `name`).
    ///
    /// This declares the WHOLE row: every field you omit is cleared, so a
    /// half-applied config can't happen behind your back. Re-state the flags
    /// you want to keep.
    Set {
        /// Backend name — free-form, unique (e.g. anthropic, gpt, zorak)
        name: String,
        /// Wire dialect: anthropic | deepseek | openai | codex-app
        #[arg(long)]
        kind: String,
        /// Endpoint URL. REQUIRED for --kind openai (it says WHICH
        /// OpenAI-compatible server); optional gateway override for
        /// anthropic; unnecessary for deepseek; required for codex-app.
        #[arg(long = "base-url")]
        base_url: Option<String>,
        /// Environment variable NAME holding the API key (never the key)
        #[arg(long = "api-key-env")]
        api_key_env: Option<String>,
        /// Path to a file whose trimmed contents are the API key (~ expanded).
        /// Tried before the env var.
        #[arg(long = "api-key-file")]
        api_key_file: Option<String>,
        /// Register even with no resolvable key, using a placeholder — for
        /// gateways where auth is network identity, not a bearer token.
        #[arg(long = "key-optional")]
        key_optional: bool,
        /// Per-request timeout in seconds (stored; wiring is Track B)
        #[arg(long = "request-timeout")]
        request_timeout: Option<u64>,
    },
    /// Remove a backend. Refused while a cast slot or alias still points at it.
    #[command(alias = "rm")]
    Remove {
        /// Backend name
        name: String,
    },
    /// Per-model metadata (context windows and sparse JSON extras).
    #[command(subcommand)]
    Model(BackendModelCommand),
    /// Show or set the kernel-wide LLM defaults (`llm_defaults`).
    #[command(subcommand)]
    Default(BackendDefaultCommand),
    /// Restore the factory backends, model windows, aliases, and defaults to
    /// their embedded definitions. Operator-added backends are left alone.
    Reseed,
}

#[derive(Subcommand, Debug)]
enum BackendModelCommand {
    /// Pin (or re-pin) one model's metadata on a backend.
    Set {
        /// Backend name
        backend: String,
        /// Model id as the provider spells it
        model: String,
        /// Total context window (input + output) in tokens. Omit when
        /// unknown — an honest "unknown" beats a fabricated denominator.
        #[arg(long = "context-window")]
        context_window: Option<u64>,
        /// Sparse provider-specific JSON
        #[arg(long)]
        extra: Option<String>,
    },
    /// Drop one model's metadata row.
    #[command(alias = "rm")]
    Remove {
        /// Backend name
        backend: String,
        /// Model id
        model: String,
    },
}

#[derive(Subcommand, Debug)]
enum BackendDefaultCommand {
    /// Print the current defaults.
    Show,
    /// Update the defaults. Only the flags you pass change.
    Set {
        /// Default backend name
        #[arg(long)]
        backend: Option<String>,
        /// Default model id
        #[arg(long)]
        model: Option<String>,
        /// Maximum RESPONSE tokens (not the context window)
        #[arg(long = "max-tokens")]
        max_tokens: Option<u64>,
        /// Sampling temperature, 0.0..=2.0
        #[arg(long)]
        temperature: Option<f64>,
        /// Nucleus sampling, (0.0, 1.0]
        #[arg(long = "top-p")]
        top_p: Option<f64>,
        /// Provider effort ladder token (e.g. low, high, max)
        #[arg(long)]
        effort: Option<String>,
        /// Extended-thinking token budget
        #[arg(long = "thinking-budget")]
        thinking_budget: Option<u64>,
        /// Extended-thinking style token
        #[arg(long = "thinking-style")]
        thinking_style: Option<String>,
    },
}

impl KjDispatcher {
    pub(crate) async fn dispatch_backend(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<BackendArgs>();
        }
        let parsed = match BackendArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj backend: {e}"));
            }
        };

        // Which model runs every turn is exactly what `config-write` exists to
        // protect (it used to gate `kj config set models.toml`); list/show stay
        // ungated, discovery is not escalation.
        let mutating = !matches!(
            parsed.command,
            BackendCommand::List
                | BackendCommand::Show { .. }
                | BackendCommand::Default(BackendDefaultCommand::Show)
        );
        if mutating
            && let Err(denied) =
                self.require_cap(caller, crate::mcp::Capability::ConfigWrite, "backend")
        {
            return denied;
        }

        // Set aside so the post-reload registration check below (the
        // clear-on-omit sharp edge fix) knows which backend name to look up
        // — the upsert can succeed while the registry still warn-skips it.
        let mut newly_set_backend: Option<String> = None;
        let mut result = match parsed.command {
            BackendCommand::List => self.backend_list(),
            BackendCommand::Show { name } => self.backend_show(&name),
            BackendCommand::Set {
                name,
                kind,
                base_url,
                api_key_env,
                api_key_file,
                key_optional,
                request_timeout,
            } => {
                newly_set_backend = Some(name.clone());
                self.backend_set(
                    &name,
                    &kind,
                    base_url,
                    api_key_env,
                    api_key_file,
                    key_optional,
                    request_timeout,
                    caller,
                )
            }
            BackendCommand::Remove { name } => self.backend_remove(&name),
            BackendCommand::Model(cmd) => self.backend_model(cmd),
            BackendCommand::Default(BackendDefaultCommand::Show) => self.backend_default_show(),
            BackendCommand::Default(BackendDefaultCommand::Set {
                backend,
                model,
                max_tokens,
                temperature,
                top_p,
                effort,
                thinking_budget,
                thinking_style,
            }) => self.backend_default_set(
                backend,
                model,
                max_tokens,
                temperature,
                top_p,
                effort,
                thinking_budget,
                thinking_style,
            ),
            BackendCommand::Reseed => self.backend_reseed(caller),
        };

        if mutating && matches!(result, KjResult::Ok { .. }) {
            // Live reload: the registry is a snapshot, so a write is only real
            // once it's swapped in. A failure here is loud — a silent one
            // would leave the operator's change invisible until a restart,
            // which is precisely the papercut this renovation removes.
            if let Err(e) = self.reload_llm_registry().await {
                return KjResult::Err(format!(
                    "kj backend: write landed but the LLM registry failed to reload: {e}"
                ));
            }
            // `backend set` is a declare-the-whole-row upsert (documented
            // policy — a partial update silently clears an omitted key
            // source). The write can therefore succeed while the registry
            // warn-skips the backend at build time (missing/invalid key,
            // say) — the row lands but nothing serves requests until fixed.
            // Say so here rather than leaving the operator to grep server
            // logs for a warning they don't know to look for.
            if let Some(name) = &newly_set_backend {
                let registered = self.kernel().llm().read().await.get(name).is_some();
                if let KjResult::Ok { message, .. } = &mut result {
                    if registered {
                        message.push_str(" — registered and live");
                    } else {
                        message.push_str(
                            " — WARNING: not registered (check server logs); the row \
                             was written but the registry skipped it, most likely a \
                             missing or unresolvable API key. It will not serve \
                             requests until `kj backend set` is retried with a fix.",
                        );
                    }
                }
            }
        }
        result
    }

    fn backend_list(&self) -> KjResult {
        let db = self.kernel_db().lock();
        let backends = match db.list_backends() {
            Ok(b) => b,
            Err(e) => return KjResult::Err(format!("kj backend list: {e}")),
        };
        // Iteration handles: a backend's NAME is its full, canonical handle
        // (UNIQUE, and what `--model <name>/<model>` takes), so no truncation.
        let names = serde_json::Value::Array(
            backends
                .iter()
                .map(|b| serde_json::Value::String(b.name.clone()))
                .collect(),
        );
        if backends.is_empty() {
            return KjResult::ok_with_data(
                "(no backends configured — `kj backend reseed` restores the factory floor)"
                    .to_string(),
                names,
            );
        }
        let default_backend = db
            .get_llm_defaults()
            .ok()
            .flatten()
            .map(|d| d.default_backend);
        let lines: Vec<String> = backends
            .iter()
            .map(|b| {
                let marker = if default_backend.as_deref() == Some(b.name.as_str()) {
                    " *"
                } else {
                    "  "
                };
                let url = b.base_url.as_deref().unwrap_or("");
                format!("{marker}{:<14} {:<10} {url}", b.name, b.kind)
            })
            .collect();
        KjResult::ok_with_data(lines.join("\n"), names)
    }

    fn backend_show(&self, name: &str) -> KjResult {
        let db = self.kernel_db().lock();
        let backend = match db.get_backend_by_name(name) {
            Ok(Some(b)) => b,
            Ok(None) => return KjResult::Err(format!("kj backend show: '{name}' not found")),
            Err(e) => return KjResult::Err(format!("kj backend show: {e}")),
        };
        let models = match db.list_backend_models(backend.backend_id) {
            Ok(m) => m,
            Err(e) => return KjResult::Err(format!("kj backend show: {e}")),
        };
        let mut lines = vec![
            format!("Backend: {}", backend.name),
            format!("Kind: {}", backend.kind),
        ];
        if let Some(u) = &backend.base_url {
            lines.push(format!("Base URL: {u}"));
        }
        // Key SOURCE, never the key. Report whether each declared source
        // currently resolves so a missing file/unset var is visible here
        // instead of at turn time.
        match &backend.api_key_file {
            Some(f) => lines.push(format!(
                "Key file: {f} ({})",
                if std::fs::read_to_string(shellexpand::tilde(f).as_ref())
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false)
                {
                    "readable"
                } else {
                    "UNREADABLE"
                }
            )),
            None => lines.push("Key file: (none)".to_string()),
        }
        let env_name = backend.api_key_env.clone().or_else(|| {
            BackendKind::parse(&backend.kind)
                .ok()
                .and_then(|k| k.standard_env_var().map(str::to_string))
        });
        match &env_name {
            Some(v) => lines.push(format!(
                "Key env: {v} ({})",
                if std::env::var(v).is_ok() { "set" } else { "UNSET" }
            )),
            None => lines.push("Key env: (none)".to_string()),
        }
        lines.push(format!("Key optional: {}", backend.key_optional));
        if let Some(t) = backend.request_timeout_secs {
            lines.push(format!("Request timeout: {t}s"));
        }
        if models.is_empty() {
            lines.push("Models: (none pinned — context windows resolve as unknown)".to_string());
        } else {
            lines.push("Models:".to_string());
            for m in &models {
                let w = m
                    .context_window
                    .map(|w| format!("{w} ctx"))
                    .unwrap_or_else(|| "unknown ctx".to_string());
                lines.push(format!("  {:<24} {w}", m.model_id));
            }
        }

        let data = serde_json::json!({
            "name": backend.name,
            "kind": backend.kind,
            "base_url": backend.base_url,
            "api_key_env": backend.api_key_env,
            "api_key_file": backend.api_key_file,
            "key_optional": backend.key_optional,
            "request_timeout_secs": backend.request_timeout_secs,
            "models": models.iter().map(|m| serde_json::json!({
                "model": m.model_id,
                "context_window": m.context_window,
                "extra": m.extra,
            })).collect::<Vec<_>>(),
        });
        KjResult::ok_with_data(lines.join("\n"), data)
    }

    #[allow(clippy::too_many_arguments)]
    fn backend_set(
        &self,
        name: &str,
        kind: &str,
        base_url: Option<String>,
        api_key_env: Option<String>,
        api_key_file: Option<String>,
        key_optional: bool,
        request_timeout: Option<u64>,
        caller: &KjCaller,
    ) -> KjResult {
        // Unknown kind fails here, naming the closed set — never a silent
        // fallback that would point a typo'd backend at the wrong API.
        let parsed_kind = match BackendKind::parse(kind) {
            Ok(k) => k,
            Err(msg) => return KjResult::Err(format!("kj backend set: {msg}")),
        };
        // Reuse the one structural validator every write surface shares.
        let mut probe = crate::llm::BackendConfig::new(name, parsed_kind);
        probe.base_url = base_url.clone();
        probe.request_timeout_secs = request_timeout;
        if let Err(msg) = probe.validate() {
            return KjResult::Err(format!("kj backend set: {msg}"));
        }

        let db = self.kernel_db().lock();
        let stored = db.upsert_backend(&BackendRow {
            backend_id: BackendId::new(),
            name: name.to_string(),
            kind: parsed_kind.as_str().to_string(),
            base_url,
            api_key_env,
            api_key_file,
            key_optional,
            request_timeout_secs: request_timeout.map(|t| t as i64),
            created_at: kaijutsu_types::now_millis() as i64,
            created_by: caller.principal_id,
        });
        match stored {
            Ok(row) => KjResult::ok(format!(
                "set backend '{}' (kind={})",
                row.name, row.kind
            )),
            Err(e) => KjResult::Err(format!("kj backend set: {e}")),
        }
    }

    fn backend_remove(&self, name: &str) -> KjResult {
        let db = self.kernel_db().lock();
        match db.delete_backend(name) {
            Ok(()) => KjResult::ok(format!("removed backend '{name}'")),
            Err(e) => KjResult::Err(format!("kj backend remove: {e}")),
        }
    }

    fn backend_model(&self, cmd: BackendModelCommand) -> KjResult {
        let db = self.kernel_db().lock();
        match cmd {
            BackendModelCommand::Set {
                backend,
                model,
                context_window,
                extra,
            } => {
                let b = match db.get_backend_by_name(&backend) {
                    Ok(Some(b)) => b,
                    Ok(None) => {
                        return KjResult::Err(format!(
                            "kj backend model set: backend '{backend}' not found"
                        ));
                    }
                    Err(e) => return KjResult::Err(format!("kj backend model set: {e}")),
                };
                if let Some(json) = &extra
                    && serde_json::from_str::<serde_json::Value>(json).is_err()
                {
                    return KjResult::Err(
                        "kj backend model set: --extra must be valid JSON".to_string(),
                    );
                }
                match db.set_backend_model(&BackendModelRow {
                    backend_id: b.backend_id,
                    model_id: model.clone(),
                    context_window: context_window.map(|w| w as i64),
                    extra,
                }) {
                    Ok(()) => KjResult::ok(format!(
                        "set {backend}/{model} ({})",
                        context_window
                            .map(|w| format!("{w} ctx"))
                            .unwrap_or_else(|| "context window unknown".to_string())
                    )),
                    Err(e) => KjResult::Err(format!("kj backend model set: {e}")),
                }
            }
            BackendModelCommand::Remove { backend, model } => {
                let b = match db.get_backend_by_name(&backend) {
                    Ok(Some(b)) => b,
                    Ok(None) => {
                        return KjResult::Err(format!(
                            "kj backend model remove: backend '{backend}' not found"
                        ));
                    }
                    Err(e) => return KjResult::Err(format!("kj backend model remove: {e}")),
                };
                match db.delete_backend_model(b.backend_id, &model) {
                    Ok(true) => KjResult::ok(format!("removed {backend}/{model}")),
                    Ok(false) => KjResult::Err(format!(
                        "kj backend model remove: {backend} has no model '{model}'"
                    )),
                    Err(e) => KjResult::Err(format!("kj backend model remove: {e}")),
                }
            }
        }
    }

    fn backend_default_show(&self) -> KjResult {
        let db = self.kernel_db().lock();
        let Some(d) = (match db.get_llm_defaults() {
            Ok(d) => d,
            Err(e) => return KjResult::Err(format!("kj backend default: {e}")),
        }) else {
            return KjResult::Err(
                "kj backend default: no defaults row — run `kj backend reseed`".to_string(),
            );
        };
        // A NULL knob prints as "(provider default)", not as a number we made
        // up: "unset" is a real answer here.
        let opt = |v: Option<String>| v.unwrap_or_else(|| "(provider default)".to_string());
        let lines = vec![
            format!("Default backend: {}", d.default_backend),
            format!("Default model: {}", d.default_model),
            format!("max_tokens: {}", opt(d.max_tokens.map(|v| v.to_string()))),
            format!("temperature: {}", opt(d.temperature.map(|v| v.to_string()))),
            format!("top_p: {}", opt(d.top_p.map(|v| v.to_string()))),
            format!("effort: {}", opt(d.effort.clone())),
            format!(
                "thinking_budget: {}",
                opt(d.thinking_budget.map(|v| v.to_string()))
            ),
            format!("thinking_style: {}", opt(d.thinking_style.clone())),
        ];
        let data = serde_json::json!({
            "default_backend": d.default_backend,
            "default_model": d.default_model,
            "max_tokens": d.max_tokens,
            "temperature": d.temperature,
            "top_p": d.top_p,
            "effort": d.effort,
            "thinking_budget": d.thinking_budget,
            "thinking_style": d.thinking_style,
        });
        KjResult::ok_with_data(lines.join("\n"), data)
    }

    #[allow(clippy::too_many_arguments)]
    fn backend_default_set(
        &self,
        backend: Option<String>,
        model: Option<String>,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
        top_p: Option<f64>,
        effort: Option<String>,
        thinking_budget: Option<u64>,
        thinking_style: Option<String>,
    ) -> KjResult {
        let db = self.kernel_db().lock();
        let current = match db.get_llm_defaults() {
            Ok(Some(d)) => d,
            Ok(None) => {
                // Nothing to merge onto — require both halves of the identity.
                let (Some(b), Some(m)) = (backend.clone(), model.clone()) else {
                    return KjResult::Err(
                        "kj backend default set: no defaults row yet — pass both \
                         --backend and --model (or run `kj backend reseed`)"
                            .to_string(),
                    );
                };
                LlmDefaultsRow {
                    default_backend: b,
                    default_model: m,
                    max_tokens: None,
                    temperature: None,
                    top_p: None,
                    effort: None,
                    thinking_budget: None,
                    thinking_style: None,
                }
            }
            Err(e) => return KjResult::Err(format!("kj backend default set: {e}")),
        };
        let next = LlmDefaultsRow {
            default_backend: backend.unwrap_or(current.default_backend),
            default_model: model.unwrap_or(current.default_model),
            max_tokens: max_tokens.map(|v| v as i64).or(current.max_tokens),
            temperature: temperature.or(current.temperature),
            top_p: top_p.or(current.top_p),
            effort: effort.or(current.effort),
            thinking_budget: thinking_budget
                .map(|v| v as i64)
                .or(current.thinking_budget),
            thinking_style: thinking_style.or(current.thinking_style),
        };
        match db.set_llm_defaults(&next) {
            Ok(()) => KjResult::ok(format!(
                "defaults now {}/{}",
                next.default_backend, next.default_model
            )),
            Err(e) => KjResult::Err(format!("kj backend default set: {e}")),
        }
    }

    fn backend_reseed(&self, caller: &KjCaller) -> KjResult {
        let mut db = self.kernel_db().lock();
        match crate::seed_backends::reseed_factory_backends(&mut db, caller.principal_id) {
            Ok(n) => KjResult::ok(format!(
                "restored {n} factory backend(s), their model windows, the factory \
                 aliases, and the defaults"
            )),
            Err(e) => KjResult::Err(format!("kj backend reseed: {e}")),
        }
    }

    /// Rebuild the kernel's `LlmRegistry` from the database and swap it in.
    ///
    /// This is the live-reload mechanism: the registry is a snapshot behind
    /// `Kernel::llm()`'s `RwLock`, so every config mutation ends here and the
    /// change is visible to the very next turn — no kernel restart, and no
    /// per-request SQLite hop on the async turn path. See
    /// `llm/db_config.rs` for why swap-the-snapshot over read-through.
    pub(crate) async fn reload_llm_registry(&self) -> Result<(), String> {
        let registry = {
            let db = self.kernel_db().lock();
            crate::llm::build_llm_registry(&db).map_err(|e| e.to_string())?
        };
        *self.kernel().llm().write().await = registry;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{test_caller, test_dispatcher};
    use super::*;

    fn s(v: &str) -> String {
        v.to_string()
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| s(p)).collect()
    }

    async fn seeded() -> KjDispatcher {
        let d = test_dispatcher().await;
        {
            let mut db = d.kernel_db().lock();
            crate::seed_backends::ensure_factory_backends(
                &mut db,
                kaijutsu_types::PrincipalId::system(),
            )
            .unwrap();
        }
        d.reload_llm_registry().await.unwrap();
        d
    }

    #[tokio::test]
    async fn list_data_is_an_array_of_backend_names() {
        let d = seeded().await;
        let c = test_caller();
        match d.dispatch(&argv(&["backend", "list"]), &c).await {
            KjResult::Ok { data: Some(v), .. } => {
                let names: Vec<&str> = v.as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
                assert!(names.contains(&"anthropic"), "{names:?}");
                assert!(names.contains(&"ollama"), "{names:?}");
            }
            other => panic!("expected list data: {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_rejects_an_unknown_kind_by_name() {
        let d = seeded().await;
        let c = test_caller();
        let r = d
            .dispatch(&argv(&["backend", "set", "gemini", "--kind", "gemini"]), &c)
            .await;
        match r {
            KjResult::Err(msg) => {
                assert!(msg.contains("unknown backend kind 'gemini'"), "{msg}");
                assert!(msg.contains("anthropic"), "error lists the closed set: {msg}");
            }
            other => panic!("an unknown kind must fail loud: {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_rejects_openai_kind_without_a_base_url() {
        let d = seeded().await;
        let c = test_caller();
        match d
            .dispatch(&argv(&["backend", "set", "vllm", "--kind", "openai"]), &c)
            .await
        {
            KjResult::Err(msg) => assert!(msg.contains("--base-url"), "{msg}"),
            other => panic!("openai kind needs a URL: {other:?}"),
        }
        // ...and accepts it with one.
        let r = d
            .dispatch(
                &argv(&[
                    "backend", "set", "vllm", "--kind", "openai", "--base-url",
                    "http://localhost:8000/v1", "--key-optional",
                ]),
                &c,
            )
            .await;
        assert!(matches!(r, KjResult::Ok { .. }), "{r:?}");
    }

    #[tokio::test]
    async fn a_new_backend_is_live_without_a_restart() {
        // The live-reload contract, end to end through the dispatcher.
        let d = seeded().await;
        let c = test_caller();
        assert!(d.kernel().llm().read().await.get("vllm").is_none());
        d.dispatch(
            &argv(&[
                "backend", "set", "vllm", "--kind", "openai", "--base-url",
                "http://localhost:8000/v1", "--key-optional",
            ]),
            &c,
        )
        .await;
        assert!(
            d.kernel().llm().read().await.get("vllm").is_some(),
            "the registry must see a new backend immediately"
        );
    }

    #[tokio::test]
    async fn set_confirms_registration_in_its_own_output() {
        // The success message must say the backend actually went live, not
        // just that the row was written.
        let d = seeded().await;
        let c = test_caller();
        let r = d
            .dispatch(
                &argv(&[
                    "backend", "set", "vllm", "--kind", "openai", "--base-url",
                    "http://localhost:8000/v1", "--key-optional",
                ]),
                &c,
            )
            .await;
        match r {
            KjResult::Ok { message, .. } => {
                assert!(
                    message.contains("registered and live"),
                    "success message should confirm registration: {message}"
                );
            }
            other => panic!("expected Ok: {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_warns_in_its_own_output_when_the_registry_skips_it() {
        // The `backend set` clear-on-omit sharp edge: the upsert can
        // succeed (row written) while the registry warn-skips the backend
        // at build time (unresolvable key here). Before this fix, the only
        // sign was a `tracing::warn!` in the server log — the command's own
        // output claimed unqualified success. It must now say so itself.
        let d = seeded().await;
        let c = test_caller();
        let r = d
            .dispatch(
                &argv(&[
                    "backend", "set", "anthropic-broken", "--kind", "anthropic",
                    "--api-key-env", "KJ_TEST_NONEXISTENT_KEY_VAR_XYZ",
                ]),
                &c,
            )
            .await;
        match r {
            KjResult::Ok { message, .. } => {
                assert!(
                    message.contains("WARNING") && message.contains("not registered"),
                    "success message must flag the registry skip: {message}"
                );
            }
            other => panic!("the upsert itself must still succeed: {other:?}"),
        }
        assert!(
            d.kernel().llm().read().await.get("anthropic-broken").is_none(),
            "a backend with no resolvable key must not be registered"
        );
    }

    #[tokio::test]
    async fn remove_is_refused_while_an_alias_points_at_the_backend() {
        let d = seeded().await;
        let c = test_caller();
        // The floor ships no aliases, so make one first.
        d.dispatch(
            &argv(&["alias", "set", "local", "--backend", "ollama", "--model", "gemma4:31b"]),
            &c,
        )
        .await;
        match d.dispatch(&argv(&["backend", "remove", "ollama"]), &c).await {
            KjResult::Err(msg) => {
                assert!(msg.contains("still referenced"), "{msg}");
                assert!(msg.contains("local"), "names the referent: {msg}");
            }
            other => panic!("a referenced backend must not vanish: {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unreferenced_backend_removes_cleanly() {
        let d = seeded().await;
        let c = test_caller();
        let r = d.dispatch(&argv(&["backend", "remove", "gpt"]), &c).await;
        assert!(matches!(r, KjResult::Ok { .. }), "{r:?}");
        assert!(d.kernel().llm().read().await.get("gpt").is_none());
    }

    #[tokio::test]
    async fn model_set_pins_a_window_and_the_registry_sees_it() {
        let d = seeded().await;
        let c = test_caller();
        assert_eq!(
            d.kernel().llm().read().await.context_window_for("gpt", "gpt-5.6-terra"),
            None,
            "unpinned models start unknown, never guessed"
        );
        d.dispatch(
            &argv(&[
                "backend", "model", "set", "gpt", "gpt-5.6-terra", "--context-window", "400000",
            ]),
            &c,
        )
        .await;
        assert_eq!(
            d.kernel().llm().read().await.context_window_for("gpt", "gpt-5.6-terra"),
            Some(400_000)
        );
    }

    #[tokio::test]
    async fn model_set_rejects_a_zero_window() {
        let d = seeded().await;
        let c = test_caller();
        match d
            .dispatch(
                &argv(&[
                    "backend", "model", "set", "gpt", "gpt-5.6-terra", "--context-window", "0",
                ]),
                &c,
            )
            .await
        {
            KjResult::Err(msg) => assert!(msg.contains("positive"), "{msg}"),
            other => panic!("a zero window is a typo, not a small window: {other:?}"),
        }
    }

    #[tokio::test]
    async fn default_set_range_checks_loudly_and_never_clamps() {
        let d = seeded().await;
        let c = test_caller();
        match d
            .dispatch(&argv(&["backend", "default", "set", "--temperature", "3.5"]), &c)
            .await
        {
            KjResult::Err(msg) => assert!(msg.contains("0.0..=2.0"), "{msg}"),
            other => panic!("out-of-range temperature must fail, not clamp: {other:?}"),
        }
        // The stored value is untouched by the rejected write.
        let db = d.kernel_db().lock();
        assert_eq!(db.get_llm_defaults().unwrap().unwrap().temperature, None);
    }

    #[tokio::test]
    async fn default_set_merges_rather_than_clearing() {
        let d = seeded().await;
        let c = test_caller();
        d.dispatch(&argv(&["backend", "default", "set", "--temperature", "0.7"]), &c)
            .await;
        let db = d.kernel_db().lock();
        let row = db.get_llm_defaults().unwrap().unwrap();
        assert_eq!(row.temperature, Some(0.7));
        assert_eq!(row.default_backend, "deepseek", "identity survives a knob-only set");
        assert_eq!(row.max_tokens, Some(16384));
    }

    #[tokio::test]
    async fn default_set_refuses_a_backend_that_does_not_exist() {
        let d = seeded().await;
        let c = test_caller();
        match d
            .dispatch(&argv(&["backend", "default", "set", "--backend", "nope"]), &c)
            .await
        {
            KjResult::Err(msg) => assert!(msg.contains("does not exist"), "{msg}"),
            other => panic!("a dangling default must be refused at write time: {other:?}"),
        }
    }

    #[tokio::test]
    async fn show_reports_key_source_never_key_material() {
        // SAFETY: single-threaded test.
        unsafe {
            std::env::set_var("ANTHROPIC_API_KEY", "sk-super-secret-value");
        }
        let d = seeded().await;
        let c = test_caller();
        let r = d.dispatch(&argv(&["backend", "show", "anthropic"]), &c).await;
        let text = match &r {
            KjResult::Ok { message, .. } => message.clone(),
            other => panic!("{other:?}"),
        };
        assert!(text.contains("ANTHROPIC_API_KEY"), "names the env var: {text}");
        assert!(
            !text.contains("sk-super-secret-value"),
            "the key VALUE must never be printed: {text}"
        );
        // SAFETY: single-threaded test cleanup.
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
    }
}
