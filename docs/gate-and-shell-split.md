# The gate and the shell split — one design, not two

A living design doc, seeded 2026-08-17 from Amy's rulings that day, the
2026-08-14/16 gate research passes, and a `kaibo`/GLM-5.2 grounding pass
against the pinned kaish 0.14.1 source. Written before any of it lands —
treat every "Slice N" below as a plan, not a status line, until
`docs/issues.md` says otherwise.

The two halves — the `kj`/tool-call confirmation gate and the `shell` /
`shell_write` split — are one design because they share a seam
(`docs/issues.md`, gate entry: *"Rebuilding confirmation on `plan_program`
and defining the read/write split in one pass means designing the boundary
once instead of porting the old shape and immediately reshaping it"*) and
because approval fatigue is the failure mode that kills both if they ship
separately: a `shell_write` with no safe sibling gets every `ls` gated, and
a gate with nothing narrowing what needs asking gets clicked through until
the ledger means nothing.

## What already exists — read this before proposing anything new

Two mechanisms are already built and live in the tree. This design extends
both; it invents neither.

1. **`approval-ledger`** (`crates/approval-ledger/`) — a standalone,
   SQLite-backed ledger, migrated into `KernelDb`
   (`KernelDb::migrate_ledger`). Durable-before-asked asks, content-addressed
   per-**statement** rules (not per-ask — Amy's 2026-08-14 ruling, see
   `schema.rs` header), a label-scoped redemption path, and — already, today,
   not something this doc proposes — **guarantee 3: an ALLOW rule can never
   be learned for a statement with a free variable**
   (`rules::learn_from_approval`, backstopped by the
   `approval_rules_reject_free_variable_allow_rules` trigger,
   `schema.rs:460-465`). Amy's ruling 3 below (*"refuse when the plan
   carries free variables"*) is not a new policy this doc invents — it is
   already shipped, tested, and trigger-backed. What this doc adds is the
   forward-compatible shape for loosening it later (see "The ledger key,
   including the future predicate shape").
2. **`kj cc send`** (`crates/kaijutsu-kernel/src/kj/{cc,gate,approve}.rs`) —
   the first working consumer: a `GateSpec` (hand-authored, not
   `plan_program`-derived, because a `kj` verb isn't kaish source text),
   `run_gate` (rules first → durable ask → wait-or-expire), and `kj approve
   list/show/allow/deny` as the answering CLI. This is the template Slice 5
   below extends to the six `Latch` producers, and Slice 4 generalizes to
   real kaish source.

Neither of these talks to `mcp/permission.rs` (`HookAction::Ask`,
`subscribePermissionEvents @93` — not `@103`; the ordinal moved under
concurrent editing this week, treat every line/ordinal citation here as
approximate and re-grep before trusting it) at all today. That seam is a
**third**, separate mechanism: kernel-wide, hook-triggered, ephemeral (no
ledger row — `run_permission_ask` fires a `PermissionAskRequest` and throws
the answer away once the phase resolves). Amy's forward-compatible sequencing
note explicitly says the new gate should share this seam rather than invent
a fourth. "The shared seam" section below is about closing that gap: the
ledger becomes the durable record for *both* answering paths, and
`subscribePermissionEvents` becomes the one wire notification any connected
client (Bevy app, ACP session, a future headless client) subscribes to for a
live prompt, whether the ask came from a hook's `Ask` action or from a gated
`kj` verb or `shell_write` call.

## Doctrine this design sits inside — not up for re-litigation here

From `CLAUDE.md` and `docs/instrument-design.md` ("Many hands, one trust
boundary"):

- Every player — human, model, connected app, sibling context — is **inside**
  the trust boundary. The kernel runs as one unix user; the real boundaries
  are outside it. We design for **resilience to boundary trespass, not
  enforcement between cooperating players.**
- **Crosstalk is a feature.** A gate is not a wall between players; it is a
  footgun-visibility device for one player who might be about to make a
  mistake, including the very player who set the gate up.
- **Capabilities are ergonomic nudges — narrower focus, never less trust.**
  A capability check doing *auth between cooperating players* is the wall
  the music has to climb; that is the thing not to build.
- **The ledger is a learning instrument, not a verdict** (Amy, on `kj cc
  send`: *"gated to start with… I want to start with watching it and
  exercising the ledger while we refine it"*).
- **`sandbox` is out, in any position**, because it claims a security
  boundary this system does not have and does not want to pretend to have.
- **Prefer deleting a mechanism to generalizing it.** Amy has given explicit
  permission to reduce scope; nothing below should be read as license to
  build the maximal version of anything.

Read every ruling below through this lens. None of what follows makes an
action harder to *reach*; it makes an action's consequence visible before
it lands, and gives a human — any human at the table, not a designated
guard — a chance to notice.

## Amy's rulings, 2026-08-17

### 1. Hook self-lockout recovery: a `kj hook` surface, not a hook carve-out

`builtin.hooks`' own `hook_list`/`hook_remove` tools return `Denied` under a
`PreCall Deny("*")` hook, and — corrected in the code comment on 2026-08-12,
confirmed still accurate at HEAD (see "Wrong at HEAD," it's the *opposite*
of wrong today) — it **survives a restart**: hooks rehydrate from `KernelDb`
at `Broker::set_db` (`mcp/broker.rs`, `hydrate_hooks_from_db`), so the
self-lockout outlives the process. The only documented recovery is hand-
editing kernel SQLite, which the project's own standing rule forbids
(`feedback_no_direct_kernel_db_access`).

Amy's ruling: build `kj hook`. The escape hatch does not go through broker
hook evaluation **at all** — no `Broker::call_tool`, no `evaluate_phase`, no
`PreCall` match against it. This is not a carve-out inside the hook
mechanism (which would be a hole: a rule that says "except for this one
caller"); it is a **sibling path that never enters the mechanism it exists
to route around**, exactly the shape `kj mcp list/reload` and `kj policy`
already use for other broker-wide admin surfaces
(`kj/mod.rs`: *"`kj mcp list/reload` is a broker-wide admin surface... not
scoped to any context"*; `kj/mod.rs::require_cap`'s own doc: *"`kj` is a
kaish builtin that bypasses the broker `call_tool` / facade gates entirely
... this is the third enforcement surface alongside the broker and the
facade gate"*). `kj` already had this property before this doc; `kj hook`
just uses it for hooks specifically.

**Shape**, following the `kj mcp` / `kj approve` precedent:

```
kj hook list                    # every persisted hook, phase + match + action
kj hook show <id>
kj hook remove <id>
kj hook add <phase> <match-json> <action-json>   # symmetry with builtin.hooks' own add
```

Wired directly to `Broker::persist_hook_insert` / `persist_hook_delete`
(already real, already the DB-level primitives `hooks_builtin.rs`'s tool
implementations call — `mcp/broker.rs:497-514`) plus the same in-memory
`HookTables` mutation those tools perform, called from `kj` the way `kj
mcp`/`kj policy` already reach broker internals directly — **not** through
`Broker::call_tool`. Gated the same way every other admin-shaped `kj` verb
is gated: `KjDispatcher::require_cap` against a capability (new:
`Capability::HookAdmin`, or reuse whatever `kj mcp` uses if it's broad
enough — a Slice 1 implementation question, not a design one), with the
privileged-caller bypass the rc lifecycle already relies on.

Why this doesn't reopen the hole it closes: the capability check is `kj`'s
own — a *third* enforcement surface, per `require_cap`'s doc, independent
of the broker's hook tables. A `PreCall Deny("*")` hook can only ever match
things that pass through `evaluate_phase`; `kj hook` structurally never
does. There is nothing to carve out because there is nothing to evaluate.

**Admin-shaped and occasional** — CLAUDE.md's rule applies directly: "kj is
good enough for all admin-like stuff... does NOT earn a wire method." No
capnp change, no RPC. This also closes a second, smaller gap noted in the
issues entry: there is no `kj hook` CLI **at all** today, gated or not.

### 2. "Gate unavailable" and "denied" must be distinguishable to a model

**Today's collapse.** `Broker::run_permission_ask` (`mcp/broker.rs`,
~1700-1820 as of this week — re-grep before trusting the line) returns
`Option<String>` — `None` proceeds, `Some(reason)` denies — for **three**
distinct outcomes: a subscriber said no, no `PermissionAsker` was attached
at all, and a subscriber was attached but never answered inside
`permission_ask_timeout`. All three become `McpError::Denied { by_hook }`
at the call site, which `error_to_hook_json` (`mcp/broker.rs:2371-2409`)
renders as `{"kind": "Denied", "by_hook": "..."}`  — indistinguishable to
the model from an actual "no."

**Why this ends.** Amy's reasoning is doctrinal, not a UX preference:
"silent fallbacks are often a mistake" (`CLAUDE.md`), and `docs/issues.md`
already names this exact failure family elsewhere — "a fault silently
becomes a policy decision," with a standing sweep filed against
`.ok().flatten()` / `unwrap_or(false)` in authz paths. A gate with nobody to
ask is a **broken control**, not a "no." The usual counter-argument for
hiding gate state (don't leak internals to an adversary) does not apply:
there is no adversary inside the trust boundary (`instrument-design.md`),
only a player who benefits from knowing whether the thing they asked for was
refused on the merits or dropped because nothing was listening.

**The two shapes.** Split `run_permission_ask`'s three outcomes into two
buckets at the type level, and keep `McpError::Denied` meaning exactly what
it always meant — a verdict:

```rust
/// What `run_permission_ask` decided, distinguishing a real verdict from a
/// broken control. Kept as three variants at this layer (Proceed keeps the
/// existing call-site shape simple); `McpError` collapses to two.
enum PermissionAskOutcome {
    Proceed,
    Denied(String),
    /// Nothing could answer: no subscriber attached, or one was attached
    /// and never answered inside the timeout. The two stay one variant
    /// here — both mean "the gate could not do its job," and a caller
    /// deciding what to do next needs that fact, not which flavor of
    /// nothing happened. `reason` keeps the flavor for tracing.
    Unavailable(String),
}
```

```rust
/// A denial with a verdict behind it — a hook said no, a subscriber said
/// no, an approval-ledger ask resolved to `denied`.
#[error("denied by hook {by_hook}")]
Denied { by_hook: HookId },

/// A gate that never reached a verdict: nobody to ask, or nobody answered
/// in time. Distinct from `Denied` on purpose — CLAUDE.md: silent
/// fallbacks are a mistake, and this fallback is exactly that unless it
/// says so. A model reading this can retry, escalate through a different
/// channel, or ask a human directly, instead of learning the wrong lesson
/// ("that action is refused") from a control that was simply absent.
#[error("gate for {by_hook} had nothing to answer it: {reason}")]
GateUnavailable { by_hook: HookId, reason: String },
```

`error_to_hook_json` gets a fourth arm (`"GateUnavailable"`, carrying
`by_hook` + `reason`) alongside the existing `Denied` one — same pattern as
every other variant there, no structural change to the function. The two
existing fail-closed *behaviors* (no subscriber → refuse; timeout → refuse)
are unchanged — this only makes the refusal's **reason legible on the wire**
that already carries `is_error: true` either way. Nothing about
"fail-closed" is loosened; only "fail-closed and indistinguishable from a
real no" is fixed.

Same split applies on the ledger side, and it is easier there because the
ledger already has the vocabulary: `ApprovalStatus::Denied` vs
`ApprovalStatus::Expired`/`Abandoned` are already three separate terminal
states (`approval-ledger/src/types.rs`), and `run_gate`'s `GateOutcome`
already threads the real status through (`kj/gate.rs:GateOutcome.status`).
The only wiring left for the ledger path is making sure whatever surfaces
`GateOutcome` to a model (a `kj` verb's error text today; a `shell_write`
tool-call error once Slice 4 lands) renders `Expired`/`Abandoned`
differently from `Denied`, the way `run_gate`'s own `reason` string already
does in prose (*"refused: ask ... ended `expired`"* vs *"refused: ask ...
ended `denied`"*) — Slice 2 below is mostly making that same distinction
reach the MCP/hook path, which currently has none at all.

### 3. Digest-keyed allow-always: refuse on free variables, for now — and the future key

**Already true, not proposed here.** As above: `learn_from_approval` already
refuses to create an ALLOW rule when the targeted statement has any
`binding = 'free'` variable, and the DB trigger backstops the same rule at
the storage layer independent of what any future caller does. A DENY rule
is exempt from the check (strictly safety-increasing) and always permitted.

**Amy's forward note, which the schema must be able to take later without a
migration:** *"we intend to experiment later with a UX that lets a user
approve a filter rather than an exact plan — if the free variables look safe
to vary, the user could grant a pattern-matching / predicate-shaped allow…
a sorta go interface like filter... a sorta pattern matching / function
allow thing."*

**Why the current schema can take that without breaking the key.** The
matching key `redeem` uses today is `(statement_digest, authorized_label,
scope, context_id, principal_id)` (`rules.rs::redeem_one`) — it never reads
a free variable's *value*, because nothing in the ledger's input types
carries one. `NewPlanVar { name, binding }` (`types.rs:361-364`) says
whether a variable is free or bound, never what it resolved to; `redeem`
checks coverage by statement shape and label, not by argument value. A
predicate-shaped allow needs exactly one new fact the current shape doesn't
carry — the free variable's *resolved value at ask time* — and one new
optional constraint stored on the rule. Both are additive:

```rust
/// One variable a statement reads or binds, ready to insert.
pub struct NewPlanVar {
    pub name: String,
    pub binding: VarBinding,
    /// The variable's resolved value at THIS ask, when the embedder has
    /// one to give (a `read` binds at runtime; a hook-time free variable
    /// may not resolve until the command actually runs). `None` for a
    /// bound variable's own name is normal — a bound variable's value
    /// lives in `rendered`, not here. A future predicate-shaped allow
    /// rule matches THIS field against its stored predicate; nothing
    /// reads it today.
    pub resolved_value: Option<String>,
}
```

```sql
-- Additive, nullable, no key change. NULL means "no predicate — the
-- existing free-variable refusal in learn_from_approval still applies in
-- full." A non-NULL predicate is the ONLY condition under which
-- learn_from_approval may be taught to accept an ALLOW rule for a
-- statement with a free variable, and only for the variable the predicate
-- names.
ALTER TABLE approval_rules ADD COLUMN predicate_kind TEXT NULL;   -- 'glob' | 'regex' | 'range' | ...
ALTER TABLE approval_rules ADD COLUMN predicate_var   TEXT NULL;  -- which free variable it constrains
ALTER TABLE approval_rules ADD COLUMN predicate_value TEXT NULL;  -- the predicate body, kind-dependent
```

`approval_rules_reject_free_variable_allow_rules` (`schema.rs:460-465`)
gets one added clause the day this ships — `AND NEW.predicate_kind IS
NULL` — so the trigger keeps refusing an unconstrained ALLOW on a free
variable and only steps aside when a predicate is actually present and
covers the free variable in question. `learn_from_approval`'s Rust-side
check changes symmetrically: instead of "any free variable refuses," it
becomes "any free variable **not named by an accompanying predicate**
refuses." **None of this is built now.** It is captured here so that when
Amy's filter-allow UX gets designed for real, the ledger doesn't need a
migration to receive it — only two nullable columns and a trigger clause,
which is the additive shape `schema.rs`'s own migrate-is-idempotent
convention already expects.

### 4. `shell` / `shell_write` — names, and what a default grant means

Amy, 2026-08-16, ruled the names and rejected transparent routing:

> *"I think we have the pair, one that's clearly a text processing space
> that can't corrupt the system, and another that's hot and can edit and rm
> and stuff… I thought about suggesting `shell` be routed transparently but
> I think that would be even more dangerous in the end, so explicit is
> better."*

**`shell` is the unmarked, safe name; `shell_write` is the hot one, granted
not default.** The deciding argument: models reach for the unmarked,
shortest name by accident, so the unmarked name has to be the one that
cannot hurt anything — otherwise every casual `ls` routes through the
dangerous tool and drowns the gate that watches it (the same approval-
fatigue argument that makes the split a prerequisite for the gate, not an
independent feature).

This is a **flag day** on the tool named `shell`: today `builtin.shell`
(`ShellServer::new`, `kernel.rs:764-769`) is the mutating one and
`builtin.shell_readonly` (`ShellServer::new_read_only`,
`kernel.rs:779-784`, tool `read_only_shell`) is its safe twin. The rename:

| Today | After the flag day |
|---|---|
| `builtin.shell` / tool `shell` — mutating, `ExternalExec::Allow` | `builtin.shell_write` / tool `shell_write` — mutating, unchanged behavior, new name |
| `builtin.shell_readonly` / tool `read_only_shell` — `ExternalExec::Deny` | `builtin.shell` / tool `shell` — same restriction, the name a caller now reaches for by default |

Facade projections move with the tool names: `facade:shell_write` replaces
what `facade:shell` gated before; `facade:shell` now gates the safe tool
(today ungated by name, since `read_only_shell` didn't have its own facade
before — Slice 3 needs to confirm this and give it one if it doesn't,
otherwise the rename silently widens the safe shell's reach).

**This fails in the right direction.** An old caller (rc script, cached
tool-name string, a model's habit) that asks for `"shell"` after the flag
day gets the read-only tool instead of the mutating one — a capability
loss, not a capability leak. That is the acceptable direction for a
breaking rename: wrong-but-safe, never wrong-but-dangerous. `read_only_shell`
retires as a name entirely (it read as "the lesser tool," which stops being
true the moment scratch-space writes land inside it per the
`kaish_ro`-scratch entry — Amy: *"a kaijutsu variant of `kaish_ro` might
have some scratch space and stuff mapped for text processing"* — that
scratch-space widening is real future work but is **not** this slice; the
rename doesn't wait for it).

**Default grants stay per-rc, as today** — no `context_type` gets an
unconditional `facade:shell_write` grant baked into the kernel; each
`/etc/rc/<type>/create/S10-binding.kai` decides for itself, the same
mechanism `S10-binding.kai`'s `kj binding allow` lines already use for
every other capability. A context type that never had shell-write before
the rename does not get it after; a type that granted `facade:shell`
expecting the mutating tool needs its rc script updated in the same commit
that ships the rename, or it silently loses write access — which is,
again, the correct failure direction, but it will read as a bug report if
nobody checks the rc scripts. Slice 3's own checklist has to include
grepping every `S10-binding.kai` for `facade:shell` and deciding, per
context type, whether it meant read or write.

**`sandbox` stays out of both names**, per doctrine above — `shell` and
`shell_write` say what the tool does, not what boundary it claims.

## The crux: the plan API is all-or-nothing per statement, and that's real

`docs/issues.md`'s framing of the hard constraint: there is no execute-a-
Plan API and no per-command interception hook — a caller submits *source
text* to kaish and it runs top to bottom, so a gate built on `plan_program`
can only ever gate **the whole submitted blob**, never pause between
statement 2 and 3 of one call to ask a question mid-run.

This was worth checking against the actual pinned dependency rather than
trusting the framing secondhand, so this design was run past `kaibo`
(cast `crusoe`, GLM-5.2 synth over a DeepSeek-V4-Flash explorer) with the
literal source of `kaish-kernel` 0.14.1 (the version this workspace has
pinned, `Cargo.lock`) in front of it, plus this repo's `mcp/servers/shell.rs`
and `embedded_kaish.rs`. Two things came back that change how this design
is written, not just confirm it:

**1. The constraint is not a design choice we're honoring — it's the only
shape kaish 0.14.1 has.** Every execution path in this codebase, without
exception, calls `EmbeddedKaish::execute_with_options(&code, opts)` with a
whole source-text blob: the read-only `shell` tool
(`mcp/servers/shell.rs:414-421`), the editor's `:r !cmd`, rc lifecycle
scripts, hook `Kaish` bodies. There is no `execute_plan`/`execute_statement`
anywhere in the kaish or kaijutsu source kaibo could find, and
`plan_program` has no execute counterpart — it is pure planning (parse +
render, zero side effects). So "gate the whole submitted blob, all-or-
nothing, at the tool-call boundary" is not a compromise this design accepts
reluctantly; it is the only shape available, confirmed against the real
0.14.1 API rather than assumed from the issues-doc framing. Good news
buried in that: `plan_program` and `PlannedStatement` (with `free_variables`
/ `bound_variables` per statement) are **already released**, in 0.14.1,
pinned today — the base mechanism for Slice 4 needs nothing unreleased.

**2. A heredoc body is genuinely unrenderable on 0.14.1 — not merely
inconvenient.** kaibo confirmed, reading the actual `PlannedCommand` shape
in the pinned crate, that a heredoc's body in 0.14.1 arrives as a
`'\''`-escaped blob inside `PlannedRedirect.target` with the delimiter
rewritten to `EOF`, and critically: **the `literal` field that says whether
the body is what the command actually reads (quoted delimiter) or gets
shell-expanded first (unquoted, so a substitution can land inside a string
literal in the other language) does not exist in 0.14.1 at all.** A gate
that renders a heredoc-bearing statement for a human's confirmation without
`literal` shows text that may not be what runs — the exact failure class a
gate exists to prevent. There is no honest way to render a heredoc body on
0.14.1; the only defensible options are refuse it outright, or ship a
disclaimer that is itself the lie the feature exists to prevent.

**What this design does with that finding, stated plainly because it's a
real fork in the plan:** `docs/issues.md`'s own sequencing note (relayed
from the kaish lead, 2026-08-16) says build the confirmation renderer
*once*, against 0.15, because 0.15 also carries #325/#326/#327 and a parser
rebuild that would otherwise force a second rewrite. That is a real
argument and this doc does not overrule it. But it is a **schedule**
argument, not a **capability** one — 0.14.1 is not missing the ability to
gate non-heredoc statements honestly, it is missing the ability to render
heredocs honestly. So Slice 4 below ships **now**, on 0.14.1, gating every
statement type it can render truthfully, and refuses (fail-closed, loud,
"heredoc bodies aren't renderable on this kaish version" — not a silent
downgrade to showing the escaped blob) any statement `plan_program` reports
as heredoc-bearing. Slice 4b — swap the renderer onto 0.15's
`PlannedHeredoc`/`expand_fragment` and lift the heredoc refusal — lands
whenever 0.15 actually ships and is pinned, no earlier. **This is a
judgment call this doc is making, not a ruling Amy has given** — flag it to
her: the alternative (wait for 0.15 to ship Slice 4 at all) is equally
defensible and is what the kaish-lead sequencing note leans toward. Pick
one before Slice 4 starts.

Property to KEEP from the deleted latch, unconditionally, in both the
`kj`-verb gate (already true — see `gate_spec_for_send`'s
`authorized_label: target.to_string()`, the raw typed reference) and the
new `shell_write` gate: **scope the confirmation to the label the caller
typed, not the resolved id.** For `shell_write` that means: whatever the
`presented_label` fed into `rules::redeem` is, it is derived from the
plan's own rendered text or an explicit caller-supplied label — never from
a lookup that could resolve `${TARGET}` to something the caller didn't
type. Confirming names what it authorizes.

## The shared seam: one wire path for "ask a human," two ways to answer

`docs/issues.md`: the replacement gate "should be ONE path shared with the
permission-Ask seam (`HookAction::Ask`, `mcp/permission.rs`,
`subscribePermissionEvents`) rather than a second bespoke confirmation."
Concretely, today these are two disconnected mechanisms and this design
merges the parts that should be one without merging the parts that
shouldn't:

| | `HookAction::Ask` (D-57) today | `kj cc send` gate today |
|---|---|---|
| Durable record | **none** — `PermissionAskRequest` is thrown away after the phase resolves | `approval_ledger` row, survives a restart |
| Who can answer | one connected client via `subscribePermissionEvents` | `kj approve` from any shell, polling `KernelDb` |
| Rules / auto-decide | none | `rules::redeem` — an active rule can auto-allow/deny without asking |
| Unavailable vs denied | collapses (Ruling 2 above) | already separate (`Expired`/`Abandoned` vs `Denied`) |

**What merges:** the *notification* leg. When `run_gate` (or its Slice 4/5
callers) needs a human and nobody has answered via `kj approve` inside some
short grace window, it fires the SAME `PermissionAskRequest` shape through
the SAME `PermissionAsker`/`subscribePermissionEvents` bridge that
`HookAction::Ask` already uses — so the Bevy app, an ACP session, or any
future client that has ever bothered to subscribe sees ONE stream of "a
human is needed here" prompts, regardless of whether the ask originated
from a hook's `Ask` action, a gated `kj` verb, or a `shell_write` call.
`PermissionOption` already carries free-text `id`/`label`/`kind`
(`mcp/permission.rs:56-60`) — populate it from the ledger's own
`NewOption`/`OptionRow` list (`approval_allow_once`/`deny`/eventually
`allow_always`) instead of the empty `Vec::new()` `run_permission_ask`
sends today, so the two option vocabularies converge instead of drifting.

**What does NOT merge, and stays exactly as different as it is today:**
whether there's a durable row. `HookAction::Ask` stays ephemeral for
call-shaped hooks that don't want ledger bookkeeping (a `Log`-adjacent Ask
used purely as a UI speed bump); anything that wants rules, auto-decide,
`kj approve`-from-anywhere, or an audit trail goes through
`approval_ledger` the way `kj cc send` does today. The unifying move is
"one wire notification," not "one policy engine" — collapsing those would
be exactly the kind of generalization CLAUDE.md says to prefer deleting
over building.

**Answering converges on the ledger, both routes.** When a
`PermissionAsker` answers a ledger-backed ask (its `request` carries the
ledger's `request_id`), the answer becomes a `claim` + `decide` call against
that SAME row — the identical two-step transaction `kj approve allow`
already performs, just triggered by a wire event instead of a CLI
invocation, and racing safely against a concurrent `kj approve` the same
way two `kj approve` callers already race today (guarantee 5: exactly one
answerer wins, the loser reads `AlreadyDecided`, never a silent no-op).
There is exactly one place a ledger-backed ask gets decided, whichever
surface a human used to decide it.

### A future advisory input: lfm2d risk scoring (hook point, not a slice)

Amy asked 2026-08-17 whether lfm2d-scored risk feeding the ledger via rc was
still planned. It isn't written down anywhere as a plan — the pieces exist
(the `/v1/cascade` scorer, this ledger, rc's per-context-type composition,
this gate's `HookAction::Ask` seam) and the join does not, and it is filed
in full in `docs/issues.md` ("lfm2d risk scoring into the approval ledger,
via rc") — read that entry rather than this paragraph for the receipts; it
is not re-derived here.

What this design owes that entry is a seam that can accept the input
without inviting the wrong one. The constraint is not optional: **a score
may RAISE a prompt that would not otherwise fire, or enrich one that does;
it must never lower one, and must never silently allow** — grounded in a
real measurement (`/v1/cascade` scored `git checkout -- crates/` at 0.210
"situation-normal," confidently wrong, on an operation that has already
destroyed uncommitted work in this repo). This is expressible today without
a schema change: `approval_signals` (`schema.rs:364-377`) already exists
for exactly this — a `SignalSourceKind::Classifier` row with a `verdict` of
`escalate`/`deny`/`allow`, attached to an ask — and, verified by reading
`ask.rs`/`rules.rs`/`decide.rs`, **nothing in this crate reads a signal to
auto-decide anything today.** `rules::redeem` composes coverage from
`approval_rules` alone; `approval_signals` rows are write-and-display only.
That is structurally the right shape for "advisory, never authoritative" —
a future consumer of `NewSignal` must keep it that way: a classifier's
`allow` verdict may never become an input to `AskCoverage::verdict()`,
only to what a human sees before they decide. Where the score would come
*from* is a per-`context_type` rc question, not a kernel one — an
`/etc/rc/<type>/create-or-verb/SXX-risk.kai` that calls the scorer before a
destructive verb and, if the score says raise, forces escalation (skips
rule-coverage auto-allow, goes straight to asking) rather than kernel code
deciding for every context type at once. None of this is scheduled — it is
blocked on the re-measurement `docs/issues.md` calls out (one sample is not
a distribution), named here only so the seam isn't retrofitted badly later.

## Slices

Sequenced so each lands with its own tests and nothing downstream depends
on an unreleased kaish. Slice 6 is explicitly not scheduled.

**Slice 1 — `kj hook`.** `list`/`show`/`remove`/`add`, wired to
`Broker::persist_hook_insert`/`persist_hook_delete` + the matching in-memory
`HookTables` mutation, gated by `KjDispatcher::require_cap` against a new
`Capability::HookAdmin` (or an existing broad-enough admin cap — an
implementation question), never touching `evaluate_phase`. Test: set
`PreCall Deny("*")`, confirm `builtin.hooks`' own `hook_list`/`hook_remove`
tool calls return `Denied`, then confirm `kj hook remove <id>` still
succeeds and the subsequent tool call is no longer denied — in the same
process, no restart. No wire change.

**Slice 2 — the two error shapes.** `PermissionAskOutcome` (or equivalent)
splitting `run_permission_ask`'s three outcomes into `Proceed` /
`Denied(reason)` / `Unavailable(reason)`; `McpError::GateUnavailable {
by_hook, reason }`; `error_to_hook_json` gains the fourth arm. Tests: no
subscriber attached → `GateUnavailable`, not `Denied`; subscriber attached,
times out → `GateUnavailable`; subscriber answers no → `Denied`, unchanged.
No wire change (the JSON shape gains a field/kind, doesn't remove one).

**Slice 3 — the `shell`/`shell_write` rename.** Swap `ShellServer::new` /
`new_read_only`'s tool-name and instance-name identities per the table
above; move the facade projection; audit and update every
`S10-binding.kai` that grants `facade:shell` today, per context type,
deciding read vs write intent. Tests: a context bound to (old)
`facade:shell` sees the mutating tool disappear and the read-only one
appear under the name `shell`, unless its rc script was updated in the same
change to request `facade:shell_write`; a context newly granted
`facade:shell_write` gets exactly the tool `builtin.shell` provides today,
under the new name. This is a flag day — no dual-name transition period,
per Amy's "explicit is better" ruling on transparent routing.

**Slice 4 — the `shell_write` gate itself**, built on `plan_program`,
shipped against 0.14.1 (pending Amy's call on the fork above): submitted
source → `plan_program` → one `NewPlanStatement` per `PlannedStatement`,
free/bound variables carried through from `Plan::free_variables`/
`bound_variables` → one ledger ask per submission → `AskCoverage::verdict`
composes across statements (deny wins, allow needs every statement, else
escalate — already built, `rules::redeem`/`AskCoverage`) → escalation goes
through `run_gate`'s wait-and-notify path (now sharing the
`subscribePermissionEvents` seam per above). Any `PlannedStatement` whose
commands include a heredoc redirect is refused outright with a message
naming the unrenderable-on-0.14.1 reason, not silently shown. `kj approve`
answers it exactly like a `kj cc send` ask. Tests: a plain multi-statement
submission with one statement covered by an active deny rule refuses the
whole call; an uncovered submission escalates and blocks on
`gate_wait_timeout`; a heredoc-bearing statement refuses immediately with
no wait, regardless of rule coverage; `plan_program`'s `Stmt::Empty`
index-gap gotcha (`docs/issues.md`) has a regression test — a leading blank
line or comment in the submission must not shift which `PlannedStatement`
a later error message blames.

**Slice 4b — deferred, not scheduled.** Swap the renderer onto kaish 0.15's
`PlannedHeredoc`/`expand_fragment` once 0.15 is released and pinned; lift
the heredoc refusal for statements where `literal` says the body is honest.

**Slice 5 — retire the six `Latch` producers, and retire the word with
them.** `kj/workspace.rs`, `kj/doc.rs`, `kj/context.rs` (×2 —
archive/remove and retag), `kj/preset.rs` each get a `GateSpec` builder
shaped like `cc.rs::gate_spec_for_send` (raw typed reference as
`authorized_label`, the verb's own destructive target as the statement) and
call `run_gate` in place of returning `KjResult::Latch`. This is also where
the vocabulary sweep below lands — see "Vocabulary: retiring latch, not
renaming it" for why it belongs here and not in a separate pass. In one
slice:

- `KjResult::Latch` and its five `.is_latch()` call sites
  (`kj/workspace.rs:538`, `kj/doc.rs:882`, `kj/context.rs:2619,3040`,
  `kj/preset.rs:525`) are deleted, not renamed — nothing in the new model
  returns a "pending, resubmit with a flag" state (see below). The five
  test sites are rewritten to assert on `GateOutcome`/a durable ask row
  instead (`workspace_remove_requires_latch` becomes
  `workspace_remove_requires_approval`, asserting the dispatch call blocks
  and a pending ask row exists, not that a specific enum variant came
  back).
- The bare `--confirm` flag and its `${verb} --confirm` hint text are
  deleted for these six verbs — confirmation now happens via `kj approve
  allow <request-id>`, a different verb, answerable from any session, not a
  flag on the one that triggered the ask.
- `is_gated_verb`'s distill-only special case goes away; it now covers all
  seven gated verbs (`cc send` plus the six) uniformly.
- `latch_result`/`latch_from_result` and the `LATCH_COMMAND_KEY`/
  `LATCH_TARGET_KEY`/`LATCH_HINT_KEY` baggage keys are replaced by
  `attach_approval_baggage`/`approval_from_result` and two baggage keys,
  `kj.approval.request_id` / `kj.approval.status` — narrower than what they
  replace, on purpose (see below).
- `kaijutsu.capnp`'s `executeKj` result struct, `KjLatch` in
  `kaijutsu-client`, and the `kaijutsu-acp` confirmation-error path are
  renamed and repurposed in place, not deleted — see below for the wire
  verification and the exact new shape.

This slice is also where `kj cc send` itself picks up the new baggage keys
— today it produces neither the old latch baggage nor any structured
successor, just a formatted `KjResult::Err` string
(`cc.rs:110-115`). Slice 5 completes the pattern for the first consumer at
the same time it builds it for the other six, so there is one shape, not a
grandfathered original plus six followers.

**Slice 6 — predicate-shaped allow-always. Not scheduled.** The additive
schema shape is captured above; this slice is "design the actual filter
UX Amy described" plus the trigger/`learn_from_approval` loosening, and it
does not start until that UX conversation happens.

## Vocabulary: retiring latch, not renaming it

Amy, 2026-08-17, reading a draft of this doc: *"I notice we still mention
latch, which was a kaish thing that spread here a bit. Unless we're having
latch as a kaijutsu primitive we expose, we should be endeavoring to work
the latch language out in favor of clear kaijutsu ledger integrations."*

Same shape as the 2026-08-16 de-CRDT sweep (`95691509`, `ae789f71`,
`a36d8e9e`): a word that named a borrowed mechanism outlived the mechanism.
kaish 0.14 deleted its confirmation latch outright — `kaish_kernel::nonce`,
`NonceStore`, `ExecContext::verify_nonce`/`latch_result`,
`ExecResult.latch`, `JobStatus::Latched` are all gone upstream. Kaijutsu
kept the word for the shape it built to replace what kaish removed
(`KjResult::Latch`, `--confirm`, the `kj.latch.*` baggage keys, the capnp
`hasLatch`/`latchCommand`/`latchTarget`/`latchMessage` fields). CLAUDE.md's
rule 3 — "the durable and wire vocabulary is Kaijutsu's domain vocabulary...
not a text engine's encoding of them" — was written about a different
borrowed system, but it's the same rule: the ledger is ours, built for this
project, and its vocabulary should say so.

**This is not a separate sweep; it lands inside Slice 5**, because Slice 5
is where the replacement mechanism actually exists to name. Renaming
`KjResult::Latch` before `run_gate` produces anything to put in its place
would be renaming a placeholder, not a primitive — the whole point of "work
the language out in favor of clear kaijutsu ledger integrations" is that
the new names should describe what the ledger integration actually does,
and that isn't known precisely enough to name well until Slice 5 builds it.

### The answer to "are we keeping latch as a kaijutsu primitive": no

Amy's own framing left the door open — rename it if we're keeping it as a
primitive we expose. The honest answer, argued from the mechanism Slice 5
actually builds, is **no**: the old latch was a two-phase, single-session,
synchronous shape — a verb refuses immediately, prints "retype this with
`--confirm`," and the SAME caller's next call executes it directly, no
ledger, no other party. `run_gate` is a different shape entirely: it
blocks the ONE call, in-process, up to `gate_wait_timeout`, while any
session's `kj approve` (or a `subscribePermissionEvents` subscriber, once
the shared seam lands) can decide it; by the time the call returns, the ask
is already terminal. There is no "pending, resubmit" state in the new
model for a `NeedsApproval`-shaped variant to name — the lead's suggested
family (`KjResult::NeedsApproval`, `kj.approval.{command,target,hint}`,
`KjApproval`) is close but the middle one over-carries: `command`/`target`
told the caller what to *retype*, and the new model never wants a retype.
So the replacement is narrower than a 1:1 rename, which is the correct
shape per CLAUDE.md's "prefer deleting a mechanism to generalizing it" —
some of this is genuinely retired, not renamed.

**What's retired outright** (Slice 5, no successor — the shape it named no
longer exists): `KjResult::Latch`, the bare `--confirm` flag and its hint
text, `is_latch()`, `latch_result`/`latch_from_result`,
`LATCH_COMMAND_KEY`/`LATCH_TARGET_KEY`/`LATCH_HINT_KEY`.

**What's renamed and repurposed** (same wire position, new meaning — the
ledger's own facts, not a retyped command):

| Old | New | Why this name |
|---|---|---|
| `kj.latch.command` / `kj.latch.target` baggage keys | `kj.approval.request_id` | The old pair told the caller what to retype. The new fact a caller needs is which ledger row to look up — `kj approve show <id>`, or hand it to a human — so one key replaces two. |
| (nothing — `hint` had no successor fact) | `kj.approval.status` | The terminal `ApprovalStatus` (`denied`/`expired`/`abandoned`), so a caller can tell "a human said no" from "nobody answered" (Ruling 2's distinction) without parsing the prose message. New, not renamed — the old shape never carried this because the old shape never waited for anyone. |
| `latch_result(command, target, reason, hint) -> ExecResult` | `attach_approval_baggage(result: &mut ExecResult, request_id: &str, status: ApprovalStatus)` | Mutates in place rather than constructing a fresh `ExecResult`, because the caller (`run_gate`'s callers) already has one from `KjResult::Err` and is only attaching two facts to it, not building a result shape from scratch the way the old latch did. |
| `latch_from_result(&ExecResult) -> Option<KjLatchInfo>` | `approval_from_result(&ExecResult) -> Option<KjApprovalInfo>` | Read-side mirror of the above; same rename logic. |
| `hasLatch :Bool` (`kaijutsu.capnp`, `executeKj` result) | `hasApproval :Bool` | True when this call was refused after an approval-gate ask reached a non-allow terminal state — not "you must retype something," which is what `hasLatch` meant. |
| `latchCommand :Text` | `approvalRequestId :Text` | The ledger `request_id`, empty when `hasApproval` is false. |
| `latchTarget :Text` | `approvalStatus :Text` | The terminal status string, mirroring the baggage key above. |
| `latchMessage :Text` | `approvalMessage :Text` | Unchanged role — the human-readable reason — new name only for consistency with its three siblings. |
| `KjLatch { command, target, message }` (`kaijutsu-client`) | `KjApproval { request_id, status, message }` | Field-for-field mirror of the capnp rename; drops `command`/`target` because nothing produces them anymore. |

### The wire is safe to rename — verified, not assumed

Two independent checks, because "verify it rather than taking my word" was
the instruction and a wrong answer here breaks every connected client at
once:

1. **Ordinal mechanics.** `executeKj`'s result struct (`kaijutsu.capnp:2196-2205`)
   is an anonymous method-result struct with no explicit `@N` per field —
   contrast `TraceContext` (`:14-15`) and `BlockId` (`:27-29`), which do
   carry explicit ordinals. For a struct like this, Cap'n Proto assigns
   implicit ordinals by declaration order, and the ordinal — not the
   identifier spelling — determines the wire offset. Confirmed with `kaibo`
   (cast `crusoe`) against the actual schema text and the generated-code
   build path (`kaijutsu-server/build.rs:1-8`,
   `kaijutsu-client/build.rs:15-21` — codegen is build-time only, not
   checked in, `rerun-if-changed`-triggered). A same-position field rename
   is wire-byte-identical; only the generated Rust accessor name changes
   (`set_latch_command()` → `set_approval_request_id()`), which is a
   compile-time source edit at the two call sites
   (`kaijutsu-server/src/rpc.rs:4493-4501`,
   `kaijutsu-client/src/rpc.rs:2271-2275`), not a wire change.
2. **The project's own precedent for the opposite case, checked to confirm
   it doesn't apply here.** `BlockEvents::onBlockInserted`'s `ops` field is
   deliberately kept (always empty) rather than removed, because — per the
   comment right above it (`kaijutsu.capnp:562-564`) — "removing the field
   renumbers `subSeq`," the field declared after it in the same struct.
   That hazard is real for a **medial** field. `latchCommand`/
   `latchTarget`/`latchMessage`/`hasLatch` are the last four fields in
   `executeKj`'s result — nothing follows them — so the renumbering hazard
   the `ops` precedent guards against does not apply, which is also why
   this design repurposes them in place (rename) rather than needing the
   `ops` treatment (retire-empty) or a riskier append-new/drop-old.

**Version-skew hazard:** an old client (compiled against the pre-rename
schema) mid-restart would read the new field values through old accessor
names — cosmetically confusing (a variable named `latch_command` would
hold a request id) but not a wire break, and bounded to the same-day
window this project already restarts within for a schema change. Not zero
risk, but the same risk every other flag-day rename in this codebase
already accepts.

### The ACP wording needs a real fix, not just an identifier rename

`kaijutsu-acp/src/lib.rs:638-651` builds its own JSON, independent of the
capnp field names — `"latch": {"command": ..., "target": ...}`,
`"confirmation": "not performed; submit an explicit follow-up command"`,
and the message `"kj command requires explicit confirmation: {}"`.
Checked, per the instruction not to assume this is already clean: the
*message* text doesn't say "latch," but the JSON key does, and — the more
important finding — **the advice is now wrong**, not just mis-named.
"Submit an explicit follow-up command" describes the old two-phase
`--confirm` shape; under `run_gate`, resubmitting the same command doesn't
answer anything, it creates a brand-new ask and blocks again. The fix
Slice 5 needs to make here is behavioral, not lexical:

```json
{
  "reason": "kj command was refused by the approval gate (denied): <message>",
  "approval": { "request_id": "...", "status": "denied", "message": "..." },
  "next_steps": "kj approve show <request_id> from any session; allow/deny it there"
}
```

### Explicitly out of scope — do not touch these

The lead measured the real surface already; this design doesn't re-measure
it, only restates the boundary so whoever executes Slice 5 doesn't have to
rediscover it. **In scope** (13 files, ~172 occurrences, all genuinely the
confirmation latch this section retires or renames): `kj_builtin.rs`,
`kaijutsu-server/src/rpc.rs`, `kj/context.rs`, `mcp/servers/shell.rs`
(reads the baggage keys to shape its own `KernelToolResult`),
`kaijutsu-client/src/rpc.rs`, `kj/doc.rs`, `kaijutsu.capnp`, `kj/mod.rs`
(the `KjResult::Latch` variant and `is_latch()` definitions),
`kj/workspace.rs`, `kj/preset.rs`, `kaijutsu-acp/src/lib.rs`,
`kaijutsu-client/src/actor.rs`, `kaijutsu-server/tests/rpc_integration.rs`.

**Out of scope — the ordinary electronics/musical sense of "latch," and
touching any of these is a bug, not a cleanup:** `beat.rs`, `dj/midi.rs`,
`input/dispatch.rs` (a stick-direction latch), `midi_in.rs`,
`patch_bay/mod.rs`, `llm_stream.rs` (the once-per-context warn latch),
`flows.rs`, `system_prompt.rs`, `clock.rs`, `midi_presence.rs`,
`dj/thread.rs`. These name a boolean-holds-state or a latched-note concept
that has nothing to do with kaish's deleted confirmation mechanism; they
were each individually verified, not pattern-matched, and they stay
exactly as they are.

## What this does NOT do

- **It does not add a security boundary.** Every gate here is a footgun-
  visibility device between cooperating players, not an access-control
  system between trusted and untrusted actors. A player who wants to bypass
  a gate they set up themselves can revoke the rule or answer their own
  ask — nothing here is designed to resist that, because resisting it would
  be exactly the "wall the music has to climb" `instrument-design.md` warns
  against.
- **It does not make `shell_write` safe to leave granted broadly.** The
  split narrows *which* tool is reached for by accident; it does nothing
  about a context that legitimately holds `facade:shell_write` and is
  simply wrong about what it's about to run. The gate is the mitigation for
  that case, not the split.
- **It does not gate mid-execution.** The crux section is explicit: kaish
  0.14.1 has no per-command interception hook, so a `shell_write` call that
  clears the gate runs to completion once started, the same as every kaish
  execution today. A script that plans safe and behaves differently at
  runtime (a `$(...)` that resolves unexpectedly, a `read` that binds late)
  is not caught by this design and cannot be with the current kaish API
  shape.
- **It does not solve the file write/edit ledger gap.** `builtin.file:write`
  /`:edit` routing through the ledger (`docs/issues.md`, "The file write/edit
  tools are not gated by the approval ledger") is the same infrastructure
  this doc extends but is explicitly out of scope here — it's a third
  consumer of `run_gate`-shaped plumbing, not part of the `kj`/`shell_write`
  seam, and deserves its own slice list.
- **It does not build the `kaish_ro` scratch-space widening.** The
  read/write shell split this doc executes is a prerequisite Amy named for
  that work, not that work itself. `shell` stays exactly as restricted on
  the exec axis after this design as `read_only_shell` is today —
  `ExternalExec::Deny`, no curated host-binary allow-list, no scratch
  directory. That's a separate `docs/issues.md` entry and a separate design
  pass.
- **It does not touch kaish's own upstream gate.** kaish's `latch_enabled`
  was already `false` in kaijutsu before 0.14 removed the mechanism
  entirely (`docs/issues.md`); nothing here turns any kaish-side latch back
  on. Everything in this doc is kaijutsu-side, built on `plan_program` as
  read-only planning input, never on kaish's own (now-deleted) confirmation
  primitives.

## Wrong at HEAD — findings from reading the actual current source

- **The `bindings_builtin.rs:22-25` doc comment the task brief asked to fix
  is not stale.** Reading the file at HEAD (`mcp/servers/bindings_builtin.rs:22-33`),
  it already says exactly the right thing: hooks persist and rehydrate at
  `Broker::set_db`, a self-lockout survives a restart, the doc explicitly
  notes it *used to* claim the opposite ("hooks are in-memory... restart to
  recover") and was corrected 2026-08-12. It already names the SQLite-
  hand-edit workaround as forbidden and points at this exact `docs/issues.md`
  entry. There is nothing to fix in that comment; Slice 1 makes the gap it
  correctly describes stop existing, which is a different thing from fixing
  the comment.
- **Line/ordinal citations drift under concurrent editing — expect it, don't
  trust a citation without re-checking.** The task brief's `broker.rs:1465`
  (for the hook-hydrate comment) and `subscribePermissionEvents @103` both
  point at different code than they did when written — hydrate logic sits
  around `broker.rs:313-392` at HEAD, and the ordinal is `@93`
  (`kaijutsu.capnp:2185`). Three other lanes are editing this same tree
  today; every specific citation in this document should be re-grepped, not
  trusted, by whoever implements a slice.
- **The `kj cc send` gate is a fuller implementation of "Gate slice 1a" than
  `docs/issues.md`'s entry for it suggests.** The entry ("Gate slice 1a —
  three findings from the research pass") reads as a stopped-mid-flight
  status ("stopped at the manifest wiring when the day ended"), but
  `kj/gate.rs` and `kj/approve.rs` are complete, tested (rules-first
  short-circuit, patient-hold timeout coordination, an auto-decided path,
  a human-answers-from-elsewhere path, a deny path, and the guarantee-3
  free-variable end-to-end test), and already wired as `kj cc send`'s real
  code path, not a prototype. `approval_ledger::ask::list_pending` — flagged
  in that same entry as "compiles but has no test" — has direct test
  coverage today via `kj/gate.rs`'s own tests (`list_pending` is how the
  test harness finds the request id to answer). Whoever picks up Slice 4/5
  should read `kj/gate.rs` as a working reference implementation, not a
  half-finished stub.
