//! Rendering surface: a flat single-list main view (state- or project-grouped) with a
//! one-line footer, bottom peek overlay, centered new/rename/help modals, and the
//! full-screen embedded-PTY attach view. The approved amber palette lives in `theme`.

use crate::app::{App, Composer, Row, Section};
use crate::logos::LogoMarks;
use crate::peek::{self, PeekKind};
use agent_viewer_core::codex::rollout::{TranscriptItem, read_transcript};
use agent_viewer_core::pty::PtySession;
use agent_viewer_core::{BackendKind, PrBadgeColor, PrRef, Session, Status};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};
use ratatui_image::Image;
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
    pub const PR_MERGED: Color = Color::Rgb(0xb0, 0x8a, 0xc9);
    // Per-backend brand marks (row + composer): Claude terracotta, Codex teal, opencode green.
    pub const BRAND_CLAUDE: Color = Color::Rgb(0xd9, 0x77, 0x57);
    pub const BRAND_CODEX: Color = Color::Rgb(0x74, 0xaa, 0x9c);
    pub const BRAND_OPENCODE: Color = Color::Rgb(0x9e, 0xcb, 0x6a);
}

/// Startup-read (once, never per-frame): when true, list rows + composer use the brand
/// glyphs (✳/◆/■) instead of the DEFAULT textual `[cc]`/`[cx]`/`[oc]` tags — an opt-in for
/// terminals whose font renders them. Set from `AGENT_VIEWER_GLYPH_MARKS=1`.
static GLYPH_MARKS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Enable the brand-glyph marks once at startup (idempotent; later calls are ignored).
pub fn set_glyph_marks(on: bool) {
    let _ = GLYPH_MARKS.set(on);
}

fn glyph_marks() -> bool {
    *GLYPH_MARKS.get().unwrap_or(&false)
}

/// Startup-read (once): when true, the mark slot is left blank (two reserved columns) and a
/// brand-logo image is overlaid there by the render path. Set after the terminal-graphics
/// probe (`LogoMarks::build`) succeeds — so it being on implies a live `LogoMarks`. Takes
/// precedence over the glyph marks.
static LOGO_MARKS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Enable the brand-logo marks once at startup (idempotent; later calls are ignored).
pub fn set_logo_marks(on: bool) {
    let _ = LOGO_MARKS.set(on);
}

pub fn logo_marks() -> bool {
    *LOGO_MARKS.get().unwrap_or(&false)
}

/// The mark for a backend on list rows + the composer (single source of truth for every
/// mark call site): by DEFAULT the textual `[cc]`/`[cx]`/`[oc]` tag, or the brand glyph when
/// `AGENT_VIEWER_GLYPH_MARKS=1`. `BackendKind::tag()` is also used directly for help/notices.
fn backend_mark(backend: BackendKind) -> &'static str {
    // Logo mode blanks the slot (two reserved columns) for the image overlay; it wins over
    // glyph mode. Two spaces keep `mark_width` == 2 so every row/composer layout math holds.
    if logo_marks() {
        return "  ";
    }
    mark_for(backend, glyph_marks())
}

/// Pure mark selector (testable without the startup OnceLock): glyph vs textual tag.
fn mark_for(backend: BackendKind, glyph: bool) -> &'static str {
    if !glyph {
        return backend.tag();
    }
    // Brand glyphs — all core Geometric Shapes (same coverage class as the status dots).
    match backend {
        BackendKind::Claude => "✳",
        BackendKind::Codex => "◆",
        BackendKind::Opencode => "■",
    }
}

/// The brand color for a backend's mark.
fn backend_mark_color(backend: BackendKind) -> ratatui::style::Color {
    match backend {
        BackendKind::Claude => theme::BRAND_CLAUDE,
        BackendKind::Codex => theme::BRAND_CODEX,
        BackendKind::Opencode => theme::BRAND_OPENCODE,
    }
}

/// Shorthand for the ubiquitous foreground-only span style.
fn fg(color: ratatui::style::Color) -> Style {
    Style::default().fg(color)
}

/// Terminal display width of a string (measured, not assumed — some glyphs are
/// ambiguous/wide).
fn display_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    s.width()
}

/// Display width of a mark glyph, floored at 1 so a zero-width glyph still reserves a column.
fn mark_width(mark: &str) -> usize {
    display_width(mark).max(1)
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

/// A group header line: a triangle indicator plus its member count. Expanded shows
/// "▼ <label>  (<count>)"; collapsed shows "▶ <label>  (<count> hidden)".
fn header_label(label: impl std::fmt::Display, count: usize, collapsed: bool) -> String {
    if collapsed {
        format!("▶ {label}  ({count} hidden)")
    } else {
        format!("▼ {label}  ({count})")
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
    /// Codex pending-approval summary (the "what is this waiting on" ask), when the focused
    /// session has one. None for non-codex backends and when nothing is pending.
    ask: Option<String>,
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
            ask: None,
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
            if session.backend == BackendKind::Opencode {
                self.refresh_opencode(session);
            } else {
                // No transcript file and not opencode: metadata fallback only.
                self.clear();
            }
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
                // Codex surfaces the pending approval (if any) as the ask; other backends
                // derive their ask elsewhere (claude from session.summary, opencode none).
                self.ask = if session.backend == BackendKind::Codex {
                    agent_viewer_core::codex::rollout::pending_approval(path)
                        .ok()
                        .flatten()
                        .map(|a| a.summary())
                } else {
                    None
                };
            }
            Err(e) => {
                self.items.clear();
                self.error = Some(format!("transcript unavailable: {e}"));
                self.ask = None;
            }
        }
    }

    /// opencode has no transcript file — the last message lives in the SQLite `message`/
    /// `part` tables. Key off the session id + its updated_at_ms so we re-read only when the
    /// session advances (there is no file mtime to stat). The synthetic file part of the key
    /// is (updated_at_ms, 0).
    fn refresh_opencode(&mut self, session: &Session) {
        let key = Some((
            session.backend,
            PathBuf::from(&session.id),
            Some((session.updated_at_ms.max(0) as u64, 0)),
        ));
        if self.key == key {
            return;
        }
        self.key = key;
        self.ask = None;
        match agent_viewer_core::opencode::read_opencode_last_message(
            &agent_viewer_core::opencode::default_opencode_db(),
            &session.id,
        ) {
            Ok(Some(item)) => {
                self.items = vec![item];
                self.error = None;
            }
            Ok(None) => {
                self.items.clear();
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
        self.ask = None;
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

/// The `Ctrl+R` inline-rename state (prefilled with the current title). Rendered in the
/// selected row itself, not a modal.
#[derive(Debug, Clone)]
pub struct RenameModal {
    pub backend: BackendKind,
    pub id: String,
    pub buffer: String,
}

/// The reply-compose state: a small input focused over the composer area to answer a
/// blocked session. Mirrors `RenameModal`; the delivery target is keyed by (backend, id).
#[derive(Debug, Clone)]
pub struct ReplyModal {
    pub backend: BackendKind,
    pub id: String,
    pub buffer: String,
}

/// Top-level input mode driving key routing and what the footer shows. The inline spawn
/// composer, inline rename, and inline peek expansion all live on the Normal list view.
pub enum Mode {
    Normal,
    Filter,
    Rename(RenameModal),
    Reply(ReplyModal),
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
    /// The currently inline-expanded row (peek), if any — its transcript tail renders
    /// indented beneath it.
    pub expanded: Option<&'a (BackendKind, String)>,
    pub now_ms: i64,
    pub attach: Option<AttachView<'a>>,
    pub pr_status: &'a crate::pr_cache::PrStatusCache,
    /// The brand-logo protocols, present when the startup graphics probe succeeded. Overlaid
    /// on the reserved mark slot during render.
    pub logos: Option<&'a LogoMarks>,
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

    // header (blank gap + title/status + blank gaps) · list · blank gap · bordered composer box
    // (3 rows) · footer.
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    // Inline rename edits the selected row in place; inline peek expands it downward.
    let rename = match d.mode {
        Mode::Rename(m) => Some((m.backend, m.id.as_str(), m.buffer.as_str())),
        _ => None,
    };
    let expand_lines = match d.expanded {
        Some(key) => peek_expansion(d.app, d.peek, key, vertical[0].width as usize),
        None => Vec::new(),
    };
    let deco = ListDeco {
        rename,
        expanded: d.expanded,
        expand_lines: &expand_lines,
    };

    draw_header(frame, d.app, vertical[0]);
    // The composer cursor blinks only in Normal mode (the composer is the active input);
    // the rename cursor is placed on the edit row by draw_list; Help/Filter show neither.
    draw_list(
        frame,
        d.app,
        d.pulses,
        d.now_ms,
        d.pr_status,
        deco,
        d.logos,
        vertical[1],
    );
    // Reply mode replaces the spawn composer with a small reply input (the ask sits in the
    // force-expanded peek above it); every other mode shows the persistent spawn composer.
    if let Mode::Reply(m) = d.mode {
        let title = d
            .app
            .session_for(&(m.backend, m.id.clone()))
            .map(|s| s.title.clone())
            .unwrap_or_default();
        draw_reply(frame, m, vertical[3], &title);
    } else {
        draw_composer(
            frame,
            d.app,
            d.composer,
            d.logos,
            vertical[3],
            matches!(d.mode, Mode::Normal),
        );
    }
    draw_footer(frame, d.app, d.mode, d.notice, d.now_ms, vertical[4]);

    // Completion popup floating just above the composer box: the /model picker when a /model
    // command is being typed, else the slash-command popup.
    if matches!(d.mode, Mode::Normal) {
        if d.composer.is_model_command() {
            draw_model_popup(frame, d.composer, vertical[3]);
        } else {
            draw_slash_popup(frame, d.composer, vertical[3]);
        }
    }

    if matches!(d.mode, Mode::Help) {
        draw_help(frame, frame.area());
    }
}

/// Render the slash-command suggestions as a few muted lines directly above the composer
/// box (the highlighted row in accent). Nothing renders when there are no suggestions.
fn draw_slash_popup(frame: &mut Frame, composer: &Composer, composer_area: Rect) {
    let suggestions = composer.suggestions();
    if suggestions.is_empty() {
        return;
    }
    let height = (suggestions.len() as u16).min(composer_area.y);
    if height == 0 {
        return;
    }
    let area = Rect {
        x: composer_area.x,
        y: composer_area.y - height,
        width: composer_area.width,
        height,
    };
    frame.render_widget(Clear, area);
    let highlight = composer.suggestion_highlight();
    let items: Vec<ListItem> = suggestions
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let style = if i == highlight {
                fg(theme::ACCENT)
            } else {
                fg(theme::MUTED)
            };
            ListItem::new(Line::from(Span::styled(format!(" /{name}"), style)))
        })
        .collect();
    frame.render_widget(List::new(items), area);
}

/// Render the `/model` picker suggestions as muted lines above the composer box (the
/// highlighted row in accent), mirroring `draw_slash_popup` but with no leading slash.
/// Nothing renders when there are no suggestions.
fn draw_model_popup(frame: &mut Frame, composer: &Composer, composer_area: Rect) {
    let suggestions = composer.model_suggestions();
    if suggestions.is_empty() {
        return;
    }
    let height = (suggestions.len() as u16).min(composer_area.y);
    if height == 0 {
        return;
    }
    let area = Rect {
        x: composer_area.x,
        y: composer_area.y - height,
        width: composer_area.width,
        height,
    };
    frame.render_widget(Clear, area);
    let highlight = composer.suggestion_highlight();
    let items: Vec<ListItem> = suggestions
        .iter()
        .enumerate()
        .map(|(i, model)| {
            let style = if i == highlight {
                fg(theme::ACCENT)
            } else {
                fg(theme::MUTED)
            };
            ListItem::new(Line::from(Span::styled(format!(" {model}"), style)))
        })
        .collect();
    frame.render_widget(List::new(items), area);
}

/// Per-row decorations layered over the list model: an in-place rename edit field and the
/// inline peek expansion under one row.
struct ListDeco<'a> {
    rename: Option<(BackendKind, &'a str, &'a str)>,
    expanded: Option<&'a (BackendKind, String)>,
    expand_lines: &'a [Line<'static>],
}

/// The inline peek expansion for the EXPANDED row (resolved by its key, not the selection —
/// which App keeps in sync but which could momentarily diverge): the last message word-
/// wrapped to the panel width (peek::build), indented and color-coded per PeekKind. opencode
/// (no transcript file) falls back to a couple of metadata lines when it has no message.
fn peek_expansion(
    app: &App,
    peek: &PeekCache,
    key: &(BackendKind, String),
    width: usize,
) -> Vec<Line<'static>> {
    let Some(session) = app.session_for(key) else {
        return Vec::new();
    };
    // Reserve the 6-column indent (matching the rename/expansion gutter).
    let inner = width.saturating_sub(6);
    // Metadata fallback for sessions with no transcript file (opencode) when items is empty.
    let meta = if session.rollout_path.is_none() {
        vec![
            format!("status: {}", status_word(session.status)),
            format!("cwd: {}", session.cwd.display()),
        ]
    } else {
        Vec::new()
    };
    // Surface WHAT a blocked session is waiting on: codex from the parsed pending approval,
    // claude from its state.json needs (session.summary), opencode has no needs-input signal.
    let ask: Option<String> = if session.status == Status::NeedsInput {
        match session.backend {
            BackendKind::Codex => peek.ask.as_ref().map(|s| format!("Awaiting approval: {s}")),
            BackendKind::Claude => (!session.summary.is_empty())
                .then(|| format!("Awaiting input: {}", session.summary)),
            BackendKind::Opencode => None,
        }
    } else {
        None
    };
    let plines = peek::build(
        ask.as_deref(),
        &peek.items,
        peek.error.as_deref(),
        &meta,
        inner,
    );
    plines
        .into_iter()
        .map(|pl| {
            let style = match pl.kind {
                PeekKind::Ask => fg(theme::ACCENT).add_modifier(Modifier::BOLD),
                PeekKind::Role => fg(theme::MUTED),
                PeekKind::Body => fg(theme::TEXT),
                PeekKind::Meta => fg(theme::MUTED),
                PeekKind::Error => fg(theme::WARN),
            };
            Line::from(vec![Span::raw("      "), Span::styled(pl.text, style)])
        })
        .collect()
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

/// Persistent inline spawn composer inside a rounded, faint-bordered box (Claude Code's
/// input-box feel): `◆ codex ~/git/foo ❯ <text>`, or a muted placeholder when empty. The
/// brand mark + backend name are in the backend's color, the target dir muted. When
/// `show_cursor` (list view, Normal mode), the terminal's native cursor is placed at the
/// end of the typed text so it blinks there.
fn draw_composer(
    frame: &mut Frame,
    app: &App,
    composer: &Composer,
    logos: Option<&LogoMarks>,
    area: Rect,
    show_cursor: bool,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(fg(theme::FAINT));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let backend = composer.backend();
    let dir = app
        .spawn_target()
        .map(|d| abbreviate_dir(&d))
        .unwrap_or_default();
    // The model shows after the backend name, muted; "default" (codex/opencode) shows nothing.
    let model = composer.model();
    let model_seg = if model == "default" {
        String::new()
    } else {
        format!("{model} ")
    };
    let mut spans = vec![Span::styled(
        format!("{} {} ", backend_mark(backend), backend.name()),
        fg(backend_mark_color(backend)),
    )];
    if !model_seg.is_empty() {
        spans.push(Span::styled(model_seg.clone(), fg(theme::MUTED)));
    }
    spans.push(Span::styled(format!("{dir} "), fg(theme::MUTED)));
    spans.push(Span::styled("❯ ", fg(theme::ACCENT)));
    if composer.is_empty() {
        spans.push(Span::styled(
            "describe a task · tab to switch agent",
            fg(theme::FAINT),
        ));
    } else {
        spans.push(Span::styled(composer.text().to_string(), fg(theme::TEXT)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), inner);

    // Logo mode: the mark prefix rendered as two blank columns above; overlay the brand image
    // onto them. inner.x is the mark slot (no leading status glyph in the composer).
    if let Some(logos) = logos
        && logo_marks()
        && inner.width >= 2
    {
        frame.render_widget(
            Image::new(logos.image(backend)),
            Rect {
                x: inner.x,
                y: inner.y,
                width: 2,
                height: 1,
            },
        );
    }

    if show_cursor && inner.width > 0 {
        // Cursor at the end of the typed text, clamped inside the box. The prefix mirrors
        // the fixed spans above: `<mark> <backend> [<model> ]<dir> ❯ `.
        let prefix = format!(
            "{} {} {model_seg}{dir} ❯ ",
            backend_mark(backend),
            backend.name()
        );
        let col = display_width(&prefix) + display_width(composer.text());
        let x = inner.x + (col as u16).min(inner.width - 1);
        frame.set_cursor_position((x, inner.y));
    }
}

/// The reply-compose input, occupying the composer box: an accent-bordered rounded box with
/// `↳ reply <title> ❯ <buffer>` (title muted, prompt accent, text bright), a muted
/// placeholder when empty, and the native cursor at the end of the buffer (as draw_composer).
fn draw_reply(frame: &mut Frame, modal: &ReplyModal, area: Rect, session_title: &str) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(fg(theme::ACCENT));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let title_seg = if session_title.is_empty() {
        String::new()
    } else {
        format!("{session_title} ")
    };
    let mut spans = vec![Span::styled("↳ reply ", fg(theme::ACCENT))];
    if !title_seg.is_empty() {
        spans.push(Span::styled(title_seg.clone(), fg(theme::MUTED)));
    }
    spans.push(Span::styled("❯ ", fg(theme::ACCENT)));
    if modal.buffer.is_empty() {
        spans.push(Span::styled(
            "type a reply · Enter send · Esc cancel",
            fg(theme::FAINT),
        ));
    } else {
        spans.push(Span::styled(modal.buffer.clone(), fg(theme::TEXT)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), inner);

    if inner.width > 0 {
        // Cursor at the end of the typed reply. Prefix mirrors the fixed spans above.
        let prefix = format!("↳ reply {title_seg}❯ ");
        let col = display_width(&prefix) + display_width(&modal.buffer);
        let x = inner.x + (col as u16).min(inner.width - 1);
        frame.set_cursor_position((x, inner.y));
    }
}

// --- Header ---------------------------------------------------------------------

/// The app title with airy padding plus a right-aligned status summary. `area` is the 4-row
/// header slice from the main layout: row 0 is a blank spacer, row 1 carries `Agent Viewer`
/// in accent on the left and the `N running · M needs input` summary (muted) right-aligned on
/// the same row, and rows 2-3 are blank spacers so the list below has breathing room. The
/// status is dropped on terminals too narrow to hold it beside the title, so the title wins.
fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);
    let title_row = rows[1];

    let title = Span::styled(
        " Agent Viewer",
        Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
    );
    let running = app.running_count();
    let needs = app.needs_input_count();
    let status = format!("{running} running · {needs} needs input ");
    let status_w = display_width(&status) as u16;

    // Reserve the status region only when it fits beside the title (13 cols for " Agent
    // Viewer" plus a one-column gap); otherwise the title claims the whole row.
    if title_row.width > 13 + status_w + 1 {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(0), Constraint::Length(status_w)])
            .split(title_row);
        frame.render_widget(Paragraph::new(Line::from(title)), cols[0]);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(status, fg(theme::MUTED))))
                .alignment(Alignment::Right),
            cols[1],
        );
    } else {
        frame.render_widget(Paragraph::new(Line::from(title)), title_row);
    }
}

// --- Main list ------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn draw_list(
    frame: &mut Frame,
    app: &App,
    pulses: &Pulses,
    now_ms: i64,
    pr_status: &crate::pr_cache::PrStatusCache,
    deco: ListDeco,
    logos: Option<&LogoMarks>,
    area: Rect,
) {
    let width = area.width as usize;
    let rows = app.visible();
    let mut items: Vec<ListItem> = Vec::with_capacity(rows.len());
    // Parallel to `items`: the backend of each pushed item that is a session row (None for
    // headers/spacers/rename/expansion lines), so the logo overlay can find its rows by
    // item index after the List has laid them out.
    let mut item_backends: Vec<Option<BackendKind>> = Vec::with_capacity(rows.len());
    // The selection index only shifts if expansion lines are inserted BEFORE the selected
    // row; expansion always sits under the (selected) expanded row, so it stays aligned.
    for row in rows {
        match row {
            Row::Session { backend, id, .. } => {
                // In-place rename edit field replaces the row while renaming it. The rename row
                // has no logo (its layout differs), so it carries no backend in the overlay vec.
                if let Some((rb, rid, buf)) = deco.rename
                    && *backend == rb
                    && id == rid
                {
                    items.push(rename_row_item(*backend, buf, width));
                    item_backends.push(None);
                } else {
                    items.push(row_to_item(row, pulses, now_ms, pr_status, width));
                    item_backends.push(Some(*backend));
                }
                // Inline peek expansion: indented muted transcript tail under the row.
                if let Some((eb, eid)) = deco.expanded
                    && backend == eb
                    && id == eid
                {
                    for line in deco.expand_lines {
                        // Lines are already indented + styled by peek_expansion.
                        items.push(ListItem::new(line.clone()));
                        item_backends.push(None);
                    }
                }
            }
            _ => {
                items.push(row_to_item(row, pulses, now_ms, pr_status, width));
                item_backends.push(None);
            }
        }
    }
    let list =
        List::new(items).highlight_style(Style::default().bg(theme::SEL_BG).fg(theme::SEL_FG));
    let mut state = ListState::default();
    if !rows.is_empty() {
        let sel = app.selected_index().min(rows.len() - 1);
        state.select(Some(sel));
        // The List widget only scrolls the SELECTED item into view — the expansion lines
        // that follow it can fall below the viewport (under the composer). When expanded,
        // push the offset up so the selected row plus all its expansion lines are visible.
        let viewport = area.height as usize;
        if !deco.expand_lines.is_empty() && viewport > 0 {
            let last = sel + deco.expand_lines.len(); // item index of the last expansion line
            if last >= viewport {
                // Keep the selected row itself visible (never scroll past it).
                *state.offset_mut() = (last + 1 - viewport).min(sel);
            }
        }
    }
    frame.render_stateful_widget(list, area, &mut state);

    // Logo overlay: for each on-screen session item, draw its brand image over the two blank
    // mark columns (x+2 = after the status glyph + its trailing space). Only the visible
    // window [offset, offset+height) is drawn; y is clamped inside the list area.
    if let Some(logos) = logos
        && logo_marks()
    {
        let offset = state.offset();
        let height = area.height as usize;
        for (j, backend) in item_backends.iter().enumerate() {
            let Some(backend) = backend else { continue };
            if j < offset || j >= offset + height {
                continue;
            }
            let y = area.y + (j - offset) as u16;
            if y >= area.y + area.height || area.width < 4 {
                continue;
            }
            frame.render_widget(
                Image::new(logos.image(*backend)),
                Rect {
                    x: area.x + 2,
                    y,
                    width: 2,
                    height: 1,
                },
            );
        }
    }

    // Place the terminal's native cursor at the end of the inline rename buffer (its row is
    // the selection, so it is always on screen). Prefix is `✎ `(2) + `<mark> `(mark_width+1).
    if let Some((rb, rid, buf)) = deco.rename
        && let Some(idx) = rows.iter().position(
            |r| matches!(r, Row::Session { backend, id, .. } if *backend == rb && *id == rid),
        )
    {
        let offset = state.offset();
        let y = area.y + idx.saturating_sub(offset) as u16;
        if idx >= offset && y < area.y + area.height {
            let shown = truncate(buf, width.saturating_sub(6));
            let col = 2 + mark_width(backend_mark(rb)) + 1 + display_width(&shown);
            let x = area.x + (col as u16).min(area.width.saturating_sub(1));
            frame.set_cursor_position((x, y));
        }
    }
}

/// The selected row rendered as an inline rename edit field: `✎ <mark> buffer`, the mark in
/// the backend's brand color and the edited title in accent. The blinking cursor at the end
/// of the buffer is the terminal's native cursor, placed by `draw_list`.
fn rename_row_item(backend: BackendKind, buffer: &str, width: usize) -> ListItem<'static> {
    ListItem::new(Line::from(vec![
        Span::styled("✎ ", fg(theme::ACCENT)),
        Span::styled(
            format!("{} ", backend_mark(backend)),
            fg(backend_mark_color(backend)),
        ),
        Span::styled(truncate(buffer, width.saturating_sub(6)), fg(theme::ACCENT)),
    ]))
}

fn row_to_item(
    row: &Row,
    pulses: &Pulses,
    now_ms: i64,
    pr_status: &crate::pr_cache::PrStatusCache,
    width: usize,
) -> ListItem<'static> {
    match row {
        Row::Spacer => ListItem::new(Line::from("")),
        Row::SectionHeader {
            section,
            count,
            collapsed,
        } => ListItem::new(Line::from(Span::styled(
            header_label(section_label(*section), *count, *collapsed),
            fg(theme::ACCENT).add_modifier(Modifier::BOLD),
        ))),
        Row::ProjectHeader {
            root,
            count,
            collapsed,
        } => ListItem::new(Line::from(Span::styled(
            header_label(root.display(), *count, *collapsed),
            fg(theme::ACCENT).add_modifier(Modifier::BOLD),
        ))),
        Row::Session {
            backend,
            id,
            summary,
            status,
            title,
            updated_at_ms,
            pr_refs,
            ..
        } => {
            // A live spawn bloom overrides the glyph and flashes the row background.
            // Linear scan instead of `get(&(_, id.clone()))`: pulses is almost always
            // empty, and the map lookup would clone the id every row every frame.
            let bloom = pulses
                .iter()
                .find(|((b, pid), _)| b == backend && pid == id)
                .and_then(|(_, start)| bloom_glyph(now_ms - *start));
            let (glyph, gcolor) = match bloom {
                Some(g) => (g, theme::ACCENT),
                None => status_glyph(*status, now_ms),
            };
            let elapsed = crate::app::format_elapsed(now_ms - *updated_at_ms);
            let pr_color = pr_badge_theme_color(pr_status.badge_color(pr_refs));
            let line = session_line(SessionRow {
                glyph,
                gcolor,
                mark: backend_mark(*backend),
                mark_color: backend_mark_color(*backend),
                name: title,
                status: *status,
                summary,
                pr: &pr_badge(pr_refs),
                pr_color,
                elapsed: &elapsed,
                width,
            });
            if bloom.is_some() {
                ListItem::new(line).style(Style::default().bg(theme::SEL_BG))
            } else {
                ListItem::new(line)
            }
        }
    }
}

/// Title-case status word for a row (Claude Code style).
fn status_display_word(status: Status) -> &'static str {
    match status {
        Status::Working => "Working",
        Status::NeedsInput => "Needs input",
        Status::Idle => "Idle",
        Status::Done => "Done",
        Status::Failed => "Failed",
        Status::Stopped => "Stopped",
    }
}

/// The state's theme color for its status word.
fn status_color(status: Status) -> ratatui::style::Color {
    match status {
        Status::Working => theme::ACCENT,
        Status::NeedsInput => theme::WARN,
        Status::Idle => theme::MUTED,
        Status::Done => theme::OK,
        Status::Failed => theme::ERR,
        Status::Stopped => theme::STOPPED,
    }
}

/// The right-aligned PR badge: "" (none), "#315" (one), or "2 PRs" (many).
fn pr_badge(pr_refs: &[PrRef]) -> String {
    match pr_refs {
        [] => String::new(),
        [one] => format!("#{}", one.id),
        many => format!("{} PRs", many.len()),
    }
}

/// The theme color for a PR badge's live-status bucket.
fn pr_badge_theme_color(c: PrBadgeColor) -> ratatui::style::Color {
    match c {
        PrBadgeColor::Default => theme::ACCENT,
        PrBadgeColor::Attention => theme::WARN,
        PrBadgeColor::Passed => theme::OK,
        PrBadgeColor::Merged => theme::PR_MERGED,
        PrBadgeColor::Muted => theme::MUTED,
    }
}

/// One session row's fields, bundled so `session_line` stays one argument wide.
struct SessionRow<'a> {
    glyph: &'a str,
    gcolor: ratatui::style::Color,
    mark: &'a str,
    mark_color: ratatui::style::Color,
    name: &'a str,
    status: Status,
    summary: &'a str,
    pr: &'a str,
    pr_color: ratatui::style::Color,
    elapsed: &'a str,
    width: usize,
}

/// `glyph mark name  summary <pad> <pr> <status word> <time>`, flush-left (glyph in column
/// 0). The animated glyph + brand mark + title sit left with a muted summary; the right
/// cluster (Claude Code style) is a right-aligned `<pr> <status word> <time>` — PR badge
/// accent, status word in its state color, elapsed muted. The title truncates first when
/// width is tight; the right cluster is never clipped.
fn session_line(r: SessionRow) -> Line<'static> {
    let word = status_display_word(r.status);
    // The right cluster as one reserved unit: [pr ]word elapsed.
    let right = if r.pr.is_empty() {
        format!("{word} {}", r.elapsed)
    } else {
        format!("{} {word} {}", r.pr, r.elapsed)
    };
    let (name_out, summary_out, pad) = crate::app::row_layout(
        r.width,
        mark_width(r.mark),
        r.name,
        r.summary,
        right.chars().count(),
    );
    let mut spans = vec![
        Span::styled(r.glyph.to_string(), fg(r.gcolor)),
        Span::raw(" "),
        Span::styled(r.mark.to_string(), fg(r.mark_color)),
        Span::raw(" "),
        Span::styled(name_out, fg(theme::TEXT)),
    ];
    if !summary_out.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(summary_out, fg(theme::MUTED)));
    }
    spans.push(Span::raw(" ".repeat(pad)));
    // Right cluster: <pr> <status word> <time>.
    if !r.pr.is_empty() {
        spans.push(Span::styled(r.pr.to_string(), fg(r.pr_color)));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(word.to_string(), fg(status_color(r.status))));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(r.elapsed.to_string(), fg(theme::MUTED)));
    Line::from(spans)
}

// --- Footer ---------------------------------------------------------------------

fn draw_footer(frame: &mut Frame, app: &App, mode: &Mode, notice: &str, now_ms: i64, area: Rect) {
    let line = match mode {
        Mode::Filter => Line::from(format!("/{}", app.filter())),
        Mode::Rename(_) => Line::from("rename in row — Enter apply · Esc cancel"),
        Mode::Reply(_) => Line::from("reply — Enter send · Esc cancel"),
        Mode::Help => Line::from("help — Esc/? to close"),
        Mode::Attached => Line::from(""),
        Mode::Normal => {
            if !notice.is_empty() {
                Line::from(Span::styled(notice.to_string(), fg(theme::WARN)))
            } else if app.is_armed(now_ms) {
                Line::from(Span::styled(
                    "[press Ctrl+X again to remove]",
                    fg(theme::ERR),
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
                        "{hidden_txt}{showing}type task · Tab agent · ⇧Tab model · /model pick · Enter spawn/attach · space peek · Ctrl+R rename · Ctrl+X stop/remove · Ctrl+S group · a all · Ctrl+F filter · ? help · q/Ctrl+C quit"
                    ),
                    fg(theme::MUTED),
                ))
            }
        }
    };
    frame.render_widget(Paragraph::new(line), area);
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
        Span::styled(format!(" {glyph}"), fg(gcolor)),
        Span::styled(
            left_trunc.chars().skip(2).collect::<String>(),
            fg(theme::TEXT),
        ),
        Span::raw(" ".repeat(pad)),
        Span::styled(
            right.to_string(),
            fg(if exited { theme::ERR } else { theme::MUTED }),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(theme::SEL_BG)),
        area,
    );
}

// --- Help overlay ---------------------------------------------------------------

fn draw_help(frame: &mut Frame, area: Rect) {
    let popup = centered_rect(56, 70, area);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(fg(theme::ACCENT))
        .title("keys");
    let entries = [
        ("↑/↓  j/k", "move selection"),
        ("→ / Enter", "attach selected (empty composer)"),
        ("type · Tab", "compose task · switch agent"),
        ("Shift+Tab", "cycle agent model"),
        ("/model", "pick from all available models"),
        ("Enter", "spawn composed task"),
        ("← back", "detach (composer empty)"),
        ("Ctrl+]", "detach (always)"),
        ("Space", "expand peek in row"),
        ("Ctrl+E", "reply to a blocked session"),
        ("Enter/Space", "collapse group (on a header)"),
        ("Ctrl+R", "rename in row"),
        ("Ctrl+X", "stop, then press again to remove"),
        ("Ctrl+S", "group by state / by project"),
        ("a", "show all (companions + archived)"),
        ("h / u", "archive / unarchive"),
        ("Ctrl+F", "filter (searches hidden too)"),
        ("?", "this help"),
        ("q / Ctrl+C", "quit"),
    ];
    let mut lines = Vec::new();
    for (k, v) in entries {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<12}"), fg(theme::ACCENT)),
            Span::styled(v.to_string(), fg(theme::TEXT)),
        ]));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_for_defaults_to_tags_and_opts_into_glyphs() {
        // Default (glyph = false): the textual tags.
        assert_eq!(mark_for(BackendKind::Claude, false), "[cc]");
        assert_eq!(mark_for(BackendKind::Codex, false), "[cx]");
        assert_eq!(mark_for(BackendKind::Opencode, false), "[oc]");
        // Opt-in (glyph = true): the brand glyphs.
        assert_eq!(mark_for(BackendKind::Claude, true), "✳");
        assert_eq!(mark_for(BackendKind::Codex, true), "◆");
        assert_eq!(mark_for(BackendKind::Opencode, true), "■");
    }

    #[test]
    fn logo_marks_blank_the_mark_slot_to_two_columns() {
        // Logo mode wins over glyph/tag and reserves exactly two blank columns for the image
        // overlay, keeping mark_width == 2 so all row/composer layout math is unchanged.
        set_logo_marks(true);
        assert!(logo_marks());
        for b in [
            BackendKind::Claude,
            BackendKind::Codex,
            BackendKind::Opencode,
        ] {
            assert_eq!(backend_mark(b), "  ");
            assert_eq!(mark_width(backend_mark(b)), 2);
        }
    }

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
