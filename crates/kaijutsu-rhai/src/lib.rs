//! Unified Rhai stdlib for kaijutsu.
//!
//! Provides math, color, format, output capture, and scope serialization
//! functions shared across all Rhai usage sites (kernel engine, app theme,
//! app keybindings, server config).
//!
//! # Usage
//!
//! ```rust,no_run
//! use rhai::Engine;
//! use kaijutsu_rhai::{register_stdlib, register_output_callbacks};
//!
//! let mut engine = Engine::new();
//! register_stdlib(&mut engine);
//! let collector = register_output_callbacks(&mut engine);
//! // Scripts can now use math, color, and svg() functions
//! ```

pub mod catalog;
pub mod color;
pub mod format;
pub mod math;
pub mod output;
pub mod scope;
pub mod theme;

pub use catalog::function_catalog;
pub use color::parse_hex;
pub use output::{OutputCollector, register_output_callbacks};
pub use scope::{dynamic_to_json, json_to_dynamic, scope_from_json, scope_to_json};

/// Register the full kaijutsu Rhai stdlib: math + color + format functions.
///
/// Does NOT set engine limits (caller's responsibility) or register
/// output callbacks (use [`register_output_callbacks`] separately).
pub fn register_stdlib(engine: &mut rhai::Engine) {
    math::register(engine);
    color::register(engine);
    format::register(engine);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_integration_math_color_svg() {
        let mut engine = rhai::Engine::new();
        register_stdlib(&mut engine);
        let collector = register_output_callbacks(&mut engine);

        let script = r##"
            let r = sin(PI() / 2.0);
            let bg = hex("#1a1b26");
            let accent = oklch(0.7, 0.15, 260.0);
            let mixed = color_mix(bg, accent, 0.5);
            let x = fmt_f(r, 1);
            svg("<svg><text>" + x + " " + mixed + "</text></svg>");
        "##;

        engine.eval::<()>(script).unwrap();

        let svg = collector.take_svg().unwrap();
        assert!(svg.starts_with("<svg>"));
        assert!(svg.contains("1.0")); // sin(pi/2) = 1.0
        assert!(svg.contains('#')); // color hex value
    }

    #[test]
    fn print_capture_with_stdlib() {
        let mut engine = rhai::Engine::new();
        register_stdlib(&mut engine);
        let collector = register_output_callbacks(&mut engine);

        engine.eval::<()>(r#"print("hello from rhai");"#).unwrap();

        let stdout = collector.take_stdout();
        assert!(stdout.contains("hello from rhai"));
    }

    #[test]
    fn stdlib_does_not_conflict_with_rhai_builtins() {
        let mut engine = rhai::Engine::new();
        register_stdlib(&mut engine);

        // Rhai's built-in abs() for integers should still work
        let result: i64 = engine.eval("(-42).abs()").unwrap();
        assert_eq!(result, 42);

        // Our abs_f() for floats
        let result: f64 = engine.eval("abs_f(-3.14)").unwrap();
        assert!((result - 3.14).abs() < 1e-10);
    }
}
