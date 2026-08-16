//! `kj cc` — roster of live Claude Code sessions on this machine, and the
//! send path into one.
//!
//! **The protocol lives in `crates/claude-code-peer`** (descriptor registry,
//! PID-reuse guard, attribution envelope, frames, delivery) with its design
//! record and provenance in `docs/cc-peer.md`. This module is the thin `kj`
//! surface on top: argv parsing, rendering, and the dry-run/real-send split.
//! One property it owns outright, because it is about *this verb* and not
//! the protocol: **`--dry-run` never opens a key file** — it builds the
//! frame shape through the crate's [`claude_code_peer::prepare`], which
//! touches no filesystem at all.
//!
//! `kj cc` is deliberately temporary: hooks give Claude Code sessions
//! contexts, then drift targets them like anything else and this verb
//! retires (Amy, 2026-08-15).
//!
//! ## The send path is ledger-gated
//!
//! Amy, 2026-08-16: *"yeah kj cc send should go through the ledger."*
//! Injecting a turn into another agent's session is exactly the action a
//! human should authorize, so every real send first leaves a durable ask
//! row ([`crate::kj::gate`]) and waits for an answer (`kj approve`) until
//! `gate_wait_timeout` — fail-closed on elapse. `--dry-run` writes nothing
//! and is exempt.

use std::path::Path;

use clap::{Parser, Subcommand};
use kaijutsu_types::ContentType;

use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};

/// The `from-name` attribute value — this kernel's attribution on the wire.
const FROM_NAME: &str = "kaijutsu";

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
    pub(crate) async fn dispatch_cc(&self, argv: &[String], caller: &KjCaller) -> KjResult {
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
        // `kj cas ls`/`kj mcp list`/`kj midi list`. `Send` writes into
        // another agent's session, so the real path goes through the
        // approval ledger first (module docs).
        match parsed.command {
            CcCommand::List => cc_list(),
            CcCommand::Send {
                target,
                message,
                dry_run,
            } => {
                let sessions_dir = match claude_code_peer::default_sessions_dir() {
                    Some(d) => d,
                    None => return KjResult::Err("kj cc send: $HOME is not set".to_string()),
                };
                let message = message.join(" ");
                if dry_run {
                    return cc_send_inner(&sessions_dir, FROM_NAME, &target, &message, true);
                }
                let wait = self.kernel.timeouts().gate_wait_timeout;
                let spec = gate_spec_for_send(&target, &message, &sessions_dir);
                let outcome = crate::kj::gate::run_gate(&self.kernel_db, caller, spec, wait).await;
                if !outcome.allowed {
                    return KjResult::Err(format!(
                        "kj cc send: approval gate refused ({}): {}",
                        outcome.status, outcome.reason
                    ));
                }
                cc_send_inner(&sessions_dir, FROM_NAME, &target, &message, false)
            }
        }
    }
}

/// Build the gate's ask for one send. The statement marks the message body
/// a FREE variable, so the ledger's guarantee 3 keeps allow-always rules
/// structurally impossible for this verb — every send stays human-approved
/// until that policy changes deliberately. `authorized_label` is the target
/// exactly as typed (gate research-pass finding #3: the raw reference,
/// never a resolved label).
fn gate_spec_for_send(
    target: &str,
    message: &str,
    sessions_dir: &Path,
) -> crate::kj::gate::GateSpec {
    // Enrich the description with the resolution when it's available, so
    // whoever answers sees *which* session they are authorizing.
    let resolution = claude_code_peer::scan_sessions_dir(sessions_dir)
        .ok()
        .and_then(|scan| {
            claude_code_peer::resolve_target(&scan.sessions, target)
                .ok()
                .map(|s| {
                    format!(
                        " (pid {}, name {}, cwd {})",
                        s.descriptor.pid,
                        s.descriptor.name.as_deref().unwrap_or("-"),
                        s.descriptor.cwd
                    )
                })
        })
        .unwrap_or_default();
    let preview: String = message.chars().take(200).collect();
    crate::kj::gate::GateSpec {
        tool: "cc.send",
        description: format!(
            "send a cross-session message to Claude Code session {target:?}{resolution}: {preview}"
        ),
        rendered: "kj cc send ${TARGET} ${MESSAGE}".into(),
        authorized_label: target.to_string(),
        vars: vec![
            ("TARGET".into(), approval_ledger::types::VarBinding::Bound),
            ("MESSAGE".into(), approval_ledger::types::VarBinding::Free),
        ],
    }
}

fn session_entry_json(s: &claude_code_peer::Session) -> serde_json::Value {
    let d = &s.descriptor;
    serde_json::json!({
        "name": d.name,
        "pid": d.pid,
        "status": d.status,
        "cwd": d.cwd,
        "session_id": d.session_id,
        "version": d.version,
        "peer_protocol": d.peer_protocol,
        "messaging_socket_path": d.messaging_socket_path,
        "reachable": s.reachable.label(),
    })
}

fn cc_list() -> KjResult {
    let sessions_dir = match claude_code_peer::default_sessions_dir() {
        Some(d) => d,
        None => return KjResult::Err("kj cc list: $HOME is not set".to_string()),
    };
    let scan = match claude_code_peer::scan_sessions_dir(&sessions_dir) {
        Ok(s) => s,
        Err(e) => return KjResult::Err(format!("kj cc list: {e}")),
    };

    // Deliberately an ARRAY OF OBJECTS, not the array-of-id-strings
    // convention most `kj … list` verbs use (see KjResult::Ok's `data`
    // doc comment). Those id strings are kaijutsu-native handles a
    // caller can round-trip into another `kj` verb; a Claude Code
    // session descriptor is a foreign record with no kaijutsu identity
    // to hand back — the useful iteration unit here is the whole row.
    let data = serde_json::Value::Array(scan.sessions.iter().map(session_entry_json).collect());

    if scan.sessions.is_empty() && scan.errors.is_empty() {
        return KjResult::ok_with_data("(no live Claude Code sessions)".to_string(), data);
    }

    let mut lines = Vec::new();
    if !scan.sessions.is_empty() {
        let name_w = scan
            .sessions
            .iter()
            .map(|s| s.descriptor.name.as_deref().unwrap_or("-").len())
            .chain(std::iter::once("NAME".len()))
            .max()
            .unwrap_or(4);
        let status_w = scan
            .sessions
            .iter()
            .map(|s| s.descriptor.status.len())
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
                s.descriptor.name.as_deref().unwrap_or("-"),
                s.descriptor.pid,
                s.descriptor.status,
                s.reachable.label(),
                s.descriptor.cwd,
                name_w = name_w,
                status_w = status_w
            ));
        }
    }
    if !scan.errors.is_empty() {
        // Not only parse failures: an undetermined liveness lands here too,
        // so the heading must not claim a specific cause.
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

/// The real logic behind `kj cc send`, taking an injected `sessions_dir` so
/// tests never touch `~/.claude` or a real session's socket. All protocol
/// work (liveness gate, envelope, frames, key handling, delivery) is the
/// crate's; this function only sequences it into `KjResult` shapes.
fn cc_send_inner(
    sessions_dir: &Path,
    from_name: &str,
    target: &str,
    message: &str,
    dry_run: bool,
) -> KjResult {
    let scan = match claude_code_peer::scan_sessions_dir(sessions_dir) {
        Ok(s) => s,
        Err(e) => return KjResult::Err(format!("kj cc send: {e}")),
    };
    let session = match claude_code_peer::resolve_target(&scan.sessions, target) {
        Ok(s) => s,
        Err(e) => return KjResult::Err(format!("kj cc send: {e}")),
    };
    if let Err(e) = session.validate_sendable() {
        return KjResult::Err(format!("kj cc send: {e}"));
    }

    if dry_run {
        // prepare() validates the body and builds the exact frames without
        // any filesystem touch — the key file is never opened on a dry run.
        let prepared = match claude_code_peer::prepare(from_name, message, None) {
            Ok(p) => p,
            Err(e) => return KjResult::Err(format!("kj cc send: {e}")),
        };
        // Redacted frame shown for real: same structure as what would be
        // written, token replaced with a literal placeholder rather than
        // any value derived from the real one.
        let redacted_auth_frame = claude_code_peer::auth_frame_line("<redacted>");
        let text = format!(
            "would send to {} (pid {}), dry-run — nothing was written:\n{redacted_auth_frame}\n{}",
            session.descriptor.name.as_deref().unwrap_or("-"),
            session.descriptor.pid,
            prepared.user_frame,
        );
        let data = serde_json::json!({
            "target": session.descriptor.name,
            "pid": session.descriptor.pid,
            "bytes_written": 0,
            "dry_run": true,
        });
        return KjResult::ok_with_data(text, data);
    }

    match claude_code_peer::send_message(sessions_dir, session, from_name, message, None) {
        Ok(outcome) => {
            let data = serde_json::json!({
                "target": session.descriptor.name,
                "pid": session.descriptor.pid,
                "bytes_written": outcome.bytes_written,
                "msg_id": outcome.msg_id.as_str(),
                "dry_run": false,
            });
            KjResult::ok_with_data(
                format!(
                    "sent to {} (pid {}), {} bytes",
                    session.descriptor.name.as_deref().unwrap_or("-"),
                    session.descriptor.pid,
                    outcome.bytes_written,
                ),
                data,
            )
        }
        Err(e) => KjResult::Err(format!("kj cc send: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_code_peer::probe_proc_starttime;
    use std::io::Read;
    use std::os::unix::net::UnixListener;

    fn write(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).expect("write fixture file");
    }

    fn own_starttime() -> u64 {
        match probe_proc_starttime(std::process::id()) {
            claude_code_peer::ProcProbe::Started(s) => s,
            other => panic!("own /proc/self/stat must parse, got {other:?}"),
        }
    }

    const FIXTURE_TOKEN: &str = "cafebabecafebabecafebabecafebabe";

    fn write_session_and_key(dir: &Path, pid: u32, token: &str) {
        let start = own_starttime();
        write(
            dir,
            &format!("{pid}.json"),
            &serde_json::json!({
                "pid": pid,
                "sessionId": "77777777-7777-7777-7777-777777777777",
                "cwd": "/tmp",
                "startedAt": 1,
                "procStart": start.to_string(),
                "version": claude_code_peer::PROBED_AGAINST_CC,
                "peerProtocol": 1,
                "messagingSocketPath": dir.join(format!("{pid}.sock")).to_string_lossy(),
                "name": "tok-target",
                "status": "busy"
            })
            .to_string(),
        );
        write(
            dir,
            &format!("{pid}.{}.key", "deadbeef".repeat(8)),
            &serde_json::json!({
                "peerToken": token,
                "procStart": start.to_string(),
            })
            .to_string(),
        );
    }

    // ── token discipline: never in dry-run output, never in an error ────

    #[test]
    fn dry_run_never_includes_the_real_token() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let own_pid = std::process::id();
        write_session_and_key(dir.path(), own_pid, FIXTURE_TOKEN);

        let result = cc_send_inner(dir.path(), "kaijutsu", "tok-target", "hi there", true);
        assert!(
            result.is_ok(),
            "dry-run against an Alive fixture must succeed: {result:?}"
        );
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
    /// the scan takes for `.key` files in `kj cc list`. A value that was
    /// never read cannot leak.
    #[test]
    fn dry_run_never_reads_the_key_file_at_all() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let own_pid = std::process::id();
        let start = own_starttime();
        // Session descriptor only — deliberately NO matching .key file.
        write(
            dir.path(),
            &format!("{own_pid}.json"),
            &serde_json::json!({
                "pid": own_pid,
                "sessionId": "88888888-8888-8888-8888-888888888888",
                "cwd": "/tmp",
                "startedAt": 1,
                "procStart": start.to_string(),
                "version": claude_code_peer::PROBED_AGAINST_CC,
                "peerProtocol": 1,
                "messagingSocketPath": dir.path().join(format!("{own_pid}.sock")).to_string_lossy(),
                "name": "no-key-here",
                "status": "busy"
            })
            .to_string(),
        );

        let result = cc_send_inner(dir.path(), "kaijutsu", "no-key-here", "hi", true);
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
        let result = cc_send_inner(dir.path(), "kaijutsu", "tok-target", "", false);
        assert!(!result.is_ok());
        assert!(!result.message().contains(FIXTURE_TOKEN));

        let result = cc_send_inner(dir.path(), "kaijutsu", "no-such-target", "hi", false);
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
        let parsed =
            CcArgs::try_parse_from(["send", "bob", "hello", "--dry-run"]).expect("must parse");
        match parsed.command {
            CcCommand::Send {
                target,
                message,
                dry_run,
            } => {
                assert_eq!(target, "bob");
                assert_eq!(message, vec!["hello".to_string(), "--dry-run".to_string()]);
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

        let result = cc_send_inner(dir.path(), "kaijutsu", "tok-target", "hello there", false);
        assert!(
            result.is_ok(),
            "real send to our own listener must succeed: {result:?}"
        );

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

        // And it must still round-trip through the receiver-mirror regex.
        assert!(claude_code_peer::envelope::receiver_regex().is_match(content));
    }

    // ── clap wiring via the full dispatcher — deliberately no filesystem
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
                !result.message().contains("no active context joined"),
                "kj cc must be exempt from the active-context gate, got: {}",
                result.message()
            );
        }
    }
}
