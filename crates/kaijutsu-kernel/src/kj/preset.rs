//! Preset subcommands: list, show (read-only).

use super::{KjCaller, KjDispatcher, KjResult};

impl KjDispatcher {
    pub(crate) fn dispatch_preset(&self, argv: &[String], _caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return KjResult::Err(self.preset_help());
        }

        match argv[0].as_str() {
            "list" | "ls" => self.preset_list(),
            "show" => self.preset_show(argv),
            "help" | "--help" | "-h" => KjResult::Ok(self.preset_help()),
            other => KjResult::Err(format!(
                "kj preset: unknown subcommand '{}'\n\n{}",
                other,
                self.preset_help()
            )),
        }
    }

    fn preset_help(&self) -> String {
        "\
kj preset — preset templates

USAGE:
    kj preset <subcommand> [args...]

SUBCOMMANDS:
    list            List all presets
    show <label>    Show preset details"
            .to_string()
    }

    fn preset_list(&self) -> KjResult {
        let db = self.kernel_db().lock().unwrap();
        match db.list_presets(self.kernel_id()) {
            Ok(presets) => {
                if presets.is_empty() {
                    return KjResult::Ok("(no presets)".to_string());
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
                KjResult::Ok(lines.join("\n"))
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
                KjResult::Ok(lines.join("\n"))
            }
            Ok(None) => KjResult::Err(format!("kj preset show: '{}' not found", label)),
            Err(e) => KjResult::Err(format!("kj preset show: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::kj::test_helpers::*;

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[tokio::test]
    async fn preset_list_empty() {
        let d = test_dispatcher();
        let c = test_caller();
        let result = d.dispatch(&[s("preset"), s("list")], &c).await;
        assert!(result.is_ok());
        assert_eq!(result.message(), "(no presets)");
    }

    #[tokio::test]
    async fn preset_show_not_found() {
        let d = test_dispatcher();
        let c = test_caller();
        let result = d
            .dispatch(&[s("preset"), s("show"), s("nonexistent")], &c)
            .await;
        assert!(!result.is_ok());
        assert!(result.message().contains("not found"));
    }
}
