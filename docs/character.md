# Character — the persistent someone a name resolves to

Design and current behavior. Built: the sheet, the keyring, attribution, the handoff log, and the type melt. `AGENTS.md` links here so documentation remains accurate.

## Current implementation

| Part | Contract |
|---|---|
| Identity and sheet | `PrincipalId`, kernel-owned name, creation/retirement timestamps, optional `handoff_ctx`, a `root` flag, `root_ctx`, `rc_dir` (directory under `/config/rc/`), `accepts_pings` (default `true`), and root pointers `console_ctx` and `mcp_hub_ctx`. `kj character create [--root] [--rc <dir>]|list|show|set --root|--no-root|--rc <dir>|--console <ctx>|--no-console|--pings|--no-pings|retire`. |
| Bootstrap | `kaijutsu-server bootstrap` runs `init`, creates missing shipped characters, and binds console models. `kaijutsu-server init --as <name> --key <pubkey-file>` creates the first root character. |
| Root context | Each live root character has one `root_ctx`: labeled with the character's name, played by it, with no parent and no senpai (root console). |
| Person | A person is a root character. Contexts performed by a person run no model turns, draft as shell, and skip self-review checks. |
| Console | Context `console_ctx` names on a person's sheet. Performed by a non-root character, answering to the person's root console. |
| Wire facts | `ContextHandleInfo` carries `performer`, `performedByPerson`, `isRootConsole`, `isConsole`, and `isMcpSession`, computed in kernel. |
| Credentials | `auth.db` binds fingerprints to principals; `add-key --as <character>` binds to an existing character. |
| Context | Carries `performer`, `senpai_ctx`, `persona`, and copied `context_slots`. Created via `kj character create <character>`. |
| Gate layers | `gate.toml` layers: `[global]`, `[rc.<dir>]`, `[root]`, and `[character.<name>]`. Narrowest decides. |
| Senpai edge | Every context answers to one senpai context (`contexts.senpai_ctx`), except a root console. |
| Model | One model (`character_models`) plus `distill`, `gate`, `image`, `audio`, `video`, `speech` slots (`character_slots`). Copied to `context_slots` at creation. |
| Approval | Asks snapshot performer and reviewer. Reviewer alone decides; performer cannot approve except self-confirmation. |
| Standing rules | `kj ledger allow <id> --remember character [--family]` binds a rule to the performer. |
| Retirement | `kj character retire <name>` concludes and archives live contexts performed by the character in one transaction. |
| Handoff | Ordinary context referenced by `handoff_ctx`, created on first note. |
| Rc | Runs `/config/rc/<rc_dir>/<verb>/`. Scripts read `KJ_CHARACTER` and `KJ_RC_DIR`. |
| Rotation | `kj context rotate [<context>]` creates a successor with the predecessor's character, senpai, slots, env, and cwd. |

## The type melts into the character

A context has no type; what it does comes from its character: the rc directory shapes its lifecycle, gate layers govern its statements, and model slots answer its turns.

- **A context**: `performer`, `senpai_ctx`, `persona`, `forked_from`, label, and copied `context_slots`.
- **A character's sheet**: name, `root` flag, `rc_dir`, model, slots (`distill`, `gate`, `image`, `audio`, `video`, `speech`), and pointers (`root_ctx`, `console_ctx`, `mcp_hub_ctx`, `handoff_ctx`).
- **rc directory**: `/config/rc/<rc_dir>/`. Defaults to character name, or `person` for roots.
- **The judge**: character `judge` whose model is a council server. Judge shadows are recorded in `context_shadows` (kind `judge`).

## Chomosuke

Chomosuke keeps a rolling summary on its own `memory` board. Each revision
reads the previous post and recent source blocks. The next reader can follow
the block references back to the conversation or score.

Create a context for the configured character with
`kj context create chomosuke --name rehearsal-reader --env KJ_SUMMARY_CONTEXT=rehearsal`
to summarize that conversation. Each tick samples its latest 12 included, completed
user and model text blocks. The memory topic is
`chomosuke/memory/context-<source-context-id>`. Omit `KJ_SUMMARY_CONTEXT`
to observe played ABC and the latest directions in each running track's
`handoff` topic. Chomosuke publishes its observation to `chomosuke/memory/vibe`
when the turn completes. A new handoff can prompt a revision on the next
observer tick even when the score is unchanged.

For musicians configured to produce 16 bars of 4/4 per turn, wake each
musician every 64 beats and Chomosuke every 128. The musician's tick script
sets the requested phrase length. On the default eight-bar phrases, use 32
and 64 beats respectively. With the musician and observer contexts ready,
repeat the bass commands for drums and chords.

```sh
kj transport attach --context 'bass' --track 'band' --wakeup 64
kj transport attach --context 'rehearsal-reader' --track 'band' --wakeup 128
kj context set 'bass' --env 'KJ_VIBE_TOPIC=chomosuke/memory/vibe'
kj board post '@track/band' --title 'handoff' 'Leave beat four of the last chord bar empty.'
```

Use `kj board reply '@track/band/handoff' '<direction>'` for later cues.
Each musician that opted in reads up to 600 characters of the latest vibe
when it starts a phrase. A turn already in progress keeps the observation it read.
Read the target tick in that musician's prompt, then check the ABC in
`score-band` and the outcome in `kj transport work --track 'band'`.

To give the score observer audio, bind the character's model and `audio` slot
to the same audio-capable model before creating the listening context.
For example, on a Nagaya backend serving LFM3-Omni:

```sh
kj character model set 'chomosuke' 'nagaya/lfm3-omni' --max-tokens 160 --effort none
kj character slot set 'chomosuke' audio 'nagaya/lfm3-omni' --max-tokens 160 --effort none
kj context create 'chomosuke' --name 'listening-reader' --env 'KJ_LISTEN=band'
kj transport attach --context 'listening-reader' --track 'band' --wakeup 256
```

With a General MIDI SoundFont at
`$XDG_DATA_HOME/kaijutsu/models/soundfont/default.sf2`, each fresh score
observation can include up to eight seconds from the start of the mix of
recent committed phrases. At 130 BPM, 256 beats gives the observer about
two minutes between listens while the players wake every 64 beats. The
listener replaces its previous clip. The
track handoff and played ABC still supply the written directions and score.

Chomosuke’s create rc composes its stance with the instructions for the
source. A conversation reader tracks decisions and open questions. A score
reader describes one attack, rest, or phrase length in the score or rendered
clip. It receives parts from the latest played tick on each track so it can
compare chairs that started together. A change to the source files takes effect when create runs. To
change an existing reader, edit its stored system block.

### Toolie at the sound desk

Give the listener and the engineer the same track board topic. Anyone with
write access to the topic can ask the engineer for a measurement or a change.
A musician can keep playing while the request waits.

```sh
kj board post '@track/band' --title 'sound' 'Sound desk for the band.'
kj context set 'listening-reader' --env 'KJ_ENGINEER_TOPIC=@track/band/sound'
kj context create 'toolie' --name 'sound-engineer' --env 'KJ_ENGINEER_TOPIC=@track/band/sound'
kj transport attach --context 'sound-engineer' --track 'band' --wakeup 32 --rotate 8
kj board reply '@track/band/sound' 'Engineer request: Check whether the three chairs launched together.'
```

The example assumes a Toolie character with the `toolie` rc directory and
a tool-calling model. For plain LFM3 on Nagaya, set the character model
with `kj character model set toolie nagaya/lfm3 --max-tokens 256 --effort none`
before creating or rotating Toolie. On this backend `--effort none` sends
`thinking: false`. Register `contrib/rehearsal_desk.py` with Python as
the stdio server `rehearsal_desk` in the rehearsal kernel’s `mcp.toml`,
then run `kj mcp reload`. An `Engineer request:` gives Toolie the status
tool. An `Engineer change:` also gives it tools to set a chair’s MIDI
control and restore its baseline for that turn. To give the desk a control,
run the script locally with `--establish chords pan 64`, for example. This
sends the baseline to the synth. Use the same state path and destination for
the local command and the server. MIDI controller values sent by a score or
another operator can supersede a desk setting.

Chomosuke writes one observation in plain text. Its rc posts the note to
the musicians’ vibe and forwards it to the configured desk as an inspection
request. Toolie decides whether a measurement would help. The listener’s
prompt contains the score, optional clip, and any new track handoff. The
desk keeps its own timing receipts and mixer controls.

To have the listener post directly, create it with `KJ_POST_NOTES=band`
and attach it to that track. Its only model tool is `add_note`. The score
excerpt shows the opening of each part, up to two bars. The listener posts
a short take and copies an explicit `@Toolie` address into the note. A successful
call writes to `chomosuke/music-notes/band`. Its rc mirrors the posted body
to the musicians’ vibe when the turn ends with a final reply. Configure
Toolie with `KJ_OBSERVER_NOTES=chomosuke/music-notes/band` at creation.
On its next wake, Toolie reads new notes. An opening `@Toolie` in the body
sends a note to the sound desk with its source block ID. An ordinary note
requires no engineer turn. Toolie can read board topics with `notes`.

Toolie wakes every 32 beats
and rotates every eight track phrases in this example. Its claim and result
refer to the original post. The result carries the desk tool’s recorded
output and block references. A turn without a desk call reports that it
recorded no measurement or change. A successor reports an interrupted claim and
leaves the decision to retry with the requester.

## What a character is

A character is one continuous lineage of interactions made of contexts, models, and rc directories. The sheet holds them together as pointers.

- The sheet is pointers, not content. Prose, policy, and lists belong in contexts or directories the sheet points at.
- The lineage is the point, not the row. A character outlives any single context; contexts are performances of it.

## Senpai and kohai

Every context answers to one senpai context (`contexts.senpai_ctx`), except a root console (`characters.root_ctx`).

- **Setting and moving the senpai edge**: Set at context creation or fork. Moved with `kj context set <ctx> --senpai <target>`. The kernel refuses cycles and archived targets.
- **What the senpai edge governs**:
  - **Review routing**: Asks climb `senpai_ctx` to the first live character reviewer.
  - **Drift permissions**: `kj drift push .senpai` is allowed in `gate.toml`.
  - **Authority**: The context's performer or any character along its senpai chain up to the lineage root may move, conclude, promote/demote, or archive the context.
- **Model slots**: `model`, `distill`, `gate`, `image`, `audio`, `video`, and `speech` live on the character (`character_models`, `character_slots`) and are copied into `context_slots` at context creation. There are no fallbacks and no mid-context recasts.

### The bridge identity: a key per model character
`credentials` is keyed by fingerprint (`auth_db.rs:44`), so the string selecting a key from the agent finds the principal. `kaijutsu-mcp` gains `--key-fingerprint` and `--key-file` to connect as a named model character rather than defaulting to `amy`.

```sh
ssh-keygen -t ed25519 -N "" -C "kaijutsu-lead" -f ~/.ssh/kaijutsu-lead
kj character create kaijutsu-lead --rc mcp
kaijutsu-server add-key ~/.ssh/kaijutsu-lead.pub --as kaijutsu-lead
ssh-add ~/.ssh/kaijutsu-lead
```

### The handoff is an ordinary context
Each character gets a handoff log: a context with no performer, pointed at by `handoff_ctx`, created by the first `kj handoff note`.
- `kj handoff note "…"` appends one block to the handoff context.
- `kj handoff tail [--window N] [<character>]` reads the last N notes.

### Bootstrap: the person creates themself
`kaijutsu-server init --as <name> --key <pubkey-file>` creates a root character with a minted id. Roots are equal: 1 is typical, more than one possible. Adding keys stays host-only, with no anonymous auth.
