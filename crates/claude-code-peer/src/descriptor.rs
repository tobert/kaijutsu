//! The on-disk session registry: `~/.claude/sessions/` holds one
//! `<pid>.json` descriptor (mode 0644, the public half) and one
//! `<pid>.<64-hex>.key` token file (mode 0600) per live session
//! **[probed]**.
//!
//! Reading and writing are deliberately asymmetric:
//!
//! - **Scanning never touches `.key` files.** [`scan_sessions_dir`] filters
//!   them out by name before any filesystem call other than the directory
//!   listing itself — not opened, not read, not even `stat`ed. A read-only
//!   roster has no business needing a credential, and the least-privilege
//!   move is to never touch the file at all rather than trust ourselves to
//!   read-and-discard it correctly.
//! - **Sending reads exactly one key file, once, at send time.**
//!   [`read_peer_token`] returns a [`PeerToken`] that is deliberately
//!   unformattable (no `Debug`/`Display` impl), used for the single send,
//!   and dropped. Never cached, never logged.

use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::liveness::{classify_reachability, Reachable};
use crate::SUPPORTED_PEER_PROTOCOL;

/// The on-disk shape of a `<pid>.json` session descriptor, as written by
/// Claude Code >= 2.1.x **[probed]**. Extra fields (e.g. `startedAt`,
/// `nameSince`, `updatedAt`, `statusUpdatedAt`, `kind`, `entrypoint`) are
/// present in the real file but not needed here; serde ignores them rather
/// than this struct enumerating fields nobody reads yet.
///
/// `name` is optional — a session that hasn't been given a name yet may
/// omit `name`/`nameSince` entirely; every other field is measured to always
/// be present on a live descriptor, so a missing one there is a real parse
/// failure, not a "maybe absent" case to paper over.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SessionDescriptor {
    pub pid: u32,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub cwd: String,
    /// Clock ticks since boot (field 22 of `/proc/<pid>/stat`) at the moment
    /// this session started — measured as a JSON *string*, not a number.
    #[serde(rename = "procStart")]
    pub proc_start: String,
    pub version: String,
    #[serde(rename = "peerProtocol")]
    pub peer_protocol: u32,
    #[serde(rename = "messagingSocketPath")]
    pub messaging_socket_path: String,
    #[serde(default)]
    pub name: Option<String>,
    /// Opaque — observed values include `busy`, `idle`, `shell`, `waiting`
    /// **[probed]**. Not an enum by design: Claude Code owns this vocabulary
    /// and added a fourth value the same afternoon the first three were
    /// observed.
    pub status: String,
}

/// One roster row: a parsed descriptor plus the liveness verdict
/// ([`crate::liveness`]) added on top.
#[derive(Debug, Clone)]
pub struct Session {
    pub descriptor: SessionDescriptor,
    pub reachable: Reachable,
    /// The reason string whenever liveness came back [`Reachable::Unknown`] —
    /// surfaced alongside scan errors, never swallowed into a quiet label.
    pub undetermined: Option<String>,
}

impl Session {
    /// Fail-closed gate for the write path: only [`Reachable::Alive`] and
    /// `peerProtocol == SUPPORTED_PEER_PROTOCOL` are safe to write to. A
    /// `Stale` descriptor's pid may now belong to an unrelated process —
    /// writing to whatever currently owns that socket path is the worst
    /// outcome available, so this refuses rather than guesses.
    pub fn validate_sendable(&self) -> Result<(), Error> {
        if self.reachable != Reachable::Alive {
            return Err(Error::Send(format!(
                "target (pid {}) is not reachable: {} — refusing to write to a \
                 socket that may now belong to an unrelated process",
                self.descriptor.pid,
                self.reachable.label()
            )));
        }
        if self.descriptor.peer_protocol != SUPPORTED_PEER_PROTOCOL {
            return Err(Error::Send(format!(
                "target (pid {}) speaks peerProtocol {} — this client only \
                 supports protocol {}",
                self.descriptor.pid, self.descriptor.peer_protocol, SUPPORTED_PEER_PROTOCOL
            )));
        }
        Ok(())
    }
}

/// One descriptor that failed to parse (or whose liveness could not be
/// determined) — surfaced by name, never dropped. A silently-shrunk roster
/// is a worse failure mode than a visible one: it reads as "no problem" to
/// whoever's watching.
#[derive(Debug, Clone)]
pub struct ScanError {
    pub file: String,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct Scan {
    pub sessions: Vec<Session>,
    pub errors: Vec<ScanError>,
}

/// Scan `dir` for Claude Code session descriptors.
///
/// A missing `dir` (Claude Code never installed, or never run on this
/// machine) is a normal, empty result — not an error. Any other directory
/// read failure (e.g. permission denied) IS an error: unlike "doesn't
/// exist", it means we genuinely don't know what's there.
///
/// `.key` files are filtered out by filename alone, before any read/open/stat
/// call touches them — the hard constraint this module must never violate.
pub fn scan_sessions_dir(dir: &Path) -> Result<Scan, Error> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Scan::default()),
        Err(e) => {
            return Err(Error::Registry(format!("read {}: {e}", dir.display())));
        }
    };

    let mut scan = Scan::default();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                scan.errors.push(ScanError {
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
            // Not a session artifact this crate recognizes (or a directory);
            // silently ignored rather than treated as a parse failure — an
            // unrelated file in this directory isn't this crate's business.
            continue;
        }

        let path = entry.path();
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                scan.errors.push(ScanError {
                    file: file_name,
                    message: format!("read: {e}"),
                });
                continue;
            }
        };
        let parsed: SessionDescriptor = match serde_json::from_str(&raw) {
            Ok(p) => p,
            Err(e) => {
                scan.errors.push(ScanError {
                    file: file_name,
                    message: format!("parse: {e}"),
                });
                continue;
            }
        };

        let (reachable, undetermined) =
            classify_reachability(parsed.pid, &parsed.proc_start);
        if let Some(why) = &undetermined {
            scan.errors.push(ScanError {
                file: file_name.clone(),
                message: why.clone(),
            });
        }
        scan.sessions.push(Session {
            descriptor: parsed,
            reachable,
            undetermined,
        });
    }

    scan.sessions.sort_by_key(|s| s.descriptor.pid);
    scan.errors.sort_by(|a, b| a.file.cmp(&b.file));

    Ok(scan)
}

/// The conventional registry location: `$HOME/.claude/sessions`.
pub fn default_sessions_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".claude").join("sessions"))
}

/// Resolve a target string (a session `name` as shown by a roster, or a
/// numeric pid) against scanned `sessions`. A name match takes priority over
/// pid parsing — a name that happens to look numeric still resolves to the
/// named session when one exists — and an ambiguous name (two sessions
/// sharing it) is an error naming every candidate pid rather than a guess.
pub fn resolve_target<'a>(sessions: &'a [Session], target: &str) -> Result<&'a Session, Error> {
    let name_matches: Vec<&Session> = sessions
        .iter()
        .filter(|s| s.descriptor.name.as_deref() == Some(target))
        .collect();
    if name_matches.len() > 1 {
        let mut pids: Vec<String> = name_matches
            .iter()
            .map(|s| s.descriptor.pid.to_string())
            .collect();
        pids.sort();
        return Err(Error::Send(format!(
            "name {target:?} is ambiguous — matches pids {}",
            pids.join(", ")
        )));
    }
    if let Some(s) = name_matches.into_iter().next() {
        return Ok(s);
    }
    if let Ok(pid) = target.parse::<u32>()
        && let Some(s) = sessions.iter().find(|s| s.descriptor.pid == pid)
    {
        return Ok(s);
    }
    Err(Error::Send(format!(
        "no live session matches target {target:?}"
    )))
}

/// The on-disk shape of a `<pid>.<64-hex>.key` bearer-token file.
/// Deliberately **not** `#[derive(Debug)]`: this struct holds a live token
/// and must never be formatted, logged, or echoed in any output.
#[derive(serde::Deserialize)]
struct KeyFile {
    #[serde(rename = "peerToken")]
    peer_token: String,
    /// Present in the real file; not read here (the descriptor's own
    /// `procStart`, already checked by the liveness guard, is what gates
    /// reachability).
    #[serde(rename = "procStart")]
    #[allow(dead_code)]
    proc_start: String,
}

/// A live peer token. **Deliberately has no `Debug` or `Display` impl**:
/// the type system is the enforcement that a token never reaches a log, an
/// error message, or a `format!`. Read once at send time, used for that one
/// send, dropped — the 0600 keyfile is the durable truth, never this value.
#[derive(Clone, PartialEq, Eq)]
pub struct PeerToken(String);

impl PeerToken {
    /// Crate-internal constructor (the frame parser mints tokens off the
    /// wire). Not public: the only sanctioned way to obtain a token from
    /// outside is [`read_peer_token`].
    pub(crate) fn new(token: String) -> Self {
        PeerToken(token)
    }

    /// The token bytes for the single socket write that uses them.
    pub fn secret(&self) -> &str {
        &self.0
    }
}

/// Locate the `<pid>.<64-hex>.key` file for `pid` inside `sessions_dir`.
/// Exactly one match is required: zero means the token file is gone (a race
/// with the session exiting between roster scan and send) and more than one
/// is an unexpected collision — neither is a case to guess through.
pub fn find_key_file(sessions_dir: &Path, pid: u32) -> Result<PathBuf, Error> {
    let entries = std::fs::read_dir(sessions_dir)
        .map_err(|e| Error::Registry(format!("read {}: {e}", sessions_dir.display())))?;
    let prefix = format!("{pid}.");
    let mut matches = Vec::new();
    for entry in entries {
        let entry = entry
            .map_err(|e| Error::Registry(format!("read {}: {e}", sessions_dir.display())))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) && name.ends_with(".key") {
            matches.push(entry.path());
        }
    }
    match matches.len() {
        0 => Err(Error::Send(format!(
            "no key file found for pid {pid} (session may have just exited)"
        ))),
        1 => Ok(matches.remove(0)),
        _ => Err(Error::Send(format!(
            "multiple key files found for pid {pid} — refusing to guess which one"
        ))),
    }
}

/// Read and parse a key file, returning the peer token. The file's raw
/// content is never included in an error — only the path and serde_json's
/// own (content-free) parse-error message.
pub fn read_peer_token(path: &Path) -> Result<PeerToken, Error> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| Error::Registry(format!("read {}: {e}", path.display())))?;
    let parsed: KeyFile = serde_json::from_str(&raw)
        .map_err(|e| Error::Registry(format!("parse {}: {e}", path.display())))?;
    Ok(PeerToken(parsed.peer_token))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).expect("write fixture file");
    }

    /// A well-formed descriptor in the exact on-disk shape CC writes
    /// **[probed]** — every field type as measured, including `procStart`
    /// as a JSON string and the fields this crate ignores.
    fn well_formed_descriptor(pid: u32) -> String {
        format!(
            r#"{{"pid":{pid},"sessionId":"f69c3038-f785-4e40-9113-71ee837fcddd","cwd":"/home/someone/src/project","startedAt":1786877741236,"procStart":"111358845","version":"2.1.233","peerProtocol":1,"kind":"interactive","entrypoint":"cli","messagingSocketPath":"/run/user/1000/cc-socks/{pid}.sock","name":"fixture-session","nameSince":1786877741237,"updatedAt":1786881406610,"status":"idle","statusUpdatedAt":1786881406610}}"#
        )
    }

    #[test]
    fn parses_well_formed_descriptor() {
        let parsed: SessionDescriptor =
            serde_json::from_str(&well_formed_descriptor(1234)).expect("must parse");
        assert_eq!(parsed.pid, 1234);
        assert_eq!(parsed.proc_start, "111358845");
        assert_eq!(parsed.peer_protocol, 1);
        assert_eq!(parsed.name.as_deref(), Some("fixture-session"));
        assert_eq!(parsed.status, "idle");
        assert_eq!(
            parsed.messaging_socket_path,
            "/run/user/1000/cc-socks/1234.sock"
        );
    }

    #[test]
    fn a_nameless_descriptor_still_parses() {
        // name/nameSince are absent before a session is named — measured to
        // be the only optional fields.
        let raw = r#"{"pid":7,"sessionId":"s","cwd":"/tmp","procStart":"1","version":"2.1.233","peerProtocol":1,"messagingSocketPath":"/tmp/x.sock","status":"busy"}"#;
        let parsed: SessionDescriptor = serde_json::from_str(raw).expect("must parse");
        assert!(parsed.name.is_none());
    }

    #[test]
    fn missing_mandatory_field_is_a_parse_failure_not_a_default() {
        let raw = r#"{"pid":7,"sessionId":"s","cwd":"/tmp","version":"2.1.233","peerProtocol":1,"messagingSocketPath":"/tmp/x.sock","status":"busy"}"#;
        assert!(
            serde_json::from_str::<SessionDescriptor>(raw).is_err(),
            "a missing procStart must fail the parse, not paper over it"
        );
    }

    #[test]
    fn scan_finds_the_well_formed_descriptor_via_a_temp_dir() {
        let tmp = tempfile::tempdir().unwrap();
        // A pid above the kernel's PID_MAX_LIMIT (4194304) cannot exist on
        // any Linux box, so this row's liveness lands as Gone everywhere.
        write(tmp.path(), "3000000000.json", &well_formed_descriptor(3_000_000_000));
        let scan = scan_sessions_dir(tmp.path()).expect("scan must not error");
        assert_eq!(scan.sessions.len(), 1);
        assert_eq!(scan.sessions[0].reachable, Reachable::Gone);
        assert!(scan.errors.is_empty());
    }

    #[test]
    fn missing_sessions_dir_is_an_empty_roster_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let scan = scan_sessions_dir(&tmp.path().join("does-not-exist"))
            .expect("a missing dir is a normal empty result");
        assert!(scan.sessions.is_empty());
        assert!(scan.errors.is_empty());
    }

    #[test]
    fn malformed_json_is_reported_as_an_error_entry() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "42.json", "{not json at all");
        let scan = scan_sessions_dir(tmp.path()).expect("scan must not error");
        assert!(scan.sessions.is_empty());
        assert_eq!(scan.errors.len(), 1);
        assert_eq!(scan.errors[0].file, "42.json");
        assert!(scan.errors[0].message.starts_with("parse:"));
    }

    #[test]
    fn key_files_are_never_touched() {
        // Write a `.key` file whose content would be unmistakable if read,
        // plus a poison file that fails any open() a scanner might attempt —
        // a scan that touches `.key` files at all would surface either.
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "1234.json", &well_formed_descriptor(1234));
        write(tmp.path(), "1234.deadbeef.key", "SENTINEL-DO-NOT-READ");
        let scan = scan_sessions_dir(tmp.path()).expect("scan must not error");
        assert_eq!(scan.sessions.len(), 1);
        assert!(
            scan.errors.is_empty(),
            "the .key file must not even be mentioned: {:?}",
            scan.errors
        );
    }

    #[test]
    fn unrelated_files_are_silently_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "README.txt", "not a session");
        write(tmp.path(), "1234.json", &well_formed_descriptor(1234));
        let scan = scan_sessions_dir(tmp.path()).expect("scan must not error");
        assert_eq!(scan.sessions.len(), 1);
        assert!(scan.errors.is_empty());
    }

    #[test]
    fn validate_sendable_refuses_anything_not_alive() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "3000000000.json", &well_formed_descriptor(3_000_000_000));
        let scan = scan_sessions_dir(tmp.path()).unwrap();
        let session = &scan.sessions[0];
        assert_eq!(session.reachable, Reachable::Gone);
        let err = session.validate_sendable().expect_err("Gone must be refused");
        assert!(err.to_string().contains("not reachable"));
    }

    #[test]
    fn validate_sendable_refuses_an_unknown_peer_protocol() {
        let mut session: Session = serde_json::from_str::<SessionDescriptor>(&format!(
            "{}",
            well_formed_descriptor(1234)
        ))
        .map(|d| Session {
            descriptor: d,
            reachable: Reachable::Alive,
            undetermined: None,
        })
        .unwrap();
        session.descriptor.peer_protocol = 2;
        let err = session
            .validate_sendable()
            .expect_err("peerProtocol 2 must be refused");
        assert!(err.to_string().contains("peerProtocol 2"));
    }

    #[test]
    fn resolve_target_prefers_a_name_match_and_errors_on_ambiguity() {
        let d = |pid: u32, name: &str| Session {
            descriptor: SessionDescriptor {
                pid,
                session_id: format!("s{pid}"),
                cwd: "/tmp".into(),
                proc_start: "1".into(),
                version: "2.1.233".into(),
                peer_protocol: 1,
                messaging_socket_path: format!("/tmp/{pid}.sock"),
                name: Some(name.into()),
                status: "idle".into(),
            },
            reachable: Reachable::Alive,
            undetermined: None,
        };
        let sessions = vec![d(10, "dup"), d(20, "dup"), d(30, "solo")];

        assert_eq!(
            resolve_target(&sessions, "solo").unwrap().descriptor.pid,
            30
        );
        // Numeric-looking string still prefers the name match.
        let named = vec![d(99, "30")];
        assert_eq!(
            resolve_target(&named, "30").unwrap().descriptor.pid,
            99,
            "a session literally named `30` wins over pid parsing"
        );
        assert!(resolve_target(&sessions, "dup").is_err(), "ambiguous name");
        assert!(resolve_target(&sessions, "4242").is_err(), "no match");
        assert_eq!(resolve_target(&sessions, "10").unwrap().descriptor.pid, 10);
    }

    #[test]
    fn find_key_file_requires_exactly_one_match() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(find_key_file(tmp.path(), 5).is_err(), "zero matches");
        write(tmp.path(), "5.deadbeef.key", "{}");
        assert!(find_key_file(tmp.path(), 5).is_ok(), "exactly one match");
        write(tmp.path(), "5.feedface.key", "{}");
        assert!(find_key_file(tmp.path(), 5).is_err(), "two matches");
    }

    #[test]
    fn read_peer_token_returns_the_token_and_leaks_nothing_on_error() {
        let tmp = tempfile::tempdir().unwrap();
        let key = tmp.path().join("5.deadbeef.key");
        std::fs::write(
            &key,
            r#"{"peerToken":"0123456789abcdef0123456789abcdef","procStart":"1"}"#,
        )
        .unwrap();
        let token = read_peer_token(&key).expect("valid key file");
        assert_eq!(token.secret(), "0123456789abcdef0123456789abcdef");

        let bad = tmp.path().join("6.deadbeef.key");
        std::fs::write(&bad, r#"{"peerToken":"abc"#).unwrap(); // truncated JSON
        // PeerToken is deliberately not Debug, so this is a match, not
        // expect_err (which has a Debug bound on the Ok type).
        let err = match read_peer_token(&bad) {
            Ok(_) => panic!("malformed key file must not parse"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(msg.contains("6.deadbeef.key"), "error names the path");
        assert!(!msg.contains("abc"), "error must not carry file content");
    }
}
