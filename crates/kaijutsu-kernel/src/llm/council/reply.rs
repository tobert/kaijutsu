//! Turn a council v2 generation's tokens and `done` into [`StreamEvent`]s.
//!
//! Each token names its phase, so [`Reply::phased`] routes each piece by
//! it and reads no markers. Each phase is trimmed the way the server trims
//! it, so the streamed blocks hold exactly `done`'s text. [`Reply::done_v2`]
//! checks that at the end: a difference is an error, never a block that
//! disagrees with what the server holds as the reply.

use gogi_proto::generation::{Finish as V2Finish, GenerationDone, Phase as V2Phase, ReplyToolCall};
use kaijutsu_council::Json;

use crate::llm::stream::{CouncilUsageExtra, StreamEvent, UsageExtra};

/// The `ThinkingEnd` signature. The server has no signature; this sentinel
/// marks council reasoning as replayable, so the runtime and hydration send it back
/// as `reasoning_content` and the template decides what to keep. Opaque: only
/// its presence is read.
pub(super) const REASONING_SIGNATURE: &str = "mk-reasoning";

/// The name a call gets when the server could not read one.
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
    think: Block,
    answer: Block,
}

impl Reply {
    /// `thinking` is whether generation starts inside thinking.
    pub(super) fn new(thinking: bool) -> Self {
        Reply {
            phase: if thinking { Phase::Think } else { Phase::Answer },
            think: Block::new(Kind::Think),
            answer: Block::new(Kind::Answer),
        }
    }

    /// A council v2 piece in `phase`. A change of phase closes the block of
    /// the phase before it; a call's pieces and a phase this kernel does not
    /// know add no text.
    pub(super) fn phased(&mut self, phase: V2Phase, piece: &str) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        let phase = match phase {
            V2Phase::Reasoning => Phase::Think,
            V2Phase::Content => Phase::Answer,
            V2Phase::Call => Phase::Tool,
            V2Phase::Unknown => return out,
        };
        if phase != self.phase {
            match self.phase {
                Phase::Think => self.think.close(&mut out),
                Phase::Answer => self.answer.close(&mut out),
                Phase::Tool => {}
            }
            self.phase = phase;
        }
        self.text(piece, &mut out);
        out
    }

    fn text(&mut self, text: &str, out: &mut Vec<StreamEvent>) {
        match self.phase {
            Phase::Think => self.think.push(text, out),
            Phase::Answer => self.answer.push(text, out),
            Phase::Tool => {}
        }
    }

    /// The events that end a council v2 generation. Its calls keep the ids
    /// the server gave them, which the next turn's tool results name.
    /// `model` and `weight_hash` are the server's identity.
    pub(super) fn done_v2(
        self,
        done: GenerationDone,
        model: &str,
        weight_hash: &str,
        timing: Timing,
    ) -> Result<Vec<StreamEvent>, String> {
        let calls = done
            .tool_calls
            .into_iter()
            .map(|call| match call {
                ReplyToolCall::Parsed { id, name, input, .. } => {
                    let arguments = serde_json::from_value(serde_json::Value::Object(input))
                        .map_err(|e| format!("tool call {id} arguments do not decode: {e}"))?;
                    Ok(Call::Parsed { id, name, arguments })
                }
                ReplyToolCall::Unparsed { id, name, raw, error, .. } => Ok(Call::Unparsed { id, name, raw, error }),
            })
            .collect::<Result<Vec<_>, String>>()?;
        let extra = CouncilUsageExtra {
            kept: done.usage.kept,
            fed: done.usage.fed,
            first_token_ms: timing.first_token_ms,
            wall_ms: timing.wall_ms,
            seed: done.seed,
            model: model.to_string(),
            weight_hash: weight_hash.to_string(),
        };
        let usage = (done.usage.prompt, done.usage.completion, extra);
        // A reply cut at the context's end is cut like one at max_tokens.
        let stop_reason = match done.finish {
            V2Finish::EndTurn => "end_turn",
            V2Finish::ToolUse => "tool_use",
            V2Finish::MaxTokens | V2Finish::ContextFull => "max_tokens",
        };
        self.end(&done.reasoning, &done.content, stop_reason, calls, usage)
    }

    /// Closes the open block, checks the streamed text against the reply's,
    /// then emits the calls and `Done` with `(prompt, completion, extra)`.
    fn end(
        mut self,
        reasoning: &str,
        content: &str,
        stop_reason: &str,
        calls: Vec<Call>,
        (prompt, completion, extra): (u64, u64, CouncilUsageExtra),
    ) -> Result<Vec<StreamEvent>, String> {
        let mut out = Vec::new();
        self.think.close(&mut out);
        self.answer.close(&mut out);
        if self.think.text != reasoning {
            return Err(mismatch("thinking", &self.think.text, reasoning));
        }
        if self.answer.text != content {
            return Err(mismatch("answer", &self.answer.text, content));
        }
        for call in calls {
            out.push(match call {
                Call::Parsed { id, name, arguments } => StreamEvent::ToolUse {
                    id,
                    name,
                    input: serde_json::to_value(&arguments).map_err(|e| format!("tool call arguments: {e}"))?,
                },
                Call::Unparsed { id, name: Some(name), raw, error } => {
                    StreamEvent::ToolUseInvalid { id, name, arguments: raw, error }
                }
                Call::Unparsed { id, name: None, raw, error } => StreamEvent::ToolUseInvalid {
                    id,
                    name: UNNAMED_CALL.into(),
                    arguments: raw,
                    error: format!("the tool name did not parse; {error}"),
                },
            });
        }
        out.push(StreamEvent::Done {
            stop_reason: Some(stop_reason.into()),
            input_tokens: Some(prompt),
            output_tokens: Some(completion),
            extra: Some(UsageExtra::Council(extra)),
        });
        Ok(out)
    }
}

/// A call the reply made, with the id the kernel records it under.
enum Call {
    Parsed { id: String, name: String, arguments: Json },
    Unparsed { id: String, name: Option<String>, raw: String, error: String },
}

fn mismatch(what: &str, streamed: &str, done: &str) -> String {
    format!(
        "the streamed {what} differs from the council server's done text, so the block would not match the \
         reply the server holds; the phase trim in the kernel's council reply mapping no longer matches the \
         server (streamed {streamed:?}, done {done:?})"
    )
}

/// Whitespace the server trims from each part, which a part's pieces and
/// `done`'s text may differ by at either end (gogi's `is_edge_space`).
fn is_space(c: char) -> bool {
    gogi_proto::generation::is_edge_space(c)
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
    use super::*;

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

    fn v2_done(reasoning: &str, content: &str, finish: &str) -> GenerationDone {
        serde_json::from_value(serde_json::json!({"model": "m", "content": content, "reasoning": reasoning, "tool_calls": [],
            "finish": finish, "usage": {"prompt": 9, "kept": 4, "fed": 5, "completion": 6, "reasoning": 2},
            "head": null, "snapshots": [], "seed": 5}))
        .unwrap()
    }

    fn run(thinking: bool, pieces: &[(V2Phase, &str)], done: GenerationDone) -> Result<Vec<StreamEvent>, String> {
        let mut reply = Reply::new(thinking);
        let mut out: Vec<StreamEvent> = pieces.iter().flat_map(|(phase, piece)| reply.phased(*phase, piece)).collect();
        out.extend(reply.done_v2(done, "m", "w", Timing::default())?);
        Ok(out)
    }

    /// A council v2 piece goes to the block its phase names, with no
    /// marker read: a reasoning piece that spells `</think>` stays
    /// thinking, and a call's pieces add no text. `finish` maps to the
    /// kernel's stop reasons, and `done`'s seed is kept.
    ///
    /// Falsified by routing pieces through a marker split, or a
    /// `context_full` reply that reads as a finished turn.
    #[test]
    fn v2_pieces_follow_their_phase_and_finish_maps_to_a_stop_reason() {
        let pieces = [(V2Phase::Reasoning, "why </think>"), (V2Phase::Content, " Yes"), (V2Phase::Call, "ls(")];
        let out = run(true, &pieces, v2_done("why </think>", "Yes", "context_full")).unwrap();
        assert_eq!(texts(&out), ("why </think>".into(), "Yes".into()));
        let StreamEvent::Done { stop_reason, extra: Some(UsageExtra::Council(x)), .. } = out.last().unwrap() else { panic!("{out:?}") };
        assert_eq!((stop_reason.as_deref(), x.seed), (Some("max_tokens"), Some(5)));
        for (finish, want) in [("end_turn", "end_turn"), ("tool_use", "tool_use"), ("max_tokens", "max_tokens")] {
            let events = Reply::new(false).done_v2(v2_done("", "", finish), "m", "w", Timing::default()).unwrap();
            assert!(matches!(events.last(), Some(StreamEvent::Done { stop_reason: Some(s), .. }) if s == want), "{finish}: {events:?}");
        }
    }

    /// Thinking closes before the answer opens, and its end carries the
    /// signature that marks it replayable.
    #[test]
    fn thinking_closes_before_the_answer_and_is_signed() {
        let pieces = [(V2Phase::Reasoning, "six times seven"), (V2Phase::Content, "42")];
        let events = run(true, &pieces, v2_done("six times seven", "42", "end_turn")).unwrap();
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
    fn inner_white_space_is_kept_and_python_separators_are_trimmed() {
        let pieces = [(V2Phase::Content, "\u{1f}a"), (V2Phase::Content, "\n\n"), (V2Phase::Content, "b"), (V2Phase::Content, " \u{1c}")];
        let events = run(false, &pieces, v2_done("", "a\n\nb", "end_turn")).unwrap();
        assert_eq!(texts(&events).1, "a\n\nb");
    }

    #[test]
    fn text_that_differs_from_done_is_an_error() {
        let err = run(false, &[(V2Phase::Content, "blue")], v2_done("", "Blue", "end_turn")).unwrap_err();
        assert!(err.contains("answer differs"), "{err}");
        let pieces = [(V2Phase::Reasoning, "a"), (V2Phase::Content, "b")];
        let err = run(true, &pieces, v2_done("x", "b", "end_turn")).unwrap_err();
        assert!(err.contains("thinking differs"), "{err}");
    }

    /// Calls keep the server's ids. An unparsed call becomes an invalid
    /// call, and one whose name did not parse gets a name.
    #[test]
    fn calls_keep_their_ids_and_unparsed_ones_become_invalid_calls() {
        let mut done = v2_done("", "", "tool_use");
        let input = serde_json::json!({"path": "/tmp"}).as_object().unwrap().clone();
        done.tool_calls = vec![
            ReplyToolCall::Parsed { id: "c0".into(), name: "ls".into(), input, stats: None },
            ReplyToolCall::Unparsed { id: "c1".into(), name: Some("ls".into()), raw: "<function=ls>".into(), error: "no close".into(), stats: None },
            ReplyToolCall::Unparsed { id: "c2".into(), name: None, raw: "<function=".into(), error: "no name".into(), stats: None },
        ];
        let events = run(false, &[], done).unwrap();
        let [StreamEvent::ToolUse { id: a, name: n0, input }, StreamEvent::ToolUseInvalid { id: b, name: n1, .. }, StreamEvent::ToolUseInvalid { id: c, name: n2, error, .. }, StreamEvent::Done { .. }] =
            events.as_slice()
        else {
            panic!("{events:?}")
        };
        assert_eq!((a.as_str(), n0.as_str(), input), ("c0", "ls", &serde_json::json!({"path": "/tmp"})));
        assert_eq!((b.as_str(), n1.as_str(), c.as_str(), n2.as_str()), ("c1", "ls", "c2", UNNAMED_CALL));
        assert!(error.contains("did not parse") && error.contains("no name"), "{error}");
    }
}
