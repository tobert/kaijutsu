//! End-to-end integration tests for shell dispatch through `EmbeddedKaish`.
//!
//! Exercises the kaish → kernel shell path with a tempdir-backed VFS so
//! `ls`, `cat`, `echo` etc. resolve deterministically regardless of host.

use std::sync::Arc;

use kaijutsu_crdt::{ContextId, PrincipalId};
use kaijutsu_kernel::{Kernel, LocalBackend, shared_block_store};
use kaijutsu_server::EmbeddedKaish;
use kaijutsu_types::DocKind;

/// Test filesystem fixture rooted in a tempdir.
///
/// Builds a realistic filesystem layout so `ls`, `cat`, etc. work reliably
/// regardless of host system state:
///
/// ```text
/// {tempdir}/
/// ├── home/kaiju/
/// │   └── src/kaijutsu/
/// │       ├── Cargo.toml
/// │       └── README.md
/// └── tmp/
/// ```
struct TestFs {
    _tmpdir: tempfile::TempDir,
    root: std::path::PathBuf,
    home: std::path::PathBuf,
    project: std::path::PathBuf,
}

impl TestFs {
    fn new() -> Self {
        let tmpdir = tempfile::tempdir().unwrap();
        let root = tmpdir.path().to_path_buf();

        let home = root.join("home/kaiju");
        let project = home.join("src/kaijutsu");
        let tmp = root.join("tmp");

        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&tmp).unwrap();

        // Seed with known files
        std::fs::write(
            project.join("Cargo.toml"),
            "[package]\nname = \"kaijutsu\"\n",
        )
        .unwrap();
        std::fs::write(project.join("README.md"), "# Kaijutsu\n").unwrap();

        Self {
            _tmpdir: tmpdir,
            root,
            home,
            project,
        }
    }
}

/// Create an EmbeddedKaish with a self-contained test filesystem.
///
/// Mounts the test fixture into the kernel's VFS so `ls`, `cat`, etc. resolve
/// against the tempdir, not the host system.
async fn setup_shell_e2e(fs: &TestFs, project_root: Option<std::path::PathBuf>) -> EmbeddedKaish {
    let kernel = Arc::new(Kernel::new("e2e-shell", None).await);
    let documents = shared_block_store(PrincipalId::system());

    documents
        .create_document(ContextId::new(), DocKind::Conversation, None)
        .unwrap();

    // Mount the test filesystem — mirrors real server setup but rooted in tempdir
    kernel.mount("/", LocalBackend::read_only(&fs.root)).await;
    kernel
        .mount(
            &format!("{}", fs.home.join("src").display()),
            LocalBackend::new(fs.home.join("src")),
        )
        .await;
    kernel
        .mount("/tmp", LocalBackend::new(fs.root.join("tmp")))
        .await;

    EmbeddedKaish::new("e2e-shell", documents, kernel, project_root)
        .expect("EmbeddedKaish::new failed")
}

#[tokio::test]
async fn test_ls_through_embedded_kaish() {
    let fs = TestFs::new();
    let kaish = setup_shell_e2e(&fs, None).await;
    let result = kaish
        .execute(&format!("ls {}", fs.project.display()))
        .await
        .unwrap();

    assert_eq!(result.code, 0, "ls failed: {}", result.err);
    assert!(
        result.text_out().contains("Cargo.toml"),
        "expected Cargo.toml in ls output, got: {}",
        result.text_out()
    );
}

#[tokio::test]
async fn test_ls_tmp() {
    let fs = TestFs::new();
    // Put a file in /tmp so there's something to see
    std::fs::write(fs.root.join("tmp/scratch.txt"), "temp\n").unwrap();

    let kaish = setup_shell_e2e(&fs, None).await;
    let result = kaish.execute("ls /tmp").await.unwrap();

    assert_eq!(result.code, 0, "ls /tmp failed: {}", result.err);
    assert!(
        result.text_out().contains("scratch.txt"),
        "expected scratch.txt in ls /tmp output, got: {}",
        result.text_out()
    );
}

#[tokio::test]
async fn test_rapid_shell_commands() {
    let fs = TestFs::new();
    let kaish = setup_shell_e2e(&fs, None).await;

    let r1 = kaish.execute("echo a").await.unwrap();
    let r2 = kaish.execute("echo b").await.unwrap();
    let r3 = kaish.execute("echo c").await.unwrap();

    assert_eq!(r1.code, 0, "echo a failed: {}", r1.err);
    assert_eq!(r2.code, 0, "echo b failed: {}", r2.err);
    assert_eq!(r3.code, 0, "echo c failed: {}", r3.err);

    assert_eq!(r1.text_out().trim(), "a");
    assert_eq!(r2.text_out().trim(), "b");
    assert_eq!(r3.text_out().trim(), "c");
}

#[tokio::test]
async fn test_shell_command_with_project_root() {
    let fs = TestFs::new();
    let kaish = setup_shell_e2e(&fs, Some(fs.project.clone())).await;
    let result = kaish.execute("ls").await.unwrap();

    assert_eq!(result.code, 0, "ls failed: {}", result.err);
    assert!(
        result.text_out().contains("Cargo.toml"),
        "expected Cargo.toml in ls output (cwd=project), got: {}",
        result.text_out()
    );
    assert!(
        result.text_out().contains("README.md"),
        "expected README.md in ls output (cwd=project), got: {}",
        result.text_out()
    );
}
