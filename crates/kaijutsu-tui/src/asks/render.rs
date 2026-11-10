//! Rendering: the ask card, the ledger view and one ask in full, as `Line`s
//! from rows and a width, with the text helpers they share.

use ratatui::text::{Line, Span};

use crate::present::Palette;

use super::fold::context_facts;
use super::state::{LedgerRow, Offer, RedeemedMark, filtered_rows};

/// One ask, resolved to what the card needs to render — the fields
/// [`crate::app::App`] already has to hand (context label/type from
/// `ContextInfo`, the rest from [`kaijutsu_types::AskDetail`]).
pub struct AskCard<'a> {
    pub request_id: &'a str,
    /// The ledger `tool` column, e.g. `"shell_write"` — `docs/tui.md` calls
    /// this the ask's "hook" in the figure; the field it reads is `tool`.
    pub hook: &'a str,
    pub context_label: &'a str,
    pub performer: &'a str,
    pub statement: &'a str,
    pub asker: Option<&'a str>,
    pub reviewer: Option<&'a str>,
    /// The keys the viewer has on this ask.
    pub offer: Offer,
    /// Whether the card's keys act yet. A disarmed card shows no keys; its
    /// key line says keys still go to the draft.
    pub armed: bool,
}

/// The key line of a card whose keys do not act yet. Text rather than a
/// color, so it reads the same with `NO_COLOR`.
pub const ARMING_KEY_LINE: &str = "  arming… keys still go to the draft";

/// `⚠ ask <id>  <hook>  from <context> (<type>)`, the statement flush-left,
/// then the key lines — `docs/tui.md`'s Asks figure. The key lines wrap by
/// width, and a disarmed card keeps the armed card's line count so arming
/// never moves the transcript.
pub fn render_ask_card(card: &AskCard<'_>, width: u16, palette: &Palette) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        format!(
            "⚠ ask {}  {}  from {} ({}){}{}",
            card.request_id,
            card.hook,
            card.context_label,
            card.performer,
            card.asker.map(|name| format!("  asker {name}")).unwrap_or_default(),
            card.reviewer.map(|name| format!("  reviewer {name}")).unwrap_or_default(),
        ),
        palette.warning(),
    )));
    for row in wrap_plain(card.statement, width.saturating_sub(2)) {
        lines.push(Line::from(Span::styled(format!("  {row}"), palette.status())));
    }
    let mut key_rows = wrap_items(&card_key_items(card), width.saturating_sub(2));
    if !card.armed {
        let held = key_rows.len();
        key_rows = vec![ARMING_KEY_LINE.trim_start().to_string()];
        key_rows.resize(held.max(1), String::new());
    }
    for row in key_rows {
        lines.push(Line::from(Span::styled(format!("  {row}"), palette.divider())));
    }
    lines
}

/// The card's keys for this viewer, one item per key.
fn card_key_items(card: &AskCard<'_>) -> Vec<String> {
    let offer = card.offer;
    let mut items = Vec::new();
    if offer.review {
        let who = card.asker.unwrap_or("the performer");
        items.push("[a]llow once".to_string());
        if offer.always {
            items.push("[A]llow always (global)".to_string());
        }
        if offer.remember {
            items.push(format!("[c] always for {who}"));
            items.push(format!("[C] {who}, command family"));
        }
        items.push("[d]eny".to_string());
        if offer.escalate {
            items.push("[e]scalate".to_string());
        }
    } else {
        items.push(format!("waiting for reviewer {}", card.reviewer.unwrap_or("(unassigned)")));
        if offer.take_over {
            items.push("[t]ake over".to_string());
        }
    }
    items.push("[v]iew ledger".to_string());
    items.push("Esc aside".to_string());
    items
}

/// Items joined by two spaces, wrapped between items at `width`.
fn wrap_items(items: &[String], width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut current = String::new();
    for item in items {
        if current.is_empty() {
            current.push_str(item);
        } else if current.chars().count() + 2 + item.chars().count() <= width {
            current.push_str("  ");
            current.push_str(item);
        } else {
            rows.push(std::mem::take(&mut current));
            current.push_str(item);
        }
    }
    rows.push(current);
    rows
}

/// `LEDGER                          pending 2   answered 7` down through the
/// PENDING and ANSWERED sections and the key line — `docs/tui.md`'s ledger
/// figure. `answered {N}` stands in for the figure's `answered today {N}`:
/// every answered row this call is handed counts, not just today's.
pub fn render_ledger(
    rows: &[LedgerRow],
    filter: &str,
    selected: usize,
    width: u16,
    palette: &Palette,
) -> Vec<Line<'static>> {
    let width_usize = usize::from(width.max(1));
    let pending_total = rows.iter().filter(|r| matches!(r, LedgerRow::Pending(_))).count();
    let answered_total = rows.len() - pending_total;
    let visible = filtered_rows(rows, filter);

    let mut lines = Vec::new();
    let header_right = format!("pending {pending_total}   answered {answered_total}");
    let header_left = "LEDGER".to_string();
    let pad = width_usize.saturating_sub(header_left.chars().count() + header_right.chars().count());
    lines.push(Line::from(Span::styled(
        format!("{header_left}{}{header_right}", " ".repeat(pad.max(1))),
        palette.status(),
    )));

    let mut section: Option<bool> = None; // Some(true) = pending, Some(false) = answered
    for (idx, row) in visible.iter().enumerate() {
        let is_pending = matches!(row, LedgerRow::Pending(_));
        if section != Some(is_pending) {
            section = Some(is_pending);
            lines.push(Line::from(Span::styled(
                if is_pending { "PENDING" } else { "ANSWERED" }.to_string(),
                palette.divider(),
            )));
        }
        let text = match row {
            LedgerRow::Pending(r) => format!(
                "! {:<36} {:>6}  {:<12} {:<10} {:<14} {:<12} {:<12} {}",
                r.request_id,
                r.age.as_deref().unwrap_or("—"),
                clip(&r.context_label, 12),
                clip(&r.performer, 10),
                clip(&r.hook, 14),
                clip(r.asker.as_deref().unwrap_or("—"), 12),
                clip(r.reviewer.as_deref().unwrap_or("—"), 12),
                r.statement,
            ),
            LedgerRow::Answered(r) => format!(
                "  {:<36} {:>6}  {:<12} {:<14} {:<8} {:<16} {}",
                r.request_id,
                r.time.as_deref().unwrap_or("—"),
                clip(&r.context_label, 12),
                clip(r.decision.as_deref().unwrap_or("—"), 14),
                clip(r.principal.as_deref().unwrap_or("—"), 8),
                r.redeemed.text(),
                r.statement,
            ),
        };
        let style = if idx == selected { palette.warning() } else { palette.status() };
        lines.push(Line::from(Span::styled(truncate_plain(&text, width_usize), style)));
    }
    if visible.is_empty() {
        lines.push(Line::from(Span::styled(
            if filter.is_empty() {
                "(nothing pending or answered)".to_string()
            } else {
                format!("(nothing matches {filter:?})")
            },
            palette.divider(),
        )));
    }

    let hints = ledger_hints(visible.get(selected).copied(), width_usize);
    lines.push(Line::from(Span::styled(hints, palette.divider())));
    lines
}

/// The ledger's key line: the selected pending row's own keys (none on
/// nothing or an answered row), then navigation. The long wording when it fits `width`, else the short one,
/// cut to `width` if even that is wider.
fn ledger_hints(selected: Option<&LedgerRow>, width: usize) -> String {
    let mut items: Vec<(String, String)> = Vec::new();
    let mut add = |full: &str, short: &str| items.push((full.to_string(), short.to_string()));
    if let Some(LedgerRow::Pending(row)) = selected {
        let who = row.asker.as_deref().unwrap_or("the performer");
        if row.offer.review {
            add("a allow once", "a allow once");
            if row.offer.always {
                add("A allow always (global)", "A global");
            }
            if row.offer.remember {
                add(&format!("c always for {who}"), &format!("c {who}"));
                add(&format!("C {who}, command family"), &format!("C {who} family"));
            }
            add("d deny", "d deny");
        }
        if row.offer.escalate {
            add("e escalate", "e escalate");
        }
        if row.offer.take_over {
            add("t take over", "t take over");
        }
    }
    add("Enter show", "Enter");
    add("j/k move", "j/k");
    add("/ filter", "/");
    add("Esc back", "Esc back");
    let full = items.iter().map(|(full, _)| full.as_str()).collect::<Vec<_>>().join("  ");
    if full.chars().count() <= width {
        return full;
    }
    let short = items.iter().map(|(_, short)| short.as_str()).collect::<Vec<_>>().join("  ");
    truncate_plain(&short, width)
}

/// One ask shown in full (`Enter` on a ledger row, or the ask card's
/// `[v]iew ledger` having answered nothing yet): every field
/// [`kaijutsu_types::AskDetail`] carries, laid out one per line, including
/// the env snapshot an approval runs with.
pub fn render_ask_detail(detail: &AskDetailView<'_>, width: u16, palette: &Palette) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut lines = vec![
        Line::from(Span::styled(format!("ask:        {}", detail.request_id), palette.status())),
        Line::from(Span::styled(format!("status:     {}", detail.status), palette.status())),
        Line::from(Span::styled(format!("origin:     {}", detail.origin), palette.status())),
        Line::from(Span::styled(
            format!("context:    {} ({})", detail.context_label, detail.performer),
            palette.status(),
        )),
    ];
    if let Some(reason) = detail.publication_abandoned {
        lines.push(Line::from(Span::styled("publication: abandoned", palette.warning())));
        for row in wrap_plain(reason, width) {
            lines.push(Line::from(Span::styled(row, palette.warning())));
        }
    }
    if let Some(hook) = detail.hook {
        lines.push(Line::from(Span::styled(format!("hook:       {hook}"), palette.status())));
    }
    if let Some(requester) = detail.requester {
        lines.push(Line::from(Span::styled(format!("requester:  {requester}"), palette.status())));
    }
    if let Some(asker) = detail.asker {
        lines.push(Line::from(Span::styled(format!("asker:      {asker}"), palette.status())));
    }
    if let Some(reviewer) = detail.reviewer {
        lines.push(Line::from(Span::styled(format!("reviewer:   {reviewer}"), palette.status())));
    }
    for statement in detail.statements {
        for row in wrap_plain(statement, width.saturating_sub(12)) {
            lines.push(Line::from(Span::styled(format!("statement:  {row}"), palette.status())));
        }
    }
    if let Some(source) = detail.exec_source {
        lines.push(Line::from(Span::styled(format!("exec_source: {source}"), palette.status())));
    }
    if let Some(cwd) = detail.cwd {
        lines.push(Line::from(Span::styled(format!("cwd:        {cwd}"), palette.status())));
    }
    for (name, value) in detail.env {
        let text = match value {
            Some(v) => format!("env:        {name}={v}"),
            None => format!("env:        {name} unset"),
        };
        lines.push(Line::from(Span::styled(text, palette.divider())));
    }
    lines.push(Line::from(Span::styled(
        format!("redeemed:   {}", detail.redeemed.text()),
        palette.status(),
    )));
    lines.push(Line::from(Span::styled(
        if matches!(detail.status, "pending" | "claimed") { "a allow once  A allow always  d deny  Esc back" } else { "Esc back" }.to_string(),
        palette.divider(),
    )));
    lines
}

/// What [`render_ask_detail`] needs, resolved by the caller from
/// [`kaijutsu_types::AskDetail`] plus the context label/type
/// [`crate::app::App`] already has.
pub struct AskDetailView<'a> {
    pub request_id: &'a str,
    pub status: &'a str,
    pub origin: &'a str,
    pub hook: Option<&'a str>,
    pub context_label: &'a str,
    pub performer: &'a str,
    pub requester: Option<&'a str>,
    pub asker: Option<&'a str>,
    pub reviewer: Option<&'a str>,
    pub statements: &'a [String],
    pub exec_source: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub env: &'a [(String, Option<String>)],
    pub redeemed: RedeemedMark,
    pub publication_abandoned: Option<&'a str>,
}

/// Cut a string to fit a column, marking the cut with `…`.
fn clip(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else if width == 0 {
        String::new()
    } else {
        let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Cut a whole line to `width` columns, marking the cut with `…` — the
/// table-row analogue of [`clip`], applied after a row's columns are already
/// joined.
fn truncate_plain(s: &str, width: usize) -> String {
    clip(s, width)
}

/// Greedy word wrap over plain text, one style throughout — the asks/ledger
/// surfaces render flush-left text, never a model's markdown
/// (`docs/tui.md`, "Surfaces"), so this needs none of `present.rs`'s
/// per-span wrap machinery.
fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    for source_line in text.lines() {
        if source_line.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        for word in source_line.split(' ') {
            if cur.is_empty() {
                cur.push_str(word);
            } else if cur.chars().count() + 1 + word.chars().count() <= width {
                cur.push(' ');
                cur.push_str(word);
            } else {
                out.push(std::mem::take(&mut cur));
                cur.push_str(word);
            }
        }
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// The overlay an open ask card or the ledger view draws over the foot of
/// the transcript, above the band — the same treatment the picker gets
/// (`crate::render::overlay_lines`). `None` when neither is open.
pub fn active_view_lines(app: &crate::app::App, width: u16) -> Option<Vec<Line<'static>>> {
    if let Some(card) = &app.ask_card {
        let (context_label, performer) = context_facts(app, card.context_id);
        let summary = &card.detail.summary;
        let statement = summary.statements.first().map(String::as_str).unwrap_or(summary.description.as_str());
        let view = AskCard {
            request_id: &card.request_id,
            hook: card.detail.tool.as_deref().unwrap_or("-"),
            context_label: &context_label,
            performer: &performer,
            statement,
            asker: summary.performer.as_ref().map(|who| who.name.as_str()),
            reviewer: summary.reviewer.as_ref().map(|who| who.name.as_str()),
            offer: Offer::of(summary, app.principal),
            armed: app.ask_card_armed(),
        };
        return Some(render_ask_card(&view, width, &app.palette));
    }
    if let Some(view) = &app.ledger_view {
        if let Some(detail) = &view.detail {
            let summary = &detail.summary;
            let (context_label, performer) = summary
                .context_id
                .map(|ctx| context_facts(app, ctx))
                .unwrap_or_else(|| ("(unknown)".to_string(), crate::app::NO_PERFORMER.to_string()));
            let env: Vec<(String, Option<String>)> = detail
                .env
                .iter()
                .map(|entry| (entry.name.clone(), entry.value.clone()))
                .collect();
            let view = AskDetailView {
                request_id: &summary.request_id,
                status: summary.status.as_str(),
                origin: summary.origin.as_str(),
                hook: detail.tool.as_deref(),
                context_label: &context_label,
                performer: &performer,
                requester: summary.requester.as_ref().map(|who| who.name.as_str()),
                asker: summary.performer.as_ref().map(|who| who.name.as_str()),
                reviewer: summary.reviewer.as_ref().map(|who| who.name.as_str()),
                statements: &summary.statements,
                exec_source: detail.exec_source.as_deref(),
                cwd: detail.cwd.as_deref(),
                env: &env,
                publication_abandoned: detail.publication_abandoned.as_deref(),
                redeemed: match detail.redeemed_at_ms {
                    Some(at) => RedeemedMark::At(crate::render::wallclock(at as u64)),
                    None => RedeemedMark::Never,
                },
            };
            return Some(render_ask_detail(&view, width, &app.palette));
        }
        return Some(render_ledger(&view.rows, &view.filter, view.selected, width, &app.palette));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::fixtures::{REVIEWER, answered, app_with_program_card, pending, reviewer, row_with};
    use crate::asks::AnsweredRow;
    use kaijutsu_types::PrincipalId;

    #[test]
    fn the_ask_card_renders_the_figures_shape() {
        let card = AskCard {
            request_id: "01a04eb6",
            hook: "shell_write",
            context_label: "kaijutsu",
            performer: "coder",
            statement: "rm -rf ~/src/wt/kaish-arith",
            asker: Some("coder"),
            reviewer: Some("amy"),
            offer: REVIEWER,
            armed: true,
        };
        let lines = render_ask_card(&card, 80, &Palette::builtin());
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(text[0], "⚠ ask 01a04eb6  shell_write  from kaijutsu (coder)  asker coder  reviewer amy");
        assert_eq!(text[1], "  rm -rf ~/src/wt/kaish-arith");
        assert_eq!(text[2], "  [a]llow once  [A]llow always (global)  [d]eny  [v]iew ledger  Esc aside");
    }

    /// A card that is not armed yet shows no keys, only that typed keys
    /// still go to the draft; the line count is the armed card's, so
    /// arming never moves the transcript.
    #[test]
    fn a_disarmed_ask_card_says_so_instead_of_offering_keys() {
        for can_review in [true, false] {
            let card = AskCard {
                request_id: "01a04eb6", hook: "shell_write", context_label: "kaijutsu", performer: "coder",
                statement: "rm -rf ~/src/wt/kaish-arith", asker: None, reviewer: None, offer: if can_review { REVIEWER } else { Offer::default() }, armed: false,
            };
            let text: Vec<String> = render_ask_card(&card, 80, &Palette::builtin()).iter().map(line_text).collect();
            assert!(text.iter().any(|line| line == ARMING_KEY_LINE), "{text:?}");
            assert!(!text.iter().any(|line| line.contains("llow")), "{text:?}");
            let armed = AskCard { armed: true, ..card };
            assert_eq!(render_ask_card(&armed, 80, &Palette::builtin()).len(), text.len());
        }
    }

    #[test]
    fn a_long_statement_wraps_under_the_ask_card_header() {
        let card = AskCard {
            request_id: "id",
            hook: "shell_write",
            context_label: "kaijutsu",
            performer: "coder",
            statement: "one two three four five six seven eight nine ten",
            asker: None,
            reviewer: None,
            offer: REVIEWER,
            armed: true,
        };
        let lines = render_ask_card(&card, 24, &Palette::builtin());
        // Header, N wrapped statement lines, key line.
        assert!(lines.len() > 3, "expected the statement to wrap: {lines:?}");
    }

    fn key_rows(offer: Offer, width: u16, armed: bool) -> Vec<String> {
        let card = AskCard {
            request_id: "id", hook: "shell_write", context_label: "kaijutsu", performer: "coder",
            statement: "rm -rf", asker: Some("coder"), reviewer: Some("lead"), offer, armed,
        };
        render_ask_card(&card, width, &Palette::builtin()).iter().map(line_text).skip(2).collect()
    }

    #[test]
    fn the_card_renders_each_viewers_keys() {
        let all = Offer { review: true, always: true, remember: true, escalate: true, take_over: false };
        assert_eq!(
            key_rows(all, 200, true),
            vec!["  [a]llow once  [A]llow always (global)  [c] always for coder  [C] coder, command family  [d]eny  [e]scalate  [v]iew ledger  Esc aside"]
        );
        let no_program = Offer { remember: false, ..all };
        assert_eq!(
            key_rows(no_program, 200, true),
            vec!["  [a]llow once  [A]llow always (global)  [d]eny  [e]scalate  [v]iew ledger  Esc aside"]
        );
        let drift = Offer { always: false, remember: false, ..all };
        assert_eq!(
            key_rows(drift, 200, true),
            vec!["  [a]llow once  [d]eny  [e]scalate  [v]iew ledger  Esc aside"],
            "a drift ask offers no standing rule"
        );
        let claimed = Offer { escalate: false, ..all };
        assert!(!key_rows(claimed, 200, true)[0].contains("[e]"));
        let root = Offer { take_over: true, ..Offer::default() };
        assert_eq!(key_rows(root, 200, true), vec!["  waiting for reviewer lead  [t]ake over  [v]iew ledger  Esc aside"]);
        assert_eq!(
            key_rows(Offer::default(), 200, true),
            vec!["  waiting for reviewer lead  [v]iew ledger  Esc aside"]
        );
    }

    /// The reviewer's key line wraps by width, between keys; a disarmed
    /// card keeps the same number of rows.
    #[test]
    fn the_key_line_wraps_between_keys_and_arming_keeps_the_row_count() {
        let all = Offer { review: true, always: true, remember: true, escalate: true, take_over: false };
        let rows = key_rows(all, 50, true);
        assert!(rows.len() > 1, "{rows:?}");
        assert!(rows.iter().all(|row| row.chars().count() <= 50), "{rows:?}");
        let joined = rows.iter().map(|row| row.trim()).collect::<Vec<_>>().join("  ");
        assert!(joined.contains("[C] coder, command family  [d]eny"), "{joined:?}");
        let disarmed = key_rows(all, 50, false);
        assert_eq!(disarmed.len(), rows.len());
        assert_eq!(disarmed[0], ARMING_KEY_LINE);
    }

    /// The ask card wraps by width: a narrow screen renders more rows than
    /// a wide one, which is what the overlay's own crop-from-the-front has
    /// to survive (`render::draw_screen`).
    #[test]
    fn the_ask_card_wraps_by_width() {
        let card = AskCard {
            request_id: "id",
            hook: "shell_write",
            context_label: "kaijutsu",
            performer: "coder",
            statement: "one two three four five six seven eight nine ten eleven twelve",
            asker: None,
            reviewer: None,
            offer: REVIEWER,
            armed: true,
        };
        let narrow_rendered = render_ask_card(&card, 16, &Palette::builtin()).len();
        let wide_rendered = render_ask_card(&card, u16::MAX, &Palette::builtin()).len();
        assert!(
            narrow_rendered > wide_rendered,
            "a narrow width should wrap into more lines than u16::MAX: narrow {narrow_rendered} wide {wide_rendered}"
        );
    }

    /// A ledger row is truncated to width, never wrapped, so its row count
    /// does not depend on width.
    #[test]
    fn the_ledger_row_count_is_width_independent() {
        let rows = vec![
            pending("p1", "kaijutsu"),
            pending("p2", "scorer"),
            answered("a1", "kaijutsu", RedeemedMark::Never),
        ];
        let rendered_40 = render_ledger(&rows, "", 0, 40, &Palette::builtin()).len();
        let rendered_200 = render_ledger(&rows, "", 0, 200, &Palette::builtin()).len();
        assert_eq!(rendered_40, rendered_200, "row count should not depend on width");
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn the_ledger_view_carries_both_sections_and_the_key_line() {
        let rows = vec![
            pending("p1", "kaijutsu"),
            pending("p2", "scorer"),
            answered("a1", "kaijutsu", RedeemedMark::At("13:58".to_string())),
        ];
        let lines = render_ledger(&rows, "", 0, 100, &Palette::builtin());
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(text[0].starts_with("LEDGER"), "got {text:?}");
        assert!(text[0].contains("pending 2"), "got {text:?}");
        assert!(text[0].contains("answered 1"), "got {text:?}");
        assert!(text.iter().any(|l| l == "PENDING"));
        assert!(text.iter().any(|l| l == "ANSWERED"));
        assert!(text.iter().any(|l| l.contains("p1") && l.contains("shell_write")));
        assert!(text.iter().any(|l| l.contains("a1") && l.contains("redeemed 13:58")));
        assert_eq!(text.last().unwrap(), "a allow once  A allow always (global)  d deny  Enter show  j/k move  / filter  Esc back");
    }

    #[test]
    fn never_redeemed_renders_as_a_dash() {
        let rows = vec![answered("a1", "kaish", RedeemedMark::Never)];
        let lines = render_ledger(&rows, "", 0, 100, &Palette::builtin());
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(text.iter().any(|l| l.trim_end().ends_with('—') || l.contains(" —  ")), "got {text:?}");
    }

    #[test]
    fn a_filter_with_no_match_says_so() {
        let rows = vec![pending("p1", "kaijutsu")];
        let lines = render_ledger(&rows, "nope", 0, 100, &Palette::builtin());
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(text.iter().any(|l| l.contains("nothing matches")), "got {text:?}");
    }

    #[test]
    fn the_ask_detail_view_shows_inputs_and_publication_disposition() {
        let env = vec![
            ("TARGET".to_string(), Some("kaish-arith".to_string())),
            ("FORCE".to_string(), None),
        ];
        let detail = AskDetailView {
            request_id: "01a04eb6",
            status: "pending",
            origin: "shell_gate",
            hook: Some("shell_write"),
            context_label: "kaijutsu",
            performer: "coder",
            requester: Some("amy"),
            asker: Some("coder"),
            reviewer: Some("amy"),
            statements: &["rm -rf ~/src/wt/kaish-arith".to_string()],
            exec_source: Some("kaish"),
            cwd: Some("/home/amy/src/wt/kaish-arith"),
            env: &env,
            redeemed: RedeemedMark::Never,
            publication_abandoned: None,
        };
        let lines = render_ask_detail(&detail, 100, &Palette::builtin());
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(text.iter().any(|l| l.contains("cwd:") && l.contains("kaish-arith")));
        assert!(text.iter().any(|l| l == "env:        TARGET=kaish-arith"));
        assert!(text.iter().any(|l| l == "env:        FORCE unset"));
        assert!(text.iter().any(|l| l.contains("redeemed:   —")));
        assert!(text.iter().any(|l| l == "asker:      coder"));
        assert!(text.iter().any(|l| l == "reviewer:   amy"));
        for status in ["allowed", "denied", "abandoned"] {
            let retired = AskDetailView { status, redeemed: RedeemedMark::At("12:00".into()),
                publication_abandoned: Some("Caller stopped. Source did not run."), ..detail };
            let text: Vec<String> = render_ask_detail(&retired, 45, &Palette::builtin()).iter().map(line_text).collect();
            assert!(text.iter().any(|line| line.contains(&format!("status:     {status}"))));
            assert!(text.iter().any(|line| line.contains("publication: abandoned")), "{text:?}");
            assert!(text.join(" ").contains("Source did not run."), "{text:?}");
            assert_eq!(text.last().unwrap(), "Esc back", "an answered ask offers no decision keys");
        }
    }

    #[test]
    fn wrap_plain_breaks_at_the_last_space_that_fits() {
        assert_eq!(
            wrap_plain("one two three", 7),
            vec!["one two".to_string(), "three".to_string()]
        );
    }

    #[test]
    fn the_card_in_the_overlay_reads_the_viewers_offer_from_the_summary() {
        let (app, _) = app_with_program_card(|ask, me| {
            ask.reviewer = reviewer(PrincipalId::new(), "lead");
            ask.lineage_root = reviewer(me, "amy");
        });
        let mut app = app;
        let t0 = std::time::Instant::now();
        app.tick_ask_arming(t0);
        assert!(app.tick_ask_arming(t0 + std::time::Duration::from_millis(500)));
        let lines = active_view_lines(&app, 200).expect("a card");
        let last = line_text(lines.last().unwrap());
        assert_eq!(last, "  waiting for reviewer lead  [t]ake over  [v]iew ledger  Esc aside");
    }

    #[test]
    fn the_ledger_key_line_lists_the_selected_rows_keys() {
        let line = |offer: Offer, width: u16| {
            let rows = vec![row_with("p-1", offer)];
            line_text(render_ledger(&rows, "", 0, width, &Palette::builtin()).last().unwrap())
        };
        let full = Offer { review: true, always: true, remember: true, escalate: true, take_over: false };
        assert_eq!(
            line(full, 200),
            "a allow once  A allow always (global)  c always for coder  C coder, command family  d deny  e escalate  Enter show  j/k move  / filter  Esc back"
        );
        let root = Offer { take_over: true, ..Offer::default() };
        assert_eq!(line(root, 200), "t take over  Enter show  j/k move  / filter  Esc back");
        assert!(line(full, 60).chars().count() <= 60);
        assert!(line(full, 100).contains("c coder"), "{:?}", line(full, 100));
    }

    #[test]
    fn the_ledger_key_line_offers_no_answer_on_an_answered_or_empty_selection() {
        let hints = |rows: &[LedgerRow]| line_text(render_ledger(rows, "", 0, 200, &Palette::builtin()).last().unwrap());
        for line in [hints(&[]), hints(&[answered_row("a-1")])] {
            assert_eq!(line, "Enter show  j/k move  / filter  Esc back");
        }
    }

    fn answered_row(id: &str) -> LedgerRow {
        LedgerRow::Answered(AnsweredRow {
            request_id: id.into(),
            time: None,
            context_label: "kaijutsu".into(),
            decision: None,
            principal: None,
            redeemed: RedeemedMark::Never,
            statement: "rm".into(),
        })
    }
}
