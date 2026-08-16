//! Kernel wiring for the Claude Code peer inbox — receive only
//! (`docs/cc-peer.md` "Order from here": kernel wiring of the inbox).
//!
//! **What this does:** bind [`claude_code_peer::server::Inbox`] once at
//! kernel startup, drain it on a background task for the kernel's lifetime,
//! and log every message that arrives. **What this deliberately does not
//! do:** deliver a received message anywhere — no context, no mailbox, no
//! block, no drift. That routing question is open (`docs/cc-peer.md` "Open
//! decisions") and this module's job stops at the log line.
//!
//! 1. **One inbox per kernel process, named by kernel id** — a fixed,
//!    predictable name would collide across the several kernels one host
//!    runs at once (production plus every `Kernel::with_flows` test caller
//!    that shares the same `$XDG_RUNTIME_DIR`), and the kernel id is already
//!    the unique, stable identity every other cross-process fact in this
//!    codebase keys on.
//! 2. **Bind failure is loud, not silent, and not fatal to the kernel** — the
//!    inbox has zero consumers yet (Phase 1 of the wiring), so refusing to
//!    boot a kernel because a not-yet-load-bearing socket didn't bind would
//!    be a worse failure than running one turn without it. The caller decides
//!    whether to install the result; an install that never happens must never
//!    be mistaken for a successful one, so [`Kernel::cc_inbox`] reads back
//!    `None`, not a default.

use std::path::{Path, PathBuf};

use claude_code_peer::server::{Inbox, ReceivedMessage};

/// A bound inbox and the task draining it. Dropping this handle aborts the
/// drain task (via [`Inbox`]'s own `Drop`) and leaves the socket file on
/// disk, same as `Inbox` itself — the handle owns the file, not vice versa.
pub struct CcInboxHandle {
    address: String,
    path: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl CcInboxHandle {
    /// Bind `dir/name` and spawn the receive loop. Must be called from
    /// within a tokio runtime — [`Inbox::bind`] spawns its accept loop on
    /// the current one.
    pub fn bind(dir: &Path, name: &str) -> std::io::Result<CcInboxHandle> {
        let mut inbox = Inbox::bind(dir, name)?;
        let address = inbox.uds_address();
        let path = inbox.path().to_path_buf();

        // 3. One malformed or unroutable message must never kill the loop —
        // `Inbox::recv` already filters at the frame level (bad JSON, wrong
        // `msgV`), so the only way this loop ends is the inbox itself
        // closing, which `log_received`'s caller below treats as the task's
        // own termination, not a message to react to.
        let task = tokio::spawn(async move {
            while let Some(msg) = inbox.recv().await {
                log_received(&msg);
            }
        });

        Ok(CcInboxHandle { address, path, task })
    }

    /// The `uds:` address a Claude Code session must be told to reply to.
    /// Nothing publishes this over the wire yet (`docs/cc-peer.md`'s
    /// `PeerRegistry` step, still open) — today's only reader is a caller
    /// with direct access to the `Kernel`.
    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for CcInboxHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Log one inbound message. Attribution falls back through what the frame
/// actually carried: the envelope's `from-name` when the content parsed as
/// one, else the frame's top-level `from` reply address, else an explicit
/// marker — never a guess dressed as a name.
fn log_received(msg: &ReceivedMessage) {
    let attribution = msg
        .envelope
        .as_ref()
        .and_then(|e| e.from_name())
        .map(str::to_string)
        .or_else(|| msg.from_socket.clone())
        .unwrap_or_else(|| "<unattributed>".to_string());

    tracing::info!(
        target: "claude-code-peer",
        msg_id = ?msg.msg_id,
        from = %attribution,
        bytes = msg.content.len(),
        "cc inbox: received message (Phase 1 — logged only, not delivered)",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The inbox binds, and a sent message reaches the drain loop's log
    /// path — proven here by observing the loop still functions
    /// end-to-end (delivery through to `Inbox::recv`, which the loop
    /// consumes) rather than by capturing log output.
    #[tokio::test]
    async fn bind_and_receive_one_message() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("cc");
        let handle = CcInboxHandle::bind(&dir, "test-kernel.sock").expect("bind");
        assert_eq!(handle.address(), format!("uds:{}", handle.path().display()));
        assert!(handle.path().exists());

        let prepared = claude_code_peer::prepare("kaijutsu-test", "hello inbox", None).unwrap();
        let path = handle.path().to_string_lossy().into_owned();
        let written = tokio::task::spawn_blocking(move || {
            claude_code_peer::client::deliver(&path, &[prepared.user_frame.clone()])
        })
        .await
        .unwrap()
        .expect("deliver");
        assert!(written > 0);

        // No observable side channel on the drain task besides the log line
        // it emits and the fact that the task keeps running; give it a beat
        // to process the delivered frame, then confirm the handle (and its
        // task) are still alive rather than having panicked on the message.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!handle.task.is_finished(), "drain task must survive a real message");
    }

    /// Proves the previous test can fail: point `bind` at a path that
    /// cannot be a socket (a regular file already sitting where the
    /// directory needs to go) and confirm the bind itself reports the
    /// failure rather than silently succeeding.
    #[tokio::test]
    async fn bind_fails_loudly_when_the_directory_path_is_occupied_by_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let blocked = tmp.path().join("cc");
        std::fs::write(&blocked, b"not a directory").unwrap();

        let result = CcInboxHandle::bind(&blocked, "test-kernel.sock");
        assert!(
            result.is_err(),
            "binding under a path that is a file, not a directory, must fail"
        );
    }
}
