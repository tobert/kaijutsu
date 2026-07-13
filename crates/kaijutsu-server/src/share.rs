//! `kaijutsu-share` subsystem orchestration — the server-side half of `/r`
//! client shares (`docs/slash-r.md`, slice 1).
//!
//! Unlike the forward SFTP adapter (kernel plays the SFTP *server* role) or
//! the capnp RPC channel (`!Send`, pinned to a dedicated thread), this is the
//! **role swap**: the kernel plays the SFTP *client* role
//! (`russh_sftp::client::rawsession::RawSftpSession`) against the channel the
//! remote client just opened and is now serving its own `Handler` on. SFTP
//! futures are `Send`, so — like the forward adapter — this rides the
//! server's ambient tokio runtime, never the capnp `LocalSet`.
//!
//! This module owns the whole registration lifecycle: validate the freshly
//! dialed session (the required generation extension, the self-describing
//! `/index` manifest, each advertised share), hand it to
//! [`kaijutsu_kernel::ShareRegistry`], then wait for the channel to close and
//! unregister. [`kaijutsu_kernel::ShareRegistry`] itself stays orchestration-free
//! — it only stores validated data and serves `VfsOps` calls.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use kaijutsu_kernel::{SHARE_OP_TIMEOUT, ShareRegistry, ShareRow};
use kaijutsu_types::Principal;
use kaijutsu_types::share::{GENERATION_EXTENSION, GENERATION_EXTENSION_VERSION, parse_manifest};

use russh_sftp::client::rawsession::RawSftpSession;
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};

/// Cap on a client's `/index` manifest — generous for any reasonable share
/// count, small enough to bound registration memory against a hostile or
/// buggy client.
const MAX_MANIFEST_LEN: usize = 64 * 1024;

/// How often an idle registered session is pinged (`ShareRegistry::ping` —
/// one `LSTAT /index` under `SHARE_OP_TIMEOUT`). [`ClosedSignalStream`]
/// catches a channel that closes CLEANLY (EOF/write error reach its polls
/// because the session's internal read loop runs continuously), but a
/// silently-dead peer (network partition, suspended laptop — no FIN ever
/// arrives) leaves the read pending forever; only traffic surfaces that, and
/// an idle session generates none. The keepalive IS that traffic, so a dead
/// idle client vanishes from `/r` within one interval instead of squatting
/// until the next VFS op ("Disconnect = unmount, loud", `docs/slash-r.md`).
pub const SHARE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Wraps a stream, cancelling `cancel` the moment it observes EOF (a
/// zero-byte read) or a read/write error — the proactive disconnect signal
/// [`run_share_session`] awaits instead of discovering the channel is dead
/// only on the next VFS-driven op. `RawSftpSession` (and the free `client::run`
/// it's built on) expose no "wait until closed" hook of their own, so this is
/// the seam: same shape as `ConnectionHandler`'s `ActivityStream` in `ssh.rs`
/// (a transparent read/write passthrough that observes traffic), fed a
/// `CancellationToken` instead of a liveness timestamp.
struct ClosedSignalStream<S> {
    inner: S,
    cancel: CancellationToken,
}

impl<S> ClosedSignalStream<S> {
    fn new(inner: S, cancel: CancellationToken) -> Self {
        Self { inner, cancel }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for ClosedSignalStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        match &result {
            Poll::Ready(Ok(())) if buf.filled().len() == before => this.cancel.cancel(),
            Poll::Ready(Err(_)) => this.cancel.cancel(),
            _ => {}
        }
        result
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for ClosedSignalStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Err(_)) = &result {
            this.cancel.cancel();
        }
        result
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Drive one `kaijutsu-share` channel end-to-end: validate + register, then
/// watch for disconnect (channel-close signal OR failed keepalive) and
/// unregister. Never returns an error — every failure is logged and the
/// session simply never (or no longer) appears under `/r`, matching the
/// fail-loud-but-don't-crash-the-connection posture the rest of the SSH
/// server uses for a single misbehaving client.
pub async fn run_share_session<S>(stream: S, principal: Principal, registry: Arc<ShareRegistry>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    run_share_session_with_keepalive(stream, principal, registry, SHARE_KEEPALIVE_INTERVAL).await
}

/// [`run_share_session`] with an explicit keepalive interval — public so the
/// integration suite can drive the keepalive path on a test-scale interval
/// instead of waiting out [`SHARE_KEEPALIVE_INTERVAL`]; production always
/// enters through [`run_share_session`].
pub async fn run_share_session_with_keepalive<S>(
    stream: S,
    principal: Principal,
    registry: Arc<ShareRegistry>,
    keepalive: Duration,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let cancel = CancellationToken::new();
    let wrapped = ClosedSignalStream::new(stream, cancel.clone());
    let raw = RawSftpSession::new(wrapped);

    match register(raw, &principal, &registry).await {
        Ok((client_id, token)) => {
            log::info!(
                "share session registered: client={client_id} principal={} ({})",
                principal.username,
                principal.display_name,
            );
            let mut tick = tokio::time::interval(keepalive);
            tick.tick().await; // first tick fires immediately; drop it
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tick.tick() => {}
                }
                // One cheap wire op; the registry takes the session lock only
                // for the ping itself, never across this loop's waiting. The
                // ping is RACED against the close signal: a channel that dies
                // mid-ping trips `ClosedSignalStream` (the write fails), and
                // waiting out the ping's full op-timeout after the channel
                // already declared itself dead would just delay eviction.
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    result = registry.ping(&client_id) => {
                        if let Err(e) = result {
                            log::info!(
                                "share session for client={client_id} ({}) failed keepalive: \
                                 {e}; treating as disconnect",
                                principal.username,
                            );
                            cancel.cancel();
                            break;
                        }
                    }
                }
            }
            registry.unregister(&client_id, token).await;
            log::info!(
                "share session for client={client_id} ({}) closed; unregistered",
                principal.username,
            );
        }
        Err(e) => {
            log::warn!(
                "share session for {} ({}) refused: {e}",
                principal.username,
                principal.display_name,
            );
        }
    }
}

/// Validate a freshly connected client-role session and register it.
/// `raw` is consumed — on success it's moved into the registry (the whole
/// point of validating first); on any failure it's simply dropped, which
/// closes the underlying channel.
async fn register(
    raw: RawSftpSession,
    principal: &Principal,
    registry: &Arc<ShareRegistry>,
) -> Result<(String, Uuid), String> {
    let version = with_timeout("init", raw.init()).await?;
    if version.extensions.get(GENERATION_EXTENSION).map(String::as_str)
        != Some(GENERATION_EXTENSION_VERSION)
    {
        return Err(format!(
            "session did not advertise {GENERATION_EXTENSION}={GENERATION_EXTENSION_VERSION} \
             — version skew, refused (docs/slash-r.md \"Coherence stamp\")"
        ));
    }

    let manifest_bytes = read_whole_file(&raw, "/index").await?;
    let rows = parse_manifest(&manifest_bytes).map_err(|e| e.to_string())?;
    // `parse_manifest` already guarantees every row shares one client_id/nick
    // and the row list is non-empty.
    let client_id = rows[0].client_id.clone();
    let nick = rows[0].nick.clone();

    let mut shares = Vec::with_capacity(rows.len());
    for row in &rows {
        let path = format!("/{}", row.name);
        let attrs = with_timeout(format!("stat {path}"), raw.lstat(path.clone())).await?;
        if !attrs.attrs.is_dir() {
            return Err(format!("advertised share {path:?} is not a directory"));
        }
        shares.push(ShareRow { name: row.name.clone(), rw: row.rw });
    }

    let token = registry
        .register(client_id.clone(), principal.clone(), nick, shares, raw)
        .await
        .map_err(|e| e.to_string())?;
    Ok((client_id, token))
}

/// `OPEN`/`READ`-to-`Eof`/`CLOSE` a small file, capped at
/// [`MAX_MANIFEST_LEN`] — used once per registration to fetch `/index`.
async fn read_whole_file(raw: &RawSftpSession, path: &str) -> Result<Vec<u8>, String> {
    let handle = with_timeout(
        format!("open {path}"),
        raw.open(path.to_string(), OpenFlags::READ, FileAttributes::empty()),
    )
    .await?
    .handle;

    let mut out = Vec::new();
    loop {
        if out.len() > MAX_MANIFEST_LEN {
            let _ = raw.close(handle).await;
            return Err(format!("{path} exceeds the {MAX_MANIFEST_LEN}-byte manifest cap"));
        }
        match tokio::time::timeout(SHARE_OP_TIMEOUT, raw.read(handle.as_str(), out.len() as u64, 65536))
            .await
        {
            Ok(Ok(data)) => {
                if data.data.is_empty() {
                    break;
                }
                out.extend_from_slice(&data.data);
            }
            Ok(Err(russh_sftp::client::error::Error::Status(s))) if s.status_code == StatusCode::Eof => {
                break;
            }
            Ok(Err(e)) => {
                let _ = raw.close(handle).await;
                return Err(format!("read {path}: {e}"));
            }
            Err(_) => {
                let _ = raw.close(handle).await;
                return Err(format!("read {path}: timed out"));
            }
        }
    }
    let _ = with_timeout(format!("close {path}"), raw.close(handle)).await;
    Ok(out)
}

/// Run one raw SFTP client call under [`SHARE_OP_TIMEOUT`], stringifying
/// both a timeout and a protocol error into one `Result<_, String>` — this
/// module reports failures as log lines, not typed errors, since a refused
/// registration has exactly one consumer (the log) and one outcome (the
/// channel closes).
async fn with_timeout<T, E: std::fmt::Display>(
    op: impl std::fmt::Display,
    fut: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, String> {
    match tokio::time::timeout(SHARE_OP_TIMEOUT, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(format!("{op}: {e}")),
        Err(_) => Err(format!("{op}: timed out")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_client::{ShareArg, ShareHandler, ShareServerConfig};

    /// Wire a client-role `run_share_session` against a real
    /// `kaijutsu-client::ShareHandler` over an in-process duplex pipe — no
    /// SSH needed. Exercises the full registration handshake end to end.
    #[tokio::test]
    async fn registers_a_well_formed_client_session() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"hello share").unwrap();
        let args = vec![ShareArg { name: "share".to_string(), path: dir.path().to_path_buf(), rw: false }];
        let config = ShareServerConfig::new(&args, "client-xyz", "test-nick").unwrap();
        let handler = ShareHandler::new(config);

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        russh_sftp::server::run(client_io, handler).await;

        let registry = Arc::new(ShareRegistry::new());
        let principal = Principal::new("amy", "Amy Tobey");
        let registry_clone = registry.clone();
        let handle = tokio::spawn(run_share_session(server_io, principal, registry_clone));

        // Give the registration handshake a moment to complete, then check
        // the registry reflects it.
        for _ in 0..50 {
            if registry.is_live("client-xyz").await {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(registry.is_live("client-xyz").await, "client-xyz should be registered");
        let shares = registry.shares_of("client-xyz").await.unwrap();
        assert_eq!(shares.len(), 1);
        assert_eq!(shares[0].name, "share");

        drop(handle); // end the test without waiting for disconnect teardown
    }

    #[tokio::test]
    async fn a_session_missing_the_generation_extension_is_refused() {
        // A bare-bones handler that answers INIT without the required
        // extension — simulates version skew (an older/foreign SFTP server
        // dialed as a share by mistake).
        struct BareHandler;
        impl russh_sftp::server::Handler for BareHandler {
            type Error = russh_sftp::server::StatusReply;
            fn unimplemented(&self) -> Self::Error {
                russh_sftp::server::StatusReply::new(russh_sftp::protocol::StatusCode::OpUnsupported)
            }
        }

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        russh_sftp::server::run(client_io, BareHandler).await;

        let registry = Arc::new(ShareRegistry::new());
        let principal = Principal::system();
        run_share_session(server_io, principal, registry.clone()).await;

        assert!(registry.live_clients().await.is_empty(), "an unversioned session must never register");
    }
}
