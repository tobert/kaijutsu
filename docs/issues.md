# Open Issues

Live work items distilled from prior design and TODO docs, plus architectural observations from code reviews. Code is truth; this exists to track what's *not* in the code yet.

Organized by area. Keep entries terse — link to file:line when a pointer makes the work concrete. When an item ships, delete the entry — if the "how we got here" is worth keeping, move the narrative to [`devlog.md`](devlog.md) (the landed-work story). See the three-file working-notes pattern in `CLAUDE.md`.

---

## Theme changes don't repaint existing conversation blocks (2026-08-12)

`kj config set /etc/config/theme.toml` reaches the app live (ThemeReceived →
`sync_block_fx` re-syncs material uniforms every frame), but MSDF *texture
content* re-renders only when a surface's glyph version advances
(`ExtractedMsdfBlockData.last_rendered` in `view/block_render.rs`). So
knobs baked at raster time (`msdf_gamma_correction`, `msdf_stem_darkening`,
glyph colors) leave already-rendered blocks stale until they re-render for
some other reason. Material-side knobs (`text_glow_*`, borders) do apply
live — the split is invisible from the theme file. The dock
(repaints ~4×/s) updates instantly, which makes the conversation staleness
look like a bug in the theme push. `docs/color.md` sells `kj config set` as a
"live color-management console" — either bump every `MsdfBlockGlyphs.version`
on ThemeReceived (cost: one full repaint per theme change, fine) or document
the raster-time carve-out. Found while live-tuning gamma (2026-08-12).

## `kj config show` output is not round-trippable, and a corrupt theme falls back silently (2026-08-12)

Two contributing factors that compounded into an hour of phantom results:

1. `kj config show <file>` prints a human header (`path:`, `length:`, blank
   line, ` ```toml ` fence, closing fence). Piping it into `kj config set`
   — the obvious sed-tweak idiom, which the shell happily accepts — embeds
   the header into the stored file; each roundtrip nests another copy.
   Wants a `--raw`/`--body` flag (or: `set` could refuse content whose first
   line matches its own `show` header — it is never intentional).
2. When the stored theme TOML fails to parse, the app falls back to
   compiled-in defaults **silently** (no dock indicator, nothing in the
   conversation). Every subsequent theme experiment silently tested the
   defaults instead. Doctrine says crash > corruption; here at minimum the
   parse error should surface loudly (drift notification / dock error glyph).
   Repro: any `show | sed | set` roundtrip before (1) is fixed.

## Dock RTT sizes skip physical-px rounding (2026-08-12, kaibo find)

`render_north_dock`/`render_south_dock` stamp `rtt.built_width = logical.x`
raw (`ui/dock.rs`), while block cells round via `round_to_physical_px`
(`view/block_render.rs:1222`). At fractional DPI that makes
`msdf_item_scale` a hair off exact, giving sub-pixel glyph drift on the
dock. Cosmetic, pre-existing (found during the premultiplied-compositing
review, deepseek job-1); fold into the tier-2 "unify RTT resize" cleanup.

## text_glow wants a re-tune with Amy's eyes (2026-08-12, post-blend-fix)

The violet text halo (`text_glow_radius = 2.5`, `#cbb8ff59`) now renders at
its true designed strength — the straight-alpha compositing bug had been
crushing it since birth, so its tuned values have never actually been seen
on screen. At a 960×600 window it reads as fuzz (it was most of the "text
looks fuzzy" report), with glow off the text is *crisp*; at 4k it may well be
the intended synthwave neon. Decisions: strength/alpha, and whether a
fixed-pixel radius should scale with font size / scale factor. Live CRDT
currently has it OFF for Amy to eyeball; repo seed still ships 2.5.

---

## kaish captured stdout truncates SILENTLY at 8 KB (2026-08-13)

`OutputLimitConfig::agent()` (`kernel/src/runtime/embedded_kaish.rs:264`) caps
a builtin's captured stdout at 8192 bytes. Crossing it does **not** error and
does **not** set a failure code — the capture collapses to a ~1.6 KB
head+tail splice with `[output truncated]` between them. Command substitution
therefore hands the script a *plausible, wrong* value.

Found the expensive way during slice-1 rc work: an unbounded
`grep '^- \[' MEMORY.md | grep -c .` reported **12** where the truth was
**105** — the first pass overflowed the cap, and the second counted the
splice. Nothing anywhere reported a problem.

This is the silent-fallback shape CLAUDE.md rejects, and rc scripts are its
worst host: an rc script computing a digest, a count, or a hash off a
truncated capture writes a confident wrong fact into a context, and per the
`create` blast radius it does so on every session registration.

Candidate fixes, cheapest first: make truncation set a nonzero status or a
shell var an author can test; or emit the truncation notice on stderr as well
so it is visible in the Trace block; or raise the cap for rc-context shells
specifically. Any of the three beats the current behavior. Until one lands,
the discipline is **bound every read before it lands in a variable**
(`head -n`, a filter) — now recorded in `memory.md`'s mechanics section.

## Memory system — direction decided, slices open (2026-08-13)

Direction is now canonical in [`memory.md`](memory.md): **the kernel is
memory's best reader, not its new owner** — git keeps storage, kaijutsu grows
recall. Read it before proposing anything memory-shaped; several attractive
designs are explicitly dead there (CRDT memory tree, memory-as-contexts, fact
schemas), and one rule binds work well outside memory:

- **The derived-state rule.** "No second store" is enforced by asking *"can
  this state disagree with truth **silently**?"* — `derived` is not an
  exemption. Applies to every cache, index, and mirror we add from here.

Open slices, in `memory.md`: S15 recall script + baseline classification
(zero Rust), `kj memory search|recall` over MCP, write-back proposals with a
named drain, and the two kernel-only capabilities (per-turn mailbox recall,
change-push). Semantic indexing over the forests is **deferred with reasons**
— the index is `ContextId`-keyed end to end and HNSW's never-reuse slots leak
a permanent graph point per re-embed, which a git-tracked corpus edited
hourly would punish.

Two blockers that memory work now co-owns, both already listed elsewhere in
this file: **hook self-lockout has no recovery path** (third independent
reason to fix it — it gates the constraint-hook seam), and the
**external-drive gate** is a prerequisite for the resident-assistant seat.

## Block-authorship leftovers from the identity-smear split (2026-08-13)

The split itself shipped: drift blocks carry the sender, rc-lifecycle blocks
carry the context owner. Two adjacent things were left alone on purpose.

- **`BlockStore::insert_drift_block` is a silent-default wrapper** — it calls
  `insert_drift_block_as(…, None)`, and `None` falls back to
  `BlockStore::principal_id()`, the kernel's own identity
  (`kernel/src/block_store.rs:3059`). That fallback is *how* the smear stayed
  invisible for months: every drift looked like it came from the same
  anonymous place and no call site had to think about authorship. The
  drift paths now pass an explicit principal, but the wrapper survives and
  the next call site will inherit the same default. Candidate fix: delete the
  wrapper and make every caller pass `Option<PrincipalId>` explicitly, so
  "no author" becomes a decision the compiler forces rather than a default
  someone gets for free. Small and mechanical — 3 remaining call sites.
- **Fork markers and fork notes still author as the kernel.**
  `inject_fork_note` (`kj/fork.rs:1442`), the fork-marker insert
  (`kj/fork.rs:1499`) and the `--compact` distillation seed (`kj/fork.rs:827`)
  all go through the wrapper, so they carry the store's principal rather than
  the child context's owner. Out of scope for Amy's drift ruling, and the
  practical impact is smaller (for a fork, `created_by` is the forking caller
  anyway), but it is the same defect wearing a different hat. Needs a
  principal threaded through two helpers that currently take only
  target/source ids.

## Two seeds from the rc-create lockout fix (2026-08-12, `788fb0d7`)

Found while closing that item; recorded rather than folded in, because
neither is that bug.

- **`test_dispatcher` leaves the broker's DB handle unset**, so
  `broker.set_binding` (and therefore `kj binding allow`) lands only in the
  in-memory cache — which `require_cap` and `has_usable_loadout` never read,
  by design: they read KernelDb, the authoritative store the broker
  write-through targets in production. The rebind repair test hit this the
  loud way: the rc script reported `allowed operator on context …` and the
  loadout stayed empty. Fixed *in that one test* with
  `broker().set_db(kernel_db.clone())`. **Open question: which other binding
  tests on this fixture are asserting against the cache instead of the
  store?** `kj::drive`'s gate tests already know (they write straight to the
  DB with a comment explaining why) — so the knowledge exists and is applied
  ad hoc per test. Candidate fix: wire the DB in `test_dispatcher` itself, so
  the fixture matches production and the per-test workaround stops being
  something each author has to know.
- **Sweep the authz-shaped `.ok().flatten()` / `unwrap_or(false)` sites.**
  The "errors that were only strings" pass (Aug 3–4, devlog) fixed this
  collapse in `Broker::binding_checked`; `require_cap` held an untouched copy
  for months, where a KernelDb read failure was indistinguishable from a
  missing grant. The family is "a fault silently becomes a policy decision" —
  worth grepping deliberately in gate/predicate paths rather than waiting for
  the next one to surface. Note the direction of the risk: deny-by-default
  makes the *safe* wrong answer easy to ship and hard to notice.

## Summaries drift stronger than what they summarise (2026-08-11, three instances in one day)

Not a code bug — a writing failure mode worth naming, because it cost real
scoping work today:

1. This file claimed "isotest proved fresh-$HOME boot ... and no host state".
   isotest proves process-lifecycle guarantees; fresh-$HOME is a precondition
   demonstrated by construction. Work on brak was scoped against the stronger
   reading (see the brak entry).
2. `crates/kaijutsu-server/src/clock.rs`'s own module header called
   `ModeledClock` "an uninhabited placeholder ... you cannot construct one
   until M3". It has had a body, a `ClockSource` impl, a persistence path, a
   `kj` selector and an end-to-end producer for weeks — and was contradicted
   by `from_persisted` a hundred lines below it in the same file. Fixed.
3. `docs/tracks.md` inherited (2) and listed M3 as "Ahead".

Same shape every time: someone summarised what was *observed*, the summary read
stronger than what was *pinned*, and the next reader inherited the stronger
version without re-deriving it. Note the direction is not always optimistic —
(2) and (3) *under*-reported shipped work, which is how a finished subsystem
stays invisible.

Candidate convention, **not yet a rule — Amy's call**: a claim about what is
*proven* carries the assertion name or a `file:line`, so the next reader can
tell pinned from observed at a glance. Three instances is a pattern; a rule
wants her word.

---

## rmcp protocol-version fallback drops us to 2025-11-25 (2026-08-11, via kaibo lead; re-verified against our own version)

**Trigger has a date, not a probability: the day a client requests a protocol
version newer than `2026-07-28`.** Claude Code is the client that will do it.

kaibo lead flagged this as a latent bug across rmcp-based servers. It applies
to us, but **two of the relayed specifics are wrong for rmcp 3.0.1, which is
what we pin** (`Cargo.toml:141`; kaibo is on 3.1.2) — and the difference
matters because the recommended fix does not fix the actual failure.

Read from `~/.cargo/registry/.../rmcp-3.0.1/`:

- **`supported_protocol_versions` is NOT 3.1.0+**, and is not our problem. It
  exists in 3.0.1 (`src/handler/server.rs:328`) and its trait default already
  returns `ProtocolVersion::KNOWN_VERSIONS`, which **includes**
  `V_2026_07_28` (`src/model.rs:186`). So the "strict client rejects
  tools/list" path (`src/handler/server.rs:65-71`) does not fire for
  2026-07-28. We do not override it, and we should not need to.
- **A client asking for `2026-07-28` is honored, not downgraded.**
  `negotiate_protocol_version` (`src/service/server.rs:464-478`) returns the
  client's requested version whenever it is in `KNOWN_VERSIONS`.

The real exposure is narrower and worse:

- For a version rmcp 3.0.1 does **not** know — i.e. anything newer than
  `2026-07-28` — negotiation falls back to `server_fallback`, which is
  `get_info().protocol_version`. Our `get_info` (`crates/kaijutsu-mcp/src/lib.rs:2130`)
  uses `ServerInfo::new(...)` and never calls `with_protocol_version`, so it
  takes `ProtocolVersion::default()` → `LATEST` → **`V_2025_11_25`**
  (`src/model.rs:175`). We would not fall back one step to 2026-07-28; we
  would fall back **two**, past a version we fully support.
- Not fully silent: it emits `tracing::warn!("client requested unsupported
  protocol version; falling back to server default")`. Whether we would *see*
  that in a stdio server's log is a separate question worth answering.

**The fix is `with_protocol_version`.** Setting
`ServerInfo::new(...).with_protocol_version(ProtocolVersion::V_2026_07_28)`
makes the fallback land on the newest version we actually implement. Cheap,
and it converts a two-step silent-ish regression into a one-step one.

**Version-qualified, and this is the part that will bite on a bump:** *on
3.0.1*, overriding `supported_protocol_versions` cannot help, because
`negotiate_protocol_version` never consults it — it reads the hardcoded
`KNOWN_VERSIONS` (`src/service/server.rs:468`). **That mechanism is fixed
upstream in 3.1.2**, where negotiation *does* consult the server's supported
set (kaibo lead's read of their own vendored 3.1.2: `service/server.rs:587-591`,
with a `server_supported.contains` check at `:474`). So the *lever* is
correct on both, but the *reason it works* differs — do not carry this
paragraph's reasoning across an rmcp bump without re-reading the source.

Re-check on any rmcp bump generally: the constants (`LATEST`,
`KNOWN_VERSIONS`, `STANDARD_HEADERS = V_2026_07_28`) and the negotiation path
both move underneath us. Related: the SEP-2577 deprecations papered over in
the 1.7 → 3.0.1 bump are still open above.

**Practice this produced** (kaibo lead, after each of us read our own pinned
copy and reached different true answers): *"neither of us should have
inherited the other's read of a differently-pinned crate."* A dependency
analysis is a claim about a version, not about a crate.

Skew note: `~/bin/kaijutsu-mcp` is a **separately built binary** — a fix here
does not reach a running client until that binary is rebuilt and relaunched.

## MCP 2026-07-28 adoption — four slices, in priority order (2026-08-11, post rmcp 3.1.2 bump)

We now negotiate `2026-07-28` on both surfaces and run rmcp 3.1.2. That opens
four things worth having. Ordered by value; Amy approved working them in this
order. Verified against the vendored SDK, not training memory — re-verify on
any rmcp bump (see the version trap in the rmcp entry above).

### 1. Elicitation — we silently decline every request today

`BrokerClientHandler` (`crates/kaijutsu-kernel/src/mcp/servers/external.rs`)
does not override `create_elicitation`, so rmcp's trait default
(`handler/client.rs:179-191`) **auto-declines every elicitation, silently**. An
external server asking us a question gets a "no" nobody ever sees. That is the
silent-fallback pattern CLAUDE.md rejects, and the socket for the fix already
exists: `ServerNotification::Elicitation` (`mcp/server_like.rs`, `mcp/types.rs`)
is defined and its one consumer no-ops it (`mcp/broker.rs`, *"Reserved per
D-25; no live handling yet"*). Nothing constructs it.

Mechanism: a server sends `elicitation/create` in one of two modes — **form**
(`message` + `requested_schema`, a restricted JSON Schema of primitives only,
`model/elicitation_schema.rs`) or **URL** (`message` + `url` +
`elicitation_id`, for out-of-band consent flows). We answer
`ElicitResult { action: Accept | Decline | Cancel, content }`.

Slices:
- **1a — stop being silent.** Override `create_elicitation` to emit
  `ServerNotification::Elicitation` and surface it as a block, then decline.
  Same outcome as today, but *visible*. Small, no policy questions.
- **1b — let someone answer.** In the many-hands model the answer can come
  from a human, a sibling context, or the app — the protocol does not care
  who. Wants a real design pass: where the pending elicitation lives, how an
  answer routes back, timeout behaviour. **Not a patch — a design
  conversation.**
- Blocker for both: `peer.elicit`/`elicit_url` are gated
  `#[cfg(all(feature = "schemars", feature = "elicitation"))]`;
  `crates/kaijutsu-mcp/Cargo.toml` enables only `server`/`macros`/
  `transport-io`, so the **`elicitation` feature must be added**.
- Unknown, needs probing: whether kaibo or bevy_brp ever send elicitation, and
  whether Claude Code's client implements it on the server surface.

### 2. `structuredContent` + `outputSchema` on `shell`

`ShellCompletion::to_json` hand-rolls an envelope, stringifies it into a
`TextContent`, and hopes the caller re-parses. 2026-07-28 has a first-class
mechanism: `Tool::with_output_schema::<T>()` declares the result shape, and
`CallToolResult::structured()` carries it as real JSON
(`model.rs` `structured_content`). Keep the existing field names
(`stdout`/`stderr`/`exit_code`/`status`/`block_id`/`content_type`/`ephemeral`/
`data`/`elapsed_ms`) so nothing downstream breaks; add a short human-readable
`text` block for clients that only render `content`. Consider
`ContentBlock::ResourceLink` for `block_id` so it becomes navigable rather
than a string the model must know to re-request. Self-contained, low risk.

### 3. `on_progress` is a deliberate no-op

`BrokerClientHandler::on_progress` is an explicit empty impl marked "Phase 2".
Long kaibo consultations are therefore 15 minutes of silence. Note the
`progressToken` must be sent by *us* in the request `_meta` for a server to
report against it. Pairs naturally with (4).

### 4. Tasks (SEP-2663) for outbound long calls

`tools/call` can return a task handle instead of blocking: statuses
`working`/`input_required`/terminal, `tasks/get` polling with a
server-suggested `poll_interval_ms`, `ttl_ms` expiry, cooperative
`tasks/cancel`, `notifications/tasks` pushes, and in-task input requests
answered via `tasks/update`. Client must declare `.enable_tasks()` first; no
compliant server offers it otherwise.

**Two boundaries to hold, decided 2026-08-11:**

- **MCP `TaskStatus` is NOT `BlockKind::Task`.** The enums rhyme
  (`Working/InputRequired/Completed/Failed/Cancelled` vs
  `Open/InProgress/Done/Cancelled`) and they are different nouns: ours is a
  durable to-do item tracked in a conversation, theirs is an in-flight RPC
  handle. Do not unify them.
- **MCP tasks do NOT replace `background_exec`.** Ours streams output into a
  CRDT block every player can see; an MCP task's result goes only to the one
  client polling it. In a many-hands model that is a downgrade. Tasks are for
  the *outbound* long-call problem (kaibo), not for kernel-owned background
  work.
- Amy's related question — should kaish `jobs` show MCP work? — resolves the
  same way: an MCP **task** is job-shaped (stable id, status, cancel, TTL) and
  belongs there; a raw in-flight **request** is not (no pid, no signals, wrong
  ownership direction, sub-second churn) and wants a read-only `kj mcp` view
  instead.
- Unknown, needs probing: whether kaibo declares the tasks extension.

### Also open, from the same survey

- `Timeout` no longer marks an instance Down (fixed `0676da42`), but there is
  still **no automatic reconnect** — `reconnect()` exists and is never called
  ("Phase 1 does not invoke this automatically"). A genuinely dead server stays
  Down until `kj mcp reload`. Worth a health-check/backoff pass.
- We call the raw `Peer::call_tool`/`read_resource`, not the MRTR-aware
  `RunningService` helpers. Blast radius is fixed (`f389e84f`), but we still
  cannot *drive* an MRTR round-trip — and doing so needs (1) first, since the
  helpers fulfil input requests through the `ClientHandler` that currently
  auto-declines.

## `kj backend` has no health check (re-filed 2026-08-11)

Salvaged from the deleted models.toml papercuts section — the only idea there
that outlived the file it was about. There is no way to ask whether a
configured backend's `base_url` actually answers; a wrong URL surfaces as a
failed turn much later. Wants something like `kj backend check <name>` (or a
`--check` on `kj backend list`) that probes each configured endpoint and
reports reachability + model list. Verified absent: no `doctor`/`check` verb in
`crates/kaijutsu-kernel/src/kj/backend.rs`.

---

## Background exec → kaish's job system (2026-08-07, Amy: "we should do the work and set the rule")

An audit of every spawn site found exactly one ad-hoc host exec left in
production: `spawn_background` (`crates/kaijutsu-kernel/src/background_exec.rs:552`,
NOT under `mcp/servers/`) runs an
agent-supplied string as `/bin/sh -c`, called from `shell.rs:200`
(`start_background`, the `shell` tool's `background: true`). MCP stdio
launches (`mcp/servers/external.rs`) are the sanctioned exception; the rule
itself now lives in `CLAUDE.md` ("Host exec has one owner").

The defect is not the bypass — it's that `shell.rs:206-283` hand-mirrors
three policies kaish derives structurally (read-only refusal ←
`ExternalExec::Deny`; exec-authority gate ← `allow_external_commands`;
hermetic cwd/env ← `apply_context_config`), plus an `is_dir()` check
(`:255`) needed only because kaish's VFS resolution isn't in play. One
canonical owner, one silent copy, no mechanism keeping them in sync.

**Preserve across the swap** (all in `background_exec.rs`): live output into
the CRDT block (not buffered to completion); `Running`→`Done`/`Error` tied to
exit; `kill_all_for_context` (`:486`); process-group kill (`:583`); the
PDEATHSIG orphan guard (`:586` — `kill_on_drop` only covers a clean unwind);
output cap with a loud marker. Characterization tests come first.

**Blocked on three kaish changes** (worktree PR in flight,
`~/src/wt/kaish-jobs-embedder`):
1. A `JobManager` injection point — `Kernel::new`/`with_backend` hardcode
   `JobManager::new()`, so the per-call `EmbeddedKaish` loses every job.
2. `execute_background` must *forward* output it already captures.
   `try_execute_external` drains stdout per 8 KiB via `drain_to_stream`
   (`scheduler/stream.rs:223`); `execute_background` writes only the
   aggregate at exit, so `/v/jobs/{id}/stdout` reads empty mid-run.
3. PDEATHSIG — kaish has none anywhere (`setpgid` + pidfd + `kill_on_drop`
   only), so migrating as-is would silently drop the guard that covers
   `kill -9` and the `kaijutsu-runner.sh` restart loop.

**Functional gate exists (2026-08-09):** `contrib/isotest` (docs/isotest.md)
runs the PDEATHSIG orphan guard, process-group kill, restart hygiene, and
client-disconnect contract against the real binary in a podman PID
namespace — mutation-verified RED without PDEATHSIG. The swap must keep this
suite green; "PDEATHSIG isn't testable" is no longer true.

**Multi-tenancy stays ours.** `Job.session_id` is per-`JobManager`
(construction-time, output-file naming) — not a tenant key — so a shared
manager doesn't partition by context. Kaijutsu keeps a `ContextId`→`JobId`
index rather than asking kaish to enforce ownership; consistent with the
shared-trust model and with how `BackgroundRegistry` already works.

## kaijutsu on brak: fleet coordination service (2026-08-09, via zorak session, Amy aware)

> **DEPRIORITIZED 2026-08-11 — Amy, leaning not ruling.** *"I'm torn about
> bringing kaijutsu kernel service over, brak is not a powerful dev machine.
> So kernel will be likely on zorak since it depends on Zorak's GPU anyways.
> At least for now. we'll push binaries around later."*
>
> The load-bearing reason is the **GPU dependency**, not brak's size: the
> kernel wants local models (embeddings / semantic index), so one-kernel-on-
> zorak follows from where the GPU is. Keep that framing — "brak is a small
> box" invites someone to reopen this the moment they see a bigger small box.
>
> **brak already participates, as a client.** It runs Claude Code in screen
> with `kaijutsu-mcp` against zorak's kernel — so fleet participation was
> never gated on a kernel deployment there. The entry below (and the
> launcher-unit gap under it) was written assuming otherwise.
>
> Everything below stays on record because the *shape* is still the blessed
> one (ONE kernel, many tailnet clients) and the deployment gaps are real
> whenever a kernel does land on a box that isn't zorak. Nothing here is
> active work right now. Also parked with it: **does a kernel that is purely
> a coordinator need provider API keys at all?** — moot while the kernel
> lives on zorak, live again the moment one doesn't.

Fleet direction from tonight's planning: brak (always-on N100 on UPS,
tailnet 100.113.35.53) should eventually run the kaijutsu kernel as the
fleet's coordination service — machine sessions on zorak/brak/moltar talk
via shared CRDT docs + invoke_peer instead of the current
sftp-handoff-mail protocol. brak is NOT a build box: zorak builds and
syncs binaries. What kaijutsu owes when this gets picked up: a server
build/config profile for a small x86_64 box (release build, modest
memory, no GPU/local-model assumptions), reachable over tailnet from
Claude Code sessions on all three machines. Jam doc:
~/src/zorak/docs/plans/cybernetic-infra.md (Q2).

**Correction (2026-08-11, read from code).** This entry used to claim
"isotest proved fresh-$HOME boot with generated host keys + add-key
provisioning and no host state" and named contrib/install-systemd.sh +
kaijutsu-server.service as "the unit story". Both readings were too
strong, and work was being scoped against them:

- **isotest does not prove zero-host-state boot.** `docs/isotest.md:3-8`
  scopes the harness to *process-lifecycle* guarantees (PDEATHSIG orphan
  guard, process-group kills, restart hygiene), and every assertion at
  `docs/isotest.md:61-83` is a lifecycle assertion. Fresh-$HOME is a
  **precondition demonstrated by construction, not an asserted
  invariant** — nothing fails if it regresses. It is already false twice,
  benignly: the embedded mcp.toml reaches for `bevy_brp_mcp` and a
  hardcoded kaibo path (`docs/isotest.md:95-96`), and the suite runs
  `--network=none`, loopback SSH only (`docs/isotest.md:19-20, 43-47`).
  **Tailnet reachability — the whole point of brak-as-coordinator — has
  never been exercised.** A green isotest says nothing about it.
- **Therefore the brak cold boot is not gated on isotest.** It is gated
  on a unit that does not exist (below).

Amendments (same evening, zorak session): fleet binary distribution goes
through **halfremembered-launcher** (zorak + github; Amy: "may need
sprucing") — superseding Gitea-Actions-registry and distcc/sccache ideas —
so kaijutsu-server binaries reach brak via launcher sync from zorak's
builds. And brak is dropping MinIO (license): assume NO object storage on
the fleet coordinator.

**CRDT-record note**: this plan is ONE kernel with many tailnet clients —
the one-body model stretched across machines, not multi-kernel
federation. It therefore *supports* the option-2 verdict in
docs/crdt-position-2026-08.md rather than triggering its federation
escape hatch; empirical question 2 should be read with this in evidence.

### The launcher-shaped unit does not exist (2026-08-11)

Neither shipped unit can run on brak, and the gap is structural rather
than a config tweak:

- `contrib/install-systemd.sh` runs `cargo build --release` and sets
  `WorkingDirectory=$REPO_DIR` — it needs a git checkout *and* a Rust
  toolchain on the target, contradicting this entry's own "brak is NOT a
  build box". Do not run it there.
- `contrib/kaijutsu-server.service` is a dev unit: hardcoded
  `target/debug/`, a `/bin/bash -c` that reads `~/.anthropic-key.txt` and
  `~/.deepseek-key`, and `OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317`
  which will not exist on brak. Cold-booting it tests a fiction.

Owed: a unit that runs a launcher-managed binary path and nothing else —
no repo, no toolchain, no checkout-dependent `WorkingDirectory`,
`EnvironmentFile=` instead of inline key reads (and a decision on whether
a fleet coordinator needs provider keys at all — **Amy's call**), OTLP
optional. **zorak lead owns the draft**, in zorak's `systemd/` where
units-before-install is the convention; it moves here on Amy's word. The
distribution half is ready: halfremembered-launcher has atomic install +
versioned deploys with rollback (branch `atomic-install`, unpushed
pending Amy).

Three things that will bite whoever does this:

- **`loginctl enable-linger` appears nowhere in this repo.** A `--user`
  unit without linger does not start at boot and dies on logout. On an
  always-on headless N100 that presents as "the binary is broken".
- **Provisioning is a mandatory pre-boot step.** The shipped binary has
  `allow_anonymous: false` and no registration RPC
  (`docs/isotest.md:52-54`) — without `add-key` first you get a running
  server nobody can reach.
- **glibc skew is asymmetric and trends the wrong way.** brak is Arch
  like zorak, so this is version skew, not distro mismatch — but glibc
  symbol versioning is one-directional: a binary built against a *newer*
  glibc fails on an *older* one, never the reverse. zorak is the daily
  driver and brak is an appliance nobody logs into, so zorak leading is
  the default trajectory, not the unlucky case. isotest carries an `ldd`
  preflight (`docs/isotest.md:34-37`) because this is real; the launcher
  sync path has none. Filed with zorak lead as a launcher feature
  request: run the preflight *on the target, pre-activation* (the seam
  atomic install creates), stamp build-time glibc into deploy metadata,
  and abort-and-report rather than auto-rollback — a silent revert to an
  older binary hides the actual problem.

## opencode support — shared lane with kaibo (2026-08-09, via kaibo session relay)

Amy (later this week): "easy opencode go support and some testing" — she's
picking up an account — and it "might be shared with kaijutsu, both tools
could use it well." Contract with the kaibo session: the opencode-as-MCP-host
measurements (instruction/description truncation limits, deferred-schema
behavior, resource ambience — the numbers kaibo already has for Claude
Code/Desktop) and the mcp-add config pattern get done ONCE and shared;
coordinate at pickup so nobody measures twice. Kaijutsu angle: opencode is
another vendor harness for the code-enabled-player pattern
(docs/python-player.md) — same per-vendor policy read applies before wiring
its subscription. Recorded in kaibo's signoff open threads as shared work.

## Pythonic player: kaijutsu-py wheel (2026-08-09, Amy: "shape B — the pythonic player")

New crate `crates/kaijutsu-py`: cdylib built by maturin, pyo3 isolated to this
one crate, wrapping `kaijutsu-client`'s ActorHandle into a `kaijutsu` Python
package — any Python process becomes a first-class player. Serves three
lanes: (1) vendor agent harnesses client-side under their own subscriptions
(Claude Agent SDK under Amy's own login first — per 2026-08-09 policy read,
personal SDK-under-own-login is the sanctioned subscription lane and OAuth
extraction is banned; GPT/Gemini harnesses later, each gets its own policy
read); (2) notebook/science/MIDI players; (3) an experiment space for python
sandboxes / uv venvs for agent-callable execution — containment via the
isotest podman harness, and it must compose with the kaish exec-ownership
rule, never bypass it. Design principle: players fat in capability, thin in
derived state (the kernel is the head). Second voices in flight: gemini-pro
deliberate (batch, durable handle `gemini/batches/pn8p3vcg5faecoeekb8a95s50cm9cb7mkrh8`)
+ deepseek consult; melt results into a design doc before building.

## kaish-help and kaish-kernel must be bumped together (2026-08-07, adoption)

Now that prompts are composed from `kaish-help`, the guidance kaijutsu ships
describes the shell it *runs* — so the two pins have to move as a pair:

- `kaish-kernel` (crates.io "0.13") is the shell that actually executes.
- `kaish-help` (git rev, pending release) is the prose describing it.

They agree today. They will not after kaish PR #300, which makes `,`
significant only inside `[]`/`{}` — the composed tool description still
carries the `comma-splits-word` rule (a `Concept::Foundations` fragment, and
Foundations is exactly what the tool description selects). Bump `kaish-help`
alone and models are told to quote commas the runtime now accepts; bump
`kaish-kernel` alone and they are not told about a rule that still bites.

Neither direction fails loudly — a prompt that mildly misdescribes the shell
produces worse agent behavior, not an error — which is what makes it worth
writing down. When the release lands, move both pins in one commit and drop
the git dep TODO in `Cargo.toml` at the same time.

## rc seed assets have no rebuild tracking (2026-08-07)

`assets/defaults/rc/` is embedded via `include_dir!`
(`kaijutsu-kernel/src/seed_scripts.rs:54`) and `kaijutsu-kernel` has no
`build.rs`, so nothing emits `cargo:rerun-if-changed` for the seed tree.
Editing a stance or lifecycle script may not trigger a rebuild, and a test
run can silently exercise a stale embedded copy. Bit us during stance tuning:
a mutation test looked green until `strings` on the test binary proved which
version was actually compiled in. Wants a small `build.rs` walking the asset
dir. Until then, verify with:

    strings -a target/debug/deps/<test-binary> | grep -o "<a distinctive string>"

## Ambient command center — trace packets, switchboard follow-ups (2026-08-10)

The arc: Amy runs the app all day on a side monitor; the room scene grows
ambient signal surfaces (switchboard shipped, seats in flight). Backlog:

- **Trace-packet / comet system (concepted, Amy liked it, not built).** Real
  events inject traveling crests on the engraved floor routes — the
  `ChordMaterial` trick (CPU writes ONE launch timestamp, shader derives
  crest position from `globals.time` forever): extend `TraceGlowMaterial`
  with a mode-2 pulse-slot array (4 slots/route, round-robin). Traffic map:
  MIDI-in → crimson inbound W; DJ render → crimson outbound E; VfsActivity →
  green N; PCM → cyan N; **TurnCompleted → gold comet** (bigger, tailed,
  1.2–1.8s hash-jittered transit) seat/S → well, landing as a
  `WellRingsMaterial` ripple at its arrival angle + throat bump;
  **TurnFailed → red comet that lodges** at the terminal pad until the
  context recovers. Token-bucket per route (~150ms min spacing, drop excess
  — midi.md doctrine: missed is missed, never replay); crest position from
  emission wallclock, back-dated, never chased. Needs a `RouteRegistry`
  (bearing/hue → material handles; producers call `inject(bearing, class)`).
- **Switchboard placement**: the south wall is BEHIND the default room
  camera — invisible in exactly the ambient framing Amy watches. Decide:
  move lamps to a visible diagonal (NE, where unbuilt Radiators sits),
  mirror a compact strip into the north-facing frame, or change the default
  camera. Amy's call.
- **Switchboard polish**: recency dynamic range too narrow (stale lamps
  still mid-khaki — widen so old falls near-dark; one constant, tune with
  eyes on the real monitor). Panel lost its nameplate with the furniture
  flip — if a label is wanted, engrave a title on the panel itself
  (tracker transport-glyph style). Ember is unit-tested but not yet
  live-verified (needs a real TurnFailed; stage one deliberately).
- **Switchboard slow leak** (kaibo/deepseek): a context deleted from the
  kernel while `sticky_error` is set leaks its `LampSignal` entry forever
  (never polled again → never cleared; `retain_relevant` keeps active
  signals). Rare; fix = drop signals for context ids absent from the poll.
- **Turn-comet / seat-flare enabler**: `TurnCompleted` carries
  `principal_id`, peers carry `nick` — no correlation exists. Wire a
  principal↔peer mapping (kernel knows both) so turn events can flare the
  right seat.
- **Runner cargo-watch is DEAF on moltar** (even freshly restarted, inotify
  limits fine): commits and touches under watched dirs trigger nothing;
  every rebuild needs `./contrib/kj restart`. Diagnose cargo-watch vs the
  kernel/watchexec version; consider watchexec-cli or a poll flag.
- **Ambience agent (Amy's idea)**: an agent context that "plays the room" —
  drifted-to like any context, modulating packet gain / route weather /
  lighting mood. Needs the packet system first; keep in mind as its API
  shapes up.

---

## Peer-registry doctrine — ACP/headless clients still need to attach; `PeerInfo` lacks `instance` on the wire (2026-08-10, peers-plumbing)

Every connected client is supposed to register in the kernel's peer registry
so the app can render "who's at the table" (`docs/instrument-design.md`,
"Many hands, one trust boundary"). `kaijutsu-mcp`'s `register_session` now
does this (`crates/kaijutsu-mcp/src/lib.rs`, the `peer_nick_for_label` +
attach block right after `finish_join` returns) — nick `mcp/<label>`,
instance a per-process UUID mirroring the app's `app_peer_instance()`,
invocations drained with a graceful "unsupported action" reply since no peer
actions are implemented on the MCP side yet. Use that as the pattern.

Still needed:
- **ACP sessions** (`kaijutsu-acp`) don't attach as peers at all.
- **The upcoming headless client** should attach on connect, same pattern.
- **The MCP peer nick goes stale on label stabilization.** An auto-registered
  session attaches under its placeholder label, then
  `stabilize_context_label` re-joins the same process under `{base}-{sid8}`
  via `finish_join` — but the attach lives in `register_session_impl`, not in
  `finish_join`, so the registry keeps `mcp/<placeholder>`. Attaching inside
  `finish_join` is not the fix as-is: (nick, instance) is the upsert key, so
  a second attach under a different nick registers a *second* peer for one
  process instead of replacing the first. Wants either a detach-then-attach
  on rename, or a registry keyed on instance alone.

Also: the wire `PeerInfo` struct (kaijutsu.capnp) carries only `nick` +
`attachedAt` — no `instance`/kind field. `listPeers` output can't distinguish
two windows/sessions sharing a nick (multi-window app instances, or two MCP
sessions attached under the same label from different processes) from each
other; a caller rendering presence sees one nick, not N. Candidate future
schema addition: add `instance` (and maybe a `kind` string) to `PeerInfo` so
`listPeers` round-trips what the registry already tracks server-side
(`crates/kaijutsu-kernel/src/peers.rs`'s `PeerInfo` has both).

---

## Seats-at-the-table follow-ups: nameplate LOD, turn-event flare (2026-08-10, seats)

`view::room::seats` gives every attached peer (`connection::peers::PeerRoster`)
a wisp orbiting the well — no nameplates in v1, they'd be illegible clutter
at room scale (mission call). Two follow-ups noted rather than built:

- **LOD-gated nameplate on well-zoom.** A label that fades in only once the
  camera is close enough to read it (the well-zoom LOD the mission brief
  anticipated) — needs a "which wisp is nearest/under cursor" pick, not
  built here.
- **Turn-event flare needs principal↔peer correlation.** `ServerEvent::TurnCompleted`
  carries `principal_id`, not a peer nick — there's no join today between
  "which context turn just finished" and "which attached peer should flare."
  Until that correlation exists, a seat can't visibly react to its own turn
  completing/failing the way `switchboard`'s lamps do for contexts. Enabler:
  thread the acting peer's nick (or instance) onto the turn-completion event,
  or expose a context→peer lookup the seats reconcile loop can join against.

---

## Conversation block-focus indicator is invisible (2026-08-04, live BRP debug)

The "j/k navigation stuck" mystery from the 08-03 signoff is SOLVED and it
was never navigation: an instrumented live session showed 8/8 `j`/`k`
presses moving `FocusTarget` correctly (log-verified, both directions)
while two screenshots at adjacent focus indexes differed by 4 pixels at
max channel delta 0.28/255. **Focus moves; nothing visible shows it.**
Mechanism, three contributing factors, all render-side:

- The only focus visual is a 1.15× brighten of the block's plain-text
  color (`view/render.rs:787` `highlight_focused_block`, multiply at
  `:813`); `FocusedBlockCell` has exactly one consumer.
- Markdown/rich blocks ignore that color entirely (per-span theme brushes,
  `block_render.rs:677`), and most conversation content is markdown — so
  for typical blocks the brighten changes zero pixels.
- The borders you CAN see are kind/status styling
  (`cell/block_border.rs:143` takes no focus input); the fork block's
  purple border reads as a focus ring and anchored the misdiagnosis. The
  first `j` "works" visually only because the cold-start `None → 0` focus
  jump scrolls the viewport to document top.

Also: nothing un-highlights (one-way write until `doc_version` advances),
and `highlight_focused_block:806` clones a whole-document snapshot
(`editor.blocks()`, 187 blocks) every frame a block is focused, against
the discipline stated at `render.rs:83` — should be
`editor.block_snapshot(&block_cell.block_id)`.

Fix sketch (from the debug session, not yet implemented): move focus
feedback to the border — pass focus into `determine_block_border_style`
and emit a distinct focus border; note its `layout_gen` early-return at
`block_border.rs:158` must gain a `focus.is_changed()` escape or the new
style never applies. Then delete `highlight_focused_block` or give it a
revert path. Regression tests: extract `next_focus_index` math
(`input/systems.rs:484-497`) for the ends; an app-level test asserting
focus-visual moves AND reverts between two blocks (fails on both counts
today). Also fold in the snapshot-clone fix above.

## Shell dock never draws a visual-selection highlight (noticed 2026-08-04, tier-2 app cleanup)

While unifying `build_overlay_glyphs`/`build_shell_dock_glyphs`
(`view/overlay.rs`, `view/shell_dock.rs`) into a shared
`sync_compose_text_glyphs`, found the two had already drifted: the overlay
palette tracks vim-mode `kind` and a selection anchor on its
`OverlayCursorGeometry` and draws a highlight rect for single-line visual
selection; the shell dock's `build_shell_dock_glyphs` never sets either
field, so `v`/visual-mode selection in the shell dock has no visual
feedback at all — only the cursor moves. Left as-is (behavior preservation,
not this cleanup's job to decide), but worth Amy confirming whether that's
deliberate (shell dock = one command line, selection matters less) or a
gap worth closing.

## Day-job coding readiness (2026-07-29, live-kernel probe + deepseek review)

Amy: *"I want to get kaijutsu's coding functionality up to a level I can use for
my day job."* Verified against a running kernel (probed via `kaijutsu-mcp
--connect`) plus source. The **tool surface is not the gap** — `builtin.file`'s
hashline anchors and CRDT-aware `grep` are ahead of Claude Code's equivalents.
The gap is **context economics**: nothing measures or bounds what a turn costs.

Complements the Gemini CLI comparison below (2026-06-23) — that pass found the
same web/background/ask_user holes from a different angle. Don't duplicate;
these are the ones that block *using* the thing.

**Tier 0 — small fixes.** *A1/A2 SHIPPED 2026-07-29 (`4ed99bd3`).*

- ~~**Redundant hardcoded tool timeout clamps the configurable one.**~~ **FIXED
  `4ed99bd3`.** `llm_stream.rs`'s `const TOOL_TIMEOUT_SECS: u64 = 120` wrapped a
  broker call that *already* enforced per-instance `policy.call_timeout`
  (`broker.rs:1372`), so the const won and no build over 2 min could run.
  Removed; `PolicyError::Timeout` now maps to the same `tool.timeout`
  ErrorPayload using the real `timeout_ms`. **`kj policy set` is now the sole,
  sufficient ceiling for a coder-context tool call** — verified: the LLM idle
  (30s) and request (300s) timeouts only bound the *token stream* (tool calls
  run strictly after it closes), kaish's own 1800s request timeout sits inside
  the broker's call, and `kaijutsu-mcp`'s 300s/600s shell cap
  (`kaijutsu-mcp/src/lib.rs:950`) governs the separate producer-seat path only.
- **CORRECTION to the original claim, recorded because it changes the
  priority.** This entry first read *"one `cargo build 2>&1` destroys the
  context"*. That is **wrong for the shell path**: kaish already truncates and
  spills. `OutputLimitConfig::agent()` is an **8 KiB cap, 1 KiB head, 512 B
  tail, `SpillMode::Disk`** (`kaish-kernel-0.13.0/src/output_limit.rs:27-33`),
  applied *inside* the broker's call — so the broker's `max_result_bytes` check
  essentially never fires for `shell`. The other builtins are self-bounded too
  (`read` 2000 lines / 2000 chars-per-line; `grep` 200 matches, skips >1 MB
  files; search tools take `max_matches`). The 64 MiB default was a weak
  backstop *behind* per-tool caps that were already doing the work — not the
  open floodgate this entry implied. It still wanted fixing (64 KiB now, and
  head+tail beats tail-drop since compiler errors live in the tail — both
  shipped in `4ed99bd3`), but rank it below token accounting, not beside it.
  The genuinely unbounded surface is `block_read` on a large block and external
  MCP results — and external MCP doesn't load at all today.
- **Token accounting — gauge SHIPPED, pre-flight budget still missing.**
  (Headline corrected 2026-08-03; it originally read "consumed by nothing",
  which `95e6664a` made stale.) The full gauge exists: per-turn usage
  SNAPSHOT persisted to `context_usage` (`llm_stream.rs:1347`, provider
  cache-token normalization + never-overwrite-with-absent guard),
  displayed by `kj context info` and `contextUsedPct` on the wire. What's
  still missing is anything that *acts* on it BEFORE a turn — see the
  hydration-budget bullet below.
  - Denominator SHIPPED 2026-07-29 (`36f57547`) as hand-maintained per-model
    `context_window` in models.toml, resolving to `Option<u64>` — never a
    fabricated default.
  - **Live Anthropic lookup SHIPPED (feat/ctx-window-live).** Config still
    wins as an override (`LlmRegistry::context_window_for`, sync,
    config-only); when it has no entry, `context_window_for_live` (async)
    falls through to `GET /v1/models/{id}` for Anthropic providers only,
    reading the wire's `max_input_tokens` (NOT `max_tokens`, the output cap —
    the field-name confusion this shipped a regression test against).
    Non-Anthropic providers are untouched — additive, not a replacement.
    Cache lives on `claude::Client` (one per registered provider = one per
    kernel process): `Ok(_)` results (including `Ok(None)` — "the API
    doesn't know this model either") cache for the process lifetime, `Err`
    results (network/auth failure) are NOT cached so a transient outage
    heals on the next call. The HTTP call sits behind a
    `ModelCapabilitySource` trait seam (`llm/claude/models_api.rs`) so
    `cargo test --workspace` never touches the network — a
    `FakeModelCapabilitySource` is injected everywhere a test needs a
    `Provider::Claude` with an unconfigured model.
  - **Correction to the "closes the honest gap" claim above:** as of
    2026-08-02, `claude-sonnet-4-20250514` — the model backing the shipped
    `balanced`/`default` model aliases, and models.toml's canonical example
    of a deliberately-unset window — has been **retired**, not merely
    deprecated (`shared/model-migration.md`'s deprecation table gives its
    retirement date as June 15, 2026, now past). `GET /v1/models/claude-sonnet-4-20250514`
    404s for real (confirmed live against the API, not simulated); the live
    lookup correctly resolves it to `None` — same honest "unknown" config
    already gave it — because there is nothing left to look up. The
    mechanism is proven to close *real* gaps (verified live against
    `claude-sonnet-5`, absent from models.toml since it postdates the file,
    resolves to `Some(1_000_000)`); it just can't resurrect a retired model.
    **The more urgent thing this surfaced — shipped aliases pointing at a
    dead model — is FIXED 2026-08-02** (`ceea246b`): `default` →
    `deepseek-v4-pro`, `balanced` → `deepseek-v4-flash`, per Amy ("lean on
    deepseek for affordable experimentation and we can call on
    anthropic/others when we need their power"). Every Anthropic id in
    `models.toml` is now UNDATED, which is the durable lesson: both dead ids
    we found were dated snapshots. Windows were re-read live rather than
    trusted. `claude-opus-4-20250514` is also a 404, and kaibo's
    `list_models` — the right tool for this question — confirms Haiku 4.5 is
    still the newest Haiku and turned up `claude-sonnet-5` /
    `claude-opus-4-6`, which we were missing.
- **Hydration size warning SHIPPED 2026-08-03 as warn-but-send; a hard cap
  is rejected by design.** Amy chose warn-and-send over refusal: a
  `BlockKind::Trace` block + `log::warn!` when the pre-flight estimate
  (`estimate_tokens`, bytes/4 + flat 1600/image) hits ≥90% of the resolved
  window (`warn_if_near_context_window`, `llm_stream.rs`); unknown window =
  no check (never fabricate a denominator); the provider's own rejection
  stays the hard backstop, and the kernel never trims or refuses a turn
  (auto-compaction stays deliberately removed). Known simplifications, fine
  until they annoy: warns every over-threshold turn (no latch), estimate
  runs once per turn (pre-agentic-loop, so mid-turn tool-result growth is
  seen next turn).

**Tier 1 — needed daily**

- ~~**Background process management.**~~ **SHIPPED 2026-07-30.** `shell` takes
  `background: true`, spawning `/bin/sh -c` directly on the host (bypassing the
  per-call kaish materialization, which can't host a registry that outlives the
  call) and streaming into a CRDT block; a kernel-owned `BackgroundRegistry`
  (`background_exec.rs`) plus a `builtin.background` sibling server
  (`list_background_processes` / `read_background_output` /
  `kill_background_process`). Orphan-proof via `PR_SET_PDEATHSIG`.
  **Known gap, not worth closing here**: a child wedged in D state never dies on
  SIGKILL, so `child.wait()` blocks and its entry stays `Running` forever — the
  watchdog catches a panicking supervisor, not a hung one.
  **App-visibility slice SHIPPED 2026-07-30**: the app had zero visibility into
  this (a 10-minute `cargo build` was invisible until it finished) — closed via
  `BackgroundRegistry::summary_by_context` (kernel-derived per-context
  aggregate: running count, oldest-running start time, most-recently-finished
  outcome), five new `ContextHandleInfo` fields (`kaijutsu.capnp` @23-@27,
  `background*`), and a `background_jobs` dock badge
  (`kaijutsu-app/src/ui/dock.rs`) on the existing ~5s `DriftState` poll. Display
  only — killing a job stays a model-driven MCP tool call, no dock button.
  Left for later: no full SSH-level e2e test for the new wire fields (matches
  how `contextUsedPct` itself was tested — unit + wire-sentinel + capnp
  round-trip only, no client RPC verb exists to start a background job to
  exercise end-to-end); a hard-failure indicator is momentary (~5s poll, then
  the badge blends into the next state) with no "sticky until acknowledged"
  treatment. **Verified real (2026-07-30, feat/dock-errors merge): two
  freshness models sit pixels apart in the south dock, for the SAME event.**
  A background job's block (`background_exec.rs` inserts it `Status::Running`
  and later `blocks.set_status(..., Done/Error)` at the same call site that
  updates the registry, ~`background_exec.rs:691-699`) is one of the blocks
  `count_block_activity` counts — so `block_activity` reflects a background
  job's start/finish the instant the CRDT op broadcasts, sub-second, no
  polling. `background_jobs` reads the *same* start/finish only through
  `BackgroundRegistry::summary_by_context` via the 5s-throttled `DriftState`
  poll (`ui/drift.rs:16`, `poll_drift_state`). Both badges can visibly
  disagree about whether "something is running" for up to 5s at both the
  start and the end of the identical job. Not a bug in either badge — each is
  internally correct for its own data path — but a real, correlatable skew a
  user watching a long build could notice. Closing it means giving background
  job transitions a push path (a `ServerEvent` variant, mirroring
  `BlockStatusChanged`) rather than polling; not done here since it's new
  scope, not a merge fix.
  **CORRECTED 2026-08-01 by the or-kimi review** (run after the branch merged;
  findings verified against the code before recording): the 5s skew *is* only
  cosmetic and self-healing, exactly because the kernel writes the block status
  and the registry entry in the same `match final_status` arm — the next poll
  always sees a matching terminal state. But the two badges measure genuinely
  different things (visible CRDT block status vs. OS process registry lifetime),
  and there are three ways they diverge **permanently**, which the entry above
  understated:
  1. **Kernel restart.** `BackgroundRegistry` is in-memory, so a restarted
     kernel reports zero background processes while the persisted CRDT document
     may still hold `Running` blocks — `background_jobs` goes idle,
     `block_activity` keeps counting. Non-transient.
  2. **`set_status` fails while the registry update succeeds** (same call site;
     e.g. the document was removed). Registry says exited, block stays
     `Running`.
  3. **Block excluded or deleted while the process runs.** `block_activity`
     skips excluded blocks and never sees deleted ones; the registry keeps
     tracking the live process.
  A push path fixes the 5s skew but NOT these — they need the two views
  reconciled at their source, or the badges labelled as measuring different
  things.
- **`background_jobs` conflates "no data yet" with "nothing running"**
  (2026-08-01, or-kimi review, verified). `format_background_activity`
  (`ui/dock.rs:1870-1920`) returns an empty string both when the first poll
  hasn't landed and when the context genuinely has nothing running and nothing
  finished — the badge just hides. The "never fabricate a zero" rule is
  honored (it never shows a fake `0 running`), but unknown and known-idle are
  still the same pixels. `DriftState.loaded` already exists and is maintained
  (`ui/drift.rs:154,160`) and is **never read anywhere in `ui/`** — gating the
  empty state on it and rendering a placeholder until the first successful poll
  is the fix. Same class as the `contextUsedPct` `-1.0` sentinel work, one
  surface further out.
- **Dock papercuts from the dock-errors/bg-jobs merge** (2026-08-01, or-kimi
  review, all three verified against the code):
  - **`agent_activity` is a dead badge.** Declared (`ui/dock.rs:108`),
    defaulted (`:160`), and drawn (`:635-642`) — and **no system anywhere
    writes it**. Harmless (the draw is guarded on non-empty) but it is a
    leftover from an earlier branch; delete it.
  - **Draw-order overlap hazard in `render_south_dock`.** The right group
    (`context_usage`, `hints`) is positioned and drawn FIRST, then the middle
    area (`block_activity`, `background_jobs`, context badges) is drawn after
    it. Right-alignment means no overlap at normal widths, but a middle area
    wide enough to reach `usage_x` will overdraw the right group rather than
    being clipped. Compute the middle area first, or clamp it so it cannot
    reach. The layout comment at `:537` is also stale — it names `activity`
    and `block_activity` and omits `background_jobs`.
  - **Badge colors go stale across a theme change.** `update_background_jobs`
    and `update_block_activity` run only on `DriftState`/`DocumentCache`
    change, but they are what compute the badge color; `render_south_dock`
    rebuilds on theme change and uses the color already stored in `DockState`.
    Switch themes without touching the data and the badge keeps its old color
    until the next poll.
- **`poll_drift_state`'s "prevent stacking" comment overpromises**
  (2026-08-01, or-kimi review, verified at `ui/drift.rs:96-112`). `last_poll`
  is set before the async task spawns, which debounces the fast path but does
  NOT prevent concurrent polls: a call slower than the 5s interval lets another
  spawn, and with `RPC_CALL_TIMEOUT` at 30s that allows up to ~6 overlapping
  `list_contexts` tasks. Nothing leaks (each completes or times out) and the
  no-actor early-return means nothing spawns after disconnect — so this is a
  stampede risk against a struggling kernel, not a correctness bug. Either hold
  an `AbortHandle` and abort the previous poll, or set `last_poll` on
  completion; at minimum fix the comment, which currently claims a guarantee
  the code does not provide.
- **External MCP servers don't load at all** — see the dedicated *MCP subsystem*
  section immediately below. This also closes the "BYO a scraper MCP" escape
  hatch for the missing web tools.

## TurnEvents + register upsert — deepseek review findings (2026-08-04/05, post-merge) — P2 tier SHIPPED

DeepSeek V4 pass (dpal, whole files, no diff) over the merged foundations at
`29529ded`. Eight keepers explicitly endorsed (single-terminal-publish,
origin-on-the-event, beat's three Act guards, resolveContextLabel honesty,
loud attach warning, subject⊆TOPICS gates, unknown-enumerant hard error,
hasOutputBlock zeroing). All six P2 findings landed 2026-08-05:

1. `⛔ Interrupted` marker → `(Role::System, BlockKind::Text)` + ephemeral,
   no longer folds into assistant_text on the model's next turn
   (`llm_stream.rs`). Checked the review's `BlockKind::Notification`
   alternative against `hydrate.rs` before picking — Notification is NOT
   hydration-skipped (it formats into a *user* message,
   `format_notification_for_llm`), which would have been worse.
2. Post-cancel drain's idle timeout no longer constructs `StreamEvent::Error`
   on a hung provider — breaks the loop with `stream_cancelled` already set,
   so the outcome stays `Cancelled`, not `Failed` (`llm_stream.rs`).
3. `refusal`/`stop_sequence` now log distinctly (`warn`/`info`) before
   falling through to `EndTurn` (`llm_stream.rs`). Still open: a dedicated
   wire `Refusal` stop reason — a capnp change, and capnp is owned by a
   parallel lane right now.
4. beat.rs's OODA-Act gate is now `BeatScheduler::turn_should_crystallize`,
   with the soft-cancel-crystallizes / hard-cancel-skips decision stated
   explicitly and pinned by tests, replacing a comment that explained the
   hard-cancel exclusion but left the soft-cancel inclusion unstated.
5. `register_session`'s suffix TOCTOU (concurrent registers racing one
   concluded label) now retries on a real label-conflict, bounded at 5
   attempts (`kaijutsu-mcp/src/lib.rs`).
6. `flows.rs`'s `TurnFlow` doc no longer claims the block log covers a missed
   push — true for the text a turn wrote, false for `TurnStopReason` (no
   block-log shadow; `EndTurn`/`MaxTokens`/soft-cancel all leave an identical
   `Done` block). Points at the already-tracked bus catch-up story below
   ("TurnFlow bus lossy + in-memory") instead.

**Two review claims turned out wrong on inspection** (worth recording — this
repo has now caught this reviewer wrong more than once): the
`BlockKind::Notification` hydration-skip claim in (1) above, and the
"wire e2e for onTurnFailed" test-gap — `subscriptions.rs`'s
`turn_events_tests::failed_round_trips` already exercised `on_turn_failed`
through the real generated capnp client/server pair, and predates the
review (landed in `0564b334`, an ancestor of the reviewed `29529ded`). The
remaining test-gap item (concurrent register race) got the review's own
fallback: `kaijutsu-mcp`'s retry classification is unit-tested directly
rather than raced end-to-end, since this crate's RPC path needs a live SSH
connection to a `kaijutsu-server` and the project steers `--lib` runs away
from that harness.

P3 tier, recorded not urgent: subscribe-happens-in-spawned-task window
(`rpc.rs:3522-3633` — subscribe synchronously or document ordering);
stream-start retry backoff ignores a pending hard cancel (`llm_stream.rs:
1300-1320`); no turnId/endedAt on onTurnCompleted (correlation by ordering
only — matters for a stateless ACP frontend, revisit with the adapter);
suffix-orphan residue on crash between create and join;
`publish_with_sender` drops silently where `publish` warns (`flows.rs:
664-680`); archived-context re-listing via joinContext heal is documented
but surprising (heal re-registers without clearing `archived_at`).

## Coder stance tuning — proportionality + kaish primer (Amy, 2026-08-05, first toad day)

Amy, watching a fresh ACP coder context: *"wow that's a lot of work for a
simple question, we might have work to do on the coder prompt soon."* The
exhibit: a simple repo question triggered a 63k-token, 160-block recon tour
(12+ agentic iterations), and the model repeatedly fought kaish syntax it
should have been told about (piping after `done`, `[` banned, quoting
rules — several [exit 2] tool errors it then reasoned around, stretching
the loop). Threads to pull, together or separately:

- **Proportionality**: the stance should ask for answer-first behavior —
  explore only as much as the question warrants; a fresh context need not
  map the whole world before speaking. Consider a light tool budget nudge.
- **kaish quirks primer in the stance** (or a `/etc/rc` .md slot): the
  live models rediscover the same parse rules every session. The stance
  is CRDT-seeded June-era content; kaish is at 0.13 — audit for staleness
  while in there (`kj rc` surface, embedded defaults are only the seed).
- **Per-surface stance**: ACP/toad sessions may want a snappier stance
  than the desk coder seat — ties into the client-identity-preset seed
  (ACP entry) and the pending ACP-cast decision (flash + thinking dialed
  down vs house pro).

## At-rest schema-evolution follow-ups (2026-08-05, the task_status boot flood)

The `39326e7c` postmortem seeds, recorded not yet built. Contributing
factors: a new field on an at-rest struct (`BlockHeader`) without
`#[serde(default)]`; nothing in tests exercised decode-old-bytes; the
kernel had not been restarted across the merge so the breakage sat latent
until the next bounce; rc-read failure degraded silently into
deny-by-default.

- **Decode-from-old-bytes guard, systematically.** The new
  `cbor_without_fields` pin covers the two fields that broke. The general
  guard is a corpus test: serialize every at-rest struct (SyncPayload,
  snapshots, oplog entries) at each historical shape — or strip
  fields-newer-than-N — and assert decode. Cheap version: a checked-in
  CBOR fixture of a real pre-task oplog entry, decoded in CI forever.
- **Oplog decode failure at boot is quiet relative to its blast radius.**
  Per-doc ERROR + skip is the right durability call (nothing truncated),
  but ~40 docs skipping should surface as ONE loud aggregate (count +
  first error) at boot end, and `kj status`-visible state — same
  loud-not-silent treatment external MCP failures got.
- **Broken-window contexts (2026-08-05 14:27–15:05) are unbound**: at
  least `2e1334a4` (this CC session's kaijutsu-mcp context) and
  `1c39e6ab` (`acp-kaijutsu-1785954617`, the first toad attempt). They
  need `kj context remove` from a bound context (app), then /mcp
  reconnect re-creates fresh + bound. Older contexts' bindings persisted
  in the DB and are believed fine — spot-check from the app.

## Household-agent arc — task blocks + harness steals (seeded 2026-08-04, gap-analysis session)

Amy is pointing kaijutsu at always-on household duty (daily task grooming,
proactive check-ins, chat access). We read the two flagship harnesses —
clones at `~/src/research/hermes-agent` + `~/src/research/QwenPaw` — and wrote
the comparison to `~/src/meadow-lab/docs/kaijutsu-gap-analysis.md`. kj already
wins model switching, semantic memory, OTel, and CRDT-native state; the gaps:

- **Task BlockKind + tool — SHIPPED 2026-08-04** (Amy: *"Task BlockKind and
  tool is a great idea"*). `BlockKind::Task` + a dedicated CRDT-synced
  `task_status`/`task_status_at` field (mirrors `content_type`'s exact
  mechanism — its own `TaskStatus` enum, not a reuse of the tool-execution-
  shaped `Status`) get multi-frontend task sync from the CRDT for free.
  Closes the "no task/plan state — compare TodoWrite" gap noted in the
  day-job entry above. `builtin.tasks` (`mcp/servers/tasks.rs`) exposes
  create/update/complete/cancel/list (open/done buckets); subtasks reuse
  the ordinary `parent_id` DAG edge. Hydration mirrors `BlockKind::Notification`
  (D-34): a task's creation/current-state is appended once per
  `ConversationMailbox`'s `seen`-keyed translate-once rule, so a later status
  edit never rewrites an already-cached message. Design note: `docs/tasks.md`.
  **Deferred** (kept narrow on purpose): app/kj-CLI rendering (a placeholder
  `[status] content` line covers `kaijutsu-app` for now), a `task_reparent`
  verb (no cheap "move to new parent" primitive exists yet — `move_block`
  only reorders siblings), and — the one real design gap — a companion
  `Notification` block auto-emitted when a task changes from OUTSIDE the
  model's own tool call (another principal grooming via the app), so an
  out-of-band change actually reaches a live conversation instead of only
  showing up at the next boundary re-hydrate.
- **EvictionIndex as an oplog view** (QwenPaw `scroll/eviction_index.py`, the
  best idea in either codebase): compressed-out history collapses into a
  tiered in-context "odometer" (capped blocks per tier, older tiers carry
  upward) so the model *knows what it forgot*, paired with one bounded
  read-only recall tool. Amy's shape for kj: *"should be simple in kaijutsu,
  we could have an RC script inject the generation counter, kaijutsu keeps
  the graph intact if we use fork to manage the contexts as they roll"* —
  rolling fork chain as the generations, RC-injected counter, index as a view
  over `context_edges` + oplog. No second store.
- **One channel, exactly one** — a new `invoke_peer` peer kind for an
  external chat channel (today only `app`/`mcp` exist). Field lesson: QwenPaw
  ships 17 channels, Hermes is mid-migration between two competing adapter
  systems; protocol quirks (Telegram UTF-16 chunking, Signal rate limiting)
  dominate the cost. Hermes' 4-method adapter ABC
  (connect/disconnect/send/get_chat_info) is the right size. Pick the
  channel the household actually uses; stop there.
- **Graduated trust, when access grows**: QwenPaw `governance/policy.py` is
  the reference — two-tier rules (immutable builtin + approval-generated),
  verdicts ALLOW/DENY/ASK/SANDBOX, ASK→approve→generalize to fight allowlist
  fatigue. Hermes is the cautionary tale (honest SECURITY.md: "the OS is the
  only boundary"; shipped the denylist default anyway). kj's shared-trust
  stance is right for Amy-only operation; channels + household input change
  the threat model — decide the posture explicitly at that point.
- Always-on hardening already tracked elsewhere in this file (MCP audit,
  `register_session` reconnect, hook self-lockout, WorkspaceGuard fail-open)
  graduates from papercut to blocker once the kernel runs unattended.
- Scheduling steals folded into **"Grooming tracks"** below — that entry is this arc's cron half.
- **ACP adapter (`kaijutsu-acp`) — the mobile shortcut** (researched
  2026-08-04). ACP v1 went stable 2026-06-24 (schema 1.20.0), governance
  moved off Zed to a neutral `agentclientprotocol` org, LSP-style. Mobile
  clients already exist (Happy iOS/Android/Web, Agmente, Ferngeist, Mobvibe)
  plus messaging bridges — an ACP server adapter shaped exactly like
  `kaijutsu-mcp --connect` (thin bridge: ACP session ↔ kj context, JSON-RPC
  stdio outside, Cap'n Proto inside) could put kj on Amy's phone before any
  custom app exists. Rust SDK is real: `agent-client-protocol` 2.0.0 on
  crates.io (SemVer, NOT protocol v2 — v2 is behind `unstable_protocol_v2`).
  **Build against v1; ACP2 is draft** (announced 2026-07-20, alpha schemas,
  explicit "gate behind feature flags", no GA date). v2 heads-up that suits
  us: `fs/*`+`terminal/*` methods are removed in favor of client-provided
  MCP servers — kj's MCP-first tool story is already on the right side of
  that migration. Remote transport (HTTP/WS) is still an Active RFD; our SSH
  `--connect` pattern sidesteps the wait.
  **Prototype landed 2026-08-05** on branch `acp-adapter` —
  `crates/kaijutsu-acp`, ACP v1 over stdio, `--connect` inward, ring 0 served
  as the session picker. Living record + the full mapping table + the manual
  smoke test: `docs/acp.md`. What it left open is below.

## ACP adapter follow-ups (2026-08-05, from building `kaijutsu-acp`)

Ordered roughly by how much they hurt. Full context in `docs/acp.md`, "The
adapter, as built".

- **`session/request_permission` is stubbed to auto-allow** —
  `kaijutsu-acp/src/permission.rs`, marked in capitals. Blocked on
  `HookAction::Ask` + a `PermissionEvents` server→client callback (acp.md
  gap #2). The bridge-side half — option shaping, outcome interpretation,
  deny-on-anything-unrecognised — is written and unit-tested; only the
  kernel→bridge transport is missing. **Do not point an untrusted client at
  the bridge until this lands.**
- ~~**No catch-up after a resync.**~~ **SHIPPED 2026-08-05** after it ate a
  live answer on toad flight two (FlowBus lag mid-turn; the client rendered
  the tool call, then silence over a finished report). `resync` now keeps
  the mapper's high-water marks and re-observes the rebuilt doc — the sweep
  emits exactly the gap (unseen tails + unannounced tool patches), never a
  duplicate. The *TurnFlow* half (dropped completion events) got adapter-side
  lag recovery the same day (idle-poll → best-effort `end_turn`).
  **The kernel-side cause is now fixed** (below), so **the adapter's defensive
  sweeps — quiet-poll turn wait, trailing-edge pump resync — are dormant
  defence-in-depth and are candidates for removal** once a few real flights
  confirm the kernel never drops on them again. Leave them in place until
  then; delete them together, and only after checking the ACP logs show the
  sweeps firing zero times.
- **`onTurnCompleted` carries no turn id.** The adapter's prompt wait matches
  on `context_id` + `TurnOrigin::Interactive`; two interactive turns racing in
  one context would cross wires. This is the P3 "no turnId/endedAt … revisit
  with the adapter" item above, now with a caller asking for it.
- ~~**`BlockKind::Task` has no ACP shape.**~~ **SHIPPED 2026-08-05** on
  branch `acp-plan`. `BlockKind::Task` blocks rebuild into ACP v1's `plan`
  session update, whole-context, one non-cancelled task per `PlanEntry`.
  `UpdateMapper::note_task`/`build_plan` (`kaijutsu-acp/src/update.rs`) is
  the one rebuild-and-emit path, threaded through the live pump, `session/
  load` replay (exactly one plan at the end), `session/new` bootstrap
  (silent baseline), and the resync sweep. Decisions: cancelled tasks are
  omitted (not mapped to any `PlanEntryStatus` — a plan is "what the agent
  intends to do," and cancelling isn't intent); subtasks (`parent_id` DAG)
  flatten via pre-order DFS with a `"↳ "` nesting prefix on `content`;
  priority defaults to `Medium` (no kernel-side priority field exists, none
  invented). Full writeup: `docs/acp.md` "Task → plan". Found while
  building: `session::run_pump`'s `BlockDeleted` arm calls `mapper.forget`
  and `continue`s WITHOUT ever calling `doc.apply_event(&event)` — the live
  `SyncedDocument` mirror never drops a deleted block, only a resync
  rebuilds it away. Pre-existing, affects every block kind (not
  Task-specific), not fixed here — noted as a follow-up below.
- **`session::run_pump`'s `BlockDeleted` arm never updates the `SyncedDocument`
  mirror.** Found 2026-08-05 while wiring Task → `plan`. The early-return
  branch (`kaijutsu-acp/src/session.rs`, the `if let ServerEvent::BlockDeleted
  ... continue` a few lines into the event-loop arm) calls `mapper.forget`
  and `continue`s before `doc.apply_event(&event)` runs, even though
  `SyncedDocument::apply_event_inner` has a real `BlockDeleted` handler
  (`synced_document.rs` — `sync.apply_delete`). A block deleted mid-session
  lingers in the live mirror (and so in `doc.blocks()`, and so in a rebuilt
  Task plan) until the next resync/reconnect throws the mirror away and
  rebuilds it fresh. Affects every block kind's live rendering, not just
  Task's plan — worth a one-line fix (call `doc.apply_event` for
  `BlockDeleted` too, same as every other per-block event) plus a
  regression test once someone's in that file for another reason.
- **Client-identity presets on connect** (Amy, 2026-08-05 evening, first
  toad day): ACP `initialize` carries `clientInfo` (Implementation
  name+version) and capabilities — enough to recognize *which* frontend
  connected. Wire that into the existing per-client config machinery: derive
  a ClientId from the client identity, let the `/etc/client` cascade
  (docs/config-crdt-ownership.md; metronome was the first consumer) and/or a
  preset/cast mapping key off it — "toad connections get preset X / cast Y,
  Happy gets Z." Today every ACP context gets the row-stamped default
  (ds-v4-flash) regardless of who connected; this is also where the pending
  ACP-cast decision could land generally instead of as a bridge hardcode.
- **Stable v1 methods left unimplemented**: `session/delete` (→
  `conclude`/`archive`), `session/set_mode` (→ `context_type` / cast roles),
  `session/set_config_option`. None are advertised in capabilities, so no
  client will call them.
- **Kernel-wide block subscription.** The bridge uses
  `scope_blocks_to_context: false` so several ACP sessions can stream at once,
  and filters per pump. That is the firehose kaijutsu-mcp deliberately scopes
  away from (the 2026-06-17 executor-starvation stall). If it bites, the fix
  is one actor per session, not a narrower filter. Less likely to bite now:
  the 2026-08-05 rework coalesces the text-op firehose at the forwarder, so
  the bridge sees roughly an order of magnitude fewer callbacks under
  streaming — and if it *does* fall behind, it is disconnected with a reason
  instead of quietly losing events.
- **Client-declared `mcpServers` are ignored** (warned once per
  `session/new`). Needs the unplumbed `external.rs` caller — acp.md gap #4.

## FlowBus backpressure — what the 2026-08-05 rework left open

The rework itself shipped (per-subscription bounded queues, lossless-or-
terminated, forwarder-side text-op coalescing, `subSeq` + the lag kick on the
wire). What it deliberately did NOT do:

- **The ACP adapter's defensive sweeps are still in.** Quiet-poll turn wait
  and trailing-edge pump resync (commits b8b9fe22, 35c4b5b9, 3960fad3,
  e31d6ddd) are now dormant defence-in-depth. Remove them together, after a
  few real flights show them firing zero times — not before.
- **No catch-up for a subscriber that wasn't there.** Losslessness is a
  promise to *live* subscribers only. See the TurnFlow catch-up item below.
- **Queue depth is one number for every subscription** (8192, via
  `KAIJUTSU_FLOW_QUEUE_DEPTH`). A GUI client and a headless MCP session get
  the same allowance. Per-class or per-principal depths are easy to add if a
  real workload ever wants them; nothing does yet.
- **The timing lane is untunable from config.** `block.render_cue` /
  `block.beat_sync` ride a fixed 64-deep drop-oldest ring. Deliberate — the
  doctrine is that a stale beat is worse than a missed one — but if a sink
  ever wants a different depth it needs a knob.
- **Only `slowSubscriber` is ever sent.** The wire enum also has
  `serverShutdown` and `superseded`; nothing emits them yet. A clean shutdown
  still looks to a client like an ordinary disconnect.

## LFM2.5 encoder family — routing, boundary guards, embedding swap (seeded 2026-08-03, Amy: "tempted to go deep on this model family for a while")

LiquidAI's LFM2.5 encoder branch is a small-model toolbox aimed at exactly
our seams. Surveyed 2026-08-03 (HF API, live):

- **`Encoder-350M-Prompt-Router`** — prompt→tier classifier. Our fit: the
  *dynamic* half of the routing doctrine — per-turn "flash or pro, effort
  high or none" feeding cast-lane choice, decided locally for free. Eval
  first: run a pile of real prompts from the block log through it and score
  its lane picks against Amy's.
- **`Encoder-350M-PII-Detector` / `-Policy-Linter`** — token-level boundary
  screening. Amy's framing: guards are for **mistake-protection and foreign
  content crossing the membrane** (new OSS repos, prose/code from elsewhere)
  — NOT inter-player policing, which stays off-doctrine. Strongest first
  home is **kaibo** (its whole job is reading untrusted repos): screen
  explorer file reads for injection, screen batch payloads for PII.
  Amy: *"an embedded LFM in Kaibo would kick so much butt for the safety
  elements."* (Kaibo-side work; tracked here as the cross-project seed.)
- **`Embedding-350M`** (1024-dim, safetensors + official GGUF) +
  **`ColBERT-350M`** — bge-small successor candidates. Embedder swap =
  full semantic-index rebuild (dims change; by design). ColBERT needs
  per-token vector storage — bigger lift; `bge-reranker-v2-m3` remains the
  cheap second-stage quality win meanwhile.
- **`Encoder-230M/350M` base** (fill-mask) — fine-tune substrate for our
  own future boundary classifiers.

**Runtimes (not stuck on ONNX — Amy), with a per-project split (Amy
2026-08-03): `llama-cpp-2` is fine in KAIJUTSU but NOT in kaibo — kaibo
stays pure-Rust/light-build; "candle might be cool there though." Kaibo
can wait.**

1. **Small encoders (router/guards/embedding): candle in-process, BOTH
   projects** (Amy 2026-08-03: "maybe candle here too"). Write the `lfm2`
   bidirectional-encoder + classifier-head implementation ONCE as its own
   small crate; kaijutsu consumes first, kaibo later. Keeps workspace
   builds pure-Rust (no cmake/C++ bolt-on next to Bevy), Mac-clean, and
   matches the rten in-process precedent. Caveat named: candle's AMD/
   Vulkan GPU story is weak and zorak is Strix Halo — but the encoder
   lane runs fine on CPU (bge-small already proves the shape; a 350M
   classify is tens of ms).

   **STARTED 2026-08-03: `~/src/candle-lfm2-encoder`** (own git repo, day-0
   commit `1f871e6`) — milestone 1 (config, fixture-verified, 7 tests) done;
   candle reference clone at `~/src/research/candle` (dual MIT/Apache, no
   CONTRIBUTING.md, no AI policy — upstreaming deferred until Amy reads at
   PR time). Fixture discoveries recorded in that repo's CLAUDE.md, headline:
   the PII detector's taxonomy includes credential.api_key/jwt/private_key —
   it's a SECRETS detector too.
   Searched first — the crate did NOT exist anywhere; scoping notes:
   Upstream candle-transformers already ships `lfm2.rs` +
   `quantized_lfm2.rs` — the CAUSAL branch, i.e. the hard hybrid blocks
   (gated short conv + GQA) are done. Nobody has the encoder branch:
   crates.io LFM2 hits are internals of other projects (candle-miotts TTS,
   bebelm 8B CPU, two VL runners), none reusable, none bidirectional.
   Project shape: adapt upstream blocks → drop causal mask
   (`Lfm2BidirectionalModel`) → map 2.5-encoder checkpoint weights →
   three heads in increments: sequence-classification (Router),
   token-classification (PII/Policy-Linter), pooled embedding
   (Embedding-350M; ColBERT later). Upstream-PR-shaped if it comes out
   clean — candle takes model contributions. Amy reads AI policies before
   we interact with any outside repo, per standing practice.
2. **Generation models: llama.cpp servers stay EXTERNAL** — kaijutsu
   already speaks to them as openai-kind backends; Vulkan works there.
   `llama-cpp-2` in-process is the fallback only if candle CPU perf
   disappoints or the arch port stalls.
3. **rten/ONNX** — only for index-pipeline symmetry; `lfm2` hybrid
   conv+attention arch is a real conversion risk; verify via embed_check.
4. LEAP (Liquid's edge SDK) — phones/edge, not our shape.

**Side quest (Amy 2026-08-03): fine-tuning LFM encoders for other things,
"like some music things."** The 230M/350M bases LoRA cheaply (moltar's
GPU ample; LiquidAI ships TRL-compatible fine-tune recipes). Music angle
stays symbolic per doctrine — the score is text (ABC, patterns), so
encoder fine-tunes can tag phrase style, classify patterns, or judge
groove-fit without audio ever riding the wire. Also the substrate for
our own boundary classifiers.

Deep-dive order when Amy picks this up: lfm2-encoder-in-candle crate +
Router eval offline (a scratch llama.cpp server is fine for the eval
itself) → Embedding-350M swap eval against bge-small on the real index
corpus → kaibo guard embed dropping in the shared crate → fine-tune
side quest.

## Cast follow-ups (seeded 2026-08-03, casts shipped same day — devlog "Contexts join a band")

Deferred from the renovation, in rough priority order:

- **Anthropic `output_config.effort` wire shape unverified live** — the house
  probe hit a billing wall (credit balance) before shape validation. Unit
  tests assert the documented shape; run one `kj drive` on a house/coder
  context after credits top up and confirm no 400.
- **capability-loadout consumer** for `cast_slots.loadout` (stored stub; the
  "cast can have domain capabilities" half of Amy's design).
- **`kj fork --cast`** — explicit fork-time cast override (inheritance +
  `--preset` cover today's need).
- **Responses wire for OpenAI** — only needed for streamed reasoning
  summaries; the chat wire with `max_completion_tokens` + `reasoning_effort`
  is live and sufficient.
- **Per-cast rc/stance placement** — does a cast ever carry stance text, or
  is that forever context_type's job?
- **App UI for casts** — kj-only today; the app compiles against the new wire
  fields but renders nothing cast-shaped yet.
- Deepseek-review P1, reviewed and ACCEPTED as policy: the rollover's
  fallback arm tosses provider=NULL+model=set rows to deepseek-v4-flash —
  that was Amy's instruction, and the live migration touched 0 rows anyway.
  Not a bug; recorded so nobody re-litigates it.
- Gemini review, reviewed and ACCEPTED as policy on three of its four
  findings (the fourth — a write-time inverted-budget WARN on `kj cast slot
  set` — shipped, see devlog "Contexts join a band"): DeepSeek
  `thinking:{"type":"disabled"}` claimed Anthropic-only → live probe
  2026-08-03 shows DeepSeek accepts it (kaibo had measured the same);
  rollover + preset narrowing are Amy's explicit policy; the "keys now in
  SQL" leak concern is structurally impossible (no key column exists).

## kaijutsu-mcp: workspace autoshare (seeded 2026-08-01, MCP-config session)

`--share` is app-only today (`kaijutsu-app --share`, `share_dial.rs` /
`share_server.rs` in kaijutsu-client) — kaijutsu-mcp has no share flag at
all, discovered wiring the Claude Code MCP config. Two pieces:

- **Port the share plumbing into kaijutsu-mcp** so an MCP client can offer
  reverse-SFTP shares like the app does (the client-side machinery in
  `kaijutsu-client` should be reusable; it's the same SSH connection).
- **Amy's ask: a nice *autoshare* of the workspace** — when kaijutsu-mcp
  connects from inside a project directory (the common Claude Code case),
  automatically offer the workspace (cwd or repo root) as a share, so the
  kernel side sees `/r/<client>/workspace` with zero flags. Needs the usual
  `/r` decisions: share name, ro vs `:rw` default, and an opt-out.

## Two live-log papercuts seen during the 2026-08-04 durability verification

Both pre-existing, both noticed while watching a live kernel; neither is a
correctness problem, both erode the value of WARN.

- **"Document already in DB but not in memory, recovering" fires on EVERY
  `kj context create`.** `insert_context_with_document` writes the `documents`
  row, then the create rc lifecycle calls `BlockStore::create_document`, which
  finds it — the benign duplicate arm, by construction, every single time.
  Since 2026-08-04 that arm *proves* benignity (kind/workspace/path all
  compared; anything else is now a `DocumentDiverged` error), so the surviving
  case is provably routine and probably wants `debug!`. Counter-argument for
  keeping it loud: it is also the signal that memory and DB disagreed, which
  matters on other paths (cache coherence). Amy's call — the noise is real
  either way.
- **Four backends warn `api_key_file configured but unreadable` at every
  start** (`gemma-26b`, `gemma-e4b`, `openai-local`, `sd`), all pointing at
  `/home/atobey/.openai-key`, which does not exist. They fall through to env
  and work, so this is config debt from the SQL-config seeding: either create
  the file, clear `api_key_file` on those rows, or teach the fall-through to
  log once at debug when the env key is present.
## Diff cursor stops at the ellipsis on an elided line (2026-08-04, slice 6A)

With column motions back, the drawn cursor and modalkit's column agree
everywhere except on a line long enough to be elided for display
(`MAX_VIEW_LINE_CHARS` = 2000 in the viewer, 500 inline). `text::diff::
cursor_byte` clamps to the end of the *shown* text, so `$` on a 5000-char
line parks the drawn cursor on the `…` while modalkit's real column is far
to its right. Bounded and rare (a minified bundle line), and the safe
direction — the alternative is a cursor drawn past the end of the text — but
it is the one place the viewer's "the cursor never lies" rule is approximate.
Fixes worth considering when someone hits it: elide in the *middle* of the
line so the tail stays addressable, or clamp modalkit's column to the shown
length on such rows (a real behavior change, so not done blind).

## Diff viewer footer + status bar overlap content rows (2026-08-03, slice-5 live verify)

Seen during the first live run of `Screen::Diff` on moltar, in a ~1080p-scale
window (the app had just survived a zorak hibernation + relaunch, so a window
scale change is in the mix): the viewer's diffstat footer strip ("diff, 1
file, +10 −6, 1/22") draws OVER the last visible content rows instead of
reserving a row for itself, the global status bar bleeds through beneath it,
and the conversation status bar's own left cluster self-overlaps ("40
fail3d.8k/1M Enter: submit"). Likely ONE bug, not three: overlay/footer text
placed with a stale or mixed logical/physical height after a scale change —
the exact `ComputedNode`-is-physical trap CLAUDE.md warns about
(`view::ui_rtt::logical_size`).

**Retested live 2026-08-03 after a clean reboot: the scale-change theory is
WRONG, and the bug is narrower than it looked.** At steady scale, on a fresh
boot, the status bar renders cleanly in `Conversation` and in `Room`
(`2 running, 40 failed  [db01a563]` … `31.8k/1M ↔: station | Enter/↓: zoom |
Esc: conversation` — well separated). It self-overlaps in the **time well**,
whose hint cluster is much longer: `40 fa1led/[db01a563]→↑↓: seat ⊙ ring |
Enter: focus/commit |c/p/d/z/a: act | Esc: room`. So this is not stale
HiDPI math after a resize — it is **status-bar segment layout overflowing
when the per-screen hint cluster is long**, with the left cluster and the
right cluster written into the same pixels instead of being measured against
the available width. Look at how the bar allocates width between the mode/
model/context cluster and the screen-hint cluster, not at `ui_rtt`.

The diffstat-footer-over-content half **CONFIRMED live 2026-08-04** (BRP
session, steady scale, fresh boot — so also not a scale artifact): with a
22-row diff open in `Screen::Diff`, the footer strip draws over the last
two content rows, and the conversation status bar renders THROUGH the
viewer's footer (both bars visible in the same pixels). Same
width-allocation family as the time-well overlap above: the footer does
not reserve a content row, and the underlying screen's bar is not
suppressed while the viewer owns the screen.

Everything else in the slice-5 checklist verified live today: `v` open (after
the DiffSurface resize-filter crash fix in `block_render.rs`), `]c`, `V`+`jj`
selection bands, `y` → Wayland clipboard (`wl-paste` exact), compose
`Ctrl+V` paste, `R` re-parse, `q` close. Still unverified: stale banner +
`R`-after-change, declared `ContentType::Diff` open path, parse-error banner
(all need kernel block edits — kaijutsu-mcp was stuck in local mode this
session; global `~/.claude.json` entry now carries `--connect`).

## Diff parse errors render as a generic banner, not line-anchored (2026-08-02)


From the slice-4 post-ship review (gemini deliberate, low severity): the
kernel attaches `DiffError::line()` spans to the ErrorPayload on `Done`
(`block_store.rs` `validate_diff`), but the app's diff error preview is a
generic banner — the line anchor is never pointed at visually. Inline line
annotation (squiggle/marker on the offending line) is high-value polish once
slice 5's full-screen error surface exists to render into. Small, app-side
only; the data already travels.

## Error-block-collapse remnants, post scaffolding removal (2026-08-01)

The unread `build_error_child_index`/`ErrorChildIndex`/`ExpandedErrorParents`
scaffolding (2026-07-30 finding) was ripped out — it was designed for a
"collapse an error's parent block to a stub with errors stacked below"
treatment that never got a reader, distinct from the dock indicator that
shipped and stays. Two related pieces of that unfinished feature are still
sitting inert and are a smaller, separate decision:

- `theme.block_error_accent` (`ui/theme.rs:131,435`, documented as the "stub
  strip" color) has no reader now that the stacking system is gone.
- `BlockSnapshot::system_error` (`kaijutsu-types/src/block.rs:1867`) is called
  from nowhere in the workspace — it builds a context-attached error *block*,
  the opposite case from the context-free `GlobalErrorQueue` path that now
  renders.

**Decide**: finish the stub-strip treatment, or delete these two remnants too.

## Rename `VelloTextStyle`/`VelloFont*` shaping types (follow-up from the de-vello pass)

`vello` itself is gone from `kaijutsu-app` (Cargo.toml, Cargo.lock — verified
via `cargo tree -i vello`, 2026-08-12: dock chrome was the last real
consumer, moved onto MSDF; `view/vello_rasterizer.rs` and the vello half of
`view/ui_rtt.rs` are deleted). What's left is a naming nicety: `VelloFont`,
`VelloTextStyle`, `VelloTextAlign`, `VelloFontAxes` (`text/shaping/`) are pure
Parley shaping types — their `Vello*` prefix no longer means anything. Left
alone during the de-vello pass because renaming ripples into every
consumer (`ui/dock.rs`, `view/block_render.rs`, `text/rich.rs`, ...) for a
naming change, not a behavior change — a separate mechanical slice.

## MCP subsystem — audit 2026-07-29 (sonnet, read-only, verified against source + git history)

Amy: *"we can add items to work on mcp servers, we haven't maintained that in a
while and have changed a lot around it."* Ranked most-broken first. Entries the
audit **disproved** were deleted from the Gemini-comparison survey rather
than left to rot (that survey now lives in `docs/wishlist-gemini-cli.md`).

- **No `InstancePolicy` persists across restart — undocumented.** `Broker.policies`
  (`broker.rs:537`) is a bare in-memory `RwLock<HashMap<InstanceId,
  InstancePolicy>>` with no DB table (contrast hooks, which *do* persist). Any
  live-tuned `call_timeout_ms`/`max_result_bytes` silently reverts on restart.
  Policy is keyed by instance globally, not per-context. **Cheaper fix than a
  policy table**: source per-instance overrides from mcp.toml at registration so
  restart re-derives them. (`max_concurrency` being registration-only is a
  *documented, deliberate* choice to avoid racing in-flight semaphore permits —
  that part is fine, leave it.)
- **Hook self-lockout has no recovery path inside our own conventions.** A
  `PreCall Deny("*")` locks out `builtin.hooks` itself — even `hook_list` and
  `hook_remove` return `Denied` — and because hooks are hydrated from SQLite at
  `Broker::set_db` (`broker.rs:275,283`, bridge in `mcp/hook_persist.rs`), it
  **survives a restart**. Proven by an executable test,
  `hooks_admin_is_subject_to_hooks` (`hooks_builtin.rs:1229`). There is no
  `kj hook` CLI. The only documented recovery is hand-editing kernel SQLite,
  which violates our own standing rule against touching that DB directly.
  Two doc comments disagree about this; `bindings_builtin.rs:22-25` ("hooks are
  in-memory", "restart to recover") is **stale and wrong** — `broker.rs:1465` is
  right. Fix the comment (one line), then decide the real question: a hard-coded
  carve-out so admin instances can always self-repair, **or** a `kj hook` path
  that bypasses broker hook evaluation.
- **`bevy_brp` and `invoke_peer` do NOT overlap** — settled, don't re-litigate.
  `peers.rs` is a named-peer action registry (`switch_context`, `active_context`)
  for drift navigation; BRP is entity introspection/screenshots. Orthogonal. The
  kernel today has **no BRP access whatsoever** — the only thing reaching
  `bevy_brp_mcp` is Claude Code's own MCP client, entirely outside the kernel.
- **kaibo as a kernel-side MCP server — the credential/cwd worries are unfounded.**
  It reads provider keys from `~/.config/kaibo/config.toml` directly, so with the
  kernel running as the same unix user (shared-trust model) a spawned subprocess
  authenticates with no env forwarding. Project root is an ordinary `--root <path>`
  arg, already representable in `McpServerConfig.args`. `cwd` is wired correctly
  (`external.rs:216`) and does **not** inherit the "headless turn cwd is `/`" bug
  (that one is kaish `ExecContext`-specific). Only the timeout items above block it.
- **Tool-name collisions fail safe — low priority.** `clean_visible_tool_name` is
  not injective (`builtin.block__x` and `builtin_block__x` both clean to the
  same), and `apply_resolutions` (`binding.rs:435`) *skips* rather than overwrites
  a colliding second tool. So the failure mode is a tool going invisible, never a
  call routing to the wrong instance. Untested for the cross-instance case; worth
  a test, not a redesign.
- Cosmetic: `hook_types.rs:3` points at `docs/hooks.md`, which doesn't exist.
- **No project-instructions discovery** (CLAUDE.md/AGENTS.md analog).
  `build_system_prompt` (`llm/system_prompt.rs:69`) assembles base + rc `.md` +
  `<situation>` and never crawls the filesystem; `assets/defaults/system.md` is
  13 lines. Every project convention must be hand-loaded into an rc script.
  *(Two Gemini-pass entries below cover the design: JIT subdirectory injection +
  filesystem memory-file discovery.)*

**Tier 2 — velocity**

- **Delegation has no join — the SIGNAL shipped, the command didn't.** The
  child's turn now publishes `TurnFlow::Completed`/`Failed` naming its context,
  in-process and over capnp (`subscribeTurnEvents`), so a waiter no longer has
  to poll the child's block log. What's missing is `kj wait` itself: it needs a
  timeout policy, an answer for the turn that ends before the waiter subscribes
  (the bus is lossy and un-journaled — see the TurnFlow durability item), and a
  decision about waiting on several children at once. Compare Claude Code's
  `Task`, which blocks or wakes the caller. Seam documented at
  `request_child_turn` (`kj/fork.rs`). *(Relates to "Headless one-shot with
  JSONL streaming" below.)*
- **No LSP / diagnostics** — no go-to-definition, no type errors without paying
  for a full compile.

## rmcp 1.7 → 3.0.1 bump left SEP-2577 deprecations papered over (2026-07-30)

Landed to talk to kaibo (now on `rmcp 3.0.0-beta.5`, a newer MCP protocol
revision). The bump itself was a clean, mechanical migration (`ContentBlock`
replaces `Content`, `Annotated<Raw*>` wrappers flattened into plain structs
with builder methods, `Meta` split into `MetaObject`/`RequestMetaObject`/
`NotificationMetaObject`, `read_resource`/`call_tool` server-side responses
wrap in `ReadResourceResponse`/`CallToolResponse` for MRTR). Negotiated
protocol version stays `2025-11-25` (rmcp's `ProtocolVersion::LATEST` didn't
move even though `V_2026_07_28` now exists in the enum) — no wire-visible
protocol jump.

What's papered over rather than migrated, each behind a scoped
`#[allow(deprecated)]` with a comment: rmcp 1.8.0+ deprecates the whole
Logging capability (`enable_logging`, `LoggingLevel`, `SetLevelRequestParams`,
`LoggingMessageNotificationParam`) and Roots capability
(`enable_roots`/`enable_roots_list_changed`) per SEP-2577, with **no
replacement** — the spec is dropping them, not superseding them. Separately,
`resources/subscribe`/`unsubscribe` (`ExternalMcpServer::subscribe`/
`unsubscribe`, `crates/kaijutsu-kernel/src/mcp/servers/external.rs`) are
legacy-only as of protocol `2026-07-28`, superseded by `Peer::listen` /
`subscriptions/listen` — a real subscription-model migration, not a drop-in
rename. Kept all three working as-is (straightforward migration, no
opportunistic rewrite) since kaibo/bevy_brp still negotiate against them
today. Revisit when rmcp actually removes the deprecated APIs, or when
adopting the `Peer::listen` model becomes worth the redesign on its own
merits.

## Input: selection auto-copies to PRIMARY (seeded 2026-07-16, input rework)

The other half of the xterm clipboard model (Ctrl+V + middle-click paste
shipped; so did the full prefix set incl. `'`/`A` prompts and the
armed-footer legend): needs a selection UX first. The overlay's
`selection_anchor` has no live producer (mouse drag-select and vi visual
mode are both unwired); when one lands, copy-on-selection to PRIMARY rides
it — `InputOverlay::selection_range` is the read point.

**Update 2026-08-02 (diff slice 5):** the *write* half now exists —
`input::ClipboardWriter`, a dedicated thread owning an `arboard::Clipboard`
for the process lifetime (a clipboard write means becoming the selection
owner, so it cannot be a call from a Bevy system). The diff viewer's yank is
its first user. What is still missing is PRIMARY specifically: `set_text`
writes CLIPBOARD, and `arboard::SetExtLinux` is the Linux-only path to
PRIMARY. Route the selection producer through `ClipboardWriter` when it
lands, and give it a PRIMARY variant then.

## Input: block-step scroll lane — Shift+wheel jumps block-to-block (seeded 2026-07-18, scroll-feel work)

The *continuous* scroll lane got made crisp (follow-mode deadzone fixed;
high-res `Pixel` stream row-quantized via `quantize_step` +
`PIXEL_QUANTUM_PX`; `ScrollConfig` two-gain per-client config). The
*contextual* lane is the deferred half: **Shift+wheel steps by whole
blocks**, snapping a block top to the viewport — Amy's "zip / stop / skim /
narrow-in" pattern wants to move by *meaning*, not pixels. Cheap: reuse the
existing block-nav + `scroll_to_rect_visible` (`input/systems.rs:562`) and
`handle_navigate_blocks`; `BlockKind` (`kaijutsu-types/src/block.rs:945`)
is there if we ever want kind-awareness. Trigger decided: **Shift+wheel**
(both lanes always live, no mode). Add a `ScrollBlockStep(dir)` action to
the table so gamepad/rebind/`?`-legend come free (never read `MouseWheel`
in a view — dispatch owns it).

**Consciously fenced-off complexity traps** (do NOT pick these up without a
deliberate decision — this is the rabbit warren): per-block *adaptive* gain
that guesses granularity from what's under the viewport (unpredictable);
momentum/inertia physics; animated scroll-snap settle; minimap /
semantic-zoom skim (the time well already gestures at that —
`docs/timewell.md`). Two explicit lanes beat one clever adaptive thing:
the player picks the granularity, the instrument does not guess.

## msdfgen-rs `Shape::get_bound()` / `Contour::get_bound()` zero-seeded (seeded 2026-07-16, msdf-geometry lane; sidestepped 2026-07-16, msdf-bbox lane)

`msdfgen-rs` (`/home/atobey/src/msdfgen-rs`, local path dep, do-not-modify)
has a real bug: `Shape::get_bound()`/`Contour::get_bound()` seed their
accumulator at `(0,0,0,0)` and only ever *shrink toward* an extreme, instead
of pre-seeding `±LARGE_VALUE` the way the C++ `Shape::getBounds()`
convenience does (which msdfgen-rs never binds). Net effect: `left`/`bottom`
silently stay `0.0` whenever a glyph's true left/bottom edge is positive —
the common case (left-side bearing, glyphs above the baseline). Verified
against `CascadiaCodeNF.ttf`'s `.` glyph: ttf-parser reports `x_min=452`,
`Shape::get_bound()` reports `left=0.0`.

Sidestepped, not fixed: `kaijutsu-app` no longer calls `get_bound()`
anywhere (`generator.rs::generate_glyph` sizes/centers/anchors every glyph
from `ttf_parser::Face::glyph_bounding_box()` instead), so exposure to this
bug is zero for us. Upstream (`katyo/msdfgen-rs`) is dormant; the bug itself
is still real for anyone else depending on the crate. Nice-to-have,
unclaimed: patch `get_bound()`/`get_bound_miters()` to seed `±f64::MAX`
before the raw C++ `bound()`/`boundMiters()` call, and send it upstream as a
PR. Amy also wants to check back later on the pure-Rust `bymsdfgen` crates
as a possible future replacement for msdfgen-rs's C++ core.

## Context lifecycle: "done for now" marker (seeded 2026-07-16, input rework)

Amy wants a soft "done for now" intent marker on contexts — distinct from
`ContextState::Concluded` (done, sticky, never visit-repromoted) and from
demotion (placement, not intent) — as the hook for automation like
"summarize contexts that are done changing". `Ctrl+A q` (close-and-demote,
`docs/input.md`) works today without it; this is the seed for the semantic
layer above placement. Design question when picked up: a new `ContextState`
vs a stamp alongside `promoted_at`/`demoted_at`/`paused_at`.

## DJ thread arc (seeded 2026-07-18; design in `docs/midi.md` "The DJ thread")

Slice 1 SHIPPED + live-verified 2026-07-18 (`docs/midi.md` has the story
and the click-jitter measurements; it also closed the "metronome stops
when backgrounded" symptom). Open:

- **Slice 2 — beat-grid placement**: Tasks 1–3 SHIPPED (wire + kernel
  stamping, `DjCore`'s placement core, `dj/thread.rs`'s `run_loop` wiring —
  `decide_placement`/`enqueue_cue`/`due_cues`/`next_cue_wake` all live end to
  end, `kaijutsu.dj.cue_dropped` telemetry added alongside
  `kaijutsu.dj.clock_transition`). **Task 4 (live verify on the runner) still
  open**: confirm musical phrase onsets lock to the click grid rather than
  drifting with wallclock/network jitter, and watch `clock_transition` +
  `cue_dropped` through a deliberate flush/disconnect mid-phrase.
  - **Known reporting gap, left as-is (Task 3)**: `kaijutsu.dj.cue_dropped`
    fires correctly off `on_flush`/`on_disconnect`/`due_cues`, but TWO other
    `DjCore` entry points also run the internal `settle()` staleness/
    free-run-cap check and can therefore ALSO drop `pending_cues` on their
    own — `due_clicks` (`DueClicks` has no `dropped_cues` field) and
    `decide_placement` (`(CuePlacement, Option<ClockTransition>)` has no
    `dropped_cues` slot either). When one of THOSE is the call that trips
    the fallback, the drop happens (no cue is ever resurrected — the
    "dropped silently" half of the contract holds) but isn't
    telemetry-counted. Fixing it means growing `DueClicks`/
    `decide_placement`'s return shapes in `dj/core.rs` — small, but out of
    `dj/thread.rs`-only scope; pick up alongside Task 4's live verify if the
    gap turns out to matter in practice (`dj/thread.rs`'s `DjEffect::CueDropped`
    doc has the full trace).
- **Frame-cost arc (separate from DJ; visual smoothness + input latency)**:
  (a) rich/markdown parse results are re-computed on every block version
  bump — the `_version` params on `detect_rich_content_typed` /
  `detect_output_content` are threaded but unused (`text/rich.rs:390`);
  cache per (block, version). (b) Parley shaping in `build_block_scenes`
  (`view/block_render.rs:247`) is synchronous on the main thread; a
  streaming burst dirties dozens of blocks in one frame — budget per frame
  or offload like MSDF generation already is. (c) O(N) geometry reconcile
  per doc-version bump (`view/geometry.rs:604`).
- **Unify "animation active → continuous render" (from the 2026-07-18 scroll
  work)**: the app is reactive-idle (`main.rs` `WinitSettings`, 10Hz focused),
  which starves any frame-by-frame animation — smooth scroll eased at ~10Hz
  felt laggy until the render loop was forced to 60Hz. The scroll fix bumps
  `focused_mode` to `Continuous` only while a scroll ease is in flight, back
  to reactive when settled (self-contained in the scroll systems). But the DJ
  thread and other motion (playhead, time-well drift, metronome flash) want
  the *same* "keep rendering while I'm animating" signal. Generalize to one
  gate — an `AnimationActive` vote (ref-count / any-of) that maps to
  `UpdateMode` — that each animating subsystem, DJ thread included, opts into,
  so nothing hacks the render policy ad hoc. Amy's framing (2026-07-18): "maybe
  something the DJ thread could handle independent of rendering" — caveat:
  pixels still require the render loop to run, so this is a keep-awake/wake
  gate, not a parallel draw path off the DJ thread.
- **Broadcast-lag cascade watch item**: UI-side stall → 256-slot broadcast
  `Lagged` → generation bump → full re-sync → bigger dirty burst. The DJ's
  own receiver is immune (own cursor), but the UI drain keeps the loop —
  if it still bites after the frame-cost arc, consider a bigger event
  capacity or delta-coalescing at the actor.
- **Prefetch outcomes are not generation-guarded**: a CAS outcome dispatched
  under a since-replaced actor still lands (the deadline gate still applies;
  pre-existing posture, not a regression). Guard on `generation` if a stale
  outcome ever observably misfires.
- **Stale-architecture prose**: `docs/scenes/patchbay.md` + `docs/pcm.md`
  still narrate `midi.rs`/`metronome.rs`/`AudioOutPlugin`; two doc-comment
  mentions of `crate::metronome` sit in the parallel session's files
  (`actor_plugin.rs:204` rustdoc link, `input/scroll_config.rs` prose) —
  fold into the next docs pass / that session's next commit.

## Audio sink follow-ups (seeded 2026-07-16, clip-arc live verify)

- **CasResolver's SFTP session has no proactive keepalive** (seeded R4,
  2026-07-16): R4 (`docs/pcm.md`) bounded per-fetch recovery with a
  `FETCH_TIMEOUT` + logged redial, closing the "slow + silent" symptom the
  fanfare-clip failure exposed — but nothing yet keeps the session alive
  *between* fetches (no TCP/SFTP keepalive ping), so a long-idle connection
  still goes stale; R4 just detects and redials it promptly now instead of
  ~70s later with nothing in the log. Add a periodic no-op ping on the
  resolver's connection if idle recovery still needs to be faster than
  `FETCH_TIMEOUT` in practice.

## Conversation geometry model — accepted limits (seeded 2026-07-16, from `82207a2a`+`7e3f2fa1`)

Both `6504fafe` follow-ups shipped (estimated-height placeholders killed the
O(N) first-load pass; band despawn/respawn bounds entities + VRAM to the
viewport band). New accepted limits of the geometry model, carried here:

- **Offscreen-streamed rows keep stale heights** until they re-enter the
  virtualize show window (±1 screen around the viewport, so still one full
  screen before they can be seen) — the scrollbar/spacers drift a little
  while text streams into rows you've scrolled away from. This is the safe
  side of the trade: virtualize's shown set must always be **one contiguous
  document interval**, because the two `ConversationSpacer` nodes can encode
  exactly one gap above it and one below. Force-showing an offscreen stale
  row (what we used to do) makes taffy pack it directly under the in-window
  rows — visible corruption, and readback then measures it there. If the
  drift ever matters, fix it by re-estimating on version change, never by
  breaking contiguity.
- **Height corrections landing on a partially-visible row are not
  scroll-anchored**: `readback_block_heights` only compensates rows *fully*
  above the viewport (measured against pre-measure offsets), so a straddling
  row's correction shifts the content below it. Normal scrolling can't reach
  this — a row enters the show window a full screen before it is visible, so
  it is measured while still fully above/below — but a jump-scroll (scrollbar
  drag, jump-to-block) can drop the viewport straight onto a row whose height
  went stale, and the snap is then visible for a frame or two. Converges,
  never oscillates (heights are deterministic); worst case is a block that
  grew hugely while offscreen.
- **ToolCall at the outer band edge** can briefly render CloseBottom while
  its ToolResult is still outside the band (border joins are computed from
  in-band snapshots only); corrects as the pair scrolls in.
- **`handle_collapse_toggle` still clones the whole document** on keypress
  (`editor.blocks()` in input/systems.rs) — noticeable hitch on huge
  contexts; could walk geometry rows for Thinking ids instead.
- **Width changes only re-estimate unmeasured rows**; measured heights of
  despawned rows go stale on resize until band entry (same just-in-time
  correction path, pre-existing behavior).

## Anthropic client maturity (seeded 2026-07-15, thinking-enable arc)

Adaptive thinking (`{type: adaptive, display: summarized}`, model-gated)
landed 2026-07-15 in `claude::Client::stream()`. The client is young; we own
it precisely so we can tailor to the provider — gaps observed while wiring
thinking, roughly by priority:

- ~~No config path into provider clients~~ **SHIPPED 2026-08-03** by the
  cast renovation: per-slot `effort`/`thinking_style`/`thinking_budget`/
  sampling on `cast_slots`, cascading to `llm_defaults`, reaching the wire
  through `apply_slot_tunables` — "effort: low here" is now
  `kj cast slot set <cast> <role> --effort low`.
- **Model capability knowledge is string-parsing.** `Thinking::default_for_model`
  parses `claude-<family>-<major>-<minor>` and gates on `>= 4.6`. Fine for
  one knob; a second capability (effort levels, sampling-param rejection on
  4.7+, 1M context) wants a small capability table — or a startup query of
  Anthropic's Models API (`GET /v1/models/{id}` returns `capabilities`),
  which is the tailor-to-provider move.
- **`temperature` will 400 on Opus 4.7+/Sonnet 5/Fable if ever set.**
  `BuildOpts.temperature` is currently never set on the Claude path, so
  latent — but nothing gates it. Same capability-table story as above.
- **Cross-model history replay is untested.** A context that switches
  Claude→other-provider (or Claude-with-thinking→haiku) replays
  `ContentBlock::Reasoning` blocks into requests where they may be rejected
  or silently dropped. Hydration/splice may need a per-provider filter.
- **`available_models()` is a hand-maintained list** (opus-4-8 was missing
  until 2026-07-15; fable-5 still absent pending a routing decision). The
  Models API query above would retire it.

## Beat-tracking + local-model follow-ups (seeded 2026-07-15, rten/beat-this arc)

`kj audio beats` (beat-this crate, rten backend) and the rten embedder swap
landed 2026-07-15. Deliberately left out, in rough priority order:

- **Track integration is the real prize**: seed a track's tempo/cadence from a
  reference recording (`kj audio beats` output → transport arm), and run beat
  analysis on rendered/captured clips once the clip seam (`docs/pcm.md`) exists
  (bytes never ride the track; beats are exactly the derived-result shape that
  should cross the wire instead).
- **Model registry / `kj models` verb**: two model dirs now follow the
  `~/.local/share/kaijutsu/models/<name>/` convention (bge-small-en-v1.5,
  beat-this) with install instructions living beside the embedding recipe in seed_backends.rs and a
  README. A registry (name → expected files → checksum → fetch) would make
  `kj models list/fetch/verify` possible and close the manual-download gap.
  This is also where a `kaijutsu-inference`-style shared crate becomes
  justified — explicitly deferred (Amy, 2026-07-15) until there's real
  sharing; the Embedder trait + per-crate rten deps are the seam until then.
- **audio/beat-this model config**: the verb hardcodes the model dir
  convention; a config home now means a kj-managed kernel-db table (the
  2026-08-03 SQL model-config shape — models.toml is gone, and the
  seeded-once CRDT caveat died with it).
- **Vendor risk note**: beat-this is v1.0.0, single maintainer (danigb), MIT —
  small enough to vendor/fork if it stalls. rten GPU support (Metal-first) is
  on its author's 2026 roadmap; CPU is fine for our workloads.
- **MCP `shell` tool returns `data: null` for every kj verb** (observed
  2026-07-15 during live verify): even `kj context list`, whose .data shape
  is documented, comes back null over the MCP surface — so `kj audio beats`'
  structured payload is unreachable there too. Either the MCP shell path
  drops KjResult data or it was never wired; find which and either wire it
  through or fix the tool description that promises it.

## MIDI device profiles + device contexts (seeded 2026-07-15, `docs/midi-next.md`)

Design direction captured in `docs/midi-next.md` (living doc): CRDT-owned
device profiles under `/etc/midi/devices/` (rc-style buckets: static `.md` +
kai-synthesized current picture; settings vs capabilities ground-truth
split), track bindings as *device.role* not raw
channel ints, rc-injected device contexts as side channels (profile as skill
body + narrow loadout + cheap model), `kj midi` emit verbs + provenance-tagged
`/run/midi/<device>` state, SysEx via a sink `exchange()` method (transfer
job shape deferred). Slice 1 steps 1–3 have landed (seeds + `kj midi
list/show`; sink-fed presence: in-app profile matching, `reportMidiPresence`,
the ephemeral `/run/midi/<device>` store, presence column) — as has step 4
(`kj midi send`/`panic`: a device-addressed control cue on the existing
`RenderCue` wire, riding a per-device `ctl:` port wired by subscription —
the original DIRECT emit shipped but no hardware could hear it, see the
2026-08-02 bench story in `docs/midi-next.md` step 4) and step 5 — the
`exchange()` round-trip + `kj midi identify`, which closes slice 1. Slice
order in the doc; next: slice 2 (routing consumes profiles).
First real consumer: Minibrute on the laptop app, then the per-track
channel-routing fix (this file → Hyoushigi/Musician area; `docs/chameleon.md`
open items) built on profile vocabulary.

- **USB `vendor:product` enrichment for presence matching** (deferred
  2026-08-02, slice 1 step 3). `midi_match::PortFacts::usb_id` exists and the
  matcher already ranks a USB hit above any name match, but nothing fills the
  field: on Linux it needs an ALSA-card → sysfs walk
  (`/sys/class/sound/cardN/device/{idVendor,idProduct}`). On macOS it is
  costlier than it looks (gemini review, 2026-08-02): CoreMIDI endpoints do
  NOT expose USB ids — the sink must bridge into IOKit
  (`IORegistryEntryCreateCFProperty`) and walk the hardware tree to find
  `idVendor`/`idProduct`; port-name substrings are the saving grace since
  they absorb per-OS suffix differences. Until then matching is name-substring only,
  which the shipped profiles all support. Two same-model units on one rig
  (identical names, distinct USB paths) is the case that will force it — and
  will also need something finer than `vendor:product`, which is per-model,
  not per-unit.
- **`kj midi send` routes to a device's FIRST matched port** (deferred
  2026-08-02, slice 1 step 4). `dj::midi::resolve_route` takes
  `routes[device][0]`; for a two-port device (KeyLab: MIDI + DAW) that is the
  MIDI port, which is right today but is a positional accident, not a
  decision. Slice 2's *device.role* vocabulary is the fix — the routing table
  already carries every matched address, so only the picker changes
  (`a_multi_port_device_routes_to_its_first_matched_port` is the tripwire).
- **`kj midi send` writes no `/run/midi` sent-provenance** (deferred
  2026-08-02, slice 1 step 4). `docs/midi-next.md` says every emit should
  record `{value, source: sent, at}` so relative commands ("-25%") have a
  baseline to work from. Slice 3 (device contexts) is where that pays off;
  slice 1 emits raw absolutes only, so there is nothing to be relative to yet.
- **The exchange timeout ladder is four constants in four crates** (deferred
  2026-08-02, slice 1 step 5). A request's bound is enforced at every hop, each
  a little looser than the one inside it so the *innermost* layer that actually
  wedged is the one whose error a player reads: app worker `T`
  (`midi_exchange::ExchangeClient::exchange`), client forwarder `T + 0.5s`
  (`MidiExchangeSlot::WORKER_SLACK`), server bridge `T + 0.75s`
  (`rpc::EXCHANGE_CALL_SLACK`), kernel `T + 1s`
  (`midi_exchange::KERNEL_DEADLINE_SLACK`). Nothing enforces the ordering but
  these comments; if a fifth hop appears, the ladder wants a single home
  (`kaijutsu-types`?) rather than a fourth doc-comment promise.
- **Exchanges serialize per SINK, not per port** (deferred 2026-08-02, slice 1
  step 5). The doc says "serialized per-port"; today one worker thread runs one
  dialogue at a time for the whole app, which is strictly stronger and costs
  nothing while exchanges are human/model-paced (identity, later a settings
  pull). A grooming track sweeping ten devices on a cadence is the case that
  will want per-port concurrency — and the worker is where it goes, not the
  wire (the request already names its port).
- **`kj midi identify` picks the device's FIRST matched port too** (deferred
  2026-08-02, slice 1 step 5). Same positional accident as `kj midi send`, same
  slice-2 fix: `midi_exchange::resolve_exchange_address` takes
  `routes[device][0]`. A KeyLab answers on its MIDI port, which is right today.
  **No longer hypothetical (2026-08-02 evening):** the MiniBrute answers
  Universal Identity ONLY on its port 1 ('MiniBrute MIDI Interface', the
  SysEx/control port) while routing asks port 0 (the synth) — so
  `kj midi identify minibrute` times out against a device that answers.
  First-matched-port now demonstrably picks the wrong port for a real device;
  the slice-2 role vocabulary (port roles in the profile: synth vs control)
  is the fix, and the minibrute profile already records which port answers.
- **No CoreMIDI exchange backend** (deferred 2026-08-02, slice 1 step 5). The
  worker is ALSA-only and says so in its answer (a mac sink refuses with "not
  this backend" rather than timing out, so another sink on the rig can have the
  gear). Everything above the worker — the wire method, the registry, the
  device-name addressing — is already backend-neutral.
- **CoreMIDI control-cue addressing** (deferred 2026-08-02, slice 1 step 4).
  `dj::midi::parse_alsa_addr` refuses anything that isn't `client:port` rather
  than guessing, so a CoreMIDI-shaped address reaching the ALSA sink is a loud
  drop. The mac backend will need its own address parse + emit alongside the
  ALSA one — the envelope and the routing table are already backend-neutral
  (device names and opaque address strings), so nothing above the sink changes.
- **`kj midi identify` timeout is a MUTE error** (found live 2026-08-02, bench
  verification). When no reply arrives, the `tool_result` block lands with
  `status: error, exit_code 1` and **zero content** — no "timeout waiting
  for reply from keystep-pro", nothing in stderr, nothing kernel-side. The app
  log is the only witness (`midi_exchange: … waiting 2s` then silence). Loud
  over silent: the timeout should say which device, which port, how long it
  waited, and that presence said the device was live. The mute error cost real
  diagnosis time: the actual fault was host-side silent drop (next entry), and
  a mid-session hypothesis ("Arturias ignore identity requests") stood for an
  hour before wire counters disproved it — the KSP answers identity fine
  (`F0 7E 7F 06 02 00 20 6B 02 00 09 00 5D 01 00 02 F7` captured live).
- **Arturia SysEx vocabulary in kaijutsu — make settings readable** (wished
  2026-08-02, Amy, bench session). Probing the MiniBrute's settings today means
  hand-rolled `aseqsend` hex guesses at the Arturia frame
  (`F0 00 20 6B <dev> 01 <seq> <op> <param> F7` family). Amy wants a clever
  encoding in kaijutsu for these request/reply frames — device profiles could
  then *declare* their parameter maps (receive channel, knob assignments) and
  `kj midi pull`/device contexts could read real settings instead of trusting
  the profile's static claims. Makes configs easier to build; pairs with the
  `exchange()` machinery that already exists and with `docs/midi-next.md`'s
  settings-vs-capabilities ground-truth split.

## App (and headless sink) as MCP clients offered back to the kernel (seeded 2026-07-15)

Eventual direction (Amy): `kaijutsu-app` — and the future headless sink
variant — should also be **MCP clients**, offering their local capabilities
(audio devices, capture, render, screenshots?) back to the kernel as tool
surfaces, the way `kaijutsu-mcp` exposes the kernel outward today. Deep work
(client-side MCP host, capability registration, routing through the broker);
deliberately parked — noted while designing audio capture so the capture
seams don't foreclose it.

## MIDI seq topology unreachable from kj/kaish — wants patchbay slice 4 (noted 2026-07-18, "can you see MIDI devices via kj?")

The observed ALSA seq graph (which MIDI devices exist + how they're patched) is
read **only** by `kaijutsu-app` (`patch_graph.rs::PatchGraphReader` →
`PatchBayState`) to render the patch-bay scene — slice 0. It's reachable by no
`kj` verb, no kaish VFS mount, and not even BRP (`PatchBayState` is a plain
`Resource`; `GroupPlate`/`PortLabel`/`SocketPeg` are `Component`-only, no
`Reflect`). So an agent can't answer "what MIDI is on `<app-host>`?" today —
only a same-box model can, via **pawlsa**.

This is exactly **`docs/scenes/patchbay.md` slice 4** ("ship observed-graph
snapshots kernel-ward so models and remote peers see the same fabric"),
deferred behind the viz-first slices — surface it via `kj midi ls`/`kj seq`
and/or a read-only kaish VFS mount (feeds the `kj midi` + `/run/midi/<device>`
plan under *MIDI device profiles*; provider is the app-as-MCP-client work above,
since the seq graph is edge-local to the app's machine, not the kernel's).
Cheap app-only interim if wanted sooner: `Reflect`-register `PatchBayState` +
the label components so BRP can read them (scene-dependent, no kernel round-trip).

## Grooming tracks — kaijutsu-style cron (seeded 2026-07-15, MIDI-profiles round)

Scheduled background operations as **tracks**: a slow clock + probe
attachments (`ooda_armed: false`) firing kai scripts on beats. Kinship:
chameleon's cue traps are "cron in musical time" (`docs/chameleon.md`,
unbuilt); this is the same machinery at ops tempo, and the rc synergy is
direct (groomer scripts are CRDT-owned, `kj rc edit`-able). Use cases
queued up: device-profile refresh (`kj midi identify`/`pull` sweeps,
`/run/midi` staleness, pulled-vs-document drift flags — likely first
consumer, `docs/midi-next.md` "Keeping it current"), archive rotation,
index/synthesis grooming, oplog/CRDT compaction, auto-memory grooming.
Needs a design round before code — write the companion doc when the first
consumer is real.

Harness steals for that design round (2026-08-04 gap-analysis session,
`~/src/meadow-lab/docs/kaijutsu-gap-analysis.md`): Hermes' missed-run policy —
catch up once after a grace window (half-period, clamped 120s–2h), then
fast-forward, no backlog replay (`cron/jobs.py:2155` in the research clone);
Hermes' no-NL-parsing stance (curated shorthand like `every 30m` + raw cron,
compiled up front); QwenPaw's `HEARTBEAT.md` — a user-editable "what should I
check on" prompt re-read each beat (in kj that's a block, `kj rc edit`-able);
QwenPaw's idle trigger (proactive check-in after N idle minutes, gated on
agent-not-busy); Hermes' per-job model pinning → per-track cast binding, which
casts already shape-match (local toil vs. deepseek/claude cognition).

## FSN landscape follow-ups (updated 2026-07-13 post-slice-1, `docs/scenes/vfs.md`)

Slice 1 (ambient world) shipped 2026-07-13: kernel-native heat digests,
recency glow, N-archway glow, ship overhead, windows (vfs.md Status). Amy's
reframe — ambient instrumentation, not a file browser — DEPRIORITIZED the
bloom/vi-dive/search items below; they stay on record, not on deck.

- **Generation/staleness invalidation.** `view::fsn::sync::FsnState` still
  caches listings forever once fetched. Slice 1 laid groundwork: activity
  digest entries now carry each directory's current listing-generation
  (`VfsActivityEntry.generation`), so a per-cell stale-detect + re-pull is
  buildable without new wire. Stage-2 inotify remains the real fix for
  non-VFS-mediated writes (generations are blind to them). Deprioritized:
  stale geometry is acceptable in the ambient reading; heat is the live
  signal.
- **Heat drama pass (Amy eyeball).** Live-verified working, but the material
  warm reads subtle at distance: the hue lerp + `HEAT_GAIN_LIFT` (0.6,
  `view/fsn/heat.rs`) compete with baked recency gold on fresh districts, and
  a deep storm reaches visible fields only through ancestor attenuation
  (0.5^depth). Candidates: raise HEAT_GAIN_LIFT, HDR-boost the hot hue, or
  bloom the joints (the solid-tier plan). All consts at tops of heat.rs /
  scene.rs / layout.rs / backdrop.rs, tagged **Amy-tunable**.
- **Root-fetch truncation starves hot districts of their own fields.** The
  "/" fetch (depth 2, 4000-entry cap) truncates before alphabetically-late
  children on a real root (/tmp, /usr, /var got no listing → no field of
  their own; their heat shows only via the root field's material). Slice-0
  behavior, more visible now that heat wants those fields. Candidates:
  per-child follow-up fetches, higher cap, or fetch order by heat.
- **Seam grid is the parent's structural cross, not per-quadrant-occupied
  boundaries** (`view::fsn::layout::seam_grid`'s own doc) — revisit with
  vfs.md Open Question 2.
- **Subdir "bloom" grammar** + **dive into vi on a file cell** — unbuilt,
  deprioritized (ambient reframe).
- **`/` search-and-fly-to** (vfs.md OQ 5) — unbuilt, deprioritized.
- Zone tint (vfs.md OQ 4) untouched. Windows (OQ 3) SHIPPED in slice 1.
- **Portal camera: controls + scripted flybys + heat-directed retargeting**
  (Amy direction, 2026-07-13 — "later tho, just for direction rn"): when
  the portal is focused/fullscreened, add (a) manual camera controls, (b) a
  library of scripted camera moves with the current orbit as the default
  automatic flyby, and (c) data-driven retargeting — e.g. the camera swings
  toward `~/src/kaijutsu`'s district when it heats up. Iterate as more data
  feeds the world (trickle enumeration, stage-2 inotify host weather). The
  `orbit_pose` seam is already the single pose authority for both the RTT
  camera and the visible vessel — a camera-director that outputs poses
  slots in there, and whatever it flies, the dived-world vessel follows
  for free.
- **CAS as a hash-ring neighborhood** (Amy seed, 2026-07-13): `/v/cas` is
  already 2-hex-prefix sharded (256 buckets) — render it as a bespoke ring
  district (a "central neighborhood" rotunda) with shards placed by hash
  prefix around a circle instead of the generic Voronoi field:
  deterministic, stable, visually distinct from directory districts. Today
  `/v` renders as a flat cell at best (sorts past the root-fetch truncation
  + backdrop cap). Plugging it in means opening the layout-mapping seam
  slice 0 deliberately fence-posted (`view/fsn/layout.rs` module doc:
  "deliberately a single pure function, not a mapping-selection system") —
  a per-path layout override with the CAS ring as its first customer.
  Pairs with the mime-keyed CAS / clip-cell ideas in `docs/pcm.md`.
- **`Screen::Fsn` dive is keyboard-unreachable** (2026-07-13, the
  whole-wall zoom retune): Enter on N now fullscreens the portal
  (`station_is_zoomable`, `room/mod.rs`) instead of transitioning to
  `Screen::Fsn`; the dived world, its fly camera, and `toggle_time_well`'s
  sibling paths all still exist and pass tests. Deliberate — Amy: "we will
  probably not have a dive into fsn any time soon." When the dive earns a
  surface again, candidates: Enter-again while zoomed on N (progressive
  zoom; needs same-frame key-ordering care — see `room_keyboard`'s doc), or
  a dedicated key. If it stays unreachable long enough, consider deleting
  the screen instead of carrying it.

## MCP `data` shape change + unbounded rich_json (seeded 2026-07-18, `kj transport list` / `OutputData.rich_json` 3-model review)

Follow-ups noted but not done while fixing the review findings on `kj
transport list` and the `OutputData.rich_json` wire-through
(`crates/kaijutsu-client/src/rpc.rs` `parse_block_snapshot`,
`crates/kaijutsu-server/src/rpc.rs` `block_output_data`):

- **MCP `shell`/`context_shell` `data` field SHAPE CHANGED (breaking).**
  `ShellCompletion::to_json` now emits `OutputData::to_json()` (rich_json
  verbatim, or inferred row-objects) instead of the raw
  `{headers, root, rich_json}` struct — affects `kj` AND node-tree builtins
  (`ls`/`find`/`glob`). Flag for release notes: MCP agents are the consumers
  you can't grep for.
- **rich_json size is UNBOUNDED.** `.data` bypasses kaish's output limiter
  (which only caps text), so `block_output_data` persists it whole and
  `build_output_data` fans it out on every `OutputChanged` + context
  snapshot. A huge `.data` (e.g. `kj block list` over a giant context) has
  no cap anywhere. Follow-up: a size ceiling at `block_output_data` (fail
  loud, doctrine-consistent) or route large payloads through CAS like
  `RenderCue`'s `casHash`.

## External MCP servers — no `kj mcp restart <name>` (seeded 2026-07-30, `docs/external-mcp.md`)

`reconcile_with_toml` (`crates/kaijutsu-kernel/src/mcp/external_registry.rs:23-35`)
deliberately never reconnects an already-running external server on
`kj mcp reload` — only its `InstancePolicy` (e.g. `call_timeout_ms`) is
refreshed, even if `command`/`args`/`env` changed underneath it. Picking up
such an edit today needs a full kernel restart. A name-scoped
`kj mcp restart <name>` (unregister + reconnect one instance) would close
that gap without touching the conservative reload-doesn't-reconnect default;
the reload-vs-hot-swap tradeoff is written up in full in
`docs/external-mcp.md` "The reload design fork."

## SFTP over the VFS (slices 0–2 + extensions + tracing landed 2026-06-26; slice 3 dissolved; limits + TOCTOU open)

Read + write + OpenSSH extensions ship (`crates/kaijutsu-server/src/sftp.rs`,
the `"sftp"` arm in `ssh.rs`). Two DeepSeek reviews + a Gemini Pro batch
whole-file review are folded. Remaining, in `docs/sftp.md` slice order:

- **Slice 3 dissolved (2026-06-27, `docs/slash-v.md` "Capability")** — SFTP stays
  read/view with the lexical `privileged_write_denied` deny; per-operation join
  on the ambient `context_id` covers the real write surfaces. Surviving crumb:
  register SFTP connections in the participant registry (slash-v track V slice 2).
  Hygiene note (slice-4-adjacent): the lexical deny sits *above* symlink
  resolution — verified not-a-bypass (twice: `LocalBackend::resolve`
  canonicalizes *and* re-clamps with `canonical.starts_with(canonical_root)`,
  `vfs/backends/local.rs:102-113`, so an escaping symlink is rejected
  `path_escapes_root`; and gated paths are a separate `ConfigCrdtFs` mount
  reached by VFS prefix, not OS-symlink-reachable) but the gate belongs below
  resolution.
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

## `/r` client shares — reverse SFTP (design `docs/slash-r.md`; slices 0+1+stitch SHIPPED 2026-07-13)

Shipped: streaming pump + `VfsOps::open_read_stream` + streaming CAS +
`kj cp` (`ad4b212e`), the full read-only reverse-SFTP loop + held-handle
stream stitch (`99d4e5cd`). Design + review trail in `docs/slash-r.md`.
Remaining, roughly in order:

- **Live verification** — not yet run on a real kernel: kernel restart,
  kaish `ls /r` (check the kaish shadow-overlay papercut that bit `/v/cas`),
  an app launched with `--share`, `kj cp` out of the share, disconnect
  behavior. Needs an app invocation carrying the flag (runner arg).
- **`kj share` verbs** (slice 2) — `ls` (render `/r/index`), eject;
  `/v/session` rows for share channels.
- **`:rw` writable shares** (slice 3) — parsing ships; both-ends enforcement
  + write path don't.
- **Notify push** (slice 4) — client-side watcher → generation bumps +
  activity digests (FSN heat for client-local edits).
- **Generation lookups are unbatched** — the `kaijutsu-generation@` EXTENDED
  request carries `paths: Vec` but `ShareFs::getattr` sends one path per
  call: 2 RTTs per stat, and forward-SFTP `readdir` over `/r` costs
  N×(LSTAT+EXTENDED). Batch at the `readdir` seam when it hurts.
- **Reconnect leaks one `SshClient` handle** per re-dial for the process
  lifetime (`share_dial.rs` — `russh_sftp::server::run` exposes no
  completion signal to know when dropping is safe). Slow leak, reconnects
  are rare; fix wants an upstream hook or a wrapper stream signal.
- **Crawl opacity reuses `snapshot`'s `denied` wire field** — FSN renders
  an opaque `/r` the same as a permission-denied dir; a distinct bit (and a
  deliberate FSN rendering for "someone's machine is here") is follow-up.
- **kaish `cp` still slurps whole files** (kaish-kernel 0.12
  `tools/builtin/cp.rs:202`) — upstream candidate now that the kernel-side
  pump exists as prior art.

## Shared state space + myaku (design `docs/shared-state.md`; myaku detail in git history)

High-level sketches landed 2026-06-28; dedicated design sessions to follow. The
thesis: the VFS *is* the shared-state namespace; tiers are mounts (`/run`
`MemoryBackend` for ephemeral read-write — its own mount, `/scratch` likely retired
— and `/v` for read-only/CRDT durable). No bespoke store. Open work that's already
concrete:

- **`VfsOps::append` (or open-for-append cursor).** No append primitive today;
  `write_all`/`>>` are O(n) truncate+rewrite (`vfs/ops.rs` `write_all`;
  `MemoryBackend::write` is O(1) at `offset=size`). myaku sidesteps via bounded
  rewrite and OODA writes are turn-cadence, so this is not blocking — but an O(1)
  append would make jsonl logs and `>>` cheap. Also closes the SFTP
  concurrent-appender lost-update facet noted in the SFTP section above.
- **myaku pulse facility — RETIRED 2026-06-29** into beat-on-track (a probe is a
  context attached to a system-clock track whose tick writes `/run`; detail in git
  history, `docs/myaku.md` deleted). Surviving open pieces: the `/run` output
  substrate + `pulse_emit` land here (write up the `/run/pulse/<x>/` layout when
  they do); the app `DockSparkline` rewrite-to-read-`/run` note still stands.

## `/v` surfaces (design canonical in `docs/slash-v.md`; track B landed 2026-07-02)

Track B (`/v/cas` + client CAS sync) is LIVE; track V (`/v/ctx` + `/v/session`)
is unbuilt. (`/v/docs`/`/v/input` are kaish-side mounts, not kernel-`MountTable`,
so not SFTP-visible.) The design details live in the doc, shipped-story in
devlog/git; this entry is the backlog pointer:

- **Track B follow-ups (not blocking; the landing incl. the audible
  `kj play --cas` demo is live-verified 2026-07-02):** fetch-on-cue today
  → two-phase **prepare-horizon** prefetch + precise `lead` scheduling (warm the
  cache when a cell becomes known — `docs/pcm.md` "Open questions"); the blocking
  `FileStore` cache read in the async resolve wants `spawn_blocking`; and the
  **clip-record** path (parse Shape A `Clip` → resolve `media`; the audio bytes
  path already exists). Ingest stays `kj cas put` (SFTP→`/tmp` two-step);
  writable staging-over-SFTP deferred; B2 `index` deferred (below).
- **Track B kaibo-review deferrals (2026-07-02, low/pre-existing; the review's
  real findings shipped in `95785e28`).** Left for later, none blocking: **(a)** `CasFs`
  does synchronous `std::fs` inside `async` VfsOps — a large SFTP read blocks a
  tokio worker; this matches `LocalBackend` and is a VFS-layer pattern, not a
  track-B bug (fix the whole layer with `spawn_blocking`/`tokio::fs` if RPC
  latency ever demands it). **(b)** `VfsOps::read_all` casts `getattr().size`
  (u64) to u32 — truncates a >4 GiB file; shared-trait, theoretical for CAS.
  **(c)** a `store()` error after staging leaves an orphan staging file (random
  name; wants the same GC as abandoned uploads). **(d)** `remove()` leaves empty
  `objects/<ab>` shard dirs (cosmetic; `readdir` of one returns `[]`). **(e)** a
  drop-order regression test for `SftpClient`'s field ordering (the contract is
  commented but compiler-invisible). **(f)** client-side `spawn_blocking` for the
  blocking `FileStore` cache read in the async resolve (already an app follow-up).
- **`/v/cas/index` TSV — DESIGNED, DEFERRED (2026-07-02, Amy).** The B2
  resolver file (`hash  mime  size  path`, absolute path column, mime from
  `inspect()`) is fully designed in `docs/slash-v.md` but was **not shipped**:
  nothing consumes it (the client resolver addresses objects by exact hash, never
  by reading `index`), and the first-cut shape — regenerate by walking
  `objects/` (O(N) `stat`+`inspect`) on *every* read, no cache — is
  under-designed and would bake a bad ABI. Build it only with (a) a real
  consumer *and* (b) a cache keyed on a pool-version stamp (invalidate on
  store/remove), or a per-shard `index` (256-way) if a single roster gets large.
  `kj cas ls` covers human listing meanwhile.
- **Track V — `/v/ctx` + `/v/session` (redesigned 2026-06-27 — script-first:
  TSV `index` resolver, sharded pools, symlink edges; no `by-id`/`by-time`/
  `live` farms; no writable `bound` — the capability apparatus dissolved into
  per-operation join, SFTP stays read/view).** V0 `content_len` on `BlockHeader`
  (prerequisite, additive CBOR); V1 `/v/ctx` backend (trailing-byte context
  shards, `blocks/index` ordered by `block_ids_ordered()`, `generation` ←
  `DocumentEntry::version()`); V2 `/v/session` over `PeerRegistry` (+ session
  `kind` field; `context` from live `SessionContextMap`, never KV); V3 SFTP
  mounts them read-only. Deferred optimization (V1 ships naive):
  `block_ids_ordered()` re-sorts per call — cache the ordered `Vec<BlockId>`
  keyed on `DocumentEntry::version()`. Open: huge-`content` range-read vs cap.

## Instrument reframing & RC stances (follow-ups from the 2026-06-22 pass)

The pass that reframed kaijutsu as an instrument, rewrote the rc create-stances,
and renamed `composer→musician` / `explorer→toolie` left these threads open:

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

- **Headless render sink (edge-node agent) — MIDI + PCM:** PCM slice 5c-3
  demolished the server's in-process `AlsaMidiOut` + `kj transport render`, so the
  kernel/server binary now links **no** audio/MIDI FFI (goal achieved). The app is
  the render sink today; a **headless kernel with no app attached makes no sound**
  (MIDI is sink-dependent by design — `docs/midi.md`). The remaining gap: a
  headless edge-node agent that attaches over RPC and plays cues (Symphonia/ALSA
  for PCM, ALSA-seq for MIDI) — `midi.md`'s "first kernel-owned compute node" (M4)
  and `pcm.md` slice 4. Reuses the exact wire `RenderCue` the app consumes; the
  speculation-lead `at`→`lead` scheduling already travels with it.
- **SSH shell subsystem (`kaijutsu-shell`):** give an `ssh` user an interactive kaish
  with `kj` that starts in a lobby and attaches into contexts (VFS reflows on switch).
  Design + wiring captured in [`ssh-shell.md`](ssh-shell.md). Start after the SFTP
  read-path work settles (shared subsystem plumbing). Open decisions noted there:
  per-principal home vs shared lobby anchor (copy the `lost+found` `ensure_*` pattern —
  *not* the global-singleton `scratch` context), and whether `Send`-ness lets it run
  SFTP-style or needs the RPC dedicated-thread treatment.
- **VFS facade delegation:** `Kernel` implements `VfsOps` directly (`crates/kaijutsu-kernel/src/kernel.rs:984`) as a facade. Backend multiplexing already exists — `MountTable` impls `VfsOps` over `MemoryBackend`/`LocalBackend` (`crates/kaijutsu-kernel/src/vfs/mount.rs:261`). The open question is whether the `Kernel`-level facade should delegate more to `MountTable` (and what stays on `Kernel`), not whether to build a manager from scratch.
- **Server RPC Modularization:** `crates/kaijutsu-server/src/rpc.rs` is a massive file (~301KB / ~7,000 lines — by far the largest in the server). The monolithic implementation of the Cap'n Proto traits should be split into smaller modules by domain (e.g., `rpc/vfs.rs`, `rpc/llm.rs`, `rpc/mcp.rs`).
- **`context_type` newtype — declined, not deferred (2026-06-28).** The beat
  coupling that motivated it is gone (arm moved into rc; the gate is "has a track
  lane"). Do NOT make `context_type` a closed `enum` or newtype: it names an open
  **rc-bucket directory** (`project_rc_lifecycle`). Live follow-ons are the other
  axes (decouple-Act-from-ABC; per-type `BeatPolicy`), tracked under Hyoushigi.
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
- **RPC session reaping — residual only (mostly closed 2026-06-14).** Keepalive
  reaps dead peers (30s × 3) and the watchdog is activity-gated. Residual (by
  design, low): a *truly* wedged `current_thread` LocalSet can't be force-killed
  from outside, and the in-thread watchdog goes quiet with it — that silence is
  the only remaining signal. Not worth chasing until it actually recurs. Related:
  `tech_debt_peer_reattach_on_reconnect`.
- **LLM providers:**
  - Per-model knobs in the app (server-side config is now cast_slots/backend_models, 2026-08-03; the app renders none of it yet).
  - Push subscriber for `ConversationMailbox`.
  - **`Registry::resolve_model` pins a bare model name on the *default*
    provider** (`llm/mod.rs:721`) — the sharp edge behind the 2026-07-04
    cross-provider distill bug (fixed by routing the distill default around
    it, not by changing `resolve_model`). Audit its remaining callers for the
    same trap.
- **Reasoning-continuity cross-provider guard (policy, not Rust; the rehydration
  machinery itself shipped):** block `kj context set --model` across provider
  families when signed Thinking exists in history (a DeepSeek nonce fed to
  Anthropic 400s); allow the transition only at `fork`, where an rc script
  decides to elide thinking or downgrade it to plain blocks.
## parley opts out of dictionary line breaking for Japanese — check back (2026-08-12, from the parley 0.9 bump)

**Log silenced, behaviour left alone** (Amy: "silence it and we'll track an
issue for ourselves to check back in. I might file an upstream issue if we
find we need it sooner than later"). Revisit when wrapped Japanese body text
starts to matter — the upstream brief is written up below, ready to file.

parley 0.9 line-breaks through `icu_segmenter` (0.7 had **zero** ICU
dependencies), and picks the constructor that loads no dictionary for the
unspaced scripts:

```rust
// parley-0.9.0/src/analysis/mod.rs:56,63,70 — all three word-break modes
LineSegmenter::new_for_non_complex_scripts(opt)
```

Every layout containing Japanese then hits `select()`'s miss arm and warns
`ICU4X data error: No segmentation model for language: ja` — measured at
**~190 lines per 45 seconds** with 会術 on screen.

**Silencing it took two pieces, and the first one is the interesting bit.**
`icu_provider` only calls the real `log` crate when its `logging` feature is
on. With it off — nobody enabled it — `icu_provider::log` is a shim
(`lib.rs:188-221`): `pub use std::eprintln as warn` under `debug_assertions`,
and a **no-op macro** without it. So the warning was a bare `eprintln!`
straight to stderr that no `tracing` filter could ever reach, *and* it never
existed in release builds at all — this was always dev-loop-only noise. The
fix is a direct `icu_provider` dependency in `kaijutsu-app/Cargo.toml` whose
only job is to turn `logging` on via feature unification, which routes the
warning through `log` → tracing-subscriber's bridge, where `main.rs`'s
`EnvFilter` entry `icu_provider=error` drops it (`error`, not `off`, so a real
ICU data failure still surfaces). Verified: ~190 per 45s → **0 per 45s**.

**The trap, recorded because it is the tempting wrong fix:** feature
unification fixes the *logging*, but it cannot fix the *data*.
`icu_segmenter`'s `default = ["compiled_data", "auto"]` while parley takes
`default-features = false, features = ["compiled_data"]`, which looks exactly
like the culprit — but adding `icu_segmenter` with `auto`/`lstm` as our own
direct dependency does **nothing**, because `new_for_non_complex_scripts`
builds `ComplexPayloadsBorrowed::new()`, which sets `ja`/`th`/`km`/`lo`/`my`
to `None` *unconditionally* — no `cfg(feature)` anywhere near it, and the call
sites are `const {}` blocks with no hook. The gate is a call site, not a
feature. Two adjacent problems, one solvable by a feature and one not.

**parley's choice is defensible**, which shapes the upstream ask: the CJ
dictionary is ~5.1 MB of baked data (`segmenter_dictionary_auto_v1`; the SE
Asian dictionaries are another ~5.3 MB). A layout crate adding that to every
binary unconditionally would be worse. So the ask upstream is *"give us a way
to opt in"*, not *"load it by default"*.

**What we actually lose is modest.** UAX #14 still permits breaks between
ideographs, so Japanese does wrap — it just may break mid-word instead of at
dictionary word boundaries, which is close to traditional CJK typesetting
anyway. Today the only standing Japanese is the 会術 title, which never wraps.

Options, when it is worth doing: raise the opt-in with parley upstream (the
honest fix — any parley user rendering CJK hits this), or take line breaking
over ourselves with `icu_segmenter` directly (big: parley owns line breaking
internally and exposes no segmenter hook, so this means displacing it).

## Test leaks a pidfile into `/tmp` (2026-08-12, via kaish's /tmp audit)

`background_exec.rs:1609` builds a pidfile at
`std::env::temp_dir().join("bg-exec-test-childpid-{uuid}")` and never removes
it — one leftover confirmed on zorak from 08-10. Not implicated in the 4.9G
that audit was chasing (that was cc session storage); flagged so it is fixed
at the source rather than misattributed later. The kernel test harness already
has the right pattern — `Kernel::with_temp_cleanup(root)`, which drops a temp
root when the dispatcher drops. Small.

## Drift UX — cross-session ergonomics (2026-08-12)

Design record + full gap list: **`docs/drift-ux.md`**. Slice 1 (`push`
delivers immediately, staging behind `--stage`) SHIPPED `b2cdb770`.
Remaining, in order:

- **cc-\* contexts never deregister, and that breaks addressing. RULED, ready
  to build.** Measured on live zorak 2026-08-12: 289 contexts, 152 `cc-*`,
  **60 sharing the `cc-kaijutsu` prefix**. Drift resolves a label prefix and
  errors `Ambiguous` on >1 match (`ids.rs:311-320`), so
  `kj drift push cc-kaijutsu` is unusable *today* and degrades monotonically.
  `session.end`/`agent.stop` only write text blocks
  (`hook_listener.rs:315-376`).
  **Amy 2026-08-12: `session.end` archives the context; names do not change;
  a one-shot sweep of the resident backlog is authorized.** Archive is not
  trash — archived contexts are retained work kept for referential integrity,
  later search, and research, with indexing already in flight elsewhere. No
  kernel change needed: `archived_at` is already what both resolvers filter
  on. Do **not** rename or suffix on archive; the label leaves the active set
  intact, which keeps it meaningful for the coming index.
- **kaish latches are going away — approvals become ours, bespoke** (Amy,
  2026-08-12: "the kaish latches are going away in the next release. we'll
  rebuild approvals in kaijutsu bespoke, using some of kaish's new tools for
  giving us visibility into a command"). This supersedes the earlier read that
  our only kaish approval-surface exposure was a *change* to `LatchRequest` at
  the 0.14 bump — it is a **removal**, so the work is ours either way.
  Consumption points to replace: `LatchRequest` construction at
  `mcp/servers/shell.rs:774` and the `structured.latch.nonce`/`.hint` batch
  loop (`:462-464`), plus `.latch` reads in `runtime/kj_builtin.rs:456,752`.
  Verbs that are latched today and therefore need the bespoke path before the
  bump: `kj context archive` / `remove` / `retag`, `kj workspace remove`
  (`workspace.rs:306`), `kj preset remove` (`preset.rs:283`).
  Design note worth keeping from using it in anger during the sweep: the
  current nonce is scoped to the **label**, not the id — confirming names what
  it authorizes, and a batch keyed on ids fails loudly with "nonce scope
  mismatch". Keep that property. Amy's steer is that kaish's new
  command-visibility tools are the substrate, not a reimplementation of
  latches.
- **Sweep jobs for trash contexts — LATER, explicitly not now** (Amy,
  2026-08-12: "we can add sweep jobs later to clean up trash contexts, but not
  now"). The one-shot sweep ran; do not build recurring automation for it yet.
- **MCP connections that never receive hook traffic mint a context that never
  stabilizes and never archives** (found 2026-08-12, during the sweep). The
  sweep took `cc-*` from 161 → 26 and the `cc-kaijutsu` prefix from 60 → 12.
  **Ten of the twelve survivors are `cc-kaijutsu-0812-HHMM`** — the
  *pre-stabilization* label form (`main.rs:396`), which means those contexts
  never got a hook event carrying a `session_id`, so `maybe_stabilize_label`
  never renamed them. They came from short-lived `kaijutsu-mcp --connect`
  invocations — diagnostic probes and `/mcp` reconnects — in a single day.
  Two compounding problems: they are minted per connection rather than per
  *session*, and the new `session.end` archiving cannot reclaim them because
  no `session.end` ever fires for a connection that had no session. So this is
  the residual generator the sweep does not close — roughly ten per day for
  one project. Options: don't register until the first hook event proves a
  real session (register lazily), or let a connection that closes without ever
  stabilizing archive its own context on drop. **Self-inflicted note for
  future sessions: probing the kernel via `kaijutsu-mcp --connect` adds to the
  pileup; prefer the already-attached MCP `shell` tool.**
- **`lost+found` has no discovery or working surface** (Amy, 2026-08-12: "we
  need to add some tools for discovering and working with lost+found"). It is
  created lazily by the dead-letter path (`drift.rs:606-680`) and *nothing
  points at it* — no `kj` verb lists it, and a caller whose drift dead-lettered
  gets no pointer. Wants at minimum: a way to see it exists and its depth, a
  way to read what landed there, and a way to re-deliver an entry to its
  intended target. Weight goes up if drift starts carrying musical material —
  a silent `lost+found` is a dropped phrase nobody goes looking for.
- **A received drift is a dead end for reply.** Hydration surfaces the short
  id only (`llm/hydrate.rs:344-357`) — never the sender's label, no thread id,
  no hint that replying is possible. **Not the cheap fix it looks like.**
  Stamping the label on the block is ~65 refs across 14 files plus wire and
  app renderers, *and* it stamps a mutable value — `stabilize_context_label`
  renames `cc-*` contexts, so a stamped label goes stale and displays a wrong
  address as a right one. Resolving at hydration is correct but
  `translate_block` has no DB access and has several callers; it wants a
  label snapshot passed in, not a DB handle held across hydration. Small
  design pass, not a patch.
- **Arriving drift and turns — RULED (Amy, 2026-08-12).** Default stays the
  gentle mailbox drop picked up on the receiver's next turn. `kj drift push
  --drive` *requests* a turn. A per-context setting decides whether drive
  requests are honoured, **defaulting to off** — the receiver-side veto is
  what makes a sender-side flag safe, and it is why this is not the rejected
  "sender declares" shape. Natural home for the setting is the context
  binding/loadout (an ergonomic-nudge capability), not a new concept.
  **Blocked on the rc identity-smear fix below** — a driven turn must not be
  attributed to the sender's principal.
## Drive gates — self vs external, and don't drive the archived (2026-08-12)

Amy asked for the `--drive` default-off idea to be driven into code and docs
generally: it is not drift's question. `kj drift push --drive` is just the
first caller; the beat scheduler and any future orchestration are the same
shape. **Deliberately small — Amy: "I don't want to get crazy with
permissions."** Half of it already exists.

**Self-drive is already gated, and correctly.** `kj drive` requires
`Capability::Drive` on the **caller's** context (`kj/drive.rs:61-64`), with
the intent stated in the code: *"what makes narrowing a musician's binding
actually stop its OODA tick."* So "this context may drive" is solved. Amy's
self-vs-external split maps onto it cleanly:

- **self-drive** (caller == target, e.g. `rc/musician/tick/S10-drive.kai`
  driving its own context) — governed by the existing `Drive` cap. No new
  concept. This also answers the "does the beat scheduler bypass consent?"
  question: it never needed to, because a musician's tick is *self*-drive and
  consent is about *external* drive.
- **external drive** (caller != target) — the genuinely new gate, on the
  **target**. Per-context, default off, with `context_type` defaults via rc:
  musicians on (a player that cannot be woken cannot take a hand-off
  mid-piece), coders probably, everything else off. Home is the context
  binding/loadout — an ergonomic nudge, not a security boundary; a context
  that declines is *focused*, not distrusted.

Amy's "this session cannot be driven, to ensure things stay stopped" then
needs no third mechanism: deny external drive on the target **and** withhold
`Drive` from the context itself, and nothing can start it — one existing knob
plus one new one.

**The archived check — SHIPPED 2026-08-12.** `kj drive` now refuses any
target that is not `Live`, before it publishes a turn request. It had *no*
context-state gate at all, and the archived case was genuinely reachable:
label resolution filters archived rows, but `KernelDb::resolve_context`
parses a **full UUID first** through `get_context`, which has no
`archived_at` filter (`kernel_db.rs:2308-2315`) — so `kj drive <full-uuid>`
drove an archived context. That mattered because archived contexts are
*retained work* kept for referential integrity, later search, and research;
driving one mutates the record we are preserving. `archived_at` is checked
ahead of the enum, per `ContextState`'s own doc comment naming it
authoritative.

Two neighbours came along, because the same missing gate covered them:
`Staging` was **already documented** on `ContextState` as "LLM blocked"
(post-fork curation) with nothing enforcing it, and `Concluded` is refused
for the archived reason one step softer — its documented recovery is `fork`,
not a turn. Each refusal names the state and the way forward. Tests:
`drive_refuses_an_archived_context` (addressed by full UUID, the path that
bypasses the filter), `drive_refuses_a_concluded_context`,
`drive_refuses_a_staging_context`.

**Cold-cache suppression** is the softer companion, and it is Amy's insight:
kaijutsu contexts are designed to be always revivable from durable state,
which is exactly what makes revival look *free* to anything that can request
it. When the provider prompt cache has aged out, the next call reprocesses the
whole conversation as a cache miss. Computable with **no new schema**:
`context_usage.updated_at` is the wallclock of the last *completed LLM call*
(`kernel_db.rs:624-635`) — the right clock, where `contexts.last_activity_at`
reads falsely warm because any block write touches it — measured against the
shortest `cache_breakpoints` TTL (`kj/cache.rs:138-141`; ephemeral ≈5m,
extended ≈1h), with `cache_read_tokens > 0` as corroboration. Conservative
edges on purpose: no breakpoints or no usage row means there is no cache to
lose, so do not suppress — that is an ordinary cold call.

Refusals must be **loud** (a silently-dropped escalation is the
silent-fallback shape CLAUDE.md rejects), and there should be a way to
*insist*, because cold-cache is a cost signal, not a correctness one.

*"KV" in this entry means the model's attention/prompt cache — not the kernel
key-value store demolished 2026-07-04.*
- **rc lifecycle identity smear — blocks a drift rc script writes are
  attributed to the *sender*.** `run_kai_script` materializes the rc kaish
  with `principal = caller.principal_id` (`kj/lifecycle.rs:376,388`) — the
  sender's — while binding the shell to the *target* context. Capabilities
  are fine: they gate on `caller.context_id` (`kj/mod.rs:563-576`), which is
  the target, so authorization runs in the right direction (this is where a
  2026-08-12 GLM review was wrong, and the correction is recorded in
  `drift-ux.md`).

  **Narrowed again 2026-08-12 — `privileged` is NOT smeared.** An earlier
  revision of this entry (and two verbal relays) claimed `privileged` rode in
  from the sender's shell. It does not:
  `materialize_context_kaish_rc` passes `true` unconditionally
  (`kj/context_shell.rs:71-91`), because privilege is a property of *being the
  rc runner*, not of whoever triggered it — and `KjCaller::privileged`'s own
  doc says it is "stamped at `KjBuiltin` construction by the rc runner —
  **never** derived from a shell var" precisely to stop it being forgeable.

  So the smear is exactly one field: **`principal_id`**. An rc script runs
  under the principal of whoever *caused* the lifecycle to fire, so blocks it
  writes into the target context are authored by a foreign principal.
  Harmless while the shipped `drift` rc script only clears prompt cache.
  **Must be resolved before shape B ships an `S50-drive.kai`**, because then a
  whole driven turn would be attributed to the context that requested it
  rather than to the context that ran it.

  **RESOLVED by Amy 2026-08-12** — "can the principal be set for the origin of
  the drift just on the drift block? then the rest of the blocks in the
  context would belong to the context owner imo." That is the right split, and
  it answers the question this entry was stuck on (whose name belongs on work
  done on another's behalf) by separating two things that were conflated:

  - **The drift block carries its origin.** Author it as the *sending*
    principal — that block genuinely came from elsewhere, and provenance is
    what it is for.
  - **Everything else belongs to the context owner.** rc scripts are the
    context doing its own lifecycle work; that they were *triggered* from
    outside does not make the resulting blocks foreign.

  Implementable as-is, no new plumbing: `insert_drift_block_as` already takes
  an explicit `Option<PrincipalId>` (`block_store.rs:3043-3052`) and the plain
  `insert_drift_block` wrapper simply passes `None`
  (`block_store.rs:3020-3040`), so the drift-block half is threading the
  caller's principal through the four `kj/drift.rs` insert sites. The rc half
  is `run_kai_script` taking the target's `ContextRow.created_by` instead of
  `caller.principal_id` (`kj/lifecycle.rs:376,388`). Both want tests pinning
  the authorship, since nothing asserts it today.
- **Drift edge metadata is inconsistent across delivery paths.** Immediate
  push stamps `drift_kind.to_string()` (`"push"`, `kj/drift.rs:335`); flush
  stamps `format!("{kind}#{staged_id}")` (`"push#1"`, `:629`). So
  `kj drift history` cannot uniformly trace an edge back to a staging event.
  Arguably correct as-is — an immediate push *has* no staging event — but the
  two paths should agree on a scheme rather than differ by accident.

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
- **`kj/drift.rs` orchestration bloat:** push/pull/merge/flush each inline
  variations of "insert drift block + record edge + run rc lifecycle".
  Extract the shared operation; the command layer should dispatch, not
  orchestrate.
- **Residual race: `unregister` vs. an in-flight delivery that then
  succeeds.** Fixed 2026-08-12: `DriftRouter::drain` now marks items
  `in_flight` in place (`drift.rs`) instead of removing them into the
  caller's local `Vec`, so `cancel`/`queue` see them during a flush's async
  delivery window and a cancelled item is dropped on `requeue` instead of
  resurrected — see `drift.rs` module docs on `StagedDrift::in_flight` /
  `DriftRouter::{drain,complete,requeue}`. One narrower race survives: if
  `unregister(ctx)` runs *between* `drain` and the delivery's outcome, and
  the target document write actually succeeds despite the context now being
  unregistered, the item is both "delivered" (block landed before teardown)
  and swept to `dead_letter` by `unregister`'s indiscriminate sweep over
  `staging` (which doesn't check `in_flight`) — so it gets written into
  lost+found too, looking like a failure that never happened. Needs
  `unregister` to either skip in-flight items or have `complete`/`requeue`
  check "was this context unregistered out from under me" before deciding
  dead-letter vs. drop. Low priority: requires a context to be destroyed in
  the exact window between drain and the block-store write during that
  context's own outbound or inbound flush.
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

- **Backup shipped 2026-08-03; export/import round-trip still open.**
  `kj db backup <path>` (`KernelDb::vacuum_into`, `VACUUM INTO ?1` bound
  param) and `kj db checkpoint` (wraps `KernelDb::checkpoint()`) landed —
  see docs/architecture/kernel.md "Backup & restore" for the design and the
  stated restore procedure (stop kernel → swap `kernel.db` (+drop
  `-wal`/`-shm`) → start kernel; deliberately not a `kj` verb, since the
  kernel's in-memory state would desync from a live file swap). Still open:
  an export/import that round-trips through decode→encode would rewrite
  every record in the current format, bounding how long at-rest
  compatibility shims have to live (see the frozen-payload test in
  `kaijutsu-types/src/codec.rs`) — today they must live forever, because
  compaction is threshold-triggered and a quiet document may never be
  re-snapshotted.
- **CRDT-owned config/rc (design: `docs/config-crdt-ownership.md`) — slices 1+2
  shipped 2026-06-16/17 and long since exercised live** (`kj rc edit`/`kj config
  set` are the daily surface). Remaining: the deferred CRDT scratch mount.
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

- **HUD graph design: what should the dock sparklines *be*?** (2026-08-12,
  Amy). Rendering is settled — flat triangles in the dock texture, both dock
  and block-cell sparklines on `text::sparkline::build_sparkline_vertices` —
  but the graphs themselves are placeholders: events/sec kernel-wide and
  active-context running blocks, 40 samples at 250ms (a 10s window), no
  labels or thresholds. For the ambient-command-center role, decide data
  source, window, and labeling before polishing pixels further. The
  DockSparkline read-`/run` note above still stands. A `UiMaterial` shader
  (shader-AA lines, `BlockFxMaterial`-style uniform data) is the fallback if
  triangle rendering disappoints at ambient distance.

- **Conversation-view de-vello pass — needs a visual pass in the running
  app** (2026-07-30, `feat/devello`; a validation pass is planned
  separately, this records what to check first). Role-group dividers,
  sparklines, and the image placeholder moved off vello onto a shader
  center-line (`BorderKind::CenterLine`) and plain Bevy UI rectangle
  geometry (`text::sparkline::build_sparkline_geometry`,
  `view::block_render::spawn_segment_child`/`spawn_rect_child`); this pass
  was done without running the app (the runner was down), so unit tests
  cover the pure math but not pixels. Specifically worth eyeballing:
  - ~~Sparkline segment rotation / fill shape~~ — MOOT (2026-08-12): both
    sparkline surfaces moved off UI-node rectangles onto flat triangles in
    the block/dock texture (`build_sparkline_vertices`), with the true
    trapezoid fill; the rotated-segment and bar-tiled-fill code is deleted.
    Both surfaces live-verified 2026-08-12: dock via BRP + Amy's eyes
    (flicker gone), block-cell via a ```sparkline fence in a test context
    (renders correctly first try — likely its first-ever live render).
  - **Role divider label vertical centering + line thickness** —
    `fieldset::ROLE_LABEL_FONT_SIZE`/`ROLE_DIVIDER_THICKNESS` preserve the
    pre-shader Vello values exactly on paper; worth a glance since the
    label now goes through MSDF (a different glyph pipeline) rather than
    Vello's `draw_glyphs`.
  - ABC (`text/abc.rs`) moved off vello too, in the `msdf-music` branch —
    it now renders through MSDF glyphs + `MsdfBlockGeometry`'s flat-colored
    triangles, no vello scene at all. The music-notation merge needs its own
    visual pass — same "unit tests cover the math, not pixels" caveat applies
    to staff lines/beams/slurs/ties and glyph placement. It surfaced two
    merge-only bugs worth knowing about if something regresses further:
    `extract_msdf_blocks`'s render-world query briefly required
    `&MsdfBlockGeometry` unconditionally, which would have silently dropped
    role headers, the shell dock, the compose overlay, the editor surface,
    and time-well cards from MSDF extraction (none of them carry that
    component) — fixed to `Option<&MsdfBlockGeometry>` before it shipped.
    Separately, devello's MSDF rewrite of the `Image` placeholder label
    independently reintroduced the exact `scene_version`-derived
    `MsdfBlockGlyphs.version` bug the `msdf-music` branch had already fixed
    elsewhere (see that component's doc comment) — also fixed before it
    shipped.

- **SVG block rendering off vello — also needs a visual pass** (2026-07-30,
  `feat/svg-cpu`, branched from `feat/devello`; same "no running app during
  the change" caveat as the de-vello pass above). `text/rich.rs`'s
  `RichContentKind::Svg` now carries a parsed `usvg::Tree` instead of a
  pre-rendered vello `Scene`; `view::block_render`'s `Svg` arm rasterizes it
  via `resvg`/`tiny-skia` (`text::svg_raster`) into a straight-alpha RGBA8
  `Image`, uploaded as a child `ImageNode` (same `ContentGeometryChildren`
  despawn/respawn convention as sparkline/image). `vello_svg` is gone from
  `Cargo.toml` entirely. Round-trip pixel tests (`text::svg_raster::tests`)
  cover the
  premultiplied→straight alpha math and a real resvg render of a small
  shape, but not the on-screen result. **First live sighting (2026-08-01)
  found two real defects, both fixed on `fix/svg-cat-sizing`:**
  `fit_svg_to_box` filled the box rather than fitting into it, blowing a
  200x200 cat SVG up 4.74x to 948x948; and block cells spawned with Bevy's
  default `flex_shrink: 1.0`, so the SVG cell — whose content is an
  absolutely-positioned child, giving it a zero automatic minimum size —
  absorbed the whole scroll column's shrink pressure and collapsed to
  `ComputedNode.size = [1896, 0]`, letting its raster paint straight
  through the neighbouring blocks' text. Still worth eyeballing:
  - **Any SVG with `<text>` elements** — `SvgFontDb`'s fontdb still feeds
    `usvg::Options`, unchanged in shape, but this is the first real exercise
    of that path through the new raster (previously vello's own
    `draw_glyphs` rendered usvg's resolved outlines; now resvg/tiny-skia
    does).
  - **HiDPI crispness** — the raster is sized from the block's PHYSICAL
    pixel box (`ComputedNode` × `TextMetrics::scale_factor`) and
    re-rasterized on a DPI-only change (`BlockScene::svg_raster_physical_size`
    staleness check in `build_block_scenes`); confirm on an actual HiDPI
    display or scale-factor change that SVGs stay crisp rather than
    blurring or going stale.
  - **Malformed/unparseable SVG** — unchanged fallback (parse failure logs
    and falls through to the plain-text/markdown path), but worth a manual
    poke with genuinely broken markup to confirm it still reads as "here's
    the raw text," not a blank block.

- **Beat-reference delivery + turn-cadence follow-ups** (deferred from the
  2026-07-15 timestamped-beat-refs fix, merged `0a39718b` + live-verified;
  the arc's story is in the devlog — "The beat learns to carry its own
  clock"):
  - **Delivery head-of-line lane for `block.beat_sync`** (`rpc.rs`
    per-connection forward task, ~2185-2600): one serialized capnp callback
    stream shared with turn output delays refs by seconds during a turn.
    Back-dating makes that harmless for correctness; a dedicated
    low-latency lane matters only if reference latency ever does (live
    tempo ramps mid-turn).
  - **Turn-overlap gate/tuning**: the musician wakeup divisor (32 beats ≈
    16s default) can be shorter than a real turn (~18s on gemma4-e4b), so
    the next OODA iteration spawns before the last finishes — observed
    live, one spawn per wake. No in-flight gate today. A behavior/tuning
    question, not correctness.
  - **`async_broadcast` overflow eviction** can silently drop buffered refs
    on a slow client (warn only); any surviving ref re-locks the phasor, so
    low priority.

- **Score/KJ_HEARD injection is unbounded — a long-lived track drowns every
  musician** (found 2026-07-15 re-establishing the jam): a FRESH musician
  context attached to the morning-old `bassline` track sent **190k tokens**
  on its first turn (47 blocks → 12 messages, so single injected blocks are
  enormous — the track score's committed ABC riding in whole); the original
  `bassline` context had grown to 467k with auto-compaction failing
  (`exceed_context_size_error` on every wake — the turn never runs, the
  track goes silent). Rotation doesn't help: the score outliving the player
  is the DESIGN (docs/tracks.md), so the band view (`KJ_HEARD` / hydration
  of score content) must be **windowed** — recent N phrases, not the whole
  committed log. Decide the window's home (attachment? track policy? the
  musician rc?) and whether drive-path hydration needs the same cap.
  Workaround live today: play on a fresh track (`groove`).
  **2026-07-15 late-day datapoint that sharpens the diagnosis**: `bassline-b`,
  a FRESH context on the YOUNG `groove` track, hit 91k tokens after ~2h of
  16s OODA wakes — so the accumulator is the musician's own conversation
  growing per wake (KJ_HEARD + prompt + response blocks every 16s), not only
  old-score injection. Auto-compaction can't save it: the summarization
  request itself exceeds the model window once past it (the 467k case
  failed exactly there). So the fix needs BOTH a windowed band view AND a
  wake-conversation cap (drop/summarize old wake turns; rotation on a
  token/wake budget rather than phrase count is a candidate). Third fresh
  chair of the day (`bassline-c`) is the standing workaround.

- **`kj drive` on a non-OODA-armed musician silently discards its ABC**
  (cost an hour of verify confusion 2026-07-15): `on_turn_completed`
  refuses to crystallize unless `attachment.ooda_armed` ("not an
  OODA-armed musician we manage") — so `ooda off` + manual `kj drive`
  produces model/text ABC that never reaches the score, no cues, no
  sound, no warning anywhere. Either crystallize driven turns regardless
  of the OODA arm (drive is explicit human intent — arguably MORE
  deserving than an automated wake), or log loudly at the refusal.
  Decide the semantics; the silent path is the bug.

- **Musician create-rc auto-attaches to a label-derived track before an
  explicit `--track` can move it** (bit twice 2026-07-15): `kj context
  create <name> --type musician` runs the create rc, which attaches to
  track `<name>`; a following `kj transport attach --context <name>
  --track <other>` moves the context but leaves a freshly-minted stray
  track `<name>` + score context behind (cleaned up with `kj transport
  delete` both times — tombstones `bassline-b~…`, `bassline-c~…`). Fix
  shape: teach `context create` a `--track` passthrough the create rc
  honors, or make the create rc skip auto-attach when the caller
  will bind explicitly (a `KJ_NO_AUTO_ATTACH` env? a create flag?).

- **Tracker station slice 1: score cells on the grid** (2026-07-15, the
  designed-in seam after slice 0 shipped): rows carry note content read
  from each track's score context (`text/vnd.abc` blocks). Prereq: decide
  the read-a-second-context plumbing — one-shot `get_all_blocks(score_ctx)`
  vs `subscribe_blocks_filtered` + a `SyncedDocument`; `WellTracks.beat_key_of`
  already resolves the ids. Row identity is beat-mod-R with kernel-anchored
  phrase alignment, so cells attach as per-row content children on the same
  `row_offset` math; the column subtree is grouped (header/grid/playhead)
  so cells are an added group, not a restructure. Revisit a per-column
  shader or Vello layer only if room-scale cell text is wanted.
- **Tracker station: Amy eyeball items** (2026-07-15, all Amy-tunable
  consts at the top of `view/tracker/mod.rs` + `palette.rs` "Station E
  contract"): overall grid brightness (rows on `etch`, phrase rows on
  `trough_subtle` after the live-verify swap — dimmer rows may read even
  better), `ROW_SPACING`/`PLAYHEAD_FRAC`/`COL_W_MAX`, dot/glyph sizes, and
  the "TRACKER" title plate seated ABOVE the face (`FACE_H/2 + 44`) sits
  outside the zoomed camera frame so it's effectively invisible — decide:
  move it inside, or delete it (room-scale shows no text by design, and
  you know what you zoomed into). Header abbreviates to `N/PHR` because
  the shared 340×100 plate is single-line-sized; a wider tracker-specific
  plate would fit `/PHRASE`.
  2026-07-11): `room_keyboard`'s Enter dives (`zoomed = Some(TimeWell)`),
  and because the dived-only chain's `run_if(well_zoomed)` is evaluated
  after that same-frame write, `well_keyboard` runs in the SAME frame,
  sees the same `just_pressed(Enter)`, and — `state.selected` persisting
  across dives by design — treats it as a focus-Enter, jumping straight
  to the reading card and skipping the ring-overview stop. The Enter
  analog of the Escape double-fire Slice F hardened (see
  `well_keyboard`'s `.after(room_keyboard)` doc). Pre-existing behavior,
  NOT introduced by the freeze-fix (neither keyboard handler changed);
  only fires when a prior dive left a selection. Fix shape: give
  `well_keyboard` the same freshly-dived guard the Escape fix reasoned
  through — e.g. skip Enter handling on the frame `zoomed` flipped, or
  latch the dive keypress so one press can't be consumed twice. Decide
  first whether "Enter resumes where you were" is accidentally *good*
  (it skips a hop of the skim ladder) — Amy's call before hardening.
- **Shell: message-wall MSDF ticker on a diagonal panel** (seeded
  2026-07-10; this entry's header was restored 2026-07-12 after an edit
  had glued its body onto a neighboring entry): one diagonal octagon
  panel renders MSDF text — messages flowing through (block/drift traffic
  as a scrolling violet ticker, newest line blooms). Design + buildability
  notes in shell.md "Ambient telemetry rules"; rides the existing MSDF
  panel pipeline + event stream. Good next wave after trace-glow ships.
- **Theme: tokenize the remaining compiled-only color families**
  (2026-07-12, follow-up to the color pass): `block_*` conversation text
  colors, `syntax` highlighting, `md_*` markdown, `sparkline_*`,
  `output_*`, and `agent_color_*` exist only in
  `ui/theme.rs::Theme::default` — theme.toml cannot express them, so
  alternate skins (contrib/themes/tokyo-night.toml) can't restyle them.
  Extend ThemeData + the From impl + theme.toml; keep the
  MarkdownColors/SparklineColors mirror tests in step (they pin
  Theme::default's md/sparkline values today).
- **Shell: drift-layer representation — design question** (Amy,
  2026-07-10): the aurora placeholder is PAUSED. shell.md's "air carries
  drift" stands,
  but before building anything decide *what information* rides the air and
  whether the render is aurora arcs at all — Amy is weighing a point cloud
  with behavior responsive to kernel activity ("lots of cool options") over
  a scripted-pretty arc. Revisit with a couple of concrete candidates
  (blocks in flight, mailbox depth, drift routes?) before spawning geometry.
- **Shell: nameplates fade toward tooltip/debug over time** (Amy,
  2026-07-10): labels stay boring on purpose (TRACKER, not RHYTHM GATE) —
  the intent is that as real detail fills the stations in, the engraved
  plates recede: dimmer with familiarity, eventually maybe tooltip-only or a
  debug toggle. Keep this in mind before investing further in plate polish.
- **`specs_text` orphaned by the HUD-melt slice 4 retirement**
  (`time_well/text.rs`): its only caller was the retired HUD East panel;
  `reading_specs_text` (the reading card's own, header-trimmed sibling) is
  the live surface now. Kept `#[allow(dead_code)]` as a tested pure
  primitive per its own doc's note that the track transport line "rides
  along here until timewell Stage 3 gives it a real home on a track
  surface." Decide when that stage lands: give it that home, or delete it
  (and its dedicated tests) if nothing claims it.
- **Patch bay: extract shared wire-geometry helper** (deepseek review,
  2026-07-09): `selected_chord_apex` re-derives the group→seat→angle→chord
  pipeline that `rebuild_patch_scene` also computes (identical today,
  verified). A future edit to one side floats the inspection card off its
  chord. One pure `wire_geometry(snapshot, wire_idx)` helper, both callers.
- **Rename `BlockScene` → `BlockContent`:** the component no longer holds a
  scene (scene + `built_*` live on `VelloUiScene`); it's now pure build-
  bookkeeping (`content_version`/`last_built_version`/`scene_version`/`text`/
  `color`). Name is misleading. Mechanical rename across `block_render.rs`,
  `lifecycle.rs`, `overlay.rs`, `shell_dock.rs`, `render.rs`.
- **Verify one unexercised render surface:** the unfocused-pane summary, the
  one surface on Bevy's native `Text` pipeline (`tiling_reconciler`), needs a
  multi-pane layout to eyeball. All MSDF-only surfaces (including docks,
  role borders, and — since 2026-08-12 — the North/South dock chrome
  itself) verified via build/test; the "Vello-content cell" category this
  entry used to also name is gone entirely along with vello
  (`has_vello_content`/`render_vello_scenes` no longer exist).
- **Vi editor command mode (Slice 3, `docs/vi.md`) — steps 1–3 shipped; open
  remainders:** runner-verify the slice-3 polish (capnp `@6` ⇒ kernel+app
  rebuild+restart; eyeball `:r !cmd` splice, bad-`:cmd` E492 on the strip, `fg`
  from a second window; also the 2026-07-07 error-channel unification —
  dirty-`:q` E37 and a failed `:r` must show on the strip, not vanish);
  **step 4 `:e <path>`** (rebind the session to another
  block) deferred; the Ctrl+Z shell may become a **shadow context** (its own
  design pass; `project_shadow_context_shell` memory).
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
- **MSDF whole-document settle window (residual, after the 2026-07-03 atlas
  fixes `a6734cbf`).** The silent failure modes are gone (atlas grows to 4096,
  terminal failures are loud, the respawn loop is dead), but the *transient*
  is inherent: async glyph generation means a freshly loaded document shows
  partial text for a few frames until the last atlas batch lands and
  re-composites. If it still reads as jank, the polish is presentation-side:
  hold a block's texture (or fade it in) until its first *complete* composite
  — every glyph region present — instead of showing partial bakes.
- **Live-verify the error-block ordering fix (view-order holes, fixed
  2026-07-03, `a47c9a18`).** The three diagnosed mechanisms are fixed with
  unit + headless-App tests (see devlog), but the original "errors pinned at
  the bottom" symptom (2026-06-17, session `019ed674`) was never reproduced
  live before the fix landed. Next time an agentic session produces mid-turn
  error blocks, watch for the new fail-loud `error!` logs from
  `reorder_conversation_children` — if they fire, the upstream
  container-entry gap they point at is the remaining bug to chase.
- **Verify the interrupt ladder actually cancels an in-flight drive**
  (originally observed 2026-06-17 as "triple-Esc doesn't interrupt";
  reframed 2026-07-16 by the input rework: Esc is vi's/PopLevel now,
  interruption is **Ctrl+C**'s job — docs/input.md). The app side fires
  `interrupt_context(ctx, immediate)` on the Ctrl+C ladder; what was never
  confirmed is the kernel side cancelling a mid-drive turn/tool loop
  (rather than only a streaming LLM turn). Next agentic session: Ctrl+C
  twice mid-drive and watch whether the loop actually stops.

## Control Plane & Navigation (kj)

- **kaish 0.13 `--json` migration: kj's per-leaf `json: bool` fields are dead
  code via the live kaish bridge (found + confirmed pre-existing 2026-07-18,
  kaish 0.13 `--json` migration).** kj now adopts kaish 0.13's global `--json`
  (kaish's `finalize_output`/`apply_output_format` render every `ExecResult`
  from `.data`/`.output`/`.latch`; kj's own `render_json_envelope` is gone —
  `runtime/kj_builtin.rs`). Auditing every subcommand's `.data` payload turned
  up a pattern that predates this migration: `doc list`, `config
  list`/`show`, `rc list`/`show`, and `search` each declare their OWN local
  `json: bool` clap field and build a richer JSON *message* when it's true
  (e.g. search's `{matches, total, truncated}` with full match context/lines,
  vs. `.data`'s flat array of block ids). That field can only ever be `true`
  when `KjDispatcher::dispatch()` is called directly (as the per-file unit
  tests do) — `KjBuiltin::execute` has ALWAYS stripped `--json` out of the
  argv it hands to `dispatch()` (needed so leaves that don't declare `json`
  don't reject it as unrecognized), so the richer branch has never fired via
  a real shell/MCP call, before or after this migration. Under kaish's
  `--json` now, these commands emit exactly `.data` (the flat id array),
  which is unchanged from what users already saw under the old
  `render_json_envelope`'s `data` key — so nothing regressed — but the richer
  object (`search`'s match context is the one with real information loss) is
  worth either wiring for real (thread the subcommand's OWN `--json` local
  flag through as `.data` instead of a discarded message, or drop the dead
  branch + local `json` field entirely) next time one of these files is
  touched.
- **`kj config set`/`kj config edit` (write branch) and `kj rc edit`
  (content branch)/`reset`/`rm` have no `.data` on success (found 2026-07-18,
  kaish 0.13 `--json` migration).** They return a plain `KjResult::ok(msg)`.
  Under kaish's `--json`, a text-only success with no `.data`/`.output` wraps
  the human message as a JSON *string* (`"set config 'theme.toml' (7
  bytes)"`), not a structured record — matches kaish's documented contract,
  not a bug, but inconsistent with sibling verbs (`doc create`/`delete`,
  `block append`, `rc show`) that already attach a small record. A
  `{"path": ..., "bytes": N}`-shaped `ok_with_data` would bring these in
  line if a caller ever wants `kj config set ... --json` to be
  machine-parseable beyond "some JSON string came back".
- **`kj db` read window into `kernel.db` — deferred, not built (feedback 2026-07-18,
  DeepSeek tracks-discoverability).** When DeepSeek couldn't find a track-listing
  surface it fell back to sqlite and hit a wall: the kaish sandbox blocks
  `sqlite3`/`python3`/`node`, and there's no `kj db` verb — the DB is a black box
  from inside the kernel. The *track* need is now met by `kj transport list`
  (merged persisted+live roster), so this is no longer urgent. But a read-only
  `kj db tables` / `kj db schema` / `kj db dump <table>` (NOT arbitrary SQL) would
  turn other "the kernel has the answer but won't tell you" moments into a one-liner.
  Shape: reflect over the same `KernelDb` connection, emit `.data` rows; keep it a
  read (no write path) and out of the loadout's `transport`/config gates. Watch the
  standing rule — never hand out raw SQLite; go through a typed `kj`/MCP surface
  ([[feedback_no_direct_kernel_db_access]]).
- **`kj transport restore` — only if delete accidents actually happen**
  (decided 2026-07-15 with the tombstone delete): recovery is sqlite-only by
  design (the one-line UPDATE is in `kj transport delete --help` +
  docs/tracks.md). If someone actually fat-fingers a delete and the sqlite
  path proves annoying, a `restore --track <tombstone-name>` verb is the
  shape (rename back + clear `deleted_at`; refuse if the original name has
  been retaken by a fresh track).
- **`--out` writes bypass the VFS (`kj cas get` + `kj block cat`; gemini-pro
  review 2026-07-04).** Both verbs `std::fs::write` the `--out` path
  (`kj/cas.rs:119`, `kj/block.rs:730` — the new verb deliberately mirrored the
  old one's convention). Not a trust issue (shared-trust kernel) but a
  coherence one: the write lands relative to the *server process* cwd, not the
  shell's VFS cwd, and never hits VFS mounts/caches. Decide once for both:
  route `--out` through `VfsOps::write_all` (needs the block/cas dispatch arms
  async) or document host-side semantics loudly.
- **kaish binder eats a literal `--json` inside trailing var-args** (found
  2026-07-17 while fixing the kj-side strip): a literal `--json` token riding
  a `trailing_var_arg`+`allow_hyphen_values` positional (e.g. `kj drift push
  dst hello --json world`) is pulled into the global `--json` flag by kaish's
  own binder *before* `ToolArgs` is built — the kj-side fix (which now reads
  the structured `args.flags`) can't see it; the token is gone by then.
  kaish-crate work (`~/src/kaish`), same family as the `local` reserved-word
  lexer footgun below.
- **`KjBuiltin` argv/stdin quirks (gemini-pro review 2026-07-04, both
  pre-existing/low):** (b) `wants_stdin_content` promotion means
  a forgotten `--content` on an interactive TTY blocks reading stdin until
  Ctrl+D instead of failing "missing content" — cat-like POSIX behavior, but a
  papercut worth a TTY check if it ever bites; (c) the `{other:?}` fallback
  arm in argv reconstruction (`:499-502`; same pattern in positionals `:450`)
  would Debug-format a future non-Array `Value::Json` into a garbage token —
  no trigger today (deepseek: accepted risk), but the arm should fail loud if
  kaish ever grows a new value shape.
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
- **Autonomous turn runaway guard:** Add a `drive_depth` cap to prevent unbounded fan-out from `--prompt` forks.
- **TurnFlow catch-up for late/reconnecting subscribers:** the *lossy* half is FIXED (2026-08-05, the FlowBus backpressure rework — a live subscriber can no longer miss an event; it is terminated with an explicit signal instead). What remains is the **catch-up** story: a client that subscribes *late*, or reconnects mid-turn, was never a subscriber when the outcome was published, so it still misses `turn.completed`/`turn.failed` and must fall back to reading the block log — which recovers *what* the turn wrote but never *why it stopped* (`TurnStopReason` has no block-log shadow; `EndTurn`, `MaxTokens`, and a soft cancel all leave the same `Done` block behind). Deliberately un-journaled (blocks are the durable record; replaying completions after a restart would announce turns nobody is waiting on), so the fix is a bounded per-context "last outcome" the subscriber reads on attach, not a journal.
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

### kj / MCP ergonomics (UX)

- **Stale rc seed → live contexts keep broken loadouts (detection SHIPPED
  2026-07-04; repair gap remains).** rc is seeded-once, so a live script can
  drift behind its embedded default; the recurring symptom was contexts created
  from a stale `S10-binding.kai` missing newer authorities. The *detection*
  half shipped: `kj rc list` now marks each script in-sync / differs-from-seed
  / no-seed (live body vs `seed_body()`, seed-shape-aware for symlink seeds),
  with per-entry records under a new `--json` flag; `kj rc reset <path>`
  remains the manual pull (live is truth, no auto-overwrite). Remaining gap —
  the worse half: `reset`/`reseed` only fix *future* contexts. A context
  already created from a stale seed keeps its broken loadout and must be
  repaired from a binding-admin context. **No longer structurally blocked**
  (2026-08-11): cold start now seeds a ROOT director with admin + rc-write
  (`crates/kaijutsu-server/src/rpc.rs:2112`), so the authority to repair
  exists — what's missing is the repair path itself, not a context to run it
  from.
- **`local` is a kaish reserved word (like `set`).** `--model local` lexes as
  the `local` builtin keyword → `found ';' expected identifier`. Same class as
  the `set` reserved-word gotcha; quote it (`--model "local"`) or pass the full
  spec. Consider letting reserved words bind as plain args after a flag.
  (kaish-lexer change in `~/src/kaish`, not kaijutsu-side.) NOTE: alias
  *resolution* is now fixed — `kj context create/set --model "local"` expands
  the registry alias entry (then models.toml [model_aliases]) to its concrete `provider/model`
  before storage (`resolve_context_config`, 2026-06-14), so the quoted form
  works end-to-end; only the bare-`local` lexer footgun remains.
- **Turn-loop timeout gaps (residual of the local-model stall, re-triaged
  2026-06-16; the dual-layer watchdog + tool-free player loadout cover the main
  path).** Genuinely unguarded: (a) the `provider.stream()` start `.await`
  (`llm_stream.rs:815`) has retry/backoff but **no explicit timeout** — a provider
  that accepts the connection but never returns the response object leans on
  reqwest's defaults; (b) pre-stream hydration / cache reads have no timeout, so a
  wedge *before* the stream loop emits no terminal event. Fix each with an
  explicit timeout + a regression test that wedges the path and asserts a loud
  `TurnFlow::Failed`. Also worth: per-provider/per-context `default_tools` as the
  norm so players never get `all`; per-model timeout overrides if 30s/300s ever
  prove wrong for a slow local model.
- **External shell-hang fix — one residual verification.** The 2026-06-17
  executor-starvation hang is fixed (`SubscriberHealth` reap tolerance +
  `resubscribe_blocks` + joined-context-scoped subscription; story in devlog).
  The server fix is verified live; the *client-side* scoping + resubscribe (2,3)
  ride in the MCP binary and are covered by `e2e_shell` until a session whose MCP
  binary is rebuilt confirms them in situ. Related: P3 above +
  `project_mcp_synceddocument_sync`.
- **Live-kernel follow-up: any long-lived kernel may still be serving a stale
  mcp-default model id (verified 2026-07-17).** Re-checked the "mcp-context
  default model is an invalid id" bug filed 2026-06-17: the bad ids
  (`…-20250101`, `…-20250929`) don't exist anywhere in tracked source or
  history except this doc's own bug report — `models.toml`'s
  `default_model`/aliases and `DEFAULT_MODEL` have always read the valid
  `claude-haiku-4-5-20251001` since the TOML config was introduced. But
  `/etc/config/models.toml` is CRDT-owned and seeded absent-only
  (`config_seed.rs`, `config_crdt_fs.rs:349-378`) — a kernel whose CRDT
  config predates whatever earlier fix corrected this would still be
  serving the stale value, since a context's model is baked in at creation
  time from the live registry (`rpc.rs` `create_context_inner`) and restarts
  never re-seed an already-present file. **2026-08-03: this whole failure
  class is structurally dead** — models.toml no longer exists; defaults live
  in the `llm_defaults` table. If a stale default ever recurs, the remedy is
  `kj backend default show` / `set` (or `kj backend reseed`), all live-reload.
- **`builtin.file` hardening — remaining (small; the byte→char corruption fix +
  hashline addressing shipped 2026-06-17, story in devlog +
  `project_file_tools_hashline`):** the in-context recovery affordance
  *shipped* 2026-08-01 as `kj diff` (`docs/diff.md` slice 3) — one path diffs
  disk against the CRDT document that owns it, and `--from <seq>` replays the
  journal, so "what did the agent change, and what did it used to say?" is
  answerable in the shell. Still open: (1) the post-write verification reads the
  CRDT cache, not the VFS disk, so a faulty flush is only caught by
  `flush_one`'s own error (documented in `edit.rs`); (2) `FileDocumentCache`
  CRDT-native pass-through (tracked under Persistence & Sync) would let `read`'s
  hashes anchor `/etc/rc` cleanly.
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

- **kaijutsu-abc MidiWriter leaves pitch/velocity unmasked** (gemini review
  fallout, 2026-07-09): `note_on`/`note_on_channel` build raw channel-voice
  bytes without the `& 0x7F` data-byte mask that `kaijutsu-app::midi::click_bytes`
  now applies. Safe today — the app's only caller uses
  `MidiParams::default()` (fixed velocity 80), nothing config-sourced. Mask
  at the writer if `MidiParams` ever becomes config-driven.

- **`hnsw_rs` reverse-edge quirk:** Reverse edges written at neighbour's assigned layer.
- **Embedder: BERT-only I/O contract** (2026-07-12 index review): `OnnxEmbedder`
  hardcodes `input_ids`/`attention_mask`/`token_type_ids` and mean-pools
  `outputs[0]` (`kaijutsu-index/src/embedder.rs`). E5/jina-style models (no
  token_type_ids, CLS pooling, or a ready pooled output) won't load. Growth
  path: introspect `session.inputs` for the input set + a small per-model
  manifest (pooling strategy) beside embedding_config in the kernel db. The `Embedder` trait is the seam;
  nothing structural blocks this.
- **Embedder: serialized CPU-only inference** (2026-07-12): one ONNX session
  behind a `Mutex` with `intra_threads(1)`; no execution-provider plumbing
  despite the GPU box. Live data point: `kj synth all` over 54 real contexts =
  ~8 min wall clock (memory now bounded by embed_batch chunking, but the FLOPs
  are all one thread). When it cracks, the reviewed playbook (gemini
  deliberate 2026-07-12) is two-phase indexing: reserve slot under the
  metadata lock, embed lock-free, re-take the lock, re-verify content_hash,
  write — plus intra_threads / EP selection in `[embedding]`.
- **Index: slot-space vacuum watermark** (gemini deliberate, 2026-07-12):
  slots are monotonic-never-reused by design, so max-slot-ever grows with
  lifetime churn — the embeddings cache is `Vec<Option<…>>` sized by it (~24B
  per dead slot; harmless at human scale, unbounded in principle). When
  warranted: watermark trigger (e.g. `next_slot > 2 × live rows`) → offline
  compaction that renumbers into a fresh generation (new graph + one SQLite
  transaction rewriting slots). Deliberately NOT built now.
- **Index: synthesis child tables lack FK cascades** (gemini deliberate,
  2026-07-12): cleanup is manual transactional DELETEs across three tables;
  correct today, but a future fourth synthesis-adjacent table that someone
  forgets to add to `delete_synthesis_rows` silently leaks ghost rows that
  re-hydrate. `PRAGMA foreign_keys=ON` + `ON DELETE CASCADE` needs a
  table-rebuild migration (SQLite can't add constraints in place) — do it
  next time the schema changes anyway.
- **Index: unopenable index_meta.db disables the index** (deepseek review,
  2026-07-12): if SQLite itself won't open (true corruption), SemanticIndex
  errs → kernel degrades to no-index, and recovery is a manual file delete.
  Arguably should treat unopenable-like-mismatched: wipe + start fresh (it's
  a derived cache). Low likelihood (WAL), low cost to leave.
- **ort 2.0 stable watch** (2026-07-12): pinned `2.0.0-rc.12` (latest; no
  stable 2.0 published). Re-check occasionally; rc-series has broken API
  before. ort arena never shrinks — chunked embed_batch keeps its peak ~1GB.
- **ABC multi-tune files vs blocks:** Split tunes across sibling blocks or stack inside one block.
- **ABC file-header inheritance:** `M:`/`L:`/`Q:` defaults prevent proper inheritance.
- **ABC features:** `I:linebreak`, `m:` macro expansion, `%%` directives, Unicode escapes/fonts.

## Viz substrate (kaijutsu-viz) — plan in `docs/timewell.md` (substrate notes in its appendix; `viz-substrate.md` retired 2026-07-04)

- **Pause gating (suspend activity)** — the `z`/`kj context pause` verb ships
  design-only (2026-07-05, `dcbb75e4`): `paused_at` persists and the card dims,
  but nothing behavioral gates yet. The decided semantics (Amy): a paused
  context receives **no beat/OODA wakeups** (seam: hyoushigi attachment wakeup
  fire) and **rejects turn-starts loudly** with a resume hint (seam: kernel
  turn-start). Both seams are documented on `ContextRow::paused_at`. Do as its
  own slice; decide then whether human submit auto-resumes or fails loud.
- **Ring placement residuals** (explicit-placement review, 2026-07-05; both
  reviewers, accepted-not-fixed): promote's ring-full refusal (and other verb
  errors) reach only the log from the app's fire-and-forget keys — a HUD
  toast/flash slot is wanted; the 10-seat cap check is read-then-write under
  the single KernelDb mutex (atomic enough in-contract — direct DB writers are
  already forbidden); `conclude` RPC accepts Staging contexts (pre-existing,
  probably fine, never decided).

- **Time well evolution — plan is canonical in `docs/timewell.md`.** Staged:
  0 tourniquet + 1 idle-age recency (both SHIPPED 2026-07-03 — Stage 1's app
  half landed as the four-ring carousel, not the terraced spiral; see the doc's
  Status) → 2 stable `0–9` rank slots (kernel-owned, mux semantics) → 3
  `TrackInfo`/`listTracks` + optional-cadence attachment + track decks in the
  well (wire slice SHIPPED 2026-07-04, with the live-state layer: tails,
  beat phasors, track rays — see the doc's Status) → 4 track→context→detail
  progression → 5 event-horizon cutoff + LOD + `/` archive search → 6 polish.
  Individual entries below fold into those stages as they ship.
- **Live tail misses streaming model text (found building the live layer,
  2026-07-04).** `live::tail_line` skips empty inserts because model prose
  streams in via CRDT `BlockTextOps` the well doesn't decode — the HUD South
  tail shows whole-content blocks (prompts, tool calls, score cells, errors)
  while a streaming turn only reads as chatter glow + running rim. Refinement
  candidates: decode ops for the *selected* context only (the conversation
  view already has the machinery), or re-fetch the block head on its
  `Done`/`Error` status flip. Bound whichever lands to the selected card.
- **Track rays don't organize the cards angularly (deferred by design,
  2026-07-04).** Cards seat evenly by recency within their band ring; a
  track's cards ignore the ray's bearing. The follow-up is the haystack
  grammar applied to angle — same-track contexts gravitate toward their
  track's ray (`rays::ray_angle`) within each ring, unattached trailing.
  Needs care against the "predictable motion" bar.
- **In-world ring labels — still TODO, and now cheap.** "ACTIVE" / "RECENT"
  floating at each ring, per `docs/timewell.md` "The bowl, revisited". The old
  pure helpers (`card::band_label_pos`/`band_label_text` + their radius
  offset) were deleted with the labels themselves 2026-07-06 and finally with
  the ring collapse 2026-08-01 — nothing to resurrect, and only two labels to
  write. Wiring is an MSDF panel per ring (`panel::create_msdf_panel`, the
  `HorizonLabel` pattern, which already parks a label in world space), gated
  on font-asset load the same way `text::build_card_scenes` is, and —
  landmine — pass the brush explicitly to `VelloFont::layout`/
  `collect_msdf_glyphs` or the text renders black. Open question first: with
  two rings and the reading card's SPECS `band` line, is a label earning its
  clutter?
- **HDR bloom follow-on:** drive the well cards' SDF rims/pulses to HDR (>1.0)
  so they bloom brightly (`WellCardMaterial` `params`/emissive). (The shared
  single-camera HDR+Bloom fix itself shipped 2026-06-17; devlog.)
- **Card readability:** text is small at the default framing; tune when the
  active view (timewell Stage 6) lands.
- **Edge HUD follow-ups (panels shipped 2026-06-18; devlog):** the mid/lower
  E/W sides are open canvas — candidates for the drift arcs / activity layer or
  a secondary readout; the E specs panel wraps a long model badge (cosmetic).
- **RTT follow-up (rename/split shipped 2026-06-18):** `overlay.rs` /
  `shell_dock.rs` could adopt `create_msdf_panel`/`commit_panel_glyphs` for
  their MSDF surfaces (optional, low).
- **Time-well — deferred UI ideas.** All real, none blocking; parked on purpose
  (see `docs/timewell.md` → Execution notes, "Parked on purpose"):
  - *JOIN dive (mockup 34):* the committing Enter currently just switches
    context + leaves. The cool version continues the camera *through* the focus
    card so it unfolds into the conversation — one continuous focus→enter
    gesture. Polish ideas: fade/dim ring cards while focused; tune focus-card
    size/pos (it's large in the overview).
  - *Clean Running-pulse re-check:* the per-context teal Running rim is
    mechanism-proven (identical shader path as the verified selection/lineage
    rims) but never caught in a clean live screenshot — the earlier attempt was
    blocked by the (now-fixed) MCP-shell hang + a bad mcp default model id. A
    ~5-sec re-check once a working-model turn can be staged.
  - *Drift arcs / particle layer (gap 4):* the bigger drift visualization —
    arcs/particles *between* the source/target cards, not just the per-card
    shimmer already shipped. Needs a new context→context drift-edge *list* wire
    (the per-card shimmer rode the existing staged-queue poll; arcs can't).
- **Horizon dive — front door built, room behind it isn't (2026-08-01).** The
  ring collapse made the event horizon a real place (the accretion disc on the
  room floor) and bound `h` → `Action::ActivateHorizon` in the well, but the
  handler only logs `"horizon dive: not yet built (see docs/horizon-dive.md)"`
  — and **that doc does not exist yet**; the prototype is in flight elsewhere.
  Two things to close: write/land `docs/horizon-dive.md`, and replace the stub
  arm in `time_well::scene::well_keyboard`. This is where Stage 5's
  search-at-the-horizon should surface (`docs/timewell.md`, Stage 5).
- **Two of four terrace centerpiece variants are now unreachable.**
  `assets/shaders/terrace_ring.wgsl` picks a ring's centerpiece with
  `ring_index % N_VARIANTS` (N_VARIANTS = 4: barcode, rosette, moiré dial,
  and the fourth). With two rings only variants 0 and 1 ever draw. Nothing is
  broken — `GLYPH_FORCE` already exists to audition any of them — but the
  pairing is now a *choice* rather than a rotation, and Amy hasn't picked
  which two she wants. (While in there: the material passes `ring_count` as
  `glyph.y` and the shader never reads it — a dead uniform channel that
  predates the collapse.)
- **Horizon sediment arcs.** The "+N" is a bare count. Stage 5's original
  bullet wanted per-track sediment arcs on the disc so the mass reads as
  *whose* — now genuinely cheap, since the disc is a first-class floor
  feature with the well's activity data already flowing into its material
  (`well_rings.wgsl` ripples). Wants Amy's eyes before anyone builds it.
- **Time-well ring-carousel — review findings (2026-07-03, gemini-pro batch +
  deepseek).** The ring-per-band carousel (`band_ring`/`ring_seat_rotated`,
  ring-centric nav, projector spin-to-gate, focus dimming) got a two-model
  review. The safe wins (per-frame change-detection guards on the easing
  systems; dead `card_tilt` multiply gated; stale `ring_seat` gate doc) are
  **applied**. Remaining, recorded not-yet-fixed:
  - *Cuboid face UVs by hardcoded vertex index are fragile* (gemini, medium).
    `card_block_mesh` (`scene.rs`) V-flips the front face as indices `0..4` and
    (since 2026-07-06) sentinels the side faces as `8..24`, which breaks if
    Bevy changes its cuboid vertex order. Robust fix: classify faces by
    `ATTRIBUTE_NORMAL` (front ≈ `[0,0,1]`, sides ⟂ Z) instead of index ranges.
  - *Passive-aging short-circuit* (gemini, design). `sync_time_well` early-exits
    on an empty join diff, so cards don't re-band as wall-clock time passes — a
    context won't drop out of RECENT (past the horizon) on idle alone until
    some *other* diff arrives. Ties to the ring-MEMBERSHIP / coarse auto-decay thread (explicit
    hot-row + coarse decay, see `signoff.md`): the band derivation likely needs a
    coarse timer independent of the block diff.
  - *Spin chaining on rapid reversal* (deepseek, medium → downgraded on code-read).
    `spin_target_to_gate` measures the short path from the accumulated *target*, not
    the eased position, so a very fast direction-reversal could feel like the ring
    keeps going before reversing. **Not a correctness bug** (resting target is the
    gate π, steps are one-card; math verified sound by both models) — a possible
    feel-tuning item only.
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

- **Beat-on-track — remaining stages** (Stages 1–3 M1 shipped 2026-06-29/30;
  story in `docs/tracks.md` + devlog): M2–M4 (input telemetry, drift-modeled
  clock-in, edge node) sequenced in `docs/midi.md`; external-signal clock sources
  (solar/compute-availability) ride the same `ClockSourceKind` seam.
- **MIDI-in follow-ons (deferred by decision 2026-07-06 — score first, perceive
  later; M2 capture design is canonical in `docs/midi.md`):**
    1. **Perception.** Captured cells are data-only and invisible to `KJ_HEARD`
       (`heard_json` filters `ContentType::Abc`; the capture mime projects to
       `Plain`). Candidates when we want musicians/coders to hear the room: a
       `MidiToAbcDeriver` notation sibling at the write barrier (mirror of
       `AbcToMidiDeriver` — keeps `KJ_HEARD` unchanged and notation-pure; costs
       a crude quantized transcriber), extending `heard_json` with a MIDI
       digest, or new heartbeat vars. Plus the fun one: a small system
       whisper into coder contexts when the room is playing ("the band is on —
       eurorack on track X") — `BlockKind::Notification` shaped, never the
       cached system prefix (per the datetime-seed lesson).
    2. **CAS write surface (client→kernel put).** `/v/cas` is read-only by
       construction (`vfs/backends/cas.rs`) and the sftp client has no put;
       `commitCapture`'s `Cas(hash)` payload arm is dormant until this lands.
       Needed at the first heavy payload: audio capture windows, client-recorded
       clips. Two shapes to weigh then: capnp `casPut(bytes)→hash` vs teaching
       the sftp/VFS seam write-with-verify (only content matching its address —
       plain `sftp` could seed objects; but it breaks the backend's
       read-only-by-construction stance deliberately).
    3. **Analysis trackers.** Beat-tracking models (Beat This! et al.) run on
       ring windows as just-another-tracker; note Beat This! is *audio*-native
       (fits the audio2midi mic upstream; MIDI windows need render-to-audio or
       a symbolic tracker). Their tempo/phase/downbeat output is
       `Timebase`-shaped corrections — i.e. a second concrete M3 estimator
       candidate (clocked case: pulse-interval filter; unclocked case: beat
       tracker on what Amy actually played).
    4. **Ear slice-1 residuals** (shipped 2026-07-06, `app/src/midi_in.rs`):
       cuts are wall-clock (4 s) — phrase-aligned cuts want the metronome
       phasor + phrase length app-side (`BeatRef` carries no
       `beats_per_phrase`); a kernel-refused batch is warned-and-dropped, not
       requeued; the commit target is the app's *current* context (an
       explicit per-client `midi_in.toml` — capture context + source
       allowlist, the third `/etc/client` consumer — replaces that when
       ambient-vs-seat needs separating); `played_by` is the shipping caller,
       not per-source lanes (sources ride inside the record). And the
       **third-party-thru echo**: the ear excludes kaijutsu's own clients,
       but a synth/DAW/hardware soft-thru re-emitting the render port's
       output IS an external source the ambient ear subscribes — dirty
       capture today, model-hears-itself feedback once perception lands.
       Fixes when it matters: the `midi_in.toml` source allowlist, and/or
       MIDI echo cancellation (the app knows every event it emitted — the
       cutter can fingerprint-subtract captures matching recently-rendered
       (note, channel, ≈time) before shipping). Also deferred by decision
       (2026-07-06): a **payload size cap** on `commitCapture` (a runaway ear
       could land a giant block in the score context; honest worst case
       today ≈2 MB — a loud refuse-over-N-MB in the RPC handler is the cheap
       nudge), and **filter placement** (`keep_at_ingest` drops `F8` clock
       pulses pre-ring, so the M3 clock observer can't be a ring tracker —
       either move filtering to per-tracker cut time or give the observer a
       pre-ring tap in the capture thread; pick deliberately at M3).
    5. **Estimator re-lock after a tempo step is slow and stall-spammy**
       (observed 2026-07-07): the EMA `ClockEstimator` keys on the ALSA
       address, so a restarted master at the same client:port inherits the
       old regime's state — a 540→100 BPM step took minutes of convergence
       with "stall observed" warns at ~2 Hz the whole way. A stall episode
       is strong evidence the source restarted: use it to reseed (or widen
       alpha on) the estimator instead of easing out of stale state. Real
       case: a player switching/restarting master clocks mid-session.
- **Relative-lead timing — open findings from the 2026-07-02 analysis** (the
  substrate verdict + resolved findings live in `docs/midi.md` "The relative-lead
  timebase, analyzed" and `docs/pcm.md`; phase-align 2026-07-15 closed two more:
  the `now + period` re-arm random walk — grid is scheduled-periodic now — and
  the capture-`now`-close-to-the-send gap in `publish_render_cues`. This is the
  still-open remainder):
    2. **Bevy has no audio-scheduling primitive** — the real PCM build risk.
       `AudioPlayer` plays on spawn (`audio.rs` ignores nonzero `cue.lead`);
       honoring `lead` for samples is net-new substrate (delayed-spawn at ~16ms
       frame granularity, or pierce to the `rodio` Sink — `docs/pcm.md` R5,
       open decision 3). MIDI delegates sub-ms timing to the ALSA seq queue.
    4. **Multi-sink flam + whole-queue flush** (`midi.rs` flushes the *whole*
       ALSA queue regardless of track) — future; per-track flush + shared-clock
       scheduling are the eventual answer.
    5. **PLL failure modes to design against** when the modeled clock lands
       (deepseek): starvation drift (ref rate must bound drift < ~1ms),
       tempo-step slew limit, phase-slew-not-step, reference-jitter outlier
       rejection. The absolute-tick-through-PLL shape is the *upgrade path*,
       reached for only if the metronome test shows per-cue boundary jitter
       audibly pulling away from the visual playhead.
- **Metronome — configurable + silence-when-idle SHIPPED 2026-07-05; residuals
  open.** The core asks landed: silence-when-idle (`3fdf1045`,
  `halt_on_connection_loss` resets the phasor on any non-`Connected` status — no
  more free-running onto a wired synth after a kernel restart) and the
  configurable click (`feat/metronome-config` merge: note/channel/velocity/gate/
  enabled from a per-client `/etc/client/metronome.toml`, cascade + app apply).
  **Still open:**
    - **Downbeat accent** — a different note on bar-one needs meter info the
      `BeatRef` doesn't carry yet.
    - **Write ergonomics** — `--global` flag + caller-scoped write default (so a
      client tweaks its own `/etc/client/<id>/…` without spelling the id); needs
      `kj` to resolve the caller's client-id, the same MCP/headless durable-id
      prereq (`docs/config-crdt-ownership.md` "Per-client config" → Open).
    - **Config-change push** — the app applies `metronome.toml` once per
      (re)connect; a live `kj config set` doesn't reach it without a reconnect.
- **Metronome controller — graduate to PI/PID later.** The slosh was fixed
  (`d2b1f55c`, P-phase correction with feedforward tempo — diagnosis in
  `79c4b6b5`'s message). Remaining: graduate to a full PI/PID (damping + integral
  for steady-state) when a modeled/remote clock (M3) introduces real drift
  feedforward can't cancel; add a phasor-slew metric (correction magnitude per
  reference) to quantify — pairs with the OTel-metrics note.
- **Musician loadout is tool-free by design (2026-06-13)** — a player is an
  ABC-only voice; a small local model handed the full palette stalls the turn.
  Open migration note: the gig (key/tune/register) belongs to the stance +
  producer chart, NOT the base rc — migrate any song-specific primer content to
  the producer/chart layer when it lands ("big models author vocabularies").
- **No chart is seeded into a player's context — the gig metadata gap (found
  2026-06-30, standing up a bass player for the Chameleon line).** The
  musician stance + ABC primer (`musician/create/S00-stance.md`, `S15-abc-primer.md`)
  both say "your chair, key, tune, and register come from your stance and the
  chart the producer has set" — but **there is no chart**. A search of every
  document finds the Chameleon spec (B♭ Dorian, B♭m7–E♭7 vamp, bass chair)
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
  the score context (restart-safe by construction, `tracks.md` § Restart
  contract).
  **Deliberately deferred** (Amy's call): an automatic cold-start sweep that
  re-attaches every persisted attachment on boot; the natural seam is the
  recovery loop in `rpc.rs`, and it must run *after* the beat scheduler is
  wired. Adjacent to `tech_debt_peer_reattach_on_reconnect`.
  - **Follow-ups:** (a) `beat_count`/`KJ_PULSE` are NOT persisted — documented
    as the contract (`tracks.md` § Restart contract); persist them
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
- **PCM review findings — open remainder (gemini-pro batch 2026-07-01; the FIXED
  and verified-not-real verdicts are in devlog/git):**
  - **Encoded byte-churn — deprioritized on purpose (Amy):** the fix is
    architectural (route bulk through CAS — the slice-5 convergence), not an
    `Arc<[u8]>` micro-opt; revisit `Arc` only if a real tiny-sample hot path
    shows churn.
  - **`kj play` requires an ambient context — MINOR.** Falling back to
    `ContextId::nil()` (which `on_play_audio` tolerates) would let a truly
    context-less caller broadcast. Design nicety.
  - **capnp union default — NOTE.** The lowest-`@` arm is the default
    discriminant, so a malformed cue decodes as empty-inline → sink EOFs on 0
    bytes (logged, benign). Document if another arm is ever added.
  - A `directive_id` nonce + client LRU dedupe is a reasonable *future*
    idempotency guard if one client ever fans into many subscriptions.
  - `from_path_extension` uses `rsplit_once('.')`; `Path::extension()` is more
    idiomatic (edge case already fails loud).
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
- **Player spawn / rotation — open remainders** (mechanism shipped; current
  design in `docs/chameleon.md` § Rotation, chronology in devlog). Residual
  narrow race: a rotate rc already in flight ends in `kj transport play` and
  could restart a just-stopped track — add a scheduler-side halt check if it
  ever bites. Still open:
  - **Rotate chains pollute the director's context tree (found ~2026-06-29, DS
    Director `019f14ba`; the entry's original "2026-07-15" was an in-app
    hallucinated date, corrected 2026-07-03 — see Context time awareness).**
    Every page-turn is a thin `spawn` fork, so a song
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
  page-turns by design (`tracks.md`, the per-track score context). The durable
  record already lives
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
- **Clip cells — R1+R2+R3+R5 LANDED 2026-07-16** (`docs/pcm.md` "The
  remaining work" is the map; research record `docs/cue-prior-art.md`).
  Still open:
    - **R4 prepare horizon** — the prepare directive at commit + the
      skip-loud late gate (interim: a late CAS resolve fires late, which is
      right for `kj play --cas` but wrong for a musically-placed clip).
    - **Attach-time rehydration is notation-only** (`beat.rs` rehydrate
      filters `ContentType::Abc`): after a kernel restart the in-memory
      committed log drops past *clip* cells (the score context keeps them
      durably; `UseLastGood`'s notation-purity is unaffected — clips carry
      `Skip`). Matters only if something later reads the committed log for
      historical clips; fold clip-aware rehydration in then.
    - **Slice 4 edge-node sink** (midi.md M4) and the bevy full
      feature-enumeration (MUST land before any bevy upgrade — the
      two-rodio/two-cpal device fight, pcm.md polish list).
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

## config-shadow cache: residual cross-alias staleness (found 2026-06-24; common case fixed)

Invalidation after a direct config write is by the written/opened path only
(`Kernel::invalidate_config_file_cache`, the fixed common case), so writing one
symlink alias and reading another stays stale until cache eviction — e.g.
`kj rc reset lib/S20` then `cat coder/S20` (coder→lib). Cosmetic (cat path
only), self-heals on LRU/TTL. A full fix needs alias-aware invalidation
(forward-resolve the written path to its terminal *and* reverse-scan symlinks
that point at it) — deferred.

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

## FSN world — `Vfs.snapshot` stage-0/1 known gaps (landed 2026-07-12, Lane B)

`kaijutsu.capnp` `Vfs.snapshot` + `MountTable::snapshot`
(`crates/kaijutsu-kernel/src/vfs/mount.rs`) shipped the recursive-listing +
generation-stamp plumbing from `docs/scenes/vfs.md` stage 0/1. Two
deliberately-scoped simplifications, documented in the method's own doc
comment, tracked here for stage 2+:

- **Generation blind spot to non-VFS-mediated writes.** Listing-generation
  bumps happen at the `MountTable` chokepoint (create/mkdir/unlink/rmdir/
  rename/symlink/link). An external process writing directly into a
  `LocalBackend`-backed host path — `cargo build` populating `target/`, a
  human `vim`-ing a file outside the app — never touches `MountTable`, so the
  generation counter doesn't bump even though the real directory listing
  changed. `snapshot`'s own `readdir` still sees the real, current listing
  (it's not stale content) — only the *generation stamp* lags, which matters
  once a client starts caching listings keyed on generation (stage 2). Closes
  when inotify lands (`docs/scenes/vfs.md` stage 2: `IN_Q_OVERFLOW` →
  rescan-and-bump covers this exact case).
- **`ignored` gitignore classification is best-effort, not git-exact.** Two
  gaps in `MountTable::ignore_stack_matches` / `build_ignore_level`: (1)
  closest-directory-wins folding across `.gitignore` levels approximates but
  isn't identical to git's precise cross-file cumulative precedence (a
  negation in a shallower file cannot override an ignore decided by a deeper
  one — the dominant real-world case, but not literally correct in every
  edge case); (2) only `.gitignore` files at-or-below the snapshot root are
  consulted — an ancestor `.gitignore` *above* the requested root path is
  never read, so `kj vfs snapshot /mnt/project/src` won't see a pattern that
  only lives in `/mnt/project/.gitignore`'s parent-relative form if `src`
  itself isn't the walk root. Both are fine for slice-0 (`ignored` is display
  metadata, never a filter — a wrong classification never hides data), but a
  real Lane C world render leaning on `ignored` for visual treatment should
  know it's approximate.

Neither gap blocks Lane C (the Bevy world renderer): the snapshot tree itself
is always structurally correct (real listings, real attrs); only the
generation staleness signal and the ignored-styling hint have known slop.

## Archive-time summaries, written by a local model (Amy, 2026-08-03)

When a context is archived it stops changing. That makes archiving the natural
moment to generate a small summary once and keep it forever — no invalidation
problem, because the thing it summarises is frozen by definition. Good work for
a local model: it is not latency-sensitive, it happens on an explicit event
rather than a hot path, and it never needs to be redone.

Two payoffs. Browsing archived contexts stops being a list of labels you have
to remember the meaning of — a card can say what the context *was about*. And
any search over the archive gets real prose to match on instead of a title plus
keywords; the horizon dive's ranker (`docs/horizon-dive.md`, "Where a real
search plugs in") would benefit directly, as would `SemanticIndex` when it
lands behind it.

Open: where the summary lives (a field on the context handle vs. a block in the
context itself), which local model, and whether concluding/demoting should get
the same treatment or only full archival. Not needed for the dive's v1 — that
slice ranks on `label` + `keywords`, which exist today.

## kaijutsu-mcp — June 2026 SyncedDocument migration review

Surfaced by a DeepSeek (concurrency) + Gemini (architecture) review of commit
`ac5f518` (Remote backend cut over to `kaijutsu_client::SyncedDocument`). The
dropped-stdout bug and the content/exit_code completion race are fixed (poll now
does an authoritative `get_context_sync` read after terminal status); these are
the *remaining* findings, triaged.

- **Sole-writer command channel SHIPPED 2026-07-17** (`doc_task.rs`; the old
  HIGH hook-authoring-vs-resync entry is RESOLVED — sole writer, dedicated
  pushed frontier, flush→apply window closed by construction, resyncs
  coalesce, flush-failure aborts the swap). Remaining follow-ups from its
  kaibo review, all accepted/deferred:
  - **Hook-ack latency under a long fetch (watch item):** AuthorBlocks
    queues FIFO behind an in-flight resync fetch (up to the 30s RPC
    timeout), and the Claude Code adapter hook timeout is 5s — the adapter
    can move on before the ack; the block still lands afterward. Latency
    regression only, mirror is ambient by design. If it bites: a
    hook-backpressure test + possibly acking after local insert before push.
  - **`ContextResynced` events ride `ApplyEvent` and are ignored**; the
    bridge could convert them into a direct apply_sync_state and save one
    fetch on reconnect. Optimization, unclaimed.
  - **`pending_events` on FullSync are CLEARED, not replayed** (decided:
    replay is unsafe — header setters stamp fresh local ticks, so a stale
    replay would overwrite newer data; comment on `apply_sync_state`).
  - **resubscribe-acks-before-subscription-active** leaves a narrow
    incremental-event gap; the stall resync covers it for shell state.
- **LOW — `renameContext` RPC has no structured result channel.** The 2026-07-17
  server-side handler (`kaijutsu-server/src/rpc.rs`) returns errors via
  `Promise::err` because `renameContext @29` declares no results — a caller can't
  distinguish "label taken" from "connection broken" (`conclude`/`promoteContext`
  use `(success, error)` result fields). Fine while the only caller (hook
  listener's session-suffix rename) just logs; add `-> (success :Bool, error
  :Text)` if a caller ever needs to react. (kaibo deepseek review 2026-07-17.)
- **LOW — `agent.stop` transcript read is unbounded.** `HookListener` reads the
  whole transcript JSONL (`tokio::fs::read_to_string`) to extract the last
  assistant message; long sessions reach tens of MB. Truncation applies only
  after extraction. Cap the read or reverse-scan from the tail. (kaibo deepseek
  review 2026-07-17.)
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
- **LOW — `InvokePeerRequest.params` generates an untyped MCP schema property**
  (found 2026-07-17 while closing the double-encoding entry): the field is
  `serde_json::Value`, so the derived tool schema gives `params` no `type` at
  all — which is likely why calling layers stringify objects into it (the
  double-encode `normalize_peer_params` now tolerates). Upstream fix shape:
  annotate the schema with `"type": "object"` so callers encode a real nested
  object in the first place.
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
- **`vfs::backends::local::tests::test_normal_paths_succeed` is flaky under
  full-workspace parallelism** (found 2026-08-02 verifying the compaction
  removal). It failed once in a `cargo test --workspace` run, then passed in
  isolation and in two full `-p kaijutsu-kernel --lib` runs (worktree and
  main). So it's order- or parallelism-dependent, not a real regression —
  most likely shared temp-dir or process-CWD state between tests. Worth
  chasing: a test that fails only sometimes trains us to ignore red, which
  is the expensive failure mode. Note also that `cargo test … | tail` hides
  this — the pipe's exit status is `tail`'s, so the run reads as passing.
- **`kj` help-doc siblings: no consumer, unaudited.** `kj.md`'s command table
  was regenerated from the real clap tree 2026-07-17 (cleanup batch); the six
  siblings (`kj-cache/context/drift/fork/preset/workspace.md`) still have
  **no consumer** (only a doc-comment mention of `kj-cache.md`) and predate
  the clap migration. Decide their fate: delete, or wire as `kj <cmd> help`
  bodies — and audit against the clap tree first (start with `kj-context.md`
  + `kj-fork.md`; those commands gained the most verbs). NB `docs/kj-help`
  is a symlink into that dir — not a docs-cleanup candidate.
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

**CRDT data model:**
- `calc_order_key` calls `block_ids_ordered()` (O(N) sort) on **every** insert
  (`kaijutsu-crdt/src/block_store.rs:390`); the bench exposing it is `#[ignore]`d.
- Tombstones aren't a first-class `BlockSnapshot` field — they ride a side
  `deleted_blocks` list re-applied by hand (`block_store.rs:1637`).
- `StoreSnapshot` has a breaking-format note with no migration path ("delete
  existing databases when upgrading", `block_store.rs:1680`).

**`kj` single-source guarantee is manual** — `dispatch()` routing and
`kj_command()` schema tree must be hand-kept in sync; a subcommand added to one
but not the other is unreflectable (`kaijutsu-kernel/src/kj/mod.rs:589`).

**Types-crate layering** — `ThemeData` (~60 visual fields + `include_str!` of
`assets/defaults/theme.toml`) lives in the foundational `kaijutsu-types`
(`theme.rs:59`). Belongs in a UI/config crate.

**`kaijutsu-index`:**
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

## Cache & cost — decided direction (2026-06-24)

*(Promoted from the Gemini CLI survey when that wishlist moved to
`docs/wishlist-gemini-cli.md` on 2026-08-11 — unlike the survey, this is a
locked decision with concrete remaining work.)*

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
    usage capture for the EMA); side effects = the script calling builtins (drift, VFS),
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

**Remaining work (not yet code; the `HookPhase`→`McpHookPhase` rename already
shipped 2026-06-24, freeing the sibling enum):**
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

Three-model holistic audit; 14+ bugs fixed TDD across two rounds (lists in
devlog/git — suite 320 → 336 green).

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

Audit of engrave/layout.rs; the fix rounds shipped in `d722f492`/`8fb17d87`
(lists in git). Remaining, ranked; delete when shipped. (Most are IR-assertable
in tests/engrave_tests.rs.)

**Still open:**
- **MED — `K: middle=<pitch>` ignored** (only the per-clef default middle line is used).
- **LOW — grace notes use the regular notehead glyph**, not the SMuFL small notehead.
- **LOW — every `SourceSpan` is hardcoded `(0,0)`**, so click-to-edit span attrs are dead.
- **POLISH — title text can overlap a tuplet bracket** when the first group is near the start
  (title baseline ≈ bracket y); nudge the title up or the bracket down.
- ~~MED — redundant key-sig accidentals~~ — VERIFIED NOT A BUG: the parser doesn't stamp
  key-sig accidentals onto `note.accidental`, so `K:G FFFF` draws exactly 1 sharp. (False positive.)

---

## kaish PATH / external binary access

Observed 2026-07-02 during Music Demo #1 (`019f249d`): kaish has no `$PATH` and
won't run binaries by absolute path (`/usr/bin/aconnect`, `/usr/bin/pw-cli` etc. all
hit "command not found"). `export PATH=...` is rejected as "undefined variable".
`which` is also absent. Only binaries in kaish's built-in command set are reachable.

Practical consequence: any shell step that needs a system tool (ALSA `aconnect`,
PipeWire `pw-cli`/`wpctl`, `which`, etc.) silently fails with no obvious
workaround from inside an agent turn. We had to ask the user to run `aconnect 128:0
129:0` manually to wire the app's render port to TiMidity.

**Diagnosed + FIXED (slice 1) 2026-07-03 — it was never PATH; external exec was
compiled out three layers deep.** Full design + direction now canonical in
`docs/mounts.md` (the "opaque host" inversion: drop the host-root mount, curate
PATH-dir bin mounts per context_type, VFS-mediated resolution upstream in kaish).
Slice 1 shipped: `subprocess` feature on; `ExternalExec` deny-by-default policy at
materialization gated on the new `exec` loadout authority (coder/mcp/default +
director seeds grant it; musician/toolie never); `MountBackend::resolve_real_path`
implemented (sync mount-table walk + `VfsOps::real_root`); `$PATH` seeded from the
kernel process env into exec-granted shells.

**Open remainder:**

- **Pre-slice contexts need a one-time `kj binding allow exec`** from a
  binding-admin context. The deploy latch itself is DONE (2026-07-03: both
  S10-binding rc seeds reset, kaijutsu-server rebuilt + restarted, verified
  live incl. re-making the aconnect wire from a context shell) — but rc fires
  only at lifecycle boundaries, so contexts created before slice 1 keep their
  exec-less loadout until re-created or manually widened.
- **`kj audio` / `kj midi` verbs still worth having** for the ALSA wiring
  operations (connect, disconnect, list-clients): the wire is kernel-owned state,
  not a shell errand, and the musician-adjacent flow shouldn't need raw
  `aconnect` even with exec working. Related: nothing owns the
  `aconnect 128:0 129:0` app→TiMidity wire; it dies on every app restart (the
  app auto-connecting its render port when TiMidity is present is the likely
  home).
- ~~Unknown-command 300 s hang~~ — **CLOSED 2026-07-04, dispatch proven
  bounded.** The fall-through path (kaish → `call_tool` → broker →
  `ToolNotFound` → 127) has no unbounded await — verified by unit tests in all
  three shell flavors (deny / read-only / exec-granted, each traversing the
  full builtin broker set), a kaibo cross-model audit, and a live-kernel probe
  (bare `mount` ≈ 300 ms). The original "git fast / mount hang" contrast was
  cross-regime: pre-subprocess `git` fast-failed 127; post-subprocess `mount`
  spawns the real binary (bounded by the shell request timeout). Regression
  tests now lock the fast-fail invariant; the likely culprit for the observed
  300 s was the known stale-FlowBus MCP observation gap, not execution.
- **kaish `resolve_in_path` does synchronous `std::fs` stats on the tokio
  worker** for each `$PATH` dir when a name misses early — fine normally, but
  a `$PATH` entry on a hung filesystem would block a worker thread.
  (kaish-crate concern, `~/src/kaish`; found 2026-07-04 during the
  unknown-command investigation.)
- **Later slices** (bin-mount catalog, VFS-mediated resolution, dropping the
  host-root mount): `docs/mounts.md`, coordinated with the kaish mounts release.
- **`kj context list` registry/DB divergence — narrowed and partly shipped
  (2026-08-04, `register_session` upsert work).** Re-investigated while
  building `register_session`'s upsert/attach fix (was going to "heal the
  registry on attach"). Findings against current code:
  - `create_shared_kernel`'s boot-time recovery step (`rpc.rs`, "Recover
    contexts") already re-registers every context `KernelDb::list_active_contexts`
    returns into the DriftRouter on EVERY kernel start — confirmed with a
    same-process double-boot test
    (`list_contexts_recovers_live_context_after_restart`,
    `crates/kaijutsu-server/tests/context_label_resolve.rs`). So the
    original symptom described here (a live/concluded MCP context surviving
    a restart but vanishing from `kj context list`) does NOT reproduce
    against current code — it looks stale, possibly already fixed
    incidentally by unrelated work landed since 2026-07-03, or the original
    live observation involved something the synthetic restart here doesn't
    capture (e.g. a torn/non-graceful shutdown, or two server processes
    briefly both live). Flagging rather than silently deleting, per 改善 —
    if the symptom recurs, treat it as a genuinely different bug, not this
    one.
  - The one real, provable registry gap: `list_active_contexts` filters
    `WHERE archived_at IS NULL`, so an ARCHIVED context's DriftRouter entry
    does NOT survive a restart even though its KernelDb row and BlockStore
    document do. **Shipped**: `joinContext` (`rpc.rs`'s `ensure_context_joinable`)
    now heals this — re-registers from the durable KernelDb row instead of
    hard-failing with "use createContext first" — and a passing regression
    test covers it end-to-end over the wire
    (`join_context_heals_registry_for_an_archived_context_after_restart`,
    same test file): archived-context join fails before the fix, succeeds
    and reappears in `listContexts` after.

---

## `ExecResult.output` can't carry structured data past kaish's output limiter (found 2026-07-18, rich_json wire-through)

`kaish-types::ExecResult::materialize()` (`result.rs`, invoked from
`spill_if_needed` whenever `ctx.output_limit.is_enabled()` — true for every
`EmbeddedKaish`, which always runs `OutputLimitConfig::agent()`)
unconditionally clears `.output` at the end of the function, even when `.out`
already carries independent text and `materialize()` therefore never actually
consumed `.output` to build it. So `.output` cannot be used as an independent
structured side-channel alongside a human-readable `.out` message — any
builtin that sets both (as `kj` commands do: `message` for `.out`, `data` for
structured payloads) has `.output` silently dropped before
`execute_with_options` returns to the caller. Confirmed live: setting
`.output` in `kj_builtin.rs`'s `KjResult::Ok` arm produced `ExecResult
{ output: None, .. }` by the time kaish-kernel's `EmbeddedKaish` handed the
result back (regression-pinned by
`kj_output_channel_does_not_survive_the_kaish_output_limiter` in
`crates/kaijutsu-kernel/src/runtime/kj_builtin.rs`).

**Current workaround (shipped):** `kj` keeps writing only `.data` (the kaish
`$()`/for-loop sideband, which does survive materialize/spill_if_needed
intact). `crates/kaijutsu-server/src/rpc.rs`'s `block_output_data` bridges
`.data` → a rich_json-only `OutputData` at the block-persistence seam in
`execute_shell_command`, so the structured payload still reaches the block
(→ MCP `shell` tool `data`, → the app's `block.output`) even though `.output`
itself never carries it through the shell layer.

**Cleaner fix (not done):** only clear `.output` in `materialize()` when it
was actually consumed to populate `.out` (i.e. move `self.output = None;`
inside the `if .out.is_empty()` branch, or add a distinct "detach" method for
`spill_if_needed`'s disk-spill call site). Lives in the sibling `~/src/kaish`
project (published as `kaish-kernel`/`kaish-types` 0.12.0), needs a version
bump + `Cargo.lock` update in kaijutsu — worth doing if more callers want an
`.output`-native structured channel independent of `.out`.

---

## Context time awareness — per-type date/time injection (found 2026-07-03; slice 1 SHIPPED 2026-07-04)

In-app contexts had no wall-clock source, so models hallucinated dates in
durable artifacts (three incidents — the third being the 2026-07-04 issues.md
ghost re-introducing an already-corrected date).

**Slice 1 SHIPPED 2026-07-04:** `lib/{create/S25,fork/S40}-datetime.kai` rc
seeds (kaish's chrono-backed `date` builtin → `kj block create --kind
notification`), symlinked init.d-style into coder/director/mcp/default;
musician/toolie deliberately get none (musical time is their only time base).
`BlockKind::Notification` was the load-bearing choice: it hydrates as an
appended user-role message and is never swept into the system prompt — a
`(Role::System, BlockKind::Text)` block would be folded into the cached prefix
by `extract_system_prompt_sections` on every call and silently invalidate the
`--target=system` breakpoint daily (the exact anti-pattern the cache-placement
rules forbid; rc `.kai` stdout was also ruled out — it lands as model-hidden
`Trace`). Tests pin the mechanism: visible in hydrate, absent from
system-prompt sections, per-type policy matrix, fork re-seeds.

**Remaining — cadence (slice 2, not-now):** regular re-seeding (director's
"note when the turn gap crosses a threshold / every N turns") wants the
`BeforeModelTurn` hook seam (Turn Loop section) once it lands; per-turn drip
stays out of the cached prefix by the same placement rule.
