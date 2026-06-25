//! Peer systems for Bevy.

use super::plugin::PeerInvocationChannel;
use bevy::prelude::*;

use crate::ui::drift::DriftState;
use crate::view::components::ContextSwitchRequested;
use crate::view::document::DocumentCache;
use crate::view::editor::EditorOpenRequested;

/// Poll the peer invocation channel and dispatch actions.
///
/// Invocations arrive from the kernel via `PeerCommands` callback →
/// mpsc channel → this system. Each invocation carries a oneshot reply.
pub fn poll_peer_invocations(
    channel: Res<PeerInvocationChannel>,
    doc_cache: Res<DocumentCache>,
    drift: Res<DriftState>,
    mut switch_writer: MessageWriter<ContextSwitchRequested>,
    mut editor_writer: MessageWriter<EditorOpenRequested>,
) {
    let rx = match channel.rx.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            warn!("Peer invocation channel mutex was poisoned, recovering");
            poisoned.into_inner()
        }
    };
    while let Ok(invocation) = rx.try_recv() {
        let result = dispatch_peer_action(
            &invocation.action,
            &invocation.params,
            &doc_cache,
            &drift,
            &mut switch_writer,
            &mut editor_writer,
        );
        let _ = invocation.reply.send(result);
    }
}

fn dispatch_peer_action(
    action: &str,
    params: &[u8],
    doc_cache: &DocumentCache,
    drift: &DriftState,
    switch_writer: &mut MessageWriter<ContextSwitchRequested>,
    editor_writer: &mut MessageWriter<EditorOpenRequested>,
) -> Result<Vec<u8>, String> {
    match action {
        "switch_context" => {
            #[derive(serde::Deserialize)]
            struct Params {
                context_id: String,
            }
            let p: Params =
                serde_json::from_slice(params).map_err(|e| format!("invalid params: {e}"))?;

            let ctx_id = kaijutsu_types::ContextId::parse(&p.context_id)
                .or_else(|_| {
                    let items = drift.contexts.iter().map(|c| {
                        let label = if c.label.is_empty() {
                            None
                        } else {
                            Some(c.label.as_str())
                        };
                        (c.id, label)
                    });
                    kaijutsu_types::resolve_context_prefix(items, &p.context_id)
                })
                .map_err(|e| format!("cannot resolve context_id: {e}"))?;

            let was_cached = doc_cache.contains(ctx_id);
            switch_writer.write(ContextSwitchRequested { context_id: ctx_id });

            serde_json::to_vec(&serde_json::json!({
                "context_id": ctx_id.to_string(),
                "was_cached": was_cached,
            }))
            .map_err(|e| format!("serialize: {e}"))
        }

        "open_editor" => {
            // The signal is self-contained: session id + path + the initial
            // editor snapshot (text/cursor/mode/dirty), so the renderer can draw
            // immediately. Shape mirrors the kernel's `EditorState::to_json` + path.
            #[derive(serde::Deserialize)]
            struct Params {
                session: u64,
                path: String,
                text: String,
                cursor: u64,
                mode: Option<String>,
                dirty: bool,
                #[serde(default)]
                command_line: Option<String>,
            }
            let p: Params =
                serde_json::from_slice(params).map_err(|e| format!("invalid params: {e}"))?;

            // Drive the editor screen from its own landing handler
            // (`view::editor::handle_editor_open`), mirroring how a context
            // switch drives Conversation — the editor owns its transition.
            editor_writer.write(EditorOpenRequested {
                path: p.path.clone(),
                state: kaijutsu_client::EditorState {
                    session: p.session,
                    text: p.text,
                    cursor: p.cursor,
                    mode: p.mode,
                    dirty: p.dirty,
                    command_line: p.command_line,
                },
            });

            serde_json::to_vec(&serde_json::json!({
                "session": p.session,
                "path": p.path,
            }))
            .map_err(|e| format!("serialize: {e}"))
        }

        "active_context" => {
            let active = doc_cache.active_id();
            let mru: Vec<_> = doc_cache
                .mru_ids()
                .iter()
                .map(|&id| {
                    let name = doc_cache
                        .get(id)
                        .map(|d| d.context_name.clone())
                        .unwrap_or_default();
                    serde_json::json!({
                        "context_id": id.to_string(),
                        "name": name,
                        "is_active": Some(id) == active,
                    })
                })
                .collect();

            serde_json::to_vec(&serde_json::json!({
                "active_id": active.map(|id| id.to_string()),
                "mru": mru,
            }))
            .map_err(|e| format!("serialize: {e}"))
        }

        _ => Err(format!("unknown action: {action}")),
    }
}
