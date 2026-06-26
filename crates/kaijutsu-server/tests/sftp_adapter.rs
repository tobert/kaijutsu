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
async fn write_is_not_yet_supported() {
    // Slice boundary: opening for write fails loud (no silent data loss) until
    // the write path lands. Flip this assertion when that slice ships.
    let client = fixture().await;
    let err = client
        .write("/hello.txt", b"clobber")
        .await
        .expect_err("write should be rejected this slice");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("unsupported") || msg.contains("not yet implemented"),
        "unexpected write error: {msg}"
    );
}
