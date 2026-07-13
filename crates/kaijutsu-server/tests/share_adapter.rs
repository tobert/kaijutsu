//! End-to-end test for `/r` client shares (`docs/slash-r.md`, slice 1),
//! driven by the REAL `kaijutsu-client::ShareHandler` over an in-process
//! duplex pipe — no SSH needed. Mirrors `tests/sftp_adapter.rs`'s harness
//! shape, reversed: here the CLIENT crate plays the SFTP *server* role and
//! the KERNEL (`ShareFs`/`ShareRegistry`) plays the SFTP *client* role.

use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use kaijutsu_client::{ShareArg, ShareHandler, ShareServerConfig};
use kaijutsu_kernel::{ShareFs, ShareRegistry, VfsError, VfsOps};
use kaijutsu_server::share::{run_share_session, run_share_session_with_keepalive};
use kaijutsu_types::Principal;

/// [`Killable`] stream modes (stored as an `AtomicU8`).
const MODE_LIVE: u8 = 0;
/// Reads return EOF, writes error — a peer that closed CLEANLY.
const MODE_EOF: u8 = 1;
/// Reads pend FOREVER, writes error — a peer that silently died (network
/// partition, suspended laptop): no FIN ever arrives, so nothing on the
/// read path can notice; only attempted TRAFFIC (the keepalive's write)
/// surfaces the death.
const MODE_WEDGED: u8 = 2;

/// Wraps a stream so a test can force it dead from the OUTSIDE, even though
/// `russh_sftp::server::run` (and the client-role `RawSftpSession`) take
/// ownership of the stream into internally spawned tasks with no exposed
/// `JoinHandle` (aborting a task that merely CALLED `run()` does nothing —
/// `run()` returns as soon as it spawns). [`KillSwitch::kill`]/[`KillSwitch::wedge`]
/// flip the mode AND wake the last-stored waker, so a task currently parked
/// in `poll_read` (waiting for the next SFTP packet, the common idle case)
/// re-polls immediately.
struct Killable<S> {
    inner: S,
    mode: Arc<AtomicU8>,
    waker: Arc<Mutex<Option<Waker>>>,
}

#[derive(Clone)]
struct KillSwitch {
    mode: Arc<AtomicU8>,
    waker: Arc<Mutex<Option<Waker>>>,
}

impl KillSwitch {
    fn set(&self, mode: u8) {
        self.mode.store(mode, Ordering::SeqCst);
        if let Some(w) = self.waker.lock().unwrap().take() {
            w.wake();
        }
    }
    /// Clean close: next read is EOF.
    fn kill(&self) {
        self.set(MODE_EOF);
    }
    /// Silent death: reads pend forever, writes fail.
    fn wedge(&self) {
        self.set(MODE_WEDGED);
    }
}

fn killable<S>(inner: S) -> (Killable<S>, KillSwitch) {
    let mode = Arc::new(AtomicU8::new(MODE_LIVE));
    let waker = Arc::new(Mutex::new(None));
    (
        Killable { inner, mode: mode.clone(), waker: waker.clone() },
        KillSwitch { mode, waker },
    )
}

impl<S: AsyncRead + Unpin> AsyncRead for Killable<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        match this.mode.load(Ordering::SeqCst) {
            // Zero-byte read (buf unchanged) == EOF.
            MODE_EOF => return Poll::Ready(Ok(())),
            MODE_WEDGED => {
                // No EOF, no error, no data — ever. (Waker deliberately NOT
                // stored: nothing will make this readable again.)
                return Poll::Pending;
            }
            _ => {}
        }
        *this.waker.lock().unwrap() = Some(cx.waker().clone());
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Killable<S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if this.mode.load(Ordering::SeqCst) != MODE_LIVE {
            return Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "killed")));
        }
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Wire a real `ShareHandler` (serving `dir` under `share_name`) to a
/// registered `ShareFs`, over an in-process duplex pipe. Returns the `ShareFs`
/// façade plus the live client id `ShareFs` paths route through.
async fn fixture(dir: &Path, share_name: &str) -> (ShareFs, String) {
    let client_id = format!("client-{}", uuid::Uuid::new_v4());
    let args = vec![ShareArg {
        name: share_name.to_string(),
        path: dir.to_path_buf(),
        rw: false,
    }];
    let config = ShareServerConfig::new(&args, client_id.clone(), "test-nick").unwrap();
    let handler = ShareHandler::new(config);

    let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
    russh_sftp::server::run(client_io, handler).await;

    let registry = Arc::new(ShareRegistry::new());
    let principal = Principal::new("amy", "Amy Tobey");
    tokio::spawn(run_share_session(server_io, principal, registry.clone()));

    wait_for(|| {
        let registry = registry.clone();
        let client_id = client_id.clone();
        async move { registry.is_live(&client_id).await }
    })
    .await;

    (ShareFs::new(registry), client_id)
}

/// Poll `pred` every 20ms for up to 2s — the registration handshake and
/// disconnect detection both cross an async task boundary, so tests can't
/// assert on them synchronously.
async fn wait_for<F, Fut>(mut pred: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..100 {
        if pred().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition never became true within 2s");
}

#[tokio::test]
async fn readdir_lists_the_client_the_share_and_its_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), b"hi").unwrap();
    let (fs, client_id) = fixture(dir.path(), "downloads").await;

    let root_entries = fs.readdir(Path::new("")).await.unwrap();
    assert!(root_entries.iter().any(|e| e.name == client_id));
    assert!(root_entries.iter().any(|e| e.name == "index"));

    let client_entries = fs.readdir(Path::new(&client_id)).await.unwrap();
    assert!(client_entries.iter().any(|e| e.name == "downloads"));

    let share_entries = fs
        .readdir(Path::new(&format!("{client_id}/downloads")))
        .await
        .unwrap();
    assert!(share_entries.iter().any(|e| e.name == "hello.txt"));
}

#[tokio::test]
async fn read_returns_file_contents() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), b"share bytes").unwrap();
    let (fs, client_id) = fixture(dir.path(), "share").await;

    let path = Path::new(&format!("{client_id}/share/f.txt")).to_path_buf();
    let data = fs.read(&path, 0, 4096).await.unwrap();
    assert_eq!(data, b"share bytes");
}

#[tokio::test]
async fn read_all_reassembles_a_file_larger_than_the_sftp_read_window() {
    let dir = tempfile::tempdir().unwrap();
    // Bigger than the 256 KiB READ cap both adapters use — exercises the
    // read_all override's OPEN/READ-loop/CLOSE, not a single chunk.
    let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(dir.path().join("big.bin"), &big).unwrap();
    let (fs, client_id) = fixture(dir.path(), "share").await;

    let path = Path::new(&format!("{client_id}/share/big.bin")).to_path_buf();
    let data = fs.read_all(&path).await.unwrap();
    assert_eq!(data, big);
}

#[tokio::test]
async fn generation_extension_is_present_and_nanosecond_scale() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
    let (fs, client_id) = fixture(dir.path(), "share").await;

    let path = Path::new(&format!("{client_id}/share/f.txt")).to_path_buf();
    let attr = fs.getattr(&path).await.unwrap();
    // Host mtime-NANOS since epoch: any real file created well after 1970
    // yields a value far past what a bare unix-SECONDS timestamp could ever
    // be (~1.7e9 today) — this pins "nanos, not seconds" without a flaky
    // exact-value assertion.
    assert!(
        attr.generation > 1_000_000_000_000_000,
        "generation {} does not look like host mtime in nanoseconds",
        attr.generation
    );
}

#[tokio::test]
async fn generation_advances_when_the_file_is_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f.txt");
    std::fs::write(&file, b"v1").unwrap();
    let (fs, client_id) = fixture(dir.path(), "share").await;
    let path = Path::new(&format!("{client_id}/share/f.txt")).to_path_buf();

    let first = fs.getattr(&path).await.unwrap().generation;

    // A generous real-clock gap so the second write's mtime is unambiguously
    // later, even on filesystems with coarser-than-nanosecond mtime
    // resolution in a test sandbox.
    tokio::time::sleep(Duration::from_millis(50)).await;
    std::fs::write(&file, b"v2-longer-body").unwrap();

    let second = fs.getattr(&path).await.unwrap().generation;
    assert!(second > first, "generation must strictly advance: {first} -> {second}");
}

#[tokio::test]
async fn disconnect_unregisters_promptly_and_pending_ops_surface_the_dedicated_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();

    let client_id = format!("client-{}", uuid::Uuid::new_v4());
    let args = vec![ShareArg { name: "share".to_string(), path: dir.path().to_path_buf(), rw: false }];
    let config = ShareServerConfig::new(&args, client_id.clone(), "nick").unwrap();
    let handler = ShareHandler::new(config);

    let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
    // `russh_sftp::server::run` takes ownership of `client_io` and hands it
    // to a task IT spawns internally — there is no `JoinHandle` to abort to
    // simulate the client vanishing. `Killable` sits in front of it so this
    // test can force closure from the outside regardless: `switch.kill()`
    // makes the wrapped stream read EOF / fail every write, which the
    // internal loop treats exactly like a real dropped connection.
    let (killable_client_io, switch) = killable(client_io);
    tokio::spawn(async move {
        russh_sftp::server::run(killable_client_io, handler).await;
        std::future::pending::<()>().await
    });

    let registry = Arc::new(ShareRegistry::new());
    tokio::spawn(run_share_session(server_io, Principal::system(), registry.clone()));
    {
        let registry = registry.clone();
        let client_id = client_id.clone();
        wait_for(move || {
            let registry = registry.clone();
            let client_id = client_id.clone();
            async move { registry.is_live(&client_id).await }
        })
        .await;
    }

    // Simulate the client vanishing: `ClosedSignalStream`
    // (kaijutsu-server/src/share.rs) on the OTHER end must observe this
    // WITHOUT any `/r` op being attempted first.
    switch.kill();

    // "Disconnect = unmount, loud" (docs/slash-r.md): the registry must
    // notice on its own — nobody has to touch `/r` first.
    {
        let registry = registry.clone();
        let client_id = client_id.clone();
        wait_for(move || {
            let registry = registry.clone();
            let client_id = client_id.clone();
            async move { !registry.is_live(&client_id).await }
        })
        .await;
    }

    let fs = ShareFs::new(registry);
    let path = Path::new(&format!("{client_id}/share/f.txt")).to_path_buf();
    let err = fs.read_all(&path).await.unwrap_err();
    assert!(
        matches!(err, VfsError::ShareDisconnected(_)),
        "expected ShareDisconnected after the client vanished, got {err:?}"
    );
}

/// Regression (HIGH-1): `ShareFs::readlink` was a dead stub returning
/// `NotASymlink` for EVERY `/r` path — while `getattr`/`readdir` correctly
/// reported the same paths as `FileType::Symlink`. An in-jail symlink must
/// readlink to its stored target over the wire.
#[tokio::test]
async fn readlink_reports_an_in_jail_symlink_target() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("real.txt"), b"target body").unwrap();
    // Relative target, so the answer is host-layout-independent.
    std::os::unix::fs::symlink("real.txt", dir.path().join("link.txt")).unwrap();
    let (fs, client_id) = fixture(dir.path(), "share").await;

    let link_path = Path::new(&format!("{client_id}/share/link.txt")).to_path_buf();

    // getattr reports it as a symlink…
    let attr = fs.getattr(&link_path).await.unwrap();
    assert!(attr.is_symlink(), "lstat-shaped getattr must report the link itself");

    // …and readlink now backs that up with the actual target.
    let target = fs.readlink(&link_path).await.unwrap();
    assert_eq!(target, Path::new("real.txt"));

    // A regular file is still not a symlink.
    let file_path = Path::new(&format!("{client_id}/share/real.txt")).to_path_buf();
    assert!(fs.readlink(&file_path).await.is_err(), "readlink on a regular file errors");
}

/// Regression (MEDIUM-3): a SILENTLY dead idle session — reads pending
/// forever (no FIN, so no EOF for `ClosedSignalStream` to observe), writes
/// failing — must be evicted by the keepalive ping within its interval,
/// WITHOUT any VFS op touching `/r` first. This is the case only the
/// keepalive catches: the clean-close case above is detected by the
/// continuously-polled read loop seeing EOF.
#[tokio::test]
async fn a_silently_dead_idle_session_is_evicted_by_the_keepalive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();

    let client_id = format!("client-{}", uuid::Uuid::new_v4());
    let args = vec![ShareArg { name: "share".to_string(), path: dir.path().to_path_buf(), rw: false }];
    let config = ShareServerConfig::new(&args, client_id.clone(), "nick").unwrap();
    let handler = ShareHandler::new(config);

    let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
    russh_sftp::server::run(client_io, handler).await;

    // Wedge-capable wrapper on the SERVER side (the stream the registered
    // session reads/writes), keepalive at test scale.
    let (killable_server_io, switch) = killable(server_io);
    let registry = Arc::new(ShareRegistry::new());
    tokio::spawn(run_share_session_with_keepalive(
        killable_server_io,
        Principal::system(),
        registry.clone(),
        Duration::from_millis(100),
    ));
    {
        let registry = registry.clone();
        let client_id = client_id.clone();
        wait_for(move || {
            let registry = registry.clone();
            let client_id = client_id.clone();
            async move { registry.is_live(&client_id).await }
        })
        .await;
    }

    // Silent death: no EOF ever arrives; only attempted traffic (the next
    // keepalive ping's write) can surface it.
    switch.wedge();

    // The registry empties within the keepalive window — no VFS op issued.
    {
        let registry = registry.clone();
        let client_id = client_id.clone();
        wait_for(move || {
            let registry = registry.clone();
            let client_id = client_id.clone();
            async move { !registry.is_live(&client_id).await }
        })
        .await;
    }
}
