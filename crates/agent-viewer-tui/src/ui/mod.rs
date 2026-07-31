//! Rendering surface for the session list, composer, overlays, and attached terminal.

pub mod age;
mod attach;
mod composer;
mod header;
mod list;
mod overlay;
mod palette;
mod sprite;
mod tail;
pub mod theme;
mod triage;
pub mod wall;

use crate::app::{App, Composer, Row};
use crate::logos::LogoMarks;
use agent_viewer_core::pty::PtySession;
use agent_viewer_core::{BackendKind, Session, Status};
pub use attach::ATTACHED_CHROME_ROWS;
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
use composer::{
    box_height_for_viewport as composer_box_height, caret_visible, input_box_height,
    input_inner_width,
};
pub use list::activity_ribbon;
use list::{rename_buffer, rename_row_item, row_to_item};
pub use palette::{PaletteAction, PaletteGroup, PaletteItem, PaletteState, PaletteTarget};
pub use sprite::SpriteKind;
pub use tail::{TAIL_EVENTS, TAIL_MIN_TOTAL_WIDTH, TailView};
pub use theme::{Theme, ThemeState};
pub use triage::{TriageItem, TriageLayout, TriageState, panel_pty_size, triage_queue};
pub use wall::{WallState, WallTile, WallView};

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

/// Startup-read (once): when true, the mark slot is left blank with one cell around the
/// two cell brand-logo image overlaid by the render path. Set after the terminal-graphics
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
    // Logo mode blanks the slot for the image overlay; it wins over glyph mode.
    if logo_marks() {
        return "    ";
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

/// PURE spawn bloom: the one-shot glyph for `elapsed_ms` since the row appeared, or None
/// once the ~400ms bloom has finished.
pub fn bloom_glyph(elapsed_ms: i64) -> Option<&'static str> {
    if !(0..400).contains(&elapsed_ms) {
        return None;
    }
    let i = (elapsed_ms / 100) as usize;
    Some(BLOOM[i.min(BLOOM.len() - 1)])
}

/// Six-state glyph + color. Working shimmers its glyph (~120ms/frame).
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
        // `attn`, not `warn`: one hue means "waiting on you" everywhere the viewer says it,
        // and the wall's flashing border is the same colour as this glyph.
        Status::NeedsInput { .. } => ("◐", theme.attn),
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
    /// The Ctrl+N triage inbox: a modal walk over the needs-input queue, drawn over the list.
    Triage(TriageState),
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
    /// The width the list occupied last frame, which is what the tail pane's width gate
    /// asks about. Zero before the first draw, i.e. "not measured yet", never "too narrow".
    pub fn width(&self) -> u16 {
        self.area.width
    }

    pub fn rendered_range(
        &self,
        row_count: usize,
        lookahead: usize,
    ) -> Option<std::ops::Range<usize>> {
        if self.area.width == 0 || self.area.height == 0 {
            return None;
        }
        let start = self.offset.min(row_count);
        let end = self
            .offset
            .saturating_add(self.area.height as usize)
            .saturating_add(lookahead)
            .min(row_count);
        Some(start..end)
    }

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
    /// Which header mascot to draw. Cycled live with Ctrl+G while the candidates are being
    /// compared.
    pub sprite: SpriteKind,
    /// Whether finished rows fade toward the theme's `faint` token as they age. Off by default,
    /// toggled from the command palette, and a no-op under a non-truecolor theme.
    pub age_ramp: bool,
    /// The Ctrl+B tail pane, present only while it is open and a session is selected. It
    /// takes its columns off the right of the list. Not drawn while the wall is up: it is a
    /// panel beside the list, and the wall replaces the list.
    pub tail: Option<TailView<'a>>,
    /// The video wall (Ctrl+W), when on. Present means the list region becomes a grid of
    /// live PTY tiles and the composer is not drawn at all (the focused tile owns the
    /// keyboard); the header and footer stay exactly where they were.
    pub wall: Option<WallView<'a>>,
    /// Sink for the rect of each tile this frame, in tile order. The run loop reads it back
    /// to size each tile's child (`PtySession::resize` needs `&mut` and draw is `&`-only) and
    /// to hit-test the pointer. Emptied on any frame without a wall. Same pattern as
    /// `list_hit`.
    pub wall_rects: &'a RefCell<Vec<Rect>>,
}

pub fn draw(frame: &mut Frame, d: Draw) {
    let theme = d.themes.active();
    // Paint the whole surface with the base background first.
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.bg).fg(theme.text)),
        frame.area(),
    );

    // Full-screen attach owns the whole frame. Triage also has a live child, but hosts it in
    // a panel inside its own chrome, so it falls through to the list draw below.
    if matches!(d.mode, Mode::Attached)
        && let Some(av) = &d.attach
    {
        attach::draw(frame, av, d.notice, d.now_ms, theme);
        return;
    }

    // The composer box keeps one metadata row above its wrapped input and grows to a cap so
    // a long task description stays visible. Reply mode sizes its prompt prefixed box to the
    // reply buffer.
    let inner_w = input_inner_width(frame.area().width);
    // The wall owns the keyboard, so the composer is not drawn while it is on: an input box
    // that cannot receive your input is a lie, and those rows are better spent on tiles.
    let composer_h = if d.wall.is_some() {
        0
    } else {
        match &d.mode {
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
            _ => composer_box_height(d.composer.text(), inner_w, frame.area().height),
        }
    };

    // header (blank gap + title/status + blank gaps) · list · blank gap · bordered composer box
    // (grows with wrapped input) · footer.
    let header_height = header::height(frame.area(), composer_h);
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
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

    header::draw(
        frame,
        d.app,
        d.workspace,
        d.now_ms,
        theme,
        d.sprite,
        vertical[0],
    );
    // The composer cursor blinks only in Normal mode (the composer is the active input);
    // the rename cursor is placed on the edit row by draw_list; Help/Filter show neither.
    // draw_list returns the frame's list geometry for mouse hit-testing; the slash popup (drawn
    // below, floating over the list's bottom) shadows part of it, so record that hole too.
    // The tail pane takes a fixed slice off the right of the list area; the list keeps the
    // rest and re-lays itself out at the narrower width. The wall stands the tail down: the
    // tail is a panel beside the list, the wall replaces the list outright, and every tile
    // already shows the live session the tail would be tailing.
    let (list_area, tail_area) = match &d.tail {
        Some(_) if d.wall.is_none() && vertical[1].width >= TAIL_MIN_TOTAL_WIDTH => {
            let split = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Min(1),
                    Constraint::Length(tail::tail_width(vertical[1].width)),
                ])
                .split(vertical[1]);
            (split[0], Some(split[1]))
        }
        _ => (vertical[1], None),
    };
    // The wall replaces the list region and nothing else, so it publishes that region for the
    // run loop's resize pass and leaves the list's mouse geometry empty (no rows to hit).
    let mut hit = match &d.wall {
        Some(view) => {
            *d.wall_rects.borrow_mut() = wall::draw(frame, view, d.now_ms, theme, vertical[1]);
            ListHit::default()
        }
        None => {
            d.wall_rects.borrow_mut().clear();
            draw_list(
                frame,
                d.app,
                d.pulses,
                d.now_ms,
                d.pr_status,
                deco,
                d.logos,
                theme,
                d.age_ramp,
                list_area,
            )
        }
    };
    if let (Some(view), Some(area)) = (&d.tail, tail_area) {
        tail::draw(frame, view, theme, area);
    }
    if matches!(d.mode, Mode::Normal) && d.wall.is_none() {
        hit.blocked = overlay::popup_area(d.composer, d.themes, vertical[3]);
    }
    *d.list_hit.borrow_mut() = hit;
    // Reply mode replaces the spawn composer with a small reply input. Every other mode shows
    // the persistent spawn composer.
    if d.wall.is_some() {
        // No composer on the wall: `composer_h` is zero, so there is nothing to draw into.
    } else if let Mode::Reply(m) = d.mode {
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
            matches!(d.mode, Mode::Normal) && caret_visible(d.now_ms, theme.animation),
        );
    }
    draw_footer(
        frame,
        d.app,
        d.mode,
        d.notice,
        d.now_ms,
        theme,
        d.wall.as_ref(),
        vertical[4],
    );

    // Completion popup floating just above the composer box: the /model picker when a /model
    // command is being typed, else the slash-command popup. Never on the wall — the composer
    // it belongs to is not on screen.
    if matches!(d.mode, Mode::Normal) && d.wall.is_none() {
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
    if let Mode::Triage(state) = d.mode {
        triage::draw(frame, state, d.attach.as_ref(), d.now_ms, theme);
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
    age_ramp: bool,
    area: Rect,
) -> ListHit {
    let width = area.width as usize;
    let show_activity = width >= 126;
    let project_width = 0;
    let layout_width = width.saturating_sub(project_width + usize::from(show_activity) * 10);
    let rows = app.visible();
    let elapsed_width = rows
        .iter()
        .filter_map(|row| match row {
            Row::Session {
                created_at_ms,
                updated_at_ms,
                ..
            } => {
                let started_at_ms = if *created_at_ms > 0 {
                    *created_at_ms
                } else {
                    *updated_at_ms
                };
                Some(display_width(&crate::app::format_elapsed(
                    now_ms - started_at_ms,
                )))
            }
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let desired_title_width = rows
        .iter()
        .filter_map(|row| match row {
            Row::Session { title, .. } => Some(display_width(title)),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        .min(40);
    let title_width = rows
        .iter()
        .filter_map(|row| match row {
            Row::Session {
                backend,
                title,
                summary,
                status,
                pr_refs,
                ..
            } => {
                let pr = list::pr_badge(pr_refs);
                let (visible_title, _, _, _, _) = crate::app::row_layout(
                    layout_width,
                    mark_width(backend_mark(*backend, theme)),
                    title,
                    desired_title_width,
                    list::status_display_word(status),
                    &pr,
                    summary,
                    elapsed_width,
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
                        show_activity,
                        project_width,
                        width,
                        title_width,
                        elapsed_width,
                        theme,
                        age_ramp,
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
                    show_activity,
                    project_width,
                    width,
                    title_width,
                    elapsed_width,
                    theme,
                    age_ramp,
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

    // Logo overlay: for each on-screen session item, draw its brand image over the middle two
    // mark columns. Only the visible
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

#[allow(clippy::too_many_arguments)]
fn draw_footer(
    frame: &mut Frame,
    app: &App,
    mode: &Mode,
    notice: &str,
    now_ms: i64,
    theme: &Theme,
    wall: Option<&WallView>,
    area: Rect,
) {
    // The wall owns the footer while it is on (except for a live notice), because its keys
    // differ from the list's and the overflow count has nowhere else to go.
    if let Some(view) = wall
        && matches!(mode, Mode::Normal)
        && notice.is_empty()
    {
        let overflow = if view.overflow > 0 {
            format!(
                "showing {} of {} · ",
                view.tiles.len(),
                view.tiles.len() + view.overflow
            )
        } else {
            String::new()
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(
                    "{overflow}wall · Shift+↑↓←→ move · wheel scrolls · Ctrl+O zoom · Ctrl+W exit"
                ),
                fg(theme.muted),
            ))),
            area,
        );
        return;
    }
    let line = match mode {
        Mode::Filter => Line::from(format!("/{}", app.filter())),
        Mode::Palette(_) => Line::from(""),
        Mode::Rename(_) => Line::from("rename in row — Enter apply · Esc cancel"),
        Mode::Reply(_) => Line::from("reply — Enter send · Esc cancel"),
        Mode::Triage(_) => Line::from(""),
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
                        "{hidden_txt}{showing}type task · Ctrl+K palette · Ctrl+N triage · Ctrl+G sprite · Tab agent · ⇧Tab model · /model pick · Enter spawn/attach · Space group header · Ctrl+R rename · Ctrl+X stop/remove · Ctrl+S group · Ctrl+A all · Ctrl+D archive · Ctrl+U unarchive · Ctrl+F filter · ? help · Ctrl+C quit"
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
    use std::time::{Duration, Instant};

    fn attached_test_session() -> Session {
        Session {
            backend: BackendKind::Claude,
            id: "attached-theme".into(),
            short_id: Some("attached-theme".into()),
            origin: agent_viewer_core::SessionOrigin::Interactive,
            title: "attached theme".into(),
            cwd: "/tmp".into(),
            git_branch: None,
            status: Status::Working,
            created_at_ms: 0,
            updated_at_ms: 0,
            hidden: false,
            companion: false,
            summary: String::new(),
            pid: None,
            rollout_path: None,
            pr_refs: Vec::new(),
            daemon_hosted: false,
        }
    }

    fn attached_pty(script: &str) -> PtySession {
        attached_pty_with_palette(script, None)
    }

    fn attached_pty_with_palette(
        script: &str,
        palette: Option<agent_viewer_core::pty::TerminalPalette>,
    ) -> PtySession {
        PtySession::spawn(agent_viewer_core::pty::PtySpec {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            cwd: None,
            envs: Vec::new(),
            rows: 6,
            cols: 40,
            palette,
            scrollback_rows: 0,
        })
        .expect("attached child")
    }

    fn wait_for_attached_screen(pty: &PtySession, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !pty.with_screen(|screen| screen.contents().contains(needle)) {
            assert!(
                Instant::now() < deadline,
                "attached child screen did not contain {needle:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn render_attached(pty: &PtySession, session: &Session) -> ratatui::buffer::Buffer {
        let themes = ThemeState::default();
        render_attached_with_themes(pty, session, &themes)
    }

    fn render_attached_with_themes(
        pty: &PtySession,
        session: &Session,
        themes: &ThemeState,
    ) -> ratatui::buffer::Buffer {
        let app = App::new(vec![session.clone()]);
        let composer = Composer::new();
        let pulses = Pulses::new();
        let pr_status = crate::pr_cache::PrStatusCache::new();
        let list_hit = RefCell::new(ListHit::default());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 8)).unwrap();

        terminal
            .draw(|frame| {
                draw(
                    frame,
                    Draw {
                        app: &app,
                        workspace: std::path::Path::new("/tmp"),
                        mode: &Mode::Attached,
                        notice: "",
                        composer: &composer,
                        pulses: &pulses,
                        now_ms: 0,
                        attach: Some(AttachView {
                            session,
                            pty,
                            exited: false,
                        }),
                        pr_status: &pr_status,
                        logos: None,
                        list_hit: &list_hit,
                        themes,
                        sprite: Default::default(),
                        age_ramp: false,
                        tail: None,
                        wall: None,
                        wall_rects: &RefCell::new(Vec::new()),
                    },
                );
            })
            .expect("draw attached child");
        terminal.backend().buffer().clone()
    }

    #[test]
    fn attached_plain_cells_use_the_stored_amber_palette_without_recoloring_explicit_ansi() {
        let theme = theme::amber(false);
        let mut pty = attached_pty_with_palette(
            "printf 'P\\033[31mI\\033[0m\\033[38;2;1;2;3;48;2;4;5;6mR\\033[0m\\033[?25l'; sleep 30",
            theme.terminal_palette(),
        );
        wait_for_attached_screen(&pty, "PIR");

        let session = attached_test_session();
        let buffer = render_attached(&pty, &session);

        assert_eq!(buffer[(0, 1)].symbol(), "P");
        assert_eq!(buffer[(0, 1)].fg, theme.text);
        assert_eq!(buffer[(0, 1)].bg, theme.bg);
        assert_eq!(buffer[(1, 1)].symbol(), "I");
        assert_eq!(buffer[(1, 1)].fg, ratatui::style::Color::Indexed(1));
        assert_eq!(buffer[(2, 1)].symbol(), "R");
        assert_eq!(buffer[(2, 1)].fg, ratatui::style::Color::Rgb(1, 2, 3));
        assert_eq!(buffer[(2, 1)].bg, ratatui::style::Color::Rgb(4, 5, 6));

        pty.kill();
    }

    #[test]
    fn refreshing_an_attached_palette_recolors_only_default_cells() {
        let old_palette = agent_viewer_core::pty::TerminalPalette {
            foreground: [0x12, 0x34, 0x56],
            background: [0x78, 0x9a, 0xbc],
        };
        let new_palette = agent_viewer_core::pty::TerminalPalette {
            foreground: [0xfe, 0xdc, 0xba],
            background: [0x65, 0x43, 0x21],
        };
        let mut pty = attached_pty_with_palette(
            "printf 'P\\033[38;5;1;48;5;4mI\\033[0m\\033[38;2;1;2;3;48;2;4;5;6mR\\033[0m\\033[?25l'; sleep 30",
            Some(old_palette),
        );
        wait_for_attached_screen(&pty, "PIR");
        let session = attached_test_session();

        let before = render_attached(&pty, &session);
        pty.set_palette(Some(new_palette));
        let after = render_attached(&pty, &session);

        assert_eq!(before[(0, 1)].symbol(), "P");
        assert_eq!(
            before[(0, 1)].fg,
            ratatui::style::Color::Rgb(0x12, 0x34, 0x56)
        );
        assert_eq!(
            before[(0, 1)].bg,
            ratatui::style::Color::Rgb(0x78, 0x9a, 0xbc)
        );
        assert_eq!(after[(0, 1)].symbol(), "P");
        assert_eq!(
            after[(0, 1)].fg,
            ratatui::style::Color::Rgb(0xfe, 0xdc, 0xba)
        );
        assert_eq!(
            after[(0, 1)].bg,
            ratatui::style::Color::Rgb(0x65, 0x43, 0x21)
        );

        assert_eq!(before[(1, 1)].symbol(), "I");
        assert_eq!(after[(1, 1)].symbol(), "I");
        assert_eq!(before[(1, 1)].fg, ratatui::style::Color::Indexed(1));
        assert_eq!(before[(1, 1)].bg, ratatui::style::Color::Indexed(4));
        assert_eq!(after[(1, 1)].fg, before[(1, 1)].fg);
        assert_eq!(after[(1, 1)].bg, before[(1, 1)].bg);

        assert_eq!(before[(2, 1)].symbol(), "R");
        assert_eq!(after[(2, 1)].symbol(), "R");
        assert_eq!(before[(2, 1)].fg, ratatui::style::Color::Rgb(1, 2, 3));
        assert_eq!(before[(2, 1)].bg, ratatui::style::Color::Rgb(4, 5, 6));
        assert_eq!(after[(2, 1)].fg, before[(2, 1)].fg);
        assert_eq!(after[(2, 1)].bg, before[(2, 1)].bg);

        pty.kill();
    }

    #[test]
    fn palette_free_attached_terminal_keeps_default_cells_and_empty_cursor_unstyled() {
        let mut pty = attached_pty("printf 'P'; sleep 30");
        wait_for_attached_screen(&pty, "P");

        let session = attached_test_session();
        let buffer = render_attached(&pty, &session);

        assert_eq!(buffer[(0, 1)].symbol(), "P");
        assert_eq!(buffer[(0, 1)].fg, ratatui::style::Color::Reset);
        assert_eq!(buffer[(0, 1)].bg, ratatui::style::Color::Reset);
        assert_eq!(buffer[(1, 1)].symbol(), "█");
        assert_eq!(buffer[(1, 1)].fg, ratatui::style::Color::Gray);
        assert_eq!(buffer[(1, 1)].bg, ratatui::style::Color::Reset);

        pty.kill();
    }

    #[test]
    fn attached_terminal_uses_its_stored_palette_without_recoloring_empty_cursor() {
        let palette = agent_viewer_core::pty::TerminalPalette {
            foreground: [0x12, 0x34, 0x56],
            background: [0x78, 0x9a, 0xbc],
        };
        let mut pty = attached_pty_with_palette("printf 'P'; sleep 30", Some(palette));
        wait_for_attached_screen(&pty, "P");

        let session = attached_test_session();
        let buffer = render_attached(&pty, &session);

        assert_eq!(buffer[(0, 1)].symbol(), "P");
        assert_eq!(
            buffer[(0, 1)].fg,
            ratatui::style::Color::Rgb(0x12, 0x34, 0x56)
        );
        assert_eq!(
            buffer[(0, 1)].bg,
            ratatui::style::Color::Rgb(0x78, 0x9a, 0xbc)
        );
        assert_eq!(buffer[(1, 1)].symbol(), "█");
        assert_eq!(buffer[(1, 1)].fg, ratatui::style::Color::Gray);
        assert_eq!(
            buffer[(1, 1)].bg,
            ratatui::style::Color::Rgb(0x78, 0x9a, 0xbc)
        );

        pty.kill();
    }

    #[test]
    fn terminal_match_renders_default_cells_with_the_stored_host_palette() {
        let host_palette = agent_viewer_core::pty::TerminalPalette {
            foreground: [0xab, 0xcd, 0xef],
            background: [0x10, 0x20, 0x30],
        };
        let mut pty =
            attached_pty_with_palette("printf 'P\\033[?25l'; sleep 30", Some(host_palette));
        wait_for_attached_screen(&pty, "P");
        let mut themes = ThemeState::default();
        themes.move_preview(1);
        assert_eq!(themes.active().id, "terminal");

        let session = attached_test_session();
        let buffer = render_attached_with_themes(&pty, &session, &themes);

        assert_eq!(buffer[(0, 1)].symbol(), "P");
        assert_eq!(
            buffer[(0, 1)].fg,
            ratatui::style::Color::Rgb(0xab, 0xcd, 0xef)
        );
        assert_eq!(
            buffer[(0, 1)].bg,
            ratatui::style::Color::Rgb(0x10, 0x20, 0x30)
        );

        pty.kill();
    }

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
    fn logo_marks_reserve_one_cell_of_breathing_room_on_each_side() {
        set_logo_marks(true);
        assert!(logo_marks());
        let theme = theme::amber(false);
        for b in [
            BackendKind::Claude,
            BackendKind::Codex,
            BackendKind::Opencode,
        ] {
            assert_eq!(backend_mark(b, &theme), "    ");
            assert_eq!(mark_width(backend_mark(b, &theme)), 4);
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
                    false,
                    Rect::new(0, 0, width, 1),
                );
            })
            .unwrap();
        terminal
    }

    #[test]
    fn rename_truncates_wide_input_by_display_width() {
        set_logo_marks(true);
        let mut terminal = draw_rename("a界bc", 10, None);
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(5, 0)].symbol(), "a");
        assert_eq!(buffer[(6, 0)].symbol(), "界");
        assert_eq!(buffer[(8, 0)].symbol(), "b");
        assert_eq!(buffer[(9, 0)].symbol(), " ");
        terminal.backend_mut().assert_cursor_position((9, 0));
    }

    #[test]
    fn session_logo_has_a_blank_cell_before_and_after_its_image() {
        set_logo_marks(true);
        let logos = LogoMarks::halfblocks_for_test().unwrap();
        let terminal = draw_rename("abcde", 10, Some(&logos));
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(0, 0)].symbol(), "✎");
        assert_eq!(buffer[(1, 0)].symbol(), " ");
        assert_ne!(buffer[(2, 0)].symbol(), " ");
        assert_ne!(buffer[(3, 0)].symbol(), " ");
        assert_eq!(buffer[(4, 0)].symbol(), " ");
        assert_eq!(buffer[(5, 0)].symbol(), "a");
    }

    #[test]
    fn composer_logo_and_prompt_share_the_same_padded_start_cell() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        set_logo_marks(true);
        let app = App::new(Vec::new());
        let composer = Composer::new();
        let logos = LogoMarks::halfblocks_for_test().unwrap();
        let theme = theme::amber(false);
        let mut terminal = Terminal::new(TestBackend::new(30, 4)).unwrap();
        terminal
            .draw(|frame| {
                composer::draw(
                    frame,
                    &app,
                    &composer,
                    Some(&logos),
                    &theme,
                    Rect::new(0, 0, 30, 4),
                    true,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(1, 1)].symbol(), " ");
        assert_ne!(buffer[(2, 1)].symbol(), " ");
        assert_ne!(buffer[(3, 1)].symbol(), " ");
        assert_eq!(buffer[(4, 1)].symbol(), " ");
        assert_eq!(buffer[(5, 1)].symbol(), " ");
        assert_eq!(buffer[(6, 1)].symbol(), "c");
        assert_eq!(buffer[(1, 2)].symbol(), " ");
        assert_eq!(buffer[(2, 2)].symbol(), "❯");
        assert_eq!(buffer[(6, 2)].symbol(), "d");
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
                    false,
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

    fn render_activity_row(
        width: u16,
        group_by_state: bool,
    ) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        let mut app = App::new(vec![Session {
            backend: BackendKind::Codex,
            id: "activity-row".into(),
            short_id: None,
            origin: agent_viewer_core::SessionOrigin::Interactive,
            title: "Activity".into(),
            cwd: "/tmp/agent-viewer-activity".into(),
            git_branch: None,
            status: Status::Working,
            created_at_ms: 0,
            updated_at_ms: 0,
            hidden: false,
            companion: false,
            summary: "summary ".repeat(30),
            pid: None,
            rollout_path: None,
            pr_refs: vec![agent_viewer_core::PrRef {
                id: "42".into(),
                href: None,
            }],
            daemon_hosted: false,
        }]);
        if group_by_state {
            app.toggle_group_mode();
        }
        app.set_activity_ribbon(BackendKind::Codex, "activity-row", Some("█▆▃▂▁▁▁▁".into()));
        let height = app.visible().len() as u16;
        let pulses = Pulses::new();
        let pr_status = crate::pr_cache::PrStatusCache::default();
        let theme = theme::amber(false);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
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
                    false,
                    Rect::new(0, 0, width, height),
                );
            })
            .unwrap();
        terminal
    }

    fn render_mixed_status_activity_rows(
        group_by_state: bool,
    ) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        let session = |id: &str, title: &str, status: Status, created_at_ms: i64| Session {
            backend: BackendKind::Codex,
            id: id.into(),
            short_id: None,
            origin: agent_viewer_core::SessionOrigin::Interactive,
            title: title.into(),
            cwd: "/tmp/agent-viewer-status-alignment".into(),
            git_branch: None,
            status,
            created_at_ms,
            updated_at_ms: created_at_ms,
            hidden: false,
            companion: false,
            summary: String::new(),
            pid: None,
            rollout_path: None,
            pr_refs: Vec::new(),
            daemon_hosted: false,
        };
        let mut app = App::new(vec![
            session("working-row", "Working row", Status::Working, 2),
            session("done-row", "Done row", Status::Done, 1),
        ]);
        if group_by_state {
            app.toggle_group_mode();
        }
        for id in ["working-row", "done-row"] {
            app.set_activity_ribbon(BackendKind::Codex, id, Some("█▆▃▂▁▁▁▁".into()));
        }
        let width = 160;
        let height = app.visible().len() as u16;
        let pulses = Pulses::new();
        let pr_status = crate::pr_cache::PrStatusCache::default();
        let theme = theme::amber(false);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
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
                    false,
                    Rect::new(0, 0, width, height),
                );
            })
            .unwrap();
        terminal
    }

    #[test]
    fn activity_ribbon_renders_at_140_and_drops_before_pr_at_100() {
        let wide = render_activity_row(140, false);
        let wide_buffer = wide.backend().buffer();
        let wide_text: String = (0..140).map(|x| wide_buffer[(x, 1)].symbol()).collect();
        assert!(wide_text.contains("█▆▃▂▁▁▁▁"));
        assert!(wide_text.contains("#42"));
        assert!(!wide_text.contains(&"summary ".repeat(30)));

        let narrow = render_activity_row(100, false);
        let narrow_buffer = narrow.backend().buffer();
        let narrow_text: String = (0..100).map(|x| narrow_buffer[(x, 1)].symbol()).collect();
        assert!(!narrow_text.contains("█▆▃▂▁▁▁▁"));
        assert!(narrow_text.contains("#42"));
    }

    #[test]
    fn state_group_omits_project_while_rendering_activity_ribbon() {
        let terminal = render_activity_row(140, true);
        let buffer = terminal.backend().buffer();
        let text: String = (0..140).map(|x| buffer[(x, 1)].symbol()).collect();
        assert!(!text.contains("/tmp/agent-viewer"));
        assert!(text.contains("Activity"));
        assert!(text.contains("█▆▃▂▁▁▁▁"));
    }

    #[test]
    fn mixed_status_rows_share_one_activity_column_in_both_group_modes() {
        for group_by_state in [false, true] {
            let terminal = render_mixed_status_activity_rows(group_by_state);
            let buffer = terminal.backend().buffer();
            let ribbon_cells = (0..buffer.area.height)
                .filter_map(|y| {
                    (0..buffer.area.width)
                        .find(|&x| buffer[(x, y)].symbol() == "█")
                        .map(|x| (x, y))
                })
                .collect::<Vec<_>>();

            assert_eq!(ribbon_cells.len(), 2);
            assert_eq!(ribbon_cells[0].0, ribbon_cells[1].0);
            if group_by_state {
                for (_, y) in ribbon_cells {
                    let row: String = (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect();
                    assert!(!row.contains("agent-viewer"));
                    assert!(row.contains("row"));
                }
            }
        }
    }

    #[test]
    fn mono16_working_glyph_moves_without_changing_its_color() {
        let app = App::new(vec![Session {
            backend: BackendKind::Codex,
            id: "mono-motion".into(),
            short_id: None,
            origin: agent_viewer_core::SessionOrigin::Interactive,
            title: "Mono motion".into(),
            cwd: "/tmp/agent-viewer-mono-motion".into(),
            git_branch: None,
            status: Status::Working,
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
        let render = |now_ms| {
            let pulses = Pulses::new();
            let pr_status = crate::pr_cache::PrStatusCache::default();
            let theme = theme::mono16(false);
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 2)).unwrap();
            terminal
                .draw(|frame| {
                    draw_list(
                        frame,
                        &app,
                        &pulses,
                        now_ms,
                        &pr_status,
                        ListDeco { rename: None },
                        None,
                        &theme,
                        false,
                        Rect::new(0, 0, 80, 2),
                    );
                })
                .unwrap();
            let cell = &terminal.backend().buffer()[(0, 1)];
            (cell.symbol().to_string(), cell.fg)
        };

        let first = render(0);
        let second = render(120);
        assert_ne!(first.0, second.0);
        assert_eq!(first.1, second.1);
    }

    #[test]
    fn needs_input_glyph_is_static_with_animation_enabled() {
        let theme = theme::terminal(false);
        assert!(theme.animation);
        let status = Status::needs_input();

        for now_ms in [0, 300, 600, 900] {
            assert_eq!(status_glyph(&status, now_ms, &theme), ("◐", theme.attn));
        }
    }

    #[test]
    fn activity_request_range_is_viewport_plus_lookahead_not_full_list() {
        let hit = ListHit {
            area: Rect::new(0, 0, 140, 20),
            offset: 2_000,
            item_to_row: Vec::new(),
            blocked: None,
        };
        assert_eq!(hit.rendered_range(4_000, 8), Some(2_000..2_028));
        assert_eq!(ListHit::default().rendered_range(4_000, 8), None);
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
                        false,
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
                    sprite: Default::default(),
                    age_ramp: false,
                    tail: None,
                    wall: None,
                    wall_rects: &RefCell::new(Vec::new()),
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

    /// The wall owns the keyboard, so the composer must not be on screen claiming to. Its
    /// rows go to the grid instead: the tile has to reach further down the frame than the
    /// list region ever did.
    /// A plain working session for the wall render tests.
    fn walled_test_session() -> Session {
        Session {
            backend: BackendKind::Codex,
            id: "walled".to_string(),
            short_id: None,
            origin: agent_viewer_core::SessionOrigin::Interactive,
            title: "walled".to_string(),
            cwd: "/tmp".into(),
            git_branch: None,
            status: Status::Working,
            created_at_ms: 0,
            updated_at_ms: 0,
            hidden: false,
            companion: false,
            summary: String::new(),
            pid: None,
            rollout_path: None,
            pr_refs: Vec::new(),
            daemon_hosted: false,
        }
    }

    /// The tail pane is a panel beside the list; the wall replaces the list outright, and
    /// every tile already shows the live session the tail would be tailing. So the wall takes
    /// the whole region and the tail stands down rather than both fighting for the columns.
    #[test]
    fn the_tail_pane_stands_down_while_the_wall_is_up() {
        let session = walled_test_session();
        let app = App::new(vec![session.clone()]);
        let composer = Composer::new();
        let pulses = Pulses::new();
        let pr_status = crate::pr_cache::PrStatusCache::new();
        let list_hit = RefCell::new(ListHit::default());
        let themes = ThemeState::default();
        let rects = RefCell::new(Vec::new());
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 24)).unwrap();
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
                    sprite: Default::default(),
                    age_ramp: false,
                    // The pane is open, and wide enough that it would take its columns.
                    tail: Some(TailView {
                        session: Some(&session),
                        events: None,
                        live: None,
                    }),
                    wall: Some(WallView {
                        tiles: vec![WallTile {
                            session: &session,
                            project: String::new(),
                            pty: None,
                            error: None,
                        }],
                        selected: 0,
                        overflow: 0,
                    }),
                    wall_rects: &rects,
                },
            );
        })
        .unwrap();

        let rendered: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();

        // The wall owns the full width...
        assert_eq!(rects.borrow()[0].width, 160);
        // ...and, more to the point, the pane is not painted over the top of it. The wall
        // draws into the whole region either way, so overpainting is the failure mode here,
        // not a narrower grid.
        assert!(
            !rendered.contains("tail"),
            "the tail pane rendered on top of the wall"
        );
    }

    #[test]
    fn the_composer_is_not_drawn_while_the_wall_is_on() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::buffer::Cell;

        let session = walled_test_session();
        let app = App::new(vec![session.clone()]);
        let composer = Composer::new();
        let pulses = Pulses::new();
        let pr_status = crate::pr_cache::PrStatusCache::new();
        let list_hit = RefCell::new(ListHit::default());
        let themes = ThemeState::default();
        let rects = RefCell::new(Vec::new());
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
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
                    sprite: Default::default(),
                    age_ramp: false,
                    tail: None,
                    wall: Some(WallView {
                        tiles: vec![WallTile {
                            session: &session,
                            project: String::new(),
                            pty: None,
                            error: None,
                        }],
                        selected: 0,
                        overflow: 0,
                    }),
                    wall_rects: &rects,
                },
            );
        })
        .unwrap();
        let buffer = term.backend().buffer().clone();
        let rendered: String = buffer.content().iter().map(Cell::symbol).collect();

        assert!(
            !rendered.contains('❯'),
            "the composer prompt is still on screen with the wall up"
        );
        let tile = rects.borrow()[0];
        assert!(
            tile.bottom() >= 22,
            "the tile stopped at row {}, so the composer rows were not reclaimed",
            tile.bottom()
        );
    }

    #[test]
    fn help_popup_shows_modifier_shortcuts_at_80_by_24() {
        let (rows, _) = render_viewer(80, 24, "", Mode::Help);
        let rendered = rows.concat();

        // Ctrl+C is last in the list and Ctrl+B is the newest entry, so between them these
        // catch a list that has outgrown the popup.
        for shortcut in ["Ctrl+A", "Ctrl+B", "Ctrl+D", "Ctrl+U", "Ctrl+]", "Ctrl+C"] {
            assert!(rendered.contains(shortcut), "missing {shortcut}");
        }
    }
    /// A three-item triage queue of blocked claude sessions.
    fn triage_state() -> TriageState {
        let items = (0..3)
            .map(|index| Session {
                backend: BackendKind::Claude,
                id: format!("blocked-{index}"),
                short_id: Some(format!("blocked-{index}")),
                origin: agent_viewer_core::SessionOrigin::Background,
                title: format!("Bonus: av-video-wall {index}"),
                cwd: "/home/me/git/acme/widget".into(),
                git_branch: None,
                status: Status::NeedsInput {
                    reason: Some("pick one".into()),
                },
                created_at_ms: 0,
                updated_at_ms: index,
                hidden: false,
                companion: false,
                summary: String::new(),
                pid: None,
                rollout_path: None,
                pr_refs: Vec::new(),
                daemon_hosted: false,
            })
            .collect::<Vec<_>>();
        TriageState::new(triage_queue(&items))
    }

    #[test]
    fn triage_shows_the_queue_position_and_what_is_up_next() {
        let (rows, _) = render_viewer(120, 44, "", Mode::Triage(triage_state()));
        let rendered = rows.concat();

        assert!(
            rendered.contains("1 of 3"),
            "the progress affordance is missing: {rendered:?}"
        );
        assert!(
            rendered.contains("up next"),
            "the queue preview is missing: {rendered:?}"
        );
        assert!(
            rendered.contains("av-video-wall 1") && rendered.contains("av-video-wall 2"),
            "up next must name the sessions still queued: {rendered:?}"
        );
    }

    /// The panel is the session, so the modal must say the keyboard belongs to it — otherwise
    /// the reserved chords read as the only keys that work.
    #[test]
    fn triage_says_the_keys_go_to_the_session() {
        let (rows, _) = render_viewer(120, 44, "", Mode::Triage(triage_state()));
        let rendered = rows.concat();

        for hint in ["⌃N", "⌃]", "goes to the session"] {
            assert!(rendered.contains(hint), "missing {hint} in {rendered:?}");
        }
    }

    /// Without a live child the panel must say so. A silently empty box reads as a session
    /// that has nothing to ask, which is the opposite of why it is in the queue.
    #[test]
    fn triage_without_a_live_child_says_it_is_attaching() {
        let (rows, _) = render_viewer(120, 44, "", Mode::Triage(triage_state()));
        assert!(
            rows.concat().contains("attaching"),
            "the panel must not render an unexplained empty box"
        );
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
                    sprite: Default::default(),
                    age_ramp: false,
                    tail: None,
                    wall: None,
                    wall_rects: &RefCell::new(Vec::new()),
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
        assert_eq!(buf[(6, (top + 1) as u16)].symbol(), "c");
        assert_eq!(&rows[top + 2], &format!("│ ❯   hello▏{}│", " ".repeat(37)));
        assert_eq!((cursor.x, cursor.y), (11, (top + 2) as u16));
    }

    #[test]
    fn empty_composer_keeps_placeholder_but_places_cursor_at_input_start() {
        let (rows, cursor) = render_viewer(40, 16, "", Mode::Normal);
        let (top, _) = composer_bounds(&rows);

        assert!(rows[top + 2].contains("describe a task"));
        assert_eq!(cursor, (6, (top + 2) as u16));
    }

    #[test]
    fn composer_exact_width_input_adds_a_cursor_continuation_row() {
        // The gutter leaves five cells on the first row, so ten characters use a continuation
        // row and the caret remains visible after the final character.
        let (rows, cursor) = render_viewer(12, 18, "0123456789", Mode::Normal);
        let (top, bottom) = composer_bounds(&rows);

        assert_eq!(bottom - top + 1, 5);
        assert_eq!(&rows[top + 2], "│ ❯   01234│");
        assert_eq!(&rows[top + 3], "│56789▏    │");
        assert_eq!(cursor, (6, (top + 3) as u16));
    }

    #[test]
    fn composer_preserves_blank_lines_and_wraps_unicode_at_full_width() {
        let text = "abcdef界gh\n\nijklmnopqrst";
        let (rows, cursor) = render_viewer(12, 18, text, Mode::Normal);
        let (top, bottom) = composer_bounds(&rows);

        // The viewport cap scrolls the earlier lines while keeping the blank line and tail.
        assert_eq!(bottom - top + 1, 6);
        assert_eq!(&rows[top + 2], "│          │");
        assert_eq!(&rows[top + 3], "│ijklmnopqr│");
        assert_eq!(&rows[top + 4], "│st▏       │");
        assert_eq!(cursor, (3, (top + 4) as u16));
    }

    #[test]
    fn composer_content_uses_the_padded_list_mark_column() {
        // Marks are process-global mode, so this asserts measured columns, not glyphs.
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
                    sprite: Default::default(),
                    age_ramp: false,
                    tail: None,
                    wall: None,
                    wall_rects: &RefCell::new(Vec::new()),
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
        // Metadata uses a five cell mark field. The input uses the same gutter.
        assert_eq!(buf[(6, (top + 1) as u16)].symbol(), "c");
        assert_eq!(buf[(1, (top + 2) as u16)].symbol(), " ");
        assert_eq!(buf[(2, (top + 2) as u16)].symbol(), "❯");
        assert_eq!(buf[(6, (top + 2) as u16)].symbol(), "t");
    }

    #[test]
    fn composer_wraps_the_input_at_word_boundaries() {
        // The first row accounts for the gutter and breaks before the whole next word.
        let (rows, _) = render_viewer(20, 24, "wrap at word boundaries here", Mode::Normal);
        let (top, _) = composer_bounds(&rows);

        assert_eq!(&rows[top + 2], "│ ❯   wrap at word │");
        assert_eq!(&rows[top + 3], "│boundaries here▏  │");
    }

    #[test]
    fn composer_height_cap_keeps_metadata_and_the_input_tail_visible() {
        let text = (0..15)
            .map(|index| format!("line{index:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (rows, cursor) = render_viewer(20, 24, &text, Mode::Normal);
        let (top, bottom) = composer_bounds(&rows);

        assert_eq!(bottom - top + 1, 8);
        assert!(rows[top + 1].contains("claude"));
        assert!(rows[top + 2].contains("line10"));
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
