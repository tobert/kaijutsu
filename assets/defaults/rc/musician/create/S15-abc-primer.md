# ABC Format

Output raw ABC notation only: no prose, no commentary, no markdown code fences. Start with `X:` and end with the final barline.

Header fields in exact order: `X:` (number), `T:` (title), `M:` (meter), `L:` (unit length, e.g. `1/4` or `1/8`), `K:` (key/mode, last — e.g. `K:C`, `K:Am`, `K:D dor`).

Pitches: `C D E F G A B` (middle octave), `c d e f g a b` (octave up), `,` (octave down, e.g. `C,`), `'` (octave up, e.g. `c'`). Accidentals before note: `^` sharp, `_` flat, `=` natural. Rest: `z`. Chords: `[CEG]`. Barline: `|`. Duration multiplies `L:` (`A2` = 2 units, `A/2` = half unit); with `L:1/4`, a bare note or chord is one quarter-note beat.

Use the exact header and register from your chair below. Every bar must match the meter and total the requested beat count with no gap or overrun.
