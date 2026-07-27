//! Rendering surface for the session list, composer, overlays, and attached terminal.

mod attach;
mod composer;
mod header;
mod list;
mod overlay;
mod palette;
pub mod theme;

use crate::app::{App, Composer, Row};
use crate::logos::LogoMarks;
use agent_viewer_core::pty::PtySession;
use agent_viewer_core::{BackendKind, Session, Status};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use ratatui_image::Image;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

#[cfg(test)]
use composer::{InputBox, MAX_LINES as COMPOSER_MAX_LINES, draw_input_box, wrap_by_width};
use composer::{box_height as composer_box_height, input_box_height, input_inner_width};
use list::{pr_badge, rename_buffer, rename_row_item, row_to_item, status_display_word};
pub use palette::{PaletteAction, PaletteGroup, PaletteItem, PaletteState, PaletteTarget};
pub use theme::{Theme, ThemeState};

/// A live spawn-bloom one-shot, keyed by session, holding the ms it started (now_ms).
pub type Pulses = HashMap<(BackendKind, String), i64>;

/// Startup-read (once, never per-frame): when true, list rows + composer use the brand
/// glyphs (✳/◆/■) instead of the DEFAULT textual `[cc]`/`[cx]`/`[oc]` tags — an opt-in for
/// terminals whose font renders them. Set from `AGENT_VIEWER_GLYPH_MARKS=1`.
static GLYPH_MARKS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Enable the brand-glyph marks once at startup (idempotent; later calls are ignored).
pub fn set_glyph_marks(on: bool) {
    let _ = GLYPH_MARKS.set(on);
}

pub fn glyph_marks() -> bool {
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
fn backend_mark(backend: BackendKind, theme: &Theme) -> &'static str {
    // Logo mode blanks the slot (two reserved columns) for the image overlay; it wins over
    // glyph mode. Two spaces keep `mark_width` == 2 so every row/composer layout math holds.
    if logo_marks() {
        return "  ";
    }
    mark_for(backend, theme.mark_set == theme::MarkSet::Glyph)
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
fn backend_mark_color(backend: BackendKind, theme: &Theme) -> ratatui::style::Color {
    match backend {
        BackendKind::Claude => theme.cc,
        BackendKind::Codex => theme.cx,
        BackendKind::Opencode => theme.oc,
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
    (pos * 4 / (half + 1)) as usize % 4
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
fn status_glyph(
    status: &Status,
    now_ms: i64,
    theme: &Theme,
) -> (&'static str, ratatui::style::Color) {
    match status {
        Status::Working => (
            if theme.animation {
                shimmer_glyph(now_ms)
            } else {
                "✽"
            },
            theme.accent,
        ),
        Status::NeedsInput { .. } => {
            let steps = [
                theme.muted,
                theme.pulse_start,
                theme.accent,
                theme.pulse_end,
            ];
            (
                "◐",
                if theme.animation {
                    steps[breath_phase(now_ms)]
                } else {
                    theme.warn
                },
            )
        }
        Status::Idle => ("∙", theme.muted),
        Status::Done => ("●", theme.ok),
        Status::Error => ("✗", theme.err),
        Status::Unknown => ("?", theme.faint),
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
    Palette(PaletteState),
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
    pub themes: &'a ThemeState,
}

pub fn draw(frame: &mut Frame, d: Draw) {
    let theme = d.themes.active();
    // Paint the whole surface with the base background first.
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.bg).fg(theme.text)),
        frame.area(),
    );

    if let Some(av) = d.attach {
        attach::draw(frame, av, d.now_ms, theme);
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

    header::draw(frame, d.app, d.workspace, theme, vertical[0]);
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
        theme,
        vertical[1],
    );
    if matches!(d.mode, Mode::Normal) {
        hit.blocked = overlay::popup_area(d.composer, d.themes, vertical[3]);
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
        composer::draw_reply(frame, m, theme, vertical[3], &title);
    } else {
        composer::draw(
            frame,
            d.app,
            d.composer,
            d.logos,
            theme,
            vertical[3],
            matches!(d.mode, Mode::Normal),
        );
    }
    draw_footer(frame, d.app, d.mode, d.notice, d.now_ms, theme, vertical[4]);

    // Completion popup floating just above the composer box: the /model picker when a /model
    // command is being typed, else the slash-command popup.
    if matches!(d.mode, Mode::Normal) {
        let highlight = d.composer.suggestion_highlight();
        if d.themes.picker_open() {
            overlay::draw_theme_picker(frame, d.themes, vertical[3]);
        } else if d.composer.is_model_command() {
            overlay::draw_suggestions(
                frame,
                &d.composer.model_suggestions(),
                highlight,
                "",
                theme,
                vertical[3],
            );
        } else {
            overlay::draw_suggestions(
                frame,
                &d.composer.suggestions(),
                highlight,
                "/",
                theme,
                vertical[3],
            );
        }
    }

    if matches!(d.mode, Mode::Help) {
        overlay::draw_help(frame, theme, frame.area());
    }
    if let Mode::Palette(state) = d.mode {
        palette::draw(frame, state, d.now_ms, theme);
    }
}

/// Per-row decorations layered over the list model.
struct ListDeco<'a> {
    rename: Option<(BackendKind, &'a str, &'a str)>,
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
    theme: &Theme,
    area: Rect,
) -> ListHit {
    let width = area.width as usize;
    let rows = app.visible();
    let desired_title_width = rows
        .iter()
        .filter_map(|row| match row {
            Row::Session { title, .. } => Some(display_width(title)),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        .min(40);
    // Resolve narrow viewport degradation once for the whole visible list. The smallest
    // viable row width becomes the shared width, so individual summaries cannot move status.
    let title_width = rows
        .iter()
        .filter_map(|row| match row {
            Row::Session {
                backend,
                title,
                summary,
                status,
                created_at_ms,
                updated_at_ms,
                pr_refs,
                ..
            } => {
                let started_at_ms = if *created_at_ms > 0 {
                    *created_at_ms
                } else {
                    *updated_at_ms
                };
                let elapsed = crate::app::format_elapsed(now_ms - started_at_ms);
                let pr = pr_badge(pr_refs);
                let (visible_title, _, _, _, _) = crate::app::row_layout(
                    width,
                    mark_width(backend_mark(*backend, theme)),
                    title,
                    desired_title_width,
                    status_display_word(status),
                    &pr,
                    summary,
                    display_width(&elapsed),
                );
                Some(display_width(&visible_title))
            }
            _ => None,
        })
        .min()
        .unwrap_or(desired_title_width);
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
        .map(|(backend, _, buffer)| rename_buffer(backend, buffer, width, theme));
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
                        theme,
                    ));
                    item_backends.push(Some(*backend));
                } else {
                    items.push(row_to_item(
                        row,
                        pulses,
                        now_ms,
                        pr_status,
                        width,
                        title_width,
                        theme,
                    ));
                    item_backends.push(Some(*backend));
                }
                item_to_row.push(target);
            }
            _ => {
                items.push(row_to_item(
                    row,
                    pulses,
                    now_ms,
                    pr_status,
                    width,
                    title_width,
                    theme,
                ));
                item_backends.push(None);
                item_to_row.push(target);
            }
        }
    }
    let list = List::new(items).highlight_style(Style::default().bg(theme.selbg));
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
            let prefix_width = display_width("✎") + mark_width(backend_mark(rb, theme));
            let shown = rename_shown.as_deref().unwrap_or_default();
            let col = prefix_width + display_width(shown);
            let x = area.x + (col as u16).min(area.width.saturating_sub(1));
            frame.set_cursor_position((x, y));
        }
    }

    hit
}

// --- Footer ---------------------------------------------------------------------

fn draw_footer(
    frame: &mut Frame,
    app: &App,
    mode: &Mode,
    notice: &str,
    now_ms: i64,
    theme: &Theme,
    area: Rect,
) {
    let line = match mode {
        Mode::Filter => Line::from(format!("/{}", app.filter())),
        Mode::Palette(_) => Line::from(""),
        Mode::Rename(_) => Line::from("rename in row — Enter apply · Esc cancel"),
        Mode::Reply(_) => Line::from("reply — Enter send · Esc cancel"),
        Mode::Help => Line::from("help — Esc/? to close"),
        Mode::Attached => Line::from(""),
        Mode::Normal => {
            if !notice.is_empty() {
                Line::from(Span::styled(notice.to_string(), fg(theme.warn)))
            } else if app.is_armed(now_ms) {
                Line::from(Span::styled(
                    "[press Ctrl+X again to remove]",
                    fg(theme.err),
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
                        "{hidden_txt}{showing}type task · Ctrl+K palette · Tab agent · ⇧Tab model · /model pick · Enter spawn/attach · Space group header · Ctrl+R rename · Ctrl+X stop/remove · Ctrl+S group · Ctrl+A all · Ctrl+D archive · Ctrl+U unarchive · Ctrl+F filter · ? help · Ctrl+C quit"
                    ),
                    fg(theme.muted),
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
        let theme = theme::amber(false);
        for b in [
            BackendKind::Claude,
            BackendKind::Codex,
            BackendKind::Opencode,
        ] {
            assert_eq!(backend_mark(b, &theme), "  ");
            assert_eq!(mark_width(backend_mark(b, &theme)), 2);
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
        let theme = theme::amber(false);
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
                    &theme,
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
        let theme = theme::amber(false);
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
                    &theme,
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

        assert_eq!(title.fg, theme.text);
        assert_eq!(summary.fg, theme.muted);
        assert_eq!(title.bg, theme.selbg);
        assert_eq!(summary.bg, theme.selbg);
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
            let theme = theme::amber(false);
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
                        &theme,
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
        let rendered = render_elapsed(&app, 121_000);
        assert!(rendered.contains("Working"));
        assert!(rendered.trim_end().ends_with("2m"));
        assert!(rendered.find("Working") < rendered.rfind("2m"));

        app.set_sessions(vec![session(1_000, 111_000)]);
        let rendered = render_elapsed(&app, 121_000);
        assert!(rendered.contains("Working"));
        assert!(rendered.trim_end().ends_with("2m"));
        assert!(rendered.find("Working") < rendered.rfind("2m"));

        let fallback = App::new(vec![session(0, 90_000)]);
        let rendered = render_elapsed(&fallback, 120_000);
        assert!(rendered.contains("Working"));
        assert!(rendered.trim_end().ends_with("30s"));
        assert!(rendered.find("Working") < rendered.rfind("30s"));
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
        let themes = ThemeState::default();
        let below = Rect::new(0, 20, 40, 3); // composer box well below the top
        assert_eq!(overlay::popup_area(&composer, &themes, below), None);
        // A bare "/" matches every installed command, so the popup shows one row per command.
        composer.set_commands(
            vec!["review".to_string(), "security-review".to_string()],
            (BackendKind::Claude, None),
        );
        composer.push_char('/');
        let n = composer.suggestions().len() as u16;
        assert_eq!(n, 2);
        let area = overlay::popup_area(&composer, &themes, below).expect("popup area");
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
            assert!(breath_phase(ms) < 4);
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
        let palette = theme::amber(false);
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| {
            let spans = vec![Span::styled(prefix.to_string(), fg(palette.accent))];
            draw_input_box(
                f,
                Rect::new(0, 0, w, h),
                InputBox {
                    border: palette.faint,
                    prefix_spans: spans,
                    prefix_width: prefix_w,
                    placeholder: "placeholder",
                },
                text,
                true,
                &palette,
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
        let themes = ThemeState::default();
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
                    themes: &themes,
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
        let themes = ThemeState::default();
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
                    themes: &themes,
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
            metadata.starts_with(&format!(
                "│{}codex",
                backend_mark(BackendKind::Codex, themes.active())
            )),
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
        let themes = ThemeState::default();
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
                    themes: &themes,
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
        let mark_width = mark_width(backend_mark(BackendKind::Claude, themes.active())) as u16;
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
