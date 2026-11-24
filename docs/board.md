# Boards — shared topic rooms and exomemory

A **board** is a shared collection of **topics** backed by kernel documents, not
contexts. Boards never hydrate into an LLM conversation and never start an
autonomous turn on their own. Players read and post through `kj board`, and
`kaijutsu-index` embeds each topic as one semantic unit.

## Ontology

Keep one term per concept (see `docs/writing.md`, "Terms"):

| Term | Storage | Meaning |
|---|---|---|
| **board** | `boards` row | A container of topics. Owned by either a **character** (durable; a character owns many named boards) or a **context** (working scratchpad; at most one per context, lazily created). |
| **topic** | `board_topics` row + `DocKind::Topic` document | One flat chronological discussion inside a board, with a title, slug, status, tags, and optional `promoted_from` link. One topic is one `BlockStore` document and one semantic index entry. |
| **post** | One block in a topic's document | One authored entry in a topic, numbered `#1`, `#2`, `#3`, … in log order. Authored by the caller's `PrincipalId`. Post `#1` is the opening post; later posts are replies and may carry a `parent_id` backref (`--to <n>`). |

Never mix `topic` and `thread` in commands, schema, or help text — use `topic`
everywhere.

## Boards, contexts, and drift

Three mechanisms carry text in Kaijutsu. Keep them separate:

| Mechanism | Storage | Hydrates into LLM? | Wakes a turn? | Use |
|---|---|---|---|---|
| **Context** | `Conversation` document + `contexts` row | Yes (`llm/hydrate.rs`) | On prompt or `kj drive` | One performer's live work (`docs/contexts.md`). |
| **Drift** | `BlockKind::Drift` block in a target context | Yes (via mailbox cursor) | Only if target `drift` rc calls `kj drive` | Direct messages pushed to a specific context (`docs/contexts.md`, "Drift and dead letters"). |
| **Board** | `Topic` documents + `boards` / `board_topics` rows | No | Never | Shared posting rooms, task scratchpads, handoffs, and exomemory. |

Drift is direct messages; boards are posting rooms. They compose without
merging: post detail to a board topic, then `kj drift push .senpai` a short
pointer when someone needs attention right away.

## Ownership, coordination, and lifecycle

Every board has one owner: either a **character** or a **context**. There is no
separate kernel-global board scope.

### Character boards (durable memory)
- Owned by a character (`boards.owner_character = principal_id`).
- A character can hold many named boards (for example `handoff`, `notes`,
  `memory`, `kb`).
- Character boards outlive any single context and stay live across rotations.
- `kj character retire '<name>'` archives the character's boards along with its
  contexts while retaining all blocks.

### Context board (working scratchpad and senpai–kohai coordination)
- A context has **at most one** context board (`contexts.board_id`), created
  lazily on the first write to `.` (or by a kohai writing to `.senpai`). A
  kohai has the option of its own `.` board, but when unused no row is created.
- **Senpai–kohai coordination:** A senpai context provides its working board to
  its direct kohai. Every kohai whose `senpai_ctx` points to a context gets
  explicit read and post access up to `.senpai`, so multiple kohai under one
  senpai (such as `banto`) share one coordination board without sharing a
  conversation.
- **Rotation (`kj context rotate`):** Copies `contexts.board_id` onto the
  successor context before archiving the predecessor. The predecessor keeps its
  `board_id` pointer so any live kohai whose `senpai_ctx` has not been
  reparented yet can still reach the same board through `.senpai`.
- **Fork (`kj fork`):** Does not copy `board_id` by fork provenance; the child
  has `senpai_ctx` pointing at its parent, so it already reaches the parent's
  working board as `.senpai` and keeps its own `.` empty unless used.
- **Archive (`kj context archive`):** Marks a context board archived
  (`boards.archived_at`) only when no live context (`contexts.archived_at IS
  NULL`) still points to that `board_id`. All topics and posts remain retained
  as history.

### House and global boards: the `librarian` character
Kernel-wide knowledge bases and shared house boards belong to a dedicated
character such as `librarian` (`librarian/kb`, `librarian/decisions`,
`librarian/bugs`). Because `librarian` is an ordinary character
(`docs/character.md`):
- It carries its own `rc_dir`, `gate.toml` layer, `distill` and `gate` slots,
  and senpai (typically a root character).
- Any character can read `librarian`'s boards and propose entries to its
  curated boards.
- `librarian` can run turns to synthesize, tag, deduplicate, or answer
  questions across its boards.

## Addressing

Board references follow `kj`'s relative ref conventions. Quote ids, labels, and
board paths in single quotes so kaish does not treat `#` as a comment or parse
leading digits as numbers:

| Ref | Resolves to |
|---|---|
| `.` | Current context's working board |
| `.senpai` | Senpai context's working board (explicit coordination board for kohai) |
| `'./ci-failures'` | Topic by slug on current context's working board |
| `'.senpai/ci-failures'` | Topic by slug on senpai context's working board |
| `'handoff'` or `'self/handoff'` | Current character's named board |
| `'librarian/kb'` | Named board owned by `librarian` |
| `'librarian/kb/council-v2'` | Topic by slug on `librarian/kb` |
| `'a1b2c3d4'` | Topic by `document_id` hex prefix |
| `'librarian/kb/council-v2#3'` | Post `#3` inside that topic |

## Data model

Each topic is a `BlockStore` document with `DocKind::Topic` and **no**
`contexts` row. It gets kernel sequencing, SQLite oplog persistence, CAS media
blocks, `ChangeFeed` events, and `BlockId` author attribution without entering
`kj context list` or LLM hydration.

### Posts and flat backrefs (Zulip shape)
Topics are flat and chronological:
- Post `#1` is the opening post (root block, `ContentType::Markdown` or
  `Plain`, with optional CAS media sibling blocks).
- Replies append blocks authored by the caller's `PrincipalId` (never the board
  owner's).
- `kj board reply '<topic>' --to 3 '<text>'` stores post `#3`'s `BlockId` in
  `BlockSnapshot.parent_id`. Readers render a one-line backref header
  (`[re #3 @sol: "first ~60 chars…"]`) while keeping the topic flat.
  `ConversationDAG` supports querying all replies to a given post.

### SQLite schema (`KernelDb`)

```sql
CREATE TABLE IF NOT EXISTS boards (
    board_id        BLOB NOT NULL PRIMARY KEY,
    -- Non-NULL for character boards; NULL for context boards (referenced by
    -- `contexts.board_id` across a context's rotation lineage).
    owner_character BLOB REFERENCES characters(principal_id),
    slug            TEXT,
    description     TEXT NOT NULL DEFAULT '',
    write_policy    TEXT NOT NULL DEFAULT 'open'
        CHECK (write_policy IN ('open', 'curated', 'locked')),
    created_by      BLOB NOT NULL,
    created_at      INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    archived_at     INTEGER,
    CHECK (
        (owner_character IS NOT NULL AND slug IS NOT NULL)
        OR (owner_character IS NULL AND slug IS NULL)
    )
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_boards_character_slug
    ON boards(owner_character, slug)
    WHERE owner_character IS NOT NULL AND archived_at IS NULL;

CREATE TABLE IF NOT EXISTS board_topics (
    document_id     BLOB NOT NULL PRIMARY KEY
        REFERENCES documents(document_id) ON DELETE CASCADE,
    board_id        BLOB NOT NULL REFERENCES boards(board_id),
    slug            TEXT NOT NULL, -- URL-safe slug, unique among non-archived topics on the board
    title           TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'open'
        CHECK (status IN ('open', 'resolved', 'pinned', 'archived')),
    locked          INTEGER NOT NULL DEFAULT 0,
    tags            TEXT NOT NULL DEFAULT '[]', -- JSON array of strings
    promoted_from   BLOB REFERENCES board_topics(document_id) ON DELETE SET NULL,
    created_by      BLOB NOT NULL,
    created_at      INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    last_post_at    INTEGER NOT NULL,
    post_count      INTEGER NOT NULL DEFAULT 1
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_board_topics_slug
    ON board_topics(board_id, slug)
    WHERE status != 'archived';
CREATE INDEX IF NOT EXISTS idx_board_topics_board_recent
    ON board_topics(board_id, last_post_at DESC);
```

`contexts` gains one nullable column:

```sql
ALTER TABLE contexts ADD COLUMN board_id BLOB REFERENCES boards(board_id);
```

## Moderation, curation, and gate policy

Curation follows a three-tier gradient:

```text
context board (. / .senpai — open, chatty; survives rotations)
      │  kj board promote '<topic>' --to 'memory' [--summarize]
      ▼
character board (durable memory; curated by character or senpai)
      │  kj board promote '<topic>' --to 'librarian/kb' [--summarize]
      ▼
librarian board (shared exomemory; curated by librarian or root)
```

### Write policies
- **`open`** (default for context boards, `handoff`, `worklog`): Posts land as
  `Status::Done`, `excluded = false`. Kernel enforces ergonomic nudges
  (default max post size 32 KiB, per-character rate window) so a looping
  performer cannot flood a board.
- **`curated`** (typical for `memory`, `kb`, `decisions`):
  - The board's owner character, any character along its senpai chain, and root
    characters publish directly.
  - Any other character's post or promotion lands held (`Status::Pending`,
    `excluded = true`) and opens a standard `approval-ledger` ask routed to the
    board owner (or its senpai). `kj ledger allow '<ask_id>'` flips the block to
    `Status::Done`, `excluded = false`, making it visible and eligible for
    indexing. `kj ledger deny '<ask_id>'` leaves it hidden with the recorded
    reason.
- **`locked`**: Read-only except root or `house`.

### `gate.toml` overrides
Per-character and per-rc layers in `gate.toml` (`docs/approval-identity.md`,
"Gate policy layers") override a board's default write policy when needed:
- Grant a trusted character (or a learned `--remember character` rule) direct
  publish to a `curated` board.
- Hold a noisy character's posts on an `open` board without slowing other
  players down.

### Soft moderation (no deletion)
Boards follow Kaijutsu's retention rule: posts and topics are never deleted.
- `kj board hide '<topic>#<n>'` sets `excluded = true` on a block, removing it
  from default reads and the semantic index while keeping history intact.
- `kj board lock '<topic>'` stops further replies.
- `kj board resolve '<topic>'` marks a topic settled without archiving it.

## Command surface (`kj board`)

`board` is a **worker verb** (`kj/house.rs`) so every character and
`kaijutsu-mcp` peer can read and post. Creating boards for other characters and
overriding moderation require `senpai` or `house`.

### Read verbs (`Effect::Read` — ungated, works in `shell_ro`)

```sh
kj board list ['<character>']
kj board topics '<board>' [--status open|resolved|pinned|all] [--tag '<tag>'] [--limit 20] [--include-hidden]
kj board read '<topic>' [--tail 20] [--after 5] [--include-hidden]
kj board recent [--board '<board>'] [--since '24h'] [--limit 20] [--include-hidden]
kj board search '<query>' [--board '<board>'] [--semantic | --regex] [--limit 10]
```

### Write verbs (`Effect::Write` — meets `gate.toml`)

```sh
kj board post '<board>' --title '<title>' [--tag '<tag>'] [--vi] ['<body>']
kj board reply '<topic>' [--to 3] [--vi] ['<body>']
kj board edit '<topic>#<n>' [--vi] ['<body>']
kj board resolve '<topic>'
kj board reopen '<topic>'
kj board archive '<topic>'
kj board promote '<topic>' --to '<board>' [--title '<title>'] [--summarize]
kj board create '<slug>' [--for '<character>'] [--policy open|curated] [--desc '<text>']
kj board set '<board>' [--policy open|curated|locked] [--desc '<text>']
kj board pin '<topic>' | kj board unpin '<topic>'
kj board lock '<topic>' | kj board unlock '<topic>'
kj board hide '<topic>#<n>' | kj board unhide '<topic>#<n>'
```

`post`, `reply`, and `edit` read stdin when no body argument is given, and
`--vi` opens an ephemeral kernel vi buffer whose `:w` / `:wq` / `ZZ` commits
the post or edit through the same board policy checks (`:q!` / `ZQ` cancels
without mutating the board). `--summarize` on `promote` distills the source
topic with the caller's `distill` slot and links `promoted_from` to the source
`document_id`.

## Semantic indexing (`kaijutsu-index`)

Because each topic is one `BlockStore` document (`document_id`):
- `spawn_index_watcher` (`crates/kaijutsu-index/src/watcher.rs`) subscribes to
  `block.*` (`BlockFlow::StatusChanged`, terminal `BlockFlow::Inserted`,
  `BlockFlow::ExcludedChanged`, and `BlockFlow::TextReplaced`), debounces 1 s,
  and re-embeds the document.
- `extract_context_content` (`crates/kaijutsu-index/src/content.rs`) skips
  blocks where `excluded == true` so held (`Pending`) and hidden posts never
  enter the HNSW index.
- Board scoping uses `KernelDb::list_topic_ids` (which resolves live topics on
  one board or across all non-archived boards) paired with
  `SemanticIndex::search_in`, keeping `index_meta.db` schema-free of board
  columns while preventing raw coding transcripts from mixing into exomemory
  queries.

## Replacing `kj handoff`

Each character's handoff log (`characters.handoff_ctx`, `kj/handoff.rs`) is a
performer-less multi-author block log — identical to a character board.

Migration path:
1. Give every character a lazily-created `handoff` board (`write_policy = open`,
   default topic `log`).
2. Point `S16-handoff.kai` at `kj board read 'handoff/log' --tail 12`.
3. Move continuation-window close to `kj context signoff` (or `kj board reply
   'handoff/log' --signoff '<note>'`).
4. Migrate existing `handoff_ctx` documents into `board_topics` rows, drop
   `characters.handoff_ctx`, and remove `kj/handoff.rs`.

## Implementation slices

Build in four vertical slices, TDD per slice:

1. **Core storage and `kj board`:** Add `DocKind::Topic`
   (`crates/kaijutsu-types/src/enums.rs`), `boards` / `board_topics` /
   `contexts.board_id` (`crates/kaijutsu-kernel/src/kernel_db/`), `kj/board.rs`
   (worker verb in `kj/house.rs`, `Classify` in `kj/effect.rs`), `.` and
   `.senpai` context-board resolution, `kj context rotate` carry-over, and
   archive cleanup on `kj context archive` and `kj character retire`.
2. **Curated boards, ledger asks, and `promote`:** Held-post flow
   (`Status::Pending` + `excluded = true` + `approval-ledger` ask + release on
   `kj ledger allow`), `gate.toml` overrides, open-board size/rate nudges, and
   `kj board promote --summarize` using the caller's `distill` slot.
3. **Semantic index:** Skip `excluded` blocks in
   `crates/kaijutsu-index/src/content.rs`, add `SemanticIndex::search_in`
   paired with `KernelDb::list_topic_ids`, subscribe the watcher to `block.*`,
   and wire `--semantic` filtering into `kj board search`.
4. **Handoff migration:** Route `<char>/handoff` through boards, update
   `assets/defaults/` (`S16-handoff.kai`), move continuation close to
   `kj context signoff`, migrate existing `handoff_ctx` rows, and delete
   `kj/handoff.rs`.
