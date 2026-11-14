//! Per-model thinking capabilities and tunable resolution for Gemini.
//!
//! - Gemini 2.5 models use integer `thinkingBudget` (`0` disables on Flash/Lite;
//!   Pro cannot disable).
//! - Gemini 3 models use discrete `thinkingLevel` (`MINIMAL`, `LOW`, `MEDIUM`,
//!   `HIGH`) and still accept `thinkingBudget` as a legacy fallback when
//!   `thinking_style = "budget"` or `thinking_budget` is set.
//! - Gemini 4 models (`gemini-4-carbon`, `gemini-4-argon`, `gemini-4-hydrogen`)
//!   use `thinkingLevel` (`LOW`, `MEDIUM`, `HIGH`, `XHIGH`). Thinking cannot be
//!   disabled (`none`/`off` and `MINIMAL` are rejected), and `thinkingBudget`
//!   is rejected.

use super::types::ThinkingConfig;
use crate::llm::stream::BuildOpts;
use crate::llm::{LlmError, LlmResult};

/// How a Gemini model family configures thinking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingControl {
    /// Gemini 2.5 family: integer `thinkingBudget`.
    Budget { max_budget: i64, can_disable: bool },
    /// Gemini 3 / 4 family: discrete `thinkingLevel`.
    Level {
        can_disable: bool,
        supports_minimal: bool,
        supports_xhigh: bool,
        supports_budget: bool,
    },
}

impl ThinkingControl {
    pub fn can_disable(self) -> bool {
        match self {
            Self::Budget { can_disable, .. } | Self::Level { can_disable, .. } => can_disable,
        }
    }
}

/// Classify a Gemini model id into its thinking control family.
pub fn classify(model: &str) -> ThinkingControl {
    let lower = model.to_ascii_lowercase();
    // Strip optional `models/` prefix if an operator wrote it.
    let name = lower.strip_prefix("models/").unwrap_or(&lower);
    if name.starts_with("gemini-2.5-pro") {
        ThinkingControl::Budget { max_budget: 32_768, can_disable: false }
    } else if name.starts_with("gemini-2.5-") {
        ThinkingControl::Budget { max_budget: 24_576, can_disable: true }
    } else if name.starts_with("gemini-4") {
        ThinkingControl::Level {
            can_disable: false,
            supports_minimal: false,
            supports_xhigh: true,
            supports_budget: false,
        }
    } else if name.contains("-pro") {
        ThinkingControl::Level {
            can_disable: false,
            supports_minimal: false,
            supports_xhigh: false,
            supports_budget: true,
        }
    } else {
        ThinkingControl::Level {
            can_disable: true,
            supports_minimal: true,
            supports_xhigh: false,
            supports_budget: true,
        }
    }
}

/// Default `one_shot_opts`: turn thinking off for models that allow it,
/// leaving always-on models thinking.
pub fn one_shot_opts(mut opts: BuildOpts) -> BuildOpts {
    if opts.effort.is_none() && opts.thinking_style.is_none() && classify(&opts.model).can_disable() {
        opts.effort = Some("none".to_string());
    }
    opts
}

/// Resolve `BuildOpts` thinking fields into Gemini's `thinkingConfig`.
///
/// Returns `Ok(None)` when the caller left thinking at provider default
/// (still enabling `includeThoughts: true` if thinking is not explicitly off,
/// so thought summaries and signatures flow when the model thinks).
pub fn resolve_thinking(opts: &BuildOpts) -> LlmResult<Option<ThinkingConfig>> {
    let control = classify(&opts.model);
    let style = opts.thinking_style.as_deref();
    let effort = opts.effort.as_deref();

    if let Some(s) = style
        && !matches!(s, "auto" | "adaptive" | "budget" | "off")
    {
        return Err(LlmError::InvalidRequest(format!(
            "gemini model '{}' does not support thinking_style '{s}' \
             (supported: auto, adaptive, budget, off)",
            opts.model
        )));
    }

    let wants_off = style == Some("off") || effort.is_some_and(|e| e.eq_ignore_ascii_case("none"));
    if wants_off {
        if !control.can_disable() {
            return Err(LlmError::InvalidRequest(format!(
                "gemini model '{}' has always-on thinking and cannot be turned off",
                opts.model
            )));
        }
        return Ok(Some(match control {
            ThinkingControl::Budget { .. } => ThinkingConfig {
                include_thoughts: Some(false),
                thinking_budget: Some(0),
                thinking_level: None,
            },
            ThinkingControl::Level { .. } => ThinkingConfig {
                include_thoughts: Some(false),
                thinking_budget: None,
                thinking_level: Some("MINIMAL".to_string()),
            },
        }));
    }

    if style == Some("budget") {
        if matches!(control, ThinkingControl::Level { supports_budget: false, .. }) {
            return Err(LlmError::InvalidRequest(format!(
                "gemini model '{}' does not support thinking_style='budget' (use thinking_level / effort)",
                opts.model
            )));
        }
        let Some(budget) = opts.thinking_budget else {
            return Err(LlmError::InvalidRequest(format!(
                "gemini model '{}': thinking_style='budget' requires thinking_budget",
                opts.model
            )));
        };
        if effort.is_some() {
            return Err(LlmError::InvalidRequest(format!(
                "gemini model '{}': thinking_style='budget' cannot be combined with effort",
                opts.model
            )));
        }
        let budget_i64 = i64::try_from(budget).unwrap_or(i64::MAX);
        return Ok(Some(ThinkingConfig {
            include_thoughts: Some(true),
            thinking_budget: Some(budget_i64),
            thinking_level: None,
        }));
    }

    if let Some(budget) = opts.thinking_budget {
        if matches!(control, ThinkingControl::Level { supports_budget: false, .. }) {
            return Err(LlmError::InvalidRequest(format!(
                "gemini model '{}' does not support thinking_budget (use effort)",
                opts.model
            )));
        }
        if effort.is_some() {
            return Err(LlmError::InvalidRequest(format!(
                "gemini model '{}': thinking_budget cannot be combined with effort",
                opts.model
            )));
        }
        let budget_i64 = i64::try_from(budget).unwrap_or(i64::MAX);
        return Ok(Some(ThinkingConfig {
            include_thoughts: Some(true),
            thinking_budget: Some(budget_i64),
            thinking_level: None,
        }));
    }

    if let Some(eff) = effort {
        return Ok(Some(match control {
            ThinkingControl::Level {
                supports_minimal,
                supports_xhigh,
                ..
            } => {
                let level = match eff.to_ascii_lowercase().as_str() {
                    "minimal" => {
                        if !supports_minimal {
                            return Err(LlmError::InvalidRequest(format!(
                                "gemini model '{}' does not support effort 'minimal' (lowest valid effort is 'low')",
                                opts.model
                            )));
                        }
                        "MINIMAL"
                    }
                    "low" => "LOW",
                    "medium" => "MEDIUM",
                    "high" => "HIGH",
                    "xhigh" | "max" => {
                        if supports_xhigh {
                            "XHIGH"
                        } else {
                            "HIGH"
                        }
                    }
                    other => {
                        return Err(LlmError::InvalidRequest(format!(
                            "gemini model '{}' does not support effort '{other}' \
                             (supported: none, minimal, low, medium, high, xhigh, max)",
                            opts.model
                        )));
                    }
                };
                ThinkingConfig {
                    include_thoughts: Some(true),
                    thinking_budget: None,
                    thinking_level: Some(level.to_string()),
                }
            }
            ThinkingControl::Budget { max_budget, .. } => {
                let budget = match eff.to_ascii_lowercase().as_str() {
                    "minimal" => 1024,
                    "low" => 2048,
                    "medium" => 8192,
                    "high" | "xhigh" | "max" => 24_576.min(max_budget),
                    other => {
                        return Err(LlmError::InvalidRequest(format!(
                            "gemini model '{}' does not support effort '{other}' \
                             (supported: none, minimal, low, medium, high, xhigh, max)",
                            opts.model
                        )));
                    }
                };
                ThinkingConfig {
                    include_thoughts: Some(true),
                    thinking_budget: Some(budget),
                    thinking_level: None,
                }
            }
        }));
    }

    // Default: include thoughts so reasoning blocks and thought signatures are
    // surfaced when the model thinks.
    Ok(Some(ThinkingConfig {
        include_thoughts: Some(true),
        thinking_budget: None,
        thinking_level: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemini_25_pro_refuses_thinking_off() {
        let mut opts = BuildOpts::new("gemini-2.5-pro", 4096);
        opts.effort = Some("none".into());
        assert!(resolve_thinking(&opts).is_err());
    }

    #[test]
    fn gemini_25_flash_disables_with_zero_budget() {
        let mut opts = BuildOpts::new("gemini-2.5-flash", 4096);
        opts.effort = Some("none".into());
        let cfg = resolve_thinking(&opts).unwrap().unwrap();
        assert_eq!(cfg.thinking_budget, Some(0));
        assert_eq!(cfg.include_thoughts, Some(false));
    }

    #[test]
    fn gemini_3_flash_maps_effort_to_thinking_level() {
        let mut opts = BuildOpts::new("gemini-3-flash", 4096);
        opts.effort = Some("medium".into());
        let cfg = resolve_thinking(&opts).unwrap().unwrap();
        assert_eq!(cfg.thinking_level.as_deref(), Some("MEDIUM"));
        assert_eq!(cfg.include_thoughts, Some(true));
    }

    #[test]
    fn budget_style_requires_budget_and_rejects_effort() {
        let mut opts = BuildOpts::new("gemini-3-pro", 4096);
        opts.thinking_style = Some("budget".into());
        assert!(resolve_thinking(&opts).is_err());

        opts.thinking_budget = Some(4096);
        let cfg = resolve_thinking(&opts).unwrap().unwrap();
        assert_eq!(cfg.thinking_budget, Some(4096));
        assert_eq!(cfg.thinking_level, None);

        opts.effort = Some("high".into());
        assert!(resolve_thinking(&opts).is_err());
    }

    #[test]
    fn gemini_4_maps_low_medium_high_xhigh_and_forbids_minimal_off_and_budget() {
        for model in [
            "gemini-4-carbon",
            "gemini-4-argon",
            "gemini-4-hydrogen",
            "models/gemini-4-hydrogen",
        ] {
            for (effort, expected_level) in [
                ("low", "LOW"),
                ("medium", "MEDIUM"),
                ("high", "HIGH"),
                ("xhigh", "XHIGH"),
                ("max", "XHIGH"),
            ] {
                let mut opts = BuildOpts::new(model, 8192);
                opts.effort = Some(effort.into());
                let cfg = resolve_thinking(&opts).unwrap().unwrap();
                assert_eq!(
                    cfg.thinking_level.as_deref(),
                    Some(expected_level),
                    "{model} effort={effort}"
                );
                assert_eq!(cfg.thinking_budget, None, "{model} must omit thinkingBudget");
                assert_eq!(cfg.include_thoughts, Some(true));
            }

            // Default leaves thinkingLevel unset (model default) and requests thoughts.
            let default_cfg = resolve_thinking(&BuildOpts::new(model, 8192)).unwrap().unwrap();
            assert_eq!(default_cfg.thinking_level, None);
            assert_eq!(default_cfg.thinking_budget, None);
            assert_eq!(default_cfg.include_thoughts, Some(true));

            // Gemini 4 cannot disable thinking or use MINIMAL.
            for rejected_effort in ["none", "minimal"] {
                let mut opts = BuildOpts::new(model, 8192);
                opts.effort = Some(rejected_effort.into());
                assert!(
                    resolve_thinking(&opts).is_err(),
                    "{model} must reject effort={rejected_effort}"
                );
            }
            let mut off_opts = BuildOpts::new(model, 8192);
            off_opts.thinking_style = Some("off".into());
            assert!(resolve_thinking(&off_opts).is_err());

            // Gemini 4 rejects thinkingBudget / thinking_style="budget".
            let mut budget_opts = BuildOpts::new(model, 8192);
            budget_opts.thinking_budget = Some(4096);
            assert!(resolve_thinking(&budget_opts).is_err());
            budget_opts.thinking_style = Some("budget".into());
            assert!(resolve_thinking(&budget_opts).is_err());

            // one_shot_opts must not inject effort="none" on Gemini 4.
            let one_shot = one_shot_opts(BuildOpts::new(model, 256));
            assert_eq!(one_shot.effort, None);
        }
    }

    #[test]
    fn gemini_3_flash_caps_xhigh_at_high_and_allows_minimal_and_off() {
        for (effort, expected_level, expected_thoughts) in [
            ("none", "MINIMAL", false),
            ("minimal", "MINIMAL", true),
            ("low", "LOW", true),
            ("medium", "MEDIUM", true),
            ("high", "HIGH", true),
            ("xhigh", "HIGH", true),
            ("max", "HIGH", true),
        ] {
            let mut opts = BuildOpts::new("gemini-3-flash", 4096);
            opts.effort = Some(effort.into());
            let cfg = resolve_thinking(&opts).unwrap().unwrap();
            assert_eq!(cfg.thinking_level.as_deref(), Some(expected_level));
            assert_eq!(cfg.thinking_budget, None);
            assert_eq!(cfg.include_thoughts, Some(expected_thoughts));
        }
        let one_shot = one_shot_opts(BuildOpts::new("gemini-3-flash", 256));
        assert_eq!(one_shot.effort.as_deref(), Some("none"));
    }

    #[test]
    fn thinking_config_wire_json_serializes_gemini_4_xhigh_without_thinking_budget() {
        let mut opts = BuildOpts::new("gemini-4-hydrogen", 4096);
        opts.effort = Some("xhigh".into());
        let cfg = resolve_thinking(&opts).unwrap().unwrap();
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "includeThoughts": true,
                "thinkingLevel": "XHIGH"
            })
        );
        assert!(json.get("thinkingBudget").is_none());
    }
}
