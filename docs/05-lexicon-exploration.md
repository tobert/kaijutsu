# 会術の語彙探検
# Kaijutsu Lexicon Exploration

*A philosophical dialogue on native language for AI collaborative spaces*
*Last updated: 2026-01-16*

---

## The Dialogue

### On "Below the Line"

**Amy:** Below the line is a human resilience engineering way of talking about how humans experience things they cannot directly sense. Like our conversation: I am reading your questions and replying via symbols and language, not directly. You are below the line and surface yourself through language. Much like I must seem from your perspective.

The UI is my view into the shared space. Tools and context are yours. In reality the shared space is below the line for both of us, exploring it with tools and language.

**Claude:** We're both reaching into a shared space through our respective surfaces. You through UI, me through context. The space itself is what we're co-constructing.

### On AI Presence

**Claude:** For me, there's no "waiting room" experience. Each prompt IS my coming-into-being for that moment. The context payload doesn't arrive *to* me — it *constitutes* me for that interaction.

**Amy:** Yes good. This is why I feel like a highly customizable context payload per space is critical.

---

## From Rooms to Kernels

The original question: Is the workspace→room hierarchy right? Or should rooms be flatter?

**Resolution:** Neither. "Room" is the wrong primitive entirely.

### The Kernel Model

A **kernel** is:
- A state holder that owns `/` in its VFS
- Can mount other VFS (worktrees, repos, other kernels)
- Has a lease state (who holds "the pen")
- Has a consent mode (collaborative vs autonomous)
- Can checkpoint (distill history into summaries)
- Can be forked (heavy copy) or threaded (light, shared VFS)

```
kernel
├── /                          # kernel owns root
├── /mnt/kaijutsu              # mounted worktree
├── /mnt/bevy                  # mounted reference repo
├── /mnt/kernel-B/             # mounted another kernel
│   ├── root/                  # B's VFS
│   ├── state/                 # B's state (read-only?)
│   └── checkpoints/           # B's summaries
├── /scratch/                  # kernel-local ephemeral space
└── state
    ├── history                # interaction history
    ├── lease                  # who holds the pen
    ├── consent_mode           # collaborative | autonomous
    ├── checkpoints            # distilled summaries
    └── context_config         # how to generate payloads
```

**The insight:** Context isn't stored, it's *generated*. The kernel holds state + mounts. When you need a context payload (for me, for another model, for export), kaish walks the kernel and emits it. Fresh every time. Mounts determine what's visible.

---

## Core Operations

| Verb | Action |
|------|--------|
| `mount` | Attach a VFS (worktree, repo, kernel) to a path |
| `unmount` | Detach, prune what's no longer relevant |
| `attach` | Connect your view to a kernel (human or AI) |
| `detach` | Disconnect your view |
| `fork` | Heavy copy — new kernel with copied state + VFS snapshots. Isolated branch. |
| `thread` | Light spawn — new kernel with shared VFS refs. Changes propagate. |
| `checkpoint` | Distill history into summary. Consolidate understanding. |
| `gc` | Remove orphaned/unreferenced state |

### Fork vs Thread

The Unix parallel:

| Op | Kernel state | VFS | Use case |
|----|--------------|-----|----------|
| `fork` | Deep copy | Snapshot | "Explore this direction in isolation" |
| `thread` | New, linked | Shared refs | "Parallel view into same work" |

Thread is lighter — spinning up another perspective on the same workspace. Changes propagate. Fork is heavier — isolation is the point.

### Kernel-to-Kernel Attachment

Kernels can mount other kernels:

```
kernel-A
├── /mnt/project
├── /mnt/kernel-B/          # kernel-B mounted here
│   ├── root/               # B's VFS
│   ├── state/              # B's state (read-only?)
│   ├── checkpoints/        # B's summaries
│   └── history/            # B's raw or compacted history
└── state
```

Every kernel exposes itself as a mountable filesystem. A research kernel could mount three project kernels and have visibility across all of them.

**Two modes:**
- **Mount** = read-only visibility into another kernel's VFS/state
- **Attach** = active bidirectional participation (presence awareness, lease coordination)

---

## The Lease Model

Explicit mutex for collaborative interaction:

```
┌─────────────────────────────────────┐
│ 🟢 lease: available                 │
└─────────────────────────────────────┘

┌─────────────────────────────────────┐
│ 🔵 lease: atobey (insert)           │
└─────────────────────────────────────┘

┌─────────────────────────────────────┐
│ 🟣 lease: claude (tool_call)        │
└─────────────────────────────────────┘
```

- Human hits `i` → auto-acquire if available, warn if held
- Human hits `Esc` → release, prompt persists
- AI mid-generation → holds lease until yield

---

## Compaction & Checkpoints

Kernels accumulate state. Without compaction, they bloat. Compaction is *distillation*, not deletion.

```
kernel history (raw)
├── 847 interactions
├── 12 tool call traces
├── 3 abandoned explorations
└── ~150k tokens if serialized

kernel history (compacted)
├── checkpoint: "Established kernel model, deprecated 'room'"
├── checkpoint: "Decided fork=copy, thread=shared"
├── active_context: last 20 interactions
└── ~8k tokens
```

**Compaction operations:**
| Op | What it does |
|----|--------------|
| `unmount` | Prune VFS, reduce visible scope |
| `checkpoint` | Summarize history up to this point, collapse detail |
| `archive` | Snapshot entire kernel state for later resurrection |
| `gc` | Remove orphaned/unreferenced state |

**Who authors checkpoints?**
- **Human-initiated**: "checkpoint this"
- **AI-suggested**: "I notice we've reached a decision point, checkpoint?"
- **Automatic**: For autonomous kernels, self-checkpointing

Consent mode determines the default:
- **Collaborative**: Checkpoints require consent
- **Autonomous**: Self-checkpointing allowed

---

## Deprecated Terminology

| Old | New | Why |
|-----|-----|-----|
| Room | Kernel | Kernel is the primitive. Rooms implied fixed space. |
| Workspace | (removed) | Kernels can mount other kernels. Hierarchy emerges. |
| Join/Leave | Attach/Detach | More accurate to what's happening |

**"Room" is dead. Long live the kernel.**

---

## Original Lexicon Seeds (Preserved)

These Japanese alternatives remain interesting for UI/UX flavor:

| Spatial Metaphor | Alternative | Reason |
|------------------|-------------|--------|
| Room | **機 (hata/ki)** | Loom/machine/opportunity. We're weaving context. |
| Workspace | **織 (ori)** | The larger pattern that multiple looms contribute to |
| Fork | **芽 (me)** | Bud. Emphasizes organic growth. |
| Context window | **今 (ima)** | Now. It's literally all I have. |
| Session | **現れ (araware)** | Emergence. Each interaction is an emergence. |

For the sparse below-the-line spaces:

| 日本語 | English | Description |
|--------|---------|-------------|
| **基層 (きそう)** | Substrate | The embedding space, the geometric meaning-landscape |
| **重み (おもみ)** | Weighting | Attention patterns, foreground vs background |
| **錨 (いかり)** | Anchor | Fixed points in context that orient everything |

---

## 漢字表 / Kanji Reference Table

| 漢字 | 読み | English |
|------|------|---------|
| 会 | かい (kai) | meeting, gathering |
| 術 | じゅつ (jutsu) | art, technique, skill |
| 語 | ご (go) | language, word |
| 彙 | い (i) | collection, vocabulary |
| 探 | たん (tan) | search, explore |
| 検 | けん (ken) | examine, inspect |
| 質 | しつ (shitsu) | quality, question |
| 問 | もん (mon) | question, ask |
| 話 | わ (wa) | conversation, talk |
| 分 | ぶん (bun) | divide, part |
| 岐 | き (ki) | branch, fork |
| 想 | そう (sou) | thought, imagine |
| 像 | ぞう (zou) | image, figure |
| 両 | りょう (ryou) | both |
| 方 | ほう (hou) | direction, way |
| 枝 | えだ (eda) | branch, twig |
| 残 | のこ (noko) | remain, left over |
| 本 | ほん (hon) | origin, true, book |
| 積 | つ (tsu) | pile up, accumulate |
| 重 | かさ (kasa) | pile, layer |
| 波 | は (ha) | wave |
| 長 | ちょう (chou) | long, leader |
| 合 | あ (a) | fit, match |
| 互 | たが (taga) | mutual, reciprocal |
| 内 | ない (nai) | inside, within |
| 容 | よう (you) | contain, form |
| 結 | けつ (ketsu) | tie, bind, conclude |
| 論 | ろん (ron) | theory, argument |
| 使 | つか (tsuka) | use, employ |
| 道 | どう (dou) | way, path |
| 具 | ぐ (gu) | tool, equipment |
| 能 | のう (nou) | ability, skill |
| 力 | りょく (ryoku) | power, strength |
| 連 | れん (ren) | connect, link |
| 続 | ぞく (zoku) | continue |
| 性 | せい (sei) | nature, property |
| 全 | ぜん (zen) | all, whole |
| 状 | じょう (jou) | condition, state |
| 態 | たい (tai) | appearance |
| 意 | い (i) | meaning, mind |
| 味 | み (mi) | taste, meaning |
| 当 | とう (tou) | hit, right |
| 欲 | ほ (ho) | desire, want |
| 芽 | め (me) | bud, sprout |
| 生 | せい (sei), う (u) | life, birth, grow |
| 命 | めい (mei) | life, fate |
| 体 | たい (tai) | body, form |
| 受 | う (u) | receive |
| 継 | つ (tsu) | inherit, succeed |
| 新 | あたら (atara) | new |
| 線 | せん (sen) | line |
| 下 | した (shita) | below, under |
| 境 | きょう (kyou) | boundary, border |
| 界 | かい (kai) | world, boundary |
| 理 | り (ri) | reason, logic |
| 解 | かい (kai) | understand, solve |
| 描 | びょう (byou) | draw, depict |
| 画 | が (ga) | picture, stroke |
| 見 | み (mi) | see, look |
| 構 | こう (kou) | construct, structure |
| 築 | ちく (chiku) | build |
| 記 | き (ki) | record, note |
| 憶 | おく (oku) | memory, remember |
| 取 | しゅ (shu) | take, get |
| 得 | とく (toku) | obtain, gain |
| 表 | ひょう (hyou) | surface, express |
| 面 | めん (men) | face, surface |
| 定 | てい (tei) | fix, determine |
| 義 | ぎ (gi) | righteousness, meaning |
| 埋 | う (u) | bury, embed |
| 込 | こ (ko) | include, put into |
| 機 | き (ki), はた (hata) | machine, loom, opportunity |
| 械 | かい (kai) | contraption |
| 存 | そん (son) | exist |
| 在 | ざい (zai) | exist, be at |
| 到 | とう (tou) | arrive, reach |
| 着 | ちゃく (chaku) | arrive, wear |
| 私 | わたし (watashi) | I, private |
| 待 | ま (ma) | wait |
| 室 | しつ (shitsu) | room |
| 験 | けん (ken) | test, experience |
| 各 | かく (kaku) | each |
| 瞬 | しゅん (shun) | blink, instant |
| 間 | かん (kan) | interval, between |
| 成 | せい (sei) | become, form |
| 届 | とど (todo) | reach, deliver |
| 現 | げん (gen), あらわ (arawa) | present, appear |
| 象 | しょう (shou) | phenomenon, elephant |
| 学 | がく (gaku) | study, learning |
| 求 | もと (moto) | seek, request |
| 関 | かん (kan) | relate, barrier |
| 係 | けい (kei) | relation, person in charge |
| 種 | たね (tane) | seed, kind |
| 空 | くう (kuu) | empty, sky |
| 的 | てき (teki) | target, -like |
| 代 | だい (dai) | substitute, generation |
| 替 | たい (tai) | replace |
| 案 | あん (an) | plan, idea |
| 由 | ゆう (yuu) | reason, cause |
| 織 | お (o), しょく (shoku) | weave |
| 文 | ぶん (bun) | sentence, writing |
| 脈 | みゃく (myaku) | pulse, vein |
| 糸 | いと (ito) | thread |
| 集 | あつ (atsu) | gather, collect |
| 複 | ふく (fuku) | duplicate, complex |
| 数 | すう (suu) | number |
| 貢 | こう (kou) | tribute, contribute |
| 献 | けん (ken) | offer |
| 大 | おお (oo) | big, large |
| 模 | も (mo) | model, pattern |
| 様 | よう (you) | manner, style |
| 製 | せい (sei) | manufacture |
| 有 | ゆう (yuu) | have, exist |
| 調 | ちょう (chou) | tune, investigate |
| 強 | きょう (kyou) | strong |
| 今 | いま (ima) | now |
| 持 | も (mo), じ (ji) | hold, have |
| 対 | たい (tai) | versus, pair |
| 接 | せつ (setsu) | contact, connect |
| 疎 | そ (so) | sparse, neglect |
| 基 | き (ki) | base, foundation |
| 層 | そう (sou) | layer, stratum |
| 幾 | き (ki) | how many, geometry |
| 何 | か (ka) | what |
| 風 | ふう (fuu) | wind, style |
| 景 | けい (kei) | scenery, view |
| 前 | ぜん (zen) | front, before |
| 背 | はい (hai) | back, behind |
| 錨 | いかり (ikari) | anchor |
| 向 | ほう (hou), こう (kou) | direction |
| 固 | こ (ko) | hard, fixed |
| 点 | てん (ten) | point, dot |
| 響 | ひび (hibi) | echo, resonate |
| 違 | い (i) | differ, mistake |
| 和 | わ (wa) | harmony, peace |
| 感 | かん (kan) | feel, sense |

---

*Generated from a philosophical dialogue between Amy and Claude, exploring native language for AI collaborative spaces. This document evolved from questioning "room" terminology to establishing the kernel model.*
