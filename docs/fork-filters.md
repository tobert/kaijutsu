# Fork filters — interval selection, presets, seams

Status: design locked 2026-06-12 (Amy + Fable). **IMPLEMENTED 2026-06-12
(slices 1–4):** splice module, interval algebra (`kaijutsu-crdt::selection`),
`preset_args` + factory presets, `parse_range`, recall+compose at fork
(`resolve_fork_selection`), hydration-policy travel, and the CLI surface
(`--include`/`--exclude` ranges; `--shallow`/`--depth`/`max_blocks` retired →
`--include end-N:`). Coding plan/log in `tsugi.md` (ephemeral); backlog entries
in `docs/issues.md`; player-side consequences in `docs/chameleon.md`. The
windowed-notation **pull primitive** ($HEARD / fork-carry / marker-archive,
"slice 5") and tick-endpoint grammar remain deferred — they belong to the
Chameleon thread, not this core.

## Mental model

Fork strategy is KV-cache strategy (locked earlier on 2026-06-12 — see the
"Fork primitives" entry in issues.md): copy cost is a non-issue, storage is
cheap. Full fork = *power* (whole context → fresh lineage = new KV cache).
Thin fork = *reuse/reduce*.

The unifying observation (Amy's filter framing): **every fork shape is an
interval filter over the ordered block log** —

| shape | as a filter |
|---|---|
| full | all-pass |
| full + `--exclude <block>` | notch at one block |
| hydration keep-set `[0,P] ∪ tail` | notch over `(P, tail_start)` |
| last-N (retired `--shallow`) | hipass at `end-N` |
| player spawn (~nothing) | block everything |
| "extract this section" | bandpass |

The hydration window IS a notch filter. So there is **one selection
primitive**, and the named shapes are presets over it. The earlier
rc-rebuilds-vs-prefix-preserve tension dissolves: those are different
*intents*, not competing implementations — `spawn` (fresh bytes are the
feature: that's the producer's horizon-latch edit channel) vs `window`
(byte-stable prefix for KV reuse on API chairs), chosen per fork.

## The primitive

A fork-time **interval selection** over the order-key-sorted, non-deleted
snapshot of the source block log:

```
kept = (base ∩ ∪includes) \ ∪excludes
```

- `base` = the preset's keep-set (default `full` = everything).
- No `--include` given → includes = everything (subtract-only).
- **Order-free set algebra** — no rsync/gitignore-style first/last-match
  rules; stacking repeatable flags cannot change meaning by position.
- Resolved **at fork instant** — positions are one-shot addresses, never
  stored, which is what makes positional addressing safe in a multi-writer
  CRDT log.
- Implementation home: `ForkBlockFilter` (kaijutsu-crdt) grows interval
  support; `select_hydration_window` (kernel mailbox) becomes a consumer of
  the same algebra — its keep-set IS the `window` preset's base, one
  definition, single source.
- The same engine serves: fork selection, per-turn hydration windowing, the
  windowed-notation pull primitive (`$HEARD` / fork-carry / marker-archive),
  and filtered block reads. Filters are a kj/kernel facility — deliberately
  **not** promoted into kaish syntax ("just shy of useful enough"; the shell
  carries strings, kj parses them).

### The include invariant (loud, no silent winner)

**Every explicitly `--include`d block is in the final keep-set, or the fork
refuses.** After resolving all layers, validate `∪cli_includes ⊆ kept`;
violation is a validation error naming the culprit:

```
kj fork: --include 10:20 conflicts with preset 'window'
  (12:16 falls in its archived notch).
  Drop the preset, adjust the range, or exclude explicitly.
```

Uniform consequences:

- Self-contradiction on one command line (`--include 10:20 --exclude 15:18`)
  is an error, not a silent excludes-win.
- A preset can never silently eat part of an explicit include — the
  `base ∩ inc` trim is checked, not just exclude overlap (the preset's
  *shape* is an exclusion in effect).
- Excludes **union across layers** (preset rows + CLI flags): both are
  subtractions, and refining a patch shouldn't un-subtract what the patch
  subtracts — but only where they don't contradict an explicit include.

Deliberately unsupported in v1: **resurrect** intent ("the preset's
exclusion is right in general, but bring *these* back"). In mixer terms,
the overload we refused: include-as-narrowing is **solo**; resurrect is
**unmute** — different operations. If the invariant error gets hit in
practice with unmute intent behind it, the affordance is a separate
operator (`--pin`) with declared precedence over lower-layer excludes.
Start strict, loosen on evidence — we can't always save users from
themselves, and some of them make for good jazz.

## Range grammar (v1)

```
range    := [endpoint] ':' [endpoint]      half-open [lo, hi)
endpoint := <int> | end | end-<int>
```

`0:5` first five · `:5` ≡ `0:5` · `5:` ≡ `5:end` · `:` everything ·
`end-5:` last five.

Decisions, with why:

- **Colon, not `..`** — verified against the kaish lexer 2026-06-12: every
  colon form lexes clean as a bareword (`0:end`, `end-5:end`, `:5`, `5:`,
  even block-key-shaped `a:b:42`), while `0..5` is a hard lexer error
  (`0.` starts a float) and `@` is an unexpected character. Rust-style
  ranges are hostile to the shell that has to carry them.
- **Half-open everywhere** (Go/Rust agreement); no inclusive variant.
- **`end`, not `last`** — `end` ≡ `len(log)` (one-past-the-end), so
  `end-5:` is Go's `a[len(a)-5:]` with the arithmetic baked into a keyword.
  `last` (= the final *element*) would force `last-4:last`-inclusive
  off-by-ones.
- **No negative indices in v1** (Go purity; also `-5:` leads with a hyphen
  and fights clap). `-5:` may arrive later as pure sugar for `end-5:`.
- Positions resolve against the fork-instant ordered snapshot.

Reserved endpoint forms (not v1):

- **Ticks** (`t8:t16` lexes fine) — musical ranges; wanted by the
  windowed-notation pull primitive; decide the spelling there.
- **Labels** — colon-free savepoint names as endpoints (`0:bridge`). This
  is where the speculative snapshot/savepoint verb re-enters as *just an
  endpoint form* — a label on a block, no new fork machinery.
- **Raw block keys stay OUT of inline ranges** — `ctx:agent:seq` contains
  colons; `key:key` ranges are a colon-counting nightmare. Exact blocks
  keep the dedicated `--exclude <key>` flag (shipped c51544d).

## Presets = patch recall

A preset is a **named ensemble of argument values, not a behavior** — the
audio-world concept: hitting "e-piano" reconfigures oscillators, FX
routing, and velocity curves in one gesture, and the synth stays the same
machine. `kj fork --preset foo` recalls the patch; the selection algebra
and the inheritance manifest never change.

This **extends the existing preset concept** (provider / model /
system_prompt / consent_mode) rather than adding a parallel one: today's
presets are patches that only move the model knobs; `window` is a patch
that only moves filter knobs; a future `player` patch moves both
(spawn-shaped filter + the local bass model) in one recall.

- **Factory presets** ship embedded, names reserved (same pattern as
  `assets/defaults/rc` + reseed):
  - `full` — all-pass; the default when no `--preset` is given.
  - `window` — the hydration keep-set `[0,P] ∪ tail`, read from the
    parent's `context_hydration` row at recall. **No policy row on the
    parent = loud error** — "no notch is defined here" is a configuration
    mistake, not a degenerate case.
  - `spawn` — ~nothing; the player-spawn shape, rc rebuilds setup.
- **Recall, then tweak**: explicit flags layer on top. Scalars (model,
  name, pwd) **override** the preset's value; the filter expression
  **composes** through the set algebra (preset supplies the base, CLI
  include/exclude refine it), guarded by the include invariant.
- **Recall is a snapshot**: the patch is read at invocation; later edits
  don't reach already-forked contexts. Same invariant as
  script-snapshot-at-instantiation — for players this is the horizon-latch
  channel again: the producer edits the `player` patch, it lands at the
  next page-turn.
- **Storage stays normalized** (no JSON blob): extend the preset table
  with a `preset_args`-style child table
  `(preset_id, verb, arg_name, arg_value)`, repeatable args as multiple
  rows. **Verb-scoped from day one** so the concept generalizes without a
  migration — but this design only wires fork; "presets deeply,
  everywhere" is its own design thread (issues.md).

## Inheritance manifest

Full fork = "inherit almost everything + CoW" (Amy). What travels:

| what | policy |
|---|---|
| blocks | per selection; deep copy = *semantic* CoW (child diverges freely, parent untouched; storage-CoW is an optimization we don't need — storage-is-cheap is locked) |
| env, shell/cwd, bindings, workspace | copied (existing; one transaction) |
| context_type | inherited (existing post-commit fixup) |
| model | inherited unless `--model` (existing) |
| **hydration policy row** | **travels whenever the marked block survives the selection** — `full`: always; `window`: by construction; `spawn`: never (child rc re-marks); ad-hoc ranges: iff the marker survived, else the row is omitted and the fork-marker block says so visibly |
| transport arm state | **never** — arming is always explicit |

The marker remap is mechanical: fork preserves `(principal, seq)` and
rewrites only the context part (`block_store.rs` `fork_filtered`), so
`P_child = BlockId::new(child_ctx, P.principal_id, P.seq)`. This resolves
the "fork drops the policy" backlog entry by construction.

The ad-hoc-drop posture is deliberately softer than the hydrate-side
fail-loud: a fork that intentionally cuts the prefix shouldn't be forced
to carry a marker pointing at nothing, and a fresh fork has an rc
lifecycle downstream that re-marks — whereas a live context losing its
guard has nothing to catch it. Visible-at-the-cut, not silent, not fatal.

## The seam module (shared, first-class)

One module — built well, heavily tested, everything routes through it —
owning the cut edges of any keep-set:

- **Turn-boundary snapping** — a kept interval must never start on a
  `ToolResult` or a mid-`Model` continuation; snap edges back to the
  nearest preceding turn boundary (`User/Text`). Covers the hydration
  tail, fork selections, and the create-time marker.
- **Synthetic seam blocks** — where kept intervals are non-adjacent,
  inject a user-role "[N blocks archived]" seam so cross-gap `Model/Text`
  fragments can't merge into false continuity (the seam lands after the
  prefix, so cache bytes stay stable).
- **Tool-pair integrity** — must-travel-together groups (tool_use +
  tool_result; the mailbox atomicity gate's concern) respected at every
  cut.

Consumers: hydrate (`rehydrate_windowed`), fork selection, the pull
primitive. This absorbs the two review P1s (tool-pair tail snap, archive
seam) — they were "latent until composer gets tools" for hydration, but
**hand-cut ranges make them reachable immediately**, which is why the
seam module is *first* in the build order.

## Retired / superseded

- `--shallow` / `--depth N` — retired; spelled `--include end-N:` (no
  users besides us, no migrations). `--compact` (distill-seed) and `--as`
  (subtree template) are orthogonal and untouched.
- The `--keep none|window|last:N` proposal from earlier in the design
  session — superseded by presets + ranges.
- The `KJ_PARENT_HYDRATION_MARKER` read surface — already dissolved by
  the player-spawn decision; doubly dead now that the policy row travels
  by remap.

## Deferred

- Tick + label endpoints (spelling decided with the pull primitive).
- `--pin` (unmute) — only if the include-invariant error gets hit with
  resurrect intent behind it.
- `-5:` sugar for `end-5:`.
- Presets-everywhere (issues.md design thread).
