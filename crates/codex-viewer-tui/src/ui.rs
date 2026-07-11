// Naive rendering surface over ratatui: left session list, right detail pane, footer.
// No themes/config (see plan section 5.6).

use crate::app::{App, Row};
use codex_viewer_core::codex::rollout::{TranscriptItem, read_transcript};
use codex_viewer_core::{BackendKind, Status};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use std::path::{Path, PathBuf};

/// Cap on transcript items retained (only the tail is ever shown).
const MAX_TRANSCRIPT_ITEMS: usize = 200;

/// Cached transcript tail for the focused codex session. Re-reads only when the
/// backing file's (mtime, len) key changes, so the per-tick/frame cost is one stat().
pub struct TranscriptCache {
    path: Option<PathBuf>,
    key: Option<(u64, u64)>,
    items: Vec<TranscriptItem>,
    error: Option<String>,
}

impl Default for TranscriptCache {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptCache {
    pub fn new() -> Self {
        TranscriptCache {
            path: None,
            key: None,
            items: Vec::new(),
            error: None,
        }
    }

    /// Point the cache at the focused session's rollout file (None for backends
    /// without a transcript). Re-reads the file only when path or (mtime,len) moved;
    /// a read error is stored as a one-line notice, never propagated.
    pub fn refresh(&mut self, path: Option<&Path>) {
        let Some(path) = path else {
            self.path = None;
            self.key = None;
            self.items.clear();
            self.error = None;
            return;
        };
        let key = file_key(path);
        if self.path.as_deref() == Some(path) && self.key == key {
            return;
        }
        self.path = Some(path.to_path_buf());
        self.key = key;
        match read_transcript(path) {
            Ok(mut items) => {
                if items.len() > MAX_TRANSCRIPT_ITEMS {
                    items.drain(0..items.len() - MAX_TRANSCRIPT_ITEMS);
                }
                self.items = items;
                self.error = None;
            }
            Err(e) => {
                self.items.clear();
                self.error = Some(format!("transcript unavailable: {e}"));
            }
        }
    }
}

fn file_key(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    Some((mtime, meta.len()))
}

/// Which field of the `n` modal has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalField {
    Backend,
    Dir,
    Task,
}

/// The `n` (new session) modal state.
#[derive(Debug, Clone)]
pub struct NewModal {
    pub backend: BackendKind,
    pub dir: String,
    pub task: String,
    pub field: ModalField,
}

impl NewModal {
    pub fn cycle_backend(&mut self, forward: bool) {
        self.backend = match (self.backend, forward) {
            (BackendKind::Codex, true) => BackendKind::Claude,
            (BackendKind::Claude, true) => BackendKind::Opencode,
            (BackendKind::Opencode, true) => BackendKind::Codex,
            (BackendKind::Codex, false) => BackendKind::Opencode,
            (BackendKind::Claude, false) => BackendKind::Codex,
            (BackendKind::Opencode, false) => BackendKind::Claude,
        };
    }

    pub fn next_field(&mut self) {
        self.field = match self.field {
            ModalField::Backend => ModalField::Dir,
            ModalField::Dir => ModalField::Task,
            ModalField::Task => ModalField::Backend,
        };
    }
}

/// Top-level input mode driving what the footer/overlay shows.
pub enum Mode {
    Normal,
    Filter,
    New(NewModal),
}

pub fn draw(frame: &mut Frame, app: &App, mode: &Mode, notice: &str, transcript: &TranscriptCache) {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(vertical[0]);

    draw_left(frame, app, panes[0]);
    draw_right(frame, app, panes[1], transcript);
    draw_footer(frame, app, mode, notice, vertical[1]);

    if let Mode::New(modal) = mode {
        draw_modal(frame, modal, frame.area());
    }
}

fn draw_left(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.visible();
    let items: Vec<ListItem> = rows.iter().map(row_to_item).collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("sessions"))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    if !rows.is_empty() {
        state.select(Some(app.selected_index().min(rows.len() - 1)));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn row_to_item(row: &Row) -> ListItem<'static> {
    match row {
        Row::GroupHeader {
            root,
            collapsed,
            count,
        } => {
            let marker = if *collapsed { "▶" } else { "▼" };
            let text = format!("{marker} {} ({count})", root.display());
            ListItem::new(Line::from(Span::styled(
                text,
                Style::default().add_modifier(Modifier::BOLD),
            )))
        }
        Row::Session {
            backend,
            title,
            status,
            hidden,
            ..
        } => {
            let (glyph, color) = status_glyph(*status, *hidden);
            let line = Line::from(vec![
                Span::raw("  "),
                Span::styled(glyph.to_string(), Style::default().fg(color)),
                Span::raw(" "),
                Span::styled(
                    backend.tag(),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(" "),
                Span::raw(title.clone()),
            ]);
            ListItem::new(line)
        }
    }
}

/// spinner=running, green=done, gray=hidden, red=errored (SPEC glyph vocabulary).
fn status_glyph(status: Status, hidden: bool) -> (char, Color) {
    if hidden {
        return ('○', Color::DarkGray);
    }
    match status {
        Status::Running => ('◐', Color::Yellow),
        Status::Done => ('●', Color::Green),
        Status::Errored => ('●', Color::Red),
    }
}

fn draw_right(frame: &mut Frame, app: &App, area: Rect, transcript: &TranscriptCache) {
    let block = Block::default().borders(Borders::ALL).title("detail");
    let Some(session) = app.selected() else {
        frame.render_widget(
            Paragraph::new("no session selected").block(block),
            area,
        );
        return;
    };

    // Codex sessions carry a rollout file: show its transcript tail, pinned to the
    // bottom. A read error falls back to the metadata summary + a one-line notice.
    // Backends without a transcript file show the metadata summary.
    match (&session.rollout_path, &transcript.error) {
        (Some(_), None) => draw_transcript(frame, area, block, session, &transcript.items),
        (Some(_), Some(err)) => {
            let mut lines = metadata_lines(session);
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                err.clone(),
                Style::default().fg(Color::Red),
            )));
            let paragraph = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
            frame.render_widget(paragraph, area);
        }
        (None, _) => {
            let paragraph = Paragraph::new(metadata_lines(session))
                .block(block)
                .wrap(Wrap { trim: false });
            frame.render_widget(paragraph, area);
        }
    }
}

fn metadata_lines(session: &codex_viewer_core::Session) -> Vec<Line<'static>> {
    let status = match session.status {
        Status::Running => "running",
        Status::Done => "done",
        Status::Errored => "errored",
    };
    vec![
        Line::from(format!("{} {}", session.backend.tag(), session.title)),
        Line::from(""),
        Line::from(format!("backend : {}", session.backend.name())),
        Line::from(format!("id      : {}", session.id)),
        Line::from(format!("status  : {status}")),
        Line::from(format!("hidden  : {}", session.hidden)),
        Line::from(format!("source  : {}", session.source_label)),
        Line::from(format!("cwd     : {}", session.cwd.display())),
    ]
}

fn draw_transcript(
    frame: &mut Frame,
    area: Rect,
    block: Block<'static>,
    session: &codex_viewer_core::Session,
    items: &[TranscriptItem],
) {
    let inner_height = area.height.saturating_sub(2) as usize;
    let width = area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        format!("{} {}", session.backend.tag(), session.title),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    for item in items {
        lines.push(Line::from(Span::styled(
            format!("{}:", item.role),
            Style::default().fg(Color::Cyan),
        )));
        for segment in item.text.split('\n') {
            lines.push(Line::from(truncate(segment, width)));
        }
    }
    // Pin to the bottom so the newest turn is visible (no wrap keeps counts exact).
    let scroll = lines.len().saturating_sub(inner_height) as u16;
    let paragraph = Paragraph::new(lines).block(block).scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

fn truncate(s: &str, width: usize) -> String {
    if width == 0 || s.chars().count() <= width {
        return s.to_string();
    }
    s.chars().take(width).collect()
}

fn draw_footer(frame: &mut Frame, app: &App, mode: &Mode, notice: &str, area: Rect) {
    let line = match mode {
        Mode::Filter => format!("/{}", app.filter()),
        Mode::New(_) => "new session (Esc to cancel)".to_string(),
        Mode::Normal => {
            if notice.is_empty() {
                "Enter attach  n new  h hide  u unhide  a show-hidden  space collapse  / filter  q quit".to_string()
            } else {
                notice.to_string()
            }
        }
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_modal(frame: &mut Frame, modal: &NewModal, area: Rect) {
    let popup = centered_rect(60, 40, area);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title("new session");
    let focus = |field: ModalField| {
        if modal.field == field {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        }
    };
    let lines = vec![
        Line::from(vec![
            Span::raw("backend: "),
            Span::styled(modal.backend.name(), focus(ModalField::Backend)),
            Span::raw("  (Left/Right)"),
        ]),
        Line::from(vec![
            Span::raw("dir    : "),
            Span::styled(modal.dir.clone(), focus(ModalField::Dir)),
        ]),
        Line::from(vec![
            Span::raw("task   : "),
            Span::styled(modal.task.clone(), focus(ModalField::Task)),
        ]),
        Line::from(""),
        Line::from("Tab switch field  Enter spawn  Esc cancel"),
    ];
    let paragraph = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, popup);
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}
