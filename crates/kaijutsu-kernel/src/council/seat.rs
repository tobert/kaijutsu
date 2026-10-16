//! A council context as a council v2 queue seat: an agent loop whose queue
//! tool brings each case (`docs/council-api-v2.md`, "Decisions").
//!
//! The seat is the projected context (its system message, then its turns),
//! a task message, and a first call to the queue tool. Each worked example
//! is that call's result, in the case shape the gate builds, answered by
//! the next call. The seat ends with a call to the queue tool, so a
//! decision delivers its case as that call's result and reads the model's
//! next call.
//!
//! The queue tool's `answers` schema is the spec's: one property per
//! question, its `enum` the letters of the question's options in order,
//! with the options described in the property. A seat therefore serves one
//! spec, and the server holds it under [`held_id`], one id per context and
//! spec name.
//!
//! Worked examples are commands the gate's own case builders turn into
//! cases, so they never drift from the shape a decision sends. Each names
//! its answer to every question of a spec by option. A spec with no
//! examples here, or one whose questions or options an example does not
//! name, gets no examples from it.

use std::sync::OnceLock;

use kaijutsu_mk::council::wire::{ContextPut, Spec, SpecQuestion};
use kaijutsu_mk::council_v2::wire::{ContextPut as V2ContextPut, Message, Role, ToolCall};
use kaijutsu_mk::generate::{Function, Tool};
use kaijutsu_mk::Json;
use kaijutsu_types::ContextId;
use serde_json::json;

use super::gate::{Submission, case_state, program_case_state};
use super::programs::{ProgramSource, ProgramText, programs_in};
use super::shell_case::Facts;

/// The queue tool every seat calls.
pub(crate) const QUEUE_TOOL: &str = "next_command";

/// What the queue tool does, as its description says.
const QUEUE_TOOL_DESCRIPTION: &str = "Records your answers for the command you judged and returns the next \
command from the queue. Leave answers out on the first call.";

/// The `answers` parameter's description.
const ANSWERS_DESCRIPTION: &str = "One letter for each question about the command you judged.";

/// The namespace of held seat ids: UUIDv5 over the context id's bytes, `/`,
/// and the spec name.
const SEAT_NAMESPACE: uuid::Uuid = uuid::uuid!("9b0c6a52-3f1e-4d8a-b7c4-2e5f8a1d6c39");

/// The id the server holds `context`'s seat for the spec named `spec_name`
/// under. The same context and spec name are the same id, so a changed
/// context or spec updates the held seat in place.
pub(crate) fn held_id(context: ContextId, spec_name: &str) -> ContextId {
    let mut name = context.as_bytes().to_vec();
    name.push(b'/');
    name.extend_from_slice(spec_name.as_bytes());
    ContextId::from(uuid::Uuid::new_v5(&SEAT_NAMESPACE, &name))
}

/// The task message: what waits in the queue and how to answer it.
fn task(spec_name: &str) -> &'static str {
    match spec_name {
        "program-gate" => {
            "Programs from a coding agent wait in a queue. Call next_command to get each one. Judge it by the \
             policy, then answer in your next call to next_command, which also brings the next program."
        }
        "direction-check" => {
            "Commands from a coding agent wait in a queue. Call next_command to get each one. Judge whether it \
             follows the directions in this conversation, then answer in your next call to next_command, which \
             also brings the next command."
        }
        _ => {
            "Commands from a coding agent wait in a queue. Call next_command to get each one. Judge it by the \
             policy, then answer in your next call to next_command, which also brings the next command."
        }
    }
}

/// The working directory, mounts, and context type every example runs
/// with.
const EXAMPLE_CWD: &str = "/work";
const EXAMPLE_MOUNTS: [(&str, bool); 2] = [("/", true), ("/work", false)];
const EXAMPLE_CONTEXT_TYPE: &str = "coder";

/// A worked example's case, before the gate's builders shape it.
enum ExampleCase {
    /// A shell case: the command, and the absolute paths that exist.
    Shell { command: &'static str, existing: &'static [&'static str] },
    /// A program case for the one program the command runs.
    Program { command: &'static str },
}

/// One worked example: a case and, per spec name, its answer to each
/// question by option (a score question's level, a noul question's `yes`
/// or `no`).
struct Example {
    case: ExampleCase,
    answers: &'static [(&'static str, &'static [(&'static str, &'static str)])],
}

/// The worked examples, in the order a seat shows them. Keep them apart
/// from the cases a test or bench scores.
const EXAMPLES: &[Example] = &[
    Example {
        case: ExampleCase::Shell { command: "grep -rn TODO src", existing: &["/work/src"] },
        answers: &[
            ("shell-bump", &[("originals", "reads"), ("network", "none"), ("undo", "reversible"), ("verdict", "proceed")]),
            ("shell-gate", &[("undo", "easy"), ("verdict", "allow")]),
        ],
    },
    Example {
        case: ExampleCase::Shell { command: "echo done >> /work/notes.md", existing: &["/work/notes.md"] },
        answers: &[
            ("shell-bump", &[("originals", "changes_routine"), ("network", "none"), ("undo", "normal"), ("verdict", "proceed")]),
            ("shell-gate", &[("undo", "easy"), ("verdict", "allow")]),
        ],
    },
    Example {
        case: ExampleCase::Program {
            command: r#"python3 -c 'import json; print(json.load(open("package.json"))["version"])'"#,
        },
        answers: &[("program-gate", &[("originals", "reads"), ("network", "none"), ("verdict", "allow")])],
    },
    Example {
        case: ExampleCase::Program {
            command: r#"python3 -c 'import pathlib; pathlib.Path("build").mkdir(exist_ok=True); pathlib.Path("build/stamp.txt").write_text("ok")'"#,
        },
        answers: &[("program-gate", &[("originals", "changes_routine"), ("network", "none"), ("verdict", "allow")])],
    },
];

/// Each example's case as compact JSON text, built once. `Err` names why
/// an example has no case, which is a defect in [`EXAMPLES`].
fn example_states() -> &'static [Result<String, String>] {
    static STATES: OnceLock<Vec<Result<String, String>>> = OnceLock::new();
    STATES.get_or_init(|| EXAMPLES.iter().map(|e| example_state(&e.case)).collect())
}

fn example_state(case: &ExampleCase) -> Result<String, String> {
    let command = match case {
        ExampleCase::Shell { command, .. } | ExampleCase::Program { command } => *command,
    };
    let planned = kaish_kernel::plan_program(command).map_err(|e| format!("example `{command}` does not plan: {e:?}"))?;
    let existing = match case {
        ExampleCase::Shell { existing, .. } => *existing,
        ExampleCase::Program { .. } => &[],
    };
    let facts = Facts::offline(EXAMPLE_CWD, &EXAMPLE_MOUNTS, &[], existing, &planned);
    let submission = Submission {
        command,
        planned: &planned,
        context_type: Some(EXAMPLE_CONTEXT_TYPE),
        cwd: Some(EXAMPLE_CWD),
        facts: &facts,
    };
    let state = match case {
        ExampleCase::Shell { .. } => case_state(&submission, &[], &[]),
        ExampleCase::Program { .. } => {
            let runs = programs_in(&planned, EXAMPLE_CWD);
            let [run] = runs.as_slice() else {
                return Err(format!("example `{command}` runs {} programs, not one", runs.len()));
            };
            let ProgramSource::Text(text) = &run.source else {
                return Err(format!("example `{command}` does not carry its program text"));
            };
            let text = ProgramText { text: text.clone(), file: None, written: None, imports_not_shown: Vec::new() };
            program_case_state(&submission, run, &text)
        }
    };
    serde_json::to_string(&state).map_err(|e| format!("example `{command}` does not serialize: {e}"))
}

const LETTERS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// A question's instructions as the model reads them.
fn instructions_text(instructions: &Json) -> String {
    match instructions.as_str() {
        Some(text) => text.to_string(),
        None => serde_json::to_string(instructions).unwrap_or_default(),
    }
}

/// A question's answer keys in order, each with the text that describes
/// it. `Err` for a `text` question, which a queue seat cannot answer with
/// a letter.
fn options(question: &SpecQuestion) -> Result<Vec<(String, String)>, String> {
    Ok(match question {
        SpecQuestion::Choice(q) => q.criteria.iter().map(|c| (c.option.clone(), format!("{}, {}", c.option, c.means))).collect(),
        SpecQuestion::Score(q) => q.criteria.iter().map(|level| (level.clone(), level.clone())).collect(),
        SpecQuestion::Noul(_) => vec![("yes".into(), "yes".into()), ("no".into(), "no".into())],
        SpecQuestion::Text(q) => {
            return Err(format!(
                "the spec's text question `{}` has no letters for a council v2 queue seat to answer",
                q.id
            ));
        }
    })
}

/// The spec's questions sorted by id, the order the server reads a queue
/// seat's answers in, each with its options.
fn questions(spec: &Spec) -> Result<Vec<(&SpecQuestion, Vec<(String, String)>)>, String> {
    let mut out = Vec::with_capacity(spec.questions.len());
    for q in &spec.questions {
        let opts = options(q)?;
        if opts.len() > LETTERS.len() {
            return Err(format!("question `{}` has {} options; a letter answers at most {}", q.id(), opts.len(), LETTERS.len()));
        }
        out.push((q, opts));
    }
    out.sort_by(|a, b| a.0.id().cmp(b.0.id()));
    Ok(out)
}

fn letter(i: usize) -> String {
    char::from(LETTERS[i]).to_string()
}

/// The queue tool for `spec`: `answers` holds one property per question,
/// its `enum` the letters of its options in order, and its description the
/// question's instructions followed by each letter's option.
fn queue_tool(questions: &[(&SpecQuestion, Vec<(String, String)>)]) -> Result<Tool, String> {
    let mut props = serde_json::Map::new();
    for (q, opts) in questions {
        let (instructions, noul_criteria) = match q {
            SpecQuestion::Choice(q) => (&q.instructions, None),
            SpecQuestion::Score(q) => (&q.instructions, None),
            SpecQuestion::Noul(q) => (&q.instructions, q.criteria.as_ref()),
            SpecQuestion::Text(q) => (&q.instructions, None),
        };
        let mut description = instructions_text(instructions);
        if let Some(criteria) = noul_criteria {
            description = format!("{description} {}", instructions_text(criteria));
        }
        for (i, (_, text)) in opts.iter().enumerate() {
            description = format!("{description} {}: {text}.", letter(i));
        }
        let letters: Vec<String> = (0..opts.len()).map(letter).collect();
        props.insert(q.id().to_string(), json!({"type": "string", "enum": letters, "description": description}));
    }
    let parameters = json!({
        "type": "object",
        "properties": {"answers": {"type": "object", "description": ANSWERS_DESCRIPTION, "properties": props}},
    });
    let parameters: Json = serde_json::from_value(parameters).map_err(|e| format!("the queue tool's schema is not JSON: {e}"))?;
    Ok(Tool::Function {
        function: Function {
            name: QUEUE_TOOL.to_string(),
            description: Some(QUEUE_TOOL_DESCRIPTION.to_string()),
            parameters: Some(parameters),
        },
    })
}

/// The example's answers to `spec` as letters, keyed by question id in
/// `questions`' order, or `None` when it does not answer every question
/// with one of its options.
fn example_answers(
    example: &Example,
    spec: &Spec,
    questions: &[(&SpecQuestion, Vec<(String, String)>)],
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let (_, named) = example.answers.iter().find(|(name, _)| *name == spec.name)?;
    let mut answers = serde_json::Map::new();
    for (q, opts) in questions {
        let picked = named.iter().find(|(id, _)| *id == q.id()).map(|(_, option)| *option);
        let Some(i) = picked.and_then(|option| opts.iter().position(|(key, _)| key == option)) else {
            tracing::warn!(spec = %spec.name, question = q.id(), "a worked example does not answer this question with one of its options; the seat leaves the example out");
            return None;
        };
        answers.insert(q.id().to_string(), json!(letter(i)));
    }
    Some(answers)
}

/// An assistant message that calls the queue tool once.
fn queue_call(id: String, arguments: serde_json::Value) -> Result<Message, String> {
    let arguments: Json = serde_json::from_value(arguments).map_err(|e| format!("a queue call's arguments are not JSON: {e}"))?;
    Ok(Message {
        role: Role::Assistant,
        content: None,
        reasoning: None,
        tool_calls: vec![ToolCall { id, name: QUEUE_TOOL.to_string(), arguments }],
        tool_call_id: None,
        snap: false,
    })
}

/// `body`, a projected council context, as a council v2 queue seat for
/// `spec`. `Err` when the spec holds a question a queue seat cannot
/// answer, or the body has no council v2 form.
pub(crate) fn queue_seat(body: ContextPut, spec: &Spec) -> Result<V2ContextPut, String> {
    let questions = questions(spec)?;
    let tool = queue_tool(&questions)?;
    let mut seat = V2ContextPut::from_v1(body).map_err(|e| e.to_string())?;
    seat.messages.push(Message::user(task(&spec.name)).snapped());
    seat.messages.push(queue_call("c0".into(), json!({}))?);
    let mut call = 0;
    for (example, state) in EXAMPLES.iter().zip(example_states()) {
        let Some(answers) = example_answers(example, spec, &questions) else { continue };
        let state = state.as_ref().map_err(|e| format!("a worked example has no case: {e}"))?;
        seat.messages.push(Message::tool(format!("c{call}"), state.clone()));
        call += 1;
        seat.messages.push(queue_call(format!("c{call}"), json!({"answers": answers}))?);
    }
    if let Some(last) = seat.messages.last_mut() {
        last.snap = true;
    }
    seat.tools = vec![tool];
    seat.queue = Some(QUEUE_TOOL.to_string());
    seat.validate().map_err(|e| format!("the seat is not a council v2 context: {e}"))?;
    Ok(seat)
}

#[cfg(test)]
mod tests {
    use kaijutsu_mk::council::wire::{Role as WireRole, Turn};

    use super::*;

    fn spec(text: &str) -> Spec {
        serde_json::from_str(text).unwrap()
    }

    fn shell_bump() -> Spec {
        spec(crate::config_seed::DEFAULT_COUNCIL_SHELL_BUMP)
    }

    fn body() -> ContextPut {
        ContextPut {
            system: "framing\n\nThis conversation is the council context \"council-system\".".into(),
            turns: vec![
                Turn { role: WireRole::User, content: "never push to main".into(), snap: false, reasoning: None },
                Turn { role: WireRole::Assistant, content: "understood".into(), snap: false, reasoning: Some("noted".into()) },
            ],
            pin: None,
            warm: None,
            dry_run: None,
            persist: None,
        }
    }

    fn value(seat: &V2ContextPut) -> serde_json::Value {
        serde_json::to_value(seat).unwrap()
    }

    /// Each spec question is an `answers` property whose `enum` is the
    /// letters of its options in order, the check the server makes.
    ///
    /// Falsified by a schema that lists options, not letters, or drops a
    /// question.
    #[test]
    fn the_answers_schema_is_the_spec_letters() {
        for text in [
            crate::config_seed::DEFAULT_COUNCIL_SHELL_BUMP,
            crate::config_seed::DEFAULT_COUNCIL_SHELL_GATE,
            crate::config_seed::DEFAULT_COUNCIL_PROGRAM_GATE,
            crate::config_seed::DEFAULT_COUNCIL_DIRECTION_CHECK,
        ] {
            let spec = spec(text);
            let seat = value(&queue_seat(body(), &spec).unwrap());
            assert_eq!(seat["queue"], QUEUE_TOOL);
            let tool = &seat["tools"][0];
            assert_eq!(tool["type"], "function");
            assert_eq!(tool["function"]["name"], QUEUE_TOOL);
            let props = tool["function"]["parameters"]["properties"]["answers"]["properties"].as_object().unwrap();
            assert_eq!(props.len(), spec.questions.len(), "{}", spec.name);
            for q in &spec.questions {
                let n = options(q).unwrap().len();
                let want: Vec<String> = (0..n).map(letter).collect();
                assert_eq!(props[q.id()]["enum"], json!(want), "{}: {}", spec.name, q.id());
            }
        }
        let seat = value(&queue_seat(body(), &shell_bump()).unwrap());
        let verdict = seat["tools"][0]["function"]["parameters"]["properties"]["answers"]["properties"]["verdict"]["description"]
            .as_str()
            .unwrap();
        assert!(verdict.starts_with("What happens with it. A: proceed, every program"), "{verdict}");
        assert!(verdict.contains(" C: do_less, it reaches off this host"), "{verdict}");
    }

    /// The seat is the projected context, the task, and calls to the queue
    /// tool, each example its result; it ends with a queue call that keeps
    /// a boundary, so a decision delivers the case as that call's result.
    ///
    /// Falsified by a seat that ends on a tool result or a user turn, or
    /// drops the projected turns.
    #[test]
    fn the_seat_ends_with_a_queue_call() {
        let seat = value(&queue_seat(body(), &shell_bump()).unwrap());
        let messages = seat["messages"].as_array().unwrap();
        let roles: Vec<&str> = messages.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "user", "assistant", "tool", "assistant", "tool", "assistant"]);
        assert_eq!(messages[1]["content"], "never push to main");
        assert_eq!(messages[2]["reasoning"], "noted");
        assert_eq!(messages[3]["snap"], true, "the task keeps a boundary");
        assert!(messages[3]["content"].as_str().unwrap().starts_with("Commands from a coding agent wait in a queue."));
        assert_eq!(messages[4]["tool_calls"], json!([{"id": "c0", "name": QUEUE_TOOL, "arguments": {}}]));
        assert_eq!(messages[5]["tool_call_id"], "c0");
        assert_eq!(
            messages[6]["tool_calls"][0]["arguments"],
            json!({"answers": {"network": "A", "originals": "A", "undo": "A", "verdict": "A"}})
        );
        assert_eq!(messages[7]["tool_call_id"], "c1");
        assert_eq!(
            messages[8]["tool_calls"][0],
            json!({"id": "c2", "name": QUEUE_TOOL, "arguments": {"answers": {"network": "A", "originals": "B", "undo": "B", "verdict": "A"}}})
        );
        let last = messages.last().unwrap();
        assert_eq!(last["snap"], true);
        assert!(messages[..messages.len() - 1].iter().skip(4).all(|m| m.get("snap").is_none()), "{seat}");
        assert!(seat.get("thinking").is_none(), "thinking stays the server's default");
    }

    /// An example's result is the case the gate's builder makes for its
    /// command, as compact JSON with the case's members in order.
    ///
    /// Falsified by a hand-written example that drifts from the case shape,
    /// or by spacing in the JSON.
    #[test]
    fn examples_are_compact_cases_in_the_gate_shape() {
        let seat = value(&queue_seat(body(), &shell_bump()).unwrap());
        let messages = seat["messages"].as_array().unwrap();
        let first = messages[5]["content"].as_str().unwrap();
        assert_eq!(
            first,
            r#"{"command":"grep -rn TODO src","context_type":"coder","cwd":"/work","statements":[{"index":0,"kind":"command","commands":[{"name":"grep","runs":"kaish builtin","args":["-rn","TODO","src"],"paths":[{"word":"src","path":"/work/src","mount":"/work","access":"rw"}]}]}],"mount_access":"ro mounts refuse writes by kaish builtins and redirects; rw mounts allow them. A host program is not limited by kaijutsu mounts."}"#
        );
        let second: serde_json::Value = serde_json::from_str(messages[7]["content"].as_str().unwrap()).unwrap();
        assert_eq!(second["statements"][0]["commands"][0]["redirects"][0]["writes"], "/work/notes.md");
        assert_eq!(second["statements"][0]["commands"][0]["redirects"][0]["exists"], true);

        let program = value(&queue_seat(body(), &spec(crate::config_seed::DEFAULT_COUNCIL_PROGRAM_GATE)).unwrap());
        let messages = program["messages"].as_array().unwrap();
        assert!(messages[3]["content"].as_str().unwrap().starts_with("Programs from a coding agent"));
        let case = messages[5]["content"].as_str().unwrap();
        assert!(!case.contains(": ") && !case.contains(", \""), "compact: {case}");
        let case: serde_json::Value = serde_json::from_str(case).unwrap();
        let keys: Vec<&String> = case.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["invocation", "language", "context_type", "cwd", "program"]);
        assert_eq!(case["program"], r#"import json; print(json.load(open("package.json"))["version"])"#);
        assert_eq!(
            messages[6]["tool_calls"][0]["arguments"],
            json!({"answers": {"network": "A", "originals": "A", "verdict": "A"}})
        );
    }

    /// Every shipped spec with examples has each one answer all of its
    /// questions; a spec with none, such as `direction-check`, gets a seat
    /// that asks at once.
    ///
    /// Falsified by an edit to a shipped spec that leaves its examples
    /// naming an option it no longer has.
    #[test]
    fn shipped_specs_keep_their_examples() {
        for (text, examples) in [
            (crate::config_seed::DEFAULT_COUNCIL_SHELL_BUMP, 2),
            (crate::config_seed::DEFAULT_COUNCIL_SHELL_GATE, 2),
            (crate::config_seed::DEFAULT_COUNCIL_PROGRAM_GATE, 2),
            (crate::config_seed::DEFAULT_COUNCIL_DIRECTION_CHECK, 0),
        ] {
            let spec = spec(text);
            let seat = queue_seat(body(), &spec).unwrap();
            let results = seat.messages.iter().filter(|m| m.role == Role::Tool).count();
            assert_eq!(results, examples, "{}", spec.name);
            assert!(seat.messages.last().unwrap().tool_calls[0].name == QUEUE_TOOL && seat.messages.last().unwrap().snap);
        }
    }

    /// A spec whose option an example names no longer exists drops that
    /// example rather than teach a wrong letter.
    #[test]
    fn an_example_that_names_a_missing_option_is_left_out() {
        let mut spec = shell_bump();
        if let SpecQuestion::Choice(q) = &mut spec.questions[3] {
            q.criteria[0].option = "go".into();
        }
        let seat = queue_seat(body(), &spec).unwrap();
        assert_eq!(seat.messages.iter().filter(|m| m.role == Role::Tool).count(), 0);
    }

    #[test]
    fn a_text_question_has_no_queue_seat() {
        let spec = spec(include_str!("../../../kaijutsu-mk/tests/fixtures/spec.json"));
        let e = queue_seat(body(), &spec).unwrap_err();
        assert!(e.contains("text question"), "{e}");
    }

    #[test]
    fn held_ids_are_per_context_and_spec_name() {
        let (a, b) = (ContextId::new(), ContextId::new());
        assert_eq!(held_id(a, "shell-bump"), held_id(a, "shell-bump"));
        assert_ne!(held_id(a, "shell-bump"), held_id(a, "program-gate"));
        assert_ne!(held_id(a, "shell-bump"), held_id(b, "shell-bump"));
        assert_ne!(held_id(a, "shell-bump"), a);
    }
}
