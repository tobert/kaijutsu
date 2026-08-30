//! End-to-end tests for the SFTP adapter, driven by russh-sftp's *own client*
//! over an in-memory duplex pipe — real SFTP protocol, no SSH transport. This
//! exercises `SftpSession` (the `VfsOps` bridge) at the wire level: the client
//! speaks `SSH_FXP_*`, the adapter answers from a live `MountTable`.
//!
//! See `docs/sftp.md` → Implementation slices for how the read and write
//! paths landed.

use std::sync::Arc;

use kaijutsu_kernel::runtime::config_doc_fs::ConfigDocFs;
use kaijutsu_kernel::{MemoryBackend, MountTable, VfsOps, shared_block_store};
use kaijutsu_server::sftp::SftpSession;
use kaijutsu_types::{Principal, PrincipalId};

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
async fn read_dir_pages_a_large_directory() {
    // More entries than one READDIR chunk (64), so the client must loop
    // READDIR until Eof and the server must page rather than send one giant
    // Name packet. Confirms the full listing arrives intact.
    let vfs = Arc::new(MountTable::new());
    vfs.mount("/", MemoryBackend::new()).await;
    vfs.mkdir(std::path::Path::new("/big"), 0o755).await.unwrap();
    for i in 0..130 {
        vfs.write_all(std::path::Path::new(&format!("/big/f{i:03}.txt")), b"x")
            .await
            .unwrap();
    }

    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    russh_sftp::server::run(server_io, SftpSession::new(Principal::system(), vfs)).await;
    let client = ClientSession::new(client_io).await.expect("handshake");

    let count = client.read_dir("/big").await.expect("read_dir /big").count();
    assert_eq!(count, 130, "all entries should page through to the client");
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
async fn statvfs_extension_reports_filesystem_stats() {
    // The client only enables fs_info() if the server advertised
    // statvfs@openssh.com in the VERSION exchange — so this exercises both the
    // init advertisement and the extended() statvfs reply end to end.
    let client = fixture().await;
    let info = client
        .fs_info("/")
        .await
        .expect("statvfs call")
        .expect("server advertised statvfs@openssh.com");
    assert!(info.block_size > 0, "block_size should be populated");
    assert!(info.blocks > 0, "blocks should be populated");
}

#[tokio::test]
async fn an_sftp_write_to_etc_rc_is_an_ordinary_file_write() {
    // `/config/rc` melted from a kernel document into a host directory
    // (`docs/rc-on-disk.md`); an SFTP write there is governed by the mount's
    // own `read_only()` flag, same as any other path — no lexical deny.
    let vfs = Arc::new(MountTable::new());
    vfs.mount("/", MemoryBackend::new()).await;
    vfs.mkdir(std::path::Path::new("/etc"), 0o755).await.unwrap();
    vfs.mkdir(std::path::Path::new("/config/rc"), 0o755).await.unwrap();

    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    russh_sftp::server::run(server_io, SftpSession::new(Principal::system(), vfs)).await;
    let client = ClientSession::new(client_io).await.expect("handshake");

    put(&client, "/config/rc/evil.kai", b"echo hi\n").await;
    assert_eq!(
        client.read("/config/rc/evil.kai").await.expect("read back"),
        b"echo hi\n"
    );
}

#[tokio::test]
async fn an_sftp_write_to_etc_config_lands_in_the_kernel_document() {
    // `/config/kernel` is still a kernel document (`ConfigDocFs`), unlike `/config/rc`
    // which is now host files — so this exercises the genuinely different write
    // path: a plain SFTP create/write must land in the block store and read
    // back, the same as any other unrestricted mount.
    let vfs = Arc::new(MountTable::new());
    vfs.mount("/", MemoryBackend::new()).await;
    let blocks = shared_block_store(PrincipalId::system());
    vfs.mount("/config/kernel", ConfigDocFs::new(blocks, "/config/kernel"))
        .await;

    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    russh_sftp::server::run(server_io, SftpSession::new(Principal::system(), vfs)).await;
    let client = ClientSession::new(client_io).await.expect("handshake");

    put(&client, "/config/kernel/theme.toml", b"accent = \"teal\"\n").await;
    assert_eq!(
        client
            .read("/config/kernel/theme.toml")
            .await
            .expect("read back"),
        b"accent = \"teal\"\n"
    );
}
