//! Golden-capture tests, through the PUBLIC API only.
//!
//! Layer 1 of the test strategy in `docs/cc-peer.md`: fixtures that came
//! from reality, so the suite cannot drift into wishful thinking about what
//! Claude Code writes or accepts.

use claude_code_peer::{
    frame::OutgoingUser, parse_line, scan_sessions_dir, user_frame_line, Envelope, Incoming,
    MessageId, Reachable,
};

/// A real `<pid>.json` captured from a live CC 2.1.233 session, anonymized
/// (pid moved above PID_MAX_LIMIT, sessionId/cwd/name replaced). Field
/// TYPES and structure are exactly as measured — `procStart` as a JSON
/// string, the ignored siblings present.
const GOLDEN_DESCRIPTOR: &str = include_str!("fixtures/golden_descriptor.json");

/// A real sender's `user` frame, verbatim **[probed]** — byte-identical to
/// a live capture modulo the sender-generated ids.
const GOLDEN_USER_FRAME: &str = r#"{"msgV":1,"msg_id":"3f2c1a9e-4b6d-4e8f-9a0b-1c2d3e4f5a6b","type":"user","message":{"role":"user","content":"<cross-session-message from-name=\"kaijutsu\" from-mode=\"prompting\">\nhello there\n</cross-session-message>"},"priority":"next","from":"uds:/run/user/1000/cc-socks/4242.sock"}"#;

#[test]
fn the_golden_descriptor_parses_field_by_field() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("2999999999.json"), GOLDEN_DESCRIPTOR).unwrap();

    let scan = scan_sessions_dir(tmp.path()).expect("scan");
    assert!(scan.errors.is_empty(), "{:?}", scan.errors);
    assert_eq!(scan.sessions.len(), 1);

    let session = &scan.sessions[0];
    let d = &session.descriptor;
    assert_eq!(d.pid, 2_999_999_999);
    assert_eq!(d.session_id, "11111111-2222-3333-4444-555555555555");
    assert_eq!(d.proc_start, "111358845");
    assert_eq!(d.version, "2.1.233");
    assert_eq!(d.peer_protocol, 1);
    assert_eq!(d.name.as_deref(), Some("golden-fixture"));
    assert_eq!(d.status, "idle");
    assert_eq!(
        d.messaging_socket_path,
        "/run/user/1000/cc-socks/2999999999.sock"
    );
    // The fixture pid cannot exist, so the guard must classify it Gone —
    // proof the liveness pass ran, not just the parse.
    assert_eq!(session.reachable, Reachable::Gone);
}

#[test]
fn our_sender_is_byte_identical_to_the_golden_capture() {
    // Same ids, same content, same from → must reproduce the captured bytes
    // exactly. This is the assertion a serializer that "merely parses" would
    // fail: key order, separators, and the from-key placement all matter on
    // a byte-level protocol.
    let content = Envelope::parse(
        "<cross-session-message from-name=\"kaijutsu\" from-mode=\"prompting\">\nhello there\n</cross-session-message>",
    )
    .expect("canonical envelope")
    .render();
    let frame = OutgoingUser {
        msg_id: MessageId::parse("3f2c1a9e-4b6d-4e8f-9a0b-1c2d3e4f5a6b").expect("uuid"),
        content: &content,
        from: Some("uds:/run/user/1000/cc-socks/4242.sock"),
    };
    assert_eq!(
        user_frame_line(&frame),
        GOLDEN_USER_FRAME,
        "the public serializer must reproduce a real sender's bytes exactly"
    );
}

#[test]
fn the_golden_frame_parses_back_to_its_fields() {
    match parse_line(GOLDEN_USER_FRAME).expect("golden frame parses") {
        Incoming::User(u) => {
            assert_eq!(u.msg_v, Some(1));
            assert_eq!(
                u.msg_id,
                MessageId::parse("3f2c1a9e-4b6d-4e8f-9a0b-1c2d3e4f5a6b")
            );
            assert_eq!(u.priority.as_deref(), Some("next"));
            assert_eq!(
                u.from.as_deref(),
                Some("uds:/run/user/1000/cc-socks/4242.sock")
            );
            let envelope = Envelope::parse(&u.content).expect("content is a canonical envelope");
            assert_eq!(envelope.from_name(), Some("kaijutsu"));
            assert_eq!(envelope.body(), "hello there");
        }
        other => panic!("expected a user frame, got {other:?}"),
    }
}
