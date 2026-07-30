use super::{
    fg,
    sprite::{
        Constellation, Fleet, HEIGHT as SPRITE_HEIGHT, Lighthouse, SpriteKind, Turbine,
        WIDTH as SPRITE_WIDTH,
    },
    theme::Theme,
};
use crate::app::App;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::path::Path;

pub(super) const BASE_HEADER_HEIGHT: u16 = 6;
pub(super) const SPRITE_HEADER_HEIGHT: u16 = SPRITE_HEIGHT + 1;
pub(super) const COMFORTABLE_WIDTH: u16 = SPRITE_WIDTH + 2 + 39;

const SPRITE_HEIGHT_BUDGET: u16 = 6;
const MIN_LIST_ROWS: u16 = 8;
const GAP_AND_FOOTER_ROWS: u16 = 2;

pub(super) fn height(viewport: Rect, composer_height: u16) -> u16 {
    let list_rows = viewport.height.saturating_sub(
        BASE_HEADER_HEIGHT + SPRITE_HEIGHT_BUDGET + composer_height + GAP_AND_FOOTER_ROWS,
    );
    if viewport.width >= COMFORTABLE_WIDTH && list_rows >= MIN_LIST_ROWS {
        SPRITE_HEADER_HEIGHT
    } else {
        BASE_HEADER_HEIGHT
    }
}

pub(super) fn draw(
    frame: &mut Frame,
    app: &App,
    workspace: &Path,
    now_ms: i64,
    theme: &Theme,
    sprite: SpriteKind,
    area: Rect,
) {
    let show_sprite = area.height >= SPRITE_HEADER_HEIGHT && area.width >= COMFORTABLE_WIDTH;
    let text_x = if show_sprite {
        area.x + SPRITE_WIDTH + 2
    } else {
        area.x
    };
    let text_area = Rect::new(
        text_x,
        area.y,
        area.right().saturating_sub(text_x),
        area.height,
    );
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
        .split(text_area);

    if show_sprite {
        let slot = Rect::new(area.x, area.y, SPRITE_WIDTH, SPRITE_HEIGHT);
        let lamp = lamp_color(app, theme);
        match sprite {
            SpriteKind::Lighthouse => {
                frame.render_widget(Lighthouse::new(theme, lamp, now_ms), slot)
            }
            SpriteKind::Constellation => {
                frame.render_widget(Constellation::new(theme, fleet(app), now_ms), slot)
            }
            SpriteKind::Turbine => frame.render_widget(
                Turbine::new(theme, lamp, app.running_count(), now_ms),
                slot,
            ),
        }
    }

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" [av] ", fg(theme.accent)),
            Span::styled(
                "Agent Viewer",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" v{}", env!("CARGO_PKG_VERSION")), fg(theme.muted)),
            Span::styled(if show_sprite { " · " } else { "" }, fg(theme.faint)),
            Span::styled(
                if show_sprite { theme.name.as_str() } else { "" },
                fg(theme.muted),
            ),
        ])),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" Workspace ", fg(theme.muted)),
            Span::styled(workspace.display().to_string(), fg(theme.text)),
        ])),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {} awaiting input", app.needs_input_count()),
                fg(theme.warn),
            ),
            Span::styled(" · ", fg(theme.muted)),
            Span::styled(format!("{} working", app.running_count()), fg(theme.accent)),
            Span::styled(" · ", fg(theme.muted)),
            Span::styled(
                format!("{} completed", app.completed_count()),
                fg(theme.muted),
            ),
        ])),
        rows[3],
    );
}

fn lamp_color(app: &App, theme: &Theme) -> Color {
    if app.needs_input_count() > 0 {
        theme.warn
    } else {
        theme.accent
    }
}

/// The same three counts the header's totals line reports, handed to the sprite as data.
fn fleet(app: &App) -> Fleet {
    Fleet {
        needs_input: app.needs_input_count(),
        working: app.running_count(),
        done: app.completed_count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_viewer_core::{BackendKind, Session, SessionOrigin, Status};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use std::path::PathBuf;

    fn session(status: Status) -> Session {
        Session {
            backend: BackendKind::Codex,
            id: "session".into(),
            short_id: None,
            origin: SessionOrigin::Interactive,
            title: "session".into(),
            cwd: PathBuf::from("/tmp/project"),
            git_branch: None,
            status,
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

    fn render_header(
        app: &App,
        theme: &Theme,
        viewport: Rect,
        composer_height: u16,
        now_ms: i64,
    ) -> Buffer {
        render_sprite_header(app, theme, viewport, composer_height, now_ms, SpriteKind::Lighthouse)
    }

    fn render_sprite_header(
        app: &App,
        theme: &Theme,
        viewport: Rect,
        composer_height: u16,
        now_ms: i64,
        sprite: SpriteKind,
    ) -> Buffer {
        let header_height = height(viewport, composer_height);
        let mut terminal = Terminal::new(TestBackend::new(viewport.width, header_height)).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    app,
                    Path::new("/tmp/project"),
                    now_ms,
                    theme,
                    sprite,
                    Rect::new(0, 0, viewport.width, header_height),
                );
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn block_count(buffer: &Buffer) -> usize {
        buffer
            .content
            .iter()
            .filter(|cell| cell.symbol() == "▀")
            .count()
    }

    #[test]
    fn lamp_uses_fleet_input_state() {
        let mut theme = super::super::theme::amber(false);
        theme.animation = false;
        let clear = App::new(vec![session(Status::Working)]);
        let alert = App::new(vec![session(Status::needs_input())]);
        let viewport = Rect::new(0, 0, COMFORTABLE_WIDTH, 40);
        let clear_buffer = render_header(&clear, &theme, viewport, 3, 0);
        let alert_buffer = render_header(&alert, &theme, viewport, 3, 0);

        assert_eq!(lamp_color(&clear, &theme), theme.accent);
        assert_eq!(lamp_color(&alert, &theme), theme.warn);
        assert_eq!(clear_buffer[(5, 1)].fg, theme.accent);
        assert_eq!(alert_buffer[(5, 1)].fg, theme.warn);
        assert_ne!(clear_buffer[(5, 1)].fg, alert_buffer[(5, 1)].fg);
    }

    #[test]
    fn each_sprite_kind_draws_its_own_mascot_in_the_same_slot() {
        let mut theme = super::super::theme::amber(false);
        theme.animation = false;
        let app = App::new(vec![session(Status::Working)]);
        let viewport = Rect::new(0, 0, COMFORTABLE_WIDTH, 40);
        let render = |sprite| render_sprite_header(&app, &theme, viewport, 3, 0, sprite);
        let buffers = SpriteKind::ALL.map(render);

        for buffer in &buffers {
            assert_eq!(block_count(buffer), 66);
        }
        for left in 0..buffers.len() {
            for right in left + 1..buffers.len() {
                assert_ne!(buffers[left], buffers[right]);
            }
        }
    }

    #[test]
    fn sprite_choices_follow_the_exact_six_scene_order() {
        assert_eq!(
            SpriteKind::ALL,
            [
                SpriteKind::Lighthouse,
                SpriteKind::Constellation,
                SpriteKind::Turbine,
                SpriteKind::Sailboat,
                SpriteKind::Airplane,
                SpriteKind::HotAirBalloon,
            ]
        );
    }

    /// Names round-trip through storage: a saved sprite must come back as the same sprite, and
    /// anything unrecognized must fall back rather than resolving to some other mascot.
    #[test]
    fn sprite_names_round_trip_and_unknown_names_fall_back() {
        for sprite in SpriteKind::ALL {
            assert_eq!(SpriteKind::from_name(Some(sprite.name())), Some(sprite));
        }
        assert_eq!(
            SpriteKind::from_name(Some(" turbine ")),
            Some(SpriteKind::Turbine)
        );
        assert_eq!(SpriteKind::from_name(Some("nonsense")), None);
        assert_eq!(SpriteKind::from_name(None), None);
    }

    #[test]
    fn cycling_visits_every_sprite_and_returns_to_the_start() {
        let mut sprite = SpriteKind::default();
        let mut seen = Vec::new();
        for _ in 0..SpriteKind::ALL.len() {
            seen.push(sprite);
            sprite = sprite.next();
        }

        assert_eq!(sprite, SpriteKind::default(), "the cycle must close");
        assert_eq!(seen.len(), SpriteKind::ALL.len());
        for kind in SpriteKind::ALL {
            assert!(seen.contains(&kind), "{} is unreachable", kind.name());
        }
    }

    #[test]
    fn sprite_layout_preserves_a_usable_list() {
        let theme = super::super::theme::mono16(false);
        let app = App::new(Vec::new());
        let comfortable = Rect::new(0, 0, COMFORTABLE_WIDTH, 40);
        let narrow = Rect::new(0, 0, COMFORTABLE_WIDTH - 1, 40);
        let short = Rect::new(0, 0, COMFORTABLE_WIDTH, 24);

        assert_eq!(height(comfortable, 3), SPRITE_HEADER_HEIGHT);
        assert_eq!(height(narrow, 3), BASE_HEADER_HEIGHT);
        assert_eq!(height(short, 3), BASE_HEADER_HEIGHT);
        assert_eq!(
            block_count(&render_header(&app, &theme, comfortable, 3, 0)),
            66
        );
        assert_eq!(block_count(&render_header(&app, &theme, narrow, 3, 0)), 0);
        assert_eq!(block_count(&render_header(&app, &theme, short, 3, 0)), 0);
    }
}
