//! Context synthesis and content-addressed reuse of its results.

use std::sync::Arc;

use kaijutsu_index::synthesis::{
    SynthesisResult, best_sentence, centroid, cosine_similarity, extract_ngrams, split_sentences,
};
use kaijutsu_index::{BlockSource, Embedder, EmbeddingPurpose, IndexError, SemanticIndex};
use kaijutsu_types::{BlockKind, BlockSnapshot, ContextId};
use sha2::{Digest, Sha256};

const TOP_BLOCKS_FOR_PREVIEW: usize = 3;
const GIST_TOP_BLOCKS: usize = 5;
const GIST_MAX_CANDIDATES: usize = 64;
const GIST_MAX_CHARS: usize = 200;

fn selected_blocks(blocks: &[BlockSnapshot]) -> Vec<&BlockSnapshot> {
    blocks.iter().filter(|block| block.kind != BlockKind::File && block.content.len() > 10).collect()
}

/// Hash the exact ordered inputs, including ids used by the previews. The
/// indexing hash covers a shorter, differently filtered projection.
fn synthesis_hash(blocks: &[BlockSnapshot], embedder: &dyn Embedder) -> String {
    let input: Vec<_> = selected_blocks(blocks).into_iter().map(|block| (block.id, &block.content)).collect();
    let bytes = serde_json::to_vec(&("synthesis-v2", embedder.cache_identity(), input))
        .expect("synthesis input contains only ids and text");
    format!("{:x}", Sha256::digest(bytes))
}

async fn read_blocks(ctx: ContextId, source: Arc<dyn BlockSource>) -> Result<Vec<BlockSnapshot>, IndexError> {
    tokio::task::spawn_blocking(move || source.block_snapshots(ctx).map_err(IndexError::Index))
        .await.map_err(|e| IndexError::Index(format!("read synthesis blocks: {e}")))?
}

/// Compute synthesis from one snapshot. Service errors are errors, including
/// errors during gist and keyword generation; partial results are never cached.
pub async fn run_synthesis(
    ctx_id: ContextId,
    embedder: Arc<dyn Embedder>,
    block_source: Arc<dyn BlockSource>,
) -> Result<SynthesisResult, IndexError> {
    let blocks = read_blocks(ctx_id, block_source).await?;
    synthesize_blocks(&blocks, embedder.as_ref()).await
}

async fn embed_texts(embedder: &dyn Embedder, texts: &[&str]) -> Result<Vec<Vec<f32>>, IndexError> {
    let vectors = embedder.embed_batch(texts, EmbeddingPurpose::Document).await?;
    if vectors.len() != texts.len() || vectors.iter().any(|v| v.len() != embedder.dimensions()) {
        return Err(IndexError::Embedding("synthesis received an incomplete embedding batch".into()));
    }
    Ok(vectors)
}

async fn synthesize_blocks(blocks: &[BlockSnapshot], embedder: &dyn Embedder) -> Result<SynthesisResult, IndexError> {
    let content_hash = synthesis_hash(blocks, embedder);
    let text_blocks = selected_blocks(blocks);
    if text_blocks.is_empty() {
        return Ok(SynthesisResult { keywords: vec![], top_blocks: vec![], gist: None, content_hash });
    }
    let texts: Vec<&str> = text_blocks.iter().map(|block| block.content.as_str()).collect();
    let embeds = embed_texts(embedder, &texts).await?;
    let embed_refs: Vec<&[f32]> = embeds.iter().map(Vec::as_slice).collect();
    let doc = centroid(&embed_refs);
    let mut scored_blocks: Vec<_> = embeds.iter().enumerate()
        .map(|(i, emb)| (i, cosine_similarity(emb, &doc))).collect();
    scored_blocks.sort_by(|a, b| b.1.total_cmp(&a.1));
    let top_blocks = scored_blocks.iter().take(TOP_BLOCKS_FOR_PREVIEW).map(|(i, score)| {
        let snap = text_blocks[*i];
        (snap.id.to_string(), *score, snap.content.chars().take(80).collect())
    }).collect();
    let gist = compute_gist(&scored_blocks, &text_blocks, &doc, embedder).await?;

    let mut candidates = extract_ngrams(&texts.join(" "), 1, 3);
    candidates.truncate(50);
    let mut keywords = Vec::new();
    if !candidates.is_empty() {
        let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();
        let embeds = embed_texts(embedder, &refs).await?;
        keywords = candidates.into_iter().zip(embeds.iter())
            .map(|(kw, emb)| (kw, cosine_similarity(emb, &doc))).collect();
        keywords.sort_by(|a, b| b.1.total_cmp(&a.1));
        keywords.truncate(8);
    }
    Ok(SynthesisResult { keywords, top_blocks, gist, content_hash })
}

async fn compute_gist(
    scored_blocks: &[(usize, f32)],
    text_blocks: &[&BlockSnapshot],
    doc_centroid: &[f32],
    embedder: &dyn Embedder,
) -> Result<Option<String>, IndexError> {
    let candidates: Vec<_> = scored_blocks.iter().take(GIST_TOP_BLOCKS)
        .flat_map(|(i, _)| split_sentences(&text_blocks[*i].content))
        .take(GIST_MAX_CANDIDATES).collect();
    if candidates.is_empty() { return Ok(None); }
    let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();
    let embeds = embed_texts(embedder, &refs).await?;
    let refs: Vec<&[f32]> = embeds.iter().map(Vec::as_slice).collect();
    Ok(best_sentence(&refs, doc_centroid).map(|winner| candidates[winner].chars().take(GIST_MAX_CHARS).collect()))
}

/// Reuse synthesis only when its full input and embedding profile match.
/// A failed or superseded refresh leaves the prior cached result intact.
/// Empty snapshots store an empty result so old previews do not survive.
pub async fn run_synthesis_and_cache(
    ctx_id: ContextId,
    index: Arc<SemanticIndex>,
    block_source: Arc<dyn BlockSource>,
    force: bool,
) -> Result<SynthesisResult, IndexError> {
    let _refresh = index.synthesis_refresh().await;
    let blocks = read_blocks(ctx_id, block_source.clone()).await?;
    let embedder = index.embedder();
    let hash = synthesis_hash(&blocks, embedder);
    if !force && let Some(cached) = index.synthesis_cache().get(ctx_id, Some(&hash)) {
        return Ok(cached);
    }
    let result = synthesize_blocks(&blocks, embedder).await?;
    let current = read_blocks(ctx_id, block_source).await?;
    if synthesis_hash(&current, embedder) != hash {
        return Err(IndexError::Index("context changed during synthesis; retry with current blocks".into()));
    }
    let store = index.clone();
    let saved = result.clone();
    tokio::task::spawn_blocking(move || store.store_synthesis(ctx_id, saved))
        .await.map_err(|e| IndexError::Index(format!("store synthesis: {e}")))??;
    Ok(result)
}

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

    #[async_trait::async_trait]
    impl Embedder for MockEmbedder {
        fn revision(&self) -> &str { "mock-v1" }
        fn model_name(&self) -> &str {
            "mock"
        }
        fn dimensions(&self) -> usize {
            self.dims
        }
        async fn embed_batch(&self, texts: &[&str], purpose: EmbeddingPurpose) -> Result<Vec<Vec<f32>>, IndexError> {
            {
                let mut vectors = Vec::new();
                for text in texts { vectors.push(self.embed(text, purpose).await?); }
                Ok(vectors)
            }
        }
        async fn embed(&self, text: &str, _purpose: EmbeddingPurpose) -> Result<Vec<f32>, IndexError> {
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

    struct CountingEmbedder(Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait::async_trait]
    impl Embedder for CountingEmbedder {
        fn revision(&self) -> &str { "mock-v1" }
        fn model_name(&self) -> &str { "mock" }
        fn dimensions(&self) -> usize { 32 }
        async fn embed_batch(&self, texts: &[&str], purpose: EmbeddingPurpose) -> Result<Vec<Vec<f32>>, IndexError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            MockEmbedder { dims: 32 }.embed_batch(texts, purpose).await
        }
    }

    #[tokio::test]
    async fn unchanged_synthesis_does_no_embedding_work() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ContextId::new();
        let source = Arc::new(MockBlockSource::with_blocks(vec![
            make_block(ctx, 1, "This unchanged context must reuse its synthesis result."),
        ]));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let config = kaijutsu_index::IndexConfig::new(32, 512, dir.path());
        let index = Arc::new(SemanticIndex::new(config, Box::new(CountingEmbedder(calls.clone()))).unwrap());
        run_synthesis_and_cache(ctx, index.clone(), source.clone(), false).await.unwrap();
        let first = calls.load(std::sync::atomic::Ordering::SeqCst);
        assert!(first > 0, "first synthesis must exercise the embedder");
        run_synthesis_and_cache(ctx, index, source, false).await.unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), first);
    }

    struct MutableSource(std::sync::Mutex<Vec<BlockSnapshot>>);
    impl BlockSource for MutableSource {
        fn block_snapshots(&self, _ctx: ContextId) -> Result<Vec<BlockSnapshot>, String> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    #[tokio::test]
    async fn force_changed_suffix_empty_and_restart_follow_exact_inputs() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let ctx = ContextId::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let config = kaijutsu_index::IndexConfig::new(32, 32, dir.path());
        let index = Arc::new(SemanticIndex::new(config.clone(), Box::new(CountingEmbedder(calls.clone()))).unwrap());
        let source = Arc::new(MutableSource(std::sync::Mutex::new(vec![make_block(ctx, 1,
            "A common prefix that is longer than the indexing budget. Original ending.")])));
        let initial = run_synthesis_and_cache(ctx, index.clone(), source.clone(), false).await.unwrap();
        let n = calls.load(Ordering::SeqCst);
        run_synthesis_and_cache(ctx, index.clone(), source.clone(), true).await.unwrap();
        assert!(calls.load(Ordering::SeqCst) > n, "force must embed unchanged content");
        let before_projection = kaijutsu_index::extract_context_content(&source.0.lock().unwrap(), 32).1;
        source.0.lock().unwrap()[0].content.push_str(" Additional words beyond the search projection.");
        assert_eq!(kaijutsu_index::extract_context_content(&source.0.lock().unwrap(), 32).1, before_projection);
        let changed = run_synthesis_and_cache(ctx, index.clone(), source.clone(), false).await.unwrap();
        assert_ne!(initial.content_hash, changed.content_hash);
        drop(index);
        let index = Arc::new(SemanticIndex::new(config, Box::new(CountingEmbedder(calls.clone()))).unwrap());
        let n = calls.load(Ordering::SeqCst);
        run_synthesis_and_cache(ctx, index.clone(), source.clone(), false).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), n, "restart must reuse persisted synthesis");
        source.0.lock().unwrap().clear();
        let empty = run_synthesis_and_cache(ctx, index.clone(), source, false).await.unwrap();
        assert!(empty.top_blocks.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), n, "empty context needs no inference");
        assert!(index.synthesis_cache().get_any(ctx).unwrap().gist.is_none());
    }

    struct FailingEmbedder(Arc<std::sync::atomic::AtomicUsize>);
    #[async_trait::async_trait]
    impl Embedder for FailingEmbedder {
        fn model_name(&self) -> &str { "mock" }
        fn revision(&self) -> &str { "mock-v1" }
        fn dimensions(&self) -> usize { 32 }
        async fn embed_batch(&self, texts: &[&str], purpose: EmbeddingPurpose) -> Result<Vec<Vec<f32>>, IndexError> {
            assert_eq!(purpose, EmbeddingPurpose::Document);
            if self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                return Err(IndexError::Embedding("injected service failure".into()));
            }
            MockEmbedder { dims: 32 }.embed_batch(texts, purpose).await
        }
    }

    #[tokio::test]
    async fn partial_failures_preserve_prior_synthesis() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for fail_at in 1..=3 {
            let dir = tempfile::tempdir().unwrap();
            let ctx = ContextId::new();
            let fail = Arc::new(AtomicUsize::new(100));
            let index = Arc::new(SemanticIndex::new(
                kaijutsu_index::IndexConfig::new(32, 512, dir.path()), Box::new(FailingEmbedder(fail.clone())),
            ).unwrap());
            let source = Arc::new(MockBlockSource::with_blocks(vec![make_block(ctx, 1,
                "This sentence produces gist and keyword candidates for synthesis.")]));
            let good = run_synthesis_and_cache(ctx, index.clone(), source.clone(), false).await.unwrap();
            fail.store(fail_at, Ordering::SeqCst);
            assert!(run_synthesis_and_cache(ctx, index.clone(), source, true).await.is_err());
            assert_eq!(index.synthesis_cache().get_any(ctx).unwrap().content_hash, good.content_hash);
        }
    }

    struct ChangingSource {
        reads: std::sync::atomic::AtomicUsize,
        before: Vec<BlockSnapshot>, after: Vec<BlockSnapshot>,
    }
    impl BlockSource for ChangingSource {
        fn block_snapshots(&self, _ctx: ContextId) -> Result<Vec<BlockSnapshot>, String> {
            Ok(if self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                self.before.clone()
            } else { self.after.clone() })
        }
    }

    #[tokio::test]
    async fn superseded_synthesis_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ContextId::new();
        let index = test_index(dir.path());
        let source = Arc::new(ChangingSource {
            reads: std::sync::atomic::AtomicUsize::new(0),
            before: vec![make_block(ctx, 1, "Original content before a concurrent edit lands.")],
            after: vec![make_block(ctx, 1, "New content after a concurrent edit lands.")],
        });
        let error = run_synthesis_and_cache(ctx, index.clone(), source, false).await.unwrap_err();
        assert!(error.to_string().contains("context changed"));
        assert!(index.synthesis_cache().get_any(ctx).is_none());
    }

    #[tokio::test]
    async fn concurrent_unchanged_refreshes_share_computation() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let ctx = ContextId::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let index = Arc::new(SemanticIndex::new(kaijutsu_index::IndexConfig::new(32, 512, dir.path()),
            Box::new(CountingEmbedder(calls.clone()))).unwrap());
        let source = Arc::new(MockBlockSource::with_blocks(vec![make_block(ctx, 1,
            "This sentence produces gist and keyword candidates for synthesis.")]));
        let (a, b) = tokio::join!(
            run_synthesis_and_cache(ctx, index.clone(), source.clone(), false),
            run_synthesis_and_cache(ctx, index.clone(), source.clone(), false),
        );
        assert_eq!(a.unwrap().content_hash, b.unwrap().content_hash);
        let n = calls.load(Ordering::SeqCst);
        run_synthesis_and_cache(ctx, index, source, true).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), n * 2, "concurrent calls should cost one computation");
    }

    #[tokio::test]
    async fn synthesis_preview_preserves_utf8() {
        let ctx = ContextId::new();
        let source = Arc::new(MockBlockSource::with_blocks(vec![
            make_block(ctx, 1, &"日本語の文章です。".repeat(20)),
        ]));
        let result = run_synthesis(ctx, Arc::new(MockEmbedder { dims: 32 }), source).await.unwrap();
        assert!(!result.top_blocks[0].2.is_empty());
        assert!(result.top_blocks[0].2.chars().count() <= 80);
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

    #[tokio::test]
    async fn test_synthesis_basic() {
        let ctx = ContextId::new();
        let blocks = vec![
            make_block(ctx, 1, "Hello world, this is a test block with enough content for synthesis."),
            make_block(ctx, 2, "Another block about machine learning and neural networks for testing."),
            make_block(ctx, 3, "A third block discussing Rust programming and type systems."),
        ];
        let embedder = Arc::new(MockEmbedder { dims: 32 });
        let source = Arc::new(MockBlockSource::with_blocks(blocks));

        let result = run_synthesis(ctx, embedder, source).await.unwrap();
        assert!(!result.top_blocks.is_empty());
        assert!(result.top_blocks.len() <= 3);
        assert!(!result.keywords.is_empty());
        assert!(result.keywords.len() <= 8);

        // Scores should be between 0 and 1
        for (_, score) in &result.keywords {
            assert!(*score >= 0.0 && *score <= 1.01, "score out of range: {score}");
        }
    }

    #[tokio::test]
    async fn test_synthesis_empty_context() {
        let ctx = ContextId::new();
        let embedder = Arc::new(MockEmbedder { dims: 32 });
        let source = Arc::new(MockBlockSource::with_blocks(Vec::new()));

        let result = run_synthesis(ctx, embedder, source).await.unwrap();
        assert!(result.top_blocks.is_empty());
    }

    #[tokio::test]
    async fn test_synthesis_short_blocks_filtered() {
        let ctx = ContextId::new();
        let blocks = vec![
            make_block(ctx, 1, "short"),  // <= 10 chars, filtered
            make_block(ctx, 2, "This block has enough content to pass the filter threshold easily."),
        ];
        let embedder = Arc::new(MockEmbedder { dims: 32 });
        let source = Arc::new(MockBlockSource::with_blocks(blocks));

        let result = run_synthesis(ctx, embedder, source).await.unwrap();
        // Only one block should make it through
        assert_eq!(result.top_blocks.len(), 1);
    }

    #[tokio::test]
    async fn test_synthesis_single_block() {
        let ctx = ContextId::new();
        let blocks = vec![
            make_block(ctx, 1, "A single block with enough content for synthesis to work properly."),
        ];
        let embedder = Arc::new(MockEmbedder { dims: 32 });
        let source = Arc::new(MockBlockSource::with_blocks(blocks));

        let result = run_synthesis(ctx, embedder, source).await.unwrap();
        assert_eq!(result.top_blocks.len(), 1);
        // Self-similarity should be ~1.0
        assert!(result.top_blocks[0].1 > 0.9);
    }

    #[tokio::test]
    async fn test_synthesis_includes_gist() {
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

        let result = run_synthesis(ctx, embedder, source).await.unwrap();
        let gist = result.gist.expect("sentence-worthy content should yield a gist");
        assert!(gist.chars().count() <= GIST_MAX_CHARS);
        assert!(!gist.is_empty());
    }

    #[tokio::test]
    async fn test_synthesis_gist_none_without_sentence_candidates() {
        let ctx = ContextId::new();
        // Long enough to pass the block-length filter (> 10 chars) but every
        // "." fragment is under MIN_SENTENCE_CHARS once split.
        let blocks = vec![make_block(ctx, 1, "Hi. Ok. Go. Now. Yes. No. Sure. Meh.")];
        let embedder = Arc::new(MockEmbedder { dims: 32 });
        let source = Arc::new(MockBlockSource::with_blocks(blocks));

        let result = run_synthesis(ctx, embedder, source).await.unwrap();
        assert!(result.gist.is_none(), "no fragment clears MIN_SENTENCE_CHARS: {:?}", result.gist);
    }

    #[tokio::test]
    async fn test_compute_gist_caps_at_200_chars() {
        // Direct unit test (not through the full pipeline) so the single
        // candidate deterministically wins regardless of the mock embedder's
        // hash-based scoring — isolates the truncation branch.
        let long_sentence = "word ".repeat(45); // 225 chars, under MAX_SENTENCE_CHARS (300)
        assert!(long_sentence.chars().count() > GIST_MAX_CHARS);
        let ctx = ContextId::new();
        let block = make_block(ctx, 1, &format!("{long_sentence}."));
        let embedder = MockEmbedder { dims: 16 };
        let doc = embedder.embed(&long_sentence, EmbeddingPurpose::Document).await.unwrap();
        let scored_blocks = vec![(0usize, 1.0f32)];
        let blocks = vec![&block];

        let gist = compute_gist(&scored_blocks, &blocks, &doc, &embedder).await.unwrap().expect("one candidate");
        assert_eq!(gist.chars().count(), GIST_MAX_CHARS, "225-char sentence truncated to exactly 200");
    }

    /// `run_synthesis_and_cache` now persists through a real `SemanticIndex`
    /// (DB + memory cache), not a bare `SynthesisCache` — same MockEmbedder
    /// pattern used by kaijutsu-index's own tests, over a TempDir.
    fn test_index(dir: &std::path::Path) -> Arc<kaijutsu_index::SemanticIndex> {
        let config = kaijutsu_index::IndexConfig {
            dimensions: 32,
            data_dir: dir.to_path_buf(),
            hnsw_max_nb_connection: 8,
            hnsw_ef_construction: 50,
            max_context_bytes: 512,
            max_contexts: None,
        };
        Arc::new(kaijutsu_index::SemanticIndex::new(config, Box::new(MockEmbedder { dims: 32 })).unwrap())
    }

    #[tokio::test]
    async fn test_synthesis_and_cache() {
        let dir = tempfile::TempDir::new().unwrap();
        let index = test_index(dir.path());

        let ctx = ContextId::new();
        let blocks = vec![
            make_block(ctx, 1, "Enough content for the synthesis algorithm to process correctly."),
        ];
        let source = Arc::new(MockBlockSource::with_blocks(blocks));

        run_synthesis_and_cache(ctx, index.clone(), source, false).await.unwrap();

        let cached = index.synthesis_cache().get(ctx, None);
        assert!(cached.is_some(), "result should land in the index's synthesis cache");
    }

    #[tokio::test]
    async fn test_synthesis_and_cache_persists_to_disk() {
        // The point of run_synthesis_and_cache going through SemanticIndex
        // instead of a bare SynthesisCache: the result must survive a reopen.
        let dir = tempfile::TempDir::new().unwrap();
        let ctx = ContextId::new();
        let blocks = vec![make_block(
            ctx,
            1,
            "Enough content for the synthesis algorithm to process correctly.",
        )];

        {
            let index = test_index(dir.path());
            let source = Arc::new(MockBlockSource::with_blocks(blocks));
            run_synthesis_and_cache(ctx, index.clone(), source, false).await.unwrap();
            assert!(index.synthesis_cache().get_any(ctx).is_some());
        }

        let reopened = test_index(dir.path());
        assert!(
            reopened.synthesis_cache().get_any(ctx).is_some(),
            "synthesis result should be hydrated from disk on reopen"
        );
    }
}
