//! Asks and the ledger view (`Ctrl+A l`) — an ask rendered in the viewport
//! and answered through `kj ledger allow|deny`.
//!
//! Pure: every render fn takes rows and a width and returns `Line`s, and
//! every key fn takes a `KeyEvent` and returns an action — no RPC, no I/O, no
//! clock. The kernel round trip (`kj ledger list`/`show`/`allow`/`deny`) is
//! [`kaijutsu_client::ledger`]; wiring these two together is [`crate::run`].
//! Spec: `docs/tui.md`, "Asks" and "The ledger (`Ctrl+A l`, proposed chord)".

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::{Line, Span};

use crate::keys::{Intent, Keys};
use crate::present::Palette;

/// Where a key goes while an ask card is up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardRoute {
    /// One of the card's own keys: `a`/`A`/`d`/`v` decide, `Esc` sets aside.
    Card(AskCardKey),
    /// A chord or control key the card does not own — a `Ctrl+A` chord,
    /// `Ctrl+C`, `Ctrl+Z` — acted on as if no card were up.
    Chord(Intent),
    /// Compose text. The card holds it: the draft never changes under a
    /// card, so a decision key and a typed letter are never confused.
    Held,
}

/// Route one key while an ask card is up. The card's own keys win unless
/// the `Ctrl+A` prefix is armed, in which case the whole chord belongs to
/// the prefix table — `Ctrl+A d` is the chord `d`, never a deny.
pub fn route_under_card(key: KeyEvent, keys: &mut Keys) -> CardRoute {
    if !keys.armed()
        && let Some(card_key) = ask_card_key(key)
    {
        return CardRoute::Card(card_key);
    }
    match keys.interpret(key) {
        Intent::InputKey(_) | Intent::Tab => CardRoute::Held,
        intent => CardRoute::Chord(intent),
    }
}

/// What a decided ask's own key answers, on the ask card and on a selected
/// ledger row alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskDecision {
    AllowOnce,
    AllowAlways,
    Deny,
    ViewLedger,
}

/// What one key does on a live ask card: decide the ask, or put the card
/// aside with the ask still pending in the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskCardKey {
    Decide(AskDecision),
    /// `Esc`: the card comes down, the ask stays pending — the seat keeps
    /// its `!` and `Ctrl+A l` reaches it.
    Aside,
}

/// `a`/`A`/`d`/`v`/`Esc` on a live ask card (`docs/tui.md`'s Asks figure).
/// `None` for anything else.
pub fn ask_card_key(key: KeyEvent) -> Option<AskCardKey> {
    if let Some(decision) = ask_key_to_decision(key) {
        return Some(AskCardKey::Decide(decision));
    }
    if key.kind == KeyEventKind::Release {
        return None;
    }
    (key.code == KeyCode::Esc && key.modifiers.is_empty()).then_some(AskCardKey::Aside)
}

/// `a`/`A`/`d`/`v` on a live ask card (`docs/tui.md`'s Asks figure). `None`
/// for anything else, including a key-release event — a card key is never
/// swallowed by acting on both edges of one press.
pub fn ask_key_to_decision(key: KeyEvent) -> Option<AskDecision> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
        return None;
    }
    match key.code {
        KeyCode::Char('a') => Some(AskDecision::AllowOnce),
        KeyCode::Char('A') => Some(AskDecision::AllowAlways),
        KeyCode::Char('d') => Some(AskDecision::Deny),
        KeyCode::Char('v') => Some(AskDecision::ViewLedger),
        _ => None,
    }
}

/// One ask, resolved to what the card needs to render — the fields
/// [`crate::app::App`] already has to hand (context label/type from
/// `ContextInfo`, the rest from [`kaijutsu_client::AskDetail`]).
pub struct AskCard<'a> {
    pub request_id: &'a str,
    /// The ledger `tool` column, e.g. `"shell_write"` — `docs/tui.md` calls
    /// this the ask's "hook" in the figure; the field it reads is `tool`.
    pub hook: &'a str,
    pub context_label: &'a str,
    pub context_type: &'a str,
    pub statement: &'a str,
    pub asker: Option<&'a str>,
    pub reviewer: Option<&'a str>,
    pub can_review: bool,
}

/// `⚠ ask <id>  <hook>  from <context> (<type>)`, the statement flush-left,
/// then the key line — `docs/tui.md`'s Asks figure, verbatim.
pub fn render_ask_card(card: &AskCard<'_>, width: u16, palette: &Palette) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        format!(
            "⚠ ask {}  {}  from {} ({}){}{}",
            card.request_id,
            card.hook,
            card.context_label,
            card.context_type,
            card.asker.map(|name| format!("  asker {name}")).unwrap_or_default(),
            card.reviewer.map(|name| format!("  reviewer {name}")).unwrap_or_default(),
        ),
        palette.warning(),
    )));
    for row in wrap_plain(card.statement, width.saturating_sub(2)) {
        lines.push(Line::from(Span::styled(format!("  {row}"), palette.status())));
    }
    let keys = if card.can_review {
        "  [a]llow once  [A]llow always (global)  [d]eny  [v]iew ledger  Esc aside"
    } else {
        "  awaiting assigned reviewer — use kj ledger cancel or escalate  [v]iew ledger  Esc aside"
    };
    lines.push(Line::from(Span::styled(keys.to_string(), palette.divider())));
    lines
}

/// Lines [`render_ask_card`] needs at `width` — [`render::viewport_lines`]
/// asks this before the resize that makes room for the card, so the count
/// must be the exact render `draw_live` will draw. Counted at `width`
/// itself, not `u16::MAX` the way [`crate::picker::viewport_lines`] counts:
/// the statement wraps by width, so a wider count would be smaller than
/// what a narrower terminal actually needs, and the growth would undercount.
///
/// [`render::viewport_lines`]: crate::render::viewport_lines
pub fn ask_card_viewport_lines(card: &AskCard<'_>, width: u16) -> u16 {
    let body = render_ask_card(card, width, &Palette::builtin()).len();
    u16::try_from(body).unwrap_or(u16::MAX)
}

/// One row of the ledger view's PENDING section.
pub struct PendingRow {
    pub request_id: String,
    /// Formatted age (`format_age` in `status.rs`'s convention) from
    /// `created_at`, or `None` when the ask has no stamp.
    pub age: Option<String>,
    pub context_label: String,
    pub context_type: String,
    pub hook: String,
    pub asker: Option<String>,
    pub reviewer: Option<String>,
    pub reviewable: bool,
    pub statement: String,
}

/// One row of the ledger view's ANSWERED section.
pub struct AnsweredRow {
    pub request_id: String,
    /// Formatted decision time (`decided_at`), or `None` when the ask has
    /// no stamp.
    pub time: Option<String>,
    pub context_label: String,
    /// `"allow once"`, `"allow always"`, `"deny"` — or `None` when the wire
    /// carried only `status` (`allowed`/`denied`) and not `decided_option`.
    pub decision: Option<String>,
    /// Who decided it — `you`, a principal's short id, or `None` for a
    /// rule's auto-decision.
    pub principal: Option<String>,
    /// `redeemed <time>` / `redeemed ×N` / `—`, pre-formatted by the caller
    /// because a rule's redeem count is a different query than the ask's own
    /// `redeemed_at` and this module has no opinion on which a caller used.
    pub redeemed: RedeemedMark,
    pub statement: String,
}

/// How an answered row's redemption renders — `docs/tui.md`'s "was this
/// consumed" column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedeemedMark {
    /// This exact ask's decision was never redeemed.
    Never,
    /// This exact ask's decision was redeemed once, at this wallclock.
    At(String),
    /// A standing rule born from this decision has been redeemed this many
    /// times (`allow always`'s `×3`).
    Count(u64),
}

impl RedeemedMark {
    fn text(&self) -> String {
        match self {
            Self::Never => "—".to_string(),
            Self::At(when) => format!("redeemed {when}"),
            Self::Count(n) => format!("redeemed ×{n}"),
        }
    }
}

/// A pending or answered row, addressed by the ledger view's cursor
/// (`j`/`k`) — the request id is enough to route `Enter`/`a`/`A`/`d` back to
/// the right `kj ledger` call.
pub enum LedgerRow {
    Pending(PendingRow),
    Answered(AnsweredRow),
}

impl LedgerRow {
    pub fn request_id(&self) -> &str {
        match self {
            Self::Pending(r) => &r.request_id,
            Self::Answered(r) => &r.request_id,
        }
    }

    pub fn reviewable(&self) -> bool {
        matches!(self, Self::Pending(row) if row.reviewable)
    }

    /// Substring match across every column a filter might target — the id,
    /// context, hook (pending only) and statement.
    fn matches(&self, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        let needle = needle.to_ascii_lowercase();
        let hay = match self {
            Self::Pending(r) => format!(
                "{} {} {} {} {} {}",
                r.request_id,
                r.context_label,
                r.hook,
                r.asker.as_deref().unwrap_or(""),
                r.reviewer.as_deref().unwrap_or(""),
                r.statement
            ),
            Self::Answered(r) => format!(
                "{} {} {}",
                r.request_id, r.context_label, r.statement
            ),
        };
        hay.to_ascii_lowercase().contains(&needle)
    }
}

/// Every row a filter admits, PENDING first then ANSWERED, in the order the
/// caller supplied within each section — [`list_pending`]/[`list_history`]'s
/// own ordering (`kaijutsu_client::ledger`), not re-sorted here.
///
/// [`list_pending`]: kaijutsu_client::list_pending
/// [`list_history`]: kaijutsu_client::list_history
pub fn filtered_rows<'a>(rows: &'a [LedgerRow], filter: &str) -> Vec<&'a LedgerRow> {
    rows.iter().filter(|r| r.matches(filter)).collect()
}

/// `LEDGER                          pending 2   answered 7` down through the
/// PENDING and ANSWERED sections and the key line — `docs/tui.md`'s ledger
/// figure. `answered {N}` stands in for the figure's `answered today {N}`
/// until `decided_at` reaches the wire (needed to scope "today";
/// [`kaijutsu_client::AskDetail`]'s doc names the gap) — every answered row
/// this call is handed counts, not just today's.
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
                clip(&r.context_type, 10),
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

    let hints = "a allow once  A allow always (global)  d deny  Enter show  j/k move  / filter  Esc back";
    let short_hints = "a allow once  A global  d deny  Enter  j/k  /  Esc back";
    lines.push(Line::from(Span::styled(
        if hints.len() <= width_usize { hints } else { short_hints }.to_string(),
        palette.divider(),
    )));
    lines
}

/// Lines [`render_ledger`] needs — width-independent, [`crate::picker`]'s
/// own pattern (`docs/tui.md`, "The picker"): counted at `u16::MAX` because
/// every row is truncated to width (`truncate_plain`), never wrapped into
/// more lines, so a narrower width needs the same row count.
pub fn ledger_viewport_lines(rows: &[LedgerRow], filter: &str, selected: usize) -> u16 {
    let body = render_ledger(rows, filter, selected, u16::MAX, &Palette::builtin()).len();
    u16::try_from(body).unwrap_or(u16::MAX)
}

/// What one key does inside the ledger view (`Ctrl+A l`). Mode-dependent:
/// while `filtering`, every printable key edits the filter text instead of
/// answering a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerAction {
    AllowOnce,
    AllowAlways,
    Deny,
    Show,
    Up,
    Down,
    StartFilter,
    FilterInsert(char),
    FilterBackspace,
    CommitFilter,
    CancelFilter,
    Back,
    Ignored,
}

/// Interpret one key inside the ledger view. `docs/tui.md`'s ledger key
/// line, plus `/` to enter filter-typing mode and `Esc`/`Enter` to leave it.
pub fn ledger_key_to_action(key: KeyEvent, filtering: bool) -> LedgerAction {
    if key.kind == KeyEventKind::Release {
        return LedgerAction::Ignored;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if filtering {
        return match key.code {
            KeyCode::Esc => LedgerAction::CancelFilter,
            KeyCode::Enter => LedgerAction::CommitFilter,
            KeyCode::Backspace => LedgerAction::FilterBackspace,
            KeyCode::Char(c) if !ctrl => LedgerAction::FilterInsert(c),
            _ => LedgerAction::Ignored,
        };
    }
    match key.code {
        KeyCode::Char('a') => LedgerAction::AllowOnce,
        KeyCode::Char('A') => LedgerAction::AllowAlways,
        KeyCode::Char('d') => LedgerAction::Deny,
        KeyCode::Enter => LedgerAction::Show,
        KeyCode::Char('j') | KeyCode::Down => LedgerAction::Down,
        KeyCode::Char('k') | KeyCode::Up => LedgerAction::Up,
        KeyCode::Char('/') => LedgerAction::StartFilter,
        KeyCode::Esc => LedgerAction::Back,
        _ => LedgerAction::Ignored,
    }
}

/// One ask shown in full (`Enter` on a ledger row, or the ask card's
/// `[v]iew ledger` having answered nothing yet): every field
/// [`kaijutsu_client::AskDetail`] carries, laid out one per line, including
/// the env snapshot an approval runs with.
pub fn render_ask_detail(detail: &AskDetailView<'_>, width: u16, palette: &Palette) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut lines = vec![
        Line::from(Span::styled(format!("ask:        {}", detail.request_id), palette.status())),
        Line::from(Span::styled(format!("status:     {}", detail.status), palette.status())),
        Line::from(Span::styled(format!("origin:     {}", detail.origin), palette.status())),
        Line::from(Span::styled(
            format!("context:    {} ({})", detail.context_label, detail.context_type),
            palette.status(),
        )),
    ];
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
        "a allow once  A allow always  d deny  Esc back".to_string(),
        palette.divider(),
    )));
    lines
}

/// What [`render_ask_detail`] needs, resolved by the caller from
/// [`kaijutsu_client::AskDetail`] plus the context label/type
/// [`crate::app::App`] already has.
pub struct AskDetailView<'a> {
    pub request_id: &'a str,
    pub status: &'a str,
    pub origin: &'a str,
    pub hook: Option<&'a str>,
    pub context_label: &'a str,
    pub context_type: &'a str,
    pub requester: Option<&'a str>,
    pub asker: Option<&'a str>,
    pub reviewer: Option<&'a str>,
    pub statements: &'a [String],
    pub exec_source: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub env: &'a [(String, Option<String>)],
    pub redeemed: RedeemedMark,
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

/// The ask card's live state: which ask is showing, and its full detail
/// (`kj ledger show`) once fetched — [`crate::run`] opens one when a new
/// ask arrives for the context on screen, and closes it once answered.
pub struct AskCardState {
    pub request_id: String,
    pub context_id: kaijutsu_types::ContextId,
    pub detail: kaijutsu_client::AskDetail,
}

/// The ledger view's live state (`Ctrl+A l`): every row, the cursor, and
/// whether `/` has put it into filter-typing mode.
#[derive(Default)]
pub struct LedgerViewState {
    pub rows: Vec<LedgerRow>,
    pub filter: String,
    pub selected: usize,
    pub filtering: bool,
    /// The selected ask after `Enter`; `Esc` returns to its row list.
    pub detail: Option<kaijutsu_client::AskDetail>,
}

impl LedgerViewState {
    /// How many rows the current filter admits — [`Self::selected`]'s valid
    /// range.
    pub fn visible_len(&self) -> usize {
        filtered_rows(&self.rows, &self.filter).len()
    }

    /// The request id `j`/`k`'s cursor is on, or `None` when the filter
    /// admits nothing.
    pub fn selected_request_id(&self) -> Option<String> {
        filtered_rows(&self.rows, &self.filter)
            .get(self.selected)
            .map(|r| r.request_id().to_string())
    }

    pub fn move_down(&mut self) {
        let len = self.visible_len();
        if len > 0 {
            self.selected = (self.selected + 1).min(len - 1);
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }
}

/// A context's label/type, defaulting to its short id and `"default"` when
/// [`crate::app::App`] has no [`kaijutsu_client::ContextInfo`] for it (an
/// ask can outlive the context it named — `docs/gate-shape-b.md`, "Archived
/// contexts are inert").
pub fn context_facts(app: &crate::app::App, ctx: kaijutsu_types::ContextId) -> (String, String) {
    match app.info(ctx) {
        Some(info) => (
            if info.label.is_empty() { ctx.short() } else { info.label.clone() },
            info.context_type.clone(),
        ),
        None => (ctx.short(), "default".to_string()),
    }
}

/// The grown-view content that replaces the live region's block stream when
/// an ask card or the ledger view is open (`docs/tui.md`'s "grows the
/// viewport" treatment, the same one the picker gets). The real resize is
/// [`crate::render::viewport_lines`] plus `run.rs`'s `set_viewport_height` —
/// this fn only builds the render; [`active_view_viewport_lines`] is the
/// matching height, counted from the same construction.
pub fn active_view_lines(app: &crate::app::App, width: u16) -> Option<Vec<Line<'static>>> {
    if let Some(card) = &app.ask_card {
        let (context_label, context_type) = context_facts(app, card.context_id);
        let statement = card
            .detail
            .statements
            .first()
            .map(String::as_str)
            .unwrap_or(card.detail.description.as_str());
        let view = AskCard {
            request_id: &card.request_id,
            hook: card.detail.tool.as_deref().unwrap_or("-"),
            context_label: &context_label,
            context_type: &context_type,
            statement,
            asker: card.detail.actor_name.as_deref(),
            reviewer: card.detail.reviewer_name.as_deref(),
            can_review: app.principal.is_some_and(|principal| card.detail.can_review(principal)),
        };
        return Some(render_ask_card(&view, width, &app.palette));
    }
    if let Some(view) = &app.ledger_view {
        if let Some(detail) = &view.detail {
            let (context_label, context_type) = detail
                .context_id
                .map(|ctx| context_facts(app, ctx))
                .unwrap_or_else(|| ("(unknown)".to_string(), "default".to_string()));
            let env: Vec<(String, Option<String>)> = detail
                .env
                .iter()
                .map(|entry| (entry.name.clone(), entry.value.clone()))
                .collect();
            let view = AskDetailView {
                request_id: &detail.request_id,
                status: &detail.status,
                origin: &detail.origin,
                hook: detail.tool.as_deref(),
                context_label: &context_label,
                context_type: &context_type,
                requester: detail.principal_name.as_deref(),
                asker: detail.actor_name.as_deref(),
                reviewer: detail.reviewer_name.as_deref(),
                statements: &detail.statements,
                exec_source: detail.exec_source.as_deref(),
                cwd: detail.cwd.as_deref(),
                env: &env,
                redeemed: match detail.redeemed_at {
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

/// The height an open ask card or ledger view needs at `width` — `None`
/// when neither is open. [`crate::render::viewport_lines`] takes this as
/// one of its `max()` arms, the same growth the picker already gets.
pub fn active_view_viewport_lines(app: &crate::app::App, width: u16) -> Option<u16> {
    if let Some(card) = &app.ask_card {
        let (context_label, context_type) = context_facts(app, card.context_id);
        let statement = card
            .detail
            .statements
            .first()
            .map(String::as_str)
            .unwrap_or(card.detail.description.as_str());
        let view = AskCard {
            request_id: &card.request_id,
            hook: card.detail.tool.as_deref().unwrap_or("-"),
            context_label: &context_label,
            context_type: &context_type,
            statement,
            asker: card.detail.actor_name.as_deref(),
            reviewer: card.detail.reviewer_name.as_deref(),
            can_review: app.principal.is_some_and(|principal| card.detail.can_review(principal)),
        };
        return Some(ask_card_viewport_lines(&view, width));
    }
    if let Some(view) = &app.ledger_view {
        if let Some(detail) = &view.detail {
            let (context_label, context_type) = detail
                .context_id
                .map(|ctx| context_facts(app, ctx))
                .unwrap_or_else(|| ("(unknown)".to_string(), "default".to_string()));
            let env: Vec<(String, Option<String>)> = detail
                .env
                .iter()
                .map(|entry| (entry.name.clone(), entry.value.clone()))
                .collect();
            let detail = AskDetailView {
                request_id: &detail.request_id, status: &detail.status, origin: &detail.origin,
                hook: detail.tool.as_deref(), context_label: &context_label, context_type: &context_type,
                requester: detail.principal_name.as_deref(), asker: detail.actor_name.as_deref(), reviewer: detail.reviewer_name.as_deref(),
                statements: &detail.statements, exec_source: detail.exec_source.as_deref(), cwd: detail.cwd.as_deref(), env: &env,
                redeemed: RedeemedMark::Never,
            };
            return Some(u16::try_from(render_ask_detail(&detail, width, &Palette::builtin()).len()).unwrap_or(u16::MAX));
        }
        return Some(ledger_viewport_lines(&view.rows, &view.filter, view.selected));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn ask_keys_map_to_decisions() {
        assert_eq!(ask_key_to_decision(press(KeyCode::Char('a'))), Some(AskDecision::AllowOnce));
        assert_eq!(ask_key_to_decision(press(KeyCode::Char('A'))), Some(AskDecision::AllowAlways));
        assert_eq!(ask_key_to_decision(press(KeyCode::Char('d'))), Some(AskDecision::Deny));
        assert_eq!(ask_key_to_decision(press(KeyCode::Char('v'))), Some(AskDecision::ViewLedger));
        assert_eq!(ask_key_to_decision(press(KeyCode::Char('x'))), None);
    }

    #[test]
    fn a_ctrl_chord_is_never_an_ask_decision() {
        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(ask_key_to_decision(ctrl_a), None);
    }

    /// `Esc` puts the card aside; a decision key still decides; any other
    /// key is still swallowed by the card rather than reaching compose.
    #[test]
    fn esc_puts_the_ask_card_aside() {
        assert_eq!(ask_card_key(press(KeyCode::Esc)), Some(AskCardKey::Aside));
        assert_eq!(ask_card_key(press(KeyCode::Char('d'))), Some(AskCardKey::Decide(AskDecision::Deny)));
        assert_eq!(ask_card_key(press(KeyCode::Char('x'))), None);
        let released = KeyEvent { kind: KeyEventKind::Release, ..press(KeyCode::Esc) };
        assert_eq!(ask_card_key(released), None);
    }

    #[test]
    fn the_ask_card_renders_the_figures_shape() {
        let card = AskCard {
            request_id: "01a04eb6",
            hook: "shell_write",
            context_label: "kaijutsu",
            context_type: "coder",
            statement: "rm -rf ~/src/wt/kaish-arith",
            asker: Some("coder"),
            reviewer: Some("amy"),
            can_review: true,
        };
        let lines = render_ask_card(&card, 80, &Palette::builtin());
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(text[0], "⚠ ask 01a04eb6  shell_write  from kaijutsu (coder)  asker coder  reviewer amy");
        assert_eq!(text[1], "  rm -rf ~/src/wt/kaish-arith");
        assert_eq!(text[2], "  [a]llow once  [A]llow always (global)  [d]eny  [v]iew ledger  Esc aside");
    }

    #[test]
    fn a_long_statement_wraps_under_the_ask_card_header() {
        let card = AskCard {
            request_id: "id",
            hook: "shell_write",
            context_label: "kaijutsu",
            context_type: "coder",
            statement: "one two three four five six seven eight nine ten",
            asker: None,
            reviewer: None,
            can_review: true,
        };
        let lines = render_ask_card(&card, 24, &Palette::builtin());
        // Header, N wrapped statement lines, key line.
        assert!(lines.len() > 3, "expected the statement to wrap: {lines:?}");
    }

    #[test]
    fn an_ask_the_viewer_cannot_review_offers_cancel_or_escalation_not_approval() {
        let card = AskCard {
            request_id: "id", hook: "shell_write", context_label: "kaijutsu", context_type: "coder",
            statement: "rm -rf", asker: Some("coder"), reviewer: Some("lead"), can_review: false,
        };
        let text: Vec<String> = render_ask_card(&card, 120, &Palette::builtin())
            .iter().map(line_text).collect();
        assert!(text.last().unwrap().contains("awaiting assigned reviewer"));
        assert!(!text.last().unwrap().contains("allow once"));
    }

    /// [`ask_card_viewport_lines`] must count the same wrapped render
    /// `draw_live` will actually draw at that width — a count taken at
    /// `u16::MAX` (the picker's pattern) would undercount, because the
    /// statement wraps by width and a wider width wraps less.
    #[test]
    fn ask_card_viewport_lines_matches_the_render_at_width() {
        let card = AskCard {
            request_id: "id",
            hook: "shell_write",
            context_label: "kaijutsu",
            context_type: "coder",
            statement: "one two three four five six seven eight nine ten eleven twelve",
            asker: None,
            reviewer: None,
            can_review: true,
        };
        let narrow_rendered = render_ask_card(&card, 16, &Palette::builtin()).len();
        assert_eq!(usize::from(ask_card_viewport_lines(&card, 16)), narrow_rendered);
        let wide_rendered = render_ask_card(&card, u16::MAX, &Palette::builtin()).len();
        assert!(
            narrow_rendered > wide_rendered,
            "a narrow width should wrap into more lines than u16::MAX: narrow {narrow_rendered} wide {wide_rendered}"
        );
    }

    /// [`ledger_viewport_lines`] is width-independent — the picker's own
    /// pattern (`docs/tui.md`, "The picker") — because rows are truncated
    /// to width, never wrapped into more lines.
    #[test]
    fn ledger_viewport_lines_is_width_independent() {
        let rows = vec![
            pending("p1", "kaijutsu"),
            pending("p2", "lfm2d"),
            answered("a1", "kaijutsu", RedeemedMark::Never),
        ];
        let rendered_40 = render_ledger(&rows, "", 0, 40, &Palette::builtin()).len();
        let rendered_200 = render_ledger(&rows, "", 0, 200, &Palette::builtin()).len();
        assert_eq!(rendered_40, rendered_200, "row count should not depend on width");
        assert_eq!(usize::from(ledger_viewport_lines(&rows, "", 0)), rendered_40);
    }

    fn pending(id: &str, ctx: &str) -> LedgerRow {
        LedgerRow::Pending(PendingRow {
            request_id: id.to_string(),
            age: Some("12s".to_string()),
            context_label: ctx.to_string(),
            context_type: "coder".to_string(),
            hook: "shell_write".to_string(),
            asker: Some("coder".to_string()),
            reviewer: Some("amy".to_string()),
            reviewable: true,
            statement: "git worktree remove --force ~/src/wt/kaish-arith".to_string(),
        })
    }

    fn answered(id: &str, ctx: &str, redeemed: RedeemedMark) -> LedgerRow {
        LedgerRow::Answered(AnsweredRow {
            request_id: id.to_string(),
            time: Some("13:58".to_string()),
            context_label: ctx.to_string(),
            decision: Some("allow once".to_string()),
            principal: Some("amy".to_string()),
            redeemed,
            statement: "git worktree remove …".to_string(),
        })
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn the_ledger_view_carries_both_sections_and_the_key_line() {
        let rows = vec![
            pending("p1", "kaijutsu"),
            pending("p2", "lfm2d"),
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
    fn a_redeem_count_renders_as_a_multiplier() {
        let rows = vec![answered("a1", "kaish", RedeemedMark::Count(3))];
        let lines = render_ledger(&rows, "", 0, 100, &Palette::builtin());
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(text.iter().any(|l| l.contains("redeemed ×3")), "got {text:?}");
    }

    #[test]
    fn never_redeemed_renders_as_a_dash() {
        let rows = vec![answered("a1", "kaish", RedeemedMark::Never)];
        let lines = render_ledger(&rows, "", 0, 100, &Palette::builtin());
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(text.iter().any(|l| l.trim_end().ends_with('—') || l.contains(" —  ")), "got {text:?}");
    }

    #[test]
    fn a_filter_narrows_both_sections() {
        let rows = vec![pending("p1", "kaijutsu"), pending("p2", "lfm2d")];
        let visible = filtered_rows(&rows, "lfm2d");
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].request_id(), "p2");
    }

    #[test]
    fn an_empty_filter_admits_everything() {
        let rows = vec![pending("p1", "kaijutsu"), pending("p2", "lfm2d")];
        assert_eq!(filtered_rows(&rows, "").len(), 2);
    }

    #[test]
    fn a_filter_with_no_match_says_so() {
        let rows = vec![pending("p1", "kaijutsu")];
        let lines = render_ledger(&rows, "nope", 0, 100, &Palette::builtin());
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(text.iter().any(|l| l.contains("nothing matches")), "got {text:?}");
    }

    #[test]
    fn ledger_keys_answer_a_selected_row() {
        assert_eq!(ledger_key_to_action(press(KeyCode::Char('a')), false), LedgerAction::AllowOnce);
        assert_eq!(ledger_key_to_action(press(KeyCode::Char('A')), false), LedgerAction::AllowAlways);
        assert_eq!(ledger_key_to_action(press(KeyCode::Char('d')), false), LedgerAction::Deny);
        assert_eq!(ledger_key_to_action(press(KeyCode::Enter), false), LedgerAction::Show);
        assert_eq!(ledger_key_to_action(press(KeyCode::Char('j')), false), LedgerAction::Down);
        assert_eq!(ledger_key_to_action(press(KeyCode::Char('k')), false), LedgerAction::Up);
        assert_eq!(ledger_key_to_action(press(KeyCode::Esc), false), LedgerAction::Back);
    }

    #[test]
    fn slash_enters_filter_mode_and_typed_chars_edit_it_not_answer_a_row() {
        assert_eq!(ledger_key_to_action(press(KeyCode::Char('/')), false), LedgerAction::StartFilter);
        assert_eq!(
            ledger_key_to_action(press(KeyCode::Char('a')), true),
            LedgerAction::FilterInsert('a'),
            "while filtering, 'a' types into the filter, it does not allow a row"
        );
        assert_eq!(ledger_key_to_action(press(KeyCode::Backspace), true), LedgerAction::FilterBackspace);
        assert_eq!(ledger_key_to_action(press(KeyCode::Enter), true), LedgerAction::CommitFilter);
        assert_eq!(ledger_key_to_action(press(KeyCode::Esc), true), LedgerAction::CancelFilter);
    }

    #[test]
    fn the_ask_detail_view_shows_the_env_snapshot() {
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
            context_type: "coder",
            requester: Some("amy"),
            asker: Some("coder"),
            reviewer: Some("amy"),
            statements: &["rm -rf ~/src/wt/kaish-arith".to_string()],
            exec_source: Some("kaish"),
            cwd: Some("/home/amy/src/wt/kaish-arith"),
            env: &env,
            redeemed: RedeemedMark::Never,
        };
        let lines = render_ask_detail(&detail, 100, &Palette::builtin());
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(text.iter().any(|l| l.contains("cwd:") && l.contains("kaish-arith")));
        assert!(text.iter().any(|l| l == "env:        TARGET=kaish-arith"));
        assert!(text.iter().any(|l| l == "env:        FORCE unset"));
        assert!(text.iter().any(|l| l.contains("redeemed:   —")));
        assert!(text.iter().any(|l| l == "asker:      coder"));
        assert!(text.iter().any(|l| l == "reviewer:   amy"));
    }

    #[test]
    fn wrap_plain_breaks_at_the_last_space_that_fits() {
        assert_eq!(
            wrap_plain("one two three", 7),
            vec!["one two".to_string(), "three".to_string()]
        );
    }
}

#[cfg(test)]
mod card_route_tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// The card's own keys decide or set aside; a typed letter is held.
    #[test]
    fn the_card_owns_its_five_keys_and_holds_text() {
        let mut keys = Keys::new();
        assert_eq!(
            route_under_card(press(KeyCode::Char('a')), &mut keys),
            CardRoute::Card(AskCardKey::Decide(AskDecision::AllowOnce))
        );
        assert_eq!(route_under_card(press(KeyCode::Esc), &mut keys), CardRoute::Card(AskCardKey::Aside));
        assert_eq!(route_under_card(press(KeyCode::Char('x')), &mut keys), CardRoute::Held);
        assert_eq!(route_under_card(press(KeyCode::Tab), &mut keys), CardRoute::Held);
    }

    /// `Ctrl+A 4` switches seats with a card up, exactly as it does without
    /// one — the prefix is the one key that works everywhere.
    #[test]
    fn a_seat_chord_acts_under_a_card() {
        let mut keys = Keys::new();
        assert_eq!(route_under_card(ctrl('a'), &mut keys), CardRoute::Chord(Intent::LegendChanged));
        assert_eq!(route_under_card(press(KeyCode::Char('4')), &mut keys), CardRoute::Chord(Intent::SwitchSeat(4)));
        assert_eq!(route_under_card(ctrl('a'), &mut keys), CardRoute::Chord(Intent::LegendChanged));
        assert_eq!(route_under_card(press(KeyCode::Char('"')), &mut keys), CardRoute::Chord(Intent::TogglePicker));
    }

    /// With the prefix armed, `d` is the chord `d`, not a deny.
    #[test]
    fn an_armed_prefix_takes_a_card_letter_as_its_chord() {
        let mut keys = Keys::new();
        route_under_card(ctrl('a'), &mut keys);
        assert!(matches!(
            route_under_card(press(KeyCode::Char('d')), &mut keys),
            CardRoute::Chord(Intent::NotYet(_))
        ));
        assert_eq!(
            route_under_card(press(KeyCode::Char('d')), &mut keys),
            CardRoute::Card(AskCardKey::Decide(AskDecision::Deny)),
            "the prefix is spent; the next d is the card's"
        );
    }

    /// `Ctrl+C` and `Ctrl+Z` are never held by a card.
    #[test]
    fn control_keys_act_under_a_card() {
        let mut keys = Keys::new();
        assert_eq!(route_under_card(ctrl('c'), &mut keys), CardRoute::Chord(Intent::Interrupt));
        assert_eq!(route_under_card(ctrl('z'), &mut keys), CardRoute::Chord(Intent::Suspend));
    }
}
