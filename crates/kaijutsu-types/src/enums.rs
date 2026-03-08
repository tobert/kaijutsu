//! Shared enumerations for kernel persistence and context management.
//!
//! These are pure data types with no kernel dependencies — they live here
//! so both the persistence layer (`KernelDb`) and the runtime (`DriftRouter`,
//! `Kernel`) can use them without circular deps.

use std::collections::HashSet;
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
// ToolFilter — per-context tool availability
// ============================================================================

/// Filter for which tools are available in a context.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "tools")]
pub enum ToolFilter {
    /// All registered tools are available.
    #[default]
    All,

    /// Only these specific tools are available.
    AllowList(HashSet<String>),

    /// All tools except these are available.
    DenyList(HashSet<String>),
}

impl ToolFilter {
    /// Create an allow list filter.
    pub fn allow<I, S>(tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::AllowList(tools.into_iter().map(Into::into).collect())
    }

    /// Create a deny list filter.
    pub fn deny<I, S>(tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::DenyList(tools.into_iter().map(Into::into).collect())
    }

    /// Check if a tool is allowed by this filter.
    pub fn allows(&self, tool_name: &str) -> bool {
        match self {
            Self::All => true,
            Self::AllowList(allowed) => allowed.contains(tool_name),
            Self::DenyList(denied) => !denied.contains(tool_name),
        }
    }

    /// Merge with another filter (intersection of allowed tools).
    pub fn merge(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::All, other) => other.clone(),
            (this, Self::All) => this.clone(),
            (Self::AllowList(a), Self::AllowList(b)) => {
                Self::AllowList(a.intersection(b).cloned().collect())
            }
            (Self::DenyList(a), Self::DenyList(b)) => {
                Self::DenyList(a.union(b).cloned().collect())
            }
            (Self::AllowList(allowed), Self::DenyList(denied)) => {
                Self::AllowList(allowed.difference(denied).cloned().collect())
            }
            (Self::DenyList(denied), Self::AllowList(allowed)) => {
                Self::AllowList(allowed.difference(denied).cloned().collect())
            }
        }
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
        for kind in [ForkKind::Full, ForkKind::Shallow, ForkKind::Compact, ForkKind::Subtree] {
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
        for kind in [ForkKind::Full, ForkKind::Shallow, ForkKind::Compact, ForkKind::Subtree] {
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
        assert_eq!(EdgeKind::from_str("STRUCTURAL").unwrap(), EdgeKind::Structural);
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
        assert_eq!(ConsentMode::from_str("COLLABORATIVE").unwrap(), ConsentMode::Collaborative);
        assert_eq!(ConsentMode::from_str("Autonomous").unwrap(), ConsentMode::Autonomous);
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

    // ── ToolFilter ──────────────────────────────────────────────────────

    #[test]
    fn tool_filter_default_is_all() {
        assert_eq!(ToolFilter::default(), ToolFilter::All);
    }

    #[test]
    fn tool_filter_all_allows_everything() {
        let filter = ToolFilter::All;
        assert!(filter.allows("anything"));
        assert!(filter.allows("bash"));
    }

    #[test]
    fn tool_filter_allow_list() {
        let filter = ToolFilter::allow(["bash", "read", "write"]);
        assert!(filter.allows("bash"));
        assert!(filter.allows("read"));
        assert!(!filter.allows("edit"));
    }

    #[test]
    fn tool_filter_deny_list() {
        let filter = ToolFilter::deny(["bash", "dangerous"]);
        assert!(!filter.allows("bash"));
        assert!(filter.allows("read"));
    }

    #[test]
    fn tool_filter_merge() {
        let all = ToolFilter::All;
        let allow = ToolFilter::allow(["bash", "read"]);
        assert_eq!(all.merge(&allow), allow);

        let a1 = ToolFilter::allow(["bash", "read", "write"]);
        let a2 = ToolFilter::allow(["read", "write", "edit"]);
        let merged = a1.merge(&a2);
        match merged {
            ToolFilter::AllowList(set) => {
                assert!(set.contains("read"));
                assert!(set.contains("write"));
                assert!(!set.contains("bash"));
                assert!(!set.contains("edit"));
            }
            _ => panic!("expected AllowList"),
        }
    }

    #[test]
    fn tool_filter_serde_roundtrip() {
        let filter = ToolFilter::allow(["bash", "read"]);
        let json = serde_json::to_string(&filter).unwrap();
        let parsed: ToolFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(filter, parsed);

        let all = ToolFilter::All;
        let json = serde_json::to_string(&all).unwrap();
        let parsed: ToolFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(all, parsed);
    }

    // NOTE: ToolFilter uses HashSet which doesn't support postcard (positional format).
    // Wire serialization uses JSON (TEXT column in SQLite, JSON on Cap'n Proto).
    // Postcard roundtrip is intentionally not tested for ToolFilter.
}
