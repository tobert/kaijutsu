//! The inbox: kaijutsu (or any embedder) as a **listener**.
//!
//! `SendMessage` delivers to an arbitrary `uds:` path with no entry in
//! `~/.claude/sessions/` **[probed]** — bind a listener, address it by
//! path, bytes arrive. So an embedder binds its own socket and becomes a
//! first-class reply target *without writing a fake descriptor into Claude
//! Code's private registry*. Replies then work by CC's own documented rule:
//! the replying agent copies the incoming `from` attribute as its `to`.
//!
//! ## Security posture (carried from the crate docs)
//!
//! Auth is not enforced on this socket; **the file permissions are the real
//! boundary**. [`Inbox::bind`] therefore creates its directory 0700 and
//! chmods the socket to 0600, and every [`ReceivedMessage`] is
//! **unauthenticated input**: attribution is sender-asserted, a forged
//! `from-name` arrives intact, and nothing downstream may treat attribution
//! as authorization.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tokio::io::AsyncReadExt;
use tokio::net::UnixListener;
use tokio::sync::mpsc;

use crate::envelope::Envelope;
use crate::frame::{parse_line, Incoming, MessageId};

/// One inbound `user` frame, parsed. `envelope` is `None` when the sender's
/// content did not carry a valid attribution envelope — the message still
/// arrives (that is Claude Code's behavior too), just anonymously; `content`
/// always carries the raw body the frame contained.
#[derive(Debug, Clone)]
pub struct ReceivedMessage {
    pub msg_v: Option<u64>,
    pub msg_id: Option<MessageId>,
    pub priority: Option<String>,
    /// Top-level reply address of the sender — the value to copy into `from`
    /// when replying.
    pub from_socket: Option<String>,
    /// The parsed attribution envelope, when the content carried one.
    pub envelope: Option<Envelope>,
    /// The raw `content` string, envelope or not.
    pub content: String,
}

/// Cap on one wire line from an untrusted peer. A real send is a few hundred
/// bytes; a line past this is abuse, and the connection is dropped rather
/// than buffered.
const MAX_LINE_BYTES: usize = 1 << 20;

/// A bound inbox socket and its accept loop. Dropping the `Inbox` aborts the
/// loop; the socket file itself is left on disk (unlink it if you must —
/// [`Inbox::path`]).
///
/// [`Inbox::bind`] spawns on the current tokio runtime — call it from within
/// one.
pub struct Inbox {
    path: PathBuf,
    rx: mpsc::UnboundedReceiver<ReceivedMessage>,
    task: tokio::task::JoinHandle<()>,
}

impl Inbox {
    /// Create `dir` (0700), bind `dir/name` (0600), and start accepting.
    /// A pre-existing socket file at the path is replaced — a stale socket
    /// from a previous run must not keep this inbox from binding.
    pub fn bind(dir: &Path, name: &str) -> std::io::Result<Inbox> {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;

        let path = dir.join(name);
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;

        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(accept_loop(listener, tx));

        Ok(Inbox { path, rx, task })
    }

    /// The `uds:`-style address to advertise as `from` — this is the string
    /// a replying agent receives. Only hand it out when this inbox is
    /// actually listening (which, while the `Inbox` is alive, it is).
    pub fn uds_address(&self) -> String {
        format!("uds:{}", self.path.display())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Next inbound message; `None` once the inbox is closed.
    pub async fn recv(&mut self) -> Option<ReceivedMessage> {
        self.rx.recv().await
    }
}

impl Drop for Inbox {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn accept_loop(listener: UnixListener, tx: mpsc::UnboundedSender<ReceivedMessage>) {
    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(_) => continue, // a single bad accept must not kill the inbox
        };
        let tx = tx.clone();
        tokio::spawn(handle_conn(stream, tx));
    }
}

async fn handle_conn(mut stream: tokio::net::UnixStream, tx: mpsc::UnboundedSender<ReceivedMessage>) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) => break, // EOF — the protocol sends and closes
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > MAX_LINE_BYTES && !buf.contains(&b'\n') {
                    // One giant line with no delimiter: abuse, drop the
                    // connection rather than buffer forever.
                    return;
                }
                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    let line = line[..line.len() - 1].to_vec();
                    if line.len() > MAX_LINE_BYTES {
                        continue; // oversized single frame: skip, keep going
                    }
                    let Ok(text) = std::str::from_utf8(&line) else {
                        continue; // non-UTF8 line: unrecognized input, skip
                    };
                    if text.trim().is_empty() {
                        continue;
                    }
                    let Ok(Incoming::User(user)) = parse_line(text) else {
                        // auth frames and unrecognized types are noted by
                        // nothing on purpose: auth is not identity here, and
                        // unknown vocabulary is the frame parser's business.
                        continue;
                    };
                    let message = ReceivedMessage {
                        msg_v: user.msg_v,
                        msg_id: user.msg_id,
                        priority: user.priority,
                        from_socket: user.from,
                        envelope: Envelope::parse(&user.content).ok(),
                        content: user.content,
                    };
                    if tx.send(message).is_err() {
                        return; // inbox dropped
                    }
                }
            }
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bind_sets_restrictive_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("inboxes");
        let inbox = Inbox::bind(&dir, "peer.sock").expect("bind");
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        let sock_mode = std::fs::metadata(inbox.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "inbox dir must be owner-only");
        assert_eq!(sock_mode, 0o600, "socket must be owner-only");
        assert_eq!(
            inbox.uds_address(),
            format!("uds:{}", inbox.path().display())
        );
    }

    #[tokio::test]
    async fn bind_replaces_a_stale_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("inboxes");
        let first = Inbox::bind(&dir, "peer.sock").expect("first bind");
        drop(first); // leaves the socket file behind
        assert!(dir.join("peer.sock").exists());
        let second = Inbox::bind(&dir, "peer.sock").expect("rebind over stale socket");
        assert!(second.path().exists());
    }

    #[tokio::test]
    async fn loopback_sender_to_our_own_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("inboxes");
        let mut inbox = Inbox::bind(&dir, "peer.sock").expect("bind");

        // A sender (our own client) delivers an attributed message.
        let prepared = crate::client::prepare("kaijutsu", "hello over the wire", None)
            .expect("prepare");
        let frames = vec![
            crate::frame::auth_frame_line("token-bytes"),
            prepared.user_frame.clone(),
        ];
        let path = inbox.path().to_string_lossy().into_owned();
        let written = tokio::task::spawn_blocking(move || crate::client::deliver(&path, &frames))
            .await
            .unwrap()
            .expect("deliver");
        assert!(written > 0);

        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), inbox.recv())
            .await
            .expect("message arrives within 5s")
            .expect("inbox still open");
        assert_eq!(msg.content, prepared.envelope);
        let envelope = msg.envelope.as_ref().expect("content was a canonical envelope");
        assert_eq!(envelope.from_name(), Some("kaijutsu"));
        assert_eq!(envelope.body(), "hello over the wire");
        assert_eq!(msg.msg_id, Some(prepared.msg_id));
        assert_eq!(msg.msg_v, Some(crate::SUPPORTED_MSG_V));
        assert!(msg.from_socket.is_none(), "sender advertised no reply socket");
    }

    #[tokio::test]
    async fn an_anonymous_message_still_arrives_with_envelope_none() {
        let tmp = tempfile::tempdir().unwrap();
        let mut inbox = Inbox::bind(&tmp.path().join("inboxes"), "peer.sock").expect("bind");

        // A frame whose content is NOT an envelope — the receiver's fate for
        // a sloppy serializer, replayed here on purpose: delivery succeeds,
        // attribution is absent.
        let line = format!(
            "{{\"msgV\":1,\"msg_id\":\"{}\",\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"plain text, no tag\"}},\"priority\":\"next\"}}",
            uuid::Uuid::new_v4()
        );
        let path = inbox.path().to_string_lossy().into_owned();
        tokio::task::spawn_blocking(move || crate::client::deliver(&path, &[line]))
            .await
            .unwrap()
            .expect("deliver");

        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), inbox.recv())
            .await
            .unwrap()
            .expect("inbox open");
        assert!(msg.envelope.is_none(), "no attribution to parse");
        assert_eq!(msg.content, "plain text, no tag");
    }

    #[tokio::test]
    async fn garbage_and_oversized_lines_do_not_kill_the_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let mut inbox = Inbox::bind(&tmp.path().join("inboxes"), "peer.sock").expect("bind");
        let path = inbox.path().to_string_lossy().into_owned();

        // Garbage first, a valid frame after — the inbox must survive the
        // garbage and still deliver the frame.
        let prepared = crate::client::prepare("kaijutsu", "still alive", None).unwrap();
        tokio::task::spawn_blocking(move || {
            crate::client::deliver(
                &path,
                &["not json at all".to_string(), prepared.user_frame.clone()],
            )
        })
        .await
        .unwrap()
        .expect("deliver");

        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), inbox.recv())
            .await
            .unwrap()
            .expect("inbox open");
        assert_eq!(msg.envelope.unwrap().body(), "still alive");
    }
}
