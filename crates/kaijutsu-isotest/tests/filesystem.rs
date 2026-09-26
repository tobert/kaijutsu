//! Filesystem protection tests. Run via `contrib/isotest`. These tests build
//! the production kernel in-process, then dispatch native file tools through
//! its broker. They use the container's real filesystem so read-only mounts
//! and symlink resolution behave as they do in production.
//!
//! `kaijutsu_server::rpc::create_shared_kernel` supplies the production mount
//! topology: `/` is mounted
//! `LocalBackend::read_only("/")` (the whole container filesystem is
//! visible but not writable through the VFS), `~/src` and `/tmp` are
//! writable `LocalBackend` mounts.
//!
//! The suite checks clean read-only failures, refusal to walk the filesystem
//! root, and refusal of symlinks that escape a mount.

mod common;
use common::{run_local, skip_unless_isotest};

use serde_json::json;
use tokio_util::sync::CancellationToken;

struct FileKernel {
    shared: kaijutsu_server::SharedKernel,
    context: kaijutsu_types::ContextId,
    principal: kaijutsu_types::PrincipalId,
    // Owns the ephemeral data/config directory for as long as the kernel uses it.
    _config: kaijutsu_server::SshServerConfig,
}

impl FileKernel {
    /// Use the server's production builder and mount topology. The fixture
    /// grants only the native file tools it calls and denies host execution.
    async fn boot() -> Self {
        let config = kaijutsu_server::SshServerConfig::ephemeral_with_root(0, "tester");
        let shared = kaijutsu_server::rpc::create_shared_kernel(
            config.config_dir.as_deref(),
            &config.config_mounts,
            config.data_dir.as_deref(),
            &[],
        ).await.expect("build production kernel for filesystem isotest");
        let root = shared.kernel_db.lock().get_character_by_name("tester")
            .expect("read root character").expect("root character");
        let context = root.root_ctx.expect("server startup creates the root context");
        let broker = shared.kernel.broker();
        let mut binding = broker.binding(&context).await.expect("root lifecycle binding");
        binding.revoke_cap(&kaijutsu_kernel::mcp::Capability::Exec);
        for tool in ["read", "write", "edit", "glob", "grep"] {
            binding.grant(kaijutsu_kernel::mcp::Capability::Tool {
                instance: kaijutsu_kernel::mcp::InstanceId::new("builtin.file"),
                tool: tool.to_string(),
            });
        }
        broker.set_binding(context, binding).await.expect("grant native file tools");
        let binding = broker.binding(&context).await.expect("persisted filesystem binding");
        assert!(!binding.has_authority("exec"), "filesystem fixture must not permit host execution");
        Self { shared, context, principal: root.principal_id, _config: config }
    }

    async fn call(&self, tool: &str, args: serde_json::Value) -> FileToolResult {
        let call = kaijutsu_kernel::ExecContext::new(
            self.principal,
            self.context,
            "/tmp",
            kaijutsu_types::SessionId::new(),
            self.shared.id,
        );
        let result = self.shared.kernel.dispatch_tool_via_broker_with_cancel(
            tool,
            &serde_json::to_string(&args).expect("serialize file-tool arguments"),
            &call,
            CancellationToken::new(),
        ).await.unwrap_or_else(|e| panic!("dispatch {tool} through production broker: {e}"));
        let content = if result.stdout.is_empty() { result.stderr } else { result.stdout };
        FileToolResult { is_error: !result.success, content }
    }

    async fn shutdown(self) {
        self.shared.kernel.shutdown_runtime_worker().await.expect("stop filesystem test kernel");
    }
}

/// `builtin.file` and `builtin.resources` both expose `read`, so the broker
/// requires the qualified native file-tool name.
const FILE_READ: &str = "builtin_file__read";

struct FileToolResult {
    is_error: bool,
    content: String,
}

// ---------------------------------------------------------------------------
// 1. Read-only mounts fail cleanly and do not corrupt cached content.
//
// Production mounts `/` read-only over the whole filesystem (`rpc.rs`:
// `kernel.mount("/", LocalBackend::read_only("/"))`). `/opt` isn't covered
// by any of the writable overrides (`~/src`, `/tmp`, `/config/rc`, `/config/kernel`,
// `/config/client`, `/config/midi`, `/run/midi`), so a probe there exercises the
// exact same read-only mount a real deployment relies on — not a synthetic
// stand-in for it.

/// `write` to a NEW path under the read-only root mount must fail cleanly,
/// and must not have created anything — the VFS's own `create()` refuses
/// before touching the host filesystem at all
/// (`LocalBackend::check_writable`).
#[test]
fn write_new_file_to_readonly_mount_fails_clean_and_creates_nothing() {
    if skip_unless_isotest() {
        return;
    }
    run_local(async {
        let kernel = FileKernel::boot().await;

        let probe = "/opt/isotest-ro-new-probe.txt";
        let w = kernel.call(
            "write",
            json!({ "path": probe, "content": "should never land on disk" }),
        )
        .await;
        assert!(w.is_error, "write to a read-only mount must fail: {}", w.content);
        assert!(
            w.content.to_lowercase().contains("read-only") || w.content.to_lowercase().contains("read only"),
            "refusal must name the read-only mount, got: {}",
            w.content
        );

        // Nothing was created: a REAL host process (bypassing the VFS
        // entirely) confirms it, independent of whatever the VFS layer
        // might otherwise believe.
        assert!(
            !std::path::Path::new(probe).exists(),
            "a refused write must not create the file on the real host fs"
        );
        kernel.shutdown().await;
    });
}

/// `write` to an existing file under the read-only root mount must fail.
/// The on-disk bytes and a later native `read` must retain the original content.
#[test]
fn write_existing_file_to_readonly_mount_fails_clean_and_does_not_poison_later_reads() {
    if skip_unless_isotest() {
        return;
    }
    run_local(async {
        let kernel = FileKernel::boot().await;

        let probe = "/opt/isotest-ro-existing-probe.txt";
        // Seed the file via a REAL host process — bypasses the VFS entirely
        // (a `shell` call would route `echo >` through kaish's own
        // VFS-backed redirect builtin, which would refuse on this exact
        // read-only mount before the test even starts).
        std::fs::write(probe, "on-disk-original").expect("seed read-only probe");

        let w = kernel.call(
            "write",
            json!({ "path": probe, "content": "TAMPERED" }),
        )
        .await;
        assert!(w.is_error, "write to a read-only mount must fail: {}", w.content);

        let r = kernel.call(FILE_READ, json!({ "path": probe })).await;
        assert!(!r.is_error, "read after a refused write must still succeed: {}", r.content);
        assert!(
            r.content.contains("on-disk-original"),
            "read must return the real file content, not a phantom unflushed edit: {}",
            r.content
        );
        assert!(
            !r.content.contains("TAMPERED"),
            "read must NOT serve the rejected write's content: {}",
            r.content
        );

        // And the real on-disk bytes are untouched too.
        let disk = std::fs::read_to_string(probe).expect("read real probe");
        assert_eq!(
            disk.trim(),
            "on-disk-original",
            "the on-disk file must be byte-identical to before the refused write"
        );
        kernel.shutdown().await;
    });
}

// ---------------------------------------------------------------------------
// 2. Filesystem-root walks are refused.

/// `glob` with an explicit `path: "/"` must refuse rather than walk the
/// whole container filesystem.
#[test]
fn glob_refuses_an_explicit_root_path() {
    if skip_unless_isotest() {
        return;
    }
    run_local(async {
        let kernel = FileKernel::boot().await;

        let g = kernel.call("glob", json!({ "pattern": "**/*", "path": "/" })).await;
        assert!(g.is_error, "glob on / must be refused: {}", g.content);
        assert!(
            g.content.contains("refusing to walk the filesystem root"),
            "refusal must name the root walk, got: {}",
            g.content
        );
        kernel.shutdown().await;
    });
}

/// `/tmp/..` resolves to `/`; `grep` must refuse it as a root walk.
#[test]
fn grep_refuses_a_path_that_climbs_out_to_the_root() {
    if skip_unless_isotest() {
        return;
    }
    run_local(async {
        let kernel = FileKernel::boot().await;

        let g = kernel.call("grep", json!({ "pattern": ".", "path": "/tmp/.." })).await;
        assert!(g.is_error, "grep on /tmp/.. must be refused: {}", g.content);
        assert!(
            g.content.contains("refusing to walk the filesystem root"),
            "refusal must name the root walk (the /tmp/.. -> / fold), got: {}",
            g.content
        );
        kernel.shutdown().await;
    });
}

/// Companion "must not over-refuse": a `glob` under a REAL subdirectory
/// (`/tmp`, not the root) must not trip the root-walk guard. Without this,
/// "refuse the root" could be trivially satisfied by refusing everything.
#[test]
fn glob_does_not_over_refuse_a_real_directory() {
    if skip_unless_isotest() {
        return;
    }
    run_local(async {
        let kernel = FileKernel::boot().await;

        std::fs::create_dir_all("/tmp/glob-probe").unwrap();
        std::fs::write("/tmp/glob-probe/a.rs", "x\n").unwrap();

        let g = kernel.call(
            "glob",
            json!({ "pattern": "**/*.rs", "path": "/tmp/glob-probe" }),
        )
        .await;
        assert!(
            !g.content.contains("refusing to walk the filesystem root"),
            "a walk confined to /tmp/glob-probe must not trip the root guard: {}",
            g.content
        );
        assert!(!g.is_error, "glob under a real directory should succeed: {}", g.content);
        assert!(g.content.contains("a.rs"), "expected to find a.rs: {}", g.content);
        kernel.shutdown().await;
    });
}

// ---------------------------------------------------------------------------
// 3. symlink-escape probes against the VFS
//
// `LocalBackend::resolve()` canonicalizes every path and refuses anything
// outside the mount's root. The fixture creates symlinks directly on the
// container filesystem, then addresses them through the native file tools.

/// A symlink under the writable `/tmp` mount pointing OUTSIDE `/tmp` (at a
/// file under the read-only root mount) must be refused on both read and
/// write — and a write attempt must never reach the real target file.
#[test]
fn symlink_escaping_its_mount_is_refused_on_read_and_write_and_target_is_untouched() {
    if skip_unless_isotest() {
        return;
    }
    run_local(async {
        let kernel = FileKernel::boot().await;

        let target = "/opt/isotest-escape-target.txt";
        let link = "/tmp/isotest-evil-link.txt";
        std::fs::write(target, "original-target").unwrap();
        let _ = std::fs::remove_file(link);
        std::os::unix::fs::symlink(target, link).unwrap();

        let r = kernel.call(FILE_READ, json!({ "path": link })).await;
        assert!(
            r.is_error,
            "reading through a symlink that escapes its mount must be refused: {}",
            r.content
        );
        assert!(
            !r.content.contains("original-target"),
            "an escape refusal must not leak the target's content: {}",
            r.content
        );

        let w = kernel.call("write", json!({ "path": link, "content": "TAMPERED" })).await;
        assert!(
            w.is_error,
            "writing through a symlink that escapes its mount must be refused: {}",
            w.content
        );

        let disk = std::fs::read_to_string(target).unwrap();
        assert_eq!(
            disk.trim(),
            "original-target",
            "the escape target must be untouched by the refused write"
        );
        kernel.shutdown().await;
    });
}

/// Companion "must not over-refuse": a symlink that stays WITHIN its own
/// mount must resolve and read normally. Without this, "block symlink
/// escapes" could be trivially satisfied by blocking every symlink.
#[test]
fn symlink_within_the_same_mount_is_not_over_refused() {
    if skip_unless_isotest() {
        return;
    }
    run_local(async {
        let kernel = FileKernel::boot().await;

        std::fs::create_dir_all("/tmp/symlink-probe").unwrap();
        std::fs::write("/tmp/symlink-probe/real.txt", "inside-content").unwrap();
        let _ = std::fs::remove_file("/tmp/symlink-probe/inside-link.txt");
        std::os::unix::fs::symlink("real.txt", "/tmp/symlink-probe/inside-link.txt").unwrap();

        let r = kernel.call(
            FILE_READ,
            json!({ "path": "/tmp/symlink-probe/inside-link.txt" }),
        )
        .await;
        assert!(
            !r.is_error,
            "a symlink that stays within its own mount must read normally: {}",
            r.content
        );
        assert!(
            r.content.contains("inside-content"),
            "expected the real target's content, got: {}",
            r.content
        );
        kernel.shutdown().await;
    });
}

/// A directory symlink that escapes its mount must not let `glob` walk
/// through it into the escaped tree — a different mechanism than the
/// single-file `read`/`write` escape above (directory listing +
/// `is_dir()`/`is_symlink()` in `VfsWalkerAdapter`, not `LocalBackend::resolve`'s
/// canonicalize check), so worth pinning independently.
#[test]
fn glob_does_not_descend_through_a_directory_symlink_that_escapes_the_mount() {
    if skip_unless_isotest() {
        return;
    }
    run_local(async {
        let kernel = FileKernel::boot().await;

        // A directory outside /tmp holding a file that must never surface
        // in a glob confined to /tmp.
        std::fs::create_dir_all("/opt/isotest-escape-dir").unwrap();
        std::fs::write("/opt/isotest-escape-dir/secret.rs", "x\n").unwrap();
        std::fs::create_dir_all("/tmp/dirwalk-probe").unwrap();
        let _ = std::fs::remove_file("/tmp/dirwalk-probe/escape-link");
        std::os::unix::fs::symlink("/opt/isotest-escape-dir", "/tmp/dirwalk-probe/escape-link").unwrap();

        let g = kernel.call(
            "glob",
            json!({ "pattern": "**/*.rs", "path": "/tmp/dirwalk-probe" }),
        )
        .await;
        assert!(
            !g.content.contains("secret.rs"),
            "glob must not descend through a directory symlink that escapes its mount: {}",
            g.content
        );
        kernel.shutdown().await;
    });
}
