# Semantic indexing and synthesis

Kaijutsu selects content, searches vectors, and computes synthesis. lfm2d
loads models and performs embedding inference. There is no builtin embedding
model or silent model fallback. Audio inference remains a separate subsystem.

## Embedding service

The kernel DB's singleton `embedding_config` selects the service:

| Field | Default | Meaning |
|---|---|---|
| enabled | 1 | Initialize semantic indexing at startup |
| endpoint | `http://lfm2d-1.taila4abc.ts.net:8088` | HTTP(S) root or `unix:///absolute/socket/path` |
| timeout_ms | 30000 | Request deadline, including queue time |
| max_in_flight | 2 | Maximum concurrent service requests per client |
| max_context_bytes | 2048 | UTF-8 byte budget of the search projection |

Startup discovers exactly one embedder from `GET /v1/models`. Its model id,
weight hash, and hidden size pin the client's vector space. `POST /embed`
uses document purpose for indexed text and all synthesis candidates, query
purpose for searches. Batches contain at most 32 inputs. lfm2d embeds each
input independently; changing that numerical contract requires a profile
change. Every response must carry matching model id and weight hash headers,
correct cardinality and dimensions, finite components, and a nonzero norm.
The adapter L2-normalizes accepted vectors before returning them. HTTP endpoints
must name the service root; path prefixes are rejected rather than discarded.

Service failures return errors and preserve prior synthesis. Failure to
connect at startup logs the problem and leaves the semantic index unavailable;
restart after repairing the endpoint. Discovery can delay startup by up to
`timeout_ms` (30 seconds by default) when the service is unreachable. `kj synth` and semantic search report unavailability; search does not return
an empty success when the index is absent.
There is no automatic reconnection or alternate model selection.

Opening an old builtin-model configuration migrates it to the default service,
preserves `enabled`, and converts its index budget from the old four-bytes-per-
token estimate. Model files and declared dimensions are no longer kernel
configuration: lfm2d owns them. Operator service settings survive later opens.

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
lfm2d does not yet advertise a full tokenizer/preprocessing profile digest;
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
