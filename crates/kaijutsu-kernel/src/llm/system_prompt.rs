//! Per-call system-prompt assembly (A4).
//!
//! The kernel ships a static system prompt at `assets/defaults/system.md`.
//! That base sets the cybernetic / 改善 stance but says nothing about where
//! the model is, what tools it has, or what's happening in the conversation.
//! `build_system_prompt` appends a structured situational addendum so the
//! model gets per-call awareness without losing the static base.
//!
//! Additional context-specific sections (the rc `.md` mechanism — task or
//! mode instructions installed at `/etc/rc/<context_type>/create/`) layer
//! between the static base and the situation block: `base → rc → situation`.

use kaijutsu_types::{BlockKind, BlockSnapshot, ContextId, ContextState, Role};

/// Per-call facts the system prompt should surface.
///
/// All fields are optional — emit only the addendum sections we have
/// data for. Pure data so the assembly can run anywhere (server hot path,
/// tests, etc.) without dragging the kernel's `Arc<RwLock<...>>` graph.
#[derive(Debug, Clone, Default)]
pub struct SituationalContext {
    pub context_id: Option<ContextId>,
    pub context_label: Option<String>,
    pub context_state: Option<ContextState>,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Names of tools currently visible in this context.
    pub tool_names: Vec<String>,
}

impl SituationalContext {
    /// True if no situational data is populated — caller can short-circuit.
    pub fn is_empty(&self) -> bool {
        self.context_id.is_none()
            && self.context_label.is_none()
            && self.context_state.is_none()
            && self.provider.is_none()
            && self.model.is_none()
            && self.tool_names.is_empty()
    }
}

/// Append rc-derived sections and a structured situational addendum to a
/// static system-prompt base.
///
/// Layout (only sections with data appear):
/// ```text
/// {base}
///
/// {rc_section_1}
///
/// {rc_section_2}
///
/// <situation>
///   <context id="…" label="…" state="live"/>
///   <model provider="anthropic" name="claude-haiku-4-5"/>
///   <tools count="N">name1, name2, ...</tools>
/// </situation>
/// ```
///
/// `rc_sections` carries the content of `(Role::System, BlockKind::Text)`
/// blocks the rc create/fork lifecycle has dropped into the conversation
/// — task/mode instructions that come between the static stance and the
/// per-call situation. Extract them with `extract_system_prompt_sections`.
///
/// XML-ish situation block so parsers in the model's prompt-engineering
/// can latch on; flat enough to read at a glance. Newlines are deliberate
/// — providers split long single-line preambles awkwardly.
pub fn build_system_prompt(
    base: &str,
    situational: &SituationalContext,
    rc_sections: &[String],
) -> String {
    if situational.is_empty() && rc_sections.is_empty() {
        return base.to_string();
    }

    let mut out = String::with_capacity(base.len() + 256);
    out.push_str(base.trim_end());

    for section in rc_sections {
        let trimmed = section.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.push_str("\n\n");
        out.push_str(trimmed);
    }

    if situational.is_empty() {
        out.push('\n');
        return out;
    }

    out.push_str("\n\n<situation>\n");

    if situational.context_id.is_some()
        || situational.context_label.is_some()
        || situational.context_state.is_some()
    {
        out.push_str("  <context");
        if let Some(id) = situational.context_id {
            out.push_str(&format!(" id=\"{}\"", id.short()));
        }
        if let Some(ref label) = situational.context_label {
            out.push_str(&format!(" label=\"{}\"", xml_escape(label)));
        }
        if let Some(state) = situational.context_state {
            out.push_str(&format!(" state=\"{}\"", state_to_str(state)));
        }
        out.push_str("/>\n");
    }

    if situational.provider.is_some() || situational.model.is_some() {
        out.push_str("  <model");
        if let Some(ref p) = situational.provider {
            out.push_str(&format!(" provider=\"{}\"", xml_escape(p)));
        }
        if let Some(ref m) = situational.model {
            out.push_str(&format!(" name=\"{}\"", xml_escape(m)));
        }
        out.push_str("/>\n");
    }

    if !situational.tool_names.is_empty() {
        out.push_str(&format!(
            "  <tools count=\"{}\">{}</tools>\n",
            situational.tool_names.len(),
            xml_escape(&situational.tool_names.join(", "))
        ));
    }

    out.push_str("</situation>\n");
    out
}

/// Pull the content of `(Role::System, BlockKind::Text)` blocks that should
/// contribute to the system prompt. Filters mirror `hydrate_from_blocks`:
/// skip ephemeral / excluded / compacted / empty blocks.
///
/// The result feeds `build_system_prompt`'s `rc_sections` parameter. rc
/// `.md` lifecycle scripts produce blocks in exactly this shape (see
/// `kj/lifecycle.rs::run_md_script`); any other producer that wants to
/// contribute system-prompt material can do the same.
pub fn extract_system_prompt_sections(blocks: &[BlockSnapshot]) -> Vec<String> {
    blocks
        .iter()
        .filter(|b| {
            b.role == Role::System
                && b.kind == BlockKind::Text
                && !b.ephemeral
                && !b.excluded
                && !b.compacted
                && !b.content.is_empty()
        })
        .map(|b| b.content.clone())
        .collect()
}

fn state_to_str(state: ContextState) -> &'static str {
    match state {
        ContextState::Live => "live",
        ContextState::Staging => "staging",
        ContextState::Archived => "archived",
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{BlockId, BlockSnapshotBuilder, ContextId, PrincipalId};

    fn snap(role: Role, kind: BlockKind, content: &str) -> BlockSnapshot {
        BlockSnapshotBuilder::new(
            BlockId::new(ContextId::new(), PrincipalId::new(), 0),
            kind,
        )
        .role(role)
        .content(content)
        .build()
    }

    #[test]
    fn empty_situational_no_rc_returns_base_unchanged() {
        let base = "static base prompt";
        let out = build_system_prompt(base, &SituationalContext::default(), &[]);
        assert_eq!(out, base);
    }

    #[test]
    fn label_state_provider_and_tools_appear_in_addendum() {
        let situational = SituationalContext {
            context_id: Some(ContextId::new()),
            context_label: Some("planning".to_string()),
            context_state: Some(ContextState::Live),
            provider: Some("anthropic".to_string()),
            model: Some("claude-haiku-4-5".to_string()),
            tool_names: vec!["block_create".to_string(), "shell".to_string()],
        };
        let out = build_system_prompt("base", &situational, &[]);
        assert!(out.contains("<situation>"));
        assert!(out.contains("label=\"planning\""));
        assert!(out.contains("state=\"live\""));
        assert!(out.contains("provider=\"anthropic\""));
        assert!(out.contains("name=\"claude-haiku-4-5\""));
        assert!(out.contains("count=\"2\""));
        assert!(out.contains("block_create"));
        assert!(out.contains("shell"));
    }

    #[test]
    fn xml_escape_keeps_addendum_well_formed() {
        let situational = SituationalContext {
            context_label: Some("a < b & \"q\"".to_string()),
            ..Default::default()
        };
        let out = build_system_prompt("base", &situational, &[]);
        assert!(out.contains("&lt;"));
        assert!(out.contains("&amp;"));
        assert!(out.contains("&quot;"));
        assert!(!out.contains("\"a <"), "raw < must be escaped, got: {out}");
    }

    #[test]
    fn only_populated_fields_render_addendum_sections() {
        // Only model+provider populated — no context, no tools sections.
        let situational = SituationalContext {
            provider: Some("anthropic".to_string()),
            model: Some("claude-haiku-4-5".to_string()),
            ..Default::default()
        };
        let out = build_system_prompt("base", &situational, &[]);
        assert!(out.contains("<model"));
        assert!(!out.contains("<context"), "no context fields → no <context> section");
        assert!(!out.contains("<tools"), "no tool names → no <tools> section");
    }

    #[test]
    fn base_prompt_is_preserved_verbatim() {
        let base = "first line\nsecond line\n";
        let situational = SituationalContext {
            model: Some("test-model".to_string()),
            ..Default::default()
        };
        let out = build_system_prompt(base, &situational, &[]);
        assert!(out.starts_with("first line\nsecond line"));
        assert!(out.contains("<situation>"));
    }

    // ── rc-derived sections (the .md system-prompt path) ─────────────────

    /// The bug the rc rework is fixing: an installed `.md` rc script
    /// produces `(Role::System, BlockKind::Text)` blocks that were
    /// invisible to the model before this change. With the extract +
    /// build pipeline wired, that content lands in the system prompt
    /// between the static base and the `<situation>` addendum.
    #[test]
    fn rc_md_content_reaches_system_prompt() {
        let blocks = vec![
            snap(Role::System, BlockKind::Text, "You are a focused planner."),
            snap(Role::User, BlockKind::Text, "user msg, must not leak"),
            snap(Role::Model, BlockKind::Text, "assistant msg, must not leak"),
        ];

        let sections = extract_system_prompt_sections(&blocks);
        assert_eq!(
            sections,
            vec!["You are a focused planner.".to_string()],
            "extractor must pick (System, Text) only, got: {sections:?}"
        );

        let situational = SituationalContext {
            model: Some("test-model".to_string()),
            ..Default::default()
        };
        let prompt = build_system_prompt("base stance", &situational, &sections);

        assert!(
            prompt.contains("You are a focused planner."),
            "rc section content must appear in prompt; got:\n{prompt}"
        );
        assert!(
            !prompt.contains("user msg"),
            "User text must not appear in system prompt; got:\n{prompt}"
        );
        assert!(
            !prompt.contains("assistant msg"),
            "Model text must not appear in system prompt; got:\n{prompt}"
        );

        // Ordering: base → rc → situation.
        let base_pos = prompt.find("base stance").expect("base present");
        let rc_pos = prompt.find("You are a focused planner.").expect("rc present");
        let situation_pos = prompt.find("<situation>").expect("situation present");
        assert!(
            base_pos < rc_pos && rc_pos < situation_pos,
            "expected base → rc → situation order; got base={base_pos}, rc={rc_pos}, situation={situation_pos}\nfull:\n{prompt}"
        );
    }

    #[test]
    fn extractor_skips_ephemeral_excluded_compacted_and_empty() {
        let mut ephemeral = snap(Role::System, BlockKind::Text, "ephemeral");
        ephemeral.ephemeral = true;
        let mut excluded = snap(Role::System, BlockKind::Text, "excluded");
        excluded.excluded = true;
        let mut compacted = snap(Role::System, BlockKind::Text, "compacted");
        compacted.compacted = true;
        let empty = snap(Role::System, BlockKind::Text, "");

        let blocks = vec![
            ephemeral,
            excluded,
            compacted,
            empty,
            snap(Role::System, BlockKind::Text, "keeper"),
        ];

        let sections = extract_system_prompt_sections(&blocks);
        assert_eq!(sections, vec!["keeper".to_string()]);
    }

    #[test]
    fn extractor_skips_non_text_system_blocks() {
        // System+Notification, System+Resource, System+Drift, etc. have
        // dedicated hydrate arms; they're not system-prompt material.
        let blocks = vec![
            snap(Role::System, BlockKind::Notification, "notif body"),
            snap(Role::System, BlockKind::Resource, "resource body"),
            snap(Role::System, BlockKind::Drift, "drift body"),
            snap(Role::System, BlockKind::Error, "error body"),
            snap(Role::System, BlockKind::Text, "keeper"),
        ];
        let sections = extract_system_prompt_sections(&blocks);
        assert_eq!(sections, vec!["keeper".to_string()]);
    }

    #[test]
    fn rc_sections_alone_without_situational_still_render() {
        // A context with rc sections but no situational data should
        // still get the rc material — no early return back to bare base.
        let sections = vec!["mode: planner".to_string()];
        let out = build_system_prompt("base", &SituationalContext::default(), &sections);
        assert!(out.contains("base"));
        assert!(out.contains("mode: planner"));
        assert!(!out.contains("<situation>"));
    }

    #[test]
    fn rc_sections_are_trimmed_and_blanks_skipped() {
        let sections = vec![
            "  leading and trailing whitespace  \n".to_string(),
            "".to_string(),
            "   ".to_string(),
            "second section".to_string(),
        ];
        let out = build_system_prompt("base", &SituationalContext::default(), &sections);
        // Blank sections shouldn't leave double-blank gaps.
        assert!(out.contains("leading and trailing whitespace"));
        assert!(out.contains("second section"));
        assert!(!out.contains("\n\n\n\n"), "no triple-blank gaps; got:\n{out}");
    }
}
