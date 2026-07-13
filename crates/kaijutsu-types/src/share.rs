//! Wire contract for `/r` client shares (reverse SFTP, `docs/slash-r.md`).
//!
//! Two independent artifacts live here, both shared between `kaijutsu-client`
//! (which writes one, answers the other) and `kaijutsu-server`/`kaijutsu-kernel`
//! (which reads/issues them):
//!
//! - The **manifest** (`/index` on a client's share session): a small TSV a
//!   client synthesizes describing the shares it's offering. Self-describing —
//!   the kernel registers a session purely from what it reads here, no
//!   out-of-band pairing state.
//! - The **generation extension**: a custom SFTP `SSH_FXP_EXTENDED` request
//!   (`kaijutsu-generation@kaijutsu.dev`), the coherence stamp for `/r`.
//!
//! # Why the generation stamp is a sibling extended request, not an ATTRS field
//!
//! `docs/slash-r.md` designs the generation stamp as a vendor extension riding
//! every `ATTRS` reply directly (SFTPv3's `SSH_FILEXFER_ATTR_EXTENDED` bit).
//! `russh-sftp` 2.3's `FileAttributes` cannot carry it: the struct has no
//! extended-attribute field at all, and its `Serialize` impl has a literal
//! `// todo: extended implementation` where that bit would be encoded — so
//! embedding it in `ATTRS` is not achievable against this dependency version
//! without forking it. This module instead carries the SAME requirement
//! (required, monotonic, refused loudly if absent) over the extension
//! request/reply mechanism the crate already ships (`extended()` /
//! `Packet::ExtendedReply`, the same channel `statvfs@openssh.com` and
//! `hardlink@openssh.com` use) — a request takes a *batch* of paths so one
//! `readdir` page costs one extra round trip, not one per entry, keeping the
//! RTT-amplification concern `docs/slash-r.md` raises for the streaming
//! primitive from also landing here by accident.

use serde::{Deserialize, Serialize};

/// SFTP extended-request name for the generation coherence stamp. Advertised
/// in the client share server's `init()` extensions map (mirroring how
/// `statvfs@openssh.com` is advertised); a session whose `init()` reply omits
/// this is refused loudly at registration (version skew, no compat shim —
/// `docs/slash-r.md` "Coherence stamp").
pub const GENERATION_EXTENSION: &str = "kaijutsu-generation@kaijutsu.dev";

/// Negotiated version string for [`GENERATION_EXTENSION`], same shape as the
/// other extension version strings (`"1"`, `"2"`) this crate advertises.
pub const GENERATION_EXTENSION_VERSION: &str = "1";

/// Request payload for [`GENERATION_EXTENSION`]: the exact path strings a
/// caller would also pass to `STAT`/found via `READDIR`, batched so one wire
/// round trip covers a whole directory page.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GenerationRequest {
    pub paths: Vec<String>,
}

/// Reply payload for [`GENERATION_EXTENSION`]: host mtime-nanos per path,
/// positional with [`GenerationRequest::paths`] — same length, same order.
/// A path that doesn't resolve fails the WHOLE batch (`SSH_FX_NO_SUCH_FILE`)
/// rather than a partial reply; callers issue this immediately after a
/// successful `STAT`/`READDIR` for the same paths, so the failure mode is the
/// same open TOCTOU window the rest of `/r` already accepts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GenerationReply {
    pub generations: Vec<u64>,
}

/// One row of a client share server's `/index` manifest: one share it is
/// offering. Column order on the wire is fixed: `name  rw  client-id  nick`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareManifestRow {
    /// The share's name (the top-level directory it appears as under the
    /// client's SFTP root, and under `/r/<client-id>/<name>` kernel-side).
    pub name: String,
    /// Whether this share was OFFERED as read-write. Metadata only in slice 1
    /// — no write path exists yet regardless of this flag
    /// (`docs/slash-r.md` slice 1 scope).
    pub rw: bool,
    /// The claimed stable installation id (`client_id.rs::load_or_seed`).
    /// Namespace, not authority: authenticated identity is the SSH principal,
    /// not this string — see `docs/slash-r.md` "Session shape".
    pub client_id: String,
    /// Human-readable label for the offering installation (e.g. hostname or
    /// username), display-only.
    pub nick: String,
}

/// Header row of the manifest TSV — greppable, and lets [`parse_manifest`]
/// skip it unambiguously.
const MANIFEST_HEADER: &str = "name\trw\tclient-id\tnick";

/// Encode a set of shares into the manifest TSV a client's `/index` serves.
/// `client_id`/`nick` are the same for every row of one client's manifest —
/// carried per-row (rather than once) because the kernel parses rows
/// independently and a manifest is a flat TSV, not a nested structure.
pub fn encode_manifest(rows: &[ShareManifestRow]) -> Vec<u8> {
    let mut out = String::new();
    out.push_str(MANIFEST_HEADER);
    out.push('\n');
    for row in rows {
        // Tabs/newlines in a share name or nick would corrupt the TSV; a
        // share name comes from a CLI arg (basename or explicit `name=`), so
        // reject rather than silently mangle — callers validate at parse time
        // (`kaijutsu_client::share_server`), this is the defense-in-depth belt.
        debug_assert!(
            !row.name.contains(['\t', '\n'])
                && !row.client_id.contains(['\t', '\n'])
                && !row.nick.contains(['\t', '\n']),
            "manifest row field must not contain a tab or newline: {row:?}"
        );
        out.push_str(&row.name);
        out.push('\t');
        out.push_str(if row.rw { "rw" } else { "ro" });
        out.push('\t');
        out.push_str(&row.client_id);
        out.push('\t');
        out.push_str(&row.nick);
        out.push('\n');
    }
    out.into_bytes()
}

/// Error parsing a manifest — always the reason the whole session is refused
/// (fail loud; a malformed manifest gets no partial trust).
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error("manifest is empty (missing header)")]
    Empty,
    #[error("manifest header {0:?} does not match expected {MANIFEST_HEADER:?}")]
    BadHeader(String),
    #[error("manifest row {0} has {1} columns, expected 4: {2:?}")]
    BadColumnCount(usize, usize, String),
    #[error("manifest row {0} has invalid rw value {1:?} (expected \"ro\" or \"rw\")")]
    BadRw(usize, String),
    #[error("manifest offers no shares")]
    NoShares,
    #[error("manifest rows disagree on client-id: {0:?} vs {1:?}")]
    InconsistentClientId(String, String),
    #[error("manifest rows disagree on nick: {0:?} vs {1:?}")]
    InconsistentNick(String, String),
}

/// Parse a client's `/index` manifest bytes into its rows. Every row must
/// agree on `client_id`/`nick` (one manifest describes one client) — a
/// mismatch is refused rather than picking a winner.
pub fn parse_manifest(bytes: &[u8]) -> Result<Vec<ShareManifestRow>, ManifestError> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.lines();
    let header = lines.next().ok_or(ManifestError::Empty)?;
    if header != MANIFEST_HEADER {
        return Err(ManifestError::BadHeader(header.to_string()));
    }

    let mut rows = Vec::new();
    for (i, line) in lines.enumerate() {
        if line.is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() != 4 {
            return Err(ManifestError::BadColumnCount(i, cols.len(), line.to_string()));
        }
        let rw = match cols[1] {
            "rw" => true,
            "ro" => false,
            other => return Err(ManifestError::BadRw(i, other.to_string())),
        };
        rows.push(ShareManifestRow {
            name: cols[0].to_string(),
            rw,
            client_id: cols[2].to_string(),
            nick: cols[3].to_string(),
        });
    }

    if rows.is_empty() {
        return Err(ManifestError::NoShares);
    }
    for row in &rows[1..] {
        if row.client_id != rows[0].client_id {
            return Err(ManifestError::InconsistentClientId(
                rows[0].client_id.clone(),
                row.client_id.clone(),
            ));
        }
        if row.nick != rows[0].nick {
            return Err(ManifestError::InconsistentNick(rows[0].nick.clone(), row.nick.clone()));
        }
    }

    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, rw: bool) -> ShareManifestRow {
        ShareManifestRow {
            name: name.to_string(),
            rw,
            client_id: "c-123".to_string(),
            nick: "amy-laptop".to_string(),
        }
    }

    #[test]
    fn round_trips_through_encode_and_parse() {
        let rows = vec![row("downloads", false), row("src", true)];
        let bytes = encode_manifest(&rows);
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.starts_with(MANIFEST_HEADER));

        let parsed = parse_manifest(&bytes).unwrap();
        assert_eq!(parsed, rows);
    }

    #[test]
    fn empty_manifest_is_empty_error() {
        assert_eq!(parse_manifest(b""), Err(ManifestError::Empty));
    }

    #[test]
    fn header_only_manifest_is_no_shares() {
        let bytes = format!("{MANIFEST_HEADER}\n").into_bytes();
        assert_eq!(parse_manifest(&bytes), Err(ManifestError::NoShares));
    }

    #[test]
    fn bad_header_is_refused() {
        let bytes = b"garbage\theader\n".to_vec();
        assert!(matches!(parse_manifest(&bytes), Err(ManifestError::BadHeader(_))));
    }

    #[test]
    fn wrong_column_count_is_refused() {
        let bytes = format!("{MANIFEST_HEADER}\nonly\tthree\tcols\n").into_bytes();
        assert!(matches!(
            parse_manifest(&bytes),
            Err(ManifestError::BadColumnCount(0, 3, _))
        ));
    }

    #[test]
    fn bad_rw_value_is_refused() {
        let bytes = format!("{MANIFEST_HEADER}\ndownloads\tmaybe\tc-1\tnick\n").into_bytes();
        assert!(matches!(parse_manifest(&bytes), Err(ManifestError::BadRw(0, _))));
    }

    #[test]
    fn inconsistent_client_id_is_refused() {
        let bytes = format!(
            "{MANIFEST_HEADER}\na\tro\tc-1\tnick\nb\tro\tc-2\tnick\n"
        )
        .into_bytes();
        assert!(matches!(
            parse_manifest(&bytes),
            Err(ManifestError::InconsistentClientId(_, _))
        ));
    }

    #[test]
    fn inconsistent_nick_is_refused() {
        let bytes = format!(
            "{MANIFEST_HEADER}\na\tro\tc-1\tnick-a\nb\tro\tc-1\tnick-b\n"
        )
        .into_bytes();
        assert!(matches!(
            parse_manifest(&bytes),
            Err(ManifestError::InconsistentNick(_, _))
        ));
    }

    #[test]
    fn generation_request_reply_are_plain_serde_structs() {
        let req = GenerationRequest {
            paths: vec!["/downloads/a.txt".to_string(), "/src".to_string()],
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: GenerationRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);

        let reply = GenerationReply {
            generations: vec![1, 2],
        };
        let json = serde_json::to_string(&reply).unwrap();
        let back: GenerationReply = serde_json::from_str(&json).unwrap();
        assert_eq!(reply, back);
    }
}
