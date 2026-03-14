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
        if let Some(hash) = content_hash {
            if result.content_hash != hash {
                return None;
            }
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
    fn test_synthesis_cache_basic() {
        let cache = SynthesisCache::new();
        let ctx = ContextId::new();

        assert!(cache.get_any(ctx).is_none());

        cache.insert(ctx, SynthesisResult {
            keywords: vec![("test".into(), 0.9)],
            top_blocks: vec![],
            content_hash: "abc123".into(),
        });

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
