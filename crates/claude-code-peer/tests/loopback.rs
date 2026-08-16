//! Loopback: this crate's own sender into this crate's own inbox, through
//! the public API only.
//!
//! Layer 3 of the test strategy in `docs/cc-peer.md`: proves framing
//! symmetry end to end. Necessary, insufficient — it says nothing about
//! real Claude Code, which is what the golden tests and the ignored live
//! tests anchor.

use claude_code_peer::{prepare, deliver, auth_frame_line, Inbox};

#[tokio::test]
async fn a_full_send_round_trips_with_attribution() {
    let tmp = tempfile::tempdir().unwrap();
    let mut inbox = Inbox::bind(&tmp.path().join("inboxes"), "kaijutsu.sock").expect("bind");

    let prepared = prepare("kaijutsu", "loopback body — 会術", None).expect("prepare");
    let frames = vec![
        auth_frame_line("a-token-we-never-check"),
        prepared.user_frame.clone(),
    ];
    let path = inbox.path().to_string_lossy().into_owned();
    let written = tokio::task::spawn_blocking(move || deliver(&path, &frames))
        .await
        .unwrap()
        .expect("deliver");
    assert!(written > 0);

    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), inbox.recv())
        .await
        .expect("a message arrives")
        .expect("inbox open");
    let envelope = msg.envelope.as_ref().expect("attributed delivery");
    assert_eq!(envelope.from_name(), Some("kaijutsu"));
    assert_eq!(envelope.from_mode(), Some("prompting"));
    assert_eq!(envelope.body(), "loopback body — 会術");
    assert_eq!(msg.msg_id, Some(prepared.msg_id));
}

#[tokio::test]
async fn a_reply_address_round_trips_as_the_advertised_from() {
    // The listener advertises its own uds address as `from`; the sender's
    // frame must carry it both top-level and inside the envelope, which is
    // what lets the other side copy it into a reply's `to`.
    let tmp = tempfile::tempdir().unwrap();
    let mut inbox = Inbox::bind(&tmp.path().join("inboxes"), "kaijutsu.sock").expect("bind");
    let address = inbox.uds_address();

    let prepared = prepare("kaijutsu", "reply here", Some(&address)).expect("prepare");
    let path = inbox.path().to_string_lossy().into_owned();
    let frames = vec![prepared.user_frame.clone()];
    tokio::task::spawn_blocking(move || deliver(&path, &frames))
        .await
        .unwrap()
        .expect("deliver");

    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), inbox.recv())
        .await
        .unwrap()
        .expect("inbox open");
    assert_eq!(msg.from_socket.as_deref(), Some(address.as_str()));
    assert_eq!(
        msg.envelope.as_ref().expect("attributed").from(),
        Some(address.as_str())
    );
}
