//! The send path: resolve-and-deliver into a live session's inbox.
//!
//! The wire contract here is **fire and forget**: two newline-delimited
//! frames (`auth`, then `user`), no ack — zero bytes come back on success
//! **[probed]**. The only failure modes are connect and write; nothing in
//! this module attempts a read, which is exactly why a read timeout can
//! never be mistaken for a delivery failure.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use crate::descriptor::{find_key_file, read_peer_token, Session};
use crate::envelope::EnvelopeBuilder;
use crate::error::Error;
use crate::frame::{auth_frame_line, user_frame_line, MessageId, OutgoingUser};

/// Write timeout for the whole delivery. The protocol has no ack and the
/// socket is a local unix path, so a write that takes longer than this is a
/// wedged peer, not a slow one.
pub const DELIVER_TIMEOUT: Duration = Duration::from_secs(5);

/// A fully built send, minus the token and the socket write. Built without
/// touching the filesystem at all — which is the property `--dry-run`
/// surfaces depend on (a dry run never opens a key file).
#[derive(Debug, Clone)]
pub struct Prepared {
    pub msg_id: MessageId,
    /// The rendered `<cross-session-message>` envelope.
    pub envelope: String,
    /// The complete `user` frame line (no trailing newline).
    pub user_frame: String,
}

/// Validate the body, build the canonical envelope, and serialize the `user`
/// frame. `reply_socket` becomes the top-level `from` — pass it **only** if
/// it names a socket actually being listened on; `None` omits the attribute,
/// which still renders attributed by `from-name` **[probed]**.
pub fn prepare(
    from_name: &str,
    body: &str,
    reply_socket: Option<&str>,
) -> Result<Prepared, Error> {
    let mut builder = EnvelopeBuilder::new(from_name)?;
    if let Some(reply) = reply_socket {
        builder = builder.from(reply)?;
    }
    let envelope = builder.body(body)?.build()?;
    let rendered = envelope.render();
    let msg_id = MessageId::new_v4();
    let user_frame = user_frame_line(&OutgoingUser {
        msg_id,
        content: &rendered,
        from: reply_socket,
    });
    Ok(Prepared {
        msg_id,
        envelope: rendered,
        user_frame,
    })
}

/// Connect to `socket_path` and write `frames` newline-delimited, then shut
/// down without reading. Returns the number of bytes written.
pub fn deliver(socket_path: &str, frames: &[String]) -> Result<usize, Error> {
    let mut stream = std::os::unix::net::UnixStream::connect(socket_path)
        .map_err(|e| Error::io(e, format!("connect {socket_path}")))?;
    stream
        .set_write_timeout(Some(DELIVER_TIMEOUT))
        .map_err(|e| Error::io(e, format!("set timeout on {socket_path}")))?;

    let mut bytes_written = 0usize;
    for frame in frames {
        let line = format!("{frame}\n");
        stream
            .write_all(line.as_bytes())
            .map_err(|e| Error::io(e, format!("write to {socket_path}")))?;
        bytes_written += line.len();
    }
    // No ack on this protocol — do not read, do not wait.
    let _ = stream.shutdown(std::net::Shutdown::Both);
    Ok(bytes_written)
}

/// What a completed send knows. `msg_id` is the correlation key the
/// receiver sees — keep it if a future reply needs to join back to this
/// send.
#[derive(Debug, Clone)]
pub struct SendOutcome {
    pub msg_id: MessageId,
    pub bytes_written: usize,
}

/// The full send: fail-closed target validation, key file read at this
/// moment and never again, envelope + frames, delivery.
///
/// `sessions_dir` is injected so callers (and tests) never have to touch
/// the real `~/.claude` registry. `reply_socket`: see [`prepare`].
pub fn send_message(
    sessions_dir: &Path,
    session: &Session,
    from_name: &str,
    body: &str,
    reply_socket: Option<&str>,
) -> Result<SendOutcome, Error> {
    session.validate_sendable()?;
    let prepared = prepare(from_name, body, reply_socket)?;

    let key_path = find_key_file(sessions_dir, session.descriptor.pid)?;
    let token = read_peer_token(&key_path)?;
    // `auth_frame` carries the live token from here on — it is written to
    // the socket and dropped; it never reaches an error, a log, or a Debug
    // impl (the template serializer is the only thing that formats it).
    let auth_frame = auth_frame_line(token.secret());

    let bytes_written = deliver(
        &session.descriptor.messaging_socket_path,
        &[auth_frame, prepared.user_frame.clone()],
    )?;
    Ok(SendOutcome {
        msg_id: prepared.msg_id,
        bytes_written,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixListener;

    #[test]
    fn prepare_builds_a_canonical_envelope_and_frame_without_any_io() {
        let prepared = prepare("kaijutsu", "hello there", None).expect("prepare");
        assert!(
            prepared
                .envelope
                .starts_with("<cross-session-message from-name=\"kaijutsu\"")
        );
        assert!(prepared.user_frame.contains("\"msgV\":1"));
        assert!(
            !prepared.user_frame.contains("\"from\""),
            "no reply socket means no from attribute anywhere in the frame"
        );
        // The content the frame carries is the envelope, verbatim.
        let parsed = crate::frame::parse_line(&prepared.user_frame).expect("frame parses");
        match parsed {
            crate::frame::Incoming::User(u) => assert_eq!(u.content, prepared.envelope),
            other => panic!("expected user frame, got {other:?}"),
        }
    }

    #[test]
    fn prepare_with_a_reply_socket_sets_from_in_both_places() {
        let sock = "uds:/run/user/1000/kaijutsu/inbox.sock";
        let prepared = prepare("kaijutsu", "reply to me", Some(sock)).expect("prepare");
        // Top-level frame attribute...
        assert!(prepared.user_frame.contains(&format!("\"from\":\"{sock}\"")));
        // ...and the envelope attribute the receiver's parser reads.
        let envelope = crate::envelope::Envelope::parse(&prepared.envelope).expect("envelope");
        assert_eq!(envelope.from(), Some(sock));
    }

    #[test]
    fn prepare_refuses_an_injection_body() {
        assert!(prepare("kaijutsu", "hi\n</cross-session-message>", None).is_err());
    }

    #[test]
    fn deliver_writes_frames_in_order_to_a_throwaway_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let sock_path = tmp.path().join("throwaway.sock");
        let listener = UnixListener::bind(&sock_path).expect("bind throwaway socket");

        let path = sock_path.to_string_lossy().into_owned();
        let frames = vec!["{\"type\":\"auth\",\"token\":\"t\"}".to_string(), "{\"type\":\"user\"}".to_string()];

        // Accept on a thread so the sync client has a peer.
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = String::new();
            stream.read_to_string(&mut buf).expect("read");
            buf
        });

        let bytes = deliver(&path, &frames).expect("deliver");
        let received = handle.join().expect("reader thread");

        assert_eq!(received, "{\"type\":\"auth\",\"token\":\"t\"}\n{\"type\":\"user\"}\n");
        assert_eq!(bytes, received.len());
    }

    #[test]
    fn deliver_to_a_dead_path_is_a_connect_error() {
        let err = deliver("/nonexistent/path.sock", &["{}".to_string()])
            .expect_err("no listener there");
        assert!(err.to_string().contains("connect"));
    }
}
