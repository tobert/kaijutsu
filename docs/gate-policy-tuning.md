# Gate policy tuning — one layered list, one runtime path

Status: designed 2026-09-08; slices 1–3 (the evaluator seam, the config
layer, learned family rules — `kj/gate_policy.rs`,
`assets/defaults/gate.toml`, `approval_rule_families`) shipped 2026-09-10,
slice 5 unbuilt, slice 4 retired by the verb class; slice 6, the uncovered
tier, shipped 2026-09-18. Amy's rulings of the same day are quoted
where they decide a shape. Reviewed against the live tree by kaibo (cast
`crusoe`) the same day; the revision absorbs its findings. This doc is
canonical for the gate-policy evaluator; `docs/gate-resume.md`
remains canonical for the gate itself.

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
| PreCall exemption | const tables, `kj/readonly.rs` | broker PreCall only (`mcp/broker.rs`, `evaluate_phase_body`) |
| Hook exemptions | jq filters over `KJ_TOOL_PLAN` | the risk-classifier hook body under `assets/defaults/rc/lib/hooks/` (since removed) |
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
  same-seat answer is refused, so every note needs a second seat.

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
`docs/gate-resume.md`, with no new `RefusalKind`); every
statement Allow → hooks are skipped, replacing today's
`program_is_gate_exempt` consult; anything else → hooks run as they do now.
All three RPC paths reach this with `tool = shell_write`, each through a
runtime entry point that calls `Broker::shell_pre_call_hooks`:
`kaijutsu-server/src/rpc.rs`'s `execute` (via `runtime::streaming::execute`),
`execute_shell_command` (via `runtime::interactive::submit`), and
`execute_kj_command` (via `runtime::structured::execute_kj`).

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
  `PlannedValue::Literal` or `Plain`, and the argv classifies (`kj::classify`, which
  parses the whole argv through the verb enums). A kj key that names no
  live leaf fails a test, not silently.
- Non-kj keys are free-form command names (`rg`) or name-plus-first-token
  (`git push`). Nothing reflects over host PATH, so a typo is a no-op —
  which fails safe, toward the default ask.

### Structural refusals veto family allows, never digest rules

A family key classifies the verb; it cannot see the arguments. So every
Allow from layers 1-3 and every *family* rule in layer 4 inherits the six
conditions: a redirect, a background flag, a heredoc, or a non-plain
argument drops the statement to Uncovered and it meets the pre_call hooks
and the gate as usual. `kj handoff note 'x' > ~/.bashrc` is not an allowed
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
  `docs/kaish-integration.md`). The refusal names the layer and key. Leaves a durable row
  when it happens inside `run_gate`.
- **Ask** (a statement the config marks ask-tier) — never auto-allowed by a
  lower layer. `run_gate` asks on the `shell_write` tool path; on the RPC
  shell paths broker PreCall opens the ask after the hooks let the call
  proceed (see "Settled while reviewing"). A tier a hook could vote down is not a tier.

**A tier allow is not a capability grant.** `require_cap` and the loadout
run exactly as before; the evaluator decides whether an *ask* fires, never
whether a verb exists for this seat. `kj drive` in the allow tier still
needs `Capability::Drive`.

**Scope boundary: the evaluator governs shell submissions, not kj-verb
dispatch.** A `kj` call made *inside* a hook body or another kj verb (a
scoring hook's own `kj ledger signal add`) reaches the kernel through the
`KjDispatcher`, which never passes through broker
PreCall or `run_gate`. A deny tier on `kj ledger signal add` catches a
`shell_write` submission containing that text; it does not, and cannot, block
the hook's audit call. That path keeps its own controls (`require_cap`).

To make Ask firm in the hook stack, `KJ_TOOL_PLAN` grows a per-command tier
field beside `kj_readonly` — `allow` / `ask` / `score`, computed by the same
evaluator when the broker builds the plan (`mcp/broker.rs`, `run_kaish_hook`), so
there is one classification with two consumers. No shipped hook scores
commands. An rc hook that does should follow two rules:

- an `ask`-tier clause exits 3 naming the tier, regardless of its own
  verdict — a tier a hook could vote down is not a tier;
- anything else is scored **whole**. The broker skip only fires for a program
  where *every* command is Allow; a mixed program
  (`kj block list; rm -rf /tmp/x`) reaches the hook, which should score the
  raw `shell_write` command as submitted, with no clause dropped and no
  exemption.

The broker fails a hook closed: exit 0 proceeds, 3 asks with the stderr
tail as the description, 124 (the body timed out) asks, and any other exit
denies (`mcp/broker.rs`, `classify_kaish_hook_exit`). A fault running the
body asks; a `kaish_path` body that cannot be read denies. A scoring hook that cannot reach its service
should exit 3 rather than fail, so an outage asks a human instead of
stopping every seat; `crates/kaijutsu-acp-fleet/fleet/hook-*.toml` pins the
three outcomes.

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

[context_type.toolie]
deny = [
  "kj context create",   # a toolie uses tools; lifecycle belongs to its parent
]
[context_type.mcp]
allow = [
  "kj context create",   # an mcp seat spins up coders; recoverable: archive it
  "kj drive",            # runs a turn on a context this seat can already reach
]

[context_type.director]
allow = [
  "kj context create",   # a director creates its lanes, `--type coder --as coder`
  "kj fork",             # a fork copies this seat's own performer and type
  "kj drive",            # runs a turn on a lane this seat directs
]
```

Unknown sections or verdict words fail the load loudly, naming the section
and key (a TOML syntax error names the line) —
a policy file that silently half-parses is the failure shape this repo does
not ship.

**A `[context_type.<name>]` section must name a live context type**, one
with an rc bucket under `/config/rc` (`kj::rc::known_context_types`, the
list `kj context create --type` validates against). A section naming no live
type applies to nobody, so a typo'd `ask` tier leaves its type on whatever
`[global]` said — fail-open for exactly the role the operator meant to
protect. The refusal names the section and lists the known types, and it
covers a section holding only key lists too, which is inert the same way.
The check needs the live rc tree, so it runs in `load_config` rather than
`GateConfig::parse`, which stays a pure shape check over the text. An rc
tree that lists nothing accepts every section, the rule
`kj::rc::check_context_type` already states for `--type`.

A context whose stored `context_type` predates a rename is the one hazard
this creates: the section naming its old type refuses the whole file. The
remedy is the same one the message gives — correct the section.

## The uncovered tier: a sandbox posture

```toml
[context_type.coder]
uncovered = "allow"   # sandboxed or throwaway kernels only
```

`uncovered` decides what happens to a statement no key covers: `ask` (the
default, and what the shipped file leaves in force) or `allow`. Under
`allow`, every statement the lists above do not name is allowed — redirects,
heredocs, background flags and substituted arguments included — and nothing
asks anyone. Explicit `ask` and `deny` keys still fire, and a learned ledger
rule still outranks the tier; the tier is decided last, after every key and
after the structural veto. A section's own setting wins over `[global]`, so
a global sandbox can be withheld from one context type with
`uncovered = "ask"`.

**Set it only where the work is disposable**: a benchmark container, a
throwaway kernel. It removes the human from the loop for that context type.
It also removes every PreCall hook: broker PreCall skips hooks for a program
it allows outright, so the shell-escape guard never runs on one. That is consistent with the design — the gate is an ergonomic nudge
inside one trust boundary, not a security boundary
(`docs/instrument-design.md`, "Many hands, one trust boundary") — and it is
the reason the setting is explicit, per section, and absent from the shipped
file.

The reason a list cannot do this job: an allow covers a command *key*, never
its arguments (§Structural refusals veto family allows). A coder's real
traffic is `python3 build.py > log 2>&1`, `cargo test 2>&1 | tee out`,
`sed -i …`, `bash -c '…'` — each drops back to `Uncovered`, so every one of
them asks. Measured on a coder turn, nearly every command a model ran was
uncovered, and each ask cost the model roughly six times the tokens hunting
for output it had not been given
(`~/exomemory/kaijutsu/coder-early-stop-2026-09-18.md`).

**An operator sees the posture.** An auto-decision made by the tier reads
`gate policy: context_type config (coder) uncovered tier allows python3
build.py` in its durable row, never as an allow-list hit, and `kj ledger
rules` lists `uncovered / allow` with the section that set it plus one line
of prose. `KJ_TOOL_PLAN` stamps `tier = "allow"` on each such command.

Two boundaries the tier does not cross. A program that does not parse still
meets the hooks, as before. An `Origin::KjVerb` ask (`kj cc send`) never
reaches the config layers, so it still asks — `docs/issues.md`, "The
uncovered tier does not reach a `KjVerb` ask".

A whole gate file for a benchmark container is two lines:

```toml
[global]
uncovered = "allow"
```

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
never saw. A `kj`-verb ask has no program and cannot teach a family. Two
consequences worth knowing: a family *deny* is refused on structure too,
though a standing deny is safety-increasing — the refusal is the same
function and stays fail-safe until a case asks otherwise; and a non-kj
command whose first argument is a flag teaches the bare command as its
family (`git -C x push` teaches `git`), which the answer-time message
names, so the human sees the width they consented to. Without
`--family`, `--remember` keeps the digest-rule behavior unchanged.

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

- `Read` is allowed by construction. The verb never meets a hook or the
  gate. `kj/readonly.rs` keeps the five structural conditions (name
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

- **`kj ledger`** is allowed as a whole verb in the builtin layer via
  `is_gate_exempt_kj`, so no hook asks about an answer. The builtin layer
  sits beneath `gate.toml`, so a `[context_type.<type>]` key can still deny
  part of the verb to a seat; moltar denies coders `kj ledger list` and
  `kj ledger show` and leaves `kj ledger cancel` open. People answer through
  the typed ledger RPCs (`listAsks`, `decideAsk`; `docs/approval-identity.md`),
  which take no context and never meet a gate. Reviewer assignment, enforced
  in the ledger, decides who may answer.
- **The `--help` rule** is a flag pattern (last word `--help`/`-h`, no
  intervening flag), not a verb. It moves into the evaluator as a
  structural rule in slice 2, with a Rust test for the `--content --help`
  bypass.

## What gets deleted

- The three jq exemptions in the classifier hook body
  (`--help`, `kj ledger`, `kj_readonly`), the hook's per-command clause split,
  and its allow-tier clause drop — deleted. The broker skip keeps all-Allow
  programs away from hooks, and the `--help` pattern lives in the evaluator as
  a structural rule with its own Rust bypass test. Everything that reaches
  the hook is scored as submitted.
- `program_is_gate_exempt`'s broker-only consult — folded into the
  evaluator both pinch points share (slice 1, done). The stack mismatch
  died by construction.
- The duplicated `kj ledger` exemption prose in `kj/readonly.rs`'s module
  doc shrinks to a pointer.

## Rollout

Slices, each landing with tests and nothing downstream depending on an
unreleased one:

1. **Evaluator seam — shipped.** `kj/gate_policy.rs` composes the verb class
   and digest rules behind `evaluate`, consulted from both broker PreCall
   and `run_gate`.
2. **Config layer — shipped.** `assets/defaults/gate.toml`, the
   `[global]`/`[context_type.<type>]` layers, the `KJ_TOOL_PLAN` `tier`
   field, and the `--help` structural rule.
3. **Learned family rules — shipped.** `approval_rule_families`,
   `--family` at answer time, `family_coverage()` beside `redeem()`, and
   `kj ledger rules`/`forget` over both digest and family rules.
4. **Retired by the verb class.** The builtin tier is each verb's declared
   `Effect` (§Builtin tier); the corpus `gate` field and the readonly tables
   it replaced are gone.
5. **First tuning pass — superseded.** Its tests ran the shipped
   classifier hook through a real `shell_write` call; the hook is removed.
   `an_allow_tier_program_skips_the_hooks` (`mcp/broker.rs`) keeps the
   all-Allow skip, and `fleet/gate-tiers.toml` keeps the tier order around a
   hook. `kj handoff note` sits in the global allow tier.
6. **The uncovered tier — shipped.** `uncovered = "ask" | "allow"` on
   `[global]` and each `[context_type.<type>]` (§The uncovered tier), decided
   last and reported as its own layer in `kj ledger rules`; a
   `[context_type.<name>]` section naming no live context type fails the
   load (§The file).

### Tests each slice must land with

The repo's own standard — a test that cannot fail is not a test:

1. Corpus `gate` exhaustiveness (slice 4): the equality assertion above,
   beside `every_live_leaf_has_an_entry`.
2. Mixed-program hook behavior: a program with one allow-tier and one
   score-tier clause reaches the hook with its raw command whole in
   `KJ_TOOL_ARGS` (`mcp/broker.rs`, the `KJ_TOOL_PLAN` tests).
3. Structural veto on family allows (slice 3): a family allow on
   `kj handoff note` does not cover `kj handoff note 'x' > ~/.bashrc` — the
   redirect drops it to Uncovered.
4. The `--help` bypass in Rust (slice 2): `kj rc add <path> --content
   --help` is not help.
5. Guarantee-4 carve-out (slice 3): a family allow learned under label A
   covers the same key under label B with no `LabelMismatch`, beside the
   digest-rule test that pins the opposite (`rules.rs:526`).
6. The uncovered tier (slice 6), in `kj/gate_policy.rs` unless noted:
   absent by default and absent from the shipped file; the tier reaching
   only the section that sets it and a context type's setting outranking
   `[global]`; an unknown `uncovered` word failing the load; deny and ask
   outranking the tier, and an allow-list entry the structural veto drops
   falling through to it while a plain allow-list hit still reads as one;
   the per-command `tier` word; which constructs plan a commandless
   statement, beside the branch that decides one; an unknown
   `[context_type.<name>]` section failing the load, with the fail-open
   scenario it prevents and the shipped file loading against the shipped rc
   tree; and through the gate itself (`kj/ledger.rs`) a whole program
   auto-allowing with the tier named in its durable row, a remembered
   family deny and a remembered exact-statement deny each still winning,
   and `kj ledger rules` stating the posture.

## Inspection

`kj ledger rules` grows the composed view — every key that currently has a
verdict for the calling context, each naming its winning layer:

```text
  KEY                  VERDICT  LAYER
  ls -la /srv/builds   allow    user rule (session, rule 01a0…)
  kj handoff note      allow    user family rule (always, rule 01a0…)

  GATE.TOML KEY        VERDICT  LAYER
  uncovered            allow    context_type config (coder) uncovered tier
  kj rc add            ask      global config
  dd                   deny     global config

builtin: every kj verb declaring Read, kj ledger, and kj … --help are allowed beneath these
forget with: kj ledger forget <rule-id>
```

As built: the learned rules of both kinds come first, newest first and
cut by `--limit`; the `gate.toml` tiers in force for the caller's
context type follow, uncounted, the uncovered tier first when it is on and
with a line of prose beneath the table; the builtin layer is one line rather
than a row per Read verb. The listing is not context-scoped — another
context's session rules appear too — which is how the digest listing
already behaved.

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
- **The `ask` tier on the RPC shell paths asks at PreCall.** Those paths
  never open the shell gate, so `Broker::shell_pre_call_hooks` opens the ask
  itself for an ask-tier statement, after the hooks let the call proceed,
  so a hook's deny refuses without asking anyone first. The ask names each
  layer, key, and statement ("gate policy: global config asks git push —
  statement #0 (`git push origin main`)"). The ask carries the command: an
  allowed ask runs it once in the approval worker, and a retry of the same
  command does not run it. A hook chain that asks raises one ask of its own,
  and the tier asks only when the chain let the call proceed, so the two
  never both ask. The `shell_write` tool path asks
  through `run_gate` instead, so it never asks twice for the tier.
- **On the RPC shell paths, an uncovered statement asks unless the actor
  is a live root character.** A root has no model, so its uncovered
  statements run as typed; any other actor, including a principal with no
  character sheet, gets the ask it would get on `shell_write`. The actor is
  the connection's character; the client's `user_initiated` flag controls
  presentation only (`docs/approval-identity.md`). PreCall composes every
  layer the way `run_gate` does: exact and family rules from the ledger,
  then the config and builtin tiers. A learned allow outranks a config deny
  there, and a remembered deny refuses with nobody asked.
- **A config allow that cannot cover a command decides nothing.** When a
  redirect or an argv that does not classify keeps an allow from covering,
  the builtin layer still decides, so `kj context create --help` stays help
  under an allow for `kj context create`.

## Open questions
- **shell-guard's interpreter lists.** They are opacity rules, not risk
  tiers, and stay a structural hook in v1. Whether `deny = ["sh", "bash",
  …]` in `gate.toml` eventually replaces the jq is a later call — one
  authoring surface is attractive, the guard's fail-closed-on-no-plan
  behavior must survive the move unchanged.
- **Session-scoped family rules** (`scope = 'session'`) are learnable
  (`--remember session --family`) and cover the raising context only;
  pinned, but no surface has asked for one yet.
- **Composed-view caching.** v1 composes at check time (one SQLite read
  that `redeem` already pays, plus an mtime-cached file read). A per-context
  materialized cache is a follow-up if measurement ever asks for it.

## Out of scope

Recorded elsewhere and deliberately untouched here: background execution
gating (`mcp/servers/shell.rs:433`), ask TTL and `kj ledger cancel`
(`docs/issues.md`), hook stdout plumbing ("The escalation seat"), and
redirects in scored clauses ("What a replacement risk scorer inherits") —
the evaluator changes who asks, never what a hook sees.
