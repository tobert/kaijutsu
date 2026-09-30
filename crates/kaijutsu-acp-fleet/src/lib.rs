//! A fleet of ACP scenario tests.
//!
//! Each scenario is a TOML file: the prompts a client sends, the replies a
//! scripted model gives, the answers to permission requests, what the ACP
//! update stream must show, and what the workspace must hold afterward. The
//! runner starts a fresh agent per scenario, drives it over stdio, and reports
//! every expectation that did not hold. See `docs/acp-fleet.md`.
//!
//! - [`client`]: a reusable ACP v1 client over a child process's stdio.
//! - [`container`]: the podman commands contained scenarios run with.
//! - [`scenario`]: the scenario file format.
//! - [`run`]: runs one scenario and judges it.
//! - [`shape`]: the Harbor-shape invariants every transcript is checked for.

pub mod client;
pub mod container;
pub mod run;
pub mod scenario;
pub mod shape;

/// The host-mode scenarios shipped with this crate.
pub const FLEET_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fleet");

/// The approval conformance matrix: host-mode scenarios, one per approval
/// property and entry path, some marking a known gap.
pub const APPROVAL_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fleet/approval");

/// The contained-mode scenarios shipped with this crate.
pub const CONTAINED_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fleet/contained");

/// Where scratch state goes by default: a real disk under `$HOME/src`, which
/// the kernel mounts read-write. `/tmp` on this host is a small tmpfs.
pub const DEFAULT_SCRATCH: &str = "/home/atobey/src/bench-work/fleet";
