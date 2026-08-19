//! SGR (Select Graphic Rendition) parameters → semantic style state.
//!
//! This is the only place that knows what an SGR integer *means*. It holds no
//! text and no offsets: `SgrState` is the running style, and the span assembly
//! in [`crate::strip`] reads it whenever it emits a character.
//!
//! Colors stay **semantic** — `Indexed(n)` is a palette slot, never resolved
//! RGB — so a retheme changes terminal output without reprojecting anything.

use kaijutsu_types::{StyleAttrs, StyleColor};
use vte::{Params, ParamsIter};

/// The active graphic rendition: what the next printed character wears.
///
/// `Default` is "unstyled" — the state a block's ordinary text is in, and the
/// state for which no span is ever emitted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SgrState {
    pub fg: Option<StyleColor>,
    pub bg: Option<StyleColor>,
    pub attrs: StyleAttrs,
}

impl SgrState {
    /// True when nothing would be drawn differently — no span is emitted for
    /// runs in this state.
    pub(crate) fn is_default(self) -> bool {
        self.fg.is_none() && self.bg.is_none() && self.attrs.is_empty()
    }

    /// Apply one CSI ... `m` sequence's parameters.
    ///
    /// `CSI m` with no parameters is `CSI 0 m` (full reset), per ECMA-48.
    /// Unrecognized parameters are skipped rather than aborting the sequence:
    /// a program that emits SGR 53 (overline, unsupported here) between two
    /// colors still gets both colors.
    pub(crate) fn apply(&mut self, params: &Params) {
        if params.is_empty() {
            *self = SgrState::default();
            return;
        }

        let mut iter = params.iter();
        while let Some(group) = iter.next() {
            // A group is one parameter plus its colon-separated subparameters
            // (`38:2::255:0:0`). Empty parameters arrive as 0, which is also
            // ECMA-48's default — `CSI ;31m` really is reset-then-red.
            let param = group.first().copied().unwrap_or(0);
            match param {
                0 => *self = SgrState::default(),
                1 => self.attrs.insert(StyleAttrs::BOLD),
                2 => self.attrs.insert(StyleAttrs::DIM),
                3 => self.attrs.insert(StyleAttrs::ITALIC),
                // 4:0 turns underline off, 4:1..4:5 select a style we do not
                // model (single/double/curly/dotted/dashed) — all of which are
                // still "underlined" as far as a span is concerned.
                4 => {
                    if group.get(1) == Some(&0) {
                        self.attrs.remove(StyleAttrs::UNDERLINE);
                    } else {
                        self.attrs.insert(StyleAttrs::UNDERLINE);
                    }
                }
                // 5 slow blink, 6 rapid blink — one bit; the app picks a rate.
                5 | 6 => self.attrs.insert(StyleAttrs::BLINK),
                7 => self.attrs.insert(StyleAttrs::INVERSE),
                9 => self.attrs.insert(StyleAttrs::STRIKETHROUGH),
                // 21 is "doubly underlined" in ECMA-48 but "bold off" in the
                // overwhelming majority of emitters (and in ncurses' exit_bold
                // habits). Bold off is the reading that cannot corrupt a
                // projection: the worst case is a missing double underline we
                // do not model anyway.
                21 => self.attrs.remove(StyleAttrs::BOLD),
                22 => self.attrs.remove(StyleAttrs::BOLD | StyleAttrs::DIM),
                23 => self.attrs.remove(StyleAttrs::ITALIC),
                24 => self.attrs.remove(StyleAttrs::UNDERLINE),
                25 => self.attrs.remove(StyleAttrs::BLINK),
                27 => self.attrs.remove(StyleAttrs::INVERSE),
                29 => self.attrs.remove(StyleAttrs::STRIKETHROUGH),
                30..=37 => self.fg = Some(StyleColor::Indexed((param - 30) as u8)),
                38 => {
                    if let Some(color) = extended_color(group, &mut iter) {
                        self.fg = color;
                    }
                }
                39 => self.fg = None,
                40..=47 => self.bg = Some(StyleColor::Indexed((param - 40) as u8)),
                48 => {
                    if let Some(color) = extended_color(group, &mut iter) {
                        self.bg = color;
                    }
                }
                49 => self.bg = None,
                90..=97 => self.fg = Some(StyleColor::Indexed((param - 90 + 8) as u8)),
                100..=107 => self.bg = Some(StyleColor::Indexed((param - 100 + 8) as u8)),
                // Everything else — 10-20 fonts, 26 proportional spacing,
                // 51-55 frame/encircle/overline, 58-59 underline color, 73-75
                // super/subscript — is outside the 80/20 scope in
                // docs/ansi-and-beyond.md. Consumed, not modelled.
                //
                // 8 (conceal) and 28 (reveal) are deliberately in that bucket:
                // modelling conceal would let output hide text from the human
                // that the model still reads, which is exactly the divergence
                // stripping exists to prevent. Concealed text is projected
                // plainly, visible to everyone.
                _ => {}
            }
        }
    }
}

/// Parse the argument of SGR 38 / 48 (extended color) in either encoding.
///
/// Returns `None` when the sequence is truncated or unrecognized — a partial
/// `CSI 38;5 m` leaves the current color alone rather than inventing one.
/// `Some(None)` is the ITU "default color" subparameter (`38:0`).
///
/// Both encodings are live in the wild:
///
/// - **subparameter** (ITU T.416, colons): the whole color is one group, e.g.
///   `38:5:196` or `38:2::255:0:0` — the empty slot is a color space id.
/// - **parameter** (xterm, semicolons): the color continues into the following
///   groups, e.g. `38;5;196` or `38;2;255;0;0`.
fn extended_color(group: &[u16], iter: &mut ParamsIter<'_>) -> Option<Option<StyleColor>> {
    if group.len() > 1 {
        // Colon form: everything is right here.
        return match group[1] {
            0 => Some(None),
            5 => group.get(2).map(|&n| Some(StyleColor::Indexed(clamp_u8(n)))),
            2 => match group.len() {
                // `38:2:r:g:b` — no color space id.
                5 => Some(Some(StyleColor::Rgb(
                    clamp_u8(group[2]),
                    clamp_u8(group[3]),
                    clamp_u8(group[4]),
                ))),
                // `38:2::r:g:b` — color space id present (usually empty → 0)
                // and ignored; extra trailing subparameters (tolerance) too.
                n if n >= 6 => Some(Some(StyleColor::Rgb(
                    clamp_u8(group[3]),
                    clamp_u8(group[4]),
                    clamp_u8(group[5]),
                ))),
                _ => None,
            },
            _ => None,
        };
    }

    // Semicolon form: pull the arguments out of the parameter stream. A
    // truncated tail simply exhausts the iterator and yields `None`, which is
    // also how vte's parameter cap surfaces here.
    match next_param(iter)? {
        0 => Some(None),
        5 => Some(Some(StyleColor::Indexed(clamp_u8(next_param(iter)?)))),
        2 => {
            let r = clamp_u8(next_param(iter)?);
            let g = clamp_u8(next_param(iter)?);
            let b = clamp_u8(next_param(iter)?);
            Some(Some(StyleColor::Rgb(r, g, b)))
        }
        _ => None,
    }
}

/// The primary value of the next parameter group, if any.
fn next_param(iter: &mut ParamsIter<'_>) -> Option<u16> {
    iter.next().map(|group| group.first().copied().unwrap_or(0))
}

/// SGR values are 16-bit; a channel above 255 is malformed. Clamping keeps the
/// transform total (the alternative would be dropping an otherwise-good color).
fn clamp_u8(value: u16) -> u8 {
    value.min(u16::from(u8::MAX)) as u8
}
