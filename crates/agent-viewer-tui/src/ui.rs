//! Rendering surface: a flat single-list main view (state- or project-grouped) with a
//! one-line footer, centered new/rename/help modals, and the full-screen embedded-PTY attach
//! view. The approved amber palette lives in `theme`.

use crate::app::{App, Composer, Row, Section};
use crate::logos::LogoMarks;
use agent_viewer_core::pty::PtySession;
use agent_viewer_core::{BackendKind, PrBadgeColor, PrRef, Session, Status};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};
use ratatui_image::Image;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use tui_term::widget::PseudoTerminal;

/// A live spawn-bloom one-shot, keyed by session, holding the ms it started (now_ms).
pub type Pulses = HashMap<(BackendKind, String), i64>;

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
fn status_glyph(status: &Status, now_ms: i64) -> (&'static str, ratatui::style::Color) {
    match status {
        Status::Working => (shimmer_glyph(now_ms), theme::ACCENT),
        Status::NeedsInput { .. } => ("◐", BREATH_STEPS[breath_phase(now_ms)]),
        Status::Idle => ("∙", theme::MUTED),
        Status::Done => ("●", theme::OK),
        Status::Error => ("✗", theme::ERR),
        Status::Unknown => ("?", theme::MUTED),
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

/// A group header line: a triangle indicator plus its member count. An open group shows
/// "▼ <label>  (<count>)"; collapsed shows "▶ <label>  (<count> hidden)".
fn header_label(label: impl std::fmt::Display, count: usize, collapsed: bool) -> String {
    if collapsed {
        format!("▶ {label}  ({count} hidden)")
    } else {
        format!("▼ {label}  ({count})")
    }
}

// --- Modals / modes -------------------------------------------------------------

/// The `Ctrl+R` inline-rename state (opens blank, never prefilled). Rendered in the
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
/// composer and inline rename both live on the Normal list view.
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

// --- Mouse hit-testing ----------------------------------------------------------

/// Frame-persistent list geometry captured during `draw_list`, so the event loop can
/// reverse a screen cell (x, y) back to a selectable `visible()`-row index for mouse
/// click/hover selection. Rebuilt every frame; the mouse handler reads the latest.
#[derive(Debug, Clone, Default)]
pub struct ListHit {
    /// The list widget's screen rectangle (origin + size) as drawn this frame.
    area: Rect,
    /// The List widget's final scroll offset (item index of the first visible item).
    offset: usize,
    /// One entry per rendered item line, in draw order: `Some(visible-row index)` for a
    /// selectable row, `None` for a non-selectable Spacer.
    item_to_row: Vec<Option<usize>>,
    /// A floating overlay (the slash-command popup) that shadows the bottom of the list this
    /// frame, if any. A cell inside it belongs to the overlay, not the row drawn underneath,
    /// so hit-testing treats it as a hole.
    blocked: Option<Rect>,
}

impl ListHit {
    /// Reverse a terminal cell `(x, y)` to the selectable `visible()`-row index under it, if
    /// the point falls on a selectable row within the list area. Pure — unit-testable with no
    /// terminal. Returns `None` outside the area, on a blank spacer, or on a cell shadowed by
    /// a floating overlay (the slash-command popup).
    pub fn row_at(&self, x: u16, y: u16) -> Option<usize> {
        let a = self.area;
        if a.width == 0 || a.height == 0 {
            return None;
        }
        if x < a.x
            || x >= a.x.saturating_add(a.width)
            || y < a.y
            || y >= a.y.saturating_add(a.height)
        {
            return None;
        }
        // A cell under the floating popup belongs to the popup, not the obscured list row.
        if let Some(b) = self.blocked
            && x >= b.x
            && x < b.x.saturating_add(b.width)
            && y >= b.y
            && y < b.y.saturating_add(b.height)
        {
            return None;
        }
        let line_in_viewport = (y - a.y) as usize;
        let item = self.offset.checked_add(line_in_viewport)?;
        self.item_to_row.get(item).copied().flatten()
    }
}

// --- Draw entry point -----------------------------------------------------------

/// Everything the frame needs, bundled so the entry point stays one argument wide.
pub struct Draw<'a> {
    pub app: &'a App,
    pub workspace: &'a Path,
    pub mode: &'a Mode,
    pub notice: &'a str,
    pub composer: &'a Composer,
    pub pulses: &'a Pulses,
    pub now_ms: i64,
    pub attach: Option<AttachView<'a>>,
    pub pr_status: &'a crate::pr_cache::PrStatusCache,
    /// The brand-logo protocols, present when the startup graphics probe succeeded. Overlaid
    /// on the reserved mark slot during render.
    pub logos: Option<&'a LogoMarks>,
    /// Sink for the frame's list geometry, so the event loop can hit-test mouse clicks/hover
    /// back to a row. Written each frame via interior mutability (draw borrows `&` throughout).
    pub list_hit: &'a RefCell<ListHit>,
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

    // The composer box keeps one metadata row above its wrapped input and grows to a cap so
    // a long task description stays visible. Reply mode sizes its prompt prefixed box to the
    // reply buffer.
    let inner_w = input_inner_width(frame.area().width);
    let composer_h = match &d.mode {
        Mode::Reply(m) => {
            let title = d
                .app
                .session_for(&(m.backend, m.id.clone()))
                .map(|s| s.title.clone())
                .unwrap_or_default();
            let title_seg = if title.is_empty() {
                String::new()
            } else {
                format!("{title} ")
            };
            let prefix = format!("↳ reply {title_seg}❯ ");
            input_box_height(display_width(&prefix), &m.buffer, inner_w)
        }
        _ => composer_box_height(d.composer.text(), inner_w),
    };

    // header (blank gap + title/status + blank gaps) · list · blank gap · bordered composer box
    // (grows with wrapped input) · footer.
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(composer_h),
            Constraint::Length(1),
        ])
        .split(frame.area());

    // Inline rename edits the selected row in place.
    let rename = match d.mode {
        Mode::Rename(m) => Some((m.backend, m.id.as_str(), m.buffer.as_str())),
        _ => None,
    };
    let deco = ListDeco { rename };

    draw_header(frame, d.app, d.workspace, vertical[0]);
    // The composer cursor blinks only in Normal mode (the composer is the active input);
    // the rename cursor is placed on the edit row by draw_list; Help/Filter show neither.
    // draw_list returns the frame's list geometry for mouse hit-testing; the slash popup (drawn
    // below, floating over the list's bottom) shadows part of it, so record that hole too.
    let mut hit = draw_list(
        frame,
        d.app,
        d.pulses,
        d.now_ms,
        d.pr_status,
        deco,
        d.logos,
        vertical[1],
    );
    if matches!(d.mode, Mode::Normal) {
        hit.blocked = slash_popup_area(d.composer, vertical[3]);
    }
    *d.list_hit.borrow_mut() = hit;
    // Reply mode replaces the spawn composer with a small reply input. Every other mode shows
    // the persistent spawn composer.
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
        let highlight = d.composer.suggestion_highlight();
        if d.composer.is_model_command() {
            draw_suggestion_popup(
                frame,
                &d.composer.model_suggestions(),
                highlight,
                "",
                vertical[3],
            );
        } else {
            draw_suggestion_popup(
                frame,
                &d.composer.suggestions(),
                highlight,
                "/",
                vertical[3],
            );
        }
    }

    if matches!(d.mode, Mode::Help) {
        draw_help(frame, frame.area());
    }
}

/// The screen rectangle the slash-command popup occupies (floating directly above the
/// composer box), or None when nothing is shown. Pure so both the renderer and the mouse
/// hit-test agree on exactly which cells the popup shadows over the list.
fn slash_popup_area(composer: &Composer, composer_area: Rect) -> Option<Rect> {
    if composer.suggestions().is_empty() {
        return None;
    }
    let height = (composer.suggestions().len() as u16).min(composer_area.y);
    if height == 0 {
        return None;
    }
    Some(Rect {
        x: composer_area.x,
        y: composer_area.y - height,
        width: composer_area.width,
        height,
    })
}

/// Render completion suggestions as a few muted lines directly above the composer box (the
/// highlighted row in accent), each rendered as `" {prefix}{item}"` — `prefix` is "/" for the
/// slash-command popup and "" for the `/model` picker. Nothing renders when the list is empty.
fn draw_suggestion_popup<S: AsRef<str>>(
    frame: &mut Frame,
    suggestions: &[S],
    highlight: usize,
    prefix: &str,
    composer_area: Rect,
) {
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
    let items: Vec<ListItem> = suggestions
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let style = if i == highlight {
                fg(theme::ACCENT)
            } else {
                fg(theme::MUTED)
            };
            ListItem::new(Line::from(Span::styled(
                format!(" {prefix}{}", item.as_ref()),
                style,
            )))
        })
        .collect();
    frame.render_widget(List::new(items), area);
}

/// Per-row decorations layered over the list model.
struct ListDeco<'a> {
    rename: Option<(BackendKind, &'a str, &'a str)>,
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

/// Upper bound on how many wrapped text rows the input box grows to before it stops growing
/// and keeps only the tail (the cursor end) visible. Keeps a runaway paste from eating the
/// whole screen while still showing a generous multi-line task description.
const COMPOSER_MAX_LINES: u16 = 10;

/// The block every input box is drawn in: rounded and bordered, with content immediately
/// inside the side borders.
fn input_block(border: ratatui::style::Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(fg(border))
}

/// The text width an input box has left inside `frame_width` once both borders are taken out.
/// The height math wraps against that width so it agrees with the render.
fn input_inner_width(frame_width: u16) -> u16 {
    frame_width.saturating_sub(2)
}

/// Display width of a single character, floored at zero for combining marks.
fn char_width(ch: char) -> usize {
    use unicode_width::UnicodeWidthChar;
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// Split `text` into visual lines by terminal display width: the first line gets `first`
/// columns, every line after it gets `rest`. Breaks at explicit newlines and, within a line,
/// at the last space that still fits, so words stay whole the way a text editor wraps them.
/// The run of spaces a break is taken at is dropped, so a wrapped line never opens with the
/// space that ended the previous one; a single word too long for a whole line still breaks
/// mid-word. Wide and zero width glyphs are measured, not counted. Always returns at least
/// one segment.
fn wrap_by_width(text: &str, first: usize, rest: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let rest = rest.max(1);
    for para in text.split('\n') {
        let budget = if lines.is_empty() { first.max(1) } else { rest };
        wrap_paragraph(para, budget, rest, &mut lines);
    }
    lines
}

/// Wrap one newline-free paragraph into `out`, always pushing at least one segment (an empty
/// paragraph is a blank line, which the composer must keep).
fn wrap_paragraph(para: &str, first: usize, rest: usize, out: &mut Vec<String>) {
    let mut cur = String::new();
    let mut cur_w = 0usize;
    let mut budget = first;
    // Spaces seen since the last word, held back so a break can drop them instead of
    // starting the next line with them.
    let mut gap = String::new();
    let mut gap_w = 0usize;

    // Push `cur` as a finished line and reopen an empty one at the continuation budget.
    macro_rules! break_line {
        () => {{
            out.push(std::mem::take(&mut cur));
            cur_w = 0;
            budget = rest;
        }};
    }

    let mut chars = para.chars().peekable();
    while let Some(&ch) = chars.peek() {
        if ch.is_whitespace() {
            chars.next();
            gap.push(ch);
            gap_w += char_width(ch);
            continue;
        }
        // The next word: a run of non-space characters, measured whole so the break decision
        // can be made before any of it is committed to the current line.
        let mut word = String::new();
        let mut word_w = 0usize;
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() {
                break;
            }
            chars.next();
            word.push(c);
            word_w += char_width(c);
        }
        if !cur.is_empty() && cur_w + gap_w + word_w > budget {
            // The word does not fit after the gap, so the line ends here and the gap is eaten.
            break_line!();
        } else {
            cur.push_str(&gap);
            cur_w += gap_w;
        }
        gap.clear();
        gap_w = 0;
        for c in word.chars() {
            let cw = char_width(c);
            if cur_w + cw > budget && !cur.is_empty() {
                break_line!();
            }
            cur.push(c);
            cur_w += cw;
        }
    }
    // Trailing spaces are kept: the user typed them and the cursor sits after them.
    for c in gap.chars() {
        let cw = char_width(c);
        if cur_w + cw > budget && !cur.is_empty() {
            break_line!();
        }
        cur.push(c);
        cur_w += cw;
    }
    out.push(cur);
}

/// The full box height (including both borders) an input box needs to show `text` wrapped to
/// `inner_width` columns after a `prefix_w`-wide prompt. Empty text (or a zero-width area)
/// keeps the resting 3-row box; longer text grows a row per wrapped line up to the cap.
fn input_box_height(prefix_w: usize, text: &str, inner_width: u16) -> u16 {
    if inner_width == 0 || text.is_empty() {
        return 3;
    }
    let w = inner_width as usize;
    let first = w.saturating_sub(prefix_w);
    let lines = wrap_by_width(text, first, w).len();
    (lines as u16).clamp(1, COMPOSER_MAX_LINES) + 2
}

/// Composer height includes one fixed metadata row, the visible input rows, and both borders.
fn composer_box_height(text: &str, inner_width: u16) -> u16 {
    let input_lines = if inner_width == 0 || text.is_empty() {
        1usize
    } else {
        let segments = wrap_by_width(text, inner_width as usize, inner_width as usize);
        segments.len()
            + usize::from(display_width(segments.last().unwrap()) == inner_width as usize)
    };
    input_lines.clamp(1, COMPOSER_MAX_LINES as usize) as u16 + 3
}

/// The look of an input box: its border color, the colored prompt spans that lead the first
/// line (with their measured width), and the faint placeholder shown when empty.
struct InputBox {
    border: ratatui::style::Color,
    prefix_spans: Vec<Span<'static>>,
    prefix_w: usize,
    placeholder: &'static str,
}

/// Render a growing bordered input box: the colored prompt spans lead the first line, the
/// text wraps across the inner rows (which the caller sized via `input_box_height`), the
/// placeholder shows when empty, and the native cursor sits at the end of `text` when
/// `show_cursor`. If the text wraps past the box, the tail is kept so the cursor stays
/// visible. Returns the inner rect and whether the first logical line (the prompt) is on
/// screen, for the caller's logo overlay.
fn draw_input_box(
    frame: &mut Frame,
    area: Rect,
    style: InputBox,
    text: &str,
    show_cursor: bool,
) -> (Rect, bool) {
    let InputBox {
        border,
        prefix_spans,
        prefix_w,
        placeholder,
    } = style;
    let block = input_block(border);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return (inner, false);
    }

    if text.is_empty() {
        let mut spans = prefix_spans;
        spans.push(Span::styled(placeholder.to_string(), fg(theme::FAINT)));
        frame.render_widget(Paragraph::new(Line::from(spans)), inner);
        if show_cursor {
            let x = inner.x + (prefix_w as u16).min(inner.width - 1);
            frame.set_cursor_position((x, inner.y));
        }
        return (inner, true);
    }

    let w = inner.width as usize;
    let first = w.saturating_sub(prefix_w);
    let segs = wrap_by_width(text, first, w);
    let total = segs.len();
    // Show the last `inner.height` wrapped lines so a very long input keeps its tail (and the
    // cursor) in view; the prompt prefix only renders when the true first line is visible.
    let start = total.saturating_sub(inner.height as usize);
    let mut lines: Vec<Line> = Vec::with_capacity(total - start);
    for (i, seg) in segs[start..].iter().enumerate() {
        if start + i == 0 {
            let mut spans = prefix_spans.clone();
            spans.push(Span::styled(seg.clone(), fg(theme::TEXT)));
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::from(Span::styled(seg.clone(), fg(theme::TEXT))));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);

    if show_cursor {
        // Cursor at the end of the text: last visible row, offset by the prompt only when the
        // end sits on the first logical line.
        let last_w = display_width(segs.last().unwrap());
        let col = if total == 1 {
            prefix_w + last_w
        } else {
            last_w
        };
        let row = (total - start - 1) as u16;
        let x = inner.x + (col as u16).min(inner.width - 1);
        frame.set_cursor_position((x, inner.y + row));
    }
    (inner, start == 0)
}

/// Persistent inline spawn composer with metadata on the first inner row and full width input
/// on every following row. Long input keeps its tail visible beneath the fixed metadata.
fn draw_composer(
    frame: &mut Frame,
    app: &App,
    composer: &Composer,
    logos: Option<&LogoMarks>,
    area: Rect,
    show_cursor: bool,
) {
    let backend = composer.backend();
    let dir = app
        .spawn_target()
        .map(|d| abbreviate_dir(&d))
        .unwrap_or_default();
    let model = composer.model();
    let model_seg = if model == "default" {
        String::new()
    } else {
        format!("{model} ")
    };
    let mut metadata_spans = vec![Span::styled(
        format!("{}{} ", backend_mark(backend), backend.name()),
        fg(backend_mark_color(backend)),
    )];
    if !model_seg.is_empty() {
        metadata_spans.push(Span::styled(model_seg, fg(theme::MUTED)));
    }
    metadata_spans.push(Span::styled(dir, fg(theme::MUTED)));

    let block = input_block(theme::FAINT);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    frame.render_widget(
        Paragraph::new(Line::from(metadata_spans)),
        Rect { height: 1, ..inner },
    );

    let input = Rect {
        y: inner.y.saturating_add(1),
        height: inner.height.saturating_sub(1),
        ..inner
    };
    if input.height > 0 {
        if composer.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "describe a task · tab to switch agent",
                    fg(theme::FAINT),
                ))),
                input,
            );
            if show_cursor {
                frame.set_cursor_position((input.x, input.y));
            }
        } else {
            let width = input.width as usize;
            let mut segments = wrap_by_width(composer.text(), width, width);
            if display_width(segments.last().unwrap()) == width {
                segments.push(String::new());
            }
            let start = segments.len().saturating_sub(input.height as usize);
            let lines = segments[start..]
                .iter()
                .map(|segment| Line::from(Span::styled(segment.clone(), fg(theme::TEXT))))
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(lines), input);

            if show_cursor {
                let last_width = display_width(segments.last().unwrap()) as u16;
                let x = input.x + last_width.min(input.width - 1);
                let y = input.y + (segments.len() - start - 1) as u16;
                frame.set_cursor_position((x, y));
            }
        }
    }

    // Logo mode: overlay the image on the reserved mark slot in the metadata row.
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
}

/// The reply-compose input, occupying the composer box: an accent-bordered rounded box with
/// `↳ reply <title> ❯ <buffer>` (title muted, prompt accent, text bright), a muted
/// placeholder when empty, and the native cursor at the end of the buffer (as draw_composer).
/// Long replies wrap down the (grown) box.
fn draw_reply(frame: &mut Frame, modal: &ReplyModal, area: Rect, session_title: &str) {
    let title_seg = if session_title.is_empty() {
        String::new()
    } else {
        format!("{session_title} ")
    };
    let mut prefix_spans = vec![Span::styled("↳ reply ", fg(theme::ACCENT))];
    if !title_seg.is_empty() {
        prefix_spans.push(Span::styled(title_seg.clone(), fg(theme::MUTED)));
    }
    prefix_spans.push(Span::styled("❯ ", fg(theme::ACCENT)));
    let prefix_w = display_width(&format!("↳ reply {title_seg}❯ "));

    draw_input_box(
        frame,
        area,
        InputBox {
            border: theme::ACCENT,
            prefix_spans,
            prefix_w,
            placeholder: "type a reply · Enter send · Esc cancel",
        },
        &modal.buffer,
        true,
    );
}

// --- Header ---------------------------------------------------------------------

/// Render the product identity, launch workspace, and full-snapshot status counts between
/// the leading gap and the two trailing rows of list breathing room.
fn draw_header(frame: &mut Frame, app: &App, workspace: &Path, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" [av] ", fg(theme::ACCENT)),
            Span::styled(
                "Agent Viewer",
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" v{}", env!("CARGO_PKG_VERSION")), fg(theme::MUTED)),
        ])),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" Workspace ", fg(theme::MUTED)),
            Span::styled(workspace.display().to_string(), fg(theme::TEXT)),
        ])),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {} awaiting input", app.needs_input_count()),
                fg(theme::WARN),
            ),
            Span::styled(" · ", fg(theme::MUTED)),
            Span::styled(
                format!("{} working", app.running_count()),
                fg(theme::ACCENT),
            ),
            Span::styled(" · ", fg(theme::MUTED)),
            Span::styled(
                format!("{} completed", app.completed_count()),
                fg(theme::MUTED),
            ),
        ])),
        rows[3],
    );
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
) -> ListHit {
    let width = area.width as usize;
    let rows = app.visible();
    let mut items: Vec<ListItem> = Vec::with_capacity(rows.len());
    // Parallel to `items`: the backend of each pushed session row (None for headers/spacers).
    // Renamed sessions retain their backend so the logo overlay can find its rows by item index
    // after the List has laid them out.
    let mut item_backends: Vec<Option<BackendKind>> = Vec::with_capacity(rows.len());
    // Also parallel to `items`, in lockstep draw order: each rendered line's selectable row
    // target (Some(visible-row index) for a header/session line, None for a Spacer). This is
    // the map the mouse handler reverses to pick a row from a cell.
    let mut item_to_row: Vec<Option<usize>> = Vec::with_capacity(rows.len());
    let rename_shown = deco
        .rename
        .map(|(backend, _, buffer)| rename_buffer(backend, buffer, width));
    for (row_idx, row) in rows.iter().enumerate() {
        // A Spacer renders a blank line but is never selectable, so it maps to no row.
        let target = if matches!(row, Row::Spacer) {
            None
        } else {
            Some(row_idx)
        };
        match row {
            Row::Session { backend, id, .. } => {
                // In-place rename edit field replaces the row while renaming it. Its compact
                // prefix uses the normal mark slot, so logo mode overlays that slot as usual.
                if let Some((rb, rid, _)) = deco.rename
                    && *backend == rb
                    && id == rid
                {
                    items.push(rename_row_item(
                        *backend,
                        rename_shown.as_deref().unwrap_or_default(),
                    ));
                    item_backends.push(Some(*backend));
                } else {
                    items.push(row_to_item(row, pulses, now_ms, pr_status, width));
                    item_backends.push(Some(*backend));
                }
                item_to_row.push(target);
            }
            _ => {
                items.push(row_to_item(row, pulses, now_ms, pr_status, width));
                item_backends.push(None);
                item_to_row.push(target);
            }
        }
    }
    let list = List::new(items).highlight_style(Style::default().bg(theme::SEL_BG));
    let mut state = ListState::default();
    if !rows.is_empty() {
        let sel = app.selected_index().min(rows.len() - 1);
        state.select(Some(sel));
    }
    frame.render_stateful_widget(list, area, &mut state);
    // Capture the final scroll offset AFTER render (the widget computes it from the selection),
    // together with the area and the item->row map, so the event loop can hit-test the mouse.
    let hit = ListHit {
        area,
        offset: state.offset(),
        item_to_row,
        // The caller (`draw`) fills this in once the slash popup area is known.
        blocked: None,
    };

    // Logo overlay: for each on-screen session item, draw its brand image over the two blank
    // mark columns (x+1 = immediately after the status glyph). Only the visible
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
            if y >= area.y + area.height || area.width < 3 {
                continue;
            }
            frame.render_widget(
                Image::new(logos.image(*backend)),
                Rect {
                    x: area.x + 1,
                    y,
                    width: 2,
                    height: 1,
                },
            );
        }
    }

    // Place the terminal's native cursor at the end of the inline rename buffer (its row is
    // the selection, so it is always on screen). Prefix is `✎` + `<mark>`.
    if let Some((rb, rid, _)) = deco.rename
        && let Some(idx) = rows.iter().position(
            |r| matches!(r, Row::Session { backend, id, .. } if *backend == rb && *id == rid),
        )
    {
        let offset = state.offset();
        let y = area.y + idx.saturating_sub(offset) as u16;
        if idx >= offset && y < area.y + area.height {
            let prefix_width = display_width("✎") + mark_width(backend_mark(rb));
            let shown = rename_shown.as_deref().unwrap_or_default();
            let col = prefix_width + display_width(shown);
            let x = area.x + (col as u16).min(area.width.saturating_sub(1));
            frame.set_cursor_position((x, y));
        }
    }

    hit
}

/// The selected row rendered as an inline rename edit field: `✎<mark>buffer`, the mark in
/// the backend's brand color and the edited title in accent. The blinking cursor at the end
/// of the buffer is the terminal's native cursor, placed by `draw_list`.
fn rename_row_item(backend: BackendKind, shown: &str) -> ListItem<'static> {
    ListItem::new(Line::from(vec![
        Span::styled("✎", fg(theme::ACCENT)),
        Span::styled(
            backend_mark(backend).to_string(),
            fg(backend_mark_color(backend)),
        ),
        Span::styled(shown.to_string(), fg(theme::ACCENT)),
    ]))
}

/// Keep one terminal cell open after the displayed edit buffer for the native cursor.
fn rename_buffer(backend: BackendKind, buffer: &str, width: usize) -> String {
    let prefix_width = display_width("✎") + mark_width(backend_mark(backend));
    let buffer_width = width.saturating_sub(prefix_width.saturating_add(1));
    truncate_display_width(buffer, buffer_width)
}

/// Truncate a string without exceeding its terminal display width.
fn truncate_display_width(s: &str, width: usize) -> String {
    let mut end = 0;
    for (index, character) in s.char_indices() {
        let next = index + character.len_utf8();
        if display_width(&s[..next]) > width {
            break;
        }
        end = next;
    }
    s[..end].to_string()
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
            created_at_ms,
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
                None => status_glyph(status, now_ms),
            };
            let started_at_ms = if *created_at_ms > 0 {
                *created_at_ms
            } else {
                *updated_at_ms
            };
            let elapsed = crate::app::format_elapsed(now_ms - started_at_ms);
            let pr_color = pr_badge_theme_color(pr_status.badge_color(pr_refs));
            let line = session_line(SessionRow {
                glyph,
                gcolor,
                mark: backend_mark(*backend),
                mark_color: backend_mark_color(*backend),
                name: title,
                status,
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
fn status_display_word(status: &Status) -> &'static str {
    match status {
        Status::Working => "Working",
        Status::NeedsInput { .. } => "Needs input",
        Status::Idle => "Idle",
        Status::Done => "Done",
        Status::Error => "Error",
        Status::Unknown => "Unknown",
    }
}

/// The state's theme color for its status word.
fn status_color(status: &Status) -> ratatui::style::Color {
    match status {
        Status::Working => theme::ACCENT,
        Status::NeedsInput { .. } => theme::WARN,
        Status::Idle => theme::MUTED,
        Status::Done => theme::OK,
        Status::Error => theme::ERR,
        Status::Unknown => theme::MUTED,
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
    status: &'a Status,
    summary: &'a str,
    pr: &'a str,
    pr_color: ratatui::style::Color,
    elapsed: &'a str,
    width: usize,
}

/// `glyphmarkname  summary <pad> <pr> <status word> <time>`, flush-left (glyph in column
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
        Span::styled(r.mark.to_string(), fg(r.mark_color)),
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
                        "{hidden_txt}{showing}type task · Tab agent · ⇧Tab model · /model pick · Enter spawn/attach · Space group header · Ctrl+R rename · Ctrl+X stop/remove · Ctrl+S group · Ctrl+A all · Ctrl+D archive · Ctrl+U unarchive · Ctrl+F filter · ? help · Ctrl+C quit"
                    ),
                    fg(theme::MUTED),
                ))
            }
        }
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// Truncate `s` to at most `width` chars. NOTE: at `width == 0` this returns the FULL string
/// (width 0 is treated as "unconstrained" by the callers here) — deliberately unlike
/// `app::truncate_to`, which returns the empty string at width 0. Do not merge the two helpers
/// without preserving each caller's zero-width behavior.
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
    let (glyph, gcolor) = status_glyph(&session.status, now_ms);
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
    let popup = centered_rect(75, 100, area);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(fg(theme::ACCENT))
        .title("keys");
    let entries = [
        ("↑/↓", "move selection"),
        ("→ / Enter", "attach selected (empty composer)"),
        ("type · Tab", "compose task · switch agent"),
        ("Shift+Tab", "cycle agent model"),
        ("/model", "pick from all available models"),
        ("Enter", "spawn composed task"),
        ("← back", "detach (composer empty)"),
        ("Ctrl+]", "detach (always)"),
        ("Enter/Space", "toggle group on a header"),
        ("Ctrl+R", "rename in row"),
        ("Ctrl+X", "stop, then press again to remove"),
        ("Ctrl+S", "group by state / by project"),
        ("Ctrl+A", "show all (companions + archived)"),
        ("Ctrl+D / Ctrl+U", "archive / unarchive"),
        ("Ctrl+F", "filter (searches hidden too)"),
        ("Ctrl+T", "mouse off/on (off = drag to select + copy)"),
        ("?", "this help"),
        ("Ctrl+C", "quit"),
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

    fn draw_rename(
        buffer: &str,
        width: u16,
        logos: Option<&LogoMarks>,
    ) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        let backend = BackendKind::Codex;
        let id = "rename";
        let app = App::new(vec![Session {
            backend,
            id: id.into(),
            short_id: None,
            origin: agent_viewer_core::SessionOrigin::Interactive,
            title: "original title".into(),
            cwd: "/tmp/agent-viewer-rename".into(),
            git_branch: None,
            status: Status::Done,
            created_at_ms: 0,
            updated_at_ms: 0,
            hidden: false,
            companion: false,
            summary: String::new(),
            pid: None,
            rollout_path: None,
            pr_refs: Vec::new(),
            daemon_hosted: false,
        }]);
        let pulses = Pulses::new();
        let pr_status = crate::pr_cache::PrStatusCache::default();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 1)).unwrap();
        terminal
            .draw(|frame| {
                draw_list(
                    frame,
                    &app,
                    &pulses,
                    0,
                    &pr_status,
                    ListDeco {
                        rename: Some((backend, id, buffer)),
                    },
                    logos,
                    Rect::new(0, 0, width, 1),
                );
            })
            .unwrap();
        terminal
    }

    #[test]
    fn rename_truncates_wide_input_by_display_width() {
        set_logo_marks(true);
        let mut terminal = draw_rename("a界bc", 8, None);
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(3, 0)].symbol(), "a");
        assert_eq!(buffer[(4, 0)].symbol(), "界");
        assert_eq!(buffer[(6, 0)].symbol(), "b");
        assert_eq!(buffer[(7, 0)].symbol(), " ");
        terminal.backend_mut().assert_cursor_position((7, 0));
    }

    #[test]
    fn rename_logo_overlay_uses_compact_mark_slot() {
        set_logo_marks(true);
        let logos = LogoMarks::halfblocks_for_test().unwrap();
        let terminal = draw_rename("abcde", 8, Some(&logos));
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(0, 0)].symbol(), "✎");
        assert_ne!(buffer[(1, 0)].symbol(), " ");
        assert_ne!(buffer[(2, 0)].symbol(), " ");
        assert_eq!(buffer[(3, 0)].symbol(), "a");
    }

    #[test]
    fn selected_row_keeps_title_and_summary_contrast() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let app = App::new(vec![Session {
            backend: BackendKind::Codex,
            id: "selected-row".into(),
            short_id: None,
            origin: agent_viewer_core::SessionOrigin::Interactive,
            title: "TITLE".into(),
            cwd: "/tmp/agent-viewer-selected-row".into(),
            git_branch: None,
            status: Status::Done,
            created_at_ms: 0,
            updated_at_ms: 0,
            hidden: false,
            companion: false,
            summary: "SUMMARY".into(),
            pid: None,
            rollout_path: None,
            pr_refs: Vec::new(),
            daemon_hosted: false,
        }]);
        let selected_y = app.selected_index() as u16;
        assert!(matches!(
            app.visible()[selected_y as usize],
            Row::Session { .. }
        ));

        let area = Rect::new(0, 0, 80, app.visible().len() as u16);
        let pulses = Pulses::new();
        let pr_status = crate::pr_cache::PrStatusCache::default();
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        terminal
            .draw(|frame| {
                draw_list(
                    frame,
                    &app,
                    &pulses,
                    0,
                    &pr_status,
                    ListDeco { rename: None },
                    None,
                    area,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let title = (0..area.width)
            .map(|x| &buffer[(x, selected_y)])
            .find(|cell| cell.symbol() == "T")
            .expect("title cell");
        let summary = (0..area.width)
            .map(|x| &buffer[(x, selected_y)])
            .find(|cell| cell.symbol() == "S")
            .expect("summary cell");

        assert_eq!(title.fg, theme::TEXT);
        assert_eq!(summary.fg, theme::MUTED);
        assert_eq!(title.bg, theme::SEL_BG);
        assert_eq!(summary.bg, theme::SEL_BG);
        assert_ne!(title.fg, summary.fg);
    }

    #[test]
    fn session_elapsed_uses_creation_time_after_activity_refresh() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let session = |created_at_ms, updated_at_ms| Session {
            backend: BackendKind::Codex,
            id: "elapsed".into(),
            short_id: None,
            origin: agent_viewer_core::SessionOrigin::Interactive,
            title: "elapsed".into(),
            cwd: "/tmp/agent-viewer-elapsed".into(),
            git_branch: None,
            status: Status::Working,
            created_at_ms,
            updated_at_ms,
            hidden: false,
            companion: false,
            summary: String::new(),
            pid: None,
            rollout_path: None,
            pr_refs: Vec::new(),
            daemon_hosted: false,
        };
        let render_elapsed = |app: &App, now_ms| {
            let pulses = Pulses::new();
            let pr_status = crate::pr_cache::PrStatusCache::default();
            let mut terminal = Terminal::new(TestBackend::new(80, 2)).unwrap();
            terminal
                .draw(|frame| {
                    draw_list(
                        frame,
                        app,
                        &pulses,
                        now_ms,
                        &pr_status,
                        ListDeco { rename: None },
                        None,
                        Rect::new(0, 0, 80, 2),
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            (0..2)
                .flat_map(|y| (0..80).map(move |x| buffer[(x, y)].symbol()))
                .collect::<String>()
        };

        let mut app = App::new(vec![session(1_000, 91_000)]);
        assert!(render_elapsed(&app, 121_000).contains("Working 2m"));

        app.set_sessions(vec![session(1_000, 111_000)]);
        assert!(render_elapsed(&app, 121_000).contains("Working 2m"));

        let fallback = App::new(vec![session(0, 90_000)]);
        assert!(render_elapsed(&fallback, 120_000).contains("Working 30s"));
    }

    #[test]
    fn list_hit_row_at_reverses_geometry() {
        // Area at (x=0, y=2), 40 wide x 5 tall; no scroll. Five rendered lines: two rows, an
        // expansion line, another row, a trailing expansion line.
        let hit = ListHit {
            area: Rect::new(0, 2, 40, 5),
            offset: 0,
            item_to_row: vec![Some(0), Some(1), None, Some(2), None],
            blocked: None,
        };
        // Each in-area line maps to its item's target; expansion lines map to None.
        assert_eq!(hit.row_at(0, 2), Some(0)); // first viewport line
        assert_eq!(hit.row_at(39, 3), Some(1)); // right edge still inside
        assert_eq!(hit.row_at(0, 4), None); // expansion line
        assert_eq!(hit.row_at(0, 5), Some(2));
        assert_eq!(hit.row_at(0, 6), None); // trailing expansion line
        // Above the list area (header rows) is out of bounds.
        assert_eq!(hit.row_at(0, 1), None);
        assert_eq!(hit.row_at(0, 0), None);
        // Below the area (y >= 2 + 5) is out of bounds.
        assert_eq!(hit.row_at(0, 7), None);
        // Past the right edge (x >= 0 + 40) is out of bounds.
        assert_eq!(hit.row_at(40, 2), None);
    }

    #[test]
    fn list_hit_row_at_applies_scroll_offset() {
        // Viewport of 3 lines showing items 2,3,4 (offset 2). A screen row maps through the
        // offset back to the underlying item.
        let hit = ListHit {
            area: Rect::new(0, 2, 40, 3),
            offset: 2,
            item_to_row: vec![Some(0), Some(1), Some(2), Some(3), Some(4)],
            blocked: None,
        };
        assert_eq!(hit.row_at(0, 2), Some(2));
        assert_eq!(hit.row_at(0, 3), Some(3));
        assert_eq!(hit.row_at(0, 4), Some(4));
        assert_eq!(hit.row_at(0, 5), None); // outside the 3-tall viewport
    }

    #[test]
    fn list_hit_row_at_handles_empty_and_blank_tail() {
        // A zero-size area (the default, before any frame is drawn) never hits.
        assert_eq!(ListHit::default().row_at(0, 0), None);
        // An in-area cell below the last rendered item (blank tail) maps to no row.
        let hit = ListHit {
            area: Rect::new(0, 2, 40, 5),
            offset: 0,
            item_to_row: vec![Some(0), Some(1)],
            blocked: None,
        };
        assert_eq!(hit.row_at(0, 3), Some(1));
        assert_eq!(hit.row_at(0, 4), None); // item index 2 is past the rendered items
    }

    #[test]
    fn list_hit_row_at_treats_popup_overlay_as_a_hole() {
        // The slash popup floats over the bottom rows of the list; a cell inside it belongs to
        // the popup, so hit-testing must not select the obscured row underneath.
        let hit = ListHit {
            area: Rect::new(0, 2, 40, 6),
            offset: 0,
            item_to_row: vec![Some(0), Some(1), Some(2), Some(3), Some(4), Some(5)],
            blocked: Some(Rect::new(0, 6, 40, 2)), // shadows screen rows 6 and 7
        };
        // Rows above the popup still select normally.
        assert_eq!(hit.row_at(0, 2), Some(0));
        assert_eq!(hit.row_at(0, 5), Some(3));
        // Cells inside the popup rectangle are holes, even though a list row is drawn there.
        assert_eq!(hit.row_at(0, 6), None);
        assert_eq!(hit.row_at(39, 7), None);
    }

    #[test]
    fn slash_popup_area_matches_suggestion_count() {
        // No suggestions -> no popup, so nothing is blocked.
        let mut composer = Composer::new();
        let below = Rect::new(0, 20, 40, 3); // composer box well below the top
        assert_eq!(slash_popup_area(&composer, below), None);
        // A bare "/" matches every installed command, so the popup shows one row per command.
        composer.set_commands(
            vec!["review".to_string(), "security-review".to_string()],
            (BackendKind::Claude, None),
        );
        composer.push_char('/');
        let n = composer.suggestions().len() as u16;
        assert_eq!(n, 2);
        let area = slash_popup_area(&composer, below).expect("popup area");
        assert_eq!(area.height, n);
        assert_eq!(area.y, below.y - n); // sits just above the composer box
        assert_eq!(area.width, below.width);
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

    #[test]
    fn wrap_by_width_splits_first_line_shorter_than_the_rest() {
        // First line budget 4, rest budget 6. One unbroken word has no space to break at, so
        // it still hard splits: "abcdefghij" -> "abcd" | "efghij".
        assert_eq!(wrap_by_width("abcdefghij", 4, 6), vec!["abcd", "efghij"]);
        // Fits on one line -> one segment.
        assert_eq!(wrap_by_width("abc", 10, 10), vec!["abc"]);
        // Empty text -> one empty segment (callers rely on .last()).
        assert_eq!(wrap_by_width("", 5, 5), vec![""]);
    }

    #[test]
    fn wrap_by_width_counts_wide_glyphs_by_display_width() {
        // Each CJK glyph is two columns wide, so only two fit in a 5-wide budget.
        assert_eq!(wrap_by_width("一二三", 5, 5), vec!["一二", "三"]);
    }

    #[test]
    fn wrap_by_width_breaks_between_words_not_mid_word() {
        // "hello world" in 8 columns breaks at the space rather than after "hello wo".
        assert_eq!(wrap_by_width("hello world", 8, 8), vec!["hello", "world"]);
        // Several short words pack greedily up to the budget.
        assert_eq!(
            wrap_by_width("the quick brown fox", 10, 10),
            vec!["the quick", "brown fox"]
        );
        // A word that fits exactly still takes the whole line, with the next word below it.
        assert_eq!(wrap_by_width("abcde fg", 5, 5), vec!["abcde", "fg"]);
    }

    #[test]
    fn wrap_by_width_drops_the_space_a_line_breaks_at() {
        // The space between the words is eaten by the break: no wrapped line opens with it.
        for segment in wrap_by_width("alpha beta gamma delta", 11, 11) {
            assert!(
                !segment.starts_with(' '),
                "wrapped line opens with a space: {segment:?}"
            );
        }
        // A whole run of spaces at the break point is dropped, not just one.
        assert_eq!(wrap_by_width("alpha     beta", 7, 7), vec!["alpha", "beta"]);
    }

    #[test]
    fn wrap_by_width_hard_breaks_a_word_longer_than_a_line() {
        // No break opportunity inside the long word, so it splits at the budget and the
        // short word that follows still wraps whole.
        assert_eq!(
            wrap_by_width("ab supercalifragilistic cd", 6, 6),
            vec!["ab", "superc", "alifra", "gilist", "ic cd"]
        );
    }

    #[test]
    fn wrap_by_width_keeps_leading_and_trailing_spaces_the_user_typed() {
        // Indentation at the start of a line is content, not a break artifact.
        assert_eq!(wrap_by_width("  indented", 12, 12), vec!["  indented"]);
        // A trailing space stays on the line so the cursor sits after it.
        assert_eq!(wrap_by_width("hi ", 12, 12), vec!["hi "]);
    }

    #[test]
    fn wrap_by_width_preserves_explicit_newlines_and_blank_lines() {
        assert_eq!(
            wrap_by_width("one\n\ntwo three", 9, 9),
            vec!["one", "", "two three"]
        );
    }

    #[test]
    fn input_box_height_grows_with_wrapped_lines_and_caps() {
        // Empty text keeps the resting 3-row box regardless of width.
        assert_eq!(input_box_height(5, "", 40), 3);
        // A zero-width area never grows.
        assert_eq!(input_box_height(5, "anything", 0), 3);
        // One wrapped line -> 1 text row + 2 borders. Prefix 2, width 12 -> first budget 10.
        assert_eq!(input_box_height(2, "0123456789", 12), 3);
        // Prefix 2, width 12 (first budget 10, rest 12), 22 chars -> 2 rows -> height 4.
        assert_eq!(input_box_height(2, &"x".repeat(22), 12), 4);
        // Runaway input is capped at COMPOSER_MAX_LINES text rows + 2 borders.
        let huge = "y".repeat(10_000);
        assert_eq!(input_box_height(0, &huge, 10), COMPOSER_MAX_LINES + 2);
    }

    /// Render `draw_input_box` into an in-memory `TestBackend` buffer and return the per-row
    /// text plus the final cursor position — the actual render path, no terminal or pty.
    fn render_input_box(
        w: u16,
        h: u16,
        prefix: &'static str,
        prefix_w: usize,
        text: &str,
    ) -> (Vec<String>, (u16, u16)) {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| {
            let spans = vec![Span::styled(prefix.to_string(), fg(theme::ACCENT))];
            draw_input_box(
                f,
                Rect::new(0, 0, w, h),
                InputBox {
                    border: theme::FAINT,
                    prefix_spans: spans,
                    prefix_w,
                    placeholder: "placeholder",
                },
                text,
                true,
            );
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        let rows: Vec<String> = (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        let pos = term.get_cursor_position().unwrap();
        (rows, (pos.x, pos.y))
    }

    fn render_viewer(w: u16, h: u16, text: &str, mode: Mode) -> (Vec<String>, (u16, u16)) {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let app = App::new(Vec::new());
        let mut composer = Composer::new();
        for ch in text.chars() {
            composer.push_char(ch);
        }
        let pulses = Pulses::new();
        let pr_status = crate::pr_cache::PrStatusCache::new();
        let list_hit = RefCell::new(ListHit::default());
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|frame| {
            draw(
                frame,
                Draw {
                    app: &app,
                    workspace: Path::new("/tmp"),
                    mode: &mode,
                    notice: "",
                    composer: &composer,
                    pulses: &pulses,
                    now_ms: 0,
                    attach: None,
                    pr_status: &pr_status,
                    logos: None,
                    list_hit: &list_hit,
                },
            );
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        let rows = (0..h)
            .map(|y| {
                let mut continuation_cells = 0;
                (0..w)
                    .filter_map(|x| {
                        if continuation_cells > 0 {
                            continuation_cells -= 1;
                            return None;
                        }

                        let symbol = buf[(x, y)].symbol();
                        continuation_cells = display_width(symbol).saturating_sub(1);
                        Some(symbol)
                    })
                    .collect()
            })
            .collect();
        let pos = term.get_cursor_position().unwrap();
        (rows, (pos.x, pos.y))
    }

    #[test]
    fn help_popup_shows_modifier_shortcuts_at_80_by_24() {
        let (rows, _) = render_viewer(80, 24, "", Mode::Help);
        let rendered = rows.concat();

        for shortcut in ["Ctrl+A", "Ctrl+D", "Ctrl+U", "Ctrl+C"] {
            assert!(rendered.contains(shortcut), "missing {shortcut}");
        }
    }

    fn composer_bounds(rows: &[String]) -> (usize, usize) {
        let top = rows
            .iter()
            .position(|row| row.starts_with('╭'))
            .expect("composer top border");
        let bottom = rows
            .iter()
            .enumerate()
            .skip(top + 1)
            .find_map(|(index, row)| row.starts_with('╰').then_some(index))
            .expect("composer bottom border");
        (top, bottom)
    }

    #[test]
    fn composer_renders_metadata_above_a_full_width_input_row() {
        let target = "/tmp/spawn-dir";
        let app = App::new(vec![Session {
            backend: BackendKind::Claude,
            id: "metadata".into(),
            short_id: None,
            origin: agent_viewer_core::SessionOrigin::Interactive,
            title: "metadata".into(),
            cwd: target.into(),
            git_branch: None,
            status: Status::Done,
            created_at_ms: 0,
            updated_at_ms: 0,
            hidden: false,
            companion: false,
            summary: String::new(),
            pid: None,
            rollout_path: None,
            pr_refs: Vec::new(),
            daemon_hosted: false,
        }]);
        let mut composer = Composer::new();
        composer.cycle_backend();
        composer.set_models(
            vec!["default".into(), "gpt-5.3-codex".into()],
            BackendKind::Codex,
        );
        composer.cycle_model();
        for ch in "hello".chars() {
            composer.push_char(ch);
        }
        let pulses = Pulses::new();
        let pr_status = crate::pr_cache::PrStatusCache::new();
        let list_hit = RefCell::new(ListHit::default());
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut term = Terminal::new(TestBackend::new(50, 16)).unwrap();
        term.draw(|frame| {
            draw(
                frame,
                Draw {
                    app: &app,
                    workspace: Path::new("/tmp"),
                    mode: &Mode::Normal,
                    notice: "",
                    composer: &composer,
                    pulses: &pulses,
                    now_ms: 0,
                    attach: None,
                    pr_status: &pr_status,
                    logos: None,
                    list_hit: &list_hit,
                },
            );
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        let rows: Vec<String> = (0..16)
            .map(|y| (0..50).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        let cursor = term.get_cursor_position().unwrap();
        let (top, bottom) = composer_bounds(&rows);

        assert_eq!(bottom - top + 1, 4);
        let metadata = &rows[top + 1];
        assert!(metadata.contains("codex"));
        assert!(metadata.contains("gpt-5.3-codex"));
        assert!(metadata.contains(target));
        assert!(!metadata.contains("hello"));
        assert!(
            metadata.starts_with(&format!("│{}codex", backend_mark(BackendKind::Codex))),
            "compact metadata: {metadata:?}"
        );
        // The input begins immediately inside the border, leaving no horizontal padding.
        assert_eq!(&rows[top + 2], &format!("│hello{}│", " ".repeat(43)));
        assert_eq!((cursor.x, cursor.y), (6, (top + 2) as u16));
    }

    #[test]
    fn composer_exact_width_input_adds_a_cursor_continuation_row() {
        // A 12 column frame leaves 10 input columns after the borders, so 10 chars fill the
        // row exactly and the cursor needs a continuation row below it.
        let (rows, cursor) = render_viewer(12, 18, "0123456789", Mode::Normal);
        let (top, bottom) = composer_bounds(&rows);

        assert_eq!(bottom - top + 1, 5);
        assert_eq!(&rows[top + 2], "│0123456789│");
        assert_eq!(&rows[top + 3], "│          │");
        assert_eq!(cursor, (1, (top + 3) as u16));
    }

    #[test]
    fn composer_preserves_blank_lines_and_wraps_unicode_at_full_width() {
        let text = "abcdef界gh\n\nijklmnopqrst";
        let (rows, cursor) = render_viewer(12, 18, text, Mode::Normal);
        let (top, bottom) = composer_bounds(&rows);

        // 10 input columns: "abcdef界gh" fills the first row, and the blank line survives.
        assert_eq!(bottom - top + 1, 7);
        assert_eq!(&rows[top + 2], "│abcdef界gh│");
        assert_eq!(&rows[top + 3], "│          │");
        assert_eq!(&rows[top + 4], "│ijklmnopqr│");
        assert_eq!(&rows[top + 5], "│st        │");
        assert_eq!(cursor, (3, (top + 5) as u16));
    }

    #[test]
    fn composer_content_starts_in_the_compact_list_mark_column() {
        // A list row is `<status glyph><backend mark><title>`, so the mark slot opens at
        // column 1. Composer metadata and input use that same column immediately inside their
        // border. Marks are process-global mode, so this asserts measured columns, not glyphs.
        let app = App::new(vec![Session {
            backend: BackendKind::Claude,
            id: "aligned".into(),
            short_id: None,
            origin: agent_viewer_core::SessionOrigin::Interactive,
            title: "aligned-session".into(),
            cwd: "/tmp/aligned".into(),
            git_branch: None,
            status: Status::Done,
            created_at_ms: 0,
            updated_at_ms: 0,
            hidden: false,
            companion: false,
            summary: String::new(),
            pid: None,
            rollout_path: None,
            pr_refs: Vec::new(),
            daemon_hosted: false,
        }]);
        let mut composer = Composer::new();
        for ch in "typed".chars() {
            composer.push_char(ch);
        }
        let pulses = Pulses::new();
        let pr_status = crate::pr_cache::PrStatusCache::new();
        let list_hit = RefCell::new(ListHit::default());
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
        term.draw(|frame| {
            draw(
                frame,
                Draw {
                    app: &app,
                    workspace: Path::new("/tmp"),
                    mode: &Mode::Normal,
                    notice: "",
                    composer: &composer,
                    pulses: &pulses,
                    now_ms: 0,
                    attach: None,
                    pr_status: &pr_status,
                    logos: None,
                    list_hit: &list_hit,
                },
            );
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        let rows: Vec<String> = (0..20)
            .map(|y| (0..60).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        let (top, _) = composer_bounds(&rows);

        let list_y = rows
            .iter()
            .position(|row| row.contains("aligned-session"))
            .expect("session row");
        let mark_width = mark_width(backend_mark(BackendKind::Claude)) as u16;
        // The list title begins directly after the status glyph and complete mark slot.
        assert_ne!(buf[(0, list_y as u16)].symbol(), " ");
        assert_eq!(buf[(1 + mark_width, list_y as u16)].symbol(), "a");
        // Metadata begins with the mark in column 1 and its backend name follows the mark.
        assert_eq!(buf[(1 + mark_width, (top + 1) as u16)].symbol(), "c");
        // The task input also starts at column 1, directly inside its border.
        assert_eq!(buf[(1, (top + 2) as u16)].symbol(), "t");
    }

    #[test]
    fn composer_wraps_the_input_at_word_boundaries() {
        // A 20 column frame leaves 18 input columns. The text breaks after "wrap" rather than
        // splitting "boundaries", and no wrapped row opens with the eaten space.
        let (rows, _) = render_viewer(20, 24, "wrap at word boundaries here", Mode::Normal);
        let (top, _) = composer_bounds(&rows);

        assert_eq!(&rows[top + 2], "│wrap at word      │");
        assert_eq!(&rows[top + 3], "│boundaries here   │");
    }

    #[test]
    fn composer_height_cap_keeps_metadata_and_the_input_tail_visible() {
        let text = (0..15)
            .map(|index| format!("line{index:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (rows, cursor) = render_viewer(20, 24, &text, Mode::Normal);
        let (top, bottom) = composer_bounds(&rows);

        assert_eq!(bottom - top + 1, 13);
        assert!(rows[top + 1].contains("claude"));
        assert!(rows[top + 2].contains("line05"));
        assert!(rows[bottom - 1].contains("line14"));
        assert_eq!(cursor, (7, (bottom - 1) as u16));
    }

    #[test]
    fn input_box_wraps_long_text_down_the_rows_with_cursor_on_the_last() {
        // Inner width 8 (10 - 2 borders), prompt "> " (width 2): first budget 6, rest 8.
        // "abcdefgh" has no space to break at, so it hard splits "abcdef" | "gh".
        let (rows, cursor) = render_input_box(10, 4, "> ", 2, "abcdefgh");
        assert!(rows[0].starts_with('╭'), "top border: {:?}", rows[0]);
        assert!(rows[3].starts_with('╰'), "bottom border: {:?}", rows[3]);
        // Row 1 begins at the border followed by the prompt and first 6 chars. Row 2 has no
        // prompt and retains the full available inner width.
        assert_eq!(&rows[1], "│> abcdef│");
        assert_eq!(&rows[2], "│gh      │");
        // Cursor sits at the end of the text on the second inner row (y == 2), not off-screen
        // to the right: inner.x (1) + 2 typed chars = col 3.
        assert_eq!(cursor, (3, 2));
    }

    #[test]
    fn input_box_short_text_stays_on_one_row() {
        // Short text does not wrap: single inner row, cursor right after it on row 1.
        let (rows, cursor) = render_input_box(12, 3, "> ", 2, "hi");
        assert_eq!(&rows[1], "│> hi      │");
        assert_eq!(cursor, (5, 1));
    }
}
