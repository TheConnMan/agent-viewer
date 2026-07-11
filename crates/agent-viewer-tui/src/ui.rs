//! Rendering surface: a flat single-list main view (state- or project-grouped) with a
//! one-line footer, bottom peek overlay, centered new/rename/help modals, and the
//! full-screen embedded-PTY attach view. The approved amber palette lives in `theme`.

use crate::app::{App, Row, Section};
use agent_viewer_core::codex::rollout::{TranscriptItem, read_transcript};
use agent_viewer_core::pty::PtySession;
use agent_viewer_core::{BackendKind, Session, Status};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use std::path::{Path, PathBuf};
use tui_term::widget::PseudoTerminal;

/// Cap on transcript items retained for peek (only the tail is ever shown).
const MAX_TRANSCRIPT_ITEMS: usize = 200;

/// User-approved palette (section 5.11). RGB truecolor throughout.
pub mod theme {
    use ratatui::style::Color;
    pub const BG: Color = Color::Rgb(0x16, 0x13, 0x0d);
    pub const TEXT: Color = Color::Rgb(0xe6, 0xdf, 0xcc);
    pub const MUTED: Color = Color::Rgb(0x8d, 0x85, 0x70);
    pub const FAINT: Color = Color::Rgb(0x5c, 0x56, 0x3f);
    pub const ACCENT: Color = Color::Rgb(0xdf, 0xa6, 0x49);
    pub const SEL_BG: Color = Color::Rgb(0x2e, 0x28, 0x17);
    pub const SEL_FG: Color = Color::Rgb(0xf4, 0xee, 0xda);
    pub const OK: Color = Color::Rgb(0x7f, 0xae, 0x5e);
    pub const WARN: Color = Color::Rgb(0xd9, 0xa9, 0x3f);
    pub const ERR: Color = Color::Rgb(0xcf, 0x6a, 0x52);
    pub const STOPPED: Color = Color::Rgb(0x85, 0x7e, 0x6a);
}

/// Six-state glyph + color. `✽` (Working) blinks on a ~500ms parity from now_ms.
fn status_glyph(status: Status, now_ms: i64) -> (char, ratatui::style::Color) {
    match status {
        Status::Working => {
            let on = (now_ms / 500) % 2 == 0;
            ('✽', if on { theme::ACCENT } else { theme::FAINT })
        }
        Status::NeedsInput => ('◐', theme::WARN),
        Status::Idle => ('∙', theme::MUTED),
        Status::Done => ('●', theme::OK),
        Status::Failed => ('✗', theme::ERR),
        Status::Stopped => ('○', theme::STOPPED),
    }
}

fn status_word(status: Status) -> &'static str {
    match status {
        Status::Working => "working",
        Status::NeedsInput => "needs-input",
        Status::Idle => "idle",
        Status::Done => "done",
        Status::Failed => "failed",
        Status::Stopped => "stopped",
    }
}

fn section_label(section: Section) -> &'static str {
    match section {
        Section::NeedsInput => "NEEDS INPUT",
        Section::Working => "WORKING",
        Section::Idle => "IDLE",
        Section::Done => "DONE",
    }
}

// --- Peek cache (backend-dispatching transcript tail) ---------------------------

/// Cache key: backend + transcript path + its (mtime, len) fingerprint.
type PeekKey = (BackendKind, PathBuf, Option<(u64, u64)>);

/// Cached transcript tail for the peek overlay. Re-reads only when the focused
/// session's backing file (path, mtime, len) key changes, so the per-frame cost is
/// one stat() (opencode has no file — metadata is rendered live).
pub struct PeekCache {
    key: Option<PeekKey>,
    items: Vec<TranscriptItem>,
    error: Option<String>,
}

impl Default for PeekCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PeekCache {
    pub fn new() -> Self {
        PeekCache {
            key: None,
            items: Vec::new(),
            error: None,
        }
    }

    /// Point the cache at the focused session. codex -> rollout transcript tail,
    /// claude -> session-JSONL tail, opencode (no path) -> metadata rendered live.
    pub fn refresh(&mut self, session: Option<&Session>) {
        let Some(session) = session else {
            self.clear();
            return;
        };
        let Some(path) = session.rollout_path.as_deref() else {
            // opencode: no transcript file — draw_peek falls back to metadata.
            self.clear();
            return;
        };
        let fkey = file_key(path);
        let key = Some((session.backend, path.to_path_buf(), fkey));
        if self.key == key {
            return;
        }
        self.key = key;
        let read = match session.backend {
            BackendKind::Claude => {
                agent_viewer_core::claude::read_claude_transcript(path, MAX_TRANSCRIPT_ITEMS)
            }
            _ => read_transcript(path),
        };
        match read {
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

    fn clear(&mut self) {
        self.key = None;
        self.items.clear();
        self.error = None;
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

// --- Modals / modes -------------------------------------------------------------

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

/// The `Ctrl+R` rename modal (prefilled with the current title).
#[derive(Debug, Clone)]
pub struct RenameModal {
    pub backend: BackendKind,
    pub id: String,
    pub buffer: String,
}

/// Top-level input mode driving key routing and what the footer/overlay shows.
pub enum Mode {
    Normal,
    Filter,
    New(NewModal),
    Rename(RenameModal),
    Peek,
    Help,
    Attached,
}

/// Everything the attach view needs: the session snapshot (header) + its live PTY.
pub struct AttachView<'a> {
    pub session: &'a Session,
    pub pty: &'a PtySession,
    pub exited: bool,
}

// --- Draw entry point -----------------------------------------------------------

pub fn draw(
    frame: &mut Frame,
    app: &App,
    mode: &Mode,
    notice: &str,
    peek: &PeekCache,
    now_ms: i64,
    attach: Option<AttachView>,
) {
    // Paint the whole surface with the base background first.
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::BG).fg(theme::TEXT)),
        frame.area(),
    );

    if let Some(av) = attach {
        draw_attach(frame, av, now_ms);
        return;
    }

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());

    draw_list(frame, app, now_ms, vertical[0]);
    draw_footer(frame, app, mode, notice, now_ms, vertical[1]);

    match mode {
        Mode::New(modal) => draw_new_modal(frame, modal, frame.area()),
        Mode::Rename(modal) => draw_rename_modal(frame, modal, frame.area()),
        Mode::Help => draw_help(frame, frame.area()),
        Mode::Peek => draw_peek(frame, app, peek, frame.area()),
        _ => {}
    }
}

// --- Main list ------------------------------------------------------------------

fn draw_list(frame: &mut Frame, app: &App, now_ms: i64, area: Rect) {
    let width = area.width as usize;
    let rows = app.visible();
    let items: Vec<ListItem> = rows.iter().map(|r| row_to_item(r, now_ms, width)).collect();
    let list = List::new(items).highlight_style(Style::default().bg(theme::SEL_BG).fg(theme::SEL_FG));
    let mut state = ListState::default();
    if !rows.is_empty() {
        state.select(Some(app.selected_index().min(rows.len() - 1)));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn row_to_item(row: &Row, now_ms: i64, width: usize) -> ListItem<'static> {
    match row {
        Row::SectionHeader { section, count } => ListItem::new(Line::from(Span::styled(
            format!("{}  ({count})", section_label(*section)),
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ))),
        Row::ProjectHeader { root, count } => ListItem::new(Line::from(Span::styled(
            format!("{}  ({count})", root.display()),
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ))),
        Row::MoreMarker { hidden } => ListItem::new(Line::from(Span::styled(
            format!("  … {hidden} more"),
            Style::default().fg(theme::FAINT),
        ))),
        Row::Session {
            backend,
            summary,
            status,
            title,
            updated_at_ms,
            ..
        } => {
            let (glyph, gcolor) = status_glyph(*status, now_ms);
            let elapsed = crate::app::format_elapsed(now_ms - *updated_at_ms);
            ListItem::new(session_line(
                glyph,
                gcolor,
                backend.tag(),
                title,
                summary,
                &elapsed,
                width,
            ))
        }
    }
}

/// `state glyph · [tag] · name · dim summary · right-aligned elapsed`, padded so the
/// elapsed sits flush right. Widths approximated by char count (glyphs are single-cell).
fn session_line(
    glyph: char,
    gcolor: ratatui::style::Color,
    tag: &str,
    name: &str,
    summary: &str,
    elapsed: &str,
    width: usize,
) -> Line<'static> {
    // Reserve the elapsed slot first so a long title truncates instead of clipping it.
    let (name_out, summary_out, pad) =
        crate::app::row_layout(width, tag.chars().count(), name, summary, elapsed.chars().count());
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(glyph.to_string(), Style::default().fg(gcolor)),
        Span::raw(" "),
        Span::styled(tag.to_string(), Style::default().fg(theme::MUTED)),
        Span::raw(" "),
        Span::styled(name_out, Style::default().fg(theme::TEXT)),
    ];
    if !summary_out.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(summary_out, Style::default().fg(theme::FAINT)));
    }
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(
        elapsed.to_string(),
        Style::default().fg(theme::MUTED),
    ));
    Line::from(spans)
}

// --- Footer ---------------------------------------------------------------------

fn draw_footer(frame: &mut Frame, app: &App, mode: &Mode, notice: &str, now_ms: i64, area: Rect) {
    let line = match mode {
        Mode::Filter => Line::from(format!("/{}", app.filter())),
        Mode::New(_) => Line::from("new session — Tab field · Enter spawn · Esc cancel"),
        Mode::Rename(_) => Line::from("rename — Enter apply · Esc cancel"),
        Mode::Help => Line::from("help — Esc/? to close"),
        Mode::Peek => Line::from("peek — Esc/Space to close"),
        Mode::Attached => Line::from(""),
        Mode::Normal => {
            if !notice.is_empty() {
                Line::from(Span::styled(
                    notice.to_string(),
                    Style::default().fg(theme::WARN),
                ))
            } else if app.is_armed(now_ms) {
                Line::from(Span::styled(
                    "[press Ctrl+X again to remove]",
                    Style::default().fg(theme::ERR),
                ))
            } else {
                let hidden = app.hidden_count();
                let hidden_txt = if hidden > 0 {
                    format!("{hidden} hidden · ")
                } else {
                    String::new()
                };
                let showing = if app.show_all() { "all · " } else { "" };
                Line::from(Span::styled(
                    format!(
                        "{hidden_txt}{showing}Enter attach · space peek · n new · Ctrl+R rename · Ctrl+X stop/remove · Ctrl+S group · a all · / filter · ? help · q quit"
                    ),
                    Style::default().fg(theme::MUTED),
                ))
            }
        }
    };
    frame.render_widget(Paragraph::new(line), area);
}

// --- Peek overlay ---------------------------------------------------------------

fn draw_peek(frame: &mut Frame, app: &App, peek: &PeekCache, area: Rect) {
    let Some(session) = app.selected() else {
        return;
    };
    let popup = bottom_rect(40, area);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::FAINT))
        .title(format!("peek — {} {}", session.backend.tag(), session.title));

    // opencode (no transcript file) or a read error -> metadata lines.
    if session.rollout_path.is_none() || peek.error.is_some() {
        let mut lines = metadata_lines(session);
        if let Some(err) = &peek.error {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                err.clone(),
                Style::default().fg(theme::ERR),
            )));
        }
        frame.render_widget(
            Paragraph::new(lines).block(block).wrap(Wrap { trim: false }),
            popup,
        );
        return;
    }

    let inner_height = popup.height.saturating_sub(2) as usize;
    let inner_width = popup.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();
    for item in &peek.items {
        lines.push(Line::from(Span::styled(
            format!("{}:", item.role),
            Style::default().fg(theme::ACCENT),
        )));
        for segment in item.text.split('\n') {
            lines.push(Line::from(Span::styled(
                truncate(segment, inner_width),
                Style::default().fg(theme::TEXT),
            )));
        }
    }
    // Pin to the bottom so the newest turn is visible.
    let scroll = lines.len().saturating_sub(inner_height) as u16;
    frame.render_widget(
        Paragraph::new(lines).block(block).scroll((scroll, 0)),
        popup,
    );
}

fn metadata_lines(session: &Session) -> Vec<Line<'static>> {
    vec![
        Line::from(format!("{} {}", session.backend.tag(), session.title)),
        Line::from(""),
        Line::from(format!("backend : {}", session.backend.name())),
        Line::from(format!("id      : {}", session.id)),
        Line::from(format!("status  : {}", status_word(session.status))),
        Line::from(format!("source  : {}", session.source_label)),
        Line::from(format!("cwd     : {}", session.cwd.display())),
    ]
}

fn truncate(s: &str, width: usize) -> String {
    if width == 0 || s.chars().count() <= width {
        return s.to_string();
    }
    s.chars().take(width).collect()
}

// --- Attach view ----------------------------------------------------------------

fn draw_attach(frame: &mut Frame, av: AttachView, now_ms: i64) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);

    draw_attach_header(frame, av.session, av.exited, now_ms, chunks[0]);

    // Render the vt100 screen under the parser lock (the render path).
    av.pty.with_screen(|screen| {
        frame.render_widget(PseudoTerminal::new(screen), chunks[1]);
    });
}

fn draw_attach_header(frame: &mut Frame, session: &Session, exited: bool, now_ms: i64, area: Rect) {
    let (glyph, gcolor) = status_glyph(session.status, now_ms);
    let right = if exited {
        "process exited · press any key"
    } else {
        "Ctrl+q detach"
    };
    let left = format!(
        " {glyph} {} {}  {}",
        session.backend.tag(),
        session.title,
        session.cwd.display()
    );
    let width = area.width as usize;
    let right_w = right.chars().count();
    let left_trunc = truncate(&left, width.saturating_sub(right_w + 1));
    let pad = width.saturating_sub(left_trunc.chars().count() + right_w);
    let line = Line::from(vec![
        Span::styled(
            format!(" {glyph}"),
            Style::default().fg(gcolor),
        ),
        Span::styled(
            left_trunc.chars().skip(2).collect::<String>(),
            Style::default().fg(theme::TEXT),
        ),
        Span::raw(" ".repeat(pad)),
        Span::styled(
            right.to_string(),
            Style::default().fg(if exited { theme::ERR } else { theme::MUTED }),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(theme::SEL_BG)),
        area,
    );
}

// --- Modals ---------------------------------------------------------------------

fn draw_new_modal(frame: &mut Frame, modal: &NewModal, area: Rect) {
    let popup = centered_rect(60, 40, area);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::ACCENT))
        .title("new session");
    let focus = |field: ModalField| {
        if modal.field == field {
            Style::default().bg(theme::SEL_BG).fg(theme::SEL_FG)
        } else {
            Style::default().fg(theme::TEXT)
        }
    };
    let lines = vec![
        Line::from(vec![
            Span::styled("backend: ", Style::default().fg(theme::MUTED)),
            Span::styled(modal.backend.name(), focus(ModalField::Backend)),
            Span::styled("  (Left/Right)", Style::default().fg(theme::FAINT)),
        ]),
        Line::from(vec![
            Span::styled("dir    : ", Style::default().fg(theme::MUTED)),
            Span::styled(modal.dir.clone(), focus(ModalField::Dir)),
        ]),
        Line::from(vec![
            Span::styled("task   : ", Style::default().fg(theme::MUTED)),
            Span::styled(modal.task.clone(), focus(ModalField::Task)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Tab switch field · Enter spawn · Esc cancel",
            Style::default().fg(theme::FAINT),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_rename_modal(frame: &mut Frame, modal: &RenameModal, area: Rect) {
    let popup = centered_rect(60, 20, area);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::ACCENT))
        .title(format!("rename {}", modal.backend.tag()));
    let lines = vec![
        Line::from(vec![
            Span::styled("name: ", Style::default().fg(theme::MUTED)),
            Span::styled(
                format!("{}_", modal.buffer),
                Style::default().bg(theme::SEL_BG).fg(theme::SEL_FG),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Enter apply · Esc cancel",
            Style::default().fg(theme::FAINT),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let popup = centered_rect(56, 70, area);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::ACCENT))
        .title("keys");
    let entries = [
        ("↑/↓  j/k", "move selection"),
        ("→ / Enter", "attach (embedded terminal)"),
        ("Ctrl+q", "detach (session keeps running)"),
        ("Space", "peek transcript tail"),
        ("n", "new session"),
        ("Ctrl+R", "rename session"),
        ("Ctrl+X", "stop, then press again to remove"),
        ("Ctrl+S", "group by state / by project"),
        ("a", "show all (companions + archived)"),
        ("h / u", "archive / unarchive"),
        ("/", "filter"),
        ("?", "this help"),
        ("q", "quit"),
    ];
    let mut lines = Vec::new();
    for (k, v) in entries {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<12}"), Style::default().fg(theme::ACCENT)),
            Span::styled(v.to_string(), Style::default().fg(theme::TEXT)),
        ]));
    }
    frame.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: false }),
        popup,
    );
}

// --- Layout helpers -------------------------------------------------------------

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

/// Bottom `percent`-height strip of `area` (peek overlay).
fn bottom_rect(percent: u16, area: Rect) -> Rect {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(100 - percent),
            Constraint::Percentage(percent),
        ])
        .split(area)[1]
}
