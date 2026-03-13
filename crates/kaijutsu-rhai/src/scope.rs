//! Scope ↔ JSON serialization for Rhai.
//!
//! Enables persisting Rhai variable state across executions.

use rhai::{Dynamic, Scope};

/// Deserialize a JSON string into a Rhai Scope.
///
/// Each key-value pair in the JSON object becomes a scope variable.
/// Constants are not restored (they should be injected by the caller).
pub fn scope_from_json(json: &str) -> Scope<'static> {
    let mut scope = Scope::new();
    if let Ok(map) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(json) {
        for (key, value) in map {
            let dynamic = json_to_dynamic(&value);
            scope.push_dynamic(key, dynamic);
        }
    }
    scope
}

/// Serialize a Rhai Scope to a JSON string, skipping constants.
pub fn scope_to_json(scope: &Scope) -> String {
    let mut map = serde_json::Map::new();
    for (name, is_constant, value) in scope.iter() {
        if is_constant {
            continue;
        }
        map.insert(name.to_string(), dynamic_to_json(&value));
    }
    serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string())
}

/// Convert a `serde_json::Value` to a Rhai `Dynamic`.
pub fn json_to_dynamic(value: &serde_json::Value) -> Dynamic {
    match value {
        serde_json::Value::Null => Dynamic::UNIT,
        serde_json::Value::Bool(b) => Dynamic::from(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Dynamic::from(i)
            } else if let Some(f) = n.as_f64() {
                Dynamic::from(f)
            } else {
                Dynamic::UNIT
            }
        }
        serde_json::Value::String(s) => Dynamic::from(s.clone()),
        serde_json::Value::Array(arr) => {
            let items: Vec<Dynamic> = arr.iter().map(json_to_dynamic).collect();
            Dynamic::from(items)
        }
        serde_json::Value::Object(obj) => {
            let mut map = rhai::Map::new();
            for (k, v) in obj {
                map.insert(k.clone().into(), json_to_dynamic(v));
            }
            Dynamic::from(map)
        }
    }
}

/// Convert a Rhai `Dynamic` to a `serde_json::Value`.
pub fn dynamic_to_json(value: &Dynamic) -> serde_json::Value {
    if value.is_unit() {
        serde_json::Value::Null
    } else if let Ok(b) = value.as_bool() {
        serde_json::Value::Bool(b)
    } else if let Ok(i) = value.as_int() {
        serde_json::Value::Number(i.into())
    } else if let Ok(f) = value.as_float() {
        serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null)
    } else if let Ok(s) = value.clone().into_string() {
        serde_json::Value::String(s)
    } else if value.is_array() {
        if let Ok(arr) = value.clone().into_typed_array::<Dynamic>() {
            serde_json::Value::Array(arr.iter().map(dynamic_to_json).collect())
        } else {
            serde_json::Value::Null
        }
    } else if value.is_map() {
        if let Some(map) = value.clone().try_cast::<rhai::Map>() {
            let obj: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| (k.to_string(), dynamic_to_json(v)))
                .collect();
            serde_json::Value::Object(obj)
        } else {
            serde_json::Value::Null
        }
    } else {
        // Fall back to string representation
        serde_json::Value::String(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_roundtrip_primitives() {
        let json = r#"{"a":1,"b":2.5,"c":"hello","d":true,"e":null}"#;
        let scope = scope_from_json(json);
        let back = scope_to_json(&scope);
        let parsed: serde_json::Value = serde_json::from_str(&back).unwrap();

        assert_eq!(parsed["a"], 1);
        assert_eq!(parsed["b"], 2.5);
        assert_eq!(parsed["c"], "hello");
        assert_eq!(parsed["d"], true);
        assert!(parsed["e"].is_null());
    }

    #[test]
    fn json_roundtrip_nested() {
        let json = r#"{"arr":[1,2,3],"obj":{"x":10}}"#;
        let scope = scope_from_json(json);
        let back = scope_to_json(&scope);
        let parsed: serde_json::Value = serde_json::from_str(&back).unwrap();

        assert_eq!(parsed["arr"], serde_json::json!([1, 2, 3]));
        assert_eq!(parsed["obj"]["x"], 10);
    }

    #[test]
    fn constants_skipped() {
        let mut scope = Scope::new();
        scope.push("mutable_var", 42_i64);
        scope.push_constant("WIDTH", 800_i64);
        scope.push_constant("HEIGHT", 600_i64);

        let json = scope_to_json(&scope);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["mutable_var"], 42);
        assert!(parsed.get("WIDTH").is_none());
        assert!(parsed.get("HEIGHT").is_none());
    }

    #[test]
    fn empty_and_invalid_json() {
        let scope = scope_from_json("{}");
        assert_eq!(scope_to_json(&scope), "{}");

        let scope = scope_from_json("not valid json");
        assert_eq!(scope_to_json(&scope), "{}");
    }
}
