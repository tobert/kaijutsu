# Open Issues

Live work items distilled from prior design and TODO docs, plus architectural observations from code reviews. Code is truth; this exists to track what's *not* in the code yet.

Organized by area. Keep entries terse — link to file:line when a pointer makes the work concrete. When an item ships, delete the entry — if the "how we got here" is worth keeping, move the narrative to [`devlog.md`](devlog.md) (the landed-work story). See the three-file working-notes pattern in `CLAUDE.md`.

---

## SFTP over the VFS (slices 0–2 + extensions + tracing landed 2026-06-26; slice 3+ open)

Read + write + OpenSSH extensions ship (`crates/kaijutsu-server/src/sftp.rs`,
the `"sftp"` arm in `ssh.rs`). Two DeepSeek reviews + a Gemini Pro batch
whole-file review are folded. Remaining, in `docs/sftp.md` slice order:

- **Slice 3 — capability binding (consumer of `/v/session`).** Prereq = the `/v`
  surfaces below (own work, `docs/slash-v.md`). SFTP's part: register each
  connection as a session, intercept `symlink`/`readlink` on
  `/v/session/self/bound` to set the per-connection arm (sliding TTL ~15m), route
  privileged writes through the bound `context_id` to the shared
  `context_allows_rc_write` guard, deny-with-message otherwise. Replaces the
  stopgap `privileged_write_denied` (lexical `/etc/rc`+`/etc/config` deny).
  **Also fixes the altitude bug the Gemini review flagged:** the lexical deny sits
  *above* symlink resolution, so a symlink resolving into the gated tree would
  slip past it (not a live bypass today — symlinks don't cross mount backends and
  host `/` is read-only — but the gate belongs below resolution).
  **Re-verified 2026-06-27** (gpal batch raised it again): `LocalBackend::resolve`
  canonicalizes *and* re-clamps with `canonical.starts_with(canonical_root)`
  (`vfs/backends/local.rs:102-113`), so a symlink escaping its backend root is
  rejected `path_escapes_root`; gated paths (`/etc/rc`, `/etc/config`) are a
  separate `ConfigCrdtFs` mount reached by VFS prefix, not OS-symlink-reachable
  from another backend. Confirmed not-a-bypass — the altitude fix is hygiene, not
  a live hole.
- **Slice 4 — adapter limits.** Rate-limiting + traversal-depth/size caps to
  survive an editor-indexer crawl (the access-pattern-shift DoS in
  `docs/sftp.md` → Security posture). The open-handle cap (1024/session) is a
  coarse down-payment; also need true streaming `readdir` — `VfsOps::readdir`
  loads the whole entry list, so only the heavy per-entry `File` build is chunked
  today, not the `DirEntry` fetch. **The retained-list angle (gpal batch
  2026-06-27):** `opendir` (`sftp.rs:392`) eagerly materializes the *entire*
  `readdir` `Vec<DirEntry>` into the session handle map at open; an editor indexer
  crawling `/v/ctx` holds many such lists open at once, so the OOM vector is the
  sum of retained `DirEntry` lists across open dir handles, not just one page's
  `File` build. The real fix is paginating `VfsOps::readdir`
  (`readdir(path, offset, limit)`) so the handle holds a cursor, not the list.
- **TOCTOU atomicity refactor.** The write/fsetstat generation guard
  (`sftp.rs:595-608`) has two non-atomic facets. (a) The post-write re-getattr can
  adopt a concurrent replacement's generation. (b) **Concurrent-appender lost
  update** (gpal batch 2026-06-27, verified): `getattr` → generation-check →
  `attr.size` → `write` spans separate `.await`s with no CAS, and APPEND offset is
  `attr.size`. The guard catches rename-replace (its job) but *not* two appenders —
  both read gen=N, both pass, both write at the same offset, one clobbers the
  other. **Scope = cross-session** (two SSH connections to the same path); a single
  client's pipelined writes are serialized by the handler's `&mut self`, so this is
  not intra-session. Returning the new `FileAttr` atomically from
  `VfsOps::write`/`setattr` closes (a); (b) also needs an atomic-append primitive
  or per-path write serialization. Kernel-wide change, worth doing before slice 4.
- **Runner-verify** with a stock `sftp`/sshfs client through the real server
  (kernel rebuild+restart; capnp schema unchanged, but it's a new subsystem).

## Shared state space + myaku (design `docs/shared-state.md`; myaku detail in git history)

High-level sketches landed 2026-06-28; dedicated design sessions to follow. The
thesis: the VFS *is* the shared-state namespace; tiers are mounts (`/run`
`MemoryBackend` for ephemeral read-write — its own mount, `/scratch` likely retired
— and `/v` for read-only/CRDT durable). No bespoke store. Open work that's already
concrete:

- **Delete `KvDocument`/`Kv`.** Being deleted, not deprecated. Remove `kv.rs`, the
  capnp surface (`kvGet`/`kvSet`/`kvDelete`/`kvKeys`/`kvWatch`, @79–83), `kj kv`. The
  audit's "test-only" was wrong: the **app is a real caller** — it persists
  `<client-id>.current_context` (`actor_plugin.rs:319`) and reads it back on reconnect
  (`:534`). That one key **splits in two** (see `docs/shared-state.md`, *Retiring KV*):
  - **Live acting context** (rendered at `/v/session/<id>/context`) → already
    `SessionContextMap` (`runtime/context_engine.rs:31`), ephemeral by design. No KV.
  - **Durable per-client restore** ("reopen last context") → a **typed per-client
    store**, a normalized `KernelDb` row keyed by stable client-id with a typed RPC
    (`setLastContext`/`getClientView`), *not* the ephemeral registries (they `detach()`
    on disconnect, `peers.rs:148` — would silently break reattach-restore). Replaces
    the stringly 64 KB-envelope/journal/compaction KV with less machinery and real
    types. Projected as **`/v/clients`** (`docs/slash-v.md`, *Future*): a *writable*
    `/v/clients/<id>/context` is both the client's own setter and a **remote steering**
    surface (drive N tablets onto different contexts; players at them also drive).
  **Touches the slash-v V2 entry below.** *(Code-alignment pass deferred per Amy:
  land it once the design docs feel consistent + resonant.)*
- **`VfsOps::append` (or open-for-append cursor).** No append primitive today;
  `write_all`/`>>` are O(n) truncate+rewrite (`vfs/ops.rs` `write_all`;
  `MemoryBackend::write` is O(1) at `offset=size`). myaku sidesteps via bounded
  rewrite and OODA writes are turn-cadence, so this is not blocking — but an O(1)
  append would make jsonl logs and `>>` cheap. Also closes the SFTP
  concurrent-appender lost-update facet noted in the SFTP section above.
- **myaku pulse facility — RETIRED 2026-06-29, folded into beat-on-track +
  shared-state.** The standalone pulse facility (cadence + death-certificate, "one
  executor, two trigger front-ends," probes writing `/run/pulse/<x>/{now,history,
  status}` via `pulse_emit`, the `KJ_TICK`/`KJ_PULSE`/`KJ_EPOCH_NS` coords) was a
  workaround for the beat being welded to the musician transport. The **beat-on-track**
  direction (see the new top-level item below + `docs/tracks.md`) dissolves it: a
  probe is *a context attached to a system-clock track whose tick behaviour writes
  `/run`*. Its surviving pieces split — **cadence/coords/death-certificate → tracks**;
  **the `/run` output substrate + `pulse_emit` → this section's shared-state work**
  (write up the `/run/pulse/<x>/` layout here when that lands). `docs/myaku.md` was
  deleted 2026-06-29; the detailed standalone design is in git history. The app
  `DockSparkline` rewrite-to-read-`/run` note still stands.

## `/v/ctx` + `/v/session` virtual surfaces (design `docs/slash-v.md`; lands ahead of SFTP slice 3)

Two sysfs-style read-mostly `VfsBackend`s under the existing `/v` namespace
(joins `/v/docs`, `/v/input`, `/v/blobs`). Every surface (app/kaish/file-tools/
SFTP) sees them. Self-contained — no SFTP dependency — and unblocks the SFTP
capability binding (which becomes a consumer of `/v/session`). Slices:

- **V0 — `content_len` on `BlockHeader` (prerequisite, not `/v`-specific).** Block
  byte size isn't stored, so a naive `getattr` would materialize the whole CRDT
  body (a 5 MB tool result re-allocated for `ls -l`). Add an additive `content_len`
  field (`kaijutsu-types/src/block.rs:134`), set on write/merge → O(1) size. CBOR
  schema bump is additive/fail-loud. Slice V1 depends on it.
- **V1 — `/v/ctx` read-only backend.** Contexts + CRDT block logs:
  `/v/ctx/by-id/<id>/blocks/by-id/<key>/{role,kind,status,content,json,...}` with
  **flat kind-conditional** attrs (tool-call: `tool_name`/`tool_input`/
  `tool_use_id`; tool-result: `exit_code`/`is_error`/`call->`) — *not* nested
  `tool/`·`error/` dirs (locked 2026-06-26) + `by-time/NNNN` symlink view
  (stable-id-primary; never iterate `BlockId` order as timeline — use
  `block_ids_ordered()`). Contexts from `list_all_contexts()` (`kernel_db.rs:1680`);
  blocks from each **per-context** store in `documents: DashMap<ContextId,
  DocumentEntry>` (`block_store.rs:182`) — no global filter. `generation` ←
  `DocumentEntry::version()` (`block_store.rs:153`, bumps on local write + remote
  merge), not a bespoke map. Scalar files + opt-in `json`; relationships as
  symlinks; `0`/`1` booleans. `EROFS` writes; size from `content_len`; `read_all`
  override (sizing gotcha). No shard (paged readdir + `/v` being a deliberate
  destination handle scale); `live/<label>` (KernelDb-unique, short-id fallback)·
  `by-type/`·`by-lineage/` (from `context_edges`) are convenience views. Testable
  via `kaish ls /v/ctx`.
  - ⚠️ **Deferred optimization (slice 1 ships naive):** `block_ids_ordered()`
    (`block_store.rs:199`) re-sorts the whole context per call and caches nothing,
    so `ls -l .../by-time/` is O(N²·log N) (one `readdir` + N `readlink`s = N+1
    sorts). Fix later: backend cache of the ordered `Vec<BlockId>` keyed on
    `DocumentEntry::version()`. Known-slow on large contexts until then.
- **V2 — `/v/session` read-only backend.** View over a live participant registry
  (generalize `PeerRegistry` to carry session *kind* — `PeerInfo` has no such field
  today, `peers.rs:50`; app/MCP already registered). `self` resolved per-surface at
  adapter altitude. `context` renders the session's **live** acting context from
  `SessionContextMap` (`context_engine.rs:31`) — **not** KV (retired; the durable
  per-client restore is a separate typed store, above). `/proc`-style ephemeral;
  reconnect-flicker visible (see peer-reattach tech-debt). *(NB: the `bound`/binding
  capability apparatus this entry predates is retired — slash-v.md now uses
  per-operation join on the ambient `context_id`; V3 below is stale on that point.)*
- **V3 — writable `bound`.** `set_bound(session, context_id)` + route privileged
  writes through the shared `context_allows_rc_write` guard. The guard already keys
  on `ctx.context_id` (`guard.rs:71`) — `guard.rs`/`binding.rs` unchanged. The real
  work is the SFTP **consumer** side: `SftpSession` (`sftp.rs:107`, holds only
  `Principal`) gains a guard handle + `bound_context_id`, and the lexical
  `privileged_write_denied` (`sftp.rs:234`) is replaced by a real guard call in
  every write handler; the `ln -s` symlink is only the setter. Join point with
  SFTP slice 3 (setter + sliding TTL + fail-loud deny). App/MCP keep `context switch`.

Locked 2026-06-26: `live/<label>` keying; flat kind-conditional block attrs;
`content_len` on `BlockHeader`; `generation` ← `DocumentEntry::version()`; slice 1
naive (ordered-id cache deferred, above). Open: huge-`content` range-read vs cap;
slice-3 sliding-TTL storage home (`PeerInfo` carries no expiry today).

## Instrument reframing & RC stances (follow-ups from the 2026-06-22 pass)

The pass that reframed kaijutsu as an instrument, rewrote the rc create-stances,
and renamed `composer→musician` / `explorer→toolie` left these threads open:

- **rc DRY — whole-file done, stance-fragment deferred (2026-06-22).** The
  duplicated whole-file scripts (3 cache bodies × 5 types, broad binding ×3) are
  now single canonical bodies under a `lib` context_type, composed in by
  init.d-style symlinks (`ConfigCrdtFs` follows; seed format = a seed file whose
  body is just a target path; `kj rc` is link-aware). What's *not* done:
  factoring the shared collaborator ethos out of the `coder`/`director`/`mcp`
  stances — assessed and **dropped**, the reframe made those genuinely distinct
  voices with only faint shared sentiment, not a verbatim block worth a fragment.
  If a real shared stance fragment emerges later, the mechanisms exist: a symlink
  for a verbatim fragment, or a `.kai` that `cat`s a shared file and emits a block
  when content must branch on the bound model (proven by the `coder` stance).
- **Toolie taxonomy:** today's `toolie` is the read-only kind (kaibo-explorer
  style). Add a second, Edit-capable toolie that does bounded editing work —
  distinct binding + stance.
- **Future `composer` context_type:** a musically-enabled *synth director* that
  drives many `musician` contexts interactively. The name is now free (the old
  beat-voice `composer` became `musician`).
- **`orchestration.md` needs a fuller rewrite:** stale persona content (personas
  yanked 2026-05-02) and example `explorer` labels remain; only the top-level
  framing was moved off the control register this pass.
- **README doc-table** repoints to `docs/instrument-design.md` in the working
  tree but is uncommitted until that doc lands.

## Architecture & System Design

- **Hardware I/O out of the kernel process (audio + MIDI):** lean (2026-06-30) —
  the kernel/server binary should own *no* audio/MIDI FFI; hardware emission lives
  in peripherals that attach over RPC (the Bevy app, or a headless edge-node
  agent = `midi.md`'s "first kernel-owned compute node"). Consequence: extract the
  already-shipped `AlsaMidiOut` from `crates/kaijutsu-server/src/render.rs` into the
  edge-node agent so the server stops linking `alsa` (the speculation-lead `at`
  scheduling travels with it). Sequence with M4 edge-node work. PCM samples follow
  the same rule from the start — see `docs/pcm.md`.
- **SSH shell subsystem (`kaijutsu-shell`):** give an `ssh` user an interactive kaish
  with `kj` that starts in a lobby and attaches into contexts (VFS reflows on switch).
  Design + wiring captured in [`ssh-shell.md`](ssh-shell.md). Start after the SFTP
  read-path work settles (shared subsystem plumbing). Open decisions noted there:
  per-principal home vs shared lobby anchor (copy the `lost+found` `ensure_*` pattern —
  *not* the global-singleton `scratch` context), and whether `Send`-ness lets it run
  SFTP-style or needs the RPC dedicated-thread treatment.
- **VFS facade delegation:** `Kernel` implements `VfsOps` directly (`crates/kaijutsu-kernel/src/kernel.rs:984`) as a facade. Backend multiplexing already exists — `MountTable` impls `VfsOps` over `MemoryBackend`/`LocalBackend` (`crates/kaijutsu-kernel/src/vfs/mount.rs:261`). The open question is whether the `Kernel`-level facade should delegate more to `MountTable` (and what stays on `Kernel`), not whether to build a manager from scratch.
- **Server RPC Modularization:** `crates/kaijutsu-server/src/rpc.rs` is a massive file (~301KB / ~7,000 lines — by far the largest in the server). The monolithic implementation of the Cap'n Proto traits should be split into smaller modules by domain (e.g., `rpc/vfs.rs`, `rpc/llm.rs`, `rpc/mcp.rs`).
- **`context_type` stringly-typed — beat-decomposition done; newtype declined
  (`docs/chameleon.md` → "context_type is an rc bundle of features", 2026-06-28).**
  `context_type` is a bare `String` duplicated across ~6 struct defs
  (`kernel_db::ContextRow`, `kj/rc.rs`, `kaijutsu-client` rpc+actor,
  `kaijutsu-mcp::models`, the time-well card), crossing SQLite, capnp, and rc-path
  resolution. The **beat** coupling — the 3 `== "musician"` sites that gated arming
  — is **resolved 2026-06-28**: the create-arm moved into `musician/create/
  S20-arm.kai` (deleting the duplicated Rust arm in both `rpc.rs` and
  `kj/context.rs`), and the arming verb's gate (now `kj transport attach`) is
  "has a track lane", not a type name. Zero beat-related string checks remain, so the `ContextType(String)`
  newtype is **not worth doing** (declined, not deferred). Remaining `"musician"`
  literals are inert (rc-path strings, test fixtures, the `seed_scripts` assertion).
  The live follow-ons are the *other axes* (decouple-Act-from-ABC; per-type
  `BeatPolicy` so `funkMusician` isn't stuck on `musician_default()`), tracked under
  Hyoushigi. Do NOT make `context_type` a closed `enum`: it names an open
  **rc-bucket directory** (`project_rc_lifecycle`).
- **Cap'n Proto Schema Clarity (doc-only):** The `BlockKind` vs `ContentType` boundary is already settled — `BlockKind` is the structural DAG role, `ContentType` is the raw MIME rendering hint. Remaining work is purely to write that distinction into `kaijutsu.capnp` as schema comments so it stops reading as overlap.
- **Context-type tool policy (unified governance):** The `kj` surface is now
  capability-gated — escalation-relevant verbs check the caller's loadout via
  `KjDispatcher::require_cap` (five authority caps: `drive`/`fork`/`drift`/
  `transport`/`operator`, plus reuse of `rc-write` and the `builtin.block`/
  `builtin.policy` tool caps). `kj` was previously an ungated hole behind
  `facade:shell`. Remaining:
  - Dynamic / principal-scoped overrides.
  - Self-lockout ergonomics (narrowing binding to exclude `builtin.bindings`).
  - Per-principal budgets + fair queuing.
  - **Live contexts need re-create/restart:** broadened role loadouts only reach
    newly-created contexts; existing ones keep their old (now authority-less)
    binding until they're re-created or the kernel restarts. (Editing the seed
    via `kj rc edit` / `kj rc reset` changes what *new* contexts get, not live
    ones — rc fires at lifecycle boundaries, not retroactively.)
- **RPC session reaping — mostly closed (2026-06-14).** The original report
  ("warned every 60s for 21+ hours that session `019eb229` is still active —
  no reap path") was two conflated problems, both now addressed:
  - *Reaping* is handled at the transport layer by SSH **keepalive**
    (`ssh.rs` server `Config`, added 2026-05-24: 30s × 3 ≈ 90s dead-peer
    detection — postdates the 21h zombie). A vanished peer's transport now
    EOFs, `rpc_system.await` returns, and `ConnectionState::Drop` removes the
    `session_contexts` entry. Verified on the live server (booted 2026-06-13):
    every session that was ever warned about *ended cleanly*, including ones
    open for hours; no surviving zombie.
  - *The watchdog was a false alarm.* `run_rpc_watchdog` logged `WARN ...
    still active` every 60s for the entire life of **any** session, so a
    healthy hour-long connection emitted ~58 lines — it could not tell a
    long-lived session from a wedge, burying the one signal it existed to
    surface. Now **activity-gated** (`ssh.rs`): an `ActivityStream` wrapper
    stamps a timestamp on every byte read/written, and the watchdog warns only
    when a connection is open but idle past `RPC_IDLE_WARN_THRESHOLD` (120s,
    above the keepalive reap window).
  - **Residual (by design, low):** a *truly* wedged `current_thread` LocalSet
    (a blocking handler) can't be force-killed from outside without thread
    injection, and the in-thread watchdog goes quiet with it — that silence is
    the only remaining signal. Not worth chasing until it actually recurs.
  - Related: auto-memory `tech_debt_peer_reattach_on_reconnect` (app doesn't
    re-attach after kernel restart). Original find: 2026-06-11 journaling
    forensics.
- **Move post-reconnect re-sync orchestration into `kaijutsu-client` — detection DONE,
  re-fetch delivery remaining (2026-06-24).** Goal: **client owns sync orchestration,
  app renders** (editor's Design A pushed one boundary out; refines
  `feedback_thin_client_smart_kernel` at the app/client seam).
  - **DONE:** reconnect *detection* now lives in the actor — `enter_connected` emits
    `ServerEvent::Reconnected` on a Connected-after-drop (`bound_kernel_id.is_some()` is
    the free signal; never the first connect). The app just reacts (bumps
    `SyncGeneration`); the `ReconnectTracker` fold + its app unit tests are gone. The
    cuttable-proxy e2e asserts the actor emits `Reconnected`.
  - **DONE — re-fetch delivery:** `enter_connected` now spawns a re-fetch of the joined
    context's `get_context_sync` and emits `ServerEvent::ContextResynced { sync }`; the
    app merges it via `apply_sync_state` (and marks the doc fresh so
    `check_cache_staleness` won't re-fetch it). The e2e applies the *delivered*
    `SyncState` and asserts it reconstructs the gap block — the whole client-side loop,
    not just the signal. The **multi-context wrinkle** resolved as a clean division (not
    a dual path): the actor eagerly delivers its *joined* context; `Reconnected`'s
    `SyncGeneration` bump is the *coarse* backstop that re-syncs *non-joined* cached docs
    lazily on next view (+ the lag case). The joined+active context is covered by the
    eager delivery, so no redundant fetch in the common case.
  - **CLEANUP:** there are now **two `SyncGeneration` types** — `kaijutsu-client`
    (`subscriptions.rs`, doc-commented "bumped on lag or reconnect", currently unused)
    and the app's own (`actor_plugin.rs`, the wired one). The client one was clearly
    meant for this; fold the app's into it (or delete the dead one) as part of moving the
    cache. (Moving the whole `DocumentCache` into the client is the bigger refactor.)
- **LLM providers:**
  - Move per-model knobs out of the config layer (`models.toml`), into the app.
  - Push subscriber for `ConversationMailbox`.
- **Reasoning-continuity cross-provider guard (policy, not Rust):** cross-turn
  thinking now rehydrates — `BlockSnapshot.signature` is an opaque "rehydratable"
  token (real Anthropic/Gemini sig, or a DeepSeek nonce), set on `ThinkingEnd`,
  persisted (CRDT snapshot + Cap'n Proto wire), and `hydrate` re-emits *signed*
  Thinking as `Reasoning` (one block per thinking block). Remaining: block
  `kj context set --model` across provider families when signed Thinking exists
  in history (a DeepSeek nonce fed to Anthropic 400s); allow the transition only
  at `fork`, where an rc script decides to elide thinking or downgrade it to
  plain blocks.
- **Cold start seeds no binding-admin context (want a ROOT director).** The
  bootstrap (`kaijutsu-server/src/rpc.rs:1369`) seeds exactly one **`coder`**
  context (`genesis`) when the kernel comes up with zero contexts — nothing with
  `admin`/`rc-write`. Consequence: any binding-admin op (e.g. repairing a live
  context whose loadout came from a stale seed — see the stale-rc entry under
  Control Plane — or running `kj rc reseed`, which needs `rc-write`) requires
  manually `kj context create <x> --type director` first, since only rc-privileged
  callers or an `admin`-capped context can widen another's loadout, and no
  user-facing shell is rc-privileged. Want: a fresh kernel seeds a **ROOT
  director** (the `director` type already grants `admin`+`rc-write`). Design
  wrinkle: a `director` loadout has **no `drive`/`fork` authority**, so ROOT can't
  itself be the conversational default the app opens into — either seed *both*
  (ROOT director + a usable coder), or have ROOT spawn the coder and let the app
  default to the coder. Confirmed not implemented as of 2026-06-13; genesis was
  repaired by hand this session via a throwaway director.

## Drift — June 2026 audit

- **Extract `ContextRegistry` from `DriftRouter`:** DriftRouter carries ~7
  responsibilities (context registry, per-context LLM config, staging
  queue, dead-letter queue, lost+found lifecycle, context state, trace-ID
  assignment) — `drift.rs:172-563`. Everything that needs "what contexts
  exist" takes a dependency on drift, inverting the hierarchy. Pull
  register/resolve/list/llm-config/trace-id into a `ContextRegistry`;
  drift keeps the queues. Cold-start hydration (`rpc.rs:1150-1183`) moves
  with the registry. (Considered 2026-06-13; deferred — it's a cohesive
  multi-file extraction touching drift.rs + rpc.rs + every "what contexts
  exist" caller, best done when the kernel isn't under concurrent edit.)
- **`drift_flush` is non-atomic over the router lock:** takes the write
  lock four separate times (`kj/drift.rs:422`, `:510`, `:516`, `:521`),
  allowing interleaving with concurrent stage/cancel between windows.
  Document why that's safe or restructure drain→requeue as one critical
  section. (The suspected lock-across-await is NOT real — db lock at
  `:455-471` drops before the `:487` await.)
- **`kj/drift.rs` orchestration bloat:** push/pull/merge/flush each inline
  variations of "insert drift block + record edge + run rc lifecycle".
  Extract the shared operation; the command layer should dispatch, not
  orchestrate.
- **Drift distillation half-integrated:** `build_distillation_prompt`
  machinery sits behind a "drift engines removed" comment + TODO
  (`drift.rs:602-665`). Decide: integrate or delete.

## Turn Loop (kaijutsu-server/src/llm_stream.rs) — June 2026 audit

- **Decompose the agentic loop** (after FlowBus settles; they share event
  paths): mailbox catch-up/snapshot (`:341-391`), cache-breakpoint policy
  via ad-hoc DB reads (`:500-511`), one-shot image resolution that goes
  stale across tool iterations (`:403`), dual-layer timeout semantics
  (`:603-634`) are all inlined in one ~1,235-line file.

## Cleanup — June 2026 audit

- **App-side ABC parse failure renders `Tune::default()` silently**
  (`kaijutsu-app/src/text/rich.rs:413-423`) — render the kernel's
  structured ABC error spans instead. Also: the app re-parses ABC on every
  view; consider a cached AST keyed on block content version.

## Persistence & Sync

- **CRDT-owned config/rc (design: `docs/config-crdt-ownership.md`).** CRDT is the
  sole owner of config + rc; embedded Rust seeds it once — no host-disk
  write-through/reload, which **deletes** the dual-ownership silent-fallback cluster
  for these mounts by construction (supersedes the `MountBackend::read` stale-bytes
  serve, `append` wipe, `LocalBackend::setattr` mtime no-op, and stale-rc-seed
  entries elsewhere here, for the rc mount).
  - **Slice 1 (rc) — ✅ SHIPPED 2026-06-16** (`debfb33`/`2b763c6`/`49c819a`/`a2c1045`):
    `ConfigCrdtFs` VfsOps backend (`UUIDv5(path)→DocKind::Config`, `documents` table
    *is* the readdir manifest), seeded from embedded; `/etc/rc` remounted on it; `kj
    rc` + `load_rc_scripts` route VFS-direct. ⚠ **Live runner verification pending**
    (needs a server restart).
  - **Slice 2 (config TOMLs) — ✅ SHIPPED 2026-06-17** (`93c72a7`/`fdd1c18`/`9e581aa`/
    `a30b266`/`6f2ce9f`): `ConfigCrdtBackend` (debounced host flush + watcher + dirty
    tracker + disk read-back) **deleted**; a second `ConfigCrdtFs` mounts at
    `/etc/config`, seeded from embedded (or, on a fresh kernel, from a host
    `config_dir` if provided — a one-time seed source for tests, never set in
    production). Readers (models.toml, system.md) route VFS-direct; `kj config
    show/list/set/reset` is the editing surface, gated on a new `config-write`
    authority; the app fetches `theme.toml` over RPC (`get_config`) on connect.
    ⚠ **Live runner verification pending** (needs a server restart), same as slice 1.
  - Deferred: CRDT scratch mount.
- **rc cutover follow-ups (from slice 1):**
  - **DB-backed test block-store deadlocks `kj::fork` tests.** `test_dispatcher_crdt_rc`
    (DB-backed block store sharing the in-memory `KernelDb` handle) hangs the
    `kj::fork` tests — a latent lock-ordering / re-entrant-`parking_lot` issue.
    Worked around by keeping the *global* `test_dispatcher` db-less + LocalBackend;
    only rc-scoped tests use the CRDT dispatcher. Production runs db-backed and fork
    works there, so it's likely test-harness-specific — but worth a look (could flag
    a real reentrancy risk). Until fixed, the global rc test tree is still host-disk
    (`ensure_rc_seed_files` + LocalBackend), inconsistent with production.
  - **Teach `FileDocumentCache` to pass through CRDT-native mounts.** `ConfigCrdtFs`
    carries an in-memory advancing mtime purely so the cache (used by agent
    `builtin.file:read /etc/rc/…`) reloads after a `kj rc` write. Cleaner: the cache
    skips mirroring `real_path()==None` mounts entirely (read straight through),
    dropping the mtime workaround. Touches all cache consumers — separate slice.
- **Graceful-shutdown WAL checkpoint on SIGTERM:** `SharedKernelState::drop`
  checkpoints only on clean exit, but the server `run()` loop never returns and
  dies on SIGKILL/SIGTERM without unwinding, so systemd `stop` skips it.
  Proactive compaction checkpoints cover durability (no data loss); this gap
  only affects bare-file forensics between the last compaction and shutdown.
  Fix: a `tokio::signal` SIGTERM handler that checkpoints before exit (needs the
  run loop to become interruptible). Forensics hygiene: tracing logs UTC,
  systemd speaks local — cite both zones when recording restart times.
- **`KernelDb` connection pool + god-table — DEFERRED ON PURPOSE (2026-06-16).**
  Currently `Arc<parking_lot::Mutex<KernelDb>>` (`block_store.rs:74`); the file is
  one ~20-table module and every write serializes on the one lock. Recognized
  smell, **not being acted on**: the justifying pressure (measured write-contention
  under concurrent contexts) isn't expected soon, so we revisit only when it's an
  observed problem — do not pre-emptively refactor (annotated at the top of
  `kernel_db.rs`). When it does come up: the single mutex prevents using WAL for
  concurrent readers; migrating to `r2d2`/`sqlx` would allow non-blocking reads
  during LLM streams. Note SQLite serializes *writes* regardless of pooling, so
  the win is concurrent reads (WAL only) — verify WAL first; narrowing lock scope
  may matter as much.
- **Config CRDT ops:** config docs (`DocKind::Config` on `ConfigCrdtFs`) need DTE
  integration so config/rc changes replicate across peers.
- **Theme hot-reload-on-edit (slice 2 follow-up):** the app fetches `theme.toml`
  over RPC only on connect (`apply_theme_from_rpc`). A live `kj config set
  /etc/config/theme.toml` won't re-theme a running app until reconnect. Closing it
  needs the app to subscribe to the config doc (or a config-changed notification)
  and re-fetch. Low priority — theme edits are rare and a reconnect already picks
  them up.
- **`kj config` help doc:** add `crates/kaijutsu-kernel/docs/help/kj-config.md`
  (parallel to the rc/cache help docs) once the surface settles.
- **`blocks_ordered()` allocation churn + sort:** `block_store.rs:185-188` calls `order_key().to_string()` for *every block*, then `sort_by` on the strings — so it's O(N log N) **plus a String allocation per block per call**. It runs on per-frame hot paths (`kaijutsu-app/src/ui/card_stack/sync.rs:48`, `view/components.rs:163`), so the allocation churn is likely the bigger cost than the asymptotics. Fixes: compare `order_key` without stringifying, and/or cache the ordering and invalidate on block change. Add a secondary sorted index when scale demands.
- **Latch state should persist with the context:** 
  - `set -o latch` mode is per-shell and lost on restart.
  - Latch nonces should eventually live in a SQLite table rather than in-memory.

## User Interface (kaijutsu-app) & UX

- **Rename `BlockScene` → `BlockContent`:** the component no longer holds a
  scene (scene + `built_*` live on `VelloUiScene`); it's now pure build-
  bookkeeping (`content_version`/`last_built_version`/`scene_version`/`text`/
  `color`). Name is misleading. Mechanical rename across `block_render.rs`,
  `lifecycle.rs`, `overlay.rs`, `shell_dock.rs`, `render.rs`.
- **Verify two unexercised render surfaces:** (1) a Vello-content *cell*
  (ABC/SVG/sparkline, `has_vello_content == true`) rasterizing via
  `render_vello_scenes` then compositing MSDF labels on top — needs a
  conversation with rich content; (2) the unfocused-pane summary, the one
  surface on Bevy's native `Text` pipeline (`tiling_reconciler`), needs a
  multi-pane layout. All MSDF-only surfaces + docks + role borders verified.
- **Vi editor command mode — `:` dialect (Slice 3, `docs/vi.md` → *Command mode*).**
  Steps 1+2+3 **shipped** (core verbs `:w/:q/:wq/:q!/:x/:w!`; `:s`/`:%s`/`:N,Ms`;
  `:r <file>`/`:r !cmd`; Ctrl+Z-suspend / `fg`-resume). Design notes that held:
  surface stays kernel-owned (app forwards every key); the dialect is its own
  thing (Rust-regex `:s`, not vim BRE); intent pattern
  (`CommandRequest`/`take_commands`), **not** modalkit's command machine.
  **Slice-3 polish landed 2026-06-27** (headless green, **runner-verify pending** —
  capnp `@6` ⇒ kernel+app rebuild+restart, eyeball `:r !cmd` splice, a bad `:cmd`
  showing E492 on the strip, and `fg` from a second window):
  - **`:r !cmd`** now shells out in the **opener's** context (was fail-loud-deferred).
  - **Opener capture fixed** — `EditorOpener { principal, context_id, session_id }`
    captured at builtin construction (the old `ToolCtx`→kaijutsu-`ExecContext`
    downcast was a type mismatch that *always* failed, so `fg` only ever hit the
    most-recent-of-any fallback). `fg` now targets the caller's own session.
  - **Bad `:cmd` / bad `:s`** report on `EditorState.message` (vim E492) and keep
    the session open, instead of erroring `editor_keys`.
  - **`:w!` == `:w`** (decided 2026-06-27) — `force` reserved for a future
    changed-under-us guard (concurrent-writer detection), not a permission gate.
  - **Step 4 — `:e <path>`** (rebind the session to another block) — deferred.
  - Related separate thread: the Ctrl+Z shell may become a **shadow context**
    (blocks excluded from the conversation until drifted) — simplify-by-construction,
    its own design pass; `project_shadow_context_shell` memory.
- **Vi editor — residual `config_owned` prefix on the cache-invalidation path.**
  `resolve_editor_target` now decides config-ownership via the mount table
  (`MountTable::owner_of` + `VfsOps::owns_config_docs`, 2026-06-27), but
  `Kernel::invalidate_config_file_cache` still uses the hardcoded `config_owned`
  prefix check. It's the **sync** guard on the sync `editor_quit` path; routing it
  through the async mount-table query would cascade `editor_quit` (+ its wire
  handlers) to async. Unify when that path is reworked, or add a sync
  mount-ownership lookup. Low stakes (cache-coherence optimization), but it's a
  second source of truth for config-ownership.
- **User presence (novel surface):** The compose input is a shared CRDT document. Surfacing in-flight compose state to an opted-in model would enable mid-sentence collaboration. Gate with explicit user opt-in.
- **Connection Polling Efficiency:** `ActorPlugin` in `crates/kaijutsu-app/src/connection/mod.rs` polls broadcast channels every frame. While `UpdateMode::reactive` helps, consider event-driven wakeups or bridging async streams directly into Bevy events more efficiently if latency/power becomes an issue.
- **Card-stack view:** Card size tuning, read-only scroll on focused card, dive-in (Enter), mouse click to focus, momentum scrolling, camera parallax, streaming card texture updates, card grouping evolution, ambient environment.
- **Card-stack texture quality (3D direction):** the renderer presents vello/MSDF
  content as textures on cards, so the 3D move brings (a) **mipmaps** on block/card
  textures — cards receding in perspective shimmer without them; (b) **reading-mode
  hi-res re-render** — promoting a card close to the camera re-renders its content at
  higher resolution (discrete, debounced — same machinery as re-render-on-change);
  (c) **MSDF live-quad escape hatch** — MSDF's scale-independence is spent at bake
  time, so if reading-mode text quality disappoints, render MSDF as live quads in the
  3D scene (the atlas + shaping pipeline already support it; a renderer change, not
  architectural). Arbitrary zoom over vector content is explicitly declined.
- **Text rendering (MSDF / 次):** TAA temporal super-resolution, glyph spacing per-font tuning, 1-frame blank flash on texture resize, large-context Vello "paint too large" crash.
- **Auto-follow on local submit:** the conversation only re-engages
  scroll-follow when already at the bottom
  (`view/sync.rs:200-206`); a shell-dock submit is a strong signal of
  intent to watch the result — force `start_following()` on local
  submits (mirror the `InputCleared` handler at `sync.rs:309`). A
  "new content below" affordance would cover non-local appends.
- **Stale GpuImage-preparation comments:** "ImageNode ensures the
  GpuImage is prepared" (`view/lifecycle.rs:258`,
  `view/block_render.rs:877-878`) is not how Bevy 0.18 works — GpuImage
  prep is `AssetEvent`-driven with an inherent one-frame delay (the
  benign single "MSDF render skipped … target_gpu=false" warn per cell).
  Correct the comments so the next renderer investigation doesn't chase
  the wrong layer.
- **Error blocks stick to the bottom of the screen and obstruct new content
  (observed 2026-06-17, THE_DIRECTOR session `019ed674`).** `system/error`
  blocks render pinned to the bottom of the conversation view; as new content
  arrives they don't scroll away with it and start occluding live output. The
  ordering *is* correct in the CRDT — after an app restart the same errors
  re-sort into their proper timeline position — so this is a view-side
  sort/placement bug (errors are laid out by a different key than their tick),
  not a data bug. Low priority for now; logged to revisit.
- **Triple-Esc does not interrupt a running agentic loop (observed 2026-06-17,
  same session).** Tapping Esc three times while a context was mid-drive
  (autonomous turn / tool loop) did not cancel or interrupt it — the loop ran to
  completion. Expected an abort path on rapid-Esc analogous to the
  double-tap-dismiss pattern. Need to wire a keyboard interrupt that reaches the
  in-flight drive/turn (InterruptState → kernel turn cancellation), not just the
  compose overlay.

## Control Plane & Navigation (kj)

- **Latch confirmation nonce is emitted on stderr (prose-only) — make it
  machine-readable.** Found 2026-06-28 trying to script a batch `kj context
  remove`. The latch is an explicit two-step *machine* protocol: the first call
  returns a nonce you must capture and pass back via `--confirm`. But the nonce
  is buried in human-prose **stderr** ("To confirm, run: … --confirm <nonce>",
  exit 2), so any automation must `2>&1` and regex-scrape it — a footgun (cost us
  a failed batch loop; nothing wrong with kaish, the prompt just wasn't on
  stdout). Fix: surface the nonce in the result's structured `data` field
  (`data.confirm_nonce`, per the kj structured-data convention) so a caller reads
  it structurally. Applies to every latched verb (`context remove/archive/retag/
  move`, …) — they share the latch helper, so it's one change at the source.
- **Workspace path mount points:** `kj workspace add --mount <target>` was
  documented + parsed but silently ignored (no backing storage) — removed during
  the clap migration so it now fails loud. To implement: add a `mount` column to
  `WorkspacePathRow` (`kernel_db.rs:168`, SQL migration), thread it through
  `workspace_add` and the context-mounting path, decide mount semantics, then
  re-add the `--mount` flag + help example.
- **Tab completion:** Context labels, preset labels, workspace labels, tag syntax. Integrate with kaish.
- **Cross-kernel drift:** Schema preserves `kernel_id` everywhere; not yet implemented.
- **Compact quality:** Distill model selection, preset-level or context-level summary-style control.
- **POSIX context quartet:** Implement `kj wait` and `kj stop` to complete the fork/drive/wait/merge paradigm.
- **`kj drive` follow-up:** Add verb-level refusal for driving Staging contexts.
- **Autonomous turn runaway guard:** Add a `drive_depth` cap to prevent unbounded fan-out from `--prompt` forks.
- **TurnFlow bus lossy + in-memory:** overflow eviction is now LOUD (`FlowBus::publish` warns when a full channel drops a slow subscriber's oldest event, `flows.rs`); the zero-subscriber case was already surfaced by `kj drive`/`kj fork --prompt`. Durable delivery (persistence) for `turn.*` remains the follow-up.
- **Headless turn cwd is `/`:** Decide whether to thread the context's stored shell cwd into the headless `ExecContext`.
- **`--switch --prompt` double-drives:** Clarify semantics when both human and autonomous turn try to drive a child.
- **Context-type ↔ fork asymmetry (discovery 2026-06-17, fork code is fresh —
  worth a code-side look).** `--type` exists only on `kj context create`
  (rc-dispatch `context_type` → selects which `/etc/rc/<type>/` bundle runs), NOT
  on `kj fork`. Fork inherits the parent's type and re-runs the *parent type's*
  `fork/` bundle, so **there is no way to fork into a different type** — switching
  type means `kj context create --type <T> --parent <src>`, which gives a
  structural edge but (apparently) none of fork's history/preset copy semantics.
  Observed: a `context create --parent .` shows `Fork: <id> ()` — empty parens
  where `kj fork` shows the preset (e.g. `Full`/`Window`). Open questions for the
  fork/create code (`kj/fork.rs`, `kj/mod.rs` context_create, `rpc.rs`
  create_context_inner): (a) is the type-on-fork omission deliberate or just
  unbuilt? (b) does `context create --parent` copy ANY blocks, or only wire the
  DAG edge — i.e. does a director created this way see what it needs to coordinate,
  or start blank? (c) should `kj fork --type <T>` exist (fork history + run the
  *target* type's create/fork bundle) for the common "branch this work into a
  director/toolie" move? Surfaced while standing up a `director` context to
  experiment with coordination.
  - *Reconfirmed 2026-06-17: the child's block log was its own rc output (`system/text` stance,
    `system/notification` tool-adds, S10/S20 rc traces) plus the seed
    `--prompt`; **zero blocks copied from the parent**. So the create-with-
    parent path starts the child blank (correct for a clean coder, wrong if
    you wanted fork's history). Strengthens the case for (c) `kj fork --type`:
    the director's natural move is "branch this work into a coder *with* the
    working context," which neither verb currently does in one step.
- **`$HOME` env var is empty in the context shell (minor; `~` now fixed).**
  `~` expansion is fixed by a kaish upgrade (2026-06-17), so `~/path` resolves.
  The remaining gap is the `$HOME` *variable*: it's still empty in both the
  read-only and full shell, so `$HOME/path` resolves to nothing while bare `~`
  works. Decide whether the headless `ExecContext` should seed `HOME` from the
  context's stored cwd/user (adjacent to "Headless turn cwd is `/`" above) so the
  variable and the tilde agree. Absolute paths always work.

### kj / MCP ergonomics (UX)

- **MIDI / blob readback is a two-step with no single tool.** A derived block
  (e.g. ABC→MIDI sibling) stores a 32-hex CAS hash as its `content`, and the
  block is `ephemeral` (won't hydrate into the conversation). Retrieving the
  bytes is: enumerate blocks (`kj block` / `kaijutsu://docs/{doc}`) → find the
  `audio/midi` sibling → read its hash → `kj cas get <hash> --out <file>`. There
  is no MCP/`kj` "give me the rendered artifact for this turn" affordance. Add a
  `kj block cat`/blob-by-block helper (and/or an MCP resource that resolves a
  block's CAS content) so consumers don't hand-assemble the hash lookup.
- **Stale rc seed → missing authorities (per-file upgrade is now `kj rc reset`).**
  rc scripts live as host files under `~/.config/kaijutsu/rc/<type>/<verb>/`; the
  deployed tree is the live source of truth and boot only bootstraps it when
  fresh (2026-06-13 model change), so a pre-existing seed file never auto-upgrades
  to a newer embedded default. Symptom (2026-06-13): a fresh `mcp` context had the
  old 125-byte binding (`*` + `facade:*` only), missing the
  `drive`/`fork`/`drift`/`transport`/`operator` authorities the current embedded
  binding grants — so it could not run `kj transport` or self-widen (`allow` needs
  a binding-admin/rc context). **Targeted fix: `kj rc reset
  /etc/rc/mcp/create/S10-binding.kai`** (restore that file from its embedded
  seed). Remaining gap: nothing *detects* a live seed has drifted behind the
  embedded default — `reset` is a manual pull, by design (live is truth). A
  staleness indicator (compare live body vs `seed_body()`, e.g. in `kj rc list`)
  would surface "this file is behind its seed" without reintroducing auto-overwrite.
  Recurred for `coder`/`genesis` 2026-06-13 (same 2-line seed). The live-context
  half is worse than the seed half: `reset`/`reseed` only fix *future* contexts —
  a context already created from a stale seed keeps its broken loadout and can only
  be repaired from a binding-admin context, which the cold-start bootstrap doesn't
  provide (see "Cold start seeds no binding-admin context" under Architecture).
- **`local` is a kaish reserved word (like `set`).** `--model local` lexes as
  the `local` builtin keyword → `found ';' expected identifier`. Same class as
  the `set` reserved-word gotcha; quote it (`--model "local"`) or pass the full
  spec. Consider letting reserved words bind as plain args after a flag.
  (kaish-lexer change in `~/src/kaish`, not kaijutsu-side.) NOTE: alias
  *resolution* is now fixed — `kj context create/set --model "local"` expands
  the `models.toml [model_aliases]` entry to its concrete `provider/model`
  before storage (`resolve_context_config`, 2026-06-14), so the quoted form
  works end-to-end; only the bare-`local` lexer footgun remains.
- **Local-model turn watchdog — mostly closed; two narrow gaps remain (re-triaged
  2026-06-16).** The original report ("small local model + full tool palette emits
  a thinking block then stalls — GPU cold, no `Completed`, no error, turn never
  terminates", observed 2026-06-13) was addressed on two fronts, both verified in
  code at HEAD (not re-reproduced):
  - *Watchdog already exists* (landed `3fdcf79`, 2026-05-10 — a month **before** the
    report). The turn loop has a dual-layer timeout: `llm_idle_timeout` (30s) wraps
    **every** `stream.next_event()` (`llm_stream.rs:912`,`:944`) and
    `llm_request_timeout` (300s) is the total cap (`:903`,`:934`); both emit
    `StreamEvent::Error` → `TurnFlow::Failed`. Tool calls are individually capped at
    `TOOL_TIMEOUT_SECS` 120s + interrupt propagation (`:1361`), and they run in a
    per-tool loop (no unbounded collective). So the mid-turn cold-GPU stall the
    report describes **should fail loud within 30s at HEAD**. `TimeoutPolicy` is
    kernel-wide (`kaijutsu-types/src/timeout.rs`); per-model/per-context overrides
    are the open knob if 30s/300s ever prove wrong for a slow local model.
  - *The trigger was also removed* — the musician/player rc loadout is now tool-free
    (see "Musician `kj` loadout — tool-free" under Hyoushigi), so small players no
    longer get the full palette that provoked the stall.
  - **Residual (small, genuinely unguarded):** (a) the `provider.stream()` start
    `.await` (`llm_stream.rs:815`) has retry/backoff but **no explicit timeout** — a
    provider that accepts the connection but never returns the response object leans
    on reqwest's defaults; (b) pre-stream hydration / cache reads have no timeout, so
    a wedge *before* the stream loop emits no terminal event. Both are off the
    mid-turn path the report hit. Fix each with an explicit timeout + a regression
    test that wedges the path and asserts a loud `TurnFlow::Failed`. Still worth:
    make per-provider/per-context `default_tools` the norm so players never get `all`.
- **P3 — external `mcp__kaijutsu__shell` `data` needs a persisted block field.**
  The *in-kernel* `builtin.shell` now carries kj's `.data` in its `structured`
  envelope (shipped 2026-06-14, `mcp/servers/shell.rs`), and `kj <cmd> --json`
  returns the payload in stdout for any consumer. The remaining gap is the
  *external* `mcp__kaijutsu__shell`, which observes the result via CRDT sync
  (polls a block snapshot, reads `snapshot.output`) rather than a return value.
  Root cause (traced 2026-06-14): kj sets `ExecResult.data` (a kaish `Value`),
  but the server's `shell_execute` only persists `ExecResult.output()`
  (`OutputData`) onto the block (`rpc.rs:6104` → `set_output`), and the block
  carries only `output: OutputData` — which can't faithfully hold arbitrary JSON
  (an inspect object). Faithful fix: a new persisted `data` field on the block,
  mirroring the `.output` vs `.data` split — thread through `kaijutsu-types`
  `BlockSnapshot`, `kaijutsu-crdt` (content/document/block_store), the capnp
  `BlockSnapshot` wire (the real cost — three-binary bounce), then `set_data` in
  `shell_execute` and read it in the MCP `to_json` (`kaijutsu-mcp/src/lib.rs`).
  CBOR oplog evolution is additive (safe); capnp is the work. P3 because the
  `--json` envelope already unblocks consumers today.
- **External `mcp__kaijutsu__shell` hangs to timeout — ✅ ROOT-CAUSED + FIXED
  2026-06-17.** Symptom: after a server+app restart, *every* external shell call
  timed out — `echo hi` at 20s, `kj context list --tree` at 300s — returning a
  `block_id` but never the output (`status: "timeout"`); non-CRDT-poll paths
  (`whoami`/`submit_input`/`listContexts`) stayed responsive, isolating it to
  shell-output replication. **Root cause (not the network — it's localhost):
  executor starvation on the MCP client's single-threaded RPC LocalSet, converted
  into a *permanent silent* failure by a too-aggressive server reap.** Three
  compounding factors: (1) the MCP subscribed to block events **kernel-wide**
  (`BlockEventFilter::default()`, `context_ids` empty = all contexts), firehosing
  it with every other context's events after a restart (cold-start re-hydration +
  app's director/musician/drift traffic); (2) every delivered event woke the shell
  poll's `find_terminal`, which called `blocks_ordered()` (the `order_key().to_string()`
  per-block re-sort, see the perf entry below) under the lock; (3) `from_sync_state`
  replays the full op-log synchronously on that same thread (register_session +
  every shell call's Phase 2). Stacked on one `current_thread` executor, a
  multi-second stall is easy — and the server's FlowBus bridge **broke the
  subscription permanently on the first 5s callback timeout** (`rpc.rs` `if !success
  { break }`), so the MCP went silent for the rest of the connection with no
  re-subscribe path. Fixes shipped: **(1)** server bridge tolerates transient
  callback stalls via `SubscriberHealth` (reap only after 3 consecutive failures,
  a success resets; the load-bearing 5s timeout stays — it protects the server's
  RpcSystem); both bridge loops + unit tests. **(2)** client `resubscribe_blocks`
  primitive (same `instance` ⇒ server replaces the prior sub); MCP calls it on a
  shell-poll timeout to recover without a full reconnect. **(3)** block subscription
  is now **scoped to the joined context** (`block_events_client_and_filter`):
  handshake scopes from the re-joined context, and `JoinedContext` re-subscribes
  scoped after register_session — cutting foreign-context volume to zero (also kills
  the factor-2 `blocks_ordered()` churn for foreign events). Verified: server +
  client unit tests, `kaijutsu-mcp` `e2e_shell` (incl. sequential commands),
  `rpc_integration`/`context_sync` green. **Live verification 2026-06-17:** after a
  server+app rebuild/restart, `echo hi` (257ms) and `kj context list --tree` (285ms,
  was the 300s-timeout command) and sequential calls all returned `status: "done"`
  against a *busy* 24-context kernel (running musicians + THE_DIRECTOR ⇒ live
  foreign-context event flow) — the original symptom is gone. Note this exercised
  the **server fix (`SubscriberHealth`) + the OLD MCP client** (this session's MCP
  binary predates the build), so fix (1) — the load-bearing one — is verified live;
  the client-side scoping + resubscribe (2,3) ride in the MCP binary and are
  covered by `e2e_shell` until a session whose MCP binary is rebuilt confirms them
  in situ. Related: P3 above + `project_mcp_synceddocument_sync`.
- **mcp-context default model is an invalid id (observed 2026-06-17).** A context
  created via `register_session` (context_type `mcp`/`default`) defaulted to
  `anthropic/claude-haiku-4-5-20250101` (also seen as `…-20250929`) — a wrong date;
  the valid id is `claude-haiku-4-5-20251001`. Chat turns fail with
  `not_found_error` after 3 attempts. Fix the default model id wherever mcp/default
  contexts are seeded.
- **`builtin.file` edit/read hardening — ✅ MOSTLY RESOLVED 2026-06-17** (the
  `docs/issues.md` corruption post-mortem, THE_DIRECTOR `019ed674`). **Root cause
  (the one the original post-mortem missed):** `edit` computed match positions
  with `str::match_indices` (BYTE offsets) and `old_string.len()` (BYTE length),
  then passed them to the **character**-indexed CRDT `BlockStore::edit_text`. On
  any file with multibyte UTF-8 before the edit site (issues.md is full of `→ ✅
  改善 ≳ ×`) the offset/length drifted, so it spliced/over-deleted at the wrong
  place while honestly reporting `Replaced 1 occurrence` (the byte search *did*
  find a match). The reported contributing factors were real but secondary: (a)
  the "lying" diff preview was the CRDT faithfully rendering already-corrupted
  bytes; (b) the line-numbered `read` prefix vs whitespace-exact matching is now
  sidestepped by hashline anchors; (c) in-context recovery (no `git`/revert) is
  still open. **Shipped** (`crates/kaijutsu-kernel/src/file_tools/`):
  - byte→char offset conversion + char-count delete length (`edit.rs`
    `plan_string_edit`/`byte_to_char`);
  - **fail-loud post-write verification** — an independently-computed `expected`
    string is compared to the read-back; any mismatch fails the op instead of
    reporting false success (the directive: crash over corruption);
  - **hashline addressing** (per "The Harness Problem" / anthropics/claude-code
    #25775): `read` now prints `LINE:hash→ content`; `edit` gained an `anchor`
    mode (`N:hash` or `N:hash..M:hash`) that re-verifies the line hash before
    writing — a stale anchor fails loud with the current hashes. Subsumes factor
    (b); the model addresses a line by reference instead of retyping it;
  - CRLF terminator preservation on anchor edits; empty-file/edge-case guards;
  - unit + e2e broker tests (multibyte round-trip, anchor round-trip, stale-anchor
    fail-loud); two DeepSeek/kaibo reviews + a `/code-review` pass.
  **Remaining (small):** (1) in-context recovery affordance — expose `git`/a
  revert or `kj block diff --original` in the kaish shell (factor c, untouched);
  (2) the post-write verification reads the CRDT cache, not the VFS disk, so a
  faulty flush is only caught by `flush_one`'s own error (documented in `edit.rs`);
  (3) `FileDocumentCache` CRDT-native pass-through (already tracked under
  Persistence & Sync) would let `read`'s hashes anchor `/etc/rc` cleanly.
  - **kaish-side build-out — design direction (not yet built).** The hash is an
    *edit-addressing* feature, so the kaish read surface wants **two read modes**:
    keep `cat`/`tail`/`sed`/`grep` streaming + **hash-free** (logs/huge files; never
    materialize), and put hashes only on a **bounded, dedicated `read` verb**
    (window-scoped hash, range arg, `--json`) paired with `edit --anchor`. To serve
    **kaibo** (only has `run_kaish`), push `line_hash` *up* into the kaish crate
    (`~/src/kaish`) as a builtin; the MCP tools become thin wrappers. Rejected: a
    `hashread`/`hashedit` pair (the edit half duplicates `edit --anchor`; doubles
    standing tool-desc tokens) and `cat -H` (cat is the large-file streaming dumper —
    a hash flag invites whole-file hashing). Add a size guard so the hashline reader
    declines huge files. (Kaish-crate work, kaijutsu-driven.)

- **`StreamingBlockHandle` implementation:** Single-block streaming primitive.
- **LLM streaming rewrite:** Move `process_llm_stream` onto `StreamingBlockHandle`.
- **Block content abstraction:** Blocks as containers for multiple content artifacts.
- **MCP `progress` → `StreamingBlockHandle` bridge.**

## Domain-Specific (ABC Parser & Engraving, Index)

- **`hnsw_rs` reverse-edge quirk:** Reverse edges written at neighbour's assigned layer.
- **ABC multi-tune files vs blocks:** Split tunes across sibling blocks or stack inside one block.
- **ABC file-header inheritance:** `M:`/`L:`/`Q:` defaults prevent proper inheritance.
- **ABC features:** `I:linebreak`, `m:` macro expansion, `%%` directives, Unicode escapes/fonts.

## Viz substrate (kaijutsu-viz) — see docs/viz-substrate.md

- **Time-well HDR+Bloom — ✅ RESOLVED 2026-06-17 via a single shared camera.**
  The earlier failure (adding `Bloom` to the `TimeWellCamera` made the cards
  vanish) was the *two-camera* mismatch: an HDR 3D camera (order 0) composited
  with the app's LDR `Camera2d` (order 1, `ClearColorConfig::None`) on one target.
  Fix: the app now has **one always-on `Camera3d`** (`main::setup_camera`, marked
  `IsDefaultUiCamera`) with `Hdr` + `Bloom::NATURAL` + `Tonemapping::TonyMcMapface`.
  Bevy UI renders on it (the UI pass runs *after* tonemapping/bloom, so the
  conversation UI is untouched), and the well repurposes the same camera on enter
  (adds the `TimeWellCamera` marker + swaps the clear color) instead of spawning
  its own. No second camera, no composite, no `Camera2d` anywhere. Well cards
  (3D meshes) now bloom; the conversation is visually unchanged. Driving the
  cards' SDF rims/pulses to HDR (>1.0) so they bloom brightly is the follow-on
  (`WellCardMaterial` `params`/emissive).
- **Time-well step-4 polish (shipped 2026-06-16, `view/time_well/`):**
  - *Fixed-pitch overlap:* band slots use a fixed angular pitch (TAU/24) so
    append stays motion-free; but a band with >24 cards wraps slots onto each
    other (coincident cards → z-fight/draw-swap; `AlphaMode::Mask` mitigates the
    sort but not coincidence). Real fix for very full bands: sub-rings, smaller
    cards, or radius LOD. Band 0 is meant for ~10 so this only bites test data.
  - *Status coverage (gap 3):* ✅ RESOLVED 2026-06-17 (`df3b65b`) — not via
    `subscribeBlocksFiltered` but via a kernel-derived `ContextInfo.liveStatus`
    @14: the server reads each context's block statuses in timeline order
    (`derive_context_live_status`: any Running→Running, else tail Error→Error,
    else idle) and ships it on every `listContexts` poll. The well sets
    `Card.status` from it for every visible card, driving the rim pulse; the
    event-based `apply_block_status` is retired (single source = the poll;
    the breathe itself is continuous via `globals.time`). Thin-client aligned.
  - *Readability:* card sizing/camera zoom is functional but text is small at
    the default framing; tune when the active view (step 6) lands.
  - *Band-1 sweep direction (cosmetic taste call):* band-1 currently sweeps CCW
    from the top anchor (positive pitch). A literal clock-face vs. this
    newest-first sweep is unsettled — the recency *ordering* is settled, the
    visual sweep is one constant flip away (`scene.rs` `band1_anchor` / pitch sign).
  - *Hot rim fills only the top semicircle (cosmetic):* ~13 cards from 3 o'clock
    CCW over 0–180°; the bottom half of the screen is unused. Rebalance the hot
    start angle if it bugs you.
- **Edge HUD → in-scene MSDF panels — ✅ SHIPPED 2026-06-18.** The HUD's
  first-prototype flat Bevy `Text` nodes are now in-scene **MSDF panels**: 3D
  quads parented to the well camera (screen-stable, no billboard), drawn as thin
  glowing **accent-tinted borders** with no body fill (`WellCardMaterial.border`
  uniform), MSDF text inside — HDR/bloom + depth, same vocabulary as the cards.
  N is centered top with the context name in a larger font; E/W tuck into the top
  corners (below the status bar); S is hidden. Placed via the pure, unit-tested
  `hud_slot_offset` (aspect-adaptive, re-derived each frame; size-aware fit hugs
  the screen edge). Built on the shared `view/time_well/panel.rs` primitive
  (`create_msdf_panel` + `commit_panel_glyphs`), also used by the rim/reading
  cards. All knobs are consts at the top of `hud.rs`. Follow-ups (non-blocking):
  - The mid/lower **E/W sides are now open canvas** — candidates for the drift
    arcs / activity layer or a secondary readout.
  - The E specs panel wraps a long model badge (cosmetic).
- **RTT type rename + split — ✅ SHIPPED 2026-06-18.** `view/vello_ui_texture.rs`
  → `view/ui_rtt.rs`; `VelloUiTexture` → `UiRttTexture` (now also carries the
  content-neutral `built_width/height`), `VelloUiScene` → `UiVectorScene`
  (`{scene, version}`, vello-only). Pure-MSDF surfaces (well cards/reading/HUD,
  overlay, shell-dock) carry **no** vello type; dual-mode block cells +
  role-group borders keep `UiVectorScene`. Follow-up (optional, low): `overlay.rs`
  / `shell_dock.rs` could also adopt `create_msdf_panel`/`commit_panel_glyphs`
  for their MSDF surfaces (Phase 0 already dropped their vector type).
- **Time-well — deferred UI ideas (parked 2026-06-17, picking up the activity
  layer instead).** All real, none blocking; the active iteration is the
  base-ring kernel-activity indicator (see `viz-substrate.md` step 7.7):
  - *JOIN dive (mockup 34):* the committing Enter currently just switches
    context + leaves. The cool version continues the camera *through* the focus
    card so it unfolds into the conversation — one continuous focus→enter
    gesture. Polish ideas: fade/dim ring cards while focused; tune focus-card
    size/pos (it's large in the overview). See `viz-substrate.md` step 7.5/7.6.
  - *Clean Running-pulse re-check:* the per-context teal Running rim is
    mechanism-proven (identical shader path as the verified selection/lineage
    rims) but never caught in a clean live screenshot — the earlier attempt was
    blocked by the (now-fixed) MCP-shell hang + a bad mcp default model id. A
    ~5-sec re-check once a working-model turn can be staged. NOTE: the
    base-ring activity work below may re-tier the per-context cue anyway.
  - *Drift arcs / particle layer (gap 4):* the bigger drift visualization —
    arcs/particles *between* the source/target cards, not just the per-card
    shimmer already shipped. Needs a new context→context drift-edge *list* wire
    (the per-card shimmer rode the existing staged-queue poll; arcs can't).
- **`ScaleLinear`/`ScaleTime` round-trip loses precision under extreme
  domain→range compression** (≳10³–10⁸×): inverting through a tiny range
  amplifies f64 representation error past any sane tolerance. This is an f64
  limitation, not a logic bug — the `invert` algebra is exact. The proptest
  strategy constrains the compression ratio to a realistic band (`rwidth_factor`
  ∈ [0.1, 10]) so the property isn't flaky; the well's actual domains (time, band
  fractions) never approach the pathological ratio. Follow-up if it ever bites: a
  one-line doc note on `ScaleLinear` about the compression boundary (parallel to
  the existing 2³ ms note on `ScaleTime`). Discovered during the scales spike
  (deepseek review N3), 2026-06-15.
- **ABC duration-summing ruler:** kaijutsu-abc has no total-beats-per-voice
  machinery; needed to validate that a committed phrase's ABC sums to
  `beats_per_phrase` (Chameleon eval ruler, new code). The tuplet/broken-rhythm
  handling in `midi.rs:261-274` is the acceptance spec.
- **ABC layout:** Linear duration spacing (needs Gould spacing/justification), system bracket/brace, closed-score layout.

## Hyoushigi / Musician

- **Beat-on-track refactor — ✅ SHIPPED (Stages 1–3 M1, 2026-06-29/30).** The track
  owns clock/playhead/transport/score (score context), contexts attach; `ClockSource`
  + `RenderTarget` landed. Remaining stages (M2–M4: input telemetry, drift-modeled
  clock-in, edge node) are sequenced in `docs/midi.md`; still-open external-signal
  clock sources (solar/compute-availability) ride the same `ClockSourceKind` seam.
  Story: `docs/tracks.md` + devlog.
- **Beat-lifecycle privilege asymmetry — RESOLVED by design (2026-06-28).**
  `fire_lifecycle` runs `tick`/`rotate` unprivileged while create runs
  privileged; gemini-pro flagged it. Decision: leave it. We deliberately keep
  capabilities as ergonomic nudges (not hard boundaries), so the asymmetry never
  bites; player focus/mistake-prevention lives in the *loadout* (a musician can't
  reach `kj transport`/`fork` at all), not in privilege/auth — and the kernel's
  own beat lifecycles stay able to act. Reasoning written into
  `docs/instrument-design.md` ("Many hands, one trust boundary") + `docs/chameleon.md`.
  (The `kj transport play` blind-success smell from the same review SHIPPED its
  ACK fix — `f0c3eb90` — so its entry is retired.)
- **Musician `kj` loadout — tool-free (2026-06-13).** `musician` seeds
  `assets/defaults/rc/musician/create/S10-binding.kai` granting only `drive`:
  no `builtin.*` tool instances, no `facade:shell`/`submit_input`, no
  `fork`/`drift`/`transport`/`operator`. A player is an ABC-only voice — its
  turn text *is* the score (`on_turn_completed` eager-parses it), so it needs no
  tools, and a small local model handed the full palette stalls the turn. The
  generic ABC-output primer rides the system slot (`create/S15-abc-primer.md`);
  the gig (key/tune/register) belongs to the stance + producer chart, NOT the
  base rc — migrate any song-specific primer content to the producer/chart
  layer when it lands ("big models author vocabularies").
- **No chart is seeded into a player's context — the gig metadata gap (found
  2026-06-30, standing up a bass player for the Chameleon line).** The
  musician stance + ABC primer (`musician/create/S00-stance.md`, `S15-abc-primer.md`)
  both say "your chair, key, tune, and register come from your stance and the
  chart the producer has set" — but **there is no chart**. A search of every
  document + KV finds the Chameleon spec (B♭ Dorian, B♭m7–E♭7 vamp, bass chair)
  only in `docs/chameleon.md`; **nothing writes it into a musician context**, and
  no `create` script seeds it. So a freshly-created player arms correctly, hears
  itself + siblings via `KJ_HEARD`, and drives on the beat — but does **not know
  what tune it's playing**. The *now-facts* channel (`KJ_TICK`/`KJ_PHRASE`/
  `KJ_TEMPO`/`KJ_HEARD`) is wired; the *gig* channel is not. This is the producer's job
  (Opus authors the vocabulary, the player speaks it) and the producer chair
  isn't built — but slice one (bass-gemma vamping B♭ Dorian) needs a chart NOW.
  Minimal fix that fits "players are rc programs / setup is declarative rc":
  a `musician/create/S05-chart.md` (numbered into the cached system prefix,
  before the generic primer) carrying the song-specific gig — key, vamp changes,
  register, the bass chair. Hand-authored for the audition; becomes the
  producer's `drift`-delivered, hydrate-latched revision surface when that chair
  lands. Pairs with the "migrate song-specific primer content to the producer/
  chart layer" note in the tool-free-loadout entry above and the
  marker-advance-on-durable-revision item below. Decide: per-song chart files vs.
  a single chart whose body the producer rewrites — the rotation/hydrate boundary
  already gives a clean delivery point either way.
- **Decouple the OODA Act from ABC (generalize the loop primitive).** The Act
  path is hardwired to one notation: `on_turn_completed` → `schedule_abc_cell`
  eager-*parses ABC* to validate, and the `DeriverRegistry` derives MIDI from
  it. The loop *shape* — drive → validate turn output → crystallize a cell →
  derive sibling artifacts — is general and would serve other loops: a
  MIDI-native model (emits MIDI directly, no ABC), non-music content, or any
  "model produces structured artifact on a beat" workflow. Generalize to a
  content-type-keyed `schedule_cell(content, content_type)` where validation is
  pluggable (the player's track/role declares its expected content type) and
  derivation stays the already-content-type-keyed `DeriverRegistry`. Then the
  malformed-quarantine (just shipped, beat.rs:850 `set_excluded`) and the
  header-carry follow-up below both become per-content-type validator behavior,
  not ABC special cases. Keep ABC as the first registered validator/deriver.
  This is one axis of the broader **`context_type` feature-decomposition**
  (`docs/chameleon.md` → "context_type is an rc bundle of features"): *what
  artifact* a player produces, separate from *whether* it has a beat.
- **Header-carry for headerless player output (robustness).** A windowed player
  naturally emits a bare continuation body (no `X:`/`K:` header) once it has a
  full tune in its context; the schedule-time validator then rejects it. Today
  we lean on the tick prompt to demand a complete tune every turn — brittle for
  small models. Robust fix: in the score scheduler, if the output is a bare body
  for a track with a last-good tune, prepend that track's last-good header
  before validating/deriving. Pairs with the decouple above (a per-content-type
  "complete the fragment" step).
- **Cold-start re-attach is MANUAL, not automatic (by choice, 2026-06-28;
  re-stated in track vocabulary 2026-07-01).** The scheduler starts with an
  empty track map on restart; nothing automatically re-attaches persisted
  musicians. **What exists:** `kj transport attach` recovers a musician after a
  restart from its persisted `tracks` + `attachments` rows — real tempo/cadence
  back, attaches stopped + OODA-armed, playhead + committed log rehydrated from
  the score context (restart-safe by construction, `tracks.md` Stage 2 WI 7).
  **Deliberately deferred** (Amy's call): an automatic cold-start sweep that
  re-attaches every persisted attachment on boot; the natural seam is the
  recovery loop in `rpc.rs`, and it must run *after* the beat scheduler is
  wired. Adjacent to `tech_debt_peer_reattach_on_reconnect`.
  - **Follow-ups:** (a) `beat_count`/`KJ_PULSE` are NOT persisted — documented
    as the contract (`tracks.md` Stage 1 deferred list); persist them
    holistically when the sweep lands. (b) attachment-row cleanup on
    disarm/archive once an archive RPC lands (no row leak today).
- **Per-type `BeatPolicy` defaults (the surviving half of "cadence settable per
  context").** The per-context cadence knob LANDED with the track model:
  `kj transport attach --wakeup N --rotate N` sets each attachment's divisors,
  persisted in the `attachments` row. What remains is per-*type* defaults for
  the track-level knobs (period / `beats_per_phrase`) so a `funkMusician` rc
  bundle isn't stuck on `musician_default()` — an axis of the **`context_type`
  feature-decomposition** (`docs/chameleon.md`).
- **`kj transport meter` inbound verb (Chameleon batch 1, F2):** add
  `kj transport meter <beats_per_phrase>` with a `--bars N --beats-per-bar M`
  convenience that multiplies to beats *at the edge* → new
  `BeatCommand::SetMeter`. Home is `kj/transport.rs`, and it gets the first
  bars→beats translation test (the kernel only ever sees beats; bars live in the
  human-facing arg). Pairs with the cadence-knob item above.
- **`ooda_every` stays beat-denominated (Chameleon batch 1, F2):** the OODA
  cadence field is kept in beats even though its default is *expressed* in
  phrases (`8 * 16`); a phrase-typed `ooda_every` is deliberately deferred —
  revisit once irregular phrases (per-phrase beat counts) make the beat
  denomination awkward.
- **Transport surface beyond `kj`:** app transport buttons / spacebar + a capnp
  transport surface (today
  `kj transport attach|detach|play|pause|stop|tempo|ooda|rotate|render` only —
  no app/capnp surface). A restart-recovery `attach` button is a natural fit.
  Overlaps the retired playback.md's `TransportFlow` idea, now recorded in
  `docs/pcm.md` § Distributed listening.
- **Per-listener audio routing (PCM slices 1–3 landed 2026-07-01):** `kj play`'s
  `BlockFlow::PlayAudio` deliberately **bypasses `matches_filter`** — every
  attached client hears every `kj play`, regardless of which context it's on.
  Correct for first-sound (robust when the caller's context ≠ the app's joined
  context), but the eventual "every listener hears playback on their own output =
  shared listening" (`docs/pcm.md` § Distributed listening) wants context-scoped
  routing + a `kj transport route <sink>` verb. Revisit when listening goes
  multi-peer; it's the natural home for the `PeerConfig` capabilities bag.
- **A capnp callback-method addition can wedge a stale client (found 2026-07-01
  during PCM live-verify):** adding `BlockEvents.onPlayAudio @13` means every
  client's `block_events` forwarder must implement it. A client built from the
  OLD schema returns `Unimplemented: Method not implemented` when the kernel
  pushes the new callback — observed on the un-rebuilt `kaijutsu-mcp` binary
  (rebuilt `kaijutsu-server` + app, forgot the MCP server), and it appeared to
  **wedge that client's MCP↔kernel session for ~300s** (a `kj play` shell RPC
  timed out at 300s, then the session reconnected and the retry returned in
  118ms; the sound itself played fine — only the un-rebuilt subscriber erred).
  Two takeaways: (1) **operational** — a capnp change requires rebuilding ALL
  clients (`-server`, `-app`, AND `-mcp`), not just the two obvious ones; worth a
  note in the dev-loop docs. (2) **design** — should the kernel tolerate a
  subscriber that `Unimplement`s a *newer* callback method without wedging or
  eventually dropping its whole (still-valid) block subscription? The bridge
  already logs+counts the failure (`SubscriberHealth`/`MAX_SUBSCRIBER_FAILURES`),
  so a forward-compat client loses its subscription for not knowing one new push.
  A "best-effort, ignore-if-unimplemented" push tier for directive-style events
  (vs. must-deliver block ops) might be the right shape.
- **PCM review findings — gemini-pro batch (2026-07-01), triaged:** the batch
  reviewed slices 1–3 holistically. Verdicts (deepseek's filtered-subs bug was
  the one already fixed in `9b1842ea`):
  - **Encoded byte-churn — noted opportunity, deprioritized (Amy, 2026-07-01).**
    `AudioRef::Encoded{bytes: Vec<u8>}` is deep-cloned per FlowBus receiver
    (async_broadcast clones), again into the client `ServerEvent`, again in the
    app. An `Arc<[u8]>` would make every clone an atomic bump — but we're NOT
    optimizing the inline path. **The decided direction is to push most/all bulky
    IO through CAS** (`Encoded` stays the genuinely-tiny path; large samples ride
    `Cas`, which keeps bytes off the bus entirely — the slice-5 convergence,
    `docs/pcm.md` "How it converges"). So the fix is architectural (route bulk →
    CAS), not an `Arc` micro-opt. Revisit `Arc` only if a real tiny-sample hot
    path ever shows churn.
  - **`Debug` byte-spam — FIXED (2026-07-01).** `AudioRef` now hand-writes `Debug`
    to print the byte *count*, not the buffer, so a `tracing::debug!(?flow)` down
    through `BlockFlow::PlayAudio` can't dump a whole sample into a log line.
    (`Serialize` still round-trips the bytes — that's the wire/record contract,
    not a logging path; nothing serializes `AudioRef` to JSON for logs.)
  - **"Echo" multi-subscription bug — NOT REAL as described (verified).** Claim: N
    open contexts → N subscriptions → N simultaneous plays. But subscriptions are
    deduped by `(principal, instance)` (a client instance holds ONE, replaced not
    stacked), and live-verify showed exactly **1** `AudioPlayer` per play ("2
    listeners" = app + mcp). Genuinely-multiple sinks each playing IS the intended
    distributed-listening behavior. A `directive_id` nonce + client LRU dedupe is a
    reasonable *future* idempotency guard if we ever fan one client into many subs.
  - **`kj play` requires an ambient context — MINOR.** It resolves the caller
    context to stamp the directive; with no ambient context it errors. Since the
    directive bypasses context filtering anyway, falling back to `ContextId::nil()`
    (which `on_play_audio` already tolerates, unlike `on_context_switched`) would
    let a truly context-less caller broadcast. Low priority; a design nicety.
  - **capnp `AudioRef` union default — NOTE only.** Lowest-`@` union arm (`encoded`)
    is the default discriminant, so a malformed/empty `AudioRef` decodes as
    `Encoded{empty, Wav}` → the sink EOFs on 0 bytes (a failed decode, logged — not
    corruption). Benign; document if a `streamingUrl @3`-style arm is ever added.
  - `from_path_extension` uses `rsplit_once('.')`; `Path::extension()` is more
    idiomatic (a dir-with-a-dot edge case already fails-loud → error, not misplay).
- **App track chip + "transport" label for beat():** author chips show the
  player's principal on played phrases and `beat()`'s on transport fallback
  repeats — truthful but mildly noisy. Add a track chip (the lane identity) and a
  "transport" label for `beat()`-authored fallback repeats so a vamp insurance
  repeat reads as the transport, not a mystery principal.
- **`KJ_HEARD` shipped as a JSON push; array + pull are follow-ups (Chameleon
  batch 2, 2026-06-11; re-pointed at the track score with Stage 2):**
  `KJ_HEARD` ships as a pragmatic **JSON-string push** — `beat.rs::heard_json`
  reads committed notation in the last `HEARD_WINDOW_PHRASES` (8) from the
  **track's score context** (`ContentType::Abc` only, all producers, across
  rotations — the real band view) and seeds it as a JSON array string.
  Load-bearing **even solo**: score blocks are `ephemeral` (hydration-silent),
  so this is the only way a player sees its own prior phrases. **Two follow-ups
  (TODOs on the code), when the kaish arrays/hashes plan lands:** (1) expose it
  as a real kaish **array of hashes** (indexable, `for phrase in $KJ_HEARD`)
  instead of a JSON string the script can't index; (2) re-shape **push → pull**
  — a `kj`-reachable windowed read so the script chooses depth/track rather
  than a fixed injected window (shares the read with the RC hydration-marker
  archive verb and fork-carry — one read, three consumers). Also open:
  per-context window tuning (`HEARD_WINDOW_PHRASES` is a const). `content_before`
  in `ResolverCtx` stays deliberately track-blind regardless (no resolver reads
  it; `CasCommitResolver` reads CAS by hash).
- **Player spawn = thin fork + rc-rebuilds (design locked 2026-06-12; core
  SHIPPED — current mechanism in `docs/chameleon.md` § Rotation, chronology in
  devlog).** A player is spawned by `kj fork --preset spawn` (fork-filters,
  `docs/fork-filters.md`); the child's `musician/fork/S40-hydrate.kai` re-marks
  the hydration window at its tail. The page-turn is scheduler-driven: at the
  rotate horizon the scheduler synchronously detaches the parent (race-free)
  and fires `rotate/S10-rotate.kai` = `kj fork --preset spawn --switch &&
  kj transport attach && kj transport play`; the attachment row (track +
  wakeup + rotate cadence) travels with the fork, and tick continuity is by
  construction (the clock lives on the track — the 2026-06-29 playhead carry
  is deleted). `kj transport stop --track <t>` halts a whole rotating lineage
  in one verb, closing the old "no clean way to stop a chain" gap; residual
  narrow race: a rotate rc already in flight ends in `kj transport play` and
  could restart a just-stopped track — add a scheduler-side halt check if it
  ever bites. Still open:
  - **Rotate chains pollute the director's context tree (found 2026-07-15, DS
    Director `019f14ba`).** Every page-turn is a thin `spawn` fork, so a song
    running N phrases produces N+1 contexts in a linear chain — `kj context
    list --tree` renders the whole lineage and an operator must visually skip
    past it (a 17-deep chain observed from one song). Fix ideas (pick one):
    (a) `--hide-archived` collapse, (b) fold same-track rotate chains into a
    compact `root→…→tip (N segments)` one-liner, (c) auto-archive rotated-out
    segments. No correctness issue — operator UX tax.
  - **The windowed-notation pull primitive.** No cross-context block-copy verb
    exists; a player carrying recent notation into its thin-forked child needs
    one. Same windowed read as `KJ_HEARD`'s push→pull follow-up and the
    marker-archive read — **one read, three consumers**; keeps the carry in rc.
  - **A declarative "fire script at tick T" timeline scheduler** — worth
    building once the producer schedules more than rotates (section/tempo/
    dynamics events are the clear second consumers).
  - **Marker-advance on durable revision** — when the producer writes revision
    blocks, re-run `kj context hydrate` to advance the marker. Pure rc once
    the producer exists.

- **Fork primitives — full/thin mental model (Amy, 2026-06-12).** Full fork
  (regular `kj fork`) is the *powerful* path: take the whole context into a fresh
  lineage = a **new KV cache** (resume-a-session-as-another-model, orchestrator
  repair, drift-a-summary-back). Thin fork is *reuse/reduce*: save tokens for a
  long-running iterating player (the `window`/`spawn` factory presets per
  `docs/fork-filters.md`). Copy cost is a non-issue (storage cheap); the axis is
  KV-cache strategy. Remaining open primitives:
  - **Retire the `max_blocks` fork field (slice 4):** `fork_filtered` now builds
    its positional universe in document (`order_key`) order, so `max_blocks`
    indexes the timeline correctly in the interim (test
    `fork_filtered_max_blocks_keeps_most_recent_by_timeline`), but the field is
    only deprecated, not removed. Fold `--depth N` into the selection engine as
    `--include end-N:` over the `block_ids_ordered()` snapshot and delete the
    field. (BlockId order is `(context, principal, seq)` — principal-major; it
    only coincides with timeline order for a single principal, so a multi-principal
    `max_blocks` over raw BTreeMap iteration was the original bug.)
  - **A snapshot/savepoint marker verb (speculative, not-now — direction set
    2026-06-12).** Absorbed by the fork-filters range grammar as a future
    **label endpoint** (`docs/fork-filters.md`): a savepoint is a colon-free
    name on a block, usable as a range endpoint (`kj fork --include 0:bridge`)
    — no new fork machinery, no verb semantics of its own. Still not-now;
    build labels when the orchestrator work or the time-well wants named
    points.
  - **Presets as a deep kaijutsu concept (design thread, 2026-06-12).**
    Preset = a named **ensemble of argument values**, not a behavior — the
    audio patch-recall model (hit "e-piano", every knob moves, same synth).
    Extends the existing model/prompt preset table (normalized `preset_args`
    child table, verb-scoped from day one) to carry fork filters; a `player`
    patch can move filter + model knobs in one recall. Recall-then-tweak:
    scalars override, filters compose under the include invariant; recall is
    a snapshot (horizon-latched, like rc scripts). Fork is the only wired
    verb for now — generalizing to other verbs (discovery, user banks,
    sharing) deserves its own design session.

  **Remaining follow-ups (deferred — from the same review):**
  - **P1 ×2 — absorbed into the shared SEAM MODULE (re-prioritized
    2026-06-12: FIRST in the fork-filters build order).** The tool-pair /
    turn-boundary tail snap (orphan `tool_result` silently dropped by the
    snapshot repair; a marker on a `tool_call` injects a synthetic
    "interrupted" result every turn forever) and the missing archive seam
    (prefix+tail concatenate with no "[N blocks archived]" signal; cross-gap
    `Model/Text` fragments can merge into false continuity) were "latent
    until musician gets tools" as hydration bugs — but fork-filters' hand-cut
    ranges make both reachable immediately. One first-class module owns every
    keep-set cut edge: turn-boundary snapping (never start an interval on
    `ToolResult`/`Model`-continuation), synthetic user-role seam injection
    (after the prefix, cache-stable), tool-pair integrity. Consumers:
    `rehydrate_windowed`, fork selection, the pull primitive. Contract in
    `docs/fork-filters.md`.
  - **`window` counts RAW blocks, not turns/phrases** (~2-3 blocks per OODA turn,
    and musician score/Trace blocks are hydration-silent so the *visible* tail is
    smaller still) — revisit if a phrase/turn-denominated window reads cleaner.
  - **Cache-breakpoint ↔ window interaction** — the musician's S20 cache
    breakpoints sit at message indices that windowing shifts; harmless for the
    local bass (no prompt cache; musician sets no breakpoints today so the
    byte-stable prefix is inert), reconcile when API-model chairs join.
- **Standing per-phrase `UseLastGood` cells (whole-turn-miss hole) (Chameleon
  batch 1, F2):** `UseLastGood` only fires when a cell was *scheduled* and then
  squashed; a turn that produces no cell at all (the model never spoke) leaves no
  cell to fall back on, so the phrase is silent rather than a vamp repeat. The
  natural hook is the new `phrase_due` boundary: stand up a per-phrase
  `UseLastGood` cell at each phrase boundary so an unscheduled phrase still vamps
  the last good one. Out of scope for batch 1; recorded so the hole is known.
- **Deriver-budget enforcement beyond convention (Chameleon batch 1, F2):** the
  `Deriver` contract says ≲1 ms per cell (it runs on the beat thread under the
  timeline lock) but nothing enforces it — today it is a measured convention
  (T22 prints ~300 µs release for the ABC deriver). Add a timed `debug_assert`
  (or a soft warn) around `derive()` so a future heavy deriver trips loudly in
  dev rather than silently stalling the beat under the lock.
- **In-RAM committed `Vec` / RAM-CAS unbounded growth (Chameleon batch 1, F2;
  reframed 2026-07-01):** the track timeline's committed `Vec` and RAM CAS grow
  without bound for a long-playing track (every phrase appends). Rotation is
  deliberately NOT the answer anymore — the track timeline *survives*
  page-turns by design (`tracks.md` Stage 2). The durable record already lives
  in the score context's blocks + CAS, and `UseLastGood`/`KJ_HEARD` only need a
  recent tail, so the fix is windowing/compacting the *in-RAM* committed log
  (drop cells older than the largest read window; rehydration-from-blocks
  already exists for the tail). Until then a marathon set leaks RAM.
- **Band track↔chair mapping source of truth:** musician-create derives a track
  from the context label (`TrackId::new`→`slugify`, hard-error on empty slug).
  Once a band config exists (multiple chairs on one timeline), decide where the
  track↔chair mapping lives — there is no registry today (track is self-describing
  on every block, by design).
- **`played_by` collapses to `system()` — `who-played` provenance is degenerate
  (Chameleon batch 1, F2):** F1 §1.2 records "who played" as `BlockId.principal_id`,
  meant to be the player's principal. But the musician turn's model-text output
  block is inserted under `PrincipalId::system()` (`llm_stream.rs` `StreamEvent::TextStart`,
  the standing model-text convention), and `on_turn_completed` (`beat.rs`) sets
  `played_by = b.id.principal_id` = `system()`. The OODA `tick` verb also fires
  under `system()` (`beat.rs::fire_tick`), so `TurnFlow::Completed.principal_id`
  carries `system()` too — reading it instead of the block author would NOT help.
  So every materialized score block is authored by `system()` (plus `PrincipalId::beat()`
  for fallback repeats). **Harmless today** — one model per musician context, and
  lanes key on `track`, not principal, so no correctness/collision issue (the
  per-principal seq lane just has a single `system()` writer). **Will mis-attribute**
  the moment multiple models share a context or we want to distinguish player from
  transport. Not a one-liner: needs the musician turn to run (and author its
  output) under a distinct per-player principal. Surfaced in the F2 adversarial
  review (deepseek+gemini, 2026-06-11); the two silent-failure bugs from that pass
  (resume parent-id from log tail; hydration-failure publishing no terminal event)
  were fixed in-slice.
- **`kj track` listing surface:** no way to enumerate the tracks present on a
  context's timeline. Add a `kj` listing surface (which tracks exist, which
  principals played each) once tracks are user-visible.
- **Section-placement policy:** the OODA notation cell is scheduled a fixed
  **one phrase** ahead (`phrase_delta()`; `OODA_LEAD` is gone, Chameleon batch 1,
  F2); a real musician wants musical placement (next section boundary, loop
  region) and a richer `compute_basis`.
- **`Midi` render variant + UI timeline:** `audio/midi` projects to `ContentType::Plain`
  today; add a `Midi` variant + renderer, and the scrubbable timeline render.
  **Deliberately deferred to its first consumer (an app-side MIDI renderer /
  peer sink — `docs/pcm.md` § Distributed listening), not added in
  Chameleon batch 1, F2:** `ContentType` is a closed enum that rides
  `BlockHeader` inside `SyncPayload` ops, and the CBOR codec is fail-loud by
  design — a new variant breaks old decoders. Per the project rule a variant
  lands with its renderer, never speculatively. Interim sink key:
  `Role::Asset && parent_id → ABC source` (one hop); the authoritative mime is in
  the CAS sidecar.
- **midi→pcm re-anchor (playback slice 3) (Chameleon batch 1, F2):** the
  `abc_to_midi` *resolver* is gone — ABC→MIDI is now a barrier-side `Deriver`,
  not a timeline resolver, so the midi→pcm chain for dumb (PCM-only) sinks has no
  resolver shape to copy. Two candidate re-anchor shapes to pick between when
  playback slice 3 lands: (a) a deferred PCM **cell keyed on the derived MIDI
  hash** (real lead time, scheduled like any resolver), or (b) a measured
  **budget-excepted deriver** (only if midi→pcm proves fast enough to run at the
  barrier — almost certainly not, soundfont synthesis is heavy). See
  `docs/pcm.md` § Distributed listening (playback.md retired 2026-07-01).
- **Clip cells — design DONE (`docs/clips.md`, 2026-07-01); implementation
  lands with pcm.md slice 5.** Shape A payload
  (`application/vnd.kaijutsu.clip+json`) over the mime-keyed render seam;
  bytes out-of-band (SFTP + `/v/blobs`, client XDG CAS cache, prefetched
  under the speculation lead). Research record: `docs/cue-prior-art.md`.
  Remaining work when slice 5 opens: the record type + validator in
  `kaijutsu-audio` (the validator is a voice of decouple-Act-from-ABC above),
  the first clip-MIME render target, the prefetch path, and the `kj play`
  clip-emitting form.
- **Trace span attribute:** attach `hyoushigi.tick` on the materialize→insert
  spans now that a producer exists.
- **Multi-listener playback (was `docs/playback.md` — retired 2026-07-01).**
  The 2026-06-10 peer-sink design predates the track/`RenderTarget`
  architecture; its superseded mechanism decisions (sink-pull scheduling, the
  pause=mute verb remap) are recorded as such and its surviving ideas
  (peer capability advertisement, capnp/`TransportFlow` transport surface,
  routing, the metronome slice, midi→pcm for dumb sinks) now live in
  `docs/pcm.md` § Distributed listening. Longer-term design conversation, not
  a task yet: unify hyoushigi beat-time and conversation wall-time ("the
  conversation has a tempo") so the timeline is the kernel's one clock rather
  than a music sidecar.

## config-shadow cache: residual cross-alias staleness (found 2026-06-24)

A config/rc path gets a *shadow* copy in the shared `FileDocumentCache` (keyed by
`file_context_id`) separate from the `ConfigCrdtFs` block (keyed by
`config_context_id`). A direct config-block write leaves that shadow stale for the
kaish `cat`/file-tool read path (execution via `ConfigCrdtFs` + the app renderer
stay coherent).

**The common case is FIXED** — editor (`b4ba9238`) and `kj rc`/`kj config`
dispatch now call `Kernel::invalidate_config_file_cache` after a direct write, via
`FileDocumentCache::invalidate_document` (drops the shadow *doc*, not just the
entry, so the next read fully reloads). Covers "write a path, read that same path."

**Residual (low priority): cross-alias.** Invalidation is by the written/opened
path only, so writing one symlink alias and reading another stays stale until
cache eviction — e.g. `kj rc reset lib/S20` then `cat coder/S20` (coder→lib), or
editing `coder/S20` then `cat lib/S20`. Cosmetic (cat path only), self-heals on
LRU/TTL. A full fix needs alias-aware invalidation (forward-resolve the written
path to its terminal *and* reverse-scan symlinks that point at it) — deferred.

## VFS / cache: coherency + consistency + test-coverage audit (2026-06-27)

External reviewers (the gpal/Gemini batches especially) keep poking at the cache
layer and finding *plausible* coherency holes that mostly turn out narrower than
claimed once checked against the wiring — but the recurring near-misses say the
substrate deserves a systematic pass rather than per-claim firefighting. The trigger
this round: SFTP rides `Arc<MountTable>` directly (`sftp.rs:115`, from
`kernel.vfs()`), while the `FileDocumentCache` write-through lives one layer up in
`MountBackend` (`runtime/mount_backend.rs:43-49`), which SFTP never traverses. Not
the "silent divergence" the review claimed (CRDT mounts still hit `ConfigCrdtFs`
in-table; the generation/mtime staleness reload exists precisely to catch
bypassing writers — that's how host `vim` stays coherent) — but the two-layer split
is real and under-tested.

Scope a deliberate audit covering three axes:

- **Cache coherency.** Enumerate every `FileDocumentCache` consumer and every path
  that *bypasses* it (SFTP via `MountTable`, app renderer, `ConfigCrdtFs` execution
  reads, kaish/MCP file tools via `MountBackend`). For each: does the generation/
  mtime staleness reload actually fire? Map the **dirty-cache-wins** windows (an
  in-flight cached edit shadows an external/SFTP write until flush) and the
  byte-offset-write vs document-level `WriteMode` impedance (SFTP `write(path,
  offset, data)` onto a UTF-8 CRDT doc). Fold in the residual cross-alias staleness
  above — it's the same family.
- **Code consistency (async-correctness).** `LocalBackend` mixes `tokio::fs` and
  blocking `std::fs` on the async worker: `write`/`read`/`truncate` use `tokio::fs`
  (offloaded, fine), but `create` (`local.rs:290`), `mkdir` (`:307`), and
  critically `resolve()` — called on *every* op, doing synchronous
  `canonicalize()` at `:80,93,105` — block the runtime thread. Under a slow/stalled
  host FS those starve the ambient tokio pool, which is exactly the path the
  "ssh-in-when-the-app-is-down" fallback depends on (the gpal `spawn_blocking`
  note, verified — but mis-aimed at `write`; the offenders are `resolve`/`create`/
  `mkdir`). Fix: route the blocking calls through `spawn_blocking` or `tokio::fs`.
- **Test coverage.** We lack concurrent multi-writer VFS tests (the kind that would
  have surfaced the SFTP concurrent-append lost-update directly), cross-layer
  coherence round-trips (SFTP write → kaish `cat` sees it; kaish edit → SFTP read
  sees it), and staleness-reload tests per backend. Build these as the audit's
  exit criteria, not an afterthought.

Not urgent, but a good forcing function alongside the SFTP/shell sidequest, which
is the consumer that stresses all three axes at once.

## kaijutsu-mcp — invoke_peer double-encodes object params (found 2026-06-23)

Calling the `invoke_peer` MCP tool with an object `params` (e.g. `{"context_id":
"019ec11b"}` for `switch_context`) fails: the app's `dispatch_peer_action`
rejects it with `invalid type: string "{\"context_id\": ...}", expected struct
Params`. Diagnosis: `InvokePeerRequest.params` is `serde_json::Value`
(`models.rs:144`) and the server does the right thing
(`serde_json::to_vec(&req.params)`, `lib.rs:1166`) — but `req.params` *arrives*
as a `Value::String` holding the JSON text, not a `Value::Object`. So the
tool-call layer stringified the object one extra time before it reached the
server; `to_vec` then emits a quoted JSON string and the app's `from_slice`
sees a string. Surfaced now because `invoke_peer` is rarely exercised (Amy:
"we haven't used it much until now"). **Proposed fix (server-side, tolerant):**
in `invoke_peer`, if `req.params` is a `Value::String`, attempt to parse it as
JSON and use the result (accept either an object or a JSON-string-of-an-object);
fail loud if neither parses. Real root may be client-side arg encoding for
`serde_json::Value` fields — worth confirming. Blocked the isolated peer-path
verification of the Screen-transition fix; verified instead via the
server-pushed `ContextSwitched` path (`kj context switch`), which exercises the
same `handle_context_switch` landing.

## kaijutsu-mcp — capnp schema skew breaks subscribe (found 2026-06-23)

After `systemctl --user restart kaijutsu-server` onto a fresh `target/debug`
build, `register_session` fails in its subscribe step with `Unimplemented:
method kernel::Server::list_mcp_prompts not implemented` (@67 in the schema).
**Not a missing handler** — no client code calls `list_mcp_prompts`; the index
is landing on the wrong method slot, i.e. the **MCP client binary Claude Code
launches was built against a different `kaijutsu.capnp` than the running
server** (capnp identifies methods by index, reports the *server's* name for
that slot). Same "fresh server + old MCP-client binary" state the 2026-06-17
signoff noted. The MCP feature-expansion that widened the schema around @60–@74
landed in `a31d802` (2026-02-01). **Fix:** rebuild/reinstall the kaijutsu MCP
client binary so its schema matches the server (it's launched outside an agent
shell). Until then the over-the-wire MCP shell is down; headless kernel tests
are unaffected. Blocked a live vi smoke-test (vi proven by 1200 headless tests).

## kaijutsu-mcp — June 2026 SyncedDocument migration review

Surfaced by a DeepSeek (concurrency) + Gemini (architecture) review of commit
`ac5f518` (Remote backend cut over to `kaijutsu_client::SyncedDocument`). The
dropped-stdout bug and the content/exit_code completion race are fixed (poll now
does an authoritative `get_context_sync` read after terminal status); these are
the *remaining* findings, triaged.

- **HIGH (PARTIAL) — hook authoring vs resync: sole-writer + pushed-frontier.**
  `HookListener` writes blocks directly (`doc_mut().insert_*` under
  `synced.lock()`), so the bg listener is NOT the sole writer. `apply_sync_state`
  replaces the doc wholesale, so un-pushed hook blocks could be wiped on resync;
  and `push_ops` bases `ops_since` on the inbound SyncManager frontier, which
  local authoring never advances → every hook event re-pushes all prior local
  ops (idempotent but O(N)). MITIGATED 2026-06-13: `resync_synced` now FLUSHES
  local ops (`flush_local_ops`) before fetching the snapshot, so hook blocks
  round-trip through the server and survive the common case. REMAINING (cohesive
  follow-up, needs design + CRDT-frontier testing): (a) a dedicated "pushed"
  frontier so flush stops re-sending; (b) close the residual flush→apply window
  where a block authored mid-resync is still lost — cleanest via a command
  channel that makes the bg task the true sole writer (authoring + push + resync
  all serialized in one task).
- **LOW — `agent.compact` hook event is mapped but unhandled.** The adapter maps
  Claude `PreCompact` → `agent.compact`, but
  `HookListener::process_event` has no arm for it (falls to `_ => {}`), so a
  compaction boundary silently produces no block. Either author a System/Trace block
  marking the compaction, or drop the mapping. (Found during the 2026-06-18 bitrot
  pass; see `docs/mcp-hook-alignment.md`.)
- **LOW — `claude-hooks.json` uses a repo-relative adapter path.** `command:
  "contrib/adapters/claude.sh"` only resolves when Claude Code's cwd is the kaijutsu
  repo root. The adapter itself now resolves its own filter via `BASH_SOURCE` dir, so
  only the settings.json entry is cwd-sensitive. Document the absolute-path
  requirement in the sample, or have install copy an absolute path.
- **MED — multi-context operations silently collapse to one in Remote.**
  `search_context`, `list_resources`, the `kaijutsu://docs` reader, and
  completions call `context_ids()`, which in Remote returns only the single
  joined context (`crates/kaijutsu-mcp/src/lib.rs`). A global search now silently
  skips every other context on the server. Fix: add an async
  `actor.list_contexts()`-backed lister for Remote multi-context surfaces.
- **MED — resource/prompt handlers hardcode `kind = "Conversation"` for Remote**
  (`analyze_document`, doc-tree, `read_resource`). Loses the real context type.
  Fix: carry the kind through the sync state or a metadata RPC.
- **MED — Remote input tools vs Local divergence:** Local `read/write/edit_input`
  swallow `create_input_doc` errors via `let _ =`; `submit_input` is
  unimplemented in Local mode. Either implement Local submit or document the gap.
- **LOW — hook insert/push failures only `warn!` then return `allow`.** The
  agent proceeds while its action's CRDT blocks are silently dropped — counter
  to the crash-loud stance. Consider returning `deny` (or a visible error) on
  push/insert failure.
- **LOW — `SyncedDocument::pending_events` not drained on `apply_sync_state`**
  (`crates/kaijutsu-client/src/synced_document.rs`): events buffered before a
  resync are never replayed against the new doc. Harmless if the server snapshot
  is always ahead of the buffered events; otherwise a silent loss.
- **LOW — dead `push_to_server` on `KaijutsuMcp`** (lib.rs): nothing calls it
  (the hook listener has its own `push_ops`); carries the same stale-frontier
  bug. Delete or consolidate.
- **PERF follow-up — the shell poll's authoritative read pulls the full context
  snapshot per command** (`execute_and_poll_shell`, Phase 2). Fine for short MCP
  contexts; a per-block read RPC (`actor.get_block(ctx, id)`) would avoid the
  O(blocks) transfer for large conversations.
- **TEST gaps beyond `tests/e2e_shell.rs`:** no coverage for Remote
  input tools, the hook-listener socket path, prompts, resources, or
  reconnect/resync. Add e2e cases (the harness in `e2e_shell.rs`
  generalizes).

## Testing & Tooling

- **russh teardown panic:** `ChannelCloseOnDrop::drop` panics with "there is no reactor running" in tests.
- **Capnp schema change ⇒ three binaries to bounce:** the dev runner
  only rebuilds/restarts `kaijutsu-app`; `kaijutsu-server.service`
  (systemd user unit) and `~/bin/kaijutsu-mcp` (running MCP processes
  hold the old binary; `cp --remove-destination` to replace, then
  reconnect MCP) keep stale codegen and fail handshakes with
  `Message contains non-list pointer where data was expected` (worse
  now that Kernel interface ordinals renumber on method deletion,
  e4c8417). Teach `contrib/kaijutsu-runner.sh`/`kj rebuild` to rebuild +
  restart all three, or at least print a loud reminder when
  `kaijutsu.capnp` changed.

---

## Architecture mapping pass — 2026-06-16

New observations from the crate-by-crate architecture sweep (see
`docs/architecture/`). Not fixed; recorded for later. Items that confirm an
existing entry are marked *(confirms above)*.

**Silent fallbacks (violate the "crash over confuse" stance):**
- `Kernel::list_tool_defs_via_broker` returns `Vec::new()` on *any* broker error
  (`kaijutsu-kernel/src/kernel.rs:467`) — a broken binding silently presents the
  LLM a tool-less context, no log/trace.
- `dispatch_tool_via_broker` does `broker.binding(...).unwrap_or_default()`
  (`kernel.rs:346`) — binding-fetch failure silently becomes deny-all; surfaces
  later as a confusing `ToolNotFound`.
- `MountBackend::read` falls through to raw on-disk content on *any*
  `FileDocumentCache::read_content` error, not just "missing/binary"
  (`kaijutsu-kernel/src/runtime/mount_backend.rs:267`) — a CRDT error could serve
  stale bytes.
- Additive `ALTER TABLE` migrations swallow SQL errors with `let _ =`
  (`kaijutsu-kernel/src/kernel_db.rs:873`) — a real failure surfaces as a
  confusing read-time error later.

**CRDT data model:**
- **Dual storage impls.** `BlockStore` (target) and `BlockDocument` (legacy) are
  both `pub` and in use; the legacy path returns newer fields
  (`ephemeral`/`stderr`/`signature`/`track`) as hardcoded `None`/`false`
  (`kaijutsu-crdt/src/document.rs:482`) and retains the duplicate-block seq bug
  fixed in `BlockStore` (`document.rs:892` vs `block_store.rs:320`). Pick a
  migration deadline.
- `calc_order_key` calls `block_ids_ordered()` (O(N) sort) on **every** insert
  (`kaijutsu-crdt/src/block_store.rs:390`); the bench exposing it is `#[ignore]`d.
- Tombstones aren't a first-class `BlockSnapshot` field — they ride a side
  `deleted_blocks` list re-applied by hand (`block_store.rs:1637`).
- `StoreSnapshot` has a breaking-format note with no migration path ("delete
  existing databases when upgrading", `block_store.rs:1680`).

**UTF-8 offset hazard:**
- `EditEngine` passes **byte** offsets/lengths to `block_store.edit_text`
  (`kaijutsu-kernel/src/file_tools/edit.rs:132`) while `FileDocumentCache` is
  careful to use **char** counts (`cache.rs:276`). Multi-byte content can corrupt
  the CRDT splice. Audit `edit_text`'s parameter semantics and unify.

**`LocalBackend::setattr` mtime is a no-op** (`kaijutsu-kernel/src/vfs/backends/
local.rs:354`) — it opens the file but doesn't set the timestamp, yet mtime is
load-bearing for `FileDocumentCache` staleness detection.

**LLM providers:**
- **Gemini stub removed (2026-06-16).** The dead `Provider::Gemini` (returned
  `Unavailable`, advertised uncallable models), its module, `UsageExtra::Gemini`,
  and `gemini_from_env` were deleted. Remaining work when Gemini is actually
  wanted: add a real provider, OR point the OpenAI-compatible core at Google's
  OpenAI-shaped endpoint (likely zero new code). Tracked in `project_unrig`
  auto-memory.
  (The stale "Phase 1: real-provider variants return Unavailable" doc comments in
  `llm/mod.rs` + `llm/stream.rs` were corrected in the same pass.)

**`kj` single-source guarantee is manual** — `dispatch()` routing and
`kj_command()` schema tree must be hand-kept in sync; a subcommand added to one
but not the other is unreflectable (`kaijutsu-kernel/src/kj/mod.rs:589`).

**Types-crate layering** — `ThemeData` (~60 visual fields + `include_str!` of
`assets/defaults/theme.toml`) lives in the foundational `kaijutsu-types`
(`theme.rs:59`). Belongs in a UI/config crate.

**`kaijutsu-index`:**
- `rebuild()` is a TODO stub (`lib.rs:214`) — evicted HNSW slots accumulate
  forever.
- Metadata lock held across ONNX `embed()` (`lib.rs:160`) serializes all
  `index_context` calls.
- `ort` uses `download-binaries` — fetches ONNX Runtime at build time, breaks
  air-gapped builds.

**`kaijutsu-cas`** — no refcounting/GC (`remove` is unconditional,
`store.rs:330`); object+metadata write isn't atomic (crash between leaves a
metadataless blob, `store.rs:254`).

**`kaijutsu-telemetry`** — the Bevy path leaks a `tokio::runtime::Runtime` and
upcasts its `EnterGuard` to `'static` (`otel.rs:28`); soundness rests on the
leaked runtime outliving the guard.

**`kaijutsu-client`:**
- Backoff reset bug — `finish_closing` reads `self.state` *after* `mem::replace`
  moved it to `Idle` (`actor.rs:1451`), so the attempt counter isn't preserved
  through `Closing → Cooldown`; backoff always resets to 1 s after a post-connect
  failure.
- `is_disconnect_error` matches on the capnp error `Display` text
  (`actor.rs:1214`) — fragile; a capnp formatting change would stop triggering
  reconnect. Prefer a typed `ErrorKind::Disconnected` match.
- Peer-reattach residual: initial `attach_peer` isn't remembered until the first
  *successful* user call, so a kernel restart before that leaves the peer
  un-reattached (`actor.rs:1933`). *(extends `tech_debt_peer_reattach_on_reconnect`)*

**App (`kaijutsu-app`):**
- Triple Chat/Shell discriminator — `FocusArea` + `ActiveSurface` +
  `InputOverlay.mode` (the last unread by submit); collapse to
  `FocusArea::Compose(ActiveSurface)` (`input/focus.rs:71`,`:116`,
  `view/components.rs:285`).
- 77 `#[allow(dead_code)]` suppressors for future-phase API — prefer
  `#[cfg(feature)]` so dead-code discovery still works.

**`kaijutsu-abc`** — `to_abc()` round-trip silently drops
`InlineField`/`Decoration`/`VoiceSwitch` (`lib.rs:406`); tuplet writer omits the
optional `:r` count (`lib.rs:366`).

**Server `unwrap()`** — `create_shared_kernel` panics on workspace-insert failure
(`rpc.rs:1092`) instead of `?`-propagating like its neighbors.

**Cap'n Proto evolution is comment-only** — no `@version`; removed-method ordinals
are renumbered/reused with a "safe because all clients updated" comment
(`kaijutsu.capnp:921`,`:933`,`:1169`). *(confirms above — fragile with 7+ dependent
crates)*

---

## Gemini CLI feature comparison — candidate work (2026-06-23)

Differentiators surfaced by scanning `~/src/research/gemini-cli` (Node/TS terminal
agent) with sonnet subagents, each verified against the kernel source before being
listed. Lens: capabilities gemini-cli has that kaijutsu plausibly lacks — *not* a
full feature inventory. Filtered through the **instrument-not-harness** stance:
items tagged ⚠️ sit in tension with it (silent override / harness-UX) and want an
opt-in or kernel-capability reframing before adoption. Pick from these; they are
candidates, not commitments.

### Provider resilience (the headline — Gemini's alignment + availability is the reason we're here)

- **LLM retry + backoff with jitter.** Claude/OpenAI/DeepSeek clients issue a
  single HTTP request and propagate `LlmError::RateLimited`/`ApiError`/`NetworkError`
  with no retry — one transient 5xx/SSL hiccup aborts the whole turn and loses
  context. gemini-cli `retryWithBackoff` (`packages/core/src/utils/retry.ts`): exp
  backoff, ±30% jitter, `Retry-After` respect, retryable-vs-terminal classification.
  Transparent to the user; clean instrument fit. **Strongest, lowest-risk item.**
- **Model availability state + fallback chain.** No per-model health map; a 429
  just fails. gemini-cli tracks terminal/transient health and walks a policy chain
  (pro→flash→…). ⚠️ Make it opt-in (`--allow-fallback` / alias policy) so the kernel
  doesn't silently swap the model the user chose.
- **Extended-thinking wiring — nearly free.** Types, builder, and SSE parsing are
  *done* (`Thinking::Enabled`, `with_thinking()` `llm/claude/build.rs:224`,
  `ResponseBlock::Thinking`) but the stream path hardcodes `thinking: None`
  (`build.rs:143`) and no `BuildOpts` field exposes it. ⚠️ `Thinking::Enabled {
  budget_tokens }` 400s on Opus 4.8 (adaptive-only) — wire **adaptive thinking +
  `effort`** through `BuildOpts` + per-model config, not `budget_tokens`. Toggling
  thinking doesn't invalidate the tools+system cache, so it's safe to flip per-context.
  Claude 4.x thinks by default — small delta, real win.
- **Token-aware context window.** Only an *output* cap exists (`max_output_tokens`,
  default 64K); no per-model *input* limit table, no pre-send estimate, no media
  weighting. Windowed hydration (`llm/mailbox.rs:197`) is block-count, not token-count
  — near-limit contexts get silently truncated or 400'd by the provider. Add a
  per-model input-limit table + pre-send estimate warning. Optionally an EMA
  chars-per-token calibrator fed by actual API usage (gemini's `AdaptiveTokenCalculator`).
- **Classifier-based model routing.** ⚠️ Opt-in only: route cheap turns to haiku,
  hard turns to opus via a fast classifier (gemini `ModelRouterService`). Surface as
  `route: auto` alias policy, never a silent override.

### Context & memory

- **Auto token-threshold compression with LLM summarization + verification.**
  Windowed hydration drops the middle block range with no summary (the motivating
  incident in `docs/conversation-session.md`). gemini `ChatCompressionService` fires
  at ≥50% of the window, LLM-summarizes the older segment into a `<state_snapshot>`,
  then runs a verification turn to catch omissions. Pair this with the windowing notch
  so dropped history leaves a distilled trace.
- **JIT subdirectory context injection.** *(merged: surfaced by both the tools and
  context scans.)* On a tool crossing into a new subtree, gemini crawls upward for
  not-yet-loaded `GEMINI.md` and appends it to the tool result. kaijutsu loads rc/
  stances at context-create only — no path-triggered per-directory injection. Append
  any `KAIJUTSU.md` between the accessed path and workspace root on first access.
- **Filesystem memory-file discovery.** No traversal of the host FS for
  user-maintained markdown memory. gemini crawls up to the git root merging
  `GEMINI.md` tiers (global→project) + recursive `@path` imports. Discover/inject a
  per-directory `KAIJUTSU.md` at hydration — user-editable working agreements that
  attach to a directory without touching kernel config.
- **Date / OS / cwd in situational context — cheap.** `build_system_prompt`
  (`llm/system_prompt.rs:69`) injects id/label/model/tools but *not* today's date,
  platform, or cwd. ~20 tokens kills a class of stale-temporal and platform-wrong
  reasoning. Add fields to `SituationalContext`.
- **`kj memory show`.** No way to inspect the *assembled* system prompt (base + rc
  sections + situational) without reading source files — memory debugging is opaque.
  Add a render command; optionally `kj memory refresh` to hot-reload stance edits
  without re-creating the context.
- **Memory inbox (LLM-proposed durable patches).** Drift targets live contexts, not
  files. gemini lets the model propose memory edits as unified diffs queued for
  user apply/dismiss. A file-targeting analog of drift: model proposes a stance/memory
  patch → inbox → user reviews before it takes effect.

### Tools

- **`web_fetch` + `web_search` builtins.** Zero web-acquisition tools exist (reqwest
  is LLM-API-only). Without a fetch primitive the instrument can't research anything
  not pre-loaded; every harness must BYO scraper MCP. Add a `builtin.web` server:
  HTML→text fetch (rate-limited, private-IP block, untrusted-content wrapper) + search.
- **Background shell + process management.** `builtin.shell` is synchronous only —
  no `is_background`, no PID registry, no tail-read companion. Long builds/test-suites/
  service-starts can't be modeled without serializing. Add `is_background` +
  `list_background_processes` / `read_background_output`.
- **`read_many` (multi-glob batch read).** Today: glob then loop-read. gemini
  `read_many_files` expands patterns, reads all matches (incl. images/PDF/audio),
  returns one joined payload with per-file truncation markers. Saves turns on
  codebase-wide context loading.
- **Omission-placeholder validator on edits — fits "crash over corruption."**
  `EditEngine` validates the `old_string` match but doesn't scan `new_string` for LLM
  shorthand (`// rest of code…`, `(unchanged)`) — so a placeholder gets applied
  verbatim, corrupting the file *past* the hash check. Reject pre-apply. Directly
  serves the no-silent-corruption directive.
- **Structured `ask_user` tool.** `KjResult::Latch` is a single destructive-op
  confirmation, not a model-callable way to surface ambiguous decisions mid-turn.
  gemini `ask_user` submits a batch of typed questions (text/confirm/choice) that
  block until answered. Kernel supplies the interrupt primitive; harness chooses to
  expose it.
- **Optional edit-correction hook.** ⚠️ When `old_string` misses, gemini runs a
  second LLM pass to repair the search string (fuzzy fallback first). kaijutsu fails
  loud *by design*. Don't auto-repair (corruption risk) — but emit a structured
  error + correction-context block so a harness can opt into recovery, mirroring
  gemini's `getDisableLLMCorrection` toggle.
- **Plan-mode toggle.** `read_only_shell` is a static binding, not a model-asserted
  mid-session mode. gemini `enter_plan_mode`/`exit_plan_mode` flips to read-only with
  a visible reason. A lightweight plan-mode token (vs the heavier fork) for
  single-session exploration constraint, surfaced to the harness via a `KjResult` variant.
- **Socket hook vs. Hook Table alignment.** The legacy MCP socket hook (for session mirroring) has drifted from core structures, causing silent data loss
  (e.g., `agent_id` vs `principal_id` mismatch, obsolete `tool_response` key, fragile PID-based socket discovery). Details in
  [mcp-hook-alignment.md](file:///home/atobey/src/kaijutsu/docs/mcp-hook-alignment.md).
- **Silent fallbacks in tool/binding lookup.** [Kernel::list_tool_defs_via_broker](file:///home/atobey/src/kaijutsu/crates/kaijutsu-kernel/src/kernel.rs#L465) maps lookup errors
  to empty vectors, silently stripping the LLM of tools. [dispatch_tool_via_broker_with_cancel](file:///home/atobey/src/kaijutsu/crates/kaijutsu-kernel/src/kernel.rs#L336) defaults to empty bindings on lookup
  failure, causing confusing `ToolNotFound` errors rather than propagating the underlying DB/resolver error.
- **Latency overhead on visible tool scans.** [list_visible_tools](file:///home/atobey/src/kaijutsu/crates/kaijutsu-kernel/src/mcp/broker.rs#L1081) is called on **every single tool dispatch** to refresh naming resolutions, causing lock contention
  on `self.instances` and `self.bindings` and extra async hops. These resolutions should be cached per context and invalidated only when bindings change.
- **Contradictory hook persistence documentation.** [BuiltinBindingsServer](file:///home/atobey/src/kaijutsu/crates/kaijutsu-kernel/src/mcp/servers/bindings_builtin.rs#L64) claims hooks are "in-memory only" when they are actually eagerly hydrated and written to SQLite in `broker.rs`.

### Safety, sandboxing & policy

- **Kernel-level process isolation for kaish shell — HIGH.** EmbeddedKaish runs real
  binaries with the full kernel process's privileges; `WorkspaceGuard` is VFS-layer
  only and is bypassed the moment a builtin shells out via `LocalBackend`. A
  compromised tool can read `/etc/passwd`, `ptrace`, or exfiltrate keys. gemini wraps
  exec in `bwrap --unshare-all` + seccomp (Linux) / seatbelt (macOS). Add an OS-isolation
  wrapper for shell-tool exec, toggled by a capability binding.
- **Env/secret masking for agent-invoked shell — HIGH (supply-chain).** A
  coder-context agent can `echo $ANTHROPIC_API_KEY` — the context env (incl. provider
  keys) is handed to kaish unstripped. gemini bind-mounts zero-byte files over `.env*`
  and strips `*_API_KEY`/`*_TOKEN`/`GITHUB_*` from the sandboxed env. Strip
  credential-pattern vars from the env visible to agent shell commands; configurable allowlist.
- **Network egress cap.** Capability model has no network axis; MCP subprocesses get
  unrestricted sockets. `npm install`/`curl` leak data with no gate — sharper risk
  given the multi-user SSH model. Add a `network` binding axis (deny-by-default),
  enforced via net-namespace when OS isolation lands.
- **Declarative policy loader + argument-pattern deny rules.** *(merged: the
  sandboxing and extensibility scans both hit this.)* Bindings are coarse (whole
  tool/instance, no arg matching) and only authorable via kaish hook syntax or
  `builtin.bindings`. gemini has a tiered TOML engine (Default<Workspace<User<Admin),
  `argsPattern` regex, `allow`/`deny`/`ask_user`. Add a TOML/`policy.kai` loader that
  hydrates PreCall Deny/Allow hooks from declarative rules (tool glob + args pattern +
  decision) at create time — e.g. "deny `write_file` to `~/.ssh/*`" without writing Rust.
- **Workspace rc trust gate.** No "do you trust this project?" gate before executing
  `.kai` rc from a workspace dir — a malicious rc runs on context create, affecting an
  always-on multi-user kernel. gemini gates project config behind a trust dialog that
  audits discovered commands/MCP/hooks. Require operator approval before running rc
  from a non-trusted-root; surface discovered rc/mcp/binding config first.
- **Sandbox-expansion protocol.** `WorkspaceGuard` denies a path with a hard failure
  and no escalation. gemini surfaces a "grant session/persistent?" modal on denial.
  Emit a structured expansion request (Cap'n Proto event) so the operator can grant
  session-scoped paths without tearing down the context.
- **Pre-execution veto hook (external checker protocol).** MCP hooks fire on
  lifecycle events, not as a pre-call veto. gemini runs external checker subprocesses
  via a versioned JSON protocol (stdin: tool call + context; stdout: allow/deny/ask;
  fail-closed on timeout). Lets operators plug in compliance/content/rate-limit checks
  without patching the kernel — a clean instrument capability.
- **LLM-derived task policy (conseca-analog).** ⚠️ Risky as a sole gate (LLM error
  ⇒ allow). gemini derives per-prompt least-privilege constraints from the request +
  tool list, then enforces at call time. Only as an *optional secondary* stage after
  static bindings, fail-open with telemetry.

### Session & workflow

- **Pre-edit filesystem checkpoint + `kj restore` — HIGH.** `KernelState::checkpoint`
  (`state.rs:160`) snapshots in-memory vars only, not the host FS, and isn't tied to
  tool execution. A bad edit run leaves files half-modified with no mechanical rollback.
  gemini auto-commits a shadow git snapshot before every file-write tool, with
  `/restore`. Auto-snapshot + `kj restore <checkpoint>` to revert FS + conversation.
- **Turn rewind + FS revert.** `kj fork` is a forward branch (explore), the inverse
  of "undo that last edit." gemini `/rewind` walks back N turns and reverses file
  edits (exact-match, patch-merge fallback) with a diff preview. A backward escape
  hatch that doesn't spawn a new context branch.
- **Named conversation bookmarks (save/resume in-place).** Fork diverges history;
  there's no "park this state, try another direction in the *same* context, snap back."
  gemini `/resume save|resume|delete <tag>` snapshots and restores LLM wire history
  in place. Distinct from fork — avoids unbounded DAG branching for quick what-ifs.
- **User-defined prompt command templates.** rc scripts fire on lifecycle events;
  there's no user-authored named command. gemini loads `.toml` from
  `~/.gemini/commands/` + project dirs → `/git:commit` etc. with `{{args}}`. Add
  `~/.config/kaijutsu/commands/` + `<project>/.kaijutsu/commands/`, invocable as
  `kj cmd:<name> [args]`.
- **Inline `@{file}` prompt injection.** The user can't say "here's the file I mean"
  in prompt text — they must wait for the model to choose to call `read`. gemini
  expands `@{path}` (text/image/PDF) in the input before submission. Parse `@{path}`
  in `write_input`, respecting the VFS boundary.
- **`!{shell}` injection in prompt templates.** Pairs with command templates: gemini
  expands `!{git diff --staged}` stdout into the prompt at construction time (policy-
  confirmed), outside the model's tool loop — e.g. a `/git:review` one-liner.
- **Conversation export.** `block_list`/`block_read` extract internally but nothing
  produces a portable file. Add `kj conversation export <path.md|json>` for sharing/
  bug-reports/archival outside the system.

### Extensibility & integration

- **Turn- and model-boundary hooks — HIGH.** The hook table (`mcp/hook_table.rs`:
  PreCall/PostCall/OnError/OnNotification/ListTools) is scoped to MCP tool calls only;
  the socket listener (`hook_listener.rs`) is an inbound *mirror*, not an outbound
  interceptor. gemini has BeforeAgent/AfterAgent/BeforeModel/AfterModel that can
  block/rewrite. The kernel owns the turn loop (`llm_stream.rs`) — add BeforeModel/
  AfterModel + BeforeTurn/AfterTurn phases so rc scripts can reshape requests/responses
  (cache hints, PII filter, retry) without bespoke Rust. **Decided 2026-06-24 — see
  *Cache & cost* below:** phase named `BeforeModelTurn`/`AfterModelTurn`; rename the
  existing MCP `PreCall`/`PostCall` to MCP-scoped names; mechanics(Rust transport) /
  policy(per-provider data) / decisions(kaish hook) split; contract = `HookAction`
  verdict + stdout→block payload (append-only).
- **Headless one-shot with JSONL streaming — HIGH.** `kj drive --prompt`
  (`kj/drive.rs:93`) fires-and-returns; the turn runs server-side with no
  consume-until-done path. CI/eval harnesses need a blocking subprocess. Add
  `kj run --prompt … --output-format jsonl` that streams turn events
  (turn.requested/tool_call/tool_result/turn.completed) and exits with a machine code.
  *(relates to the existing "headless turn cwd is `/`" item.)*
- **Python/TS thin SDK.** `kaijutsu-client` is full-featured but requires Rust
  compilation; eval/CI tooling lives in Python/TS. Wrap `kj run --json` JSONL (or the
  RPC bindings) into an async session driver so harnesses don't compile Rust.
- **IDE peer integration.** No editor bridge (`kaijutsu-editor` is a terminal vi
  builtin, not an IDE plugin). The peer model (`PeerRegistry`/`invoke_peer`) already
  fits: a VS Code extension registers as a kaijutsu peer, sends open-file/cursor/
  selection blocks into the active context, and renders kernel-proposed edits as diffs.
- **Extension bundle manifest.** rc bundles exist but with no named-unit manifest,
  install/update lifecycle, or scoped enable/disable. gemini's `gemini-extension.json`
  bundles MCP servers + hooks + commands + context as one versioned, git-URL-installable
  unit. An `extension.toml` (rc scripts + contrib adapters + context configs) installable
  via `kj extension install <git-url>` — configures the instrument, doesn't host it.
- **Hook fingerprinting / trust.** CRDT ownership is the integrity model but there's
  no change-detection warning when an rc/hook body changes via `kj rc reset` or sync
  (extends the existing "stale rc seed" item). gemini fingerprints project hooks and
  warns on change. Track hook-body hashes; warn/block-by-default on unexpected change.

### Cache & cost — decided direction (2026-06-24)

A working session with the lead context converged several candidates above into
decisions. Organizing lens: **the Anthropic prompt cache is a prefix match — any byte
change in the `tools → system → messages` prefix invalidates every cached token after
it** (writes 1.25×/5m, reads ~0.1×, ≤4 breakpoints, model-scoped). We already ship the
machinery: `cache_breakpoints: Vec<CacheTarget>` (`llm/stream.rs`), set per-context via
rc create/fork/drift (`project_cache_breakpoint_policy`); `usage.cache_*` parsed back
(`llm/claude/stream.rs`). So these are placement/policy decisions, not new infra.

- **Cache placement is load-bearing, not cosmetic.** Three rules fall out of the prefix
  invariant and should hold by construction:
  - **Date/OS/cwd in situational context** (the "cheap ~20 token win") is a *silent
    invalidator* if it lands in the cached `system` prompt — date rolls at midnight, cwd
    churns, blowing tools+system every change. MUST land *after the last breakpoint* (a
    message), never in `build_system_prompt`.
  - **JIT `KAIJUTSU.md` injection** must *append to the tool result* (extends the prefix,
    cache-neutral), not re-hydrate into `system` (mutates prefix, cache-hostile). Same
    content, opposite cost by placement.
  - **Model switching invalidates the whole cache** (model-scoped). Classifier routing /
    fallback-chain must therefore be fork/subagent-grained, never per-turn — reinforces
    the ⚠️ opt-in framing.
- **Compression: not pursued.** SQLite-on-btrfs (compressed) covers storage for a long
  horizon; conversations flush organically to `signoff.md` near ~80% window and restart.
  If it ever lands, it fires only at the fork/hydrate boundary (cache already cold),
  never mid-conversation.
- **AdaptiveTokenCalculator — EMA, not PID.** Token estimation is an observer problem,
  not control: use an **EMA** for the chars→tokens ratio, calibrated by the provider
  `usage` we already parse. No local Claude tokenizer exists and `tiktoken` is wrong for
  Claude, so the loop is: local estimate gates the (block-count) windowing in
  `mailbox.rs` + a near-limit warning; provider `usage` corrects the ratio after each
  turn. A static **per-model input-limit table** is just config and kills the "blindly
  400'd by the provider" case on its own. Optional follow-up: escalate to the
  `count_tokens` endpoint only when the estimate is within ~10% of the limit. No
  budget→window controller — windows aren't dynamic in practice.
- **Per-turn seam: `BeforeModelTurn` / `AfterModelTurn`.** A new turn-loop hook phase,
  *distinct from* the MCP-tool-call hooks. **Rename the existing `PreCall`/`PostCall`
  (`mcp/hook_table.rs`) to MCP-scoped names** — they only fire around MCP tool calls — so
  the two surfaces are separable and a script can subscribe to just one. Design:
  - **Mechanics compiled, policy as data, decisions as hooks.** The retry *loop*
    (backoff, jitter, `Retry-After`, SSE re-issue) is one Rust implementation in the
    transport. The retry *policy* is a per-provider data table (max attempts, base delay,
    jitter %, retryable codes). "Gemini has different retry needs" (e.g.
    `RESOURCE_EXHAUSTED` vs bare 429) is a **policy row, not a code fork** — folds into
    the declarative-policy-loader item. Per-turn *decisions* are the kaish hook surface.
  - **Engine always runs with sensible defaults** — no "zero-overhead when unhooked"
    special case; the retry/policy engine works unconfigured. A *slow* hook script is the
    author's problem, not the framework's.
  - **Append-only / transport-wrapping only** — a hook may append a `role:"system"` note
    (cache-safe mid-conversation injection on Opus 4.8) or wrap the call; it must never
    rewrite the cached prefix. Enforced by the channel shape below.
  - **Contract — three channels, each already precedented:** verdict =
    `HookAction::{Allow, Deny(reason), Log}` (mirror the existing MCP hook return, don't
    invent a parallel protocol); payload = **stdout → block** (the `rc .kai` stdout-
    producer idiom — stdout becomes an *appended* block, so a hook physically cannot
    rewrite the prefix; System/Text → mid-conversation system note, Trace → model-hidden
    usage capture for the EMA); side effects = the script calling builtins (KV, drift),
    its own business, *not* the verdict path (a tool call as the return path is a
    reentrancy trap). stdin carries the event-kind + assembled-request metadata (model,
    context_type, token estimate).
- **Fork-boundary rc vs per-turn hook — don't conflate.** Fork-boundary rc owns
  *context-shaping* and runs once per hydrate boundary: transplanting a conversation (or
  a selected interval) into a new `context_type` is fork-with-filters — the interval
  primitive is already LOCKED (`docs/fork-filters.md`), and retargeting `context_type`
  just runs that type's create rc. The per-turn seam owns only the reactive/mechanical
  (retry, estimate-gate, usage capture). Rewriting the request every turn would fight the
  cache by construction — keep that out of the per-turn hook.

**Remaining work (not yet code):**
- **`HookPhase` → `McpHookPhase` rename — SHIPPED 2026-06-24.** All five variants are
  MCP-broker-scoped, so the *enum* was renamed, not the variants; persistence strings
  (`pre_call`…) unchanged (empty DB, no migration). Frees `BeforeModelTurn`/
  `AfterModelTurn` to be a sibling enum.
- **Per-model input-limit table** — static config + `model_input_limit(model) -> Option<u32>`.
  Kills the "blindly 400'd by the provider" case on its own; foundation for the calculator.
- **AdaptiveTokenCalculator** — EMA chars→tokens ratio, calibrated by the provider `usage`
  already parsed at `llm/claude/stream.rs`. Feeds the (block-count) windowing in
  `mailbox.rs` + a near-limit warning. No local Claude tokenizer; `tiktoken` is wrong.
  Optional follow-up: escalate to the `count_tokens` endpoint only within ~10% of the limit.
- **`RetryPolicy` data type + per-provider table** — one Rust backoff engine (jitter,
  `Retry-After`, SSE re-issue) reads it; provider divergence (gemini `RESOURCE_EXHAUSTED`
  vs bare 429) is a policy row, not a code fork. Engine runs with sensible defaults even
  unconfigured (no zero-overhead-when-unhooked special case).
- **`BeforeModelTurn`/`AfterModelTurn` sibling phase** (e.g. `ModelTurnPhase { Before, After }`)
  on the LLM turn loop. Contract: `HookAction` verdict + stdout→block payload (append-only)
  + side-effects-via-builtins; stdin carries event-kind + assembled-request metadata.
  ⚠️ **OPEN FORK: reuse the `HookEntry`/`HookAction`/kaish-body/persistence stack, or a
  parallel table? Decide before laying code.**
- **Encode the cache-placement rules by construction:** situational date/OS/cwd lands
  *after* the last breakpoint (a message, not `build_system_prompt`); per-directory
  `KAIJUTSU.md` *appends to the tool result*, never re-hydrates `system`.

---
## kaijutsu-abc — ABC v2.1 spec conformance (audit 2026-06-30)

Three-model holistic audit (deepseek interactive ×2, gemini-pro batch, opus batch) over the
full verbatim spec cache + all crate code. **14 bugs fixed TDD this round** (failing test →
fix), suite 320 → 336 green. Shipped: tempo `Q:` beat-unit; multi-measure-rest `Z`
denominator; tuplets honouring inner rests/chords; `K:` explicit accidentals (`K:Hp`/`exp`);
sharp-minor/modal key signatures (`G#m`…); chord inner-note durations (`[c4a4]`); tie carrying
an accidental across a bar line (was a hung note); inline mid-tune `[K:]`; mode abbreviations
(`mixo`); `K:Bbm` parse; `A//`/`///` durations; `to_abc` note-accidental round-trip (`^`/`_`
not `#`/`b`); first/second/`[N` variant-ending expansion (+ `|2`/`:|N` parser labels);
`K:` `transpose=`/`octave=`.

**Still open:**
- **LOW — tuplet default-q for `(5 (7 (9` ignores compound meter** (3 in 6/8). §4.13. Skipped:
  `default_q` is computed in `try_parse_tuplet` with no meter access; threading the meter
  through `parse_body → … → try_parse_tuplet` is high churn (10 test call sites) for a rare
  corner (5/7/9 *without* explicit `:q` *in compound meter*).
- **LOW — `Duration::to_ticks` integer-truncates** (odd denominators; inaudible at 480 TPQN).
  Would need rational accumulation; leave unless it bites.
- **LAYOUT (rendering phase) — `+:` continuation corrupts lyric alignment** (joined with `\n`;
  `tokenize_lyrics` doesn't treat `\n` as whitespace). §3.3.
- **LAYOUT (rendering phase) — lyrics `w:` `|` barline-sync marker ignored** (v1 limit). §5.1.
- **Engrave parity (rendering phase):** `engrave/layout.rs` has its own copies of the
  tuplet-drops-rests/chords and key-signature bugs — fix when we move to rendering.

**Shipped since the audit (this MED/LOW round):** `X` invisible multi-measure rest; broken
rhythm transparent to chord/grace neighbours (§4.4/4.12/4.17); inline mid-tune `[M:]`/`[L:]`
in MIDI; `%%MIDI transpose`; short-form decorations H/T/u/v; aligned the dead
`ast::Note::to_midi_pitch` octave convention; plus div-by-zero guards from Round 2's fuzz.

**Verified NOT bugs (don't "fix"):** cross-octave accidental propagation (spec default
`%%propagate-accidentals pitch` = all octaves); unit-length default; broken-rhythm multipliers.

---

## Players / loadout

- **EXPLORE — give players a read-only kaish instead of "tool-free" (found 2026-06-30,
  standing up the bass player).** Today a musician's loadout grants only `drive` and **no
  tools at all**, because a small local model handed the full tool palette emits a thinking
  block then *hangs* (GPU cold, no completion, no error — a fail-loud violation; the
  hard-won Chameleon lesson, `project_chameleon_first_loop`). "Tool-free" was the blunt
  fix. The better future: a **read-only kaish** loadout — the same RO-kaish posture kaibo
  already uses (reads the repo, never mutates), which is *great* for cheap on-the-fly
  arithmetic/lookups that are cheaper via a tool than via the model's weights (true for
  humans and models alike). A player could compute bar math, transpositions, scale degrees,
  etc. with RO kaish rather than burning weights or risking a wrong count. **Not wiring
  this now** — the immediate bar-fill math is precomputed in the tick rc (kaish math in
  `musician/tick/S10-drive.kai`, injected as spelled-out facts), so the model needs no tool.
  But RO-kaish-for-players is worth designing: it removes the "tool palette = hang" cliff by
  construction (no mutation surface to stall on) and makes the calculator-as-tool option
  real. Pairs with the precompute-in-rc win (rc does the arithmetic) — RO kaish is the
  *escape hatch* for math the rc didn't precompute. Decide: which RO builtins (math/`expr`,
  read-only `grep`/`glob`, block/resource reads — but no mutation) + whether small local
  models tolerate a *read-only* palette where they choke on the full one.

## kaijutsu-abc — engrave (SVG rendering) audit (2026-06-30, kaibo/deepseek)

Audit of engrave/layout.rs. The two parallel bugs (tuplet drops rests/chords; sharp-minor
key sig) are FIXED via shared `Key::signature()` + tuplet render arm (commit d722f492).
Remaining, ranked; delete when shipped. (Most are IR-assertable in tests/engrave_tests.rs.)

**Shipped (commits d722f492, 8fb17d87, + this round):** shared `Key::signature()` (sharp-minor
keys); tuplet renders rests/chords; augmentation dots on dotted notes/rests; chord-second
notehead offset; invisible rest `x` draws nothing; whole-rest hangs higher than half;
`M:none` omits the time sig; multi-measure rest H-bar + count numeral; tuplet bracket + numeral;
mid-staff inline `[K:]` clef change (redraws clef + repositions notes) and key-sig redraw.

**Still open:**
- **MED — `K: middle=<pitch>` ignored** (only the per-clef default middle line is used).
- **LOW — grace notes use the regular notehead glyph**, not the SMuFL small notehead.
- **LOW — every `SourceSpan` is hardcoded `(0,0)`**, so click-to-edit span attrs are dead.
- **POLISH — title text can overlap a tuplet bracket** when the first group is near the start
  (title baseline ≈ bracket y); nudge the title up or the bracket down.
- ~~MED — redundant key-sig accidentals~~ — VERIFIED NOT A BUG: the parser doesn't stamp
  key-sig accidentals onto `note.accidental`, so `K:G FFFF` draws exactly 1 sharp. (False positive.)

---

## kj config / shell surface (papercuts — found 2026-06-30 wiring local llama.cpp providers)

Standing up a local-model musician meant editing `models.toml`, which surfaced a
cluster of friction in the config + shell surface:

- **Config drift is silent (want a `kj config doctor`).** The live CRDT
  `models.toml` pointed its local providers at `ollama` (:11434) and `lemonade`
  (:8000) — both stopped/disabled — with **no provider** for the actually-running
  llama.cpp servers (:2020 gemma4-26b, :2021 gemma4-e4b); the stale host
  `~/.config/kaijutsu/models.toml` pointed at a *third* dead endpoint (vestigial
  lemonade :13305). Nothing flags that a configured provider's `base_url` is
  unreachable until a turn fails (or, worse, hangs). A `kj config doctor` /
  startup probe that pings each enabled provider's `base_url` and warns on the dead
  ones would turn a silent config-vs-reality drift into a loud one (same class as
  the rc/source drift we watch for).
- **`kj config set` ignores piped stdin** even though `--help` says "stdin is piped
  here when omitted": `cat new.toml | kj config set /etc/config/models.toml` →
  `missing content`. Had to use `--content "$(cat …)"`. Either wire stdin through
  or fix the help text (the wrong help is the real footgun).
- **No `kj config edit` and no set-from-path.** `kj rc` has `edit` (opens an
  interactive vi session on the script); `kj config` has only `show`/`set`/`reset`.
  `show` wraps the body in a `path:`/`length:` header + ```` ```toml ```` fences, but
  `set` wants the *raw* body — so editing a 6 KB file is a clunky show→strip→edit→set
  round-trip. Add `kj config edit` (mirror `kj rc edit`) and/or `set --from <path>`.
- **The MCP/context shell is read-only for host writes** — `> file` (even
  `> /dev/null`) fails `redirect: read-only filesystem`, so you can't stage a temp
  file in-shell; the edit had to be staged via a separate host write and read back.
  If RO is intentional, a `/dev/null` sink and a writable scratch dir would remove
  the sharp edge for scripting.
- **`kj context create` took ~60 s** for one musician (others were instant) —
  anomalous, possibly a blocking create-time hydrate/model call; worth a look.
- **(unreproduced) a `kj` shell call hung the full 300 s timeout once** mid-session;
  `kj context list --tree` and `kj model list` both return fast now, so noting it
  as a one-off to watch, not a fixable repro yet.
- **A provider's NAME must be a built-in TYPE — and an unknown one fails silently
  (found 2026-06-30, the real blocker for local models).** `[providers.<name>]`'s
  `<name>` *is* the provider type, matched against a fixed set
  (`anthropic, deepseek, openai, ollama, lemonade, local` — the last four all the
  one OpenAI-compatible client keyed by `base_url`). A sensible-looking
  `[providers.local-e4b]` is **silently dropped at startup** — logged as
  `WARN failed to initialize provider (missing API key?) ... Unknown or unsupported
  provider type: local-e4b` (the "missing API key?" is misleading; the real cause
  is the bad type) and then skipped, so the kernel boots "fine" and you only learn
  at `kj context create` ("unknown provider"). Two fixes: (1) **validate provider
  names at `kj config set`** (reject/​warn on an unknown type then, not silently at
  boot) — a config doctor; (2) **drop the "missing API key?" guess** from that warn
  and say "unknown provider type 'X' (supported: …)". Bonus: with only the 4 fixed
  OpenAI-compat type-names, you can't have two *distinct-named* local servers —
  you reuse `ollama`/`lemonade`/`local`/`openai` as base_url slots, which reads
  oddly (a llama.cpp server named `lemonade`). Consider a real `type =` field so a
  provider can be named freely (`[providers.gemma-e4b] type = "openai"`).
- **Two `models.toml` files, only one is read.** The kernel loads providers from
  the **CRDT** `/etc/config/models.toml` (via `kj config`), and the legacy host
  `~/.config/kaijutsu/models.toml` is **ignored** — but it still exists, looks
  authoritative, and disagrees (it had a vestigial `openai-local` → dead :13305).
  Editing the host file does nothing; you must `kj config set`. Either delete the
  host file on migration, or have the kernel warn that it found+ignored it. (Same
  CRDT-vs-host ownership confusion as rc, but here there's a stale host artifact
  actively misleading.)
