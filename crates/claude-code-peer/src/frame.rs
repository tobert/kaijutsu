//! Newline-delimited JSON framing for the peer socket.
//!
//! A real sender emits, verbatim **[probed]**:
//!
//! ```json
//! {"msgV":1,"msg_id":"<uuid>","type":"user",
//!  "message":{"role":"user","content":"<envelope>"},
//!  "priority":"next","from":"uds:/run/user/1000/cc-socks/<pid>.sock"}
//! ```
//!
//! Wire notes, all load-bearing:
//!
//! - **`msgV` is a framing version**, distinct from the descriptor's
//!   `peerProtocol`. Two version fields; check both, fail closed on either.
//! - **`msg_id` is the uuid the sender's `SendMessage` returns to its
//!   caller** — a sender-generated correlation key. Use it; do not invent
//!   one.
//! - `priority: "next"` observed; rest of the enum unknown **[probed]**.
//! - `from` appears both top-level and inside the envelope.
//! - An `{"type":"auth","token":…}` frame may precede it. Auth is not
//!   enforced today **[probed]** — see the crate docs — but keep sending it:
//!   a future CC that begins enforcing it would drop messages *silently*.
//! - **No ack.** Zero bytes come back on success **[probed]**. A read
//!   timeout is not an error.
//!
//! Outgoing frames are serialized by a **fixed template**, not a JSON map:
//! key order and spacing are byte-stable and match what a real sender emits
//! (workspace-wide `serde_json` does not preserve insertion order, so a
//! `json!` map could not make that promise). Incoming frames parse leniently
//! through serde — key order is irrelevant to a parser.

use serde::Deserialize;

use crate::error::Error;
use crate::SUPPORTED_MSG_V;

/// A sender-generated correlation key: the uuid a `SendMessage` returns to
/// its caller **[probed]**. Ours is a fresh v4 per send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageId(pub uuid::Uuid);

impl MessageId {
    pub fn new_v4() -> Self {
        MessageId(uuid::Uuid::new_v4())
    }

    pub fn parse(s: &str) -> Option<Self> {
        uuid::Uuid::parse_str(s).ok().map(MessageId)
    }

    pub fn as_str(&self) -> String {
        self.0.to_string()
    }
}

/// One outgoing `user` frame, pre-serialization. `from` is the top-level
/// reply address — set it only when it names a socket actually being
/// listened on (see [`crate::envelope::Envelope::from`]).
#[derive(Debug, Clone)]
pub struct OutgoingUser<'a> {
    pub msg_id: MessageId,
    /// The full attribution envelope (rendered `<cross-session-message>` tag).
    pub content: &'a str,
    pub from: Option<&'a str>,
}

/// Serialize a `user` frame. The template matches a real sender's bytes
/// exactly **[probed]**: same key order (`msgV`, `msg_id`, `type`,
/// `message`, `priority`, then `from` when present), no spaces after
/// separators. `content` is the one field that needs JSON string escaping —
/// done by routing it through serde_json's string serializer alone.
pub fn user_frame_line(frame: &OutgoingUser<'_>) -> String {
    let content = serde_json::to_string(frame.content).expect("a &str always serializes");
    let mut out = format!(
        "{{\"msgV\":{},\"msg_id\":\"{}\",\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":{}}},\"priority\":\"next\"",
        SUPPORTED_MSG_V,
        frame.msg_id.as_str(),
        content,
    );
    if let Some(from) = frame.from {
        let from = serde_json::to_string(from).expect("a &str always serializes");
        out.push_str(&format!(",\"from\":{from}"));
    }
    out.push('}');
    out
}

/// Serialize the `auth` frame that precedes a send. Takes the token as a
/// `&str` (from [`crate::descriptor::PeerToken::secret`]) so the token type
/// itself never needs a formatting impl.
pub fn auth_frame_line(token: &str) -> String {
    let token = serde_json::to_string(token).expect("a &str always serializes");
    format!("{{\"type\":\"auth\",\"token\":{token}}}")
}

/// One recognized inbound frame. Deliberately **not** a closed enum over
/// wire vocabulary Claude Code owns: unrecognized `type`s land in
/// [`Incoming::Unrecognized`] with their name intact rather than failing the
/// whole connection — same posture as the descriptor's `status` field.
///
/// `Debug` is implemented by hand so the `Auth` variant can never format
/// its token.
#[derive(Clone)]
pub enum Incoming {
    /// A `user` frame carrying an attribution envelope in `content`.
    User(IncomingUser),
    /// An `auth` frame. The token is wrapped so it inherits
    /// [`crate::descriptor::PeerToken`]'s no-format guarantee.
    Auth(crate::descriptor::PeerToken),
    /// A syntactically valid frame with a `type` we don't know. Kept (with
    /// its type name) rather than dropped: a future CC that adds frame types
    /// should show up in diagnostics, not vanish into a parse error.
    Unrecognized { frame_type: String },
}

impl std::fmt::Debug for Incoming {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Incoming::User(u) => f.debug_tuple("User").field(u).finish(),
            // The token is deliberately unformattable; Debug must not be the
            // hole in that guarantee.
            Incoming::Auth(_) => f.debug_tuple("Auth").field(&"<token redacted>").finish(),
            Incoming::Unrecognized { frame_type } => f
                .debug_struct("Unrecognized")
                .field("frame_type", frame_type)
                .finish(),
        }
    }
}

/// The payload of an inbound `user` frame.
#[derive(Debug, Clone)]
pub struct IncomingUser {
    pub msg_v: Option<u64>,
    pub msg_id: Option<MessageId>,
    pub content: String,
    pub priority: Option<String>,
    /// Top-level reply address.
    pub from: Option<String>,
}

#[derive(Deserialize)]
struct WireMessage {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct WireFrame {
    #[serde(rename = "msgV", default)]
    msg_v: Option<u64>,
    #[serde(default)]
    msg_id: Option<String>,
    #[serde(rename = "type", default)]
    frame_type: Option<String>,
    #[serde(default)]
    message: Option<WireMessage>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    token: Option<String>,
}

/// Parse one wire line (without its trailing newline) into an [`Incoming`].
///
/// Fail-closed on the parts that matter: a line that is not JSON, a frame
/// with no `type`, or a `user` frame missing its `content` all error. A
/// `user` frame whose `msgV` disagrees with [`SUPPORTED_MSG_V`] still parses
/// — the version check belongs to whoever acts on the frame, and this crate's
/// own actor ([`crate::server::Inbox`]) refuses it there, dropping the frame
/// loudly — because silently accepting a framing version we have never seen
/// is how a protocol bump turns into corruption.
pub fn parse_line(line: &str) -> Result<Incoming, Error> {
    let wire: WireFrame = serde_json::from_str(line)
        .map_err(|e| Error::Frame(format!("line is not JSON: {e}")))?;
    let Some(frame_type) = wire.frame_type else {
        return Err(Error::Frame("frame has no `type` field".to_string()));
    };
    match frame_type.as_str() {
        "auth" => {
            let token = wire.token.ok_or_else(|| {
                Error::Frame("auth frame carries no token".to_string())
            })?;
            Ok(Incoming::Auth(crate::descriptor::PeerToken::from_wire(token)))
        }
        "user" => {
            let message = wire.message.ok_or_else(|| {
                Error::Frame("user frame carries no `message`".to_string())
            })?;
            if message.role.as_deref() != Some("user") {
                return Err(Error::Frame(format!(
                    "user frame with unexpected role {:?} — refusing to guess",
                    message.role
                )));
            }
            let content = message.content.ok_or_else(|| {
                Error::Frame("user frame's message carries no `content`".to_string())
            })?;
            Ok(Incoming::User(IncomingUser {
                msg_v: wire.msg_v,
                msg_id: wire.msg_id.as_deref().and_then(MessageId::parse),
                content,
                priority: wire.priority,
                from: wire.from,
            }))
        }
        other => Ok(Incoming::Unrecognized {
            frame_type: other.to_string(),
        }),
    }
}

impl crate::descriptor::PeerToken {
    /// Crate-internal constructor for the frame parser.
    pub(crate) fn from_wire(token: String) -> Self {
        Self::new(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact shape a real sender emits **[probed]** — the golden frame.
    /// Byte-identical to a live capture modulo the ids, which are
    /// sender-generated.
    const GOLDEN_USER_FRAME: &str = r#"{"msgV":1,"msg_id":"3f2c1a9e-4b6d-4e8f-9a0b-1c2d3e4f5a6b","type":"user","message":{"role":"user","content":"<cross-session-message from-name=\"kaijutsu\" from-mode=\"prompting\">\nhello there\n</cross-session-message>"},"priority":"next","from":"uds:/run/user/1000/cc-socks/4242.sock"}"#;

    #[test]
    fn our_user_frame_is_byte_identical_to_the_golden_capture() {
        let content = "<cross-session-message from-name=\"kaijutsu\" from-mode=\"prompting\">\nhello there\n</cross-session-message>";
        let frame = OutgoingUser {
            msg_id: MessageId(
                uuid::Uuid::parse_str("3f2c1a9e-4b6d-4e8f-9a0b-1c2d3e4f5a6b").unwrap(),
            ),
            content,
            from: Some("uds:/run/user/1000/cc-socks/4242.sock"),
        };
        assert_eq!(
            user_frame_line(&frame),
            GOLDEN_USER_FRAME,
            "the serializer must reproduce a real sender's bytes exactly"
        );
    }

    #[test]
    fn a_fromless_frame_omits_the_from_key_entirely() {
        let frame = OutgoingUser {
            msg_id: MessageId(
                uuid::Uuid::parse_str("3f2c1a9e-4b6d-4e8f-9a0b-1c2d3e4f5a6b").unwrap(),
            ),
            content: "c",
            from: None,
        };
        let line = user_frame_line(&frame);
        assert!(!line.contains("from"), "no from key at all: {line}");
        assert!(line.ends_with("}"));
    }

    #[test]
    fn content_is_json_escaped_in_the_template() {
        let frame = OutgoingUser {
            msg_id: MessageId::new_v4(),
            content: "line one\nline \"two\" — 日本語",
            from: None,
        };
        let line = user_frame_line(&frame);
        // The newline must be escaped on the wire...
        assert!(line.contains("\\n"), "escaped newline: {line}");
        // ...and serde must recover the exact content on parse.
        match parse_line(&line).expect("our own frame must parse") {
            Incoming::User(u) => assert_eq!(u.content, "line one\nline \"two\" — 日本語"),
            other => panic!("expected a user frame, got {other:?}"),
        }
    }

    #[test]
    fn golden_frame_parses_with_every_field() {
        match parse_line(GOLDEN_USER_FRAME).expect("golden frame must parse") {
            Incoming::User(u) => {
                assert_eq!(u.msg_v, Some(1));
                assert!(u.msg_id.is_some());
                assert_eq!(u.priority.as_deref(), Some("next"));
                assert_eq!(
                    u.from.as_deref(),
                    Some("uds:/run/user/1000/cc-socks/4242.sock")
                );
                assert!(u.content.starts_with("<cross-session-message"));
                // The content itself is a canonical envelope.
                assert!(crate::envelope::Envelope::parse(&u.content).is_ok());
            }
            other => panic!("expected a user frame, got {other:?}"),
        }
    }

    #[test]
    fn auth_frame_round_trips_without_leaking() {
        let line = auth_frame_line("0123456789abcdef");
        assert_eq!(line, r#"{"type":"auth","token":"0123456789abcdef"}"#);
        match parse_line(&line).expect("auth frame parses") {
            Incoming::Auth(token) => assert_eq!(token.secret(), "0123456789abcdef"),
            other => panic!("expected an auth frame, got {other:?}"),
        }
    }

    #[test]
    fn unknown_frame_types_are_kept_not_dropped() {
        match parse_line(r#"{"type":"future-thing","x":1}"#).expect("parses") {
            Incoming::Unrecognized { frame_type } => assert_eq!(frame_type, "future-thing"),
            other => panic!("expected Unrecognized, got {other:?}"),
        }
    }

    #[test]
    fn malformed_lines_are_loud_errors() {
        assert!(parse_line("not json").is_err());
        assert!(parse_line("{}").is_err(), "no type");
        assert!(
            parse_line(r#"{"type":"user","message":{"role":"user"}}"#).is_err(),
            "user frame without content"
        );
        assert!(
            parse_line(r#"{"type":"user","message":{"role":"assistant","content":"x"}}"#).is_err(),
            "non-user role refused"
        );
        assert!(
            parse_line(r#"{"type":"auth"}"#).is_err(),
            "auth frame without token"
        );
    }

    #[test]
    fn a_future_msg_v_still_parses_so_the_caller_can_refuse_it() {
        // parse_line is deliberately lenient about msgV: the framing version
        // check is fail-closed policy, owned by whoever acts on the frame —
        // a parser that silently swallowed version 2 would hide a protocol
        // bump instead of surfacing it.
        match parse_line(
            r#"{"msgV":2,"msg_id":"3f2c1a9e-4b6d-4e8f-9a0b-1c2d3e4f5a6b","type":"user","message":{"role":"user","content":"x"},"priority":"next"}"#,
        )
        .expect("parses")
        {
            Incoming::User(u) => assert_eq!(u.msg_v, Some(2)),
            other => panic!("expected user, got {other:?}"),
        }
    }
}
