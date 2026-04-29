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

use tokio::sync::{mpsc, oneshot};

/// Configuration for attaching a peer.
#[derive(Debug, Clone)]
pub struct PeerConfig {
    /// Stable address (e.g. "kaijutsu-app").
    pub nick: String,
}

/// Information about an attached peer.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub nick: String,
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
            attached_at: now,
        }
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

    /// Attach a peer to this registry.
    ///
    /// The optional `invoke_sender` enables kernel → peer invocation.
    /// If the nick already exists, replaces the old registration: the
    /// previous invoke channel is dropped (closing the previous bridge
    /// task) before the new sender is installed.
    pub fn attach(
        &mut self,
        config: PeerConfig,
        invoke_sender: Option<mpsc::Sender<InvokeRequest>>,
    ) -> Result<PeerInfo, PeerError> {
        if self.peers.contains_key(&config.nick) {
            tracing::info!(
                nick = %config.nick,
                "Peer re-attaching (replacing previous registration)",
            );
            self.invoke_senders.remove(&config.nick);
        }

        let info = PeerInfo::from_config(config);
        let nick = info.nick.clone();
        self.peers.insert(nick.clone(), info.clone());
        if let Some(sender) = invoke_sender {
            self.invoke_senders.insert(nick, sender);
        }
        Ok(info)
    }

    /// Detach a peer from this registry.
    pub fn detach(&mut self, nick: &str) -> Option<PeerInfo> {
        self.invoke_senders.remove(nick);
        self.peers.remove(nick)
    }

    /// Get the invoke sender for a peer (if it registered one).
    pub fn get_invoke_sender(&self, nick: &str) -> Option<mpsc::Sender<InvokeRequest>> {
        self.invoke_senders.get(nick).cloned()
    }

    /// Get a peer by nick.
    pub fn get(&self, nick: &str) -> Option<&PeerInfo> {
        self.peers.get(nick)
    }

    /// List all attached peers.
    pub fn list(&self) -> Vec<&PeerInfo> {
        self.peers.values().collect()
    }

    /// Number of attached peers.
    pub fn count(&self) -> usize {
        self.peers.len()
    }

    /// Check if a peer with this nick is attached.
    pub fn contains(&self, nick: &str) -> bool {
        self.peers.contains_key(nick)
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

    #[test]
    fn test_attach_detach() {
        let mut registry = PeerRegistry::new();

        let config = PeerConfig {
            nick: "kaijutsu-app".to_string(),
        };

        let info = registry.attach(config.clone(), None).unwrap();
        assert_eq!(info.nick, "kaijutsu-app");

        // Re-attaching same nick replaces the old registration
        let info2 = registry.attach(config, None).unwrap();
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

        let config = PeerConfig {
            nick: "app".to_string(),
        };

        registry.attach(config, Some(tx)).unwrap();
        assert!(registry.get_invoke_sender("app").is_some());
        assert!(registry.get_invoke_sender("nonexistent").is_none());
    }

    #[test]
    fn test_detach_cleans_sender() {
        let mut registry = PeerRegistry::new();
        let (tx, _rx) = mpsc::channel(32);

        let config = PeerConfig {
            nick: "app".to_string(),
        };

        registry.attach(config, Some(tx)).unwrap();
        assert!(registry.get_invoke_sender("app").is_some());

        registry.detach("app");
        assert!(registry.get_invoke_sender("app").is_none());
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

        registry
            .attach(
                PeerConfig {
                    nick: "app".to_string(),
                },
                Some(tx_old),
            )
            .unwrap();

        registry
            .attach(
                PeerConfig {
                    nick: "app".to_string(),
                },
                Some(tx_new),
            )
            .unwrap();

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

        let config = PeerConfig {
            nick: "echo".to_string(),
        };
        registry.attach(config, Some(tx)).unwrap();

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

        let config = PeerConfig {
            nick: "gone".to_string(),
        };
        registry.attach(config, Some(tx)).unwrap();

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
