//! The in-flight strip: one row of the live band, always present, naming
//! every tool call of the current context that has not settled — running
//! with its elapsed time, or waiting on an ask with the ask's id and age.
//!
//! The row's height never changes, so the band never rebuilds for it; only
//! its text does. That is the whole reason it exists (`docs/tui.md`, "The
//! in-flight strip"). Pure: entries come from blocks and a clock, the line
//! from entries and a width.

use std::time::Duration;

use kaijutsu_types::{BlockId, BlockKind, BlockSnapshot, Status};
use ratatui::text::{Line, Span};

use crate::present::Palette;

/// What one unsettled tool call is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Doing {
    /// The tool is running, or was just authored and has no result yet.
    Running,
    /// A gate holds it: the ask's id when the result text names one.
    Waiting { ask: Option<String> },
}

/// One strip entry: the call's tool name, a short argument, what it is
/// doing, and how long since the call was authored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub tool: String,
    pub arg: String,
    pub doing: Doing,
    pub age: Duration,
}

/// Whether `block` belongs to the strip rather than the stream: an
/// unsettled `ToolCall`, or a `ToolResult` a gate is holding. A `Running`
/// result stays in the stream so its output streams on screen.
pub fn takes_from_stream(block: &BlockSnapshot) -> bool {
    if crate::render::is_settled(block) {
        return false;
    }
    match block.kind {
        BlockKind::ToolCall => true,
        BlockKind::ToolResult => matches!(block.status, Status::Waiting | Status::Pending),
        _ => false,
    }
}

/// The strip's entries from a context's unsettled blocks, in document
/// order. A waiting result refines its call's entry through `tool_call_id`;
/// a waiting result whose call already settled (or was never seen) is an
/// entry of its own.
pub fn entries<'a>(blocks: impl Iterator<Item = &'a BlockSnapshot>, now_millis: u64) -> Vec<Entry> {
    let mut out: Vec<(Option<BlockId>, Entry)> = Vec::new();
    for block in blocks {
        if !takes_from_stream(block) {
            continue;
        }
        let age = Duration::from_millis(now_millis.saturating_sub(block.created_at));
        match block.kind {
            BlockKind::ToolCall => out.push((
                Some(block.id),
                Entry {
                    tool: tool_label(block),
                    arg: short_arg(block.tool_input.as_deref().unwrap_or("")),
                    doing: Doing::Running,
                    age,
                },
            )),
            BlockKind::ToolResult => {
                let doing = Doing::Waiting { ask: ask_id(&block.content) };
                match block.tool_call_id.and_then(|call| out.iter_mut().find(|(id, _)| *id == Some(call))) {
                    Some((_, entry)) => entry.doing = doing,
                    None => out.push((
                        None,
                        Entry {
                            tool: tool_label(block),
                            arg: short_arg(&block.content),
                            doing,
                            age,
                        },
                    )),
                }
            }
            _ => {}
        }
    }
    out.into_iter().map(|(_, e)| e).collect()
}

/// How long one animation step lasts: the spinner turns and the running
/// region's ground breathes one step per `PHASE_MILLIS`.
pub const PHASE_MILLIS: u64 = 250;
/// Steps per cycle — the spinner's four glyphs.
pub const PHASES: u8 = 4;

/// The animation phase at `now_millis`, `0..PHASES`.
pub fn phase(now_millis: u64) -> u8 {
    u8::try_from((now_millis / PHASE_MILLIS) % u64::from(PHASES)).unwrap_or(0)
}

/// Whether any entry animates — the event loop redraws on the phase clock
/// only while this is true.
pub fn animating(entries: &[Entry]) -> bool {
    entries.iter().any(|e| e.doing == Doing::Running)
}

/// The strip row at `width`: every entry as its own region on a faint
/// ground, joined by a space of the strip's own ground, padded to the full
/// width so the ground reads as one row. Empty entries give an empty row of
/// the same ground. `phase` turns the running entries' spinner and breathes
/// their ground; a waiting entry is still.
pub fn strip_line(entries: &[Entry], width: u16, palette: &Palette, phase: u8) -> Line<'static> {
    let width = usize::from(width.max(1));
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for (i, entry) in entries.iter().enumerate() {
        let text = format!(" {} ", entry_text(entry, phase));
        let sep = if i == 0 { 0 } else { 1 };
        let cell = text.chars().count();
        if used + sep + cell > width {
            let left = width.saturating_sub(used + sep);
            if left > 1 {
                if sep == 1 {
                    spans.push(Span::styled(" ".to_string(), palette.strip()));
                }
                let clipped: String = text.chars().take(left - 1).collect::<String>() + "…";
                spans.push(Span::styled(clipped, palette.strip_region(&entry.doing, phase)));
                used = width;
            }
            break;
        }
        if sep == 1 {
            spans.push(Span::styled(" ".to_string(), palette.strip()));
        }
        spans.push(Span::styled(text, palette.strip_region(&entry.doing, phase)));
        used += sep + cell;
    }
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), palette.strip()));
    }
    Line::from(spans)
}

/// The running spinner, one glyph per phase.
const SPINNER: [char; PHASES as usize] = ['◐', '◓', '◑', '◒'];

/// `◐ shell cargo test -p x · 4s` / `⏳ shell_write · waiting on ask 01a0686d · 17h`.
fn entry_text(entry: &Entry, phase: u8) -> String {
    let age = crate::status::format_age(entry.age);
    let head = if entry.arg.is_empty() {
        entry.tool.clone()
    } else {
        format!("{} {}", entry.tool, entry.arg)
    };
    match &entry.doing {
        Doing::Running => format!("{} {head} · {age}", SPINNER[usize::from(phase % PHASES)]),
        Doing::Waiting { ask: Some(ask) } => format!("⏳ {head} · waiting on ask {} · {age}", short_ask(ask)),
        Doing::Waiting { ask: None } => format!("⏳ {head} · waiting · {age}"),
    }
}

/// `tool_name` when the block carries one, else the kind's own word.
fn tool_label(block: &BlockSnapshot) -> String {
    match block.tool_name.as_deref() {
        Some(name) if !name.is_empty() => name.rsplit('.').next().unwrap_or(name).to_string(),
        _ => match block.kind {
            BlockKind::ToolCall => "call".to_string(),
            _ => "result".to_string(),
        },
    }
}

/// The one argument a call's input amounts to, when it amounts to one: the
/// input is a JSON object whose only member is a string (`{"command": …}`,
/// `{"path": …}`), or a bare string. Whitespace is folded so a heredoc
/// reads on one line. `None` for anything with more shape than that — a
/// pair header then names the tool alone and the body prints whole
/// (`docs/tui.md`, "Conversation", the pair header).
pub fn one_line_arg(input: &str) -> Option<String> {
    let text = single_arg(input)?;
    let folded: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!folded.is_empty()).then_some(folded)
}

/// [`one_line_arg`]'s source string, unfolded — a caller that must know
/// whether the argument spans lines (a heredoc) reads this.
pub fn single_arg(input: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(input).ok()?;
    match value {
        serde_json::Value::String(s) => Some(s),
        serde_json::Value::Object(map) if map.len() == 1 => map.into_iter().next()?.1.as_str().map(str::to_string),
        _ => None,
    }
}

/// The first `MAX_ARG` characters of the call's argument on one line: a
/// shell command as typed when the input is `{"command": …}`, else the
/// raw input with its whitespace folded.
const MAX_ARG: usize = 32;
fn short_arg(input: &str) -> String {
    let text = serde_json::from_str::<serde_json::Value>(input)
        .ok()
        .and_then(|v| {
            ["command", "path", "pattern", "query"]
                .iter()
                .find_map(|k| v.get(k).and_then(|x| x.as_str()).map(str::to_string))
        })
        .unwrap_or_else(|| input.to_string());
    let folded: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if folded.chars().count() > MAX_ARG {
        folded.chars().take(MAX_ARG - 1).collect::<String>() + "…"
    } else {
        folded
    }
}

/// The ask id a gate's result text names (`ask <uuid>`), or `None`.
fn ask_id(text: &str) -> Option<String> {
    let mut rest = text;
    while let Some(i) = rest.find("ask ") {
        let cand: String = rest[i + 4..]
            .chars()
            .take_while(|c| c.is_ascii_hexdigit() || *c == '-')
            .collect();
        if cand.len() >= 8 {
            return Some(cand);
        }
        rest = &rest[i + 4..];
    }
    None
}

/// The first segment of a uuid, the same eight characters `kj ledger list`
/// keys on.
fn short_ask(ask: &str) -> &str {
    ask.split('-').next().unwrap_or(ask)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{BlockSnapshotBuilder, ContextId, PrincipalId, Role};

    fn call(ctx: ContextId, seq: u64, status: Status, tool: &str, input: &str, at: u64) -> BlockSnapshot {
        BlockSnapshotBuilder::new(BlockId::new(ctx, PrincipalId::new(), seq), BlockKind::ToolCall)
            .role(Role::Model)
            .status(status)
            .tool_name(tool)
            .tool_input(input)
            .content(input)
            .created_at(at)
            .build()
    }

    fn result(ctx: ContextId, seq: u64, status: Status, call: BlockId, text: &str, at: u64) -> BlockSnapshot {
        BlockSnapshotBuilder::new(BlockId::new(ctx, PrincipalId::new(), seq), BlockKind::ToolResult)
            .role(Role::Tool)
            .status(status)
            .tool_call_id(call)
            .content(text)
            .created_at(at)
            .build()
    }

    fn text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn a_running_call_is_one_entry_with_its_command_and_age() {
        let ctx = ContextId::new();
        let c = call(ctx, 1, Status::Running, "builtin.shell.shell", r#"{"command":"cargo test -p kaijutsu-kernel"}"#, 10_000);
        let entries = entries([&c].into_iter(), 14_000);
        assert_eq!(
            entries,
            vec![Entry {
                tool: "shell".to_string(),
                arg: "cargo test -p kaijutsu-kernel".to_string(),
                doing: Doing::Running,
                age: Duration::from_secs(4),
            }]
        );
        assert_eq!(
            text(&strip_line(&entries, 80, &Palette::builtin(), 0)).trim_end(),
            " ◐ shell cargo test -p kaijutsu-kernel · 4s"
        );
    }

    /// A gate's result refines its call's entry: `waiting on ask <id>`,
    /// not a second entry.
    #[test]
    fn a_waiting_result_names_its_ask_on_the_calls_entry() {
        let ctx = ContextId::new();
        let c = call(ctx, 7, Status::Waiting, "shell_write", r#"{"command":"cargo test"}"#, 0);
        let r = result(
            ctx,
            8,
            Status::Waiting,
            c.id,
            "gate for lfm2d-advisory is waiting on a human: ask 01a0686d-895e-7b31-aa17-e497ed849f67 (pending) — nothing was run.",
            0,
        );
        let entries = entries([&c, &r].into_iter(), 3_600_000);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].doing, Doing::Waiting { ask: Some("01a0686d-895e-7b31-aa17-e497ed849f67".to_string()) });
        let line = text(&strip_line(&entries, 80, &Palette::builtin(), 2));
        assert!(line.contains("⏳ shell_write cargo test · waiting on ask 01a0686d · 1h00m"), "{line:?}");
        assert!(!animating(&entries), "a held ask has nothing to animate");
    }

    /// A `Running` result is the tool's output streaming in; it stays in
    /// the stream, and a settled pair is nobody's business here.
    #[test]
    fn only_calls_and_held_results_leave_the_stream() {
        let ctx = ContextId::new();
        let c = call(ctx, 1, Status::Running, "shell", "{}", 0);
        assert!(takes_from_stream(&c));
        assert!(takes_from_stream(&result(ctx, 2, Status::Waiting, c.id, "gate …", 0)));
        assert!(!takes_from_stream(&result(ctx, 2, Status::Running, c.id, "test foo ... ok", 0)));
        assert!(!takes_from_stream(&call(ctx, 3, Status::Done, "shell", "{}", 0)));
        assert!(!takes_from_stream(&result(ctx, 4, Status::Done, c.id, "ok", 0)));
    }

    /// Two regions share the row; the row is always exactly `width` wide
    /// (the ground pads it), and an overflowing entry is clipped with `…`
    /// rather than wrapping the strip to two rows.
    #[test]
    fn the_strip_is_one_row_at_any_width() {
        let ctx = ContextId::new();
        let a = call(ctx, 1, Status::Running, "shell", r#"{"command":"cargo build"}"#, 0);
        let b = call(ctx, 2, Status::Running, "grep", r#"{"pattern":"fn main"}"#, 0);
        let entries = entries([&a, &b].into_iter(), 1000);
        assert_eq!(entries.len(), 2);
        for width in [12u16, 30, 80] {
            let line = strip_line(&entries, width, &Palette::builtin(), 0);
            assert_eq!(text(&line).chars().count(), usize::from(width), "width {width}");
        }
        let wide = text(&strip_line(&entries, 80, &Palette::builtin(), 0));
        assert!(wide.contains("◐ shell cargo build · 1s") && wide.contains("◐ grep fn main · 1s"), "{wide:?}");
        let empty = text(&strip_line(&[], 20, &Palette::builtin(), 0));
        assert_eq!(empty, " ".repeat(20));
    }

    /// A running entry turns its spinner and breathes its ground with the
    /// phase; the phase comes from the clock, one step per `PHASE_MILLIS`.
    #[test]
    fn a_running_entry_animates_with_the_phase() {
        let ctx = ContextId::new();
        let c = call(ctx, 1, Status::Running, "shell", "{}", 0);
        let entries = entries([&c].into_iter(), 1000);
        assert!(animating(&entries));
        let frames: Vec<Line<'static>> = (0..PHASES).map(|p| strip_line(&entries, 40, &Palette::builtin(), p)).collect();
        let glyphs: Vec<char> = frames.iter().map(|l| text(l).trim_start().chars().next().unwrap()).collect();
        assert_eq!(glyphs, SPINNER.to_vec());
        let grounds: Vec<_> = frames.iter().map(|l| l.spans[0].style.bg).collect();
        assert!(grounds.iter().any(|g| *g != grounds[0]), "the ground breathes: {grounds:?}");
        assert_eq!(phase(0), 0);
        assert_eq!(phase(PHASE_MILLIS), 1);
        assert_eq!(phase(PHASE_MILLIS * u64::from(PHASES)), 0);
    }

    #[test]
    fn one_line_arg_is_the_single_string_member_or_nothing() {
        assert_eq!(one_line_arg(r#"{"command":"kj ledger list"}"#).as_deref(), Some("kj ledger list"));
        assert_eq!(one_line_arg(r#"{"path":"a.rs"}"#).as_deref(), Some("a.rs"));
        assert_eq!(one_line_arg("\"bare\"").as_deref(), Some("bare"));
        assert_eq!(
            one_line_arg("{\"command\":\"cat <<'EOF'\\nline one\\nEOF\"}").as_deref(),
            Some("cat <<'EOF' line one EOF"),
            "a heredoc folds onto one line"
        );
        assert_eq!(one_line_arg(r#"{"path":"a.rs","range":"1:4"}"#), None, "two members is a shape, not an arg");
        assert_eq!(one_line_arg(r#"{"n":3}"#), None);
        assert_eq!(one_line_arg("not json"), None);
        assert_eq!(one_line_arg(r#"{"command":"   "}"#), None);
    }

    #[test]
    fn ask_ids_are_read_from_gate_text() {
        assert_eq!(
            ask_id("waiting on a human: ask 01a0686d-895e-7b31-aa17-e497ed849f67 (pending)").as_deref(),
            Some("01a0686d-895e-7b31-aa17-e497ed849f67")
        );
        assert_eq!(ask_id("please ask the human"), None);
    }
}
