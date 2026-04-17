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
    /// Full deep copy of parent state.
    #[default]
    Full,
    /// Shallow fork — shares block history, diverges on new writes.
    Shallow,
    /// Fork from a compaction boundary.
    Compact,
    /// Fork of a subtree (subset of parent's blocks).
    Subtree,
}

impl ForkKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Shallow => "shallow",
            Self::Compact => "compact",
            Self::Subtree => "subtree",
        }
    }
}

impl fmt::Display for ForkKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ============================================================================
// EdgeKind — edge type in the context graph
// ============================================================================

/// Edge type in the context graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, EnumString)]
#[serde(rename_all = "lowercase")]
#[strum(ascii_case_insensitive)]
pub enum EdgeKind {
    /// Structural parent-child edge (fork lineage).
    #[default]
    Structural,
    /// Drift communication edge (content transfer).
    Drift,
}

impl EdgeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Structural => "structural",
            Self::Drift => "drift",
        }
    }
}

impl fmt::Display for EdgeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ============================================================================
// ConsentMode — collaborative vs autonomous
// ============================================================================

/// Consent mode determines how collaborative vs autonomous the kernel is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, EnumString)]
#[serde(rename_all = "lowercase")]
#[strum(ascii_case_insensitive)]
pub enum ConsentMode {
    /// Human approval required for mutations.
    #[default]
    Collaborative,
    /// Agent can act autonomously.
    Autonomous,
}

impl ConsentMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Collaborative => "collaborative",
            Self::Autonomous => "autonomous",
        }
    }
}

impl fmt::Display for ConsentMode {
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
    /// Frozen (future-proofing).
    Archived,
}

impl ContextState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Staging => "staging",
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
// `ContextToolBinding` + `HookPhase::ListTools` subsume allow/deny semantics
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
    /// Executable code.
    Code,
    /// Static markdown/text.
    #[strum(serialize = "text", serialize = "markdown")]
    Text,
    /// Configuration file (theme.toml, models.toml).
    #[strum(serialize = "config")]
    Config,
}

impl DocKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Code => "code",
            Self::Text => "text",
            Self::Config => "config",
        }
    }
}

impl fmt::Display for DocKind {
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
        for kind in [
            ForkKind::Full,
            ForkKind::Shallow,
            ForkKind::Compact,
            ForkKind::Subtree,
        ] {
            let s = kind.as_str();
            let parsed = ForkKind::from_str(s).unwrap();
            assert_eq!(kind, parsed);
        }
    }

    #[test]
    fn fork_kind_case_insensitive() {
        assert_eq!(ForkKind::from_str("FULL").unwrap(), ForkKind::Full);
        assert_eq!(ForkKind::from_str("Shallow").unwrap(), ForkKind::Shallow);
    }

    #[test]
    fn fork_kind_display() {
        assert_eq!(format!("{}", ForkKind::Full), "full");
        assert_eq!(format!("{}", ForkKind::Subtree), "subtree");
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
    fn fork_kind_postcard_roundtrip() {
        for kind in [
            ForkKind::Full,
            ForkKind::Shallow,
            ForkKind::Compact,
            ForkKind::Subtree,
        ] {
            let bytes = postcard::to_stdvec(&kind).unwrap();
            let parsed: ForkKind = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(kind, parsed);
        }
    }

    // ── EdgeKind ────────────────────────────────────────────────────────

    #[test]
    fn edge_kind_default() {
        assert_eq!(EdgeKind::default(), EdgeKind::Structural);
    }

    #[test]
    fn edge_kind_as_str_roundtrip() {
        for kind in [EdgeKind::Structural, EdgeKind::Drift] {
            let s = kind.as_str();
            let parsed = EdgeKind::from_str(s).unwrap();
            assert_eq!(kind, parsed);
        }
    }

    #[test]
    fn edge_kind_case_insensitive() {
        assert_eq!(
            EdgeKind::from_str("STRUCTURAL").unwrap(),
            EdgeKind::Structural
        );
        assert_eq!(EdgeKind::from_str("Drift").unwrap(), EdgeKind::Drift);
    }

    #[test]
    fn edge_kind_display() {
        assert_eq!(format!("{}", EdgeKind::Structural), "structural");
        assert_eq!(format!("{}", EdgeKind::Drift), "drift");
    }

    #[test]
    fn edge_kind_serde_roundtrip() {
        let kind = EdgeKind::Drift;
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, "\"drift\"");
        let parsed: EdgeKind = serde_json::from_str(&json).unwrap();
        assert_eq!(kind, parsed);
    }

    #[test]
    fn edge_kind_postcard_roundtrip() {
        for kind in [EdgeKind::Structural, EdgeKind::Drift] {
            let bytes = postcard::to_stdvec(&kind).unwrap();
            let parsed: EdgeKind = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(kind, parsed);
        }
    }

    // ── ConsentMode ─────────────────────────────────────────────────────

    #[test]
    fn consent_mode_default() {
        assert_eq!(ConsentMode::default(), ConsentMode::Collaborative);
    }

    #[test]
    fn consent_mode_as_str_roundtrip() {
        for mode in [ConsentMode::Collaborative, ConsentMode::Autonomous] {
            let s = mode.as_str();
            let parsed = ConsentMode::from_str(s).unwrap();
            assert_eq!(mode, parsed);
        }
    }

    #[test]
    fn consent_mode_case_insensitive() {
        assert_eq!(
            ConsentMode::from_str("COLLABORATIVE").unwrap(),
            ConsentMode::Collaborative
        );
        assert_eq!(
            ConsentMode::from_str("Autonomous").unwrap(),
            ConsentMode::Autonomous
        );
    }

    #[test]
    fn consent_mode_display() {
        assert_eq!(format!("{}", ConsentMode::Collaborative), "collaborative");
        assert_eq!(format!("{}", ConsentMode::Autonomous), "autonomous");
    }

    #[test]
    fn consent_mode_serde_roundtrip() {
        let mode = ConsentMode::Autonomous;
        let json = serde_json::to_string(&mode).unwrap();
        assert_eq!(json, "\"autonomous\"");
        let parsed: ConsentMode = serde_json::from_str(&json).unwrap();
        assert_eq!(mode, parsed);
    }

    #[test]
    fn consent_mode_postcard_roundtrip() {
        for mode in [ConsentMode::Collaborative, ConsentMode::Autonomous] {
            let bytes = postcard::to_stdvec(&mode).unwrap();
            let parsed: ConsentMode = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(mode, parsed);
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
    fn context_state_postcard_roundtrip() {
        for state in [
            ContextState::Live,
            ContextState::Staging,
            ContextState::Archived,
        ] {
            let bytes = postcard::to_stdvec(&state).unwrap();
            let parsed: ContextState = postcard::from_bytes(&bytes).unwrap();
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
            DocKind::Code,
            DocKind::Text,
            DocKind::Config,
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
        assert_eq!(DocKind::from_str("Code").unwrap(), DocKind::Code);
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
        assert_eq!(DocKind::from_str("markdown").unwrap(), DocKind::Text);
    }

    #[test]
    fn doc_kind_display() {
        assert_eq!(format!("{}", DocKind::Conversation), "conversation");
        assert_eq!(format!("{}", DocKind::Config), "config");
    }

    #[test]
    fn doc_kind_serde_roundtrip() {
        let kind = DocKind::Config;
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, "\"config\"");
        let parsed: DocKind = serde_json::from_str(&json).unwrap();
        assert_eq!(kind, parsed);
    }

    #[test]
    fn doc_kind_postcard_roundtrip() {
        for kind in [
            DocKind::Conversation,
            DocKind::Code,
            DocKind::Text,
            DocKind::Config,
        ] {
            let bytes = postcard::to_stdvec(&kind).unwrap();
            let parsed: DocKind = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(kind, parsed);
        }
    }
}
