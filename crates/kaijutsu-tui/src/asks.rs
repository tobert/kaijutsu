//! Asks and the ledger view (`Ctrl+A l`) — an ask rendered as an overlay
//! and answered through `ActorHandle::decide_ask`.
//!
//! Pure: every render fn takes rows and a width and returns `Line`s, every
//! key fn takes a `KeyEvent` and returns an action, and [`fold_ledger`]
//! takes a [`LedgerState`] — no RPC, no I/O, no clock. The open asks come
//! from the actor's ledger watch ([`kaijutsu_client::ledger`]); the calls
//! that read and answer an ask, and wiring them to this module, are
//! [`crate::run`]'s.
//! Spec: `docs/tui.md`, "Asks" and "The ledger (`Ctrl+A l`, proposed chord)".

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::{Line, Span};

use kaijutsu_client::LedgerState;
use kaijutsu_client::actor::CallError;
use kaijutsu_types::{AskAnswerFailureKind, AskDetail, AskVerdict, ContextId, PrincipalId, Remember, RememberScope};

use crate::app::App;
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
/// `ContextInfo`, the rest from [`AskDetail`]).
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
    /// Whether the card's keys act yet. A disarmed card shows no keys; its
    /// key line says keys still go to the draft.
    pub armed: bool,
}

/// The key line of a card whose keys do not act yet. Text rather than a
/// color, so it reads the same with `NO_COLOR`.
pub const ARMING_KEY_LINE: &str = "  arming… keys still go to the draft";

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
    let keys = if !card.armed {
        ARMING_KEY_LINE
    } else if card.can_review {
        "  [a]llow once  [A]llow always (global)  [d]eny  [v]iew ledger  Esc aside"
    } else {
        "  awaiting assigned reviewer — use kj ledger cancel or escalate  [v]iew ledger  Esc aside"
    };
    lines.push(Line::from(Span::styled(keys.to_string(), palette.divider())));
    lines
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
/// the right ask.
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
/// caller supplied within each section (oldest pending first, newest
/// answered first), not re-sorted here.
pub fn filtered_rows<'a>(rows: &'a [LedgerRow], filter: &str) -> Vec<&'a LedgerRow> {
    rows.iter().filter(|r| r.matches(filter)).collect()
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
    // Every ledger key is bare; a Ctrl or Alt chord is never a decision.
    if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
        return LedgerAction::Ignored;
    }
    if filtering {
        return match key.code {
            KeyCode::Esc => LedgerAction::CancelFilter,
            KeyCode::Enter => LedgerAction::CommitFilter,
            KeyCode::Backspace => LedgerAction::FilterBackspace,
            KeyCode::Char(c) => LedgerAction::FilterInsert(c),
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
/// [`AskDetail`] carries, laid out one per line, including
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
/// [`AskDetail`] plus the context label/type
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

/// The ask card's live state: which ask is showing, and its full record
/// (`get_ask`). [`crate::run`] reads the record for [`card_candidate`] and
/// opens the card with [`open_card`]; [`fold_ledger`] keeps its summary
/// current and takes it down once the ask leaves the open set.
pub struct AskCardState {
    pub request_id: String,
    pub context_id: ContextId,
    pub detail: AskDetail,
}

/// The ask card's arming, kept beside the card for the ask it was started
/// for. `armed` is what the last frame drew, so a key acts on the card
/// only after the player has seen its keys (`docs/tui.md`, "Asks").
pub struct CardArming {
    pub request_id: String,
    clock: kaijutsu_client::AskArming<std::time::Instant>,
    armed: bool,
}

impl CardArming {
    /// A card for `request_id` first drawn at `now`, disarmed.
    pub fn shown(request_id: String, now: std::time::Instant) -> Self {
        Self { request_id, clock: kaijutsu_client::AskArming::shown(now), armed: false }
    }

    /// Whether the last frame drew the card armed.
    pub fn armed(&self) -> bool {
        self.armed
    }

    /// Arm if the hold has passed at `now`. True when that changed.
    pub fn tick(&mut self, now: std::time::Instant) -> bool {
        if !self.armed && self.clock.armed(now) {
            self.armed = true;
            return true;
        }
        false
    }

    /// A key that went past the disarmed card at `now`.
    pub fn keystroke(&mut self, now: std::time::Instant) {
        self.clock.keystroke(now);
    }
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
    pub detail: Option<AskDetail>,
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
/// ask can outlive the context it named — `docs/gate-resume.md`, "Archived
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

/// The overlay an open ask card or the ledger view draws over the foot of
/// the transcript, above the band — the same treatment the picker gets
/// (`crate::render::overlay_lines`). `None` when neither is open.
pub fn active_view_lines(app: &crate::app::App, width: u16) -> Option<Vec<Line<'static>>> {
    if let Some(card) = &app.ask_card {
        let (context_label, context_type) = context_facts(app, card.context_id);
        let summary = &card.detail.summary;
        let statement = summary.statements.first().map(String::as_str).unwrap_or(summary.description.as_str());
        let view = AskCard {
            request_id: &card.request_id,
            hook: card.detail.tool.as_deref().unwrap_or("-"),
            context_label: &context_label,
            context_type: &context_type,
            statement,
            asker: summary.performer.as_ref().map(|who| who.name.as_str()),
            reviewer: summary.reviewer.as_ref().map(|who| who.name.as_str()),
            can_review: app.principal.is_some_and(|principal| summary.answerable_by(principal)),
            armed: app.ask_card_armed(),
        };
        return Some(render_ask_card(&view, width, &app.palette));
    }
    if let Some(view) = &app.ledger_view {
        if let Some(detail) = &view.detail {
            let summary = &detail.summary;
            let (context_label, context_type) = summary
                .context_id
                .map(|ctx| context_facts(app, ctx))
                .unwrap_or_else(|| ("(unknown)".to_string(), "default".to_string()));
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
                context_type: &context_type,
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

/// What folding one ledger state into the app leaves for [`crate::run`] to
/// do. Each item needs a kernel call, which the loop runs on its own task.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LedgerFold {
    /// The session's first synced fold ([`NotifySeen::baseline`]).
    pub baseline: bool,
    /// The context of an ask this fold found newly open: one raised in the
    /// context on screen when there is one, else the first. `None` when no
    /// ask is new.
    pub raised_in: Option<ContextId>,
    /// The card this fold took down because its ask left the open set:
    /// answered from another surface, cancelled, expired, or abandoned. The
    /// loop reads its record for [`answered_notice`].
    pub answered: Option<String>,
}

/// Fold the actor's ledger state into the app: the open set the seats' `!`
/// and the `!n` read, the card's summary (a reassignment shows while the
/// card is up), and the card itself once its ask leaves the open set.
///
/// `None` when the state is not synced. Before the first listing after a
/// (re)connect lands, `open` may be empty only because nothing was read, so
/// the app keeps what it had.
pub fn fold_ledger(app: &mut App, state: &LedgerState) -> Option<LedgerFold> {
    if !state.synced {
        return None;
    }
    let baseline = !std::mem::replace(&mut app.ledger_folded, true);
    let raised: Vec<ContextId> = state
        .open
        .values()
        .filter(|ask| !app.open_asks.contains_key(&ask.request_id))
        .filter_map(|ask| ask.context_id)
        .collect();
    let raised_in = raised
        .iter()
        .copied()
        .find(|ctx| app.current == Some(*ctx))
        .or_else(|| raised.first().copied());
    let mut answered = None;
    if let Some(card) = app.ask_card.as_mut() {
        match state.open.get(&card.request_id) {
            Some(summary) => card.detail.summary = summary.clone(),
            None => answered = Some(card.request_id.clone()),
        }
    }
    if answered.is_some() {
        app.ask_card = None;
    }
    app.asks_set_aside.retain(|id| state.open.contains_key(id));
    app.card_reads_missed.clear();
    app.open_asks = state.open.clone();
    Some(LedgerFold { baseline, raised_in, answered })
}

/// The ask whose card should come up now: none while a card is up, else the
/// oldest open ask raised in the context on screen that was not set aside
/// and whose last card read did not miss. A card is never raised for a seat
/// you are not looking at.
pub fn card_candidate(app: &App) -> Option<String> {
    if app.ask_card.is_some() {
        return None;
    }
    let current = app.current?;
    app.open_asks
        .values()
        .filter(|ask| ask.context_id == Some(current))
        .filter(|ask| !app.asks_set_aside.contains(&ask.request_id) && !app.card_reads_missed.contains(&ask.request_id))
        .min_by(|a, b| (a.created_at_ms, &a.request_id).cmp(&(b.created_at_ms, &b.request_id)))
        .map(|ask| ask.request_id.clone())
}

/// Put up the card for `detail`, read for [`card_candidate`]'s ask, if that
/// ask is still the candidate when the read lands: a switch, an `Esc`, or
/// the ask closing may have moved on since. The summary comes from the
/// folded open set, which the push keeps current. True when it went up.
pub fn open_card(app: &mut App, mut detail: AskDetail) -> bool {
    let request_id = detail.summary.request_id.clone();
    if card_candidate(app).as_deref() != Some(request_id.as_str()) {
        return false;
    }
    let (Some(summary), Some(context_id)) = (app.open_asks.get(&request_id), app.current) else {
        return false;
    };
    detail.summary = summary.clone();
    app.ask_card = Some(AskCardState { request_id, context_id, detail });
    true
}

/// Land the card candidate's record: put the card up, or, when the read
/// found nothing or failed, leave that ask out of [`card_candidate`] until
/// the next fold.
pub fn land_card_read(app: &mut App, request_id: &str, read: Result<Option<AskDetail>, CallError>) {
    match read {
        Ok(Some(detail)) => {
            open_card(app, detail);
        }
        // Closed before the read; the next push says so.
        Ok(None) => {
            app.card_reads_missed.insert(request_id.to_string());
        }
        Err(error) => {
            app.card_reads_missed.insert(request_id.to_string());
            app.note(format!("cannot read ask {}: {error}", short_ask(request_id)));
        }
    }
}

/// Kernel work about asks that a key asked for ([`App::ask_work`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskWork {
    /// Read the ledger view's rows: the open asks named here, then the
    /// recent answers.
    OpenLedger { open: Vec<String> },
    /// Read one ask in full for the ledger view (`Enter`).
    Show(String),
    /// Answer one ask.
    Decide { request_id: String, verdict: AskVerdict, remember: Option<Remember> },
}

/// How many answered asks the ledger view lists, newest first.
pub const LEDGER_HISTORY_LIMIT: u32 = 20;

/// Ask for the ledger view: the open asks, oldest first, then the recent
/// answers.
pub fn request_ledger(app: &mut App) {
    let mut open: Vec<&kaijutsu_types::AskSummary> = app.open_asks.values().collect();
    open.sort_by(|a, b| (a.created_at_ms, &a.request_id).cmp(&(b.created_at_ms, &b.request_id)));
    let open = open.into_iter().map(|ask| ask.request_id.clone()).collect();
    app.ask_work.push(AskWork::OpenLedger { open });
}

/// The verdict and standing rule a decision key asks for. `None` for `v`,
/// which answers nothing.
pub fn verdict_for(decision: AskDecision) -> Option<(AskVerdict, Option<Remember>)> {
    match decision {
        AskDecision::AllowOnce => Some((AskVerdict::Allow, None)),
        AskDecision::AllowAlways => {
            Some((AskVerdict::Allow, Some(Remember { scope: RememberScope::Always, family: false })))
        }
        AskDecision::Deny => Some((AskVerdict::Deny, None)),
        AskDecision::ViewLedger => None,
    }
}

/// The notice for an ask its reviewer must answer.
fn awaits_reviewer(request_id: &str) -> String {
    format!(
        "ask {} awaits its assigned reviewer; cancel it or ask that reviewer to escalate",
        short_ask(request_id)
    )
}

/// One of the armed card's own keys. Every key that takes the card down
/// sets its ask aside, so [`card_candidate`] does not raise it again while
/// it stays open; a failed answer clears that ([`land_decision`]).
pub fn card_key(app: &mut App, key: AskCardKey) {
    let Some(card) = app.ask_card.take() else { return };
    let decision = match key {
        AskCardKey::Aside => {
            app.note(format!("ask {} set aside, still pending (Ctrl+A l)", short_ask(&card.request_id)));
            app.asks_set_aside.insert(card.request_id);
            return;
        }
        AskCardKey::Decide(decision) => decision,
    };
    let Some((verdict, remember)) = verdict_for(decision) else {
        app.asks_set_aside.insert(card.request_id);
        request_ledger(app);
        return;
    };
    if !app.principal.is_some_and(|principal| card.detail.summary.answerable_by(principal)) {
        app.note(awaits_reviewer(&card.request_id));
        app.ask_card = Some(card);
        return;
    }
    app.asks_set_aside.insert(card.request_id.clone());
    app.ask_work.push(AskWork::Decide { request_id: card.request_id, verdict, remember });
}

/// Clamp the ledger view's cursor into the rows its filter admits.
fn clamp_selected(view: &mut LedgerViewState) {
    view.selected = view.selected.min(view.visible_len().saturating_sub(1));
}

/// One key inside the ledger view: navigate, filter, show, or answer the
/// selected row. An answer closes the view; the ledger push updates the
/// seat flags and the pending count.
pub fn ledger_key(app: &mut App, key: KeyEvent) {
    if app.ledger_view.as_ref().is_some_and(|view| view.detail.is_some()) {
        if key.code == KeyCode::Esc && key.modifiers.is_empty()
            && let Some(view) = app.ledger_view.as_mut()
        {
            view.detail = None;
        }
        return;
    }
    let Some(view) = app.ledger_view.as_mut() else { return };
    let filtering = view.filtering;
    let action = ledger_key_to_action(key, filtering);
    let decision = match action {
        LedgerAction::Back => {
            app.ledger_view = None;
            return;
        }
        LedgerAction::Up => {
            view.move_up();
            return;
        }
        LedgerAction::Down => {
            view.move_down();
            return;
        }
        LedgerAction::StartFilter => {
            view.filtering = true;
            return;
        }
        LedgerAction::FilterInsert(c) => {
            view.filter.push(c);
            clamp_selected(view);
            return;
        }
        LedgerAction::FilterBackspace => {
            view.filter.pop();
            clamp_selected(view);
            return;
        }
        LedgerAction::CommitFilter | LedgerAction::CancelFilter => {
            view.filtering = false;
            return;
        }
        LedgerAction::Show => {
            if let Some(request_id) = view.selected_request_id() {
                app.ask_work.push(AskWork::Show(request_id));
            }
            return;
        }
        LedgerAction::AllowOnce => AskDecision::AllowOnce,
        LedgerAction::AllowAlways => AskDecision::AllowAlways,
        LedgerAction::Deny => AskDecision::Deny,
        LedgerAction::Ignored => return,
    };
    let Some(request_id) = view.selected_request_id() else { return };
    let reviewable = filtered_rows(&view.rows, &view.filter).get(view.selected).is_some_and(|row| row.reviewable());
    if !reviewable {
        app.note(awaits_reviewer(&request_id));
        return;
    }
    let (verdict, remember) = verdict_for(decision).expect("a decision key carries a verdict");
    app.ledger_view = None;
    app.asks_set_aside.insert(request_id.clone());
    app.ask_work.push(AskWork::Decide { request_id, verdict, remember });
}

/// Land one answer: say what the ledger did with it. An answer that did
/// not land, other than a lost race, un-sets its ask so its card can come
/// up again.
pub fn land_decision(app: &mut App, request_id: &str, answer: Result<kaijutsu_client::AskAnswer, CallError>) {
    let lost = match &answer {
        Ok(Ok(_)) => false,
        Ok(Err(failure)) => failure.kind != AskAnswerFailureKind::AlreadyAnswered,
        Err(_) => true,
    };
    if lost {
        app.asks_set_aside.remove(request_id);
    }
    app.note(decision_notice(request_id, answer));
}

/// The notice [`land_decision`] posts: the answer, a lost race (ordinary,
/// not an error), a refusal, or a call failure.
pub fn decision_notice(request_id: &str, answer: Result<kaijutsu_client::AskAnswer, CallError>) -> String {
    let id = short_ask(request_id);
    match answer {
        Ok(Ok(answered)) => {
            let verb = answered.summary.status.as_str();
            match answered.remembered {
                Some(rule) if rule.learned => format!("ask {id} {verb}; remembered: {}", rule.note),
                Some(rule) => format!("ask {id} {verb}; not remembered: {}", rule.note),
                None => format!("ask {id} {verb}"),
            }
        }
        Ok(Err(failure)) if failure.kind == AskAnswerFailureKind::AlreadyAnswered => {
            format!("ask {id} was already answered")
        }
        Ok(Err(failure)) => format!("ask {id} not answered: {}", failure.message),
        Err(error) => format!("cannot answer ask {id}: {error}"),
    }
}

/// Land a `Show` read into the open ledger view.
pub fn land_show(app: &mut App, request_id: &str, read: Result<Option<AskDetail>, CallError>) {
    match read {
        Ok(Some(detail)) => {
            if let Some(view) = app.ledger_view.as_mut() {
                view.detail = Some(detail);
            }
        }
        Ok(None) => app.note(format!("ask {} is not in the ledger", short_ask(request_id))),
        Err(error) => app.note(format!("cannot read ask {}: {error}", short_ask(request_id))),
    }
}

/// Every record the ledger view asked for, in the order asked: the open
/// asks, then the recent answers.
pub type LedgerReads = Vec<(String, Result<Option<AskDetail>, CallError>)>;

/// Land the ledger view's reads at `now_ms`. A record read answered is an
/// ANSWERED row wherever it was listed, and an ask in both listings is
/// listed once. An unreadable row is named on the status line.
pub fn land_ledger(app: &mut App, reads: Result<LedgerReads, CallError>, now_ms: u64) {
    let reads = match reads {
        Ok(reads) => reads,
        Err(error) => {
            app.note(format!("cannot read ledger history: {error}"));
            return;
        }
    };
    let mut rows = Vec::with_capacity(reads.len());
    let mut listed = std::collections::HashSet::new();
    for (request_id, read) in reads {
        match read {
            Ok(Some(detail)) if listed.insert(request_id.clone()) => {
                rows.push(if detail.summary.status.is_open() {
                    LedgerRow::Pending(pending_row(app, &detail, now_ms))
                } else {
                    LedgerRow::Answered(answered_row(app, &detail))
                })
            }
            Ok(_) => {}
            Err(error) => app.note(format!("cannot read ask {}: {error}", short_ask(&request_id))),
        }
    }
    rows.sort_by_key(|row| !matches!(row, LedgerRow::Pending(_)));
    app.ledger_view = Some(LedgerViewState { rows, filter: String::new(), selected: 0, filtering: false, detail: None });
}

/// One ask's record as the ledger view's PENDING row, its age taken at
/// `now_ms`.
fn pending_row(app: &App, detail: &AskDetail, now_ms: u64) -> PendingRow {
    let summary = &detail.summary;
    let (context_label, context_type) = summary
        .context_id
        .map(|ctx| context_facts(app, ctx))
        .unwrap_or_else(|| ("(unknown)".to_string(), "default".to_string()));
    PendingRow {
        request_id: summary.request_id.clone(),
        age: Some(crate::status::format_age(std::time::Duration::from_millis(
            now_ms.saturating_sub(summary.created_at_ms.max(0) as u64),
        ))),
        context_label,
        context_type,
        hook: detail.tool.clone().unwrap_or_else(|| "-".to_string()),
        asker: summary.performer.as_ref().map(|who| who.name.clone()),
        reviewer: summary.reviewer.as_ref().map(|who| who.name.clone()),
        reviewable: app.principal.is_some_and(|principal| summary.answerable_by(principal)),
        statement: summary.statements.first().cloned().unwrap_or_else(|| summary.description.clone()),
    }
}

/// One ask's record as the ledger view's ANSWERED row: when it was decided,
/// how (`allow once`/`allow always`/`deny`, or the status for an expired or
/// abandoned ask), and by whom (`you`, the decider's name, or `—` for a
/// rule's decision).
fn answered_row(app: &App, detail: &AskDetail) -> AnsweredRow {
    let summary = &detail.summary;
    let (context_label, _context_type) = summary
        .context_id
        .map(|ctx| context_facts(app, ctx))
        .unwrap_or_else(|| ("(unknown)".to_string(), "default".to_string()));
    let redeemed = match detail.redeemed_at_ms {
        Some(at) => RedeemedMark::At(crate::render::wallclock(at as u64)),
        None => RedeemedMark::Never,
    };
    AnsweredRow {
        request_id: summary.request_id.clone(),
        time: summary.decided_at_ms.map(|at| crate::render::wallclock(at as u64)),
        context_label,
        decision: Some(decision_words(detail)),
        principal: detail.decision.as_ref().and_then(|decision| decision.decided_by.as_ref()).map(|by| {
            if app.principal == Some(by.id) { "you".to_string() } else { by.name.clone() }
        }),
        redeemed,
        statement: summary.statements.first().cloned().unwrap_or_else(|| summary.description.clone()),
    }
}

/// The status-line notice for a card whose ask left the open set without
/// this card answering it: `ask <id> allow once by you`, `by <name>`, or
/// `expired`. Falls back to `no longer pending` when the record could not
/// be read; the card is already down either way.
pub fn answered_notice(me: Option<PrincipalId>, request_id: &str, detail: Option<&AskDetail>) -> String {
    let id = short_ask(request_id);
    let Some(detail) = detail else {
        return format!("ask {id} no longer pending");
    };
    let outcome = decision_words(detail);
    match detail.decision.as_ref().and_then(|decision| decision.decided_by.as_ref()) {
        Some(by) if me == Some(by.id) => format!("ask {id} {outcome} by you"),
        Some(by) => format!("ask {id} {outcome} by {}", by.name),
        None => format!("ask {id} {outcome}"),
    }
}

/// The first segment of a request id: enough to find it in `kj ledger
/// list`, short enough for a status-line notice beside the facts.
pub fn short_ask(request_id: &str) -> &str {
    request_id.split('-').next().unwrap_or(request_id)
}

/// `allow once` / `allow always` / `deny` from the decision's option, else
/// the coarser status (`allowed`, `denied`, `expired`, `abandoned`).
pub fn decision_words(detail: &AskDetail) -> String {
    match detail.decision.as_ref().and_then(|decision| decision.option.as_deref()) {
        Some(option) => option.replace('_', " "),
        None => detail.summary.status.as_str().to_string(),
    }
}

/// Records for tests across the crate: an open ask, its full record, and a
/// card over it.
#[cfg(test)]
pub(crate) mod fixtures {
    use kaijutsu_types::{AskDetail, AskOrigin, AskStatus, AskSummary, ContextId};

    use super::AskCardState;

    pub fn summary(request_id: &str, ctx: ContextId, statement: &str) -> AskSummary {
        AskSummary {
            request_id: request_id.to_string(),
            status: AskStatus::Pending,
            origin: AskOrigin::ShellGate,
            context_id: Some(ctx),
            description: statement.to_string(),
            statements: vec![statement.to_string()],
            requester: None,
            performer: None,
            reviewer: None,
            created_at_ms: 0,
            decided_at_ms: None,
        }
    }

    pub fn detail(request_id: &str, ctx: ContextId, statement: &str) -> AskDetail {
        AskDetail {
            summary: summary(request_id, ctx, statement),
            instance: None,
            tool: Some("shell_write".to_string()),
            hook_id: None,
            label: None,
            tool_call_block_id: None,
            exec_source: None,
            cwd: None,
            env: Vec::new(),
            decision: None,
            redeemed_at_ms: None,
            publication_abandoned: None,
            reassignments: Vec::new(),
        }
    }

    pub fn card(request_id: &str, ctx: ContextId, statement: &str) -> AskCardState {
        AskCardState { request_id: request_id.to_string(), context_id: ctx, detail: detail(request_id, ctx, statement) }
    }
}

/// What one ledger fold saw about a newly open ask — the whole input to
/// [`notify_target`], so the rule is one pure decision rather than a chain
/// of conditions at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotifySeen {
    /// Whether the terminal reports itself focused (`App::focused`).
    pub focused: bool,
    /// Whether this was the session's first synced fold. It brings every
    /// ask already open, including asks raised before this client attached;
    /// it is the baseline, not news.
    pub baseline: bool,
    /// The context of the ask the fold found newly open: one in the context
    /// on screen when there is one ([`LedgerFold::raised_in`]).
    pub raised_in: Option<ContextId>,
    /// The context on screen when the fold landed.
    pub current: Option<ContextId>,
}

/// The context whose new ask is worth a desktop notification, or `None`.
///
/// One ask, one notification, and only for the seat on screen: an ask in
/// another context is that seat's `!`, not a toast.
pub fn notify_target(seen: NotifySeen) -> Option<ContextId> {
    if seen.focused || seen.baseline {
        return None;
    }
    let raised_in = seen.raised_in?;
    (seen.current == Some(raised_in)).then_some(raised_in)
}

/// The desktop notification for an ask that landed while nobody was
/// looking, or `None` while the terminal has focus — an ask you are sitting
/// in front of is the card on screen, not a toast
/// (`docs/tui.md`, "What owning the screen lets us use").
///
/// Two sequences, because no one escape covers the target terminals: OSC
/// 777 (`notify`) is wezterm, kitty, foot and rxvt-unicode; OSC 9 is
/// iTerm2. A terminal that knows neither ignores both. Control bytes are
/// dropped from `label`: one inside a sequence would end it early and print
/// the rest onto the screen.
pub fn ask_notification(focused: bool, label: &str) -> Option<String> {
    if focused {
        return None;
    }
    let label: String = label.chars().filter(|c| !c.is_control()).collect();
    let body = format!("{label}: ask pending");
    Some(format!("\x1b]777;notify;kaijutsu;{body}\x1b\\\x1b]9;{body}\x1b\\"))
}

#[cfg(test)]
mod notify_tests {
    use super::*;
    use kaijutsu_types::ContextId;

    /// The round that first polls the ledger reports every ask already
    /// pending as new — asks raised before this client attached, some of
    /// them hours old. They are the baseline, not news.
    #[test]
    fn the_first_poll_round_of_a_session_never_notifies() {
        let ctx = ContextId::new();
        let seen = NotifySeen { focused: false, baseline: true, raised_in: Some(ctx), current: Some(ctx) };
        assert_eq!(notify_target(seen), None);
    }

    /// A switch can land between a round starting and its answer arriving.
    /// The seat the ask belongs to is no longer the seat on screen, and the
    /// notification would name the context the player just left.
    #[test]
    fn an_ask_for_a_context_no_longer_on_screen_never_notifies() {
        let seen = NotifySeen {
            focused: false,
            baseline: false,
            raised_in: Some(ContextId::new()),
            current: Some(ContextId::new()),
        };
        assert_eq!(notify_target(seen), None);
    }

    #[test]
    fn an_ask_raised_out_of_focus_on_the_seat_on_screen_notifies() {
        let ctx = ContextId::new();
        let seen = NotifySeen { focused: false, baseline: false, raised_in: Some(ctx), current: Some(ctx) };
        assert_eq!(notify_target(seen), Some(ctx));

        let focused = NotifySeen { focused: true, ..seen };
        assert_eq!(notify_target(focused), None);

        let nothing = NotifySeen { raised_in: None, ..seen };
        assert_eq!(notify_target(nothing), None);
    }

    #[test]
    fn a_focused_terminal_is_never_notified() {
        assert_eq!(ask_notification(true, "probe"), None);
    }

    #[test]
    fn an_unfocused_terminal_gets_one_notification_of_each_kind() {
        let bytes = ask_notification(false, "probe").expect("an unfocused terminal is notified");
        assert_eq!(bytes.matches("\x1b]777;notify;kaijutsu;").count(), 1, "got {bytes:?}");
        assert_eq!(bytes.matches("\x1b]9;").count(), 1, "got {bytes:?}");
        assert!(bytes.contains("probe: ask pending"), "got {bytes:?}");
    }

    /// A label carries whatever a player typed. An escape byte inside one
    /// would end the sequence early and print the rest onto the screen, so
    /// control bytes never reach the terminal.
    #[test]
    fn a_control_byte_in_a_label_never_reaches_the_terminal() {
        let bytes = ask_notification(false, "pr\x1b]0;pwned\x07obe").expect("notified");
        // Two sequences, each an `ESC ]` introducer and an `ESC \` terminator.
        assert_eq!(bytes.matches('\x1b').count(), 4, "no escape but the four it writes: {bytes:?}");
        assert!(!bytes.contains("\x07"), "got {bytes:?}");
        assert!(bytes.contains("pr]0;pwnedobe: ask pending"), "got {bytes:?}");
    }
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

    /// `Ctrl+A` is the prefix and `Ctrl+D` is a vi scroll; neither answers
    /// the selected ask.
    #[test]
    fn a_ctrl_chord_is_never_a_ledger_decision() {
        for c in ['a', 'd', 'A'] {
            let chord = KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
            assert_eq!(ledger_key_to_action(chord, false), LedgerAction::Ignored);
            let alt = KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT);
            assert_eq!(ledger_key_to_action(alt, false), LedgerAction::Ignored);
        }
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
                request_id: "01a04eb6", hook: "shell_write", context_label: "kaijutsu", context_type: "coder",
                statement: "rm -rf ~/src/wt/kaish-arith", asker: None, reviewer: None, can_review, armed: false,
            };
            let text: Vec<String> = render_ask_card(&card, 80, &Palette::builtin()).iter().map(line_text).collect();
            assert_eq!(text.last().map(String::as_str), Some(ARMING_KEY_LINE));
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
            context_type: "coder",
            statement: "one two three four five six seven eight nine ten",
            asker: None,
            reviewer: None,
            can_review: true,
            armed: true,
        };
        let lines = render_ask_card(&card, 24, &Palette::builtin());
        // Header, N wrapped statement lines, key line.
        assert!(lines.len() > 3, "expected the statement to wrap: {lines:?}");
    }

    #[test]
    fn an_ask_the_viewer_cannot_review_offers_cancel_or_escalation_not_approval() {
        let card = AskCard {
            request_id: "id", hook: "shell_write", context_label: "kaijutsu", context_type: "coder",
            statement: "rm -rf", asker: Some("coder"), reviewer: Some("lead"), can_review: false, armed: true,
        };
        let text: Vec<String> = render_ask_card(&card, 120, &Palette::builtin())
            .iter().map(line_text).collect();
        assert!(text.last().unwrap().contains("awaiting assigned reviewer"));
        assert!(!text.last().unwrap().contains("allow once"));
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
            context_type: "coder",
            statement: "one two three four five six seven eight nine ten eleven twelve",
            asker: None,
            reviewer: None,
            can_review: true,
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
        let rows = vec![pending("p1", "kaijutsu"), pending("p2", "scorer")];
        let visible = filtered_rows(&rows, "scorer");
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].request_id(), "p2");
    }

    #[test]
    fn an_empty_filter_admits_everything() {
        let rows = vec![pending("p1", "kaijutsu"), pending("p2", "scorer")];
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
            context_type: "coder",
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

#[cfg(test)]
mod ledger_tests {
    use super::fixtures::{card, detail, summary};
    use super::*;
    use kaijutsu_types::{AskDecision as Decision, AskSummary, PrincipalRef};

    fn state(asks: &[AskSummary]) -> LedgerState {
        LedgerState::from_listing(1, asks.to_vec())
    }

    fn reviewer(id: PrincipalId, name: &str) -> Option<PrincipalRef> {
        Some(PrincipalRef { id, name: name.to_string() })
    }

    #[test]
    fn an_unsynced_state_changes_nothing() {
        let ctx = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        app.open_asks.insert("a-1".into(), summary("a-1", ctx, "rm"));
        app.ask_card = Some(card("a-1", ctx, "rm"));
        let unsynced = LedgerState::default();
        assert_eq!(fold_ledger(&mut app, &unsynced), None);
        assert!(app.has_pending_ask(ctx), "an empty listing that was never read is not an empty ledger");
        assert!(app.ask_card.is_some());
        assert_eq!(app.status_model(0).pending_asks, 1);
    }

    #[test]
    fn the_first_fold_is_the_baseline_and_later_ones_report_new_asks() {
        let ctx = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        let first = fold_ledger(&mut app, &state(&[summary("a-1", ctx, "rm")])).unwrap();
        assert!(first.baseline);
        assert_eq!(first.raised_in, Some(ctx));
        let again = fold_ledger(&mut app, &state(&[summary("a-1", ctx, "rm")])).unwrap();
        assert!(!again.baseline);
        assert_eq!(again.raised_in, None, "an ask already folded in is not news");
        let other = ContextId::new();
        let both = state(&[summary("a-1", ctx, "rm"), summary("b-2", other, "ls"), summary("c-3", ctx, "cp")]);
        assert_eq!(fold_ledger(&mut app, &both).unwrap().raised_in, Some(ctx), "the seat on screen wins");
    }

    #[test]
    fn an_open_ask_in_the_current_context_is_the_card_candidate_and_counts() {
        let ctx = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        fold_ledger(&mut app, &state(&[summary("a-1", ctx, "rm")]));
        assert_eq!(card_candidate(&app).as_deref(), Some("a-1"));
        assert!(open_card(&mut app, detail("a-1", ctx, "rm")));
        assert_eq!(app.ask_card.as_ref().map(|c| c.request_id.as_str()), Some("a-1"));
        assert!(app.has_pending_ask(ctx));
        assert_eq!(app.status_model(0).pending_asks, 1);
    }

    #[test]
    fn a_card_never_opens_over_one_already_up() {
        let ctx = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        fold_ledger(&mut app, &state(&[summary("a-1", ctx, "rm")]));
        assert!(open_card(&mut app, detail("a-1", ctx, "rm")));
        fold_ledger(&mut app, &state(&[summary("a-1", ctx, "rm"), summary("b-2", ctx, "ls")]));
        assert_eq!(card_candidate(&app), None);
        assert!(!open_card(&mut app, detail("b-2", ctx, "ls")));
        assert_eq!(app.ask_card.as_ref().map(|c| c.request_id.as_str()), Some("a-1"));
        assert_eq!(app.status_model(0).pending_asks, 2);
    }

    #[test]
    fn the_oldest_open_ask_comes_up_first_and_the_next_follows_it() {
        let ctx = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        let mut older = summary("z-older", ctx, "rm");
        older.created_at_ms = 10;
        let mut newer = summary("a-newer", ctx, "ls");
        newer.created_at_ms = 20;
        fold_ledger(&mut app, &state(&[older.clone(), newer.clone()]));
        assert_eq!(card_candidate(&app).as_deref(), Some("z-older"));
        assert!(open_card(&mut app, detail("z-older", ctx, "rm")));

        // Answered elsewhere: the card comes down and the next ask is the
        // candidate, so the next card goes up rather than only a `!`.
        let fold = fold_ledger(&mut app, &state(&[newer])).unwrap();
        assert_eq!(fold.answered.as_deref(), Some("z-older"));
        assert!(app.ask_card.is_none());
        assert_eq!(card_candidate(&app).as_deref(), Some("a-newer"));
    }

    #[test]
    fn a_set_aside_ask_waits_for_a_switch_back_and_stays_aside() {
        let ctx = ContextId::new();
        let other = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        fold_ledger(&mut app, &state(&[summary("a-1", ctx, "rm"), summary("b-2", ctx, "ls")]));
        app.asks_set_aside.insert("a-1".into());
        assert_eq!(card_candidate(&app).as_deref(), Some("b-2"), "Esc on one card lets the next one up");
        assert!(open_card(&mut app, detail("b-2", ctx, "ls")));

        app.switch_to(other);
        assert!(app.ask_card.is_none(), "a switch takes the card down");
        assert_eq!(card_candidate(&app), None, "no card for a seat you are not looking at");
        app.switch_to(ctx);
        assert_eq!(card_candidate(&app).as_deref(), Some("b-2"), "the card comes back on return");
    }

    #[test]
    fn a_card_read_that_lands_after_a_switch_does_not_go_up() {
        let ctx = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        fold_ledger(&mut app, &state(&[summary("a-1", ctx, "rm")]));
        app.switch_to(ContextId::new());
        assert!(!open_card(&mut app, detail("a-1", ctx, "rm")));
        assert!(app.ask_card.is_none());
    }

    #[test]
    fn an_other_context_open_ask_keeps_its_seat_indicator_without_a_card() {
        let current = ContextId::new();
        let other = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(current);
        fold_ledger(&mut app, &state(&[summary("o-1", other, "rm")]));
        assert!(app.has_pending_ask(other));
        assert!(!app.has_pending_ask(current));
        assert_eq!(card_candidate(&app), None);
    }

    #[test]
    fn an_open_card_follows_its_reassignment() {
        let ctx = ContextId::new();
        let me = PrincipalId::new();
        let lead = PrincipalId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        app.principal = Some(me);
        let mut mine = summary("a-1", ctx, "rm");
        mine.performer = reviewer(PrincipalId::new(), "coder");
        mine.reviewer = reviewer(me, "amy");
        fold_ledger(&mut app, &state(&[mine.clone()]));
        assert!(open_card(&mut app, detail("a-1", ctx, "rm")));
        assert!(app.ask_card.as_ref().unwrap().detail.summary.answerable_by(me));

        let mut moved = mine;
        moved.reviewer = reviewer(lead, "lead");
        fold_ledger(&mut app, &state(&[moved]));
        let shown = &app.ask_card.as_ref().unwrap().detail.summary;
        assert_eq!(shown.reviewer.as_ref().map(|r| r.id), Some(lead));
        assert!(!shown.answerable_by(me), "the card stops offering approval keys");
    }

    #[test]
    fn a_card_whose_ask_left_the_open_set_comes_down_and_clears_the_seat() {
        let ctx = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        fold_ledger(&mut app, &state(&[summary("a-1", ctx, "rm")]));
        assert!(open_card(&mut app, detail("a-1", ctx, "rm")));
        app.asks_set_aside.insert("gone".into());
        let fold = fold_ledger(&mut app, &state(&[])).unwrap();
        assert_eq!(fold.answered.as_deref(), Some("a-1"));
        assert!(app.ask_card.is_none());
        assert!(!app.has_pending_ask(ctx), "the seat's `!` clears with the ask");
        assert_eq!(app.status_model(0).pending_asks, 0);
        assert!(app.asks_set_aside.is_empty(), "set-aside ids are pruned to the open set");
    }

    #[test]
    fn a_card_answered_here_raises_no_notice_when_its_ask_closes() {
        let ctx = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        fold_ledger(&mut app, &state(&[summary("a-1", ctx, "rm")]));
        assert!(open_card(&mut app, detail("a-1", ctx, "rm")));
        // The card's own key takes it down and sets the ask aside, so it
        // does not come back before the push closes it.
        app.ask_card = None;
        app.asks_set_aside.insert("a-1".into());
        assert_eq!(card_candidate(&app), None);
        assert_eq!(fold_ledger(&mut app, &state(&[])).unwrap().answered, None);
    }

    /// An answer goes through the ledger and nowhere else, and the notice
    /// says what the ledger did with it: a lost race is ordinary, not an
    /// error (`docs/tui.md`, "Asks").
    #[test]
    fn a_decision_notice_reports_the_answer_the_race_and_the_refusal() {
        use kaijutsu_types::{AskAnswerFailure, AskAnswerFailureKind, AskAnswered, AskStatus, RememberResult};
        let ctx = ContextId::new();
        let mut allowed = summary("01a04eb6-77", ctx, "rm");
        allowed.status = AskStatus::Allowed;
        let answered = AskAnswered { summary: allowed.clone(), remembered: None };
        assert_eq!(decision_notice("01a04eb6-77", Ok(Ok(answered))), "ask 01a04eb6 allowed");
        let learned = AskAnswered {
            summary: allowed,
            remembered: Some(RememberResult { learned: false, note: "no family for this command".into() }),
        };
        assert_eq!(
            decision_notice("01a04eb6-77", Ok(Ok(learned))),
            "ask 01a04eb6 allowed; not remembered: no family for this command"
        );
        let raced = AskAnswerFailure { kind: AskAnswerFailureKind::AlreadyAnswered, message: "claimed".into() };
        assert_eq!(decision_notice("01a04eb6-77", Ok(Err(raced))), "ask 01a04eb6 was already answered");
        let refused = AskAnswerFailure { kind: AskAnswerFailureKind::NotReviewer, message: "not the reviewer".into() };
        assert_eq!(decision_notice("01a04eb6-77", Ok(Err(refused))), "ask 01a04eb6 not answered: not the reviewer");
    }

    #[test]
    fn allow_always_asks_for_a_standing_rule_and_view_answers_nothing() {
        use kaijutsu_types::{AskVerdict, Remember, RememberScope};
        assert_eq!(verdict_for(AskDecision::AllowOnce), Some((AskVerdict::Allow, None)));
        assert_eq!(
            verdict_for(AskDecision::AllowAlways),
            Some((AskVerdict::Allow, Some(Remember { scope: RememberScope::Always, family: false })))
        );
        assert_eq!(verdict_for(AskDecision::Deny), Some((AskVerdict::Deny, None)));
        assert_eq!(verdict_for(AskDecision::ViewLedger), None);
    }

    fn reviewed_by(me: PrincipalId, id: &str, ctx: ContextId) -> AskSummary {
        let mut ask = summary(id, ctx, "rm");
        ask.performer = reviewer(PrincipalId::new(), "coder");
        ask.reviewer = reviewer(me, "amy");
        ask
    }

    /// An app with `ids` open in the context on screen, reviewed by its own
    /// principal, and the first one's card up.
    fn app_with_card(ids: &[&str]) -> (App, ContextId) {
        let ctx = ContextId::new();
        let me = PrincipalId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        app.principal = Some(me);
        let open: Vec<AskSummary> = ids.iter().map(|id| reviewed_by(me, id, ctx)).collect();
        fold_ledger(&mut app, &state(&open));
        assert!(open_card(&mut app, detail(ids[0], ctx, "rm")));
        (app, ctx)
    }

    #[test]
    fn esc_on_the_card_sets_its_ask_aside_and_lets_the_next_one_up() {
        let (mut app, _ctx) = app_with_card(&["a-1", "b-2"]);
        card_key(&mut app, AskCardKey::Aside);
        assert!(app.ask_card.is_none());
        assert!(app.asks_set_aside.contains("a-1"));
        assert_eq!(card_candidate(&app).as_deref(), Some("b-2"), "the set-aside card does not come straight back");
        assert!(app.ask_work.is_empty(), "Esc asks the kernel for nothing");
    }

    #[test]
    fn a_decision_key_records_the_answer_and_v_records_the_ledger() {
        let (mut app, _ctx) = app_with_card(&["a-1"]);
        card_key(&mut app, AskCardKey::Decide(AskDecision::AllowAlways));
        assert_eq!(card_candidate(&app), None, "an answered card waits for the push to close it");
        let (verdict, remember) = verdict_for(AskDecision::AllowAlways).unwrap();
        assert_eq!(app.ask_work, vec![AskWork::Decide { request_id: "a-1".into(), verdict, remember }]);

        let (mut app, _ctx) = app_with_card(&["a-1"]);
        card_key(&mut app, AskCardKey::Decide(AskDecision::ViewLedger));
        assert_eq!(card_candidate(&app), None);
        assert_eq!(app.ask_work, vec![AskWork::OpenLedger { open: vec!["a-1".into()] }]);
    }

    #[test]
    fn a_card_the_viewer_cannot_answer_stays_up_and_records_nothing() {
        let (mut app, _ctx) = app_with_card(&["a-1"]);
        app.principal = Some(PrincipalId::new());
        card_key(&mut app, AskCardKey::Decide(AskDecision::AllowOnce));
        assert_eq!(app.ask_card.as_ref().map(|c| c.request_id.as_str()), Some("a-1"));
        assert!(app.ask_work.is_empty());
        assert!(!app.asks_set_aside.contains("a-1"));
    }

    #[test]
    fn an_answer_that_did_not_land_puts_its_card_back_and_a_lost_race_does_not() {
        use kaijutsu_types::{AskAnswerFailure, AskAnswerFailureKind};
        let (mut app, _ctx) = app_with_card(&["a-1"]);
        card_key(&mut app, AskCardKey::Decide(AskDecision::Deny));
        land_decision(&mut app, "a-1", Err(CallError::PermanentlyFailed("pipe closed".into())));
        assert_eq!(app.notice(), Some("cannot answer ask a: permanently failed: pipe closed"));
        assert_eq!(card_candidate(&app).as_deref(), Some("a-1"), "the ask is still open and answerable");

        card_key(&mut app, AskCardKey::Aside);
        app.asks_set_aside.insert("a-1".into());
        let refused = AskAnswerFailure { kind: AskAnswerFailureKind::Archived, message: "archived".into() };
        land_decision(&mut app, "a-1", Ok(Err(refused)));
        assert_eq!(card_candidate(&app).as_deref(), Some("a-1"));

        app.asks_set_aside.insert("a-1".into());
        let raced = AskAnswerFailure { kind: AskAnswerFailureKind::AlreadyAnswered, message: "claimed".into() };
        land_decision(&mut app, "a-1", Ok(Err(raced)));
        assert_eq!(card_candidate(&app), None, "the push is about to close it");
    }

    #[test]
    fn a_card_read_that_finds_nothing_is_not_read_again_until_the_next_fold() {
        let ctx = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        fold_ledger(&mut app, &state(&[summary("a-1", ctx, "rm")]));
        land_card_read(&mut app, "a-1", Ok(None));
        assert_eq!(card_candidate(&app), None, "a stale open set does not spin the read");
        land_card_read(&mut app, "a-1", Err(CallError::PermanentlyFailed("gone".into())));
        assert_eq!(card_candidate(&app), None);
        fold_ledger(&mut app, &state(&[summary("a-1", ctx, "rm")]));
        assert_eq!(card_candidate(&app).as_deref(), Some("a-1"));
        land_card_read(&mut app, "a-1", Ok(Some(detail("a-1", ctx, "rm"))));
        assert!(app.ask_card.is_some());
    }

    fn ledger_with(ids: &[&str]) -> App {
        let mut app = App::new("amy");
        app.ledger_view = Some(LedgerViewState {
            rows: ids
                .iter()
                .map(|id| {
                    LedgerRow::Pending(PendingRow {
                        request_id: id.to_string(),
                        age: None,
                        context_label: "kaijutsu".into(),
                        context_type: "coder".into(),
                        hook: "shell_write".into(),
                        asker: None,
                        reviewer: None,
                        reviewable: true,
                        statement: format!("echo {id}"),
                    })
                })
                .collect(),
            ..Default::default()
        });
        app
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn typing_a_filter_keeps_the_cursor_on_a_row_it_admits() {
        let mut app = ledger_with(&["alpha", "beta", "gamma"]);
        ledger_key(&mut app, key(KeyCode::Char('j')));
        ledger_key(&mut app, key(KeyCode::Char('j')));
        ledger_key(&mut app, key(KeyCode::Char('/')));
        for c in "alpha".chars() {
            ledger_key(&mut app, key(KeyCode::Char(c)));
        }
        let view = app.ledger_view.as_ref().unwrap();
        assert_eq!(view.selected_request_id().as_deref(), Some("alpha"), "the cursor follows the filter");
        ledger_key(&mut app, key(KeyCode::Enter));
        ledger_key(&mut app, key(KeyCode::Enter));
        assert_eq!(app.ask_work, vec![AskWork::Show("alpha".into())]);
        ledger_key(&mut app, key(KeyCode::Char('d')));
        assert!(app.ledger_view.is_none(), "an answer closes the view");
        assert!(matches!(app.ask_work.last(), Some(AskWork::Decide { request_id, .. }) if request_id == "alpha"));
    }

    #[test]
    fn the_ledger_lists_an_ask_answered_mid_read_once_as_answered() {
        let ctx = ContextId::new();
        let mut app = App::new("amy");
        let open = detail("o-1", ctx, "ls");
        let mut done = detail("a-1", ctx, "rm");
        done.summary.status = kaijutsu_types::AskStatus::Allowed;
        let reads = vec![
            ("a-1".to_string(), Ok(Some(done.clone()))),
            ("o-1".to_string(), Ok(Some(open))),
            ("a-1".to_string(), Ok(Some(done))),
        ];
        land_ledger(&mut app, Ok(reads), 0);
        let rows = &app.ledger_view.as_ref().unwrap().rows;
        let shape: Vec<(bool, &str)> =
            rows.iter().map(|row| (matches!(row, LedgerRow::Pending(_)), row.request_id())).collect();
        assert_eq!(shape, vec![(true, "o-1"), (false, "a-1")]);
    }

    #[test]
    fn notice_wording_names_the_decider_or_the_outcome_alone() {
        let ctx = ContextId::new();
        let me = PrincipalId::new();
        let other = PrincipalId::new();
        let mut decided = detail("01a04eb6-77", ctx, "rm");
        decided.summary.status = kaijutsu_types::AskStatus::Denied;
        decided.decision = Some(Decision {
            decided_by: reviewer(other, "lead"),
            option: Some("deny".into()),
            remember_scope: None,
            auto_reason: None,
        });
        assert_eq!(answered_notice(Some(me), "01a04eb6-77", Some(&decided)), "ask 01a04eb6 deny by lead");
        let mut mine = decided.clone();
        mine.decision = Some(Decision { decided_by: reviewer(me, "amy"), option: Some("allow_once".into()),
            remember_scope: None, auto_reason: None });
        assert_eq!(answered_notice(Some(me), "01a04eb6-77", Some(&mine)), "ask 01a04eb6 allow once by you");
        let mut expired = detail("01a04eb6-77", ctx, "rm");
        expired.summary.status = kaijutsu_types::AskStatus::Expired;
        assert_eq!(answered_notice(None, "01a04eb6-77", Some(&expired)), "ask 01a04eb6 expired");
        assert_eq!(answered_notice(None, "01a04eb6-77", None), "ask 01a04eb6 no longer pending");
    }
}
