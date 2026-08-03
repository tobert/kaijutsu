//! `kj alias` — the short `--model` handles.
//!
//! An alias — `opus`, `fast`, whatever you find useful — resolves to a
//! concrete `(backend, model)` pair. This replaces the demolished
//! `models.toml` `[model_aliases]` table with real rows: the `backend_id` FK
//! means a backend can no longer be deleted out from under an alias, and the
//! `COLLATE NOCASE` primary key means `opus` and `OPUS` are the same handle
//! rather than two silently divergent ones.
//!
//! The kernel ships **no aliases**. The old shipped set (`fast`/`smart`/
//! `best`/`ds-*`/`local`/…) encoded guesses about which model deserves which
//! adjective, and they went stale — `default` pointed at a retired model id
//! for a while. Aliases are operator/agent data now, like casts.
//!
//! `kj context set --model <alias>` and `kj fork --model <alias>` resolve
//! through `LlmRegistry::resolve_alias`, which reads the snapshot this verb
//! rebuilds — so an alias change is live on the next turn.

use clap::{Parser, Subcommand};
use kaijutsu_types::ContentType;

use crate::kernel_db::ModelAliasRow;

use super::{KjCaller, KjDispatcher, KjResult, clap_help_for};

#[derive(Parser, Debug)]
#[command(
    name = "alias",
    about = "Model aliases: short --model handles → backend/model",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct AliasArgs {
    #[command(subcommand)]
    command: AliasCommand,
}

#[derive(Subcommand, Debug)]
enum AliasCommand {
    /// List aliases.
    #[command(alias = "ls")]
    List,
    /// Create or re-point an alias.
    Set {
        /// Alias name (case-insensitive)
        alias: String,
        /// Backend name
        #[arg(long)]
        backend: String,
        /// Model id
        #[arg(long)]
        model: String,
    },
    /// Remove an alias.
    #[command(alias = "rm")]
    Remove {
        /// Alias name
        alias: String,
    },
}

impl KjDispatcher {
    pub(crate) async fn dispatch_alias(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<AliasArgs>();
        }
        let parsed = match AliasArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj alias: {e}"));
            }
        };

        let mutating = !matches!(parsed.command, AliasCommand::List);
        if mutating
            && let Err(denied) =
                self.require_cap(caller, crate::mcp::Capability::ConfigWrite, "alias")
        {
            return denied;
        }

        let result = match parsed.command {
            AliasCommand::List => self.alias_list(),
            AliasCommand::Set {
                alias,
                backend,
                model,
            } => self.alias_set(&alias, &backend, &model),
            AliasCommand::Remove { alias } => self.alias_remove(&alias),
        };

        if mutating
            && matches!(result, KjResult::Ok { .. })
            && let Err(e) = self.reload_llm_registry().await
        {
            return KjResult::Err(format!(
                "kj alias: write landed but the LLM registry failed to reload: {e}"
            ));
        }
        result
    }

    fn alias_list(&self) -> KjResult {
        let db = self.kernel_db().lock();
        let aliases = match db.list_model_aliases() {
            Ok(a) => a,
            Err(e) => return KjResult::Err(format!("kj alias list: {e}")),
        };
        let backends: std::collections::HashMap<_, _> = match db.list_backends() {
            Ok(b) => b.into_iter().map(|b| (b.backend_id, b.name)).collect(),
            Err(e) => return KjResult::Err(format!("kj alias list: {e}")),
        };
        // An alias NAME is its full handle — it is the PK and exactly what you
        // pass to `--model`.
        let names = serde_json::Value::Array(
            aliases
                .iter()
                .map(|a| serde_json::Value::String(a.alias.clone()))
                .collect(),
        );
        if aliases.is_empty() {
            return KjResult::ok_with_data("(no aliases)".to_string(), names);
        }
        let lines: Vec<String> = aliases
            .iter()
            .map(|a| {
                let backend = backends
                    .get(&a.backend_id)
                    .cloned()
                    .unwrap_or_else(|| "(missing backend)".to_string());
                format!("  {:<16} {backend}/{}", a.alias, a.model)
            })
            .collect();
        KjResult::ok_with_data(lines.join("\n"), names)
    }

    fn alias_set(&self, alias: &str, backend: &str, model: &str) -> KjResult {
        let db = self.kernel_db().lock();
        let b = match db.get_backend_by_name(backend) {
            Ok(Some(b)) => b,
            Ok(None) => {
                return KjResult::Err(format!("kj alias set: backend '{backend}' not found"));
            }
            Err(e) => return KjResult::Err(format!("kj alias set: {e}")),
        };
        match db.set_model_alias(&ModelAliasRow {
            alias: alias.to_string(),
            backend_id: b.backend_id,
            model: model.to_string(),
        }) {
            Ok(()) => KjResult::ok(format!("set alias '{alias}' → {backend}/{model}")),
            Err(e) => KjResult::Err(format!("kj alias set: {e}")),
        }
    }

    fn alias_remove(&self, alias: &str) -> KjResult {
        let db = self.kernel_db().lock();
        match db.delete_model_alias(alias) {
            Ok(true) => KjResult::ok(format!("removed alias '{alias}'")),
            Ok(false) => KjResult::Err(format!("kj alias remove: '{alias}' not found")),
            Err(e) => KjResult::Err(format!("kj alias remove: {e}")),
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
    async fn the_kernel_ships_no_aliases() {
        let d = seeded().await;
        let c = test_caller();
        match d.dispatch(&argv(&["alias", "list"]), &c).await {
            KjResult::Ok { data: Some(v), .. } => {
                assert!(
                    v.as_array().unwrap().is_empty(),
                    "aliases are operator data — the floor ships none: {v}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn list_data_is_an_array_of_alias_names() {
        let d = seeded().await;
        let c = test_caller();
        d.dispatch(
            &argv(&["alias", "set", "opus", "--backend", "anthropic", "--model", "claude-opus-5"]),
            &c,
        )
        .await;
        match d.dispatch(&argv(&["alias", "list"]), &c).await {
            KjResult::Ok { data: Some(v), .. } => {
                let names: Vec<&str> =
                    v.as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
                assert_eq!(names, vec!["opus"]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn set_repoints_an_alias_live() {
        let d = seeded().await;
        let c = test_caller();
        d.dispatch(
            &argv(&[
                "alias", "set", "smart", "--backend", "anthropic", "--model", "claude-opus-4-8",
            ]),
            &c,
        )
        .await;
        assert_eq!(
            d.kernel().llm().read().await.resolve_alias("smart"),
            Some(("anthropic", "claude-opus-4-8"))
        );
        d.dispatch(
            &argv(&[
                "alias", "set", "smart", "--backend", "deepseek", "--model", "deepseek-v4-pro",
            ]),
            &c,
        )
        .await;
        assert_eq!(
            d.kernel().llm().read().await.resolve_alias("smart"),
            Some(("deepseek", "deepseek-v4-pro")),
            "an alias change must be live without a kernel restart"
        );
    }

    #[tokio::test]
    async fn set_refuses_an_unknown_backend() {
        let d = seeded().await;
        let c = test_caller();
        match d
            .dispatch(&argv(&["alias", "set", "x", "--backend", "nope", "--model", "m"]), &c)
            .await
        {
            KjResult::Err(msg) => assert!(msg.contains("backend 'nope' not found"), "{msg}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn remove_drops_it_from_the_registry() {
        let d = seeded().await;
        let c = test_caller();
        d.dispatch(
            &argv(&["alias", "set", "opus", "--backend", "anthropic", "--model", "claude-opus-5"]),
            &c,
        )
        .await;
        assert!(d.kernel().llm().read().await.resolve_alias("opus").is_some());
        d.dispatch(&argv(&["alias", "remove", "opus"]), &c).await;
        assert!(d.kernel().llm().read().await.resolve_alias("opus").is_none());
    }

    #[tokio::test]
    async fn removing_a_missing_alias_is_an_error_not_a_silent_noop() {
        let d = seeded().await;
        let c = test_caller();
        match d.dispatch(&argv(&["alias", "remove", "nonesuch"]), &c).await {
            KjResult::Err(msg) => assert!(msg.contains("not found"), "{msg}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn aliases_are_case_insensitive_handles() {
        let d = seeded().await;
        let c = test_caller();
        d.dispatch(
            &argv(&["alias", "set", "opus", "--backend", "anthropic", "--model", "claude-opus-5"]),
            &c,
        )
        .await;
        // Setting "OPUS" re-points the existing "opus" row rather than
        // creating a second, divergent handle.
        d.dispatch(
            &argv(&["alias", "set", "OPUS", "--backend", "ollama", "--model", "gemma4:31b"]),
            &c,
        )
        .await;
        let db = d.kernel_db().lock();
        let matching: Vec<_> = db
            .list_model_aliases()
            .unwrap()
            .into_iter()
            .filter(|a| a.alias.eq_ignore_ascii_case("opus"))
            .collect();
        assert_eq!(matching.len(), 1, "one handle, not two: {matching:?}");
    }
}
