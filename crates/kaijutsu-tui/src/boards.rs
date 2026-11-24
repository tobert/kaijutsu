//! Full-screen Message Board browser (`Ctrl+A b`).
//!
//! Pure model + renderer over `kj board` (`docs/board.md`): no RPC, no I/O,
//! no clock. The event loop drives `bridge.execute_kj` in response to
//! [`BoardsOutcome`] and feeds stdout back into [`BoardsModel`].

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthChar;

use crate::present::Palette;

/// Virtual board handle showing recent topics across all boards (`kj board recent`).
pub const RECENT_BOARD: &str = "recent";

/// One entry in the boards pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardEntry {
    /// Board handle passed to `kj board topics` (`"."`, `"coder/notes"`, or `"recent"`).
    pub handle: String,
    /// Write policy (`open`, `curated`, `locked`, or empty for the virtual `recent` view).
    pub policy: String,
    pub description: String,
}

/// One entry in the topics pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicEntry {
    /// Short topic document id (passed to `kj board read|reply|resolve|reopen|pin|unpin`).
    pub id: String,
    /// Status string (`open`, `pinned`, `resolved`, `archived`).
    pub status: String,
    /// Post count badge (e.g. `"3p"`).
    pub posts: String,
    /// Compact age (e.g. `"4m"`).
    pub age: String,
    /// Slug + optional `[locked]` + title + tags.
    pub summary: String,
}

/// Which pane currently owns navigation keys (`j`/`k`, `PgUp`/`PgDn`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Boards,
    Topics,
    Posts,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Focus::Boards => Focus::Topics,
            Focus::Topics => Focus::Posts,
            Focus::Posts => Focus::Boards,
        }
    }

    fn prev(self) -> Self {
        match self {
            Focus::Boards => Focus::Posts,
            Focus::Topics => Focus::Boards,
            Focus::Posts => Focus::Topics,
        }
    }
}

/// Inline compose prompt at the foot of the posts pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardCompose {
    Reply { topic_id: String, body: String },
    NewTopicTitle { board: String, title: String },
    NewTopicBody { board: String, title: String, body: String },
}

/// What a keystroke asks the event loop to run against the kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardsOutcome {
    None,
    Dismiss,
    LoadBoards,
    LoadTopics { board: String },
    LoadPosts { topic_id: String },
    Reply { topic_id: String, body: String },
    NewTopic { board: String, title: String, body: String },
    ToggleResolve { topic_id: String, currently_resolved: bool },
    TogglePin { topic_id: String, currently_pinned: bool },
}

/// Pure state for the full-screen message board browser.
#[derive(Debug, Default)]
pub struct BoardsModel {
    pub boards: Vec<BoardEntry>,
    pub board_idx: usize,
    pub topics: Vec<TopicEntry>,
    pub topic_idx: usize,
    pub posts_text: String,
    pub posts_scroll: usize,
    pub focus: Focus,
    pub compose: Option<BoardCompose>,
    pub status: Option<String>,
}

impl BoardsModel {
    pub fn new() -> Self {
        Self {
            status: Some("loading boards…".to_string()),
            ..Self::default()
        }
    }

    pub fn selected_board(&self) -> Option<&BoardEntry> {
        self.boards.get(self.board_idx)
    }

    pub fn selected_topic(&self) -> Option<&TopicEntry> {
        self.topics.get(self.topic_idx)
    }

    pub fn set_boards(&mut self, boards: Vec<BoardEntry>) {
        let prev = self.selected_board().map(|b| b.handle.clone());
        self.boards = boards;
        self.board_idx = prev
            .and_then(|h| self.boards.iter().position(|b| b.handle == h))
            .unwrap_or(0)
            .min(self.boards.len().saturating_sub(1));
        self.status = None;
    }

    pub fn set_topics(&mut self, topics: Vec<TopicEntry>) {
        let prev = self.selected_topic().map(|t| t.id.clone());
        self.topics = topics;
        self.topic_idx = prev
            .and_then(|id| self.topics.iter().position(|t| t.id == id))
            .unwrap_or(0)
            .min(self.topics.len().saturating_sub(1));
        if self.topics.is_empty() {
            self.posts_text.clear();
            self.posts_scroll = 0;
        }
        self.status = None;
    }

    pub fn set_posts(&mut self, text: String) {
        self.posts_text = text;
        self.posts_scroll = 0;
        self.status = None;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> BoardsOutcome {
        if key.kind == KeyEventKind::Release {
            return BoardsOutcome::None;
        }
        if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            return BoardsOutcome::None;
        }

        if self.compose.is_some() {
            return self.handle_compose_key(key);
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => BoardsOutcome::Dismiss,
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                self.focus = self.focus.next();
                BoardsOutcome::None
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                self.focus = self.focus.prev();
                BoardsOutcome::None
            }
            KeyCode::Char('j') | KeyCode::Down => self.step(1),
            KeyCode::Char('k') | KeyCode::Up => self.step(-1),
            KeyCode::PageDown => self.step(10),
            KeyCode::PageUp => self.step(-10),
            KeyCode::Home | KeyCode::Char('g') => self.jump_start(),
            KeyCode::End | KeyCode::Char('G') => self.jump_end(),
            KeyCode::Enter => match self.focus {
                Focus::Boards => {
                    self.focus = Focus::Topics;
                    self.selected_board()
                        .map(|b| BoardsOutcome::LoadTopics { board: b.handle.clone() })
                        .unwrap_or(BoardsOutcome::None)
                }
                Focus::Topics => {
                    self.focus = Focus::Posts;
                    self.selected_topic()
                        .map(|t| BoardsOutcome::LoadPosts { topic_id: t.id.clone() })
                        .unwrap_or(BoardsOutcome::None)
                }
                Focus::Posts => BoardsOutcome::None,
            },
            KeyCode::Char('r') => match self.focus {
                Focus::Boards => BoardsOutcome::LoadBoards,
                Focus::Topics => self
                    .selected_board()
                    .map(|b| BoardsOutcome::LoadTopics { board: b.handle.clone() })
                    .unwrap_or(BoardsOutcome::LoadBoards),
                Focus::Posts => self
                    .selected_topic()
                    .map(|t| BoardsOutcome::LoadPosts { topic_id: t.id.clone() })
                    .unwrap_or(BoardsOutcome::LoadBoards),
            },
            KeyCode::Char('R') => {
                if let Some(topic) = self.selected_topic() {
                    self.compose = Some(BoardCompose::Reply {
                        topic_id: topic.id.clone(),
                        body: String::new(),
                    });
                    self.focus = Focus::Posts;
                } else {
                    self.status = Some("select a topic to reply to".to_string());
                }
                BoardsOutcome::None
            }
            KeyCode::Char('n') => {
                if let Some(board) = self.selected_board() {
                    if board.handle == RECENT_BOARD {
                        self.status = Some("select a board (not 'recent') to post a new topic".to_string());
                    } else {
                        self.compose = Some(BoardCompose::NewTopicTitle {
                            board: board.handle.clone(),
                            title: String::new(),
                        });
                        self.focus = Focus::Posts;
                    }
                } else {
                    self.status = Some("no board selected".to_string());
                }
                BoardsOutcome::None
            }
            KeyCode::Char('x') => match self.selected_topic() {
                Some(t) => BoardsOutcome::ToggleResolve {
                    topic_id: t.id.clone(),
                    currently_resolved: t.status == "resolved",
                },
                None => BoardsOutcome::None,
            },
            KeyCode::Char('p') => match self.selected_topic() {
                Some(t) => BoardsOutcome::TogglePin {
                    topic_id: t.id.clone(),
                    currently_pinned: t.status == "pinned",
                },
                None => BoardsOutcome::None,
            },
            _ => BoardsOutcome::None,
        }
    }

    fn handle_compose_key(&mut self, key: KeyEvent) -> BoardsOutcome {
        let Some(compose) = self.compose.take() else {
            return BoardsOutcome::None;
        };
        match key.code {
            KeyCode::Esc => {
                self.status = Some("cancelled".to_string());
                BoardsOutcome::None
            }
            KeyCode::Enter => match compose {
                BoardCompose::Reply { topic_id, body } => {
                    let body = body.trim().to_string();
                    if body.is_empty() {
                        self.status = Some("reply body is empty".to_string());
                        BoardsOutcome::None
                    } else {
                        BoardsOutcome::Reply { topic_id, body }
                    }
                }
                BoardCompose::NewTopicTitle { board, title } => {
                    let title = title.trim().to_string();
                    if title.is_empty() {
                        self.status = Some("topic title is empty".to_string());
                        BoardsOutcome::None
                    } else {
                        self.compose = Some(BoardCompose::NewTopicBody {
                            board,
                            title,
                            body: String::new(),
                        });
                        BoardsOutcome::None
                    }
                }
                BoardCompose::NewTopicBody { board, title, body } => {
                    let body = body.trim().to_string();
                    if body.is_empty() {
                        self.status = Some("topic body is empty".to_string());
                        BoardsOutcome::None
                    } else {
                        BoardsOutcome::NewTopic { board, title, body }
                    }
                }
            },
            KeyCode::Backspace => {
                let mut next = compose;
                match &mut next {
                    BoardCompose::Reply { body, .. }
                    | BoardCompose::NewTopicBody { body, .. } => {
                        body.pop();
                    }
                    BoardCompose::NewTopicTitle { title, .. } => {
                        title.pop();
                    }
                }
                self.compose = Some(next);
                BoardsOutcome::None
            }
            KeyCode::Char(c) => {
                let mut next = compose;
                match &mut next {
                    BoardCompose::Reply { body, .. }
                    | BoardCompose::NewTopicBody { body, .. } => {
                        body.push(c);
                    }
                    BoardCompose::NewTopicTitle { title, .. } => {
                        title.push(c);
                    }
                }
                self.compose = Some(next);
                BoardsOutcome::None
            }
            _ => {
                self.compose = Some(compose);
                BoardsOutcome::None
            }
        }
    }

    fn step(&mut self, delta: isize) -> BoardsOutcome {
        match self.focus {
            Focus::Boards => {
                if self.boards.is_empty() {
                    return BoardsOutcome::None;
                }
                let prev = self.board_idx;
                let max = self.boards.len() - 1;
                self.board_idx = (self.board_idx as isize + delta).clamp(0, max as isize) as usize;
                if self.board_idx != prev {
                    self.selected_board()
                        .map(|b| BoardsOutcome::LoadTopics { board: b.handle.clone() })
                        .unwrap_or(BoardsOutcome::None)
                } else {
                    BoardsOutcome::None
                }
            }
            Focus::Topics => {
                if self.topics.is_empty() {
                    return BoardsOutcome::None;
                }
                let prev = self.topic_idx;
                let max = self.topics.len() - 1;
                self.topic_idx = (self.topic_idx as isize + delta).clamp(0, max as isize) as usize;
                if self.topic_idx != prev {
                    self.selected_topic()
                        .map(|t| BoardsOutcome::LoadPosts { topic_id: t.id.clone() })
                        .unwrap_or(BoardsOutcome::None)
                } else {
                    BoardsOutcome::None
                }
            }
            Focus::Posts => {
                let total = self.posts_text.lines().count();
                let max = total.saturating_sub(1);
                self.posts_scroll = (self.posts_scroll as isize + delta).clamp(0, max as isize) as usize;
                BoardsOutcome::None
            }
        }
    }

    fn jump_start(&mut self) -> BoardsOutcome {
        match self.focus {
            Focus::Boards => self.step(-(self.board_idx as isize)),
            Focus::Topics => self.step(-(self.topic_idx as isize)),
            Focus::Posts => {
                self.posts_scroll = 0;
                BoardsOutcome::None
            }
        }
    }

    fn jump_end(&mut self) -> BoardsOutcome {
        match self.focus {
            Focus::Boards => self.step(self.boards.len() as isize),
            Focus::Topics => self.step(self.topics.len() as isize),
            Focus::Posts => {
                self.posts_scroll = self.posts_text.lines().count().saturating_sub(1);
                BoardsOutcome::None
            }
        }
    }
}

/// Parse `kj board list` stdout into [`BoardEntry`] rows, always appending the
/// virtual `recent` board so cross-board activity is one keypress away.
pub fn parse_board_list(stdout: &str) -> Vec<BoardEntry> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "(no boards)" {
            continue;
        }
        let parts: Vec<&str> = trimmed.split("  ").map(str::trim).filter(|s| !s.is_empty()).collect();
        let Some(handle) = parts.first() else {
            continue;
        };
        let policy = parts
            .get(1)
            .map(|s| s.trim_matches(|c| c == '[' || c == ']').to_string())
            .unwrap_or_default();
        let description = if parts.len() > 2 {
            parts[2..].join("  ")
        } else {
            String::new()
        };
        out.push(BoardEntry {
            handle: (*handle).to_string(),
            policy,
            description,
        });
    }
    out.push(BoardEntry {
        handle: RECENT_BOARD.to_string(),
        policy: String::new(),
        description: "all recent topics".to_string(),
    });
    out
}

/// Parse `kj board topics` / `kj board recent` stdout into [`TopicEntry`] rows.
///
/// Expected line format (`kj/board.rs::format_topic_line`):
/// `<short_id>  [<status>]  <N>p  <age>  <slug>  "<title>"`
pub fn parse_topic_list(stdout: &str) -> Vec<TopicEntry> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "(no topics)" || trimmed.starts_with("(no ") {
            continue;
        }
        // Format: `<short_id>  [<status padded>]  <N>p  <age>  <summary...>`
        let Some((id_part, after_bracket_open)) = trimmed.split_once('[') else {
            continue;
        };
        let Some((status_part, rest)) = after_bracket_open.split_once(']') else {
            continue;
        };
        let id = id_part.trim().to_string();
        if id.is_empty() {
            continue;
        }
        let status = status_part.trim().to_string();
        let mut rest_iter = rest.trim().splitn(3, char::is_whitespace);
        let posts = rest_iter.next().unwrap_or("").trim().to_string();
        let after_posts = rest_iter.next().unwrap_or("").trim_start();
        let mut tail = format!("{after_posts} {}", rest_iter.next().unwrap_or("")).trim().to_string();
        // Split `<age>` off the front of `tail`.
        let (age, summary) = match tail.split_once(char::is_whitespace) {
            Some((a, s)) => (a.trim().to_string(), s.trim().to_string()),
            None => (std::mem::take(&mut tail), String::new()),
        };
        out.push(TopicEntry {
            id,
            status,
            posts,
            age,
            summary,
        });
    }
    out
}

// ============================================================================
// RENDER
// ============================================================================

const HINTS_FULL: &str = "Tab pane  j/k move  Enter open  R reply  n new topic  x resolve/reopen  p pin  r refresh  q/Esc close";
const HINTS_SHORT: &str = "Tab pane  j/k move  R reply  n new  r refresh  q close";

/// Draw the full-screen Message Board browser onto `frame`.
pub fn draw_fullscreen(
    frame: &mut ratatui::Frame<'_>,
    model: &BoardsModel,
    notice: Option<&str>,
    armed: bool,
    palette: &Palette,
) {
    let area = frame.area();
    let width = usize::from(area.width.max(1));

    let [header_area, body_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);

    // 1. Header bar
    let board_name = model
        .selected_board()
        .map(|b| b.handle.as_str())
        .unwrap_or("—");
    let topic_part = model
        .selected_topic()
        .map(|t| format!(" › {}", t.id))
        .unwrap_or_default();
    let left = format!(" BOARDS  {board_name}{topic_part}");
    let right = format!("{} boards · {} topics ", model.boards.len(), model.topics.len());
    let right_w = display_width(&right);
    let left_clipped = truncate_chars(&left, width.saturating_sub(right_w + 1));
    let gap = width.saturating_sub(display_width(&left_clipped) + right_w);
    let header = Line::from(vec![
        Span::styled(left_clipped, palette.status_active()),
        Span::styled(" ".repeat(gap), palette.status_bar()),
        Span::styled(right, palette.status_bar()),
    ]);
    frame.render_widget(Paragraph::new(header), header_area);

    // 2. Body: left sidebar (boards + topics) | divider | right reader (posts + compose)
    let left_w = (area.width / 3).clamp(24, 44).min(area.width.saturating_sub(2));
    let [left_area, sep_area, right_area] = Layout::horizontal([
        Constraint::Length(left_w),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(body_area);

    let boards_h = (left_area.height / 3).clamp(4, 10).min(left_area.height);
    let [boards_area, topics_area] = Layout::vertical([
        Constraint::Length(boards_h),
        Constraint::Min(1),
    ])
    .areas(left_area);

    // Boards list
    let boards_lines = render_boards_pane(model, usize::from(boards_area.width), usize::from(boards_area.height), palette);
    frame.render_widget(Paragraph::new(boards_lines), boards_area);

    // Topics list
    let topics_lines = render_topics_pane(model, usize::from(topics_area.width), usize::from(topics_area.height), palette);
    frame.render_widget(Paragraph::new(topics_lines), topics_area);

    // Vertical separator
    let sep_lines: Vec<Line<'static>> = (0..sep_area.height)
        .map(|_| Line::from(Span::styled("│", palette.divider())))
        .collect();
    frame.render_widget(Paragraph::new(sep_lines), sep_area);

    // Right pane: posts reader + optional inline compose bar
    let compose_h: u16 = if model.compose.is_some() { 2 } else { 0 };
    let [posts_area, compose_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(compose_h),
    ])
    .areas(right_area);

    let posts_lines = render_posts_pane(model, usize::from(posts_area.width), usize::from(posts_area.height), palette);
    frame.render_widget(Paragraph::new(posts_lines), posts_area);

    if let Some(compose) = &model.compose {
        let (prompt, value) = match compose {
            BoardCompose::Reply { topic_id, body } => (format!("Reply to {topic_id}: "), body.as_str()),
            BoardCompose::NewTopicTitle { board, title } => (format!("New topic on {board} — title: "), title.as_str()),
            BoardCompose::NewTopicBody { title, body, .. } => (format!("Body for \"{title}\": "), body.as_str()),
        };
        let rw = usize::from(compose_area.width.max(1));
        let rule = Line::from(Span::styled("─".repeat(rw), palette.divider()));
        let input_line = Line::from(vec![
            Span::styled(prompt.clone(), palette.status()),
            Span::styled(value.to_string(), palette.compose()),
        ]);
        frame.render_widget(Paragraph::new(vec![rule, input_line]), compose_area);
        let cursor_x = compose_area.x
            + u16::try_from(display_width(&prompt) + display_width(value))
                .unwrap_or(u16::MAX)
                .min(compose_area.width.saturating_sub(1));
        frame.set_cursor_position((cursor_x, compose_area.y + 1));
    }

    // 3. Footer bar
    let footer = if armed {
        crate::status::legend_line(area.width, palette)
    } else if let Some(msg) = model.status.as_deref().or(notice) {
        let clipped = truncate_chars(msg, width);
        let pad = width.saturating_sub(display_width(&clipped));
        Line::from(vec![
            Span::styled(clipped, palette.status_warning()),
            Span::styled(" ".repeat(pad), palette.status_bar()),
        ])
    } else if model.compose.is_some() {
        let hint = "Enter submit  Esc cancel";
        let pad = width.saturating_sub(display_width(hint));
        Line::from(vec![
            Span::styled(hint, palette.status_bar()),
            Span::styled(" ".repeat(pad), palette.status_bar()),
        ])
    } else {
        let hints = if display_width(HINTS_FULL) <= width { HINTS_FULL } else { HINTS_SHORT };
        let clipped = truncate_chars(hints, width);
        let pad = width.saturating_sub(display_width(&clipped));
        Line::from(vec![
            Span::styled(clipped, palette.status_bar()),
            Span::styled(" ".repeat(pad), palette.status_bar()),
        ])
    };
    frame.render_widget(Paragraph::new(footer), footer_area);
}

fn pane_heading(title: &str, focused: bool, width: usize, palette: &Palette) -> Line<'static> {
    let style = if focused {
        palette.status().add_modifier(Modifier::BOLD)
    } else {
        palette.divider()
    };
    let text = format!(" {title}");
    Line::from(Span::styled(truncate_chars(&text, width), style))
}

fn render_boards_pane(model: &BoardsModel, width: usize, height: usize, palette: &Palette) -> Vec<Line<'static>> {
    let mut lines = vec![pane_heading("BOARDS", model.focus == Focus::Boards, width, palette)];
    let body_h = height.saturating_sub(1);
    if body_h == 0 {
        return lines;
    }
    if model.boards.is_empty() {
        lines.push(Line::from(Span::styled("  (no boards)", palette.divider())));
        return lines;
    }
    let scroll = scroll_window(model.board_idx, model.boards.len(), body_h);
    for (i, b) in model.boards.iter().enumerate().skip(scroll).take(body_h) {
        let selected = i == model.board_idx;
        let marker = if selected { "› " } else { "  " };
        let policy = if b.policy.is_empty() {
            String::new()
        } else {
            format!(" [{}]", b.policy)
        };
        let raw = format!("{marker}{}{policy}", b.handle);
        let clipped = truncate_chars(&raw, width);
        let fill = width.saturating_sub(display_width(&clipped));
        let style = if selected && model.focus == Focus::Boards {
            palette.copy_selection()
        } else if selected {
            palette.status().add_modifier(Modifier::BOLD)
        } else {
            palette.status()
        };
        lines.push(Line::from(Span::styled(format!("{clipped}{}", " ".repeat(fill)), style)));
    }
    lines
}

fn render_topics_pane(model: &BoardsModel, width: usize, height: usize, palette: &Palette) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("─".repeat(width), palette.divider())),
        pane_heading("TOPICS", model.focus == Focus::Topics, width, palette),
    ];
    let body_h = height.saturating_sub(2);
    if body_h == 0 {
        return lines;
    }
    if model.topics.is_empty() {
        lines.push(Line::from(Span::styled("  (no topics)", palette.divider())));
        return lines;
    }
    let scroll = scroll_window(model.topic_idx, model.topics.len(), body_h);
    for (i, t) in model.topics.iter().enumerate().skip(scroll).take(body_h) {
        let selected = i == model.topic_idx;
        let marker = if selected { "› " } else { "  " };
        let badge = match t.status.as_str() {
            "pinned" => "★ ",
            "resolved" => "✓ ",
            "archived" => "· ",
            _ => "",
        };
        let raw = format!("{marker}{badge}{} ({}) {}", t.summary, t.posts, t.age);
        let clipped = truncate_chars(&raw, width);
        let fill = width.saturating_sub(display_width(&clipped));
        let style = if selected && model.focus == Focus::Topics {
            palette.copy_selection()
        } else if selected {
            palette.status().add_modifier(Modifier::BOLD)
        } else if t.status == "resolved" || t.status == "archived" {
            palette.divider()
        } else {
            palette.compose()
        };
        lines.push(Line::from(Span::styled(format!("{clipped}{}", " ".repeat(fill)), style)));
    }
    lines
}

fn render_posts_pane(model: &BoardsModel, width: usize, height: usize, palette: &Palette) -> Vec<Line<'static>> {
    let title = model
        .selected_topic()
        .map(|t| format!("POSTS — {}", t.summary))
        .unwrap_or_else(|| "POSTS".to_string());
    let mut lines = vec![pane_heading(&title, model.focus == Focus::Posts, width, palette)];
    let body_h = height.saturating_sub(1);
    if body_h == 0 {
        return lines;
    }
    if model.posts_text.trim().is_empty() {
        lines.push(Line::from(Span::styled("  (select a topic to read)", palette.divider())));
        return lines;
    }
    for line in model.posts_text.lines().skip(model.posts_scroll).take(body_h) {
        let style = if line.starts_with('#') || line.starts_with("──") {
            palette.status().add_modifier(Modifier::BOLD)
        } else if line.starts_with('>') {
            palette.divider()
        } else {
            palette.compose()
        };
        lines.push(Line::from(Span::styled(truncate_chars(line, width), style)));
    }
    lines
}

fn scroll_window(selected: usize, total: usize, view_h: usize) -> usize {
    if total <= view_h || view_h == 0 {
        0
    } else {
        selected.saturating_sub(view_h / 2).min(total.saturating_sub(view_h))
    }
}

fn display_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

fn truncate_chars(s: &str, width: usize) -> String {
    if display_width(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn parses_board_list_and_appends_recent() {
        let out = ".  [open]  (context board)\ncoder/notes  [curated]  design notes\n";
        let boards = parse_board_list(out);
        assert_eq!(boards.len(), 3);
        assert_eq!(boards[0].handle, ".");
        assert_eq!(boards[0].policy, "open");
        assert_eq!(boards[1].handle, "coder/notes");
        assert_eq!(boards[1].policy, "curated");
        assert_eq!(boards[1].description, "design notes");
        assert_eq!(boards[2].handle, RECENT_BOARD);
    }

    #[test]
    fn parses_topic_list_from_kj_board_format() {
        let out = "a1b2c3d4  [open    ]    3p    4m  tui-cleanup  \"Make TUI clean\"\n\
                   e5f60718  [pinned  ]    1p    2h  rules  \"House rules\" (meta)\n";
        let topics = parse_topic_list(out);
        assert_eq!(topics.len(), 2);
        assert_eq!(topics[0].id, "a1b2c3d4");
        assert_eq!(topics[0].status, "open");
        assert_eq!(topics[0].posts, "3p");
        assert_eq!(topics[0].age, "4m");
        assert!(topics[0].summary.contains("Make TUI clean"));
        assert_eq!(topics[1].status, "pinned");
    }

    #[test]
    fn reply_and_new_topic_compose_flows_emit_expected_outcomes() {
        let mut model = BoardsModel::new();
        model.set_boards(parse_board_list(".  [open]  (context board)\n"));
        model.set_topics(parse_topic_list("a1b2c3d4  [open    ]    1p    1m  slug  \"Hello\"\n"));

        // Reply flow
        assert_eq!(model.handle_key(press(KeyCode::Char('R'))), BoardsOutcome::None);
        for c in "sounds good".chars() {
            model.handle_key(press(KeyCode::Char(c)));
        }
        assert_eq!(
            model.handle_key(press(KeyCode::Enter)),
            BoardsOutcome::Reply {
                topic_id: "a1b2c3d4".to_string(),
                body: "sounds good".to_string(),
            }
        );

        // New topic flow (title -> body -> submit)
        assert_eq!(model.handle_key(press(KeyCode::Char('n'))), BoardsOutcome::None);
        for c in "My Title".chars() {
            model.handle_key(press(KeyCode::Char(c)));
        }
        assert_eq!(model.handle_key(press(KeyCode::Enter)), BoardsOutcome::None);
        for c in "First post".chars() {
            model.handle_key(press(KeyCode::Char(c)));
        }
        assert_eq!(
            model.handle_key(press(KeyCode::Enter)),
            BoardsOutcome::NewTopic {
                board: ".".to_string(),
                title: "My Title".to_string(),
                body: "First post".to_string(),
            }
        );
    }

    #[test]
    fn draw_fullscreen_renders_all_three_panes_and_footer() {
        let mut model = BoardsModel::new();
        model.set_boards(parse_board_list(".  [open]  (context board)\n"));
        model.set_topics(parse_topic_list("a1b2c3d4  [open    ]    2p    3m  tui  \"TUI cleanup\"\n"));
        model.set_posts("#1 amy (3m)\nLet's make it clean.".to_string());

        let mut terminal = Terminal::new(TestBackend::new(100, 20)).expect("backend");
        terminal
            .draw(|frame| draw_fullscreen(frame, &model, None, false, &Palette::tokyo_midnight()))
            .expect("draw");
        let buf = terminal.backend().buffer();
        let screen: Vec<String> = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect();
        assert!(screen[0].contains("BOARDS"), "header: {:?}", screen[0]);
        assert!(screen.iter().any(|l| l.contains("TUI cleanup")), "topic rendered");
        assert!(screen.iter().any(|l| l.contains("Let's make it clean.")), "post rendered");
        assert!(screen.last().unwrap().contains("R reply"), "footer hints");
    }
}
