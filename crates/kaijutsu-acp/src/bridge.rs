//! The kernel side of the bridge — everything that speaks Cap'n Proto.
//!
//! Deliberately thin, and deliberately *not* forked from `kaijutsu-mcp`: both
//! bridges consume `kaijutsu-client`'s `ActorHandle` unchanged. The actor owns
//! the SSH connection, the reconnect FSM, the liveness ping, and the automatic
//! re-subscribe on reconnect; nothing in this crate reimplements any of that.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use kaijutsu_client::{
    ActorHandle, ContextInfo, PeerConfig, PeerInvocation, SshConfig, SyncedDocument, connect_ssh,
    spawn_actor,
};
use kaijutsu_crdt::{BlockId, ContextId, PrincipalId};

/// Per-process subscription identity.
///
/// The server dedupes block-event subscriptions by `(principal, instance)`. A
/// constant here would make two concurrent bridges steal each other's event
/// stream, so it is minted once per process — same reasoning as
/// `mcp_peer_instance()`.
fn acp_peer_instance() -> &'static str {
    static INSTANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| format!("kaijutsu-acp-{}", uuid::Uuid::new_v4()))
}

/// Turn an ACP implementation name into the stable address shown in the
/// kernel's peer registry. ACP names are programmatic identifiers, but they
/// are supplied by another process, so keep the useful identifier characters
/// and collapse everything else rather than putting arbitrary display text in
/// a peer address.
pub fn peer_nick_for_client(name: &str) -> String {
    let slug = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() { "acp/client".to_string() } else { format!("acp/{slug}") }
}

/// A live connection to a kaijutsu kernel.
#[derive(Clone)]
pub struct KernelBridge {
    actor: ActorHandle,
    principal: PrincipalId,
    context_type: String,
}

impl KernelBridge {
    /// Connect over SSH and bring the actor up to `Connected`.
    ///
    /// The eager `connect_ssh` + `bind_kernel` + `drop` is copied from
    /// kaijutsu-mcp on purpose: it fails fast on auth and kernel
    /// reachability *before* committing to a background actor that would
    /// otherwise retry a hopeless connection forever. The ~50ms re-handshake
    /// is the price.
    pub async fn connect(
        config: SshConfig,
        context_type: String,
        connect_timeout: std::time::Duration,
    ) -> Result<Self> {
        let client = connect_ssh(config.clone())
            .await
            .context("ssh connect to kaijutsu-server")?;
        let (_kernel, kernel_id) = client.bind_kernel().await.context("bind kernel")?;
        drop(client);
        tracing::info!(kernel = %kernel_id, "bound kernel");

        // `scope_blocks_to_context: false` — kernel-wide block events.
        //
        // An ACP client can hold several sessions at once (that is what
        // `session/list` is *for*), and each one needs its own block stream.
        // Scoping to a single joined context would starve every session but
        // the last one joined. The cost is the firehose kaijutsu-mcp scopes
        // away from; each session pump filters by context id. If that ever
        // bites, the fix is an actor per session, not a narrower filter.
        let actor = spawn_actor(config, None, acp_peer_instance().to_string(), false);

        // A fresh actor sits in Idle until something pokes it. Kick it, then
        // wait on the *level* mirror — `subscribe_status()` never replays the
        // edge, so a fast local handshake would hang a broadcast waiter
        // forever.
        //
        // The wait is **bounded**. The actor's reconnect FSM treats a failed
        // handshake as transient and retries with backoff forever; it never
        // reaches Terminal. An unbounded wait here therefore hangs the bridge
        // silently — an ACP client sees `initialize` go unanswered with no
        // hint why. Found by the first live smoke test, against a kernel too
        // old to serve `subscribeTurnEvents`. Bounding it turns "hangs" into
        // "says what went wrong and exits", which is the trade we want.
        let mut status = actor.watch_status();
        let _ = actor.whoami().await;
        let settled = tokio::time::timeout(
            connect_timeout,
            status.wait_for(|s| {
                matches!(
                    s,
                    kaijutsu_client::ConnectionStatus::Connected { .. }
                        | kaijutsu_client::ConnectionStatus::Terminal { .. }
                )
            }),
        )
        .await;
        match actor.current_status() {
            kaijutsu_client::ConnectionStatus::Connected { .. } => {}
            kaijutsu_client::ConnectionStatus::Terminal { reason } => {
                bail!("kernel connection terminal: {reason}");
            }
            other if settled.is_err() => {
                bail!(
                    "kernel did not reach Connected within {:?} (status: {other:?}). \
                     A handshake that keeps failing transiently usually means the \
                     running kaijutsu-server predates a wire method this client \
                     requires — check its log for 'Method not implemented' and \
                     rebuild/restart it.",
                    connect_timeout
                );
            }
            other => bail!("kernel connection ended in an unexpected state: {other:?}"),
        }

        Ok(Self {
            actor,
            principal: PrincipalId::new(),
            context_type,
        })
    }

    pub fn actor(&self) -> &ActorHandle {
        &self.actor
    }

    /// Advertise the ACP client process as one peer, independent of how many
    /// contexts it opens. `ActorHandle` remembers this registration and
    /// replays it after reconnect; cancelling the underlying SSH connection
    /// removes the server-side registration at process teardown.
    pub async fn attach_client_peer(&self, client_name: &str) -> Result<String> {
        let nick = peer_nick_for_client(client_name);
        let (peer_tx, peer_rx) = std::sync::mpsc::channel::<PeerInvocation>();
        std::thread::spawn(move || {
            while let Ok(invocation) = peer_rx.recv() {
                let _ = invocation.reply.send(Err(format!(
                    "ACP client peer does not implement action '{}'",
                    invocation.action
                )));
            }
        });
        self.actor
            .attach_peer(
                PeerConfig {
                    nick: nick.clone(),
                    instance: acp_peer_instance().to_string(),
                },
                peer_tx,
            )
            .await
            .context("attach ACP client peer")?;
        Ok(nick)
    }

    pub async fn list_contexts(&self) -> Result<Vec<ContextInfo>> {
        self.actor
            .list_contexts()
            .await
            .context("list contexts")
    }

    /// Read the durable kaish cwd for one context without depending on the
    /// actor's ambient joined context.
    pub async fn context_cwd(&self, context_id: ContextId) -> Result<Option<PathBuf>> {
        self.actor
            .get_context_cwd(context_id)
            .await
            .context("get context cwd")
            .map(|cwd| cwd.map(PathBuf::from))
    }

    /// Reassert an ACP attachment's cwd as durable kaish state.
    pub async fn set_context_cwd(&self, context_id: ContextId, cwd: &std::path::Path) -> Result<()> {
        if !cwd.is_absolute() {
            bail!("ACP cwd must be absolute: {}", cwd.display());
        }
        let cwd = cwd
            .to_str()
            .with_context(|| format!("ACP cwd is not valid UTF-8: {}", cwd.display()))?;
        self.actor
            .set_context_cwd(context_id, cwd)
            .await
            .with_context(|| format!("set context cwd to {cwd}"))
    }

    pub async fn archive_context(&self, context_id: ContextId) -> Result<()> {
        self.actor
            .archive_context(context_id)
            .await
            .context("archive context")
    }

    /// Resolve-or-create a context for a label, then join it.
    ///
    /// Upsert semantics lifted from `register_session`: a live context under
    /// the label is *attached* (an ACP client reconnecting should land back in
    /// its conversation), a concluded or archived one is never resurrected —
    /// we take a fresh suffixed label instead — and an unknown label creates.
    pub async fn open_or_create(&self, label: &str) -> Result<OpenedContext> {
        match self.actor.resolve_context_label(label).await? {
            Some(existing) if existing.concluded_at.is_none() && !existing.archived => {
                // Loud on purpose: a reused label attaching to a prior
                // conversation is the stale-id hazard, not a convenience.
                tracing::warn!(
                    context = %existing.id.short(),
                    label,
                    "attaching to an existing live context"
                );
                self.actor.join_context(existing.id).await?;
                Ok(OpenedContext {
                    context_id: existing.id,
                    label: existing.label,
                    resumed: true,
                    previous: None,
                })
            }
            Some(stale) => {
                let fresh = self.free_label(label).await?;
                tracing::info!(
                    previous = %stale.id.short(),
                    label = %fresh,
                    "prior context is concluded/archived; taking a fresh label"
                );
                let id = self
                    .actor
                    .create_context_typed(&fresh, &self.context_type)
                    .await?;
                self.actor.join_context(id).await?;
                Ok(OpenedContext {
                    context_id: id,
                    label: fresh,
                    resumed: false,
                    previous: Some(stale.id),
                })
            }
            None => {
                let id = self
                    .actor
                    .create_context_typed(label, &self.context_type)
                    .await?;
                self.actor.join_context(id).await?;
                Ok(OpenedContext {
                    context_id: id,
                    label: label.to_string(),
                    resumed: false,
                    previous: None,
                })
            }
        }
    }

    /// Join a context by id (the `session/load` path).
    pub async fn attach(&self, context_id: ContextId) -> Result<ContextInfo> {
        let contexts = self.list_contexts().await?;
        let Some(info) = contexts.into_iter().find(|c| c.id == context_id) else {
            bail!("no such context: {}", context_id.short());
        };
        ensure_context_attachable(&info)?;
        self.actor.join_context(context_id).await?;
        Ok(info)
    }

    /// Build a CRDT mirror of a context's blocks.
    pub async fn synced(&self, context_id: ContextId) -> Result<SyncedDocument> {
        let state = self.actor.get_context_sync(context_id).await?;
        SyncedDocument::from_sync_state(&state, self.principal)
            .with_context(|| format!("build synced document for {}", context_id.short()))
    }

    /// Write `text` into the context's input doc and submit it as a chat turn.
    ///
    /// Returns the block id `submit_input` minted for the prompt.
    pub async fn send_prompt(&self, context_id: ContextId, text: &str) -> Result<BlockId> {
        let state = self.actor.get_input_state(context_id).await?;
        // Char count, not `.len()`. The input doc is a text CRDT addressed in
        // characters; a byte length here truncates or over-deletes the moment
        // anyone types non-ASCII. (kaijutsu-mcp's `write_input` still uses
        // bytes — logged in docs/issues.md.)
        let existing = state.content.chars().count() as u64;
        self.actor
            .edit_input(context_id, 0, text, existing)
            .await
            .context("write input doc")?;
        let result = self
            .actor
            .submit_input(context_id, false)
            .await
            .context("submit input")?;
        Ok(result.block_id)
    }

    /// Interrupt the running turn. `immediate: false` is the soft stop — the
    /// in-flight model call finishes, so the block is a complete phrase and
    /// only the agentic loop halts. That is the right default for a user
    /// pressing Esc on a phone.
    pub async fn interrupt(&self, context_id: ContextId, immediate: bool) -> Result<bool> {
        self.actor
            .interrupt_context(context_id, immediate)
            .await
            .context("interrupt context")
    }

    /// Probe `base`, `base-2`, `base-3`, … for a label nothing holds.
    async fn free_label(&self, base: &str) -> Result<String> {
        for n in 2..1000 {
            let candidate = format!("{base}-{n}");
            if self.actor.resolve_context_label(&candidate).await?.is_none() {
                return Ok(candidate);
            }
        }
        bail!("no free label under {base} after 1000 tries");
    }
}

/// Loading/resuming is an attachment, never a resurrection act. Keep this
/// check immediately before `join_context` so every ACP attach-by-id path has
/// the same lifecycle boundary.
fn ensure_context_attachable(info: &ContextInfo) -> Result<()> {
    if info.archived {
        bail!("context {} is archived", info.id.short());
    }
    if info.concluded_at.is_some() {
        bail!("context {} is concluded", info.id.short());
    }
    Ok(())
}

/// What `open_or_create` did.
#[derive(Debug, Clone)]
pub struct OpenedContext {
    pub context_id: ContextId,
    pub label: String,
    /// True when we attached to a context that already existed.
    pub resumed: bool,
    /// The concluded/archived context we declined to resurrect, if any.
    pub previous: Option<ContextId>,
}

/// A label for a brand-new ACP session: the client's cwd plus a timestamp, so
/// two sessions in the same directory do not collide and a human scanning
/// `kj context list` can tell what a row is.
pub fn new_session_label(cwd: &std::path::Path) -> String {
    let leaf = cwd
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("acp");
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("acp-{leaf}-{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context_lifecycle(archived: bool, concluded_at: Option<u64>) -> ContextInfo {
        ContextInfo {
            id: ContextId::new(),
            label: "test".into(),
            forked_from: None,
            provider: String::new(),
            model: String::new(),
            created_at: 0,
            trace_id: [0; 16],
            fork_kind: None,
            context_type: "coder".into(),
            archived,
            concluded_at,
            keywords: Vec::new(),
            top_block_preview: None,
            live_status: kaijutsu_types::Status::Pending,
            last_activity_at: None,
            track_id: None,
            promoted_at: None,
            demoted_at: None,
            paused_at: None,
            context_window: None,
            context_used_tokens: None,
            context_used_pct: None,
            background_running_count: 0,
            background_oldest_running_started_at: None,
            background_last_finished_at: None,
            background_last_finished_status: None,
            background_last_exit_code: None,
            cast_label: None,
            origin_host: None,
        }
    }

    #[test]
    fn session_labels_name_the_directory() {
        let label = new_session_label(std::path::Path::new("/home/amy/src/kaijutsu"));
        assert!(label.starts_with("acp-kaijutsu-"), "got {label}");
    }

    #[test]
    fn a_rootless_cwd_still_yields_a_label() {
        let label = new_session_label(std::path::Path::new("/"));
        assert!(label.starts_with("acp-acp-"), "got {label}");
    }

    #[test]
    fn the_instance_id_is_stable_within_a_process() {
        // Stability matters: the server's (principal, instance) dedupe must
        // replace our subscription on reconnect, not stack a second one.
        assert_eq!(acp_peer_instance(), acp_peer_instance());
        assert!(acp_peer_instance().starts_with("kaijutsu-acp-"));
    }

    #[test]
    fn client_name_becomes_namespaced_peer_nick() {
        assert_eq!(peer_nick_for_client("toad"), "acp/toad");
        assert_eq!(peer_nick_for_client("Zed Industries"), "acp/zed-industries");
        assert_eq!(peer_nick_for_client("Happy/手机"), "acp/happy");
    }

    #[test]
    fn empty_client_name_has_a_safe_fallback() {
        assert_eq!(peer_nick_for_client(" \t/ "), "acp/client");
    }

    #[test]
    fn attach_by_id_never_resurrects_archived_or_concluded_contexts() {
        ensure_context_attachable(&context_lifecycle(false, None)).unwrap();
        let archived = ensure_context_attachable(&context_lifecycle(true, None)).unwrap_err();
        assert!(archived.to_string().contains("archived"));
        let concluded =
            ensure_context_attachable(&context_lifecycle(false, Some(1))).unwrap_err();
        assert!(concluded.to_string().contains("concluded"));
    }
}
