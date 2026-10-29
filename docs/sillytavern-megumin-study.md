# SillyTavern and Megumin Suite study

A living document. We delete it when the study ends; git history keeps it.
Started 2026-10-28.

We read two roleplay tools to learn how to make persistent characters for
play, to complement the musicians. This document sorts what we learned into
changes to kaijutsu itself and things we build on kaijutsu without changing
it. Open storyteller work that predates this split stays in
`docs/issues.md`, "Storyteller: what the prototype leaves open".

## Sources

Both clones are read-only references. We do not install or run them.

- **SillyTavern**: `~/src/research/SillyTavern`, shallow clone at `06bde93`.
  Most of it is prompt assembly. The useful parts are where text goes (depth
  injection, post-history instructions), lore that loads on a keyword, a
  greeting and example dialogue, and a rolling summary. Files we read:
  `public/scripts/world-info.js`, `public/scripts/extensions/memory/index.js`,
  `public/scripts/group-chats.js`, `public/scripts/PromptManager.js`.
- **Megumin Suite**: `~/src/research/Megumin-Suite-Beta`, at `87493c2`
  (2026-10-28, its V11 beta). A SillyTavern extension by KazumaONIISAN
  (Arif-salah), under CC BY-NC 4.0. Its README is a dated changelog that
  records why each feature exists. The engine text is in
  `data/modes/v11taina.js`, and the background jobs are under `src/features/`.

The license lets us borrow Megumin text for personal, noncommercial use with
credit. Borrowed text stays in host rc and never goes into `assets/defaults`.

## What runs today

- **Stock** (commit `6db3b86a`): the `storyteller` type.
  - `create/S05-card.kai` reads `cards/<performer>.md`, the SillyTavern card
    description.
  - `submit/S10-reminder.kai` adds a `(System, Notification)` after each
    submit, SillyTavern's post-history instructions. A card can replace it
    with `cards/<performer>.reminder.md`.
  - The stance carries the roleplay rules: never write the person's
    character, stop where the person can act, and `((...))` is out-of-character
    talk.
- **Local, on spaceghost**, in `/config/rc/storyteller/`:
  - `megumin/` holds six V11 Тайна (Taina) sections, taken word for word:
    reader, characters, world, story, narration, and dialogue. It also holds
    the "V11 Inside" style in `style.md`. `megumin/SOURCE.md` records the
    source and what was left out.
  - `create/S03-local-megumin.kai` composes those sections into one
    instruction block between the stance and the card.
  - `cards/kotone.megumin` turns it on for one performer and sets its names.
  - The performer is Kotone, the night watch of Sawari Lane, on
    `anthropic/claude-haiku-5-5`.

If the borrowed text works, Amy writes a new version by hand. That version
can ship.

## Changes to kaijutsu

These need kernel, seed, or `kj` work.

### Local rc files survive a forced reseed

Today a local file at a path the seed does not ship survives every reseed
(`seed_scripts.rs`, `reseed_rc_files`). An edit to a stock file does not
survive: `rc reseed --force` restores it, and nothing can turn a stock script
off locally.

Proposal: a `.keep` sidecar file. After `touch S00-stance.md.keep`, reseed
leaves `S00-stance.md` alone, even with `--force`.

- Rc already ignores any name that is not `SXX-name.kai`, so only reseed
  changes.
- To turn a stock script off, keep it and make its body `exit 0`.
- A symlinked seed is replaced by a local file plus `.keep`.
- Reseed lists kept files. Its report on each diverged file names the
  `.keep` command.
- Later, the `.keep` file could store the seed hash it was kept against, so
  that reseed can report a kept file whose seed changed since.

We chose a sidecar over a marker comment in the file. A Markdown companion is
read whole into the prompt, and a symlink cannot carry a comment.

### A notification that replaces the previous one

Megumin's biggest lesson: the extension keeps the true state and sends one
current copy each turn. It also removes old copies from the history the model
reads, because models read stale values from old blocks ("20, 35, 65, 42
affection all the way down").

Our submit reminder accumulates, one per submit. Excluding the old copy does
not help a live conversation: an excluded block already in the session stays
in the next request. Only the next hydrate drops it
(`docs/conversation-session.md`, "Original target": excludes on blocks already
sent are no-ops).

We need a way for a notification to replace the one before it. It must work
both in the live conversation and at hydration. One possible design is a
supersede key on the notification: a new block with the same key hides the
older ones. That would serve reminders, current state, and the notebook below.
It touches the mailbox and snapshot code, so it needs a design conversation.
The rotation filter in `docs/issues.md` is the alternative that needs only
existing mechanisms. It is coarser.

### A scribe seat on the pulse

Megumin's Chomusuke is a second, cheap model. It reads each finished reply
and updates world state, blocks, and the notebook, so the story model spends
its words on the story.

- When the person submits while Chomusuke is still working, the submit waits
  up to 30 s, then goes ahead without the update.
- Each job can be canceled.
- Each prompt can be previewed before it is sent.

This is the kernel's central job at a small scale: work that has a deadline
(the next turn), a declared fallback (go ahead with the old state), and a
miss that should be observable. In kaijutsu, the scribe would be a seat with
its own cast, for example DeepSeek Flash. A lifecycle point after each model
turn would wake it. The next submit would wait for its result up to a
deadline, and record a miss when the result is late.

The unbuilt part is the wait: today a submit does not wait for named
background work. We read `docs/hyoushigi.md` and the anticipation notes in
`docs/issues.md` before designing it.

### Smaller items

- **Character rc directory**: cards at `/config/rc/storyteller/cards/` stand
  in for the planned `rc_dir` sheet field (`docs/character.md`, slice 5).
- **Interjecting while a model thinks**, for council API clients. This is an
  idea, already listed in `docs/issues.md`.

## Built on kaijutsu

These need only rc scripts, cards, files, and seats that exist today, plus,
where named, a change from the section above.

### Craft text

- **Megumin's sections** are running locally (see "What runs today"). Watch:
  - Who speaks. The stance says to play the card's character, and V11 Inside
    says a narrator rides inside one character.
  - Length. The stance asks for 1 to 3 short paragraphs, and Megumin asks for
    full scenes.
  - The tension curve: Simmer, Build, Build, Peak, Breather. After three
    turns that each raised tension, the next is a breather, but never in the
    middle of a scene.
  - Endings: a turn ends on a character's action or line, never on a menu of
    choices.
- **A last-position nudge.** Megumin repeats its thinking rules as the very
  last message, because models that think too long obey the last thing they
  read. A per-card reminder file already does this; no code is needed.

### Voice and opening

- **Greeting** (SillyTavern `first_mes`): a create script appends the card's
  greeting as the first model turn. Check first that `kj block create`
  accepts the model role and that hydration accepts a model turn first.
- **Example dialogue** (`mes_example`): a card section. Kotone's card has one
  ("Kotone's voice").

### Lore that loads on a keyword

SillyTavern's World Info loads lore when a keyword appears in recent turns,
with a scan depth and a size budget. Here a submit script would:

1. Read the person's newest turn.
2. Match it against `lore/*.md`, with the keywords in the first line of each
   file.
3. Add the matches to the reminder, up to a byte budget.

It can work today, with the same accumulation as the reminder. It works
better once a notification can replace the previous one.

### The author's notebook

Megumin's notebook keeps two or three hidden truths per story. Each truth
records:

- who it is hidden from
- when it comes out
- what it changes when it does

Clues come before the reveal. The extension holds the notebook, not the
chat. Then the model cannot forget a secret, rewrite it, or give it away by
accident.

- At most 3 live truths.
- A clue the model claims it planted is checked against the reply text.
- A truth comes out when the person reaches where it is kept. A truth is
  protected by being hard to reach, never by refusing the person.
- Amy does not see the notebook unless she asks; spoilers are opt-in.

Here, the notebook would be a file per context. The scribe writes it, and a
submit script adds the live truths to the per-turn notification. It depends on
the scribe seat, and on where per-context state lives for a seat with no
tools. The three `<story>` paragraphs we left out of the local Megumin text
come back with it.

### Current state with short lists

Megumin limits its lists: 3 unresolved threads, 3 planted seeds, 2
consequence timers, 4 characters off-screen. When a list is full, the oldest
item drops. Lists are numbered, so a model deletes an item by number. Any
state the scribe keeps follows these rules.

### A list of overused phrases

A periodic job asks a model for the 5 most overused patterns in the story,
each written as a general rule rather than a quoted phrase. The reminder then
carries those rules. It runs as a scribe job, or on a timer as a drift. It
helps long stories on small models, which repeat themselves.

### Continuity past the context limit

- **Rolling summary** (SillyTavern summarizes every 10 messages): we use
  rotation with a handoff, and `kj fork --compact`. The storyteller has no
  tools, so it cannot write its own handoff note (`docs/issues.md`).
- **Swipes** (regenerating a reply): `kj stage exclude` on the reply, then
  `kj fork`. Megumin also undoes the state changes a swiped reply made. Our
  state would need the same rule.

### A clock for the lane

A `tick` script could make the night advance on its own, with the clappers at
each corner as a notification. Wait until a few nights of play show whether
Amy wants it.

## Not taking

- Megumin's five-role writers' room inside the thinking. It is a prompt
  layout, and its authors had to add a "Thinking Cap" because models
  over-thought it.
- The content rules that remove limits. The person sets a story's limits.
- The ComfyUI image pipeline.
- Megumin's per-model loadouts. Casts do this here, and per-model tuning
  waits for comparison evidence.
