//! Run every ACP fleet scenario against this binary (`docs/acp-fleet.md`).
//!
//! The scenarios live in `crates/kaijutsu-acp-fleet/fleet/`. Each one starts
//! a fresh `kaijutsu-solo-acp` with the scripted mock model, so this target
//! needs the `test-mock` feature:
//!
//! `cargo test -p kaijutsu-solo-acp --features test-mock --test acp_fleet`
//!
//! `acp-fleet run` runs the same scenarios by hand, one report line each.

use std::path::PathBuf;

use kaijutsu_acp_fleet::run::{RunConfig, run_file};
use kaijutsu_acp_fleet::{DEFAULT_SCRATCH, FLEET_DIR, scenario};

#[test]
fn every_fleet_scenario_passes() {
    let files = scenario::discover(&[PathBuf::from(FLEET_DIR)]).expect("list the fleet");
    assert!(!files.is_empty(), "no scenarios in {FLEET_DIR}");

    let config = RunConfig::new(env!("CARGO_BIN_EXE_kaijutsu-solo-acp"), DEFAULT_SCRATCH);
    let mut report = Vec::new();
    for file in &files {
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
