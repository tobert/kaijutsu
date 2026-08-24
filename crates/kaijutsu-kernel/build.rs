//! Rebuild when the embedded default tree changes.
//!
//! `seed_scripts.rs` pulls `assets/defaults/` in with `include_dir!`, which
//! cargo does not track as an input. Without these directives an edit to a
//! seed script is invisible until some `.rs` in this crate changes, so the
//! seed tests run against the previously embedded copy and pass while the
//! binary carries the old script. See `docs/config-ownership.md`.
//!
//! The path is workspace-relative because `rerun-if-changed` resolves
//! against the PACKAGE root while `include_dir!` reads
//! `$CARGO_MANIFEST_DIR/../../assets/defaults`. A crate-relative
//! `assets/defaults` names a directory that does not exist, and cargo
//! answers a missing path by rerunning every build — which looks like the
//! tracking working and is not.

fn main() {
    println!("cargo:rerun-if-changed=../../assets/defaults");
    println!("cargo:rerun-if-changed=build.rs");
}
