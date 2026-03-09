//! Text table/tree formatting helpers for kj command output.

use kaijutsu_types::ContextId;

use crate::kernel_db::ContextRow;

/// Format a context list as a flat table.
///
/// Marks the current context with `*`.
pub fn format_context_table(contexts: &[ContextRow], current: ContextId) -> String {
    if contexts.is_empty() {
        return "(no contexts)".to_string();
    }

    let mut lines = Vec::new();

    for ctx in contexts {
        let marker = if ctx.context_id == current { "*" } else { " " };
        let label = ctx.label.as_deref().unwrap_or("-");
        let model = format_model(&ctx.provider, &ctx.model);
        let id_short = ctx.context_id.short();
        lines.push(format!("{marker} {id_short}  {label:<16} {model}"));
    }

    lines.join("\n")
}

/// Format context DAG results as an indented tree.
///
/// `dag` is a list of (ContextRow, depth) from the recursive CTE.
pub fn format_context_tree(dag: &[(ContextRow, i64)], current: ContextId) -> String {
    if dag.is_empty() {
        return "(no contexts)".to_string();
    }

    let mut lines = Vec::new();

    for (ctx, depth) in dag {
        let indent = "  ".repeat(*depth as usize);
        let marker = if ctx.context_id == current { "*" } else { " " };
        let label = ctx.label.as_deref().unwrap_or("-");
        let model = format_model(&ctx.provider, &ctx.model);
        let id_short = ctx.context_id.short();

        let prefix = if *depth > 0 { "└─ " } else { "" };
        lines.push(format!("{marker} {indent}{prefix}{id_short}  {label:<16} {model}"));
    }

    lines.join("\n")
}

/// Format a single context's info for `kj context info`.
pub fn format_context_info(
    ctx: &ContextRow,
    children_count: usize,
    drift_edge_count: usize,
    is_current: bool,
) -> String {
    let mut lines = Vec::new();

    let label_display = ctx.label.as_deref().unwrap_or("(none)");
    if is_current {
        lines.push(format!("Context: {} *", label_display));
    } else {
        lines.push(format!("Context: {}", label_display));
    }
    lines.push(format!("ID:      {}", ctx.context_id.short()));
    lines.push(format!("Model:   {}", format_model(&ctx.provider, &ctx.model)));

    if let Some(forked_from) = ctx.forked_from {
        let kind = ctx
            .fork_kind
            .as_ref()
            .map(|k| format!("{k:?}"))
            .unwrap_or_default();
        lines.push(format!("Fork:    {} ({})", forked_from.short(), kind));
    }

    lines.push(format!("Created: {}", format_timestamp(ctx.created_at)));
    lines.push(format!("By:      {}", ctx.created_by.short()));

    if children_count > 0 {
        lines.push(format!("Children: {}", children_count));
    }
    if drift_edge_count > 0 {
        lines.push(format!("Drifts:  {}", drift_edge_count));
    }

    lines.join("\n")
}

/// Format provider/model as a display string.
fn format_model(provider: &Option<String>, model: &Option<String>) -> String {
    match (provider.as_deref(), model.as_deref()) {
        (Some(p), Some(m)) => format!("{p}/{m}"),
        (None, Some(m)) => m.to_string(),
        (Some(p), None) => format!("{p}/(default)"),
        (None, None) => "(no model)".to_string(),
    }
}

/// Format a Unix-millis timestamp for display.
fn format_timestamp(millis: i64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let secs = (millis / 1000) as u64;
    let dt = UNIX_EPOCH + Duration::from_secs(secs);
    // Use a simple format since we don't want to pull in chrono
    let elapsed = std::time::SystemTime::now()
        .duration_since(dt)
        .unwrap_or_default();
    if elapsed.as_secs() < 60 {
        "just now".to_string()
    } else if elapsed.as_secs() < 3600 {
        format!("{}m ago", elapsed.as_secs() / 60)
    } else if elapsed.as_secs() < 86400 {
        format!("{}h ago", elapsed.as_secs() / 3600)
    } else {
        format!("{}d ago", elapsed.as_secs() / 86400)
    }
}

/// Format staged drift items for `kj drift queue`.
pub fn format_drift_queue(items: &[crate::drift::StagedDrift]) -> String {
    if items.is_empty() {
        return "(queue empty)".to_string();
    }

    let mut lines = Vec::new();
    for item in items {
        let preview = if item.content.len() > 60 {
            format!("{}...", &item.content[..57])
        } else {
            item.content.clone()
        };
        lines.push(format!(
            "#{:<3} {} → {}  {:?}  {}",
            item.id,
            item.source_ctx.short(),
            item.target_ctx.short(),
            item.drift_kind,
            preview,
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{ConsentMode, PrincipalId};

    fn make_row(label: Option<&str>, id: ContextId) -> ContextRow {
        ContextRow {
            context_id: id,
            kernel_id: kaijutsu_types::KernelId::new(),
            label: label.map(|s| s.to_string()),
            provider: Some("anthropic".to_string()),
            model: Some("claude-opus-4-6".to_string()),
            system_prompt: None,
            tool_filter: None,
            consent_mode: ConsentMode::Collaborative,
            created_at: kaijutsu_types::now_millis() as i64,
            created_by: PrincipalId::new(),
            forked_from: None,
            fork_kind: None,
            archived_at: None,
            workspace_id: None,
            preset_id: None,
        }
    }

    #[test]
    fn table_marks_current() {
        let current = ContextId::new();
        let other = ContextId::new();
        let rows = vec![make_row(Some("default"), current), make_row(Some("alt"), other)];

        let output = format_context_table(&rows, current);
        assert!(output.contains("* "));
        assert!(output.contains("default"));
        assert!(output.contains("alt"));
    }

    #[test]
    fn table_empty() {
        let output = format_context_table(&[], ContextId::new());
        assert_eq!(output, "(no contexts)");
    }

    #[test]
    fn info_shows_fields() {
        let id = ContextId::new();
        let row = make_row(Some("test-ctx"), id);
        let output = format_context_info(&row, 2, 1, true);
        assert!(output.contains("test-ctx *"));
        assert!(output.contains("anthropic/claude-opus-4-6"));
        assert!(output.contains("Children: 2"));
        assert!(output.contains("Drifts:  1"));
    }

    #[test]
    fn model_format_variants() {
        assert_eq!(
            format_model(&Some("a".into()), &Some("b".into())),
            "a/b"
        );
        assert_eq!(format_model(&None, &Some("b".into())), "b");
        assert_eq!(format_model(&None, &None), "(no model)");
    }
}
