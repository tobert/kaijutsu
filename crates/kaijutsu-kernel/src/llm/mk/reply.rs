//! Turn the service's token pieces and `done` into [`StreamEvent`]s
//! (`docs/mk.md`, "Response mapping").
//!
//! Token pieces carry the template's markers and the whitespace around them;
//! `done` carries each phase's text without them. [`Reply`] splits the pieces
//! the way the service does (text before `</think>` is thinking; with tools,
//! text from `<tool_call>` on is tool text) and trims each phase the way the
//! service trims it, so the streamed blocks hold exactly `done`'s text. It
//! checks that at the end: a difference is an error, never a block that
//! disagrees with what the service holds as the reply.

use kaijutsu_mk::generate::{Done, Finish, ToolCall};

use crate::llm::stream::{MkUsageExtra, StreamEvent, UsageExtra};

const THINK_END: &str = "</think>";
const CALL_OPEN: &str = "<tool_call>";

/// The `ThinkingEnd` signature. The service has no signature; this sentinel
/// marks mk reasoning as replayable, so the runtime and hydration send it back
/// as `reasoning_content` and the template decides what to keep. Opaque: only
/// its presence is read.
pub(super) const REASONING_SIGNATURE: &str = "mk-reasoning";

/// The name a call gets when the service could not read one.
pub(super) const UNNAMED_CALL: &str = "unnamed_tool_call";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Think,
    Answer,
    Tool,
}

/// What the provider measured around one generation.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Timing {
    pub(super) first_token_ms: u64,
    pub(super) wall_ms: u64,
}

pub(super) struct Reply {
    phase: Phase,
    tools: bool,
    /// Text that could still become the current phase's marker.
    held: String,
    think: Block,
    answer: Block,
}

impl Reply {
    /// `thinking` is whether generation starts inside thinking; `tools`,
    /// whether the request declared tools (only then is `<tool_call>` a
    /// marker).
    pub(super) fn new(thinking: bool, tools: bool) -> Self {
        Reply {
            phase: if thinking { Phase::Think } else { Phase::Answer },
            tools,
            held: String::new(),
            think: Block::new(Kind::Think),
            answer: Block::new(Kind::Answer),
        }
    }

    pub(super) fn piece(&mut self, piece: &str) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        self.held.push_str(piece);
        while !self.held.is_empty() {
            let marker = match self.phase {
                Phase::Think => THINK_END,
                Phase::Answer if self.tools => CALL_OPEN,
                Phase::Answer | Phase::Tool => {
                    let text = std::mem::take(&mut self.held);
                    self.text(&text, &mut out);
                    break;
                }
            };
            if let Some(i) = self.held.find(marker) {
                let text = self.held[..i].to_string();
                self.text(&text, &mut out);
                self.held.drain(..i + marker.len());
                if self.phase == Phase::Think {
                    self.think.close(&mut out);
                    self.phase = Phase::Answer;
                } else {
                    self.answer.close(&mut out);
                    self.phase = Phase::Tool;
                    self.held.clear();
                }
                continue;
            }
            let keep = marker_prefix_len(&self.held, marker);
            let text = self.held[..self.held.len() - keep].to_string();
            self.text(&text, &mut out);
            self.held.drain(..self.held.len() - keep);
            break;
        }
        out
    }

    fn text(&mut self, text: &str, out: &mut Vec<StreamEvent>) {
        match self.phase {
            Phase::Think => self.think.push(text, out),
            Phase::Answer => self.answer.push(text, out),
            Phase::Tool => {}
        }
    }

    /// The events that end the reply: the open block closes, the calls
    /// follow, then `Done`. `id_base` names the calls (`<id_base>-<index>`).
    pub(super) fn done(mut self, done: Done, id_base: &str, timing: Timing) -> Result<Vec<StreamEvent>, String> {
        let mut out = Vec::new();
        let held = std::mem::take(&mut self.held);
        self.text(&held, &mut out);
        self.think.close(&mut out);
        self.answer.close(&mut out);
        if self.think.text != done.reasoning_content {
            return Err(mismatch("thinking", &self.think.text, &done.reasoning_content));
        }
        if self.answer.text != done.content {
            return Err(mismatch("answer", &self.answer.text, &done.content));
        }
        let stop_reason = match done.finish {
            Finish::Stop if !done.tool_calls.is_empty() => "tool_calls",
            Finish::Stop => "stop",
            Finish::Length => "length",
            Finish::Cancelled => {
                return Err("the megakernel cancelled the generation, and the kernel did not ask it to".into());
            }
        };
        for (index, call) in done.tool_calls.into_iter().enumerate() {
            let id = format!("{id_base}-{index}");
            out.push(match call {
                ToolCall::Parsed { name, arguments } => StreamEvent::ToolUse {
                    id,
                    name,
                    input: serde_json::to_value(&arguments).map_err(|e| format!("tool call arguments: {e}"))?,
                },
                ToolCall::Unparsed { name: Some(name), raw, error } => {
                    StreamEvent::ToolUseInvalid { id, name, arguments: raw, error }
                }
                ToolCall::Unparsed { name: None, raw, error } => StreamEvent::ToolUseInvalid {
                    id,
                    name: UNNAMED_CALL.into(),
                    arguments: raw,
                    error: format!("the tool name did not parse; {error}"),
                },
            });
        }
        out.push(StreamEvent::Done {
            stop_reason: Some(stop_reason.into()),
            input_tokens: Some(done.usage.prompt.into()),
            output_tokens: Some(done.usage.completion.into()),
            extra: Some(UsageExtra::Mk(MkUsageExtra {
                kept: done.usage.kept.into(),
                fed: done.usage.fed.into(),
                prefill_ms: done.ms.prefill.round() as u64,
                decode_ms: done.ms.decode.round() as u64,
                first_token_ms: timing.first_token_ms,
                wall_ms: timing.wall_ms,
                seed: done.seed,
                model: done.model.id,
                weight_hash: done.model.weight_hash,
            })),
        });
        Ok(out)
    }
}

fn mismatch(what: &str, streamed: &str, done: &str) -> String {
    format!(
        "the streamed {what} differs from the megakernel's done text, so the block would not match the reply \
         the service holds; the token split in llm/mk/reply.rs no longer matches the service \
         (streamed {streamed:?}, done {done:?})"
    )
}

/// The longest end of `text` that is a proper start of `marker`.
fn marker_prefix_len(text: &str, marker: &str) -> usize {
    (1..marker.len()).rev().find(|&k| text.ends_with(&marker[..k])).unwrap_or(0)
}

/// Whitespace as Python's `str.strip()` sees it, which is how the service
/// trims: Unicode white space plus the four ASCII separators U+001C..U+001F.
fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

#[derive(Clone, Copy)]
enum Kind {
    Think,
    Answer,
}

/// One phase's block. Leading white space is dropped; trailing white space
/// is held until more text follows it, and dropped at the end.
struct Block {
    kind: Kind,
    open: bool,
    /// White space that may yet be trailing.
    held: String,
    /// The text emitted so far.
    text: String,
}

impl Block {
    fn new(kind: Kind) -> Self {
        Block { kind, open: false, held: String::new(), text: String::new() }
    }

    fn push(&mut self, text: &str, out: &mut Vec<StreamEvent>) {
        let text = if self.open { text } else { text.trim_start_matches(is_space) };
        let body = text.trim_end_matches(is_space);
        if body.is_empty() {
            if self.open {
                self.held.push_str(text);
            }
            return;
        }
        if !self.open {
            self.open = true;
            out.push(match self.kind {
                Kind::Think => StreamEvent::ThinkingStart,
                Kind::Answer => StreamEvent::TextStart,
            });
        }
        let delta = std::mem::take(&mut self.held) + body;
        self.held.push_str(&text[body.len()..]);
        self.text.push_str(&delta);
        out.push(match self.kind {
            Kind::Think => StreamEvent::ThinkingDelta(delta),
            Kind::Answer => StreamEvent::TextDelta(delta),
        });
    }

    fn close(&mut self, out: &mut Vec<StreamEvent>) {
        if std::mem::take(&mut self.open) {
            self.held.clear();
            out.push(match self.kind {
                Kind::Think => StreamEvent::ThinkingEnd { signature: Some(REASONING_SIGNATURE.into()) },
                Kind::Answer => StreamEvent::TextEnd,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use kaijutsu_mk::generate::TokenEvent;

    use super::*;

    const ANSWER: &str = include_str!("../../../../kaijutsu-mk/tests/fixtures/mk_stream_answer.sse");
    const THINK: &str = include_str!("../../../../kaijutsu-mk/tests/fixtures/mk_stream_think.sse");
    const TOOL: &str = include_str!("../../../../kaijutsu-mk/tests/fixtures/mk_stream_tool.sse");

    /// The recorded stream's token pieces and its `done`.
    fn recorded(text: &str) -> (Vec<String>, Done) {
        let mut pieces = Vec::new();
        let mut event = String::new();
        for line in text.lines() {
            if let Some(e) = line.strip_prefix("event: ") {
                event = e.to_string();
            } else if let Some(data) = line.strip_prefix("data: ") {
                if event == "token" {
                    pieces.push(serde_json::from_str::<TokenEvent>(data).unwrap().piece);
                } else {
                    return (pieces, serde_json::from_str(data).unwrap());
                }
            }
        }
        panic!("no done in the recording");
    }

    fn run(thinking: bool, tools: bool, pieces: &[&str], done: Done) -> Result<Vec<StreamEvent>, String> {
        let mut reply = Reply::new(thinking, tools);
        let mut out: Vec<StreamEvent> = pieces.iter().flat_map(|p| reply.piece(p)).collect();
        out.extend(reply.done(done, "mk-test", Timing::default())?);
        Ok(out)
    }

    fn texts(events: &[StreamEvent]) -> (String, String) {
        let mut think = String::new();
        let mut answer = String::new();
        for e in events {
            match e {
                StreamEvent::ThinkingDelta(t) => think.push_str(t),
                StreamEvent::TextDelta(t) => answer.push_str(t),
                _ => {}
            }
        }
        (think, answer)
    }

    fn done_with(base: &Done, reasoning: &str, content: &str) -> Done {
        Done { reasoning_content: reasoning.into(), content: content.into(), tool_calls: vec![], ..base.clone() }
    }

    #[test]
    fn a_recorded_thinking_reply_streams_exactly_the_done_text() {
        let (pieces, done) = recorded(THINK);
        let pieces: Vec<&str> = pieces.iter().map(String::as_str).collect();
        let events = run(true, false, &pieces, done.clone()).unwrap();
        assert_eq!(texts(&events), (done.reasoning_content.clone(), done.content.clone()));
        let order: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ThinkingStart => Some("think["),
                StreamEvent::ThinkingEnd { .. } => Some("]think"),
                StreamEvent::TextStart => Some("text["),
                StreamEvent::TextEnd => Some("]text"),
                StreamEvent::Done { .. } => Some("done"),
                _ => None,
            })
            .collect();
        assert_eq!(order, ["think[", "]think", "text[", "]text", "done"]);
        let signature = events.iter().find_map(|e| match e {
            StreamEvent::ThinkingEnd { signature } => Some(signature.clone()),
            _ => None,
        });
        assert_eq!(signature, Some(Some(REASONING_SIGNATURE.to_string())), "unsigned reasoning is never replayed");
    }

    #[test]
    fn a_recorded_answer_and_tool_call_map_to_text_then_calls_then_done() {
        let (pieces, done) = recorded(ANSWER);
        let pieces: Vec<&str> = pieces.iter().map(String::as_str).collect();
        let events = run(false, false, &pieces, done.clone()).unwrap();
        assert_eq!(texts(&events).1, done.content);
        let StreamEvent::Done { stop_reason, input_tokens, output_tokens, extra: Some(UsageExtra::Mk(x)) } = events.last().unwrap() else {
            panic!("{events:?}")
        };
        assert_eq!((stop_reason.as_deref(), *input_tokens, *output_tokens), (Some("stop"), Some(33), Some(9)));
        assert_eq!((x.fed, x.seed, x.model.as_str()), (33, 7, "Qwen3.8-Flash-Next-UD-IQ4_XS"));

        let (pieces, done) = recorded(TOOL);
        let pieces: Vec<&str> = pieces.iter().map(String::as_str).collect();
        let events = run(false, true, &pieces, done).unwrap();
        assert!(!events.iter().any(|e| matches!(e, StreamEvent::TextStart)), "{events:?}");
        let StreamEvent::ToolUse { id, name, input } = &events[0] else { panic!("{events:?}") };
        assert_eq!((id.as_str(), name.as_str()), ("mk-test-0", "ls"));
        assert_eq!(input, &serde_json::json!({"path": "/tmp/demo"}));
        assert!(matches!(&events[1], StreamEvent::Done { stop_reason: Some(s), .. } if s == "tool_calls"));
    }

    #[test]
    fn a_marker_split_across_pieces_is_still_a_marker() {
        let (_, base) = recorded(THINK);
        let pieces = ["  I think", " so.\n", "</thi", "nk>", "\n\nYes", " ", "<tool_", "call>\n<function=x>"];
        let events = run(true, true, &pieces, done_with(&base, "I think so.", "Yes")).unwrap();
        assert_eq!(texts(&events), ("I think so.".into(), "Yes".into()));
    }

    #[test]
    fn without_tools_a_tool_call_marker_is_answer_text() {
        let (_, base) = recorded(ANSWER);
        let events = run(false, false, &["Use ", "<tool_call>", " here"], done_with(&base, "", "Use <tool_call> here")).unwrap();
        assert_eq!(texts(&events).1, "Use <tool_call> here");
    }

    #[test]
    fn thinking_that_never_closes_is_all_thinking() {
        let (_, base) = recorded(THINK);
        let done = Done { finish: Finish::Length, ..done_with(&base, "Let me think about </th", "") };
        let events = run(true, false, &["Let me", " think ", "about </th"], done).unwrap();
        assert_eq!(texts(&events), ("Let me think about </th".into(), String::new()));
        assert!(matches!(events.last(), Some(StreamEvent::Done { stop_reason: Some(s), .. }) if s == "length"));
    }

    #[test]
    fn inner_white_space_is_kept_and_python_separators_are_trimmed() {
        let (_, base) = recorded(ANSWER);
        let events = run(false, false, &["\u{1f}a", "\n\n", "b", " \u{1c}"], done_with(&base, "", "a\n\nb")).unwrap();
        assert_eq!(texts(&events).1, "a\n\nb");
    }

    #[test]
    fn text_that_differs_from_done_is_an_error() {
        let (_, base) = recorded(ANSWER);
        let err = run(false, false, &["blue"], done_with(&base, "", "Blue")).unwrap_err();
        assert!(err.contains("answer differs"), "{err}");
        let err = run(true, false, &["a</think>b"], done_with(&base, "x", "b")).unwrap_err();
        assert!(err.contains("thinking differs"), "{err}");
    }

    #[test]
    fn a_cancelled_finish_the_kernel_did_not_ask_for_is_an_error() {
        let (_, base) = recorded(ANSWER);
        let done = Done { finish: Finish::Cancelled, ..done_with(&base, "", "") };
        assert!(run(false, false, &[], done).unwrap_err().contains("cancelled"));
    }

    #[test]
    fn unparsed_calls_become_invalid_calls_and_an_unnamed_one_gets_a_name() {
        let (_, base) = recorded(TOOL);
        let mut done = done_with(&base, "", "");
        done.tool_calls = vec![
            ToolCall::Unparsed { name: Some("ls".into()), raw: "<function=ls>".into(), error: "no close".into() },
            ToolCall::Unparsed { name: None, raw: "<function=".into(), error: "no name".into() },
        ];
        let events = run(false, true, &[], done).unwrap();
        let [StreamEvent::ToolUseInvalid { id: a, name: n0, .. }, StreamEvent::ToolUseInvalid { id: b, name: n1, error, .. }, StreamEvent::Done { .. }] = events.as_slice() else {
            panic!("{events:?}")
        };
        assert_eq!((a.as_str(), n0.as_str(), b.as_str(), n1.as_str()), ("mk-test-0", "ls", "mk-test-1", UNNAMED_CALL));
        assert!(error.contains("did not parse") && error.contains("no name"), "{error}");
    }
}
