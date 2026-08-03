//! "What model does this context play?" — the one function that answers
//! that question.
//!
//! Track D (2026-08-03, cast-on-context renovation): a context now has THREE
//! possible sources for its effective model, in priority order:
//!
//! 1. An explicit **per-context override** — the context row's own
//!    `provider`/`model` columns, set via `kj context set --model` (or at
//!    `create`/fork time). This always wins; it is the same mechanism
//!    `kj model` already reports as `source: "context"`.
//! 2. A **cast slot** matched on the context's `context_type` — the named
//!    ensemble assigned via `kj context create --cast <label>` /
//!    `kj context set --cast <label>` (`ContextRow::cast_id`, resolved to a
//!    label by the caller) or inherited from a preset's `cast_id` at fork.
//! 3. The **registry-wide default** backend/model (`kj backend`'s
//!    `llm_defaults` row) — the same fallback `kj model` already reports as
//!    `source: "default"`.
//!
//! This module is deliberately NOT under `llm/` (that tree is a concurrent
//! Track B change) and takes no `KernelDb`/lock — it is a pure function over
//! already-resolved primitives, so it is trivially unit tested here and easy
//! to splice into the turn path later. The splice site is
//! `kaijutsu-server/src/llm_stream.rs`'s `spawn_llm_for_prompt`, in the
//! `provider_resolution` block (currently: explicit param > per-context
//! DriftRouter fields > registry default — see the module doc there). A
//! caller there would resolve `cast_label` from `ContextRow::cast_id` (via
//! `KernelDb::get_cast`) once per turn, then call this function with the
//! context's `context_type` in hand.

use crate::llm::{LlmRegistry, SlotTunables};

/// Why [`resolve_context_model`] answered the way it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSource {
    /// The context's own `provider`/`model` columns (`kj context set --model`,
    /// or set at `create`/fork time).
    ContextOverride,
    /// A cast slot matched on the context's `context_type`. Carries the cast
    /// label that answered, for display/logging.
    CastSlot { cast: String },
    /// The registry-wide default backend/model (`llm_defaults`).
    RegistryDefault,
}

/// The resolved answer to "what model does this context play?"
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedContextModel {
    /// Backend NAME (e.g. `anthropic`, `deepseek`, `gpt`).
    pub backend: String,
    pub model: String,
    /// Slot tunables with the `llm_defaults` floor already applied — `Some`
    /// only when [`ModelSource::CastSlot`] answered; neither a per-context
    /// override nor the bare registry default carries slot-level tunables
    /// today.
    pub tunables: Option<SlotTunables>,
    pub source: ModelSource,
}

/// Resolve the effective model for a context.
///
/// - `context_type`: the context's rc bucket (`coder`, `musician`, `mcp`, …)
///   — the cast slot's ROLE key.
/// - `context_provider` / `context_model`: the context row's own columns.
///   `context_model.is_some()` is what makes this an override (a provider
///   with no model is not enough) — mirrors `kj/model.rs`'s existing
///   `row_model.is_some() -> source = "context"` rule, so the two surfaces
///   can never disagree about what counts as "explicitly set".
/// - `cast_label`: the context's assigned cast, already resolved from
///   `ContextRow::cast_id` to its label by the caller (keeps this function
///   DB-free) — `None` when the context has no cast assigned.
/// - `registry`: for the cast-slot lookup and the final default fallback.
///
/// Returns `None` only when there is truly nothing to answer with: no
/// override, no matching cast slot, and no registry default configured
/// (`kj backend list` is empty). Never fabricates a model.
pub fn resolve_context_model(
    context_type: &str,
    context_provider: Option<&str>,
    context_model: Option<&str>,
    cast_label: Option<&str>,
    registry: &LlmRegistry,
) -> Option<ResolvedContextModel> {
    // 1. Explicit per-context override wins outright.
    if let Some(model) = context_model {
        let backend = context_provider
            .map(str::to_string)
            .or_else(|| registry.default_provider_name().map(str::to_string))?;
        return Some(ResolvedContextModel {
            backend,
            model: model.to_string(),
            tunables: None,
            source: ModelSource::ContextOverride,
        });
    }

    // 2. A cast slot matched on this context's context_type.
    if let Some(label) = cast_label
        && let Some(slot) = registry.resolved_slot(label, context_type)
    {
        return Some(ResolvedContextModel {
            backend: slot.backend.clone(),
            model: slot.model.clone(),
            tunables: Some(slot.tunables.clone()),
            source: ModelSource::CastSlot { cast: label.to_string() },
        });
    }

    // 3. Registry-wide default.
    let backend = registry.default_provider_name()?.to_string();
    let model = registry.default_model()?.to_string();
    Some(ResolvedContextModel {
        backend,
        model,
        tunables: None,
        source: ModelSource::RegistryDefault,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{BackendConfig, BackendKind, MockClient, Provider, ResolvedSlot};
    use std::sync::Arc;

    fn registry_with_default() -> LlmRegistry {
        let mut reg = LlmRegistry::new();
        reg.register("anthropic", Arc::new(Provider::Mock(MockClient::new("a"))));
        reg.set_default("anthropic");
        reg.set_default_model("claude-opus-4-8");
        reg.set_backends(vec![BackendConfig::new("anthropic", BackendKind::Anthropic)]);
        reg
    }

    /// Seat one cast slot (`house`/`coder` -> deepseek/deepseek-v4-pro) on an
    /// otherwise-default registry.
    fn registry_with_cast_slot() -> LlmRegistry {
        let mut reg = registry_with_default();
        reg.register("deepseek", Arc::new(Provider::Mock(MockClient::new("d"))));
        reg.set_backends(vec![
            BackendConfig::new("anthropic", BackendKind::Anthropic),
            BackendConfig::new("deepseek", BackendKind::DeepSeek),
        ]);
        reg.set_cast_slots(vec![(
            "house".to_string(),
            ResolvedSlot {
                role: "coder".to_string(),
                backend: "deepseek".to_string(),
                model: "deepseek-v4-pro".to_string(),
                tunables: SlotTunables {
                    max_tokens: Some(8192),
                    ..Default::default()
                },
                loadout: None,
                extra: None,
            },
        )]);
        reg
    }

    #[test]
    fn explicit_override_wins_over_everything() {
        let registry = registry_with_cast_slot();
        let resolved = resolve_context_model(
            "coder",
            Some("anthropic"),
            Some("claude-haiku-4-5"),
            Some("house"),
            &registry,
        )
        .expect("override present");
        assert_eq!(resolved.backend, "anthropic");
        assert_eq!(resolved.model, "claude-haiku-4-5");
        assert_eq!(resolved.source, ModelSource::ContextOverride);
        assert!(resolved.tunables.is_none());
    }

    #[test]
    fn override_with_model_but_no_provider_falls_back_to_registry_default_provider() {
        let registry = registry_with_default();
        let resolved = resolve_context_model("coder", None, Some("claude-haiku-4-5"), None, &registry)
            .expect("override present");
        assert_eq!(resolved.backend, "anthropic", "registry default provider fills the gap");
        assert_eq!(resolved.model, "claude-haiku-4-5");
        assert_eq!(resolved.source, ModelSource::ContextOverride);
    }

    #[test]
    fn cast_slot_wins_when_no_override_and_role_matches() {
        let registry = registry_with_cast_slot();
        let resolved = resolve_context_model("coder", None, None, Some("house"), &registry)
            .expect("cast slot present");
        assert_eq!(resolved.backend, "deepseek");
        assert_eq!(resolved.model, "deepseek-v4-pro");
        assert_eq!(resolved.source, ModelSource::CastSlot { cast: "house".to_string() });
        assert_eq!(resolved.tunables.as_ref().unwrap().max_tokens, Some(8192));
    }

    #[test]
    fn cast_assigned_but_role_has_no_seat_falls_through_to_registry_default() {
        let registry = registry_with_cast_slot();
        // "musician" has no slot in `house` — must fall through, not error.
        let resolved = resolve_context_model("musician", None, None, Some("house"), &registry)
            .expect("registry default present");
        assert_eq!(resolved.backend, "anthropic");
        assert_eq!(resolved.model, "claude-opus-4-8");
        assert_eq!(resolved.source, ModelSource::RegistryDefault);
    }

    #[test]
    fn unknown_cast_label_falls_through_to_registry_default() {
        // The context's cast_id pointed at a cast that's since been removed
        // (dangling label) — must not panic or error, just fall through.
        let registry = registry_with_cast_slot();
        let resolved = resolve_context_model("coder", None, None, Some("nonesuch"), &registry)
            .expect("registry default present");
        assert_eq!(resolved.source, ModelSource::RegistryDefault);
    }

    #[test]
    fn no_override_no_cast_falls_through_to_registry_default() {
        let registry = registry_with_default();
        let resolved =
            resolve_context_model("coder", None, None, None, &registry).expect("default present");
        assert_eq!(resolved.backend, "anthropic");
        assert_eq!(resolved.model, "claude-opus-4-8");
        assert_eq!(resolved.source, ModelSource::RegistryDefault);
    }

    #[test]
    fn nothing_configured_at_all_returns_none_never_fabricates() {
        let registry = LlmRegistry::new();
        let resolved = resolve_context_model("coder", None, None, None, &registry);
        assert!(resolved.is_none());
    }

    /// A cast-id-to-label lookup is the caller's job (this function is
    /// DB-free), but the whole point is that a REAL cast/slot round-trips
    /// through `LlmRegistry` the same way `kj cast slot set` +
    /// `LlmRegistry::resolved_slot` already do — this guards against a
    /// second, diverging notion of "what a cast slot means" creeping in here.
    #[test]
    fn agrees_with_registry_resolved_slot_directly() {
        let registry = registry_with_cast_slot();
        let direct = registry.resolved_slot("house", "coder").expect("slot exists");
        let resolved = resolve_context_model("coder", None, None, Some("house"), &registry)
            .expect("resolves");
        assert_eq!(resolved.backend, direct.backend);
        assert_eq!(resolved.model, direct.model);
        assert_eq!(resolved.tunables, Some(direct.tunables.clone()));
    }
}
