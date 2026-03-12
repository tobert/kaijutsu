# kj preset — preset templates

Presets save context configurations (model, tools, system prompt) for reuse.

```bash
kj preset list
kj preset show code-review
kj preset save code-review --model anthropic:claude-sonnet-4-5-20250929 --tools deny:shell --desc "Code review preset"
kj preset remove old-preset   # latched
kj fork --name review --preset code-review
```
