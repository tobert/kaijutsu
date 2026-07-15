//! Ad hoc verification of `RtenEmbedder` against real on-disk ONNX exports.
//!
//! Not part of the test suite (real model files may not be present on a
//! given machine); run manually:
//!
//! ```text
//! cargo run -p kaijutsu-index --example embed_check [candidate.onnx ...]
//! ```
//!
//! Always tries the configured model dir
//! (`~/.local/share/kaijutsu/models/bge-small-en-v1.5`); any extra args are
//! candidate `.onnx` files, each staged in a temp dir with that model dir's
//! `tokenizer.json`. Each candidate is tried independently and reported — a
//! failure to load one does not stop the others.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use kaijutsu_index::{Embedder, RtenEmbedder};

const DIMS: usize = 384;
const MAX_TOKENS: usize = 512;

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb)
}

/// Build a temp dir containing `model.onnx` (copied from `onnx_path`) and
/// `tokenizer.json` (copied from `tokenizer_src_dir`), named `name` so the
/// embedder's model_name is legible in output.
fn stage_model_dir(tmp_root: &Path, name: &str, onnx_path: &Path, tokenizer_src_dir: &Path) -> PathBuf {
    let dir = tmp_root.join(name);
    fs::create_dir_all(&dir).expect("create staging dir");
    fs::copy(onnx_path, dir.join("model.onnx")).expect("copy model.onnx");
    fs::copy(
        tokenizer_src_dir.join("tokenizer.json"),
        dir.join("tokenizer.json"),
    )
    .expect("copy tokenizer.json");
    dir
}

fn try_model(label: &str, model_dir: &Path) {
    println!("\n=== {label} ({}) ===", model_dir.display());
    if !model_dir.join("model.onnx").exists() {
        println!("SKIP: {} not found", model_dir.join("model.onnx").display());
        return;
    }

    let load_start = Instant::now();
    let embedder = match RtenEmbedder::new(model_dir, DIMS, MAX_TOKENS) {
        Ok(e) => e,
        Err(e) => {
            println!("LOAD FAILED: {e}");
            return;
        }
    };
    println!("loaded in {:?}", load_start.elapsed());

    let related_a = "The cat sat on the warm windowsill in the afternoon sun.";
    let related_b = "A kitten curled up on the sunny window ledge.";
    let unrelated = "Quarterly revenue grew twelve percent on strong cloud demand.";

    let run_start = Instant::now();
    let embeddings = match embedder.embed_batch(&[related_a, related_b, unrelated]) {
        Ok(v) => v,
        Err(e) => {
            println!("EMBED FAILED: {e}");
            return;
        }
    };
    let elapsed = run_start.elapsed();
    println!("embedded batch of 3 in {elapsed:?} ({:?}/text)", elapsed / 3);

    let sim_related = cosine(&embeddings[0], &embeddings[1]);
    let sim_unrelated = cosine(&embeddings[0], &embeddings[2]);
    println!("cosine(related_a, related_b)   = {sim_related:.4}");
    println!("cosine(related_a, unrelated)   = {sim_unrelated:.4}");
    println!(
        "sane: {}",
        if sim_related > sim_unrelated {
            "YES (related > unrelated)"
        } else {
            "NO -- related <= unrelated!"
        }
    );
}

fn main() {
    let home = std::env::var("HOME").expect("HOME not set");
    let real_model_dir = PathBuf::from(&home).join(".local/share/kaijutsu/models/bge-small-en-v1.5");

    try_model("configured model dir", &real_model_dir);

    let tmp = tempfile::tempdir().expect("create tempdir");
    for (i, arg) in std::env::args().skip(1).enumerate() {
        let onnx = PathBuf::from(&arg);
        if !onnx.exists() {
            println!("\n=== {arg} ===\nSKIP: not found");
            continue;
        }
        let name = onnx
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("candidate-{i}"));
        let dir = stage_model_dir(tmp.path(), &name, &onnx, &real_model_dir);
        try_model(&name, &dir);
    }
}
