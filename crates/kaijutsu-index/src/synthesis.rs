//! Synthesis primitives: keyword extraction and representative block selection.
//!
//! Pure Rust functions — no Rhai dependency here (keeps kaijutsu-index's dep tree clean).
//! The composition logic lives in Rhai scripts; these are the building blocks.

use std::collections::HashMap;
use std::sync::RwLock;

use kaijutsu_types::ContextId;

// ============================================================================
// Pure math primitives
// ============================================================================

/// Cosine similarity between two vectors. Returns 0.0 for zero-norm inputs.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    (dot / (norm_a * norm_b)).clamp(-1.0, 1.0)
}

/// Compute the centroid (mean) of embeddings, L2-normalized.
///
/// Returns a zero vector if `embeddings` is empty.
pub fn centroid(embeddings: &[&[f32]]) -> Vec<f32> {
    if embeddings.is_empty() {
        return vec![];
    }
    let dims = embeddings[0].len();
    let n = embeddings.len() as f32;

    let mut mean = vec![0.0f32; dims];
    for emb in embeddings {
        for (i, &v) in emb.iter().enumerate() {
            mean[i] += v;
        }
    }
    for v in &mut mean {
        *v /= n;
    }

    // L2 normalize
    let norm: f32 = mean.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in &mut mean {
            *v /= norm;
        }
    }

    mean
}

/// Extract n-gram candidates from text.
///
/// Splits on word boundaries (whitespace + punctuation), generates sliding
/// windows from `min_n` through `max_n` words. Lowercased, deduplicated,
/// candidates shorter than 3 chars are skipped.
pub fn extract_ngrams(text: &str, min_n: usize, max_n: usize) -> Vec<String> {
    let words: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
        .filter(|w| !w.is_empty())
        .collect();

    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();

    for n in min_n..=max_n {
        if n == 0 || n > words.len() {
            continue;
        }
        for window in words.windows(n) {
            let candidate = window
                .iter()
                .map(|w| w.to_lowercase())
                .collect::<Vec<_>>()
                .join(" ");
            if candidate.len() < 3 {
                continue;
            }
            if seen.insert(candidate.clone()) {
                result.push(candidate);
            }
        }
    }

    result
}

/// Shortest sentence candidate [`split_sentences`] keeps — fragments below
/// this carry no signal ("Yes.", "Ok then").
pub const MIN_SENTENCE_CHARS: usize = 15;
/// Longest sentence candidate [`split_sentences`] keeps — beyond this it's
/// probably code or a paragraph the splitter failed to break up further.
pub const MAX_SENTENCE_CHARS: usize = 300;

/// Split `text` into deterministic sentence candidates for gist scoring.
///
/// Splits on `.`/`!`/`?`/newlines, trims whitespace off each piece, and keeps
/// only candidates within `[MIN_SENTENCE_CHARS, MAX_SENTENCE_CHARS]`. Pure and
/// synchronous — no locale/NLP sentence-boundary detection, just the cheap
/// deterministic split the extractive gist needs.
pub fn split_sentences(text: &str) -> Vec<String> {
    text.split(['.', '!', '?', '\n'])
        .map(str::trim)
        .filter(|s| {
            let len = s.chars().count();
            (MIN_SENTENCE_CHARS..=MAX_SENTENCE_CHARS).contains(&len)
        })
        .map(str::to_string)
        .collect()
}

/// Index of the sentence embedding closest (cosine similarity) to `centroid`.
///
/// `None` when `sentence_embeds` is empty. Ties keep the earliest index —
/// deterministic, no dependence on sort stability elsewhere.
pub fn best_sentence(sentence_embeds: &[&[f32]], centroid: &[f32]) -> Option<usize> {
    let mut best: Option<(usize, f32)> = None;
    for (i, emb) in sentence_embeds.iter().enumerate() {
        let score = cosine_similarity(emb, centroid);
        let replace = match best {
            Some((_, best_score)) => score > best_score,
            None => true,
        };
        if replace {
            best = Some((i, score));
        }
    }
    best.map(|(i, _)| i)
}

// ============================================================================
// Synthesis result + cache
// ============================================================================

/// Result of synthesis for a single context.
#[derive(Debug, Clone)]
pub struct SynthesisResult {
    /// (term, score) pairs — best keywords for this context.
    pub keywords: Vec<(String, f32)>,
    /// (block_id_short, score, preview) — most representative blocks.
    pub top_blocks: Vec<(String, f32, String)>,
    /// Sentence-level extractive gist: the single sentence (drawn from the
    /// top-scored blocks) closest to the context centroid, capped at 200
    /// chars. `None` when no sentence candidate cleared the bar (e.g. every
    /// block was one giant unbroken line) — callers fall back to
    /// `top_blocks`' block-head preview.
    pub gist: Option<String>,
    /// Content hash at the time of synthesis (for invalidation).
    pub content_hash: String,
}

/// Thread-safe cache of synthesis results, keyed by ContextId.
pub struct SynthesisCache {
    inner: RwLock<HashMap<ContextId, SynthesisResult>>,
}

impl SynthesisCache {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// Get a cached synthesis result. Returns None if not cached or hash mismatches.
    pub fn get(&self, ctx: ContextId, content_hash: Option<&str>) -> Option<SynthesisResult> {
        let map = self.inner.read().unwrap();
        let result = map.get(&ctx)?;
        if let Some(hash) = content_hash
            && result.content_hash != hash
        {
            return None;
        }
        Some(result.clone())
    }

    /// Get a cached synthesis result without hash checking.
    pub fn get_any(&self, ctx: ContextId) -> Option<SynthesisResult> {
        let map = self.inner.read().unwrap();
        map.get(&ctx).cloned()
    }

    /// Store a synthesis result.
    pub fn insert(&self, ctx: ContextId, result: SynthesisResult) {
        let mut map = self.inner.write().unwrap();
        map.insert(ctx, result);
    }

    /// Remove a cached result.
    pub fn remove(&self, ctx: ContextId) {
        let mut map = self.inner.write().unwrap();
        map.remove(&ctx);
    }
}

impl Default for SynthesisCache {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity_identical() {
        let a = [1.0, 0.0, 0.0];
        let b = [1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = [1.0, 0.0, 0.0];
        let b = [0.0, 1.0, 0.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = [1.0, 0.0, 0.0];
        let b = [-1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - (-1.0)).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_zero_vector() {
        let a = [0.0, 0.0, 0.0];
        let b = [1.0, 0.0, 0.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn test_centroid_single() {
        let v = [0.6, 0.8, 0.0];
        let c = centroid(&[&v]);
        // Should be L2-normalized: same direction
        assert!((c[0] - 0.6).abs() < 1e-5);
        assert!((c[1] - 0.8).abs() < 1e-5);
        let norm: f32 = c.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn test_centroid_two_vectors() {
        let a = [1.0, 0.0];
        let b = [0.0, 1.0];
        let c = centroid(&[&a[..], &b[..]]);
        // Mean = [0.5, 0.5], normalized = [1/sqrt(2), 1/sqrt(2)]
        let expected = 1.0 / 2.0f32.sqrt();
        assert!((c[0] - expected).abs() < 1e-5);
        assert!((c[1] - expected).abs() < 1e-5);
    }

    #[test]
    fn test_centroid_empty() {
        let c = centroid(&[]);
        assert!(c.is_empty());
    }

    #[test]
    fn test_ngrams_basic() {
        let text = "the quick brown fox";
        let grams = extract_ngrams(text, 1, 2);
        assert!(grams.contains(&"the".to_string()));
        assert!(grams.contains(&"quick".to_string()));
        assert!(grams.contains(&"the quick".to_string()));
        assert!(grams.contains(&"quick brown".to_string()));
        assert!(grams.contains(&"brown fox".to_string()));
    }

    #[test]
    fn test_ngrams_skips_short() {
        let text = "I am a big cat";
        let grams = extract_ngrams(text, 1, 1);
        // "I", "a" are < 3 chars, should be skipped as 1-grams
        assert!(!grams.contains(&"i".to_string()));
        assert!(!grams.contains(&"a".to_string()));
        assert!(grams.contains(&"big".to_string()));
        assert!(grams.contains(&"cat".to_string()));
    }

    #[test]
    fn test_ngrams_deduplicates() {
        let text = "go go go";
        let grams = extract_ngrams(text, 1, 1);
        let go_count = grams.iter().filter(|g| *g == "go").count();
        // "go" is only 2 chars, should be skipped
        assert_eq!(go_count, 0);

        let text2 = "run run run";
        let grams2 = extract_ngrams(text2, 1, 1);
        let run_count = grams2.iter().filter(|g| *g == "run").count();
        assert_eq!(run_count, 1);
    }

    #[test]
    fn test_ngrams_punctuation_split() {
        let text = "hello, world! foo-bar";
        let grams = extract_ngrams(text, 1, 1);
        assert!(grams.contains(&"hello".to_string()));
        assert!(grams.contains(&"world".to_string()));
        assert!(grams.contains(&"foo".to_string()));
        assert!(grams.contains(&"bar".to_string()));
    }

    #[test]
    fn test_ngrams_trigrams() {
        let text = "alpha beta gamma delta";
        let grams = extract_ngrams(text, 3, 3);
        assert!(grams.contains(&"alpha beta gamma".to_string()));
        assert!(grams.contains(&"beta gamma delta".to_string()));
        assert_eq!(grams.len(), 2);
    }

    #[test]
    fn test_split_sentences_basic() {
        let text = "Hello there, this is sentence one. Is this sentence two? Yes! And a newline break\nhere is another sentence for good measure.";
        let sentences = split_sentences(text);
        assert!(sentences.iter().any(|s| s.starts_with("Hello there")));
        assert!(sentences.iter().any(|s| s.starts_with("Is this sentence two")));
        assert!(sentences.iter().any(|s| s.starts_with("here is another")));
        // Fragments trimmed of surrounding whitespace, no leading/trailing space.
        for s in &sentences {
            assert_eq!(s.trim(), s);
        }
    }

    #[test]
    fn test_split_sentences_drops_short_fragments() {
        // "Yes!" -> "Yes" is 3 chars, well under MIN_SENTENCE_CHARS.
        let text = "Yes! No. Ok then.";
        let sentences = split_sentences(text);
        assert!(sentences.is_empty(), "all fragments too short: {sentences:?}");
    }

    #[test]
    fn test_split_sentences_drops_overlong_fragments() {
        let long = "a".repeat(MAX_SENTENCE_CHARS + 1);
        let text = format!("{long}. This one is a normal-length sentence for the test.");
        let sentences = split_sentences(&text);
        assert!(!sentences.iter().any(|s| s.len() > MAX_SENTENCE_CHARS));
        assert!(sentences.iter().any(|s| s.starts_with("This one is")));
    }

    #[test]
    fn test_split_sentences_empty_input() {
        assert!(split_sentences("").is_empty());
        assert!(split_sentences("   ").is_empty());
        assert!(split_sentences("...???!!!").is_empty());
    }

    #[test]
    fn test_best_sentence_picks_closest_to_centroid() {
        let centroid = [1.0, 0.0, 0.0];
        let a = [1.0, 0.0, 0.0]; // identical -> score 1.0
        let b = [0.0, 1.0, 0.0]; // orthogonal -> score 0.0
        let c = [0.7, 0.7, 0.0]; // partial match
        let embeds: Vec<&[f32]> = vec![&b, &c, &a];
        assert_eq!(best_sentence(&embeds, &centroid), Some(2), "index 2 (a) is the closest match");
    }

    #[test]
    fn test_best_sentence_empty_is_none() {
        let centroid = [1.0, 0.0];
        assert_eq!(best_sentence(&[], &centroid), None);
    }

    #[test]
    fn test_best_sentence_ties_keep_earliest() {
        let centroid = [1.0, 0.0];
        let a = [1.0, 0.0];
        let b = [1.0, 0.0]; // identical score to a
        let embeds: Vec<&[f32]> = vec![&a, &b];
        assert_eq!(best_sentence(&embeds, &centroid), Some(0), "earliest index wins a tie");
    }

    #[test]
    fn test_synthesis_cache_basic() {
        let cache = SynthesisCache::new();
        let ctx = ContextId::new();

        assert!(cache.get_any(ctx).is_none());

        cache.insert(
            ctx,
            SynthesisResult {
                keywords: vec![("test".into(), 0.9)],
                top_blocks: vec![],
                gist: None,
                content_hash: "abc123".into(),
            },
        );

        let result = cache.get_any(ctx).unwrap();
        assert_eq!(result.keywords.len(), 1);
        assert_eq!(result.keywords[0].0, "test");

        // Hash match
        assert!(cache.get(ctx, Some("abc123")).is_some());
        // Hash mismatch
        assert!(cache.get(ctx, Some("different")).is_none());
        // No hash check
        assert!(cache.get(ctx, None).is_some());

        cache.remove(ctx);
        assert!(cache.get_any(ctx).is_none());
    }
}
