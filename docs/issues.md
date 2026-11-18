# Open Issues

Live work items distilled from prior design and TODO docs, plus architectural observations from code reviews. Code is truth; this exists to track what's *not* in the code yet.

Organized by area. Keep entries terse — link to file:line when a pointer makes the work concrete. When an item ships, delete the entry — if the "how we got here" is worth keeping, move the narrative to [`devlog.md`](devlog.md). See `AGENTS.md`, "Writing, memory, and git".

---

## rc bindings: the person-input facades are copied per directory (2026-11-15)

- Every rc directory a person talks to grants `edit_input`, `submit_input`, and `commit_capture` in its own `S10-binding.kai`. When the toolie became an assistant Amy talks to, her tui hit one missing facade, then the next (`submit_input`, then `edit_input`). A shared lib script for the person input set, sourced by each person-facing directory, would keep the set in one place.

## Context shadows: what the description slice leaves open

- **A resumed ask's error is lost.** 2026-11-15, live: from an MCP seat, `kj context create --as shadow-probe --label shadow-probe` raised an ask; Amy allowed it; gate-resume logged "running approved ask" and nothing more. The ask's output block is empty (0 bytes) and no error was logged, so a command that fails to parse after approval says nothing to anyone.
- **No RPC and no asynchronous delivery.** `kj shadow describe` waits up to `DESCRIBE_TIMEOUT` (120 seconds) and prints the description. The app and tui need a request that returns at once, and a rendition that arrives through the change feed when it is ready (`docs/contexts.md`).
- **Turns do not follow the media slots yet.** A turn sends an image whenever its provider can; the character's `image` slot (`docs/contexts.md`) should decide: native when it names the character's model, a description in place when it names another, a marker when absent.
- **A shadow keeps the `image` slot it was made with.** A character's later `kj character slot set <name> image ...` reaches a new shadow only after the old one is archived. The maker in the index is the shadow's own slot, so the key stays truthful. A rotation of the source does not yet carry its description shadow to the successor (`docs/contexts.md`).
- **A refused marker leaves an empty shadow.** The shadow is made before the provider says it cannot read the type, because the maker comes from the shadow's slot. The request writes no rendition and calls no model.
- **Simultaneous misses both call the model.** Two requests for one key that both miss the index each pay for a description. The second one is linked and not indexed, so its shadow holds a rendition the key does not name.
- **A failed commit stops the shadow's document.** A rendition whose link or index row cannot be written fails the request and leaves no block, and the shadow's document refuses writes until the kernel restarts (the rule of `BlockStore::append_block_recorded`).
- **A failed `create_document` strands a shadow's document.** `ensure_description_shadow` returns the live shadow before it creates the document, so after one failed `create_document` every later describe of that source fails with DocumentNotFound until the kernel restarts.
- **A reused copy names no original.** The copy has its own link to its source block and no row that names the rendition it was copied from. The index keeps naming the original rendition after its shadow is archived; it still serves, since archive keeps blocks.
- **Presets and spans have no writer.** `preset` in `rendition_index` is empty, and `span_start` and `span_end` are NULL.
- **Video reads only on a council backend that lists it.** Claude, the OpenAI-compatible providers, and Gemini send images only, so `describe` of a video on them is refused before the call.
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
- **The request dump keeps Gemini's media.** `request_dump::elide_images` knows Anthropic's base64 source and `data:` URLs, not Gemini's `inlineData`, so a Gemini dump holds the whole base64 body under a header that says images are elided.
- **Gemini sends images only.** Its API reads audio and video inline; the builder writes a marker for them. Gemini omni models will want them.
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
- **A lagging client loses pushed writes for good.** The client's event broadcast holds 256 events and drops on lag with a log line.
- **A `ZZ` whose flush fails drops the batch's yank** from the reply.
- **The app feeds `Esc` and `i` outside its register lending.**
- **No test types `dd` in a kernel vi session and `p` in client compose.**
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

## The headless app harness cannot seed (2026-11-14)

`contrib/headless-app.sh start` stops at "kernel listened but never answered". Its readiness probe and seed steps run `kj` through `contrib/kjmcp.py` with the root character's key, and `kaijutsu-mcp` now refuses to auto-register a context a person performs. The harness needs a non-root character to seed through, or a seed path that needs no MCP session. At the headless app's ~1 fps, `type_text` also drops characters after a surface switch.

## The tui's local tasks stall, and a picker switch went missing (2026-11-10)

- **The tui's local tasks sometimes do not run for hundreds of milliseconds.** The draft outbox writer, the actor and the RPC system are `spawn_local` tasks inside `local.run_until` (`main.rs`).
- **The picker failure is unexplained.** After Enter on the picker row, `switch_seat` never reached the kernel.
- **A sibling's submit leaves our compose text behind.**
- **A draft that survives a shell submit is hidden from its submitter.**
- **The tui's `submitted` map is never pruned.**

## App and tui tune-up: what the first pass leaves open (2026-11-10)

Folds and the reader's block landed in both clients (`docs/input.md`). Left from the survey:
- **`render.rs`, `app.rs` and `present.rs` have smaller splits.**
- **Four tui tests read the `run` module's source as text.**
- **Logic both clients carry.** Ask-to-tool join, short age/count formatting, picker head/tail lines.
- **Two layouts of tool output** (`compute_output_layout` vs `layout_output`).
- **The tui's `:kj` completion draws no popup.**
- **`Ctrl+O` in the tui's draft is the fold toggle.**
- **In the app:** `ui/tiling.rs`, `cell/mod.rs`, `bindings.toml` parsing duplication, unused `agent_color_*` theme fields, and large files like `ui/dock.rs`.

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
- **Review leftovers (kaibo gpt-plan + deepseek).** Not fixed:
  - A 401 refreshes and resends even when its code is a permanent refusal, and a failed second `bearer` call hides the first 401's detail.
  - The refresh margin is a flat 300 s; an `expires_in` under that would refresh on every request.
  - Only a connect error counts as an unsent refresh; a TLS failure before the request was written still drops the grant.
  - The ID token's `azp` is not checked.
  - `chatgpt::Client::stream` clones the history for the 401 resend.
  - `redact` matches the bearer verbatim; a JSON-escaped or truncated echo survives.
  - The plan route's default output ceiling is unknown.
- **Failing kj tests on main.** `kj::reflect::tests::every_live_leaf_classifies` (`kj audio render` needs `<SOURCES>`) and `kj::db::tests::db_backup_help_documents_the_no_restore_verb_stance` (the `backup` long help lost its no-restore text) fail independent of this work.

## Sol's first task: friction from a GPT model driving kaijutsu (2026-11-18)

Sol (gpt-6.2-sol on `gpt-plan`, coder rc) wrote its own rc directory (`sol-characters`, 30b07019) in 25 asks, all reviewed by its senpai. Its report and the review log surfaced:

- **Every read asks.** `[rc.coder.council] enabled = false` and the global allow list has only `rg` and `wc`, so each `grep`, `ls`, `help`, `glob`, and `git status` waits on the reviewer. For a GPT seat in an auto mode, this is the load a council or an OpenAI-style reviewer would carry (see the auto-review notes under the ChatGPT plan backend).
- **A denial carries no reason.** `kj ledger deny` has no message, so the model sees only that the reviewer said no. A correction has to go in a separate `kj block create -c <ctx> --role user` block, which arrives at the next request and can miss a turn that ends first (it did).
- **`source FILE` is approved blind.** The ask shows the path, not the file, and the file can change between approval and execution. The reviewer had to read it from outside the kernel.
- **No in-kernel kaish parse check.** `contrib/kai-parse-check.sh` needs a standalone `kaish` binary that the container does not have. Sol wrapped each script in `if false; then … fi` and sourced it.
- **The shell guard's parse-error advice suggests `bash FILE`,** which cannot validate kaish syntax.
- **`${var#prefix}` fails indirectly.** The gate reported that it could not restore an invalid environment name rather than naming the unsupported expansion.
- **No git identity in the container.** HOME is a tmpfs, so git has no `user.name`; Sol supplied its own identity per command and the reviewer denied it. Container env does not help: kaish clears the environment for host commands. It does pass `HOME`, so spaceghost's quadlet now mounts an identity-only gitconfig (`~/kj-dev/kaijutsu/gitconfig`, `[user]` and `[init]`) read-only at `~/.gitconfig`. Next: the writable kaish-extras `git` (`commit` feature, `WritableGitConfig::with_identity`) with the author from the lineage root's sheet and the committer from the performer.
- **Reading a background run's output is awkward.** Sol ran cargo with `run_in_background` and then grepped `kj wait --operation <id>` for `FAILED` and `test result:` lines; it took several asks to find which tests failed. A way to page or tail an operation's stdout would help.
- **A reviewer's piped read asks too.** `kj character show sol | grep …` from the senpai console waited on Amy, while the same command without the pipe ran.

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
  - Every tick stores a new clip and a new block, and nothing prunes them: about 250 KB a phrase at 16 kHz, so a player left running fills the CAS. Wanted: one replaced block, or a kept window.
  - A decision turn with clips in its context took 18 to 32 s on `lfm3-omni`, against 4 s without, and it grows as the kept history holds more clips. Each of the 16 decisions reads the audio again. The lead adapts and phrases commit, with repeats between them; one committed with no margin left.
  - A dormant or misspelled track is one failure: the tick drives, then ends with status 1 and an error block every phrase. After a restart every track is dormant until attached.
  - The render reads the whole score context and loads the soundfont file on every call, on the dispatch task; only synthesis runs on the blocking pool.
  - Cells are placed with the track's current beat period, so a tempo change misplaces earlier phrases in a clip.
  - A rate under 16 kHz is made by averaging sample pairs with no filter. `--phrases` has no upper bound before the offset arithmetic.
  - The render uses its own soundfont (`~/.local/share/kaijutsu/models/soundfont/default.sf2`); the speakers use the audio node's synth. They can differ.
  - The second source is not built: a clip taken from an audio node's buffer (microphone, eurorack) should feed the same step.
- **Decision turns: what is open.** A character model with `--decide <program>` on a council backend builds its reply from `choice` decisions (`llm/council/decide.rs`; the shipped program is `rc/musician/programs/bass-bars.json`). From the kaibo review of the first build:
  - The whole turn runs inside `provider.stream`, before the runtime's request and idle deadlines start, so only the council client's per-call timeout bounds it, and a transient failure runs every decision again. The request sends no `timeout_ms`.
  - A slot that fails loses the usage of the decisions before it; the done event reports the largest prompt and not the spend of all decisions.
  - A council `model` slot with no `max_tokens` is refused at turn time, though a decision turn samples no tokens. Set `--max-tokens` on the character for now.
  - Any 404 from a decision is read as a spec the server lost; a server with no decisions route gets a spec post per decision before the turn fails.
  - A decision turn writes no `llm request` line and no request dump.
  - `Provider::one_shot_opts` returns early for a slot with an effort or thinking style, before the council client clears `decide`. No one-shot carries a program today.
  - `Reply::whole` trims white space at the edges of the program's reply, and a cancelled decision stream yields no cancel-confirm `Done`.
  - With fewer distinct orderings than `shuffle`, orderings repeat and the pool is uneven (a 2-option slot with `shuffle` above 2).
  - The turn refuses a served model without `reads`; the gate decides without that check. The contract should settle which is right.
  - A context past the server's decision limit (3,072 tokens on the nagaya) drops its oldest messages and the slot is sent again. The size that fit is not remembered, so each turn pays for the refused sends again: about 1 s of a 4 s turn on the live `omnijam` run, where the musician stance and ABC primer fill most of the limit. A decision player wants a shorter create rc, or a measured size before the first send.

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
- **Shared client recovery:** `kaijutsu-client` needs ownership of subscription, mirror, snapshot recovery, and rejection of obsolete responses across app, TUI, and ACP.
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
- **5.1 Client Subscriptions:** Client refuses subscribe while connecting, failed resubscribe freezes views, and `BlockEventsForwarder::closed` latches.
- **5.2 Actor Lifecycles:** `run_context_feed` wakes on every context's events; `spawn_editor_reconciler` holds an `Arc` cycle preventing kernel drops.
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
