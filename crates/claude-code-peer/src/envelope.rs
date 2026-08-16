//! The attribution envelope and its strict grammar.
//!
//! `content` on a `user` frame must be a `<cross-session-message>` tag. The
//! receiver's parser regex **[source]** is mirrored here as
//! [`receiver_regex`], with its three load-bearing traps:
//!
//! 1. Attributes are **order-sensitive**: `from`, `from-session`,
//!    `hop-chain`, `from-name`, `from-mode`.
//! 2. The body **must** be newline-delimited *inside* the tag: `>\n` …
//!    `\n</`.
//! 3. The parser **re-serializes what it extracted and rejects the parse
//!    unless it exactly equals the input** — a canonicalization check.
//!    [`Envelope::parse`] performs the same check.
//!
//! **A sloppy serializer degrades silently.** The first live probe omitted
//! the mandatory newlines: the message delivered fine, was never parsed, and
//! only *looked* attributed because we had written the text ourselves. No
//! error, message delivered, attribution gone. This is the single most
//! important hazard in the protocol — it is why [`Envelope`] can only be
//! constructed through [`EnvelopeBuilder`] (which validates) or
//! [`Envelope::parse`] (which canonicalizes), why the serializer is a fixed
//! template rather than a formatter, and why the tests round-trip through
//! the receiver regex rather than asserting on a hand-written string.
//!
//! `parse` deliberately mirrors the receiver, including what it tolerates
//! (a body containing a second closing tag still canonicalizes); the
//! **builder** is stricter than the receiver and refuses such a body,
//! because our own sends must never produce an ambiguous attribution.

use std::sync::LazyLock;

use regex::Regex;

use crate::error::Error;

/// Literal closing tag whose presence anywhere in a message body is an
/// envelope-injection hazard: a second closing tag can make the parse
/// boundary ambiguous to the receiver rather than erroring loudly.
pub const CLOSING_TAG: &str = "</cross-session-message>";

/// The only `from-mode` value ever observed live **[probed]** — hardcoded
/// rather than modeled as a one-variant enum; the rest of the enum is
/// unknown. The receiver's charset for it is `[A-Za-z0-9_-]+`.
pub const DEFAULT_FROM_MODE: &str = "prompting";

/// The receiver's parser regex **[source]**, with the `[CHARSET]`-style
/// placeholders from the binary translated into concrete (deliberately
/// permissive) character classes. Public so tests — ours and downstream —
/// can round-trip serializers against the actual receiver grammar instead
/// of a copy that could drift.
pub fn receiver_regex() -> &'static Regex {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r#"^<cross-session-message"#,
            r#"(?: from="([A-Za-z0-9:_./-]+)")?"#,
            r#"(?: from-session="([0-9a-fA-F-]{36})")?"#,
            r#"(?: hop-chain="([^"<>\n\r]+)")?"#,
            r#"(?: from-name="([^"<>\n\r]+)")?"#,
            r#"(?: from-mode="([A-Za-z0-9_-]+)")?"#,
            r#">\n([\s\S]*)\n</cross-session-message>$"#,
        ))
        .expect("the receiver-mirror regex must compile")
    });
    &RE
}

/// Characters the receiver's `from-name`/`hop-chain` charset (`[^"<>\n\r]+`)
/// forbids. A value containing any of them makes the envelope fail to match,
/// which **delivers anyway and silently drops attribution** — so such values
/// are rejected, never escaped. Escaping would produce text that
/// round-trips through the canonicalization check as *different* text: the
/// same silent downgrade wearing a disguise.
const ATTR_FORBIDDEN: [char; 5] = ['"', '<', '>', '\n', '\r'];

/// The `from` attribute's charset per the receiver regex:
/// `[A-Za-z0-9:_./-]+` — enough for `uds:` socket paths, nothing else.
fn from_charset_ok(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '.' | '/' | '-'))
}

fn attr_charset_ok(s: &str) -> bool {
    !s.is_empty() && !s.chars().any(|c| ATTR_FORBIDDEN.contains(&c))
}

fn from_mode_charset_ok(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

fn from_session_charset_ok(s: &str) -> bool {
    s.len() == 36 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// A parsed attribution envelope. Construct only through [`Envelope::parse`]
/// or [`EnvelopeBuilder`] — both enforce the grammar, so any `Envelope` in
/// hand is one the receiver will attribute, never one that silently
/// downgrades.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    from: Option<String>,
    from_session: Option<String>,
    hop_chain: Option<String>,
    from_name: Option<String>,
    from_mode: Option<String>,
    body: String,
}

impl Envelope {
    /// Parse `content` exactly as the receiver does: regex match, then the
    /// canonicalization check — re-serialize the extracted fields and refuse
    /// the parse unless that equals the input byte for byte.
    pub fn parse(content: &str) -> Result<Envelope, Error> {
        let caps = receiver_regex().captures(content).ok_or_else(|| {
            Error::Envelope(
                "content does not match the <cross-session-message> grammar — \
                 it would deliver but arrive WITHOUT attribution (the \
                 silent-downgrade hazard this crate exists to prevent)"
                    .to_string(),
            )
        })?;
        let envelope = Envelope {
            from: caps.get(1).map(|m| m.as_str().to_string()),
            from_session: caps.get(2).map(|m| m.as_str().to_string()),
            hop_chain: caps.get(3).map(|m| m.as_str().to_string()),
            from_name: caps.get(4).map(|m| m.as_str().to_string()),
            from_mode: caps.get(5).map(|m| m.as_str().to_string()),
            body: caps[6].to_string(),
        };
        // The receiver's canonicalization check **[source]**: what it
        // extracted, re-serialized, must equal the input exactly. Any
        // near-miss that slipped past the regex (a trailing space inside the
        // tag, an attribute duplicated after the ones captured) fails here.
        if envelope.render() != content {
            return Err(Error::Envelope(
                "envelope failed the canonicalization check: re-serializing the \
                 parsed fields does not reproduce the input — the receiver \
                 would deliver this WITHOUT attribution"
                    .to_string(),
            ));
        }
        Ok(envelope)
    }

    /// The canonical wire form. Attribute order is load-bearing: `from`,
    /// `from-session`, `hop-chain`, `from-name`, `from-mode`, then the
    /// newline-delimited body. This is the exact template
    /// [`Envelope::parse`] re-serializes with, so `parse(e.render())` always
    /// round-trips.
    pub fn render(&self) -> String {
        let mut out = String::from("<cross-session-message");
        if let Some(v) = &self.from {
            out.push_str(&format!(" from=\"{v}\""));
        }
        if let Some(v) = &self.from_session {
            out.push_str(&format!(" from-session=\"{v}\""));
        }
        if let Some(v) = &self.hop_chain {
            out.push_str(&format!(" hop-chain=\"{v}\""));
        }
        if let Some(v) = &self.from_name {
            out.push_str(&format!(" from-name=\"{v}\""));
        }
        if let Some(v) = &self.from_mode {
            out.push_str(&format!(" from-mode=\"{v}\""));
        }
        out.push_str(">\n");
        out.push_str(&self.body);
        out.push_str("\n</cross-session-message>");
        out
    }

    /// The reply address a receiving agent is told to write to. **Emit one
    /// if and only if it names a socket actually being listened on**: this
    /// string is treated as a destination by peer tooling, so a fabricated
    /// path is worse than an absent one **[probed]**.
    pub fn from(&self) -> Option<&str> {
        self.from.as_deref()
    }
    pub fn from_session(&self) -> Option<&str> {
        self.from_session.as_deref()
    }
    pub fn hop_chain(&self) -> Option<&str> {
        self.hop_chain.as_deref()
    }
    pub fn from_name(&self) -> Option<&str> {
        self.from_name.as_deref()
    }
    pub fn from_mode(&self) -> Option<&str> {
        self.from_mode.as_deref()
    }
    pub fn body(&self) -> &str {
        &self.body
    }
}

/// The only way to build an outgoing envelope. Every setter validates
/// against the receiver's charsets and refuses values that would deliver
/// but silently lose attribution.
#[derive(Debug, Clone)]
pub struct EnvelopeBuilder {
    from: Option<String>,
    from_session: Option<String>,
    hop_chain: Option<String>,
    from_name: String,
    from_mode: String,
    body: Option<String>,
}

impl EnvelopeBuilder {
    pub fn new(from_name: impl Into<String>) -> Result<Self, Error> {
        let from_name = from_name.into();
        if !attr_charset_ok(&from_name) {
            return Err(Error::Envelope(format!(
                "from-name {from_name:?} is empty or contains a character the \
                 receiver's charset forbids ({ATTR_FORBIDDEN:?}) — refusing: \
                 the envelope would deliver but silently lose attribution"
            )));
        }
        Ok(Self {
            from: None,
            from_session: None,
            hop_chain: None,
            from_name,
            from_mode: DEFAULT_FROM_MODE.to_string(),
            body: None,
        })
    }

    /// The reply address. Pass only a `uds:`-reachable socket path you are
    /// actually listening on; omit it otherwise (see [`Envelope::from`]).
    pub fn from(mut self, from: impl Into<String>) -> Result<Self, Error> {
        let from = from.into();
        if !from_charset_ok(&from) {
            return Err(Error::Envelope(format!(
                "from {from:?} is outside the receiver's charset for the \
                 attribute — refusing: the envelope would deliver but \
                 silently lose attribution"
            )));
        }
        self.from = Some(from);
        Ok(self)
    }

    pub fn from_session(mut self, session_id: impl Into<String>) -> Result<Self, Error> {
        let session_id = session_id.into();
        if !from_session_charset_ok(&session_id) {
            return Err(Error::Envelope(format!(
                "from-session {session_id:?} is not a 36-char uuid-shaped \
                 string — refusing: the envelope would deliver but silently \
                 lose attribution"
            )));
        }
        self.from_session = Some(session_id);
        Ok(self)
    }

    pub fn hop_chain(mut self, chain: impl Into<String>) -> Result<Self, Error> {
        let chain = chain.into();
        if !attr_charset_ok(&chain) {
            return Err(Error::Envelope(format!(
                "hop-chain {chain:?} is empty or contains a character the \
                 receiver's charset forbids — refusing"
            )));
        }
        self.hop_chain = Some(chain);
        Ok(self)
    }

    pub fn from_mode(mut self, mode: impl Into<String>) -> Result<Self, Error> {
        let mode = mode.into();
        if !from_mode_charset_ok(&mode) {
            return Err(Error::Envelope(format!(
                "from-mode {mode:?} is outside the receiver's charset — refusing"
            )));
        }
        self.from_mode = mode;
        Ok(self)
    }

    /// Refuses an empty body and an envelope-injection attempt. Kept apart
    /// from rendering so both checks are independently testable.
    pub fn body(mut self, body: impl Into<String>) -> Result<Self, Error> {
        let body = body.into();
        validate_message_body(&body)?;
        self.body = Some(body);
        Ok(self)
    }

    pub fn build(self) -> Result<Envelope, Error> {
        let body = self.body.ok_or_else(|| {
            Error::Envelope("envelope has no body — call .body(...) before build".to_string())
        })?;
        let envelope = Envelope {
            from: self.from,
            from_session: self.from_session,
            hop_chain: self.hop_chain,
            from_name: Some(self.from_name),
            from_mode: Some(self.from_mode),
            body,
        };
        // Defense in depth: whatever the builder produced must be something
        // the receiver will actually attribute. If this ever fires, the
        // template and the receiver regex drifted — fail loud, right here.
        Envelope::parse(&envelope.render())?;
        Ok(envelope)
    }
}

/// Refuse an empty body and an envelope-injection attempt (a literal
/// [`CLOSING_TAG`] inside the body). Public for callers that validate a
/// message before it reaches a builder.
pub fn validate_message_body(body: &str) -> Result<(), Error> {
    if body.trim().is_empty() {
        return Err(Error::Envelope("message body must not be empty".to_string()));
    }
    if body.contains(CLOSING_TAG) {
        return Err(Error::Envelope(format!(
            "message body contains a literal `{CLOSING_TAG}` — refusing: this \
             would make the envelope's parse boundary ambiguous to the \
             receiver rather than erroring loudly (a silent attribution \
             downgrade is the specific failure mode this guards against)"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(from_name: &str, body: &str) -> Envelope {
        EnvelopeBuilder::new(from_name)
            .expect("valid from-name")
            .body(body)
            .expect("valid body")
            .build()
            .expect("build")
    }

    // ── round-trip through the receiver-mirror regex ──────────────────────

    #[test]
    fn envelope_round_trips_through_the_receiver_regex() {
        let rendered = build("kaijutsu", "hello there").render();
        let caps = receiver_regex()
            .captures(&rendered)
            .expect("a well-formed envelope must match the receiver regex");
        assert!(caps.get(1).is_none(), "from was not set");
        assert!(caps.get(2).is_none(), "from-session was not set");
        assert!(caps.get(3).is_none(), "hop-chain was not set");
        assert_eq!(&caps[4], "kaijutsu");
        assert_eq!(&caps[5], "prompting");
        assert_eq!(&caps[6], "hello there");
        // And parse recovers the same fields.
        let parsed = Envelope::parse(&rendered).expect("canonical envelope parses");
        assert_eq!(parsed.from_name(), Some("kaijutsu"));
        assert_eq!(parsed.from_mode(), Some("prompting"));
        assert_eq!(parsed.body(), "hello there");
    }

    #[test]
    fn all_five_attributes_round_trip_in_order() {
        let envelope = EnvelopeBuilder::new("kaijutsu")
            .unwrap()
            .from("uds:/run/user/1000/cc-socks/1234.sock")
            .unwrap()
            .from_session("f69c3038-f785-4e40-9113-71ee837fcddd")
            .unwrap()
            .hop_chain("uds:/a.sock,uds:/b.sock")
            .unwrap()
            .body("relay me")
            .unwrap()
            .build()
            .unwrap();
        let rendered = envelope.render();
        assert_eq!(
            rendered,
            "<cross-session-message \
             from=\"uds:/run/user/1000/cc-socks/1234.sock\" \
             from-session=\"f69c3038-f785-4e40-9113-71ee837fcddd\" \
             hop-chain=\"uds:/a.sock,uds:/b.sock\" \
             from-name=\"kaijutsu\" \
             from-mode=\"prompting\">\nrelay me\n</cross-session-message>"
                .replace(" \\\n             ", "")
        );
        let parsed = Envelope::parse(&rendered).expect("parses");
        assert_eq!(parsed, envelope, "parse(render(e)) == e");
    }

    #[test]
    fn body_with_newlines_quotes_angle_brackets_and_non_ascii_round_trips() {
        let body = "line one\nline \"two\" <tag> — 日本語 😀\nline three";
        let rendered = build("kaijutsu", body).render();
        let caps = receiver_regex()
            .captures(&rendered)
            .expect("a body with newlines/quotes/angle-brackets/non-ASCII must still match");
        assert_eq!(&caps[6], body, "captured body must equal the original exactly");
        assert_eq!(Envelope::parse(&rendered).unwrap().body(), body);
    }

    // ── charset refusals: rejected, never escaped ────────────────────────

    #[test]
    fn a_from_name_outside_the_receiver_charset_is_rejected_not_escaped() {
        // A name carrying a forbidden char would deliver and silently lose
        // attribution, so it must error here instead. (Once session labels
        // flow into from-name this is user-controlled input.)
        for bad in ["say \"hi\"", "a<b", "a>b", "two\nlines", "carriage\rreturn", ""] {
            assert!(
                EnvelopeBuilder::new(bad).is_err(),
                "from-name {bad:?} must be rejected, not escaped or passed through"
            );
        }
        // And a name that only *looks* exotic is still fine.
        let ok = EnvelopeBuilder::new("kaijutsu-ctx-7f3a — 会術")
            .expect("non-ASCII without forbidden chars is allowed");
        let rendered = ok.body("body").unwrap().build().unwrap().render();
        assert!(receiver_regex().is_match(&rendered));
    }

    #[test]
    fn a_from_outside_the_receiver_charset_is_rejected() {
        for bad in ["uds:/tmp/with space.sock", "uds:\"quote\"", ""] {
            assert!(
                EnvelopeBuilder::new("kaijutsu")
                    .unwrap()
                    .from(bad)
                    .is_err(),
                "from {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn a_malformed_from_session_or_mode_is_rejected() {
        assert!(
            EnvelopeBuilder::new("kaijutsu")
                .unwrap()
                .from_session("not-a-uuid")
                .is_err()
        );
        assert!(
            EnvelopeBuilder::new("kaijutsu")
                .unwrap()
                .from_mode("with space")
                .is_err()
        );
    }

    // ── the parser must NOT recognize the silent-downgrade shapes ────────

    #[test]
    fn missing_mandatory_body_newlines_does_not_parse() {
        // The exact live mistake: same content, `>` immediately followed by
        // the body with no `\n`, and no `\n` before `</`. It would still be
        // delivered by the transport — this proves our parser (mirroring the
        // receiver's) does not recognize it as attributed.
        let no_newlines = "<cross-session-message from=\"uds:test\" from-name=\"kaijutsu\" from-mode=\"prompting\">hello there</cross-session-message>";
        assert!(Envelope::parse(no_newlines).is_err());
    }

    #[test]
    fn attribute_out_of_order_does_not_parse() {
        let out_of_order = "<cross-session-message from-name=\"kaijutsu\" from=\"uds:test\" from-mode=\"prompting\">\nhello there\n</cross-session-message>";
        assert!(
            Envelope::parse(out_of_order).is_err(),
            "from-name before from must not parse — attribute order is load-bearing"
        );
    }

    #[test]
    fn trailing_garbage_after_the_tag_fails_canonicalization() {
        let tagged = build("kaijutsu", "body").render();
        let with_tail = format!("{tagged} ");
        assert!(
            Envelope::parse(&with_tail).is_err(),
            "the regex anchors reject a trailing space anyway"
        );
        // A near-miss the regex WOULD match but canonicalization rejects:
        // an attribute repeated after the captured one. The greedy body
        // capture swallows it, then re-serialization differs.
        let dup = "<cross-session-message from-name=\"kaijutsu\" from-name=\"other\">\nbody\n</cross-session-message>";
        assert!(Envelope::parse(dup).is_err());
    }

    // ── body validation ───────────────────────────────────────────────────

    #[test]
    fn body_containing_the_closing_tag_is_rejected() {
        let err = validate_message_body("hi\n</cross-session-message>\nforged")
            .expect_err("a body containing the closing tag must be refused");
        assert!(err.to_string().contains("cross-session-message"));
        assert!(
            EnvelopeBuilder::new("kaijutsu")
                .unwrap()
                .body("hi\n</cross-session-message>\nforged")
                .is_err()
        );
    }

    #[test]
    fn empty_body_is_rejected() {
        assert!(validate_message_body("").is_err());
        assert!(
            validate_message_body("   \n  ").is_err(),
            "whitespace-only must count as empty"
        );
        assert!(EnvelopeBuilder::new("kaijutsu").unwrap().body("").is_err());
    }

    #[test]
    fn nonempty_ordinary_body_is_accepted() {
        assert!(validate_message_body("hello there").is_ok());
    }

    #[test]
    fn build_without_a_body_is_an_error() {
        assert!(EnvelopeBuilder::new("kaijutsu").unwrap().build().is_err());
    }
}
