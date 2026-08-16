//! `kaijutsu-acp` — an Agent Client Protocol (ACP v1) bridge onto a kaijutsu
//! kernel.
//!
//! ```text
//! ACP client (toad / Zed / Happy / …)
//!    │  JSON-RPC 2.0 over stdio (ACP v1)
//! kaijutsu-acp
//!    │  kaijutsu-client (SSH + Cap'n Proto), --connect
//! kaijutsu-server / kernel
//! ```
//!
//! Same architecture as `kaijutsu-mcp`, and for the same reason: the kernel
//! already speaks a rich protocol over SSH, so a bridge is a translation layer
//! and nothing else. No RPC code is forked — `ActorHandle` is consumed as-is.
//!
//! **Which way does the agency point?** In MCP, kaijutsu is a *tool* another
//! agent calls. In ACP, kaijutsu is the *agent* and the ACP client is the
//! human's seat. So a session's context is created with `context_type=coder`
//! by default — the model-facing stance bundle — not `mcp`.
//!
//! ## Version stance
//!
//! ACP **v1 only**. The `agent-client-protocol` crate's `2.0.0` is crate
//! SemVer; protocol v2 is a draft behind `unstable_protocol_v2`, which stays
//! off. See docs/acp.md.

pub mod bridge;
pub mod permission;
pub mod rank;
pub mod session;
pub mod update;

use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate,
    DeleteSessionRequest, DeleteSessionResponse, Error, Implementation,
    InitializeRequest, InitializeResponse,
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, PromptCapabilities, PromptRequest, PromptResponse,
    ResumeSessionRequest, ResumeSessionResponse, SessionCapabilities, SessionId,
    SessionDeleteCapabilities, SessionListCapabilities, SessionNotification,
    SessionResumeCapabilities, SessionUpdate, UnstructuredCommandInput,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::ContentBlock;
use agent_client_protocol::{Agent, Client, ConnectionTo, Responder, Stdio};
use parking_lot::Mutex;

use bridge::{KernelBridge, new_session_label};
use session::{Session, SessionRegistry, TurnOutcome, run_pump, run_turn, settle_command_delivery};
use update::UpdateMapper;

/// Shared state behind every ACP handler.
pub struct AcpBridge {
    pub kernel: KernelBridge,
    pub sessions: SessionRegistry,
    client_info: Mutex<Option<Implementation>>,
}

impl AcpBridge {
    pub fn new(kernel: KernelBridge) -> Self {
        Self {
            kernel,
            sessions: SessionRegistry::default(),
            client_info: Mutex::new(None),
        }
    }

    pub fn client_info(&self) -> Option<Implementation> {
        self.client_info.lock().clone()
    }
}

/// What this agent tells a client it can do.
///
/// `load_session` and `session/list` are both on, because both map onto
/// something kaijutsu genuinely has (a durable context, and the time well's
/// rank). Prompt capabilities are all `false`: this bridge forwards text and
/// nothing else today — an image in a prompt would be silently dropped, and
/// advertising a capability we drop is worse than declining it.
fn agent_capabilities() -> AgentCapabilities {
    AgentCapabilities::new()
        .load_session(true)
        .prompt_capabilities(
            PromptCapabilities::new()
                .image(false)
                .audio(false)
                .embedded_context(false),
        )
        .session_capabilities(
            SessionCapabilities::new()
                .list(SessionListCapabilities::new())
                .delete(SessionDeleteCapabilities::new())
                .resume(SessionResumeCapabilities::new()),
        )
}

/// Flatten an ACP prompt into the text kaijutsu's input doc accepts.
///
/// Non-text blocks are named rather than dropped in silence — a resource link
/// the model cannot see should at least be visible in the transcript as
/// something that was sent and not understood.
pub fn prompt_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        let piece = match block {
            ContentBlock::Text(t) => t.text.clone(),
            ContentBlock::ResourceLink(l) => format!("[resource_link {} {}]", l.name, l.uri),
            ContentBlock::Image(_) => "[image omitted: this bridge forwards text only]".to_string(),
            ContentBlock::Audio(_) => "[audio omitted: this bridge forwards text only]".to_string(),
            ContentBlock::Resource(_) => {
                "[embedded resource omitted: this bridge forwards text only]".to_string()
            }
            _ => "[unsupported content block]".to_string(),
        };
        if !out.is_empty() && !piece.is_empty() {
            out.push('\n');
        }
        out.push_str(&piece);
    }
    out
}

/// Build and run the ACP agent over stdio until the client disconnects.
///
/// Must be called from inside a `tokio::task::LocalSet` — the Cap'n Proto
/// actor spawned by [`KernelBridge::connect`] is `!Send` internally.
pub async fn serve_stdio(bridge: Arc<AcpBridge>) -> Result<(), Error> {
    let init = Arc::clone(&bridge);
    let new = Arc::clone(&bridge);
    let load = Arc::clone(&bridge);
    let resume = Arc::clone(&bridge);
    let list = Arc::clone(&bridge);
    let delete = Arc::clone(&bridge);
    let prompt = Arc::clone(&bridge);
    let cancel = Arc::clone(&bridge);
    let permission = Arc::clone(&bridge);

    Agent
        .builder()
        .name("kaijutsu-acp")
        .with_spawned(move |cx| async move {
            permission::start_permission_pump(&permission, cx).await;
            Ok(())
        })
        .on_receive_request(
            async move |req: InitializeRequest, responder, cx| {
                handle_initialize(&init, req, responder, cx)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NewSessionRequest, responder, cx| {
                handle_new_session(Arc::clone(&new), req, responder, cx).await
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: LoadSessionRequest, responder, cx| {
                handle_load_session(Arc::clone(&load), req, responder, cx).await
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: ResumeSessionRequest, responder, cx| {
                handle_resume_session(Arc::clone(&resume), req, responder, cx).await
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: ListSessionsRequest, responder, _cx| {
                handle_list_sessions(Arc::clone(&list), req, responder).await
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: DeleteSessionRequest, responder, _cx| {
                handle_delete_session(Arc::clone(&delete), req, responder).await
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest, responder, cx| {
                handle_prompt(Arc::clone(&prompt), req, responder, cx)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notif: agent_client_protocol::schema::v1::CancelNotification, _cx| {
                handle_cancel(Arc::clone(&cancel), notif).await
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
}

/// Negotiate down, never up.
///
/// We answer with the lower of what the client asked for and the highest we
/// implement — v1. A client that asks for v2 (or anything later) gets v1 and
/// speaks v1; claiming a version we do not implement is how a client ends up
/// calling a method that does not exist. See docs/acp.md, "Version stance".
pub fn negotiate_version(client_asked: ProtocolVersion) -> ProtocolVersion {
    if client_asked.as_u16() >= ProtocolVersion::V1.as_u16() {
        ProtocolVersion::V1
    } else {
        client_asked
    }
}

fn handle_initialize(
    bridge: &Arc<AcpBridge>,
    req: InitializeRequest,
    responder: Responder<InitializeResponse>,
    cx: ConnectionTo<Client>,
) -> Result<(), Error> {
    let negotiated = negotiate_version(req.protocol_version);
    let client_info = req.client_info.clone();
    *bridge.client_info.lock() = client_info.clone();
    if let Some(info) = &client_info {
        let kernel = bridge.kernel.clone();
        let info = info.clone();
        cx.spawn(async move {
            match kernel.attach_client_peer(&info.name).await {
                Ok(nick) => tracing::info!(%nick, version = %info.version, "ACP client attached as peer"),
                Err(e) => tracing::warn!(client = %info.name, "ACP client peer attach failed (non-fatal): {e}"),
            }
            Ok(())
        })?;
    }
    tracing::info!(
        client_asked = req.protocol_version.as_u16(),
        negotiated = negotiated.as_u16(),
        client = client_info.as_ref().map(|info| info.name.as_str()),
        "initialize"
    );
    responder.respond(
        InitializeResponse::new(negotiated)
            .agent_capabilities(agent_capabilities())
            .agent_info(Implementation::new(
                "kaijutsu-acp",
                env!("CARGO_PKG_VERSION"),
            )),
    )
}

async fn handle_new_session(
    bridge: Arc<AcpBridge>,
    req: NewSessionRequest,
    responder: Responder<NewSessionResponse>,
    cx: ConnectionTo<Client>,
) -> Result<(), Error> {
    warn_ignored_mcp_servers(req.mcp_servers.len(), "session/new");
    if let Err(e) = validate_acp_cwd(&req.cwd) {
        return responder.respond_with_error(invalid_cwd(&req.cwd, e));
    }
    let label = new_session_label(&req.cwd);
    let opened = match bridge.kernel.open_or_create(&label).await {
        Ok(o) => o,
        Err(e) => return responder.respond_with_error(internal(e)),
    };
    if let Err(e) = bridge.kernel.set_context_cwd(opened.context_id, &req.cwd).await {
        if !opened.resumed
            && let Err(cleanup) = bridge.kernel.archive_context(opened.context_id).await
        {
            tracing::warn!(
                context = %opened.context_id.short(),
                error = %cleanup,
                "failed to archive fresh context after invalid ACP cwd"
            );
        }
        return responder.respond_with_error(invalid_cwd(&req.cwd, e));
    }
    let session_id = rank::session_id_of(opened.context_id);
    match start_session(&bridge, &session_id, opened.context_id, opened.label, &cx, false).await {
        Ok(()) => responder.respond(NewSessionResponse::new(session_id)),
        Err(e) => responder.respond_with_error(e),
    }
}

async fn handle_load_session(
    bridge: Arc<AcpBridge>,
    req: LoadSessionRequest,
    responder: Responder<LoadSessionResponse>,
    cx: ConnectionTo<Client>,
) -> Result<(), Error> {
    warn_ignored_mcp_servers(req.mcp_servers.len(), "session/load");
    let Some(context_id) = rank::context_id_of(&req.session_id) else {
        return responder.respond_with_error(
            Error::invalid_params().data(serde_json::json!({
                "reason": "session id is not a kaijutsu context id",
                "sessionId": req.session_id.0.as_ref(),
            })),
        );
    };
    let info = match bridge.kernel.attach(context_id).await {
        Ok(i) => i,
        Err(e) => {
            return responder
                .respond_with_error(Error::resource_not_found(Some(e.to_string())));
        }
    };
    if let Err(e) = bridge.kernel.set_context_cwd(context_id, &req.cwd).await {
        return responder.respond_with_error(invalid_cwd(&req.cwd, e));
    }
    // Replay the transcript: `session/load` exists so a client can render the
    // conversation it is rejoining.
    match start_session(&bridge, &req.session_id, context_id, info.label, &cx, true).await {
        Ok(()) => responder.respond(LoadSessionResponse::new()),
        Err(e) => responder.respond_with_error(e),
    }
}

async fn handle_resume_session(
    bridge: Arc<AcpBridge>,
    req: ResumeSessionRequest,
    responder: Responder<ResumeSessionResponse>,
    cx: ConnectionTo<Client>,
) -> Result<(), Error> {
    warn_ignored_mcp_servers(req.mcp_servers.len(), "session/resume");
    let Some(context_id) = rank::context_id_of(&req.session_id) else {
        return responder.respond_with_error(
            Error::invalid_params().data(serde_json::json!({
                "reason": "session id is not a kaijutsu context id",
                "sessionId": req.session_id.0.as_ref(),
            })),
        );
    };
    let info = match bridge.kernel.attach(context_id).await {
        Ok(i) => i,
        Err(e) => {
            return responder
                .respond_with_error(Error::resource_not_found(Some(e.to_string())));
        }
    };
    if let Err(e) = bridge.kernel.set_context_cwd(context_id, &req.cwd).await {
        return responder.respond_with_error(invalid_cwd(&req.cwd, e));
    }
    match start_session(&bridge, &req.session_id, context_id, info.label, &cx, false).await {
        Ok(()) => responder.respond(ResumeSessionResponse::new()),
        Err(e) => responder.respond_with_error(e),
    }
}

fn warn_ignored_mcp_servers(count: usize, method: &'static str) {
    if count != 0 {
        tracing::warn!(
            count,
            method,
            "ignoring client-declared mcpServers — external MCP wiring is unplumbed (docs/acp.md gap #5)"
        );
    }
}

fn invalid_cwd(cwd: &std::path::Path, error: anyhow::Error) -> Error {
    Error::invalid_params().data(serde_json::json!({
        "reason": error.to_string(),
        "cwd": cwd,
    }))
}

fn validate_acp_cwd(cwd: &std::path::Path) -> anyhow::Result<()> {
    if !cwd.is_absolute() {
        anyhow::bail!("ACP cwd must be absolute: {}", cwd.display());
    }
    Ok(())
}

/// Bind a session and start its pump. Idempotent: loading an already-bound
/// session is a no-op rather than a second pump on the same context.
async fn start_session(
    bridge: &Arc<AcpBridge>,
    session_id: &SessionId,
    context_id: kaijutsu_types::ContextId,
    label: String,
    cx: &ConnectionTo<Client>,
    replay_history: bool,
) -> Result<(), Error> {
    if let Some(session) = bridge.sessions.get(session_id) {
        tracing::info!(session = %session_id, "session already bound; keeping its pump");
        refresh_command_catalog(bridge, session_id, &session, cx, true).await?;
        return Ok(());
    }
    let commands = bridge.kernel.kj_command_catalog(context_id).await.map_err(internal)?;
    let (mirror, feed_rx) = bridge
        .kernel
        .hydrate_context(context_id)
        .await
        .map_err(internal)?;

    let session = Session::new(
        context_id,
        label,
        UpdateMapper::new(session_id.clone()),
        commands,
    );
    let Some(session) = bridge.sessions.bind(session_id.clone(), session) else {
        // Lost a race with a concurrent bind; the winner's pump is running.
        return Ok(());
    };

    send_command_catalog(session_id, &session.commands(), cx)?;

    let kernel = bridge.kernel.clone();
    let pump_cx = cx.clone();
    let pump_id = session_id.clone();
    cx.spawn(async move {
        run_pump(kernel, session, pump_id, pump_cx, mirror, feed_rx, replay_history).await
    })?;
    Ok(())
}

fn available_commands(commands: &[kaijutsu_client::rpc::KjCommandInfo]) -> Vec<AvailableCommand> {
    commands
        .iter()
        .map(|command| {
            let available = AvailableCommand::new(&command.name, &command.description);
            if command.input_hint.is_empty() {
                available
            } else {
                available.input(AvailableCommandInput::Unstructured(
                    UnstructuredCommandInput::new(&command.input_hint),
                ))
            }
        })
        .collect()
}

fn send_command_catalog(
    session_id: &SessionId,
    commands: &[kaijutsu_client::rpc::KjCommandInfo],
    cx: &ConnectionTo<Client>,
) -> Result<(), Error> {
    cx.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(
            available_commands(commands),
        )),
    ))
}

/// Refresh only at a request boundary, never while a prompt is executing.
/// This keeps the advertised names in step with loadout changes without
/// allowing a catalog mutation to reinterpret an in-flight prompt.
async fn refresh_command_catalog(
    bridge: &AcpBridge,
    session_id: &SessionId,
    session: &Arc<Session>,
    cx: &ConnectionTo<Client>,
    force_send: bool,
) -> Result<(), Error> {
    let commands = bridge
        .kernel
        .kj_command_catalog(session.context_id)
        .await
        .map_err(internal)?;
    let changed = session.replace_commands(commands);
    if changed || force_send {
        send_command_catalog(session_id, &session.commands(), cx)?;
    }
    Ok(())
}

async fn handle_list_sessions(
    bridge: Arc<AcpBridge>,
    _req: ListSessionsRequest,
    responder: Responder<ListSessionsResponse>,
) -> Result<(), Error> {
    match bridge.kernel.list_contexts().await {
        Ok(contexts) => {
            let mut cwds = std::collections::HashMap::new();
            for context_id in rank::ranked_context_ids(&contexts) {
                match bridge.kernel.context_cwd(context_id).await {
                    Ok(Some(cwd)) => {
                        cwds.insert(context_id, cwd);
                    }
                    Ok(None) => {
                        tracing::debug!(context = %context_id.short(), "omitting context without durable cwd from ACP session/list");
                    }
                    Err(e) => return responder.respond_with_error(internal(e)),
                }
            }
            let sessions = rank::ranked_sessions(&contexts, &cwds);
            responder.respond(ListSessionsResponse::new(sessions))
        }
        Err(e) => responder.respond_with_error(internal(e)),
    }
}

/// Archive the durable kj context, then tear down only this process's live
/// ACP binding. Kernel archive is idempotent, so a client may safely retry a
/// successful delete after losing its response.
async fn handle_delete_session(
    bridge: Arc<AcpBridge>,
    req: DeleteSessionRequest,
    responder: Responder<DeleteSessionResponse>,
) -> Result<(), Error> {
    let Some(context_id) = rank::context_id_of(&req.session_id) else {
        return responder.respond_with_error(
            Error::invalid_params().data(serde_json::json!({
                "reason": "session id is not a kaijutsu context id",
                "sessionId": req.session_id.0.as_ref(),
            })),
        );
    };
    match bridge.kernel.archive_context(context_id).await {
        Ok(()) => {
            bridge.sessions.unbind(&req.session_id);
            responder.respond(DeleteSessionResponse::new())
        }
        Err(e) => responder.respond_with_error(Error::resource_not_found(Some(e.to_string()))),
    }
}

/// `session/prompt` — spawned, never run in the dispatch loop.
///
/// A turn can run for minutes. Holding the dispatch loop for that long would
/// make `session/cancel` unreachable, which is precisely the notification a
/// user sends when a turn is running long.
fn handle_prompt(
    bridge: Arc<AcpBridge>,
    req: PromptRequest,
    responder: Responder<PromptResponse>,
    cx: ConnectionTo<Client>,
) -> Result<(), Error> {
    let prompt_cx = cx.clone();
    cx.spawn(async move {
        let Some(session) = bridge.sessions.get(&req.session_id) else {
            return responder.respond_with_error(
                Error::resource_not_found(Some(format!(
                    "no such session: {} — call session/new or session/load first",
                    req.session_id
                ))),
            );
        };
        if let Err(e) = refresh_command_catalog(
            &bridge,
            &req.session_id,
            &session,
            &prompt_cx,
            false,
        )
        .await
        {
            return responder.respond_with_error(e);
        }

        match classify_command(&req.prompt, &session.commands()) {
            Ok(Some(command)) => {
                let result = match bridge
                    .kernel
                    .execute_kj(session.context_id, command.argv)
                    .await
                {
                    Ok(result) => result,
                    Err(e) => return responder.respond_with_error(internal(e)),
                };
                settle_command_delivery(
                    &bridge.kernel,
                    &session,
                    result.command_block_id,
                )
                .await;
                if let Some(latch) = result.latch {
                    return responder.respond_with_error(
                        Error::internal_error().data(serde_json::json!({
                            "reason": format!(
                                "kj command requires explicit confirmation: {}",
                                latch.message
                            ),
                            "latch": {
                                "command": latch.command,
                                "target": latch.target,
                            },
                            "confirmation": "not performed; submit an explicit follow-up command",
                        })),
                    );
                }
                if result.exit_code != 0 {
                    return responder.respond_with_error(
                        Error::internal_error().data(serde_json::json!({
                            "reason": "kj command failed",
                            "exitCode": result.exit_code,
                            "stdout": result.stdout,
                            "stderr": result.stderr,
                        })),
                    );
                }
                return responder.respond(PromptResponse::new(
                    agent_client_protocol::schema::v1::StopReason::EndTurn,
                ));
            }
            Ok(None) => {}
            Err(reason) => {
                return responder.respond_with_error(
                    Error::invalid_params().data(serde_json::json!({ "reason": reason })),
                );
            }
        }
        let text = prompt_text(&req.prompt);
        match run_turn(&bridge.kernel, &session, &text).await {
            Ok(TurnOutcome::Stopped(stop)) => responder.respond(PromptResponse::new(stop)),
            // A broken turn is an error, not a turn that ended quietly. ACP's
            // StopReason has no "failed", and reporting `end_turn` for a
            // crashed turn would be a silent fallback.
            Ok(TurnOutcome::Failed(err)) => responder.respond_with_error(
                Error::internal_error().data(serde_json::json!({ "turnFailed": err })),
            ),
            Err(e) => responder.respond_with_error(internal(e)),
        }
    })
}

#[derive(Debug, PartialEq, Eq)]
struct CommandPrompt {
    argv: Vec<String>,
}

/// Recognize only the exact shape ACP clients use for an invoked advertised
/// command. Everything else remains ordinary model chat, including unknown
/// slash text and prompts carrying a second content block.
fn classify_command(
    prompt: &[ContentBlock],
    commands: &[kaijutsu_client::rpc::KjCommandInfo],
) -> Result<Option<CommandPrompt>, String> {
    let [ContentBlock::Text(text)] = prompt else {
        return Ok(None);
    };
    let Some(body) = text.text.strip_prefix('/') else {
        return Ok(None);
    };
    let (name, input) = match body.find(' ') {
        Some(boundary) => (&body[..boundary], Some(&body[boundary + 1..])),
        None => (body, None),
    };
    let Some(command) = commands.iter().find(|command| command.name == name) else {
        return Ok(None);
    };

    let mut argv = command.argv_prefix.clone();
    let input = input.unwrap_or("");
    if command.input_hint.is_empty() {
        if !input.trim().is_empty() {
            return Err(format!("/{name} does not accept input"));
        }
    } else if !input.trim().is_empty() {
        let parsed = shell_words::split(input)
            .map_err(|e| format!("invalid input for /{name}: {e}"))?;
        argv.extend(parsed);
    }
    Ok(Some(CommandPrompt { argv }))
}

/// `session/cancel` — soft interrupt.
///
/// Soft is the right default: the in-flight model call finishes, so the
/// transcript keeps a complete phrase instead of a truncated fragment, and
/// only the agentic loop stops. The in-flight `session/prompt` then observes
/// `TurnCompleted { CancelledSoft }` and answers `stopReason: cancelled` —
/// which is what ACP requires of a cancelled prompt.
async fn handle_cancel(
    bridge: Arc<AcpBridge>,
    notif: agent_client_protocol::schema::v1::CancelNotification,
) -> Result<(), Error> {
    let Some(session) = bridge.sessions.get(&notif.session_id) else {
        tracing::warn!(session = %notif.session_id, "cancel for an unknown session");
        return Ok(());
    };
    match bridge.kernel.interrupt(session.context_id, false).await {
        Ok(true) => tracing::info!(session = %notif.session_id, "soft interrupt sent"),
        Ok(false) => tracing::info!(session = %notif.session_id, "nothing running to cancel"),
        Err(e) => tracing::error!(session = %notif.session_id, error = %e, "interrupt failed"),
    }
    Ok(())
}

fn internal(e: impl std::fmt::Display) -> Error {
    Error::internal_error().data(serde_json::json!({ "reason": e.to_string() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{ResourceLink, TextContent};

    fn command(
        name: &str,
        hint: &str,
        prefix: &[&str],
    ) -> kaijutsu_client::rpc::KjCommandInfo {
        kaijutsu_client::rpc::KjCommandInfo {
            name: name.into(),
            description: format!("Run {name}"),
            input_hint: hint.into(),
            argv_prefix: prefix.iter().map(|part| (*part).to_string()).collect(),
        }
    }

    #[test]
    fn text_blocks_join_with_newlines() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("first")),
            ContentBlock::Text(TextContent::new("second")),
        ];
        assert_eq!(prompt_text(&blocks), "first\nsecond");
    }

    #[test]
    fn a_single_text_block_passes_through_untouched() {
        let blocks = vec![ContentBlock::Text(TextContent::new("hello 日本語"))];
        assert_eq!(prompt_text(&blocks), "hello 日本語");
    }

    #[test]
    fn unsupported_content_is_named_not_silently_dropped() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("look at")),
            ContentBlock::ResourceLink(ResourceLink::new("notes.md", "file:///notes.md")),
        ];
        let out = prompt_text(&blocks);
        assert!(out.contains("look at"));
        assert!(
            out.contains("resource_link") && out.contains("file:///notes.md"),
            "a dropped block must leave a trace in the transcript: {out}"
        );
    }

    #[test]
    fn an_empty_prompt_is_empty_not_a_stray_newline() {
        assert_eq!(prompt_text(&[]), "");
    }

    #[test]
    fn catalog_projects_names_descriptions_and_only_real_input_hints() {
        let commands = available_commands(&[
            command("models", "", &["models"]),
            command("context-info", "[context]", &["context", "info"]),
        ]);
        assert_eq!(commands[0].name, "models");
        assert_eq!(commands[0].description, "Run models");
        assert!(commands[0].input.is_none());
        assert!(matches!(
            &commands[1].input,
            Some(AvailableCommandInput::Unstructured(input))
                if input.hint == "[context]"
        ));
    }

    #[test]
    fn advertised_slash_command_builds_structured_argv() {
        let commands = [command(
            "context-info",
            "[context]",
            &["context", "info"],
        )];
        let prompt = [ContentBlock::Text(TextContent::new(
            "/context-info 'label with spaces'",
        ))];
        assert_eq!(
            classify_command(&prompt, &commands).unwrap(),
            Some(CommandPrompt {
                argv: vec!["context".into(), "info".into(), "label with spaces".into()],
            })
        );
    }

    #[test]
    fn command_splits_only_at_first_ascii_space() {
        let commands = [command("model", "[--context <context>]", &["model"])];
        let prompt = [ContentBlock::Text(TextContent::new(
            "/model --context 'alpha beta'",
        ))];
        assert_eq!(
            classify_command(&prompt, &commands).unwrap().unwrap().argv,
            ["model", "--context", "alpha beta"]
        );

        let tab = [ContentBlock::Text(TextContent::new("/model\t--context alpha"))];
        assert_eq!(classify_command(&tab, &commands).unwrap(), None);
    }

    #[test]
    fn unknown_slash_and_multiblock_prompts_remain_chat() {
        let commands = [command("models", "", &["models"])];
        let unknown = [ContentBlock::Text(TextContent::new("/not-a-command hello"))];
        assert_eq!(classify_command(&unknown, &commands).unwrap(), None);

        let multi = [
            ContentBlock::Text(TextContent::new("/models")),
            ContentBlock::ResourceLink(ResourceLink::new("notes", "file:///notes")),
        ];
        assert_eq!(classify_command(&multi, &commands).unwrap(), None);
    }

    #[test]
    fn no_input_commands_reject_arguments_and_bad_quotes_are_clear() {
        let no_input = [command("models", "", &["models"])];
        let extra = [ContentBlock::Text(TextContent::new("/models surprise"))];
        assert!(
            classify_command(&extra, &no_input)
                .unwrap_err()
                .contains("does not accept input")
        );

        let with_input = [command("context-info", "[context]", &["context", "info"])];
        let unclosed = [ContentBlock::Text(TextContent::new("/context-info 'oops"))];
        assert!(
            classify_command(&unclosed, &with_input)
                .unwrap_err()
                .contains("invalid input")
        );
    }

    #[test]
    fn we_negotiate_down_to_v1_never_up() {
        assert_eq!(negotiate_version(ProtocolVersion::V1), ProtocolVersion::V1);
        // A client speaking a later protocol still gets v1 — that is all we
        // implement, and saying otherwise would be a lie the client acts on.
        assert_eq!(
            negotiate_version(ProtocolVersion::LATEST),
            ProtocolVersion::V1
        );
        assert_eq!(negotiate_version(ProtocolVersion::V0), ProtocolVersion::V0);
    }

    #[test]
    fn we_advertise_only_what_we_actually_forward() {
        let caps = agent_capabilities();
        assert!(caps.load_session, "session/load is implemented");
        assert!(
            caps.session_capabilities.list.is_some(),
            "session/list serves the rank"
        );
        assert!(
            caps.session_capabilities.resume.is_some(),
            "session/resume attaches without replay"
        );
        assert!(
            caps.session_capabilities.delete.is_some(),
            "session/delete archives contexts"
        );
        // Advertising image/audio we would drop is worse than declining them.
        assert!(!caps.prompt_capabilities.image);
        assert!(!caps.prompt_capabilities.audio);
        assert!(!caps.prompt_capabilities.embedded_context);
    }

    #[test]
    fn relative_new_session_cwd_is_rejected_before_kernel_work() {
        let err = validate_acp_cwd(std::path::Path::new("relative/project"))
            .expect_err("ACP requires absolute cwd");
        assert!(err.to_string().contains("must be absolute"));
        validate_acp_cwd(std::path::Path::new("/absolute/project")).unwrap();
    }
}
