//! Filesystem-protection tests. Run via `contrib/isotest`. Slice 2 of the
//! isotest suite (`docs/isotest.md`): same harness as `isolation.rs`
//! (process lifecycle), new file for VFS protections. These tests don't
//! need the empty PID namespace `isolation.rs` cares about, but they DO
//! need a real host filesystem behind the kernel's VFS — a symlink escape
//! or a read-only mount can't be meaningfully faked in-process, and the
//! container keeps any hostile filesystem layout off the real host (see
//! "Why a container" in `docs/isotest.md`).
//!
//! Production's real mount topology drives what's tested here
//! (`kaijutsu-server/src/rpc.rs`, `build_kernel`): `/` is mounted
//! `LocalBackend::read_only("/")` (the whole container filesystem is
//! visible but not writable through the VFS), `~/src` and `/tmp` are
//! writable `LocalBackend` mounts. Every probe below targets that exact
//! topology rather than a synthetic one, so a passing suite says something
//! about the shipped kernel, not a test-only stand-in.
//!
//! Three things pinned, matching `docs/isotest.md`'s slice 2 plan:
//! 1. Read-only mounts surface as clean errors, not corruption — including
//!    the cache-poisoning corruption found and fixed while writing this
//!    suite (see `write_file`/`apply_edit_plan` in
//!    `kaijutsu-kernel/src/mcp/servers/file.rs`).
//! 2. The filesystem-root walk refusal (`ce0a4146`) fires through the live
//!    `glob`/`grep` MCP tools, including the `..`-folding regression that
//!    commit closed.
//! 3. Symlink-escape probes against the VFS: a symlink under a writable
//!    mount that points outside that mount's root is refused, real
//!    within-mount symlinks are not over-refused, and the escape target is
//!    never touched.

mod common;
use common::{join_root, run_local, run_real, run_shell, skip_unless_isotest, TestKernel};

use serde_json::json;

/// Give the test's context a durable cwd via the sanctioned channel (`kj
/// context set . --cwd <path>`, `kaijutsu-kernel/src/kj/context.rs`). Every
/// file tool (`read`/`edit`/`write`/`glob`/`grep`) refuses up front on a
/// context with no cwd at all (`refuse_missing_cwd` in
/// `mcp/servers/file.rs`) — a plain `cd` in a one-shot `shell` call does
/// NOT persist this (that materialized kaish is throwaway; only `kj context
/// set` writes the durable `context_shell.cwd` row), so this is the only
/// channel reachable from an RPC-only client. The value itself doesn't
/// matter for tests that pass an explicit absolute `path` — only that it's
/// `Some`.
async fn set_durable_cwd(kernel: &kaijutsu_client::rpc::KernelHandle, path: &str) {
    let out = run_shell(kernel, &format!("kj context set . --cwd {path}")).await;
    assert!(
        out.contains("cwd="),
        "kj context set --cwd didn't report a cwd change: {out}"
    );
}

/// `builtin.file` isn't a facade-projected instance
/// (`mcp/binding.rs`'s `FACADE_PROJECTED_INSTANCES` covers only
/// `builtin.shell`/`builtin.shell_readonly`/`builtin.background`), so
/// `exec`+`facade:shell` — what the genesis ROOT director gets out of the
/// box — doesn't light up `read`/`write`/`edit`/`glob`/`grep` on its own.
/// The director's own create-lifecycle rc script
/// (`assets/defaults/rc/director/create/S10-binding.kai`) grants
/// `read`/`write`/`edit` but not `glob`/`grep`. Rather than depend on that
/// seed's exact shape (and risk drifting silently if it changes), grant all
/// five explicitly and self-contained — ROOT already holds binding-admin
/// via its own S10 script, so it can widen its own binding with no extra
/// capability.
async fn grant_file_tools(kernel: &kaijutsu_client::rpc::KernelHandle) {
    for tool in ["read", "write", "edit", "glob", "grep"] {
        // run_shell itself asserts !is_error and panics with the tool's
        // message on failure — no separate check needed here.
        run_shell(kernel, &format!("kj binding allow \"builtin.file:{tool}\"")).await;
    }
}

/// `builtin.file`'s `read` collides with `builtin.resources`'s own `"read"`
/// tool (`mcp/servers/resources_builtin.rs`) — ROOT's default binding grants
/// `builtin.resources` at the instance level, so both are visible at once.
/// The broker's collision resolver (`Broker::list_visible_tools`,
/// `mcp/broker.rs`) auto-qualifies every colliding name as
/// `{instance}__{tool}` (dots cleaned to `_` for the Anthropic-safe name
/// pattern) rather than leaving either bare-callable, so the bare `"read"`
/// name is NOT in the visible name_map at all once ROOT holds both grants —
/// calling it fails with "not granted by its loadout/binding", which reads
/// exactly like a missing grant even though the grant is there (confirmed
/// empirically via `kj binding show`). `write`/`edit`/`glob`/`grep` have no
/// same-name collision under ROOT's default instance set, so only `read`
/// needs this.
const FILE_READ: &str = "builtin_file__read";

async fn file_tool(
    kernel: &kaijutsu_client::rpc::KernelHandle,
    tool: &str,
    args: serde_json::Value,
) -> kaijutsu_client::rpc::McpToolResult {
    kernel
        .call_mcp_tool(tool, &args)
        .await
        .unwrap_or_else(|e| panic!("call_mcp_tool({tool}) failed to dispatch: {e}"))
}

// ---------------------------------------------------------------------------
// 1. read-only mounts: clean errors, never corruption
//
// Production mounts `/` read-only over the whole filesystem (`rpc.rs`:
// `kernel.mount("/", LocalBackend::read_only("/"))`). `/opt` isn't covered
// by any of the writable overrides (`~/src`, `/tmp`, `/etc/rc`, `/etc/config`,
// `/etc/client`, `/etc/midi`, `/run/midi`), so a probe there exercises the
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
        let tk = TestKernel::boot("ro-new", 2401);
        let kernel = tk.connect().await;
        join_root(&kernel).await;
        grant_file_tools(&kernel).await;
        set_durable_cwd(&kernel, "/tmp").await;

        let probe = "/opt/isotest-ro-new-probe.txt";
        let w = file_tool(
            &kernel,
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
        // entirely) confirms it, independent of whatever the VFS/CRDT layer
        // might otherwise believe.
        let stat = run_real(&kernel, &format!("test -e {probe} && echo EXISTS || echo ABSENT")).await;
        assert!(
            stat.contains("ABSENT"),
            "a refused write must not create the file on the real host fs: {stat}"
        );
    });
}

/// `write` to an EXISTING file under the read-only root mount must fail
/// cleanly AND must not leave the CRDT cache serving the rejected content on
/// a later `read` — the corruption this suite was written to catch: before
/// the fix, a failed flush left `write_file`'s CRDT cache entry holding the
/// unflushed edit forever (`dirty` had no path back to `false`, and
/// staleness detection is keyed on a disk `generation` a failed write never
/// advances), so a later `read` served the phantom tampered content while
/// the real file was untouched. Both the disk bytes AND the model-visible
/// `read` must show the original content.
#[test]
fn write_existing_file_to_readonly_mount_fails_clean_and_does_not_poison_later_reads() {
    if skip_unless_isotest() {
        return;
    }
    run_local(async {
        let tk = TestKernel::boot("ro-existing", 2402);
        let kernel = tk.connect().await;
        join_root(&kernel).await;
        grant_file_tools(&kernel).await;
        set_durable_cwd(&kernel, "/tmp").await;

        let probe = "/opt/isotest-ro-existing-probe.txt";
        // Seed the file via a REAL host process — bypasses the VFS entirely
        // (a foreground `shell` call would route `echo >` through kaish's own
        // VFS-backed redirect builtin, which would refuse on this exact
        // read-only mount before the test even starts).
        run_real(&kernel, &format!("echo -n on-disk-original > {probe}")).await;

        let w = file_tool(
            &kernel,
            "write",
            json!({ "path": probe, "content": "TAMPERED" }),
        )
        .await;
        assert!(w.is_error, "write to a read-only mount must fail: {}", w.content);

        let r = file_tool(&kernel, FILE_READ, json!({ "path": probe })).await;
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
        let disk = run_real(&kernel, &format!("cat {probe}")).await;
        assert_eq!(
            disk.trim(),
            "on-disk-original",
            "the on-disk file must be byte-identical to before the refused write"
        );
    });
}

// ---------------------------------------------------------------------------
// 2. filesystem-root walk refusal (ce0a4146)

/// `glob` with an explicit `path: "/"` must refuse rather than walk the
/// whole container filesystem — `refuse_filesystem_root_walk` in
/// `mcp/servers/file.rs`, the guard `ce0a4146` hardened.
#[test]
fn glob_refuses_an_explicit_root_path() {
    if skip_unless_isotest() {
        return;
    }
    run_local(async {
        let tk = TestKernel::boot("root-walk-glob", 2403);
        let kernel = tk.connect().await;
        join_root(&kernel).await;
        grant_file_tools(&kernel).await;
        set_durable_cwd(&kernel, "/tmp").await;

        let g = file_tool(&kernel, "glob", json!({ "pattern": "**/*", "path": "/" })).await;
        assert!(g.is_error, "glob on / must be refused: {}", g.content);
        assert!(
            g.content.contains("refusing to walk the filesystem root"),
            "refusal must name the root walk, got: {}",
            g.content
        );
    });
}

/// The exact `ce0a4146` regression: `/tmp/..` folds to `/` lexically even
/// though naive `Path` equality doesn't see it as `/`. `grep` must refuse
/// this the same way it refuses a bare `/` — this is `grep`'s guard, not
/// `glob`'s, so it's worth pinning independently (they compute
/// `search_root` differently: `glob` checks the walker's actual root after
/// the pattern's static prefix is applied, `grep` checks its `path` arg
/// directly).
#[test]
fn grep_refuses_a_path_that_climbs_out_to_the_root() {
    if skip_unless_isotest() {
        return;
    }
    run_local(async {
        let tk = TestKernel::boot("root-walk-grep", 2404);
        let kernel = tk.connect().await;
        join_root(&kernel).await;
        grant_file_tools(&kernel).await;
        set_durable_cwd(&kernel, "/tmp").await;

        let g = file_tool(&kernel, "grep", json!({ "pattern": ".", "path": "/tmp/.." })).await;
        assert!(g.is_error, "grep on /tmp/.. must be refused: {}", g.content);
        assert!(
            g.content.contains("refusing to walk the filesystem root"),
            "refusal must name the root walk (the /tmp/.. -> / fold), got: {}",
            g.content
        );
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
        let tk = TestKernel::boot("root-walk-noover", 2405);
        let kernel = tk.connect().await;
        join_root(&kernel).await;
        grant_file_tools(&kernel).await;
        set_durable_cwd(&kernel, "/tmp").await;

        run_real(&kernel, "mkdir -p /tmp/glob-probe && echo x > /tmp/glob-probe/a.rs").await;

        let g = file_tool(
            &kernel,
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
    });
}

// ---------------------------------------------------------------------------
// 3. symlink-escape probes against the VFS
//
// `LocalBackend::resolve()` (`vfs/backends/local.rs`) canonicalizes every
// path and refuses anything that resolves outside the mount's own root —
// including through a symlink. These probes create REAL symlinks via
// `run_real` (an actual host `ln -s`, exactly how a hostile or careless
// script would produce one — even kaish's own `ln` builtin is VFS-routed, so
// a foreground `shell` call isn't a genuine bypass here) and then address
// them through the `read`/`write` MCP tools, which DO go through the VFS.

/// A symlink under the writable `/tmp` mount pointing OUTSIDE `/tmp` (at a
/// file under the read-only root mount) must be refused on both read and
/// write — and a write attempt must never reach the real target file.
#[test]
fn symlink_escaping_its_mount_is_refused_on_read_and_write_and_target_is_untouched() {
    if skip_unless_isotest() {
        return;
    }
    run_local(async {
        let tk = TestKernel::boot("symlink-escape", 2406);
        let kernel = tk.connect().await;
        join_root(&kernel).await;
        grant_file_tools(&kernel).await;
        set_durable_cwd(&kernel, "/tmp").await;

        let target = "/opt/isotest-escape-target.txt";
        let link = "/tmp/isotest-evil-link.txt";
        run_real(&kernel, &format!("echo -n original-target > {target} && ln -s {target} {link}")).await;

        let r = file_tool(&kernel, FILE_READ, json!({ "path": link })).await;
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

        let w = file_tool(&kernel, "write", json!({ "path": link, "content": "TAMPERED" })).await;
        assert!(
            w.is_error,
            "writing through a symlink that escapes its mount must be refused: {}",
            w.content
        );

        let disk = run_real(&kernel, &format!("cat {target}")).await;
        assert_eq!(
            disk.trim(),
            "original-target",
            "the escape target must be untouched by the refused write"
        );
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
        let tk = TestKernel::boot("symlink-inside", 2407);
        let kernel = tk.connect().await;
        join_root(&kernel).await;
        grant_file_tools(&kernel).await;
        set_durable_cwd(&kernel, "/tmp").await;

        run_real(
            &kernel,
            "mkdir -p /tmp/symlink-probe && echo -n inside-content > /tmp/symlink-probe/real.txt && ln -s real.txt /tmp/symlink-probe/inside-link.txt",
        )
        .await;

        let r = file_tool(
            &kernel,
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
        let tk = TestKernel::boot("symlink-dirwalk", 2408);
        let kernel = tk.connect().await;
        join_root(&kernel).await;
        grant_file_tools(&kernel).await;
        set_durable_cwd(&kernel, "/tmp").await;

        // A directory outside /tmp holding a file that must never surface
        // in a glob confined to /tmp.
        run_real(
            &kernel,
            "mkdir -p /opt/isotest-escape-dir && echo x > /opt/isotest-escape-dir/secret.rs",
        )
        .await;
        run_real(
            &kernel,
            "mkdir -p /tmp/dirwalk-probe && ln -s /opt/isotest-escape-dir /tmp/dirwalk-probe/escape-link",
        )
        .await;

        let g = file_tool(
            &kernel,
            "glob",
            json!({ "pattern": "**/*.rs", "path": "/tmp/dirwalk-probe" }),
        )
        .await;
        assert!(
            !g.content.contains("secret.rs"),
            "glob must not descend through a directory symlink that escapes its mount: {}",
            g.content
        );
    });
}
