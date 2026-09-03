//! The kernel side of the terminal client — everything that speaks Cap'n Proto.
//!
//! Deliberately thin, and deliberately not forked from `kaijutsu-acp`: both
//! bridges consume `kaijutsu-client`'s `ActorHandle` unchanged. The actor owns
//! the SSH connection, the reconnect FSM, the liveness ping, and the automatic
//! re-subscribe on reconnect; nothing here reimplements any of that.
//!
//! `docs/tui.md`, "Shape: the ACP bridge minus the protocol".

use anyhow::{Context, Result, bail};
use kaijutsu_client::rpc::KjExecutionResult;
use kaijutsu_client::{
    ActorHandle, ContextInfo, ContextMirror, FeedEvent, SshConfig, connect_ssh, spawn_actor,
};
use kaijutsu_types::{BlockId, BlockQuery, ContextId, PrincipalId};
use tokio::sync::mpsc;

/// Per-process subscription identity.
///
/// The server dedupes block-event subscriptions by `(principal, instance)`. A
/// constant here would make two terminal clients on one machine steal each
/// other's event stream, so it is minted once per process.
fn tui_peer_instance() -> &'static str {
    static INSTANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| format!("kaijutsu-tui-{}", uuid::Uuid::new_v4()))
}

/// A live connection to a kaijutsu kernel.
#[derive(Clone)]
pub struct KernelBridge {
    actor: ActorHandle,
    context_type: String,
}

impl KernelBridge {
    /// Connect over SSH and bring the actor up to `Connected`.
    ///
    /// The eager `connect_ssh` + `bind_kernel` + `drop` fails fast on auth and
    /// kernel reachability before committing to a background actor that would
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
        let (_kernel, kernel_id) = client
            .bind_kernel()
            .await
            .map_err(|e| {
                let msg = e.to_string();
                // A wire-version mismatch needs one unambiguous line before
                // raw mode is ever entered. `msg` already names both wire
                // versions and which side is stale; only add the rebuild
                // command when THIS binary is the stale side.
                if msg.contains("client is stale") {
                    eprintln!(
                        "kaijutsu-tui: wire version mismatch — {msg} Rebuild this binary: \
                         cargo build -p kaijutsu-tui"
                    );
                } else if msg.contains("kernel is stale") {
                    eprintln!("kaijutsu-tui: wire version mismatch — {msg}");
                }
                anyhow::Error::from(e)
            })
            .context("bind kernel")?;
        drop(client);
        tracing::info!(kernel = %kernel_id, "bound kernel");

        // `scope_blocks_to_context: false` — kernel-wide block events. One
        // process is the mux (`docs/tui.md`, ruling 2): several contexts are
        // watched at once and each needs its own stream, so scoping to a
        // single joined context would starve every context but the last
        // joined.
        let actor = spawn_actor(config, None, tui_peer_instance().to_string(), false);

        // A fresh actor sits in Idle until something pokes it. Kick it, then
        // wait on the level mirror — `subscribe_status()` never replays the
        // edge, so a fast local handshake would hang a broadcast waiter
        // forever.
        //
        // The wait is bounded. The actor's reconnect FSM treats a failed
        // handshake as transient and retries with backoff forever; it never
        // reaches Terminal. An unbounded wait would hang the client with no
        // hint why.
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
                    "kernel did not reach Connected within {connect_timeout:?} (status: \
                     {other:?}). A handshake that keeps failing transiently usually means the \
                     running kaijutsu-server predates a wire method this client requires — \
                     check its log for 'Method not implemented' and rebuild/restart it."
                );
            }
            other => bail!("kernel connection ended in an unexpected state: {other:?}"),
        }

        Ok(Self {
            actor,
            context_type,
        })
    }

    pub fn actor(&self) -> &ActorHandle {
        &self.actor
    }

    pub async fn list_contexts(&self) -> Result<Vec<ContextInfo>> {
        self.actor.list_contexts().await.context("list contexts")
    }

    /// Resolve a `--context` argument: a context id, then a label, then
    /// create under that label.
    pub async fn open(&self, target: &str) -> Result<ContextInfo> {
        if let Ok(id) = ContextId::parse(target) {
            let contexts = self.list_contexts().await?;
            if let Some(info) = contexts.into_iter().find(|c| c.id == id) {
                ensure_attachable(&info)?;
                self.actor.join_context(info.id).await?;
                return Ok(info);
            }
        }
        if let Some(info) = self.actor.resolve_context_label(target).await?
            && info.concluded_at.is_none()
            && !info.archived
        {
            self.actor.join_context(info.id).await?;
            return Ok(info);
        }
        let id = self
            .actor
            .create_context_typed(target, &self.context_type)
            .await
            .with_context(|| format!("create context {target}"))?;
        self.actor.join_context(id).await?;
        let contexts = self.list_contexts().await?;
        contexts
            .into_iter()
            .find(|c| c.id == id)
            .with_context(|| format!("created context {} vanished from the listing", id.short()))
    }

    /// Join the highest-ranked live context, for a start with no `--context`.
    pub async fn open_ranked(&self) -> Result<ContextInfo> {
        let contexts = self.list_contexts().await?;
        let seats = kaijutsu_client::ranked_seats(&contexts);
        let Some(first) = seats.first() else {
            bail!(
                "no live context to attach to. Pass --context <label> to create one, or make \
                 one with `kj context new`."
            );
        };
        let info = contexts
            .into_iter()
            .find(|c| c.id == first.context_id)
            .context("ranked seat names a context the listing does not carry")?;
        self.actor.join_context(info.id).await?;
        Ok(info)
    }

    /// Subscribe to a context's change feed and hydrate a fresh
    /// [`ContextMirror`] over it: subscribe first, then fetch a snapshot,
    /// then apply it. Keep both halves: apply later deliveries to the mirror,
    /// and redo this on `FeedEvent::Terminated`; on `FeedEvent::Resubscribed`
    /// call [`Self::rehydrate_context`] instead — the actor has already
    /// re-subscribed, so there is no new receiver.
    pub async fn hydrate_context(
        &self,
        context_id: ContextId,
    ) -> Result<(ContextMirror, mpsc::Receiver<FeedEvent>)> {
        let rx = self
            .actor
            .subscribe_context(context_id)
            .await
            .with_context(|| format!("subscribe to context feed for {}", context_id.short()))?;
        let mirror = self.hydrate_mirror(context_id).await?;
        Ok((mirror, rx))
    }

    /// Rebuild a fresh mirror after `FeedEvent::Resubscribed`, on the same
    /// receiver the caller already holds.
    pub async fn rehydrate_context(&self, context_id: ContextId) -> Result<ContextMirror> {
        self.hydrate_mirror(context_id).await
    }

    async fn hydrate_mirror(&self, context_id: ContextId) -> Result<ContextMirror> {
        let mut mirror = ContextMirror::new(context_id);
        let (blocks, version) = self
            .actor
            .get_blocks_versioned(context_id, BlockQuery::All)
            .await
            .with_context(|| format!("get_blocks_versioned for {}", context_id.short()))?;
        mirror
            .apply_snapshot(blocks, version)
            .with_context(|| format!("apply initial snapshot for {}", context_id.short()))?;
        Ok(mirror)
    }

    /// This client's own principal, which selects its draft block out of the
    /// mirror.
    pub async fn principal(&self) -> Result<PrincipalId> {
        Ok(self.actor.whoami().await.context("whoami")?.principal_id)
    }

    /// The context's draft as the kernel holds it right now, for the first
    /// compose buffer of a session.
    pub async fn read_input(&self, context_id: ContextId) -> Result<String> {
        Ok(self
            .actor
            .get_input_state(context_id)
            .await
            .context("read input doc")?
            .content)
    }

    /// One edit against the context's draft block. `pos` and `delete` are
    /// **character** offsets, matching `kaijutsu_editor::EditOp`; a byte
    /// length truncates or over-deletes the moment anyone types non-ASCII.
    /// Returns the context version the kernel acknowledged.
    pub async fn edit_input(
        &self,
        context_id: ContextId,
        pos: u64,
        insert: &str,
        delete: u64,
    ) -> Result<u64> {
        self.actor
            .edit_input(context_id, pos, insert, delete)
            .await
            .context("edit input doc")
    }

    /// Submit the draft as a chat turn. The kernel snapshots it into a block
    /// and clears the draft.
    pub async fn submit_input(&self, context_id: ContextId) -> Result<BlockId> {
        let result = self
            .actor
            .submit_input(context_id, false)
            .await
            .context("submit input")?;
        Ok(result.block_id)
    }

    /// Run one kaish statement as the human — the gated path
    /// (`docs/gate-and-shell-split.md`), what `:!<statement>` runs through.
    /// Output arrives as blocks on the context feed; nothing is returned
    /// here to print.
    pub async fn shell_execute(&self, context_id: ContextId, code: &str) -> Result<BlockId> {
        self.actor
            .shell_execute(code, context_id, true)
            .await
            .context("shell execute")
    }

    /// `Ctrl+C`'s escalation ladder (`docs/tui.md`, "Ctrl+C reclaimed").
    /// `immediate = false` lets the in-flight model call finish before the
    /// agentic loop stops; `immediate = true` aborts mid-stream.
    pub async fn interrupt_context(&self, context_id: ContextId, immediate: bool) -> Result<bool> {
        self.actor
            .interrupt_context(context_id, immediate)
            .await
            .context("interrupt context")
    }

    /// Execute structured `kj` argv against a context without changing the
    /// actor connection's ambient context.
    pub async fn execute_kj(
        &self,
        context_id: ContextId,
        argv: Vec<String>,
    ) -> Result<KjExecutionResult> {
        self.actor
            .execute_kj(context_id, argv)
            .await
            .context("execute addressed kj command")
    }

    /// The context's authoritative, loadout-filtered `kj` command surface —
    /// what `completion.rs`'s slash completion offers.
    pub async fn kj_command_catalog(
        &self,
        context_id: ContextId,
    ) -> Result<Vec<kaijutsu_client::rpc::KjCommandInfo>> {
        self.actor
            .get_kj_command_catalog(context_id)
            .await
            .context("get kj command catalog")
    }
}

/// Attaching is never a resurrection act. Keep this immediately before
/// `join_context` so every attach-by-id path has the same lifecycle boundary.
fn ensure_attachable(info: &ContextInfo) -> Result<()> {
    if info.archived {
        bail!("context {} is archived", info.id.short());
    }
    if info.concluded_at.is_some() {
        bail!("context {} is concluded", info.id.short());
    }
    Ok(())
}
