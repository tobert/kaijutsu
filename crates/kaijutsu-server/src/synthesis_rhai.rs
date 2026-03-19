//! Rhai function registration for synthesis scripts.
//!
//! Lives in the server crate (has access to both `rhai` and `kaijutsu-index`).
//! All functions return native Rhai types (String, f64, Array, Map) — no CustomType.

use std::sync::Arc;

use kaijutsu_index::{BlockSource, Embedder};
use kaijutsu_types::ContextId;

/// Register synthesis functions on a Rhai engine.
///
/// Seven functions: embed, embed_batch, cosine_sim, ngrams, centroid,
/// top_k_by_score, context_blocks.
pub fn register_synthesis_fns(
    engine: &mut rhai::Engine,
    embedder: Arc<dyn Embedder>,
    block_source: Arc<dyn BlockSource>,
) {
    // embed(text: String) -> Array<f64>
    let emb = embedder.clone();
    engine.register_fn("embed", move |text: String| -> rhai::Array {
        match emb.embed(&text) {
            Ok(v) => v
                .into_iter()
                .map(|f| rhai::Dynamic::from(f as f64))
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "embed() failed");
                rhai::Array::new()
            }
        }
    });

    // embed_batch(texts: Array<String>) -> Array<Array<f64>>
    let emb = embedder.clone();
    engine.register_fn("embed_batch", move |texts: rhai::Array| -> rhai::Array {
        let strs: Vec<String> = texts
            .into_iter()
            .map(|d| d.into_string().unwrap_or_default())
            .collect();
        let refs: Vec<&str> = strs.iter().map(|s| s.as_str()).collect();

        match emb.embed_batch(&refs) {
            Ok(vecs) => vecs
                .into_iter()
                .map(|v| {
                    let inner: rhai::Array = v
                        .into_iter()
                        .map(|f| rhai::Dynamic::from(f as f64))
                        .collect();
                    rhai::Dynamic::from(inner)
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "embed_batch() failed");
                rhai::Array::new()
            }
        }
    });

    // cosine_sim(a: Array<f64>, b: Array<f64>) -> f64
    engine.register_fn("cosine_sim", |a: rhai::Array, b: rhai::Array| -> f64 {
        let fa: Vec<f32> = a
            .into_iter()
            .map(|d| d.as_float().unwrap_or(0.0) as f32)
            .collect();
        let fb: Vec<f32> = b
            .into_iter()
            .map(|d| d.as_float().unwrap_or(0.0) as f32)
            .collect();
        kaijutsu_index::synthesis::cosine_similarity(&fa, &fb) as f64
    });

    // ngrams(text: String, min_n: i64, max_n: i64) -> Array<String>
    engine.register_fn(
        "ngrams",
        |text: String, min_n: i64, max_n: i64| -> rhai::Array {
            kaijutsu_index::synthesis::extract_ngrams(&text, min_n as usize, max_n as usize)
                .into_iter()
                .map(rhai::Dynamic::from)
                .collect()
        },
    );

    // centroid(embeddings: Array<Array<f64>>) -> Array<f64>
    engine.register_fn("centroid", |embeddings: rhai::Array| -> rhai::Array {
        let vecs: Vec<Vec<f32>> = embeddings
            .into_iter()
            .map(|d| {
                d.into_array()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|v| v.as_float().unwrap_or(0.0) as f32)
                    .collect()
            })
            .collect();
        let refs: Vec<&[f32]> = vecs.iter().map(|v| v.as_slice()).collect();
        kaijutsu_index::synthesis::centroid(&refs)
            .into_iter()
            .map(|f| rhai::Dynamic::from(f as f64))
            .collect()
    });

    // top_k_by_score(items: Array<Map>, k: i64) -> Array<Map>
    // Expects maps with a `score` key (f64). Sorts descending, truncates to k.
    engine.register_fn(
        "top_k_by_score",
        |mut items: rhai::Array, k: i64| -> rhai::Array {
            items.sort_by(|a, b| {
                let sa = a
                    .clone()
                    .try_cast::<rhai::Map>()
                    .and_then(|m| m.get("score").and_then(|v| v.as_float().ok()))
                    .unwrap_or(0.0);
                let sb = b
                    .clone()
                    .try_cast::<rhai::Map>()
                    .and_then(|m| m.get("score").and_then(|v| v.as_float().ok()))
                    .unwrap_or(0.0);
                sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
            });
            items.truncate(k.max(0) as usize);
            items
        },
    );

    // context_blocks(ctx_id: String) -> Array<Map>
    // Returns [{id: String, role: String, kind: String, content: String}]
    let src = block_source.clone();
    engine.register_fn("context_blocks", move |ctx_id_str: String| -> rhai::Array {
        let ctx_id = match ContextId::parse(&ctx_id_str) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(error = %e, ctx = %ctx_id_str, "context_blocks: invalid id");
                return rhai::Array::new();
            }
        };

        match src.block_snapshots(ctx_id) {
            Ok(snaps) => snaps
                .into_iter()
                .map(|snap| {
                    let mut map = rhai::Map::new();
                    map.insert("id".into(), rhai::Dynamic::from(snap.id.to_string()));
                    map.insert("role".into(), rhai::Dynamic::from(snap.role.to_string()));
                    map.insert("kind".into(), rhai::Dynamic::from(snap.kind.to_string()));
                    map.insert("content".into(), rhai::Dynamic::from(snap.content.clone()));
                    rhai::Dynamic::from(map)
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, ctx = %ctx_id_str, "context_blocks: fetch failed");
                rhai::Array::new()
            }
        }
    });
}

/// Create a Rhai engine configured for synthesis scripts.
///
/// Registers stdlib + synthesis functions. Sets safety limits.
pub fn create_synthesis_engine(
    embedder: Arc<dyn Embedder>,
    block_source: Arc<dyn BlockSource>,
) -> rhai::Engine {
    let mut engine = rhai::Engine::new();

    // Safety limits
    engine.set_max_expr_depths(64, 64);
    engine.set_max_operations(10_000_000);
    engine.set_max_string_size(1_000_000);
    engine.set_max_array_size(10_000);
    engine.set_max_map_size(1_000);

    kaijutsu_rhai::register_stdlib(&mut engine);
    register_synthesis_fns(&mut engine, embedder, block_source);

    engine
}

// ============================================================================
// Synthesis runner
// ============================================================================

/// Default synthesis script, embedded at compile time.
const DEFAULT_SYNTHESIS_RHAI: &str = include_str!("../../../assets/defaults/synthesis.rhai");

/// Run synthesis for a context: evaluate Rhai script, return result.
///
/// Blocking operation (Rhai eval + ONNX embed calls) — call from `spawn_blocking`.
/// Returns `None` on script errors or empty results.
pub fn run_synthesis(
    ctx_id: ContextId,
    embedder: Arc<dyn Embedder>,
    block_source: Arc<dyn BlockSource>,
) -> Option<kaijutsu_index::synthesis::SynthesisResult> {
    let script = load_synthesis_script();
    let engine = create_synthesis_engine(embedder, block_source);

    let mut scope = rhai::Scope::new();
    scope.push_constant("CONTEXT_ID", ctx_id.to_string());

    let ast = match engine.compile(&script) {
        Ok(ast) => ast,
        Err(e) => {
            tracing::warn!(error = %e, "synthesis.rhai compile error");
            return None;
        }
    };

    let result = match engine.eval_ast_with_scope::<rhai::Dynamic>(&mut scope, &ast) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, ctx = %ctx_id.short(), "synthesis.rhai eval error");
            return None;
        }
    };

    let map = match result.try_cast::<rhai::Map>() {
        Some(m) if !m.is_empty() => m,
        _ => {
            tracing::debug!(ctx = %ctx_id.short(), "synthesis.rhai returned empty/non-map");
            return None;
        }
    };

    let keywords: Vec<(String, f32)> = map
        .get("keywords")
        .and_then(|v| v.clone().into_array().ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|d| {
            if let Ok(s) = d.clone().into_string() {
                Some((s, 0.0))
            } else if let Some(m) = d.try_cast::<rhai::Map>() {
                let kw = m.get("keyword")?.clone().into_string().ok()?;
                let score = m
                    .get("score")
                    .and_then(|v| v.as_float().ok())
                    .unwrap_or(0.0) as f32;
                Some((kw, score))
            } else {
                None
            }
        })
        .collect();

    let top_blocks: Vec<(String, f32, String)> = map
        .get("top_blocks")
        .and_then(|v| v.clone().into_array().ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|d| {
            let m = d.try_cast::<rhai::Map>()?;
            let id = m.get("id")?.clone().into_string().ok()?;
            let score = m
                .get("score")
                .and_then(|v| v.as_float().ok())
                .unwrap_or(0.0) as f32;
            let preview = m
                .get("preview")
                .and_then(|v| v.clone().into_string().ok())
                .unwrap_or_default();
            Some((id, score, preview))
        })
        .collect();

    let synth = kaijutsu_index::synthesis::SynthesisResult {
        keywords,
        top_blocks,
        content_hash: String::new(),
    };

    tracing::debug!(
        ctx = %ctx_id.short(),
        keywords = synth.keywords.len(),
        blocks = synth.top_blocks.len(),
        "synthesis complete"
    );

    Some(synth)
}

/// Run synthesis and cache the result. Convenience wrapper for the watcher callback.
pub fn run_synthesis_and_cache(
    ctx_id: ContextId,
    embedder: Arc<dyn Embedder>,
    block_source: Arc<dyn BlockSource>,
    cache: &kaijutsu_index::synthesis::SynthesisCache,
) {
    if let Some(synth) = run_synthesis(ctx_id, embedder, block_source) {
        cache.insert(ctx_id, synth);
    }
}

/// Load synthesis.rhai from user config dir, falling back to embedded default.
pub fn load_synthesis_script() -> String {
    if let Some(config_dir) = dirs_config_path() {
        let user_script = config_dir.join("synthesis.rhai");
        if user_script.exists() {
            match std::fs::read_to_string(&user_script) {
                Ok(s) => return s,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        path = %user_script.display(),
                        "failed to read user synthesis.rhai, using default"
                    );
                }
            }
        }
    }
    DEFAULT_SYNTHESIS_RHAI.to_string()
}

/// Get the kaijutsu config directory (~/.config/kaijutsu).
fn dirs_config_path() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .map(|d| d.join("kaijutsu"))
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

    /// Deterministic mock embedder for testing (same as in kaijutsu-index).
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

    struct MockBlockSource;
    impl BlockSource for MockBlockSource {
        fn block_snapshots(&self, ctx: ContextId) -> Result<Vec<BlockSnapshot>, String> {
            let agent = PrincipalId::new();
            let id = BlockId::new(ctx, agent, 1);
            Ok(vec![BlockSnapshot {
                id,
                parent_id: None,
                role: Role::Model,
                kind: BlockKind::Text,
                status: Status::Done,
                content: "Hello world, this is a test block with enough content.".to_string(),
                ..BlockSnapshot::text(
                    id,
                    None,
                    Role::Model,
                    "Hello world, this is a test block with enough content.",
                )
            }])
        }
    }

    fn test_engine() -> rhai::Engine {
        create_synthesis_engine(
            Arc::new(MockEmbedder { dims: 32 }),
            Arc::new(MockBlockSource),
        )
    }

    #[test]
    fn test_embed_returns_array() {
        let engine = test_engine();
        let result: rhai::Array = engine.eval(r#"embed("hello world")"#).unwrap();
        assert_eq!(result.len(), 32);
        // All elements should be f64
        for v in &result {
            assert!(v.as_float().is_ok(), "embed element should be f64");
        }
    }

    #[test]
    fn test_embed_batch_returns_nested_array() {
        let engine = test_engine();
        let result: rhai::Array = engine.eval(r#"embed_batch(["hello", "world"])"#).unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_cosine_sim_identical() {
        let engine = test_engine();
        let result: f64 = engine
            .eval(r#"let v = embed("test"); cosine_sim(v, v)"#)
            .unwrap();
        assert!(
            (result - 1.0).abs() < 1e-5,
            "self-similarity should be ~1.0, got {result}"
        );
    }

    #[test]
    fn test_ngrams_from_rhai() {
        let engine = test_engine();
        let result: rhai::Array = engine
            .eval(r#"ngrams("the quick brown fox", 1, 2)"#)
            .unwrap();
        let strs: Vec<String> = result
            .into_iter()
            .filter_map(|d| d.into_string().ok())
            .collect();
        assert!(strs.contains(&"the".to_string()));
        assert!(strs.contains(&"the quick".to_string()));
    }

    #[test]
    fn test_centroid_from_rhai() {
        let engine = test_engine();
        let result: rhai::Array = engine
            .eval(r#"let e = embed_batch(["hello", "world"]); centroid(e)"#)
            .unwrap();
        assert_eq!(result.len(), 32);
    }

    #[test]
    fn test_top_k_by_score() {
        let engine = test_engine();
        let result: rhai::Array = engine
            .eval(
                r#"
                let items = [
                    #{ name: "a", score: 0.3 },
                    #{ name: "b", score: 0.9 },
                    #{ name: "c", score: 0.5 },
                ];
                top_k_by_score(items, 2)
            "#,
            )
            .unwrap();
        assert_eq!(result.len(), 2);
        // First should be "b" (highest score)
        let first = result[0].clone().try_cast::<rhai::Map>().unwrap();
        assert_eq!(
            first.get("name").unwrap().clone().into_string().unwrap(),
            "b"
        );
    }
}
