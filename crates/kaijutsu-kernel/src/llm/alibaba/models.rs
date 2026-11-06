//! Which thinking control each Alibaba model takes, and how `BuildOpts`
//! maps onto it.
//!
//! The control a model takes is protocol, so it lives here in code, keyed by
//! model id prefix (case-insensitive). What the operator tunes (the effort
//! level, `max_tokens`, the context window) stays on model rows and slots.
//! A model that matches no prefix is treated as toggle-only, with one warning
//! per model. See `docs/alibaba-backend.md`, "Thinking".

use crate::llm::stream::BuildOpts;
use crate::llm::{LlmError, LlmResult};

/// The effort ladder qwen3.8 takes.
const QWEN38_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
/// The effort ladder DeepSeek V4 takes on Alibaba.
const DEEPSEEK_V4_EFFORTS: &[&str] = &["low", "high", "max"];

/// The thinking control a model takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// `reasoning_effort` with one of `levels`; `off` says how thinking is
    /// turned off.
    Effort { levels: &'static [&'static str], off: Off },
    /// `enable_thinking` true or false, and no effort ladder.
    Toggle,
    /// Thinks on every request and refuses thinking off.
    AlwaysOn,
}

/// How an effort-ladder model turns thinking off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Off {
    /// `reasoning_effort: "none"`.
    EffortNone,
    /// `enable_thinking: false`.
    EnableThinkingFalse,
}

/// Model id prefixes and their control. The first match wins.
const MODELS: &[(&str, Control)] = &[
    ("qwen3.8-max", Control::Effort { levels: QWEN38_EFFORTS, off: Off::EffortNone }),
    ("qwen3.8-flash", Control::Effort { levels: QWEN38_EFFORTS, off: Off::EffortNone }),
    ("qwen3.7", Control::Toggle),
    ("qwen3.6", Control::Toggle),
    ("glm-5.3", Control::AlwaysOn),
    ("glm-5.2", Control::Toggle),
    ("glm-5.1", Control::Toggle),
    ("kimi-k2.7-code", Control::AlwaysOn),
    ("kimi-k2.6", Control::Toggle),
    ("kimi-k2.5", Control::Toggle),
    ("minimax-m2.5", Control::AlwaysOn),
    ("deepseek-v4", Control::Effort { levels: DEEPSEEK_V4_EFFORTS, off: Off::EnableThinkingFalse }),
];

/// The control `model` takes, or `None` when no prefix matches.
pub fn known_control(model: &str) -> Option<Control> {
    let model = model.to_ascii_lowercase();
    MODELS.iter().find(|(prefix, _)| model.starts_with(prefix)).map(|(_, c)| *c)
}

/// The control `model` takes. An unknown model is toggle-only, with one
/// warning per model this process.
pub fn control(model: &str) -> Control {
    if let Some(control) = known_control(model) {
        return control;
    }
    static WARNED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let first = WARNED
        .get_or_init(Default::default)
        .lock()
        .expect("warned-model mutex poisoned")
        .insert(model.to_string());
    if first {
        tracing::warn!(
            model,
            "Alibaba model is not in kaijutsu's thinking table; treating it as thinking \
             on or off only (enable_thinking), with no effort ladder"
        );
    }
    Control::Toggle
}

/// Whether a one-shot may turn thinking off for `model`.
pub fn can_turn_thinking_off(model: &str) -> bool {
    control(model) != Control::AlwaysOn
}

/// The thinking fields of one request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WireThinking {
    pub reasoning_effort: Option<String>,
    pub enable_thinking: Option<bool>,
    pub thinking_budget: Option<u64>,
    /// Thinking is on, so earlier turns' reasoning is preserved.
    pub on: bool,
}

/// Map `opts`'s `effort`, `thinking_style` and `thinking_budget` onto the
/// control `opts.model` takes.
///
/// Thinking is off for `effort = "none"` or `thinking_style = "off"`, and on
/// otherwise. A request the model cannot take is refused here, naming the
/// model: thinking off for a model that always thinks, an effort the model
/// does not list, an effort beside a `thinking_budget` (qwen3.8 refuses the
/// pair), and a thinking style outside `auto`, `adaptive`, `budget` and
/// `off`. An effort is never clamped to a neighbor.
pub fn resolve(opts: &BuildOpts) -> LlmResult<WireThinking> {
    let model = opts.model.as_str();
    let refuse = |msg: String| Err(LlmError::InvalidRequest(msg));
    let style_off = match opts.thinking_style.as_deref() {
        None | Some("auto" | "adaptive" | "budget") => false,
        Some("off") => true,
        Some(other) => {
            return refuse(format!(
                "thinking_style '{other}' is not one of auto, adaptive, budget or off (model {model})"
            ));
        }
    };
    if opts.thinking_style.as_deref() == Some("budget") && opts.thinking_budget.is_none() {
        return refuse(format!(
            "thinking_style=budget needs thinking_budget to be set (model {model})"
        ));
    }
    let level = opts.effort.as_deref().filter(|e| *e != "none");
    if style_off && let Some(level) = level {
        return refuse(format!(
            "thinking_style=off conflicts with effort '{level}' (model {model}); unset one of them"
        ));
    }
    let off = style_off || opts.effort.as_deref() == Some("none");
    let control = control(model);

    if off {
        if let Some(budget) = opts.thinking_budget
            && crate::llm::openai::build::should_warn_effort_dropped("alibaba", model, "thinking_budget", &budget.to_string())
        {
            tracing::warn!(model, budget, "thinking is off, so thinking_budget is not sent");
        }
        return match control {
            Control::AlwaysOn => refuse(format!(
                "model {model} thinks on every request and refuses thinking off; unset \
                 effort none and thinking_style off for this model"
            )),
            Control::Effort { off: Off::EffortNone, .. } => {
                Ok(WireThinking { reasoning_effort: Some("none".into()), ..Default::default() })
            }
            Control::Effort { off: Off::EnableThinkingFalse, .. } | Control::Toggle => {
                Ok(WireThinking { enable_thinking: Some(false), ..Default::default() })
            }
        };
    }

    let mut wire = WireThinking { on: true, thinking_budget: opts.thinking_budget, ..Default::default() };
    match control {
        Control::Effort { levels, .. } => {
            if let Some(level) = level {
                if !levels.contains(&level) {
                    return refuse(format!(
                        "model {model} takes effort {}, not '{level}'",
                        levels.join(", ")
                    ));
                }
                if opts.thinking_budget.is_some() {
                    return refuse(format!(
                        "model {model} refuses reasoning_effort together with thinking_budget; \
                         set effort '{level}' or thinking_budget, not both"
                    ));
                }
                wire.reasoning_effort = Some(level.to_string());
            }
        }
        Control::Toggle => {
            if let Some(level) = level {
                return refuse(format!(
                    "model {model} takes thinking on or off, not effort '{level}'; unset effort \
                     to think, or set effort none to turn thinking off"
                ));
            }
            wire.enable_thinking = Some(true);
        }
        Control::AlwaysOn => {
            if let Some(level) = level {
                return refuse(format!(
                    "model {model} thinks on every request and takes no effort, not '{level}'; \
                     unset effort for this model"
                ));
            }
        }
    }
    Ok(wire)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Prefixes match case-insensitively, and the first match wins, so a
    /// dated or suffixed id finds its family.
    #[test]
    fn prefixes_match_dated_and_suffixed_ids() {
        assert_eq!(known_control("MiniMax-M2.5"), Some(Control::AlwaysOn));
        assert_eq!(known_control("qwen3.7-plus-2026-10-01"), Some(Control::Toggle));
        assert!(matches!(known_control("deepseek-v4-pro-0813"), Some(Control::Effort { .. })));
        assert_eq!(known_control("qwen3-coder-plus"), None);
    }
}
