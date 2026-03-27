//! Custom BRP methods for kaijutsu context operations.
//!
//! Registered at runtime via `RemoteMethods` so agents can navigate
//! contexts by ID through the Bevy Remote Protocol.

use bevy::prelude::*;
use bevy_remote::{error_codes, BrpError, BrpResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::components::ContextSwitchRequested;
use super::document::DocumentCache;
use crate::ui::drift::DriftState;
use kaijutsu_types::ContextId;

/// Method name for switching to a context by ID.
pub const SWITCH_CONTEXT_METHOD: &str = "kaijutsu/switch_context";

/// Method name for querying the active context.
pub const ACTIVE_CONTEXT_METHOD: &str = "kaijutsu/active_context";

// ── Request / Response types ─────────────────────────────────────────

#[derive(Deserialize)]
struct SwitchContextParams {
    /// UUID string (hyphenated or plain hex).
    context_id: String,
}

#[derive(Serialize)]
struct SwitchContextResponse {
    context_id: String,
    was_cached: bool,
}

#[derive(Serialize)]
struct ActiveContextResponse {
    /// Currently rendered context (null if none).
    active_id: Option<String>,
    /// Most-recently-used context IDs, most recent first.
    mru: Vec<MruEntry>,
}

#[derive(Serialize)]
struct MruEntry {
    context_id: String,
    name: String,
    is_active: bool,
}

// ── Handlers ─────────────────────────────────────────────────────────

/// BRP handler: `kaijutsu/switch_context`
///
/// Params: `{ "context_id": "<uuid-or-prefix>" }`
/// Accepts full UUIDs (hyphenated or plain hex) or short hex prefixes.
/// Resolves against the document cache's known contexts.
/// Writes a `ContextSwitchRequested` message, which the existing
/// `handle_context_switch` system picks up next frame.
pub fn handle_switch_context(
    In(params): In<Option<Value>>,
    doc_cache: Res<DocumentCache>,
    drift: Res<DriftState>,
    mut writer: MessageWriter<ContextSwitchRequested>,
) -> BrpResult {
    let SwitchContextParams { context_id: raw } = parse_params(params)?;

    // Try full UUID parse first, fall back to prefix resolution against
    // all known contexts (drift state from the kernel).
    let ctx_id = match ContextId::parse(&raw) {
        Ok(id) => id,
        Err(_) => {
            let items = drift.contexts.iter().map(|c| {
                let label = if c.label.is_empty() {
                    None
                } else {
                    Some(c.label.as_str())
                };
                (c.id, label)
            });
            kaijutsu_types::resolve_context_prefix(items, &raw).map_err(|e| BrpError {
                code: error_codes::INVALID_PARAMS,
                message: format!("Cannot resolve context_id: {e}"),
                data: None,
            })?
        }
    };

    let was_cached = doc_cache.contains(ctx_id);

    writer.write(ContextSwitchRequested { context_id: ctx_id });

    serde_json::to_value(SwitchContextResponse {
        context_id: ctx_id.to_string(),
        was_cached,
    })
    .map_err(BrpError::internal)
}

/// BRP handler: `kaijutsu/active_context`
///
/// No params. Returns the active context ID and MRU list.
pub fn handle_active_context(
    In(_params): In<Option<Value>>,
    doc_cache: Res<DocumentCache>,
) -> BrpResult {
    let active = doc_cache.active_id();

    let mru: Vec<MruEntry> = doc_cache
        .mru_ids()
        .iter()
        .map(|&id| {
            let name = doc_cache
                .get(id)
                .map(|d| d.context_name.clone())
                .unwrap_or_default();
            MruEntry {
                context_id: id.to_string(),
                name,
                is_active: Some(id) == active,
            }
        })
        .collect();

    serde_json::to_value(ActiveContextResponse {
        active_id: active.map(|id| id.to_string()),
        mru,
    })
    .map_err(BrpError::internal)
}

// ── Helpers ──────────────────────────────────────────────────────────

fn parse_params<T: for<'de> Deserialize<'de>>(params: Option<Value>) -> Result<T, BrpError> {
    match params {
        Some(v) => serde_json::from_value(v).map_err(|e| BrpError {
            code: error_codes::INVALID_PARAMS,
            message: e.to_string(),
            data: None,
        }),
        None => Err(BrpError {
            code: error_codes::INVALID_PARAMS,
            message: "Params not provided".into(),
            data: None,
        }),
    }
}
