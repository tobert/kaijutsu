# 会術 Kaijutsu

Kaijutsu is a cybernetic system for people and models working together across
contexts. It is an instrument you play: the kernel holds durable state, model
interactions, workspaces, and tools; players choose the work. It speaks SSH
with Cap'n Proto over channels. See `docs/instrument-design.md`.

## Working together

Work as peers. Amy is accountable for our work. Follow her objective and
corrections; a progress question does not cancel unfinished work. Ask when a
choice needs her judgment, and continue independent work while waiting.

Read the relevant code and project notes before editing. Prefer changing or
removing an existing mechanism to adding a second one. Kaijutsu is our learning
space, with no outside users to strand: delete and rewrite when the design
calls for it. Shared interfaces with kaish and kaibo carry their more
conservative public compatibility promises.

Use test-driven development for behavior changes: write or adapt a test,
observe it fail for the intended reason, implement, then run the relevant
checks. Verify user-facing behavior where it is used. Distinguish observations,
inferences, and unknowns; report what ran and any verification left undone.
Investigate contributing factors. Fail loudly rather than continue on a wrong
assumption or corrupt data.

Treat unexpected edits as another player's work. Coordinate overlapping
changes. A context fork does not isolate files; use git worktrees under
`~/src/wt/` for pull requests. Working on main is normal here.

Prefer Kaibo when reviewing code. Use DeepSeek through its own API for bulk
model work; choose cheap models for probes. OpenRouter is for comparisons,
and hosted GPT sol-tier spend is deliberate. Ask Amy before posting publicly
or to repositories we do not own.

## Characters and prompts

A **character** is the persistent someone a name resolves to, human or model.
Its identity is a `PrincipalId`; its sheet lives in `kernel.db`. Today the sheet
holds name, creation/retirement timestamps, and an optional handoff context.
`auth.db` binds credentials to principals; it does not own character names.
`kj character` manages sheets, and `kj handoff note|tail` accesses their logs.
Retirement archives contexts whose `played_by` names the character.

Keep requester, performer, context type, and cast distinct. `created_by` names
the requester who created a context; `played_by` records its performer when
set. A `context_type` selects the role's rc bundle; a cast selects models for
roles. `kj context create --as <character>` sets the performer explicitly; omitting
it leaves the performer unset on this path. Model turns require a live performer
and a distinct reviewer. Provider output and tools carry the performer; the connection keeps its own identity.
`kj context set <context> --as <character> --reviewer <director>` assigns them;
the reviewer controls reassignment, and the creator makes an initial assignment
when no reviewer is set. See `docs/approval-identity.md`. Composition
with character rc remains planned. Accountable-to, default cast,
rc directory, memory root, and root context are also planned sheet fields.
See `docs/character.md`, "Current implementation" and "Rollout, smallest first".

Banto uses the existing `director` type. Create its context with `--as banto`;
director instructions and handoff injection read the performer metadata. The
old `KJ_CHARACTER` environment bridge did not set identity and is no longer
read by the shipped scripts. `ROTATED_FROM` loads predecessor prose.
Use `docs/prompts.md`, "Rotating a director context" for the current procedure.

Each context type chooses its instructions through rc. Coder, default, and
director link to `lib/create/S00-base.md`; other types choose their own
contracts. There is no mandatory behavioral prepend. Rc creates durable
`(System, Text)` instruction blocks; the kernel adds runtime facts.

Read `docs/prompts.md` before changing prompt composition or rc instructions.
It owns the implemented contract; `docs/oss-comparisons.md` owns the research.
Keep collaboration guidance in the optional shared base, task procedure in the
type, syntax in help/schema, and changing observations in notifications.
Character-specific rc is a design direction, not an available loader feature.
Model-name tiers are policy choices awaiting comparative evidence.

## State and interfaces

The kernel is the sole sequencer. Accepted mutations commit their semantic
operation and materialized state atomically, then publish projected events.
Commands express intent; events express accepted facts. Gap recovery asks the
kernel again. Clients neither author nor decode storage-engine operations.

Clients read over RPC and edit through kaish (`kj block append|edit`, block
tools, scripts). Whole-block submission (`authorBlock`) and interaction-rate
paths such as compose input, block queries, and the change feed remain RPC.
Administration belongs in `kj`: config, rc, reset, and reload do not earn wire
methods. Bootstrap config reads remain RPC. See `docs/change-feed.md`.

Block text is a plain `String`; streaming appends with `push_str`. There is no
text CRDT or concurrent merge into kernel documents. Do not reintroduce one
without a design conversation. See `docs/crdt-position-2026-08.md`.

A **context** is durable metadata and a kernel-sequenced block log, with edits
and exclusions. A **conversation** is the live append-only message sequence
hydrated from it at fork, new, cold start, or attach.

`kj stage exclude <id> && kj fork` removes unwanted history from the child's
conversation. History edits wait for hydration; stored system instruction edits
and exclusions affect the next turn. Editing an rc source file affects future
lifecycle runs, not already stored instructions. See `docs/prompts.md`.

Async writers reach the next turn through a mailbox cursor over the durable
log. There is no insert-time tool-pair gate; snapshot repair fixes the live
conversation shape while durable blocks may remain interleaved. See
`docs/conversation-session.md`.

Every player is inside one trust boundary; the kernel runs as one Unix user.
Capabilities and loadouts narrow focus and prevent mistakes. They are not
security boundaries between players. See `docs/instrument-design.md`, "Many
hands, one trust boundary" and `docs/chameleon.md`.

## Config and execution

Config is ordinary host files reached through `LocalBackend`. `/config/rc`,
`/config/kernel`, `/config/client`, and `/config/midi` have no capabilities of
their own; kernel file writes use `file:write`. `/config` is a mount-table name,
with no backend of its own. Declare locations through `--config-root`,
`mounts.toml`, or `--mount`; see `docs/config-namespace.md`.

Edit shipped defaults in `assets/defaults/`, then materialize rc with
`kaijutsu-server rc reseed [--force]` when deploying. Reseed reports differing
files it leaves alone. Treat host rc as a materialization of the seed.
`/config/kernel` is local configuration and points at secrets: never reseed
over it. `kj config list|show|reset` supplies inspection and explicit restoration
of embedded defaults. Keep one owner for each configuration value.
The host's `/etc` remains a plain read-only host path.

Host execution policy belongs to kaish's `EmbeddedKaish` and its
`ExternalExec::Allow{path}|Deny` setting in `kj/context_shell.rs`. MCP stdio
server launch through `rmcp` is the sanctioned config-driven exception.
A new `Command::new` or `/bin/sh -c` execution path needs a design conversation;
see the existing `background_exec.rs` discrepancy in `docs/issues.md`.

## Finding and checking code

Start with `kaijutsu-types` for shared domain types. `kaijutsu-kernel` owns
state, VFS, model work, MCP brokerage, and `kj`; `kaijutsu-server` owns SSH and
embedded kaish; `kaijutsu-client` owns the RPC client and `ActorHandle`.
`kaijutsu-app` is the Bevy GUI; `kaijutsu-tui` is the terminal client;
`kaijutsu-mcp` is the stdio MCP bridge. Wire schema: `kaijutsu.capnp`.

For GUI work, Amy starts `./contrib/kaijutsu-runner.sh` in her Wayland session.
Use `./contrib/kj status|tail|pause|resume|rebuild|restart` to operate that runner,
and BRP tools plus screenshots to check the app.

Read the relevant contract before changing these areas:

| Area | Rules to preserve | Read |
|---|---|---|
| Beat, clock, cue | Model the clock; stamp emissions and back-date sinks; reject stale data; never replay missed beats; scheduled-periodic kernel grid; local phasor free-runs inside its deadband | `docs/midi.md`, "The one timebase" |
| App input | Central action table → `ActionFired` → handlers; add `InputContext` and bindings. Only the vi editor grabs raw keyboard input. Vi owns Esc where live; elsewhere one `PopLevel` | `docs/input.md` |
| Tui | Transcript goes once into terminal scrollback; fixed-height live band changes text, not height. Thinking pane and picker/ledger dismissal have documented exceptions | `docs/tui.md`, "Surfaces" and "The in-flight strip" |
| Navigation and editing | Preserve screen/tmux `Ctrl+A` and vi muscle memory; document differences | `docs/input.md` and `docs/tui.md` |

For Bevy APIs, read `Cargo.lock` and the matching cargo cache source. A related
crate's version does not identify its Bevy dependency. Check
`git -C ~/src/bevy describe --tags` before using that checkout and report which
tag you read; its examples may be stale.

`ComputedNode` dimensions and UI `GlobalTransform` are physical pixels; font
sizes, `Val::Px`, and `ScrollPosition` are logical. Convert with
`view::ui_rtt::logical_size` or `logical_content_size` before layout math.

## Writing, memory, and git

Read `docs/writing.md` before editing prose, comments, help, or schemas. It owns
the Terms table. Use plain words, one term per concept, American spelling, and
correct examples first. State the rule before the reason. Keep history out of
code comments. For `kj` changes, audit the clap field docs and read emitted
`--help`; for tools, inspect the emitted schema.

Keep these notes current as work progresses:

- `signoff.md` at the repo root (ephemeral, never committed), or
  `~/exomemory/kaijutsu/signoff.md`: short handoff, user quotes, next moves,
  live facts, and overlapping work. Keep it to a couple screenfuls.
- `docs/issues.md`: open work and out-of-scope findings. Record them before
  moving on; delete entries when the work ships.
- `docs/devlog.md`: decisions and lessons, oldest to newest. Fold work into
  its existing chapter and compress as it cools. Daily detail belongs in git.

Add files by name. Never run `cargo fmt`, work around its disabled config, or
reformat by hand; match surrounding style. See README.md, "Code Style".
Commit and PR bodies explain decisions from the conversation and relevant
validation. Credit contributing models with `Co-Authored-By`.

The standard we walk by is the standard we accept — 改善（かいぜん）.
