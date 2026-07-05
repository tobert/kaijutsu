//! Text table/tree formatting helpers for kj command output.

use kaijutsu_types::ContextId;

use crate::kernel_db::ContextRow;

/// Format a context list as a flat table.
///
/// Marks the current context with `*` and ring-0 (promoted) contexts with a
/// trailing `[ring0]` tag.
pub fn format_context_table(contexts: &[ContextRow], current: Option<ContextId>) -> String {
    if contexts.is_empty() {
        return "(no contexts)".to_string();
    }

    let mut lines = Vec::new();

    for ctx in contexts {
        let marker = if Some(ctx.context_id) == current {
            "*"
        } else {
            " "
        };
        let label = ctx.label.as_deref().unwrap_or("-");
        let model = format_model(&ctx.provider, &ctx.model);
        let id_short = ctx.context_id.short();
        let ring0 = if ctx.promoted_at.is_some() {
            " [ring0]"
        } else {
            ""
        };
        lines.push(format!("{marker} {id_short}  {label:<16} {model}{ring0}"));
    }

    lines.join("\n")
}

/// Format context DAG results as an indented tree.
///
/// `dag` is a list of (ContextRow, depth) from the recursive CTE.
pub fn format_context_tree(dag: &[(ContextRow, i64)], current: Option<ContextId>) -> String {
    if dag.is_empty() {
        return "(no contexts)".to_string();
    }

    let mut lines = Vec::new();

    for (ctx, depth) in dag {
        let indent = "  ".repeat(*depth as usize);
        let marker = if Some(ctx.context_id) == current {
            "*"
        } else {
            " "
        };
        let label = ctx.label.as_deref().unwrap_or("-");
        let model = format_model(&ctx.provider, &ctx.model);
        let id_short = ctx.context_id.short();

        let prefix = if *depth > 0 { "└─ " } else { "" };
        lines.push(format!(
            "{marker} {indent}{prefix}{id_short}  {label:<16} {model}"
        ));
    }

    lines.join("\n")
}

/// Render a 16-byte OTel trace id as 32 lowercase hex chars (no dashes) —
/// the canonical W3C `trace-id` text form a trace viewer expects.
pub(crate) fn hex32(bytes: [u8; 16]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Format a single context's info for `kj context info`.
///
/// `trace_id` is the context's long-running OTel trace id (from the drift
/// router handle); `None` when the context isn't registered in the router.
pub fn format_context_info(
    ctx: &ContextRow,
    children_count: usize,
    drift_edge_count: usize,
    is_current: bool,
    trace_id: Option<[u8; 16]>,
) -> String {
    let mut lines = Vec::new();

    let label_display = ctx.label.as_deref().unwrap_or("(none)");
    if is_current {
        lines.push(format!("Context: {} *", label_display));
    } else {
        lines.push(format!("Context: {}", label_display));
    }
    lines.push(format!("ID:      {}", ctx.context_id.short()));
    lines.push(format!(
        "Model:   {}",
        format_model(&ctx.provider, &ctx.model)
    ));
    lines.push(format!("Type:    {}", ctx.context_type));
    if let Some(tid) = trace_id {
        lines.push(format!("Trace:   {}", hex32(tid)));
    }

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

    // Time-well placement stamps — omitted entirely when unset, so the
    // common (auto-placed, unpaused) context shows nothing extra.
    if let Some(ts) = ctx.promoted_at {
        lines.push(format!("Promoted: {}", format_timestamp(ts)));
    }
    if let Some(ts) = ctx.demoted_at {
        lines.push(format!("Demoted: {}", format_timestamp(ts)));
    }
    if let Some(ts) = ctx.paused_at {
        lines.push(format!("Paused:  {}", format_timestamp(ts)));
    }

    if children_count > 0 {
        lines.push(format!("Children: {}", children_count));
    }
    if drift_edge_count > 0 {
        lines.push(format!("Drifts:  {}", drift_edge_count));
    }

    lines.join("\n")
}

/// Format provider/model as a display string.
pub(crate) fn format_model(provider: &Option<String>, model: &Option<String>) -> String {
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

/// Format fork lineage chain for `kj context log`.
///
/// Shows the chain from the starting context up to the root, with depth markers.
pub fn format_fork_lineage(lineage: &[(ContextRow, i64)], current: Option<ContextId>) -> String {
    if lineage.is_empty() {
        return "(no lineage)".to_string();
    }

    let mut lines = Vec::new();
    for (ctx, depth) in lineage {
        let marker = if Some(ctx.context_id) == current { "*" } else { " " };
        let label = ctx.label.as_deref().unwrap_or("-");
        let model = format_model(&ctx.provider, &ctx.model);
        let id_short = ctx.context_id.short();

        let depth_indicator = if *depth == 0 {
            "".to_string()
        } else {
            format!("{} ← ", "·".repeat(*depth as usize))
        };

        let kind = ctx
            .fork_kind
            .as_ref()
            .map(|k| format!(" ({k:?})"))
            .unwrap_or_default();

        lines.push(format!(
            "{marker} {depth_indicator}{id_short}  {label:<16} {model}{kind}"
        ));
    }
    lines.join("\n")
}

/// Format drift history for `kj drift history`.
pub fn format_drift_history(
    outgoing: &[crate::kernel_db::ContextEdgeRow],
    incoming: &[crate::kernel_db::ContextEdgeRow],
    db: &crate::kernel_db::KernelDb,
) -> String {
    let mut lines = Vec::new();

    if !outgoing.is_empty() {
        lines.push("Sent:".to_string());
        for edge in outgoing {
            let target_label = db
                .get_context(edge.target_id)
                .ok()
                .flatten()
                .and_then(|r| r.label)
                .unwrap_or_else(|| edge.target_id.short());
            lines.push(format!(
                "  → {}  {}",
                target_label,
                format_timestamp(edge.created_at)
            ));
        }
    }

    if !incoming.is_empty() {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push("Received:".to_string());
        for edge in incoming {
            let source_label = db
                .get_context(edge.source_id)
                .ok()
                .flatten()
                .and_then(|r| r.label)
                .unwrap_or_else(|| edge.source_id.short());
            lines.push(format!(
                "  ← {}  {}",
                source_label,
                format_timestamp(edge.created_at)
            ));
        }
    }

    if lines.is_empty() {
        "(no drift history)".to_string()
    } else {
        lines.join("\n")
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
    use kaijutsu_types::{ConsentMode, ContextState, PrincipalId};

    fn make_row(label: Option<&str>, id: ContextId) -> ContextRow {
        ContextRow {
            context_id: id,
                        label: label.map(|s| s.to_string()),
            provider: Some("anthropic".to_string()),
            model: Some("claude-opus-4-6".to_string()),
            system_prompt: None,
            consent_mode: ConsentMode::Collaborative,
            context_state: ContextState::Live,
            context_type: "default".to_string(),
            created_at: kaijutsu_types::now_millis() as i64,
            created_by: PrincipalId::new(),
            forked_from: None,
            fork_kind: None,
            archived_at: None,
            workspace_id: None,
            preset_id: None,
            concluded_at: None,
            last_activity_at: None,
            promoted_at: None,
            demoted_at: None,
            paused_at: None,
        }
    }

    #[test]
    fn table_marks_current() {
        let current = ContextId::new();
        let other = ContextId::new();
        let rows = vec![
            make_row(Some("default"), current),
            make_row(Some("alt"), other),
        ];

        let output = format_context_table(&rows, Some(current));
        assert!(output.contains("* "));
        assert!(output.contains("default"));
        assert!(output.contains("alt"));
    }

    #[test]
    fn table_empty() {
        let output = format_context_table(&[], Some(ContextId::new()));
        assert_eq!(output, "(no contexts)");
    }

    #[test]
    fn info_shows_fields() {
        let id = ContextId::new();
        let row = make_row(Some("test-ctx"), id);
        let output = format_context_info(&row, 2, 1, true, None);
        assert!(output.contains("test-ctx *"));
        assert!(output.contains("anthropic/claude-opus-4-6"));
        assert!(output.contains("Children: 2"));
        assert!(output.contains("Drifts:  1"));
        // context_type always renders.
        assert!(output.contains("Type:    default"));
        // No trace id passed -> no Trace line.
        assert!(!output.contains("Trace:"));
    }

    #[test]
    fn info_shows_placement_stamps_only_when_set() {
        let id = ContextId::new();
        let plain = make_row(Some("auto-placed"), id);
        let output = format_context_info(&plain, 0, 0, false, None);
        assert!(!output.contains("Promoted:"), "output: {output}");
        assert!(!output.contains("Demoted:"), "output: {output}");
        assert!(!output.contains("Paused:"), "output: {output}");

        let mut seated = make_row(Some("seated"), ContextId::new());
        seated.promoted_at = Some(kaijutsu_types::now_millis() as i64);
        seated.paused_at = Some(kaijutsu_types::now_millis() as i64);
        let output = format_context_info(&seated, 0, 0, false, None);
        assert!(output.contains("Promoted: just now"), "output: {output}");
        assert!(output.contains("Paused:  just now"), "output: {output}");
        assert!(!output.contains("Demoted:"), "output: {output}");
    }

    #[test]
    fn table_tags_promoted_contexts() {
        let seated_id = ContextId::new();
        let mut seated = make_row(Some("seated"), seated_id);
        seated.promoted_at = Some(1_000);
        let plain = make_row(Some("plain"), ContextId::new());

        let output = format_context_table(&[seated, plain], None);
        let lines: Vec<&str> = output.lines().collect();
        assert!(lines[0].contains("[ring0]"), "output: {output}");
        assert!(!lines[1].contains("[ring0]"), "output: {output}");
    }

    #[test]
    fn info_shows_trace_id_when_present() {
        let id = ContextId::new();
        let mut row = make_row(Some("coder-ctx"), id);
        row.context_type = "coder".to_string();
        let mut tid = [0u8; 16];
        tid[15] = 0xab;
        let output = format_context_info(&row, 0, 0, false, Some(tid));
        assert!(output.contains("Type:    coder"));
        // 32 lowercase hex chars, leading zeros preserved.
        assert!(output.contains("Trace:   000000000000000000000000000000ab"));
    }

    #[test]
    fn hex32_pads_and_lowercases() {
        let mut bytes = [0u8; 16];
        bytes[0] = 0x0a;
        bytes[15] = 0xff;
        let s = hex32(bytes);
        assert_eq!(s.len(), 32);
        assert_eq!(s, "0a0000000000000000000000000000ff");
    }

    #[test]
    fn model_format_variants() {
        assert_eq!(format_model(&Some("a".into()), &Some("b".into())), "a/b");
        assert_eq!(format_model(&None, &Some("b".into())), "b");
        assert_eq!(format_model(&None, &None), "(no model)");
    }
}
