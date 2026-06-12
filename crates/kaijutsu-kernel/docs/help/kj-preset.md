# kj preset — preset templates

Presets save context configurations (model, system prompt, consent mode)
for reuse.

```bash
kj preset list
kj preset show code-review
kj preset save code-review --model anthropic/claude-sonnet-4-6 \
    --system-prompt "You are reviewing a PR…" --consent collaborative \
    --desc "Code review preset"
kj preset remove old-preset   # latched
kj fork --name review --preset code-review
```

Save flags: `--model`/`-m` (`provider/model` or bare model),
`--system-prompt`, `--consent` (collaborative | autonomous), `--desc`.

## Coming (designed, not yet built — `docs/fork-filters.md`)

Presets widen into **patch recall**: a named ensemble of argument values
(audio-synth sense — one recall moves every knob the patch cares about),
extended to carry fork filters alongside model/prompt settings. Factory
presets `full` / `window` / `spawn` ship with reserved names. Recall is a
snapshot: editing a preset later doesn't reach contexts already forked
from it.
