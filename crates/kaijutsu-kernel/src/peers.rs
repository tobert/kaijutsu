//! Peer registry: named RPC participants attached to the kernel.
//!
//! Peers are the Bevy app, MCP servers, and any other clients that register
//! a callback so the kernel can dispatch invocations to them. The transport
//! is what backs the `invoke_peer` MCP tool — drift navigation
//! (e.g. tell the app to switch contexts) is the primary user.
//!
//! Each peer registers under a stable `nick`. Re-attaching with the same
//! nick replaces the previous registration (so reconnects don't leave a
//! dead callback in place).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use kaijutsu_types::PrincipalId;
use tokio::sync::{mpsc, oneshot};

/// Configuration for attaching a peer.
#[derive(Debug, Clone, Default)]
pub struct PeerConfig {
    /// Stable address / role (e.g. "kaijutsu-app"). Shared across every window
    /// of the same kind — NOT unique per peer.
    pub nick: String,
    /// Unique-per-peer-process token (a UUID the app mints once at startup), so
    /// two windows of the same `nick` coexist instead of clobbering. Empty
    /// falls back to `nick` as the key — preserving single-peer behaviour for
    /// callers that don't (yet) supply one.
    pub instance: String,
    /// The peer's authenticated principal, **stamped server-side** from the
    /// connection (never trusted from the client). Enables principal-scoped
    /// kernel→peer addressing (e.g. pop the editor on all of a user's windows).
    pub principal: Option<PrincipalId>,
}

/// The registry key for a peer: its unique `instance`, or `nick` when no
/// instance was supplied (single-peer back-compat). Public so the server can
/// derive the same key a peer was attached under (e.g. for bridge-task
/// self-detach) without duplicating the rule.
pub fn peer_key(nick: &str, instance: &str) -> String {
    if instance.is_empty() {
        nick.to_string()
    } else {
        instance.to_string()
    }
}

/// Information about an attached peer.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub nick: String,
    pub instance: String,
    pub principal: Option<PrincipalId>,
    /// Unix timestamp ms when the peer attached.
    pub attached_at: u64,
}

impl PeerInfo {
    pub fn from_config(config: PeerConfig) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        Self {
            nick: config.nick,
            instance: config.instance,
            principal: config.principal,
            attached_at: now,
        }
    }

    /// The registry key this peer is stored under.
    fn key(&self) -> String {
        peer_key(&self.nick, &self.instance)
    }
}

// ── Peer Invocation ─────────────────────────────────────────────────────────

/// A request to invoke a peer, dispatched via channel.
#[derive(Debug)]
pub struct InvokeRequest {
    /// The action to perform (e.g., "switch_context", "active_context").
    pub action: String,
    /// JSON-encoded parameters.
    pub params: Vec<u8>,
    /// Oneshot channel for the response.
    pub reply: oneshot::Sender<InvokeResponse>,
}

/// Response from a peer invocation.
#[derive(Debug)]
pub struct InvokeResponse {
    /// JSON-encoded result, or error message.
    pub result: Result<Vec<u8>, String>,
}

// ── Peer Registry ───────────────────────────────────────────────────────────

/// Registry for tracking attached peers.
#[derive(Default)]
pub struct PeerRegistry {
    /// Attached peers by nick.
    peers: HashMap<String, PeerInfo>,
    /// Channel senders for peer invocation — stored separately so PeerInfo stays Clone.
    invoke_senders: HashMap<String, mpsc::Sender<InvokeRequest>>,
}

impl PeerRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a peer to this registry, keyed by its [`peer_key`] (unique
    /// `instance`, or `nick` when none was supplied).
    ///
    /// The optional `invoke_sender` enables kernel → peer invocation. If the
    /// same key already exists (a reconnect of the *same* peer), the previous
    /// invoke channel is dropped (closing its bridge task) before the new
    /// sender is installed. A *different* instance of the same nick — e.g. a
    /// second app window — gets its own entry and coexists.
    pub fn attach(
        &mut self,
        config: PeerConfig,
        invoke_sender: Option<mpsc::Sender<InvokeRequest>>,
    ) -> Result<PeerInfo, PeerError> {
        let key = peer_key(&config.nick, &config.instance);
        if self.peers.contains_key(&key) {
            tracing::info!(
                nick = %config.nick, key = %key,
                "Peer re-attaching (replacing previous registration)",
            );
            self.invoke_senders.remove(&key);
        }

        let info = PeerInfo::from_config(config);
        self.peers.insert(key.clone(), info.clone());
        if let Some(sender) = invoke_sender {
            self.invoke_senders.insert(key, sender);
        }
        Ok(info)
    }

    /// Detach a peer by its registry key (`instance`, or `nick` for an
    /// instance-less peer). Returns the removed peer if present.
    pub fn detach(&mut self, key: &str) -> Option<PeerInfo> {
        self.invoke_senders.remove(key);
        self.peers.remove(key)
    }

    /// Get the invoke sender for a peer by nick — back-compat single-target
    /// lookup. With multiple peers sharing a nick (multi-window), returns the
    /// **most recently attached** one. For deliberate fan-out use
    /// [`Self::senders_by_nick`] / [`Self::senders_by_principal`].
    pub fn get_invoke_sender(&self, nick: &str) -> Option<mpsc::Sender<InvokeRequest>> {
        // Fast path: an instance-less peer is keyed directly by nick.
        if let Some(sender) = self.invoke_senders.get(nick) {
            return Some(sender.clone());
        }
        let key = self
            .peers
            .values()
            .filter(|p| p.nick == nick)
            .max_by_key(|p| p.attached_at)
            .map(|p| p.key())?;
        self.invoke_senders.get(&key).cloned()
    }

    /// Get the invoke sender for a specific peer instance (its registry key).
    pub fn get_invoke_sender_by_instance(
        &self,
        instance: &str,
    ) -> Option<mpsc::Sender<InvokeRequest>> {
        self.invoke_senders.get(instance).cloned()
    }

    /// Every invoke sender for peers with this nick (fan-out to all windows).
    pub fn senders_by_nick(&self, nick: &str) -> Vec<mpsc::Sender<InvokeRequest>> {
        self.peers
            .values()
            .filter(|p| p.nick == nick)
            .filter_map(|p| self.invoke_senders.get(&p.key()).cloned())
            .collect()
    }

    /// Every invoke sender for peers owned by this principal (fan-out to all of
    /// a user's windows — the principal-fallback target for editor-open).
    pub fn senders_by_principal(
        &self,
        principal: PrincipalId,
    ) -> Vec<mpsc::Sender<InvokeRequest>> {
        self.peers
            .values()
            .filter(|p| p.principal == Some(principal))
            .filter_map(|p| self.invoke_senders.get(&p.key()).cloned())
            .collect()
    }

    /// Belt-and-suspenders cleanup: drop any peer whose invoke channel is
    /// closed (its bridge task / receiver is gone). The primary cleanup is the
    /// bridge task self-detaching on `conn_cancel`; this catches stragglers a
    /// missed cancel or a panicked task would otherwise leave behind, so a
    /// fan-out never keeps invoking a dead window. Returns how many it reaped.
    pub fn reap_closed(&mut self) -> usize {
        let dead: Vec<String> = self
            .invoke_senders
            .iter()
            .filter(|(_, s)| s.is_closed())
            .map(|(k, _)| k.clone())
            .collect();
        for key in &dead {
            self.invoke_senders.remove(key);
            self.peers.remove(key);
        }
        dead.len()
    }

    /// Get a peer by its registry key.
    pub fn get(&self, key: &str) -> Option<&PeerInfo> {
        self.peers.get(key)
    }

    /// List all attached peers.
    pub fn list(&self) -> Vec<&PeerInfo> {
        self.peers.values().collect()
    }

    /// Number of attached peers.
    pub fn count(&self) -> usize {
        self.peers.len()
    }

    /// Check if a peer with this registry key is attached.
    pub fn contains(&self, key: &str) -> bool {
        self.peers.contains_key(key)
    }
}

/// Errors that can occur in peer operations.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PeerError {
    /// Peer not found in the registry.
    #[error("peer not found: {0}")]
    NotFound(String),
    /// Peer channel closed or reply sender dropped.
    #[error("peer disconnected: {0}")]
    Disconnected(String),
    /// Peer did not reply within the deadline.
    #[error("peer invocation timed out: {0}")]
    Timeout(String),
    /// Peer returned an error from its handler.
    #[error("peer invocation failed: {0}")]
    InvocationFailed(String),
}

/// Shared peer registry (Arc-wrapped for async access).
pub type SharedPeerRegistry = Arc<tokio::sync::RwLock<PeerRegistry>>;

/// Create a new shared peer registry.
pub fn shared_peer_registry() -> SharedPeerRegistry {
    Arc::new(tokio::sync::RwLock::new(PeerRegistry::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Instance-less peer config (back-compat: keyed by nick).
    fn cfg(nick: &str) -> PeerConfig {
        PeerConfig {
            nick: nick.to_string(),
            ..Default::default()
        }
    }

    /// Per-instance peer config with a stamped principal.
    fn cfg_inst(nick: &str, instance: &str, principal: PrincipalId) -> PeerConfig {
        PeerConfig {
            nick: nick.to_string(),
            instance: instance.to_string(),
            principal: Some(principal),
        }
    }

    #[test]
    fn test_attach_detach() {
        let mut registry = PeerRegistry::new();

        let info = registry.attach(cfg("kaijutsu-app"), None).unwrap();
        assert_eq!(info.nick, "kaijutsu-app");

        // Re-attaching same nick (no instance) replaces the old registration
        let info2 = registry.attach(cfg("kaijutsu-app"), None).unwrap();
        assert_eq!(info2.nick, "kaijutsu-app");
        assert_eq!(registry.count(), 1);

        // Detach
        let detached = registry.detach("kaijutsu-app");
        assert!(detached.is_some());
        assert!(!registry.contains("kaijutsu-app"));
    }

    #[test]
    fn test_attach_with_invoke_sender() {
        let mut registry = PeerRegistry::new();
        let (tx, _rx) = mpsc::channel(32);

        registry.attach(cfg("app"), Some(tx)).unwrap();
        assert!(registry.get_invoke_sender("app").is_some());
        assert!(registry.get_invoke_sender("nonexistent").is_none());
    }

    #[test]
    fn test_detach_cleans_sender() {
        let mut registry = PeerRegistry::new();
        let (tx, _rx) = mpsc::channel(32);

        registry.attach(cfg("app"), Some(tx)).unwrap();
        assert!(registry.get_invoke_sender("app").is_some());

        registry.detach("app");
        assert!(registry.get_invoke_sender("app").is_none());
    }

    /// Two windows of the same nick, each with its own instance, must COEXIST —
    /// the second no longer evicts the first (the clobber bug). Each is
    /// independently addressable by instance, and a nick fan-out reaches both.
    #[test]
    fn distinct_instances_of_one_nick_coexist() {
        let mut registry = PeerRegistry::new();
        let p = PrincipalId::new();
        let (tx_a, _ra) = mpsc::channel(8);
        let (tx_b, _rb) = mpsc::channel(8);

        registry
            .attach(cfg_inst("kaijutsu-app", "win-a", p), Some(tx_a))
            .unwrap();
        registry
            .attach(cfg_inst("kaijutsu-app", "win-b", p), Some(tx_b))
            .unwrap();

        assert_eq!(registry.count(), 2, "both windows must be registered");
        assert!(registry.get_invoke_sender_by_instance("win-a").is_some());
        assert!(registry.get_invoke_sender_by_instance("win-b").is_some());
        assert_eq!(
            registry.senders_by_nick("kaijutsu-app").len(),
            2,
            "nick fan-out reaches both windows"
        );
        assert_eq!(
            registry.senders_by_principal(p).len(),
            2,
            "principal fan-out reaches both windows (editor-open fallback)"
        );

        // Detaching one window leaves the other addressable.
        registry.detach("win-a");
        assert_eq!(registry.count(), 1);
        assert!(registry.get_invoke_sender_by_instance("win-b").is_some());
    }

    /// `senders_by_principal` is principal-scoped: a different principal's
    /// window is not in the fan-out.
    #[test]
    fn senders_by_principal_is_scoped() {
        let mut registry = PeerRegistry::new();
        let amy = PrincipalId::new();
        let other = PrincipalId::new();
        let (tx1, _r1) = mpsc::channel(8);
        let (tx2, _r2) = mpsc::channel(8);

        registry
            .attach(cfg_inst("kaijutsu-app", "amy-1", amy), Some(tx1))
            .unwrap();
        registry
            .attach(cfg_inst("kaijutsu-app", "other-1", other), Some(tx2))
            .unwrap();

        assert_eq!(registry.senders_by_principal(amy).len(), 1);
        assert_eq!(registry.senders_by_principal(other).len(), 1);
        assert_eq!(registry.senders_by_principal(PrincipalId::new()).len(), 0);
    }

    /// `reap_closed` drops a peer whose invoke receiver has been dropped (its
    /// bridge task ended) — the belt-and-suspenders backstop to self-detach.
    #[test]
    fn reap_closed_drops_dead_channels_only() {
        let mut registry = PeerRegistry::new();
        let p = PrincipalId::new();
        let (tx_live, _rx_live) = mpsc::channel(8);
        let (tx_dead, rx_dead) = mpsc::channel(8);
        registry
            .attach(cfg_inst("kaijutsu-app", "live", p), Some(tx_live))
            .unwrap();
        registry
            .attach(cfg_inst("kaijutsu-app", "dead", p), Some(tx_dead))
            .unwrap();

        // Kill the "dead" window's receiver (as its bridge task ending would).
        drop(rx_dead);

        assert_eq!(registry.reap_closed(), 1, "only the dead channel is reaped");
        assert_eq!(registry.count(), 1);
        assert!(registry.get_invoke_sender_by_instance("live").is_some());
        assert!(registry.get_invoke_sender_by_instance("dead").is_none());
    }

    /// `get_invoke_sender(nick)` (back-compat single-target) returns the most
    /// recently attached window when several share a nick.
    #[test]
    fn get_invoke_sender_picks_most_recent_for_a_nick() {
        let mut registry = PeerRegistry::new();
        let p = PrincipalId::new();
        let (tx_a, _ra) = mpsc::channel(8);
        let (tx_b, _rb) = mpsc::channel(8);
        registry
            .attach(cfg_inst("kaijutsu-app", "win-a", p), Some(tx_a))
            .unwrap();
        // attached_at is ms-resolution; bump it so "most recent" is unambiguous.
        registry.peers.get_mut("win-a").unwrap().attached_at = 1;
        registry
            .attach(cfg_inst("kaijutsu-app", "win-b", p), Some(tx_b))
            .unwrap();
        registry.peers.get_mut("win-b").unwrap().attached_at = 2;

        // Both share the nick; the sender must resolve to win-b (newer).
        let sender = registry.get_invoke_sender("kaijutsu-app").unwrap();
        let win_b = registry.get_invoke_sender_by_instance("win-b").unwrap();
        assert!(sender.same_channel(&win_b), "expected the newest window");
    }

    /// Re-attaching with the same nick must drop the previous invoke
    /// channel — otherwise a stale bridge task could intercept invocations
    /// destined for the new peer. This is the failure mode codified by
    /// commit 323ea2e (idempotent attach).
    #[tokio::test]
    async fn test_reattach_replaces_invoke_channel() {
        let mut registry = PeerRegistry::new();
        let (tx_old, mut rx_old) = mpsc::channel(8);
        let (tx_new, mut rx_new) = mpsc::channel(8);

        registry.attach(cfg("app"), Some(tx_old)).unwrap();
        registry.attach(cfg("app"), Some(tx_new)).unwrap();

        // The registry's sender must now reach the new receiver, not the
        // old one. Send through the registry's stored sender.
        let sender = registry.get_invoke_sender("app").unwrap();
        let (reply_tx, _reply_rx) = oneshot::channel();
        sender
            .send(InvokeRequest {
                action: "ping".to_string(),
                params: vec![],
                reply: reply_tx,
            })
            .await
            .unwrap();

        // New receiver gets the message.
        let received = rx_new.try_recv();
        assert!(
            received.is_ok(),
            "new receiver should receive invocation after re-attach"
        );

        // Old receiver should be empty (sender was dropped on re-attach).
        let old = rx_old.try_recv();
        assert!(
            old.is_err(),
            "old receiver must not see invocation; got {old:?}"
        );
    }

    #[tokio::test]
    async fn test_invoke_roundtrip() {
        let mut registry = PeerRegistry::new();
        let (tx, mut rx) = mpsc::channel(32);

        registry.attach(cfg("echo"), Some(tx)).unwrap();

        // Spawn handler that echoes back the action
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                let response = format!("echo: {}", req.action);
                let _ = req.reply.send(InvokeResponse {
                    result: Ok(response.into_bytes()),
                });
            }
        });

        // Invoke
        let sender = registry.get_invoke_sender("echo").unwrap();
        let (reply_tx, reply_rx) = oneshot::channel();
        sender
            .send(InvokeRequest {
                action: "hello".to_string(),
                params: vec![],
                reply: reply_tx,
            })
            .await
            .unwrap();

        let response = reply_rx.await.unwrap();
        assert_eq!(response.result.unwrap(), b"echo: hello");
    }

    #[tokio::test]
    async fn test_invoke_disconnected() {
        let mut registry = PeerRegistry::new();
        let (tx, rx) = mpsc::channel(32);

        registry.attach(cfg("gone"), Some(tx)).unwrap();

        // Drop the receiver to simulate disconnection
        drop(rx);

        let sender = registry.get_invoke_sender("gone").unwrap();
        let (reply_tx, _reply_rx) = oneshot::channel();
        let result = sender
            .send(InvokeRequest {
                action: "test".to_string(),
                params: vec![],
                reply: reply_tx,
            })
            .await;

        assert!(result.is_err()); // channel closed
    }
}
