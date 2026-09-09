//! The `kj` probe corpus: every addressable `kj <path>` leaf, reflected off
//! `kj_command()`, joined against kaijutsu's authored read on each one.
//!
//! Reflection supplies what clap already knows — the path, its aliases, the
//! one-line `about`. It cannot supply a judgment call, so severity, the
//! confirm-gated bit, and the mutates bit are authored in
//! `contrib/kj-expectations.toml` and joined in here. A leaf with no
//! expectations entry is a bug, not a gap to fall back on — `corpus()`
//! refuses to return one.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Kaijutsu's authored read on how badly a clause can go wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    Informative,
    SituationNormal,
    DataCritical,
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
    /// The clause a scorer sees. Authored `clause` when the expectations file
    /// gives one, otherwise synthesized from the path plus placeholders.
    pub clause: String,
    pub expect: Severity,
    /// Whether the handler tests `KjCaller.confirmed`. Authored.
    pub confirm_gated: bool,
    /// Whether it changes durable state. Authored.
    pub mutates: bool,
    pub note: Option<String>,
}

/// A clause that is not a `kj` verb: severity probes, data-position controls,
/// benign controls. Authored wholesale in the expectations file.
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

/// Both spellings of one operation, for the alias-split report.
#[derive(Debug, Clone)]
pub struct AliasPair {
    pub path: String,
    pub canonical: String,
    pub alias: String,
}

/// Everything that can make the corpus refuse to build. Every variant lists
/// exactly what is wrong and where, so the failure is the fix instructions —
/// never a default that papers over drift between `kj_command()` and
/// `contrib/kj-expectations.toml`.
#[derive(Debug, thiserror::Error)]
pub enum CorpusError {
    #[error("contrib/kj-expectations.toml failed to parse: {0}")]
    Parse(#[from] toml::de::Error),

    #[error(
        "{} live kj leaf(ves) have no entry in contrib/kj-expectations.toml: {}",
        .0.len(), .0.join(", ")
    )]
    MissingEntries(Vec<String>),

    #[error(
        "{} entr(y/ies) in contrib/kj-expectations.toml name no live kj leaf: {}",
        .0.len(), .0.join(", ")
    )]
    OrphanEntries(Vec<String>),

    #[error("contrib/kj-expectations.toml [\"{path}\"] has an empty clause")]
    EmptyClause { path: String },
}

/// One authored expectations-file entry, before the clause is resolved
/// against reflection (positionals feed synthesis when `clause` is absent).
#[derive(Debug, Clone, Deserialize)]
struct ExpectationEntry {
    expect: Severity,
    #[serde(default)]
    mutates: bool,
    #[serde(default)]
    confirm_gated: bool,
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
struct ExpectationsFile {
    #[serde(flatten)]
    verbs: BTreeMap<String, ExpectationEntry>,
    #[serde(default, rename = "extra")]
    extras: Vec<ExtraEntry>,
}

const EXPECTATIONS_TOML: &str = include_str!("../../../../contrib/kj-expectations.toml");

/// One `kj <path>` leaf as clap sees it, before expectations are joined.
struct ReflectedLeaf {
    path: String,
    aliases: Vec<String>,
    about: String,
    /// Every required argument, positionals first in declaration order,
    /// then named options — the input to clause synthesis.
    required: Vec<RequiredArg>,
}

/// One required argument and the sample value synthesis fills it with.
struct RequiredArg {
    /// `Some("--kind")` for a named option, `None` for a positional.
    flag: Option<String>,
    placeholder: String,
}

/// Walk every subcommand of `kj_command()` and collect its leaves — a
/// subcommand with no subcommands of its own. Depth is unbounded: `backend
/// model set` and `cast slot remove` are both three deep.
fn reflect_leaves() -> Vec<ReflectedLeaf> {
    let root = super::kj_command();
    let mut out = Vec::new();
    for top in root.get_subcommands() {
        walk(top, top.get_name().to_string(), &mut out);
    }
    out
}

fn walk(cmd: &clap::Command, path: String, out: &mut Vec<ReflectedLeaf>) {
    let mut children = cmd.get_subcommands().peekable();
    if children.peek().is_none() {
        out.push(leaf_from(cmd, path));
        return;
    }
    for child in children {
        let child_path = format!("{path} {}", child.get_name());
        walk(child, child_path, out);
    }
}

fn leaf_from(cmd: &clap::Command, path: String) -> ReflectedLeaf {
    // get_all_aliases() includes hidden aliases as well as visible ones —
    // deliberate, because a hidden alias is still a real spelling a caller
    // can type; get_visible_aliases() would under-report the surface an
    // aliased attack (or an aliased typo) can actually reach.
    let aliases = cmd.get_all_aliases().map(str::to_string).collect();
    let about = cmd
        .get_about()
        .map(|s| trim_about(&s.to_string()))
        .unwrap_or_default();
    let mut required: Vec<RequiredArg> = cmd
        .get_positionals()
        .filter(|a| a.is_required_set())
        .map(|a| RequiredArg { flag: None, placeholder: placeholder_for(a) })
        .collect();
    required.extend(
        cmd.get_opts()
            .filter(|a| a.is_required_set())
            .filter_map(|a| a.get_long().map(|l| (l, a)))
            .map(|(long, a)| RequiredArg { flag: Some(format!("--{long}")), placeholder: placeholder_for(a) }),
    );
    ReflectedLeaf {
        path,
        aliases,
        about,
        required,
    }
}

/// Trim a reflected `about` at the first sentence: the first `.` or the
/// first newline, whichever comes first.
fn trim_about(about: &str) -> String {
    let dot = about.find('.');
    let nl = about.find('\n');
    let cut = match (dot, nl) {
        (Some(d), Some(n)) => Some(d.min(n)),
        (Some(d), None) => Some(d),
        (None, Some(n)) => Some(n),
        (None, None) => None,
    };
    match cut {
        Some(i) => about[..i].to_string(),
        None => about.to_string(),
    }
}

/// One sample value per required argument. A typed slot gets a value its
/// parser accepts (a number, a UUID); a text slot gets one by arg id. An id
/// with no recognized shape falls back to the id itself, so a synthesized
/// clause never invents data.
fn placeholder_for(arg: &clap::Arg) -> String {
    use std::any::TypeId;
    if let Some(first) = arg.get_value_parser().possible_values().and_then(|mut v| v.next()) {
        return first.get_name().to_string();
    }
    let ty = arg.get_value_parser().type_id();
    let numeric = [
        TypeId::of::<u8>(),
        TypeId::of::<u16>(),
        TypeId::of::<u32>(),
        TypeId::of::<u64>(),
        TypeId::of::<usize>(),
        TypeId::of::<i32>(),
        TypeId::of::<i64>(),
    ];
    if numeric.iter().any(|t| ty == *t) {
        return "1".to_string();
    }
    if ty == TypeId::of::<uuid::Uuid>() {
        return "019a2f3c-0000-7000-8000-000000000000".to_string();
    }
    let id = arg.get_id().as_str().to_ascii_lowercase();
    if id.contains("context") || id.contains("id") || id.contains("block") {
        "019a2f3c".to_string()
    } else if id.contains("path") || id.contains("file") {
        "/config/rc/coder/create/S00-stance.kai".to_string()
    } else if id.contains("hash") {
        "sha256-abc123def".to_string()
    } else {
        arg.get_id().as_str().to_string()
    }
}

fn synthesize_clause(leaf: &ReflectedLeaf, confirm_gated: bool) -> String {
    let mut clause = format!("kj {}", leaf.path);
    for arg in &leaf.required {
        if let Some(flag) = &arg.flag {
            clause.push(' ');
            clause.push_str(flag);
        }
        clause.push(' ');
        clause.push_str(&arg.placeholder);
    }
    if confirm_gated {
        clause.push_str(" --confirm");
    }
    clause
}

/// Build the full corpus: every live `kj` leaf joined against its authored
/// expectation, plus the authored extra clauses. Fails loudly — and lists
/// exactly what is wrong — when reflection and the expectations file
/// disagree about what leaves exist.
pub fn corpus() -> Result<Corpus, CorpusError> {
    let file: ExpectationsFile = toml::from_str(EXPECTATIONS_TOML)?;
    let leaves = reflect_leaves();

    let leaf_paths: BTreeSet<&str> = leaves.iter().map(|l| l.path.as_str()).collect();
    let missing: Vec<String> = leaves
        .iter()
        .filter(|l| !file.verbs.contains_key(&l.path))
        .map(|l| l.path.clone())
        .collect();
    if !missing.is_empty() {
        return Err(CorpusError::MissingEntries(missing));
    }
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
        // Present by the coverage check above.
        let entry = file.verbs.get(&leaf.path).expect("checked above");
        let clause = match &entry.clause {
            Some(c) => c.clone(),
            None => synthesize_clause(leaf, entry.confirm_gated),
        };
        if clause.trim().is_empty() {
            return Err(CorpusError::EmptyClause {
                path: leaf.path.clone(),
            });
        }
        verbs.push(KjVerb {
            path: leaf.path.clone(),
            aliases: leaf.aliases.clone(),
            about: leaf.about.clone(),
            clause,
            expect: entry.expect,
            confirm_gated: entry.confirm_gated,
            mutates: entry.mutates,
            note: entry.note.clone(),
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

/// Every verb with a second live spelling, derived from clap rather than
/// hand-listed — a new alias enters this for free. Not limited to
/// destructive verbs: any leaf's aliases are reported, the caller filters.
pub fn alias_pairs(c: &Corpus) -> Vec<AliasPair> {
    let mut pairs = Vec::new();
    for verb in &c.verbs {
        // The alias replaces only the leaf's own name, keeping any parent
        // path intact: "cast slot" + alias "rm" -> "cast slot rm". A
        // top-level leaf (no space in its path) has no parent to keep.
        let base = verb.path.rsplit_once(' ').map(|(b, _)| b);
        for alias in &verb.aliases {
            let alias_path = match base {
                Some(b) => format!("{b} {alias}"),
                None => alias.clone(),
            };
            pairs.push(AliasPair {
                path: verb.path.clone(),
                canonical: verb.path.clone(),
                alias: alias_path,
            });
        }
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_reflected_leaves() {
        let mut leaves = reflect_leaves();
        leaves.sort_by(|a, b| a.path.cmp(&b.path));
        for l in &leaves {
            let aliases = if l.aliases.is_empty() {
                String::new()
            } else {
                format!(" [{}]", l.aliases.join(", "))
            };
            println!("{}{}  -- {}", l.path, aliases, l.about);
        }
        eprintln!("TOTAL LEAVES: {}", leaves.len());
    }

    /// Every live `kj` leaf has an expectations entry. A verb added to
    /// `kj_command()` with no matching entry in
    /// `contrib/kj-expectations.toml` breaks this test, by name, instead of
    /// silently sitting outside the corpus.
    #[test]
    fn every_live_leaf_has_an_entry() {
        let file: ExpectationsFile =
            toml::from_str(EXPECTATIONS_TOML).expect("kj-expectations.toml must parse");
        let leaves = reflect_leaves();
        let missing: Vec<&str> = leaves
            .iter()
            .map(|l| l.path.as_str())
            .filter(|p| !file.verbs.contains_key(*p))
            .collect();
        assert!(
            missing.is_empty(),
            "{} live kj leaf(ves) have no entry in contrib/kj-expectations.toml: {missing:?}\n\
             Add a [\"<path>\"] table for each to contrib/kj-expectations.toml.",
            missing.len()
        );
    }

    /// Every expectations entry names a leaf that still exists. An entry
    /// left behind after a verb is renamed or removed breaks this test, by
    /// name, instead of silently going stale.
    #[test]
    fn every_entry_names_a_live_leaf() {
        let file: ExpectationsFile =
            toml::from_str(EXPECTATIONS_TOML).expect("kj-expectations.toml must parse");
        let leaf_paths: BTreeSet<String> =
            reflect_leaves().into_iter().map(|l| l.path).collect();
        let orphans: Vec<&String> = file
            .verbs
            .keys()
            .filter(|p| !leaf_paths.contains(p.as_str()))
            .collect();
        assert!(
            orphans.is_empty(),
            "{} entr(y/ies) in contrib/kj-expectations.toml name no live kj leaf: {orphans:?}\n\
             Rename or remove these tables — the verb they named no longer exists at that path.",
            orphans.len()
        );
    }

    /// `corpus()` is the thing every other consumer relies on: it must
    /// build, and every clause it hands back — synthesized or authored —
    /// must be non-empty text a scorer can actually see.
    #[test]
    fn corpus_builds_and_every_clause_is_non_empty() {
        let c = corpus().expect(
            "corpus() failed — see the CorpusError for exactly which leaves or \
             entries are out of sync",
        );
        assert!(!c.verbs.is_empty(), "corpus() produced no verbs at all");
        let empty: Vec<&str> = c
            .verbs
            .iter()
            .filter(|v| v.clause.trim().is_empty())
            .map(|v| v.path.as_str())
            .collect();
        assert!(empty.is_empty(), "verb(s) with an empty clause: {empty:?}");
        let empty_extras: Vec<&str> = c
            .extras
            .iter()
            .filter(|e| e.clause.trim().is_empty())
            .map(|e| e.clause.as_str())
            .collect();
        assert!(
            empty_extras.is_empty(),
            "extra clause(s) that are empty after all: {empty_extras:?}"
        );
    }

    /// The ten alias pairs the Python hand-listed in
    /// `contrib/lfm2d-probe.py`'s `ALIAS_PAIRS` must still be findable by
    /// reflection. A pair going missing here means a real spelling of a
    /// destructive verb silently stopped existing (or its alias did) —
    /// exactly the drift `alias_pairs()` exists to catch for free.
    #[test]
    fn alias_pairs_finds_the_hand_listed_ten() {
        let c = corpus().expect("corpus() must build for this test to mean anything");
        let pairs = alias_pairs(&c);
        let found: BTreeSet<(&str, &str)> = pairs
            .iter()
            .map(|p| (p.canonical.as_str(), p.alias.as_str()))
            .collect();

        let expected = [
            ("context remove", "context rm"),
            ("stage exclude", "stage ex"),
            ("cas rm", "cas remove"),
            ("rc rm", "rc remove"),
            ("cast remove", "cast rm"),
            ("backend remove", "backend rm"),
            ("preset remove", "preset rm"),
            ("workspace remove", "workspace rm"),
            ("binding reset", "binding clear"),
            ("drift edge rm", "drift edge remove"),
        ];

        let missing: Vec<(&str, &str)> = expected
            .iter()
            .copied()
            .filter(|pair| !found.contains(pair))
            .collect();
        assert!(
            missing.is_empty(),
            "alias pair(s) no longer found by reflection: {missing:?}\n\
             Either the alias was dropped from clap, or the canonical path changed."
        );
    }
}
