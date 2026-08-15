//! Tripwire: the dependency graph must never carry `gix-command` (or its two
//! usual companions, `gix-transport`/`gix-filter`) — the crates that give
//! gitoxide the ability to spawn a subprocess. Their presence would mean
//! something pulled in the `gix` facade, or a feature that drags the facade
//! along, defeating the entire point of building on plumbing crates instead
//! (see the crate-level docs in `src/lib.rs`).
//!
//! Mirrors `~/src/kaish-extras`'s own tripwire (`.github/workflows/ci.yml`,
//! job `git-tool-dependency-tripwires`), which runs `cargo tree -i <pkg>
//! --workspace --locked` per package and asserts a non-zero exit (cargo's
//! signal for "no such package in the graph"). That repo runs it as a CI
//! matrix job rather than a `#[test]`; this repo has no CI workflow files to
//! extend yet, so it's a test instead — the task brief calls this
//! acceptable when it's what kaish-extras itself does.
//!
//! Shelling out to `cargo` from a `#[test]` is dev-tooling, not shipped
//! kernel behavior — this file never ships in the kaijutsu binary, so it
//! does not collide with `CLAUDE.md`'s host-exec-has-one-owner rule (which
//! governs how the running kernel executes host processes at runtime, via
//! kaish). It's the same category as `cargo clippy`/`cargo fmt --check`
//! running outside the product.

use std::path::PathBuf;
use std::process::Command;

/// Walk up from this crate's manifest dir to find the workspace root, i.e.
/// the first ancestor containing a `Cargo.toml` with a `[workspace]` table.
/// Scoped `--workspace`, deliberately, same as kaish-extras: a regression
/// introduced by *any* crate in the tree should trip this, not only one
/// introduced directly under `kaijutsu-configgit`.
fn workspace_root() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.is_file() {
            let contents = std::fs::read_to_string(&candidate).expect("read Cargo.toml");
            if contents.contains("[workspace]") {
                return dir;
            }
        }
        if !dir.pop() {
            panic!("walked past filesystem root without finding a workspace Cargo.toml");
        }
    }
}

/// Assert `pkg` is absent from the whole workspace's resolved dependency
/// graph. `cargo tree -i <pkg>` exits non-zero with "did not match any
/// packages" when genuinely absent, and exits 0 printing the reverse-
/// dependency tree when present — so success is the failure case, and the
/// two are distinguished by exit code rather than by scraping the message
/// text (which is not a stability-guaranteed interface).
fn assert_absent_from_dependency_graph(pkg: &str) {
    let output = Command::new("cargo")
        .args(["tree", "-i", pkg, "--workspace", "--locked"])
        .current_dir(workspace_root())
        .output()
        .unwrap_or_else(|e| panic!("failed to run `cargo tree -i {pkg}`: {e}"));

    if output.status.success() {
        panic!(
            "'{pkg}' IS present in the dependency graph — expected absent (spawn-capable gitoxide crate leaked in):\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[test]
fn gix_command_is_absent() {
    assert_absent_from_dependency_graph("gix-command");
}

#[test]
fn gix_transport_is_absent() {
    assert_absent_from_dependency_graph("gix-transport");
}

#[test]
fn gix_filter_is_absent() {
    assert_absent_from_dependency_graph("gix-filter");
}
