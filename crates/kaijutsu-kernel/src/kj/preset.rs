//! Preset subcommands: list, show, save, remove.

use kaijutsu_types::PresetId;

use crate::kernel_db::PresetRow;

use super::parse::{extract_named_arg, parse_model_spec, parse_tool_filter_spec};
use super::{KjCaller, KjDispatcher, KjResult};

impl KjDispatcher {
    pub(crate) fn dispatch_preset(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return KjResult::Err(self.preset_help());
        }

        match argv[0].as_str() {
            "list" | "ls" => self.preset_list(),
            "show" => self.preset_show(argv),
            "save" => self.preset_save(argv, caller),
            "remove" | "rm" => self.preset_remove(argv, caller),
            "help" | "--help" | "-h" => KjResult::ok_typed(self.preset_help(), "text/markdown"),
            other => KjResult::Err(format!(
                "kj preset: unknown subcommand '{}'\n\n{}",
                other,
                self.preset_help()
            )),
        }
    }

    fn preset_help(&self) -> String {
        include_str!("../../docs/help/kj-preset.md").to_string()
    }

    fn preset_list(&self) -> KjResult {
        let db = self.kernel_db().lock().unwrap();
        match db.list_presets(self.kernel_id()) {
            Ok(presets) => {
                if presets.is_empty() {
                    return KjResult::ok("(no presets)".to_string());
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
                KjResult::ok(lines.join("\n"))
            }
            Err(e) => KjResult::Err(format!("kj preset list: {e}")),
        }
    }

    fn preset_show(&self, argv: &[String]) -> KjResult {
        let label = match argv.get(1) {
            Some(l) => l.as_str(),
            None => return KjResult::Err("kj preset show: requires a label".to_string()),
        };

        let db = self.kernel_db().lock().unwrap();
        match db.get_preset_by_label(self.kernel_id(), label) {
            Ok(Some(p)) => {
                let mut lines = vec![
                    format!("Preset: {}", p.label),
                ];
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
                if let Some(ref tf) = p.tool_filter {
                    lines.push(format!("Tools: {:?}", tf));
                }
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

    /// `kj preset save <label> [--model p/m] [--system-prompt text] [--tool-filter spec] [--consent mode] [--desc text]`
    fn preset_save(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let label = match argv.get(1) {
            Some(l) => l.as_str(),
            None => return KjResult::Err("kj preset save: requires a label".to_string()),
        };

        let model_spec = extract_named_arg(argv, &["--model", "-m"]);
        let system_prompt = extract_named_arg(argv, &["--system-prompt"]);
        let tool_filter_spec = extract_named_arg(argv, &["--tool-filter"]);
        let consent_spec = extract_named_arg(argv, &["--consent"]);
        let desc = extract_named_arg(argv, &["--desc", "--description"]);

        let (provider, model) = model_spec
            .as_deref()
            .map(parse_model_spec)
            .unwrap_or((None, None));

        let tool_filter = match tool_filter_spec {
            Some(ref spec) => match parse_tool_filter_spec(spec) {
                Ok(tf) => Some(tf),
                Err(e) => return KjResult::Err(format!("kj preset save: {e}")),
            },
            None => None,
        };

        let consent_mode = match consent_spec {
            Some(ref spec) => {
                match spec.parse::<kaijutsu_types::ConsentMode>() {
                    Ok(cm) => cm,
                    Err(_) => return KjResult::Err(format!("kj preset save: invalid consent mode '{spec}'")),
                }
            }
            None => kaijutsu_types::ConsentMode::Collaborative,
        };

        let db = self.kernel_db().lock().unwrap();
        let kernel_id = self.kernel_id();

        // Check if preset already exists → update
        match db.get_preset_by_label(kernel_id, label) {
            Ok(Some(existing)) => {
                let updated = PresetRow {
                    preset_id: existing.preset_id,
                    kernel_id,
                    label: label.to_string(),
                    description: desc.or(existing.description),
                    provider: provider.or(existing.provider),
                    model: model.or(existing.model),
                    system_prompt: system_prompt.or(existing.system_prompt),
                    tool_filter: tool_filter.or(existing.tool_filter),
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
                    kernel_id,
                    label: label.to_string(),
                    description: desc,
                    provider,
                    model,
                    system_prompt,
                    tool_filter,
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
    fn preset_remove(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let label = match argv.get(1) {
            Some(l) => l.as_str(),
            None => return KjResult::Err("kj preset remove: requires a label".to_string()),
        };

        let db = self.kernel_db().lock().unwrap();
        let kernel_id = self.kernel_id();

        let preset = match db.get_preset_by_label(kernel_id, label) {
            Ok(Some(p)) => p,
            Ok(None) => return KjResult::Err(format!("kj preset remove: '{}' not found", label)),
            Err(e) => return KjResult::Err(format!("kj preset remove: {e}")),
        };

        if !caller.confirmed {
            let usage_count = db.contexts_using_preset(kernel_id, preset.preset_id).unwrap_or(0);
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
        let ctx = register_context(&d, Some("ctx"), None, principal).await;
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(&[s("preset"), s("save"), s("fast"), s("--model"), s("anthropic/claude-haiku-4-5-20251001"), s("--desc"), s("Fast preset")], &c)
            .await;
        assert!(result.is_ok(), "save failed: {}", result.message());
        assert!(result.message().contains("created"));

        // List should show it
        let result = d.dispatch(&[s("preset"), s("list")], &c).await;
        assert!(result.is_ok());
        assert!(result.message().contains("fast"), "msg: {}", result.message());
    }

    #[tokio::test]
    async fn preset_save_update() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("ctx"), None, principal).await;
        let c = caller_with_context(ctx);

        // Create
        d.dispatch(&[s("preset"), s("save"), s("p"), s("--model"), s("a/b")], &c).await;

        // Update same label
        let result = d
            .dispatch(&[s("preset"), s("save"), s("p"), s("--model"), s("c/d")], &c)
            .await;
        assert!(result.is_ok(), "update failed: {}", result.message());
        assert!(result.message().contains("updated"));
    }

    #[tokio::test]
    async fn preset_remove_requires_latch() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("ctx"), None, principal).await;
        let c = caller_with_context(ctx);

        d.dispatch(&[s("preset"), s("save"), s("doomed"), s("--model"), s("a/b")], &c).await;

        let result = d.dispatch(&[s("preset"), s("remove"), s("doomed")], &c).await;
        assert!(result.is_latch(), "expected latch, got: {:?}", result);
    }

    #[tokio::test]
    async fn preset_remove_confirmed() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("ctx"), None, principal).await;
        let c = caller_with_context(ctx);

        d.dispatch(&[s("preset"), s("save"), s("doomed"), s("--model"), s("a/b")], &c).await;

        let c = confirmed_caller(ctx);
        let result = d.dispatch(&[s("preset"), s("remove"), s("doomed")], &c).await;
        assert!(result.is_ok(), "remove failed: {}", result.message());
        assert!(result.message().contains("deleted"));

        // Verify gone
        let result = d.dispatch(&[s("preset"), s("show"), s("doomed")], &c).await;
        assert!(!result.is_ok());
    }
}
