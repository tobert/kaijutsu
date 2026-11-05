//! What `kj fork --drop` removes from the child's copy of a conversation.
//!
//! A drop keeps every turn's shape. Images and thinking blocks leave the
//! child, except thinking in the conversation's last turn. A tool result
//! stays, paired with its call, but loses its bulk. A shell result keeps the
//! bracketed fact lines a model reads (`[exit 1]`, `[cwd now ...]`, small
//! `[data]`), with one `[dropped: ...]` line in place of its output. A JSON
//! result keeps its small fields and names the fields it lost under
//! `dropped`. Any other result becomes one line. Each names the source block,
//! which keeps the original. Nothing is cut partway: each field is kept whole
//! or dropped whole.

use std::collections::HashSet;

use kaijutsu_types::{BlockKind, BlockSnapshot, ContentType, Role};
use serde_json::{Map, Value};

/// The kinds of content a fork drops (`kj fork --drop`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DropSpec {
    /// Asset image blocks.
    pub images: bool,
    /// Thinking blocks, except those after the last person's text: that
    /// turn may be an open tool loop that must send them back.
    pub thinking: bool,
    /// The bulk of every tool result.
    pub tool_results: bool,
    /// The bulk of results whose call has one of these tool names. An
    /// `mcp:<server>` entry resolves to its server's tool names.
    pub tool_names: HashSet<String>,
    /// A result whose text is at most this many bytes stays whole; a larger
    /// JSON result keeps its smallest fields up to this total.
    pub limit: usize,
}

/// What a fork dropped, for the note on the child's fork marker.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DropReport {
    pub images: usize,
    pub thinking: usize,
    pub tool_results: usize,
    /// Bytes the slimmed results no longer send.
    pub bytes: usize,
}

impl DropReport {
    /// One line naming what was dropped, or `None` when nothing was.
    pub fn note(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.tool_results > 0 {
            parts.push(format!("{} tool result bodies ({} bytes)", self.tool_results, self.bytes));
        }
        if self.images > 0 {
            parts.push(format!("{} images", self.images));
        }
        if self.thinking > 0 {
            parts.push(format!("{} thinking blocks", self.thinking));
        }
        (!parts.is_empty()).then(|| {
            format!(
                "dropped {}; the source context keeps the originals, and each stub names \
                 its block for `kj block read`",
                parts.join(", ")
            )
        })
    }
}

impl DropSpec {
    /// Whether the block leaves the child entirely.
    pub fn drops_block(&self, snap: &BlockSnapshot) -> bool {
        (self.images && snap.role == Role::Asset && snap.content_type == ContentType::Image)
            || (self.thinking && snap.kind == BlockKind::Thinking)
    }

    /// Whether a tool result whose call is named `tool_name` is slimmed.
    pub fn slims_result_of(&self, tool_name: Option<&str>) -> bool {
        self.tool_results || tool_name.is_some_and(|name| self.tool_names.contains(name))
    }
}

/// The command that reads a dropped block's original text.
fn read_command(source_key: &str) -> String {
    format!("kj block read {source_key}")
}

/// Slim a tool result in place. Returns the bytes no longer sent, or `None`
/// when the result already fits within `limit`, or is already a stub, and is
/// left alone.
///
/// A shell result is rebuilt from its stored envelope with
/// `ShellEnvelope::model_text`, so its fact lines stay; `data` over `limit`
/// is dropped too. A JSON object keeps its fields, smallest first, while they
/// fit within `limit`; the rest are named under `dropped` with their sizes
/// and the command that reads the original. Any other text becomes one line.
///
/// A result no model turn sent keeps `model_content` empty and a `stderr`
/// within `limit`, so hydration still joins its streams and folds in its
/// Error child. The people-facing `content` shows the stub, and the
/// rendering, style spans and content type are reset to match it.
pub fn slim_tool_result(snap: &mut BlockSnapshot, limit: usize, source_key: &str) -> Option<usize> {
    let sent = crate::llm::hydrate::model_tool_result_text(snap, &snap.content);
    if sent.len() <= limit && snap.content.len() <= limit {
        return None;
    }
    let read = read_command(source_key);
    let envelope = snap
        .shell_envelope
        .as_deref()
        .and_then(|json| serde_json::from_str::<kaijutsu_types::shell_envelope::ShellEnvelope>(json).ok());
    let slim = if let Some(mut envelope) = envelope {
        let mut what = format!("{} bytes of output", snap.content.len());
        if let Some(data) = envelope.data.as_ref().map(Value::to_string)
            && data.len() > limit
        {
            what.push_str(&format!(" and {} bytes of data", data.len()));
            envelope.data = None;
        }
        let slim = envelope.model_text(&format!("[dropped: {what}; read with: {read}]"));
        snap.shell_envelope = serde_json::to_string(&envelope).ok();
        slim
    } else {
        match serde_json::from_str::<Value>(&sent) {
            Ok(Value::Object(fields)) if fields.contains_key("dropped") => return None,
            Ok(Value::Object(fields)) => slim_object(fields, limit, &read),
            _ => format!("[dropped: {} bytes; read with: {read}]", sent.len()),
        }
    };
    if snap.model_content.is_some() {
        snap.model_content = Some(slim.clone());
        snap.stderr = None;
    } else if snap.stderr.as_ref().is_some_and(|stderr| stderr.len() > limit) {
        snap.stderr = None;
    }
    snap.content = slim;
    snap.output = None;
    snap.style_spans.clear();
    snap.content_type = ContentType::Plain;
    if snap.provenance.is_some() {
        snap.edited_since_ingest = true;
    }
    let now_sent = crate::llm::hydrate::model_tool_result_text(snap, &snap.content);
    Some(sent.len().saturating_sub(now_sent.len()))
}

/// Keep the smallest fields that fit within `limit`, in their original order,
/// and name the others under `dropped`. Each kept field counts as it
/// serializes, `"name":value,`.
fn slim_object(fields: Map<String, Value>, limit: usize, read: &str) -> String {
    let sizes: Vec<(String, usize)> = fields.iter().map(|(k, v)| (k.clone(), value_size(v))).collect();
    let mut by_size: Vec<&(String, usize)> = sizes.iter().collect();
    by_size.sort_by_key(|(name, size)| field_cost(name, *size));
    let mut kept_names = HashSet::new();
    let mut total = 0;
    for (name, size) in by_size {
        let cost = field_cost(name, *size);
        if total + cost > limit {
            break;
        }
        total += cost;
        kept_names.insert(name.clone());
    }
    let mut kept = Map::new();
    let mut lost = Map::new();
    for (name, value) in fields {
        if kept_names.contains(&name) {
            kept.insert(name, value);
        } else {
            let size = sizes.iter().find(|(n, _)| *n == name).map_or(0, |(_, s)| *s);
            lost.insert(name, Value::from(size));
        }
    }
    let mut dropped = Map::new();
    dropped.insert("fields".to_string(), Value::Object(lost));
    dropped.insert("read".to_string(), Value::from(read));
    kept.insert("dropped".to_string(), Value::Object(dropped));
    Value::Object(kept).to_string()
}

/// A field's size: a string's bytes, or another value's JSON length.
fn value_size(value: &Value) -> usize {
    match value {
        Value::String(s) => s.len(),
        other => other.to_string().len(),
    }
}

/// What a kept field adds to the serialized object: its name and value with
/// quotes, colon and comma.
fn field_cost(name: &str, size: usize) -> usize {
    name.len() + size + 6
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::shell_envelope::{ShellEnvelope, ShellStatus};
    use kaijutsu_types::{BlockId, ContextId, PrincipalId, ToolKind};

    const KEY: &str = "src_key_7";

    fn result(content: &str, model_content: Option<&str>) -> BlockSnapshot {
        let call = BlockId::new(ContextId::new(), PrincipalId::new(), 1);
        let mut snap = BlockSnapshot::tool_result(
            BlockId::new(call.context_id, PrincipalId::new(), 2),
            call,
            ToolKind::Shell,
            content,
            false,
            Some(0),
            Some("toolu_1".to_string()),
        );
        snap.model_content = model_content.map(str::to_owned);
        snap
    }

    /// A shell result as a model turn stores it: `content` is the output,
    /// `model_content` the text `model_text` built, `shell_envelope` the record.
    fn shell_result(output: &str, envelope: ShellEnvelope) -> BlockSnapshot {
        let mut snap = result(output, Some(&envelope.model_text(output)));
        snap.shell_envelope = Some(serde_json::to_string(&envelope).unwrap());
        snap
    }

    #[test]
    fn a_small_result_stays_whole() {
        let mut snap = result("ok", Some("ok"));
        let before = snap.clone();
        assert_eq!(slim_tool_result(&mut snap, 1024, KEY), None);
        assert_eq!(snap, before);
    }

    /// A shell result keeps the fact lines a model reads, and one line says
    /// what was dropped and how to read it.
    #[test]
    fn a_shell_result_keeps_its_fact_lines() {
        let output = "x".repeat(5000);
        let mut envelope = ShellEnvelope::new(ShellStatus::Error);
        envelope.exit_code = Some(2);
        envelope.cwd = Some("/src".to_string());
        envelope.data = Some(serde_json::json!({"rows": 3}));
        let mut snap = shell_result(&output, envelope);
        let saved = slim_tool_result(&mut snap, 1024, KEY).expect("slimmed");

        assert_eq!(
            snap.model_content.as_deref(),
            Some(
                "[dropped: 5000 bytes of output; read with: kj block read src_key_7]\n\
                 [exit 2]\n[data] {\"rows\":3}\n[cwd now /src]"
            )
        );
        assert_eq!(snap.content, snap.model_content.clone().unwrap(), "people see the same text");
        assert!(saved > 4000, "saved {saved}");
    }

    /// Data over the limit is dropped with the output, and named.
    #[test]
    fn a_shell_results_big_data_is_dropped_too() {
        let mut envelope = ShellEnvelope::new(ShellStatus::Done);
        envelope.data = Some(serde_json::json!({"rows": "r".repeat(3000)}));
        let mut snap = shell_result("ok", envelope);
        slim_tool_result(&mut snap, 1024, KEY).expect("slimmed");
        assert_eq!(
            snap.model_content.as_deref(),
            Some("[dropped: 2 bytes of output and 3011 bytes of data; read with: kj block read src_key_7]")
        );
        let record: ShellEnvelope = serde_json::from_str(snap.shell_envelope.as_deref().unwrap()).unwrap();
        assert_eq!(record.data, None, "the record matches what is sent");
    }

    /// A JSON result keeps its small fields and names the bulky one.
    #[test]
    fn a_json_result_keeps_its_small_fields_and_names_the_rest() {
        let json = serde_json::json!({"text": "y".repeat(5000), "ok": true, "count": 3}).to_string();
        let mut snap = result(&json, Some(&json));
        slim_tool_result(&mut snap, 1024, KEY).expect("slimmed");
        let slim: Value = serde_json::from_str(snap.model_content.as_deref().unwrap()).unwrap();
        assert_eq!(slim["ok"], true);
        assert_eq!(slim["count"], 3);
        assert!(slim.get("text").is_none(), "{slim}");
        assert_eq!(slim["dropped"]["fields"]["text"], 5000);
        assert_eq!(slim["dropped"]["read"], format!("kj block read {KEY}"));
    }

    /// Fields that fit stay in their original order; the largest go first.
    #[test]
    fn the_largest_fields_go_first_and_order_is_kept() {
        let json = serde_json::json!({
            "a": "y".repeat(600), "b": 1, "c": "z".repeat(700), "d": "small",
        })
        .to_string();
        let mut snap = result(&json, Some(&json));
        slim_tool_result(&mut snap, 1024, KEY).expect("slimmed");
        let slim: Value = serde_json::from_str(snap.model_content.as_deref().unwrap()).unwrap();
        let keys: Vec<&str> = slim.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["a", "b", "d", "dropped"]);
        assert_eq!(slim["dropped"]["fields"]["c"], 700);
    }

    /// A stub is never slimmed again, so a fork of a fork keeps the pointer
    /// to the original.
    #[test]
    fn a_stub_is_left_alone() {
        let json = serde_json::json!({"text": "y".repeat(5000), "ok": true}).to_string();
        let mut snap = result(&json, Some(&json));
        slim_tool_result(&mut snap, 1024, KEY).expect("slimmed");
        let stub = snap.clone();
        assert_eq!(slim_tool_result(&mut snap, 10, "another_key"), None);
        assert_eq!(snap, stub);
    }

    /// Text that is not a JSON object becomes one line naming its size.
    #[test]
    fn plain_text_becomes_one_line() {
        let text = "line\n".repeat(1000);
        let mut snap = result(&text, Some(&text));
        slim_tool_result(&mut snap, 1024, KEY).expect("slimmed");
        assert_eq!(
            snap.model_content.as_deref(),
            Some(format!("[dropped: {} bytes; read with: kj block read {KEY}]", text.len()).as_str())
        );
    }

    /// A result no model turn sent keeps `model_content` empty, so hydration
    /// still joins its small stderr and folds in an Error child.
    #[test]
    fn a_result_no_turn_sent_keeps_its_small_stderr() {
        let text = "line\n".repeat(1000);
        let mut snap = result(&text, None);
        snap.stderr = Some("warning".to_string());
        slim_tool_result(&mut snap, 1024, KEY).expect("slimmed");
        assert_eq!(snap.model_content, None);
        assert_eq!(snap.stderr.as_deref(), Some("warning"));
        let sent = crate::llm::hydrate::model_tool_result_text(&snap, &snap.content);
        assert!(sent.starts_with("[dropped: ") && sent.ends_with("[stderr]\nwarning"), "{sent}");
    }

    /// Spans and type described the old text; the stub drops them.
    #[test]
    fn a_stub_carries_no_stale_rendering() {
        let text = "\u{1b}[31mred\u{1b}[0m ".repeat(500);
        let mut snap = result(&text, Some(&text));
        snap.style_spans = serde_json::from_value(serde_json::json!([{"start": 0, "end": 3000}])).unwrap();
        snap.content_type = ContentType::Diff;
        slim_tool_result(&mut snap, 1024, KEY).expect("slimmed");
        assert!(snap.style_spans.is_empty());
        assert_eq!(snap.content_type, ContentType::Plain);
    }

    #[test]
    fn images_and_thinking_are_dropped_when_named() {
        let id = BlockId::new(ContextId::new(), PrincipalId::new(), 3);
        let mut image = BlockSnapshot::text(id, None, Role::Asset, "hash");
        image.content_type = ContentType::Image;
        let mut thinking = BlockSnapshot::text(id, None, Role::Model, "hmm");
        thinking.kind = BlockKind::Thinking;
        let text = BlockSnapshot::text(id, None, Role::Model, "hello");

        let spec = DropSpec { images: true, thinking: true, ..Default::default() };
        assert!(spec.drops_block(&image));
        assert!(spec.drops_block(&thinking));
        assert!(!spec.drops_block(&text));
        assert!(!DropSpec::default().drops_block(&image));
    }
}
