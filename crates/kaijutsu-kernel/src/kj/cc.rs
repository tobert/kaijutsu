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
//!
//! ## `kj cc send` — the write path (this slice)
//!
//! Delivers a message into a live session's inbox over its
//! `messagingSocketPath` unix stream socket: two newline-delimited JSON
//! frames (`auth` then `user`), no ack, then close. Protocol measured live
//! 2026-08-14 against Claude Code 2.1.232 — see `docs/issues.md` ("`drift
//! --drive`") for the fuller writeup. Three things this slice is strict
//! about, all load-bearing:
//!
//! 1. **Fail closed before opening a socket.** A target must resolve to
//!    [`Reachable::Alive`] and `peerProtocol == 1`. Writing to a `Stale`
//!    descriptor's socket path means writing to whatever unrelated process
//!    now owns that pid — worse than refusing outright. See
//!    [`validate_target`].
//! 2. **The token is read, used, and dropped — never logged or Debugged.**
//!    [`CcKeyFile`] deliberately does not derive `Debug`. `--dry-run` shows
//!    the real frame *shape* but redacts the token to the literal string
//!    `"<redacted>"` rather than reusing any value derived from the real
//!    one.
//! 3. **Envelope injection is rejected, not encoded around.** Claude Code's
//!    receiver re-serializes what its regex extracted and rejects the parse
//!    unless that exactly equals the input — a canonicalization check. A
//!    near-miss (wrong attribute order, missing the mandatory `>\n`/`\n<`
//!    body newlines, or a body that smuggles a second closing tag) doesn't
//!    error on the wire: the message still arrives, just **anonymously**,
//!    attribution silently dropped. That silent downgrade — not a loud
//!    failure — is the specific hazard [`build_envelope`] and
//!    [`validate_message_body`] exist to prevent.
//!
//! This slice does not gate `send` behind a `Capability` the way `kj drift
//! push` gates on [`crate::mcp::Capability::Drift`] — `kj cc send` talks to
//! a process outside kaijutsu entirely, and `docs/issues.md` records that
//! whether cross-session delivery should route through the approval ledger
//! is still Amy's open call (`--drive`'s ledger-gating question, same
//! surface). Left ungated here rather than guessed at; flag before this
//! path is exposed beyond a deliberate `kj cc send` invocation.

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
    /// Deliver a message into a live session's inbox over its messaging
    /// socket, attributed as from this kaijutsu kernel. Refuses any target
    /// that isn't Alive or doesn't speak peerProtocol 1. Place `--dry-run`
    /// before <target> — the message is a trailing variadic argument and
    /// will otherwise swallow a later flag.
    Send {
        /// Target session: a `name` as shown by `kj cc list`, or a numeric pid.
        target: String,
        /// Message body (all remaining words joined with spaces).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        message: Vec<String>,
        /// Print the exact bytes that would be written (token redacted); send nothing.
        #[arg(long)]
        dry_run: bool,
    },
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

        // `List` is read-only host introspection, same ungated rationale as
        // `kj cas ls`/`kj mcp list`/`kj midi list`. `Send` is a write to an
        // external process; it is ALSO ungated for now — see the module
        // doc's "`kj cc send` — the write path" section for why that's a
        // deliberate deferral, not an oversight.
        match parsed.command {
            CcCommand::List => self.cc_list(),
            CcCommand::Send {
                target,
                message,
                dry_run,
            } => self.cc_send(&target, &message.join(" "), dry_run),
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
            // Not only parse failures now: an undetermined liveness lands here
            // too, so the heading must not claim a specific cause.
            lines.push(format!(
                "{} descriptor problem{}:",
                scan.errors.len(),
                if scan.errors.len() == 1 { "" } else { "s" }
            ));
            for e in &scan.errors {
                lines.push(format!("  ! {}: {}", e.file, e.message));
            }
        }

        KjResult::ok_with_data(lines.join("\n"), data)
    }

    /// `kj cc send <target> <message>` — resolve `target` against the live
    /// roster, fail closed on reachability/protocol, then deliver `message`
    /// as an attributed envelope over the target's messaging socket.
    ///
    /// Thin wrapper over [`cc_send_inner`]: this method supplies the real
    /// `~/.claude/sessions` dir and this kernel's identity; the actual logic
    /// lives in the free function so tests can inject a temp dir instead of
    /// touching the real registry, same shape as `cc_list`/`scan_sessions_dir`.
    fn cc_send(&self, target: &str, message: &str, dry_run: bool) -> KjResult {
        let home = match std::env::var_os("HOME") {
            Some(h) if !h.is_empty() => PathBuf::from(h),
            _ => return KjResult::Err("kj cc send: $HOME is not set".to_string()),
        };
        let sessions_dir = home.join(".claude").join("sessions");
        cc_send_inner(&sessions_dir, FROM_NAME, target, message, dry_run)
    }
}

fn reach_label(r: Reachable) -> &'static str {
    match r {
        Reachable::Alive => "alive",
        Reachable::Stale => "stale",
        Reachable::Gone => "gone",
        Reachable::Unknown => "unknown",
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
    /// We could not determine liveness: `/proc/<pid>/stat` was unreadable for
    /// a reason other than the process being absent, its format was not one we
    /// recognize, or the descriptor's own `procStart` was not a number.
    ///
    /// This variant exists so those cases cannot masquerade as `Gone`. `Gone`
    /// asserts a fact ("that process is dead"); if a `/proc` format shift or a
    /// bug in our parser were folded into it, *every* session would report
    /// dead and the roster would look plausibly empty instead of obviously
    /// broken — a silent failure that reads as a true answer.
    Unknown,
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

        let (reachable, undetermined_why) = classify_reachability(parsed.pid, &parsed.proc_start);
        if let Some(why) = undetermined_why {
            scan.errors.push(CcScanError {
                file: file_name.clone(),
                message: why,
            });
        }
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
/// Classify a descriptor's liveness, plus a reason whenever the answer is
/// [`Reachable::Unknown`]. The reason is not decoration: an undetermined
/// liveness is reported alongside parse failures so it cannot pass as a quiet
/// label in a table nobody reads closely.
fn classify_reachability(pid: u32, recorded_proc_start: &str) -> (Reachable, Option<String>) {
    let live_start = match probe_proc_starttime(pid) {
        ProcProbe::Started(s) => s,
        ProcProbe::NoSuchProcess => return (Reachable::Gone, None),
        ProcProbe::Undetermined(why) => return (Reachable::Unknown, Some(why)),
    };
    match recorded_proc_start.trim().parse::<u64>() {
        Ok(recorded) if recorded == live_start => (Reachable::Alive, None),
        Ok(_) => (Reachable::Stale, None),
        // A non-numeric `procStart` is a corrupt descriptor, not a reused pid.
        // Calling it `Stale` would assert a specific thing we did not observe.
        Err(_) => (
            Reachable::Unknown,
            Some(format!(
                "procStart {recorded_proc_start:?} is not a number — corrupt descriptor"
            )),
        ),
    }
}

/// Outcome of reading field 22 (`starttime`) from `/proc/<pid>/stat`.
///
/// The three cases are kept apart on purpose: only an absent process licenses
/// [`Reachable::Gone`]. See that variant's note.
enum ProcProbe {
    Started(u64),
    NoSuchProcess,
    Undetermined(String),
}

fn probe_proc_starttime(pid: u32) -> ProcProbe {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(raw) => match parse_stat_starttime(&raw) {
            Some(s) => ProcProbe::Started(s),
            None => ProcProbe::Undetermined(format!("/proc/{pid}/stat: unrecognized format")),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ProcProbe::NoSuchProcess,
        Err(e) => ProcProbe::Undetermined(format!("/proc/{pid}/stat: {e}")),
    }
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

// ============================================================================
// Send path — envelope, target resolution, key handling, socket delivery.
// Free functions taking an injected `sessions_dir` (like `scan_sessions_dir`
// above) so tests never touch the real ~/.claude registry or a real session.
// ============================================================================

/// Literal closing tag whose presence anywhere in a message body is an
/// envelope-injection hazard: Claude Code's receiver re-serializes what it
/// extracted and rejects the parse unless it exactly matches the input, so a
/// body containing a second `</cross-session-message>` can make the parse
/// boundary ambiguous rather than erroring loudly. Reject it outright.
const CLOSING_TAG: &str = "</cross-session-message>";

/// The `from-name` attribute value.
const FROM_NAME: &str = "kaijutsu";

/// Characters the receiver's `from-name` charset (`[^"<>\n\r]+`) forbids.
/// A name containing any of them makes the envelope fail to match, which
/// **delivers anyway and silently drops attribution** — so this is rejected
/// rather than escaped. Escaping would produce a name that round-trips
/// through the parser's canonicalization check as *different* text, which is
/// the same silent downgrade wearing a disguise.
const FROM_NAME_FORBIDDEN: [char; 4] = ['"', '<', '>', '\n'];

/// Build the canonical attributed envelope Claude Code's receiver expects —
/// the regex measured live against CC 2.1.232 (module docs / `docs/issues.md`
/// "`drift --drive`").
///
/// **We deliberately omit `from`.** The attribute is optional in the grammar,
/// and it is the address the receiving agent is told to reply to. Kaijutsu is
/// not a Claude Code session and owns no `uds:`-addressable inbox, so any
/// value we invent is an invitation to a reply that cannot arrive — and a
/// fabricated `uds:` path is worse than an absent one, because that string is
/// what the peer tooling treats as a destination. Probed live: with `from`
/// omitted the message still renders attributed by `from-name`, and the
/// receiver simply has no address to try. Callers who want a reply must say
/// so in the body, over a channel that exists (the hook/MCP path).
///
/// Attribute order is load-bearing, the body must be newline-delimited
/// *inside* the tag (`>\n` … `\n</`), and `from-mode="prompting"` is the only
/// value ever observed live — hardcoded rather than modeled as a one-variant
/// enum.
fn build_envelope(from_name: &str, body: &str) -> Result<String, String> {
    if from_name.is_empty() {
        return Err("from-name must not be empty".to_string());
    }
    if let Some(bad) = from_name.chars().find(|c| FROM_NAME_FORBIDDEN.contains(c)) {
        return Err(format!(
            "from-name {from_name:?} contains {bad:?}, which the receiver's \
             from-name charset forbids — refusing: the envelope would deliver \
             but silently lose attribution"
        ));
    }
    Ok(format!(
        "<cross-session-message from-name=\"{from_name}\" from-mode=\"prompting\">\n{body}\n</cross-session-message>"
    ))
}

/// Refuse an empty body and refuse an envelope-injection attempt. Kept
/// separate from the caller so both checks are independently unit-testable
/// without needing a socket or a roster fixture.
fn validate_message_body(body: &str) -> Result<(), String> {
    if body.trim().is_empty() {
        return Err("message body must not be empty".to_string());
    }
    if body.contains(CLOSING_TAG) {
        return Err(format!(
            "message body contains a literal `{CLOSING_TAG}` — refusing: this \
             would make the envelope's parse boundary ambiguous to the \
             receiver rather than erroring loudly (a silent attribution \
             downgrade is the specific failure mode this guards against)"
        ));
    }
    Ok(())
}

/// Fail-closed gate: only [`Reachable::Alive`] and `peerProtocol == 1` are
/// safe to write to. A `Stale` descriptor's pid may now belong to an
/// unrelated process — writing to whatever currently owns that socket path
/// is the worst outcome available, so this refuses rather than guesses.
fn validate_target(entry: &CcSessionEntry) -> Result<(), String> {
    if entry.reachable != Reachable::Alive {
        return Err(format!(
            "target (pid {}) is not reachable: {} — refusing to write to a \
             socket that may now belong to an unrelated process",
            entry.pid,
            reach_label(entry.reachable)
        ));
    }
    if entry.peer_protocol != 1 {
        return Err(format!(
            "target (pid {}) speaks peerProtocol {} — this client only \
             supports protocol 1",
            entry.pid, entry.peer_protocol
        ));
    }
    Ok(())
}

/// Resolve `target` (a session `name` as shown by `kj cc list`, or a numeric
/// pid) against `sessions`. A name match takes priority over pid parsing —
/// a name that happens to look numeric still resolves to the named session
/// when one exists — and an ambiguous name (two sessions sharing it) is an
/// error naming every candidate pid rather than a guess.
fn resolve_target<'a>(
    sessions: &'a [CcSessionEntry],
    target: &str,
) -> Result<&'a CcSessionEntry, String> {
    let name_matches: Vec<&CcSessionEntry> = sessions
        .iter()
        .filter(|s| s.name.as_deref() == Some(target))
        .collect();
    if name_matches.len() > 1 {
        let mut pids: Vec<String> = name_matches.iter().map(|s| s.pid.to_string()).collect();
        pids.sort();
        return Err(format!(
            "name {target:?} is ambiguous — matches pids {}",
            pids.join(", ")
        ));
    }
    if let Some(s) = name_matches.into_iter().next() {
        return Ok(s);
    }
    if let Ok(pid) = target.parse::<u32>()
        && let Some(s) = sessions.iter().find(|s| s.pid == pid)
    {
        return Ok(s);
    }
    Err(format!("no live session matches target {target:?}"))
}

/// The on-disk shape of a `<pid>.<64-hex>.key` bearer-token file.
/// Deliberately **not** `#[derive(Debug)]`: this struct holds a live token
/// and must never be formatted, logged, or echoed in any output. Callers
/// read `.peer_token` once at send time and let the value drop; nothing
/// here is cached beyond the single send it's used for.
#[derive(serde::Deserialize)]
struct CcKeyFile {
    #[serde(rename = "peerToken")]
    peer_token: String,
    /// Present in the real file; not read by this slice (the descriptor's
    /// own `procStart`, already checked by [`validate_target`] via the
    /// roster scan, is what gates reachability here).
    #[serde(rename = "procStart")]
    #[allow(dead_code)]
    proc_start: String,
}

/// Locate the `<pid>.<64-hex>.key` file for `pid` inside `sessions_dir`.
/// Exactly one match is required: zero means the token file is gone (a race
/// with the session exiting between roster scan and send) and more than one
/// is an unexpected collision — neither is a case to guess through.
fn find_key_file(sessions_dir: &Path, pid: u32) -> Result<PathBuf, String> {
    let entries = std::fs::read_dir(sessions_dir)
        .map_err(|e| format!("read {}: {e}", sessions_dir.display()))?;
    let prefix = format!("{pid}.");
    let mut matches = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("read {}: {e}", sessions_dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) && name.ends_with(".key") {
            matches.push(entry.path());
        }
    }
    match matches.len() {
        0 => Err(format!(
            "no key file found for pid {pid} (session may have just exited)"
        )),
        1 => Ok(matches.remove(0)),
        _ => Err(format!(
            "multiple key files found for pid {pid} — refusing to guess which one"
        )),
    }
}

/// Read and parse a key file, returning only the peer token. The file's raw
/// content is never included in an error — only the path and serde_json's
/// own (content-free) parse-error message.
fn read_peer_token(path: &Path) -> Result<String, String> {
    let raw =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let parsed: CcKeyFile =
        serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;
    Ok(parsed.peer_token)
}

/// The real logic behind `kj cc send`, taking an injected `sessions_dir` so
/// tests never touch `~/.claude` or a real session's socket — same shape as
/// [`scan_sessions_dir`]. `from_name` is the envelope's attribution;
/// `target`/`message`/`dry_run` are the parsed CLI args.
fn cc_send_inner(
    sessions_dir: &Path,
    from_name: &str,
    target: &str,
    message: &str,
    dry_run: bool,
) -> KjResult {
    let scan = match scan_sessions_dir(sessions_dir) {
        Ok(s) => s,
        Err(e) => return KjResult::Err(format!("kj cc send: {e}")),
    };

    let entry = match resolve_target(&scan.sessions, target) {
        Ok(e) => e,
        Err(e) => return KjResult::Err(format!("kj cc send: {e}")),
    };
    if let Err(e) = validate_target(entry) {
        return KjResult::Err(format!("kj cc send: {e}"));
    }
    if let Err(e) = validate_message_body(message) {
        return KjResult::Err(format!("kj cc send: {e}"));
    }

    let envelope = match build_envelope(from_name, message) {
        Ok(e) => e,
        Err(e) => return KjResult::Err(format!("kj cc send: {e}")),
    };
    let user_frame = serde_json::json!({
        "type": "user",
        "message": {"role": "user", "content": envelope}
    })
    .to_string();

    if dry_run {
        // Redacted frame shown for real: same structure as what would be
        // written, token replaced with a literal placeholder rather than
        // any value derived from the real one.
        let redacted_auth_frame =
            serde_json::json!({"type": "auth", "token": "<redacted>"}).to_string();
        let text = format!(
            "would send to {} (pid {}), dry-run — nothing was written:\n{redacted_auth_frame}\n{user_frame}",
            entry.name.as_deref().unwrap_or("-"),
            entry.pid,
        );
        let data = serde_json::json!({
            "target": entry.name,
            "pid": entry.pid,
            "bytes_written": 0,
            "dry_run": true,
        });
        return KjResult::ok_with_data(text, data);
    }

    let key_path = match find_key_file(sessions_dir, entry.pid) {
        Ok(p) => p,
        Err(e) => return KjResult::Err(format!("kj cc send: {e}")),
    };
    let token = match read_peer_token(&key_path) {
        Ok(t) => t,
        Err(e) => return KjResult::Err(format!("kj cc send: {e}")),
    };
    // `auth_frame` carries the live token from here on — never format it
    // with anything but `.to_string()` into the socket write below, never
    // into an error, log, or the dry-run branch above (which builds its own
    // redacted frame instead of touching this one).
    let auth_frame = serde_json::json!({"type": "auth", "token": token}).to_string();
    drop(token);

    use std::io::Write;
    let mut stream = match std::os::unix::net::UnixStream::connect(&entry.messaging_socket_path) {
        Ok(s) => s,
        Err(e) => {
            return KjResult::Err(format!(
                "kj cc send: connect {}: {e}",
                entry.messaging_socket_path
            ));
        }
    };
    let mut bytes_written = 0usize;
    for frame in [&auth_frame, &user_frame] {
        let line = format!("{frame}\n");
        if let Err(e) = stream.write_all(line.as_bytes()) {
            return KjResult::Err(format!(
                "kj cc send: write to {}: {e}",
                entry.messaging_socket_path
            ));
        }
        bytes_written += line.len();
    }
    // No ack on this protocol — do not read, do not wait. A connect/write
    // failure above is the only failure mode; a read timeout would not be
    // one, which is exactly why nothing here attempts a read at all.
    let _ = stream.shutdown(std::net::Shutdown::Both);

    let data = serde_json::json!({
        "target": entry.name,
        "pid": entry.pid,
        "bytes_written": bytes_written,
        "dry_run": false,
    });
    KjResult::ok_with_data(
        format!(
            "sent to {} (pid {}), {bytes_written} bytes",
            entry.name.as_deref().unwrap_or("-"),
            entry.pid,
        ),
        data,
    )
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

        let (reachable, _) = classify_reachability(own_pid, &wrong);
        assert_eq!(
            reachable,
            Reachable::Stale,
            "a live pid whose recorded procStart disagrees with reality must read as Stale"
        );
    }

    /// Test-only convenience keeping the terser `Option` shape. Production code
    /// uses [`probe_proc_starttime`] so it can distinguish "no such process"
    /// from "could not determine"; tests that only want the number don't care.
    fn read_proc_starttime(pid: u32) -> Option<u64> {
        match probe_proc_starttime(pid) {
            ProcProbe::Started(s) => Some(s),
            _ => None,
        }
    }

    #[test]
    fn corrupt_procstart_is_unknown_not_stale() {
        // A live pid, but the descriptor's procStart is not a number at all.
        // That is a corrupt descriptor; calling it Stale would assert a
        // specific event (pid reuse) that we did not observe.
        let (reachable, why) = classify_reachability(std::process::id(), "not-a-number");
        assert_eq!(
            reachable,
            Reachable::Unknown,
            "a non-numeric procStart must not be reported as pid reuse"
        );
        // The reason must travel with the verdict, so the caller can report it
        // rather than leaving a bare "unknown" in a table.
        let why = why.expect("Unknown must carry a reason");
        assert!(
            why.contains("not a number"),
            "reason should name the actual problem: {why}"
        );
    }

    #[test]
    fn an_unrecognized_stat_format_does_not_read_as_gone() {
        // The failure this guards: if /proc's format shifted (or our parser
        // regressed), folding that into Gone would report every session dead
        // and the roster would look plausibly empty rather than broken.
        assert!(
            parse_stat_starttime("wholly unparseable").is_none(),
            "precondition: this line must not parse"
        );
        match probe_proc_starttime(std::process::id()) {
            ProcProbe::Started(_) => {}
            ProcProbe::NoSuchProcess => panic!("our own pid must exist"),
            ProcProbe::Undetermined(m) => panic!("own stat should parse, got: {m}"),
        }
        // And the label set covers the variant, so it can never render blank.
        assert_eq!(reach_label(Reachable::Unknown), "unknown");
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
            classify_reachability(never_a_pid, "1").0,
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

    // ── send path: envelope, target resolution, key handling, delivery ────
    //
    // Never writes to a real live session socket. The one end-to-end test
    // binds its own throwaway `UnixListener` in a temp dir.

    mod send_path {
        use super::*;
        use regex::Regex;
        use std::io::Read;
        use std::os::unix::net::UnixListener;

        // The parser regex from the task brief, translated from its
        // `[CHARSET]`/`[UUID]`/`[LIST]`/`[MODE]` placeholders into concrete
        // (deliberately permissive) character classes — this crate's own
        // `regex` dependency (already used elsewhere in kaijutsu-kernel, not
        // added for this test) round-tripped against [`build_envelope`]'s
        // actual output. `from-session`/`hop-chain` are never emitted by
        // this slice, so their groups only need to stay optional, not be
        // exercised as non-None.
        fn envelope_regex() -> Regex {
            Regex::new(concat!(
                r#"^<cross-session-message"#,
                r#"(?: from="([A-Za-z0-9:_./-]+)")?"#,
                r#"(?: from-session="([0-9a-fA-F-]{36})")?"#,
                r#"(?: hop-chain="([^"<>\n\r]+)")?"#,
                r#"(?: from-name="([^"<>\n\r]+)")?"#,
                r#"(?: from-mode="([A-Za-z0-9_-]+)")?"#,
                r#">\n([\s\S]*)\n</cross-session-message>$"#,
            ))
            .expect("hand-written regex must compile")
        }

        // ── round-trip: well-formed envelope parses and recovers from/
        //    from-name/from-mode/body exactly ─────────────────────────────

        #[test]
        fn envelope_round_trips_through_the_regex() {
            let envelope = build_envelope("kaijutsu", "hello there").expect("valid from-name");
            let re = envelope_regex();
            let caps = re
                .captures(&envelope)
                .expect("a well-formed envelope must match the parser regex");
            // `from` is deliberately omitted — see build_envelope's docs. The
            // grammar makes it optional, and inventing an unreachable reply
            // address is worse than offering none.
            assert!(caps.get(1).is_none(), "from must not be emitted");
            assert!(caps.get(2).is_none(), "from-session was never emitted");
            assert!(caps.get(3).is_none(), "hop-chain was never emitted");
            assert_eq!(&caps[4], "kaijutsu");
            assert_eq!(&caps[5], "prompting");
            assert_eq!(&caps[6], "hello there");
        }

        #[test]
        fn body_with_newlines_quotes_angle_brackets_and_non_ascii_round_trips() {
            let body = "line one\nline \"two\" <tag> — 日本語 😀\nline three";
            let envelope = build_envelope("kaijutsu", body).expect("valid from-name");
            let re = envelope_regex();
            let caps = re
                .captures(&envelope)
                .expect("a body with newlines/quotes/angle-brackets/non-ASCII must still match");
            assert_eq!(&caps[6], body, "captured body must equal the original exactly");
        }

        #[test]
        fn a_from_name_outside_the_receiver_charset_is_rejected_not_escaped() {
            // Today FROM_NAME is a constant, but this becomes a context label
            // once `kj cc` melts into `kj drift` — at which point it is
            // user-controlled. A name carrying a quote would deliver and
            // silently lose attribution, so it must error here instead.
            for bad in ["say \"hi\"", "a<b", "a>b", "two\nlines"] {
                let err = build_envelope(bad, "body").expect_err(&format!(
                    "from-name {bad:?} must be rejected, not escaped or passed through"
                ));
                assert!(
                    err.contains("from-name"),
                    "error should name the offending attribute: {err}"
                );
            }
            // And a name that only *looks* exotic is still fine.
            let ok = build_envelope("kaijutsu-ctx-7f3a — 会術", "body")
                .expect("non-ASCII without forbidden chars is allowed");
            assert!(envelope_regex().is_match(&ok));
        }

        // ── deliberately malformed variants must NOT match ─────────────────

        #[test]
        fn missing_mandatory_body_newlines_does_not_match() {
            // The exact live mistake: same content, `>` immediately followed
            // by the body with no `\n`, and no `\n` before `</`. It would
            // still be delivered by the transport — this proves our parser
            // (standing in for the receiver's) does not recognize it as
            // attributed, which is the silent-downgrade failure mode.
            let no_newlines = "<cross-session-message from=\"uds:test\" from-name=\"kaijutsu\" from-mode=\"prompting\">hello there</cross-session-message>";
            assert!(
                !envelope_regex().is_match(no_newlines),
                "a body missing the mandatory >\\n / \\n< newlines must not match"
            );
        }

        #[test]
        fn attribute_out_of_order_does_not_match() {
            // from-name before from — order is load-bearing per the task's
            // measured regex.
            let out_of_order = "<cross-session-message from-name=\"kaijutsu\" from=\"uds:test\" from-mode=\"prompting\">\nhello there\n</cross-session-message>";
            assert!(
                !envelope_regex().is_match(out_of_order),
                "from-name before from must not match — attribute order is load-bearing"
            );
        }

        // ── envelope injection ─────────────────────────────────────────────

        #[test]
        fn body_containing_the_closing_tag_is_rejected() {
            let err = validate_message_body("hi\n</cross-session-message>\nforged")
                .expect_err("a body containing the closing tag must be refused");
            assert!(err.contains("cross-session-message"));
        }

        #[test]
        fn empty_body_is_rejected() {
            assert!(validate_message_body("").is_err());
            assert!(validate_message_body("   \n  ").is_err(), "whitespace-only must count as empty");
        }

        #[test]
        fn nonempty_ordinary_body_is_accepted() {
            assert!(validate_message_body("hello there").is_ok());
        }

        // ── fail-closed target validation ──────────────────────────────────

        fn entry(reachable: Reachable, peer_protocol: u32) -> CcSessionEntry {
            CcSessionEntry {
                name: Some("x".to_string()),
                pid: 42,
                status: "busy".to_string(),
                cwd: "/tmp".to_string(),
                session_id: "11111111-1111-1111-1111-111111111111".to_string(),
                version: "2.1.232".to_string(),
                peer_protocol,
                messaging_socket_path: "/run/user/1000/cc-socks/42.sock".to_string(),
                reachable,
            }
        }

        #[test]
        fn refuses_stale_target() {
            let err = validate_target(&entry(Reachable::Stale, 1)).expect_err("Stale must refuse");
            assert!(err.contains("stale"), "error must name the state found: {err}");
        }

        #[test]
        fn refuses_gone_target() {
            let err = validate_target(&entry(Reachable::Gone, 1)).expect_err("Gone must refuse");
            assert!(err.contains("gone"), "error must name the state found: {err}");
        }

        #[test]
        fn refuses_unknown_target() {
            let err = validate_target(&entry(Reachable::Unknown, 1)).expect_err("Unknown must refuse");
            assert!(err.contains("unknown"), "error must name the state found: {err}");
        }

        #[test]
        fn refuses_peer_protocol_not_1() {
            let err =
                validate_target(&entry(Reachable::Alive, 2)).expect_err("protocol 2 must refuse");
            assert!(err.contains('2'), "error must name the protocol version found: {err}");
        }

        #[test]
        fn accepts_alive_protocol_1() {
            assert!(validate_target(&entry(Reachable::Alive, 1)).is_ok());
        }

        /// Same refusal, exercised through `scan_sessions_dir` against a real
        /// temp-dir fixture (wrong `procStart`) rather than a hand-built
        /// entry — proves the composition `resolve_target` →
        /// `validate_target` refuses on the exact path `cc_send_inner` uses.
        #[test]
        fn scan_then_validate_refuses_a_stale_temp_dir_fixture() {
            let dir = tempfile::tempdir().expect("tmpdir");
            let own_pid = std::process::id();
            let own_start = read_proc_starttime(own_pid).expect("own /proc/self/stat must parse");
            let wrong = (own_start + 1).to_string();
            write(
                dir.path(),
                &format!("{own_pid}.json"),
                &serde_json::json!({
                    "pid": own_pid,
                    "sessionId": "55555555-5555-5555-5555-555555555555",
                    "cwd": "/tmp",
                    "startedAt": 1,
                    "procStart": wrong,
                    "version": "2.1.232",
                    "peerProtocol": 1,
                    "messagingSocketPath": "/run/user/1000/cc-socks/stale.sock",
                    "name": "stale-one",
                    "status": "busy"
                })
                .to_string(),
            );

            let scan = scan_sessions_dir(dir.path()).expect("scan must succeed");
            let found = resolve_target(&scan.sessions, "stale-one").expect("name must resolve");
            let err = validate_target(found).expect_err("a Stale fixture must refuse");
            assert!(err.contains("stale"));
        }

        /// Same, but for a descriptor whose pid does not exist at all (Gone).
        #[test]
        fn scan_then_validate_refuses_a_gone_temp_dir_fixture() {
            let dir = tempfile::tempdir().expect("tmpdir");
            let never_a_pid: u32 = 4_200_000_001;
            write(
                dir.path(),
                &format!("{never_a_pid}.json"),
                &serde_json::json!({
                    "pid": never_a_pid,
                    "sessionId": "66666666-6666-6666-6666-666666666666",
                    "cwd": "/tmp",
                    "startedAt": 1,
                    "procStart": "1",
                    "version": "2.1.232",
                    "peerProtocol": 1,
                    "messagingSocketPath": "/run/user/1000/cc-socks/gone.sock",
                    "name": "gone-one",
                    "status": "busy"
                })
                .to_string(),
            );

            let scan = scan_sessions_dir(dir.path()).expect("scan must succeed");
            let found = resolve_target(&scan.sessions, "gone-one").expect("name must resolve");
            let err = validate_target(found).expect_err("a Gone fixture must refuse");
            assert!(err.contains("gone"));
        }

        // ── target resolution: ambiguous name, unknown target, pid fallback ─

        #[test]
        fn ambiguous_name_errors_and_names_both_candidates() {
            let sessions = vec![
                CcSessionEntry {
                    pid: 100,
                    ..entry(Reachable::Alive, 1)
                },
                CcSessionEntry {
                    pid: 200,
                    ..entry(Reachable::Alive, 1)
                },
            ];
            let err = resolve_target(&sessions, "x").expect_err("duplicate name must be ambiguous");
            assert!(err.contains("100") && err.contains("200"), "error must list both candidates: {err}");
        }

        #[test]
        fn unknown_target_errors() {
            let sessions = vec![entry(Reachable::Alive, 1)];
            assert!(resolve_target(&sessions, "nope").is_err());
            assert!(resolve_target(&sessions, "9999").is_err());
        }

        #[test]
        fn numeric_pid_resolves_when_no_name_matches() {
            let sessions = vec![entry(Reachable::Alive, 1)]; // pid 42, name "x"
            let found = resolve_target(&sessions, "42").expect("pid fallback must resolve");
            assert_eq!(found.pid, 42);
        }

        #[test]
        fn a_name_match_takes_priority_over_pid_parsing() {
            let mut named_like_a_pid = entry(Reachable::Alive, 1);
            named_like_a_pid.name = Some("42".to_string());
            let mut other = entry(Reachable::Alive, 1);
            other.pid = 42;
            other.name = None;
            // Two entries: one literally has pid 42, the other is *named*
            // "42" but has a different pid. Resolving "42" must prefer the
            // name match.
            let mut a = named_like_a_pid;
            a.pid = 7;
            let sessions = vec![a, other];
            let found = resolve_target(&sessions, "42").expect("must resolve");
            assert_eq!(found.pid, 7, "a name match must win over pid parsing");
        }

        // ── token discipline: never in dry-run output, never in an error ────

        const FIXTURE_TOKEN: &str = "cafebabecafebabecafebabecafebabe";

        fn write_session_and_key(dir: &std::path::Path, pid: u32, token: &str) {
            let start = read_proc_starttime(pid).expect("own /proc/self/stat must parse");
            write(
                dir,
                &format!("{pid}.json"),
                &serde_json::json!({
                    "pid": pid,
                    "sessionId": "77777777-7777-7777-7777-777777777777",
                    "cwd": "/tmp",
                    "startedAt": 1,
                    "procStart": start.to_string(),
                    "version": "2.1.232",
                    "peerProtocol": 1,
                    "messagingSocketPath": dir.join(format!("{pid}.sock")).to_string_lossy(),
                    "name": "tok-target",
                    "status": "busy"
                })
                .to_string(),
            );
            write(
                dir,
                &format!(
                    "{pid}.{:0<64}.key",
                    "deadbeef" // pad to a plausible 64-hex-char filename; content is what's tested
                ),
                &serde_json::json!({
                    "peerToken": token,
                    "procStart": start.to_string(),
                })
                .to_string(),
            );
        }

        #[test]
        fn dry_run_never_includes_the_real_token() {
            let dir = tempfile::tempdir().expect("tmpdir");
            let own_pid = std::process::id();
            write_session_and_key(dir.path(), own_pid, FIXTURE_TOKEN);

            let result = cc_send_inner(
                dir.path(),
                "kaijutsu",
                "tok-target",
                "hi there",
                true,
            );
            assert!(result.is_ok(), "dry-run against an Alive fixture must succeed: {result:?}");
            let msg = result.message();
            assert!(
                !msg.contains(FIXTURE_TOKEN),
                "dry-run message must never contain the real token"
            );
            assert!(
                msg.contains("<redacted>"),
                "dry-run output should show the frame shape with a redaction placeholder"
            );
            if let KjResult::Ok { data: Some(d), .. } = &result {
                assert!(
                    !d.to_string().contains(FIXTURE_TOKEN),
                    "structured .data must never contain the real token"
                );
            } else {
                panic!("expected Ok with data, got {result:?}");
            }
        }

        /// Stronger than "the token never appears in the output": `--dry-run`
        /// never even opens the `.key` file, the same least-privilege stance
        /// `scan_sessions_dir` takes for `.key` files in `kj cc list`. A
        /// value that was never read cannot leak.
        #[test]
        fn dry_run_never_reads_the_key_file_at_all() {
            let dir = tempfile::tempdir().expect("tmpdir");
            let own_pid = std::process::id();
            let own_start = read_proc_starttime(own_pid).expect("own /proc/self/stat must parse");
            // Session descriptor only — deliberately NO matching .key file.
            write(
                dir.path(),
                &format!("{own_pid}.json"),
                &serde_json::json!({
                    "pid": own_pid,
                    "sessionId": "88888888-8888-8888-8888-888888888888",
                    "cwd": "/tmp",
                    "startedAt": 1,
                    "procStart": own_start.to_string(),
                    "version": "2.1.232",
                    "peerProtocol": 1,
                    "messagingSocketPath": dir.path().join(format!("{own_pid}.sock")).to_string_lossy(),
                    "name": "no-key-here",
                    "status": "busy"
                })
                .to_string(),
            );

            let result = cc_send_inner(
                dir.path(),
                "kaijutsu",
                "no-key-here",
                "hi",
                true,
            );
            assert!(
                result.is_ok(),
                "dry-run must succeed with no key file present at all — it never reads one: {result:?}"
            );
        }

        #[test]
        fn errors_never_include_the_real_token() {
            let dir = tempfile::tempdir().expect("tmpdir");
            let own_pid = std::process::id();
            write_session_and_key(dir.path(), own_pid, FIXTURE_TOKEN);

            // An empty message is refused before the key is ever read, but
            // assert the invariant anyway: the fixture token must not leak
            // into any error string this function can produce.
            let result = cc_send_inner(
                dir.path(),
                "kaijutsu",
                "tok-target",
                "",
                false,
            );
            assert!(!result.is_ok());
            assert!(!result.message().contains(FIXTURE_TOKEN));

            let result = cc_send_inner(
                dir.path(),
                "kaijutsu",
                "no-such-target",
                "hi",
                false,
            );
            assert!(!result.is_ok());
            assert!(!result.message().contains(FIXTURE_TOKEN));
        }

        // ── clap wiring: pure argv parsing, no filesystem touch ────────────

        #[test]
        fn send_argv_parses_leading_dry_run_and_trailing_message() {
            let parsed = CcArgs::try_parse_from(["send", "--dry-run", "bob", "hello", "there"])
                .expect("must parse");
            match parsed.command {
                CcCommand::Send {
                    target,
                    message,
                    dry_run,
                } => {
                    assert_eq!(target, "bob");
                    assert_eq!(message, vec!["hello".to_string(), "there".to_string()]);
                    assert!(dry_run);
                }
                other => panic!("expected Send, got {other:?}"),
            }
        }

        /// Documents the `trailing_var_arg` trap the `Send` variant's doc
        /// comment warns about: placed *after* the target, `--dry-run` is
        /// swallowed into the message rather than parsed as a flag. This
        /// test exists so a future clap upgrade that changes that behavior
        /// is caught rather than silently making the doc comment wrong.
        #[test]
        fn send_argv_dry_run_after_target_is_swallowed_into_the_message() {
            let parsed = CcArgs::try_parse_from(["send", "bob", "hello", "--dry-run"])
                .expect("must parse");
            match parsed.command {
                CcCommand::Send {
                    target,
                    message,
                    dry_run,
                } => {
                    assert_eq!(target, "bob");
                    assert_eq!(
                        message,
                        vec!["hello".to_string(), "--dry-run".to_string()]
                    );
                    assert!(
                        !dry_run,
                        "a flag placed after the target must NOT be parsed as dry_run"
                    );
                }
                other => panic!("expected Send, got {other:?}"),
            }
        }

        // ── end-to-end: real delivery to OUR OWN throwaway socket, never a
        //    real live session ──────────────────────────────────────────────

        #[test]
        fn real_send_writes_two_frames_in_order_to_a_throwaway_socket() {
            let dir = tempfile::tempdir().expect("tmpdir");
            let own_pid = std::process::id();
            write_session_and_key(dir.path(), own_pid, FIXTURE_TOKEN);
            let sock_path = dir.path().join(format!("{own_pid}.sock"));

            let listener = UnixListener::bind(&sock_path).expect("bind throwaway listener");
            let listener_thread = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept one connection");
                let mut buf = Vec::new();
                stream
                    .read_to_end(&mut buf)
                    .expect("read until peer closes");
                buf
            });

            let result = cc_send_inner(
                dir.path(),
                "kaijutsu",
                "tok-target",
                "hello there",
                false,
            );
            assert!(result.is_ok(), "real send to our own listener must succeed: {result:?}");

            let received = listener_thread.join().expect("listener thread must not panic");
            let text = String::from_utf8(received).expect("frames must be valid UTF-8");
            let mut lines = text.lines();

            let auth_line = lines.next().expect("auth frame must be present");
            let user_line = lines.next().expect("user frame must be present");
            assert!(lines.next().is_none(), "exactly two frames, nothing else");

            let auth: serde_json::Value = serde_json::from_str(auth_line).expect("auth frame is JSON");
            assert_eq!(auth["type"], "auth");
            assert_eq!(auth["token"], FIXTURE_TOKEN);

            let user: serde_json::Value = serde_json::from_str(user_line).expect("user frame is JSON");
            assert_eq!(user["type"], "user");
            assert_eq!(user["message"]["role"], "user");
            let content = user["message"]["content"]
                .as_str()
                .expect("content must be a string");
            assert!(content.starts_with(
                "<cross-session-message from-name=\"kaijutsu\" from-mode=\"prompting\">\n"
            ));
            assert!(
                !content.contains(" from=\""),
                "no `from` attribute may be emitted: it is the reply address, \
                 and kaijutsu has no inbox to receive one — {content}"
            );
            assert!(content.ends_with("\n</cross-session-message>"));
            assert!(content.contains("hello there"));

            // And it must still round-trip through the parser regex end to end.
            assert!(envelope_regex().is_match(content));
        }
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
    /// Manual smoke check against this machine's real registry. Ignored by
    /// default: it asserts nothing about content (the live roster changes
    /// constantly) and exists to eyeball real output —
    /// `cargo test -p kaijutsu-kernel cc::tests::real_registry -- --ignored --nocapture`
    #[test]
    #[ignore = "reads the real ~/.claude/sessions; run manually"]
    fn real_registry_smoke() {
        let home = std::env::var_os("HOME").expect("HOME");
        let dir = PathBuf::from(home).join(".claude").join("sessions");
        let scan = scan_sessions_dir(&dir).expect("real scan should not error");
        println!("sessions: {}  problems: {}", scan.sessions.len(), scan.errors.len());
        for s in &scan.sessions {
            println!(
                "  {:<16} pid={:<8} status={:<8} reach={:<8} proto={} {}",
                s.name.as_deref().unwrap_or("-"),
                s.pid,
                s.status,
                reach_label(s.reachable),
                s.peer_protocol,
                s.cwd
            );
        }
        for e in &scan.errors {
            println!("  ! {}: {}", e.file, e.message);
        }
    }

    /// End-to-end against this machine's real registry and a real live Claude
    /// Code session — the only test that validates our serializer against the
    /// *actual* receiver rather than against our own copy of its regex.
    /// Ignored: it needs a live session and it delivers a real message.
    /// `cargo test -p kaijutsu-kernel cc::tests::real_send -- --ignored --nocapture`
    #[test]
    #[ignore = "sends a real message to a live Claude Code session; run manually"]
    fn real_send_to_a_live_session() {
        let home = std::env::var_os("HOME").expect("HOME");
        let dir = PathBuf::from(home).join(".claude").join("sessions");
        let target = std::env::var("KJ_CC_TARGET").unwrap_or_else(|_| "kaijutsu-chan".to_string());
        let result = cc_send_inner(
            &dir,
            FROM_NAME,
            &target,
            "REAL SEND via kj cc send: if this arrives attributed to kaijutsu, \
             the canonical serializer is validated against the real receiver.",
            false,
        );
        println!("result: {:?}", result.message());
        assert!(result.is_ok(), "real send failed: {}", result.message());
    }

}
