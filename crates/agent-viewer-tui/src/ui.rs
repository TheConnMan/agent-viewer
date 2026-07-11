//! Rendering surface: a flat single-list main view (state- or project-grouped) with a
//! one-line footer, bottom peek overlay, centered new/rename/help modals, and the
//! full-screen embedded-PTY attach view. The approved amber palette lives in `theme`.

use crate::app::{App, Composer, Row, Section};
use agent_viewer_core::codex::rollout::{TranscriptItem, read_transcript};
use agent_viewer_core::pty::PtySession;
use agent_viewer_core::{BackendKind, Session, Status};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tui_term::widget::PseudoTerminal;

/// A live spawn-bloom one-shot, keyed by session, holding the ms it started (now_ms).
pub type Pulses = HashMap<(BackendKind, String), i64>;

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

/// Working shimmer: the glyph cycles this frame table at ~120ms/frame.
const SHIMMER: [&str; 8] = ["✻", "✽", "✶", "✢", "·", "✢", "✶", "✽"];
/// Spawn one-shot bloom: a ~400ms grow when a composer-spawned row first appears.
const BLOOM: [&str; 4] = ["·", "✢", "✶", "✽"];
/// Needs-input breathing brightness steps (muted -> accent-bright), a discrete pulse.
const BREATH_STEPS: [ratatui::style::Color; 4] = [
    theme::MUTED,
    ratatui::style::Color::Rgb(0xb5, 0x95, 0x50),
    theme::ACCENT,
    ratatui::style::Color::Rgb(0xf4, 0xc0, 0x72),
];

/// PURE working-shimmer frame: cycles `SHIMMER` at ~120ms/frame off a monotonic ms clock.
pub fn shimmer_glyph(elapsed_ms: i64) -> &'static str {
    let i = (elapsed_ms.max(0) / 120) as usize % SHIMMER.len();
    SHIMMER[i]
}

/// PURE needs-input breath phase: a 0..3 triangle wave over a ~1.2s period.
pub fn breath_phase(elapsed_ms: i64) -> usize {
    let period = 1200_i64;
    let t = elapsed_ms.rem_euclid(period);
    let half = period / 2;
    let up = t < half;
    let pos = if up { t } else { period - t };
    (pos * BREATH_STEPS.len() as i64 / (half + 1)) as usize % BREATH_STEPS.len()
}

/// PURE spawn bloom: the one-shot glyph for `elapsed_ms` since the row appeared, or None
/// once the ~400ms bloom has finished.
pub fn bloom_glyph(elapsed_ms: i64) -> Option<&'static str> {
    if !(0..400).contains(&elapsed_ms) {
        return None;
    }
    let i = (elapsed_ms / 100) as usize;
    Some(BLOOM[i.min(BLOOM.len() - 1)])
}

/// Six-state glyph + color. Working shimmers its glyph (~120ms/frame); needs-input
/// breathes its color (muted <-> accent-bright, ~1.2s). Both derive from `now_ms`.
fn status_glyph(status: Status, now_ms: i64) -> (&'static str, ratatui::style::Color) {
    match status {
        Status::Working => (shimmer_glyph(now_ms), theme::ACCENT),
        Status::NeedsInput => ("◐", BREATH_STEPS[breath_phase(now_ms)]),
        Status::Idle => ("∙", theme::MUTED),
        Status::Done => ("●", theme::OK),
        Status::Failed => ("✗", theme::ERR),
        Status::Stopped => ("○", theme::STOPPED),
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

/// The `Ctrl+R` rename modal (prefilled with the current title).
#[derive(Debug, Clone)]
pub struct RenameModal {
    pub backend: BackendKind,
    pub id: String,
    pub buffer: String,
}

/// Top-level input mode driving key routing and what the footer/overlay shows. The
/// inline spawn composer is not a mode — it lives on the Normal list view at all times.
pub enum Mode {
    Normal,
    Filter,
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

/// Everything the frame needs, bundled so the entry point stays one argument wide.
pub struct Draw<'a> {
    pub app: &'a App,
    pub mode: &'a Mode,
    pub notice: &'a str,
    pub peek: &'a PeekCache,
    pub composer: &'a Composer,
    pub pulses: &'a Pulses,
    pub now_ms: i64,
    pub attach: Option<AttachView<'a>>,
}

pub fn draw(frame: &mut Frame, d: Draw) {
    // Paint the whole surface with the base background first.
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::BG).fg(theme::TEXT)),
        frame.area(),
    );

    if let Some(av) = d.attach {
        draw_attach(frame, av, d.now_ms);
        return;
    }

    // list · persistent composer input line · footer.
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    draw_list(frame, d.app, d.pulses, d.now_ms, vertical[0]);
    draw_composer(frame, d.app, d.composer, vertical[1]);
    draw_footer(frame, d.app, d.mode, d.notice, d.now_ms, vertical[2]);

    match d.mode {
        Mode::Rename(modal) => draw_rename_modal(frame, modal, frame.area()),
        Mode::Help => draw_help(frame, frame.area()),
        Mode::Peek => draw_peek(frame, d.app, d.peek, frame.area()),
        _ => {}
    }
}

/// Abbreviate a spawn-target dir with a leading `~` for $HOME (display only).
fn abbreviate_dir(dir: &Path) -> String {
    let s = dir.display().to_string();
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
        && let Some(rest) = s.strip_prefix(&home)
    {
        return format!("~{rest}");
    }
    s
}

/// Persistent inline spawn composer: `[cc] ~/git/foo ❯ <text>`, or a muted placeholder
/// when empty. The tag is accent, the target dir muted.
fn draw_composer(frame: &mut Frame, app: &App, composer: &Composer, area: Rect) {
    // Same tag as the row prefix ([cc]/[cx]/[oc]) now that Claude's row tag is [cc].
    let tag = composer.backend().tag();
    let dir = app
        .spawn_target()
        .map(|d| abbreviate_dir(&d))
        .unwrap_or_default();
    let mut spans = vec![
        Span::styled(format!(" {tag} "), Style::default().fg(theme::ACCENT)),
        Span::styled(format!("{dir} "), Style::default().fg(theme::MUTED)),
        Span::styled("❯ ", Style::default().fg(theme::ACCENT)),
    ];
    if composer.is_empty() {
        spans.push(Span::styled(
            "describe a task · tab to switch agent",
            Style::default().fg(theme::FAINT),
        ));
    } else {
        spans.push(Span::styled(
            composer.text().to_string(),
            Style::default().fg(theme::TEXT),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

// --- Main list ------------------------------------------------------------------

fn draw_list(frame: &mut Frame, app: &App, pulses: &Pulses, now_ms: i64, area: Rect) {
    let width = area.width as usize;
    let rows = app.visible();
    let items: Vec<ListItem> = rows
        .iter()
        .map(|r| row_to_item(r, pulses, now_ms, width))
        .collect();
    let list = List::new(items).highlight_style(Style::default().bg(theme::SEL_BG).fg(theme::SEL_FG));
    let mut state = ListState::default();
    if !rows.is_empty() {
        state.select(Some(app.selected_index().min(rows.len() - 1)));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn row_to_item(row: &Row, pulses: &Pulses, now_ms: i64, width: usize) -> ListItem<'static> {
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
        Row::Session {
            backend,
            id,
            summary,
            status,
            title,
            updated_at_ms,
            ..
        } => {
            // A live spawn bloom overrides the glyph and flashes the row background.
            let bloom = pulses
                .get(&(*backend, id.clone()))
                .and_then(|start| bloom_glyph(now_ms - *start));
            let (glyph, gcolor) = match bloom {
                Some(g) => (g, theme::ACCENT),
                None => status_glyph(*status, now_ms),
            };
            let elapsed = crate::app::format_elapsed(now_ms - *updated_at_ms);
            let line = session_line(
                glyph,
                gcolor,
                backend.tag(),
                title,
                summary,
                &elapsed,
                width,
            );
            if bloom.is_some() {
                ListItem::new(line).style(Style::default().bg(theme::SEL_BG))
            } else {
                ListItem::new(line)
            }
        }
    }
}

/// `state glyph · [tag] · name · dim summary · right-aligned elapsed`, padded so the
/// elapsed sits flush right. Widths approximated by char count (glyphs are single-cell).
fn session_line(
    glyph: &str,
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
                        "{hidden_txt}{showing}type task · Tab agent · Enter spawn/attach · space peek · Ctrl+R rename · Ctrl+X stop/remove · Ctrl+S group · a all · / filter · ? help · q quit"
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
        "← back · ctrl+] detach"
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
        ("→ / Enter", "attach selected (empty composer)"),
        ("type · Tab", "compose task · switch agent"),
        ("Enter", "spawn composed task"),
        ("← back", "detach (composer empty)"),
        ("Ctrl+]", "detach (always)"),
        ("Space", "peek transcript tail"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shimmer_cycles_frames_on_120ms() {
        // Frame index steps every 120ms and wraps at 8 frames.
        assert_eq!(shimmer_glyph(0), SHIMMER[0]);
        assert_eq!(shimmer_glyph(120), SHIMMER[1]);
        assert_eq!(shimmer_glyph(119), SHIMMER[0]);
        assert_eq!(shimmer_glyph(120 * 8), SHIMMER[0]); // wraps
        assert_eq!(shimmer_glyph(-500), SHIMMER[0]); // negative clamps to frame 0
    }

    #[test]
    fn breath_phase_is_a_bounded_triangle() {
        // Always in range, rises then falls across the 1.2s period.
        for ms in [0, 150, 300, 450, 600, 750, 900, 1050, 1200, 5321] {
            assert!(breath_phase(ms) < BREATH_STEPS.len());
        }
        // Trough at the start, near the peak mid-period.
        assert_eq!(breath_phase(0), 0);
        assert!(breath_phase(600) >= breath_phase(0));
    }

    #[test]
    fn bloom_runs_for_400ms_then_stops() {
        assert_eq!(bloom_glyph(0), Some(BLOOM[0]));
        assert_eq!(bloom_glyph(100), Some(BLOOM[1]));
        assert_eq!(bloom_glyph(399), Some(BLOOM[3]));
        assert_eq!(bloom_glyph(400), None); // finished
        assert_eq!(bloom_glyph(-1), None); // not started
    }
}
