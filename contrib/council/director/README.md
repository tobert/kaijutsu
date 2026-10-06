# Director council

The director reads `council-amy` and `council-banto` in place of the shared
`council-system` context. Shell and program worked examples and workspace
house rules still join each decision. Coder keeps its shared council.

`amy.md` is Amy's `/home/atobey/AGENTS.md` as loaded on October 6, 2026.
`banto.md` is the shipped director stance as loaded that day. These are
starting guidance; chat, edits, and exclusions in Kaijutsu evolve it.
Editing these seed files does not rewrite live contexts.

After deploying and reseeding rc, run `../seed-director.kai` once from Amy's
Kaijutsu shell. It creates the two chat contexts, assigns a council performer
reviewed by Amy, and loads these files as user guidance. Existing contexts
must be inspected and edited instead of reseeded, so their chat is retained.

Add this to the director's existing council section in `gate.toml`:

```toml
[context_type.director.council]
enabled = true
contexts = ["council-amy", "council-banto"]
```

The `council` context type owns the acknowledgment prompt and input loadout
under `assets/defaults/rc/council/create/`. Its regular model thinks about
submitted guidance and acknowledges it; the council server judges actions.
This context list does not enable automatic reviewer-chain voices.
