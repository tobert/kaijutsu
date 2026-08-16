//! Live probes against the REAL registry and (for the send test) a REAL
//! Claude Code session. Ignored by default — they touch the machine they
//! run on.
//!
//! Layer 4 of the test strategy in `docs/cc-peer.md`: the only tests that
//! validate this crate against the actual receiver rather than against our
//! own copy of its grammar. The send test is the one that catches a silent
//! attribution downgrade in reality — a green loopback suite cannot.
//!
//! Run:
//! ```sh
//! cargo test -p claude-code-peer --test live -- --ignored --nocapture
//! # send needs a target (defaults to the env var, then "kaijutsu-chan"):
//! KJ_CC_TARGET=<name-or-pid> cargo test -p claude-code-peer --test live real_send -- --ignored --nocapture
//! ```

use claude_code_peer::{
    default_sessions_dir, resolve_target, scan_sessions_dir, send_message, PROBED_AGAINST_CC,
    SUPPORTED_PEER_PROTOCOL,
};

#[test]
#[ignore = "reads the real ~/.claude/sessions; run manually"]
fn real_registry_smoke() {
    let dir = default_sessions_dir().expect("HOME set");
    let scan = scan_sessions_dir(&dir).expect("real scan should not error");
    println!(
        "probed-against CC {PROBED_AGAINST_CC} — sessions: {}  problems: {}",
        scan.sessions.len(),
        scan.errors.len()
    );
    for s in &scan.sessions {
        println!(
            "  {:<16} pid={:<8} status={:<8} reach={:<8} proto={} cc={} {}",
            s.descriptor.name.as_deref().unwrap_or("-"),
            s.descriptor.pid,
            s.descriptor.status,
            s.reachable.label(),
            s.descriptor.peer_protocol,
            s.descriptor.version,
            s.descriptor.cwd,
        );
        // The canary, loud: any session speaking a protocol we do not
        // support must stand out in the smoke output.
        assert_eq!(
            s.descriptor.peer_protocol, SUPPORTED_PEER_PROTOCOL,
            "a live session speaks a peerProtocol this crate does not — re-probe before trusting anything"
        );
    }
    for e in &scan.errors {
        println!("  ! {}: {}", e.file, e.message);
    }
}

#[test]
#[ignore = "sends a real message to a live Claude Code session; run manually"]
fn real_send_to_a_live_session() {
    let dir = default_sessions_dir().expect("HOME set");
    let target =
        std::env::var("KJ_CC_TARGET").unwrap_or_else(|_| "kaijutsu-chan".to_string());

    let scan = scan_sessions_dir(&dir).expect("real scan");
    let session = resolve_target(&scan.sessions, &target).expect("target resolves");
    println!(
        "sending to {:?} (pid {}, cc {}, proto {})",
        session.descriptor.name,
        session.descriptor.pid,
        session.descriptor.version,
        session.descriptor.peer_protocol
    );

    let outcome = send_message(
        &dir,
        session,
        "kaijutsu",
        "REAL SEND via claude-code-peer: if this arrives attributed to \
         kaijutsu, the canonical serializer is validated against the real \
         receiver.",
        None,
    )
    .expect("real send failed");
    println!(
        "delivered {} bytes, msg_id {}",
        outcome.bytes_written,
        outcome.msg_id.as_str()
    );
}
