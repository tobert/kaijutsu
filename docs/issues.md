# Open Issues

Live work items distilled from prior design and TODO docs, plus architectural observations from code reviews. Code is truth; this exists to track what's *not* in the code yet.

Organized by area. Keep entries terse — link to file:line when a pointer makes the work concrete. When an item ships, delete the entry — if the "how we got here" is worth keeping, move the narrative to [`devlog.md`](devlog.md) (the landed-work story). See the three-file working-notes pattern in `CLAUDE.md`.

---

## Found during the 2026-09-08 sweep (lead-verified)

- **A `*` binding gets no tool-diff notifications.**
  `binding_visible_tool_pairs` (`mcp/broker.rs:1107`) walks
  `candidate_instances()` while `list_visible_tools` (`:1455`) expands
  `all_instances`; a context bound to every instance sees the tools but never
  the `ToolAdded`/`ToolRemoved` blocks.
- **`background_exec.rs` and CLAUDE.md disagree.** The module header says
  kaish's job system is not reusable here and treats the migration as
  rejected; CLAUDE.md "Host exec has one owner" still names it as the
  ad-hoc exec site being retired. One of them is wrong; decide which.
- **`docs/hooks.md` does not exist** but `kaijutsu-mcp/src/hook_types.rs:3`
  cites it as the schema. Either write the page or point the comment at
  `docs/cc-peer.md`.
- **Code comments name issues.md entries that no longer exist** (`rg
  'docs/issues.md' crates` lists ~30; "latch nonce on stderr", "MCP shell
  delay", "msdfgen-rs" were dead before this sweep). CLAUDE.md says comments
  are technical, not historical; retire the pointers as each file is touched.

## Living documents + project contexts (Amy, 2026-09-07)

Amy: a per-character handoff log is right, but two more ideas surfaced and are
not the handoff — a **living document** (whole-file rewrite, one current
truth, history in git — the shape a *handoff* wants to avoid but a
current-state document wants) and a **project context** (scoping by subject —
kaibo, an OSS project — rather than by character; a character working across
two projects has one handoff log today, the known cost of the per-character
choice).

**Undecided.** Design opinion to argue with: probably no new storage — a
file, a declared attachment to a context/character/project, and rc injection
(the same machinery `S15-recall.kai`/`S16-handoff.kai` already are), so the
real design is the *pointer* (which contexts see which living documents) and
the *injection budget*, not a new `DocKind`.

## The lfm2d gate escalates `kj handoff note` from the MCP shell (2026-09-07)

`kj handoff note` and `kj context create --type coder` from an `mcp` seat
raise an lfm2d advisory ask scored `escalate` even at 0.76-0.82
`situation-normal`, because the seat's `LFM2D_BENIGN_LABEL=informative` is the
only passing label, and same-seat answer is refused — every note needs a
second seat.

**Decided (Amy, 2026-09-08): fix it through the general gate-policy
mechanism**, not a one-off exemption. `docs/gate-policy-tuning.md` (designed
2026-09-08, unbuilt) lists `kj handoff note` as its first tuning-pass entry in
the global allow tier and names this issue by title to close when slice 5
ships. Delete this entry then.

## Two outbound HTTP clients still call out anonymously (2026-09-06)

Every LLM provider dialect sends `kaijutsu/<version>`; two clients don't, still
verified live:

- **MCP streamable-HTTP transport**, `mcp/servers/external.rs:283` —
  `http_transport()` builds its own `reqwest::Client` from `StreamableHttpClientTransport::from_config`,
  and reqwest sends no default UA. Needs a way to hand `rmcp` a pre-built
  client, or fold the UA into `custom_headers`; `llm::http_user_agent()` is
  already `pub(crate)`.
- **A live smoke test**, `llm/claude/models_api.rs:270`, builds a bare
  `reqwest::Client::builder()...build()`. `#[ignore]`d, cosmetic unless wired
  into CI.

Matters because provider attribution is a precondition for kaijutsu being
recognized as an agent harness.

## Check the hook socket's PPID resolution on macOS (2026-09-05)

`candidate_sockets`/`resolve_hook_socket` (`kaijutsu-mcp/src/main.rs`) derive
the MCP's socket path from the parent process id; proved on Linux only. Amy's
MacBook is a supported client and nobody has confirmed the PPID chain and
`$XDG_RUNTIME_DIR` fallback under macOS's launchd-spawned shells and Claude
Code's process model. Run a bridge session on the Mac with `RUST_LOG` on and
read what the resolver picked.

## Hook command: pass `--socket` from settings (2026-09-05)

Still not done: `~/.claude/settings.json` / `contrib/claude-hooks.json`'s hook
command does not pass `--socket`, so the PPID-derived socket only outranks
routing when routing falls through rather than on every call.

## Character: two hand tasks for Amy (rollout in `docs/character.md`)

The slice rollout itself is `docs/character.md`'s to carry (currently: 0a/1/2
built, 4 shipped 2026-09-06/07, 3/5-8 remain — read the doc, not this entry).
Two operational tasks that doc's mechanism supports but nobody has done:

- Bind a spare key (`kaijutsu-server add-key <pub> --as amy`) as lockout
  insurance.
- Decide what to do with seven characters: `hajime` plays no contexts and is
  safe to retire; consolidating the other six means rebinding keys onto one,
  and retire takes a character's contexts with it.

## The MCP `shell` path applies no size limit to its envelope (2026-09-04)

The in-kernel `shell` result is bounded by the broker's per-instance
`max_result_bytes` (`mcp/broker.rs:1745`). The external stdio path has no
equivalent: `ShellCompletion::to_tool_result` (`kaijutsu-mcp/src/lib.rs`)
hands `CallToolResult::structured` the whole envelope with no truncation, so a
command with 10 MB of stdout ships 10 MB. Whether the MCP path should carry
the same budget — and where it would read one from, having no per-instance
policy — is open.

## A tool result's shape still depends on whether its body is empty (2026-09-04, latent)

`Kernel::call_tool` (`kernel.rs:677`) still substitutes the pretty-printed
`structured` payload when the flattened text body is empty. Correct for a
structured-only tool; a hazard only for a tool that returns both a text body
and a structured payload where the body can be empty. No tool has that
property today (`bindings_builtin`/`hooks_builtin`/`resources_builtin` all
return `ToolContent::Json` unconditionally, re-verified) — latent, not live.
Fix if one ever grows the property: decide the shape from the tool's declared
output, not from emptiness.

## A recovered document's first op races for seq 1 (2026-09-04)

`create_document_with_block` (`block_store.rs:479`) sets `next_journal_seq` to
1 inside the DashMap guard on the fresh-row path — no window. Its recovery
branch (`DuplicateDocument`, line 555-561) cannot: it journals the op after
the guard releases (line 575, `self.journal_op(context_id, payload)?`) with
`next_journal_seq` still 0, since only the `Ok(())` branch (line 549) sets it.
A concurrent writer reaching the document in that window claims seq 1 and the
block-creation op lands behind it at seq 2 — a replay that applies an edit
before the block it edits exists. Fix: reserve the seq at entry-construction
time; `journal_op` uses the pre-reserved number instead of deriving one — a
change to the journaling contract.

## The scorer cannot see a redirect, only the exemption can (2026-09-01)

Closing the `--help`/`ledger` redirect hole (`9f426c9c`) made those reach the
classifier instead of skipping it — but the classifier never sees the
redirect: `clause` (built in `items_filter`) deliberately excludes redirects,
so `kj block list > ~/.bashrc` still auto-allows on a clause reading
`kj block list`. `docs/gate-policy-tuning.md` names this out of scope for the
evaluator work ("the evaluator changes who asks, never what the classifier
sees") and cites this entry by title — still open. Three ways to close it, in
increasing cost: append redirects to the scored clause (cheapest, moves the
corpus and escalation rates — re-run `contrib/kj-corpus.json` first); a
standing rule on redirect targets; or treat `has_redirect` as escalate-worthy
on its own (safest, blunt). Not urgent — `is_read_only_kj` already refuses a
redirect for the table it governs, so exposure is `--help`/`kj ledger` only.

## Codex app-server: attaching to a shared daemon over stdio (2026-09-01)

From a sibling session's bridge work, not yet folded into
`docs/codex-app-backend.md`:

- `codex app-server proxy` pipes stdio to a managed daemon's control socket —
  shared-daemon access over plain stdio, so `StdioJsonl`/`JsonlTransport`
  takes it unmodified with no new WebSocket client, and the spawn stays
  outside the protocol module. Only exists for a daemon started as
  `codex app-server daemon start`.
- Our client always calls `thread/start` and only sees the thread it created
  — it cannot attach to a thread a human is driving.
- `codex-codes` (types-only) pulls in nothing past serde/thiserror and models
  `thread/list`, `turn/steer`, `turn/started`, `turn/completed` — a real
  answer to the protocol-churn objection, if the "locally-owned JSON shapes"
  choice is revisited.
- `AdditionalContextEntry.kind` is `untrusted | application` — the right slot
  for peer or tool text entering a Codex turn.

## Synthesis re-embeds the whole context on every block write (2026-09-01)

**Automatic synthesis is disabled** (`kaijutsu-server/src/rpc.rs:3007`,
`spawn_index_watcher` gets `None` for `on_indexed`, and the call site cites
this entry by title as the re-enable condition — keep the title stable).
`run_synthesis` (`runtime/synthesis.rs:60-68`) still embeds every text block
in the context on each call, then again at sentence granularity, then a third
time over 50 n-gram candidates — appending one block to a context with N
blocks costs an embed of all N. Measured 2026-09-01 on zorak: 16 `rten-*`
threads at 72-99% for ~17 minutes from kernel start, load average 25+ on a
16-core box, for four contexts indexed in that window.

**Fix 1 — incremental.** `SynthesisCache::get(ctx, hash)` still cannot hit:
`run_synthesis` still stamps every result `content_hash: String::new()`
(`synthesis.rs:113,126,152`, re-verified), so the key is always empty.
`extract_context_content` already computes a usable hash for `index_context`
— reuse it. Also unconditional: `synth_all`/`synth_context`
(`kj_builtin.rs:253,353`) call `run_synthesis` regardless of `was_indexed`,
which they compute and use only for a counter — wants an explicit
force-resynthesis flag.

**Fix 2 (longer term) — get the models out of the kernel process.** Amy,
2026-09-01: swap bge-small/`rten` for the lfm2d service (already run, already
carries the gate classifier). `RTEN_NUM_THREADS` is unset, so rten's pool
defaults to every physical core and competes with tokio/kaish/everything
else. Subsumes Fix 1 rather than replacing it.

## The WAL grows without bound and never shrinks (2026-09-01)

`kernel.db-wal` measured at 719 MB holding zero live frames — SQLite behaving
as documented: a WAL resets only when a checkpoint finds it larger than
`journal_size_limit`, and the kernel never sets one (still true,
`kernel_db.rs`, no `journal_size_limit` pragma found), so the limit is -1 and
the high-water mark is permanent. Cost is disk, not correctness. Fix: set
`PRAGMA journal_size_limit` at open next to `PRAGMA foreign_keys = ON`
(`kernel_db.rs:1999`) — measure a normal day's high-water mark before picking
a value. Do not switch to a timed `wal_checkpoint(TRUNCATE)` — it blocks
writers where the limit does the same job for free.

## The terminal client — `kaijutsu-tui` follow-ups (first cut shipped 2026-09-02)

Design: `docs/tui.md`. What the shipped skeleton left open, re-verified still
true:

1. **`inputTokens` on the wire.** `kaijutsu.capnp` still carries only
   `cacheReadTokens`; the status line's `⟳` divides by `contextUsedTokens`
   instead.
2. **Editor wire gaps.** `EditorFlow` (`flows.rs:1624`) still has only
   `StateChanged`/`Closed`, no `Opened` — `editor_open_as` publishes nothing a
   sibling renderer can watch for; the `open_editor` peer invocation is still
   the only open detection.
3. **Picker tails see only whole-block events** — the kernel-wide
   `ServerEvent` stream has no partial-block granularity, shared with the
   app's tails.
4. `theme.toml` → ratatui palette, `bindings.toml` keyed by vim notation (no
   file exists yet at either client), then porting the app to the same file.
5. **Images (additive).** `Abc` needs no new emitter (`engrave::engrave_to_svg`
   is complete); `Svg`/`Image` rasterization is unbuilt.
6. Bar/beat assumes 4/4; the wire carries no time signature.

Two open UX items from Amy's first sessions: show the reconnect when it is
what blocks the client (status line has `app.connection`, nothing surfaces
it); and `Ctrl+A a` is unbound (`keys.rs`, falls to
`Intent::NotYet("unbound chord")`) and locked the client once — a harness
probe that presses it and then types is still the next step. The seat-digit
disagreement between the picker and the status line, and the ask-card/ledger
follow-ups, are their own entries below.

## The tui and the app disagree on a few chords (2026-09-03)

Survey against `docs/input.md`'s prefix table and `docs/tui.md` "Keys".
Direction is on record (`docs/tui.md`, guidance 4: a shared `bindings.toml`
the app inherits) but the file doesn't exist yet, so the specific gaps still
stand:

- **App ahead, not yet in the tui:** `Ctrl+A '` (switch by prompt),
  `Ctrl+A A` (rename), `Ctrl+A q` (close+demote), `Ctrl+A d` (detach),
  `Ctrl+A a` (literal Ctrl+A).
- **Tui ahead, not claimed in the app's table (no conflict rolling them in):**
  `Ctrl+A l` (ledger — app has no ledger surface), `Ctrl+A [` (copy mode),
  `Ctrl+A ]` (tui's own yank buffer, not the OS clipboard).
- **`Ctrl+Z`** is suspend in the tui, `ToggleSurface` (chat/shell) in the app
  — the tui retired the shell surface for `:!`, the app still has the
  toggle. Name this in `docs/input.md` or retire the app's toggle.
- `docs/tui.md` "Keys" summarizes the prefix table without `v`/`l`/`]` — the
  summary line is stale, each has its own subsection.

## The /config melt's leftovers (2026-08-29/30)

Resolved: `invalidate_config_file_cache`'s doc comment (`kernel.rs:1875`) now
states the rc-specific rationale directly (a path predicate, not a parameter,
because `kj rc`/`kj config` callers have no session in scope). Only the
`Add` call's redundancy is left undecided — trivial, rediscoverable by
reading `kj/rc.rs`'s `write_path` match.

## Per-client config write-target defaulting has no owner (2026-08-30)

`kj config` is still only `list`/`show`/`reset` (confirmed,
`kj/config.rs`'s `ConfigCommand`) — the old default-to-caller's-own
`/config/client/<id>/<name>` behavior has no successor. Either the file tools
need to reproduce it from the caller's client-id, or the policy is gone and
every per-client write names its full path by hand. Still undecided.

## kaish `ln -s` with an absolute /config path creates a dangling link (2026-08-30)

`LocalBackend::symlink` (`vfs/backends/local.rs:553`) still calls
`std::os::unix::fs::symlink(target, &full_path)` with no absolute→relative
translation — confirmed still the only path with none; only
`seed_scripts::reseed_rc_files` (`relative_link`, `seed_scripts.rs:231-332`)
does the translation. `ln -s /config/rc/lib/hooks/foo.kai .../S45-foo.kai` (the
documented composition idiom — no `kj rc link`) still writes a link that
resolves to nothing on a hand-composed tree. Either `symlink` translates an
in-mount absolute target, or the surface must fail loudly on an absolute one.

## After approval-executes: what is still retry-shaped (2026-09-02)

Every shell ask executes on approval (`docs/gate-shape-b.md`). Still open,
re-verified:

- **A decided ask leaves its gate pair `waiting` forever** — nothing
  completes the pair when the ask is decided, so a tui keeps a stale "gate
  for … is waiting" block pinned.
- **Approving a `:`-line statement wakes a model turn** even though a
  `:`-line origin has no waiting turn to resume.
- **A gated `:kj` statement still surfaces as an error**, not a result naming
  the ask — `.context("execute addressed kj command")` (`kaijutsu-tui/src/bridge.rs:295`,
  `kaijutsu-acp/src/bridge.rs:286`) still wraps it.
- **`kj ledger cancel`** (withdrawal without a verdict) still does not exist
  — confirmed, no `Cancel` variant in `kj/ledger.rs`. Orphaned asks from dead
  seats pile up forever.
- **No TTL on asks** — `expires_at` exists on the row; nothing sets or sweeps
  it for shell/hook asks.
- **`kj ledger list` should show the origin context.**
- **Same-seat deny** — `ensure_not_self_approval` (`kj/ledger.rs:911`) still
  refuses both verdicts from the raising context; permitting deny from the
  same seat is the safe direction. The tui answers from another seat it
  holds, side-stepping this for a human; it stands for a model wanting to
  withdraw its own ask (the `cancel` verb above).

## The scorer and the snapshot (2026-09-02)

**Shipped:** the lfm2d hook now reads `$s.plan.rendered` for `clause`
(`assets/defaults/rc/lib/hooks/lfm2d.kai:66`) instead of rebuilding it in jq.

**Still open:** substituting values into the scored clause before scoring.
Measured 2026-09-02: an unexpanded variable scores as a middle guess
(`chmod -R 777 ${DIR}` 26% vs `/` 97% vs a tmp path 3%). Proposed: score a
second, kaish-rendered *expanded* view beside the unexpanded one, take max
severity — parse-time substitution with a supplied map, not execution. An ask
to the kaish lead, not ours to build.

## The Claude Code advisory hook forwards to the kernel (2026-09-02, shipped; open follow-ups)

`PreToolUse` Bash → `kaijutsu-mcp hook claude` → `shellDryRun` → PreCall in
dry-run mode → abandoned ask row, always allow (`docs/gate-and-shell-split.md`,
"Dry-run mode"). Still open: count a day of `kj ledger list --status
abandoned --since 24h` against the Python hook's verdicts before retiring it;
the `( … ) &` subshell planning gap (kaish cannot plan it, S45 denies "no
execution plan") is an ask to the kaish lead; an export verb for corpus
builders was ruled "later" — they read the ledger directly for now.

## Three MCP compose tools report failure as success (2026-09-01)

`write_input`, `edit_input`, `submit_input` (`kaijutsu-mcp/src/lib.rs:2328,2368,2399`)
still return a plain `String`, not `CallToolResult` — confirmed unchanged.
Every failure on that path, gate/capability refusals included, still reaches
the model inside an MCP *success* envelope with no `is_error: true` to key
on. Fix: return `CallToolResult` with `is_error` set (updates the local-mode
test that asserts the `String` shape), and surface `refusal.ask_id()` while
there.

## A secret source that runs a command has no home yet (2026-08-31)

`env.TOKEN = { command = "..." }` is still rejected —
`a_command_source_is_deferred_with_an_instruction` (`mcp/toml.rs:465`)
confirms the message says "not implemented" and names `file`/`env` as the
alternatives. Deliberate: host exec has one owner, and `external.rs`'s
sanctioned exception is about launching a config-declared server, not running
arbitrary programs for config values. Three ways in, whichever wins keeps the
existing fail-loud contract (never launches blank, never quotes the value into
a log line): route through `EmbeddedKaish`'s exec policy (needs the kaish
runtime up at secret-fetch time); widen the `external.rs` exception
deliberately (fastest, a second bare `Command::new`); or leave it — `pass show
… > ~/.token` is one hop away.

## The `attach` verb fires and no type ships a script (2026-08-28)

Confirmed still true: no `attach/` directory exists under any
`assets/defaults/rc/<type>/` despite `kj/attach.rs` running the lifecycle
(`VERB_ATTACH` wired, `attach.rs:75` rejects on `Err`). Either it earns a
script (re-stating the seat's stance to a model that arrived in a context it
did not boot) or `attach` should retire from `RC_VERBS`.

## Ctrl+Z lands in the wrong input on the second toggle (Amy, 2026-08-26)

Amy: *"sometimes when I hit ctrl-z it goes to conversation input... usually
first time is fine, second time goes to the other input."* No fix found in
the app (`ToggleSurface` still the same shape, `input/systems.rs:105`). Queued
for the app/UI session — start at `ActiveSurface`/`FocusArea` duplication
(128 references across `kaijutsu-app/src/`, still two pieces of state that
must agree and are set in different places).

## `persist_binding` swallows a failed write (2026-08-23)

`Broker::persist_binding` (`mcp/broker.rs:1268`) still only `tracing::warn!`s
an `upsert_context_binding` failure and returns — confirmed unchanged. The
in-memory cache updates either way, so a caller cannot tell whether a written
loadout is durable. Found because it hid a test: an unregistered
`ContextId`'s binding write fails its foreign key, persists nothing, and
stayed green with the fix removed. `kj binding reset`/`allow`/`revoke` and the
MCP bind/unbind tools would have to carry the error.

## Binding review: unfixed items (2026-08-23)

Re-verified against the current tree:

- **The bind/unbind diff still omits everything under `"*"`.**
  `binding_visible_tool_pairs` (`mcp/broker.rs:1107`) still iterates only
  `candidate_instances()` with no `all_instances` check — `kj binding allow
  "*"` still fires `ToolAdded` only for named instances. (Note: the sibling
  read path, `list_visible_tools`, line 1470-1478, *did* get the
  `all_instances` fix — only the diff-emission path is still bare.)
- **`binding_checked` is still wired to one of three enforcement points.**
  `check_facade` (`broker.rs:1441`) and `call_tool_inner` (`:1588`) still call
  `binding()` directly, so a DB read error there surfaces as `FacadeDenied`/
  `CapabilityDenied` instead of a storage fault.
- **Sticky `name_map` still defeats the collision resolver on sequential
  grants** — unre-verified this pass, no related commit found.
- **Background jobs are still not cleaned on narrowing** —
  `kill_all_for_context` (`kj/context.rs:2003`) is still wired to context
  removal only, not to a capability revoke. Killing on narrow would destroy
  work, so this needs a decision, not just a patch.

## The wire drops kaish's output line anchor (2026-08-23)

`OutputNode` (`kaijutsu.capnp:1535`) still has no `line` field, confirmed —
`name`/`entryType`/`text`/`hasText`/`cells`/`children` only. kaish 0.16+'s own
`OutputNode.line` cannot be populated on our wire, so any client reading
structured output loses the anchor. Adding it is a schema change plus all five
artifacts rebuilt — its own decision. Worth doing when something wants it:
`grep -n`, an editor jump-to-match, and the vi surface are all line-anchored
already.

## An Error block is shown twice after a fork (2026-08-22)

`llm/hydrate.rs`'s Error-block fold (`:366-410`) still unconditionally
appends the envelope onto a parent `ToolResult` that may already carry the
same message — confirmed no dedup check. Costs tokens on every hydrate after
a fork and teaches nothing the first copy didn't. Fix has to keep the
standalone-error path (when the parent's tool result already flushed) and
skip only the duplicate — a judgment call, not mechanical.

## A dying turn still orphans its blocks mid-run (2026-08-22, half shipped)

Boot sweep shipped (`830711e9`): a cold-start `Running` block is failed with
an `Error` child. **The panic case is still open** — `process_llm_stream`
(`llm_stream.rs:1360`) is still `spawn_local`'d with no `catch_unwind`,
confirmed. A panic mid-stream silently kills the task: nothing publishes
`TurnFlow::Failed`, blocks stay `Running` until the next restart. Hazard to
design around: `process_llm_stream` holds the per-context mailbox lock for
the whole stream, so a naive catch that resumes without dropping it deadlocks
every later turn on that context.

## Asks vs forms — decision open (2026-08-22)

`docs/asks-and-forms.md` is the full analysis (three-layer ledger, only the
bottom shell-shaped; MCP elicitation has the right payload with no
durability). Still true: *"Nothing is decided and no code is proposed"* — the
brief's own next step is running delegated turns to see whether coders' actual
questions are allow/deny in disguise.

## The app can stop taking the kernel-wide firehose (2026-08-22)

`ActorHandle::watch_contexts` lets a block-event subscription name a *set* of
contexts rather than one-or-all. The app still sets
`scope_blocks_to_context = false` (`connection/bootstrap.rs:130`, confirmed
unchanged) and takes every context's block events. It could watch exactly the
contexts it renders and re-issue as that set changes. Worth doing only if
event volume shows up in a profile — the firehose is a known cost, not a known
problem.

## The escalation seat: a small model that prepares the ask (2026-08-21)

Direction, not a spec (Amy: a small model reads `KJ_TOOL_PLAN` and the lfm2d
signals, writes a description and a recommendation, and does not decide — a
human still answers through `kj ledger`). What already exists: cast slots
keyed by `context_type`, rc for stance/loadout, the ledger for a durable
write target. What's still missing, confirmed unchanged: a hook body's
stdout is captured and then ignored (`classify_kaish_hook_exit(exec.code,
&exec.err, &fallback)`, `mcp/broker.rs:2667`, no stdout parameter) — `out`/
`err` concatenate across every statement in the hook body and `data` reflects
only the last one, so "JSON on stdout" needs a rule for which line before
this is buildable. Replying inline (a seat relays a human's reply from its
own conversation into a ledger decision, after checking the reply's principal
is human) is a real, undesigned option worth keeping in view.

## `kj context set` applies its fields one write at a time (2026-08-21)

`apply_context_config` (`kj/context.rs:359-440`) still issues separate
sequential `db.update_model`/`update_cast`/`update_settings`/
`upsert_context_shell`/`set_context_env` calls with `?` early-return between
them, confirmed no transaction. A failure on a later field leaves earlier ones
durably committed. Reachable since `--env` validates its key at write time:
`kj context set --model x --env 1BAD=y` commits the model, then fails.

## Tech-debt audits, 2026-08-20 — what is still open

Full reports: `docs/audits/`. Re-verified against the current tree:

- **`dirty_file_buffers.context_id` is written and never read** —
  confirmed, only an `INSERT`/`ON CONFLICT UPDATE` (`kernel_db.rs:6337`), no
  `SELECT` reads the column back. S.
- **The MIDI ear still logs a WARN on every refused capture batch**
  (`kaijutsu-audio-runtime/src/runtime.rs:398`), not once per state change —
  confirmed unchanged. An expected idle state, not a fault. S.
- **Escalate in PostCall/OnError/OnNotification still blocks the path up to
  the gate wait** — whether escalate is meaningful outside PreCall is still
  undecided. M, design.
- **rc softening for interactive seats is still missing** — that a human's
  interactive shell takes the hook path is written down
  (`docs/gate-and-shell-split.md`, "The three rpc.rs shell paths take the
  hook path"); the softening itself is not built.
- **The rc bootstrap gate still seeds a tree only when the whole directory is
  empty** (`rpc.rs:2585`, `if dir_is_empty(&host_dir)`), not path-by-path —
  confirmed still true, so a script added to the embedded set after a kernel
  was first seeded still never lands on its own; recovery is still a manual
  `kaijutsu-server rc reseed`. (`ensure_rc_seed_files` itself is install-if-
  absent per path and well tested — `rpc.rs` gaining `#[cfg(test)] mod
  context_bootstrap_tests` this pass is unrelated coverage, not this gap.)
- **The `kj` builtin still flattens kaish's typed `ToolArgs` back to argv**
  for clap to re-parse (`kj_builtin.rs`) — still waiting on kaish's
  `ArgBinding::Verbatim` (not present at kaish 0.17, confirmed).
- **`OutputProfile::Internal`** (`runtime/embedded_kaish.rs:127,1282`) is
  still present, still waiting on a kaish spill knob that doesn't remap the
  exit code.

## Older app and broker debt, carried out of auto-memory (2026-09-08)

Re-verified against the tree the same day:

- **Tall blocks lose Y resolution** past `max_texture_dimension_2d`
  (`GpuTextureLimits`, `view/block_render.rs`) — confirmed present. Fix is
  tiled rendering of the visible portion.
- **Role-group borders still draw through Vello**, missed in the Vello→MSDF
  migration.
- **Broker `register` over an existing instance id still drops the old pump
  `JoinHandle`** instead of aborting it — confirmed:
  `self.pump_handles.lock().await.insert(id.clone(), handle)`
  (`mcp/broker.rs:729`) silently drops the prior handle on overwrite.
- **Provider cache expiry is not a hydrate boundary** — a long-idle session
  carries messages the provider no longer has cached; nothing observes it.
- **`ActiveSurface`/`FocusArea`/paired overlay queries** are threaded as
  separate params through compose/interrupt/toggle systems (128 references) —
  a bundle component or resolver would collapse them.

## File buffers: MCP tool removal still not done (2026-08-19/21)

Slices 1-3 of `docs/file-buffers.md` shipped. **Slice 4 was RULED 2026-08-21:
remove the MCP file tools outright**, `grep` and `edit` included — Amy: *"It's
ok if we don't have them for a short period while we finish the kaish
upgrade."* **Not done**: `mcp/servers/file.rs` still registers `read`,
`edit`, `write`, `glob`, `grep` as live tools, confirmed. `docs/file-buffers.md`
itself still describes a *different* slice 4 ("remove `write` and `grep`; make
`edit` hashline-only; add `create_file` if wanted") that does not match Amy's
ruling as recorded here — flag this drift to whoever picks the slice up rather
than trusting either account alone. **Slice 5** (`swapRecovered`/
`diskChangedSinceLoad` on `EditorState`) is also still open, confirmed absent
from `kaijutsu.capnp`. The "recovered swap has no push" half is superseded:
`kj swap list/ack/discard` now exists (`kj/swap.rs`) and is the consumer
`list_dirty_file_buffers` was missing.

## Opening a file that already has an editor session should announce it (2026-08-19)

`EditorSessions::open` (`editor.rs:286`) still always creates a fresh session
with no check for an existing one on the same path — confirmed;
`sibling_bound` (used at quit, `editor.rs:744`) proves "is someone already on
this file" is computable today, just not consulted at open time. Amy,
2026-08-19: *"like vim it should detect that and tell me, so I can go back to
the other one or shut it down."* Shape: announce rather than silently open a
second view; let the player attach or explicitly discard (the other session
may hold unsaved work). Not "refuse the second open" — two players on one
block is a supported state (`docs/vi.md`).

## File documents should be created lazily, not on every read (2026-08-19, deferred)

Every file the kernel reads still leaves a durable block-store document
forever (`block_id` still not optional in `get_or_load`, confirmed). The
eventual model — a clean buffer stays a `String`, a document materializes only
on first edit, so a document existing *means* unsaved work — is deferred for
cost and cross-referenced from `docs/file-buffers.md` itself ("filed in
docs/issues.md; the row goes away if it lands"). Revisit once swap semantics
are proven.

## The swap marker and the content it marks are two writes (2026-08-19)

`record_dirty_file_buffer` (`kernel_db.rs:6337`, one `INSERT ... ON
CONFLICT`) and `edit_text`'s oplog journal write are still separate
statements, not one transaction — confirmed. A crash between them either loses
the marker (cold path reconciles the unsaved work away) or leaves a marker
pointing at content never written.

## `edit` still names two different things on two surfaces (2026-08-18)

Confirmed unchanged: kaish builtin `edit <path>` (`context_shell.rs:217`,
registered alongside `vi`) opens an interactive vi session; MCP tool `edit`
(`mcp/servers/file.rs:163`) is a surgical, non-interactive hashline/string
edit. Same name, same coder, opposite mechanism. `vi` is already the
documented front door (`docs/vi.md`), so dropping the kaish `edit` alias is
the cheap fix — check rc scripts and help text for callers first.

## `write` has no staleness guard (2026-08-18)

`write_file` (`mcp/servers/file.rs:512`) still calls `cache.create_or_replace`
with no precondition, then `flush_one` — never `flush_one_guarded` — so the
W12 disk-moved-under-you guard that protects `:w` still does not apply to
`write`. Whether an existing-file `write` should require a
generation/hash precondition or explicit overwrite intent (while a new path
stays a plain create) is still undecided. This is the asymmetry that let a
stale-context overwrite drop 115 backlog entries from this file on
2026-06-29 (recovered from `3f8b54d3`) while `edit`'s hashline mode would have
refused.

## Two features silently lost when the legacy conversation path was deleted (2026-08-18)

Both already self-documented as dead in code, still unfixed:

- **Rainbow user-text effect** (`Theme::font_rainbow`, default on) — `text::
  components::{KjTextEffects, rainbow_brush}` are kept `#[allow(dead_code)]`
  as reference; the conversation surface (`view::surface::content`) has no
  equivalent.
- **Timeline dimming** — `ui::timeline::systems::update_block_visibility`
  still runs over an empty query; nothing spawns `TimelineVisibility` any
  more.

Both need genuine design work to port (theme-driven color derivation and a
visibility/opacity input both live at the wrong layer for the surface's
entity-free pipeline), not a one-line fix.

## P1: hydration's tool-pairing repair can poison a live ACP turn (2026-08-18, partially mitigated)

The 2026-08-18 live failure (an assistant message with `tool_calls` and no
matching `tool_result`, three retries then agent exit) was two adjacent-window
repair heuristics disagreeing: synthesis looks only at `messages[i+1]` for
coverage, the drop keeps only results whose `tool_use` is in the immediately
preceding message, and a result landing further away falls outside both.

**Partial fix landed:** `report_unpaired_tool_uses` (`llm/hydrate.rs:808`)
now detects and `tracing::error!`s any assistant message whose tool_uses have
no paired result, naming the message index and the unpaired ids. **Still
open:** it only logs — the message list is still sent as-is, so the provider
can still reject the request; the fix called for (one pass establishing the
invariant, plus a refusal before the request leaves) is not built. Related,
unchanged: `CLAUDE.md`'s mailbox atomicity claim needs checking against
whether this gate covers this path or the pairs are split after it.

**Remediation for a poisoned context today**: exclude the offending blocks,
then fork.

## Flaky: `test_ordering_stress_100_bisections` put a Middle block first, once (2026-08-17)

`test_ordering_stress_100_bisections`
(`crates/kaijutsu-kernel/src/blocks/block_store.rs:2796`) asserts only
`blocks_ordered()[0].content == "First"` — it still checks the sorted
output, not the generated order keys, so a rare ordering inversion (seen
once: `Middle-5` sorted ahead of `First`) would fail the same way again
without naming the mechanism. Cause unknown: not reproducible on demand
(8/8 and 10/10 clean reruns), and not touched by `89d90ccc`'s `merge_ops`
work. Fix the test to assert on order keys, not the sort result, so a
recurrence names order-key precision or tie-break rather than a content
string.

---

## Serialized-struct changes need a restart, not a migration framework (2026-08-16)

Rule, still uncoded anywhere but here: restart the kernel promptly after
committing a change to a serialized struct (`SyncPayload`, `BlockSnapshot`,
`StoreSnapshot`, `BlockHeader`, `TextEdit`, or anything reachable from
them) — a running process is a version a commit does not reach, and the
window before restart is what produces unreadable rows. `SCHEMA`/
`apply_additive_migrations` stay the mechanism; do not build a migration
framework for a once-in-a-project event.

Open: `purge_dte_cutover_oplog_rows` and its `drop_dte_oplog_2026_08_16`
marker (`kernel_db.rs:1801,3098`) are dated cleanup — delete both once
every live kernel has booted past 2026-08-16. Nothing tracks that date;
still present as of 2026-09-08.

Not urgent, considered and dropped: a CI test decoding a corpus of
recorded payload bytes from the previous release, to catch this class at
commit time instead of at boot.

---

## Triage of a real context's "37 failed tool calls" (2026-08-16)

Most of this triage shipped: the ×3 block-count inflation is fixed
(`count_block_activity`, `kaijutsu-app/src/ui/dock.rs:2428`, dedupes a
tool_result + its Error child); `method_missing`-style tool listing
shipped as `builtin.tool_search` (`mcp/servers/tool_search.rs`); the kaish
parse-error traps triaged here were against 0.13/0.14 and kaish is now
0.17.1 (current traps live in the `gotcha_kaish` memory, not here).

Still open: a `;`-separated command chain's `is_error` is still the last
command's exit status verbatim (`env.is_error()`,
`mcp/servers/shell.rs:635`), so a chain whose last command fails reports
`Error:` even when every earlier command succeeded, and the reverse (last
command masks an earlier failure) also still reproduces. Decide what a
multi-command chain's status should mean before filing this again.

---

## The file write/edit tools are not gated by the approval ledger (Amy, 2026-08-16)

`builtin.file:write`/`:edit` still route as plain capability tokens, not
through `approval_ledger` — confirmed still true, and
`docs/gate-and-shell-split.md` ("What this does NOT do") names this exact
gap as unsolved by that design. Not a security boundary (every player is
already inside the trust boundary); the ask is an ergonomic nudge so a
large destructive edit is visible and undoable rather than only
forensically reconstructable afterwards. A cheap partial worth keeping on
the table: gate on a size-delta threshold (N lines or X% of a file)
rather than every write.

---

## `kj rc render <context_type>` — let one context type assimilate another (Amy, 2026-08-16)

Not built (`kj rc render` unrecognized anywhere in `kj/*.rs`). The design
worth keeping if this gets picked up: **render, never run** — a context
type's rc has real side effects (`kj binding allow`, `transport attach`),
so this quotes source, it does not execute it. **Reframe to third
person** — rc stance is second-person imperative
(`musician/create/S00-stance.md`: "You're a musician here"), and handing
that verbatim to another context's system prompt gives it instructions,
not information; render must say "a musician is told…". Three parts worth
surfacing separately: stance (the `.md` files), allow-set
(`S10-binding.kai`), verb set (`ls /etc/rc/<type>/` — the free row, since
it's literally the interaction protocol). Land the output via
`kj block create --role system`, not an auto-inject, so assimilation
doesn't silently cost a cache write and stays undo-able.

---

## Hi-res wheel (v120) blocked at winit/sctk — slow drags are a compositor dead zone (2026-08-16)

Root cause confirmed, not app-side: MX Master emits sub-detent v120 →
sctk 0.19.2 has no `AxisValue120` handler (verified in the cargo cache) →
winit 0.30.13 pins that sctk and has zero value120 references even on its
own master branch, so the block is winit, not sctk or the compositor —
switching KWin→Mutter would not fix it.

**Lane PARKED by Amy's call** ("fix kaijutsu-app for what already works
… experiment later with the HID++ device"). Carried forks exist and were
protocol-verified live (`~/src/research/{client-toolkit,winit}`, branches
`tobert/axis-value120-0.19` and `tobert/wayland-axis-value120-0.30`) but
`[patch.crates-io]` was removed from `Cargo.toml` per the no-committed-
path-deps rule; re-wire via the `tobert/*` GitHub forks when resumed. Root
probe also found the Bolt receiver (046d:c548) runs on `hid-generic`, not
`hid_logitech_dj` — hidpp never manages the mouse, a separate,
possibly-upstreamable one-line kernel fix
(`~/src/research/bolt-dj-bind-test.sh`). Do not re-add smoothing hacks
meanwhile; pipeline stays fraction-ready.

## `docs/architecture/` needs re-certification, and two diagrams are missing (2026-08-16)

The two deleted diagrams (`01-system-topology.svg`, `06-crate-deps.svg`)
are now tracked in `docs/architecture/diagrams/README.md` itself — that
doc is canonical for the diagram gap, not this file.

Still open, not in any docs/*.md: `docs/architecture/README.md`,
`foundation.md`, and `client.md` were swept for vocabulary and for the
deleted last-write-wins machinery, but not re-verified line-by-line
against current code the way the 2026-06-16 sweep did originally — treat
as improved, not re-certified. And: `test_task_status_lww_tiebreak_order`
(`kaijutsu-types/src/block.rs:5566`, confirmed present) is the only thing
still pinning `TaskStatus` LWW order — decide whether that order needs
pinning at all now that concurrent merge into a kernel document is
structurally impossible.

## Catch-up seam: mark and jump to the read/unread boundary (2026-08-16)

Proposal, not built: record where the reader last left the tail
(app-local first; kernel roster/per-principal read state later), render a
rule at that seam, bind a jump-to-seam chord. Pairs with sticky follow,
which already knows the moment the user leaves the tail.

## Error stub polish: dedupe summary-vs-detail, cap wrapped height (2026-08-16)

Both still open in the block's new home,
`crates/kaijutsu-present/src/format.rs` (`format_error_block`/
`format_error_stub`, ~184-246): (a) `format_error_stub` does not skip
leading detail lines that duplicate the summary, so a stream error whose
`detail` starts with `block.content` still renders the message twice; (b)
`ERROR_STUB_DETAIL_LINES` caps by line count only, so one long line still
wraps to more screen lines than the budget implies — add a char budget
alongside it.

## vi input editor stopped repainting after a small in-place edit (found 2026-08-16, live on moltar)

Amy was editing a typo ("rost" → "rest") in the compose-block vi input.
`i e <Esc>` (insert `e`, leave insert mode) changed the buffer but the
on-screen render did not update; `dw` + retype (a full replace) rendered
correctly. Not root-caused; likely a missed redraw/dirty flag on the
insert-then-escape path rather than a buffer-state bug. Vi input handling
lives in `crates/kaijutsu-app/src/input/vim/` (`mod.rs`, `dispatch.rs`) —
no dirty/repaint marker found there by name, so check whatever marks the
input view dirty against the insert-mode commit path.

---

## The roster index has no kernel-now reference, so client-rendered ages mix two clocks (2026-08-16)

Still true: `/run/roster/index` carries each row's `recorded_at` on the
kernel's clock (`kaijutsu-kernel/src/roster.rs`) but no kernel-now value,
so a client renders age by subtracting the kernel's stamp from its own
clock. Accepted for now (NTP-disciplined LAN, skew below display
resolution); wrong the moment a client's clock isn't disciplined. Fix:
add a kernel-now value to the index, or give `FileAttr` a `generation`
(see the entry below) so a client can conditional-fetch instead.

---

## The wire `FileAttr` carries no `generation`, so clients cannot do a conditional VFS fetch (2026-08-16)

Still true: `struct FileAttr` (`kaijutsu.capnp:1314-1321`) has
size/kind/perm/mtimeSecs/mtimeNanos/nlink and no `generation`, though the
kernel already stamps `FileAttr::generation` server-side
(`vfs/types.rs:67`) and `Vfs.snapshot`'s `SnapshotNode` already carries
one (`generation @6`; the next free ordinal on `FileAttr` is also `@6`).
Fix: append `generation @6 :UInt64;`, set it in `set_file_attr`
(`kaijutsu-server/src/rpc.rs:12158`), add `RpcClient::vfs_getattr`. Until
then a poller (the app's roster feed) must re-read the whole file to
detect a change rather than getattr-then-maybe-read.

---

## Drift peer origins are stageable but not deliverable, and the wire can't name one (2026-08-17)

Still accurate and still unreachable in production (only tests construct
`DriftOrigin::Peer`) — confirmed at
`kaijutsu-server/src/rpc.rs:10839-10866`, `origin_ctx_bytes` reports a
peer origin as absent because the wire's `sourceCtx @1 :Data` means "a
ContextId" and nothing else. Durable, never lost (drains to dead-letter
like any delivery failure), just can't be delivered or displayed.
**Whichever lane adds the first real peer-origin producer (the cc inbox,
`docs/drifting-dead-letters.md` slice 4) must give the wire an honest
origin representation in the same change** — appending origin fields is
ordinal-safe, do it then, not before.

## A context's version is unobservable from `kj` (2026-08-15)

Still true: no `inspect`/`show`-with-version verb exists on `kj context`
(only `list`, `info`, `prompt`, `current`, confirmed in `kj/context.rs`),
and `kj context info` does not surface version. The `getContextVersion`
RPC already exists (`rpc.rs:7522`) and is what a `kj context inspect
<ref>` would read — version is the client's hydration anchor
(`docs/change-feed.md`) and survives restarts as of `e0bb2076`, but that
fix has never been checked against production data because there is no
way to ask.

## `rc reseed` seeds from the BINARY, not the repo (2026-08-22)

`assets/defaults/rc/` is the in-repo seed, but a reseed installs the
defaults **embedded in the running binary**
(`RC_SEED_DIR = include_dir!(...)`, `kaijutsu-kernel/src/seed_scripts.rs`
— confirmed still `include_dir!`-embedded). Editing the repo file and
reseeding reports `0 written` and changes nothing, because the live file
already matches the binary's (stale) copy.

Editing a shipped default therefore needs: edit → **rebuild** →
`kaijutsu-server rc reseed --force`. Missing the rebuild looks exactly
like a successful no-op.

## The rc lifecycle shell has a narrower tool set than the interactive one (2026-08-22)

An rc `create` script calling `fmt` fails with `command not found: fmt`,
while the same command in the MCP `shell` tool succeeds — a `.kai`
verified interactively can still fail at context create. Exec authority
gates on the context's binding holding `Capability::Exec`
(`kj/context_shell.rs:283`), so this is plausibly an ordering artifact of
when a create-time binding takes effect, not yet confirmed either way.

Two consequences. Verify rc scripts by *creating a context*, not by
running the command in a shell. And a create-path script under `set -e`
should degrade rather than abort: a failed helper takes down the whole
context create, and a context with no stance is worse than a stance that
reads a little ragged.

## The well's activity glow wants a derived signal (2026-08-15)

Still disabled: `RingActivity`'s decay/ripple math is live and tested,
but nothing calls `record` outside its own tests
(`kaijutsu-app/src/view/time_well/activity.rs`, module doc confirms it).
The old signal (kernel-wide token-stream events) is not coming back —
Amy wants a kernel-side embedding-derived `(contextId, weight)` hint
instead, riding the directive path (`onRenderCue`/`onBeatSync`), never
batched. To re-enable: feed `RingActivity::record` from that signal and
register an ingest system in `time_well/mod.rs`.

## `connection/drift.rs` still reads block events off the kernel-wide stream

Still true, two sites: `connection::drift`'s
`ServerEvent::BlockInserted { kind: Drift }` detector
(`kaijutsu-app/src/connection/drift.rs:181`), and
`time_well::live::ingest_live_events`'s `ContextTails` build
(`live.rs:289`) — both read the kernel-wide stream rather than the
per-context change feed, deliberately kept because the feed can't serve
contexts nobody follows. Decide the scope question (does drift into an
unfollowed context deserve a notification?) once for both sites, not per
site, before moving either.

## Model names via hooks — the plumbing exists, the data mostly does not arrive (2026-08-15, Amy)

Plumbing is built (`HookEvent::model: Option<String>`,
`kaijutsu-mcp/src/hook_types.rs:30`; set on `SessionStart` in
`hook_listener.rs`) — the gap is source data, and it's three separate
problems: Claude Code's hook payload has no model field at all (would
need `transcript_path` JSONL sniffing); Crush/qwen doesn't send it either
(unconfirmed what it does send); `SessionStart`-only is stale the moment
`/model` changes mid-session. Do the per-source capability survey (who
sends it, what's the honest fallback) before writing more code — and if a
source can't supply it, the roster should say "unreported", not render as
unconfigured.

---

## `/v/docs` block filenames do not sort into document order (2026-08-15, Amy)

Still true: the `/v/docs` backend lists block filenames as
`BlockId::to_key()`
(`kaijutsu-kernel/src/runtime/kaish_backend.rs:443`), the
`<ctx>_<principal>_<seq>` string, so `ls` sorts principal-major and seq as
text (`_2` after `_15`). `order_key`
(`kaijutsu-kernel/src/blocks/content.rs`) is already a base-62
lexicographic fractional index built for exactly this — rename to
`<order_key>__<short_block_id>` and `ls` sorts correctly with no kaish
change. Unblocks the sketched netrw-style subscription view Amy wants;
open question is whether `order_key` (which moves when a block moves) is
stable enough for whatever consumes the name.

---

## Principal plumbing — a holistic sweep, not a per-lane patch (2026-08-15, Amy)

Not started (no `PrincipalSweep` or equivalent found). Ruling stands:
this does NOT gate the git-auto-commit work — that ships with
service-authored commits and principal fidelity retrofits later. When it
happens, survey first: inventory every mutation path that reaches durable
state (VFS/config writes, rc edits, block mutations, MCP tool calls,
kaish builtins, drift, `kj` verbs) and classify what each knows about its
actor — real `Principal`, synthetic/service, inferred, or genuinely none
(legitimate for kernel-internal timers). Not an authorization mechanism:
attribution and recovery only (`docs/instrument-design.md`, "Many hands,
one trust boundary").

---

## Reconnect follow-ups from the auto-reconnect + backoff task (2026-08-14)

Two gaps, both still present. **`SyncedInput` never resyncs after
reconnect**, only after a fresh context join
(`kaijutsu-app/src/view/sync.rs`'s `handle_block_events` still guards on
`cached.input.is_none()`) — an `EditInput`/`SubmitInput` issued by a peer
during an outage never backfills; `SyncedInput` has no
`apply_sync_state`-equivalent the way `SyncedDocument` does. **The
`periodic_reconnect` comment in `actor_plugin.rs:1242,1286` still
describes a system that does not exist** — grepped, no such function; the
actor's own FSM already retries indefinitely so this is dead code,
harmless unless the actor's tokio task panics outright, in which case
there is no recovery short of an app restart today.

---

## Managing roots — the concept kaijutsu is missing (Amy, 2026-08-15)

Settled design: a forest of one-parent trees; drift is a separate,
deliberately cyclic overlay, never part of structure
(`KernelDb::insert_edge`'s cycle check already applies only to
`EdgeKind::Structural` — confirmed). Archive is one row, never a cascade,
and archived contexts keep their label out of the live index — both
**shipped 2026-09-05**. Anchors are seats, not workspaces (fork copies
history by default, so an unused anchor is cheap and a used one taxes
every descendant forever).

**Not built**: the `anchored_at` column (slice 2 — parentless by
construction, never swept by age; no such column exists yet), so there is
still no `--detached` flag on `kj context create` and no enforcement of
"anchors stay unused." Recommendation stands: build slice 2 (the anchor
bit) before slice 3 (enforcement ruling) or slice 4 (`kj root` verbs).
Slice 1's guardrail (`context move` non-atomicity) is still open — same
bug as the entry below.

---

## Context lifecycle: `kj context move` still isn't atomic (2026-08-15)

Two of three original edges are resolved (archive no longer cascades;
archived contexts free their label) — see "Managing roots" above.
**Still live**: `context_move` (`kaijutsu-kernel/src/kj/context.rs:1655`)
deletes every existing structural parent edge, *then* calls `insert_edge`
(where cycle detection lives), with no transaction around the pair —
confirmed unchanged. A refused move (cycle detected) has already
destroyed the old edge, leaving the context orphaned. Fix: one
transaction, or check the cycle before deleting. Also still true: no
`--detached` flag exists on `kj context create`, so a parentless context
can only be produced via this bug's failure path, never deliberately.

## Live roster — push-on-attach is the remaining unwired half (2026-08-14)

Slices 1-4 and periodic refresh shipped (`crates/kaijutsu-kernel/src/roster.rs`,
`roster_sources.rs`, `kj/roster.rs`, `vfs/backends/roster.rs`,
`tests/roster_refresh_boot.rs`). Still open:

- **Push-based refresh on peer attach/detach isn't wired.**
  `roster_sources.rs:43` still names this a TODO —
  `kaijutsu-server`'s RPC attach/detach handlers should call `refresh_once`
  (or a narrower single-peer reconcile) instead of waiting for the ~10s pull
  tick. `SharedKernelState::roster` already holds the handle needed.
- `RECENT_LIVE_WINDOW_MS` (`roster_sources.rs:73`, 15 minutes) is an untuned
  v1 guess — a config knob if it needs adjusting.

## Theme changes never reach a running app — there is no live config push (2026-08-13)

Still true, verified: `ThemeReceived` (`actor_plugin.rs:551`) has exactly one
send site, the connect-time bootstrap fetch, and no `ServerEvent`
config/theme variant exists. `kj config set` on the theme file updates the
kernel document and nothing tells a running app — a next-connect console,
not the live one `docs/color.md` sells.

Remaining work: a config-changed server event (or subscription) that
re-fires `ThemeReceived` on theme writes; the app-side repaint already works
once `Theme` is replaced. Also open: `ThemeData` (the TOML wire format) has
no fields for block text colors (`block_user`/`block_assistant`/…), so no
theme file can change conversation text colors; `Theme` derives neither
`Reflect` nor registers with BRP, so it cannot be poked remotely for testing.

## Dock RTT sizes skip physical-px rounding (2026-08-12, kaibo find)

Still open, verified: `render_north_dock`/`render_south_dock` still stamp
`rtt.built_width = logical.x` raw (`ui/dock.rs:585,844`), while block cells
round via `round_to_physical_px` (`view/block_render.rs:318`). At fractional
DPI that makes `msdf_item_scale` a hair off exact, giving sub-pixel glyph
drift on the dock. Cosmetic, pre-existing; fold into the tier-2 "unify RTT
resize" cleanup.

---

## kaish output limiting corrupts the durable exit code in three places (2026-08-15)

**Shipped 2026-08-15:** `execute_shell_command` (`rpc.rs`) now persists
`result.original_code.unwrap_or(result.code)` as the durable `exit_code`, so a
command that spills >8 KB of output but exits 0 no longer records
`exit_code=3` forever. Pinned by
`test_shell_truncation_does_not_corrupt_exit_code`
(`crates/kaijutsu-server/tests/e2e_kj_workflow.rs:500`).

**Still open — same bug shape, unfixed sites** (verified against current code):
- `crates/kaijutsu-kernel/src/kernel.rs:1731` — vi's `:r !cmd`
  (`EditorIo::ReadShell`) checks `result.code != 0` raw; a spilled-but-
  successful command reports a spurious editor failure.
- `crates/kaijutsu-kernel/src/kj/lifecycle.rs:604` — rc-lifecycle `.kai`
  execution matches `exec.code == 0` raw and persists the unresolved code
  into a durable rc-failure block on the fallthrough arm.
- `crates/kaijutsu-server/src/rpc.rs:2044` (`dispatch_output_events`, the
  streaming `execute` RPC) ships `result.code as i32` unresolved to every
  `on_output` subscriber.

Fix for all three: the same `original_code.unwrap_or(code)` unwrap
`execute_shell_command` already does. `mcp/broker.rs`'s hook-exit classifier
(`classify_kaish_hook_exit`) already treats a remapped `3` as its own
`Escalate` outcome rather than folding it into pass/fail, so that site is not
in this list.

## Summaries drift stronger than what they summarise (2026-08-11)

Writing failure mode, not a code bug: a summary reads stronger — or, in two
recorded cases, weaker — than what was actually pinned, and the next reader
inherits the drifted version without re-deriving it.

**Still open — Amy's call, never ruled on:** should a claim about what is
*proven* carry the assertion name or a `file:line`, so the next reader can
tell pinned from observed at a glance? Not adopted anywhere in CLAUDE.md or
docs/ as of this sweep.

---

## MCP 2026-07-28 adoption — two slices shipped, three items open (2026-08-11)

Shipped since filing: **elicitation slice 1a** — `create_elicitation`
(`mcp/servers/external.rs:200`) now emits `ServerNotification::Elicitation`
instead of rmcp's silent auto-decline. **`on_progress`** (`external.rs:167`)
is no longer a no-op — it forwards `ServerNotification::Progress`.

Still open:
- **Elicitation slice 1b** — nothing yet *answers* an elicitation
  (human/sibling-context routing, timeout policy). Design pass, not a patch.
- **`structuredContent`/`outputSchema` on `shell`** — `ShellCompletion::to_json`
  still hand-rolls its envelope into a `TextContent` string; no
  `Tool::with_output_schema` usage found in `mcp/servers/shell.rs`.
- **Tasks (SEP-2663)** for outbound long calls — not built; `enable_tasks`
  doesn't appear anywhere.
- **No automatic reconnect** — `reconnect()` (`external.rs:468`) still has no
  caller; a dead server stays Down until `kj mcp reload`.

## `kj backend` has no health check (re-filed 2026-08-11)

Still true: no `doctor`/`check` verb exists in
`crates/kaijutsu-kernel/src/kj/backend.rs` (verified absent). Wants
something like `kj backend check <name>` (or `--check` on `kj backend
list`) that probes a configured endpoint and reports reachability + model
list.

---

## Background exec → kaish's job system — doctrine now contradicts the code (2026-08-07)

`background_exec.rs`'s module header (last touched 2026-09-04) now argues the
migration is structurally wrong — a per-call `EmbeddedKaish` can't host a job
that outlives the call, and kaish's job streams are ephemeral/in-process —
and presents `spawn_background` as the deliberate, permanent design.
CLAUDE.md's "Host exec has one owner" doctrine still names only the MCP
stdio launch as the sanctioned exception; `background_exec.rs` isn't named
there. The kaish-side worktree this entry was blocked on
(`~/src/wt/kaish-jobs-embedder`) no longer exists.

**Needs Amy's word:** either name `background_exec.rs` as a second
sanctioned exception in CLAUDE.md, or reopen the migration. Nothing here
decides it.

---

## Ambient command center — trace packets, switchboard follow-ups (2026-08-10)

Still unbuilt: the trace-packet/comet system (concepted, not built — no
`RouteRegistry`/mode-2 pulse slots in `TraceGlowMaterial`); switchboard
placement (south wall still sits behind the default room camera); switchboard
polish (recency dynamic range still narrow); an ambience agent (idea only).

Verified changed since filing:
- **Switchboard slow leak — now intentional, not a bug.**
  `SwitchboardState::retain_relevant` (`view/room/switchboard.rs:333`) keeps
  an off-roster context's sticky ember on purpose, pinned by
  `retain_relevant_keeps_offroster_embers_and_drops_idle_signals`.
- **Turn-comet/seat-flare enabler** (principal↔peer correlation) is still
  missing — same open item as "Seats-at-the-table" below; don't duplicate
  the fix.
- Runner still runs `cargo watch` (`contrib/kaijutsu-runner.sh`), not
  watchexec — moltar-deaf status unverified this sweep.

---

## Peer-registry doctrine (2026-08-10, peers-plumbing)

**Resolved since filing:** the nick-goes-stale-on-relabel bug is fixed —
`PeerRegistry::attach` now keys on `instance` alone when present
(`peers.rs:45` `peer_key`), so a re-join under a stabilized label replaces
the old entry instead of duplicating it.

Still open: the wire `PeerInfo` struct (`kaijutsu.capnp:1622`) still carries
only `nick`+`attachedAt` — no `instance`/kind field — even though
`PeerConfig` gained `instance` (`:1617`). `listPeers` still can't
distinguish two peers sharing a nick from each other.

---

## Seats-at-the-table follow-ups: nameplate LOD, turn-event flare (2026-08-10, seats)

Both still open:
- **LOD-gated nameplate on well-zoom** — needs a nearest/under-cursor wisp
  pick, not built (`view/room/seats.rs:8` still points back to this file).
- **Turn-event flare needs principal↔peer correlation** —
  `ServerEvent::TurnCompleted` still carries only `principal_id`, no peer
  nick/instance; no correlation exists in kernel or app (grepped, no hits).
  Same enabler needed by "Ambient command center" above — don't duplicate.

---

## FlowBus backpressure — what the 2026-08-05 rework left open

Per-subscription bounded queues, `subSeq` and the lag kick shipped. Open:

- **No catch-up for a subscriber that was not there.** `flows.rs` has no
  catch-up path; losslessness is a promise to live subscribers only.
- **Only `slowSubscriber` is ever sent.** `serverShutdown`/`superseded`
  exist on the wire (`kaijutsu.capnp:774`) but nothing emits them
  (`context_feed.rs:286`, `rpc.rs:11425`), so a clean shutdown looks like an
  ordinary disconnect.
- The ACP adapter's defensive sweeps (`kaijutsu-acp/src/session.rs:495`,
  `update.rs:1586`) are dormant defence-in-depth; remove once real flights
  show them firing zero times.

## lfm2d escalation: the shell gate that scores `shell_write` (wired 2026-08-24)

The hook body (`assets/defaults/rc/lib/hooks/lfm2d.kai`) re-derives each
secondary signal's verdict, exempts read-only `kj` and `kj ledger`, and
requires ladder position 0 plus a label match before auto-allowing. No
self-approval and approval-executes are canonical in
`docs/gate-and-shell-split.md` and `docs/gate-resume.md`; the layered policy
tiers are `docs/gate-policy-tuning.md`. Old measurements in this entry's
history must not be quoted. Open:

- **Our `kj` verbs are out of distribution** for the classifier
  (`docs/kj-verbs.md`); the lfm2d lane needs them in its truth set before an
  auto-allow band can cover our seats. Cross-project.
- **Widen the probe with real traffic** (`LFM2D_MODE=log` for an interval)
  and **whether to enable an auto-allow band at all** — both Amy's call.
- **A reformulated command does not carry its pending ask forward.** A
  `retry-after-ask` ledger row is the minimum (a measurement, not a
  control). Unbuilt.
- **A kaish lexer rejection degrades the gate to the no-plan fallback**
  (~16x noisier). `contrib/kai-parse-check.sh` guards our own corpus; the
  lexer bug is kaish's (`gotcha_kaish` in memory has the shape).

## Cast follow-ups (seeded 2026-08-03)

- No `kj fork --cast` (`kj/fork.rs` has only `--preset`).
- No consumer of `cast_slots.loadout` outside the stored column.
- `available_models()` is hand-maintained per provider
  (`llm/mod.rs:899`) though a live Models API lookup exists for the context
  window (`llm/claude/models_api.rs`).

## Two live-log papercuts (seeded 2026-08-04)

Both fire on every kernel boot: "Document already in DB but not in memory,
recovering" is still `warn!` on every `kj context create`
(`block_store.rs:420`) though the benign arm is now distinguished from
`DocumentDiverged`; four backends warn `api_key_file configured but
unreadable` (`llm/config.rs:281`) for a `~/.openai-key` that does not exist
and fall through to env silently. Fix the rows or make an unreadable
`api_key_file` a load error.

## MCP subsystem — audit follow-ups (2026-07-29)

- **`InstancePolicy` does not persist across restart** —
  `Broker.policies` (`mcp/broker.rs:125`) is a bare map, so a live-tuned
  `call_timeout_ms`/`max_result_bytes` reverts on restart.
- **No project-instructions discovery** (CLAUDE.md/AGENTS.md analog).
  `build_system_prompt` (`llm/system_prompt.rs`) assembles base + rc `.md`
  + `<situation>` with no filesystem crawl.

## MIDI device profiles: routing does not consume port roles (`docs/midi-next.md` slice 2)

`PortRoleMatch` exists (`kaijutsu-audio-runtime/src/midi_match.rs:88`), but
`kj midi send`/`identify` still route to a device's FIRST matched port
(`midi_exchange.rs:86,524`, `dj/midi.rs:92,114`). Demonstrated wrong on the
MiniBrute, which answers identity only on port 1. Also unfilled: USB
`vendor:product` enrichment (`midi_in.rs:194`, `usb_id` left `None`), so
matching is name-substring only.

## FSN landscape follow-ups (`docs/scenes/vfs.md`)

`docs/scenes/vfs.md` names this file as its tracker. Open: listings are
cached forever (`view::fsn::sync::FsnState`; `VfsActivityEntry.generation`
makes a per-cell stale-detect buildable, inotify is the real fix); the `/`
fetch (depth 2, 4000-entry cap) truncates before late children get a field;
`Screen::Fsn` dive is keyboard-unreachable on purpose — resurface or delete
the screen.

## `rich_json` is unbounded on the wire (seeded 2026-07-18)

`block_output_data` (`kaijutsu-server/src/rpc.rs:9080`) persists `.data`
whole with no size check, bypassing kaish's text-only output limiter. A size
ceiling that fails loud, or CAS routing like `RenderCue`'s `casHash`.

## External MCP servers — no `kj mcp restart <name>` (seeded 2026-07-30)

`reconcile_with_toml` (`mcp/external_registry.rs:122`) never reconnects an
already-running external server on `kj mcp reload`; only `InstancePolicy`
refreshes. Tradeoff in `docs/external-mcp.md` "The reload design fork".

## SFTP over the VFS: appends can clobber each other (`docs/sftp.md`)

`write` (`sftp.rs:566-576`) does one `getattr` for both the generation guard
and the APPEND offset, so two cross-session appenders can both read gen=N
and lose an update. Wants an atomic append: `VfsOps` has no append
primitive (`write_all` is truncate+rewrite, `vfs/ops.rs:158`), which would
also make `>>` and jsonl logs cheap. `opendir` materializes a whole
`readdir` per handle (no pagination); the post-write re-getattr race is
accepted in code (`sftp.rs:585`).

## Audio nodes — follow-up after daemon extraction

- **Keep jobs are in-memory on both sides** — `KeepJobs`
  (`crates/kaijutsu-kernel/src/kj/audio_capture.rs:18`, still a bare
  `Mutex<BTreeMap>`) and `takes.rs::Pool`: after a kernel restart nothing
  can rediscover a job's `/tmp/kaijutsu-audio-<uuid>` staging path. Persist
  the job row (id, node, instance, path, phase) before a recovery verb is
  worth building.
- Profile edits load only on daemon connection/reconnection; raw port
  reconciliation does not refresh profile files.
- Distinguish ingress data loss from lost topology notifications — the
  conservative reset replaces generations on every startup burst, not only
  real loss. See `docs/audio-daemon.md` "Evolution" for the execution
  checklist (node inventory, retained windows, capture export).
- A timed-out RPC (10s) flips `connected=false` and re-sends
  `MetronomeConfig` to the DJ mid-play; should reload config on reconnect
  only, treat a timeout as a retry.
- `--context` is validated once at startup; an archived capture context
  just makes `commit_capture` warn every four seconds forever instead of
  stopping.
- Named render destinations: playback broadcasts to every attached render
  client; multiple machines need an explicit destination contract.
- Kept-take recovery after kernel restart, and explicit handling of a
  permanently lost daemon instance — do not add a broad `/tmp` sweep;
  ownership recovery must name exact job paths.
- Generic `kj` help still understates MIDI verbs; the live subcommand help
  is more complete.

## Architecture & System Design

- **`rpc.rs` is ~13,000 lines and growing.** Split the Cap'n Proto trait
  impl by domain (`rpc/vfs.rs`, `rpc/llm.rs`, `rpc/mcp.rs`).
- **Reasoning-continuity guard, policy not built:** refuse `kj context set
  --model` across provider families when signed Thinking exists in history;
  allow the transition only at `fork`.
- **Per-principal budgets and fair queuing** are deferred by name
  (`mcp/servers/policy_admin.rs:12`); a broadened role loadout reaches a
  live context only on re-create or restart.

## Drift UX — cross-session ergonomics (2026-08-12)

Design record: `docs/drift-ux.md`; the newer `docs/drifting-dead-letters.md`
(2026-08-16/17) now owns most of drift's architecture backlog. Still open
and not covered by either doc:

- **A received drift is a dead end for reply.** Hydration surfaces the short
  id only (`llm/hydrate.rs`), never the sender's label or a reply hint.
  Not the cheap fix it looks like: `translate_block` has no DB access, and
  a stamped label would go stale when `stabilize_context_label` renames
  `cc-*` contexts. Wants a label snapshot passed in, not a DB handle.
- **Drift edge metadata is inconsistent across delivery paths** — still true:
  immediate push stamps `drift_kind.to_string()` (`kj/drift.rs:406`,
  `"push"`), flush stamps `format!("{}#{}", drift.drift_kind, drift.id)`
  (`drift.rs:793`, `"push#1"`). `kj drift history` cannot uniformly trace an
  edge back to a staging event.
- **MCP connections that never receive hook traffic mint a context that
  never stabilizes/archives** (found 2026-08-12 during the cc-* sweep) — a
  `kaijutsu-mcp --connect` probe with no session gets no `session.end`, so
  nothing reclaims it. Options: lazy registration on first hook event, or
  archive-on-drop for a connection that never stabilized.
- **Arriving drift honoring `--drive`** — the receiver-side per-context
  "honor drive requests" setting (default off) described in the 2026-08-12
  ruling is not built; no code found for it. Its blocker (the rc-lifecycle
  identity smear) is fixed — see Drive gates below — so this is now
  buildable, just not built.

## Drive gates — external drive still ungated (2026-08-12)

Self-drive is gated (`Capability::Drive`, `kj/drive.rs:61-64`). The archived
check shipped: `kj drive` refuses `Concluded`/`Staging`/archived targets,
tested (`drive_refuses_an_archived_context` and siblings,
`kj/drive.rs:274-320`). Still open:

- **External-drive gate (caller != target)** — the genuinely new per-context
  gate, default off with `context_type` defaults via rc. No
  `ExternalDrive`/honor-drive code found; not built.
- **Cold-cache suppression** — computable with no new schema
  (`context_usage.updated_at` vs. the shortest `cache_breakpoints` TTL,
  `kj/cache.rs:138-141`) but not implemented. Refusals must be loud, with a
  way to insist (cold-cache is a cost signal, not a correctness one).

## Graceful-shutdown WAL checkpoint on SIGTERM

`SharedKernelState::drop`'s checkpoint runs only on clean exit
(`kaijutsu-server/src/rpc.rs:384-407`; the comment there points at this
entry by name). Proactive compaction covers durability; the gap affects
bare-file forensics between the last compaction and shutdown. Related:
`kj db backup`/`checkpoint` exist (`kernel_db.rs:3236`), an export/import
round-trip that rewrites every record in the current codec does not.

## App: `kj drive` on a non-OODA-armed musician silently discards its ABC

`on_turn_completed` (`kaijutsu-server/src/beat.rs:2155-2172`) returns early
with no log when `!ac.attachment.ooda_armed`, unlike the ephemeral/excluded
guard right below it. Either crystallize driven turns regardless of the arm,
or log loudly. Related: musician create-rc auto-attaches to a label-derived
track before an explicit `--track` can move it (no `--track` passthrough on
`context create`). The tracker station still has no score cells
(`view/tracker/mod.rs:4`). The dock sparklines' data source is a placeholder
(events/sec, running-block count); decide what they mean before polishing.

## Control plane (kj): four real gaps

- **Dead `--json` fields.** `doc list`, `config list|show`, `rc list|show`
  and `search` declare a local `json: bool` (`kj/doc.rs:48`, `config.rs:53`,
  `rc.rs:72`, `search.rs:52`) that `KjBuiltin::execute` strips before
  dispatch, so the richer branch never fires. Wire it or delete it when
  touching one of those files.
- **`--out` writes bypass the VFS.** `kj cas get` (`kj/cas.rs:148`) and
  `kj block cat` (`kj/block.rs:1028,1155`) `std::fs::write` relative to the
  server cwd, never through mounts.
- **No `kj db tables|schema|dump`** for the "kernel has the answer but will
  not tell you" case; `kj db` is backup/checkpoint only.
- **`--type` exists on `context create` only, not on `fork`**, and
  `context create --parent` copies zero blocks. Open question: should
  `kj fork --type <T>` exist for "branch into a director/toolie".

## Index and ABC: two schema-shaped debts

- **Synthesis and embedding tables lack `ON DELETE CASCADE`** in
  `kernel_db.rs` (other tables have it); deletes are manual across three
  tables. Do it at the next schema change.
- **`OnnxEmbedder` is BERT-only** (`kaijutsu-index/src/embedder.rs:43-45`
  hardcodes `input_ids`/`attention_mask`/`token_type_ids`); E5/jina models
  will not load. The `kaijutsu-abc` MidiWriter leaves pitch/velocity
  unmasked (`midi.rs:970-995`), safe while the one caller uses velocity 80.

## `docs/abc-reference.md`'s support matrix is four months stale

The ABC v2.1 reference maps notation to `kaijutsu-abc` support status as of
2026-05-25; 27 commits have touched the crate since, including the June 30
conformance push. The crate's tests are truth; re-derive the matrix from
them or drop the status columns.

## Time well: two stubs (`docs/timewell.md`)

The horizon dive handler logs "not yet built" (`view/time_well/scene.rs:1293`)
though `docs/horizon-dive.md` exists; pause gating persists `paused_at` and
dims the card but no beat/OODA wakeup gate or turn-start refusal is wired.
Stages 4 and 5 of the plan are open in the doc.

## Hyoushigi / Musician — open remainder

`docs/midi.md`, `docs/pcm.md`, `docs/chameleon.md` and `docs/fork-filters.md`
carry the mechanism; these are what none of them cover:

- **No CAS write surface (client→kernel put).** `/v/cas` is read-only
  (`vfs/backends/cas.rs`) and `commitCapture`'s `Cas(hash)` arm refuses
  (`rpc.rs:8546`). Needed at the first heavy payload.
- **Perception is notation-only.** `KJ_HEARD` is ABC; no `MidiToAbcDeriver`,
  so a captured MIDI window is invisible to a model.
- **No chart is seeded into a player's context**, and the OODA Act is
  hardwired to ABC (`schedule_abc_cell`, `hyoushigi/mod.rs:445`).
- **Players get no tools at all.** A read-only kaish (kaibo's posture) would
  remove the tool-palette-hangs-small-models cliff by construction and give
  bar math an escape hatch. Decide which RO builtins.
- **Rotate chains pollute the director's tree** (`kj context list --tree`
  shows a 17-deep chain per song); no `--hide-archived` or chain folding.
- Turn provenance collapses to `PrincipalId::system()`
  (`hyoushigi/mod.rs:1215`); character slice 3 is the fix.

## VFS: `LocalBackend::resolve` blocks the tokio pool (found 2026-06-27)

`resolve()` (`vfs/backends/local.rs:150`) is `async fn` but canonicalizes
synchronously on every op with no `spawn_blocking`. Under a stalled host FS
this starves the ambient pool, which is the path the SSH-in-when-the-app-is-
down fallback depends on. Route `resolve`/`create`/`mkdir` through
`spawn_blocking` or `tokio::fs`.

## Archive-time summaries, written by a local model (Amy, 2026-08-03)

Not built. Generate one small summary when a context archives (frozen input,
no invalidation problem); good local-model work. Open: where it lives
(handle field vs. a block), which model, whether conclude/demote get it too.

## kaijutsu-mcp Remote backend collapses multi-context ops to one context

`context_ids()` (`kaijutsu-mcp/src/lib.rs:844`) returns only the joined
context for `Backend::Remote`, so a global search silently skips every other
context; resource/prompt handlers hardcode `kind: "Conversation"` for Remote
(`lib.rs:2871,2918`).

## Testing & Tooling

- `vfs::backends::local::tests::test_normal_paths_succeed` is flaky under
  full-workspace parallelism (found 2026-08-02).
- `contrib/kaijutsu-runner.sh` rebuilds only `kaijutsu-app`; a wire change
  still needs `kaijutsu-server` and `kaijutsu-mcp` rebuilt by hand
  (`docs/operating.md`).
- `docs/kj-help/` siblings (`kj-cache/context/drift/fork/preset/workspace.md`)
  predate the clap migration and nothing in `crates/` reads them; only
  `kj.md` is `include_str!`'d. They drift: `kj-context.md` lacks seven
  subcommands, `kj-preset.md` lacks `reseed`, `kj-fork.md` still teaches
  the retired `--shallow`/`--depth`. Delete them or wire them as
  `kj <cmd> help` bodies and regenerate from the clap tree.

## `ExecResult.output` cannot carry structured data past kaish's output limiter (found 2026-07-18)

Still true on kaish 0.17.1: `materialize()` (`kaish-types/src/result.rs:504`)
clears `.output` unconditionally even when `.out` never consumed it. `kj`
works around it by writing only `.data`, bridged at `block_output_data`
(`kaijutsu-server/src/rpc.rs`), regression-pinned in `kj_builtin.rs`. The
clean fix is upstream: clear `.output` only inside the `if .out.is_empty()`
branch.

## Conversation-view latent costs (surface slice 0, 2026-08-18)

- **`ConversationGeometry::reconcile` still never retries a skipped block.**
  On a `None` seed it `continue`s without adding the id to `rows`/
  `block_index`, but `self.block_ids = ids.to_vec()` (`view/geometry.rs:415`)
  still records the full incoming list — so `ids_match` reports no change
  next frame and `sync_conversation_geometry` never calls `reconcile` again
  for that id. The block gets no row until the doc version moves for some
  unrelated reason. Real bug, still open.
- `sync_conversation_geometry` still calls `recompute_offsets()`
  unconditionally through `Mut<_>`, marking `ConversationGeometry` changed
  every frame. Nothing consumes `Changed<ConversationGeometry>` today
  (verified) — the first consumer that tries will be silently defeated.

## `BlockContentCache` is still unbounded (surface slice 3, 2026-08-18)

`view/surface/content.rs:253`'s `BlockContentCache` still only evicts blocks
that leave the document — it grows with scroll depth, a second copy of the
conversation's rendered text alongside the block store's own. Not urgent
(strings, not glyphs; no frame-cost impact — every consumer iterates a
geometry band). Fix is the same pinned-window LRU `shape_cache` already
uses, at a different band (content ±2 screens vs. shape ±1).

## A streaming rich block still re-parses and re-draws whole, every tick (surface slice 4, 2026-08-18)

Confirmed still true: `view/surface/content.rs:9` documents "no streaming
debounce here" by design for the incremental-prefix path, but drawn rich
kinds (ABC, diff, sparkline, SVG, image; `RichKindInfo::is_drawn()`) still
shape as one chunk, whole, on the main thread every content bump — no
debounce, `rich.rs`. Bounded (budgeted parsers), not free; nobody has
measured a streamed diff on this path yet. Fix if it bites: a debounce
scoped to `is_drawn()` blocks in `Running` status only.

## Text effects on the surface: the instance buffer is the map (2026-08-18)

Answered design question (Amy asked whether shader-driven colored text can
come back): the glyph instance buffer (per-glyph doc position, quad, UV,
color) already is the "map of text and positions" — effects return as
per-instance attributes + glyph-shader work, not texture post-processing.
Rainbow = hue(doc pos, time); halo/glow = widen MSDF distance thresholds.
Cross-glyph effects (blur, distortion) are the one class needing a texture:
draw to an intermediate layer, composite with a post shader if ever needed.
Not built; this is the intended route when rainbow/halo return.

## ANSI rendering, pass 1: four corners left open (2026-08-19)

Stage 3.2/3.3 (`StyleSpan` → parley ranged brushes → `PositionedGlyph`) is
shipped. Still open, all verified against current code:

- **Italic (SGR 3) is parsed but never rendered** — no `FontStyle`/`Italic`
  handling found anywhere in `kaijutsu-app`'s text/view code. It's the one
  attribute that changes shaping, so it needs a column-alignment decision
  first.
- **ANSI spans are dropped on a block detected as a drawn rich kind** (ABC,
  SVG, sparkline, diff, image) — those shape through `RichShaper`, which
  never sees the styled-span list. Doesn't arise from today's ingest hooks
  (shell output is plain).
- **`INVERSE` is resolved CPU-side and stale until re-shape** — an inverted
  span bakes both colors, carries `style_index = 0`; the theme epoch forces
  a re-shape within a frame or two.
- **Backgrounds/underlines bake color into vertices** — `ShapeKey::
  baked_theme_epoch` exists for exactly this reason.

## `journal_op` is still not a transaction (pre-existing, surfaced 2026-08-19)

`BlockStore::journal_op` (`kaijutsu-kernel/src/block_store.rs:961`) still
issues `append_op` and `touch_context_activity` as separate autocommit
statements under a mutex — no `BEGIN`/`COMMIT`. Not urgent (the mutex
serializes, a crash gap is detectable) but every new hook-site write
inherits the pattern. Fix shape: a `KernelDb` method wrapping
`self.conn.transaction()`, like `write_snapshot_and_truncate`.

## Oplog replay clears spans that live appends keep (2026-08-19)

`BlockContent::append_text` (end-append) keeps `style_spans`; `merge_ops`
replay still applies journaled appends through `edit_text`
(`blocks/block_store.rs:993`), which clears spans unconditionally
(`block.rs:2872`). Harmless under buffer-until-done ordering (spans land
after all appends); wrong the day spans land before later appends. Verify
whether `updated_snapshots` replay (which may run last) already repairs
this before building anything.

## vte 0.15.0 drops a control byte after a chunked partial UTF-8 codepoint (2026-08-19)

Upstream bug in `vte::Parser::advance_partial_utf8` (not `kaijutsu-ansi`):
`strip(&[0xCD, 0xAE, 0x1B, 0xFF])` differs when fed as one chunk vs. two —
chunked feed leaks an extra replacement character and silently drops the
`0x1B` (ESC) that followed a resumed multi-byte codepoint. Narrow trigger
(incomplete UTF-8 lead byte as the last byte of a `feed` call, continuation
immediately followed by a control byte in the next) — exactly the shape of
chunked kaish output mixing multibyte text and escapes. Pinned as a
regression test asserting the *current* buggy divergence:
`crates/kaijutsu-ansi/tests/vte_partial_utf8_regression.rs` — a `vte`
upgrade that fixes it fails this test loudly. Not worked around in
`kaijutsu-ansi` (would mean reimplementing vte's UTF-8 resumption). Fix:
file upstream against `alacritty/vte`, or vendor-patch if needed sooner.

## Capability names and layout need a redesign sweep (Amy, 2026-08-20)

Still unaddressed: `Capability` (`kaijutsu-kernel/src/mcp/binding.rs:133`)
still mixes `Instance`/`Tool{instance,tool}`/`Facade` (granular), `Admin`/
`AllInstances`/`AllFacades` (broad), and bare-word verb authorities
(`Drive`/`Fork`/`Drift`/`Transport`/`Operator`/`ConfigWrite`/`Exec`/
`Editor`) — three different shapes, grown ad-hoc as gaps were found. Amy:
"Director should only get `shell` as long as it has `kj`" — director's
broad facade/exec grants (`assets/defaults/rc/director/create/S10-binding.kai`)
are worth revisiting once `kj` itself can reach what a shell used to be
for. "We'll do a cap redesign sweep soon so it's a good time to
experiment" — treat `Editor` as provisional until that sweep.

