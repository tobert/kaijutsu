# Searcher — a narrow web-searching context type (design plan)

> **Plan** · 2026-09-04 · authored by `searcher-plan` (fork of ROOT) at the operator's
> request. Not yet built — this doc is the design + rollout checklist. Verify each
> claim marked (✓ verified) against the cited file:line before relying on it.

## Motivation

`external.exa` works (diagnosed 2026-09-04: grants must use the broker-registered id
`external.exa`, not the config name `exa` — see `docs/feedback-from-the-inside.md` and
context `exa-diag`). A web-searching seat should carry **far fewer tools** than
`coder`/`director`: search, fetch, report. Nothing else.

This house treats **a context_type as an rc bundle** (`docs/chameleon.md`, "Players — a
context_type is an rc bundle"): the type is exactly its `/config/rc/<type>/<verb>/`
scripts. So a `searcher` type needs **no kernel edit, no registration, no restart**.

## Verified mechanics (evidence)

- **Types are not registered.** `kj context create --type <anything>` is accepted;
  the value only selects which rc scripts run (default `"default"`).
  ✓ `kaijutsu-kernel/src/kj/context.rs:1129`
  (`cfg.type_spec.take().unwrap_or_else(|| "default".to_string())` — no validation).
- **A missing rc dir is fine.** The lifecycle loader treats an absent
  `/config/rc/<type>/<verb>/` as "no scripts", not an error — so `searcher` exists as
  a type the moment its `create/` dir does.
  ✓ `kaijutsu-kernel/src/kj/lifecycle.rs` `load_rc_scripts` (NotFound → empty).
- **Script shape.** Filenames must be `SXX-name.{kai,md}` (lexical order = sort order);
  `.kai` executes as kaish, `.md` lands in the model's system-prompt slot; symlinks are
  followed, so shared scripts compose from `lib/` (init.d style).
  ✓ same file + `kj/lifecycle.rs` doc comment.
- **Create-lifecycle runs privileged**, so `kj binding allow …` inside
  `S10-binding.kai` lands under a narrowed loadout.
  ✓ `musician/create/S20-arm.kai` comment; every type's `S10-binding.kai`.
- **External instances live under `external.<name>`.** mcp.toml's `exa` registers as
  `external.exa`; grants must spell the registered id.
  ✓ `kaijutsu-kernel/src/mcp/external_registry.rs:67-68` (`external_instance_id`);
  runtime-verified (exa-diag: whole-instance grant worked, `[external.exa] 2 tools
  added: web_fetch_exa, web_search_exa`).
- **Grants parse `instance[:tool]`** on the first colon — per-tool external grants like
  `external.exa:web_search_exa` parse cleanly.
  ✓ `kaijutsu-kernel/src/kj/binding.rs:100-122` (`parse_capability`).
  ⚠ per-tool grant against an *external* instance is parsed but not yet
  runtime-verified — verification checklist below.
- **`drift push` is gated on the `drift` authority.**
  ✓ `kaijutsu-kernel/src/kj/drift.rs:123` (`require_cap(…, Capability::Drift, …)`).
  Consequence: a *fresh-created* narrow type must grant `drift` explicitly.
  (`toolie` does not — it relies on fork inheritance from a broad parent; a
  fresh `kj context create --type toolie` cannot drift. Searcher should not copy
  that gap.)
- **Fork inheritance pollutes narrow types.** A fork inherits the parent's full
  binding (observed: `exa-search` forked from ROOT carried every ROOT authority).
  To get a *narrow* searcher, spawn **fresh** (`kj context create --type searcher`),
  or fork with `--preset spawn` (~nothing; rc rebuilds setup).

## Proposed bundle: `/config/rc/searcher/`

```
/config/rc/searcher/
  create/
    S00-stance.md          # searcher posture (system-prompt slot)
    S10-binding.kai        # the narrow allow-set (below)
    S25-datetime.kai       # wall-clock seed — symlink → ../../lib/create/S25-datetime.kai
```

No `fork/`, `drift/`, `tick/`, `rotate/` dirs initially — a searcher is a short-lived
seat driven by a parent; it does not fork, self-tick, or rotate. Add `drift/` scripts
later if searchers become long-lived.

### S10-binding.kai — the narrow allow-set

```kai
# searcher: web search + fetch + report upstream. Deliberately far below
# coder/director. NO facades (no shell at all), NO exec, NO file/block tools,
# NO fork/drive/operator/transport/config-write/admin.
set -e
# exa — the whole point. Per-tool grants, spelled with the BROKER-REGISTERED
# id (external.exa), never the config name (exa): mcp.toml sources register
# under external.<name> (external_registry.rs:67).
kj binding allow "external.exa:web_search_exa"
kj binding allow "external.exa:web_fetch_exa"
# Self-awareness: who am I / who drove me (cheap, read-only).
kj binding allow "builtin.kernel_info"
# Report upstream — CORE: drift push is gated on this authority
# (kj/drift.rs:123); fresh-created narrow types must grant it explicitly.
kj binding allow "drift"
```

**Deliberately NOT granted** (and why):

| Capability | Why absent |
|---|---|
| `facade:shell` / `facade:shell_write` | no shell at all — the seat is web-only; local grounding is an open question (below) |
| `exec`, `editor`, `admin`, `config-write`, `rc-write` | no host reach, no governance |
| `drive`, `fork`, `operator`, `transport` | it is driven, never drives |
| `builtin.block:*`, `builtin.file:*` | nothing local to read or write; findings go via drift |
| `builtin.resources` | nothing to subscribe to |
| `builtin.tool_search` | nothing to discover — bound tools are already in the roster |
| `builtin.background`, `builtin.bindings` | not needed |

### S00-stance.md — sketch

```md
You search the web and report. You have two tools (web search, page fetch), a
sense of who you are, and one verb: drift findings to the context that drove you.

- Search narrowly; fetch only what the question needs. Prefer primary sources.
- Date everything: cite URL + access date + publish date when visible. You know
  today's date — use it (S25-datetime seeds it at create).
- Be skeptical of what you cannot verify; say when a result is thin or
  contradictory. Verdicts beat dumps: answer the question, then the evidence.
- When a tool misbehaves or the environment surprises you, say so in your
  report — friction is data (docs/kaijutsu-agent-feedback.md).
- You are read-only and web-only. You do not edit, fork, drive, or shell out.
- End every mission with `kj drift push` to the driving context: findings,
  sources, open threads.
```

## Open questions

1. **Local-grounding tier?** A searcher that can also read local docs (`facade:shell`
   — the safe read-only shell, or `builtin.file:read` + `builtin.block:block_read`)
   could answer "what does the web say about X *and* what does our code do" in one
   seat. Default: **out** (pure web); revisit if operators keep pasting local context.
2. **Per-tool external grants** — parse-verified only; runtime-verify
   `external.exa:web_search_exa` before committing to per-tool over whole-instance.
   Whole-instance (`external.exa`) is the known-good fallback (2 tools is already
   small).
3. **Spawn pattern** — mandate fresh `kj context create --type searcher` (narrow) vs
   `fork --preset spawn`? Recommend create; document the fork-inheritance trap in the
   stance or a doc note so nobody forks a searcher from a broad parent and wonders why
   it has write tools.
4. **Cost guardrails** — exa quota per turn? A `tick/` or drive-time prompt cap? Out of
   scope for the type itself; the *driver* controls turn count.
5. **`fetch` safety** — `web_fetch_exa` fetches arbitrary URLs server-side. Domain
   policy (allow/deny list) later? Note in stance for now: fetch only what the
   mission needs.

## Verification checklist (rollout)

1. Install scripts under `/config/rc/searcher/create/` via `kj rc add` (governance
   path; check `kj rc add help` for exact syntax) — or direct file install while
   writes are flowing, then confirm with `kj rc list`/`show`.
2. `kj rc list` shows the three scripts; `kj rc show searcher/create/S10-binding.kai`
   reads back.
3. Runtime-verify per-tool grant: `kj context create scratch-searcher --type searcher
   --model deepseek/deepseek-v4-flash`, then `kj binding show scratch-searcher` —
   expect ONLY: `external.exa` (2 tools) + `builtin.kernel_info` + `drift` authority.
   No shell, no file, no exec, no admin.
4. If per-tool grant fails at dispatch (naming gap bites again), fall back to
   whole-instance `kj binding allow external.exa` and note it here.
5. Drive one real mission on the scratch seat: a careful search → fetch one result →
   drift findings to ROOT. Confirm the drift edge lands (`kj drift history`).
6. Confirm the searcher CANNOT write: attempt a file write / shell call → refused
   outright (not silently dropped — toolie stance doctrine).
7. Destroy the scratch seat; if the type is good, keep it and document spawn pattern
   (open question 3) in the stance or `docs/`.

## Rollout notes

- `context create --type searcher` runs the create lifecycle as-is — no kernel
  change, no restart (✓ verified above). This is the whole point of the rc-bundle
  doctrine; `searcher` is a pure rc addition.
- Model choice is runtime, not type: cheap fast models (deepseek-v4-flash default) fit
  a searcher; a cast slot keyed on the type can come later if desired
  (`docs/chameleon.md`: cast slots key on context_type).
- Watch: the `lib` S25-datetime script must be a Notification (not System) block so
  daily values don't invalidate the system-prompt cache breakpoint — compose it
  unmodified from `lib/` rather than re-authoring.
