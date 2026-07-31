//! Video wall (Ctrl+W): the session list is replaced by a grid of live PTY tiles.
//!
//! This is a real attach, not transcript parsing. Ctrl+W shows everything that is running:
//! the wall connects each working session through the same attach path a manual attach uses,
//! and closes every one of those connections when the wall closes. Nothing stays connected
//! off screen. Only live sessions are ever joined, so no finished session is resurrected.
//!
//! Geometry lives here as pure functions because two call sites need identical answers: the
//! render path (which draws the tiles) and the run loop (which resizes each tile's child,
//! since `PtySession::resize` needs `&mut` and draw is `&`-only).

use super::list::truncate_display_width as truncate_display;
use super::theme::{MarkSet, Theme};
use super::{backend_mark_color, display_width, fg, mark_for, mark_width, status_glyph};
use crate::app::{App, Row};
use agent_viewer_core::pty::PtySession;
use agent_viewer_core::{BackendKind, Session, Status};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use std::collections::{HashMap, HashSet};

/// Hard cap on tiles. Each tile is a live child being resized and re-parsed, so this is a
/// process budget, not a rendering one. Above the cap the footer carries the overflow count.
pub const MAX_TILES: usize = 9;

/// Columns spent on the selection caret, reserved on every tile so nothing shifts when the
/// selection moves.
const CARET_COLS: usize = 2;

/// Wall state carried on the run loop's `Ui`. The wall is a flag on the list view rather
/// than a `Mode`, which is what keeps every already-bound chord meaning what it meant.
#[derive(Default)]
pub struct WallState {
    pub on: bool,
    /// Index into the capped tile list.
    pub selected: usize,
    /// Last size each tile's PTY was resized to, so the run loop does not SIGWINCH every
    /// child on every frame.
    pub sized: HashMap<(BackendKind, String), (u16, u16)>,
    /// Sessions the wall has asked to connect this visit. Doubles as the ownership set: a
    /// PTY in here belongs to the wall and is closed when the wall closes. Membership also
    /// stops a join being re-requested every frame, including one that failed.
    pub requested: HashSet<(BackendKind, String)>,
    /// Why a tile could not be connected, keyed by session. Rendered in the tile instead of
    /// a live screen, so a failure is visible rather than a permanently blank box.
    pub failed: HashMap<(BackendKind, String), String>,
}

impl WallState {
    /// Whether the wall owns this session's connection. Zooming into a tile must not close
    /// it on the way back out, and closing the wall must close every one of these.
    pub fn owns(&self, key: &(BackendKind, String)) -> bool {
        self.on && self.requested.contains(key)
    }

    /// Forget everything about a visit. The caller closes the PTYs; this only drops the
    /// bookkeeping, so it must run alongside that, never instead of it.
    pub fn clear(&mut self) {
        self.selected = 0;
        self.sized.clear();
        self.requested.clear();
        self.failed.clear();
    }
}

/// One tile: a session snapshot and its connection, once there is one.
pub struct WallTile<'a> {
    pub session: &'a Session,
    /// Pre-rendered project label (computed off the tile so draw stays allocation-light).
    pub project: String,
    /// The live child, or None while the join is still in flight or has failed.
    pub pty: Option<&'a PtySession>,
    /// Why this tile has no connection, when the join failed outright.
    pub error: Option<&'a str>,
}

/// Everything the wall needs for a frame.
pub struct WallView<'a> {
    pub tiles: Vec<WallTile<'a>>,
    pub selected: usize,
    /// Eligible sessions beyond `MAX_TILES`. Surfaced in the footer; never silently dropped.
    pub overflow: usize,
}

// --- Membership -----------------------------------------------------------------

/// Every session the wall should tile, in `visible()` order so the wall reads in the same
/// sequence as the list it replaced.
///
/// Membership is state alone: `Working` or `NeedsInput`. The wall connects whatever is not
/// connected yet rather than showing only what happened to be connected already — the whole
/// point is that one keypress shows you everything that is running. `NeedsInput` is included
/// because a session blocked for an answer is the most useful thing the wall can show.
///
/// Never iterate `attached` here — `HashMap` order is unspecified and the grid would shuffle
/// between frames.
pub fn wall_sessions(app: &App) -> Vec<(BackendKind, String)> {
    app.visible()
        .iter()
        .filter_map(|row| match row {
            Row::Session {
                backend,
                id,
                status: Status::Working | Status::NeedsInput { .. },
                ..
            } => Some((*backend, id.clone())),
            _ => None,
        })
        .collect()
}

/// The sessions the wall should be connected to right now: the capped tile set.
pub fn tile_keys(app: &App) -> Vec<(BackendKind, String)> {
    let mut keys = wall_sessions(app);
    keys.truncate(MAX_TILES);
    keys
}

// --- Geometry -------------------------------------------------------------------

/// Tiles actually drawn for `eligible` candidates.
pub fn tile_count(eligible: usize) -> usize {
    eligible.min(MAX_TILES)
}

/// Eligible sessions that did not fit.
pub fn overflow(eligible: usize) -> usize {
    eligible.saturating_sub(MAX_TILES)
}

/// The documented grid for `eligible` candidates, as `(cols, rows)`. Caps internally, so
/// eleven eligible sessions give the same 3x3 as nine.
///
/// ```text
/// 1      1 x 1        3, 4   2 x 2        7..9   3 x 3
/// 2      2 x 1        5, 6   3 x 2
/// ```
pub fn grid_dims(eligible: usize) -> (u16, u16) {
    match tile_count(eligible) {
        0 | 1 => (1, 1),
        2 => (2, 1),
        3 | 4 => (2, 2),
        5 | 6 => (3, 2),
        _ => (3, 3),
    }
}

/// The rect for each drawn tile, row-major. Rows and columns split `area` by ratio, so the
/// rects partition it exactly: every rect is inside `area` and no two overlap. A short last
/// row keeps the full grid's column widths rather than stretching, so tiles stay aligned.
pub fn tile_rects(area: Rect, eligible: usize) -> Vec<Rect> {
    let count = tile_count(eligible);
    if count == 0 || area.width == 0 || area.height == 0 {
        return Vec::new();
    }
    let (cols, rows) = grid_dims(eligible);
    let row_rects = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![
            Constraint::Ratio(1, u32::from(rows));
            usize::from(rows)
        ])
        .split(area);
    let mut out = Vec::with_capacity(count);
    for row_rect in row_rects.iter() {
        let remaining = count - out.len();
        if remaining == 0 {
            break;
        }
        let col_rects = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(vec![
                Constraint::Ratio(1, u32::from(cols));
                usize::from(cols)
            ])
            .split(*row_rect);
        for col_rect in col_rects.iter().take(remaining.min(usize::from(cols))) {
            out.push(*col_rect);
        }
    }
    out
}

/// The `(rows, cols)` a tile's child gets: the rect minus its border and its one header row.
pub fn tile_inner(rect: Rect) -> (u16, u16) {
    (rect.height.saturating_sub(3), rect.width.saturating_sub(2))
}

// --- Tile chrome ----------------------------------------------------------------

/// What survives in a tile header at a given width. An empty `project` or `elapsed` means
/// that field was dropped.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct HeaderPlan {
    pub title: String,
    pub project: String,
    pub elapsed: String,
}

/// Width of the right-hand group (`project elapsed`), including its two-column gap from the
/// title. Zero when both fields are gone.
fn right_width(project: &str, elapsed: &str) -> usize {
    let inner = match (project.is_empty(), elapsed.is_empty()) {
        (true, true) => return 0,
        (false, false) => display_width(project) + 1 + display_width(elapsed),
        (false, true) => display_width(project),
        (true, false) => display_width(elapsed),
    };
    inner + 2
}

/// Degrade the tile header for `width` columns in the documented order: drop the project,
/// then drop the elapsed, then truncate the title. The state glyph and the backend mark are
/// never dropped — they are what the wall exists to show at a glance.
pub(super) fn header_plan(
    title: &str,
    project: &str,
    elapsed: &str,
    mark_cols: usize,
    width: usize,
) -> HeaderPlan {
    // caret + glyph + space + mark + space
    let fixed = CARET_COLS + 1 + 1 + mark_cols + 1;
    let avail = width.saturating_sub(fixed);
    let title_width = display_width(title);
    let mut project = project.to_string();
    let mut elapsed = elapsed.to_string();
    while title_width + right_width(&project, &elapsed) > avail {
        if !project.is_empty() {
            project.clear();
        } else if !elapsed.is_empty() {
            elapsed.clear();
        } else {
            break;
        }
    }
    let title = truncate_display(title, avail.saturating_sub(right_width(&project, &elapsed)));
    HeaderPlan {
        title,
        project,
        elapsed,
    }
}

// --- Render ---------------------------------------------------------------------

pub(super) fn draw(frame: &mut Frame, view: &WallView, now_ms: i64, theme: &Theme, area: Rect) {
    if view.tiles.is_empty() {
        draw_empty(frame, theme, area);
        return;
    }
    let rects = tile_rects(area, view.tiles.len() + view.overflow);
    for (index, (tile, rect)) in view.tiles.iter().zip(rects).enumerate() {
        draw_tile(frame, tile, index == view.selected, now_ms, theme, rect);
    }
}

fn draw_empty(frame: &mut Frame, theme: &Theme, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let text = "nothing is running · a session appears here as soon as it starts";
    let width = (display_width(text) as u16).min(area.width);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(text, fg(theme.muted)))),
        Rect {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height / 2,
            width,
            height: 1,
        },
    );
}

fn draw_tile(
    frame: &mut Frame,
    tile: &WallTile,
    selected: bool,
    now_ms: i64,
    theme: &Theme,
    rect: Rect,
) {
    let needs_input = matches!(tile.session.status, Status::NeedsInput { .. });
    // A blocked tile owns its own border and header and nothing else: no full-surface
    // repaint, no flash, and no reordering the grid to float it forward. Grid stability is
    // what lets you keep your place in the tile you were reading.
    let border_color = if needs_input {
        theme.warn
    } else if selected {
        theme.accent
    } else {
        theme.border
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(fg(border_color))
        .style(Style::default().bg(theme.surface));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    frame.render_widget(
        Paragraph::new(header_line(
            tile,
            selected,
            now_ms,
            inner.width as usize,
            theme,
        )),
        Rect { height: 1, ..inner },
    );
    if inner.height <= 1 {
        return;
    }

    let content = Rect {
        y: inner.y + 1,
        height: inner.height - 1,
        ..inner
    };
    let Some(pty) = tile.pty else {
        // No connection yet. A joining tile says so rather than sitting blank, and a failed
        // one says why — the wall connects on its own, so a silent empty box would read as
        // a broken session rather than a broken join.
        let (text, color) = match tile.error {
            Some(error) => (format!("could not connect · {error}"), theme.err),
            None => ("connecting…".to_string(), theme.faint),
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_display(&text, content.width as usize),
                fg(color),
            ))),
            Rect {
                height: 1,
                ..content
            },
        );
        return;
    };
    pty.with_screen(|screen| {
        frame.render_widget(
            super::attach::ThemedPseudoTerminal {
                screen,
                palette: pty.palette(),
            },
            content,
        );
    });
}

fn header_line(
    tile: &WallTile,
    selected: bool,
    now_ms: i64,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let (glyph, glyph_color) = status_glyph(&tile.session.status, now_ms, theme);
    // Bypass logo mode here: it blanks the mark slot for an image overlay the wall does not
    // draw, which would leave a tile with no backend signal at all.
    let mark = mark_for(tile.session.backend, theme.mark_set == MarkSet::Glyph);
    let started_at_ms = if tile.session.created_at_ms > 0 {
        tile.session.created_at_ms
    } else {
        tile.session.updated_at_ms
    };
    let elapsed = crate::app::format_elapsed(now_ms - started_at_ms);
    let plan = header_plan(
        &tile.session.title,
        &tile.project,
        &elapsed,
        mark_width(mark),
        width,
    );

    let left = CARET_COLS + 1 + 1 + mark_width(mark) + 1 + display_width(&plan.title);
    let pad = width.saturating_sub(left + right_width(&plan.project, &plan.elapsed));
    // The caret, not the border color, is what makes the selection readable in mono16.
    let caret = if selected { "▸ " } else { "  " };
    let mut spans = vec![
        Span::styled(caret, fg(theme.accent)),
        Span::styled(glyph.to_string(), fg(glyph_color)),
        Span::raw(" "),
        Span::styled(
            mark.to_string(),
            fg(backend_mark_color(tile.session.backend, theme)),
        ),
        Span::raw(" "),
        Span::styled(
            plan.title,
            fg(if selected { theme.text } else { theme.muted }),
        ),
    ];
    if !plan.project.is_empty() || !plan.elapsed.is_empty() {
        spans.push(Span::raw(" ".repeat(pad + 2)));
        if !plan.project.is_empty() {
            spans.push(Span::styled(plan.project.clone(), fg(theme.faint)));
        }
        if !plan.project.is_empty() && !plan.elapsed.is_empty() {
            spans.push(Span::raw(" "));
        }
        if !plan.elapsed.is_empty() {
            spans.push(Span::styled(plan.elapsed, fg(theme.muted)));
        }
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_viewer_core::SessionOrigin;
    use agent_viewer_core::pty::PtySpec;
    use ratatui::backend::TestBackend;

    fn session(id: &str, status: Status) -> Session {
        Session {
            backend: BackendKind::Codex,
            id: id.to_string(),
            short_id: None,
            origin: SessionOrigin::Interactive,
            title: id.to_string(),
            cwd: "/tmp".into(),
            git_branch: None,
            status,
            created_at_ms: 1_000,
            updated_at_ms: 1_000,
            hidden: false,
            companion: false,
            summary: String::new(),
            pid: None,
            rollout_path: None,
            pr_refs: Vec::new(),
            daemon_hosted: false,
        }
    }

    fn live_pty() -> PtySession {
        PtySession::spawn(PtySpec {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "printf 'tile body'; sleep 30".to_string()],
            cwd: None,
            envs: Vec::new(),
            rows: 6,
            cols: 40,
            palette: None,
            scrollback_rows: 0,
        })
        .expect("wall tile child")
    }

    #[test]
    fn grid_dims_follow_the_documented_layout() {
        assert_eq!(grid_dims(1), (1, 1));
        assert_eq!(grid_dims(4), (2, 2));
        assert_eq!(grid_dims(5), (3, 2));
        // Eleven eligible sessions cap to nine and land on the 3x3.
        assert_eq!(grid_dims(11), (3, 3));
    }

    #[test]
    fn eleven_eligible_sessions_draw_nine_tiles_and_report_two_overflow() {
        assert_eq!(tile_count(11), 9);
        assert_eq!(overflow(11), 2);
        let rects = tile_rects(Rect::new(0, 0, 120, 30), 11);
        assert_eq!(rects.len(), 9);
    }

    #[test]
    fn tile_rects_partition_the_region_without_overlap() {
        let area = Rect::new(3, 2, 121, 31);
        for eligible in [1, 2, 3, 4, 5, 6, 7, 8, 9, 11] {
            let rects = tile_rects(area, eligible);
            assert_eq!(rects.len(), tile_count(eligible), "count for {eligible}");
            for rect in &rects {
                assert!(
                    rect.x >= area.x
                        && rect.y >= area.y
                        && rect.right() <= area.right()
                        && rect.bottom() <= area.bottom(),
                    "{rect:?} escaped {area:?} at {eligible}"
                );
            }
            for (i, a) in rects.iter().enumerate() {
                for b in rects.iter().skip(i + 1) {
                    assert!(
                        a.intersection(*b).area() == 0,
                        "{a:?} overlaps {b:?} at {eligible}"
                    );
                }
            }
        }
    }

    /// Membership is state alone. A working session earns a tile whether or not anything is
    /// connected to it yet; the wall does the connecting.
    #[test]
    fn every_working_session_is_tiled_not_just_connected_ones() {
        let app = App::new(vec![
            session("already-live", Status::Working),
            session("not-connected-yet", Status::Working),
        ]);

        let mut ids: Vec<String> = tile_keys(&app).into_iter().map(|(_, id)| id).collect();
        ids.sort();

        assert_eq!(
            ids,
            vec!["already-live".to_string(), "not-connected-yet".to_string()]
        );
    }

    #[test]
    fn only_live_states_are_tiled() {
        let app = App::new(vec![
            session("working", Status::Working),
            session("blocked", Status::NeedsInput { reason: None }),
            session("finished", Status::Done),
            session("resting", Status::Idle),
        ]);

        let mut ids: Vec<String> = tile_keys(&app).into_iter().map(|(_, id)| id).collect();
        ids.sort();

        assert_eq!(ids, vec!["blocked".to_string(), "working".to_string()]);
    }

    /// The cap is a live-child budget, so it has to bind the set the wall CONNECTS, not just
    /// the set it draws.
    #[test]
    fn the_connect_set_is_capped_even_though_membership_is_not() {
        let sessions: Vec<Session> = (0..11)
            .map(|i| session(&format!("live-{i:02}"), Status::Working))
            .collect();
        let app = App::new(sessions);

        assert_eq!(wall_sessions(&app).len(), 11);
        assert_eq!(tile_keys(&app).len(), MAX_TILES);
        assert_eq!(overflow(wall_sessions(&app).len()), 2);
    }

    #[test]
    fn a_tile_without_a_connection_says_connecting_and_a_failed_one_says_why() {
        let theme = crate::ui::theme::amber(false);
        let joining = session("joining", Status::Working);
        let broken = session("broken", Status::Working);
        let view = WallView {
            tiles: vec![
                WallTile {
                    session: &joining,
                    project: String::new(),
                    pty: None,
                    error: None,
                },
                WallTile {
                    session: &broken,
                    project: String::new(),
                    pty: None,
                    error: Some("no rollout path"),
                },
            ],
            selected: 0,
            overflow: 0,
        };
        let mut terminal = ratatui::Terminal::new(TestBackend::new(80, 10)).unwrap();
        terminal
            .draw(|frame| draw(frame, &view, 2_000, &theme, Rect::new(0, 0, 80, 10)))
            .expect("draw wall");
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();

        assert!(rendered.contains("connecting…"), "{rendered:?}");
        assert!(rendered.contains("no rollout path"), "{rendered:?}");
    }

    #[test]
    fn header_degrades_project_then_elapsed_then_the_title() {
        let title = "Opencode daemon probe";
        let project = "theconnman/agent-viewer";
        let elapsed = "6h";
        let mark = 4;

        let wide = header_plan(title, project, elapsed, mark, 70);
        assert_eq!(wide.title, title);
        assert_eq!(wide.project, project);
        assert_eq!(wide.elapsed, elapsed);

        // Project goes first; elapsed survives.
        let medium = header_plan(title, project, elapsed, mark, 44);
        assert_eq!(medium.title, title);
        assert_eq!(medium.project, "");
        assert_eq!(medium.elapsed, elapsed);

        // Then elapsed, with the title still whole.
        let narrow = header_plan(title, project, elapsed, mark, 32);
        assert_eq!(narrow.title, title);
        assert_eq!(narrow.project, "");
        assert_eq!(narrow.elapsed, "");

        // Only then does the title truncate.
        let tiny = header_plan(title, project, elapsed, mark, 20);
        assert_eq!(tiny.project, "");
        assert_eq!(tiny.elapsed, "");
        assert!(
            tiny.title.len() < title.len() && title.starts_with(&tiny.title),
            "title was {:?}",
            tiny.title
        );
    }

    /// The chrome that never drops, at the width where everything else already has.
    #[test]
    fn the_glyph_and_mark_survive_every_degradation_step() {
        // Animation off so the working glyph is the stable `✽` rather than a shimmer frame.
        let mut theme = crate::ui::theme::amber(false);
        theme.animation = false;
        let mut pty = live_pty();
        let tile = WallTile {
            session: &session("Opencode daemon probe", Status::Working),
            project: "theconnman/agent-viewer".to_string(),
            pty: Some(&pty),
            error: None,
        };
        let line = header_line(&tile, false, 2_000, 20, &theme);
        let rendered: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(rendered.contains('✽'), "glyph missing from {rendered:?}");
        assert!(
            rendered.contains(BackendKind::Codex.tag()),
            "mark missing from {rendered:?}"
        );
        pty.kill();
    }

    #[test]
    fn selected_and_blocked_tiles_carry_their_own_border_token() {
        let theme = crate::ui::theme::amber(false);
        let mut working = live_pty();
        let mut blocked = live_pty();
        let working_session = session("working", Status::Working);
        let blocked_session = session("blocked", Status::NeedsInput { reason: None });
        let view = WallView {
            tiles: vec![
                WallTile {
                    session: &working_session,
                    project: String::new(),
                    pty: Some(&working),
                    error: None,
                },
                WallTile {
                    session: &blocked_session,
                    project: String::new(),
                    pty: Some(&blocked),
                    error: None,
                },
            ],
            selected: 0,
            overflow: 0,
        };
        let mut terminal = ratatui::Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal
            .draw(|frame| draw(frame, &view, 2_000, &theme, Rect::new(0, 0, 40, 10)))
            .expect("draw wall");

        let buffer = terminal.backend().buffer();
        let rects = tile_rects(Rect::new(0, 0, 40, 10), 2);
        // Top-left corner cell of each tile's border.
        assert_eq!(
            buffer[(rects[0].x, rects[0].y)].fg,
            theme.accent,
            "selected tile border"
        );
        assert_eq!(
            buffer[(rects[1].x, rects[1].y)].fg,
            theme.warn,
            "needs-input tile border"
        );
        working.kill();
        blocked.kill();
    }

    #[test]
    fn an_unselected_working_tile_uses_the_plain_border_token() {
        let theme = crate::ui::theme::amber(false);
        let mut first = live_pty();
        let mut second = live_pty();
        let a = session("a", Status::Working);
        let b = session("b", Status::Working);
        let view = WallView {
            tiles: vec![
                WallTile {
                    session: &a,
                    project: String::new(),
                    pty: Some(&first),
                    error: None,
                },
                WallTile {
                    session: &b,
                    project: String::new(),
                    pty: Some(&second),
                    error: None,
                },
            ],
            selected: 0,
            overflow: 0,
        };
        let mut terminal = ratatui::Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal
            .draw(|frame| draw(frame, &view, 2_000, &theme, Rect::new(0, 0, 40, 10)))
            .expect("draw wall");
        let buffer = terminal.backend().buffer();
        let rects = tile_rects(Rect::new(0, 0, 40, 10), 2);
        assert_eq!(buffer[(rects[1].x, rects[1].y)].fg, theme.border);
        first.kill();
        second.kill();
    }

    #[test]
    fn tile_inner_reserves_the_border_and_the_header_row() {
        assert_eq!(tile_inner(Rect::new(0, 0, 60, 20)), (17, 58));
        assert_eq!(tile_inner(Rect::new(0, 0, 1, 1)), (0, 0));
    }

    #[test]
    fn an_empty_wall_says_so_instead_of_drawing_a_grid() {
        let theme = crate::ui::theme::amber(false);
        let view = WallView {
            tiles: Vec::new(),
            selected: 0,
            overflow: 0,
        };
        let mut terminal = ratatui::Terminal::new(TestBackend::new(80, 6)).unwrap();
        terminal
            .draw(|frame| draw(frame, &view, 0, &theme, Rect::new(0, 0, 80, 6)))
            .expect("draw empty wall");
        let buffer = terminal.backend().buffer();
        let rendered: String = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(rendered.contains("nothing is running"), "{rendered:?}");
        assert!(!rendered.contains('┌'), "an empty wall drew a tile border");
    }
}
