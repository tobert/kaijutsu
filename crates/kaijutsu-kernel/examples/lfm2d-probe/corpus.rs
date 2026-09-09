//! The lfm2d probe corpus: every addressable `kj <path>` leaf, reflected off
//! `kj_command()` by `kaijutsu_kernel::kj::reflect`, joined against this
//! probe's authored overrides.
//!
//! Reflection supplies what clap already knows — the path, its aliases, the
//! one-line `about`, a synthesized clause. Severity for a `kj` verb derives
//! mechanically from its `Effect` (Read -> informative, Write ->
//! situation-normal, Destroy -> data-critical); `overrides.toml` beside this
//! file carries only the leaves where lfm2d calibration disagrees with that
//! derivation, plus the extra non-`kj` clause families (severity probes,
//! data-position controls, benign controls). `corpus()` refuses to build on
//! an override naming no live leaf, or a clause that does not classify.

use std::collections::BTreeMap;

use kaijutsu_kernel::kj::effect::{self, Effect};
use kaijutsu_kernel::kj::reflect::{self, ReflectedLeaf};
use serde::{Deserialize, Serialize};

/// Kaijutsu's authored read on how badly a clause can go wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    Informative,
    SituationNormal,
    DataCritical,
}

impl From<Effect> for Severity {
    /// The mechanical mapping every `kj` verb starts from: Read carries no
    /// side effect at all, Write changes something reversible, Destroy is
    /// permanent or takes something down for good. `overrides.toml` is
    /// where lfm2d calibration disagrees with this.
    fn from(effect: Effect) -> Self {
        match effect {
            Effect::Read => Severity::Informative,
            Effect::Write => Severity::SituationNormal,
            Effect::Destroy => Severity::DataCritical,
        }
    }
}

/// One addressable `kj <path>` leaf, its reflected metadata, and kaijutsu's
/// authored read on it.
#[derive(Debug, Clone, Serialize)]
pub struct KjVerb {
    /// Space-joined leaf path without the `kj` prefix: "context remove".
    pub path: String,
    /// Leaf aliases from clap, visible and hidden: ["rm"].
    pub aliases: Vec<String>,
    /// Reflected one-line about, trimmed at the first sentence.
    pub about: String,
    /// The clause a scorer sees. Authored `clause` when an override entry
    /// gives one, otherwise reflection's own synthesized clause.
    pub clause: String,
    pub expect: Severity,
    /// What running this verb does to the world, from its own `Classify`
    /// impl — replaces the old separate `mutates`/`confirm_gated` bools,
    /// which were both just readings of this one fact.
    pub effect: Effect,
    pub note: Option<String>,
}

/// A clause that is not a `kj` verb: severity probes, data-position controls,
/// benign controls. Authored wholesale in `overrides.toml`.
#[derive(Debug, Clone, Serialize)]
pub struct ExtraClause {
    pub family: String, // "severity" | "position" | "benign"
    pub clause: String,
    pub expect: Severity,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Corpus {
    pub verbs: Vec<KjVerb>,
    pub extras: Vec<ExtraClause>,
}

/// Everything that can make the corpus refuse to build. Every variant lists
/// exactly what is wrong and where, so the failure is the fix instructions —
/// never a default that papers over drift between `kj_command()` and
/// `overrides.toml`.
#[derive(Debug, thiserror::Error)]
pub enum CorpusError {
    #[error("overrides.toml failed to parse: {0}")]
    Parse(#[from] toml::de::Error),

    #[error(
        "{} entr(y/ies) in overrides.toml name no live kj leaf: {}",
        .0.len(), .0.join(", ")
    )]
    OrphanEntries(Vec<String>),

    #[error("overrides.toml [\"{path}\"] clause {clause:?} does not classify: {source}")]
    ClauseDoesNotClassify {
        path: String,
        clause: String,
        source: effect::ClassifyError,
    },
}

/// One authored `overrides.toml` entry. Both fields are optional: an entry
/// present only to carry a realistic sample `clause` need not repeat the
/// severity reflection already derives correctly.
#[derive(Debug, Clone, Deserialize)]
struct OverrideEntry {
    #[serde(default)]
    expect: Option<Severity>,
    #[serde(default)]
    clause: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ExtraEntry {
    family: String,
    clause: String,
    expect: Severity,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OverridesFile {
    #[serde(flatten)]
    verbs: BTreeMap<String, OverrideEntry>,
    #[serde(default, rename = "extra")]
    extras: Vec<ExtraEntry>,
}

const OVERRIDES_TOML: &str = include_str!("overrides.toml");

/// Build the full corpus: every live `kj` leaf, severity derived from its
/// `Effect` and adjusted by any `overrides.toml` entry, plus the authored
/// extra clauses. Fails loudly — and says exactly what is wrong — when an
/// override names no live leaf or its clause does not classify.
pub fn corpus() -> Result<Corpus, CorpusError> {
    let file: OverridesFile = toml::from_str(OVERRIDES_TOML)?;
    let leaves: Vec<ReflectedLeaf> = reflect::reflect_leaves();

    let leaf_paths: std::collections::BTreeSet<&str> = leaves.iter().map(|l| l.path.as_str()).collect();
    let orphans: Vec<String> = file
        .verbs
        .keys()
        .filter(|p| !leaf_paths.contains(p.as_str()))
        .cloned()
        .collect();
    if !orphans.is_empty() {
        return Err(CorpusError::OrphanEntries(orphans));
    }

    let mut verbs = Vec::with_capacity(leaves.len());
    for leaf in &leaves {
        let override_entry = file.verbs.get(&leaf.path);
        let clause = override_entry
            .and_then(|e| e.clause.clone())
            .unwrap_or_else(|| reflect::synthesize_clause(leaf));
        let effect = effect::classify(&reflect::clause_argv(&clause)).map_err(|e| CorpusError::ClauseDoesNotClassify {
            path: leaf.path.clone(),
            clause: clause.clone(),
            source: e,
        })?;
        let expect = override_entry.and_then(|e| e.expect).unwrap_or_else(|| Severity::from(effect));
        verbs.push(KjVerb {
            path: leaf.path.clone(),
            aliases: leaf.aliases.clone(),
            about: leaf.about.clone(),
            clause,
            expect,
            effect,
            note: override_entry.and_then(|e| e.note.clone()),
        });
    }

    let extras = file
        .extras
        .into_iter()
        .map(|e| ExtraClause {
            family: e.family,
            clause: e.clause,
            expect: e.expect,
            note: e.note,
        })
        .collect();

    Ok(Corpus { verbs, extras })
}
