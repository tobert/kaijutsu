//! `kj cast` — named model ensembles.
//!
//! A **cast** is a team: one slot per *role*, where a role is a
//! `context_type` (coder, musician, mcp, …). A slot names a backend + model
//! and may override any of the `llm_defaults` tunables; a NULL knob falls back
//! to that floor at resolution time (see `llm::SlotTunables::over`).
//!
//! Roles are free-form TEXT with **no FK**: context types are an open set
//! defined by the rc tree, and a cast that names a role nobody has created yet
//! is a legitimate forward declaration, not an error.
//!
//! The kernel ships **no factory casts** — an ensemble is an operator/agent
//! decision, and shipping one would be the kernel guessing at who plays which
//! seat. Nothing on the turn path selects a cast yet; that wiring is Track B,
//! which consumes `LlmRegistry::resolved_slot`.

use clap::{Parser, Subcommand};
use kaijutsu_types::{CastId, ContentType};

use crate::kernel_db::{CastRow, CastSlotRow};

use super::{KjCaller, KjDispatcher, KjResult, clap_help_for};

#[derive(Parser, Debug)]
#[command(
    name = "cast",
    about = "Model ensembles: role → backend + model + tunables",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct CastArgs {
    #[command(subcommand)]
    command: CastCommand,
}

#[derive(Subcommand, Debug)]
enum CastCommand {
    /// List casts.
    #[command(alias = "ls")]
    List,
    /// Show one cast and its slots, with the defaults cascade applied.
    Show {
        /// Cast label
        label: String,
    },
    /// Create a cast (no slots yet).
    Create {
        /// Cast label (unique, case-insensitive)
        label: String,
        /// Description text
        #[arg(long, alias = "description")]
        desc: Option<String>,
    },
    /// Remove a cast; its slots cascade away with it.
    #[command(alias = "rm")]
    Remove {
        /// Cast label
        label: String,
    },
    /// Manage a cast's per-role slots.
    #[command(subcommand)]
    Slot(CastSlotCommand),
}

#[derive(Subcommand, Debug)]
enum CastSlotCommand {
    /// Create or replace one role's seat.
    Set {
        /// Cast label
        cast: String,
        /// Role — a context_type (coder, musician, mcp, …)
        role: String,
        /// Backend name
        #[arg(long)]
        backend: String,
        /// Model id
        #[arg(long)]
        model: String,
        /// Maximum RESPONSE tokens (omit to inherit the defaults floor)
        #[arg(long = "max-tokens")]
        max_tokens: Option<u64>,
        /// Sampling temperature, 0.0..=2.0
        #[arg(long)]
        temperature: Option<f64>,
        /// Nucleus sampling, (0.0, 1.0]
        #[arg(long = "top-p")]
        top_p: Option<f64>,
        /// Provider effort ladder token (e.g. low, high, max)
        #[arg(long)]
        effort: Option<String>,
        /// Extended-thinking token budget
        #[arg(long = "thinking-budget")]
        thinking_budget: Option<u64>,
        /// Extended-thinking style token
        #[arg(long = "thinking-style")]
        thinking_style: Option<String>,
        /// Loadout name (stored for a future capability binding; no consumer yet)
        #[arg(long)]
        loadout: Option<String>,
        /// Sparse slot-specific JSON
        #[arg(long)]
        extra: Option<String>,
    },
    /// Remove one role's seat.
    #[command(alias = "rm")]
    Remove {
        /// Cast label
        cast: String,
        /// Role
        role: String,
    },
}

impl KjDispatcher {
    pub(crate) async fn dispatch_cast(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<CastArgs>();
        }
        let parsed = match CastArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj cast: {e}"));
            }
        };

        let mutating = !matches!(parsed.command, CastCommand::List | CastCommand::Show { .. });
        if mutating
            && let Err(denied) = self.require_cap(caller, crate::mcp::Capability::ConfigWrite, "cast")
        {
            return denied;
        }

        let result = match parsed.command {
            CastCommand::List => self.cast_list(),
            CastCommand::Show { label } => self.cast_show(&label),
            CastCommand::Create { label, desc } => self.cast_create(&label, desc, caller),
            CastCommand::Remove { label } => self.cast_remove(&label),
            CastCommand::Slot(cmd) => self.cast_slot(cmd),
        };

        if mutating
            && matches!(result, KjResult::Ok { .. })
            && let Err(e) = self.reload_llm_registry().await
        {
            return KjResult::Err(format!(
                "kj cast: write landed but the LLM registry failed to reload: {e}"
            ));
        }
        result
    }

    fn cast_list(&self) -> KjResult {
        let db = self.kernel_db().lock();
        let casts = match db.list_casts() {
            Ok(c) => c,
            Err(e) => return KjResult::Err(format!("kj cast list: {e}")),
        };
        // A cast's LABEL is its full handle (UNIQUE, the resolver key).
        let labels = serde_json::Value::Array(
            casts
                .iter()
                .map(|c| serde_json::Value::String(c.label.clone()))
                .collect(),
        );
        if casts.is_empty() {
            return KjResult::ok_with_data(
                "(no casts — the kernel ships none; `kj cast create <label>` starts one)"
                    .to_string(),
                labels,
            );
        }
        let lines: Vec<String> = casts
            .iter()
            .map(|c| {
                let n = db.list_cast_slots(c.cast_id).map(|s| s.len()).unwrap_or(0);
                let desc = c
                    .description
                    .as_deref()
                    .map(|d| format!("  — {d}"))
                    .unwrap_or_default();
                format!("  {:<20} {n} slot(s){desc}", c.label)
            })
            .collect();
        KjResult::ok_with_data(lines.join("\n"), labels)
    }

    fn cast_show(&self, label: &str) -> KjResult {
        let db = self.kernel_db().lock();
        let cast = match db.get_cast_by_label(label) {
            Ok(Some(c)) => c,
            Ok(None) => return KjResult::Err(format!("kj cast show: '{label}' not found")),
            Err(e) => return KjResult::Err(format!("kj cast show: {e}")),
        };
        let slots = match db.list_cast_slots(cast.cast_id) {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj cast show: {e}")),
        };
        let backends: std::collections::HashMap<_, _> = match db.list_backends() {
            Ok(b) => b.into_iter().map(|b| (b.backend_id, b.name)).collect(),
            Err(e) => return KjResult::Err(format!("kj cast show: {e}")),
        };

        let mut lines = vec![format!("Cast: {}", cast.label)];
        if let Some(d) = &cast.description {
            lines.push(format!("Description: {d}"));
        }
        if slots.is_empty() {
            lines.push("Slots: (none)".to_string());
        }
        let mut slot_json = Vec::new();
        for s in &slots {
            let backend = backends
                .get(&s.backend_id)
                .cloned()
                .unwrap_or_else(|| "(missing backend)".to_string());
            let mut knobs = Vec::new();
            if let Some(v) = s.max_tokens {
                knobs.push(format!("max_tokens={v}"));
            }
            if let Some(v) = s.temperature {
                knobs.push(format!("temperature={v}"));
            }
            if let Some(v) = s.top_p {
                knobs.push(format!("top_p={v}"));
            }
            if let Some(v) = &s.effort {
                knobs.push(format!("effort={v}"));
            }
            if let Some(v) = s.thinking_budget {
                knobs.push(format!("thinking_budget={v}"));
            }
            if let Some(v) = &s.thinking_style {
                knobs.push(format!("thinking_style={v}"));
            }
            // Only overrides are shown; everything else inherits the floor,
            // and printing the inherited value here would read as a per-slot
            // decision nobody made.
            let knob_str = if knobs.is_empty() {
                " (all tunables inherit llm_defaults)".to_string()
            } else {
                format!(" [{}]", knobs.join(", "))
            };
            lines.push(format!("  {:<12} {backend}/{}{knob_str}", s.role, s.model));
            slot_json.push(serde_json::json!({
                "role": s.role,
                "backend": backend,
                "model": s.model,
                "max_tokens": s.max_tokens,
                "temperature": s.temperature,
                "top_p": s.top_p,
                "effort": s.effort,
                "thinking_budget": s.thinking_budget,
                "thinking_style": s.thinking_style,
                "loadout": s.loadout,
                "extra": s.extra,
            }));
        }
        let data = serde_json::json!({
            "label": cast.label,
            "description": cast.description,
            "slots": slot_json,
        });
        KjResult::ok_with_data(lines.join("\n"), data)
    }

    fn cast_create(&self, label: &str, desc: Option<String>, caller: &KjCaller) -> KjResult {
        let db = self.kernel_db().lock();
        match db.insert_cast(&CastRow {
            cast_id: CastId::new(),
            label: label.to_string(),
            description: desc,
            created_at: kaijutsu_types::now_millis() as i64,
            created_by: caller.principal_id,
        }) {
            Ok(()) => KjResult::ok(format!("created cast '{label}'")),
            Err(e) => KjResult::Err(format!("kj cast create: {e}")),
        }
    }

    fn cast_remove(&self, label: &str) -> KjResult {
        let db = self.kernel_db().lock();
        let cast = match db.get_cast_by_label(label) {
            Ok(Some(c)) => c,
            Ok(None) => return KjResult::Err(format!("kj cast remove: '{label}' not found")),
            Err(e) => return KjResult::Err(format!("kj cast remove: {e}")),
        };
        match db.delete_cast(cast.cast_id) {
            Ok(()) => KjResult::ok(format!("removed cast '{label}' (slots cascaded)")),
            Err(e) => KjResult::Err(format!("kj cast remove: {e}")),
        }
    }

    fn cast_slot(&self, cmd: CastSlotCommand) -> KjResult {
        let db = self.kernel_db().lock();
        match cmd {
            CastSlotCommand::Set {
                cast,
                role,
                backend,
                model,
                max_tokens,
                temperature,
                top_p,
                effort,
                thinking_budget,
                thinking_style,
                loadout,
                extra,
            } => {
                let c = match db.get_cast_by_label(&cast) {
                    Ok(Some(c)) => c,
                    Ok(None) => {
                        return KjResult::Err(format!(
                            "kj cast slot set: cast '{cast}' not found (create it first)"
                        ));
                    }
                    Err(e) => return KjResult::Err(format!("kj cast slot set: {e}")),
                };
                let b = match db.get_backend_by_name(&backend) {
                    Ok(Some(b)) => b,
                    Ok(None) => {
                        return KjResult::Err(format!(
                            "kj cast slot set: backend '{backend}' not found"
                        ));
                    }
                    Err(e) => return KjResult::Err(format!("kj cast slot set: {e}")),
                };
                if let Some(json) = &extra
                    && serde_json::from_str::<serde_json::Value>(json).is_err()
                {
                    return KjResult::Err("kj cast slot set: --extra must be valid JSON".to_string());
                }
                match db.set_cast_slot(&CastSlotRow {
                    cast_id: c.cast_id,
                    role: role.clone(),
                    backend_id: b.backend_id,
                    model: model.clone(),
                    max_tokens: max_tokens.map(|v| v as i64),
                    temperature,
                    top_p,
                    effort,
                    thinking_budget: thinking_budget.map(|v| v as i64),
                    thinking_style,
                    loadout,
                    extra,
                }) {
                    Ok(()) => KjResult::ok(format!("set {cast}/{role} → {backend}/{model}")),
                    Err(e) => KjResult::Err(format!("kj cast slot set: {e}")),
                }
            }
            CastSlotCommand::Remove { cast, role } => {
                let c = match db.get_cast_by_label(&cast) {
                    Ok(Some(c)) => c,
                    Ok(None) => {
                        return KjResult::Err(format!("kj cast slot remove: cast '{cast}' not found"));
                    }
                    Err(e) => return KjResult::Err(format!("kj cast slot remove: {e}")),
                };
                match db.delete_cast_slot(c.cast_id, &role) {
                    Ok(true) => KjResult::ok(format!("removed {cast}/{role}")),
                    Ok(false) => {
                        KjResult::Err(format!("kj cast slot remove: {cast} has no role '{role}'"))
                    }
                    Err(e) => KjResult::Err(format!("kj cast slot remove: {e}")),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{test_caller, test_dispatcher};
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    async fn seeded() -> KjDispatcher {
        let d = test_dispatcher().await;
        {
            let mut db = d.kernel_db().lock();
            crate::seed_backends::ensure_factory_backends(
                &mut db,
                kaijutsu_types::PrincipalId::system(),
            )
            .unwrap();
        }
        d.reload_llm_registry().await.unwrap();
        d
    }

    #[tokio::test]
    async fn kernel_ships_no_factory_casts() {
        let d = seeded().await;
        let c = test_caller();
        match d.dispatch(&argv(&["cast", "list"]), &c).await {
            KjResult::Ok { data: Some(v), .. } => {
                assert!(v.as_array().unwrap().is_empty(), "casts are operator-configured");
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn create_slot_set_then_the_registry_resolves_it_live() {
        let d = seeded().await;
        let c = test_caller();
        d.dispatch(&argv(&["cast", "create", "house"]), &c).await;
        let r = d
            .dispatch(
                &argv(&[
                    "cast", "slot", "set", "house", "coder", "--backend", "anthropic", "--model",
                    "claude-opus-5", "--temperature", "0.3",
                ]),
                &c,
            )
            .await;
        assert!(matches!(r, KjResult::Ok { .. }), "{r:?}");

        let registry = d.kernel().llm().read().await;
        let slot = registry.resolved_slot("house", "coder").expect("live after write");
        assert_eq!(slot.backend, "anthropic");
        assert_eq!(slot.model, "claude-opus-5");
        assert_eq!(slot.tunables.temperature, Some(0.3));
        assert_eq!(
            slot.tunables.max_tokens,
            Some(16384),
            "an unset slot knob inherits the llm_defaults floor"
        );
    }

    #[tokio::test]
    async fn slot_set_refuses_an_unknown_backend_and_an_unknown_cast() {
        let d = seeded().await;
        let c = test_caller();
        d.dispatch(&argv(&["cast", "create", "house"]), &c).await;
        match d
            .dispatch(
                &argv(&[
                    "cast", "slot", "set", "house", "coder", "--backend", "nope", "--model", "m",
                ]),
                &c,
            )
            .await
        {
            KjResult::Err(msg) => assert!(msg.contains("backend 'nope' not found"), "{msg}"),
            other => panic!("{other:?}"),
        }
        match d
            .dispatch(
                &argv(&[
                    "cast", "slot", "set", "ghost", "coder", "--backend", "anthropic", "--model",
                    "m",
                ]),
                &c,
            )
            .await
        {
            KjResult::Err(msg) => assert!(msg.contains("cast 'ghost' not found"), "{msg}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_role_needs_no_context_type_to_exist() {
        // Roles are free-form: a cast may name a context_type nobody has
        // created yet. That is a forward declaration, not an error.
        let d = seeded().await;
        let c = test_caller();
        d.dispatch(&argv(&["cast", "create", "future"]), &c).await;
        let r = d
            .dispatch(
                &argv(&[
                    "cast", "slot", "set", "future", "choreographer", "--backend", "deepseek",
                    "--model", "deepseek-v4-pro",
                ]),
                &c,
            )
            .await;
        assert!(matches!(r, KjResult::Ok { .. }), "{r:?}");
    }

    #[tokio::test]
    async fn slot_tunables_are_range_checked_loudly() {
        let d = seeded().await;
        let c = test_caller();
        d.dispatch(&argv(&["cast", "create", "house"]), &c).await;
        match d
            .dispatch(
                &argv(&[
                    "cast", "slot", "set", "house", "coder", "--backend", "anthropic", "--model",
                    "claude-opus-5", "--top-p", "1.5",
                ]),
                &c,
            )
            .await
        {
            KjResult::Err(msg) => assert!(msg.contains("top_p"), "{msg}"),
            other => panic!("out-of-range top_p must fail, not clamp: {other:?}"),
        }
    }

    #[tokio::test]
    async fn removing_a_cast_cascades_its_slots_and_clears_the_registry() {
        let d = seeded().await;
        let c = test_caller();
        d.dispatch(&argv(&["cast", "create", "house"]), &c).await;
        d.dispatch(
            &argv(&[
                "cast", "slot", "set", "house", "coder", "--backend", "anthropic", "--model",
                "claude-opus-5",
            ]),
            &c,
        )
        .await;
        assert!(d.kernel().llm().read().await.resolved_slot("house", "coder").is_some());
        d.dispatch(&argv(&["cast", "remove", "house"]), &c).await;
        assert!(d.kernel().llm().read().await.resolved_slot("house", "coder").is_none());
    }

    #[tokio::test]
    async fn duplicate_cast_labels_are_refused() {
        let d = seeded().await;
        let c = test_caller();
        d.dispatch(&argv(&["cast", "create", "house"]), &c).await;
        match d.dispatch(&argv(&["cast", "create", "HOUSE"]), &c).await {
            KjResult::Err(msg) => assert!(msg.to_lowercase().contains("already in use"), "{msg}"),
            other => panic!("labels are case-insensitively unique: {other:?}"),
        }
    }
}
