//! Static function metadata catalog for LLM system prompts.
//!
//! Provides introspectable metadata about all stdlib functions so agents
//! know what's available when writing Rhai scripts.

/// Returns a JSON value describing all stdlib functions.
///
/// Structure: `{"functions": [{"name", "sig", "doc"}, ...]}`.
/// Suitable for embedding in LLM system prompts or tool descriptions.
pub fn function_catalog() -> serde_json::Value {
    serde_json::json!({
        "functions": [
            // Output
            { "name": "svg",     "sig": "svg(content: string)",     "doc": "Set SVG content output. Call once per execution." },
            { "name": "print",   "sig": "print(value)",             "doc": "Print to stdout (captured in output)." },

            // Math — trig
            { "name": "sin",     "sig": "sin(x: f64) -> f64",      "doc": "Sine." },
            { "name": "cos",     "sig": "cos(x: f64) -> f64",      "doc": "Cosine." },
            { "name": "tan",     "sig": "tan(x: f64) -> f64",      "doc": "Tangent." },
            { "name": "asin",    "sig": "asin(x: f64) -> f64",     "doc": "Arc sine." },
            { "name": "acos",    "sig": "acos(x: f64) -> f64",     "doc": "Arc cosine." },
            { "name": "atan",    "sig": "atan(x: f64) -> f64",     "doc": "Arc tangent." },
            { "name": "atan2",   "sig": "atan2(y: f64, x: f64) -> f64", "doc": "Two-argument arc tangent." },

            // Math — basic
            { "name": "sqrt",    "sig": "sqrt(x: f64) -> f64",     "doc": "Square root." },
            { "name": "abs_f",   "sig": "abs_f(x: f64) -> f64",    "doc": "Absolute value." },
            { "name": "floor",   "sig": "floor(x: f64) -> f64",    "doc": "Floor." },
            { "name": "ceil",    "sig": "ceil(x: f64) -> f64",     "doc": "Ceiling." },
            { "name": "round",   "sig": "round(x: f64) -> f64",    "doc": "Round to nearest integer." },
            { "name": "min_f",   "sig": "min_f(a: f64, b: f64) -> f64", "doc": "Minimum of two floats." },
            { "name": "max_f",   "sig": "max_f(a: f64, b: f64) -> f64", "doc": "Maximum of two floats." },

            // Math — constants
            { "name": "PI",      "sig": "PI() -> f64",             "doc": "Returns \u{03c0} (3.14159...)." },
            { "name": "TAU",     "sig": "TAU() -> f64",            "doc": "Returns \u{03c4} (6.28318...)." },
            { "name": "E",       "sig": "E() -> f64",              "doc": "Returns Euler's number e (2.71828...)." },

            // Math — powers/logs
            { "name": "pow",     "sig": "pow(base: f64, exp: f64) -> f64", "doc": "Exponentiation (base^exp)." },
            { "name": "exp",     "sig": "exp(x: f64) -> f64",     "doc": "e^x." },
            { "name": "ln",      "sig": "ln(x: f64) -> f64",      "doc": "Natural logarithm." },
            { "name": "log2",    "sig": "log2(x: f64) -> f64",    "doc": "Base-2 logarithm." },
            { "name": "log10",   "sig": "log10(x: f64) -> f64",   "doc": "Base-10 logarithm." },

            // Math — hyperbolic
            { "name": "sinh",    "sig": "sinh(x: f64) -> f64",    "doc": "Hyperbolic sine." },
            { "name": "cosh",    "sig": "cosh(x: f64) -> f64",    "doc": "Hyperbolic cosine." },
            { "name": "tanh",    "sig": "tanh(x: f64) -> f64",    "doc": "Hyperbolic tangent." },

            // Math — geometry/interpolation
            { "name": "hypot",   "sig": "hypot(x: f64, y: f64) -> f64", "doc": "Hypotenuse \u{221a}(x\u{00b2}+y\u{00b2}), avoids overflow." },
            { "name": "lerp",    "sig": "lerp(a: f64, b: f64, t: f64) -> f64", "doc": "Linear interpolation: a + (b-a)*t." },
            { "name": "clamp",   "sig": "clamp(x: f64, min: f64, max: f64) -> f64", "doc": "Clamp x to [min, max]." },
            { "name": "degrees", "sig": "degrees(x: f64) -> f64", "doc": "Radians to degrees." },
            { "name": "radians", "sig": "radians(x: f64) -> f64", "doc": "Degrees to radians." },

            // Math — numeric utils
            { "name": "fract",   "sig": "fract(x: f64) -> f64",   "doc": "Fractional part of x." },
            { "name": "signum",  "sig": "signum(x: f64) -> f64",  "doc": "Sign: -1.0, 0.0, or 1.0." },
            { "name": "rem_euclid", "sig": "rem_euclid(x: f64, y: f64) -> f64", "doc": "Always-positive remainder (modulo)." },
            { "name": "copysign","sig": "copysign(x: f64, y: f64) -> f64", "doc": "x with the sign of y." },

            // Format/conversion
            { "name": "to_float","sig": "to_float(x: i64) -> f64", "doc": "Integer to float." },
            { "name": "to_int",  "sig": "to_int(x: f64) -> i64",  "doc": "Float to integer (truncates toward zero)." },
            { "name": "xml_escape", "sig": "xml_escape(s: string) -> string", "doc": "Escape &, <, >, \", ' for XML/SVG." },
            { "name": "fmt_f", "sig": "fmt_f(x: f64, decimals: i64) -> string", "doc": "Format float to N decimal places." },

            // Color — constructors
            { "name": "hex",    "sig": "hex(s: string) -> string", "doc": "Validate and normalize a hex color. Returns \"#rrggbb\" or \"#rrggbbaa\"." },
            { "name": "hexa",   "sig": "hexa(s: string, alpha: f64) -> string", "doc": "Hex color with explicit alpha." },
            { "name": "rgb",    "sig": "rgb(r: f64, g: f64, b: f64) -> string", "doc": "RGB to hex. 0-255 per channel." },
            { "name": "rgba",   "sig": "rgba(r: f64, g: f64, b: f64, a: f64) -> string", "doc": "RGB+alpha to hex. a=0.0-1.0." },
            { "name": "hsl",    "sig": "hsl(h: f64, s: f64, l: f64) -> string", "doc": "HSL to hex. h=0-360, s=0-100, l=0-100." },
            { "name": "hsla",   "sig": "hsla(h: f64, s: f64, l: f64, a: f64) -> string", "doc": "HSL+alpha to hex." },
            { "name": "oklch",  "sig": "oklch(l: f64, c: f64, h: f64) -> string", "doc": "Oklch to hex. Perceptually uniform. l=0-1, c=0-0.4, h=0-360." },
            { "name": "oklcha", "sig": "oklcha(l: f64, c: f64, h: f64, a: f64) -> string", "doc": "Oklch+alpha to hex." },

            // Color — operations
            { "name": "color_mix", "sig": "color_mix(hex1: string, hex2: string, t: f64) -> string", "doc": "Mix two hex colors in Oklab space. t=0.0\u{2192}hex1, t=1.0\u{2192}hex2." },
            { "name": "color_lighten", "sig": "color_lighten(hex: string, amount: f64) -> string", "doc": "Lighten a hex color in Oklch. amount=0.0-1.0." },
            { "name": "color_darken",  "sig": "color_darken(hex: string, amount: f64) -> string",  "doc": "Darken a hex color in Oklch. amount=0.0-1.0." },
            { "name": "color_saturate", "sig": "color_saturate(hex: string, amount: f64) -> string", "doc": "Increase chroma of a hex color." },
            { "name": "color_desaturate", "sig": "color_desaturate(hex: string, amount: f64) -> string", "doc": "Decrease chroma of a hex color." },
            { "name": "hue_shift", "sig": "hue_shift(hex: string, degrees: f64) -> string", "doc": "Shift hue of a hex color by degrees in Oklch." },
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_valid_json() {
        let catalog = function_catalog();
        // Should serialize without error
        let json = serde_json::to_string_pretty(&catalog).unwrap();
        assert!(json.contains("sin"));
        assert!(json.contains("oklch"));
        assert!(json.contains("color_mix"));
    }

    #[test]
    fn catalog_contains_expected_functions() {
        let catalog = function_catalog();
        let functions = catalog["functions"].as_array().unwrap();
        let names: Vec<&str> = functions
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();

        // Spot-check key functions exist
        assert!(names.contains(&"sin"));
        assert!(names.contains(&"cos"));
        assert!(names.contains(&"PI"));
        assert!(names.contains(&"lerp"));
        assert!(names.contains(&"hex"));
        assert!(names.contains(&"oklch"));
        assert!(names.contains(&"color_mix"));
        assert!(names.contains(&"svg"));
        assert!(names.contains(&"xml_escape"));
        assert!(names.contains(&"to_float"));
    }
}
