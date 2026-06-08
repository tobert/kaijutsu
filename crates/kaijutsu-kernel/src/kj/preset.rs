//! Preset subcommands: list, show, save, remove.

use clap::{Parser, Subcommand};
use kaijutsu_types::{ContentType, PresetId};

use crate::kernel_db::PresetRow;

use super::parse::parse_model_spec;
use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "preset",
    about = "Manage model/consent presets",
    disable_help_subcommand = true,
    no_binary_name = true
)]
struct PresetArgs {
    #[command(subcommand)]
    command: PresetCommand,
}

#[derive(Subcommand, Debug)]
enum PresetCommand {
    /// List all presets.
    #[command(alias = "ls")]
    List,
    /// Show details for a preset.
    Show {
        /// Preset label to inspect
        label: String,
    },
    /// Create or update a preset.
    Save {
        /// Preset label (resolver key)
        label: String,
        /// Model spec `provider/model` (or bare model)
        #[arg(long, short = 'm')]
        model: Option<String>,
        /// System prompt text
        #[arg(long = "system-prompt")]
        system_prompt: Option<String>,
        /// Consent mode (e.g. collaborative, autonomous)
        #[arg(long)]
        consent: Option<String>,
        /// Description text
        #[arg(long, alias = "description")]
        desc: Option<String>,
    },
    /// Remove a preset (latched).
    #[command(alias = "rm")]
    Remove {
        /// Preset label to delete
        label: String,
    },
}

impl KjDispatcher {
    pub(crate) fn dispatch_preset(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<PresetArgs>();
        }
        let parsed = match PresetArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj preset: {e}"));
            }
        };

        // Preset mutation is operator authority; list/show stay ungated.
        if matches!(
            parsed.command,
            PresetCommand::Save { .. } | PresetCommand::Remove { .. }
        ) && let Err(denied) =
            self.require_cap(caller, crate::mcp::Capability::Operator, "preset")
        {
            return denied;
        }

        match parsed.command {
            PresetCommand::List => self.preset_list(),
            PresetCommand::Show { label } => self.preset_show(&label),
            PresetCommand::Save {
                label,
                model,
                system_prompt,
                consent,
                desc,
            } => self.preset_save(&label, model, system_prompt, consent, desc, caller),
            PresetCommand::Remove { label } => self.preset_remove(&label, caller),
        }
    }

    fn preset_list(&self) -> KjResult {
        let db = self.kernel_db().lock();
        match db.list_presets() {
            Ok(presets) => {
                // Iteration handles: preset labels are the resolver key
                // (`get_preset_by_label`) and they're required (non-nullable
                // in the schema), so labels are the canonical full handle —
                // no truncation occurs here.
                let labels = serde_json::Value::Array(
                    presets
                        .iter()
                        .map(|p| serde_json::Value::String(p.label.clone()))
                        .collect(),
                );
                if presets.is_empty() {
                    return KjResult::ok_with_data("(no presets)".to_string(), labels);
                }
                let lines: Vec<String> = presets
                    .iter()
                    .map(|p| {
                        let model = match (&p.provider, &p.model) {
                            (Some(prov), Some(m)) => format!("{prov}/{m}"),
                            (None, Some(m)) => m.clone(),
                            _ => "(no model)".to_string(),
                        };
                        let desc = p
                            .description
                            .as_deref()
                            .map(|d| format!("  — {d}"))
                            .unwrap_or_default();
                        format!("  {:<20} {}{}", p.label, model, desc)
                    })
                    .collect();
                KjResult::ok_with_data(lines.join("\n"), labels)
            }
            Err(e) => KjResult::Err(format!("kj preset list: {e}")),
        }
    }

    fn preset_show(&self, label: &str) -> KjResult {
        let db = self.kernel_db().lock();
        match db.get_preset_by_label(label) {
            Ok(Some(p)) => {
                let mut lines = vec![format!("Preset: {}", p.label)];
                if let Some(desc) = &p.description {
                    lines.push(format!("Description: {desc}"));
                }
                let model = match (&p.provider, &p.model) {
                    (Some(prov), Some(m)) => format!("{prov}/{m}"),
                    (None, Some(m)) => m.clone(),
                    _ => "(no model)".to_string(),
                };
                lines.push(format!("Model: {model}"));
                lines.push(format!("Consent: {:?}", p.consent_mode));
                if let Some(ref sp) = p.system_prompt {
                    let preview = if sp.len() > 80 {
                        format!("{}...", &sp[..77])
                    } else {
                        sp.clone()
                    };
                    lines.push(format!("System: {preview}"));
                }
                KjResult::ok(lines.join("\n"))
            }
            Ok(None) => KjResult::Err(format!("kj preset show: '{}' not found", label)),
            Err(e) => KjResult::Err(format!("kj preset show: {e}")),
        }
    }

    /// `kj preset save <label> [--model p/m] [--system-prompt text] [--consent mode] [--desc text]`
    fn preset_save(
        &self,
        label: &str,
        model_spec: Option<String>,
        system_prompt: Option<String>,
        consent_spec: Option<String>,
        desc: Option<String>,
        caller: &KjCaller,
    ) -> KjResult {
        let (provider, model) = model_spec
            .as_deref()
            .map(parse_model_spec)
            .unwrap_or((None, None));

        let consent_mode = match consent_spec {
            Some(ref spec) => match spec.parse::<kaijutsu_types::ConsentMode>() {
                Ok(cm) => cm,
                Err(_) => {
                    return KjResult::Err(format!("kj preset save: invalid consent mode '{spec}'"));
                }
            },
            None => kaijutsu_types::ConsentMode::Collaborative,
        };

        let db = self.kernel_db().lock();

        // Check if preset already exists → update
        match db.get_preset_by_label(label) {
            Ok(Some(existing)) => {
                let updated = PresetRow {
                    preset_id: existing.preset_id,
                                        label: label.to_string(),
                    description: desc.or(existing.description),
                    provider: provider.or(existing.provider),
                    model: model.or(existing.model),
                    system_prompt: system_prompt.or(existing.system_prompt),
                    consent_mode,
                    created_at: existing.created_at,
                    created_by: existing.created_by,
                };
                match db.update_preset(&updated) {
                    Ok(()) => KjResult::ok(format!("updated preset '{}'", label)),
                    Err(e) => KjResult::Err(format!("kj preset save: {e}")),
                }
            }
            Ok(None) => {
                let row = PresetRow {
                    preset_id: PresetId::new(),
                                        label: label.to_string(),
                    description: desc,
                    provider,
                    model,
                    system_prompt,
                    consent_mode,
                    created_at: kaijutsu_types::now_millis() as i64,
                    created_by: caller.principal_id,
                };
                match db.insert_preset(&row) {
                    Ok(()) => KjResult::ok(format!("created preset '{}'", label)),
                    Err(e) => KjResult::Err(format!("kj preset save: {e}")),
                }
            }
            Err(e) => KjResult::Err(format!("kj preset save: {e}")),
        }
    }

    /// `kj preset remove <label>` — delete a preset (latched).
    fn preset_remove(&self, label: &str, caller: &KjCaller) -> KjResult {
        let db = self.kernel_db().lock();

        let preset = match db.get_preset_by_label(label) {
            Ok(Some(p)) => p,
            Ok(None) => return KjResult::Err(format!("kj preset remove: '{}' not found", label)),
            Err(e) => return KjResult::Err(format!("kj preset remove: {e}")),
        };

        if !caller.confirmed {
            let usage_count = db
                .contexts_using_preset(preset.preset_id)
                .unwrap_or(0);
            return KjResult::Latch {
                command: "kj preset remove".to_string(),
                target: label.to_string(),
                message: format!("{} context(s) using this preset", usage_count),
            };
        }

        match db.delete_preset(preset.preset_id) {
            Ok(true) => KjResult::ok(format!("deleted preset '{}'", label)),
            Ok(false) => KjResult::Err(format!("kj preset remove: '{}' not found", label)),
            Err(e) => KjResult::Err(format!("kj preset remove: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::kj::test_helpers::*;
    use kaijutsu_types::PrincipalId;

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[tokio::test]
    async fn preset_list_empty() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d.dispatch(&[s("preset"), s("list")], &c).await;
        assert!(result.is_ok());
        assert_eq!(result.message(), "(no presets)");
    }

    /// `kj preset list` populates `.data` with the labels (the resolver
    /// key) so kaish for-loops iterate by label.
    #[tokio::test]
    async fn preset_list_emits_label_array() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("ctx"), None, principal);
        let c = caller_with_context(ctx);

        d.dispatch(
            &[s("preset"), s("save"), s("fast"), s("--model"), s("a/b")],
            &c,
        )
        .await;
        d.dispatch(
            &[s("preset"), s("save"), s("slow"), s("--model"), s("c/d")],
            &c,
        )
        .await;

        let result = d.dispatch(&[s("preset"), s("list")], &c).await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                let labels: Vec<&str> = v
                    .as_array()
                    .expect("array")
                    .iter()
                    .filter_map(|x| x.as_str())
                    .collect();
                assert!(labels.contains(&"fast"), "got: {labels:?}");
                assert!(labels.contains(&"slow"), "got: {labels:?}");
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn preset_show_not_found() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d
            .dispatch(&[s("preset"), s("show"), s("nonexistent")], &c)
            .await;
        assert!(!result.is_ok());
        assert!(result.message().contains("not found"));
    }

    #[tokio::test]
    async fn preset_save_and_list() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("ctx"), None, principal);
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(
                &[
                    s("preset"),
                    s("save"),
                    s("fast"),
                    s("--model"),
                    s("anthropic/claude-haiku-4-5-20251001"),
                    s("--desc"),
                    s("Fast preset"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "save failed: {}", result.message());
        assert!(result.message().contains("created"));

        // List should show it
        let result = d.dispatch(&[s("preset"), s("list")], &c).await;
        assert!(result.is_ok());
        assert!(
            result.message().contains("fast"),
            "msg: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn preset_save_update() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("ctx"), None, principal);
        let c = caller_with_context(ctx);

        // Create
        d.dispatch(
            &[s("preset"), s("save"), s("p"), s("--model"), s("a/b")],
            &c,
        )
        .await;

        // Update same label
        let result = d
            .dispatch(
                &[s("preset"), s("save"), s("p"), s("--model"), s("c/d")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "update failed: {}", result.message());
        assert!(result.message().contains("updated"));
    }

    #[tokio::test]
    async fn preset_remove_requires_latch() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("ctx"), None, principal);
        let c = caller_with_context(ctx);

        d.dispatch(
            &[s("preset"), s("save"), s("doomed"), s("--model"), s("a/b")],
            &c,
        )
        .await;

        let result = d
            .dispatch(&[s("preset"), s("remove"), s("doomed")], &c)
            .await;
        assert!(result.is_latch(), "expected latch, got: {:?}", result);
    }

    #[tokio::test]
    async fn preset_remove_confirmed() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("ctx"), None, principal);
        let c = caller_with_context(ctx);

        d.dispatch(
            &[s("preset"), s("save"), s("doomed"), s("--model"), s("a/b")],
            &c,
        )
        .await;

        let c = confirmed_caller(ctx);
        let result = d
            .dispatch(&[s("preset"), s("remove"), s("doomed")], &c)
            .await;
        assert!(result.is_ok(), "remove failed: {}", result.message());
        assert!(result.message().contains("deleted"));

        // Verify gone
        let result = d.dispatch(&[s("preset"), s("show"), s("doomed")], &c).await;
        assert!(!result.is_ok());
    }
}
