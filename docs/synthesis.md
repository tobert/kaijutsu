# Semantic indexing and synthesis

Kaijutsu selects content, searches vectors, and computes synthesis. An HTTP
embedding service loads models and performs embedding inference. There is no
builtin embedding model or silent model fallback. Audio inference remains a
separate subsystem.

## Embedding service

The kernel DB's singleton `embedding_config` selects the embedding model. A
fresh kernel has no row: the semantic index is off, boot logs "semantic index
off", and `kj synth` and semantic search report the index unavailable.

The row names a backend, a model, and a vector size. The backend is an
ordinary row (`kj backend`) of one of two kinds. Kind `openai` has a
`base_url` with an `/embeddings` route: OpenRouter, OpenAI, Google's Gemini API
through its OpenAI-compatible base URL, or a local server. Kind `mk` is a
council server that serves `POST /council/v2/embeddings`, such as nagaya's
EmbeddingGemma 2. The backend owns the URL and the key, so the key never sits
in this row. Configure it with the kernel stopped,
then start it:

```sh
kj backend set openrouter --kind openai --base-url https://openrouter.ai/api/v1 \
  --api-key-file ~/.openrouter-key
sqlite3 ~/.local/share/kaijutsu/kernel/kernel.db \
  "INSERT INTO embedding_config (id, enabled, backend, model, dimensions, profile,
                                 timeout_ms, max_in_flight, max_context_bytes)
   VALUES (1, 1, 'openrouter', 'qwen/qwen3-embedding-8b', 1024, 'qwen3', 30000, 2, 2048)
   ON CONFLICT(id) DO UPDATE SET backend = excluded.backend, model = excluded.model,
     dimensions = excluded.dimensions, profile = excluded.profile, enabled = 1"
```

| Field | Suggested | Meaning |
|---|---|---|
| enabled | 1 | Initialize semantic indexing at startup; 0 leaves the index off |
| backend | none | Name of an `openai` or `mk` kind backend; any other kind is an error at boot |
| model | none | The provider's model id, sent as `model` |
| dimensions | 1024 | Vector size, sent as `dimensions` and checked on every response |
| profile | `plain` | How queries and documents are prefixed: `plain`, `qwen3`, `embeddinggemma`. An `mk` backend takes only `plain` |
| timeout_ms | 30000 | Request deadline, including queue time |
| max_in_flight | 2 | Maximum concurrent requests |
| max_context_bytes | 2048 | UTF-8 byte budget of the search projection |

No `kj` verb writes this row yet; see
`docs/issues.md`, "Embedding service configuration has no `kj` verb".

The client is `kaijutsu_index::OpenAiEmbedder`, behind the `Embedder` trait.
It posts `{base_url}/embeddings` with `model`, `input` (at most 32 per
request), `dimensions` and `encoding_format: "float"`, and sends the key as a
bearer token. It puts the response in input order and refuses a response that
has the wrong count, a repeated or out-of-range index, the wrong size, a
non-finite component, or a zero vector. It L2-normalizes accepted vectors.
An HTTP error names the status and the start of the provider's message, never
the key.

A council server is reached by `kaijutsu_index::CouncilEmbedder`. Startup reads
`GET /council/v2/identity` and requires that the served model lists `embed`, is
ready, has the `query` and `document` tasks, and offers `dimensions`; the
refusal names what the server does offer. It then embeds one probe string and
pins the `identity` the response carries (`model`, `weight_hash`,
`tokenizer_hash`, `template`, `engine`). The cache identity covers a hash of
those fields, so new weights, a new tokenizer or a new task prompt re-index. A
later response with any other identity is refused. The client sends text and
the task name only: the server renders the task prompt and never cuts an
input, so an input past the model's context limit is an error naming the
limit (`413`, `context_tokens`), and nothing is embedded. A request carries at
most 32 inputs, fewer if the server's `inputs_per_request` is lower.

The profile is the model's own preparation, written by the client because a
provider's endpoint takes plain text: `qwen3` prefixes a query with an
instruction and leaves documents plain; `embeddinggemma` prefixes queries with
`task: search result | query: ` and documents with `title: none | text: `.
A server that adds the task prompt itself takes `plain`.

Startup embeds one probe string. A wrong key, an unknown model, or a model
that returns other than `dimensions` values fails there, and the index stays
off. The index attaches in the background once the probe succeeds; boot never
waits on it. A failure logs the cause and leaves the semantic index
unavailable; restart after repairing the row or the backend. `kj synth` and
semantic search report unavailability; search does not return an empty success
when the index is absent. There is no automatic reconnection or alternate
model selection.

A hosted `openai` provider can change a model's weights without notice, and
this client cannot see it. Its cache identity covers the model id, the
dimensions and the profile, so changing any of them re-indexes. Changing only a
provider's weights does not. A council server reports its weights, so it has no
such gap. Vectors from different models do not compare: change the
row, and the index rebuilds.

Opening a kernel DB drops an `embedding_config` row in an older shape (a
builtin model directory, or a service endpoint). The index is then off until an
operator names a backend and model.

## Cache identity and concurrency

Search indexes a truncated projection of terminal Text/Thinking blocks with
role prefixes. Synthesis retains its existing selection: non-File blocks with
more than ten UTF-8 bytes of content. Its hash covers the exact ordered block
ids and full content, the embedding profile, and a synthesis algorithm version.
The search hash cannot represent those larger, differently selected inputs.

`kj synth <context>` and `kj synth all` reuse unchanged synthesis, including
after restart. Add `--force` to recompute synthesis; unchanged search vectors
still reuse their index entry. Empty synthesis clears prior previews. Errors
in any embedding stage refuse the whole refresh rather than storing a partial
gist or keyword list. Bulk synthesis exits 1 when any context fails.

The global persisted embedding profile invalidates the HNSW graph and synthesis
on model changes, even when there are no indexed contexts. It currently covers
model id, weight hash, dimensions, and kaijutsu's purpose/normalization contract.
The service does not yet advertise a full tokenizer/preprocessing profile digest;
changing those without changing weights requires clearing the derived index.
Service contract feedback is recorded in exomemory.

Inference awaits occur outside SQLite and graph locks. Index writes compare
the saved input hash again before committing so a concurrent refresh cannot
overwrite one that committed while inference was running. Manual synthesis
refreshes serialize per index, reuse a predecessor's matching result, and
re-read input before storing; changed input produces an explicit retry error.
This final snapshot check is not atomic with subsequent context mutations.
Cached previews describe their recorded snapshot until a later refresh.

## Remaining incremental work

Automatic synthesis stays disabled. Changed contexts still embed all selected
blocks, then gist sentences and keyword candidates. Per-block embedding reuse,
coalescing changed refresh requests, and tighter revision-aware publication are
required before turning automatic synthesis back on. See `docs/issues.md`,
“Synthesis re-embeds the whole context on every block write”.
