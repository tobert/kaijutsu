# Chameleon bass from System 1 decisions

A Python prototype: the bass chair plays a vamp from council decisions on the
megakernel at `http://zorak:8090`. One decision covers a window of 4, 8, or 16
bars. The model chooses from a menu, and the client renders the choice as ABC
in the chart's key. Drums come after bass works.

```sh
python3 contrib/chameleon-s1/bass_s1.py                                   # 2 windows of 8 bars, Chameleon chart
python3 contrib/chameleon-s1/bass_s1.py --chart contrib/chameleon-s1/charts/d-dorian.json --window 4 --windows 4
python3 contrib/chameleon-s1/bass_s1.py --key F# --mode minor --changes "F#m7 Bm7 C#7"
python3 contrib/chameleon-s1/bass_s1.py --compare --log compare.jsonl     # every shape at 4, 8 and 16 bars
python3 contrib/chameleon-s1/bass_s1.py --replay compare.jsonl --seed 2   # re-sample a log, no server calls
python3 contrib/chameleon-s1/bass_s1.py --render-all --chart contrib/chameleon-s1/charts/d-dorian.json
python3 contrib/chameleon-s1/bass_s1.py --print-spec plan
```

The ABC tune goes to stdout, and one line per window goes to stderr.
`--render-all` prints every cell on every chord and makes no server calls.

## The chart

A chart is a small JSON file. `--key`, `--mode`, `--meter`, and `--changes`
override its fields.

```json
{"title": "Chameleon", "key": "Bb", "mode": "dorian", "meter": "4/4", "changes": ["Bbm7", "Eb7"]}
```

- **`changes`** is one chord per bar, repeating. A chord is a root (`A` to
  `G`, then `b` or `#`) and a quality: `""`, `maj7`, `6`, `m`, `m7`, `m6`,
  `7`, `9`, `13`, `m7b5`, `dim`, `dim7`, `sus4`, or `7sus4`. An unknown quality
  exits with the list.
- **`mode`** is `major`, `minor`, or a church mode (`dorian`, `mixolydian`,
  and so on). It sets the `K:` line, the spelling, and the scale a `walk` uses.
- **`meter`** must be `4/4`. The cells are written for 4/4 only, and any
  other meter exits.

Cells are built from each chord's root and quality, so any key works. Roots
sit between C2 and B2 (MIDI 36 to 47), and every note stays below middle C.
Pitches are spelled in the key. An accidental is written when a pitch
differs from the key signature, or when its letter was already altered in
that bar. That way, readers that carry an accidental per letter and readers
that carry it per octave hear the same pitch. I checked every cell on every
chord in five charts (Bb Dorian, D Dorian, F# minor, Eb major with `m7b5`
and `sus4`, A Mixolydian on one chord) with `kaijutsu-abc` in Strict mode,
using a scratch binary outside the repo. Every bar parsed with no feedback,
and the MIDI note numbers matched the intended pitches.

| Cell | On Bbm7, before Eb7 | Meaning |
|---|---|---|
| `root_fifth` | `B,,4 F,4` | root then fifth, two half notes |
| `hold` | `B,,8` | the root for the whole bar |
| `riff` | `B,,3 B,, z2 F, A,` | syncopated funk figure |
| `pump` | `B,, B, B,, B, B,, B, B,, B,` | root and octave in eighth notes |
| `arpeggio` | `B,,2 D,2 F,2 A,2` | root, third, fifth, seventh |
| `walk` | `B,,2 A,,2 G,,2 F,,2` | scale steps into the next bar's root |
| `approach` | `B,,6 =E,,2` | root, then a half step into the next bar's root |
| `space` | `B,,2 z6` | one short root, then rest |

`walk` and `approach` aim at the pitch the next bar's root actually plays.
When the scale has fewer than three steps between the two roots, `walk` ends
with the next root's chromatic neighbor instead: Dm7 to G7 walks `D E F F#`.

## One decision per window

The council reads non-text questions independently. "Each `choice`, `score`,
and `noul` question is read after the `text` answers before it in the spec's
order, and its own answer never enters the text any later question is read
after" (`docs/council-api.md`, "Specs"). This server has no `text`
questions, so every answer in a decision is read on its own, and the client
samples each answer on its own. I compared three spec shapes:

- **`per_bar`.** One `choice` question per bar, `bar1` to `bar16`, each over
  the eight cells. `ask` picks the first 4, 8, or 16.
- **`form`.** Four questions: a form over the window's quarters (`AAAA`,
  `AAAB`, `AABA`, `ABAB`), a groove cell for A, a contrast cell for B, and an
  ending for the last bar (`walk`, `approach`, `space`, `pump`, or `none`).
- **`plan`.** One question whose 20 options are whole plans: a groove
  (`root_fifth`, `riff`, `hold`, `pump`, `arpeggio`) for every bar and an
  ending (`walk`, `approach`, `space`, `none`) for the last one, such as
  `riff_walk`.

Each decision's case is one sentence: the window's bars, its chords, the
chord after it, and the last cell of the previous window. The held context
holds the playing guidance, then the chart. Both are turns marked `snap`, so
a new chart re-feeds only the chart turn.

### Latency

One decision per shape and window size on the Chameleon chart, run
sequentially with 1 s pauses, from moltar to zorak while gate traffic was
live (`compare-bb.jsonl`):

| Shape | Questions | 4 bars | 8 bars | 16 bars | Input tokens at 16 bars |
|---|---|---|---|---|---|
| `per_bar` | 4, 8, 16 | 665 ms | 1144 ms | 2047 ms | 1671 |
| `form` | 4 | 622 ms | 639 ms | 665 ms | 416 |
| `plan` | 1 | 241 ms | 258 ms | 273 ms | 104 |

- **Cost grows with the number of questions, not the window.** `per_bar`
  grows linearly. At 16 bars it passed the server's default 2000 ms timeout,
  so the script sends `timeout_ms: 10000`.
- **The specs cost a lot to warm.** The `PUT` that warmed all three spec
  layers fed 4166 tokens and took 9053 ms. Most of that is `per_bar`, whose
  menu is repeated in 16 questions. Warming `plan` alone fed 503 to 543
  tokens and took 972 to 1170 ms. After that, the sample runs' `plan`
  decisions took 244 to 273 ms.
- At about 95 BPM a 4/4 bar lasts about 2.5 s. A `plan` decision is about a
  tenth of one bar for a window of 4 to 16 bars.

### Coherence

From the same distributions (argmax, and 5 sampled seeds at temperature
1.8):

- **`per_bar` does not follow the phrase.** Each bar's question sees the
  same case, so the distributions barely differ: `bar1` gives `riff` 1.00,
  and the last bar gives `riff` 0.85 to 0.90 and `approach` 0.04 to 0.09.
  Argmax plays `riff` in every bar with no turnaround. Sampling scatters
  `approach` into the middle of the window (`riff approach riff riff`):
  jumps that point nowhere.
- **`form` is coherent but has no turnaround.** A window cannot jump,
  because it is made from two cells. But the `ending` question cannot see
  the form or the groove, and it gives `none` 0.52 to 0.84. The form is
  `AAAA` 0.92 to 0.95.
- **`plan` follows the phrase.** The top plans are `riff_walk` 0.39 to 0.40,
  `riff_approach` 0.25 to 0.29, and `riff_none` 0.18 to 0.20. Argmax ends
  every window size with a turnaround, and 4 of 5 seeds did. Choices that
  belong together have to be one option, because separate questions
  cannot condition each other.
- **The window size hardly moved any distribution.** Within one shape,
  4, 8, and 16 bars got nearly the same numbers.

The shipped default is `plan`, with 8-bar windows.

## Samples

`--temperature 1.8` is the default. Above 1 it flattens the distribution
before sampling (`p ** (1 / T)`, renormalized), so less likely plans get
played. At 1.8, `riff_walk` 0.39 and `riff_space` 0.08 sample about 7 to 3
instead of 5 to 1.

Chameleon, 2 windows of 8 bars, seed 1, live (`sample-bb.jsonl`). The draws
were `root_fifth_none` and then `riff_none`, so this run has no turnaround:

```text
X:1
T:bass
M:4/4
L:1/8
K:Bb dor
B,,4 F,4 | E,,4 B,,4 | B,,4 F,4 | E,,4 B,,4 |
B,,4 F,4 | E,,4 B,,4 | B,,4 F,4 | E,,4 B,,4 |
B,,3 B,, z2 F, A, | E,,3 E,, z2 B,, D, | B,,3 B,, z2 F, A, | E,,3 E,, z2 B,, D, |
B,,3 B,, z2 F, A, | E,,3 E,, z2 B,, D, | B,,3 B,, z2 F, A, | E,,3 E,, z2 B,, D, |
```

The same distributions re-sampled with `--replay sample-bb.jsonl --seed 3`
drew `riff_walk`, then `riff_approach`. In a replay, the case for the second
window still says how the live run's first window ended:

```text
B,,3 B,, z2 F, A, | E,,3 E,, z2 B,, D, | B,,3 B,, z2 F, A, | E,,3 E,, z2 B,, D, |
B,,3 B,, z2 F, A, | E,,3 E,, z2 B,, D, | B,,3 B,, z2 F, A, | E,,2 F,,2 G,,2 A,,2 |
B,,3 B,, z2 F, A, | E,,3 E,, z2 B,, D, | B,,3 B,, z2 F, A, | E,,3 E,, z2 B,, D, |
B,,3 B,, z2 F, A, | E,,3 E,, z2 B,, D, | B,,3 B,, z2 F, A, | E,,6 =A,,2 |
```

D Dorian, Dm7 to G7, 4 windows of 4 bars, seed 1, live (`sample-d.jsonl`).
The draws were `root_fifth_approach`, `riff_none`, `riff_space`, and
`root_fifth_none`:

```text
X:1
T:bass
M:4/4
L:1/8
K:D dor
D,,4 A,,4 | G,,4 D,4 | D,,4 A,,4 | G,,6 ^D,,2 |
D,,3 D,, z2 A,, C, | G,,3 G,, z2 D, F, | D,,3 D,, z2 A,, C, | G,,3 G,, z2 D, F, |
D,,3 D,, z2 A,, C, | G,,3 G,, z2 D, F, | D,,3 D,, z2 A,, C, | G,,2 z6 |
D,,4 A,,4 | G,,4 D,4 | D,,4 A,,4 | G,,4 D,4 |
```

Each `plan` decision in these runs took 244 to 273 ms.

## What the council API can do for music

It can choose, and it cannot write.

- **Choices with probabilities: yes.** `choice` (2 to 26 options on this
  server, `choice_options`), `score` (2 to 10 ordered levels), and `noul`
  (probability of yes). See `docs/council-api.md`, "Specs".
- **Answers within one decision are independent.** Only `text` answers
  condition later questions. A musical dependency has to be one option in a
  joint menu, as `plan` does, or the input to a second decision.
- **Free text: not on this server.** `text` questions need the `describe`
  capability. The megakernel lists `leave_one_out`, `persist`, and `warm`,
  and a spec with a `text` question gets a `400`. Even with `describe`, a
  `text` answer is a short description, not generation.
- **Generation is out of the contract.** `docs/council-api.md`, "Open
  questions", names the megakernel's `/mk/v1/generate` as outside it. A
  `GET` on that path answers `405`; this prototype does not use it.
- **Questions come from the spec, never the request.** A request may pick
  questions with `ask` and narrow a `choice` with `options`. A new idea is a
  new spec.
- **The client decides.** `choice` is an argmax. Sampling, temperature, and
  fallback belong to the caller.
- **The response has `ms` but no `queue_ms`.** `ms` was 3 to 5 ms below
  client wall time. Waiting behind gate traffic cannot be told apart from
  compute.

## Open questions

- **A 20-plan menu is all-riff at the top.** "The riff is the signature" in
  the guidance puts `riff` near 1.0 as a groove. Is that the band she wants,
  or should the guidance spread the grooves?
- **Contrast inside a window.** `plan` has no B section. Adding forms to the
  menu would pass the 26-option limit. A second decision could choose a
  contrast with the plan in its case, for about 250 ms more.
- **Temperature 1.8** is a first guess at "loose and fun". It needs her ear.
- **`walk` on a short step.** Dm7 to G7 walks `D E F F#`. That is chromatic
  at the end, which is idiomatic, but she should hear it.
