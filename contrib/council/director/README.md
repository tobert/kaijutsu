# Director council

The director reads `council-amy` and `council-banto` in place of the shared
`council-system` context. Shell and program worked examples and workspace
house rules still join each decision. Coder keeps its shared council.

`amy.md` is Amy's `/home/atobey/AGENTS.md` as loaded on October 6, 2026.
`banto.md` is the shipped director stance as loaded that day. Each opens
with one line telling the council what the text is. These are
starting guidance; chat, edits, and exclusions in Kaijutsu evolve it.
Editing these seed files does not rewrite live contexts.

After deploying and reseeding rc, run the seed from Amy's Kaijutsu shell:

```sh
cd ~/src/kaijutsu/contrib/council && source seed-director.kai
```

It creates the two chat contexts, assigns a council performer reviewed by
Amy, and loads these files as user guidance. `source` leaves `$0` empty, so
the files resolve from the working directory, and the script stops with an
error anywhere else. A context that already exists is skipped, never
reseeded, so its chat is retained; edit it in Kaijutsu instead. Running a
script by its path in a context shell does nothing, so do not run it that way.

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
