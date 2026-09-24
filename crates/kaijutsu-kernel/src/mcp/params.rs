//! Schema-directed coercion for MCP tool call arguments.
//!
//! Some models encode integer-typed tool parameters as JSON strings
//! (`{"offset": "240"}`), which `serde_json::from_value` refuses.
//! [`decode_params`] reads `T`'s schemars schema and, for each top-level
//! property typed `integer` or `number`, replaces a string that parses
//! cleanly as that kind with a JSON number, then deserializes. schemars
//! renders an `Option<..>` field's type as an array (`["integer", "null"]`),
//! so both shapes count. A `string`-typed field is never touched, and a
//! string that does not parse (`"24x"`, `""`) still fails with
//! [`McpError::InvalidParams`].
//!
//! Only top-level properties are coerced; nested objects and arrays are not
//! walked. No params struct here nests a numeric field today.

use std::any::{TypeId, type_name};
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::error::{McpError, McpResult};

/// `T`'s schema, rendered to JSON exactly as `tool_def` in `servers/*.rs`
/// renders it for `input_schema`, cached per type so repeat calls for the
/// same params struct — the common case, one struct per tool called many
/// times — pay schema generation only once.
fn schema_for_cached<T: JsonSchema + 'static>() -> Value {
    static CACHE: OnceLock<RwLock<HashMap<TypeId, Value>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| RwLock::new(HashMap::new()));
    let id = TypeId::of::<T>();
    if let Some(v) = cache.read().expect("schema cache lock poisoned").get(&id) {
        return v.clone();
    }
    let schema = serde_json::to_value(schemars::schema_for!(T))
        .expect("a schemars-generated schema always serializes to JSON");
    cache
        .write()
        .expect("schema cache lock poisoned")
        .insert(id, schema.clone());
    schema
}

/// The JSON Schema `type` name(s) a property declares. schemars 1.2.2
/// renders a required field's type as a bare string (`"integer"`) and an
/// `Option<..>` field's as an array (`["integer", "null"]`); this reads
/// either shape uniformly.
fn declared_types(property_schema: &Value) -> Vec<&str> {
    match property_schema.get("type") {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// Parse `s` into a JSON number matching `property_schema`'s declared
/// numeric kind, or `None` if the schema isn't numeric-typed there, or `s`
/// doesn't parse cleanly as that kind. No trimming: `"240"` parses,
/// `"24x"` and `""` do not.
fn coerce_numeric_string(property_schema: &Value, s: &str) -> Option<Value> {
    let types = declared_types(property_schema);
    if types.contains(&"integer") {
        return s
            .parse::<i64>()
            .map(Value::from)
            .or_else(|_| s.parse::<u64>().map(Value::from))
            .ok();
    }
    if types.contains(&"number") {
        return s
            .parse::<f64>()
            .ok()
            .and_then(|f| serde_json::Number::from_f64(f).map(Value::Number));
    }
    None
}

/// Decode a tool call's `arguments` into `T`, first coercing any
/// string-encoded numeric top-level field per `T`'s own JSON schema.
/// Behaves exactly like
/// `serde_json::from_value(arguments).map_err(McpError::InvalidParams)`
/// otherwise — same error type, same failure on a string that isn't
/// cleanly numeric, same failure on any other shape mismatch.
pub fn decode_params<T: DeserializeOwned + JsonSchema + 'static>(
    mut arguments: Value,
) -> McpResult<T> {
    if let Value::Object(fields) = &mut arguments {
        let schema = schema_for_cached::<T>();
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (name, property_schema) in properties {
                let Some(value) = fields.get_mut(name) else {
                    continue;
                };
                let Value::String(s) = value else {
                    continue;
                };
                if let Some(coerced) = coerce_numeric_string(property_schema, s) {
                    tracing::debug!(
                        param_type = type_name::<T>(),
                        field = %name,
                        original = %s,
                        "coerced string-encoded numeric MCP tool argument"
                    );
                    *value = coerced;
                }
            }
        }
    }
    serde_json::from_value(arguments).map_err(McpError::InvalidParams)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Debug, Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct NumericProbe {
        required_usize: usize,
        #[serde(default)]
        optional_u32: Option<u32>,
        plain_string: String,
    }

    /// The concrete live defect: an `Option<u32>` field sent as a JSON
    /// string ("240") must decode to the number, not refuse.
    #[test]
    fn optional_numeric_string_coerces_to_a_number() {
        let v = decode_params::<NumericProbe>(json!({
            "required_usize": "7",
            "optional_u32": "240",
            "plain_string": "hello",
        }))
        .expect("a clean numeric string must coerce");
        assert_eq!(v.optional_u32, Some(240));
    }

    /// A required numeric field (not wrapped in `Option`) sent as a string
    /// must coerce the same way — the coercion is keyed off the schema's
    /// declared type, not off optionality.
    #[test]
    fn required_numeric_string_coerces_to_a_number() {
        let v = decode_params::<NumericProbe>(json!({
            "required_usize": "7",
            "plain_string": "x",
        }))
        .expect("a clean numeric string must coerce");
        assert_eq!(v.required_usize, 7);
    }

    /// A string that is not cleanly numeric must still fail loud with the
    /// same `InvalidParams` the caller got before this helper existed — no
    /// silent fallback, no partial coercion.
    #[test]
    fn a_non_numeric_string_in_a_numeric_field_still_fails_loud() {
        let err = decode_params::<NumericProbe>(json!({
            "required_usize": "7",
            "optional_u32": "24x",
            "plain_string": "hello",
        }))
        .expect_err("\"24x\" is not a clean u32 and must not silently become one");
        assert!(matches!(err, McpError::InvalidParams(_)));
    }

    /// An empty string in a numeric field is not coerced either — parsing
    /// "" as an integer fails, so this must reach the same refusal.
    #[test]
    fn empty_string_in_a_numeric_field_still_fails_loud() {
        let err = decode_params::<NumericProbe>(json!({
            "required_usize": "",
            "plain_string": "x",
        }))
        .expect_err("an empty string must not coerce to 0 or anything else");
        assert!(matches!(err, McpError::InvalidParams(_)));
    }

    /// The field the brief calls out by name: a `String`-typed field
    /// holding digits must stay a string. If this coerced, decoding would
    /// either fail (a `Number` can't deserialize into `String`) or — worse
    /// — silently change the value's type; asserting the round-tripped
    /// content proves neither happened.
    #[test]
    fn a_string_typed_field_holding_digits_is_never_coerced() {
        let v = decode_params::<NumericProbe>(json!({
            "required_usize": "7",
            "plain_string": "240",
        }))
        .expect("a string field holding digits is still valid input");
        assert_eq!(
            v.plain_string, "240",
            "a string-typed field must never be coerced to a number"
        );
    }

    /// A genuine JSON number (the common, well-behaved case) must keep
    /// working unchanged — this helper's coercion path only touches
    /// `Value::String`.
    #[test]
    fn a_real_json_number_still_decodes_unchanged() {
        let v = decode_params::<NumericProbe>(json!({
            "required_usize": 7,
            "optional_u32": 240,
            "plain_string": "hello",
        }))
        .expect("a real JSON number was always valid input");
        assert_eq!(v.required_usize, 7);
        assert_eq!(v.optional_u32, Some(240));
    }
}
