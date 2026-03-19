//! Perceptual color functions for the Rhai stdlib.
//!
//! All color functions return hex strings (`"#rrggbb"` or `"#rrggbbaa"`).
//! Built on the `palette` crate for perceptually uniform operations in Oklch/Oklab space.

use palette::{
    Clamp, FromColor, Hsl, IntoColor, Lighten, Mix, Oklch, ShiftHue, Srgb, Srgba, WithAlpha,
};
use rhai::{Engine, ImmutableString};

/// Format an Srgb as `"#rrggbb"`.
fn srgb_to_hex(c: Srgb<f32>) -> String {
    let r = (c.red.clamp(0.0, 1.0) * 255.0).round() as u8;
    let g = (c.green.clamp(0.0, 1.0) * 255.0).round() as u8;
    let b = (c.blue.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Format an Srgba as `"#rrggbbaa"`.
fn srgba_to_hex(c: Srgba<f32>) -> String {
    let r = (c.red.clamp(0.0, 1.0) * 255.0).round() as u8;
    let g = (c.green.clamp(0.0, 1.0) * 255.0).round() as u8;
    let b = (c.blue.clamp(0.0, 1.0) * 255.0).round() as u8;
    let a = (c.alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
}

/// Parse a hex color string (`"#RGB"`, `"#RRGGBB"`, or `"#RRGGBBAA"`) into Srgba.
///
/// Public so downstream crates can reuse hex parsing (e.g. theme loader).
pub fn parse_hex(hex: &str) -> Option<Srgba<f32>> {
    let hex = hex.trim_start_matches('#');
    match hex.len() {
        3 => {
            let r = u8::from_str_radix(&hex[0..1].repeat(2), 16).ok()?;
            let g = u8::from_str_radix(&hex[1..2].repeat(2), 16).ok()?;
            let b = u8::from_str_radix(&hex[2..3].repeat(2), 16).ok()?;
            Some(Srgba::new(r, g, b, 255u8).into_format())
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some(Srgba::new(r, g, b, 255u8).into_format())
        }
        8 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
            Some(Srgba::new(r, g, b, a).into_format())
        }
        _ => None,
    }
}

/// Convert Srgba to hex string, choosing `#rrggbb` or `#rrggbbaa` based on alpha.
fn to_hex_auto(c: Srgba<f32>) -> String {
    if c.alpha >= 1.0 {
        srgb_to_hex(Srgb::new(c.red, c.green, c.blue))
    } else {
        srgba_to_hex(c)
    }
}

/// Helper for color operations: parse hex, apply transform, return hex.
/// Returns an error string if parsing fails.
fn color_op(hex: &str, f: impl FnOnce(Srgba<f32>) -> Srgba<f32>) -> String {
    match parse_hex(hex) {
        Some(c) => to_hex_auto(f(c)),
        None => format!("!invalid color: {hex}"),
    }
}

/// Register all color functions on a Rhai engine.
pub fn register(engine: &mut Engine) {
    // --- hex passthrough (identity for new stdlib, replaces Array-returning hex) ---
    engine.register_fn("hex", |s: ImmutableString| -> String {
        // Validate and normalize: parse then re-emit
        match parse_hex(&s) {
            Some(c) => to_hex_auto(c),
            None => format!("!invalid color: {s}"),
        }
    });

    engine.register_fn("hexa", |s: ImmutableString, alpha: f64| -> String {
        match parse_hex(&s) {
            Some(mut c) => {
                c.alpha = alpha as f32;
                srgba_to_hex(c)
            }
            None => format!("!invalid color: {s}"),
        }
    });

    // --- Constructors ---

    engine.register_fn("rgb", |r: f64, g: f64, b: f64| -> String {
        srgb_to_hex(Srgb::new(
            r as f32 / 255.0,
            g as f32 / 255.0,
            b as f32 / 255.0,
        ))
    });

    engine.register_fn("rgba", |r: f64, g: f64, b: f64, a: f64| -> String {
        srgba_to_hex(Srgba::new(
            r as f32 / 255.0,
            g as f32 / 255.0,
            b as f32 / 255.0,
            a as f32,
        ))
    });

    engine.register_fn("hsl", |h: f64, s: f64, l: f64| -> String {
        let c: Srgb<f32> = Hsl::new(h as f32, s as f32 / 100.0, l as f32 / 100.0).into_color();
        srgb_to_hex(c)
    });

    engine.register_fn("hsla", |h: f64, s: f64, l: f64, a: f64| -> String {
        let c: Srgb<f32> = Hsl::new(h as f32, s as f32 / 100.0, l as f32 / 100.0).into_color();
        srgba_to_hex(c.with_alpha(a as f32))
    });

    // --- Oklch (perceptually uniform) ---

    engine.register_fn("oklch", |l: f64, c: f64, h: f64| -> String {
        let oklch = Oklch::new(l as f32, c as f32, h as f32);
        let rgb: Srgb<f32> = oklch.into_color();
        srgb_to_hex(rgb.clamp())
    });

    engine.register_fn("oklcha", |l: f64, c: f64, h: f64, a: f64| -> String {
        let oklch = Oklch::new(l as f32, c as f32, h as f32);
        let rgb: Srgb<f32> = oklch.into_color();
        srgba_to_hex(rgb.clamp().with_alpha(a as f32))
    });

    // --- Color operations ---

    engine.register_fn(
        "color_mix",
        |hex1: ImmutableString, hex2: ImmutableString, t: f64| -> String {
            let c1 = match parse_hex(&hex1) {
                Some(c) => c,
                None => return format!("!invalid color: {hex1}"),
            };
            let c2 = match parse_hex(&hex2) {
                Some(c) => c,
                None => return format!("!invalid color: {hex2}"),
            };
            let ok1: Oklch<f32> = Oklch::from_color(Srgb::from_color(c1));
            let ok2: Oklch<f32> = Oklch::from_color(Srgb::from_color(c2));
            let mixed = ok1.mix(ok2, t as f32);
            let rgb: Srgb<f32> = mixed.into_color();
            let a = c1.alpha + (c2.alpha - c1.alpha) * t as f32;
            if a < 1.0 {
                srgba_to_hex(rgb.clamp().with_alpha(a))
            } else {
                srgb_to_hex(rgb.clamp())
            }
        },
    );

    engine.register_fn(
        "color_lighten",
        |hex: ImmutableString, amount: f64| -> String {
            color_op(&hex, |c| {
                let oklch: Oklch<f32> = Oklch::from_color(Srgb::from_color(c));
                let lightened = oklch.lighten(amount as f32);
                let rgb: Srgb<f32> = lightened.into_color();
                rgb.clamp().with_alpha(c.alpha)
            })
        },
    );

    engine.register_fn(
        "color_darken",
        |hex: ImmutableString, amount: f64| -> String {
            color_op(&hex, |c| {
                let oklch: Oklch<f32> = Oklch::from_color(Srgb::from_color(c));
                let darkened = oklch.lighten(-(amount as f32));
                let rgb: Srgb<f32> = darkened.into_color();
                rgb.clamp().with_alpha(c.alpha)
            })
        },
    );

    engine.register_fn(
        "color_saturate",
        |hex: ImmutableString, amount: f64| -> String {
            color_op(&hex, |c| {
                let mut oklch: Oklch<f32> = Oklch::from_color(Srgb::from_color(c));
                oklch.chroma *= 1.0 + amount as f32;
                let rgb: Srgb<f32> = oklch.into_color();
                rgb.clamp().with_alpha(c.alpha)
            })
        },
    );

    engine.register_fn(
        "color_desaturate",
        |hex: ImmutableString, amount: f64| -> String {
            color_op(&hex, |c| {
                let mut oklch: Oklch<f32> = Oklch::from_color(Srgb::from_color(c));
                oklch.chroma *= (1.0 - amount as f32).max(0.0);
                let rgb: Srgb<f32> = oklch.into_color();
                rgb.clamp().with_alpha(c.alpha)
            })
        },
    );

    engine.register_fn(
        "hue_shift",
        |hex: ImmutableString, degrees: f64| -> String {
            color_op(&hex, |c| {
                let oklch: Oklch<f32> = Oklch::from_color(Srgb::from_color(c));
                let shifted = oklch.shift_hue(degrees as f32);
                let rgb: Srgb<f32> = shifted.into_color();
                rgb.clamp().with_alpha(c.alpha)
            })
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_hex tests ---

    #[test]
    fn parse_hex_rgb_short() {
        let c = parse_hex("#fff").unwrap();
        assert!((c.red - 1.0).abs() < 0.01);
        assert!((c.green - 1.0).abs() < 0.01);
        assert!((c.blue - 1.0).abs() < 0.01);
        assert!((c.alpha - 1.0).abs() < 0.01);
    }

    #[test]
    fn parse_hex_rrggbb() {
        let c = parse_hex("#1a1b26").unwrap();
        assert!((c.red - 26.0 / 255.0).abs() < 0.01);
        assert!((c.alpha - 1.0).abs() < 0.01);
    }

    #[test]
    fn parse_hex_rrggbbaa() {
        let c = parse_hex("#1a1b2680").unwrap();
        assert!((c.alpha - 128.0 / 255.0).abs() < 0.01);
    }

    #[test]
    fn parse_hex_invalid() {
        assert!(parse_hex("#gg").is_none());
        assert!(parse_hex("not-hex").is_none());
        assert!(parse_hex("#12345").is_none());
    }

    #[test]
    fn hex_roundtrip_6() {
        let original = "#7aa2f7";
        let parsed = parse_hex(original).unwrap();
        let back = srgb_to_hex(Srgb::new(parsed.red, parsed.green, parsed.blue));
        assert_eq!(back, original);
    }

    #[test]
    fn hex_roundtrip_8() {
        let original = "#7aa2f780";
        let parsed = parse_hex(original).unwrap();
        let back = srgba_to_hex(parsed);
        assert_eq!(back, original);
    }

    // --- Rhai integration tests ---

    fn eval(expr: &str) -> String {
        let mut engine = Engine::new();
        register(&mut engine);
        engine.eval::<String>(expr).unwrap()
    }

    #[test]
    fn hex_fn_passthrough() {
        assert_eq!(eval(r##"hex("#7aa2f7")"##), "#7aa2f7");
    }

    #[test]
    fn hex_fn_normalizes_short() {
        assert_eq!(eval(r##"hex("#fff")"##), "#ffffff");
    }

    #[test]
    fn hsl_known_values() {
        // Pure red: hsl(0, 100, 50)
        let result = eval("hsl(0.0, 100.0, 50.0)");
        assert_eq!(result, "#ff0000");

        // Pure blue: hsl(240, 100, 50)
        let result = eval("hsl(240.0, 100.0, 50.0)");
        assert_eq!(result, "#0000ff");
    }

    #[test]
    fn oklch_gamut_clamp() {
        // High chroma should be clamped to valid sRGB
        let result = eval("oklch(0.5, 0.4, 120.0)");
        // Should not start with "!" (error marker)
        assert!(result.starts_with('#'));
        assert!(result.len() == 7); // #rrggbb, no alpha needed
    }

    #[test]
    fn color_mix_endpoints() {
        let red = "#ff0000";
        let blue = "#0000ff";

        // t=0 → first color
        let result = eval(&format!(r#"color_mix("{red}", "{blue}", 0.0)"#));
        assert_eq!(result, red);

        // t=1 → second color
        let result = eval(&format!(r#"color_mix("{red}", "{blue}", 1.0)"#));
        assert_eq!(result, blue);
    }

    #[test]
    fn lighten_darken_approximate_inverse() {
        let original = "#7aa2f7";
        let lightened = eval(&format!(r#"color_lighten("{original}", 0.1)"#));
        let back = eval(&format!(r#"color_darken("{lightened}", 0.1)"#));
        // Not exact inverse but should be close
        let orig = parse_hex(original).unwrap();
        let roundtrip = parse_hex(&back).unwrap();
        // Oklch lighten/darken aren't exact inverses — generous tolerance
        assert!((orig.red - roundtrip.red).abs() < 0.15);
        assert!((orig.green - roundtrip.green).abs() < 0.15);
        assert!((orig.blue - roundtrip.blue).abs() < 0.15);
    }

    #[test]
    fn hue_shift_360_identity() {
        let original = "#7aa2f7";
        let shifted = eval(&format!(r#"hue_shift("{original}", 360.0)"#));
        assert_eq!(shifted, original);
    }

    #[test]
    fn rgb_css_convention() {
        // 0-255 values
        assert_eq!(eval("rgb(255.0, 0.0, 0.0)"), "#ff0000");
        assert_eq!(eval("rgb(0.0, 255.0, 0.0)"), "#00ff00");
    }

    #[test]
    fn hexa_sets_alpha() {
        let result = eval(r##"hexa("#ff0000", 0.5)"##);
        assert!(result.starts_with("#ff0000"));
        // Should have alpha appended
        assert_eq!(result.len(), 9); // #rrggbbaa
    }
}
