//! Booting a kernel against a `/config` mount registry
//! (`docs/config-namespace.md`).
//!
//! The registry's unit tests cover path resolution; these cover the half only
//! a real boot can show — that every declared tree is actually mounted, that a
//! fresh tree is seeded onto the host filesystem, and that `/config` lists its
//! children with nothing serving `/config` itself.

use kaijutsu_kernel::vfs::VfsOps;
use kaijutsu_server::config_mounts::ConfigMounts;
use kaijutsu_types::paths::{CLIENT_ROOT, CONFIG_NAMESPACE_ROOT, CONFIG_ROOT, CONFIG_TREES, MIDI_ROOT, RC_ROOT};
use std::path::Path;

/// Every config tree mounts, seeds onto the host filesystem, and reads back
/// through the VFS at its well-known name.
///
/// Seeding onto *disk* is the half that would silently regress into the old
/// document-backed behavior: a tree could read correctly through the VFS while
/// nothing existed on the filesystem for `vim` or git to reach.
#[tokio::test]
async fn every_config_tree_mounts_and_seeds_onto_the_host_filesystem() {
    let root = tempfile::tempdir().expect("config root");
    let data = tempfile::tempdir().expect("data dir");
    let mounts = ConfigMounts::new(root.path());

    let shared = kaijutsu_server::rpc::create_shared_kernel(None, &mounts, Some(data.path()))
        .await
        .expect("kernel boots against a fresh config root");
    let vfs = shared.kernel.vfs();

    for tree in CONFIG_TREES {
        let host = mounts.host_dir(tree);
        assert!(host.is_dir(), "{tree} must exist on disk at {}", host.display());
        let listed = vfs
            .readdir(Path::new(tree))
            .await
            .unwrap_or_else(|e| panic!("{tree} must be readable through the VFS: {e}"));
        assert!(!listed.is_empty(), "{tree} seeded nothing");
    }

    // A specific file, both ways: on disk for `vim`/git, and through the VFS
    // at its canonical path. Melting means both are the same bytes.
    let theme_host = mounts.host_dir(CONFIG_ROOT).join("theme.toml");
    assert!(theme_host.is_file(), "theme.toml must be a real file");
    let via_vfs = vfs
        .read_all(Path::new(&kaijutsu_types::paths::config_path("theme.toml")))
        .await
        .expect("theme.toml reads through the VFS");
    assert_eq!(
        via_vfs,
        std::fs::read(&theme_host).unwrap(),
        "the VFS and the host file must be one thing, not two copies"
    );
}

/// The host's `/etc` is refused like any other read-only host path, with no
/// guard of its own.
///
/// `deny_etc_write` used to draw a line *inside* `/etc`, between kaijutsu's
/// four mounts and the host's real files. With the mounts moved to `/config`
/// there is no line to draw, and singling `/etc` out would be arbitrary —
/// `/usr` and `/boot` are equally the host's and equally protected by the
/// read-only `/` mount. This pins that the uniform mechanism actually covers
/// it, which is what made deleting the guard safe rather than a hole.
#[tokio::test]
async fn the_hosts_etc_is_refused_by_the_read_only_root_like_any_host_path() {
    let root = tempfile::tempdir().expect("config root");
    let data = tempfile::tempdir().expect("data dir");
    let mounts = ConfigMounts::new(root.path());

    let shared = kaijutsu_server::rpc::create_shared_kernel(None, &mounts, Some(data.path()))
        .await
        .expect("kernel boots");
    let vfs = shared.kernel.vfs();

    for host_path in ["/etc/kaijutsu-probe.conf", "/usr/kaijutsu-probe", "/boot/kaijutsu-probe"] {
        let err = vfs
            .write_all(Path::new(host_path), b"nope")
            .await
            .expect_err("a write under the read-only host root must be refused");
        assert!(
            matches!(err, kaijutsu_kernel::vfs::VfsError::ReadOnly),
            "{host_path} must be refused as read-only, got {err:?}"
        );
        assert!(
            !std::path::Path::new(host_path).exists(),
            "{host_path} must not have been created"
        );
    }
}

/// `/config` has no backend of its own and still lists its children, because
/// the mount table merges synthetic mount points into a parent listing. This
/// is what makes the namespace browsable without a mount that owns it.
#[tokio::test]
async fn the_config_namespace_lists_its_trees_with_no_backend_of_its_own() {
    let root = tempfile::tempdir().expect("config root");
    let data = tempfile::tempdir().expect("data dir");
    let mounts = ConfigMounts::new(root.path());

    let shared = kaijutsu_server::rpc::create_shared_kernel(None, &mounts, Some(data.path()))
        .await
        .expect("kernel boots");

    let listed = shared
        .kernel
        .vfs()
        .readdir(Path::new(CONFIG_NAMESPACE_ROOT))
        .await
        .expect("/config lists");
    let names: Vec<&str> = listed.iter().map(|e| e.name.as_str()).collect();
    for tree in CONFIG_TREES {
        let want = kaijutsu_types::paths::config_tree_name(tree).unwrap();
        assert!(names.contains(&want), "/config must list {want}: {names:?}");
    }
}

/// A declared tree is read from where it was declared, and the others stay
/// put. This is the registry's whole reason to exist — pointing one tree at a
/// checkout or a fixture without moving the rest.
///
/// Falsified by having the bootstrap mount `root/<name>` unconditionally
/// instead of asking the registry: the declared directory stays empty and the
/// assertion on its contents trips.
#[tokio::test]
async fn a_declared_tree_is_mounted_where_it_was_declared() {
    let root = tempfile::tempdir().expect("config root");
    let elsewhere = tempfile::tempdir().expect("declared rc dir");
    let data = tempfile::tempdir().expect("data dir");

    let mut mounts = ConfigMounts::new(root.path());
    mounts.set(RC_ROOT, elsewhere.path()).unwrap();

    let shared = kaijutsu_server::rpc::create_shared_kernel(None, &mounts, Some(data.path()))
        .await
        .expect("kernel boots with a declared tree");

    assert!(
        elsewhere.path().join("coder").is_dir(),
        "rc must have seeded into the DECLARED directory"
    );
    assert!(
        !root.path().join("rc").exists(),
        "the default rc location must be untouched when rc is declared elsewhere"
    );
    // The other trees still default under the root.
    for tree in [CONFIG_ROOT, CLIENT_ROOT, MIDI_ROOT] {
        let name = kaijutsu_types::paths::config_tree_name(tree).unwrap();
        assert!(
            root.path().join(name).is_dir(),
            "{tree} must stay at its default while rc is declared elsewhere"
        );
    }

    // And the kernel reads the declared directory, not the default one.
    let listed = shared
        .kernel
        .vfs()
        .readdir(Path::new(RC_ROOT))
        .await
        .expect("declared rc reads");
    assert!(!listed.is_empty(), "the declared rc tree serves its contents");
}
