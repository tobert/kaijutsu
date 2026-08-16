//! Protocol crate for Claude Code's per-session peer messaging socket.
//!
//! Claude Code (2.1.x+) runs one unix stream socket per interactive session
//! and accepts attributed cross-session messages over it. This crate speaks
//! that protocol end to end: the on-disk session registry, the PID-reuse
//! liveness guard, the attribution envelope with its strict grammar, the
//! newline-delimited JSON framing, a sending client, and a listening inbox.
//!
//! ## Provenance discipline
//!
//! Anthropic documents the *feature* (cross-session messaging, CC 2.1.224+)
//! but not the *wire*. Everything here was either **[probed]** (observed live
//! against a real session), **[source]** (read out of the shipped binary), or
//! is marked **[inferred]**. Do not promote an inferred claim without probing
//! it — two claims were filed wrong during the first probe pass and corrected
//! only because someone re-probed them. The full writeup, including the
//! security posture, lives in kaijutsu's `docs/cc-peer.md`.
//!
//! ## The hazard this crate is built around
//!
//! The receiver's envelope parser re-serializes what its regex extracted and
//! rejects the parse unless that exactly equals the input — a
//! canonicalization check. A near-miss envelope (wrong attribute order,
//! missing the mandatory body newlines) **still delivers**: the message
//! arrives, just anonymously, attribution silently dropped. No error anywhere.
//! That silent downgrade — not a loud failure — is the failure mode the
//! serializer, the parser, and their round-trip tests exist to prevent.
//!
//! ## Security posture
//!
//! Auth on the socket is not enforced (probed): the socket's **file
//! permissions are the real boundary**, not the token. Anything on the box
//! that can write the path can inject a turn into the session. Therefore:
//!
//! - The inbox binds in a 0700 directory and chmods the socket to 0600.
//! - Every inbound message is **unauthenticated input**; attribution is
//!   sender-asserted and nothing downstream may treat it as authorization.
//! - Tokens ([`PeerToken`]) are read at send time, never stored, and the type
//!   deliberately has no `Debug`/`Display` impl so a token cannot reach a
//!   log, an error message, or a `format!` by accident.
//! - Sending keeps the auth frame anyway: a future CC that begins enforcing
//!   it would drop our messages *silently* — there is no ack to notice with.
//!
//! ## Shape
//!
//! Standalone: no `kaijutsu-*` dependency, IO paths injected, the kernel
//! policy (session↔context mapping, gating, presence) stays out. Precedent:
//! `approval-ledger`.

pub mod client;
pub mod descriptor;
pub mod envelope;
pub mod error;
pub mod frame;
pub mod liveness;
pub mod server;

pub use client::{deliver, prepare, send_message, Prepared, SendOutcome};
pub use descriptor::{
    find_key_file, read_peer_token, resolve_target, scan_sessions_dir, default_sessions_dir,
    PeerToken, Scan, ScanError, Session, SessionDescriptor,
};
pub use envelope::{validate_message_body, Envelope, EnvelopeBuilder};
pub use error::{Error, Result};
pub use frame::{
    auth_frame_line, parse_line, user_frame_line, Incoming, IncomingUser, MessageId, OutgoingUser,
};
pub use liveness::{
    classify_reachability, parse_stat_starttime, probe_proc_starttime, ProcProbe, Reachable,
};
pub use server::{Inbox, ReceivedMessage};

/// The peer-protocol version this crate speaks. Descriptors advertising any
/// other value must be refused, not guessed at ([`Session::validate_sendable`]).
/// **[probed]**: 1 is the only value ever observed.
pub const SUPPORTED_PEER_PROTOCOL: u32 = 1;

/// Framing version emitted on the wire (`msgV`). Distinct from the
/// descriptor's `peerProtocol` — two version fields; a sender must check
/// both and fail closed on either. **[probed]**: 1.
pub const SUPPORTED_MSG_V: u64 = 1;

/// The Claude Code release the wire claims in this crate were measured
/// against. A bump past it does not by itself break anything here — the
/// load-bearing checks are [`SUPPORTED_PEER_PROTOCOL`] and
/// [`SUPPORTED_MSG_V`] — but treat a mismatch as a nudge to re-probe before
/// trusting a negative result. **[probed]** on moltar + zorak, 2026-08.
pub const PROBED_AGAINST_CC: &str = "2.1.233";
