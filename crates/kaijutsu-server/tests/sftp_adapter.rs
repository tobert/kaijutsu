//! End-to-end tests for the SFTP adapter, driven by russh-sftp's *own client*
//! over an in-memory duplex pipe — real SFTP protocol, no SSH transport. This
//! exercises `SftpSession` (the `VfsOps` bridge) at the wire level: the client
//! speaks `SSH_FXP_*`, the adapter answers from a live `MountTable`.
//!
//! The read path lands first (see `docs/sftp.md` → Implementation slices); the
//! write assertions here pin the current slice boundary and flip as write
//! support arrives.

use std::sync::Arc;

use kaijutsu_kernel::{MemoryBackend, MountTable, VfsOps};
use kaijutsu_server::sftp::SftpSession;
use kaijutsu_types::Principal;

use russh_sftp::client::SftpSession as ClientSession;
use tokio::io::AsyncWriteExt;

/// Create-or-truncate `path` and write `data`, then close the handle. The
/// high-level `SftpSession::write` helper opens WRITE-only (no CREATE), so it
/// can't create a new file against a spec-correct server — this drives the
/// `create()` (CREATE|TRUNCATE|WRITE) + `File` AsyncWrite path instead.
async fn put(client: &ClientSession, path: &str, data: &[u8]) {
    let mut file = client.create(path).await.expect("create for write");
    file.write_all(data).await.expect("write_all");
    file.shutdown().await.expect("close handle");
}

/// Build a `MountTable` with an in-memory backend at `/`, seeded with a file
/// and a nested directory, plus a connected SFTP client speaking to the
/// adapter over a duplex pipe.
async fn fixture() -> ClientSession {
    let vfs = Arc::new(MountTable::new());
    vfs.mount("/", MemoryBackend::new()).await;

    vfs.write_all(std::path::Path::new("/hello.txt"), b"hello sftp\n")
        .await
        .expect("seed hello.txt");
    vfs.mkdir(std::path::Path::new("/sub"), 0o755)
        .await
        .expect("seed /sub");
    vfs.write_all(std::path::Path::new("/sub/nested.txt"), b"nested body")
        .await
        .expect("seed nested.txt");

    let (client_io, server_io) = tokio::io::duplex(64 * 1024);

    let handler = SftpSession::new(Principal::system(), vfs);
    russh_sftp::server::run(server_io, handler).await;

    ClientSession::new(client_io)
        .await
        .expect("client handshake (SSH_FXP_INIT/VERSION)")
}

#[tokio::test]
async fn realpath_of_dot_is_root() {
    let client = fixture().await;
    let resolved = client.canonicalize(".").await.expect("canonicalize .");
    assert_eq!(resolved, "/");
}

#[tokio::test]
async fn read_dir_lists_seeded_entries() {
    let client = fixture().await;
    let mut names: Vec<String> = client
        .read_dir("/")
        .await
        .expect("read_dir /")
        .map(|entry| entry.file_name())
        .collect();
    names.sort();
    assert_eq!(names, vec!["hello.txt".to_string(), "sub".to_string()]);
}

#[tokio::test]
async fn read_returns_file_contents() {
    let client = fixture().await;
    let body = client.read("/hello.txt").await.expect("read hello.txt");
    assert_eq!(body, b"hello sftp\n");

    let nested = client
        .read("/sub/nested.txt")
        .await
        .expect("read nested.txt");
    assert_eq!(nested, b"nested body");
}

#[tokio::test]
async fn metadata_reports_size_and_type() {
    let client = fixture().await;

    let file = client.metadata("/hello.txt").await.expect("stat hello.txt");
    assert_eq!(file.size, Some("hello sftp\n".len() as u64));
    assert!(file.is_regular());
    assert!(!file.is_dir());

    let dir = client.metadata("/sub").await.expect("stat /sub");
    assert!(dir.is_dir());
}

#[tokio::test]
async fn missing_file_is_no_such_file() {
    let client = fixture().await;
    let err = client.read("/nope.txt").await.expect_err("missing read");
    // russh-sftp surfaces the server status code in the error; just assert it
    // failed rather than hanging or returning empty.
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("no such file") || msg.contains("nosuchfile"),
        "unexpected error for missing file: {msg}"
    );
}

#[tokio::test]
async fn write_creates_and_overwrites() {
    let client = fixture().await;

    // Create a new file.
    put(&client, "/fresh.txt", b"first body").await;
    assert_eq!(
        client.read("/fresh.txt").await.expect("read back"),
        b"first body"
    );

    // Overwrite an existing file (create+truncate+write); truncate means no
    // stale tail from the longer original survives.
    put(&client, "/hello.txt", b"replaced").await;
    assert_eq!(
        client.read("/hello.txt").await.expect("read back"),
        b"replaced"
    );
}

#[tokio::test]
async fn mkdir_remove_rename_round_trip() {
    let client = fixture().await;

    client.create_dir("/newdir").await.expect("mkdir /newdir");
    assert!(client.metadata("/newdir").await.expect("stat").is_dir());

    put(&client, "/newdir/a.txt", b"aaa").await;
    client
        .rename("/newdir/a.txt", "/newdir/b.txt")
        .await
        .expect("rename");
    assert_eq!(
        client.read("/newdir/b.txt").await.expect("read renamed"),
        b"aaa"
    );

    client
        .remove_file("/newdir/b.txt")
        .await
        .expect("remove file");
    client.remove_dir("/newdir").await.expect("rmdir");
    assert!(!client.try_exists("/newdir").await.expect("exists check"));
}

#[tokio::test]
async fn setstat_resizes_via_truncate() {
    let client = fixture().await;
    let mut meta = russh_sftp::protocol::FileAttributes::empty();
    meta.size = Some(4);
    client
        .set_metadata("/hello.txt", meta)
        .await
        .expect("setstat size");
    let body = client.read("/hello.txt").await.expect("read truncated");
    assert_eq!(body, b"hell");
}

#[tokio::test]
async fn writes_to_etc_rc_are_refused() {
    // Until the SFTP session carries a capability binding (slice 3), a write to
    // the capability-gated trees fails loud rather than bypassing the gate.
    let vfs = Arc::new(MountTable::new());
    vfs.mount("/", MemoryBackend::new()).await;
    vfs.mkdir(std::path::Path::new("/etc"), 0o755).await.unwrap();
    vfs.mkdir(std::path::Path::new("/etc/rc"), 0o755).await.unwrap();

    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    russh_sftp::server::run(server_io, SftpSession::new(Principal::system(), vfs)).await;
    let client = ClientSession::new(client_io).await.expect("handshake");

    // `File` isn't `Debug`, so match rather than `expect_err`.
    let err = match client.create("/etc/rc/evil.kai").await {
        Ok(_) => panic!("etc/rc create must be refused"),
        Err(e) => e,
    };
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("permission") || msg.contains("capability-gated"),
        "unexpected etc/rc error: {msg}"
    );
}
