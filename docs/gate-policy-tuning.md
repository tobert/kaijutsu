# Gate policy tuning — one layered list, one runtime path

Status: designed 2026-09-08; slices 1–3 (the evaluator seam, the config
layer, learned family rules — `kj/gate_policy.rs`,
`assets/defaults/gate.toml`, `approval_rule_families`) shipped 2026-09-10,
slice 5 unbuilt, slice 4 retired by the verb class. Amy's rulings of the same day are quoted
where they decide a shape. Reviewed against the live tree by kaibo (cast
`crusoe`) the same day; the revision absorbs its findings. This doc is
canonical for the gate-policy evaluator; `docs/gate-and-shell-split.md`
remains canonical for the gate and the shell themselves.

## The rulings this design sits on (Amy, 2026-09-08)

1. **The explicit list from the user overrides the builtin and generated
   lists.** A human decision recorded in the ledger outranks everything
   shipped or configured.
2. **`context_type` is another layer on top of global.** Config composes
   global first, then one section per context type.
3. **Start with `kj`; add a few non-`kj` entries to test the key space.**

## The problem: three checkers, three sources, no one path

A `shell_write` submission meets three separate allow/deny checkers today,
each reading a different store, and they do not agree:

| Checker | Reads | Where consulted |
|---|---|---|
| PreCall exemption | const tables, `kj/readonly.rs` | broker PreCall only (`mcp/broker.rs`, `evaluate_phase_with_mode`) |
| Hook exemptions | jq filters over `KJ_TOOL_PLAN` | `assets/defaults/rc/lib/hooks/lfm2d.kai` |
| Rules redeem | `approval_rules` (SQLite, digest-keyed) | `run_gate` (`kj/gate.rs`), which both ask origins already flow through: the shell gate (`mcp/servers/shell.rs:497`) and hook escalation (`mcp/broker.rs:2323`) |

The consequences, each observed live rather than predicted:

- **The stacks disagree.** `program_is_gate_exempt` exempts read-only `kj`
  and the whole `kj ledger` verb from hooks, but `run_gate` never consults
  it — through the MCP `shell_write` tool, a read still escalates to a human.
  Invisible today only because models rarely shell out for pure reads and
  answering seats ride the RPC paths.
- **The dynamic layer cannot generalize.** `approval_rules` is strictly
  digest-keyed (`approval-ledger/src/schema.rs:1586`), and guarantee 3
  refuses an allow rule on any statement with a free variable. "Always allow
  `kj handoff note`" — for any note text — is not writable. Every distinct
  note re-asks.
- **There is no config layer at all.** Changing which calls skip the ask
  means editing Rust tables (a rebuild) or jq inside a hook body (a second
  copy of the same rule — `kj/readonly.rs` module doc already flags the
  `kj ledger` duplication as "can go when that hook is next edited").
- **The friction is real**: `kj handoff note` escalates from MCP seats and a
  same-seat answer is refused, so every note needs a second seat
  (`docs/issues.md`, "The lfm2d gate escalates `kj handoff note` from the
  MCP shell").

## The design: one evaluator, layered stores

One function composes every layer into a per-statement verdict, and the two
existing pinch points consult it — nothing else grows a checker.

```text
gate_policy::evaluate(planned_statements, ctx)
    -> per-statement Allow { source, key } | Deny { source, key } | Uncovered

composition per statement:
  1. resolve the statement's key (below); unresolvable -> Uncovered
  2. walk the layers top-down; the first layer with a verdict for the
     most specific key wins; within one layer the more specific key wins
     (a digest rule beats a family rule), and deny beats allow at equal
     specificity
  3. structural refusals veto every family-keyed Allow (below)

composition per program: the existing AskCoverage semantics — a Deny
  anywhere denies the whole submission, every statement must be Allow to
  auto-allow, anything else escalates. Never partially applied.
```

At broker PreCall the evaluator runs **before** the hook snapshot, in this
order: any statement Deny → the call is refused (a `PhaseOutcome::Deny`
whose `subject` reads `gate policy` — there is no hook id, and the reason
names the layer and key, so the refusal stays distinguishable per
`docs/gate-and-shell-split.md`, ruling 2, with no new `RefusalKind`); every
statement Allow → hooks are skipped, replacing today's
`program_is_gate_exempt` consult; anything else → hooks run as they do now.
All three RPC paths reach this: `execute`, `execute_shell_command`, and
`execute_kj_command` each evaluate PreCall with `tool = shell_write`
(`kaijutsu-server/src/rpc.rs:3625`, `:9738`, `:10163`).

Inside `run_gate` the evaluator replaces the direct `rules::redeem` call as
step 1. The archived-context check still runs first and is never bypassed by
an auto-allow; `find_redeemable` still runs only on an Escalate; every
auto-decision still commits its durable row before returning, with
`auto_reason` now naming the winning layer and key instead of today's
`rule coverage: …` text — a visible change in `kj ledger show`, and a
slice-1 observable on purpose.

**Origin boundary.** The builtin and family tiers classify a *planned
command tree*, which `GateSpec` carries for `ShellGate` and shell-shaped
`Hook` origins but not for `KjVerb` (`kj cc send` builds its own spec). For
a `KjVerb` ask the evaluator consults digest rules only and returns
Uncovered otherwise — today's behavior, stated rather than discovered.

### Layers, bottom to top

| Layer | Store | Materialized by | Who authors it |
|---|---|---|---|
| 1. Builtin | each verb's declared `Effect` (`kj/effect.rs`, §Builtin tier) | compile | shipped in-repo, reviewed like code |
| 2. Global config | `/config/kernel/gate.toml`, `[global]` | host file, seeded from `assets/defaults/gate.toml` via `config_seed.rs` | Amy, at the keyboard, no rebuild |
| 3. context_type config | same file, `[context_type.<type>]` sections | same | Amy, per role |
| 4. User explicit | `approval_rules` (digest) + family rules (§Learned family rules) | `kj ledger allow --remember` at answer time | a human decision, attributed and revocable |

Top wins (ruling 1). A learned human allow overrides a config deny; a
config deny overrides the builtin allow tier. Every auto-decision leaves the
durable row it leaves today, with `auto_reason` naming the winning layer and
key — `gate policy: context_type config (coder) allows family kj handoff
note` — so `kj ledger show` says which list decided.

### The key space

One shape for every program, kj or not: **(command name, optional first
positional token)**.

- `kj` keys resolve through the same six structural conditions
  `is_read_only_kj` enforces today (`kj/readonly.rs`, `is_read_only_kj`): name exactly
  `kj`, no redirect, no background, no heredoc, every argument
  `PlannedValue::Plain`, and the argv classifies (`kj::classify`, which
  parses the whole argv through the verb enums). A kj key that names no
  live leaf fails a test, not silently.
- Non-kj keys are free-form command names (`rg`) or name-plus-first-token
  (`git push`). Nothing reflects over host PATH, so a typo is a no-op —
  which fails safe, toward the default ask.

### Structural refusals veto family allows, never digest rules

A family key classifies the verb; it cannot see the arguments. So every
Allow from layers 1-3 and every *family* rule in layer 4 inherits the six
conditions: a redirect, a background flag, a heredoc, or a non-plain
argument drops the statement to Uncovered and it meets the classifier and
the gate as usual. `kj handoff note 'x' > ~/.bashrc` is not an allowed
`handoff note`; it is an uncovered statement with a redirect.

A digest-keyed rule is the exception, and it is today's behavior kept
deliberately: the human saw that exact rendered text — redirect and all —
on the ask they answered. Exact text, exact authorization.

### Verdicts

- **Allow** — no ask from any checker that consults the evaluator. Broker
  PreCall skips hooks for a program of all-Allow statements (this replaces
  `program_is_gate_exempt`); `run_gate` auto-allows with the durable row.
- **Deny** — refused at broker PreCall for both stacks (the RPC paths
  evaluate PreCall too, so a config deny reaches a human at their own
  keyboard exactly like a model — the existing doctrine,
  `docs/gate-and-shell-split.md`, "The three rpc.rs shell paths take the
  hook path"). The refusal names the layer and key. Leaves a durable row
  when it happens inside `run_gate`.
- **Ask** (a statement the config marks ask-tier) — never auto-allowed by a
  lower layer; the existing ask machinery of each stack does the asking, and
  in the hook stack the tier is firm rather than advisory (the mechanism
  follows below). A tier the classifier could vote down is not a tier.

**A tier allow is not a capability grant.** `require_cap` and the loadout
run exactly as before; the evaluator decides whether an *ask* fires, never
whether a verb exists for this seat. `kj drive` in the allow tier still
needs `Capability::Drive`.

**Scope boundary: the evaluator governs shell submissions, not kj-verb
dispatch.** A `kj` call made *inside* a hook body or another kj verb (the
lfm2d hook's own `kj ledger signal add`, `lfm2d.kai:320`) reaches the
kernel through the `KjDispatcher`, which never passes through broker
PreCall or `run_gate`. A deny tier on `kj ledger signal add` catches a
`shell_write` submission containing that text; it does not, and cannot, block
the hook's audit call. That path keeps its own controls (`require_cap`).

To make Ask firm in the hook stack, `KJ_TOOL_PLAN` grows a per-command tier
field beside `kj_readonly` — `allow` / `ask` / `score`, computed by the same
evaluator when the broker builds the plan (`mcp/broker.rs`, `run_kaish_hook`), so
there is one classification with two consumers. The lfm2d hook gains **two**
rules, and both are load-bearing for slice 5:

- an `ask`-tier clause exits 3 naming the tier, regardless of the
  classifier's verdict — a tier the classifier could vote down is not a
  tier;
- an `allow`-tier clause is **dropped from the scored clause set**. The
  broker skip only fires for a program where *every* command is Allow; a
  mixed program (`kj block list; rm -rf /tmp/x`) still reaches the hook, and
  without this rule the classifier re-scores the allow clause — and it does
  escalate on reads (`kj block read` measured `situation-normal` 0.791,
  `kj/readonly.rs:6`) — blocking a submission the evaluator allowed. This
  rule is what replaces the hook's `kj_readonly` and `kj ledger` jq
  exemptions; deleting them without it is unsafe.

## The file

`/config/kernel/gate.toml`, seeded from `assets/defaults/gate.toml` through
`config_seed.rs` like `mcp.toml` and `theme.toml` — the running kernel reads
the host file, so an edit lands with no rebuild and no reseed
(`docs/config-namespace.md`). The seed writes only into an empty tree, so
an install that predates the file gets it by `kj config reset gate.toml`
or by copying the asset into the config directory; until then the config
layer is empty and `kj ledger rules` says so. The example is the rule:

```toml
# Gate policy — the tuned allow/ask/deny tiers beneath the ledger's
# per-decision rules. Keys: "kj <verb> [<subcommand>]" or
# "<command> [<first-token>]". Verdicts: allow, ask, deny.
# Layers compose: [global], then [context_type.<type>] over it.
# A learned ledger rule outranks everything here.

[global]
allow = [
  "kj handoff note",     # recoverable by construction; the friction that started this
  "kj block create",
  "kj block append",
  "kj stage include",
  "kj stage exclude",
  "rg",                  # non-kj test entries: one-level key
  "wc",
]
ask = [
  "kj rc add",           # rc reaches every future context — a human sees each change
  "kj rc rm",
  "kj config reset",
  "git push",            # non-kj test entry: two-level key
]
deny = [
  "dd",                  # no kaijutsu seat has a reason; uniform loud refusal
]

[context_type.explorer]
deny = [
  "kj context create",   # an explorer explores; lifecycle belongs to its parent
]
```

Unknown sections or verdict words fail the load loudly, naming the section
and key (a TOML syntax error names the line) —
a policy file that silently half-parses is the failure shape this repo does
not ship.

## Learned family rules

The dynamic layer grows one table beside `approval_rules`:

```sql
CREATE TABLE approval_rule_families (
    rule_id      TEXT NOT NULL PRIMARY KEY,
    program      TEXT NOT NULL,           -- "kj", "git", ...
    subcommand   TEXT,                    -- NULL = whole-program family
    allow        INTEGER NOT NULL CHECK (allow IN (0, 1)),
    scope        TEXT NOT NULL CHECK (scope IN ('session', 'always')),
    context_id   BLOB,                    -- session scope only
    principal_id BLOB,                    -- session scope only
    created_at   INTEGER NOT NULL,
    created_by   BLOB,
    learned_from TEXT REFERENCES approvals(request_id),
    revoked_at   INTEGER
);
```

Learned at answer time: `kj ledger allow <id> --remember always --family`
mints one family rule per command in the answered ask's program. The
single write site is `learn_family_from_approval` beside
`learn_from_approval` (`approval-ledger/src/rules.rs`). **As built**, the
row carries one `family_key` column — the same key space as `gate.toml`
(`kj <verb> [<subcommand>]`, `<command> [<first argument>]`) — instead of
the `program`/`subcommand` pair sketched above, so one key space serves
both layers; and the structural check lives in the kernel
(`kj::gate_policy::family_keys_for_program`), not the ledger: the ledger
stores no heredocs and cannot canonicalize a `kj` verb, so the kernel
re-plans the ask's `exec_source` and refuses — loudly, naming the
condition — when a command carries a redirect, a background flag, a
heredoc, or a non-plain argument, or is a `kj` argv that does not
classify, because the family key would then authorize text the human
never saw. A `kj`-verb ask has no program and cannot teach a family.
Without `--family`, `--remember` keeps the digest-rule behavior unchanged.

**Guarantee 3 does not apply to family rules, and the reason is the key.**
The free-variable refusal exists because a digest rule generalizes statement
*text* whose variable values the answerer never saw. A family key never reads
arguments at all — `kj handoff note ${ANYTHING}` is allowed because
`handoff note` is, which is exactly the generalization the answerer asked
for.

**Guarantee 4 does not apply to family rules either, for the same reason.**
The label-mismatch loud error (`rules::redeem_one`,
`approval-ledger/src/rules.rs:186`) fires when a digest rule covers a
statement but authorized different *text* than what is presented. A family
rule matches by key, never by label — `kj handoff note 'first'` and
`kj handoff note 'second'` share a key and differ in label by design. Both
carve-outs are documented at `learn_family_from_approval` and pinned by
tests, not discovered later.

**Integration seam.** `StatementVerdict` carries a `RuleRow` whose
`statement_digest` is NOT NULL (`approval-ledger/src/types.rs:618`) — a
family rule cannot ride it. So the ledger grows a `family_coverage()` reader
beside `redeem()`, returning the family verdict per key, and the evaluator
(kernel-side) composes digest coverage, family coverage, config, and builtin
tiers into its own per-statement verdict. `AskCoverage` and `StatementVerdict`
keep their digest shape; nothing in approval-ledger learns about config.

`kj ledger rules` lists both kinds with their layer; `kj ledger forget`
revokes either by rule_id. No-self-approval is untouched: a family rule is
minted from an answer, and answers still come from a peer seat.

## Builtin tier: the verb class

Every `kj` verb declares its effect in code, on the verb itself:
`Effect::Read | Write | Destroy`, an exhaustive match per subcommand enum
(`kj/effect.rs` is the reference). The builtin tier is that declaration:

- `Read` is allowed by construction. The verb never meets the classifier
  or the gate. `kj/readonly.rs` keeps the five structural conditions (name
  exactly `kj`, no redirect, no background, no heredoc, plain arguments)
  and asks `classify()` for the sixth.
- `Write` and `Destroy` meet the gate like any other statement, and the
  layers above decide.
- `Destroy` is additionally latched by the dispatcher: no `--confirm`, no
  run, from one place.

There is no authored `gate` field and no TOML. A Write verb that policy
should allow by default (`kj handoff note`) is a rule in the global allow
tier, not a claim on the class, so the class stays honest about what the
verb does. That is a choice: an authored allow on the class would also
work, and reversing it is a decision to record here, not a drift.

Two structural rules stay outside the class because they are not about a
verb's effect:

- **`kj ledger`** is exempt as a whole verb via `is_gate_exempt_kj`. It is
  the gate's answer path, and a gated answer path is not one. `ledger
  list` is a Read verb and `ledger allow` a Write verb by class, and the
  exemption applies over both.
- **The `--help` rule** is a flag pattern (last word `--help`/`-h`, no
  intervening flag), not a verb. It moves into the evaluator as a
  structural rule in slice 2, with a Rust test for the `--content --help`
  bypass that `contrib/lfm2d-ladder-check.kai` asserts today.

## What gets deleted

- The three jq exemptions in `assets/defaults/rc/lib/hooks/lfm2d.kai`
  (`--help`, `kj ledger`, `kj_readonly`) — **in slice 5, and only after
  slice 2's two hook rules have landed.** The broker skip keeps all-Allow
  programs away from hooks; the hook's allow-tier clause drop covers mixed
  programs, where the skip by construction does not fire (`commands.all`,
  `kj/readonly.rs:430`); the `--help` pattern lives in the evaluator as a
  structural rule with its own Rust bypass test. Deleting the exemptions
  before the allow-tier drop exists would hand mixed programs' read clauses
  back to the classifier, which escalates them.
- `program_is_gate_exempt`'s broker-only consult — folded into the
  evaluator both pinch points share (slice 1, done). The stack mismatch
  died by construction.
- The duplicated `kj ledger` exemption prose in `kj/readonly.rs`'s module
  doc shrinks to a pointer.

## Rollout

Slices, each landing with tests and nothing downstream depending on an
unreleased one:

1. **Evaluator seam — shipped.** `kj/gate_policy.rs` composes today's two
   sources (the verb class, digest rules) behind `evaluate`; broker PreCall
   consults `evaluate_planned` (builtin layer only, no store) and
   `run_gate` consults `evaluate`, with the `KjVerb`-origin boundary stated
   above. Behavior changes, both deliberate: an exempt program now
   auto-allows inside `run_gate` too (the mismatch fix, with the durable
   row), and `auto_reason` reads `gate policy: builtin allows kj block
   list` / `gate policy: user rule denies the exact statement (rule …) —
   statement #2 (…)`. Parity tests pin everything else.
2. **Config layer — shipped.** `assets/defaults/gate.toml` + seed + `[global]` /
   `[context_type.<type>]` + kj key validation through the verb tables + the
   PreCall Deny branch (`subject = gate policy`) + the `KJ_TOOL_PLAN` `tier`
   field + **both** lfm2d hook rules (ask-tier exit 3, allow-tier clause
   drop) + the `--help` structural rule in the evaluator with its Rust
   bypass test. The non-kj test entries (`rg`, `wc`, `git push`, `dd`) ride
   in this slice — ruling 3's "a few other things just to test it out".
3. **Learned family rules — shipped.** Schema, `learn_family_from_approval`,
   `--family` at answer time, `family_coverage()` beside `redeem()`,
   `kj ledger rules` (the composed view) and `forget` over both kinds,
   both guarantee carve-outs pinned. Two deviations from the section above,
   both recorded there: the row carries one `family_key` column in the
   config key space rather than `program`/`subcommand`, and the structural
   refusal lives in the kernel (`gate_policy::family_keys_for_program`),
   which re-plans the ask's `exec_source`.
4. **Retired by the verb class (2026-09-09).** This slice authored a corpus
   `gate` field and retired the readonly tables; both went with
   `kj-expectations.toml`, and the builtin tier is each verb's declared
   `Effect` (§Builtin tier). Nothing remains to build here.
5. **First tuning pass.** Before it: one test that runs the real
   `lfm2d.kai` pipeline (or a fixture of its tier section) against a mixed
   `KJ_TOOL_PLAN`, pinning that the allow-tier drop precedes `clauses_json`
   and the ask check precedes the jq exemptions — the ladder check pins the
   filters' logic, not their placement, and the deletion below rests on the
   placement. Then `kj handoff note` in the global allow tier closes
   `docs/issues.md`, "The lfm2d gate escalates `kj handoff note` from the
   MCP shell" — its option 1, arrived at through the general mechanism
   instead of a scorer special case. Delete the jq exemptions (safe now:
   slice 2's allow-tier drop is in).

### Tests each slice must land with

The repo's own standard — a test that cannot fail is not a test:

1. Corpus `gate` exhaustiveness (slice 4): the equality assertion above,
   beside `every_live_leaf_has_an_entry`.
2. Mixed-program hook behavior (slice 2): a program with one allow-tier and
   one score-tier clause reaches the hook, the allow clause is dropped from
   the scored set, and the score clause still escalates — the parity tests
   at `mcp/broker.rs`, `a_ledger_answer_never_reaches_an_asking_hook` only cover all-exempt programs today.
3. Structural veto on family allows (slice 3): a family allow on
   `kj handoff note` does not cover `kj handoff note 'x' > ~/.bashrc` — the
   redirect drops it to Uncovered.
4. The `--help` bypass in Rust (slice 2): `kj rc add <path> --content
   --help` is not help, mirroring `contrib/lfm2d-ladder-check.kai`.
5. Guarantee-4 carve-out (slice 3): a family allow learned under label A
   covers the same key under label B with no `LabelMismatch`, beside the
   digest-rule test that pins the opposite (`rules.rs:526`).

## Inspection

`kj ledger rules` grows the composed view — every key that currently has a
verdict for the calling context, each naming its winning layer:

```text
KEY                  VERDICT  LAYER
kj handoff note      allow    learned family (always, rule 01a0…, from ask 01a0…)
kj rc add            ask      global config
dd                   deny     global config
kj block read        allow    builtin (Effect::Read)
```

The user-facing word is **gate rules**, never "policy": `kj policy` is the
per-instance QoS surface and one term keeps one meaning. The internal module
name `gate_policy` is the sanctioned exception, visible only in source.

## Settled while reviewing

- **`gate.toml` joins `kj config`.** `kj config list/show/reset` already
  canonicalizes into `/config/kernel/<name>` (`kj/config.rs:113`), so the
  file gets `show` and `reset` for free — and `reset` on a tuning file is
  the recovery path for a bad edit.
- **The within-layer tie-break**: the more specific key wins (digest beats
  family), deny beats allow at equal specificity. A human's exact-statement
  decision is their most deliberate act; both kinds stay revocable and both
  show in `kj ledger rules`.

- **A file that does not load refuses, it does not degrade.** An
  unreadable or unparseable `gate.toml` refuses every shell submission that
  consults it — `GateUnavailable` at broker PreCall, `Unavailable` inside
  `run_gate` — naming the file and `kj config reset gate.toml`. A policy
  file that silently half-applies is the failure shape this repo does not
  ship, and the host file is one edit from fixed. An absent file is the
  empty config: deleting it is a deliberate act the seed respects. The one
  consult that degrades is the `KJ_TOOL_PLAN` `tier` stamp: a file that
  breaks between PreCall (which refused on it) and the hook run stamps
  `score` everywhere and logs an error — the enforcing pinch points reload
  and fail closed either side of it.
- **The `ask` tier on the RPC shell paths rides the hook stack.** Those
  paths evaluate PreCall and never open the shell gate, so an ask-tier
  statement there is asked only when the lfm2d hook is installed and in
  `escalate` mode (in `log` mode it records a trace and proceeds). The MCP
  `shell_write` path asks through `run_gate` regardless. Whether PreCall
  should open its own ask for an ask-tier statement when no hook does is
  the open question below.

## Open questions
- **Should broker PreCall open an ask for an ask-tier statement itself?**
  Today the ask tier is firm through `run_gate` (MCP `shell_write`) and
  through the lfm2d hook's exit 3 (every path, escalate mode only). A
  PreCall-owned ask would make the tier firm on the RPC paths with no hook
  installed, at the cost of a second ask on the MCP path, which already
  double-asks when a hook escalates ahead of the shell gate.
- **shell-guard's interpreter lists.** They are opacity rules, not risk
  tiers, and stay a structural hook in v1. Whether `deny = ["sh", "bash",
  …]` in `gate.toml` eventually replaces the jq is a later call — one
  authoring surface is attractive, the guard's fail-closed-on-no-plan
  behavior must survive the move unchanged.
- **Session-scoped family rules** (`scope = 'session'`) are in the schema
  for symmetry with digest rules; nothing learns one until a UX wants it.
- **Composed-view caching.** v1 composes at check time (one SQLite read
  that `redeem` already pays, plus an mtime-cached file read). A per-context
  materialized cache is a follow-up if measurement ever asks for it.

## Out of scope

Recorded elsewhere and deliberately untouched here: background execution
gating (`mcp/servers/shell.rs:433`), ask TTL and `kj ledger cancel`
(`docs/issues.md`), hook stdout plumbing ("The escalation seat"), and
redirects in scored clauses ("The scorer cannot see a redirect, only the
exemption can") — the evaluator changes who asks, never what the classifier
sees.
