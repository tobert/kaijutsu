# Gemini CLI feature comparison — candidate work (2026-06-23)

**Moved out of `docs/issues.md` on 2026-08-11.** This is a competitive-analysis
snapshot, not a work backlog: of ~35 candidates, a 2026-08-11 verification pass
found exactly one shipped (extended-thinking wiring, since removed from this
list) and one self-resolved. The rest are untouched two months on — not
"done and forgotten" but "never started". That is fine for a wishlist and wrong
for a backlog, where 323 lines of unstarted candidates crowded out live work.

Nothing here is dead. Pull an item back into `docs/issues.md` when it becomes
actual work. The decisions this survey *did* produce — the cache-placement
rules — stayed behind in `issues.md` as real backlog.

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
  The completion half is now available to a client: `subscribeTurnEvents` pushes
  a structured stop reason (end_turn / cancelled / max_tokens / max_iterations),
  which is exactly the machine exit code this wants.
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
