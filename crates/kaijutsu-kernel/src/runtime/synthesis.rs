//! Synthesis runner — extracts keywords and representative blocks for a context.
//!
//! Replaces the Rhai-based synthesis script with a direct Rust implementation.
//! Uses the same `Embedder` and `BlockSource` traits from `kaijutsu-index`.

use std::sync::Arc;

use kaijutsu_index::synthesis::{SynthesisCache, SynthesisResult, centroid, cosine_similarity, extract_ngrams};
use kaijutsu_index::{BlockSource, Embedder};
use kaijutsu_types::ContextId;

/// Run synthesis for a context: extract keywords and representative blocks.
///
/// Blocking operation (ONNX embed calls) — call from `spawn_blocking`.
/// Returns `None` on errors or empty contexts.
pub fn run_synthesis(
    ctx_id: ContextId,
    embedder: Arc<dyn Embedder>,
    block_source: Arc<dyn BlockSource>,
) -> Option<SynthesisResult> {
    // 1. Get blocks, filter out files and very short content
    let blocks = match block_source.block_snapshots(ctx_id) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, ctx = %ctx_id.short(), "synthesis: fetch blocks failed");
            return None;
        }
    };

    let text_blocks: Vec<_> = blocks
        .into_iter()
        .filter(|b| b.kind.to_string() != "file" && b.content.len() > 10)
        .collect();

    if text_blocks.is_empty() {
        return None;
    }

    // 2. Embed all block texts
    let texts: Vec<&str> = text_blocks.iter().map(|b| b.content.as_str()).collect();
    let embeds = match embedder.embed_batch(&texts) {
        Ok(e) if e.len() == texts.len() && !e.is_empty() => e,
        Ok(_) => return None,
        Err(e) => {
            tracing::warn!(error = %e, ctx = %ctx_id.short(), "synthesis: embed_batch failed");
            return None;
        }
    };

    // 3. Compute document centroid
    let embed_refs: Vec<&[f32]> = embeds.iter().map(|v| v.as_slice()).collect();
    let doc = centroid(&embed_refs);

    // 4. Score blocks by cosine similarity to centroid, take top 3
    let mut scored_blocks: Vec<(usize, f32)> = embeds
        .iter()
        .enumerate()
        .map(|(i, emb)| (i, cosine_similarity(emb, &doc)))
        .collect();
    scored_blocks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored_blocks.truncate(3);

    let top_blocks: Vec<(String, f32, String)> = scored_blocks
        .iter()
        .map(|(i, score)| {
            let snap = &text_blocks[*i];
            let preview = if snap.content.len() > 80 {
                snap.content[..80].to_string()
            } else {
                snap.content.clone()
            };
            (snap.id.to_string(), *score, preview)
        })
        .collect();

    // 5. Extract ngram candidates, cap at 50
    let all_text: String = texts.join(" ");
    let mut candidates = extract_ngrams(&all_text, 1, 3);
    candidates.truncate(50);

    if candidates.is_empty() {
        return Some(SynthesisResult {
            keywords: Vec::new(),
            top_blocks,
            content_hash: String::new(),
        });
    }

    // 6. Embed candidates
    let cand_refs: Vec<&str> = candidates.iter().map(|s| s.as_str()).collect();
    let cand_embeds = match embedder.embed_batch(&cand_refs) {
        Ok(e) if e.len() == candidates.len() => e,
        _ => {
            return Some(SynthesisResult {
                keywords: Vec::new(),
                top_blocks,
                content_hash: String::new(),
            });
        }
    };

    // 7. Score keywords by cosine similarity to centroid, take top 8
    let mut scored_kw: Vec<(String, f32)> = candidates
        .into_iter()
        .zip(cand_embeds.iter())
        .map(|(kw, emb)| (kw, cosine_similarity(emb, &doc)))
        .collect();
    scored_kw.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored_kw.truncate(8);

    tracing::debug!(
        ctx = %ctx_id.short(),
        keywords = scored_kw.len(),
        blocks = top_blocks.len(),
        "synthesis complete"
    );

    Some(SynthesisResult {
        keywords: scored_kw,
        top_blocks,
        content_hash: String::new(),
    })
}

/// Run synthesis and cache the result.
pub fn run_synthesis_and_cache(
    ctx_id: ContextId,
    embedder: Arc<dyn Embedder>,
    block_source: Arc<dyn BlockSource>,
    cache: &SynthesisCache,
) {
    if let Some(synth) = run_synthesis(ctx_id, embedder, block_source) {
        cache.insert(ctx_id, synth);
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_index::{Embedder, IndexError};
    use kaijutsu_types::{BlockId, BlockKind, BlockSnapshot, PrincipalId, Role, Status};
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    /// Deterministic mock embedder (same as in kaijutsu-index).
    struct MockEmbedder {
        dims: usize,
    }

    impl Embedder for MockEmbedder {
        fn model_name(&self) -> &str {
            "mock"
        }
        fn dimensions(&self) -> usize {
            self.dims
        }
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, IndexError> {
            texts.iter().map(|t| self.embed(t)).collect()
        }
        fn embed(&self, text: &str) -> Result<Vec<f32>, IndexError> {
            let mut v = vec![0.0f32; self.dims];
            for (i, byte) in text.bytes().enumerate() {
                let mut hasher = DefaultHasher::new();
                (i, byte).hash(&mut hasher);
                let h = hasher.finish();
                let idx = (h as usize) % self.dims;
                v[idx] += (h as f32) / u64::MAX as f32;
            }
            let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for x in &mut v {
                    *x /= norm;
                }
            } else {
                v[0] = 1.0;
            }
            Ok(v)
        }
    }

    struct MockBlockSource {
        blocks: Vec<BlockSnapshot>,
    }

    impl MockBlockSource {
        fn with_blocks(blocks: Vec<BlockSnapshot>) -> Self {
            Self { blocks }
        }
    }

    impl BlockSource for MockBlockSource {
        fn block_snapshots(&self, _ctx: ContextId) -> Result<Vec<BlockSnapshot>, String> {
            Ok(self.blocks.clone())
        }
    }

    fn make_block(ctx: ContextId, seq: u64, content: &str) -> BlockSnapshot {
        let agent = PrincipalId::new();
        let id = BlockId::new(ctx, agent, seq);
        BlockSnapshot {
            id,
            parent_id: None,
            role: Role::Model,
            kind: BlockKind::Text,
            status: Status::Done,
            content: content.to_string(),
            ..BlockSnapshot::text(id, None, Role::Model, content)
        }
    }

    #[test]
    fn test_synthesis_basic() {
        let ctx = ContextId::new();
        let blocks = vec![
            make_block(ctx, 1, "Hello world, this is a test block with enough content for synthesis."),
            make_block(ctx, 2, "Another block about machine learning and neural networks for testing."),
            make_block(ctx, 3, "A third block discussing Rust programming and type systems."),
        ];
        let embedder = Arc::new(MockEmbedder { dims: 32 });
        let source = Arc::new(MockBlockSource::with_blocks(blocks));

        let result = run_synthesis(ctx, embedder, source).unwrap();
        assert!(!result.top_blocks.is_empty());
        assert!(result.top_blocks.len() <= 3);
        assert!(!result.keywords.is_empty());
        assert!(result.keywords.len() <= 8);

        // Scores should be between 0 and 1
        for (_, score) in &result.keywords {
            assert!(*score >= 0.0 && *score <= 1.01, "score out of range: {score}");
        }
    }

    #[test]
    fn test_synthesis_empty_context() {
        let ctx = ContextId::new();
        let embedder = Arc::new(MockEmbedder { dims: 32 });
        let source = Arc::new(MockBlockSource::with_blocks(Vec::new()));

        let result = run_synthesis(ctx, embedder, source);
        assert!(result.is_none());
    }

    #[test]
    fn test_synthesis_short_blocks_filtered() {
        let ctx = ContextId::new();
        let blocks = vec![
            make_block(ctx, 1, "short"),  // <= 10 chars, filtered
            make_block(ctx, 2, "This block has enough content to pass the filter threshold easily."),
        ];
        let embedder = Arc::new(MockEmbedder { dims: 32 });
        let source = Arc::new(MockBlockSource::with_blocks(blocks));

        let result = run_synthesis(ctx, embedder, source).unwrap();
        // Only one block should make it through
        assert_eq!(result.top_blocks.len(), 1);
    }

    #[test]
    fn test_synthesis_single_block() {
        let ctx = ContextId::new();
        let blocks = vec![
            make_block(ctx, 1, "A single block with enough content for synthesis to work properly."),
        ];
        let embedder = Arc::new(MockEmbedder { dims: 32 });
        let source = Arc::new(MockBlockSource::with_blocks(blocks));

        let result = run_synthesis(ctx, embedder, source).unwrap();
        assert_eq!(result.top_blocks.len(), 1);
        // Self-similarity should be ~1.0
        assert!(result.top_blocks[0].1 > 0.9);
    }

    #[test]
    fn test_synthesis_and_cache() {
        let ctx = ContextId::new();
        let blocks = vec![
            make_block(ctx, 1, "Enough content for the synthesis algorithm to process correctly."),
        ];
        let embedder = Arc::new(MockEmbedder { dims: 32 });
        let source = Arc::new(MockBlockSource::with_blocks(blocks));
        let cache = SynthesisCache::new();

        run_synthesis_and_cache(ctx, embedder, source, &cache);

        let cached = cache.get(ctx, None);
        assert!(cached.is_some());
    }
}
