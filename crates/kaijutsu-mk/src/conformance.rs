//! Requests this crate builds and replies the service sent, checked against
//! the vendored `/mk/v1` OpenAPI file (`tests/fixtures/mk-openapi.json`, its
//! source in `mk-openapi.source`). The running decoder ignores unknown
//! fields, so this test is what reports a server change.

use serde_json::{json, Value};

use crate::generate::{
    Function, GenerateRequest, Message, ReasoningEffort, Role, Sample, Source, Tool, ToolCallIn, Wire,
};
use crate::json::Json;
use crate::model::RenderRequest;

const OPENAPI: &str = include_str!("../tests/fixtures/mk-openapi.json");
const STREAMS: [(&str, &str); 3] = [
    ("answer", include_str!("../tests/fixtures/mk_stream_answer.sse")),
    ("think", include_str!("../tests/fixtures/mk_stream_think.sse")),
    ("tool", include_str!("../tests/fixtures/mk_stream_tool.sse")),
];
const MODEL: &str = include_str!("../tests/fixtures/mk_model.json");

/// A validator for one named schema, resolved within the file's components.
fn validator(name: &str) -> jsonschema::Validator {
    let doc: Value = serde_json::from_str(OPENAPI).unwrap();
    let schema = json!({"$ref": format!("#/components/schemas/{name}"), "components": doc["components"]});
    jsonschema::validator_for(&schema).unwrap_or_else(|e| panic!("schema {name}: {e}"))
}

fn assert_valid(name: &str, instance: &Value) {
    let errors: Vec<String> = validator(name).iter_errors(instance).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{name} rejects {instance}:\n{}", errors.join("\n"));
}

fn object(v: Value) -> Json {
    serde_json::from_value(v).unwrap()
}

fn requests() -> Vec<GenerateRequest> {
    let mut assistant = Message {
        role: Role::Assistant,
        content: Some(String::new()),
        reasoning_content: Some("look first".into()),
        tool_calls: vec![ToolCallIn { name: "ls".into(), arguments: object(json!({"path": "/tmp", "all": true})) }],
    };
    let ls = Tool::Function {
        function: Function {
            name: "ls".into(),
            description: Some("List a directory.".into()),
            parameters: Some(object(json!({"type": "object", "properties": {"path": {"type": "string"}}}))),
        },
    };
    let mut full = GenerateRequest::messages(
        vec![Message::system("rules"), Message::user("list /tmp"), assistant.clone(), Message::tool("a\nb")],
        vec![ls],
    );
    full.thinking = Some(true);
    full.reasoning_effort = Some(ReasoningEffort::Low);
    full.prefill = Some("Sure".into());
    full.max_tokens = Some(512);
    full.sample = Some(Sample { temperature: Some(0.0), top_p: Some(0.9), top_k: Some(20), min_p: Some(0.05), seed: Some(7) });
    assistant.content = None;
    let bare = GenerateRequest::messages(vec![Message::user("hi"), assistant], vec![]);
    let source = |from| GenerateRequest { from, ..GenerateRequest::messages(vec![], vec![]) };
    vec![
        full,
        bare,
        source(Source::Context { context: "a".repeat(64) }),
        source(Source::Prompt { prompt: "<|im_start|>user\nhi".into() }),
        source(Source::Tokens { tokens: vec![1, 2, 3] }),
    ]
}

/// `(event, data)` pairs of a recorded stream.
fn frames(text: &str) -> Vec<(String, Value)> {
    let mut d = crate::generate::SseDecoder::default();
    let frames = d.push(text.as_bytes()).unwrap();
    d.finish().unwrap();
    frames.into_iter().map(|f| (f.event, serde_json::from_str(&f.data).unwrap())).collect()
}

#[test]
fn every_request_shape_matches_the_schema() {
    for request in requests() {
        request.validate().unwrap();
        for stream in [true, false] {
            assert_valid("GenerateRequest", &serde_json::to_value(Wire { request: &request, stream }).unwrap());
        }
    }
}

#[test]
fn a_render_request_matches_the_schema() {
    let Source::Messages { messages, tools } = requests().remove(0).from else { unreachable!() };
    let render = RenderRequest {
        messages,
        tools,
        thinking: Some(false),
        reasoning_effort: Some(ReasoningEffort::Medium),
        generation_prompt: Some(true),
    };
    assert_valid("RenderRequest", &serde_json::to_value(render).unwrap());
}

#[test]
fn every_recorded_event_matches_the_schema() {
    for (name, text) in STREAMS {
        let frames = frames(text);
        assert!(frames.len() > 1, "{name} has no tokens");
        let (last, tokens) = frames.split_last().unwrap();
        assert_eq!(last.0, "done", "{name}");
        assert_valid("Done", &last.1);
        for (event, data) in tokens {
            assert_eq!(event, "token", "{name}");
            assert_valid("TokenEvent", data);
        }
    }
    assert_valid("Model", &serde_json::from_str(MODEL).unwrap());
}

/// The sensor can fail: the validator rejects what the service would.
#[test]
fn the_schema_rejects_a_renamed_field_and_a_wrong_value() {
    let (_, mut done) = frames(STREAMS[0].1).pop().unwrap();
    let seed = done.as_object_mut().unwrap().remove("seed").unwrap();
    done["sed"] = seed;
    assert!(!validator("Done").is_valid(&done));

    let mut request = serde_json::to_value(Wire { request: &requests()[0], stream: true }).unwrap();
    request["from"]["messages"][0]["role"] = json!("robot");
    assert!(!validator("GenerateRequest").is_valid(&request));
}
