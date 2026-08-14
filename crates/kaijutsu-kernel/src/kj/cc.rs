//! `kj cc list` — the roster of live Claude Code sessions on this machine,
//! read from Claude Code's own on-disk session registry.
//!
//! Claude Code (>= 2.1.x) writes one descriptor per live session under
//! `~/.claude/sessions/`, named `<pid>.json` (mode 0644). A companion
//! `<pid>.<64-hex>.key` file (mode 0600) sits alongside it and carries a
//! bearer token for that session's messaging socket.
//!
//! **This slice reads only the `.json` descriptor.** It never opens, reads,
//! or stats a `.key` file — not even to check existence — because this is a
//! read-only roster feature with no business needing a credential, and the
//! least-privilege move is to never touch the file at all rather than trust
//! ourselves to read-and-discard it correctly. `scan_sessions_dir` below
//! filters `.key` files out by name before any filesystem call other than
//! the directory listing itself touches them.
//!
//! ## The PID-reuse guard
//!
//! A `<pid>.json` descriptor can outlive the process that wrote it (a crash
//! that skipped cleanup, a killed session) and the OS is free to hand that
//! pid to an unrelated process later. Presenting a recycled pid as "this is
//! still that Claude Code session" would be actively wrong — worse than
//! saying nothing. Claude Code defends against exactly this by recording
//! `procStart`: field 22 (`starttime`) of `/proc/<pid>/stat` at the moment
//! the session started, measured in clock ticks since boot. That field is
//! monotonic per-boot and reused pids get a new (larger) starttime, so
//! comparing the recorded value against the live process's current starttime
//! tells alive-and-same-process apart from alive-but-a-stranger. We report
//! that distinction as [`Reachable`] rather than silently treating a stale
//! pid as reachable.
//!
//! `/proc/<pid>/stat`'s second field (`comm`, the process name) is wrapped in
//! parentheses and MAY itself contain spaces or parentheses, so naively
//! splitting the line on whitespace misindexes every field after it. The
//! correct extraction splits after the LAST `)` in the line (the kernel
//! itself guarantees `comm` cannot contain a `)` followed by a space then
//! more of the line reinterpreted as comm — the last `)` is unambiguously
//! the end of the comm field) and counts fields from there. See
//! [`parse_stat_starttime`].
//!
//! ## Why a malformed descriptor is never silently dropped
//!
//! A `<pid>.json` that fails to parse is surfaced by name and reason in the
//! human-readable output and counted — never swallowed into a shorter list
//! that merely looks complete. A silently-shrunk roster is a worse failure
//! mode than a visible one: it reads as "no problem" to whoever's watching.

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use kaijutsu_types::ContentType;

use super::{KjCaller, KjDispatcher, KjResult, clap_help_for};

#[derive(Parser, Debug)]
#[command(
    name = "cc",
    about = "Roster of live Claude Code sessions on this machine (~/.claude/sessions/)",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct CcArgs {
    #[command(subcommand)]
    command: CcCommand,
}

#[derive(Subcommand, Debug)]
enum CcCommand {
    /// List live Claude Code sessions from ~/.claude/sessions/*.json.
    List,
}

impl KjDispatcher {
    pub(crate) fn dispatch_cc(&self, argv: &[String], _caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<CcArgs>();
        }
        let parsed = match CcArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj cc: {e}"));
            }
        };

        // Read-only host introspection, same ungated rationale as `kj cas
        // ls`/`kj mcp list`/`kj midi list` — no capability check.
        match parsed.command {
            CcCommand::List => self.cc_list(),
        }
    }

    fn cc_list(&self) -> KjResult {
        let home = match std::env::var_os("HOME") {
            Some(h) if !h.is_empty() => PathBuf::from(h),
            _ => return KjResult::Err("kj cc list: $HOME is not set".to_string()),
        };
        let sessions_dir = home.join(".claude").join("sessions");

        let scan = match scan_sessions_dir(&sessions_dir) {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj cc list: {e}")),
        };

        // Deliberately an ARRAY OF OBJECTS, not the array-of-id-strings
        // convention most `kj … list` verbs use (see KjResult::Ok's `data`
        // doc comment). Those id strings are kaijutsu-native handles a
        // caller can round-trip into another `kj` verb; a Claude Code
        // session descriptor is a foreign record with no kaijutsu identity
        // to hand back — the useful iteration unit here is the whole row.
        let data = serde_json::Value::Array(
            scan.sessions.iter().map(session_entry_json).collect(),
        );

        if scan.sessions.is_empty() && scan.errors.is_empty() {
            return KjResult::ok_with_data(
                "(no live Claude Code sessions)".to_string(),
                data,
            );
        }

        let mut lines = Vec::new();
        if !scan.sessions.is_empty() {
            let name_w = scan
                .sessions
                .iter()
                .map(|s| s.name.as_deref().unwrap_or("-").len())
                .chain(std::iter::once("NAME".len()))
                .max()
                .unwrap_or(4);
            let status_w = scan
                .sessions
                .iter()
                .map(|s| s.status.len())
                .chain(std::iter::once("STATUS".len()))
                .max()
                .unwrap_or(6);
            lines.push(format!(
                "  {:<name_w$}  {:>7}  {:<status_w$}  {:<6}  CWD",
                "NAME",
                "PID",
                "STATUS",
                "REACH",
                name_w = name_w,
                status_w = status_w
            ));
            for s in &scan.sessions {
                lines.push(format!(
                    "  {:<name_w$}  {:>7}  {:<status_w$}  {:<6}  {}",
                    s.name.as_deref().unwrap_or("-"),
                    s.pid,
                    s.status,
                    reach_label(s.reachable),
                    s.cwd,
                    name_w = name_w,
                    status_w = status_w
                ));
            }
        }
        if !scan.errors.is_empty() {
            lines.push(format!(
                "{} descriptor{} failed to parse:",
                scan.errors.len(),
                if scan.errors.len() == 1 { "" } else { "s" }
            ));
            for e in &scan.errors {
                lines.push(format!("  ! {}: {}", e.file, e.message));
            }
        }

        KjResult::ok_with_data(lines.join("\n"), data)
    }
}

fn reach_label(r: Reachable) -> &'static str {
    match r {
        Reachable::Alive => "alive",
        Reachable::Stale => "stale",
        Reachable::Gone => "gone",
    }
}

fn session_entry_json(s: &CcSessionEntry) -> serde_json::Value {
    serde_json::json!({
        "name": s.name,
        "pid": s.pid,
        "status": s.status,
        "cwd": s.cwd,
        "session_id": s.session_id,
        "version": s.version,
        "peer_protocol": s.peer_protocol,
        "messaging_socket_path": s.messaging_socket_path,
        "reachable": reach_label(s.reachable),
    })
}

// ============================================================================
// Descriptor parsing + scanning (host-fs, no context needed — free functions
// so the fs/proc logic is unit-testable against an injected temp dir rather
// than the real ~/.claude).
// ============================================================================

/// The on-disk shape of a `<pid>.json` session descriptor, as written by
/// Claude Code >= 2.1.x. Extra fields (e.g. `startedAt`, `nameSince`,
/// `updatedAt`, `statusUpdatedAt`, `kind`, `entrypoint`) are present in the
/// real file but not needed by this slice; serde ignores them rather than
/// this struct enumerating fields nobody reads yet.
///
/// `name` is optional — a session that hasn't been given a name yet may omit
/// `name`/`nameSince` entirely; every other field is measured to always be
/// present on a live descriptor, so a missing one there is a real parse
/// failure, not a "maybe absent" case to paper over.
#[derive(Debug, Clone, serde::Deserialize)]
struct CcSessionFile {
    pid: u32,
    #[serde(rename = "sessionId")]
    session_id: String,
    cwd: String,
    /// Clock ticks since boot (field 22 of `/proc/<pid>/stat`) at the moment
    /// this session started — measured as a JSON *string*, not a number.
    #[serde(rename = "procStart")]
    proc_start: String,
    version: String,
    #[serde(rename = "peerProtocol")]
    peer_protocol: u32,
    #[serde(rename = "messagingSocketPath")]
    messaging_socket_path: String,
    #[serde(default)]
    name: Option<String>,
    /// Opaque — observed values include "busy" and "shell". Not an enum by
    /// design: Claude Code owns this vocabulary and can add values without
    /// this slice needing a matching update.
    status: String,
}

/// One session row for `kj cc list`'s structured output — [`CcSessionFile`]
/// plus the liveness verdict this slice adds.
#[derive(Debug, Clone)]
struct CcSessionEntry {
    name: Option<String>,
    pid: u32,
    status: String,
    cwd: String,
    session_id: String,
    version: String,
    peer_protocol: u32,
    messaging_socket_path: String,
    reachable: Reachable,
}

/// How a descriptor's recorded pid maps onto the live process table right
/// now. Never collapsed into a bare bool — "stale" (pid reused by a stranger)
/// and "gone" (nothing there at all) are different facts a caller might act
/// on differently, and `[Alive]` is the only one that means "this is safe to
/// treat as the same session that wrote the descriptor."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reachable {
    /// The pid is running and its live `/proc` starttime matches the
    /// descriptor's recorded `procStart` — genuinely the same process.
    Alive,
    /// The pid is running, but its starttime disagrees — this is a different
    /// process that reused the pid after the original session exited.
    Stale,
    /// No process with this pid exists at all.
    Gone,
}

/// One descriptor that failed to parse — surfaced by name, never dropped.
#[derive(Debug, Clone)]
struct CcScanError {
    file: String,
    message: String,
}

#[derive(Debug, Clone, Default)]
struct CcScan {
    sessions: Vec<CcSessionEntry>,
    errors: Vec<CcScanError>,
}

/// Scan `dir` for Claude Code session descriptors.
///
/// A missing `dir` (Claude Code never installed, or never run on this
/// machine) is a normal, empty result — not an error. Any other directory
/// read failure (e.g. permission denied) IS an error: unlike "doesn't
/// exist", it means we genuinely don't know what's there.
///
/// `.key` files are filtered out by filename alone, before any read/open/stat
/// call touches them — the hard constraint this slice must never violate.
fn scan_sessions_dir(dir: &Path) -> Result<CcScan, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(CcScan::default()),
        Err(e) => return Err(format!("read {}: {e}", dir.display())),
    };

    let mut scan = CcScan::default();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                scan.errors.push(CcScanError {
                    file: "<unreadable directory entry>".to_string(),
                    message: e.to_string(),
                });
                continue;
            }
        };
        let file_name = entry.file_name().to_string_lossy().into_owned();

        // Hard constraint: never open, read, or stat a `.key` file — not
        // even to check existence. This filename-only check is the entire
        // enforcement; nothing below ever calls a filesystem op on a name
        // that matches this.
        if file_name.ends_with(".key") {
            continue;
        }
        if !file_name.ends_with(".json") {
            // Not a session artifact this slice recognizes (or a directory);
            // silently ignored rather than treated as a parse failure — an
            // unrelated file in this directory isn't this verb's business.
            continue;
        }

        let path = entry.path();
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                scan.errors.push(CcScanError {
                    file: file_name,
                    message: format!("read: {e}"),
                });
                continue;
            }
        };
        let parsed: CcSessionFile = match serde_json::from_str(&raw) {
            Ok(p) => p,
            Err(e) => {
                scan.errors.push(CcScanError {
                    file: file_name,
                    message: format!("parse: {e}"),
                });
                continue;
            }
        };

        let reachable = classify_reachability(parsed.pid, &parsed.proc_start);
        scan.sessions.push(CcSessionEntry {
            name: parsed.name,
            pid: parsed.pid,
            status: parsed.status,
            cwd: parsed.cwd,
            session_id: parsed.session_id,
            version: parsed.version,
            peer_protocol: parsed.peer_protocol,
            messaging_socket_path: parsed.messaging_socket_path,
            reachable,
        });
    }

    scan.sessions.sort_by_key(|s| s.pid);
    scan.errors.sort_by(|a, b| a.file.cmp(&b.file));

    Ok(scan)
}

/// Compare a descriptor's recorded `procStart` against the live process's
/// current `/proc/<pid>/stat` starttime.
fn classify_reachability(pid: u32, recorded_proc_start: &str) -> Reachable {
    let Some(live_start) = read_proc_starttime(pid) else {
        return Reachable::Gone;
    };
    match recorded_proc_start.trim().parse::<u64>() {
        Ok(recorded) if recorded == live_start => Reachable::Alive,
        _ => Reachable::Stale,
    }
}

/// Read field 22 (`starttime`) from `/proc/<pid>/stat`. `None` means no such
/// process (or its stat line couldn't be read/parsed) — callers treat that
/// as [`Reachable::Gone`].
fn read_proc_starttime(pid: u32) -> Option<u64> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat_starttime(&raw)
}

/// Extract field 22 (`starttime`) from a raw `/proc/<pid>/stat` line.
///
/// Field 2 (`comm`, the process name) is wrapped in parentheses and MAY
/// itself contain spaces or parentheses (e.g. a process renamed to `weird
/// (name) here`), so splitting the whole line on whitespace misindexes every
/// field that follows. The kernel's own `/proc` formatter guarantees comm
/// cannot contain a `)` that is followed by " (" — so the LAST `)` in the
/// line unambiguously ends the comm field; everything after it is field 3
/// (`state`) onward, safe to whitespace-split. Field 22 is then at index 19
/// of that split (state=index 0, ..., starttime=field 22=index 22-3=19).
fn parse_stat_starttime(stat_line: &str) -> Option<u64> {
    let close = stat_line.rfind(')')?;
    let rest = stat_line.get(close + 1..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    fields.get(19)?.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).expect("write fixture file");
    }

    // The real descriptor from the task brief, verbatim.
    const WELL_FORMED: &str = r#"{
  "pid": 936710,
  "sessionId": "d336c249-5e57-4b35-89cb-ae92dcc8fc03",
  "cwd": "/home/atobey/src/kaijutsu",
  "startedAt": 1786737813607,
  "procStart": "97366068",
  "version": "2.1.232",
  "peerProtocol": 1,
  "kind": "interactive",
  "entrypoint": "cli",
  "messagingSocketPath": "/run/user/1000/cc-socks/936710.sock",
  "name": "kaijutsu-chan",
  "nameSince": 1786737813610,
  "updatedAt": 1786740497924,
  "status": "busy",
  "statusUpdatedAt": 1786740497924
}"#;

    // ── parses a well-formed descriptor into the expected struct ──────────

    #[test]
    fn parses_well_formed_descriptor() {
        let parsed: CcSessionFile =
            serde_json::from_str(WELL_FORMED).expect("well-formed descriptor must parse");
        assert_eq!(parsed.pid, 936710);
        assert_eq!(parsed.session_id, "d336c249-5e57-4b35-89cb-ae92dcc8fc03");
        assert_eq!(parsed.cwd, "/home/atobey/src/kaijutsu");
        assert_eq!(parsed.proc_start, "97366068");
        assert_eq!(parsed.version, "2.1.232");
        assert_eq!(parsed.peer_protocol, 1);
        assert_eq!(
            parsed.messaging_socket_path,
            "/run/user/1000/cc-socks/936710.sock"
        );
        assert_eq!(parsed.name.as_deref(), Some("kaijutsu-chan"));
        assert_eq!(parsed.status, "busy");
    }

    #[test]
    fn scan_finds_the_well_formed_descriptor_via_a_temp_dir() {
        let dir = tempfile::tempdir().expect("tmpdir");
        // Use OUR OWN live pid + its real procStart so this session reads as
        // Alive without depending on any other fixture pid being alive.
        let own_pid = std::process::id();
        let own_start = read_proc_starttime(own_pid).expect("own /proc/self/stat must parse");
        let body = serde_json::json!({
            "pid": own_pid,
            "sessionId": "11111111-1111-1111-1111-111111111111",
            "cwd": "/tmp/somewhere",
            "startedAt": 1,
            "procStart": own_start.to_string(),
            "version": "2.1.232",
            "peerProtocol": 1,
            "kind": "interactive",
            "entrypoint": "cli",
            "messagingSocketPath": "/run/user/1000/cc-socks/x.sock",
            "name": "here",
            "nameSince": 1,
            "updatedAt": 1,
            "status": "shell",
            "statusUpdatedAt": 1
        })
        .to_string();
        write(dir.path(), &format!("{own_pid}.json"), &body);

        let scan = scan_sessions_dir(dir.path()).expect("scan must succeed");
        assert_eq!(scan.sessions.len(), 1, "the one descriptor must be found");
        assert!(scan.errors.is_empty());
        let entry = &scan.sessions[0];
        assert_eq!(entry.pid, own_pid);
        assert_eq!(entry.reachable, Reachable::Alive);
    }

    // ── procStart disagreement with a real live pid => Stale ──────────────

    #[test]
    fn procstart_mismatch_against_a_real_live_pid_is_stale() {
        let own_pid = std::process::id();
        let own_start = read_proc_starttime(own_pid).expect("own /proc/self/stat must parse");
        // Deliberately wrong: one more than the real, live value.
        let wrong = (own_start + 1).to_string();

        let reachable = classify_reachability(own_pid, &wrong);
        assert_eq!(
            reachable,
            Reachable::Stale,
            "a live pid whose recorded procStart disagrees with reality must read as Stale"
        );
    }

    #[test]
    fn scan_reports_procstart_mismatch_as_stale_not_alive() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let own_pid = std::process::id();
        let own_start = read_proc_starttime(own_pid).expect("own /proc/self/stat must parse");
        let wrong = own_start + 1;
        let body = serde_json::json!({
            "pid": own_pid,
            "sessionId": "22222222-2222-2222-2222-222222222222",
            "cwd": "/tmp/somewhere",
            "startedAt": 1,
            "procStart": wrong.to_string(),
            "version": "2.1.232",
            "peerProtocol": 1,
            "messagingSocketPath": "/run/user/1000/cc-socks/y.sock",
            "status": "busy"
        })
        .to_string();
        write(dir.path(), &format!("{own_pid}.json"), &body);

        let scan = scan_sessions_dir(dir.path()).expect("scan must succeed");
        assert_eq!(scan.sessions.len(), 1);
        assert_eq!(scan.sessions[0].reachable, Reachable::Stale);
    }

    // ── a pid that does not exist at all => not alive (Gone) ──────────────

    #[test]
    fn nonexistent_pid_is_gone() {
        // Linux's pid_max is at most 2^22 (4194304) even on 64-bit systems
        // with the widened default; this value is far beyond any pid the
        // kernel could ever hand out.
        let never_a_pid: u32 = 4_200_000_000;
        assert_eq!(read_proc_starttime(never_a_pid), None);
        assert_eq!(
            classify_reachability(never_a_pid, "1"),
            Reachable::Gone,
            "a pid with no /proc entry at all must read as Gone, not Stale"
        );
    }

    #[test]
    fn scan_reports_nonexistent_pid_as_gone() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let never_a_pid: u32 = 4_200_000_000;
        let body = serde_json::json!({
            "pid": never_a_pid,
            "sessionId": "33333333-3333-3333-3333-333333333333",
            "cwd": "/tmp/somewhere",
            "startedAt": 1,
            "procStart": "1",
            "version": "2.1.232",
            "peerProtocol": 1,
            "messagingSocketPath": "/run/user/1000/cc-socks/z.sock",
            "status": "busy"
        })
        .to_string();
        write(dir.path(), &format!("{never_a_pid}.json"), &body);

        let scan = scan_sessions_dir(dir.path()).expect("scan must succeed");
        assert_eq!(scan.sessions.len(), 1);
        assert_eq!(scan.sessions[0].reachable, Reachable::Gone);
    }

    // ── missing sessions dir => empty roster, no error ─────────────────────

    #[test]
    fn missing_sessions_dir_is_an_empty_roster_not_an_error() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let missing = dir.path().join("does-not-exist").join("sessions");
        let scan = scan_sessions_dir(&missing).expect("a missing dir must not be an error");
        assert!(scan.sessions.is_empty());
        assert!(scan.errors.is_empty());
    }

    // ── a malformed descriptor is surfaced, never dropped or panicked on ──

    #[test]
    fn malformed_json_is_reported_as_an_error_entry() {
        let dir = tempfile::tempdir().expect("tmpdir");
        write(dir.path(), "12345.json", "{ not valid json at all");
        // A well-formed sibling too, so the malformed one being surfaced
        // doesn't abort or hide the rest of the listing.
        let own_pid = std::process::id();
        let own_start = read_proc_starttime(own_pid).expect("own /proc/self/stat must parse");
        write(
            dir.path(),
            &format!("{own_pid}.json"),
            &serde_json::json!({
                "pid": own_pid,
                "sessionId": "44444444-4444-4444-4444-444444444444",
                "cwd": "/tmp",
                "startedAt": 1,
                "procStart": own_start.to_string(),
                "version": "2.1.232",
                "peerProtocol": 1,
                "messagingSocketPath": "/run/user/1000/cc-socks/w.sock",
                "status": "shell"
            })
            .to_string(),
        );

        let scan = scan_sessions_dir(dir.path()).expect("scan itself must not fail");
        assert_eq!(
            scan.sessions.len(),
            1,
            "the well-formed sibling must still be listed"
        );
        assert_eq!(scan.errors.len(), 1, "the malformed file must be reported, not dropped");
        assert_eq!(scan.errors[0].file, "12345.json");
        assert!(
            !scan.errors[0].message.is_empty(),
            "the error must carry a reason, not just a filename"
        );
    }

    // ── .key files are never opened, read, or stat'd ───────────────────────

    #[test]
    fn key_files_are_never_touched() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let key_path = dir.path().join("12345.deadbeef00000000000000000000000000000000000000000000000000dead.key");
        std::fs::write(&key_path, b"a bearer token").expect("write key fixture");
        // chmod 000: any attempted open (even to check existence in a way
        // that reads it) would fail loudly under a non-root test user. A
        // silent scan.errors entry for this file would itself prove the
        // constraint was violated (an open was attempted), so absence of
        // any error/session referencing it is the assertion.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o000))
                .expect("chmod key fixture");
        }

        let scan = scan_sessions_dir(dir.path()).expect("scan must succeed");
        assert!(scan.sessions.is_empty());
        assert!(
            scan.errors.is_empty(),
            "a .key file must never be opened at all, so it can never produce a parse/read error: {:?}",
            scan.errors
        );
    }

    // ── /proc/<pid>/stat field extraction with a comm containing spaces and
    //    parentheses — the trap naive whitespace-splitting falls into ──────

    #[test]
    fn parses_starttime_past_a_comm_with_spaces_and_parens() {
        let line = "1234 (weird (name) here) S 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 5551212";
        assert_eq!(parse_stat_starttime(line), Some(5551212));
    }

    #[test]
    fn parses_starttime_with_a_plain_comm() {
        let line = "1 (systemd) S 0 1 1 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 42";
        assert_eq!(parse_stat_starttime(line), Some(42));
    }

    #[test]
    fn malformed_stat_line_returns_none_rather_than_panicking() {
        assert_eq!(parse_stat_starttime("no parens at all here"), None);
        assert_eq!(parse_stat_starttime(""), None);
        assert_eq!(parse_stat_starttime("1 (ok) S 1 1"), None); // too few fields
    }

    // ── real /proc/self/stat round-trips (sanity check against the live
    //    system, not just a synthetic line) ────────────────────────────────

    #[test]
    fn real_proc_self_stat_parses() {
        let own_pid = std::process::id();
        let start = read_proc_starttime(own_pid);
        assert!(start.is_some(), "our own /proc/<pid>/stat must parse");
    }

    // ── clap wiring, via the full dispatcher — deliberately no filesystem
    //    dependency (never touches real ~/.claude) ─────────────────────────

    mod dispatch_wiring {
        use crate::kj::KjCaller;
        use crate::kj::test_helpers::*;

        fn s(v: &str) -> String {
            v.to_string()
        }

        #[tokio::test]
        async fn cc_bare_renders_help_without_error() {
            let d = test_dispatcher().await;
            let c = test_caller();
            let result = d.dispatch(&[s("cc")], &c).await;
            assert!(
                matches!(&result, crate::kj::KjResult::Ok { ephemeral: true, .. }),
                "kj cc (no subcommand) should render help, got {result:?}"
            );
        }

        #[tokio::test]
        async fn cc_unknown_subcommand_errors() {
            let d = test_dispatcher().await;
            let c = test_caller();
            let result = d.dispatch(&[s("cc"), s("bogus")], &c).await;
            assert!(!result.is_ok(), "unknown `kj cc` subcommand must error");
        }

        /// `kj cc` must not require an active context — it reads a host
        /// filesystem registry, not a kaijutsu context. Exercised via `--help`
        /// so this stays filesystem-independent (no real ~/.claude touch).
        #[tokio::test]
        async fn cc_works_without_a_joined_context() {
            let d = test_dispatcher().await;
            let c = KjCaller {
                principal_id: kaijutsu_types::PrincipalId::new(),
                context_id: None,
                session_id: kaijutsu_types::SessionId::new(),
                confirmed: false,
                rc_depth: 0,
                privileged: false,
            };
            let result = d.dispatch(&[s("cc"), s("--help")], &c).await;
            assert!(
                !result
                    .message()
                    .contains("no active context joined"),
                "kj cc must be exempt from the active-context gate, got: {}",
                result.message()
            );
        }
    }
}
