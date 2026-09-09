# Gate policy tuning — one layered list, one runtime path

Status: designed 2026-09-08, unbuilt. Amy's rulings of the same day are quoted
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
| PreCall exemption | const tables, `kj/readonly.rs` | broker PreCall only (`mcp/broker.rs:1887`) |
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
| 1. Builtin/generated | const tables (`kj/readonly.rs`) and the corpus `gate` field (§Generated tier) | compile | shipped in-repo, reviewed like code |
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
  `is_read_only_kj` enforces today (`kj/readonly.rs:390`): name exactly
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
evaluator when the broker builds the plan (`mcp/broker.rs:2540` ff.), so
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
(`docs/config-namespace.md`). The example is the rule:

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

Unknown sections or verdict words fail the load loudly, naming the line —
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
mints a family rule from the answered ask's statement. The single write site
is a new `learn_family_from_approval` beside `learn_from_approval` (the
single write site for digest rules today, `approval-ledger/src/rules.rs:8`),
and the structural check lives there: it refuses — loudly, naming the
condition — when the statement carries a redirect, a background flag, a
heredoc, or a non-plain argument, because the family key would then
authorize text the human never saw. Without `--family`, `--remember` keeps
today's digest-rule behavior unchanged.

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
(`docs/kj-verb-class.md` carries the build plan; after it ships,
`kj/effect.rs` is the reference). The builtin tier is that declaration:

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
  bypass that `contrib/lfm2d-ladder-check.kai:146` asserts today.

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
  evaluator both pinch points share. The stack mismatch dies by
  construction.
- The duplicated `kj ledger` exemption prose in `kj/readonly.rs`'s module
  doc shrinks to a pointer.
- `READ_ONLY_TABLE`/`READ_ONLY_NO_SUBCOMMAND` after the corpus fold.

## Rollout

Slices, each landing with tests and nothing downstream depending on an
unreleased one:

1. **Evaluator seam.** `kj/gate_policy.rs` composing today's two sources
   (code tables, digest rules) behind one function; broker PreCall and
   `run_gate` both consult it, with the `KjVerb`-origin boundary stated
   above. Behavior changes, both deliberate: an exempt program now
   auto-allows inside `run_gate` too (the mismatch fix, with the durable
   row), and `auto_reason` text grows layer names. Parity tests pin
   everything else.
2. **Config layer.** `assets/defaults/gate.toml` + seed + `[global]` /
   `[context_type.<type>]` + corpus key validation + the PreCall Deny branch
   (`subject = gate policy`) + the `KJ_TOOL_PLAN` tier field + **both** lfm2d
   hook rules (ask-tier exit 3, allow-tier clause drop) + the `--help`
   structural rule in the evaluator with its Rust bypass test. The non-kj
   test entries (`rg`, `wc`, `git push`, `dd`) ride in this slice — ruling
   3's "a few other things just to test it out".
3. **Learned family rules.** Schema, `learn_family_from_approval` with the
   structural refusal, `--family` at answer time, `family_coverage()`
   beside `redeem()`, `kj ledger rules`/`forget` extension, both
   guarantee carve-outs documented and pinned.
4. **Corpus `gate` field + the fold.** Authored builtin tier, the two
   exhaustiveness assertions, the three structural leaves handled by name,
   then `READ_ONLY_TABLE`/`READ_ONLY_NO_SUBCOMMAND` retire.
5. **First tuning pass.** `kj handoff note` in the global allow tier closes
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
   at `broker.rs:8105` only cover all-exempt programs today.
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
kj block read        allow    builtin (corpus gate)
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

## Open questions
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
