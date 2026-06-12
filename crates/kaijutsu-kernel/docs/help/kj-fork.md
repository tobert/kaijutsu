# kj fork — fork the current context

Creates a new context from the current one. The caller stays on the parent
(POSIX `fork()` semantics); the child's id is returned in `.data` so scripts
can pick it up. `--switch` opts into moving your session to the child.

Fork strategy is KV-cache strategy — copy cost is a non-issue (storage is
cheap). See `docs/fork-filters.md` for the locked design this surface is
converging on.

## Fork Kinds

- **full** (default) — Deep copy of all blocks into a fresh lineage = a new
  KV cache. The *power* path: resume-as-another-model, orchestrator repair,
  drift-a-summary-back. `--exclude` drops named blocks from the copy.
- **--shallow** — Only recent blocks (`--depth`, default 50). **Slated for
  retirement** (`docs/fork-filters.md`): last-N is a weak primitive; it
  becomes `--include end-N:` when range filters land. Don't grow scripts on
  it.
- **--compact** — LLM-summarized. Starts with a distilled summary instead of
  full history.
- **--as <template>** — Subtree fork: copies tree shape from a template
  context.

## Options

- `--name`, `-n` — Label for the forked context
- `--model`, `-m` — Override model (format: `provider/model`). Inherits from
  parent if omitted.
- `--distill-model` — Override model used for `--compact` summarization.
  Cheap-model knob: use Haiku to summarize for an Opus follow-up. Inherits
  from parent if omitted.
- `--prompt` — Inject a note into the fork as a drift block AND drive the
  child's first autonomous turn
- `--exclude <block>` — Drop a named block from a FULL fork (repeatable;
  `context:agent:seq` key form). Fail-loud if the block isn't in the source.
  The orchestrator-repair path: "fork X without the block that blew it up."
- `--preset` — Apply a preset's settings after forking
- `--pwd` — Override the working directory on the fork
- `--depth` — Block limit for `--shallow` (default: 50)
- `--switch` — Move your session to the child after forking (default: stay
  on the parent)
- `--stage` (alias `--staging`) — Start the child in liminal staging state
  (awaits human curation; `--prompt` won't auto-drive a staged child)

## Examples

```bash
# Basic fork with a label — you stay on the parent; child id is in .data
kj fork --name debug-auth

# Fork and move into the child
kj fork --name debug-auth --switch

# Fork with a different model
kj fork --name fast-check --model anthropic/claude-haiku-4-5-20251001

# Fork with a seed prompt — child starts working on its own
kj fork --name explore --prompt "investigate the auth timeout"

# Repair fork: everything except the poisoned block
kj fork --name repaired --exclude 0198a…:0199b…:42

# Fork from a preset
kj fork --name review --preset code-review

# When done, merge findings back
kj drift merge
```

## Coming (designed, not yet built — `docs/fork-filters.md`)

`--include`/`--exclude` range filters (`0:5`, `end-5:`, half-open, `end`
keyword) and factory presets `full` / `window` / `spawn` recalled via the
existing `--preset` flag.
