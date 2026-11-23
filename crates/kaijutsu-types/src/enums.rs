//! Shared enumerations for kernel persistence and context management.
//!
//! These are pure data types with no kernel dependencies — they live here
//! so both the persistence layer (`KernelDb`) and the runtime (`DriftRouter`,
//! `Kernel`) can use them without circular deps.

use std::fmt;

use serde::{Deserialize, Serialize};
use strum::EnumString;

// ============================================================================
// ForkKind — how a context was forked from its parent
// ============================================================================

/// How a context was forked from its parent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, EnumString)]
#[serde(rename_all = "lowercase")]
#[strum(ascii_case_insensitive)]
pub enum ForkKind {
    /// Deep copy of the parent's blocks, except unsent drafts, which stay
    /// with the parent.
    #[default]
    Full,
    /// Selection-driven fork — a subset of the parent's blocks chosen by an
    /// interval selection (`window` / `spawn` presets, or ad-hoc
    /// `--include`/`--exclude` ranges). See `docs/fork-filters.md`.
    Filtered,
    /// Fork from a compaction boundary.
    Compact,
}

impl ForkKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Filtered => "filtered",
            Self::Compact => "compact",
        }
    }
}

impl fmt::Display for ForkKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ============================================================================
// ContextState — lifecycle phase of a context
// ============================================================================

/// Lifecycle phase of a context.
///
/// Controls what operations are permitted. `Staging` contexts allow block
/// curation (toggling `excluded`) but block LLM invocation. `Live` contexts
/// are the normal operating mode. `Archived` is reserved for future use
/// (the `archived_at` timestamp on `ContextRow` remains authoritative).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, EnumString)]
#[serde(rename_all = "lowercase")]
#[strum(ascii_case_insensitive)]
pub enum ContextState {
    /// Normal operating state — LLM calls enabled, blocks read-only.
    #[default]
    Live,
    /// Post-fork curation — user can toggle excluded, LLM blocked.
    Staging,
    /// Explicitly concluded ("done") — recoverable via fork; recency-ranked by
    /// `concluded_at`. Distinct from a transient detach (which stays `Live`).
    Concluded,
    /// Frozen / hidden (future-proofing).
    Archived,
}

impl ContextState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Staging => "staging",
            Self::Concluded => "concluded",
            Self::Archived => "archived",
        }
    }
}

impl fmt::Display for ContextState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ============================================================================
// ToolFilter — retired in Phase 5 (D-54).
// `ContextToolBinding` + `McpHookPhase::ListTools` subsume allow/deny semantics
// at the instance+tool granularity operators actually care about. See
// `docs/tool-system-redesign.md` §8 Phase 5 for the replacement.
// ============================================================================

// ============================================================================
// DocKind — type of document content
// ============================================================================

/// Type of document content.
///
/// Role distinctions (User/Model/System) stay at the block level via `Role` enum.
/// This enum categorizes the document itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, EnumString)]
#[serde(rename_all = "lowercase")]
#[strum(ascii_case_insensitive)]
pub enum DocKind {
    /// Interactive human/model dialog.
    #[default]
    #[strum(
        serialize = "conversation",
        serialize = "output",
        serialize = "system",
        serialize = "user_message",
        serialize = "agent_message"
    )]
    Conversation,
    /// File body in a single block — source, markdown, and config are one
    /// kind, because the `language` field already carries the distinction.
    /// The `code`, `text`, `markdown`, and `config` tags are the retired
    /// spellings and still parse, so a row written before the collapse reads.
    #[strum(
        serialize = "file",
        serialize = "code",
        serialize = "text",
        serialize = "markdown",
        serialize = "config"
    )]
    #[serde(alias = "code", alias = "text", alias = "markdown", alias = "config")]
    File,
    /// Symbolic link. Git-style: the single block's content *is* the link
    /// target path; this kind is the authoritative "mode bit" that tells a
    /// reader to follow rather than treat the content as a file body. No
    /// backend creates one today — config/rc are ordinary host directories
    /// (`docs/config-namespace.md`) — but the kind is a persisted enum and
    /// stays for any document-backed mount that wants it.
    #[strum(serialize = "symlink")]
    Symlink,
    /// Board topic document — flat chronological discussion inside a board
    /// (`docs/board.md`). Never hydrates into an LLM conversation.
    #[strum(serialize = "topic")]
    Topic,
}

impl DocKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::File => "file",
            Self::Symlink => "symlink",
            Self::Topic => "topic",
        }
    }
}

impl fmt::Display for DocKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ============================================================================
// BoardWritePolicy — write/moderation policy for a board
// ============================================================================

/// Default write policy for a board (`docs/board.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, EnumString)]
#[serde(rename_all = "lowercase")]
#[strum(ascii_case_insensitive)]
pub enum BoardWritePolicy {
    /// Posts land directly as `Status::Done`, `excluded = false`.
    #[default]
    Open,
    /// Owner, senpai chain, and root publish directly; others land held for review.
    Curated,
    /// Read-only except root or house.
    Locked,
}

impl BoardWritePolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Curated => "curated",
            Self::Locked => "locked",
        }
    }
}

impl fmt::Display for BoardWritePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ============================================================================
// TopicStatus — lifecycle status of a board topic
// ============================================================================

/// Lifecycle status of a board topic (`docs/board.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, EnumString)]
#[serde(rename_all = "lowercase")]
#[strum(ascii_case_insensitive)]
pub enum TopicStatus {
    /// Active discussion.
    #[default]
    Open,
    /// Settled without archiving.
    Resolved,
    /// Pinned to the top of board topic listings.
    Pinned,
    /// Retained as history and hidden from active topic listings.
    Archived,
}

impl TopicStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Resolved => "resolved",
            Self::Pinned => "pinned",
            Self::Archived => "archived",
        }
    }
}

impl fmt::Display for TopicStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    // ── ForkKind ────────────────────────────────────────────────────────

    #[test]
    fn fork_kind_default() {
        assert_eq!(ForkKind::default(), ForkKind::Full);
    }

    #[test]
    fn fork_kind_as_str_roundtrip() {
        for kind in [ForkKind::Full, ForkKind::Filtered, ForkKind::Compact] {
            let s = kind.as_str();
            let parsed = ForkKind::from_str(s).unwrap();
            assert_eq!(kind, parsed);
        }
    }

    #[test]
    fn fork_kind_case_insensitive() {
        assert_eq!(ForkKind::from_str("FULL").unwrap(), ForkKind::Full);
        assert_eq!(ForkKind::from_str("Filtered").unwrap(), ForkKind::Filtered);
    }

    #[test]
    fn fork_kind_rejects_unknown() {
        // No silent fallback — an unknown kind must fail to parse so the codec
        // layer can crash rather than erase provenance.
        assert!(ForkKind::from_str("shallow").is_err(), "retired variant must not parse");
        assert!(ForkKind::from_str("subtree").is_err(), "retired variant must not parse");
        assert!(ForkKind::from_str("bogus").is_err());
    }

    #[test]
    fn fork_kind_display() {
        assert_eq!(format!("{}", ForkKind::Full), "full");
        assert_eq!(format!("{}", ForkKind::Compact), "compact");
    }

    #[test]
    fn fork_kind_serde_roundtrip() {
        let kind = ForkKind::Compact;
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, "\"compact\"");
        let parsed: ForkKind = serde_json::from_str(&json).unwrap();
        assert_eq!(kind, parsed);
    }

    #[test]
    fn fork_kind_cbor_roundtrip() {
        for kind in [ForkKind::Full, ForkKind::Filtered, ForkKind::Compact] {
            let bytes = crate::codec::encode(&kind).unwrap();
            let parsed: ForkKind = crate::codec::decode(&bytes).unwrap();
            assert_eq!(kind, parsed);
        }
    }

    // ── ContextState ────────────────────────────────────────────────────

    #[test]
    fn context_state_default() {
        assert_eq!(ContextState::default(), ContextState::Live);
    }

    #[test]
    fn context_state_as_str_roundtrip() {
        for state in [
            ContextState::Live,
            ContextState::Staging,
            ContextState::Concluded,
            ContextState::Archived,
        ] {
            let s = state.as_str();
            let parsed = ContextState::from_str(s).unwrap();
            assert_eq!(state, parsed);
        }
    }

    #[test]
    fn context_state_case_insensitive() {
        assert_eq!(
            ContextState::from_str("LIVE").unwrap(),
            ContextState::Live
        );
        assert_eq!(
            ContextState::from_str("Staging").unwrap(),
            ContextState::Staging
        );
    }

    #[test]
    fn context_state_display() {
        assert_eq!(format!("{}", ContextState::Live), "live");
        assert_eq!(format!("{}", ContextState::Staging), "staging");
        assert_eq!(format!("{}", ContextState::Concluded), "concluded");
        assert_eq!(format!("{}", ContextState::Archived), "archived");
    }

    #[test]
    fn context_state_serde_roundtrip() {
        let state = ContextState::Staging;
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(json, "\"staging\"");
        let parsed: ContextState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, parsed);
    }

    #[test]
    fn context_state_cbor_roundtrip() {
        for state in [
            ContextState::Live,
            ContextState::Staging,
            ContextState::Concluded,
            ContextState::Archived,
        ] {
            let bytes = crate::codec::encode(&state).unwrap();
            let parsed: ContextState = crate::codec::decode(&bytes).unwrap();
            assert_eq!(state, parsed);
        }
    }

    // ── ToolFilter retired in Phase 5 (D-54) — tests removed. ─────────────

    // ── DocKind ────────────────────────────────────────────────────────

    #[test]
    fn doc_kind_default() {
        assert_eq!(DocKind::default(), DocKind::Conversation);
    }

    #[test]
    fn doc_kind_as_str_roundtrip() {
        for kind in [
            DocKind::Conversation,
            DocKind::File,
            DocKind::Symlink,
            DocKind::Topic,
        ] {
            let s = kind.as_str();
            let parsed = DocKind::from_str(s).unwrap();
            assert_eq!(kind, parsed);
        }
    }

    #[test]
    fn doc_kind_case_insensitive() {
        assert_eq!(
            DocKind::from_str("CONVERSATION").unwrap(),
            DocKind::Conversation
        );
        assert_eq!(DocKind::from_str("File").unwrap(), DocKind::File);
    }

    #[test]
    fn doc_kind_legacy_aliases() {
        assert_eq!(DocKind::from_str("output").unwrap(), DocKind::Conversation);
        assert_eq!(DocKind::from_str("system").unwrap(), DocKind::Conversation);
        assert_eq!(
            DocKind::from_str("user_message").unwrap(),
            DocKind::Conversation
        );
        assert_eq!(
            DocKind::from_str("agent_message").unwrap(),
            DocKind::Conversation
        );
    }

    /// `code`, `text`, `markdown`, and `config` are the pre-collapse
    /// spellings of `File`. A row or CBOR snapshot written before the collapse
    /// must still read, so both parse paths — strum and serde — keep them.
    #[test]
    fn doc_kind_legacy_file_tags() {
        for tag in ["code", "text", "markdown", "config"] {
            assert_eq!(DocKind::from_str(tag).unwrap(), DocKind::File, "strum: {tag}");
            let parsed: DocKind = serde_json::from_str(&format!("\"{tag}\"")).unwrap();
            assert_eq!(parsed, DocKind::File, "serde: {tag}");
        }
    }

    #[test]
    fn doc_kind_display() {
        assert_eq!(format!("{}", DocKind::Conversation), "conversation");
        assert_eq!(format!("{}", DocKind::File), "file");
    }

    #[test]
    fn doc_kind_serde_roundtrip() {
        let kind = DocKind::File;
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, "\"file\"");
        let parsed: DocKind = serde_json::from_str(&json).unwrap();
        assert_eq!(kind, parsed);
    }

    #[test]
    fn doc_kind_cbor_roundtrip() {
        for kind in [
            DocKind::Conversation,
            DocKind::File,
            DocKind::Symlink,
            DocKind::Topic,
        ] {
            let bytes = crate::codec::encode(&kind).unwrap();
            let parsed: DocKind = crate::codec::decode(&bytes).unwrap();
            assert_eq!(kind, parsed);
        }
    }

    // ── BoardWritePolicy ────────────────────────────────────────────────

    #[test]
    fn board_write_policy_default() {
        assert_eq!(BoardWritePolicy::default(), BoardWritePolicy::Open);
    }

    #[test]
    fn board_write_policy_roundtrip() {
        for policy in [
            BoardWritePolicy::Open,
            BoardWritePolicy::Curated,
            BoardWritePolicy::Locked,
        ] {
            let s = policy.as_str();
            assert_eq!(format!("{policy}"), s);
            assert_eq!(BoardWritePolicy::from_str(s).unwrap(), policy);
            assert_eq!(
                BoardWritePolicy::from_str(&s.to_ascii_uppercase()).unwrap(),
                policy
            );
            let json = serde_json::to_string(&policy).unwrap();
            assert_eq!(json, format!("\"{s}\""));
            let parsed: BoardWritePolicy = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, policy);
            let cbor = crate::codec::encode(&policy).unwrap();
            let decoded: BoardWritePolicy = crate::codec::decode(&cbor).unwrap();
            assert_eq!(decoded, policy);
        }
        assert!(BoardWritePolicy::from_str("bogus").is_err());
    }

    // ── TopicStatus ─────────────────────────────────────────────────────

    #[test]
    fn topic_status_default() {
        assert_eq!(TopicStatus::default(), TopicStatus::Open);
    }

    #[test]
    fn topic_status_roundtrip() {
        for status in [
            TopicStatus::Open,
            TopicStatus::Resolved,
            TopicStatus::Pinned,
            TopicStatus::Archived,
        ] {
            let s = status.as_str();
            assert_eq!(format!("{status}"), s);
            assert_eq!(TopicStatus::from_str(s).unwrap(), status);
            assert_eq!(
                TopicStatus::from_str(&s.to_ascii_uppercase()).unwrap(),
                status
            );
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, format!("\"{s}\""));
            let parsed: TopicStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, status);
            let cbor = crate::codec::encode(&status).unwrap();
            let decoded: TopicStatus = crate::codec::decode(&cbor).unwrap();
            assert_eq!(decoded, status);
        }
        assert!(TopicStatus::from_str("bogus").is_err());
    }
}
