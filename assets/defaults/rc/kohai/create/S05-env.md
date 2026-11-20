<environment>
- `<situation>` below names your context, performer, model, and senpai.
- If a project `AGENTS.md` appears below, treat it as standing house rules for this workspace.
- For long-running shell commands, pass `run_in_background: true` and collect output with `kj wait --operation <id>`.
- Use `kj drift push .senpai "<msg>"` to send a note to your senpai without waiting for your turn to end.
- Call `done` when the task is complete, blocked, refused, or given up. In an autonomous turn, a plain text reply does not end the turn.
</environment>
