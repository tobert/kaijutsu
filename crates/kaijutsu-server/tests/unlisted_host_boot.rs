//! Booting a kernel leaves the host's `/proc` and `/sys` out of the root
//! listing (`docs/mounts.md`, "Unlisted paths").
//!
//! The kernel test (`a_recursive_walk_from_above_skips_an_unlisted_path`)
//! covers what an unlisted path does to a walk; this covers the half only a
//! real boot can show, that the production kernel unlists them.

mod support;

use kaijutsu_kernel::vfs::VfsOps;
use kaijutsu_server::config_mounts::ConfigMounts;
use std::path::Path;

/// A walk from `/` reaches neither tree, and naming either still does.
/// Linux only: the trees are the host's own.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_hosts_proc_and_sys_are_unlisted_but_reachable_by_name() {
    let root = tempfile::tempdir().expect("config root");
    let data = tempfile::tempdir().expect("data dir");
    let mounts = ConfigMounts::new(root.path());

    support::init_root(data.path(), "tester");
    let shared = kaijutsu_server::rpc::create_shared_kernel(None, &mounts, Some(data.path()), &[])
        .await
        .expect("kernel boots");
    let vfs = shared.kernel.vfs();

    let listed = vfs.readdir(Path::new("/")).await.expect("/ lists");
    let names: Vec<&str> = listed.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"usr"), "the host root still lists: {names:?}");
    for hidden in ["proc", "sys"] {
        assert!(!names.contains(&hidden), "/ must leave {hidden} out: {names:?}");
    }

    let status = vfs
        .read_all(Path::new("/proc/self/status"))
        .await
        .expect("/proc reads by name");
    assert!(status.starts_with(b"Name:"), "{}", String::from_utf8_lossy(&status));
    let sys = vfs.readdir(Path::new("/sys")).await.expect("/sys lists by name");
    assert!(sys.iter().any(|e| e.name == "kernel"), "{sys:?}");
}
