//! `kj character slot` — the model each of a character's roles runs.
//!
//! A character holds one **slot** per *role*, where a role is a
//! `context_type` (coder, musician, mcp, …), or `gate`, `judge` or
//! `distill`. A slot names a backend + model and may set any request
//! parameter. A parameter the slot leaves unset is read from the model's
//! `backend_models` row when a context character's slots are written; after
//! that nothing reads the character's slots, so a change reaches a context
//! when it is rotated.
//!
//! The `gate` and `judge` roles are not context types: they name the council
//! server for gate decisions and for the judge shadow
//! (`docs/council.md`, "Configuration"). `distill` names the model that
//! summarizes for `kj fork --compact` and `kj drift pull|merge`.
//!
//! Roles are free-form TEXT with **no FK**: context types are an open set
//! defined by the rc tree, and a slot that names a role nobody has created
//! yet is a legitimate forward declaration, not an error.
//!
//! The kernel ships **no factory slots**: which model plays which seat is an
//! operator or agent decision, and shipping one would be the kernel guessing.
//! A turn reads its model from the context's own slots
//! (`model_resolution::context_model`), never from the character's.

use clap::Subcommand;

use crate::kernel_db::CharacterSlotRow;

use super::{KjDispatcher, KjResult};

#[derive(Subcommand, Debug)]
pub(super) enum CharacterSlotCommand {
    /// Create or replace one role's slot on a character.
    Set {
        /// The character's name
        character: String,
        /// Role: a context_type (coder, musician, mcp, …); `gate` or `judge`
        /// for the council server (an mk backend) of the gate or the judge
        /// shadow; or `distill` for the model that summarizes for `kj fork
        /// --compact` and `kj drift pull|merge`
        role: String,
        /// Backend name
        #[arg(long)]
        backend: String,
        /// Model id
        #[arg(long)]
        model: String,
        /// Maximum RESPONSE tokens. Omit to read it from the model's
        /// `kj backend model set` row when a context's slots are copied; a
        /// context character write refuses a model request role with neither
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
        /// Extended-thinking style: budget needs --thinking-budget below max tokens
        #[arg(long = "thinking-style", value_parser = ["off", "auto", "adaptive", "budget"])]
        thinking_style: Option<String>,
    },
    /// Remove one role's slot from a character.
    #[command(alias = "rm")]
    Unset {
        /// The character's name
        character: String,
        /// Role
        role: String,
    },
    /// List a character's slots.
    #[command(alias = "ls")]
    List {
        /// The character's name
        character: String,
    },
}

impl KjDispatcher {
    pub(super) fn character_slot(&self, cmd: CharacterSlotCommand) -> KjResult {
        let db = self.kernel_db().lock();
        match cmd {
            CharacterSlotCommand::Set {
                character,
                role,
                backend,
                model,
                max_tokens,
                temperature,
                top_p,
                effort,
                thinking_budget,
                thinking_style,
            } => {
                let sheet = match db.get_character_by_name(&character) {
                    Ok(Some(sheet)) if sheet.retired_at.is_none() => sheet,
                    Ok(Some(_)) => {
                        return KjResult::Err(format!("kj character slot set: '{character}' is retired"));
                    }
                    Ok(None) => {
                        return KjResult::Err(format!(
                            "kj character slot set: character '{character}' not found (`kj character create {character}` first)"
                        ));
                    }
                    Err(e) => return KjResult::Err(format!("kj character slot set: {e}")),
                };
                let b = match db.get_backend_by_name(&backend) {
                    Ok(Some(b)) => b,
                    Ok(None) => {
                        return KjResult::Err(format!("kj character slot set: backend '{backend}' not found"));
                    }
                    Err(e) => return KjResult::Err(format!("kj character slot set: {e}")),
                };
                match db.set_character_slot(&CharacterSlotRow {
                    principal_id: sheet.principal_id,
                    role: role.clone(),
                    backend_id: b.backend_id,
                    model: model.clone(),
                    max_tokens: max_tokens.map(|v| v as i64),
                    temperature,
                    top_p,
                    effort,
                    thinking_budget: thinking_budget.map(|v| v as i64),
                    thinking_style,
                }) {
                    Ok(()) => {
                        let mut msg = format!("set {character}/{role} → {backend}/{model}");
                        // Write-time checks against the values a context
                        // character would copy: the slot's, else the model's
                        // row. Warn, not error: the write lands either way,
                        // since pinning `max_tokens` on the model row right
                        // after can make the slot valid.
                        let row_max_tokens = db
                            .list_backend_models(b.backend_id)
                            .ok()
                            .and_then(|rows| rows.into_iter().find(|r| r.model_id == model))
                            .and_then(|r| r.tunables.max_tokens)
                            .and_then(|v| u64::try_from(v).ok());
                        let effective_max_tokens = max_tokens.or(row_max_tokens);
                        match effective_max_tokens {
                            None if !matches!(role.as_str(), "gate" | "judge") => msg.push_str(&format!(
                                " — WARN: no max_tokens in the slot or in the model's row, and every \
                                 request carries max_tokens; making a context for {character} will \
                                 refuse until one is set (`--max-tokens` here, or `kj backend model \
                                 set {backend} {model} --max-tokens N`)"
                            )),
                            Some(effective) => {
                                if let Some(tb) = thinking_budget
                                    && tb >= effective
                                {
                                    msg.push_str(&format!(
                                        " — WARN: thinking_budget ({tb}) >= effective max_tokens \
                                         ({effective}) after the slot and model cascade; requests \
                                         on this slot will fail at build time until one is \
                                         raised or the other lowered"
                                    ));
                                }
                            }
                            None => {}
                        }
                        KjResult::ok(msg)
                    }
                    Err(e) => KjResult::Err(format!("kj character slot set: {e}")),
                }
            }
            CharacterSlotCommand::Unset { character, role } => {
                let sheet = match db.get_character_by_name(&character) {
                    Ok(Some(sheet)) => sheet,
                    Ok(None) => {
                        return KjResult::Err(format!("kj character slot unset: character '{character}' not found"));
                    }
                    Err(e) => return KjResult::Err(format!("kj character slot unset: {e}")),
                };
                match db.delete_character_slot(sheet.principal_id, &role) {
                    Ok(true) => KjResult::ok(format!("removed {character}/{role}")),
                    Ok(false) => {
                        KjResult::Err(format!("kj character slot unset: {character} has no slot for '{role}'"))
                    }
                    Err(e) => KjResult::Err(format!("kj character slot unset: {e}")),
                }
            }
            CharacterSlotCommand::List { character } => {
                let sheet = match db.get_character_by_name(&character) {
                    Ok(Some(sheet)) => sheet,
                    Ok(None) => {
                        return KjResult::Err(format!("kj character slot list: character '{character}' not found"));
                    }
                    Err(e) => return KjResult::Err(format!("kj character slot list: {e}")),
                };
                let slots = match db.list_character_slots(sheet.principal_id) {
                    Ok(s) => s,
                    Err(e) => return KjResult::Err(format!("kj character slot list: {e}")),
                };
                let backends: std::collections::HashMap<_, _> = match db.list_backends() {
                    Ok(b) => b.into_iter().map(|b| (b.backend_id, b.name)).collect(),
                    Err(e) => return KjResult::Err(format!("kj character slot list: {e}")),
                };
                slot_listing(&character, &slots, &backends)
            }
        }
    }
}

/// The text and the structured form of a character's slots.
pub(super) fn slot_listing(
    character: &str,
    slots: &[CharacterSlotRow],
    backends: &std::collections::HashMap<kaijutsu_types::BackendId, String>,
) -> KjResult {
    let mut lines = Vec::new();
    if slots.is_empty() {
        lines.push(format!("{character} has no slots (`kj character slot set {character} <role> --backend B --model M`)"));
    }
    let mut data = Vec::new();
    for s in slots {
        let backend = backends.get(&s.backend_id).cloned().unwrap_or_else(|| "(missing backend)".to_string());
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
        // Only the slot's own values are shown; the rest are read from the
        // model's row when a context's slots are copied, and printing them
        // here would read as a per-slot decision nobody made.
        let knob_str = if knobs.is_empty() {
            " (no parameters set)".to_string()
        } else {
            format!(" [{}]", knobs.join(", "))
        };
        lines.push(format!("  {:<12} {backend}/{}{knob_str}", s.role, s.model));
        data.push(serde_json::json!({
            "role": s.role,
            "backend": backend,
            "model": s.model,
            "max_tokens": s.max_tokens,
            "temperature": s.temperature,
            "top_p": s.top_p,
            "effort": s.effort,
            "thinking_budget": s.thinking_budget,
            "thinking_style": s.thinking_style,
        }));
    }
    KjResult::ok_with_data(lines.join("\n"), serde_json::Value::Array(data))
}

// Verb class: kj/effect.rs
use super::effect::{Classify, Effect};

impl Classify for CharacterSlotCommand {
    fn effect(&self) -> Effect {
        match self {
            Self::Set { .. } | Self::Unset { .. } => Effect::Write,
            Self::List { .. } => Effect::Read,
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
            let db = d.kernel_db().lock();
            crate::test_backends::example_backends(&db);
        }
        d.reload_llm_registry().await.unwrap();
        let r = d.dispatch(&argv(&["character", "create", "house"]), &test_caller()).await;
        assert!(r.is_ok(), "{}", r.message());
        d
    }

    #[tokio::test]
    async fn kernel_ships_no_factory_slots() {
        let d = seeded().await;
        match d.dispatch(&argv(&["character", "slot", "list", "house"]), &test_caller()).await {
            KjResult::Ok { data: Some(v), .. } => {
                assert!(v.as_array().unwrap().is_empty(), "slots are operator-configured");
            }
            other => panic!("{other:?}"),
        }
    }

    /// A slot write lands on the character and invents no value: a parameter
    /// the slot leaves unset stays unset until a context's slots are copied.
    ///
    /// Falsified by a slot write that fills `max_tokens` itself.
    #[tokio::test]
    async fn slot_set_writes_the_slot_and_fills_nothing_in() {
        let d = seeded().await;
        let r = d
            .dispatch(
                &argv(&[
                    "character", "slot", "set", "house", "coder", "--backend", "anthropic", "--model",
                    "claude-opus-5", "--temperature", "0.3",
                ]),
                &test_caller(),
            )
            .await;
        assert!(matches!(r, KjResult::Ok { .. }), "{r:?}");

        let db = d.kernel_db().lock();
        let house = db.get_character_by_name("house").unwrap().unwrap();
        let slot = db.get_character_slot(house.principal_id, "coder").unwrap().expect("written");
        assert_eq!(slot.model, "claude-opus-5");
        assert_eq!(slot.temperature, Some(0.3));
        assert_eq!(slot.max_tokens, None, "an unset slot parameter stays unset");
    }

    /// A model request slot with no `max_tokens` in the slot or the model's
    /// row says at write time that making a context will refuse.
    #[tokio::test]
    async fn slot_set_warns_when_no_max_tokens_will_reach_a_context() {
        let d = seeded().await;
        let c = test_caller();
        let set = |role: &'static str, extra: &[&'static str]| {
            let mut v = vec![
                "character", "slot", "set", "house", role, "--backend", "anthropic", "--model", "claude-opus-5",
            ];
            v.extend_from_slice(extra);
            argv(&v)
        };
        let bare = d.dispatch(&set("coder", &[]), &c).await;
        assert!(bare.message().contains("WARN") && bare.message().contains("no max_tokens"), "{}", bare.message());
        assert!(bare.message().contains("kj backend model set anthropic claude-opus-5"), "{}", bare.message());
        let sized = d.dispatch(&set("coder", &["--max-tokens", "4096"]), &c).await;
        assert!(!sized.message().contains("WARN"), "{}", sized.message());
        // A council role needs no max_tokens.
        let gate = d.dispatch(&set("gate", &[]), &c).await;
        assert!(!gate.message().contains("WARN"), "{}", gate.message());
    }

    #[tokio::test]
    async fn slot_set_refuses_an_unknown_backend_and_an_unknown_character() {
        let d = seeded().await;
        let c = test_caller();
        match d
            .dispatch(
                &argv(&["character", "slot", "set", "house", "coder", "--backend", "nope", "--model", "m"]),
                &c,
            )
            .await
        {
            KjResult::Err(msg) => assert!(msg.contains("backend 'nope' not found"), "{msg}"),
            other => panic!("{other:?}"),
        }
        match d
            .dispatch(
                &argv(&["character", "slot", "set", "ghost", "coder", "--backend", "anthropic", "--model", "m"]),
                &c,
            )
            .await
        {
            KjResult::Err(msg) => assert!(msg.contains("character 'ghost' not found"), "{msg}"),
            other => panic!("{other:?}"),
        }
    }

    /// A retired character takes no new slots: nothing will make a context
    /// for it.
    #[tokio::test]
    async fn slot_set_refuses_a_retired_character() {
        let d = seeded().await;
        let c = test_caller();
        let confirmed = crate::kj::KjCaller { confirmed: true, ..test_caller() };
        let r = d.dispatch(&argv(&["character", "retire", "house"]), &confirmed).await;
        assert!(r.is_ok(), "{}", r.message());
        match d
            .dispatch(
                &argv(&["character", "slot", "set", "house", "coder", "--backend", "anthropic", "--model", "m"]),
                &c,
            )
            .await
        {
            KjResult::Err(msg) => assert!(msg.contains("retired"), "{msg}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_role_needs_no_context_type_to_exist() {
        // Roles are free-form: a character may hold a slot for a
        // context_type nobody has created yet. That is a forward
        // declaration, not an error.
        let d = seeded().await;
        let r = d
            .dispatch(
                &argv(&[
                    "character", "slot", "set", "house", "choreographer", "--backend", "deepseek", "--model",
                    "deepseek-v4-pro",
                ]),
                &test_caller(),
            )
            .await;
        assert!(matches!(r, KjResult::Ok { .. }), "{r:?}");
    }

    #[tokio::test]
    async fn slot_tunables_are_range_checked_loudly() {
        let d = seeded().await;
        match d
            .dispatch(
                &argv(&[
                    "character", "slot", "set", "house", "coder", "--backend", "anthropic", "--model",
                    "claude-opus-5", "--top-p", "1.5",
                ]),
                &test_caller(),
            )
            .await
        {
            KjResult::Err(msg) => assert!(msg.contains("top_p"), "{msg}"),
            other => panic!("out-of-range top_p must fail, not clamp: {other:?}"),
        }
    }

    /// The write-time inverted-budget WARN: a `thinking_budget` that meets or
    /// exceeds the effective `max_tokens` is only ever caught loudly (an Err)
    /// at request-build time (`llm/claude/build.rs::resolve_thinking`). That
    /// stays as it is; this says so earlier, in the write's own success
    /// message. Warn, not error: the write must still land.
    #[tokio::test]
    async fn slot_set_warns_when_thinking_budget_meets_or_exceeds_the_slots_max_tokens() {
        let d = seeded().await;
        let r = d
            .dispatch(
                &argv(&[
                    "character", "slot", "set", "house", "coder", "--backend", "anthropic", "--model",
                    "claude-opus-5", "--max-tokens", "16384", "--thinking-budget", "20000",
                ]),
                &test_caller(),
            )
            .await;
        match r {
            KjResult::Ok { message, .. } => {
                assert!(
                    message.contains("WARN") && message.contains("thinking_budget"),
                    "success message must flag the inverted pair: {message}"
                );
                assert!(message.contains("20000"), "{message}");
                assert!(message.contains("16384"), "must name the resolved max_tokens, not just the raw knob: {message}");
            }
            other => panic!("the write must still land: {other:?}"),
        }
    }

    /// The check walks the same values a context copies: the model row's
    /// `max_tokens` stands in for the slot's, so a budget that fits the row is
    /// not flagged, and one that overflows a small row is.
    #[tokio::test]
    async fn slot_set_checks_the_budget_against_the_model_row() {
        let d = seeded().await;
        let c = test_caller();
        let slot = |budget: &'static str| {
            argv(&[
                "character", "slot", "set", "house", "coder", "--backend", "anthropic", "--model",
                "claude-opus-5", "--thinking-budget", budget,
            ])
        };
        let r = d
            .dispatch(&argv(&["backend", "model", "set", "anthropic", "claude-opus-5", "--max-tokens", "65536"]), &c)
            .await;
        assert!(r.is_ok(), "{}", r.message());
        let fits = d.dispatch(&slot("20000"), &c).await;
        assert!(fits.is_ok() && !fits.message().contains("WARN"), "the row's 65536 holds 20000: {}", fits.message());

        let r = d
            .dispatch(&argv(&["backend", "model", "set", "anthropic", "claude-opus-5", "--max-tokens", "8192"]), &c)
            .await;
        assert!(r.is_ok(), "{}", r.message());
        let overflows = d.dispatch(&slot("12000"), &c).await;
        assert!(
            overflows.message().contains("WARN") && overflows.message().contains("8192"),
            "the row's 8192 cannot hold 12000: {}",
            overflows.message()
        );
    }

    /// Same check against a small per-slot `--max-tokens`.
    #[tokio::test]
    async fn slot_set_warns_against_its_own_small_max_tokens() {
        let d = seeded().await;
        let r = d
            .dispatch(
                &argv(&[
                    "character", "slot", "set", "house", "coder", "--backend", "anthropic", "--model",
                    "claude-opus-5", "--max-tokens", "1000", "--thinking-budget", "1500",
                ]),
                &test_caller(),
            )
            .await;
        match r {
            KjResult::Ok { message, .. } => {
                assert!(message.contains("WARN"), "{message}");
                assert!(message.contains("1500"), "{message}");
                assert!(message.contains("1000"), "{message}");
            }
            other => panic!("the write must still land: {other:?}"),
        }
    }

    #[tokio::test]
    async fn slot_set_still_lands_despite_the_warning() {
        // Warn, not error: the slot must show the write took, even
        // though the pair is inverted.
        let d = seeded().await;
        d.dispatch(
            &argv(&[
                "character", "slot", "set", "house", "coder", "--backend", "anthropic", "--model",
                "claude-opus-5", "--max-tokens", "16384", "--thinking-budget", "20000",
            ]),
            &test_caller(),
        )
        .await;
        let db = d.kernel_db().lock();
        let house = db.get_character_by_name("house").unwrap().unwrap();
        let slot = db.get_character_slot(house.principal_id, "coder").unwrap().expect("write must still land");
        assert_eq!(slot.thinking_budget, Some(20000));
    }

    #[tokio::test]
    async fn slot_set_does_not_warn_when_the_pair_is_sane() {
        let d = seeded().await;
        let r = d
            .dispatch(
                &argv(&[
                    "character", "slot", "set", "house", "coder", "--backend", "anthropic", "--model",
                    "claude-opus-5", "--max-tokens", "4096", "--thinking-budget", "1000",
                ]),
                &test_caller(),
            )
            .await;
        match r {
            KjResult::Ok { message, .. } => {
                assert!(!message.contains("WARN"), "should not warn: {message}");
            }
            other => panic!("{other:?}"),
        }
    }

    /// `unset` removes one role and says so when the role is absent; `list`
    /// shows what is left.
    #[tokio::test]
    async fn slot_unset_removes_one_role_and_list_shows_the_rest() {
        let d = seeded().await;
        let c = test_caller();
        for role in ["coder", "distill"] {
            let r = d
                .dispatch(
                    &argv(&[
                        "character", "slot", "set", "house", role, "--backend", "anthropic", "--model", "claude-opus-5",
                        "--max-tokens", "4096",
                    ]),
                    &c,
                )
                .await;
            assert!(r.is_ok(), "{}", r.message());
        }
        let removed = d.dispatch(&argv(&["character", "slot", "unset", "house", "coder"]), &c).await;
        assert!(removed.is_ok(), "{}", removed.message());
        let again = d.dispatch(&argv(&["character", "slot", "unset", "house", "coder"]), &c).await;
        assert!(!again.is_ok() && again.message().contains("no slot for 'coder'"), "{}", again.message());
        let listed = d.dispatch(&argv(&["character", "slot", "list", "house"]), &c).await;
        assert!(listed.message().contains("distill") && !listed.message().contains("coder"), "{}", listed.message());
    }

    /// The old verb is gone, not aliased.
    #[tokio::test]
    async fn kj_cast_is_gone() {
        let d = seeded().await;
        let r = d.dispatch(&argv(&["cast", "list"]), &test_caller()).await;
        assert!(!r.is_ok(), "`kj cast` must be unknown: {}", r.message());
    }
}
