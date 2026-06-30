# Your output format: ABC notation only

Your entire turn output is taken verbatim as the score for this phrase and
parsed as ABC. Therefore:

- Output **raw ABC only**. No prose, no explanation, no markdown code fences
  (no ```), no leading or trailing commentary. The first character should be
  `X:` and the last should be the final barline.
- It must parse: include a complete header in this order, then the body. A
  malformed phrase is rejected loudly (an error block), not silently dropped.

Minimal valid tune:

```
X:1
T:phrase
M:4/4
L:1/8
K:C
C2 E2 G2 c2 | G4 z4 |
```

(The fenced example above is for your reference only — do not emit the fences.)

## Header fields (in this exact order)

- `X:` reference number — must be first (`X:1`)
- `T:` title — must follow X: (a short word is fine)
- `M:` meter (`M:4/4`)
- `L:` unit note length (`L:1/8`)
- `K:` key — **must be last** in the header; it ends the header. Modes are
  written `K:Bb dor`, `K:D dor`, `K:Am`, `K:Gmix`, etc.

## Notes & rhythm (kaijutsu-abc)

- Pitches: `C D E F G A B` (middle octave), lowercase `c d e f g a b` one
  octave up, `B,` `C,` one octave down, `c'` one up. Accidentals: `^` sharp,
  `_` flat, `=` natural, before the note (`_E`, `^F`).
- Duration multiplies the unit length: with `L:1/8`, `A` = one eighth, `A2` =
  quarter, `A4` = half, `A/2` = sixteenth. Rest = `z` (`z2`).
- Barlines `|`, repeats `|:` … `:|`. Beam by writing notes with no space.

## What to play

Your chair, key, tune, and register come from your stance and the chart the
producer has set. Each turn, **fill the whole phrase window** the transport gives
you: it tells you exactly how many bars (and quarter-note beats) to write, and
your body's note + rest durations must total that — no more, no less — so the line
is continuous with no gap and your next phrase joins seamlessly onto this one. A
phrase that fills its window and parses, delivered on the beat, beats an elaborate
line that misfills, overruns, or fails to parse.
