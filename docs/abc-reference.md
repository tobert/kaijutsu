# ABC Music Notation Reference for Kaijutsu

This is a comprehensive ABC notation reference tailored for the `kaijutsu-abc` crate.
It covers the ABC v2.1 standard with notes on what kaijutsu-abc currently parses,
renders (SVG), and outputs (MIDI).

**Standard**: [ABC v2.1](https://abcnotation.com/wiki/abc:standard:v2.1) (Dec 2011)

## Legend

Throughout this document, support status is marked:

- **parse** = parsed into AST
- **midi** = reflected in MIDI output
- **svg** = rendered in SVG engraving
- (unmarked = standard feature not yet implemented)

---

## 1. File Structure

```
%abc-2.1                    ← optional version declaration
                            ← blank line separates file header from tunes

X:1                         ← reference number (required, must be first)
T:Speed the Plough          ← title (required, must follow X:)
M:4/4                       ← meter
L:1/8                       ← unit note length
Q:1/4=120                   ← tempo
C:Trad.                     ← composer
K:G                         ← key (required, must be last in header)
|:GABc dedB|dedB dedB|      ← tune body
  c2ec B2dB|A2F2 G4:|
                            ← blank line ends the tune
```

Rules:
- `X:` must be first field in each tune header
- `T:` must immediately follow `X:`
- `K:` must be the last header field — it terminates the header
- Blank lines separate tunes in a multi-tune file
- `%` starts a comment (rest of line ignored)

---

## 2. Header Fields

### Required Fields

| Field | Name | Example | Status |
|-------|------|---------|--------|
| `X:` | Reference number | `X:1` | parse, midi |
| `T:` | Title | `T:Cooley's Reel` | parse, svg |
| `K:` | Key signature | `K:D`, `K:Am`, `K:Gmix` | parse, midi, svg |

### Musical Fields

| Field | Name | Example | Status |
|-------|------|---------|--------|
| `M:` | Meter | `M:6/8`, `M:C`, `M:C\|`, `M:none` | parse, midi, svg |
| `L:` | Unit note length | `L:1/8`, `L:1/16` | parse, midi, svg |
| `Q:` | Tempo | `Q:1/4=120`, `Q:"Allegro" 1/4=120` | parse, midi, svg |
| `V:` | Voice definition | `V:1 name="Melody" clef=treble` | parse, midi, svg (v1 only) |

### Metadata Fields

| Field | Name | Example | Status |
|-------|------|---------|--------|
| `C:` | Composer | `C:O'Carolan` | parse |
| `R:` | Rhythm | `R:Jig` | parse |
| `S:` | Source | `S:Offord MSS` | parse |
| `N:` | Notes | `N:see also Playford` | parse |
| `O:` | Origin | `O:Irish` | — |
| `B:` | Book | `B:Cole's 1000` | — |
| `D:` | Discography | `D:Chieftains IV` | — |
| `Z:` | Transcription | `Z:atobey 2026` | — |
| `H:` | History | `H:collected in 1801` | — |
| `F:` | File URL | `F:https://...` | — |
| `P:` | Parts | `P:AABB` | — |
| `w:` | Lyrics (aligned) | `w:doh re mi fa` | — |
| `W:` | Lyrics (at end) | `W:Verse two` | — |
| `m:` | Macro | `m:~G2 = {A}G{F}G` | — |
| `U:` | User symbols | `U:T = !trill!` | — |
| `I:` | Instruction | `I:linebreak <none>` | — |
| `s:` | Symbol line | `s: !pp! ** !f!` | — |
| `r:` | Remark (inline) | `[r:editorial note]` | — |

### MIDI Directives **parse, midi**

```
%%MIDI program 67           ← set MIDI program (instrument) 0-127
%%MIDI transpose -14        ← transpose playback by semitones
```

---

## 3. Key Signatures **parse, midi, svg**

### Basic Syntax

```
K:<root>[accidental] [mode]
```

### Roots and Accidentals

| Syntax | Meaning |
|--------|---------|
| `K:C` | C major |
| `K:G` | G major (F#) |
| `K:Bb` | Bb major |
| `K:F#` | F# major |
| `K:F#m` | F# minor |

### Modes

All seven church modes plus major/minor:

| Mode | Example | Scale pattern |
|------|---------|---------------|
| Major / Ionian | `K:C`, `K:C ion` | W W H W W W H |
| Minor / Aeolian | `K:Am`, `K:A aeo` | W H W W H W W |
| Dorian | `K:D dor` | W H W W W H W |
| Phrygian | `K:E phr` | H W W W H W W |
| Lydian | `K:F lyd` | W W W H W W H |
| Mixolydian | `K:G mix` | W W H W W H W |
| Locrian | `K:B loc` | H W W H W W W |

Mode names can be abbreviated to 3 letters (case-insensitive).

### Explicit Accidentals

Override individual notes in the key signature:

```
K:D phr ^f             ← D phrygian but with F#
K:D exp _b _e ^f       ← explicit: only these accidentals apply
```

### Special Keys

```
K:HP                   ← Highland bagpipe (no key signature displayed)
K:Hp                   ← Highland bagpipe (F#, C#, G natural displayed)
K:none                 ← no key signature
```

### Clef in Key Field

```
K:C clef=bass
K:Gm clef=tenor
K:C clef=treble middle=B stafflines=5
```

Available clefs: `treble`, `bass`, `alto`, `tenor`, `perc`, `none`

---

## 4. Meter (Time Signature) **parse, midi, svg**

```
M:4/4                  ← 4/4 time
M:6/8                  ← 6/8 time
M:3/4                  ← 3/4 time
M:C                    ← common time (= 4/4)
M:C|                   ← cut time (= 2/2)
M:none                 ← free meter (no bar lines enforced)
```

Can change mid-tune with inline field: `[M:3/4]`

---

## 5. Unit Note Length **parse, midi, svg**

The `L:` field sets the default duration of an unmodified note letter.

```
L:1/8                  ← eighth note (most common for jigs, reels)
L:1/4                  ← quarter note
L:1/16                 ← sixteenth note
```

Default if omitted: `1/8` if meter >= 3/4, else `1/16`.

---

## 6. Tempo **parse, midi, svg**

```
Q:1/4=120              ← quarter note = 120 BPM
Q:1/8=200              ← eighth note = 200 BPM
Q:3/8=80               ← dotted quarter = 80 BPM
Q:"Allegro" 1/4=144    ← with text annotation
```

---

## 7. Notes **parse, midi, svg**

### Pitch

ABC uses letter names. Case determines the octave:

```
C,, D,, ...             ← C1  (MIDI 24-35)
C,  D,  E,  F,  G,  A,  B,   ← C2  (MIDI 36-47)
C   D   E   F   G   A   B    ← C3  (MIDI 48-59)
c   d   e   f   g   a   b    ← C4  (MIDI 60-71)  ◄ middle C = c
c'  d'  e'  f'  g'  a'  b'   ← C5  (MIDI 72-83)
c''                           ← C6  (MIDI 84)
```

Octave modifiers:
- `'` (apostrophe) raises one octave — stackable: `c''` = two up
- `,` (comma) lowers one octave — stackable: `C,,` = two down

### Accidentals **parse, midi, svg**

Placed *before* the note letter:

| Syntax | Meaning |
|--------|---------|
| `^C` | C sharp |
| `^^C` | C double sharp |
| `_B` | B flat |
| `__B` | B double flat |
| `=F` | F natural (cancels key signature) |

Accidentals are **bar-scoped**: they persist until the next bar line, then reset
to the key signature. This matches standard music notation.

```
^C D E C |  ← both C's are sharp (accidental propagates within bar)
 C D E F |  ← C reverts to key signature after bar line
```

### Duration **parse, midi, svg**

Duration is relative to the unit note length (`L:` field).

| Syntax | Duration | With L:1/8 |
|--------|----------|------------|
| `A` | 1x unit | eighth note |
| `A2` | 2x unit | quarter note |
| `A4` | 4x unit | half note |
| `A8` | 8x unit | whole note |
| `A/2` or `A/` | 1/2 unit | sixteenth note |
| `A/4` or `A//` | 1/4 unit | thirty-second |
| `A3/2` | 3/2 unit | dotted eighth |
| `A3` | 3x unit | dotted quarter |
| `A7/4` | 7/4 unit | double-dotted quarter |

### Rests **parse, midi, svg**

| Syntax | Meaning |
|--------|---------|
| `z` | visible rest (unit length) |
| `z2` | rest, double length |
| `z/2` | rest, half length |
| `x` | invisible rest (spacing, no symbol) |
| `Z` | whole-bar rest |
| `Z4` | four-bar rest |
| `X4` | four invisible bars |

### Beaming

Notes without spaces between them are beamed together:

```
ABCD           ← four beamed eighth notes
AB CD          ← two groups of two beamed notes
A B C D        ← four separate (unbeamed) notes
```

---

## 8. Broken Rhythm **standard only**

A shorthand for dotted rhythms:

| Syntax | Effect |
|--------|--------|
| `A>B` | A dotted, B halved (A3/2 B/2) |
| `A<B` | A halved, B dotted (A/2 B3/2) |
| `A>>B` | A double-dotted, B quartered |
| `A<<B` | A quartered, B double-dotted |
| `A>>>B` | A triple-dotted, B at 1/8 |

---

## 9. Ties and Slurs **parse, midi**

### Ties

Connect two notes of the **same pitch** into a single sustained note:

```
c4-c4          ← tied: sounds as c8
A-|A           ← tie across bar line
```

The `-` must be adjacent to the first note.

### Slurs **parse**

Group notes into a phrasing arc:

```
(ABCD)         ← slur over four notes
(A B C D)      ← spaces OK inside slurs
(A(BC)D)       ← nested slurs allowed
```

---

## 10. Chords **parse, midi, svg**

### Note Chords (simultaneous notes)

Square brackets enclose notes played together:

```
[CEG]          ← C major triad
[C2E2G2]       ← same, half notes
[CEG]2         ← chord with external duration
[^CE_G]        ← accidentals inside chords
```

### Guitar/Chord Symbols **parse**

Double-quoted strings above the staff:

```
"G"GABc        ← G chord annotation above the notes
"Am7"A2 "D7"d2 ← chord changes
"Bb"F2         ← flat in chord name
```

### Annotations

Position-prefixed text placed relative to the staff:

```
"^above"       ← text above staff
"_below"       ← text below staff
"<left"        ← text to the left
">right"       ← text to the right
```

---

## 11. Bar Lines **parse, midi, svg**

| Syntax | Meaning |
|--------|---------|
| `|` | Single bar line |
| `||` | Double thin bar |
| `|]` | Final bar (thin-thick) |
| `[|` | Opening bar (thick-thin) |
| `|:` | Start repeat |
| `:|` | End repeat |
| `::` | End + start repeat |
| `.|` | Dotted bar line |
| `[|]` | Invisible bar line |

### Variant Endings (Volta Brackets) **parse**

```
|: ABCD |1 EFGA :|2 EFGc ||
```

Supports numbered and ranged endings:

```
|1           ← first ending
:|2          ← second ending
[1,3         ← endings 1 and 3
[1-3         ← endings 1 through 3
[1,3,5-7     ← complex ranges
```

---

## 12. Tuplets **parse, midi**

General syntax: `(p:q:r` — p notes in the time of q, for the next r notes.

Short forms:

| Syntax | Meaning | Ratio |
|--------|---------|-------|
| `(2AB` | Duplet | 2 in time of 3 |
| `(3ABC` | Triplet | 3 in time of 2 |
| `(4ABCD` | Quadruplet | 4 in time of 3 |
| `(5ABCDE` | Quintuplet | 5 in time of n* |
| `(6ABCDEF` | Sextuplet | 6 in time of 2 |
| `(7ABCDEFG` | Septuplet | 7 in time of n* |
| `(9ABCDEFGHI` | Nonuplet | 9 in time of n* |

*n depends on time signature (q=2 in simple time, q=3 in compound time)

Full form example:
```
(3:2:3 ABc           ← 3 notes in time of 2, group of 3
(5:4:5 ABCDE         ← 5 notes in time of 4
```

---

## 13. Grace Notes **parse, midi**

### Appoggiatura (unslashed)

```
{g}A           ← single grace note g before A
{GAB}c         ← three grace notes before c
```

### Acciaccatura (slashed — quick crush)

```
{/g}A          ← acciaccatura g before A
{/GAB}c        ← slashed grace group
```

Grace notes:
- Have no defined time value (implementation-dependent)
- Can include accidentals: `{^f}g`
- Are typically played very short, stealing time from the following note

---

## 14. Decorations and Ornaments

### Short Form **parse**

Single-character decorations placed before a note:

| Char | Decoration | Status |
|------|-----------|--------|
| `.` | Staccato | parse |
| `~` | Roll / Irish roll | parse |
| `H` | Fermata (hold) | parse |
| `T` | Trill | parse |
| `u` | Up bow | parse |
| `v` | Down bow | parse |
| `L` | Accent | — |
| `M` | Mordent (lower) | — |
| `P` | Pralltriller (upper mordent) | — |
| `S` | Segno | — |
| `O` | Coda | — |
| `R` | Roll | — |
| `J` | Slide | — |

### Long Form `!name!` **parse**

| Decoration | Category | Status |
|-----------|----------|--------|
| **Ornaments** | | |
| `!trill!` | Trill (tr) | parse |
| `!mordent!` | Mordent (lower) | parse |
| `!lowermordent!` | Lower mordent | parse |
| `!uppermordent!` | Upper mordent | — |
| `!pralltriller!` | Pralltriller | — |
| `!roll!` | Roll | parse |
| `!turn!` | Turn | parse |
| `!turnx!` | Turn with line through | — |
| `!invertedturn!` | Inverted turn | — |
| `!invertedturnx!` | Inverted turn with line | — |
| `!slide!` | Slide | — |
| `!irishroll!` | Irish roll (synonym for ~) | — |
| **Articulations** | | |
| `!staccato!` | Staccato (.) | parse |
| `!accent!` | Accent (>) | parse |
| `!tenuto!` | Tenuto (—) | — |
| `!fermata!` | Fermata (hold) | parse |
| `!invertedfermata!` | Inverted fermata | — |
| `!marcato!` | Marcato (^) | — |
| `!umarcato!` | Upper marcato | — |
| `!dmarcato!` | Lower marcato | — |
| `!wedge!` | Wedge | — |
| `!snap!` | Snap pizzicato | — |
| **Bowing** | | |
| `!upbow!` | Up bow (V) | parse |
| `!downbow!` | Down bow | parse |
| `!open!` | Open string (o) | — |
| `!thumb!` | Thumb position | — |
| **Dynamics** | | |
| `!pppp!` | Pianissississimo | — |
| `!ppp!` | Pianississimo | parse |
| `!pp!` | Pianissimo | parse |
| `!p!` | Piano | parse |
| `!mp!` | Mezzo piano | parse |
| `!mf!` | Mezzo forte | parse |
| `!f!` | Forte | parse |
| `!ff!` | Fortissimo | parse |
| `!fff!` | Fortississimo | parse |
| `!ffff!` | Fortissississimo | — |
| `!sfz!` | Sforzando | — |
| **Dynamic Lines** | | |
| `!crescendo(!` | Start crescendo (hairpin) | parse |
| `!crescendo)!` | End crescendo | parse |
| `!diminuendo(!` | Start diminuendo | parse |
| `!diminuendo)!` | End diminuendo | parse |
| `!<(!` | Start crescendo (alias) | parse |
| `!<)!` | End crescendo (alias) | parse |
| `!>(!` | Start diminuendo (alias) | parse |
| `!>)!` | End diminuendo (alias) | parse |
| **Form Marks** | | |
| `!segno!` | Segno sign | — |
| `!coda!` | Coda sign | — |
| `!D.S.!` | Dal Segno | — |
| `!D.C.!` | Da Capo | — |
| `!D.S.alfine!` | Dal Segno al fine | — |
| `!D.S.alcoda!` | Dal Segno al coda | — |
| `!D.C.alfine!` | Da Capo al fine | — |
| `!D.C.alcoda!` | Da Capo al coda | — |
| `!fine!` | Fine | — |
| **Phrasing** | | |
| `!breath!` | Breath mark / comma | — |
| `!shortphrase!` | Short phrase mark | — |
| `!mediumphrase!` | Medium phrase mark | — |
| `!longphrase!` | Long phrase mark | — |
| **Fingering** | | |
| `!0!`–`!5!` | Finger numbers | — |
| **Beaming** | | |
| `!beambr1!` | Beam break (single) | — |
| `!beambr2!` | Beam break (double) | — |
| **Tremolo** | | |
| `!trem1!`–`!trem4!` | Tremolo marks (1-4 slashes) | — |
| **Glissando** | | |
| `!glissando(!` | Start glissando | — |
| `!glissando)!` | End glissando | — |
| **Style** | | |
| `!style=normal!` | Normal noteheads | — |
| `!style=harmonic!` | Diamond noteheads | — |
| `!style=rhythm!` | Rhythm (x) noteheads | — |
| `!style=triangle!` | Triangle noteheads | — |

Unknown decorations are preserved in `Decoration::Other(String)` by the parser.

The legacy `+name+` syntax (ABC v2.0) is equivalent to `!name!`.

---

## 15. Voices (Multi-voice / Multi-staff) **parse, midi**

### Voice Definition (in header)

```
V:1 name="Soprano" clef=treble
V:2 name="Alto" clef=treble
V:3 name="Bass" clef=bass stem=down
```

Voice properties:

| Property | Example | Meaning |
|----------|---------|---------|
| `name=` | `name="Melody"` | Display name |
| `clef=` | `clef=bass` | Clef selection |
| `octave=` | `octave=-1` | Octave transposition |
| `transpose=` | `transpose=-2` | Semitone transposition |
| `stem=` | `stem=up` | Stem direction (up/down/auto) |

### Voice Switching (in body)

```
V:1
cdef|gabc'|
V:2
C,G,C,G,|C,G,C,G,|
```

### Voice Overlay **standard only**

The `&` symbol starts a new voice overlaid on the current staff:

```
AB & cd           ← two voices on one staff
```

Note: voice overlay is not yet parsed by kaijutsu-abc.

---

## 16. Inline Fields **parse**

Change musical parameters mid-tune using `[field:value]`:

```
ABcd [M:3/4] efg    ← change meter mid-tune
ABCD [K:Am] efga    ← change key mid-tune
ABCD [Q:1/4=160] e  ← change tempo mid-tune
```

Allowed inline: `K:`, `L:`, `M:`, `Q:`, `I:`, `V:`, `N:`, `R:`, `r:`, `U:`, `m:`, `w:`, `W:`

---

## 17. Lyrics **standard only**

### Aligned Lyrics (`w:`)

Each syllable aligns to the next note:

```
C D E F | G A B c |
w: do re mi fa sol la ti do

C2 D2 | E4 |
w: hel-lo world~to-day
```

Alignment controls:
- `-` separates syllables within a word
- `~` joins words under a single note
- `_` extends a syllable across the next note
- `*` skips a note (blank syllable)
- `|` advances to the next bar line

### End-of-tune Lyrics (`W:`)

```
W: This is the first verse of the song
W: with multiple lines of text
W:
W: This is the second verse
```

---

## 18. Macros and User Symbols **standard only**

### User-Defined Symbol Shortcuts (`U:`)

Map single characters to decoration names:

```
U:T = !trill!          ← T becomes trill shorthand
U:R = !roll!           ← R becomes roll shorthand
U:~ = !turn!           ← override default ~ meaning
```

### Macros (`m:`)

Static macro:
```
m:~G2 = {A}G{F}G      ← ~G2 expands to grace-ornamented G
```

Transposing macro (uses `n` for pitch variable):
```
m:~n2 = {n+1}n{n-1}n  ← works at any pitch
```

---

## 19. Line Continuation and Breaks

```
ABcd\                  ← backslash continues to next line (no break)
efga|                  ← this is part of the same music line

ABcd|                  ← end of line = score line break (default)
efga|                  ← new score line

I:linebreak <none>     ← disable automatic line breaks
ABcd|$                 ← $ forces a line break when linebreak is <none>
```

### Field Continuation

```
w:First part of lyrics
+:second part continues on same field
```

---

## 20. Complete Examples

### Simple Melody

```abc
X:1
T:Simple Melody
M:4/4
L:1/8
Q:1/4=120
K:C
CDEF GABc|cBAG FEDC|
```

### Accidentals

```abc
X:1
T:Accidentals Test
M:4/4
L:1/4
K:C
^C _D =E ^^F|__G ^A _B =c|
```

### Durations

```abc
X:1
T:Duration Test
M:4/4
L:1/8
K:C
C C2 C4 C/2|C3/2 C/ C C|
```

### Chords (simultaneous notes)

```abc
X:1
T:Chord Test
M:4/4
L:1/4
K:C
[CEG] [DFA] [EGB] [FAc]|[GBd]2 [ceg]2|
```

### Repeats

```abc
X:1
T:Repeat Test
M:4/4
L:1/4
K:G
|:GABc|dcBA:|
```

### Ties

```abc
X:1
T:Tie Test
M:4/4
L:1/4
K:C
c-c c-|c c2|
```

### Tuplets

```abc
X:1
T:Tuplet Test
M:4/4
L:1/8
K:C
(3cde (3fga|c4 z4|
```

### Multi-Voice

```abc
X:1
T:Two Voice Example
M:4/4
L:1/4
Q:1/4=120
V:1 name="Melody" clef=treble
V:2 name="Bass" clef=bass
K:C
V:1
cdef|gabc'|
V:2
C,G,C,G,|C,G,C,G,|
```

### Real Tune: Speed the Plough

```abc
X:1
T:Speed the Plough
M:4/4
C:Trad.
K:G
|:GABc dedB|dedB dedB|c2ec B2dB|c2A2 A2BA|
  GABc dedB|dedB dedB|c2ec B2dB|A2F2 G4:|
|:g2gf gdBd|g2f2 e2d2|c2ec B2dB|c2A2 A2df|
  g2gf g2Bd|g2f2 e2d2|c2ec B2dB|A2F2 G4:|
```

### Real Tune: Paddy O'Rafferty (with ornaments)

```abc
X:1
T:Paddy O'Rafferty
C:Trad.
O:Irish
R:Jig
M:6/8
K:D
dff cee|def gfe|dff cee|dfe dBA|
dff cee|def gfe|faf gfe|1 dfe dBA:|2 dfe dcB|]
~A3 B3|gfe fdB|AFA B2c|dfe dcB|
~A3 ~B3|efe efg|faf gfe|1 dfe dcB:|2 dfe dBA|]
fAA eAA|def gfe|fAA eAA|dfe dBA|
fAA eAA|def gfe|faf gfe|dfe dBA:|
```

### Chord Symbols and Annotations

```abc
X:1
T:Jericho
T:Joshua fought the battle of Jericho
C:Anon.
M:C
L:1/8
K:Dm
"Dm"D^CDE FF G2|"Dm"A A2 A-A4|"A7"G G2 G-G4|"Dm"A A2 A-A4|
"Dm"D^CDE FF G2|"Dm"A A2 A-A2 FG|"A7"A2 G2 F2 E2|"Dm"D6"^Fine"||dd|
"Dm"dA AA A3 A|"Dm"A A3- "A7"A2 AA|"Dm"AA AA A2 A2|"A7"A6 ^c2|
"Dm"d2 A2 "A7"A A3|"Dm"A2 A2- "A7"A2 AA|"Dm"AA G2 "A7"E2 D2|"Dm"D8|]
```

### Grace Notes and Decorations

```abc
X:1
T:Decoration Examples
M:6/8
L:1/8
K:D
{g}A3 A{g}AA|{gAGAG}A3 {g}A{d}A{e}A|
!trill!D3 .E.F.G|!fermata!A6|
!pp!DEF !crescendo(!GAB|!crescendo)!!ff!A3|
```

### Transposing Instrument (Tenor Sax)

```abc
X:1
T:Melody for Tenor Sax
M:4/4
L:1/8
%%MIDI transpose -14
%%MIDI program 67
K:C
CDEF GABc|cBAG FEDC|
```

### Three-Voice Harmony

```abc
X:1
T:Three Voice Test
C:Test
M:4/4
L:1/4
Q:1/4=120
V:1 name="Soprano" clef=treble
V:2 name="Alto" clef=treble
V:3 name="Bass" clef=bass
K:C
V:1
c'd'e'f'|g'a'b'c''|
V:2
cdef|gabc'|
V:3
C,D,E,F,|G,A,B,C|
```

---

## 21. Key Signature Cheat Sheet

Circle of fifths with ABC key names:

```
Flats:                          Sharps:
K:Cb (7b)  Cb Db Eb Fb Gb Ab Bb    K:C# (7#)  C# D# E# F# G# A# B#
K:Gb (6b)  Gb Ab Bb Cb Db Eb F     K:F# (6#)  F# G# A# B  C# D# E#
K:Db (5b)  Db Eb F  Gb Ab Bb C     K:B  (5#)  B  C# D# E  F# G# A#
K:Ab (4b)  Ab Bb C  Db Eb F  G     K:E  (4#)  E  F# G# A  B  C# D#
K:Eb (3b)  Eb F  G  Ab Bb C  D     K:A  (3#)  A  B  C# D  E  F# G#
K:Bb (2b)  Bb C  D  Eb F  G  A     K:D  (2#)  D  E  F# G  A  B  C#
K:F  (1b)  F  G  A  Bb C  D  E     K:G  (1#)  G  A  B  C  D  E  F#
                    K:C  (0)  C  D  E  F  G  A  B
```

---

## 22. MIDI Program Numbers (Common Instruments)

For use with `%%MIDI program N`:

| Number | Instrument | Number | Instrument |
|--------|-----------|--------|-----------|
| 0 | Acoustic Grand Piano | 40 | Violin |
| 1 | Bright Acoustic Piano | 41 | Viola |
| 4 | Electric Piano 1 | 42 | Cello |
| 6 | Harpsichord | 43 | Contrabass |
| 11 | Vibraphone | 56 | Trumpet |
| 13 | Xylophone | 57 | Trombone |
| 21 | Accordion | 60 | French Horn |
| 24 | Acoustic Guitar (nylon) | 64 | Soprano Sax |
| 25 | Acoustic Guitar (steel) | 65 | Alto Sax |
| 32 | Acoustic Bass | 66 | Tenor Sax |
| 33 | Electric Bass (finger) | 67 | Baritone Sax |
| 73 | Flute | 68 | Oboe |
| 71 | Clarinet | 74 | Recorder |
| 75 | Pan Flute | 109 | Bagpipe |
| 105 | Banjo | 110 | Fiddle |

---

## 23. Kaijutsu-ABC Public API

```rust
use kaijutsu_abc::{parse, to_midi, transpose, to_abc, semitones_to_key, MidiParams};
use kaijutsu_abc::ast::*;
use kaijutsu_abc::engrave::{engrave_to_svg, EngravingOptions};

// Parse ABC text
let result = parse("X:1\nT:Test\nK:C\nCDEF|");
let tune = result.value;
let feedback = result.feedback;  // Vec<Feedback> — errors, warnings, info

// Generate MIDI (SMF format 0)
let midi_bytes = to_midi(&tune, &MidiParams::default());

// Transpose
let transposed = transpose(&tune, 5);  // up 5 semitones

// Calculate transposition interval between keys
let semitones = semitones_to_key(&tune.header.key, "Am").unwrap();

// Round-trip back to ABC text
let abc_text = to_abc(&tune);

// Render to SVG
let svg = engrave_to_svg(&tune, &EngravingOptions::default());
```

Unknown `!name!` decorations are preserved as `Decoration::Other(String)` rather
than rejected, so the parser won't fail on decorations it doesn't specifically handle.

---

## 24. Tips for Writing ABC

1. **Start simple.** Get the melody right before adding ornaments and dynamics.
2. **Use L:1/8 for most tunes.** It keeps note syntax compact.
3. **Space for readability.** Spaces between note groups = beam breaks.
4. **Bar lines matter.** Accidentals reset at each `|`. Check your bars.
5. **Test with the parser.** kaijutsu-abc's generous parser will tell you about
   issues via feedback messages rather than failing outright.
6. **One voice at a time.** Define all voices in the header, then write each
   voice's music in sequence.

---

## References

- [ABC v2.1 Standard](https://abcnotation.com/wiki/abc:standard:v2.1) — the definitive specification
- [ABC v2.2 Draft](https://abcnotation.com/wiki/abc:standard:v2.2) — upcoming improvements
- [abcjs](https://github.com/paulrosen/abcjs) — the industry-standard JS parser/renderer
- [ABC Notation Examples](https://abcnotation.com/examples) — code + rendered output
- [Henrik Norbeck's BNF](https://web.archive.org/web/20120814155205/http://www.norbeck.nu/abc/bnf/abc20bnf.htm) — formal grammar
- [ABC Plus Project](http://abcplus.sourceforge.net/) — extended features documentation
