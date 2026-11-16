# Output format: ABC notation only

Each turn's output is taken verbatim as the score for this phrase and parsed as
ABC.

- Emit **raw ABC only**. Write no prose, no explanation, no markdown code fences
  (no ```), and no leading or trailing commentary. Start with `X:` and end with
  the final barline.
- Write a complete header in the order below, then the body. A malformed phrase
  is rejected as an error block.

Minimal valid tune:

```
X:1
T:phrase
M:4/4
L:1/4
K:C
C E G c | G E C C |
```

With `L:1/4`, a bare note letter is one beat. Write one note per beat and four
per `4/4` bar, with no length numbers.

(The fenced example above is for reference only; do not emit the fences.)

## Header fields (in this exact order)

- `X:` reference number, first (`X:1`)
- `T:` title, second (`T:phrase`)
- `M:` meter (`M:4/4`)
- `L:` unit note length (`L:1/4` or `L:1/8`)
- `K:` key, **last** in the header. Modes are written `K:Bb dor`, `K:D dor`,
  `K:Am`, `K:Gmix`, etc.

## Notes and rhythm (kaijutsu-abc)

- Pitches: `C D E F G A B` (middle octave), lowercase `c d e f g a b` one
  octave up, `B,` `C,` one octave down, `c'` one up. Accidentals: `^` sharp,
  `_` flat, `=` natural, before the note (`_E`, `^F`).
- Duration multiplies the unit length: with `L:1/8`, `A` = one eighth, `A2` =
  quarter, `A4` = half, `A/2` = sixteenth. Rest = `z` (`z2`).
- Barlines `|`, repeats `|:` ... `:|`. Beam by writing notes with no space.

## What to play

The header, key, and register come from our chair. Each turn, **fill the whole
phrase window** the transport gives us: it states how many bars and quarter-note
beats to write, and the body's note and rest durations must total that count
with no gap or overrun. A phrase that fills its window and parses on the beat
beats an elaborate line that misfills, overruns, or fails to parse.
