//! `kj character slot` and `kj character model` — the models a character
//! runs.
//!
//! `kj character model set|unset|show` holds the character's **one model**.
//! A context copies it under role `model`, which its turns run on, and under
//! `distill` when the character has no `distill` slot. A root character runs
//! no model.
//!
//! A character also holds up to two **slots**: `distill`, a cheaper model
//! that summarizes for `kj fork --compact` and `kj drift pull|merge`, and
//! `gate`, the council server (a backend of kind `council`) that judges the gate
//! (`docs/council.md`, "Configuration"). No other role exists: the judge is
//! a character of its own, whose model is its council server.
//!
//! A slot names a backend + model and may set any request parameter. A
//! parameter the slot leaves unset is read from the model's `backend_models`
//! row when a context's slots are written; after that nothing reads the
//! character's slots, so a change reaches a context when it is rotated.
//!
//! The kernel ships **no factory slots**: which model plays which seat is an
//! operator or agent decision, and shipping one would be the kernel guessing.
//! A turn reads its model from the context's own slots
//! (`model_resolution::context_model`), never from the character's.

use clap::Subcommand;

use crate::kernel_db::{CharacterModelRow, CharacterSlotRow};

use super::{KjDispatcher, KjResult};

#[derive(Subcommand, Debug)]
pub(super) enum CharacterSlotCommand {
    /// Create or replace one role's slot on a character.
    Set {
        /// The character's name
        character: String,
        /// Role: `distill` for the model that summarizes for `kj fork
        /// --compact` and `kj drift pull|merge`, or `gate` for the council
        /// server (a council backend) that judges the gate. The character's chat
        /// model is `kj character model set`
        role: String,
        /// The model as `<backend>/<model>`, split at the first `/`, as `kj
        /// character show` prints it (for example `deepseek/deepseek-flash`).
        /// `kj backend list` names the backends and `kj models` the models
        #[arg(value_name = "BACKEND/MODEL")]
        model: String,
        /// Maximum RESPONSE tokens. Omit to read it from the model's
        /// `kj backend model set` row when a context's slots are copied;
        /// making a context refuses a `distill` slot with neither. A council
        /// server needs none
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
        /// Role: `distill` or `gate`
        role: String,
    },
    /// List a character's slots.
    #[command(alias = "ls")]
    List {
        /// The character's name
        character: String,
    },
}

#[derive(Subcommand, Debug)]
pub(super) enum CharacterModelCommand {
    /// Set the character's model, replacing the one it has. A context copies
    /// it under role `model`, which its turns run on, and under `distill`
    /// when the character has no `distill` slot. The copy is made when the
    /// context is, so a change reaches a running context at `kj context
    /// rotate`. A root character refuses it.
    Set {
        /// The character's name
        character: String,
        /// The model as `<backend>/<model>`, split at the first `/`, as `kj
        /// character show` prints it (for example `deepseek/deepseek-flash`).
        /// `kj backend list` names the backends and `kj models` the models
        #[arg(value_name = "BACKEND/MODEL")]
        model: String,
        /// Maximum RESPONSE tokens. Omit to read it from the model's
        /// `kj backend model set` row when a context is made; making a
        /// context refuses a model with neither
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
    /// Remove the character's model. Role slots stay. Contexts keep the
    /// slots they copied until they are rotated.
    #[command(alias = "rm")]
    Unset {
        /// The character's name
        character: String,
    },
    /// Show the character's model.
    Show {
        /// The character's name
        character: String,
    },
}

/// Split a `<backend>/<model>` value at the first `/`. Backend names hold no
/// `/` (`kj backend set` refuses one), while model ids may (`zai-org/GLM-5.3`).
/// A value with no `/`, or an empty half, refuses with the form.
pub(super) fn split_backend_model(value: &str) -> Result<(String, String), String> {
    match value.trim().split_once('/') {
        Some((backend, model)) if !backend.trim().is_empty() && !model.trim().is_empty() => {
            Ok((backend.trim().to_string(), model.trim().to_string()))
        }
        _ => Err(format!(
            "model '{value}' must be <backend>/<model>, such as deepseek/deepseek-flash; \
             `kj backend list` names the backends and `kj models` lists their models"
        )),
    }
}

/// The structured form of a character's model, or null.
pub(super) fn model_data(
    model: Option<&CharacterModelRow>,
    backends: &std::collections::HashMap<kaijutsu_types::BackendId, String>,
) -> serde_json::Value {
    let Some(m) = model else { return serde_json::Value::Null };
    serde_json::json!({
        "backend": backends.get(&m.backend_id).cloned().unwrap_or_else(|| "(missing backend)".to_string()),
        "model": m.model,
        "max_tokens": m.max_tokens,
        "temperature": m.temperature,
        "top_p": m.top_p,
        "effort": m.effort,
        "thinking_budget": m.thinking_budget,
        "thinking_style": m.thinking_style,
    })
}

/// `backend/model [knobs]` for a character's model.
pub(super) fn model_summary(
    m: &CharacterModelRow,
    backends: &std::collections::HashMap<kaijutsu_types::BackendId, String>,
) -> String {
    let backend = backends.get(&m.backend_id).cloned().unwrap_or_else(|| "(missing backend)".to_string());
    let knobs = knob_list(m.max_tokens, m.temperature, m.top_p, m.effort.as_deref(), m.thinking_budget, m.thinking_style.as_deref());
    if knobs.is_empty() {
        format!("{backend}/{} (no parameters set)", m.model)
    } else {
        format!("{backend}/{} [{}]", m.model, knobs.join(", "))
    }
}

fn knob_list(
    max_tokens: Option<i64>,
    temperature: Option<f64>,
    top_p: Option<f64>,
    effort: Option<&str>,
    thinking_budget: Option<i64>,
    thinking_style: Option<&str>,
) -> Vec<String> {
    let mut knobs = Vec::new();
    if let Some(v) = max_tokens {
        knobs.push(format!("max_tokens={v}"));
    }
    if let Some(v) = temperature {
        knobs.push(format!("temperature={v}"));
    }
    if let Some(v) = top_p {
        knobs.push(format!("top_p={v}"));
    }
    if let Some(v) = effort {
        knobs.push(format!("effort={v}"));
    }
    if let Some(v) = thinking_budget {
        knobs.push(format!("thinking_budget={v}"));
    }
    if let Some(v) = thinking_style {
        knobs.push(format!("thinking_style={v}"));
    }
    knobs
}

impl KjDispatcher {
    pub(super) fn character_model(&self, cmd: CharacterModelCommand) -> KjResult {
        let db = self.kernel_db().lock();
        match cmd {
            CharacterModelCommand::Set {
                character,
                model,
                max_tokens,
                temperature,
                top_p,
                effort,
                thinking_budget,
                thinking_style,
            } => {
                let (backend, model) = match split_backend_model(&model) {
                    Ok(pair) => pair,
                    Err(e) => return KjResult::Err(format!("kj character model set: {e}")),
                };
                let sheet = match db.get_character_by_name(&character) {
                    Ok(Some(sheet)) if sheet.retired_at.is_none() => sheet,
                    Ok(Some(_)) => {
                        return KjResult::Err(format!("kj character model set: '{character}' is retired"));
                    }
                    Ok(None) => {
                        return KjResult::Err(format!(
                            "kj character model set: character '{character}' not found (`kj character create {character}` first)"
                        ));
                    }
                    Err(e) => return KjResult::Err(format!("kj character model set: {e}")),
                };
                let b = match db.get_backend_by_name(&backend) {
                    Ok(Some(b)) => b,
                    Ok(None) => {
                        return KjResult::Err(format!("kj character model set: backend '{backend}' not found"));
                    }
                    Err(e) => return KjResult::Err(format!("kj character model set: {e}")),
                };
                if let Err(e) = db.set_character_model(&CharacterModelRow {
                    principal_id: sheet.principal_id,
                    backend_id: b.backend_id,
                    model: model.clone(),
                    max_tokens: max_tokens.map(|v| v as i64),
                    temperature,
                    top_p,
                    effort,
                    thinking_budget: thinking_budget.map(|v| v as i64),
                    thinking_style,
                }) {
                    return KjResult::Err(format!("kj character model set: {e}"));
                }
                let mut msg = format!("set the model of {character} → {backend}/{model}");
                msg.push_str(&max_tokens_warnings(&db, &character, &backend, &model, b.backend_id, max_tokens, thinking_budget, !crate::model_resolution::is_council_kind(&b.kind)));
                KjResult::ok(msg)
            }
            CharacterModelCommand::Unset { character } => {
                let sheet = match db.get_character_by_name(&character) {
                    Ok(Some(sheet)) => sheet,
                    Ok(None) => {
                        return KjResult::Err(format!("kj character model unset: character '{character}' not found"));
                    }
                    Err(e) => return KjResult::Err(format!("kj character model unset: {e}")),
                };
                match db.delete_character_model(sheet.principal_id) {
                    Ok(true) => KjResult::ok(format!("removed the model of {character}")),
                    Ok(false) => KjResult::Err(format!("kj character model unset: {character} has no model")),
                    Err(e) => KjResult::Err(format!("kj character model unset: {e}")),
                }
            }
            CharacterModelCommand::Show { character } => {
                let sheet = match db.get_character_by_name(&character) {
                    Ok(Some(sheet)) => sheet,
                    Ok(None) => {
                        return KjResult::Err(format!("kj character model show: character '{character}' not found"));
                    }
                    Err(e) => return KjResult::Err(format!("kj character model show: {e}")),
                };
                let model = match db.get_character_model(sheet.principal_id) {
                    Ok(m) => m,
                    Err(e) => return KjResult::Err(format!("kj character model show: {e}")),
                };
                let backends: std::collections::HashMap<_, _> = match db.list_backends() {
                    Ok(b) => b.into_iter().map(|b| (b.backend_id, b.name)).collect(),
                    Err(e) => return KjResult::Err(format!("kj character model show: {e}")),
                };
                let text = match &model {
                    Some(m) => format!("{character}: {}", model_summary(m, &backends)),
                    None => format!(
                        "{character} has no model (`kj character model set {character} BACKEND/MODEL`)"
                    ),
                };
                KjResult::ok_with_data(text, model_data(model.as_ref(), &backends))
            }
        }
    }
}

/// Write-time warnings against the values a context would copy: the
/// source's own, else the model's row. Warn, not error: the write lands
/// either way, since pinning `max_tokens` on the model row right after can
/// make it valid. `needs_max_tokens` is false for a council server.
#[allow(clippy::too_many_arguments)]
fn max_tokens_warnings(
    db: &crate::kernel_db::KernelDb,
    character: &str,
    backend: &str,
    model: &str,
    backend_id: kaijutsu_types::BackendId,
    max_tokens: Option<u64>,
    thinking_budget: Option<u64>,
    needs_max_tokens: bool,
) -> String {
    let row_max_tokens = db
        .list_backend_models(backend_id)
        .ok()
        .and_then(|rows| rows.into_iter().find(|r| r.model_id == model))
        .and_then(|r| r.tunables.max_tokens)
        .and_then(|v| u64::try_from(v).ok());
    match max_tokens.or(row_max_tokens) {
        None if needs_max_tokens => format!(
            " — WARN: no max_tokens in the settings or in the model's row, and every \
             request carries max_tokens; making a context for {character} will \
             refuse until one is set (`--max-tokens` here, or `kj backend model \
             set {backend} {model} --max-tokens N`)"
        ),
        Some(effective) => match thinking_budget {
            Some(tb) if tb >= effective => format!(
                " — WARN: thinking_budget ({tb}) >= effective max_tokens \
                 ({effective}) after the settings and model cascade; requests \
                 will fail at build time until one is raised or the other lowered"
            ),
            _ => String::new(),
        },
        None => String::new(),
    }
}

impl KjDispatcher {
    pub(super) fn character_slot(&self, cmd: CharacterSlotCommand) -> KjResult {
        let db = self.kernel_db().lock();
        match cmd {
            CharacterSlotCommand::Set {
                character,
                role,
                model,
                max_tokens,
                temperature,
                top_p,
                effort,
                thinking_budget,
                thinking_style,
            } => {
                if let Err(e) = crate::model_resolution::refuse_character_slot_role(&role, &character) {
                    return KjResult::Err(format!("kj character slot set: {e}"));
                }
                let (backend, model) = match split_backend_model(&model) {
                    Ok(pair) => pair,
                    Err(e) => return KjResult::Err(format!("kj character slot set: {e}")),
                };
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
                        msg.push_str(&max_tokens_warnings(
                            &db,
                            &character,
                            &backend,
                            &model,
                            b.backend_id,
                            max_tokens,
                            thinking_budget,
                            !crate::model_resolution::is_council_kind(&b.kind),
                        ));
                        KjResult::ok(msg)
                    }
                    Err(e) => KjResult::Err(format!("kj character slot set: {e}")),
                }
            }
            CharacterSlotCommand::Unset { character, role } => {
                if let Err(e) = crate::model_resolution::refuse_character_slot_role(&role, &character) {
                    return KjResult::Err(format!("kj character slot unset: {e}"));
                }
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

/// The structured form of a character's slots: one object per role.
pub(super) fn slot_data(
    slots: &[CharacterSlotRow],
    backends: &std::collections::HashMap<kaijutsu_types::BackendId, String>,
) -> serde_json::Value {
    serde_json::Value::Array(
        slots
            .iter()
            .map(|s| {
                serde_json::json!({
                    "role": s.role,
                    "backend": backends.get(&s.backend_id).cloned().unwrap_or_else(|| "(missing backend)".to_string()),
                    "model": s.model,
                    "max_tokens": s.max_tokens,
                    "temperature": s.temperature,
                    "top_p": s.top_p,
                    "effort": s.effort,
                    "thinking_budget": s.thinking_budget,
                    "thinking_style": s.thinking_style,
                })
            })
            .collect(),
    )
}

/// The text and the structured form of a character's slots.
fn slot_listing(
    character: &str,
    slots: &[CharacterSlotRow],
    backends: &std::collections::HashMap<kaijutsu_types::BackendId, String>,
) -> KjResult {
    let mut lines = Vec::new();
    if slots.is_empty() {
        lines.push(format!("{character} has no slots (`kj character slot set {character} distill|gate BACKEND/MODEL`)"));
    }
    for s in slots {
        let backend = backends.get(&s.backend_id).cloned().unwrap_or_else(|| "(missing backend)".to_string());
        let knobs = knob_list(s.max_tokens, s.temperature, s.top_p, s.effort.as_deref(), s.thinking_budget, s.thinking_style.as_deref());
        // Only the slot's own values are shown; the rest are read from the
        // model's row when a context's slots are copied, and printing them
        // here would read as a per-slot decision nobody made.
        let knob_str = if knobs.is_empty() {
            " (no parameters set)".to_string()
        } else {
            format!(" [{}]", knobs.join(", "))
        };
        lines.push(format!("  {:<12} {backend}/{}{knob_str}", s.role, s.model));
    }
    KjResult::ok_with_data(lines.join("\n"), slot_data(slots, backends))
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

impl Classify for CharacterModelCommand {
    fn effect(&self) -> Effect {
        match self {
            Self::Set { .. } | Self::Unset { .. } => Effect::Write,
            Self::Show { .. } => Effect::Read,
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn split_backend_model_trims_both_halves() {
        assert_eq!(super::split_backend_model(" deepseek/x ").unwrap(), ("deepseek".to_string(), "x".to_string()));
        assert_eq!(super::split_backend_model(" deepseek / x ").unwrap(), ("deepseek".to_string(), "x".to_string()));
        assert!(super::split_backend_model("deepseek/ ").is_err());
        assert!(super::split_backend_model(" /x").is_err());
    }

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
        let r = d.dispatch(&argv(&["character", "create", "house", "--rc", "coder"]), &test_caller()).await;
        assert!(r.is_ok(), "{}", r.message());
        let r = d.dispatch(&argv(&["backend", "set", "nagaya", "--kind", "council", "--base-url", "http://nagaya:8091", "--key-optional"]), &test_caller()).await;
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
                    "character", "slot", "set", "house", "distill", "anthropic/claude-opus-5", "--temperature", "0.3",
                ]),
                &test_caller(),
            )
            .await;
        assert!(matches!(r, KjResult::Ok { .. }), "{r:?}");

        let db = d.kernel_db().lock();
        let house = db.get_character_by_name("house").unwrap().unwrap();
        let slot = db.get_character_slot(house.principal_id, "distill").unwrap().expect("written");
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
                "character", "slot", "set", "house", role, "anthropic/claude-opus-5",
            ];
            v.extend_from_slice(extra);
            argv(&v)
        };
        let bare = d.dispatch(&set("distill", &[]), &c).await;
        assert!(bare.message().contains("WARN") && bare.message().contains("no max_tokens"), "{}", bare.message());
        assert!(bare.message().contains("kj backend model set anthropic claude-opus-5"), "{}", bare.message());
        let sized = d.dispatch(&set("distill", &["--max-tokens", "4096"]), &c).await;
        assert!(!sized.message().contains("WARN"), "{}", sized.message());
        // A council server needs no max_tokens, by its backend's kind.
        let gate = d.dispatch(&argv(&["character", "slot", "set", "house", "gate", "nagaya/gemma"]), &c).await;
        assert!(gate.is_ok() && !gate.message().contains("WARN"), "{}", gate.message());
        let council_model = d.dispatch(&argv(&["character", "model", "set", "house", "nagaya/gemma"]), &c).await;
        assert!(council_model.is_ok() && !council_model.message().contains("WARN"), "{}", council_model.message());
    }

    /// A council server serves chat turns, so a `distill` slot may name one,
    /// with the `max_tokens` a distillation sends.
    ///
    /// Falsified by refusing a `distill` slot by its backend's kind.
    #[tokio::test]
    async fn slot_set_takes_a_distill_slot_on_a_council_server() {
        let d = seeded().await;
        let c = test_caller();
        let set = d.dispatch(&argv(&["character", "slot", "set", "house", "distill", "nagaya/gemma", "--max-tokens", "1024"]), &c).await;
        assert!(set.is_ok(), "{}", set.message());
        let db = d.kernel_db().lock();
        let house = db.get_character_by_name("house").unwrap().unwrap();
        let slot = db.get_character_slot(house.principal_id, "distill").unwrap().expect("the slot is written");
        assert_eq!((slot.model.as_str(), slot.max_tokens), ("gemma", Some(1024)));
    }

    #[tokio::test]
    async fn slot_set_refuses_an_unknown_backend_and_an_unknown_character() {
        let d = seeded().await;
        let c = test_caller();
        match d
            .dispatch(
                &argv(&["character", "slot", "set", "house", "distill", "nope/m"]),
                &c,
            )
            .await
        {
            KjResult::Err(msg) => assert!(msg.contains("backend 'nope' not found"), "{msg}"),
            other => panic!("{other:?}"),
        }
        match d
            .dispatch(
                &argv(&["character", "slot", "set", "ghost", "distill", "anthropic/m"]),
                &c,
            )
            .await
        {
            KjResult::Err(msg) => assert!(msg.contains("character 'ghost' not found"), "{msg}"),
            other => panic!("{other:?}"),
        }
    }

    /// `model set` writes the model and invents no value; `show` reads it
    /// back, structured; `unset` removes it and the second `unset` says there
    /// is none.
    #[tokio::test]
    async fn model_set_show_and_unset_round_trip() {
        let d = seeded().await;
        let c = test_caller();
        let none = d.dispatch(&argv(&["character", "model", "show", "house"]), &c).await;
        assert!(none.message().contains("has no model"), "{}", none.message());
        let KjResult::Ok { data: Some(v), .. } = &none else { panic!("{none:?}") };
        assert!(v.is_null(), "no model is null: {v}");

        let set = d
            .dispatch(
                &argv(&[
                    "character", "model", "set", "house", "anthropic/claude-opus-5",
                    "--temperature", "0.3",
                ]),
                &c,
            )
            .await;
        assert!(set.is_ok(), "{}", set.message());
        assert!(set.message().contains("WARN") && set.message().contains("no max_tokens"), "{}", set.message());
        let shown = d.dispatch(&argv(&["character", "model", "show", "house"]), &c).await;
        let KjResult::Ok { data: Some(v), .. } = &shown else { panic!("{shown:?}") };
        assert_eq!((v["backend"].as_str(), v["model"].as_str()), (Some("anthropic"), Some("claude-opus-5")));
        assert_eq!(v["temperature"], 0.3);
        assert!(v["max_tokens"].is_null(), "an unset parameter stays unset: {v}");

        assert!(d.dispatch(&argv(&["character", "model", "unset", "house"]), &c).await.is_ok());
        let again = d.dispatch(&argv(&["character", "model", "unset", "house"]), &c).await;
        assert!(matches!(&again, KjResult::Err(m) if m.contains("has no model")), "{again:?}");
    }

    /// `model set` shares `slot set`'s range checks and refuses an unknown
    /// backend, an unknown character, and a root.
    #[tokio::test]
    async fn model_set_refuses_bad_input_and_a_root() {
        let d = seeded().await;
        let c = test_caller();
        let bad_backend = d
            .dispatch(&argv(&["character", "model", "set", "house", "nope/m"]), &c)
            .await;
        assert!(matches!(&bad_backend, KjResult::Err(m) if m.contains("backend 'nope' not found")), "{bad_backend:?}");
        let ghost = d
            .dispatch(&argv(&["character", "model", "set", "ghost", "anthropic/m"]), &c)
            .await;
        assert!(matches!(&ghost, KjResult::Err(m) if m.contains("character 'ghost' not found")), "{ghost:?}");
        let hot = d
            .dispatch(
                &argv(&[
                    "character", "model", "set", "house", "anthropic/m", "--temperature", "9",
                ]),
                &c,
            )
            .await;
        assert!(matches!(&hot, KjResult::Err(m) if m.contains("temperature")), "{hot:?}");

        d.kernel_db()
            .lock()
            .insert_character(&crate::kernel_db::CharacterRow { rc_dir: Some("person".into()), console_ctx: None, mcp_hub_ctx: None,
                principal_id: kaijutsu_types::PrincipalId::new(),
                name: "boss".into(),
                created_at: 0,
                retired_at: None,
                handoff_ctx: None,
                root_ctx: None,
                root: true,
            })
            .unwrap();
        let refused = d
            .dispatch(&argv(&["character", "model", "set", "boss", "anthropic/m"]), &c)
            .await;
        assert!(matches!(&refused, KjResult::Err(m) if m.contains("a root runs no model")), "{refused:?}");
    }

    /// A character with a model cannot become a root, since a root runs no
    /// model; unsetting the model first is the way through.
    #[tokio::test]
    async fn set_root_refuses_a_character_with_a_model() {
        let d = seeded().await;
        let c = test_caller();
        let set = d
            .dispatch(&argv(&["character", "model", "set", "house", "anthropic/m"]), &c)
            .await;
        assert!(set.is_ok(), "{set:?}");
        let refused = d.dispatch(&argv(&["character", "set", "house", "--root"]), &c).await;
        assert!(
            matches!(&refused, KjResult::Err(m) if m.contains("a root runs no model") && m.contains("kj character model unset house")),
            "{refused:?}"
        );
        let sheet = d.kernel_db().lock().get_character_by_name("house").unwrap().unwrap();
        assert!(!sheet.root, "a refused set --root leaves the flag alone");
    }

    /// `create --model <backend>/<model>` writes the character and its model; a
    /// bad backend leaves no character; a value with no `/` refuses; a root
    /// refuses it.
    #[tokio::test]
    async fn create_with_a_model_is_all_or_nothing() {
        let d = seeded().await;
        let c = test_caller();
        let made = d
            .dispatch(&argv(&["character", "create", "worker", "--rc", "coder", "--model", "anthropic/claude-opus-5"]), &c)
            .await;
        assert!(made.is_ok(), "{}", made.message());
        {
            let db = d.kernel_db().lock();
            let worker = db.get_character_by_name("worker").unwrap().unwrap();
            assert_eq!(db.get_character_model(worker.principal_id).unwrap().unwrap().model, "claude-opus-5");
        }

        let bad = d
            .dispatch(&argv(&["character", "create", "ghost", "--rc", "coder", "--model", "nope/m"]), &c)
            .await;
        assert!(matches!(&bad, KjResult::Err(m) if m.contains("backend 'nope' not found")), "{bad:?}");
        assert!(d.kernel_db().lock().get_character_by_name("ghost").unwrap().is_none(), "a bad backend leaves no character");

        let half = d.dispatch(&argv(&["character", "create", "half", "--rc", "coder", "--model", "anthropic"]), &c).await;
        assert!(matches!(&half, KjResult::Err(m) if m.contains("<backend>/<model>")), "a value with no '/': {half:?}");
        assert!(d.kernel_db().lock().get_character_by_name("half").unwrap().is_none());

        let root = d
            .dispatch(&argv(&["character", "create", "boss", "--root", "--model", "anthropic/m"]), &c)
            .await;
        assert!(matches!(&root, KjResult::Err(m) if m.contains("a root runs no model")), "{root:?}");
        assert!(d.kernel_db().lock().get_character_by_name("boss").unwrap().is_none());
    }

    /// The value splits at the FIRST `/`, so a model id that holds a `/` stays
    /// whole: `anthropic/zai-org/GLM-5.3` is backend `anthropic`, model
    /// `zai-org/GLM-5.3`, for `create`, `model set` and `slot set`. Falsified
    /// by splitting at the last `/`.
    #[tokio::test]
    async fn a_model_id_may_hold_a_slash() {
        let d = seeded().await;
        let c = test_caller();
        let set = d.dispatch(&argv(&["character", "model", "set", "house", "anthropic/zai-org/GLM-5.3", "--max-tokens", "4096"]), &c).await;
        assert!(set.is_ok(), "{}", set.message());
        let slot = d.dispatch(&argv(&["character", "slot", "set", "house", "gate", "nagaya/zai-org/GLM-5.3"]), &c).await;
        assert!(slot.is_ok(), "{}", slot.message());
        let made = d.dispatch(&argv(&["character", "create", "glm", "--rc", "coder", "--model", "anthropic/zai-org/GLM-5.3", "--max-tokens", "4096"]), &c).await;
        assert!(made.is_ok(), "{}", made.message());
        let db = d.kernel_db().lock();
        for name in ["house", "glm"] {
            let sheet = db.get_character_by_name(name).unwrap().unwrap();
            let model = db.get_character_model(sheet.principal_id).unwrap().unwrap();
            assert_eq!(model.model, "zai-org/GLM-5.3", "{name}");
        }
        let house = db.get_character_by_name("house").unwrap().unwrap();
        assert_eq!(db.get_character_slot(house.principal_id, "gate").unwrap().unwrap().model, "zai-org/GLM-5.3");
    }

    /// A value with no `/`, or an empty half, refuses with the form and names
    /// where the parts are listed; nothing is written.
    #[tokio::test]
    async fn a_model_value_without_a_slash_refuses_with_the_form() {
        let d = seeded().await;
        let c = test_caller();
        for value in ["anthropic", "/claude", "anthropic/"] {
            for verb in [
                vec!["character", "model", "set", "house", value],
                vec!["character", "slot", "set", "house", "distill", value],
                vec!["character", "create", "fresh", "--rc", "coder", "--model", value],
            ] {
                match d.dispatch(&argv(&verb), &c).await {
                    KjResult::Err(m) => {
                        assert!(m.contains("<backend>/<model>"), "{verb:?}: {m}");
                        assert!(m.contains("kj backend list") && m.contains("kj models"), "{verb:?}: {m}");
                    }
                    other => panic!("{verb:?} must refuse, got {other:?}"),
                }
            }
        }
        let db = d.kernel_db().lock();
        let house = db.get_character_by_name("house").unwrap().unwrap();
        assert!(db.get_character_model(house.principal_id).unwrap().is_none());
        assert!(db.get_character_by_name("fresh").unwrap().is_none());
    }

    /// `kj character show` reports the model and keeps the slots apart as
    /// overrides; its data carries `model` as an object or null.
    #[tokio::test]
    async fn show_reports_the_model_and_the_slots() {
        let d = seeded().await;
        let c = test_caller();
        let bare = d.dispatch(&argv(&["character", "show", "house"]), &c).await;
        let KjResult::Ok { data: Some(v), .. } = &bare else { panic!("{bare:?}") };
        assert!(v["model"].is_null(), "{v}");

        d.dispatch(&argv(&["character", "model", "set", "house", "anthropic/claude-opus-5"]), &c).await;
        d.dispatch(&argv(&["character", "slot", "set", "house", "gate", "nagaya/g"]), &c).await;
        let shown = d.dispatch(&argv(&["character", "show", "house"]), &c).await;
        let KjResult::Ok { data: Some(v), message, .. } = &shown else { panic!("{shown:?}") };
        assert_eq!(v["model"]["model"], "claude-opus-5");
        assert_eq!(v["slots"][0]["role"], "gate");
        assert!(message.contains("Model:") && message.contains("anthropic/claude-opus-5"), "{message}");
        assert!(message.contains("gate"), "{message}");
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
                &argv(&["character", "slot", "set", "house", "distill", "anthropic/m"]),
                &c,
            )
            .await
        {
            KjResult::Err(msg) => assert!(msg.contains("retired"), "{msg}"),
            other => panic!("{other:?}"),
        }
    }

    /// A character's slots are `distill` and `gate`. `slot set` and `slot
    /// unset` refuse any other role and name the verb that holds it: `kj
    /// character model set` for `model`, the judge character for
    /// `judge`. A `gate` slot names a council server, so a backend of
    /// another kind refuses. Nothing is written.
    ///
    /// Falsified by accepting a role as a forward declaration, or a gate on
    /// a chat backend.
    #[tokio::test]
    async fn slot_set_takes_distill_and_gate_only() {
        let d = seeded().await;
        let c = test_caller();
        for verb in ["set", "unset"] {
            for role in ["coder", "choreographer", "model"] {
                let mut words = vec!["character", "slot", verb, "house", role];
                if verb == "set" {
                    words.push("deepseek/deepseek-v4-pro");
                }
                let r = d.dispatch(&argv(&words), &c).await;
                assert!(
                    matches!(&r, KjResult::Err(m) if m.contains("a character has one model")
                        && m.contains("kj character model set house <backend>/<model>")),
                    "{verb} {role}: {r:?}"
                );
            }
            let mut words = vec!["character", "slot", verb, "house", "judge"];
            if verb == "set" {
                words.push("nagaya/gemma");
            }
            let r = d.dispatch(&argv(&words), &c).await;
            assert!(
                matches!(&r, KjResult::Err(m) if m.contains("the judge is a character") && m.contains("kj character model set judge")),
                "{verb} judge: {r:?}"
            );
        }
        let gate = d.dispatch(&argv(&["character", "slot", "set", "house", "gate", "anthropic/claude-opus-5"]), &c).await;
        assert!(matches!(&gate, KjResult::Err(m) if m.contains("council server") && m.contains("not council")), "{gate:?}");
        let db = d.kernel_db().lock();
        let house = db.get_character_by_name("house").unwrap().unwrap();
        assert!(db.list_character_slots(house.principal_id).unwrap().is_empty(), "nothing was written");
    }

    #[tokio::test]
    async fn slot_tunables_are_range_checked_loudly() {
        let d = seeded().await;
        match d
            .dispatch(
                &argv(&[
                    "character", "slot", "set", "house", "distill", "anthropic/claude-opus-5", "--top-p", "1.5",
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
                    "character", "slot", "set", "house", "distill", "anthropic/claude-opus-5", "--max-tokens", "16384", "--thinking-budget", "20000",
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
                "character", "slot", "set", "house", "distill", "anthropic/claude-opus-5", "--thinking-budget", budget,
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
                    "character", "slot", "set", "house", "distill", "anthropic/claude-opus-5", "--max-tokens", "1000", "--thinking-budget", "1500",
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
                "character", "slot", "set", "house", "distill", "anthropic/claude-opus-5", "--max-tokens", "16384", "--thinking-budget", "20000",
            ]),
            &test_caller(),
        )
        .await;
        let db = d.kernel_db().lock();
        let house = db.get_character_by_name("house").unwrap().unwrap();
        let slot = db.get_character_slot(house.principal_id, "distill").unwrap().expect("write must still land");
        assert_eq!(slot.thinking_budget, Some(20000));
    }

    #[tokio::test]
    async fn slot_set_does_not_warn_when_the_pair_is_sane() {
        let d = seeded().await;
        let r = d
            .dispatch(
                &argv(&[
                    "character", "slot", "set", "house", "distill", "anthropic/claude-opus-5", "--max-tokens", "4096", "--thinking-budget", "1000",
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
        for (role, model) in [("distill", "anthropic/claude-opus-5"), ("gate", "nagaya/gemma")] {
            let r = d
                .dispatch(&argv(&["character", "slot", "set", "house", role, model, "--max-tokens", "4096"]), &c)
                .await;
            assert!(r.is_ok(), "{}", r.message());
        }
        let removed = d.dispatch(&argv(&["character", "slot", "unset", "house", "distill"]), &c).await;
        assert!(removed.is_ok(), "{}", removed.message());
        let again = d.dispatch(&argv(&["character", "slot", "unset", "house", "distill"]), &c).await;
        assert!(!again.is_ok() && again.message().contains("no slot for 'distill'"), "{}", again.message());
        let listed = d.dispatch(&argv(&["character", "slot", "list", "house"]), &c).await;
        assert!(listed.message().contains("gate") && !listed.message().contains("distill"), "{}", listed.message());
    }

    /// The old verb is gone, not aliased.
    #[tokio::test]
    async fn kj_cast_is_gone() {
        let d = seeded().await;
        let r = d.dispatch(&argv(&["cast", "list"]), &test_caller()).await;
        assert!(!r.is_ok(), "`kj cast` must be unknown: {}", r.message());
    }
}
