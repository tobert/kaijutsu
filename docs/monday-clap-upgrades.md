# Monday clap upgrades — per-subcommand tool schemas

Status: **Part 1 (kaish) DONE 2026-06-08 · Part 2 (kj) in progress — `cas` migrated 2026-06-08**
Spans two repos: **kaish** (`~/src/kaish`) and **kaijutsu** (`~/src/kaijutsu`, the `kj` builtin).

---

## Session log — 2026-06-08 (Part 1, kaish side, complete)

Part 1 landed in `~/src/kaish`, generic and **inert** until a tool populates
`subcommands` — safe to ship ahead of kj. Decisions made while implementing:

- **Forward-only, no backward compat.** The only consumers are kj + kaish, both
  ours, so we move them forward together rather than carrying compat shims. The
  new `ToolSchema` fields use `#[serde(default, skip_serializing_if =
  "Vec::is_empty")]` purely for **wire hygiene** (the ~85 flat builtins don't emit
  `"subcommands":[],"aliases":[]`); `default` is then required so a flat tool's
  key-absent payload round-trips. This is not a compatibility mechanism.
- **`select_leaf` routes on literals only.** A subcommand selector must be a
  bareword / quoted string (both parse to `Expr::Literal(Value::String)`). A
  *computed* positional (`$(…)`, `$VAR`, glob, arithmetic, interpolation) sitting
  where a subcommand is required is a **loud error**, not a silent degrade — e.g.
  `kj $(echo context) …` fails with "a subcommand name is required here, but got a
  command substitution". The `--flag=value` form binds without schema lookup and
  always works regardless of routing.
- **`select_leaf` lives only in the async `build_args_async`.** Investigation
  showed the sync `build_tool_args` twin is **not** a real binding path for
  commands — every real command (foreground and in pipelines) binds through
  `build_args_async`; the sync builder only parses scatter/gather *options* and
  backs a `#[cfg(test)]` dispatcher. So subcommand tools never reach the twin, and
  there's no routing-divergence risk. Eliminating the twin entirely (the stated
  goal) was deferred to kaish `docs/issues.md` (P3) — it's a "fewer paths to reason
  about" cleanup touching ~27 test sites, out of this feature's way.
- **Global value-flags skip their value during routing.** A root-declared non-bool
  flag (kj's global `--confirm <nonce>`) can precede the subcommand path in space
  form; routing skips its value positional so it isn't mistaken for a subcommand
  selector. Leaf-specific value flags can't precede their own subcommand by
  construction, so only the root's flags need this.
- **Subcommand-path positionals stay in argv** (not consumed). kj keeps
  `map_positionals=false` and re-parses the full argv with its own clap, so it
  needs `context create` as positionals; `select_leaf` only picks the leaf for
  `param_lookup`.
- **`get_tool` map_positionals override** now only force-sets `map_positionals=true`
  when `subcommands.is_empty()` (genuinely flat backend/MCP tools). Subcommand
  tools keep their declared per-leaf value.

**`--json` — RESOLVED 2026-06-08 (was the 2.4 open question).** Decision: **kj
keeps its own JSON** (richer per-leaf envelopes), via a new explicit kaish-side
marker rather than relying on dispatch-path accident.

- Finding: the kernel only re-formats output for the **builtin** dispatch path;
  the **backend** path (where `kj` runs, via `backend.call_tool`) already returns
  the tool's `ExecResult` untouched. So kj was *already* self-rendering — just
  implicitly and unadvertised.
- New marker: `ToolSchema.owns_output` (kaish, commit `cd23012`). When set, the
  kernel skips `apply_output_format`; the tool owns `--json`. Builder
  `with_owned_output()` marks the whole subcommand tree **and** re-advertises a
  `json` flag on each node (reflection skips `json` as kernel-global, so this is
  what makes `help kj <sub>` list `--json` where kj genuinely handles it).
- **kj action in Part 2:** call `.with_owned_output()` on the composed schema
  (`schema_tree_from_clap(&kj_command(), …).with_owned_output()`). kj continues
  to render its own JSON; no adoption of the generic formatter. Leaf clap structs
  keep their `GlobalFlags`/`json` parse so kj sees `--json` and renders.

No remaining open questions for the kaish side.

---

## Session log — 2026-06-08 (Part 2 planning, kj side)

Two architectural decisions taken before starting Part 2:

- **Dispatch: keep the manual top-level `match`, add a separate `kj_command()`
  for schema only.** The `match` in `kj/mod.rs::dispatch` (~291) carries real
  behavior the bare clap tree does not model: the active-context gate (most verbs
  exempt; `fork`/`drive`/`stage`/`drift`/`cache` require a joined context), the
  async-vs-sync handler split, and per-leaf capability checks already living
  inside each `dispatch_*`. Re-threading all that through a derived `KjArgs` enum
  (the doc's original §2.1) buys little: the leaf flag-complexity is single-sourced
  through each `*Args` struct **either way**, because each `dispatch_*` parses its
  own argv via `*Args::try_parse_from` (block/doc/search already do this). So
  `kj_command()` composes the same `*Args::command()`s purely for reflection; the
  only surface that must agree between router and schema is the top-level
  name/alias list, which is small and stable. **Chosen over the full enum.**
- **Strategy: migrate every subcommand to clap first, then flip the schema in one
  atomic step (Option A).** The flat `schema()` that `11160e5` hand-reconciled
  keeps driving kaish unchanged while each subcommand is migrated as an
  internal-only refactor (no external behavior change — kaish still binds against
  the flat schema). The final commit composes `kj_command()`, reflects it with
  `.with_owned_output()`, and deletes the flat `schema()` together. Lowest risk;
  no window where an un-migrated leaf binds *worse* than today. **Chosen over
  incremental schema (Option B).**

**Inventory correction — the §2.2 table was missing 7 subcommands.** `dispatch`
routes **~19** subcommands, not the 12 the original table listed. The flat
`schema()` cannot be deleted (§2.3) until *all* of them are clap. Critically,
**`cache` carries `--target`/`-t`** and is the *other half* of the `-t` collision
§2.3 claims to resolve (`cache add -t target` vs `context list -t tree`) — it was
absent from the table. Full set, with the 7 additions folded into the migration
order below:

| Subcommand | clap today? | value flags | ctx-gated | notes |
|------------|:-----------:|:-----------:|:---------:|-------|
| `block`     | ✅ | — | no  | done |
| `doc`       | ✅ | — | no  | done |
| `search`    | ✅ | — | no  | done |
| `models`    | ❌ | 0 | no  | pure discovery |
| `attach`    | ❌ | 0 | no  | ctx positional only |
| `cas`       | ❌ | 0 | no  | fixes `--out` positional (`cas.rs:75`) |
| `stage`     | ❌ | 0 | **yes** | `commit/go`, `include/in`, `exclude/ex` aliases |
| `drive`     | ❌ | 1 | **yes** | `--prompt` |
| `transport` | ❌ | 1 | no  | `--context` |
| `model`     | ❌ | 1 | no  | `--context` |
| `drift`     | ❌ | 2 | **yes** | `--summarize`/`-s` |
| `policy`    | ❌ | 2 | no  | `--timeout-ms`, `--max-result-bytes` |
| `binding`   | ❌ | 0 | no  | show/allow/revoke/reset + aliases; ctx positional |
| `workspace` | ❌ | 3 | no  | `--read-only`, `--mount` (doc-only, decide) |
| `cache`     | ❌ | 3 | **yes** | **`-t`/`--target`**, `--ttl`, `--index`/`-i` |
| `preset`    | ❌ | 5 | no  | `--model`/`-m` |
| `rc`        | ❌ | 7 | no  | `--type`, `--verb`, `--content` (+ stdin path) |
| `context`   | ❌ | 9 | no  | `--type`, `--parent`/`-p`, `set`-keyword footgun |
| `fork`      | ❌ | 30| **yes** | 4 variants; flags route between them — `fork` last |

16 manual subcommands to migrate. Order = ascending flag-count / risk (table is
already sorted). `fork` stays last. `cache` must land before the §2.3 flip for
`-t` to resolve.

**As-built so far:**

- **Shared `clap_help_for`** lifted out of `block.rs` into `kj/mod.rs` (now
  `pub(crate)`); every clap-migrated subcommand uses it for the empty-argv help
  path. `block.rs` imports it.
- **`cas` migrated** (`kj/cas.rs`): `CasArgs`/`CasCommand` clap derive
  (put/get/ls+`list`/info/rm+`remove`), `dispatch_cas` parses via
  `try_parse_from` with the block.rs DisplayHelp handling; `--out` is now a clap
  `Option<String>` (killed the `argv[1] == "--out"` index fragility); helper fns
  take `&str` instead of `&[String]`; the hand-rolled `cas_help` is gone (clap
  renders it). Operator-cap gate collapsed to a let-chain. 2 unit tests added.
  Flat `schema()` still drives kaish — no external change yet.
- **Test-dispatcher data-dir isolation** (`kj/mod.rs`) — see §2.5 prerequisite.

What shipped (all with tests, `cargo clippy --all` clean, `--no-default-features`
compiles):

- `kaish-types/src/tool.rs`: `ToolSchema.subcommands` + `.aliases`; builders
  `subcommand()`, `with_command_aliases()`; `matches_command()`. Serde round-trip
  + flat-payload tests.
- `kaish-tool-api/src/clap_schema.rs`: `schema_tree_from_clap` (recurses
  `get_subcommands()`, child name/desc from `get_name`/`get_about`, command
  aliases from `get_all_aliases()`). Exported from the crate root.
- `kaish-kernel/src/scheduler/pipeline.rs`: `select_leaf` (literal-only walk,
  fail-loud on computed, global-flag-value skip) + 12 unit tests; exported via
  `scheduler::select_leaf`.
- `kaish-kernel/src/kernel.rs`: `build_args_async` selects the leaf and binds
  flags / runs the `map_positionals` guard / does positional remapping against it;
  `get_tool` override guarded by `subcommands.is_empty()`. 4 end-to-end binding
  tests (deep-leaf space-form value flag, leaf bool not swallowing a positional,
  alias-routed leaf, computed-selector fail-loud).

---

## Why

`kj` hands kaish **one flat `ToolSchema`** via `get_tool`. kaish has no notion
of subcommands, so that schema is the union of every flag across every `kj`
subcommand. Two problems fall out of the flatness:

1. **Maintenance burden.** Every value flag any subcommand reads must be hand-
   listed in `kj_builtin.rs::schema()`, or kaish parses `--flag value` as a bool
   flag and divorces the value (silently dropped on the in-kernel builtin path;
   a loud failure on the `map_positionals=true` backend path). The fix landed in
   `11160e5` was exactly this hand-reconciliation — it will rot again.
2. **Short-flag collisions can't be resolved.** `-t` means `target` (value,
   `kj cache add`) in one subcommand and `--tree` (bool, `kj context list`) in
   another. A flat schema binds one short flag to one meaning; the long forms
   work but `-t` is permanently ambiguous.

Both dissolve if the schema kaish sees is **per-subcommand** instead of flat.

## North star: clap's `Command` tree is the single source of truth

clap already models exactly the shape we want: a `Command` has args
(`get_arguments`) and child `Command`s (`get_subcommands`), each with names and
aliases. The design is:

- Each builtin owns its clap struct (`BlockArgs`, `DocArgs`, …). `kj` **composes
  them at runtime** into one top-level `Command` (`Command::subcommand(...)`),
  and **dispatches through that same tree**.
- kaish reflects that tree into a **recursive `ToolSchema`** and, at parse time,
  **walks it** to pick the active leaf's params before binding flags.

Because routing and schema derive from the *same* composed `Command`, the
subcommand selector in kaish and the dispatcher in kj cannot drift — there is no
second copy of "what are the subcommands and their aliases." That single-source
property is what makes this safe; without it, a selector that disagrees with the
dispatcher just relocates the silent-misroute bug to a new layer.

Subcommand-awareness is **opt-in**: flat tools (`cat`, `grep`, `ls`) leave the
new field empty and the binder takes today's flat path. No flat tool changes
behavior.

---

# Part 1 — kaish side (`~/src/kaish`) — ✅ DONE 2026-06-08

Goal: let any tool expose a tree of subcommand schemas, and bind flags against
the correct leaf. Generic — not kj-specific. See the Session log at the top for
decisions and the exact files touched; the subsections below are the as-built
record.

### 1.1 Schema model — additive recursive field

`crates/kaish-types/src/tool.rs`

- Add to `ToolSchema`:
  - `subcommands: Vec<ToolSchema>` — child schemas; empty for flat tools.
  - `aliases: Vec<String>` — command-level aliases (`ls` → `list`, `rm` →
    `remove`), distinct from `ParamSchema::aliases`.
- Both `#[serde(default, skip_serializing_if = …)]` so the wire payload is
  unchanged for flat tools and the field is backward-compatible (old peers that
  don't send it deserialize to empty).
- Builder: `ToolSchema::subcommand(self, child: ToolSchema) -> Self`,
  `with_command_aliases(...)`.
- A leaf's `params` keep today's semantics, including `positional: true` markers
  and `consumes`.

`ToolSchema` becoming recursive is fine for serde (recursive types serialize),
and for `map_positionals` (a per-leaf flag — see 1.3 / 2.4).

### 1.2 Recursive clap reflection

`crates/kaish-tool-api/src/clap_schema.rs`

- `params_from_clap` already reflects **top-level** args only. Add:
  - `schema_tree_from_clap(cmd: &Command, name, description, examples) -> ToolSchema`
    that sets `params` from `params_from_clap(cmd)`, `aliases` from
    `cmd.get_all_aliases()`, and recurses over `cmd.get_subcommands()` into
    `subcommands`.
- Keep the existing skip rules (`--help`/`-h`, `--version`, kernel-owned
  `--json`). `json` stays skipped by reflection; kj re-advertises it on every
  node via `with_owned_output()` (the `--json` decision — see 2.4 / Session log).
- Bool detection stays `ArgAction::SetTrue/SetFalse`; value flags get
  `param_type` from clap's value parser where derivable (int vs string).

### 1.3 Subcommand-aware arg binding (the walk)

`crates/kaish-kernel/src/kernel.rs` (`build_args_async`, ~2440). **As-built note:**
the walk went into the async `build_args_async` *only*, not the sync
`build_tool_args` twin — the twin isn't a real command-binding path (scatter/gather
options + `#[cfg(test)]` only), so it never sees a subcommand tool. `select_leaf`
lives in `scheduler/pipeline.rs` next to `schema_param_lookup` and is exported as
`scheduler::select_leaf`.

Before building `param_lookup`:

```
let leaf = select_leaf(schema, &positionals);   // new
let param_lookup = schema_param_lookup(leaf);    // from the leaf, not the root
```

`select_leaf`:
- Walk positional tokens left to right. While the next unconsumed positional
  matches a child's `name` or one of its `aliases`, descend into that child and
  mark the token consumed-as-subcommand-path.
- Stop at the first positional that matches no child → that schema is the leaf.
- Handles multi-level (`block edit insert`, two hops) by construction.
- If `subcommands` is empty (flat tool), returns the root immediately — today's
  behavior, one branch.

Notes:
- **Global flags**: flags that apply at every level (kj's `--confirm`, maybe
  `--json`) should be marked `global` in clap so reflection lifts them onto every
  leaf, OR stay special-cased upstream (kj already extracts `--confirm` in
  `kj_builtin` before dispatch — leave that, just don't let the walk choke on it).
- **`map_positionals`** stays a property of the selected leaf. For kj it stays
  `false` (kj re-parses a reconstructed argv with clap; we do not want kaish
  remapping bare positionals onto named params). The loud-divorce guard
  (~2596) keys off `map_positionals`, so leaving it false keeps the guard off
  for kj while the per-leaf `param_lookup` makes value flags bind correctly
  anyway.

### 1.4 Wire / `get_tool`

`crates/kaish-kernel/src/kernel.rs` ~2197 (backend path forces
`map_positionals=true` today).

- `get_tool`/`list_tools` already return `ToolSchema`; the recursive field rides
  along for free once serde is wired. No protocol method changes.
- Re-examine the `s.map_positionals = true` override at ~2199: with per-leaf
  schemas this should respect the leaf's own `map_positionals` rather than
  blanket-forcing true. (kj wants false; genuinely flat MCP tools want true.)
  Safest: only force-true when the schema has no subcommands AND the leaf didn't
  set it — i.e. preserve an explicit author choice.

### 1.5 Tests (kaish)

- `select_leaf`: flat tool → root; single hop; two hops (`a b c`); alias hop
  (`ls` resolves to `list`'s params); unknown subcommand → root leaf, no panic.
- Binding: a value flag declared only on a deep leaf binds in space form; a
  bool flag on the leaf doesn't swallow the next positional.
- Backward-compat: a schema with no `subcommands`/`aliases` round-trips over
  serde identically to today.

---

# Part 2 — kj side (`~/src/kaijutsu`)

Goal: make `kj`'s clap `Command` tree the one source of both **routing** and
**schema**, and delete the hand-written flat `schema()`.

Recipe reference: `~/src/kaish/docs/clap-migration.md` (the per-builtin clap
migration the block/doc/search subcommands already followed).

### 2.1 Compose `kj_command()` for schema; keep the `match` for routing

`crates/kaijutsu-kernel/src/kj/mod.rs` (`dispatch`, ~291).

**Decision (2026-06-08): the manual top-level `match` stays.** It carries the
active-context gate, the async/sync split, and per-leaf capability checks — see
the Part 2 planning session log. Routing continues to call each `dispatch_*`,
which parses its own argv via `*Args::try_parse_from` (the block/doc/search
pattern). The single-source guarantee holds at the **leaf** level, where the flag
complexity is, because router and reflection consume the same `*Args`.

- Add a free function `kj_command() -> clap::Command` that composes every
  subcommand's `::command()` as a child:
  `Command::new("kj").subcommand(ContextArgs::command()).subcommand(BlockArgs::command())…`
  (all ~19). This is used **only** for reflection, not dispatch.
- The top-level name/alias list in `kj_command()` must mirror the `match`'s
  `cmd ==`/`match cmd` arms (e.g. `ctx`→`context`, `ws`→`workspace`). Keep them
  as clap `visible_alias` on each `*Args` so a single edit moves both. This
  small, stable surface is the only thing that can drift; the leaves can't.
- `kj_builtin.rs::schema()` (~378) becomes
  `schema_tree_from_clap(&kj_command(), "kj", <desc>, <examples>).with_owned_output()`.
- `kj_builtin::execute` is unchanged — it keeps reconstructing a flat argv from
  `ToolArgs` (positionals + named + flags) and handing it to `dispatch`, which
  routes to the leaf's `*Args::try_parse_from`. clap accepts flags in any
  position, so the existing reconstruction ordering is fine. Additive: kaish
  binds against the per-leaf schema, kj re-parses with clap.

### 2.2 Migrate the manual subcommands (lightest first, `fork` last)

**Migration order is the corrected inventory table in the Part 2 planning session
log** (16 manual subcommands, ascending flag-count). The original 9-row table here
was missing `models`/`attach`/`stage`/`model`/`policy`/`binding`/`cache`; see the
session log for the full set. Key sequencing constraints:

- `fork` (30 flag-sites, 4 variants: full/shallow/compact/subtree — flags route
  *between* variants) stays **last**; model it as a `ForkArgs` with a mode group.
- `cache` (`--target`/`-t`, `--ttl`, `--index`/`-i`) must land **before the §2.3
  flip** — it's the leaf that resolves the `-t` collision against `context list`.
- `context` carries the `set`-keyword footgun (`gotcha_kaish_set_keyword`): kaish
  treats bareword `set` as reserved, so `kj context "set"` is the routed form. The
  clap `Set` variant just needs its name; the quoting lives on the caller side.
- Context-gated leaves (`stage`/`drive`/`drift`/`cache`/`fork`) are gated by the
  `match` arms in `dispatch`, *not* by clap — migrating them to `*Args` doesn't
  move the gate. Leave the gate where it is (§2.1 decision).

Per subcommand: lift the `extract_named_arg`/`has_flag` reads into a clap struct
with typed fields, keep the handler body, parse via `try_parse_from` (block/doc/
search already show the shape). Aliases and help text move onto the struct.
Each migration is internal-only — the flat `schema()` still drives kaish until
§2.3, so external binding behavior is unchanged and the two `11160e5` regression
tests stay green throughout.

### 2.3 Retire the hand-written flat schema

Once all subcommands are clap and composed in 2.1:

- Delete the long `.param(...)` chain in `kj_builtin.rs::schema()` (the thing
  `11160e5` grew); schema is now reflected.
- The `-t` collision **resolves**: `target` and `tree` live on different leaves,
  each with its own `-t`. Drop the inline TODO note added in `11160e5`.
- The regression tests added in `11160e5`
  (`context_create_type_flag_binds_both_forms`,
  `block_append_text_flag_binds_space_form`) should pass unchanged — they assert
  end-to-end behavior, not schema shape. Keep them as the guard that the
  reflected schema still binds correctly.

### 2.4 Decisions to settle while migrating

- **`--json`** — ✅ RESOLVED 2026-06-08 (see Session log at top). Keep kj's own
  JSON. kaish added `ToolSchema.owns_output` + `with_owned_output()` (commit
  `cd23012`): the kernel skips its generic formatter for tools that own their
  output, and `with_owned_output()` re-advertises `json` across the tree. kj
  calls `.with_owned_output()` on the composed schema and keeps rendering its own
  envelopes; no generic-formatter adoption.
- **`map_positionals`**: confirm kj leaves it false on every leaf (see 1.3).
- **`--confirm`**: keep the pre-dispatch nonce extraction in `kj_builtin`; mark
  it a clap `global` arg so reflection still advertises it on every leaf.

### 2.5 Tests (kj) — two levels

**Key realization (2026-06-08):** the flat `schema()` *already* hand-lists every
subcommand's flags, and kaish reconstructs argv positionals-first. So during
Option A a per-subcommand migration has **no externally-observable change at the
kaish level** — a `embedded_with_kj` test on, say, cas's `--out` would be green
both before and after, because kaish normalizes order before kj ever sees it. The
genuine behavior the flat schema *cannot* express (and that gives real red→green)
only appears at the flip. So tests split into two levels:

- **Per-subcommand (unit, lands with each migration):** call `dispatch_<sub>`
  **directly** with argv that exercises kj's *own* clap re-parse robustness —
  flag-before-positional order independence, alias routing, value-flag typing.
  These go red against the manual hand-parser and green against clap, and they're
  the right granularity because the re-parse is the thing each slice changes.
  *Example shipped:* `kj/cas.rs::tests` — `cas_get_out_flag_before_positional`
  (kills the old `argv[1] == "--out"` fragility) + `cas_aliases_route`
  (`list`→`ls`, `remove`→`rm`).
- **The flip (integration, lands with §2.3):** through the real
  `embedded_with_kj` → kaish → kj path —
  - `-t` disambiguation: `kj cache add … -t <target>` (value) vs
    `kj context list -t` (bool) both correct *only after* per-leaf schemas. This
    is RED until the flip; it's the flip's acceptance gate.
  - The two `11160e5` tests (`context_create_type_flag_binds_both_forms`,
    `block_append_text_flag_binds_space_form`) stay green throughout — kept as the
    guard that the reflected schema still binds.

**Harness prerequisite (done):** `test_dispatcher` now roots the kernel at a
per-test temp `data_dir` (`kj/mod.rs`) so CAS (and other data_dir state) never
touches the user's real XDG store. Broader audit tracked in `docs/issues.md`.

---

## How the two parts interlock

1. **kaish Part 1 first** (schema field + reflection + walk + tests). It's inert
   until a tool populates `subcommands`, so it ships safely ahead of kj.
   ✅ DONE 2026-06-08 — see Session log at top.
2. **kj Part 2 incrementally.** Compose the top-level `Command` (2.1) and migrate
   subcommands one at a time (2.2); each is independently shippable. The flat
   `schema()` can stay as a fallback until the last subcommand lands, then 2.3
   deletes it.
3. The clap migration in Part 2 is the same work the cheaper "flat union" option
   would have needed — so none of it is throwaway, and it's what gives Part 1 its
   single-source-of-truth guarantee.

## Out of scope / deferred

- Passing **structured `ToolArgs`** straight to kj instead of reconstructing a
  flat argv for clap to re-parse. Tempting (one parse instead of two) but a
  larger surface; the argv round-trip is fine for now.
- Generic positional→named remapping for kj (we explicitly keep
  `map_positionals=false`).
- Any change to remote-kaish protocol versioning beyond the additive serde field.
