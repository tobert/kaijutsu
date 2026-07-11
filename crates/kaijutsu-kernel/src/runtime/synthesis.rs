//! Synthesis runner — extracts keywords and representative blocks for a context.
//!
//! Replaces the Rhai-based synthesis script with a direct Rust implementation.
//! Uses the same `Embedder` and `BlockSource` traits from `kaijutsu-index`.

use std::sync::Arc;

use kaijutsu_index::synthesis::{
    SynthesisCache, SynthesisResult, best_sentence, centroid, cosine_similarity, extract_ngrams,
    split_sentences,
};
use kaijutsu_index::{BlockSource, Embedder};
use kaijutsu_types::{BlockSnapshot, ContextId};

/// How many of the top-scored blocks feed the block-head `top_blocks` preview
/// (unchanged from before the gist landed — the card face's fresher line is
/// the sentence-level `gist` below, this stays the coarse fallback).
const TOP_BLOCKS_FOR_PREVIEW: usize = 3;

/// How many of the top-scored blocks contribute *sentence* candidates to the
/// gist — wider net than the 3-block preview since a good sentence can live
/// in the 4th- or 5th-best block even when that block isn't the single best
/// match as a whole.
const GIST_TOP_BLOCKS: usize = 5;

/// Hard cap on sentence candidates sent to the embedder in one batch — bounds
/// embed cost regardless of how verbose the top-5 blocks are.
const GIST_MAX_CANDIDATES: usize = 64;

/// Max stored gist length (chars, not bytes — see `chars().take` below).
const GIST_MAX_CHARS: usize = 200;

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

    // 4. Score blocks by cosine similarity to centroid, best-first. Kept
    // un-truncated here so the gist (step 4b) can draw sentence candidates
    // from a wider top-K than the 3-block preview below.
    let mut scored_blocks: Vec<(usize, f32)> = embeds
        .iter()
        .enumerate()
        .map(|(i, emb)| (i, cosine_similarity(emb, &doc)))
        .collect();
    scored_blocks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let top_blocks: Vec<(String, f32, String)> = scored_blocks
        .iter()
        .take(TOP_BLOCKS_FOR_PREVIEW)
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

    // 4b. Sentence-level gist: extractive (the embedder can't generate prose),
    // scored the same way as everything else here — cosine similarity to the
    // doc centroid, just at sentence granularity instead of block granularity.
    let gist = compute_gist(&scored_blocks, &text_blocks, &doc, embedder.as_ref());

    // 5. Extract ngram candidates, cap at 50
    let all_text: String = texts.join(" ");
    let mut candidates = extract_ngrams(&all_text, 1, 3);
    candidates.truncate(50);

    if candidates.is_empty() {
        return Some(SynthesisResult {
            keywords: Vec::new(),
            top_blocks,
            gist,
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
                gist,
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
        gist = gist.is_some(),
        "synthesis complete"
    );

    Some(SynthesisResult {
        keywords: scored_kw,
        top_blocks,
        gist,
        content_hash: String::new(),
    })
}

/// Sentence-level extractive gist: split the top-`GIST_TOP_BLOCKS` scored
/// blocks' content into sentence candidates (capped at `GIST_MAX_CANDIDATES`
/// total to bound embed cost), embed them in one batch with the same
/// embedder, and keep the sentence closest to `doc_centroid`.
///
/// `None` on any soft failure — no sentence candidates, or the embed batch
/// didn't come back the right shape — callers fall back to the block-head
/// `top_blocks` preview.
fn compute_gist(
    scored_blocks: &[(usize, f32)],
    text_blocks: &[BlockSnapshot],
    doc_centroid: &[f32],
    embedder: &dyn Embedder,
) -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();
    'blocks: for (i, _) in scored_blocks.iter().take(GIST_TOP_BLOCKS) {
        for sentence in split_sentences(&text_blocks[*i].content) {
            candidates.push(sentence);
            if candidates.len() >= GIST_MAX_CANDIDATES {
                break 'blocks;
            }
        }
    }
    if candidates.is_empty() {
        return None;
    }

    let cand_refs: Vec<&str> = candidates.iter().map(|s| s.as_str()).collect();
    let embeds = match embedder.embed_batch(&cand_refs) {
        Ok(e) if e.len() == candidates.len() => e,
        _ => return None,
    };
    let embed_refs: Vec<&[f32]> = embeds.iter().map(|v| v.as_slice()).collect();
    let winner = best_sentence(&embed_refs, doc_centroid)?;
    let sentence = &candidates[winner];
    Some(if sentence.chars().count() > GIST_MAX_CHARS {
        sentence.chars().take(GIST_MAX_CHARS).collect()
    } else {
        sentence.clone()
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
    fn test_synthesis_includes_gist() {
        let ctx = ContextId::new();
        let blocks = vec![
            make_block(
                ctx,
                1,
                "This is the first sentence of the block. Here is a second sentence with different words.",
            ),
            make_block(
                ctx,
                2,
                "A completely unrelated block about neural networks and machine learning models.",
            ),
        ];
        let embedder = Arc::new(MockEmbedder { dims: 32 });
        let source = Arc::new(MockBlockSource::with_blocks(blocks));

        let result = run_synthesis(ctx, embedder, source).unwrap();
        let gist = result.gist.expect("sentence-worthy content should yield a gist");
        assert!(gist.chars().count() <= GIST_MAX_CHARS);
        assert!(!gist.is_empty());
    }

    #[test]
    fn test_synthesis_gist_none_without_sentence_candidates() {
        let ctx = ContextId::new();
        // Long enough to pass the block-length filter (> 10 chars) but every
        // "." fragment is under MIN_SENTENCE_CHARS once split.
        let blocks = vec![make_block(ctx, 1, "Hi. Ok. Go. Now. Yes. No. Sure. Meh.")];
        let embedder = Arc::new(MockEmbedder { dims: 32 });
        let source = Arc::new(MockBlockSource::with_blocks(blocks));

        let result = run_synthesis(ctx, embedder, source).unwrap();
        assert!(result.gist.is_none(), "no fragment clears MIN_SENTENCE_CHARS: {:?}", result.gist);
    }

    #[test]
    fn test_compute_gist_caps_at_200_chars() {
        // Direct unit test (not through the full pipeline) so the single
        // candidate deterministically wins regardless of the mock embedder's
        // hash-based scoring — isolates the truncation branch.
        let long_sentence = "word ".repeat(45); // 225 chars, under MAX_SENTENCE_CHARS (300)
        assert!(long_sentence.chars().count() > GIST_MAX_CHARS);
        let ctx = ContextId::new();
        let block = make_block(ctx, 1, &format!("{long_sentence}."));
        let embedder = MockEmbedder { dims: 16 };
        let doc = embedder.embed(&long_sentence).unwrap();
        let scored_blocks = vec![(0usize, 1.0f32)];
        let blocks = vec![block];

        let gist = compute_gist(&scored_blocks, &blocks, &doc, &embedder).expect("one candidate");
        assert_eq!(gist.chars().count(), GIST_MAX_CHARS, "225-char sentence truncated to exactly 200");
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
