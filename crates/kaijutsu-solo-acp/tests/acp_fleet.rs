//! Run every ACP fleet scenario against this binary (`docs/acp-fleet.md`).
//!
//! Each scenario starts a fresh `kaijutsu-solo-acp` with the scripted mock
//! model, so this target needs the `test-mock` feature:
//!
//! `cargo test -p kaijutsu-solo-acp --features test-mock --test acp_fleet`
//!
//! Host scenarios live in `crates/kaijutsu-acp-fleet/fleet/`. Contained
//! scenarios live in its `contained/` directory and need podman and the
//! fleet image, so their test is ignored by default:
//!
//! `cargo test -p kaijutsu-solo-acp --features test-mock --test acp_fleet -- --ignored`
//!
//! The approval conformance matrix lives in its `approval/` directory and
//! has its own test, so it runs beside the host scenarios. A scenario that
//! marks a known gap must fail in the ways it names, and fails the test when
//! the gap no longer reproduces.
//!
//! `acp-fleet run` runs the same scenarios by hand, one report line each.

use std::path::PathBuf;

use kaijutsu_acp_fleet::run::{RunConfig, run_file};
use kaijutsu_acp_fleet::scenario::{Mode, Scenario};
use kaijutsu_acp_fleet::{APPROVAL_DIR, CONTAINED_DIR, DEFAULT_SCRATCH, FLEET_DIR, scenario};

/// Run every scenario in `dir`, each of which must declare `mode`.
fn run_all(dir: &str, mode: Mode) {
    let files = scenario::discover(&[PathBuf::from(dir)]).expect("list the fleet");
    assert!(!files.is_empty(), "no scenarios in {dir}");

    let config = RunConfig::new(env!("CARGO_BIN_EXE_kaijutsu-solo-acp"), DEFAULT_SCRATCH);
    let mut report = Vec::new();
    for file in &files {
        let declared = Scenario::load(file).map(|s| s.mode);
        if let Ok(declared) = declared
            && declared != mode
        {
            report.push(format!("MISPLACED {}: mode {declared:?} in the {mode:?} directory {dir}", file.display()));
            continue;
        }
        let outcome = run_file(file, &config);
        if !outcome.passed() {
            report.push(format!(
                "FAIL {}\n  - {}\n  agent stderr (tail):\n{}",
                outcome.name,
                outcome.failures.join("\n  - "),
                outcome.stderr_tail
            ));
        }
    }
    assert!(report.is_empty(), "{} of {} scenarios failed:\n{}", report.len(), files.len(), report.join("\n"));
}

#[test]
fn every_fleet_scenario_passes() {
    run_all(FLEET_DIR, Mode::Host);
}

#[test]
fn every_approval_scenario_holds() {
    run_all(APPROVAL_DIR, Mode::Host);
}

#[test]
#[ignore = "needs podman and the kaijutsu-fleet image; see docs/acp-fleet.md"]
fn every_contained_scenario_passes() {
    run_all(CONTAINED_DIR, Mode::Contained);
}
