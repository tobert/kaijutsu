//! A fleet of ACP scenario tests.
//!
//! Each scenario is a TOML file: the prompts a client sends, the replies a
//! scripted model gives, the answers to permission requests, what the ACP
//! update stream must show, and what the workspace must hold afterward. The
//! runner starts a fresh agent per scenario, drives it over stdio, and reports
//! every expectation that did not hold. See `docs/acp-fleet.md`.
//!
//! - [`client`]: a reusable ACP v1 client over a child process's stdio.
//! - [`scenario`]: the scenario file format.
//! - [`run`]: runs one scenario and judges it.

pub mod client;
pub mod run;
pub mod scenario;

/// The scenarios shipped with this crate.
pub const FLEET_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fleet");

/// Where scratch state goes by default: a real disk under `$HOME/src`, which
/// the kernel mounts read-write. `/tmp` on this host is a small tmpfs.
pub const DEFAULT_SCRATCH: &str = "/home/atobey/src/bench-work/fleet";
