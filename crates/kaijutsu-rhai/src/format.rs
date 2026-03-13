//! String/number formatting and conversion functions for the Rhai stdlib.

use rhai::{Engine, ImmutableString};

/// Register format and conversion functions on a Rhai engine.
pub fn register(engine: &mut Engine) {
    engine.register_fn("to_float", |x: i64| x as f64);
    engine.register_fn("to_int", |x: f64| x as i64);

    engine.register_fn("xml_escape", |s: ImmutableString| -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    });

    engine.register_fn("fmt_f", |x: f64, decimals: i64| -> String {
        format!("{:.prec$}", x, prec = decimals.max(0) as usize)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_float_and_back() {
        let mut engine = Engine::new();
        register(&mut engine);

        let result: f64 = engine.eval("to_float(42)").unwrap();
        assert!((result - 42.0).abs() < f64::EPSILON);

        let result: i64 = engine.eval("to_int(3.14)").unwrap();
        assert_eq!(result, 3);
    }

    #[test]
    fn xml_escape_all_entities() {
        let mut engine = Engine::new();
        register(&mut engine);

        let result: String = engine.eval(r#"xml_escape("<b>\"hello\" & 'world'</b>")"#).unwrap();
        assert_eq!(result, "&lt;b&gt;&quot;hello&quot; &amp; &apos;world&apos;&lt;/b&gt;");
    }

    #[test]
    fn fmt_f_precision() {
        let mut engine = Engine::new();
        register(&mut engine);

        let result: String = engine.eval("fmt_f(3.14159, 2)").unwrap();
        assert_eq!(result, "3.14");

        let result: String = engine.eval("fmt_f(3.14159, 0)").unwrap();
        assert_eq!(result, "3");
    }
}
