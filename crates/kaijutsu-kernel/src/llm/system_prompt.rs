//! Per-call system-prompt assembly (A4).
//!
//! The kernel ships a static system prompt at `assets/defaults/system.md`.
//! That base sets the cybernetic / 改善 stance but says nothing about where
//! the model is, what tools it has, or what's happening in the conversation.
//! `build_system_prompt` appends a structured situational addendum so the
//! model gets per-call awareness without losing the static base.

use kaijutsu_types::{ContextId, ContextState};

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

/// Append a structured situational addendum to a static system-prompt base.
///
/// Schema (only sections with data appear):
/// ```text
/// {base}
///
/// <situation>
///   <context id="…" label="…" state="live"/>
///   <model provider="anthropic" name="claude-haiku-4-5"/>
///   <tools count="N">name1, name2, ...</tools>
/// </situation>
/// ```
///
/// XML-ish so parsers in the model's prompt-engineering can latch on; flat
/// enough to read at a glance. Newlines are deliberate — providers split
/// long single-line preambles awkwardly.
pub fn build_system_prompt(base: &str, situational: &SituationalContext) -> String {
    if situational.is_empty() {
        return base.to_string();
    }

    let mut out = String::with_capacity(base.len() + 256);
    out.push_str(base.trim_end());
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

    #[test]
    fn empty_situational_returns_base_unchanged() {
        let base = "static base prompt";
        let out = build_system_prompt(base, &SituationalContext::default());
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
        let out = build_system_prompt("base", &situational);
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
        let out = build_system_prompt("base", &situational);
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
        let out = build_system_prompt("base", &situational);
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
        let out = build_system_prompt(base, &situational);
        assert!(out.starts_with("first line\nsecond line"));
        assert!(out.contains("<situation>"));
    }
}
