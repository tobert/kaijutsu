//! The server refuses to start against a pre-melt `auth.db`.
//!
//! `AuthDb::open`'s `CREATE TABLE IF NOT EXISTS` is a no-op against an
//! existing `principals` table, so a binary carrying the keyring melt opens
//! an unmigrated file without complaint: existing fingerprints keep
//! authenticating (the lookup never reads the dropped columns), while every
//! path that mints a principal — the seed character, anonymous
//! auto-register — fails later on the legacy `NOT NULL username`
//! constraint. That is a half-working kernel discovered at the worst
//! moment. Refuse at boot instead, and name the migration that fixes it.

use std::time::Duration;

use kaijutsu_server::auth_db::AuthDb;
use kaijutsu_server::ssh::{SshServer, SshServerConfig};

/// The pre-melt `principals` shape, as a deployed installation's file
/// carries it. Only the column the guard keys on has to be right.
const LEGACY_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS principals (
    id BLOB NOT NULL PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
"#;

/// Start a server against `auth_db` and report what boot did within a
/// second: `Some(message)` when it refused, `None` when it came up and kept
/// running. Bounded on purpose — a server that starts never returns, so an
/// unbounded await would turn a broken guard into a hung test instead of a
/// failing one.
async fn boot_outcome(auth_db_path: std::path::PathBuf) -> Option<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = SshServerConfig::ephemeral(listener.local_addr().unwrap().port());
    config.auth_db_path = Some(auth_db_path);

    tokio::task::LocalSet::new()
        .run_until(async move {
            let server =
                tokio::task::spawn_local(
                    async move { SshServer::new(config).run_on_listener(listener).await },
                );
            match tokio::time::timeout(Duration::from_secs(1), server).await {
                Ok(joined) => match joined.expect("server task must not panic") {
                    Err(e) => Some(e.to_string()),
                    Ok(()) => panic!("run_on_listener returned Ok before the listener closed"),
                },
                Err(_) => None,
            }
        })
        .await
}

#[tokio::test]
async fn server_refuses_to_start_on_a_pre_melt_auth_db() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.db");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(LEGACY_SCHEMA)
        .unwrap();

    let msg = boot_outcome(path).await.expect(
        "a pre-melt auth.db must stop the server at boot, not later at the first \
         principal mint",
    );
    assert!(
        msg.contains("migrate-keyring"),
        "the refusal must name the command that fixes it, got: {msg}"
    );
}

/// The companion: a melted database starts normally, so the guard cannot
/// pass by refusing everything.
#[tokio::test]
async fn server_starts_on_a_melted_auth_db() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.db");
    // `AuthDb::open` on a fresh file writes the post-melt schema.
    drop(AuthDb::open(&path).unwrap());

    if let Some(msg) = boot_outcome(path).await {
        panic!("a melted auth.db must not stop the server, got: {msg}");
    }
}
