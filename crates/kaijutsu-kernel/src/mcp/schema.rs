//! Shared JSON Schema generation for builtin MCP tool `input_schema`s.
//!
//! schemars 1.x renders an `Option<T>` field by generating `T`'s schema and
//! then admitting `null`: a bare `"type"` string becomes `["T", "null"]`, an
//! existing `"type"` array gains `"null"`, an `"enum"` gains `null`, and —
//! when `T`'s own schema is a `$ref`/`anyOf`/`oneOf`/`allOf`/`if` — the whole
//! schema is wrapped as `{"anyOf": [<original>, {"type": "null"}]}` (see
//! schemars' private `allow_null`, `_private/mod.rs`). A direct probe of
//! qwen3.8-flash showed that a `null` entry in a declared `type` makes it
//! send optional integers and tuples as JSON strings (`"240"`, `"[0, 400]"`)
//! every time; without it, real numbers and lists every time
//! (`docs/issues.md`, "Nullable schema types make qwen send strings").
//!
//! [`tool_input_schema`] generates `T`'s schema through a [`SchemaGenerator`]
//! carrying the [`StripNullable`] transform, which reverses each of those
//! `null` insertions after generation — walking `properties`, `items`,
//! `prefixItems`, `anyOf`/`oneOf`/`allOf`, and `$defs` recursively, so a
//! nested or `$ref`'d optional field is covered too. A field's optionality
//! still comes through its absence from `required`; only the `null` variant
//! a model could echo back is removed.

use std::any::TypeId;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use schemars::generate::{SchemaGenerator, SchemaSettings};
use schemars::transform::{Transform, transform_subschemas};
use schemars::{JsonSchema, Schema};
use serde_json::{Map, Value};

/// `T`'s JSON Schema, rendered for use as an MCP tool's `input_schema`, with
/// no `null` admitted anywhere a schemars `Option<_>` field would otherwise
/// carry one. Cached per type: builtin `list_tools` calls happen on every
/// binding resolution, and schema generation is pure per `T`.
pub fn tool_input_schema<T: JsonSchema + 'static>() -> Value {
    static CACHE: OnceLock<RwLock<HashMap<TypeId, Value>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| RwLock::new(HashMap::new()));
    let id = TypeId::of::<T>();
    if let Some(v) = cache.read().expect("schema cache lock poisoned").get(&id) {
        return v.clone();
    }

    let mut settings = SchemaSettings::draft2020_12();
    settings.transforms.push(Box::new(StripNullable));
    let mut generator = SchemaGenerator::new(settings);
    let schema = generator.root_schema_for::<T>();
    let value = serde_json::to_value(schema)
        .expect("a schemars-generated schema always serializes to JSON");

    cache
        .write()
        .expect("schema cache lock poisoned")
        .insert(id, value.clone());
    value
}

/// Reverses schemars' `allow_null` mutation on every subschema it reaches.
/// See the module doc for the three shapes it undoes.
#[derive(Debug, Clone)]
struct StripNullable;

impl Transform for StripNullable {
    fn transform(&mut self, schema: &mut Schema) {
        if let Some(obj) = schema.as_object_mut() {
            strip_null_type(obj);
            strip_null_enum(obj);
            collapse_nullable_any_of(obj);
        }
        transform_subschemas(self, schema);
    }
}

/// `{"type": ["T", "null"]}` → `{"type": "T"}` (or the narrowed array, if
/// `T` itself was already multi-typed). A bare `"type": "T"` string is
/// untouched — only `allow_null`'s array form is ever null-bearing.
fn strip_null_type(obj: &mut Map<String, Value>) {
    let Some(Value::Array(types)) = obj.get("type") else {
        return;
    };
    if !types.iter().any(|t| t.as_str() == Some("null")) {
        return;
    }
    let retained: Vec<Value> = types
        .iter()
        .filter(|t| t.as_str() != Some("null"))
        .cloned()
        .collect();
    match retained.len() {
        0 => {
            obj.remove("type");
        }
        1 => {
            obj.insert("type".to_string(), retained.into_iter().next().expect("len checked above"));
        }
        _ => {
            obj.insert("type".to_string(), Value::Array(retained));
        }
    }
}

/// `allow_null` also pushes a `null` onto a field's `"enum"` (or turns a
/// `"const"` into a two-entry `"enum"` including `null`) alongside the
/// `"type"` mutation above.
fn strip_null_enum(obj: &mut Map<String, Value>) {
    if let Some(Value::Array(values)) = obj.get_mut("enum") {
        values.retain(|v| !v.is_null());
    }
}

/// `{"anyOf": [<original>, {"type": "null"}]}` → `<original>`'s keys merged
/// into this object (the shape `allow_null` produces when `T`'s own schema
/// is a `$ref`/`anyOf`/`oneOf`/`allOf`/`if`, so it can't just gain `"null"`
/// in a `"type"` array). A sibling key already on this object — e.g. a
/// `description` the derive macro attached alongside the wrap — is kept
/// over the hoisted one.
fn collapse_nullable_any_of(obj: &mut Map<String, Value>) {
    let Some(Value::Array(variants)) = obj.get("anyOf") else {
        return;
    };
    if !variants.iter().any(is_null_marker) {
        return;
    }
    let remaining: Vec<Value> = variants.iter().filter(|v| !is_null_marker(v)).cloned().collect();
    obj.remove("anyOf");
    match remaining.len() {
        0 => {}
        1 => {
            if let Value::Object(inner) = remaining.into_iter().next().expect("len checked above") {
                for (k, v) in inner {
                    obj.entry(k).or_insert(v);
                }
            }
        }
        _ => {
            obj.insert("anyOf".to_string(), Value::Array(remaining));
        }
    }
}

/// schemars' own null marker for a nullable `anyOf` branch: exactly
/// `{"type": "null"}` (`simple_impl!(() => "null")`), never anything wider.
fn is_null_marker(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("null")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct Primitives {
        required: u32,
        #[serde(default)]
        optional_int: Option<u32>,
        #[serde(default)]
        optional_str: Option<String>,
        #[serde(default)]
        optional_tuple: Option<(u32, u32)>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct Inner {
        text: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct WithNestedOptional {
        /// carries through the collapse
        #[serde(default)]
        inner: Option<Inner>,
    }

    fn walk_no_null_type(value: &Value, path: String) {
        if let Value::Object(map) = value {
            if let Some(t) = map.get("type") {
                let bad = match t {
                    Value::String(s) => s == "null",
                    Value::Array(items) => items.iter().any(|v| v.as_str() == Some("null")),
                    _ => false,
                };
                assert!(!bad, "null-bearing type at {path}: {t}");
            }
            for (k, v) in map {
                walk_no_null_type(v, format!("{path}/{k}"));
            }
        } else if let Value::Array(items) = value {
            for (i, v) in items.iter().enumerate() {
                walk_no_null_type(v, format!("{path}[{i}]"));
            }
        }
    }

    #[test]
    fn optional_primitives_render_without_a_null_type() {
        let schema = tool_input_schema::<Primitives>();
        walk_no_null_type(&schema, "$".to_string());

        let props = schema.get("properties").and_then(Value::as_object).expect("properties");
        assert_eq!(props["optional_int"]["type"], Value::String("integer".to_string()));
        assert_eq!(props["optional_str"]["type"], Value::String("string".to_string()));
        assert_eq!(props["optional_tuple"]["type"], Value::String("array".to_string()));

        let required = schema.get("required").and_then(Value::as_array).cloned().unwrap_or_default();
        assert!(!required.iter().any(|v| v.as_str() == Some("optional_int")));
        assert!(required.iter().any(|v| v.as_str() == Some("required")));
    }

    /// A field typed as a nested struct is `$ref`'d, so schemars wraps its
    /// `Option<_>` in `anyOf` rather than widening `type`. The collapse must
    /// still land on a plain `$ref` with no `null` anywhere, `$defs`
    /// included, and the doc-comment description must survive the collapse.
    #[test]
    fn nested_ref_optional_collapses_to_a_plain_ref() {
        let schema = tool_input_schema::<WithNestedOptional>();
        walk_no_null_type(&schema, "$".to_string());

        let inner_prop = &schema["properties"]["inner"];
        assert!(inner_prop.get("$ref").is_some(), "expected a bare $ref, got {inner_prop}");
        assert!(inner_prop.get("anyOf").is_none(), "anyOf must be collapsed away");
        assert_eq!(inner_prop["description"], Value::String("carries through the collapse".to_string()));

        let required = schema.get("required").and_then(Value::as_array).cloned().unwrap_or_default();
        assert!(!required.iter().any(|v| v.as_str() == Some("inner")));
    }
}

/// Every builtin MCP tool's `input_schema`, walked as the actual kernel
/// registers and advertises them — not a hand-picked list of params structs,
/// so a new builtin server that bypasses [`tool_input_schema`] still fails
/// this. `docs/issues.md`, "Nullable schema types make qwen send strings".
#[cfg(test)]
mod builtin_tool_registry_tests {
    use std::sync::Arc;

    use serde_json::Value;

    use crate::kj::test_helpers::test_dispatcher;
    use crate::mcp::CallContext;
    use crate::mcp::types::KernelTool;

    /// Boots a real kernel, registers the full builtin MCP server set the
    /// way production does (`Kernel::register_builtin_mcp_servers`), and
    /// lists every tool every registered instance advertises — bypassing
    /// context bindings (`instances_snapshot`, not `list_visible_tools`),
    /// since this checks what the kernel is capable of advertising, not
    /// what one context is bound to.
    async fn all_builtin_tools() -> Vec<KernelTool> {
        let d = Arc::new(test_dispatcher().await);
        d.set_self_arc();
        let store = d.block_store().clone();
        let file_cache = d.kernel().file_cache().clone();
        d.kernel()
            .register_builtin_mcp_servers(store, file_cache, None, d.kernel_db().clone())
            .await
            .expect("register builtin mcp servers");

        let servers = d.kernel().broker().instances_snapshot().await;
        assert!(!servers.is_empty(), "the builtin registry must register at least one instance");

        let mut tools = Vec::new();
        for server in servers.values() {
            tools.extend(
                server
                    .list_tools(&CallContext::system())
                    .await
                    .expect("list_tools on a builtin server must not fail"),
            );
        }
        tools
    }

    /// Collects every `path` under `value` where a `"type"` key's value is,
    /// or contains, `"null"` — recursing into every object and array,
    /// `$defs` and `properties` included, so a `null` buried in a `$ref`'d
    /// subschema is still caught.
    fn null_type_paths(value: &Value, path: &str, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                if let Some(t) = map.get("type") {
                    let bad = match t {
                        Value::String(s) => s == "null",
                        Value::Array(items) => items.iter().any(|v| v.as_str() == Some("null")),
                        _ => false,
                    };
                    if bad {
                        out.push(format!("{path}/type = {t}"));
                    }
                }
                for (k, v) in map {
                    null_type_paths(v, &format!("{path}/{k}"), out);
                }
            }
            Value::Array(items) => {
                for (i, v) in items.iter().enumerate() {
                    null_type_paths(v, &format!("{path}[{i}]"), out);
                }
            }
            _ => {}
        }
    }

    /// The direct regression test for the finding: no builtin tool's
    /// `input_schema` may admit `null` in a declared `type`, anywhere in the
    /// schema tree. A qwen3.8-flash probe showed a model sends an optional
    /// integer or tuple as a JSON string whenever it sees `null` there.
    #[tokio::test]
    async fn no_builtin_tool_schema_admits_a_null_type() {
        let tools = all_builtin_tools().await;

        let mut offenders = Vec::new();
        for tool in &tools {
            let mut found = Vec::new();
            null_type_paths(&tool.input_schema, "$", &mut found);
            if !found.is_empty() {
                offenders.push(format!("{}.{}: {}", tool.instance, tool.name, found.join(", ")));
            }
        }
        assert!(
            offenders.is_empty(),
            "tools whose input_schema admits a null type:\n{}",
            offenders.join("\n")
        );
    }

    /// The concrete example the brief names: `builtin.file`'s `read` tool
    /// must still advertise `offset` as an optional integer property — in
    /// `properties`, absent from `required` — not merely null-free.
    #[tokio::test]
    async fn file_read_offset_is_optional_not_required() {
        let tools = all_builtin_tools().await;
        let read = tools
            .iter()
            .find(|t| t.instance.as_str() == "builtin.file" && t.name == "read")
            .expect("builtin.file's read tool must be registered");

        let props = read
            .input_schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("read's input_schema must have properties");
        assert!(props.contains_key("offset"), "offset must appear in properties: {props:?}");
        assert_eq!(props["offset"]["type"], Value::String("integer".to_string()));

        let required = read
            .input_schema
            .get("required")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert!(
            !required.iter().any(|v| v.as_str() == Some("offset")),
            "offset must not be required: {required:?}"
        );
    }
}
