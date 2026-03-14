// Synthesis script — extracts keywords and representative blocks for a context.
//
// Available functions:
//   embed(text) -> Array<f64>          — single text embedding
//   embed_batch(texts) -> Array<Array> — batch embedding
//   cosine_sim(a, b) -> f64            — cosine similarity
//   ngrams(text, min_n, max_n) -> Array<String>
//   centroid(embeddings) -> Array<f64> — mean + L2-normalize
//   top_k_by_score(items, k) -> Array  — sort by .score descending, truncate
//   context_blocks(ctx_id) -> Array<Map{id, role, kind, content}>
//
// Scope constants:
//   CONTEXT_ID — String (UUID of the context being synthesized)
//
// Return: #{ keywords: Array<String>, top_blocks: Array<Map> }

let blocks = context_blocks(CONTEXT_ID);
let text_blocks = blocks.filter(|b| b.kind == "text" && b.content.len() > 10);
if text_blocks.len() == 0 {
    return #{};
}

let texts = text_blocks.map(|b| b.content);
let embeds = embed_batch(texts);
if embeds.len() != texts.len() || embeds.len() == 0 {
    return #{};
}

let doc = centroid(embeds);

// Top-k representative blocks
let scored = [];
for i in range(0, texts.len()) {
    scored.push(#{
        id: text_blocks[i].id,
        score: cosine_sim(embeds[i], doc),
        preview: if texts[i].len() > 80 { texts[i].sub_string(0, 80) } else { texts[i] },
    });
}
let top_blocks = top_k_by_score(scored, 3);

// KeyBERT-style keyword extraction
let all_text = texts.reduce(|a, b| a + " " + b, "");
let candidates = ngrams(all_text, 1, 3);

// Cap candidate count to keep embed_batch reasonable
if candidates.len() > 50 {
    candidates = candidates.sub_list(0, 50);
}

if candidates.len() == 0 {
    return #{ keywords: [], top_blocks: top_blocks };
}

let cand_embeds = embed_batch(candidates);
if cand_embeds.len() != candidates.len() {
    return #{ keywords: [], top_blocks: top_blocks };
}

let kw = [];
for i in range(0, candidates.len()) {
    kw.push(#{ keyword: candidates[i], score: cosine_sim(cand_embeds[i], doc) });
}
let keywords = top_k_by_score(kw, 8);

#{ keywords: keywords.map(|k| k.keyword), top_blocks: top_blocks }
