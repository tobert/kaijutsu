//! Math functions for the Rhai stdlib.
//!
//! 35+ functions covering trig, powers, interpolation, geometry, and conversion.
//! All operate on f64 (Rhai's native float type).

use rhai::Engine;

/// Register all math functions on a Rhai engine.
pub fn register(engine: &mut Engine) {
    // Trigonometry
    engine.register_fn("sin", |x: f64| x.sin());
    engine.register_fn("cos", |x: f64| x.cos());
    engine.register_fn("tan", |x: f64| x.tan());
    engine.register_fn("asin", |x: f64| x.asin());
    engine.register_fn("acos", |x: f64| x.acos());
    engine.register_fn("atan", |x: f64| x.atan());
    engine.register_fn("atan2", |y: f64, x: f64| y.atan2(x));

    // Basic math
    engine.register_fn("sqrt", |x: f64| x.sqrt());
    engine.register_fn("abs_f", |x: f64| x.abs());
    engine.register_fn("floor", |x: f64| x.floor());
    engine.register_fn("ceil", |x: f64| x.ceil());
    engine.register_fn("round", |x: f64| x.round());
    engine.register_fn("min_f", |a: f64, b: f64| a.min(b));
    engine.register_fn("max_f", |a: f64, b: f64| a.max(b));

    // Constants
    engine.register_fn("PI", || std::f64::consts::PI);
    engine.register_fn("TAU", || std::f64::consts::TAU);
    engine.register_fn("E", || std::f64::consts::E);

    // Powers, exponentials, logarithms
    engine.register_fn("pow", |base: f64, exp: f64| base.powf(exp));
    engine.register_fn("exp", |x: f64| x.exp());
    engine.register_fn("ln", |x: f64| x.ln());
    engine.register_fn("log2", |x: f64| x.log2());
    engine.register_fn("log10", |x: f64| x.log10());

    // Hyperbolic trig
    engine.register_fn("sinh", |x: f64| x.sinh());
    engine.register_fn("cosh", |x: f64| x.cosh());
    engine.register_fn("tanh", |x: f64| x.tanh());

    // Geometry / interpolation
    engine.register_fn("hypot", |x: f64, y: f64| x.hypot(y));
    engine.register_fn("lerp", |a: f64, b: f64, t: f64| a + (b - a) * t);
    engine.register_fn("clamp", |x: f64, min: f64, max: f64| x.clamp(min, max));
    engine.register_fn("degrees", |x: f64| x.to_degrees());
    engine.register_fn("radians", |x: f64| x.to_radians());

    // Numeric utilities
    engine.register_fn("fract", |x: f64| x.fract());
    engine.register_fn("signum", |x: f64| x.signum());
    engine.register_fn("rem_euclid", |x: f64, y: f64| x.rem_euclid(y));
    engine.register_fn("copysign", |x: f64, y: f64| x.copysign(y));
}

#[cfg(test)]
mod tests {
    use super::*;
    fn eval(expr: &str) -> f64 {
        let mut engine = Engine::new();
        register(&mut engine);
        engine.eval::<f64>(expr).unwrap()
    }

    #[test]
    fn trig_identities() {
        // sin²(x) + cos²(x) = 1
        let x: f64 = 1.23;
        let sin_x = x.sin();
        let cos_x = x.cos();
        assert!((sin_x * sin_x + cos_x * cos_x - 1.0).abs() < 1e-12);

        let result = eval("sin(1.23) * sin(1.23) + cos(1.23) * cos(1.23)");
        assert!((result - 1.0).abs() < 1e-12);
    }

    #[test]
    fn constants() {
        assert!((eval("PI()") - std::f64::consts::PI).abs() < 1e-15);
        assert!((eval("TAU()") - std::f64::consts::TAU).abs() < 1e-15);
        assert!((eval("E()") - std::f64::consts::E).abs() < 1e-15);
    }

    #[test]
    fn lerp_endpoints() {
        assert!((eval("lerp(10.0, 20.0, 0.0)") - 10.0).abs() < 1e-15);
        assert!((eval("lerp(10.0, 20.0, 1.0)") - 20.0).abs() < 1e-15);
        assert!((eval("lerp(10.0, 20.0, 0.5)") - 15.0).abs() < 1e-15);
    }

    #[test]
    fn clamp_bounds() {
        assert!((eval("clamp(-5.0, 0.0, 10.0)") - 0.0).abs() < 1e-15);
        assert!((eval("clamp(15.0, 0.0, 10.0)") - 10.0).abs() < 1e-15);
        assert!((eval("clamp(5.0, 0.0, 10.0)") - 5.0).abs() < 1e-15);
    }

    #[test]
    fn powers_and_logs() {
        assert!((eval("pow(2.0, 10.0)") - 1024.0).abs() < 1e-10);
        assert!((eval("ln(E())") - 1.0).abs() < 1e-15);
        assert!((eval("log2(8.0)") - 3.0).abs() < 1e-15);
        assert!((eval("log10(1000.0)") - 3.0).abs() < 1e-15);
    }

    #[test]
    fn degrees_radians_roundtrip() {
        assert!((eval("degrees(radians(45.0))") - 45.0).abs() < 1e-10);
    }

    #[test]
    fn hyperbolic() {
        // tanh(0) = 0
        assert!(eval("tanh(0.0)").abs() < 1e-15);
        // cosh(0) = 1
        assert!((eval("cosh(0.0)") - 1.0).abs() < 1e-15);
    }

    #[test]
    fn numeric_utils() {
        assert!((eval("fract(3.75)") - 0.75).abs() < 1e-15);
        assert!((eval("signum(-42.0)") - (-1.0)).abs() < 1e-15);
        assert!((eval("rem_euclid(-7.0, 4.0)") - 1.0).abs() < 1e-15);
    }
}
