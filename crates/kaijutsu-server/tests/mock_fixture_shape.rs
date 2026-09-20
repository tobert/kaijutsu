//! Unit-speed check that the committed `KJ_MOCK_SCRIPT_DIR` fixtures still
//! parse as the type `MockClient::with_script_dir` deserializes them into.
//!
//! `with_script_dir` (`kaijutsu-kernel/src/llm/mod.rs`) reads each
//! `<model>.json` as `Vec<Vec<stream::StreamEvent>>` — an ordered array of
//! turns, each turn an ordered array of `StreamEvent` in its derived,
//! externally tagged shape. Parsing the fixtures against that type here
//! names the offending file in milliseconds; the alternative is discovering
//! a shape drift through `session_scenario.rs`'s 30 s timeout.
//!
//! Covers both crates that load fixtures through that one mechanism: this
//! crate's `tests/mock_scripts/*.json` and `kaijutsu-solo-acp`'s
//! `tests/mock_scripts/{chat,file}/solo-mock.json`, which
//! `solo_acp_stdio.rs`'s `mock_scripts()` points the variable at. Both are
//! reached by a filesystem walk from `CARGO_MANIFEST_DIR`: this test needs
//! nothing from `kaijutsu-solo-acp` but its checked-in fixture files.

use kaijutsu_kernel::llm::stream::StreamEvent;

/// Every committed mock-script fixture under `dir`, recursively (the
/// solo-acp fixtures are one directory deeper, under `chat/` and `file/`).
fn collect_json_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| panic!("reading an entry in {}: {e}", dir.display()));
        let path = entry.path();
        if path.is_dir() {
            collect_json_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
            out.push(path);
        }
    }
}

/// Every fixture this test covers, across both crates.
fn fixture_paths() -> Vec<std::path::PathBuf> {
    let server_scripts =
        std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/mock_scripts"));
    let solo_scripts = std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../kaijutsu-solo-acp/tests/mock_scripts"
    ));
    let mut paths = Vec::new();
    for dir in [server_scripts, solo_scripts] {
        collect_json_files(dir, &mut paths);
    }
    assert!(
        !paths.is_empty(),
        "no fixtures found under {} or {} — a path moved",
        server_scripts.display(),
        solo_scripts.display(),
    );
    paths
}

/// Parses every committed fixture as `Vec<Vec<StreamEvent>>` — the exact
/// type `MockClient::with_script_dir` deserializes each file into. A shape
/// drift here is a fixture bug, not a runtime condition to recover from, so
/// this panics with the offending file's path rather than collecting a
/// report.
#[test]
fn committed_mock_scripts_parse_as_stream_event_turns() {
    for path in fixture_paths() {
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let _turns: Vec<Vec<StreamEvent>> = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("{} does not parse as Vec<Vec<StreamEvent>>: {e}", path.display()));
    }
}
