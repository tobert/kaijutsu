//! The horizon corpus — what lives past the event horizon, and how a query
//! lights it.
//!
//! Two things live here, both pure and unit-tested:
//!
//! - [`HorizonContext`], the flattened record the dive navigates. It is
//!   deliberately **not** `kaijutsu_client::ContextInfo`: the dive needs a
//!   lineage parent, drift partners, a family root, and a "fell past the
//!   horizon at" stamp, and only the first of those exists on the wire today
//!   (`docs/horizon-dive.md`, "What the kernel still owes us"). Keeping the
//!   prototype's own record makes the gap explicit instead of pretending the
//!   wire already carries it.
//! - [`HorizonRanker`], the search seam. The prototype ships
//!   [`SubstringRanker`] (token AND over titles + keywords, no index, no
//!   embeddings); `kaijutsu_index::SemanticIndex::search` slots in behind the
//!   same trait without the scene knowing (see the trait's own doc).
//!
//! [`synth_corpus`] builds the ~300-context synthetic dataset the spike runs
//! on: a deterministic fork forest plus ~10% drift cross-links, so the graph
//! is genuinely undirected-with-cycles rather than a tidy tree.

// ── Hashing ────────────────────────────────────────────────────────────────

/// FNV-1a over bytes. Same recipe the well's track bearings use
/// (`time_well::rays`) and for the same reason: a *stable* hash, so a
/// context's angular lane is the same on every run and across restarts —
/// spatial memory is only worth anything if the space holds still.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// A stable `0.0..1.0` value from `(v, salt)` — the one randomness primitive
/// the layout uses. `salt` separates independent draws for the same id
/// (angular jitter vs radial jitter) without needing a second hash function.
pub fn unit_hash(v: u32, salt: u64) -> f32 {
    let mut bytes = [0u8; 12];
    bytes[..4].copy_from_slice(&v.to_le_bytes());
    bytes[4..].copy_from_slice(&salt.to_le_bytes());
    // Top 24 bits → a clean 0..1 with no modulo bias.
    ((fnv1a(&bytes) >> 40) as f32) / ((1u64 << 24) as f32)
}

// ── The record ─────────────────────────────────────────────────────────────

/// Why a context is past the horizon. Purely presentational in the prototype
/// (it picks the accent bucket's dimming tier); a real slice would read it
/// off the kernel's own lifecycle stamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HorizonState {
    /// Explicitly demoted past the last ring.
    Demoted,
    /// Concluded and aged out of the Bumped ring.
    Concluded,
    /// Archived by hand (`a` in the well).
    Archived,
    /// Never placed anywhere — plain seat overflow.
    Overflow,
}

impl HorizonState {
    /// Short label for the reading line.
    pub fn label(self) -> &'static str {
        match self {
            HorizonState::Demoted => "demoted",
            HorizonState::Concluded => "concluded",
            HorizonState::Archived => "archived",
            HorizonState::Overflow => "overflow",
        }
    }
}

/// One context past the horizon.
///
/// `id` is the index into the corpus vector — the prototype's stand-in for
/// `ContextId`. Every cross-reference (`parent`, `drift`) is such an index,
/// which is what lets the pure graph functions stay allocation-light.
#[derive(Debug, Clone, PartialEq)]
pub struct HorizonContext {
    /// Index into the corpus — this record's own id.
    pub id: u32,
    /// Display title.
    pub title: String,
    /// Accent bucket (a `context_type`-shaped string), fed to
    /// `time_well::scene::accent_color` so the dive and the well agree on
    /// what colour a "coder" context is.
    pub accent: String,
    /// Synthesis keywords — the second field the ranker matches on.
    pub keywords: Vec<String>,
    /// Unix-ms when this context fell past the horizon. Drives depth.
    pub fell_at_ms: i64,
    /// Fork parent, `None` at a family root.
    pub parent: Option<u32>,
    /// Drift / crosstalk partners. Symmetric by construction in
    /// [`synth_corpus`]; the edge walk does not assume it.
    pub drift: Vec<u32>,
    /// The family root's id — the angular bucket every member shares.
    pub family: u32,
    /// Why it's out here.
    pub state: HorizonState,
}

// ── The search seam ────────────────────────────────────────────────────────

/// Score a whole corpus against one query line.
///
/// **The seam a real index slots into.** The prototype's implementation is
/// [`SubstringRanker`]; the shape is chosen to fit
/// `kaijutsu_index::SemanticIndex::search(query, k) -> Vec<SearchResult>`
/// without contortion — a real ranker embeds the query once, takes the top
/// `k` neighbours, writes their `score` into the returned vector and leaves
/// everything else at `0.0`. Returning a **dense** vector (one light per
/// corpus entry, in corpus order) rather than a ranked list is deliberate:
/// the scene never re-sorts or re-places anything, it only changes how
/// brightly each card burns where it already sits (principle 2,
/// query-as-light).
pub trait HorizonRanker: Send + Sync {
    /// One `0.0..=1.0` light per corpus entry, in corpus order.
    ///
    /// An **empty query lights everything** (`1.0` across the board): no
    /// query is a query that matches all of it, and the space should read as
    /// a calm even field before you start typing rather than going dark.
    fn light(&self, query: &str, corpus: &[HorizonContext]) -> Vec<f32>;
}

/// The prototype ranker: whitespace-split tokens, ANDed, matched against the
/// title and the keyword list.
///
/// Per token, best-of: exact substring in the title (`1.0`), exact keyword
/// (`0.9`), subsequence-of-title fuzzy match (`0.5`, scaled by how compact
/// the match is). A token that matches nothing zeroes the whole record — AND
/// semantics, so adding a word always narrows. The per-record light is the
/// mean of its token scores.
pub struct SubstringRanker;

impl HorizonRanker for SubstringRanker {
    fn light(&self, query: &str, corpus: &[HorizonContext]) -> Vec<f32> {
        let tokens: Vec<String> = query
            .split_whitespace()
            .map(|t| t.to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        if tokens.is_empty() {
            return vec![1.0; corpus.len()];
        }
        corpus.iter().map(|c| score_one(&tokens, c)).collect()
    }
}

/// One record's light against already-lowercased `tokens`. Split out so the
/// AND/mean rule is testable on its own.
fn score_one(tokens: &[String], c: &HorizonContext) -> f32 {
    let title = c.title.to_lowercase();
    let keywords: Vec<String> = c.keywords.iter().map(|k| k.to_lowercase()).collect();
    let mut sum = 0.0;
    for token in tokens {
        let s = token_score(token, &title, &keywords);
        if s <= 0.0 {
            return 0.0; // AND: one miss and the record is dark
        }
        sum += s;
    }
    (sum / tokens.len() as f32).clamp(0.0, 1.0)
}

/// Best score for one token against a lowercased title + keyword set.
fn token_score(token: &str, title: &str, keywords: &[String]) -> f32 {
    if title.contains(token) {
        return 1.0;
    }
    if keywords.iter().any(|k| k == token) {
        return 0.9;
    }
    if keywords.iter().any(|k| k.contains(token)) {
        return 0.75;
    }
    subsequence_score(token, title).map_or(0.0, |compactness| 0.35 + 0.2 * compactness)
}

/// If every char of `needle` appears in `haystack` in order, return how
/// *compact* the match was (`1.0` = the chars were adjacent, → `0.0` as they
/// scatter). `None` when it isn't a subsequence at all.
fn subsequence_score(needle: &str, haystack: &str) -> Option<f32> {
    if needle.is_empty() {
        return None;
    }
    let mut hay = haystack.chars().enumerate();
    let mut first = None;
    let mut last = 0usize;
    for nc in needle.chars() {
        let hit = hay.find(|(_, hc)| *hc == nc)?;
        first.get_or_insert(hit.0);
        last = hit.0;
    }
    let span = (last - first.unwrap_or(0) + 1) as f32;
    Some((needle.chars().count() as f32 / span).clamp(0.0, 1.0))
}

// ── Synthetic dataset ──────────────────────────────────────────────────────

/// Word pools for synthetic titles — deliberately overlapping so a typed
/// query lights a *scattered* set rather than one tidy cluster (the case the
/// snap navigation has to survive).
const SUBJECTS: &[&str] = &[
    "wire", "kernel", "drift", "well", "ring", "block", "context", "shell", "index", "cluster",
    "beat", "track", "glyph", "atlas", "card", "portal", "vfs", "rc", "mailbox", "phasor",
];
const PREDICATES: &[&str] = &[
    "refactor", "audit", "spike", "bugfix", "review", "port", "bench", "notes", "triage",
    "rewrite", "cleanup", "design", "repro", "teardown", "probe",
];
const ACCENTS: &[&str] = &["coder", "shell", "review", "music", "ops", "default"];
const KEYWORD_POOL: &[&str] = &[
    "capnp", "bevy", "msdf", "crdt", "ssh", "wgsl", "tokio", "proptest", "rmcp", "parley",
    "hnsw", "onnx", "alsa", "midi", "vello",
];

/// Milliseconds in a day — the depth axis's unit.
pub const DAY_MS: i64 = 86_400_000;

/// Build a deterministic synthetic corpus of `n` contexts.
///
/// Shape, chosen to stress the things the design claims to handle:
/// - a **fork forest**: each new context adopts a random already-existing
///   parent with probability `P_FORK`, otherwise starts a new family root.
///   Families therefore vary wildly in size, which is what makes the
///   angular-lane layout worth testing.
/// - **~10% drift cross-links**, drawn *across* families on purpose — these
///   are the cycles. A lineage-only graph would be a tree and the "undirected
///   graph with cycles" claim would be untested.
/// - `fell_at_ms` spread log-uniformly over the last year back from `now_ms`,
///   so the near depth band is crowded and the far one is sparse — the real
///   distribution, and the one that makes a naive linear depth mapping look
///   bad.
pub fn synth_corpus(n: usize, seed: u64, now_ms: i64) -> Vec<HorizonContext> {
    /// Probability (as a 0..1 threshold on a unit hash) that a new context
    /// forks from an existing one rather than starting a family.
    const P_FORK: f32 = 0.82;
    /// Fraction of contexts that get one drift cross-link.
    const P_DRIFT: f32 = 0.10;
    /// The oldest a synthetic context can be, in days.
    const AGE_SPAN_DAYS: f32 = 365.0;

    let salt = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut out: Vec<HorizonContext> = Vec::with_capacity(n);

    for i in 0..n {
        let id = i as u32;
        let parent = if i > 0 && unit_hash(id, salt ^ 0x01) < P_FORK {
            // Prefer a recent parent (chains, not a flat star): bias the draw
            // toward the tail of what already exists.
            let r = unit_hash(id, salt ^ 0x02);
            let pick = (i as f32 * (1.0 - r * r)) as usize;
            Some(pick.min(i - 1) as u32)
        } else {
            None
        };
        let family = match parent {
            Some(p) => out[p as usize].family,
            None => id,
        };

        let subject = SUBJECTS[(unit_hash(id, salt ^ 0x10) * SUBJECTS.len() as f32) as usize
            % SUBJECTS.len()];
        let predicate = PREDICATES[(unit_hash(id, salt ^ 0x11) * PREDICATES.len() as f32) as usize
            % PREDICATES.len()];
        let title = format!("{subject} {predicate} {id}");

        let accent =
            ACCENTS[(unit_hash(family, salt ^ 0x12) * ACCENTS.len() as f32) as usize % ACCENTS.len()];

        let kw_a = KEYWORD_POOL
            [(unit_hash(id, salt ^ 0x20) * KEYWORD_POOL.len() as f32) as usize % KEYWORD_POOL.len()];
        let kw_b = KEYWORD_POOL
            [(unit_hash(id, salt ^ 0x21) * KEYWORD_POOL.len() as f32) as usize % KEYWORD_POOL.len()];
        let mut keywords = vec![kw_a.to_string()];
        if kw_b != kw_a {
            keywords.push(kw_b.to_string());
        }

        // Log-uniform age: `u^3` piles the mass near 0 (recent).
        let u = unit_hash(id, salt ^ 0x30);
        let age_days = AGE_SPAN_DAYS * u * u * u;
        let fell_at_ms = now_ms - (age_days as f64 * DAY_MS as f64) as i64;

        let state = match (unit_hash(id, salt ^ 0x40) * 4.0) as usize {
            0 => HorizonState::Demoted,
            1 => HorizonState::Concluded,
            2 => HorizonState::Archived,
            _ => HorizonState::Overflow,
        };

        out.push(HorizonContext {
            id,
            title,
            accent: accent.to_string(),
            keywords,
            fell_at_ms,
            parent,
            drift: Vec::new(),
            family,
            state,
        });
    }

    // Drift cross-links, added after the forest exists so a link can point
    // anywhere (including "backwards" in creation order — crosstalk has no
    // respect for lineage). Stored on BOTH ends: drift is a relationship, not
    // a direction, as far as the constellation is concerned.
    for i in 0..n {
        let id = i as u32;
        if unit_hash(id, salt ^ 0x50) >= P_DRIFT {
            continue;
        }
        let target = (unit_hash(id, salt ^ 0x51) * n as f32) as usize % n;
        if target == i || out[i].family == out[target].family {
            continue; // a within-family "drift" is just lineage wearing a hat
        }
        let t = target as u32;
        if !out[i].drift.contains(&t) {
            out[i].drift.push(t);
        }
        if !out[target].drift.contains(&id) {
            out[target].drift.push(id);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    fn ctx(id: u32, title: &str, keywords: &[&str]) -> HorizonContext {
        HorizonContext {
            id,
            title: title.to_string(),
            accent: "coder".into(),
            keywords: keywords.iter().map(|k| k.to_string()).collect(),
            fell_at_ms: NOW,
            parent: None,
            drift: Vec::new(),
            family: id,
            state: HorizonState::Overflow,
        }
    }

    // ── unit_hash ──

    #[test]
    fn unit_hash_is_stable_and_in_range() {
        for v in 0..500u32 {
            let a = unit_hash(v, 7);
            assert_eq!(a, unit_hash(v, 7), "same input must hash the same way");
            assert!((0.0..1.0).contains(&a), "unit_hash({v}) = {a} out of range");
        }
    }

    #[test]
    fn unit_hash_salt_separates_draws() {
        // Two independent draws for the same id must not be the same number —
        // the angular and radial jitters would collapse onto each other.
        let differing = (0..200u32).filter(|v| unit_hash(*v, 1) != unit_hash(*v, 2)).count();
        assert!(differing > 190, "salt barely separated the draws ({differing}/200)");
    }

    // ── SubstringRanker ──

    #[test]
    fn empty_query_lights_everything() {
        let corpus = vec![ctx(0, "wire audit 0", &["capnp"]), ctx(1, "well spike 1", &["bevy"])];
        assert_eq!(SubstringRanker.light("", &corpus), vec![1.0, 1.0]);
        assert_eq!(SubstringRanker.light("   ", &corpus), vec![1.0, 1.0]);
    }

    #[test]
    fn substring_in_title_is_a_full_light() {
        let corpus = vec![ctx(0, "wire audit 0", &["capnp"])];
        assert_eq!(SubstringRanker.light("wire", &corpus), vec![1.0]);
        assert_eq!(SubstringRanker.light("WIRE", &corpus), vec![1.0], "case-insensitive");
    }

    #[test]
    fn keyword_match_lights_below_a_title_hit() {
        let corpus = vec![ctx(0, "wire audit 0", &["capnp"])];
        let kw = SubstringRanker.light("capnp", &corpus)[0];
        let title = SubstringRanker.light("wire", &corpus)[0];
        assert!(kw > 0.0 && kw < title, "keyword {kw} must light, but under a title hit {title}");
    }

    #[test]
    fn tokens_are_anded_so_more_words_narrow() {
        let corpus = vec![ctx(0, "wire audit 0", &["capnp"]), ctx(1, "wire spike 1", &["bevy"])];
        let both = SubstringRanker.light("wire audit", &corpus);
        assert!(both[0] > 0.0, "record 0 matches both tokens");
        assert_eq!(both[1], 0.0, "record 1 misses 'audit' — AND darkens it entirely");
    }

    #[test]
    fn a_token_matching_nothing_darkens_the_whole_record() {
        let corpus = vec![ctx(0, "wire audit 0", &["capnp"])];
        assert_eq!(SubstringRanker.light("wire zzzz", &corpus), vec![0.0]);
    }

    #[test]
    fn fuzzy_subsequence_lights_dimly() {
        // "wrd" is a subsequence of "wire drift" but not a substring.
        let corpus = vec![ctx(0, "wire drift 0", &[])];
        let s = SubstringRanker.light("wrd", &corpus)[0];
        assert!(s > 0.0, "subsequence must light at all");
        assert!(s < 0.9, "…but well under a real substring hit: {s}");
    }

    #[test]
    fn subsequence_score_rewards_compactness() {
        let tight = subsequence_score("abc", "abcxxxxxxxx").unwrap();
        let loose = subsequence_score("abc", "axxxxbxxxxc").unwrap();
        assert!(tight > loose, "adjacent chars must beat scattered ones: {tight} vs {loose}");
        assert_eq!(subsequence_score("abc", "acb"), None, "out of order is not a subsequence");
    }

    #[test]
    fn every_light_stays_in_unit_range() {
        let corpus = synth_corpus(120, 9, NOW);
        for q in ["", "wire", "wire audit", "capnp bevy", "zzz", "k"] {
            for (i, l) in SubstringRanker.light(q, &corpus).into_iter().enumerate() {
                assert!((0.0..=1.0).contains(&l), "query {q:?} record {i} light {l} out of range");
            }
        }
    }

    // ── synth_corpus ──

    #[test]
    fn synth_corpus_is_deterministic() {
        assert_eq!(synth_corpus(80, 3, NOW), synth_corpus(80, 3, NOW));
        assert_ne!(
            synth_corpus(80, 3, NOW),
            synth_corpus(80, 4, NOW),
            "a different seed must give a different world"
        );
    }

    #[test]
    fn synth_corpus_builds_a_forest_with_consistent_families() {
        let corpus = synth_corpus(300, 1, NOW);
        assert_eq!(corpus.len(), 300);
        let mut roots = 0;
        for c in &corpus {
            match c.parent {
                None => {
                    roots += 1;
                    assert_eq!(c.family, c.id, "a family root is its own family");
                }
                Some(p) => {
                    assert!(p < c.id, "parents must precede children (acyclic lineage)");
                    assert_eq!(c.family, corpus[p as usize].family, "family is inherited");
                }
            }
        }
        assert!(roots > 1, "more than one family expected, got {roots}");
        assert!(roots < 300, "not every context should be a root, got {roots}");
    }

    #[test]
    fn synth_corpus_adds_symmetric_cross_family_drift_links() {
        let corpus = synth_corpus(300, 1, NOW);
        let linked = corpus.iter().filter(|c| !c.drift.is_empty()).count();
        assert!(linked > 20, "expected a meaningful number of drift endpoints, got {linked}");
        for c in &corpus {
            for &d in &c.drift {
                assert_ne!(d, c.id, "no self-drift");
                assert!(
                    corpus[d as usize].drift.contains(&c.id),
                    "drift must be recorded on both ends ({} ↔ {})",
                    c.id,
                    d
                );
                assert_ne!(
                    corpus[d as usize].family, c.family,
                    "drift links are cross-family — that's what makes the cycles"
                );
            }
        }
    }

    #[test]
    fn synth_corpus_ages_are_in_the_past_and_skewed_recent() {
        let corpus = synth_corpus(300, 2, NOW);
        let mut ages: Vec<f64> =
            corpus.iter().map(|c| (NOW - c.fell_at_ms) as f64 / DAY_MS as f64).collect();
        for a in &ages {
            assert!(*a >= 0.0, "nothing falls past the horizon in the future");
            assert!(*a <= 366.0, "age {a} beyond the synthetic span");
        }
        ages.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = ages[ages.len() / 2];
        assert!(median < 100.0, "the age distribution should pile up recent, median was {median}");
    }
}
