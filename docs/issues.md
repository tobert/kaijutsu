# Open Issues

Live work items distilled from prior design and TODO docs, plus architectural observations from code reviews. Code is truth; this exists to track what's *not* in the code yet.

Organized by area. Keep entries terse — link to file:line when a pointer makes the work concrete. When an item ships, delete the entry — if the "how we got here" is worth keeping, move the narrative to [`devlog.md`](devlog.md). See `AGENTS.md`, "Writing, memory, and git".

---

## Approval UX and `gate.toml` ergonomics (2026-11-24)

- **Frequent `'t a'` approvals in interactive use.** Amy is hitting too many manual `'t a'` approvals during normal banto/senpai/kohai work; needs a design discussion on how to reduce approval friction (e.g., broader safe defaults, scoped standing rules, or board/handoff scoping across senpai/kohai) without weakening the turn-identity / auth model (`docs/approval-identity.md`).

---

## Boards: what Slices 1–3 leave open (2026-11-22)

From DeepSeek Kaibo reviews and the Slices 1–3 build (`docs/board.md`):

- **Slice 4 (`kj handoff` migration).** Intentionally deferred until `kj board` has live mileage: route `<char>/handoff` through boards, update `S16-handoff.kai`, move continuation close to `kj context signoff`, migrate `handoff_ctx` rows, and delete `kj/handoff.rs`.
- **Two-step topic creation can leave an orphan `DocKind::Topic` document** if concurrent posts collide on `idx_board_topics_slug` or if `open_board_post_ask` fails after `create_document_with_held_block` (`kj/board.rs`). Fix: wrap document + topic insert or delete the document on error.
- **Plain `kj context archive <senpai>` archives the context board** when no other live context holds `board_id`, even if live kohai still address it via `.senpai` (rotation is safe because the successor receives `board_id` first).
- **`kj board read --include-hidden` and `kj ledger list` previews** show pending held posts to non-moderators (`kj/board.rs`); gate `--include-hidden` or split `--include-held` for moderators/authors if stricter visibility is wanted.
- **TUI / App board picker** (Phase 2 of `docs/board.md`, "Human perusal and future clients"): topics are `BlockStore` documents, so adding a picker entry unlocks existing rendering, folds, and copy mode.

## App layout follow-ups (2026-11-23)

- **Neo rich workspace and temporal scene.** The visible workspace now
  indexes recent artifacts and events and reveals their source blocks.
  Independent rich artifact surfaces need a rendering input boundary:
  current panes share the focused `MainCell`, geometry, and scroll. A temporal
  3D scene and particle flows remain design work. Keep history still while
  idle; recorded creation time is not an execution duration or scheduled target.
- **Narrow dock labels collide.** At 640px the header and footer labels overlap
  even though the conversation and Neo panels fit. Give dock labels a width
  budget and elide lower-priority status before they cover navigation hints.
- **Pending ask receipt status.** The desktop's open `shell_write` ask
  `01a20b2c` had a receipt displayed as `Error` after app restart, while the
  ask remained pending. Audit the durable status and server repair path;
  `runtime/command.rs` already tests replacement of a `Waiting` placeholder.
  The app must use the live ledger association for waiting presentation.
- **Waiting attention.** Amy wants waiting on approval or execution to be
  able to remain animated while blocked, distinct from finite error
  attention. Deferred until the ledger and execution states can drive the
  treatment consistently across captions and panels.
- **Border glow does not follow border opacity.** In
  `assets/shaders/surface_chrome.wgsl`, edge-glow RGB omits the configured
  color alpha while output alpha includes it, despite the documented
  premultiplied blend contract. The legacy `block_fx.wesl` uses the same
  formula. Low-opacity borders can therefore retain bright glow. Decide
  whether glow is deliberately emissive, then verify opacity sweeps in the
  rendered conversation before changing the blend.
- **Dock heights shrink with the content area.** North and South request
  40px and 32px but retain default flex shrink. In a 1280×720 live preview,
  the conversation received 655px instead of 648px. Check fixed dock
  heights and content-area sizing together before changing the vertical
  layout policy.

## Heroic view follow-ups (2026-11-21)

- Live kernel work updates still poll `kj transport work --track <id>` at 1 Hz via `execute_kj_quiet` (`view/heroic/live.rs`); a dedicated work-status wire event + measured performer lead field remains future work (`docs/newui.md`, "The live driver").
- Audio spectrum hills currently derive 16-band energy from beat crossings and `RenderCue` onsets rather than FFT of mixed audio frames.
- Heroic demo/test scenarios should be driven via a kernel/fixture script that creates and plays real tracks over RPC rather than a client-side fake driver (`view/heroic/fake.rs` removed).
- Council `/decide` turns currently emit only the final chosen ABC text block without persisting per-slot pooled option probability distributions on the block or `WorkStatus`, so live 3D candidate probability bars (`HeroicPerformer.candidates`) remain unpopulated until decision weights cross the wire.

## Shell output a model reads: what the preview-limit change leaves open (2026-11-19)

- **kaish `--json` prints the output tree, not `.data`.** `seq 1 3 --json` prints `["1","2","3"]` while its `.data` is `[1,2,3]` (`apply_output_format` in kaish-types prefers `.output` over `.data`). A kaish change, under its compatibility promises.
- **`[data]` is chosen by content.** `ShellEnvelope::prints_data` leaves it out when stdout is `data` as JSON or prints its scalars one per line; anything else still sends it. A `kj` verb whose printed text carries what the model needs (`kj vi`, `kj block append` keep the buffer and length only in `data`) could print it and drop the line.
- **The MCP shell tool still sends the whole envelope** (stdout and `data` both) to MCP clients such as Claude Code (`kaijutsu-mcp`).
- **`kj help` is budgeted against the default limit** (`HELP_BUDGET_BYTES` in `kj/mod.rs`), so a model with `--preview-kib` under 8 has its `kj help` cut.
- **stdout and stderr together can pass the broker's 64 KiB shell budget** even at 40 KiB, and the broker then cuts the envelope with no `/v/cas` copy. `--preview-kib` stops at 48 for the same reason.
- **No doc owns the shell result contract** since the consolidation removed `docs/shell-envelope.md`; it lives in `ShellEnvelope::model_text` and the shell tool description. About 2,000 code comments still point at docs the consolidation (44a0a8cc, 46e12db4) removed or folded (e.g., `docs/timewell.md`); only the shell output and heroic/theme paths were cleaned up.

## Context shadows: what the description slice leaves open

- **No RPC and no asynchronous delivery.** `kj shadow describe` waits up to `DESCRIBE_TIMEOUT` (120 seconds) and prints the description. The app and tui need a request that returns at once, and a rendition that arrives through the change feed when it is ready (`docs/contexts.md`).
- **A shadow keeps the `image` slot it was made with.** A character's later `kj character slot set <name> image ...` reaches a new shadow only after the old one is archived. The maker in the index is the shadow's own slot, so the key stays truthful. A rotation of the source does not yet carry its description shadow to the successor (`docs/contexts.md`).
- **Simultaneous misses both call the model.** Two requests for one key that both miss the index each pay for a description. The second one is linked and not indexed, so its shadow holds a rendition the key does not name.
- **A failed commit stops the shadow's document.** A rendition whose link or index row cannot be written fails the request and leaves no block, and the shadow's document refuses writes until the kernel restarts (the rule of `BlockStore::append_block_recorded`).
- **A reused copy names no original.** The copy has its own link to its source block and no row that names the rendition it was copied from. The index keeps naming the original rendition after its shadow is archived; it still serves, since archive keeps blocks. If the indexed rendition block itself is deleted (`BlockStore::delete_block` / document deletion), `rendition_text` fails rather than recomputing because `shadow_renditions`/`rendition_index` are not pruned on block deletion.
- **Shadows assume the store's default workspace.** `create_document` writes the store's `default_workspace_id` while `shadow_row` copies `seat.workspace_id`, so creating a shadow for a context moved to a non-default workspace triggers `DocumentDiverged` (`council/shadow.rs:547-557`).
- **Static-allowed gate calls still prime the judge.** `council::shadow::record_call` spawns `spawn_prime` before `council::gate` consults the static rules, so calls allowed without judging still send a `POST /council/v2/contexts` prime.
- **Presets and spans have no writer.** `preset` in `rendition_index` is empty, and `span_start` and `span_end` are NULL.
- **`kj shadow describe` reads `image/*` only;** Claude and the OpenAI-compatible providers send images only (`video` and `audio` slot routing works on turns for Gemini and council backends). Media-role slots also enforce `requires_slot_output_limit` when copying onto every newly created context (`context_characters.rs:472`), even text-only turns that never invoke `kj shadow describe`.
- **The turn path guards one kind.** `chat_turn_refusal` refuses a description shadow's chat turn; a judge shadow still relies on its slot having no `max_tokens`.

## Control lists: what the first slice leaves open

- **No kernel-level check of the vector.** `kj context control add` checks the id's shape only. A vector made for other weights, or an unknown id, reaches the server and fails the turn there. A `check` against the council objects route at `add` time would fail earlier.
- **Fork filters do not name the list.** `kj fork` copies it in every preset; a `spawn` preset child that should start plain has to `kj context control clear`.
- **A model that stops declaring `control_vectors` is found late.** The identity is read again only after a `thinking` refusal or when it is 600 s old, so for that long each turn reaches the server and fails there.
- **The prefix sensor ignores the list.** `SentPrefix::of` fingerprints the system, tools, and messages, so a turn whose only change is its control list reports no divergence, although the server feeds the context again.
- **No way to list the vectors a server holds.** Council v2 has no object listing. The nagaya web UI keeps its vectors' names in browser storage and leases the ids as keep owner `nagaya-web`, so `GET /council/v2/keeps/nagaya-web` lists ids without names. A vector's safetensors header names its `model` and `weight_hash`, which a ranged read could show. Direction (Amy, 2026-11-15): the nagaya will store named references, "probably simple kv style. needs some considerations so it'll be later." Until then, `kj context control add` takes raw ids.
- **Objects are per node.** A vector made on spaceghost is a 404 on zorak's nagaya, and the control-vectors route is not in zorak's cluster forward list. A generation forwarded through zorak does read an object put on zorak: on 2026-11-15 a PNG put on zorak reached `lfm3-omni` on spaceghost (`usage.media.read` 1 object, 6,479 bytes). Whether a forwarded generation carries `control` is untested.

## Council client: what the objects slice leaves open (2026-11-15)

From kaibo (deepseek) on `kaijutsu-council`:

- **Object reads are whole.** `get_object` sends no `Range` and buffers the whole object, while `object_bytes` on the nagaya is 8 GiB. Media for omni turns is small; a large object needs ranged or streamed reads.
- **A HEAD 404 has no body.** `has_object` cannot tell a server without the objects route from an absent object; its caller checks the identity's `objects` capability first.
- **`send` panics on a capitalized header name.** It uses `HeaderName::from_static`; callers pass lowercase names today.
- **`refuses_thinking` misses `context.thinking`.** It matches `thinking` and `contexts[i].thinking`; a generation names its one context `context`. Unverified which name the nagaya sends.
- **Media blocks: producers still make images only.** `ContentType::Media` holds a CAS hash of an image, audio, or video object, and the turn path takes its real type from the CAS record (`resolve_media_blocks_from_cas`). The attach paths validate image bytes by magic number; no producer sniffs audio or video bytes. `kj block create --content-type audio/*|video/*` takes a hash whose CAS record has that type, and is the only way an audio block reaches a model today. A derived sibling of an `audio/*` timeline cell (including `audio/midi`) is a `Media` block, but it is stamped ephemeral, so hydration never shows it to a model.
- **No byte limit on audio or video blocks.** The 5 MiB cap lives only in the image attach path (`MAX_ATTACHED_IMAGE_BYTES`, `mcp/servers/block.rs`); `kj block create --content-type video/mp4` checks the CAS record and no size, and the resolver encodes the whole object.
- **Audio and video bytes sit base64 in memory per turn.** The resolver fills `data_base64` for every media block, and `MediaBase64Cache` is bounded by entry count, not bytes, so a few long clips can hold far more than a few screenshots.
- **Audio costs tokens by the second.** `estimate_tokens` charges every audio or video block the flat `ESTIMATED_TOKENS_PER_AUDIO_VIDEO`, and it sees the type only once the resolver has run; an unresolved block counts as an image. `prefix::edit_cost`, behind the cost notes of `kj stage exclude` and `kj fork`, estimates unresolved hydration, so it charges audio and video as images.
- **The `images` fork filter drops audio and video too.** `kj fork --drop images` matches every Asset `Media` block; the filter name still says image.
- **Kaijutsu's CAS ids are 128 bits.** `ContentHash` keeps half of BLAKE3, so every council object id is computed again from the bytes.
- **Continuations replay a cut reply as closed.** The contract marks a reply cut by `max_tokens` open, and a reply to an open message takes its place. The kernel sets `open: false` in its projection and ignores `GenerationDone.context`, so the server appends a new message instead.

## vi registers: what sharing leaves open (2026-11-10)

A player's text registers are shared through the kernel's register store: compose in the tui and the app and the kernel's vi sessions replay one another's writes (`docs/input.md`). Amy's bar: "the register works for my common workflow of yy/dd/p". Open:

- **A client without `onRegisterWrite` can lose its editor pushes.** It answers the new callback method with an unimplemented error, which the server's editor bridge counts as a callback failure; 15 s of continuous failures reap the bridge. State pushes that succeed reset the streak, so the risk is a mixed-version deploy, not a steady state.
- **`registers_put` holds the editor sessions lock across one synchronous SQLite write.**
- **A key that writes more than once is recorded as its net change.** A macro running `"ayy"byy` with the same text is recorded as the first write, and older ring rows stay in the table until the next delete.
- **A write to `"1`–`"9` by name (`"1yy`) is replayed as a delete.**
- **`:regpull` replaces recorded macros** in `"a`–`"z`.
- **The app sends writes made before it connected** once its writer starts, so they reach the kernel without being asked for.
- **A refused `registersPut` leaves the kernel's copy behind** the client's registers until the next write or a `:regpull`.
- **A `ZZ` whose flush fails drops the batch's yank** from the reply.
- **The app feeds `Esc` and `i` outside its register lending.**
- **A recovered buffer's dirty test compares normalized text.** The checkpoint and the buffer drop one trailing newline.
- **A `:e` whose load fails after the path resolved leaves the path in the list.**
- **A recovered cursor never rests past the last character.**
- **`vi -r` resolves every file of the list at reopen** to drop the ones that no longer open.
- **A config write to a path an editor session holds leaves the cache stale.**
- **A line cursor in the app conversation, eventually.**
- **The system clipboard takes yanks and gives nothing back.** Middle-click paste, reading clipboard into registers, yanking twice, and kernel vi yanks need unified handling across clients.
- **The two clients still differ on yank.** The tui yanks hunks at the top of viewport; the app yanks by block while tui yanks by line.
- **No selection is written to PRIMARY.**
- **The kernel's register map is never pruned.**

## A kernel ledger ask for agent control

An agent asks for control with `kaijutsu/request_control`, and only the player at that app sees the request (`docs/input.md`, "Agent control over BRP"). A ledger ask would also reach the tui and other clients, and leave a record. It needs a gated kernel verb that flips the latch on a named app peer, so it waits on the peer route and a design conversation about the identity rules in `docs/approval-identity.md`. `AgentControl` is one resource, so the approval only has to write it.

The footer line grows leftward over the badge cluster when a request is pending at 1280 px, and the dock's connection text clips at the right edge at 800 and 1280 px.

## The headless app harness: input at a low frame rate

`contrib/headless-app.sh start` runs end to end on gamescope's headless backend (about 10 fps with a debug build), at the size `--size` names (the app takes it as `--window-size`, since gamescope sends no size); `--compositor labwc` keeps the old path. `contrib/headless-smoke.sh` checks start, `act`, `state`, and a screenshot. The harness's `text` and `keys` now return only after the frames their input needs have run (two per typed character, from `get_diagnostics`' frame counter), so a following key cannot overtake the typing. A 54-character `type_text` at 12 fps arrived whole; the earlier drop after a surface switch was not reproduced and is unexplained. Key names are BRP key codes such as `KeyW`. The draft is not readable by design, so no test asserts what was typed. `kaijutsu/act` and `kaijutsu/state` (`docs/input.md`, "Agent control over BRP") now replace most key synthesis; move the harness's `keys` users onto `act`.

## The tui's picker switch and compose edge cases (2026-11-10)

- **The picker failure is unexplained.** After Enter on the picker row, `switch_seat` never reached the kernel.
- **A draft that survives a shell submit is hidden from its submitter.**

## App and tui tune-up: what the first pass leaves open (2026-11-10)

Folds and the reader's block landed in both clients (`docs/input.md`). Left from the survey:
- **`render.rs`, `app.rs` and `present.rs` have smaller splits.**
- **Four tui tests read the `run` module's source as text.**
- **Logic both clients carry.** Ask-to-tool join, short age/count formatting, picker head/tail lines.
- **Two layouts of tool output** (`compute_output_layout` vs `layout_output`).
- **`Ctrl+O` in the tui's draft is the fold toggle.**
- **In the app:** `ui/tiling.rs`, `cell/mod.rs`, `bindings.toml` parsing duplication, and large files like `ui/dock.rs`.

## Council backend: what retiring mk leaves open (2026-11-08)

- **Liveness during a long `call` phase.** A slow served model writing a long call can reach the runtime's 120 s idle timeout.
- **Media.** `lfm3-omni` reads images, audio and video and writes audio. The provider sends each media part the served model reads as an object: on 2026-11-17 an `audio/wav` asset block (`kj cas put`, then `kj block create --role asset --kind text --content-type audio/wav --content <hash>`) reached `lfm3-omni` on a generation turn, which described a 16-note clip and a 2-note clip differently. Video in is untested. Nothing carries audio out: the request sends no `output` and the reply reads none.
- **lfm3 on tool loops.** A 1.7B model is weaker than Gemma 4 on long tool loops.

## `kj watch`: what the first build leaves open (2026-11-08)

`kj watch` watches host files and delivers waiting changes before the next prompt. Open:
- **Only path watches.** Next sources named: a ledger event for a context, a council report.
- **No way to wake a turn.**
- **A fork copies no watches.**
- **Two acceptances per prompt.**
- **Archive and promote do not resync the watcher.**
- **A draft submit in the `llm_stream` fixture never completes its turn.**

## Work Mac on the Aperture gateway (2026-11-19)

The Crusoe Aperture gateway (`http://ai.civet-hops.ts.net`) authenticates by Tailscale identity, so each backend takes `--key-optional`. Claude (`anthropic`, base URL without `/v1`), Gemini (`gemini`, base URL ending `/v1beta`), and the Crusoe-hosted open models (`openai`, `/v1`) work today. Open:

- **GPT models refuse `max_tokens`.** The gateway's `gpt-5.x`/`gpt-6.x` answer 400 "Use 'max_completion_tokens' instead". `Dialect::HostedOpenAi` is chosen only at api.openai.com (`llm/openai/mod.rs`, `wire_dialect`), and the Responses wire exists only on the `chatgpt` kind. Options: a backend setting that selects OpenAI's rules at any URL, or the Responses wire on `openai`.
- **`effort` is inert on generic servers.** The gateway's vLLM DeepSeek honored `reasoning_effort: low`; `Dialect::Generic` drops `effort` with a warning (`llm/openai/build.rs`, `build_request`).
- **Gateway errors.** GLM-5.3-Flash answered `503 {"code":"capacity_unavailable",...}`, a body not in OpenAI's error shape. The limiter cools down on 503; what a player reads has not been checked.
- **Gateway embeddings.** `/v1/models` lists `text-embedding-3-small`, but `POST /v1/embeddings` answers 405. A local Ollama serves embeddings instead (`contrib/install-ollama-embed-macos.sh`).
- **The kernel suite on macOS.** With `RUST_MIN_STACK=16777216` (two council and context tests overflow 2 MB test threads otherwise), 9 macOS-observed failures remain to triage (`kj::cc` (4), `kj::audio_capture` staging, `runtime::context_shell` git (2), and `runtime::python_tool` (2)); `kj::reflect::every_live_leaf_classifies` and `kj::db::db_backup_help_documents_the_no_restore_verb_stance` are fixed on `main`.

## Embedding model: what `kj embedding` leaves open (2026-11-19)

- **A change applies at restart.** `kj embedding set` and `off` write the setting; the index connects at server start (`connect_semantic_index` in `kaijutsu-server/src/rpc.rs`). Live reconnect moves that function into the kernel, stops the old index's re-index watcher, and swaps through `set_semantic_index`.

## Decisions backend (2026-11-18)

`kind = decisions` (`llm/decisions.rs`) asks OpenRouter's or Gemini's Decisions API a `choice` question; the gate's model judge calls `Provider::decisions`. Open:

- **Question types.** Only `choice`. kaibo's `decide.rs` also has `score` and `noul` (Gemini reads `noul` criteria; Jev ignores them).
- **Wire from the URL path.** `--base-url` is the full endpoint and its path (`/api/alpha/decisions` or `/v1beta/decisions`) picks the wire. A proxy that rewrites the path needs an explicit setting, which the `backends` table has no column for.
- **`data_collection`.** OpenRouter takes `provider.data_collection = "deny"` on a decisions request; kaijutsu backends have no such setting, so none is sent.
- **Character model on a decisions backend.** `kj character model set` accepts one; the refusal comes at `kj context create`. A backend whose kind is changed to `decisions` after contexts copied its model fails at the turn instead (`Provider::stream` refuses).
- **Cost and usage.** `kj backend decide` prints tokens and cost; a gate call records neither yet.

## ChatGPT plan backend (2026-11-18)

- **Live validation.** Checked live on gpt-6-luna: function calls and outputs, `include: ["reasoning.encrypted_content"]`, reasoning items replayed without an id under `store: false`, and `reasoning.summary: "auto"`. Still fixture-only: the `subscription_sharing_*` error shapes and the raw event stream (capture one for the fixture tests; `KJ_LLM_REQUEST_DUMP` records requests, not responses).
- **Plan prompt caching.** The plan route keeps prompt caches per machine and routes by the `session-id` request header. Codex says so in `core/src/client.rs`: "ChatGPT derives cache affinity from the Responses session-id header." Without the header, probes of identical requests rarely hit; with it, gpt-6-luna and gpt-6.2-sol both cached about 95% of a 3.5k-token prompt by the third call, 20 seconds in. Requests now send the context id as `session-id` and as `prompt_cache_key`. The route refuses `prompt_cache_options` and never reports a cache write. Still open:
  - **WebSocket mode.** The default transport (`llm/openai/responses/ws.rs`; `KJ_CHATGPT_WEBSOCKET=0` selects HTTP). Checked live on gpt-6-luna: the `wss://` handshake, reuse across turns, and incremental continuations the server accepts, both across turns and inside a tool loop (step 2 sent only the tool output; 1,920 of 2,064 tokens cached). The plan route sends `codex.response.metadata` before `response.created`. Not seen live yet: the real shapes of `previous_response_not_found`, `websocket_connection_limit_reached`, and a rejected continuation. Known gaps:
    - A `previous_response_not_found` after the first event fails the response instead of resending.
    - `refused` (set by a 404, 405 or 426 upgrade answer) lasts until the registry is rebuilt.
    - A failed platform-verifier load is cached for the process.
    - A continued request leaves earlier turns' reasoning in the server's context, while a full resend drops it (`build.rs`, `opens_a_turn`); the model can see different context depending on the transport.
    - Cancel closes the connection rather than sending `response.interrupt`, so the next request opens a new connection and sends in full.
    - Idle connections close at the next request of the same backend, not on a timer.
    - Two requests in flight for one context each take a connection; the one that finishes last is kept.
  - **`x-codex-turn-state`.** Codex echoes this response header on later requests in a turn, likely for routing; we ignore it.
- **Characters for GPT models.** One character per GPT model we use (sol, astra, terra), each with its own rc directory built from what OpenAI ships for that model: the plan's model list carries each model's Codex `base_instructions` and defaults (saved in `~/src/research/gpt-base-instructions/`; Codex itself is at `~/src/research/codex`). Translate them into kaijutsu's prompt style rather than copy them, and keep only what the Responses API on the plan route accepts. Sol is drafting its own on branch `sol-characters`. Not yet used from the model metadata: `default_verbosity` (`text.verbosity`), `default_reasoning_summary: none` (we ask for `auto`), and the freeform `apply_patch` tool shape.
- **Model discovery.** `GET /v1/models` with a plan token answers `{models: [{slug, visibility, supported_in_api}]}` (kaibo found this live); nothing in kaijutsu reads it, so model names come from config.
- **Pasted callback in history.** `kj backend login <name> --callback '<address>'` puts the single-use authorization code in the shell block. The kernel exchanges it at once, so a recorded code cannot be replayed, but it still appears in the context's history.
- **Tools on the Responses API.** kaijutsu sends a context's whole tool set as top-level `tools`, which OpenAI renders before the input, so any loadout change misses the cache from the first token. The Responses API offers two shapes that keep the prefix stable:
  - **Namespaces:** a group of functions under one name and description. With `defer_loading`, the model sees only the namespace until it loads the schemas. OpenAI suggests fewer than 10 functions each, so one namespace per MCP server or tool family.
  - **`additional_tools` input items:** tools that become callable at a point in the conversation. They are appended where they appear and must be replayed in place.
  The ChatGPT plan route takes both but refuses `tool_search`, so deferred functions have no way to load. A loadout change could become an `additional_tools` item at the turn it happens instead of rewriting the prefix. Probe first that the plan route caches across an `additional_tools` item.
- **`redact` matches the bearer verbatim; a JSON-escaped or truncated echo survives.**
- **The plan route's default output ceiling is unknown.**

## The guardian: what the first pass leaves open (2026-11-18)

Live, on Sol's third task (gpt-6-luna as guardian): it allowed a run of read-only `sed -n`/`grep` asks at low risk with a one-line rationale each, and denied one at high risk because it contained `kj ledger list` (the policy named `kj ledger` whole as administration; fixed in 98bf958f).

Second kaibo review (DeepSeek, of 1bdec2df) confirmed the first round's fixes and found:

- **The pin is checked once per submission.** `verify_judged` runs before `kaish.execute`, so a submission can rewrite a judged file mid-run: through a program's own write (`python3 -c 'open("fix.py","w")…'; python3 fix.py`) or an existing symlink or hardlink, which `writes_of` does not resolve. Re-verify before the statement that runs each file.
- **Local imports and `python3 -m` modules are not pinned.** `ShownProgram` now notes omitted local imports in the guardian prompt, while `ProgramSource::Module` (`python3 -m pkg.mod`) skips `programs_in`'s written-before check and is silently dropped by `read_programs` when not yet written, and local imports beside a judged file are still not hashed/pinned.
- **An ask's approved run is not pinned.** The approval worker runs without `judged`, so a program a judge read can change before a person approves it.

A chat model (the guardian) or a Decisions API (the decisions judge) on a context's `gate` slot judges escalated shell submissions (`council/guardian.rs`, `council/decisions_judge.rs`; `docs/approval-identity.md`, "The guardian"). Not done:

- **No structured output.** No provider client sends a JSON schema (`text.format` on Responses, `response_format` on chat completions), so the answer format is asked for in the prompt and parsed strictly; drift is a recorded miss. Adding a schema to `BuildOpts` would remove most parse misses.
- **No tools.** Codex's guardian may run read-only commands (check a path before a delete, read git remotes). Ours judges from the prompt alone; the policy says so.
- **Spend has no turn.** The call's tokens are recorded with no turn and no context, so `kj` spend views do not attribute them to the judged context.
- **The guardian reads only the shell gate.** `kj` verb asks and hook asks never reach it, as with the council.
- **Not live-tested.** Every check runs on the mock provider or a loopback Decisions server; no chatgpt, anthropic, or Decisions judge has judged a real submission yet.
- **The decisions judge's threshold is not on its row.** `council_decisions.allow_at` needs a `mass_floor` beside it, which a Decisions answer has none of, so the row leaves the threshold empty; the ask note carries it. A comparison across judges reads `allow_at` from `gate.toml`.
- **The decisions judge reads a policy written for the guardian.** `guardian.md` asks for `risk_level`, `user_authorization`, and a rationale the `verdict` question has no field for; a line maps its outcome onto the options. A policy written for a choice question may read better.
- **Model-writable trusted input.** The judges trust `AGENTS.md` and read their policy from `/config/kernel`, and a context with `file:write` can change both. `gate.toml` rules are command prefixes, so it cannot statically deny writes to `/config/kernel`; that needs a path rule in the gate.
- **`kj stage exclude` hides blocks from the judges.** An excluded block leaves the transcript tail, so an agent can drop the refusal or the user's "no" before it submits again.
- **No secret redaction.** The transcript tail goes to the `gate` slot's backend as written; a key printed in a tool result goes with it.
- **The ledger does not show what a judge read.** `kj ledger show` surfaces neither `house_rules_head` nor the `council_assessments` row.
- **The tail loads the whole log.** `evidence` reads every block of the context to keep the last few kilobytes.
- **Trust names no requester.** A `user` block is trusted when the performer did not write it; a stricter rule would trust only a person's blocks, or the context's requester's.
- **A policy added to an existing host.** `/config/kernel` is seeded only when its host directory is empty, so a host made before the guardian has no `council/guardian.md`; each judge misses until `kj config reset council/guardian.md`.
- **`[guardian]` names both model judges.** Its keys and switch configure the decisions judge too. A rename to `[model_judge]` would read better; nothing depends on the name yet.

## Sol's first task: friction from a GPT model driving kaijutsu (2026-11-18)

Sol (gpt-6.2-sol on `gpt-plan`, coder rc) wrote its own rc directory (`sol-characters`, 30b07019) in 25 asks, all reviewed by its senpai. Its report and the review log surfaced:

- **Every read asks.** `[rc.coder.council] enabled = false` and the global allow list has only `rg` and `wc`, so each `grep`, `ls`, `help`, `glob`, and `git status` waits on the reviewer. For a GPT seat in an auto mode, this is the load a council or an OpenAI-style reviewer would carry (see the auto-review notes under the ChatGPT plan backend).
- **`source FILE` is approved blind.** The ask shows the path, not the file, and the file can change between approval and execution. The reviewer had to read it from outside the kernel.
- **kaish's parse check was not found.** kaish ships `kaish-validate script.kai` (and `-e 'code'`), which checks a script without running it, but neither `help` nor `contrib/kai-parse-check.sh` (which wants a standalone `kaish` binary) led Sol to it. Sol wrapped each script in `if false; then … fi` and sourced it instead. Point the shell guard's parse-error advice and the rc docs at `kaish-validate`.
- **No effect metadata for kaish builtins.** kj verbs declare Read and the gate allows them; kaish tools declare nothing, so a read-only allow list is kept by hand (`[rc.sol]` in spaceghost's gate.toml). Proposal: an effect declaration on kaish's tool trait (Read, Write, Exec, Network), reported per command by `plan --json`, so the gate allows statements made only of Read builtins. A kaish change, under its compatibility promises.
- **The shell guard's parse-error advice suggests `bash FILE`,** which cannot validate kaish syntax.
- **`${var#prefix}` fails indirectly.** The gate reported that it could not restore an invalid environment name rather than naming the unsupported expansion.
- **No git identity in the container.** HOME is a tmpfs, so git has no `user.name`; Sol supplied its own identity per command and the reviewer denied it. Container env does not help: kaish clears the environment for host commands. It does pass `HOME`, so spaceghost's quadlet now mounts an identity-only gitconfig (`~/kj-dev/kaijutsu/gitconfig`, `[user]` and `[init]`) read-only at `~/.gitconfig`. Next: the writable kaish-extras `git` (`commit` feature, `WritableGitConfig::with_identity`) with the author from the lineage root's sheet and the committer from the performer.
- **Reading a background run's output is awkward.** Sol ran cargo with `run_in_background` and then grepped `kj wait --operation <id>` for `FAILED` and `test result:` lines; it took several asks to find which tests failed. A way to page or tail an operation's stdout would help.
- **A reviewer's piped read asks too.** `kj character show sol | grep …` from the senpai console waited on Amy, while the same command without the pipe ran.

## Senpai/kohai smoke test and archive work (2026-11-20)

- **Make the shell choice obvious.** The earlier two-shell setup was confusing;
  the current kohai binding exposes only `shell_write`. The archive worker
  confirmed that routine `grep` and `ls` went through approval, and it fell
  back to individual file reads after refusals. Try `shell_ro` and `shell_rw`
  with contrasting descriptions. Lead with `shell_ro`: use freely for
  inspection without approval (`find`, `grep`, `git status`, `kj` reads, and
  read-only pipes). Refuse output redirects, writes, host programs, and
  state-changing verbs before running anything, and say why. Use `shell_rw`
  for changes and host commands such as `cargo test`; the gate may ask.
  Use Kaibo's approval-free read path as the reference. Test that a fresh
  kohai picks `shell_ro` on its own and can finish an inspection brief without
  asks, while writes still go through the right gate.
- **Make course corrections visible.** During the archive work, a drift
  changed the brief while the kohai was waiting on tools. The senpai could not
  tell whether the worker had adopted it and interrupted and drove it again.
  The kohai found concrete pointers useful but had to infer whether the new
  brief superseded the original read-only scope. Mark a replacement brief
  explicitly and show when the worker acknowledges it. For small briefs,
  state what evidence is enough to stop.
- **Link approvals to command results.** `allowed` answers who approved, not
  whether the command ran or finished. Link the ask, shell operation, and
  exit result so `kj wait`, the ledger, and the TUI agree on waiting for
  approval (and whose), running, and finished. The kohai repeatedly polled
  for cargo results even though approval itself was quick. Check that a failed
  background test updates the TUI without requiring a refresh.

## Council house rules, objects, and verbs (2026-11-08)

- **Council house rules deadline.** An edited `AGENTS.md` reaches the council at the next decision, not at rotation.
- **Council objects and provider file handles.** Kaijutsu needs its own CAS-backed facade and provider handle maps for blake3 ids and file URIs.
- **`kj council` verbs beyond `build`.** Candidates: `status`, manual house-rules rebuilds, and `decisions`.

## Council v2 `thinking` & OpenAI-compatible client audits (2026-11-07)

- `judge_one` reads the identity twice for one decision, risking mismatched `thinking` values if caches drop between reads.
- Claude Opus thinking budget mismatches, unknown stop reasons, parallel tool call ID collisions, and missing error classifications.

## Server-side tools & Alibaba backend (2026-11-06)

- **Server-side tools.** Provider-run tools (Alibaba web search, Anthropic server tool use) lack kernel tool representations or receipt tracking.
- **Alibaba backend.** Cache write details and Token Plan credit billing need live validation against captured responses.

## Type melt M2–M8 remaining items (2026-11-05)

- **rc seeding & config reset.** Shipped defaults never reach host if directories are nonempty (`kj config reset` cannot reach nested json).
- **Always-asking acts & --no-root.** Hook bodies and rc scripts run `kj` ungated; gate refusal mirrors need broader coverage.
- **Retirement & rotation.** `kj character retire` runs archives outside locks; rotation does not automatically hand off active contexts answered to predecessors.
- **Keyring migration.** Legacy characters without `rc_dir` fail lifecycle runs until explicitly set.

## Companion & Storyteller open items (2026-10-28)

- **Companion rotation.** Seats cannot autonomously trigger rotation; find-in-tick waits hourly, and detach leaves attachment rows behind.
- **Storyteller cards & lore.** Cards live in static directories pending `rc_dir` support; models lack tools to write handoff notes or query keyword lore.

## Token and cost accounting open items (2026-10-29)

- **Error usage & split pricing.** Turn errors during cancellation drop usage; provider cache fields (lemonade/llama.cpp) need validation.
- **Granular cost tracking.** Cost is tracked as a context lifetime total without per-model breakdowns or per-turn cost in `kj context turns`.
- **Tool pricing & peak rates.** Provider-billed tool uses lack automated capture, and fixed pricing models undercount peak-rate or long-prompt billing tiers.

## Senpai/Kohai branch merge & Root console (2026-10-28)

- **Root performer privileges.** Root characters cast as model performers inherit placement privileges; authority rules need strict context checks.
- **Console approval & shell children.** Drift asks take no `--remember`; shell child grants need alignment with admin capabilities.
- **Session cleanup.** Archived label reuse and MCP session lifecycle synchronization require robust checks.

## Music refresh: live run findings (2026-11-14)

- **Endpoint cooldown & stop behavior.** Unloaded models trigger endpoint-wide cooldowns; stopped tracks leave admitted turns retrying until interrupted.
- **Model pacing & thinking.** Thinking models struggle with tight phrase deadlines (`--effort none` vs `--thinking-style off`); output bounds need strict enforcement.
- **Audio sink and track management.** Sinks share context tags and flush states; transport deletion and track naming need CLI refinements.
- **lfm3 players: decisions, not free ABC (2026-11-17).** Measured against zorak's nagaya, outside the kernel. A free ABC turn on `lfm3` or `lfm3-omni` ends cleanly in about 1 s when thinking is off and earlier phrases are assistant messages, but only about 10% of the phrases have 4 bars of the right length; an open last message (the contract's continuation) fixes the header and nothing else. A per-bar `choice` decision on `lfm3-omni` over a 12-bar menu takes 0.12 s a read and is valid by construction. Open:
  - Option order sways the answer (`lfm3` mostly picks the first option listed), so each question is read under several orderings and pooled. A server-side option for this would cut the round trips.
  - The chooser repeats itself: one bar everywhere without constraints, one phrase over and over with them. The menu needs more than 12 bars, or separate rhythm and contour questions.
  - A leader's chat message moves the answer only when each option has a description in words, and the performer's own earlier phrases outweigh it.
  - Audio in a decision works as an audio part of a context message (0.99 "sparse", 1.00 "busy" on test clips). The same clip as `case_media` is read but changes nothing; this is the nagaya's to examine. In a generation, omni called an 82 Hz tone "high-pitched".
  - The lfm engines left zorak's served list for about a minute and came back; a player must survive that.
- **Listening players: what is open.** `kj audio render` turns CAS notation or MIDI, or a track's last phrases, into a WAV in the CAS, and a musician with `KJ_LISTEN=<track>` gets that clip in its context before each turn (`rc/musician/tick/S10-drive.kai`). From the first live run and a kaibo review:
  - Prior listening blocks in the context are now excluded (`KJ_LISTEN_KEEP`), but older WAV blobs still accumulate in CAS with no blob pruning (~250 KB a phrase at 16 kHz).
  - A decision turn with clips in its context took 18 to 32 s on `lfm3-omni`, against 4 s without, and it grows as the kept history holds more clips. Each of the 16 decisions reads the audio again. The lead adapts and phrases commit, with repeats between them; one committed with no margin left.
  - The render reads the whole score context (`window_cells` scans all document blocks per cell) and loads the soundfont file on every call; mixing, `encode_wav`, and CAS store run on `spawn_blocking`.
  - Cells are placed with the track's current beat period, so a tempo change misplaces earlier phrases in a clip.
  - A rate under 16 kHz is made by averaging sample pairs with no filter.
  - The render uses its own soundfont (`~/.local/share/kaijutsu/models/soundfont/default.sf2`); the speakers use the audio node's synth. They can differ.
  - The second source is not built: a clip taken from an audio node's buffer (microphone, eurorack) should feed the same step.
- **Decision turns: what is open.** A character model with `--decide <program>` on a council backend builds its reply from `choice` decisions (`llm/council/decide.rs`; the shipped program is `rc/musician/programs/bass-bars.json`). Remaining open items:
  - Individual decision requests send no `timeout_ms`, and a transient failure inside a slot re-runs that decision via `call_classified` rather than resuming partial slot progress across stream retries.
  - Any 404 from a decision is read as a spec the server lost; a server with no decisions route gets a spec post per decision before the turn fails.
  - The turn refuses a served model without `reads`; the gate decides without that check. The contract should settle which is right.
  - A context past the server's decision limit (3,072 tokens on the nagaya) drops its oldest messages and the slot is sent again. The size that fit is not remembered, so each turn pays for the refused sends again: about 1 s of a 4 s turn on the live `omnijam` run, where the musician stance and ABC primer fill most of the limit. A decision player wants a shorter create rc, or a measured size before the first send.
  - Multi-slot decision loops (`StepOutcome::Continue` in `llm/council/mod.rs:304`) complete all slots inside one `stream.next_event()` wait, so long programs risk hitting `llm_stream`'s `idle_timeout` without intermediate liveness events; concurrent orderings (`shuffle > 1`) also overwrite single-slot `SlotWaits` state (`llm/endpoint.rs`).

---

## Anticipation and commitment — iteration order

The kernel coordinates anticipation and commitment on the shared pulse. Iterate through one observable performance with producers at different speeds; each change should improve that flow, remove a competing mechanism, or establish a testable contract. Keep model placement independent of this work. `docs/audio-io.md` and `docs/time.md` record the timing and placement rules.

1. **Keep interaction dependable.** Continue the kaish/rc caller migration alongside the following steps (`docs/rc.md`). Each migrated caller must retain identity, cancellation, complete output, and terminal settlement, with the old path removed. These are the means of playing the instrument, so verify them through the actual client as well as unit tests.
2. **Prove different producer speeds on one pulse.** Done: `crates/kaijutsu-server/tests/timeline_commitment_wire.rs` drives controlled producers against manual beats through the SSH client. Fast, slow, obsolete, and failed producers share one pulse; stale output never commits; a failed producer plays its declared fallback once.
3. **Align execution with that proof.** Audit admission through completion and commitment against exact work ownership and intended musical time. Keep slow preparation outside timeline locks; give attempts bounded resource admission and explicit cancellation/shutdown behavior. Preserve a small synchronous commit step. Use existing runtime and resolver seams; delete superseded paths as the scenario starts passing.
4. **Expose the feedback needed to play.** `kj transport work --track` reports each attempt's intended tick, admission, start, readiness, basis validity, and disposition (committed, fallback with its reason, cancelled, superseded), with the estimate and planned preparation tick beside them, as a table and as JSON. Producers report measured queue and compute time on results and errors, and each timeline keeps the newest 32 samples per resolver (`docs/time.md`). The model output resolver's estimate is the performer's largest recent sample, and the musician tick script targets by it ("Lead time from measured cost: what is open"). Open: CAS preparation's estimate is a fixed guess; transfer durations wait for media delivery in the scenario; no client shows work yet. Measure under overlap before choosing lead-time targets; avoid turning unmeasured percentiles into guarantees.
5. **Exercise replacement, then extract.** Run the same scenario with one real producer and CAS/SFTP media delivery, observing it through a client and audiod when hardware is available. Use the evidence to choose which model-specific behavior becomes an optional tool or resolver adapter. `kj audio beats` is a candidate; its new home remains undecided. Remove the kernel dependency and obsolete verb only with a verified replacement or an explicit decision to drop the feature.

The first day's useful checkpoint is a repaired interaction path and a repeatable scenario that exposes the next scheduling gap. A controlled failure is useful evidence; a new model integration is not required to prove the coordination contract. Update the checklist as each step lands, and correct adjacent docs/comments in the same change.

### Measure and recover admitted timeline work

Live model score work now binds an absolute target and a seed/score basis at admission, with turn-owned delivery and timeline-owned cancellation. The old completion listener is deleted. Controlled SSH tests exercise both the generic producer scenario and timed `kj drive`, including the shipped musician tick script (`docs/time.md`). The basis deliberately excludes the changing conversation, tool inputs and cross-track heard view. Broader dependencies need an explicit projection; current model handoffs do not automatically replay their producer on invalidity.

CAS preparation has a process-wide limit of four operations. An uninterruptible host read retains its slot even after its owner is cancelled; four stuck reads would stop further CAS preparation until a read returns or the process restarts. Timeline deadlines still settle waiting work and the pulse keeps advancing. The 20 ms cost estimate remains an initial guess, not a measured size/queue model. The open-work bound counts operations, not bytes; ready-source and committed CAS residency still need measurement before admitting large rendered artifacts through this adapter (placed clips carry small records referring to media in CAS).

Feedback retains its original source anchor when available and retries missing anchors or failed writes on subsequent pulses. The delivery cursor and bounded work history remain live state, not restart recovery or durable work provenance. A persistent write fault also holds later feedback behind the failed event. Measure and design recovery before promising durable admission or delivery.

---

## Council v2 Client & Gate Open Items

- **Queue seats & specs:** Past decisions are not replayed as call/result pairs; seats miss native spec instructions and input labels; custom specs get no examples.
- **Ahead-of-time builds:** Built once at create/fork; edited `AGENTS.md` or changed reviewers are not re-built until decision deadlines.
- **Request IDs & reachability:** Successful calls log no request IDs; pin sweeps keep seats without checking server/model reachability.
- **Observation lock:** Observations share the sync lock with decisions, risking "deadline passed during prepare" misses under slow preparation.
- **Collected allow & decision records:** Redeemed asks with no shell operation remain unsettled permanently; decision records do not durably store report stops.
- **Additional gateway hazards:** `timeout_ms` represents the whole deadline rather than remaining time; observations run regardless of decision outcome; decision record store lacks durable report stops.

---

## Architecture & System Design Backlog

- **Idle conversation reset policy:** LRU eviction resets semantic conversation history, making stored edits visible on next turns.
- **Turn execution & shell settlement:** Durable per-audit records lack edit actors; failed doc acceptances poison contexts until restart; async shell results can store empty envelopes.
- **Render live mirror:** Separate selection state from folds so the app can render its mirror over both without rebuilding `RenderBlockStore` per version.
- **Per-cast turn token budget:** Replace the removed iteration cap with a per-cast/model cumulative input+output token budget across turn model calls.
- **Lazy file documents:** Ordinary reads should not require durable documents; persist unsaved editor content with recovery metadata.

---

## Renovation Review (2026-10-14) — Remaining Open Items

### Behavior Gaps
- **4.2b Placement Authority:** A root character cast as a model performer carries the human root's placement privilege into that model's `kj` calls.
- **2.3 Approval Task:** `approval_resume` runs approved commands inline in its single task, stalling ask delivery for every context.
- **2.6 Startup & Admission:** The submit-rc race, `interrupt.hard()` on persistence failures, and turns running without admission permits.
- **4.6 Block Tools Scope:** Block and task tools edit any context a caller's `BlockId` names, breaking focus boundaries.
- **4.7b Hydration Splitting:** Notification, Resource, Task, Error, and Drift blocks inside open model batches flush during hydration.
- **2.2 File Caching:** Clean cached files keep serving after delete or rename; `MountBackend` never invalidates caches.
- **5.3c Block Copying:** `cherry_pick_block` drops metadata; correct copying requires dedicated block-store minting APIs.
- **3.1 Council Synchronization:** Council shadow's `CREATE` lock drops before `run_create_lifecycle`, priming shadows twice per call.
- **6.3 MIDI Cues:** CAS-backed ABC cues do not play because `dispatch_midi_cue` has no prefetch path.
- **7.2 App Rpc Actors:** App spawns new `RpcActor` on every cache-miss context switch, losing collapse state.
- **7.3 UI Layout Handling:** App input/scroll handlers fire on every screen; split panes draw main cells; shell dock misses cursor/selection tracking.
- **8.1 HNSW Errors:** Garbage graph headers panic inside `hnsw_rs` instead of returning errors; `HnswIo` leaks on load.
- **5.4 SFTP Rename & Symlinks:** `posix-rename` fails to detect duplicate names and is non-atomic; `getattr` follows symlinks.

### Clarity Items
- **1.7 Hydration & Errors:** Two hydration loaders; `promote_context` returns raw SQLite errors on label conflicts; `publish_batch` takes one lock per message.
- **3.3 Database Indexes:** `council_programs(program_decision_id)` lacks an index; `list_bump_flavors` lacks program filters.
- **3.5 Approval Database:** `approval_identity` locks DB up to four times per call; `approval_ledger::claim` has test-only callers; `rules::revoke` is non-transactional.
- **8.2 Index Watcher:** Index watcher treats closed streams as timeouts; corrupt `context_id` blobs are skipped silently.

---

## Additional System Subsystem Debt & Open Items

### Capability & Roster Management
- Capability names and layout need a redesign sweep (`Instance`/`Tool`/`Facade` vs broad grants vs bare verbs).
- Worker loadout entries still point at house verbs (`kj ledger`, `rc`, etc.) via clap schemas and held-ask text.

### Shell & Execution Settlement
- `did_spill` misses nested spills (kaish, open upstream).
- No timeout knob on server kernels for wire timeout tests.
- Failed wakes after completion notices are never retried.

### Upstream Dependencies & Workarounds
- Kaish positional suffix expansion `${0%.kai}` evaluates to empty values.
- `ls -l` prints host owners as numeric UIDs instead of names through `MountBackend`.
- OSC 8 hyperlinks wait on Ratatui span attribute support.
- Hi-res wheel (v120) blocked at winit/sctk.
- `ExecResult.output` cannot carry structured data past kaish's output limiter.
- `vte` 0.15.0 drops control bytes after chunked partial UTF-8 codepoints.

---

## Technical Debt & Subsystems

### Audio & MIDI
- **Offline analysis:** `kj/audio.rs::audio_beats` starts a blocking task for each request without concurrency limits, decoded sample bounds, or inference cancellation mechanisms.
- **Audio nodes:** Keep jobs are in-memory on both sides (`KeepJobs`, `takes.rs::Pool`) and cannot survive kernel restarts; profile edits load only on connection/reconnection.

### VFS & SFTP
- **CLI VFS bypass:** `--out` writes in `kj cas get`, `kj block cat`, and `kj block original` bypass VFS mounts due to synchronous dispatcher limitations.
- **POSIX SFTP limits:** `posix-rename` does not detect duplicate names for files; `getattr` follows symlinks.

### Editor & Vi
- **Terminator reconcile:** `mirror_ops` reads blocks and conditionally deletes terminators in two separate block-store calls, risking race conditions if modified externally.
- **Lossy text round-trips:** `EditorCore::insert_at` normalizes text and strips trailing newlines, leaving buffers short of final newlines until hydration.

### Retries & Rate Limits
- **Error classification:** Catch-all HTTP statuses and JSON parse failures map to retriable `ApiError`, risking infinite retry loops on permanent failures.
- **Lock contention & burst limits:** Concurrency limits and queue timeouts need tuning under heavy load, and SQLite locking across editor sessions and council megakernels stalls concurrent work.
- **Deadlines and timeouts:** Endpoints give free slots regardless of elapsed caller deadlines; long-running shell calls need robust timeout policy enforcement.

## The `done` verb and the senpai wake: what is left (2026-11-22)

The `done` rc verb, `kj drive --if-idle`, and the rule that a seat holding `drive` without `operator` reaches only itself and its senpai are in (`rc/mod.rs`, `kj/drive.rs`, `docs/rc.md`, "The `done` verb"). Tested on the mock provider; not yet run on the live kernel. Open:

- **Deploy.** A host made before this has no `kohai/done/S10-notify.kai`, and its `kohai/create/S10-binding.kai` lacks `drive`; `kaijutsu-server rc reseed` leaves a differing binding alone, so a live kohai needs `--force` or a hand edit, then a new context (a loadout is copied at create).
- **A drive into a busy senpai can still leave a seed.** `kj drive --if-idle --prompt …` checks for a running turn before it writes the prompt, but a turn that starts between that check and admission leaves the prompt block in the log with no turn of its own. The atomic admission prevents a second turn.
- **Whether the gate sees the script.** The `done` script runs `kj drift push` and `kj drive` as the system caller on the kohai's context. Check on the live kernel that neither waits on a gate ask or a drift ask.
- **A senpai has no clean way to message a person.** The senpai does not hold `done` on purpose: offering `done` makes a context call it to end an autonomous turn, which a senpai woken by a kohai report should not have to do. It needs a tool of its own that reaches a person without that effect, for the cases where it stops to wait for one. A drift into a root console already holds for the person's answer and raises an ask, so that edge is one place to start. The person-facing text of `done`'s description is reached only by a context whose senpai is a person, such as a coder under one.
- **One verb call per turn, not per task.** A kohai that calls `done` and is driven again fires `done` again; a senpai that drives it in a loop sees one report per turn.

## ChatGPT plan route: closing the gap with Codex's protocol (2026-11-22)

Six `cyber_policy` refusals landed on one `gpt-6.2-sol` senpai context in a day of ordinary work in this repository; Codex has done much work here on the same models without that. We identify as kaijutsu (own user agent, no Codex headers) and aim to follow the protocol as faithfully as Codex does. Compared by reading `codex-rs` at 5a6fb36 and Codex's catalog entry for the model; no request from either side was captured. Open:

- **A refusal logs the request's shape (`llm.refusal`) and the server's metadata is logged (`llm.server_metadata`).** Shape and sizes only, never text. Read them with `journalctl --user -u kaijutsu | grep -E 'llm.refusal|llm.server_metadata'` after the next refusal. Compare the refused request with the turns before it.
- **Server signals still not acted on.** Codex reads `openai_verification_recommendation` (`trusted_access_for_cyber`) and `openai_chatgpt_moderation_metadata` from `response.metadata`, the `OpenAI-Model` reroute header, and the safety-buffering headers. We log the metadata only.
- **Request knobs.** Codex sends `text.verbosity` (catalog default `low`) and defaults the reasoning summary to `none`; we send `summary: auto` and no verbosity. Both belong on the character (`kj character model set`), with medium verbosity as our default.
- **Tool delivery.** The catalog says `use_responses_lite` and `tool_mode: code_mode_only` with a freeform `apply_patch`; we send a classic `tools` array of our own function tools. Lite delivery (tools as a developer item) is a contained change in `llm/openai/responses/build.rs`; `code_mode_only` is not. Read Codex's lite format before building, and do not try it on the live account without a reason.
- **Routing and client metadata.** Codex replays `x-codex-turn-state` and sends installation, window, and turn metadata. Ours would carry kaijutsu's own names; nothing is sent today.
- **A refusal should raise an ask.** A refused turn now stops: its error block leaves the model's history, the notice tells the model to carry on from the last result, and the continuation closes to automatic resume until a new drive or prompt. That stop should also be a decision a person sees: a ledger ask (`Origin::Refusal`, like `Origin::Drift` an ask that never executes source) whose approval opens a new epoch and resumes the context, and whose denial leaves it stopped. Its text names the provider's decision and says that sending again may be refused again, and that `kj context rotate` continues from a handoff. Needs the origin in `approval-ledger`, the reviewer routing for the refused context, `kj ledger` rendering, and the approval worker's resume.
- **The senpai's tool surface** includes `admin`, `config-write`, `house`, `system`, `builtin.bindings`, and the ledger verbs, which Codex's agent never has. Moving context operations to a banto supervisor would shrink it. A seat that holds `drive` without `operator` reaches only itself and its senpai (`kj drive`), so a senpai without `operator` could not drive its own kohai until that rule also covers the contexts that answer to it.

## Review of the day's parallel work: what is left (2026-11-22)

An Opus review of `8b346f61..HEAD` found these open. Claude's own findings (the refusal stop, `declined` through `done`) are fixed; these belong to the other two authors or are small.

- **Banto tick drives on any pending ask.** `rc/banto/tick/S10-watch.kai` marks a tick urgent whenever `kj ledger list --json` is non-empty, which is kernel-wide and pending-only, and the urgent path ignores `elapsed`, the backoff, and `BANTO_MIN_SWEEP_SECS`. Any pending ask wakes banto every wakeup; its own advised commands (`kj context rotate --handoff`, `kj transport ooda off`, `kj context archive`) are not in `[rc.banto] allow`, so they ask, and the next tick sees that ask. Narrow urgency to asks banto may answer, and honor the backoff.
- **A senpai can create a banto.** `senpai_allowed` in `kj/context.rs` covers any `kj context create`, whatever the character, and a created banto runs its create rc with house, operator, admin, and config-write. Limit the senpai's create to the characters it supervises.
- **`kj interrupt` has no target scope.** It checks only `drive`, and it moved from the house verbs to the senpai's, so a senpai can interrupt any context. Scope it like `kj drive`.
- **`rotate --handoff` beside `kj handoff note`.** S16 still injects the character's notes when a `--handoff` brief is given, so the successor reads both. Prefer one mechanism, and say in `S17-predecessor.kai` what `ROTATED_HANDOFF` does.
- **The narrowed senpai loses verbs a signoff may plan on:** `kj transport`, `kj context set`, `kj context hydrate --clear`, `kj context conclude|promote`, `kj system`. It applies to a new or rotated senpai only.
- **`kj drive` help, its denial text, and `docs/contexts.md`** still say a seat without `operator` drives "itself and its senpai", and omit its direct kohai.
- **Banto's tick writes a trace block per tick** (about 1,440 a day in a console's log) and a model-visible error block on every failed tick.
- **A bare `senpai drift banto`** resolves a label; where a banto is the console, the drift is held for a person and the wake script runs before the release. Use `.senpai` or the real label.
- **File cache: a window remains where a reload can overwrite an edit.** A reload now checks, under the cache lock and before replacing the text, that the entry is still clean with the same marks, and a `getattr` error other than not-found no longer evicts. Editors still edit the block first and call `mark_dirty` afterward, so a reload that snapshots and replaces between those two steps sees a clean entry and clobbers the edit. Closing it means making the edit and `mark_dirty` one step, or comparing block text. The cold-load path can still race a concurrent insert, and the cache lock is now held across the block store's synchronous calls (no deadlock found; the store's internal locks were not audited).
- **One corrupt `context_id` row disables the whole index** (`kaijutsu-index/metadata.rs`): loud as intended, but the only repair is deleting `index_meta.db`.
- **The guardian's whole-file check** reads one byte past the budget, so a multibyte character that straddles it can leave a cut file reading as whole; the repository's own `AGENTS.md` is far below it.
- **`beat.rs`** keeps a comment and a test doc stating the crash-loop invariant that the change made opt-in, plus a dead `let _ = tail;`.

## Second review: the refusal ask, response shaping, and drift as a user message (2026-11-22)

Open after an Opus review of `d80a29ad..HEAD`. The ask's races and the default-on `phase` are being fixed on their branches; these are the rest.

- **A refusal whose ask cannot open leaves a held context with nothing to answer.** No performer, a failed reviewer route (retired senpai, no reviewer above, a cycle), or a failed error-block write is only logged. A prompt or `kj context rotate` still moves the context; nothing tells the senpai.
- **A plain `kj drive` can queue behind a person's prompt.** If an allow settles while that prompt's turn is admitted but has not reached `begin_turn`, the latest turn still reads `refused`, the release succeeds, and the non-idle drive queues a second turn.
- **Derived `phase` differs from the model's own.** A text-only reply with no reasoning hydrates as plain text and is never tagged, so the same position is `final_answer` or nothing depending on whether reasoning preceded it; an autonomous text-only reply followed by a kernel nudge and more tool calls is tagged `commentary`; `partial_answer` is never produced. Whether the server tokenizes `phase` into its cached prefix is unknown. A continuation over the WebSocket is unaffected (it compares role and content).
- **A mid-turn drift opens a turn in the Responses builder.** A drift is now a user message, and `opens_a_turn` (`responses/build.rs`) treats any user message with more than tool results as a turn start, so encrypted reasoning from earlier in the same turn is dropped from later requests and the prefix changes once. Notifications and resources already do this. Decide whether a drift should open a turn for reasoning retention.
- **A held banto may fail every tick.** `banto/tick/S10-watch.kai` marks a tick urgent while `kj ledger list` is non-empty and runs `kj drive --if-idle` under `set -e`; if banto itself is held, each tick errors on the hold.

## Orientation, quoting, and `kj context info`: what is left (2026-11-22)

- **Quote the ids the kernel prints.** About two dozen command hints built in Rust strings (for example the next step a verb prints after an ask) still show an unquoted id; the rule is in `docs/writing.md`, "Commands in examples", and the rc text now follows it. A test could scan hint strings the way one now could scan the rc text.
- **Two rc scripts keep unquoted ids because Rust tests pin their text:** `lib/create/S35-orient.kai` and `kohai/done/S10-notify.kai`. Quote them and update those tests together.
- **`kj help` is at its size limit.** `crates/kaijutsu-kernel/docs/help/kj.md` is 4,095 of 4,096 bytes, so its `<command>` placeholders are unquoted. Raising the budget, or moving the verb groups out, would let them be.
- **`kj ledger list` has no reviewer filter,** so the arrival card says "Asks pending on this kernel", not "waiting on you". `kj ledger bumps --context` still resolves only live contexts.
- **A context id made only of digits breaks `editor_recovery::a_corrupt_record_refuses_reopen_*`** once in a while; it is the same class of problem as an unquoted id read as a number.
- **`kj context info` has no drift-queue count:** the router's `staged_len` is not per context. Its gate-judge line is tested only for the no-judge case.
- **The rc layout** (88 link files, an `S` prefix nothing reads) and the console and banto roles are written up for a decision: layering the shared scripts with per-directory overrides, a phase letter in place of `S`, a sleepy `console` rc, and a banto context created by bootstrap and left unattached.

## The rotation signoff: what is left (2026-11-23)

A rotation asked for inside a context's own running turn now defers its archive to that turn's end: the turn takes one more inference with only `done` offered, then the `archive` lifecycle runs and one transaction archives the predecessor and moves its label, ring seat, and handoff pointer (`docs/contexts.md`, "Rotation, handoffs, and archive"). What that leaves open:

- **A prompt that lands during the signoff call rides it.** The kernel instruction that asks for the signoff delivers with any pending input, so a person's submit becomes part of the farewell and its wake is spent. Only a rotation with `--migrate` carries a conversation forward, so that prompt is not answered by the successor. Decide whether the signoff should deliver no input, leaving the submit to fail loudly instead.
- **A rotation aimed at another context still archives mid-turn.** A senpai rotating a busy kohai, or a person rotating another context, keeps the immediate path: the target's turn sees an archived context and its next tool call is refused. Deferring those rotations would put the signoff call in a turn whose caller is not the rotation's caller.
- **Who asked is read from the context lock, not the call's origin.** A shell in the rotating context that is not the turn's own tool call (a person attached to the same context) also asks the running turn for a signoff. `CallContext.tool_call` is `Some` only for a model tool call, and `ShellIdentity` and `KjBuiltin` discard it; threading it through would make the origin exact.
- **The `done` verb and the archive lifecycle can overlap.** The `done` verb is queued while the predecessor is still live and the archive lifecycle then runs inline, so the worker pool may run both scripts for the same context at once. Ordering within each is unchanged.
- **A deferred rotation has no recovery pass.** A kernel restart between the rotate result and the turn's end leaves the predecessor live and the successor unlabeled; only a fresh `kj context rotate` moves the context on. The kernel logs this when a lease ends holding a rotation it never settled.
- **The signoff's `done` answers from the registered `builtin.turn` server directly,** not through the broker, so a loadout that omits the instance cannot refuse it and a colliding `done` cannot intercept it. Broker hooks do not run for that call.

### After a cold review of the first land (2026-11-23)

A reviewer that did not write `03a69195` read it and raised nine findings. The three blockers are fixed in `0f790062`: the reservation is taken after authority and before the first side effect, a settle refuses rather than assigning a label to an archived successor, and the pending check no longer tells a caller who may not rotate. What that review leaves open:

- **A deferral can arrive at a turn that is already stopping**, or after the signoff decision but before ingress closes, so the settlement archives with no signoff call even though the command reported one would happen (`src/runtime/turn_state.rs`, `src/runtime/llm_stream.rs`).
- **Completion is published before the settlement.** The turn drops its liveness lease and publishes completion before the archive lifecycle and the commit, so `kj wait` can return while the predecessor still owns the label, or before a settlement failure is known (`src/runtime/llm_stream.rs`).
- **A rename or retag during the signoff can disagree with the commit.** The live router keeps the label captured when the rotation began while the transaction transfers the predecessor's current label (`src/kj/context.rs`).
- **The held-call test does not assert the whole transfer.** It checks the label but not the handoff pointer or the live seat while the signoff call is held, and `ROTATED_HANDOFF` is a flag rather than evidence of the pointer (`src/rc/tests.rs`).
- **Who asked is still read from the conversation lock** (see the earlier item): a person's separate shell in the same context can trigger a signoff on a turn it did not originate.
