# Personas, Evolved

> **Status (2026-05-15):** v1 `builtin.personas` was yanked. The
> "Context Templates" direction in this doc was superseded mid-design
> by the **rc system** — `/etc/rc/<context_type>/<verb>/SXX-name.{kai,md}`
> with `.md` → block insertion and `.kai` → kaish via
> `kaish_kernel::Kernel::execute_with_vars`. Templates as a separate
> stored type are dropped; init scripts handle dynamic templating
> directly. The rc lifecycle wires at `context.create`, the four
> fork variants, and `drift`. The `attach` verb is reserved with the
> semantic **`kj attach <ctx>` user action that pulls an existing
> context into the current session** — not the "driver attaches to
> resume" reading below, which predates the chosen semantic. See
> `docs/issues.md` for current rc follow-ups. The body below is
> retained as design history — it captures the invariants and the
> reasoning that led to rc.

How "personas" work today, where they're heading, and why the
substrate already gives us most of what they need for free.

This doc is planning-orientation reading. Open it before scoping work
that touches personas, context lifecycle, system prompts, or the
boundary between what's stored and what's run.

## What this doc is not

- Not a spec. The shape is sketched; details are deliberately open.
- Not a deprecation notice. v1 stays in tree until the replacement
  lands and seeds migrate.
- Not a description of how the model behaves at runtime — that's
  [`orchestration.md`](orchestration.md). This doc is about what
  shapes the binding *before* the turn pump runs.

## Three invariants we're building on

Everything else in this doc derives from these. They're written up in
detail in the auto-memory file `architecture_context_invariants.md`;
the short form:

1. **Conversations are append-only.** Blocks accrete; we don't
   rewrite history. Hydration from blocks happens at context create
   or re-instantiation. Forced rehydration exists for recovery.
2. **Fork is the mutation primitive.** When work would otherwise
   mutate a context, fork. The DAG keeps provenance; drift carries
   results back.
3. **Persona / mode switching has exactly two paths.**
   - **Fork-with-new-preamble** (preferred). New provider-side
     session, full re-ingest cost, fresh KV cache, clean state.
   - **Append-and-renotify** (possible, discouraged). Append a
     prompt to the running context, fire tool-change notifications,
     reinforce via reminder messages. Code-switching mid-conversation
     is unhealthy for models. Reserve for guidance/redirection.

A persona system that swaps the binding under a running context, with
no notification and no fork, violates all three. v1 does this. The
redesign doesn't.

## Where v1 stands

`builtin.personas` shipped in M3-D1 (commits `1190a0c`, `3fc0e17`).
It's a small MCP server with three tools:

| Tool | What it does |
| --- | --- |
| `personas_list` | Enumerate the registered personas (sorted JSON). |
| `personas_apply { name }` | Install the persona's instance allowlist as the calling context's `ContextToolBinding`. |
| `personas_define { name, instances, description? }` | Upsert a persona into the in-memory store. |

Three archetypes seed at server construction:

| Persona | Instances |
| --- | --- |
| `planner` | block, kernel_info |
| `coder` | block, file, hooks_builtin, kernel_info |
| `explorer` | file, block, resources_builtin, kernel_info |

The store is a `DashMap<String, Persona>` on the server itself —
in-memory only, no persistence, no `delete`, no validation of
instance ids at define time. `personas_apply` auto-injects
`builtin.personas` and `builtin.tool_search` regardless of the
persona definition, so a context can never paint itself into a corner
where it can't switch back or discover its own surface.

**Honest read**: v1 was probably premature as a separate type. The
auto-injection guard is paternalistic — it silently rewrites the
user's persona definition on apply, hiding the gap between defined
and effective bindings. The whole apply path swaps a binding under a
running context, which violates fork-as-mutation. The DashMap
duplicates persistence machinery the kernel already has via
`KernelDb`. The "personas need a system prompt" follow-up duplicates
what blocks already do.

What v1 still gets right: the role names (`planner` /
`coder` / `explorer`, plus the dropped `sound-engineer`) describe
useful tool surfaces. They translate to seed templates in the
redesign.

## The redesign: Context Templates + Init Scripts

Two ideas, both leaning on the substrate.

### Context Templates

A **template** is a named, reusable starting point for a context.
Concretely: a context with a marker that says "this is a template,
don't run a conversation against it." Templates hold whatever a
fresh context needs:

- A binding spec (the instances + per-instance policy).
- Preamble blocks: system prompt, AGENTS.md / .kaijutsu.md
  fragments, role-specific guidance.
- A list of init scripts to run at fork-from time.
- Optional hook bundles (`HookPhase::ListTools` filters, kaish
  hooks at other phases).

"Apply a persona" becomes **fork from a template**. Not "rewrite the
binding under the current context" — that path is gone. The DAG
records the fork; the templates are the named start states; the user
can drift refinements back from a working context to its source
template if the role is evolving.

This collapses several of v1's deferred items into "use what we
already have":

| v1 deferred item | What it becomes |
| --- | --- |
| Persistence of user-defined personas | Templates are contexts. KernelDb persists contexts. Free. |
| `personas_delete` | Delete a context. Existing primitive. |
| ListTools hook bundles in the persona shape | Hooks already attach to contexts. Templates carry hooks because templates are contexts. |
| System prompt component | A template's preamble blocks become the new context's preamble blocks at fork time. |
| `personas_define` instance-id validation | Editing a template is editing a context — same surface, same validation. |
| Multi-user / multi-agent persona evolution | CRDT comes free. |

### Context Init Scripts

A template alone is a static starting point. An **init script** is
the dynamic piece — a kaish script that runs at a context lifecycle
hook to build up blocks, set policy, or perform any setup the
template needs.

Lifecycle hook points (initial set; expect to refine):

| Hook | When it fires |
| --- | --- |
| `context.create` | A new context is created from scratch. |
| `context.fork` | A context is forked from another (template or working). |
| `context.attach` | `kj attach <ctx>` — user pulls an existing context into the current session. (Reserved; not yet wired. See status banner above for the chosen semantic, which differs from this doc's original "driver attaches to resume" reading.) |

Init scripts are kaish. The kernel runs them in template-specified
order, with the new context already created and addressable. A
script's job is anything that can be expressed as kaish + the
builtin MCP tools — insert blocks, configure binding, register
resource subscriptions, kick off background drift listeners.

Multiple templates compose by concatenating their init script lists.
A working context can be forked with `template = planner +
project-local-overlay`; the resulting context runs both sets of
init scripts, in declared order.

## System V RC, borrowed and not

System V `init` is the inspiration but not the target. What we
borrow:

- **Definition / invocation split.** SysV separates `/etc/init.d/`
  (where scripts live) from `/etc/rcN.d/` (where they're invoked
  per-runlevel via symlinks). Templates are the definitions; the
  lifecycle hooks are the invocation points.
- **Ordering matters and is explicit.** SysV uses numeric prefixes
  (`S20foo`, `S30bar`). Templates carry an ordered list of init
  scripts.
- **Multiple lifecycle stages.** Not just "boot" — different
  runlevels with different scripts. We have `create`, `fork`,
  `attach`, possibly more.
- **Composition by concatenation.** Multiple templates can apply
  to one context; their init scripts run as one ordered sequence.

What we don't borrow:

- **Stop scripts.** SysV pairs every start with a stop for clean
  teardown. We don't need symmetric init/teardown for context
  lifecycle in v1; if a teardown hook becomes useful, it's an
  additive lifecycle point, not a paired counterpart of every
  init script.
- **Runlevels as a global mode.** SysV runlevels are kernel-wide.
  Our equivalent is per-context — the template *is* the runlevel.
- **`run-parts` over a directory.** SysV scans a directory at
  runtime. Our scripts are referenced by template, not discovered
  from a filesystem location, so the kernel knows up front what
  will run.

## Epics

Suggested chunks of work. Order is rough; some can run in parallel
once the schema is settled.

### E1 — Template type and storage

Define the template schema. Likely a context with `kind=Template`
or equivalent marker, plus metadata for the init script list and
the binding spec. KernelDb shape, migration plan, query patterns
("list all templates," "get template by name," "find templates
referenced by these contexts"). Decide whether templates live
alongside working contexts in the same DAG or in a separate root.

### E2 — Init script execution at lifecycle hooks

Wire `context.create`, `context.fork`, `context.attach` into the
kernel. Run the template-specified init script list in order via
kaish. Compose multiple templates by concatenating script lists.
Surface failures cleanly (a failed init script crashes the
fork — partial init is worse than none, per the no-silent-fallback
principle).

### E3 — Template authoring surface

How a user creates a template. Three candidates, not mutually
exclusive:

1. **`templates_define` MCP tool**, analogous to today's
   `personas_define`. Programmatic creation.
2. **"Mark context as template"** — promote any working context to
   a template via a kernel admin call. Useful for "this conversation
   crystallized something worth reusing."
3. **Template editing as drift.** Because templates are contexts,
   open one, edit blocks directly, save. Multi-user CRDT applies.

### E4 — Fork-from-template ergonomics

`kj fork --template planner` (or whatever the surface settles on),
plus the MCP equivalent. Internally: create context, run init
scripts, position in DAG. Decide how the new context's name /
parent relationship is recorded — fork-from-template should be
distinguishable from fork-from-working-context for provenance
queries.

### E5 — Seed templates from v1 personas

Re-implement `planner` / `coder` / `explorer` as seed templates,
with the same instance lists today's seeds carry plus a system
prompt block per template. Add `sound-engineer` back if/when the
relevant audio instances are reliably available. Verify behavior
matches v1 expectations end-to-end.

### E6 — Migration: deprecate `builtin.personas` v1

Once E1–E5 are stable, remove the DashMap server. Don't pile
features on v1 in the meantime — every deferred persona feature
is duplicated effort against templates. Communicate the
deprecation in `docs/issues.md` and the persona memory.

### E7 — Hook bundles via templates

Today's `HookPhase::ListTools` hook mechanism (D-56) becomes
template-attached. A template can declare hooks alongside its
init scripts. Address the v1 deferred case "filesystem read but
not write" — the persona-level filter that v1 couldn't express
because `builtin.file` is one instance with mixed tools — by
template-level hook bundles.

## Open questions

Things to resolve when this gets scoped, not now:

- **Where do templates live in the DAG?** Same root as working
  contexts (with a marker), or a separate template forest? Likely
  same root — keeps drift natural — but worth discussing.
- **What's a template's identity?** Name (human-friendly) plus
  context UUID (stable)? Versioning?
- **Init script failure semantics.** Halt the fork? Fall through
  with a recorded error? Per-script declared severity?
- **Template inheritance vs. composition.** Can a template
  inherit from another template (single-parent) in addition to
  composition at fork time (multiple-template overlay)? Probably
  composition only; inheritance is a complexity multiplier.
- **Re-running init on attach.** If `context.attach` runs init
  scripts, are they idempotent? Do they no-op when their effect
  already exists, or are some explicitly attach-only?
- **Drift back from working context to source template.** What
  does it mean to "promote" a refinement from a forked working
  context up to the template? A normal drift with explicit
  intent, or a distinct operation?

## Related directions

These are not part of personas-evolution but neighbor it; flagged so
they get considered together when scoping:

- **Memory context forest** — auto-memory drifts into kaijutsu as
  queryable-but-not-conversational contexts. Same "contexts have a
  marker that changes execution semantics" pattern as templates.
  Likely shares lifecycle hook machinery.
- **MCP-centric tools redesign** — the broker / virtual MCP work
  from the now-retired tool-system redesign doc is the substrate
  this rides on. Templates lean on bindings, hooks, and instance
  policy.
- **Streaming block handle** — init scripts that produce blocks
  may want to stream large preambles. Not a blocker but worth
  knowing the primitive will be there.

## Why this is worth doing

Two payoffs.

**For users**: editing a persona becomes editing a context. Same UI,
same multi-user CRDT, same drift semantics. Personas evolve in place
the way every other piece of state in kaijutsu does. No parallel
storage, no parallel editing surface.

**For the kernel**: one fewer concept. Templates and init scripts
collapse a half-dozen deferred persona items into "the substrate
already does this." The DashMap goes away. The auto-injection guard
goes away. The "swap binding under running context" path goes away.
What's left is a small lifecycle mechanism plus a marker on the
context type — two things the kernel will need anyway, used by other
features (memory contexts, scheduled drift, eventually more).
