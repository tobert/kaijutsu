//! Model tools for reading and adding notes on a track's board.

use std::sync::Weak;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::kj::{KjCaller, KjResult};
use crate::mcp::{Broker, CallContext, InstanceId, KernelCallParams, KernelTool, KernelToolResult, McpError, McpResult};
use super::super::schema::tool_input_schema;
use super::super::server_like::{McpServerLike, ServerNotification};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NotesParams {
    /// Track name. Omit to use your attached track. Cannot be combined with topic.
    track: Option<String>,
    /// Board topic to read, such as @track/band/handoff or chomosuke/memory/vibe. Omit for your track notes.
    topic: Option<String>,
    /// Return notes older than this number. Cannot be combined with after.
    before: Option<u64>,
    /// Return notes newer than this number, oldest first. Cannot be combined with before.
    after: Option<u64>,
    /// Maximum number of notes, from 1 to 4. Defaults to 4.
    limit: Option<u64>,
    /// Read one note by number. Use with offset to continue a long note.
    number: Option<u64>,
    /// Character offset in the selected note. Each body is limited to 1000 characters. Defaults to 0.
    offset: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddNoteParams {
    /// Note text.
    body: String,
    /// Track name. Omit to use your attached track.
    track: Option<String>,
    /// Tick associated with the note.
    at: Option<u64>,
    /// Phrase start tick associated with the note.
    phrase_start: Option<u64>,
    /// Score hash associated with the note.
    score: Option<String>,
}

pub struct BuiltinNotesServer {
    instance: InstanceId,
    broker: Weak<Broker>,
    notif_tx: broadcast::Sender<ServerNotification>,
}

impl BuiltinNotesServer {
    pub const INSTANCE: &'static str = "builtin.notes";
    pub fn new(broker: Weak<Broker>) -> Self {
        let (notif_tx, _) = broadcast::channel(1);
        Self { instance: InstanceId::new(Self::INSTANCE), broker, notif_tx }
    }
}

fn truncate_post(post: &mut serde_json::Value, offset: usize) {
    if let Some(body) = post.get("body").and_then(|v| v.as_str()) {
        let count = body.chars().count();
        let start = offset.min(count);
        let end = start.saturating_add(1000).min(count);
        let chunk: String = body.chars().skip(start).take(end - start).collect();
        post["body"] = chunk.into();
        if end < count { post["next_offset"] = end.into(); }
    }
}

#[async_trait]
impl McpServerLike for BuiltinNotesServer {
    fn instance_id(&self) -> &InstanceId { &self.instance }
    async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
        Ok(vec![
            KernelTool {
                instance: self.instance.clone(),
                name: "notes".into(),
                description: Some("Read your track notes or a board topic. Results are newest first, or oldest first with after. Page older notes with before; continue a long note with number and offset. At most four bodies of 1000 characters are returned.".into()),
                input_schema: tool_input_schema::<NotesParams>(),
            },
            KernelTool {
                instance: self.instance.clone(),
                name: "add_note".into(),
                description: Some("Add a note to your own board for a track. Omit track to use your attached track.".into()),
                input_schema: tool_input_schema::<AddNoteParams>(),
            },
        ])
    }
    async fn call_tool(&self, params: KernelCallParams, ctx: &CallContext, cancel: CancellationToken) -> McpResult<KernelToolResult> {
        let mut argv = vec!["board".to_string()];
        let mut selected = None;
        let mut offset = 0;
        match params.tool.as_str() {
            "notes" => {
                let p: NotesParams = serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;
                if p.limit.is_some_and(|n| !(1..=4).contains(&n))
                    || (p.number.is_none() && p.offset.is_some())
                    || (p.number.is_some() && (p.before.is_some() || p.after.is_some() || p.limit.is_some()))
                    || (p.before.is_some() && p.after.is_some())
                {
                    return Ok(KernelToolResult::error_text("notes: use limit 1 through 4; before and after cannot be combined; number and offset cannot be combined with pagination"));
                }
                if p.topic.is_some() && p.track.is_some() {
                    return Ok(KernelToolResult::error_text("notes: choose topic or track"));
                }
                argv.push("notes".into());
                if let Some(topic) = p.topic { argv.extend(["--topic".into(), topic]); }
                if let Some(track) = p.track { argv.extend(["--track".into(), track]); }
                if let Some(n) = p.number {
                    let Some(before) = n.checked_add(1) else {
                        return Ok(KernelToolResult::error_text("notes: invalid note number"));
                    };
                    argv.extend(["--before".into(), before.to_string(), "--limit".into(), "1".into()]);
                    selected = Some(n);
                } else {
                    if let Some(n) = p.before { argv.extend(["--before".into(), n.to_string()]); }
                    if let Some(n) = p.after { argv.extend(["--after".into(), n.to_string()]); }
                    argv.extend(["--limit".into(), p.limit.unwrap_or(4).to_string()]);
                }
                offset = p.offset.unwrap_or(0);
            }
            "add_note" => {
                let p: AddNoteParams = serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;
                argv.push("add-note".into());
                if let Some(track) = p.track { argv.extend(["--track".into(), track]); }
                if let Some(n) = p.at { argv.extend(["--at".into(), n.to_string()]); }
                if let Some(n) = p.phrase_start { argv.extend(["--phrase-start".into(), n.to_string()]); }
                if let Some(score) = p.score { argv.extend(["--score".into(), score]); }
                argv.extend(["--".into(), p.body]);
            }
            _ => return Err(McpError::ToolNotFound { instance: self.instance.clone(), tool: params.tool }),
        }
        let broker = self.broker.upgrade().ok_or_else(|| McpError::Protocol("notes: broker unavailable".into()))?;
        let dispatcher = broker.kj_dispatcher().await.ok_or_else(|| McpError::Protocol("notes: dispatcher unavailable".into()))?;
        let caller = KjCaller { principal_id: ctx.principal_id, actor_id: ctx.actor_id, reviewer_id: ctx.reviewer_id, context_id: Some(ctx.context_id), session_id: ctx.session_id, confirmed: false, rc_depth: 0, privileged: false, cancel };
        match dispatcher.dispatch(&argv, &caller).await {
            KjResult::Ok { message, data, .. } if params.tool == "notes" => {
                let Some(mut data) = data else { return Err(McpError::Protocol("notes: missing structured result".into())) };
                let Some(posts) = data.get_mut("posts").and_then(|p| p.as_array_mut()) else { return Err(McpError::Protocol("notes: invalid structured result".into())) };
                if let Some(n) = selected {
                    posts.retain(|p| p.get("number").and_then(|v| v.as_u64()) == Some(n));
                }
                for post in posts { truncate_post(post, offset); }
                let _ = message;
                Ok(KernelToolResult::text(data.to_string()))
            }
            KjResult::Ok { message, .. } => Ok(KernelToolResult::text(message)),
            KjResult::Err(message) => Ok(KernelToolResult::error_text(message)),
            other => Err(McpError::Protocol(format!("notes: unexpected command result: {other:?}"))),
        }
    }
    fn notifications(&self) -> broadcast::Receiver<ServerNotification> { self.notif_tx.subscribe() }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn broker_notes_use_performer_and_attached_track() {
        use crate::kj::test_helpers::{register_context, seed_test_performer, test_dispatcher};
        use crate::kernel_db::{PersistedAttachment, PersistedTrack};
        use crate::mcp::{Capability, ContextToolBinding, InstancePolicy, ToolContent};
        let dispatcher = std::sync::Arc::new(test_dispatcher().await);
        let performer = seed_test_performer(&dispatcher.kernel_db().lock());
        let requester = kaijutsu_types::PrincipalId::new();
        let context = register_context(&dispatcher, Some("notes-tool"), None, requester);
        {
            let db = dispatcher.kernel_db().lock();
            db.update_performer(context, Some(performer)).unwrap();
            db.upsert_track(&PersistedTrack { track_id: "band".into(), period_ms: 500, beats_per_phrase: 16, playhead_tick: None, playing: false, score_context_id: Some(context), clock_kind: "system".into() }).unwrap();
            db.upsert_attachment(&PersistedAttachment { track_id: "band".into(), context_id: context, wakeup_every: 128, rotate_every_phrases: None, ooda_armed: true }).unwrap();
        }
        dispatcher.kernel().arm_track_timeline(kaijutsu_types::TrackId::new("band").unwrap(), kaijutsu_hyoushigi::TickClock::default(), kaijutsu_types::Tick::new(10));
        let broker = dispatcher.kernel().broker();
        broker.set_kj_dispatcher(&dispatcher).await;
        broker.register(std::sync::Arc::new(BuiltinNotesServer::new(std::sync::Arc::downgrade(broker))), InstancePolicy::for_kernel(dispatcher.kernel())).await.unwrap();
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Instance(InstanceId::new(BuiltinNotesServer::INSTANCE)));
        dispatcher.kernel_db().lock().upsert_context_binding(context, &binding).unwrap();
        broker.set_binding(context, binding).await.unwrap();
        let mut ctx = CallContext::test();
        ctx.context_id = context;
        ctx.principal_id = requester;
        ctx.actor_id = performer;
        let invoke = |name: &str, arguments| KernelCallParams { instance: InstanceId::new(BuiltinNotesServer::INSTANCE), tool: name.into(), arguments };
        let written = broker.call_tool(invoke("add_note", serde_json::json!({"body": "hello"})), &ctx, CancellationToken::new()).await.unwrap();
        assert!(!written.is_error, "{:?}", written.content);
        let read = broker.call_tool(invoke("notes", serde_json::json!({})), &ctx, CancellationToken::new()).await.unwrap();
        assert!(!read.is_error, "{:?}", read.content);
        let [ToolContent::Text(text)] = read.content.as_slice() else { panic!("{:?}", read.content) };
        let data: serde_json::Value = serde_json::from_str(text).unwrap();
        assert!(data["posts"][0]["body"].as_str().unwrap().contains("hello"), "{data}");
        assert_eq!(data["posts"][0]["author_id"], performer.to_hex());
        let caller = KjCaller { principal_id: requester, actor_id: performer, reviewer_id: None, context_id: Some(context), session_id: ctx.session_id, confirmed: false, rc_depth: 0, privileged: false, cancel: CancellationToken::new() };
        let shared = dispatcher.dispatch(&["board".into(), "post".into(), "@track/band".into(), "--title".into(), "handoff".into(), "next player: leave space".into()], &caller).await;
        assert!(shared.is_ok(), "{}", shared.message());
        let newcomer = kaijutsu_types::PrincipalId::new();
        let newcomer_context = register_context(&dispatcher, Some("newcomer"), None, newcomer);
        let mut newcomer_binding = ContextToolBinding::new();
        newcomer_binding.grant(Capability::Instance(InstanceId::new(BuiltinNotesServer::INSTANCE)));
        broker.set_binding(newcomer_context, newcomer_binding).await.unwrap();
        let mut newcomer_call = CallContext::test();
        newcomer_call.context_id = newcomer_context;
        newcomer_call.principal_id = newcomer;
        newcomer_call.actor_id = newcomer;
        let handoff = broker.call_tool(invoke("notes", serde_json::json!({"topic": "@track/band/handoff"})), &newcomer_call, CancellationToken::new()).await.unwrap();
        assert!(!handoff.is_error, "{:?}", handoff.content);
        let [ToolContent::Text(handoff_text)] = handoff.content.as_slice() else { panic!("{:?}", handoff.content) };
        let handoff_json: serde_json::Value = serde_json::from_str(handoff_text).unwrap();
        assert!(handoff_json["posts"][0]["body"].as_str().unwrap().contains("leave space"), "{handoff_json}");
        for body in ["second", "third"] {
            let reply = dispatcher.dispatch(&["board".into(), "reply".into(), "@track/band/handoff".into(), body.into()], &caller).await;
            assert!(reply.is_ok(), "{}", reply.message());
        }
        let page = broker.call_tool(invoke("notes", serde_json::json!({"topic": "@track/band/handoff", "after": 1, "limit": 1})), &newcomer_call, CancellationToken::new()).await.unwrap();
        let [ToolContent::Text(page_text)] = page.content.as_slice() else { panic!("{:?}", page.content) };
        let page_json: serde_json::Value = serde_json::from_str(page_text).unwrap();
        assert_eq!(page_json["posts"][0]["number"], 2);
        let conflict = broker.call_tool(invoke("notes", serde_json::json!({"topic": "@track/band/handoff", "after": 1, "before": 3})), &newcomer_call, CancellationToken::new()).await.unwrap();
        assert!(conflict.is_error, "{:?}", conflict.content);
        let owner = dispatcher.kernel_db().lock().get_character(performer).unwrap().unwrap().name;
        let result = dispatcher.dispatch(&["board".into(), "set".into(), format!("{owner}/music-notes"), "--policy".into(), "locked".into()], &caller).await;
        assert!(result.is_ok(), "{}", result.message());
        let denied = broker.call_tool(invoke("add_note", serde_json::json!({"body": "blocked"})), &ctx, CancellationToken::new()).await.unwrap();
        assert!(denied.is_error, "{:?}", denied.content);

    }

    #[test]
    fn long_unicode_note_can_be_read_in_chunks() {
        let original = "音".repeat(1201);
        let mut first = serde_json::json!({"body": original});
        truncate_post(&mut first, 0);
        assert_eq!(first["body"].as_str().unwrap().chars().count(), 1000);
        assert_eq!(first["next_offset"], 1000);
        let mut rest = serde_json::json!({"body": original});
        truncate_post(&mut rest, 1000);
        assert_eq!(rest["body"].as_str().unwrap().chars().count(), 201);
        assert!(rest.get("next_offset").is_none());
    }

    #[tokio::test]
    async fn notes_tools_describe_their_inputs() {
        let broker = std::sync::Arc::new(Broker::new());
        let server = BuiltinNotesServer::new(std::sync::Arc::downgrade(&broker));
        let tools = server.list_tools(&CallContext::test()).await.unwrap();
        assert_eq!(tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["notes", "add_note"]);
        let schemas = format!("{:?}", tools.iter().map(|t| &t.input_schema).collect::<Vec<_>>());
        println!("{schemas}");
        assert!(schemas.contains("body") && schemas.contains("after") && schemas.contains("offset"), "{schemas}");
    }
}
