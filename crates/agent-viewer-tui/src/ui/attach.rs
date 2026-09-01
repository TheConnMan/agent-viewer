use super::{AttachView, fg, logo_marks, mark_for, mark_width, status_glyph, truncate};
use crate::logos::LogoMarks;
use crate::ui::list;
use crate::ui::theme::Theme;
use agent_viewer_core::pty::TerminalPalette;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui_image::Image;
use tui_term::{
    vt100,
    widget::{PseudoTerminal, Screen},
};

pub const ATTACHED_CHROME_ROWS: u16 = 2;

struct HeaderContext<'a> {
    pr_status: &'a crate::pr_cache::PrStatusCache,
    logos: Option<&'a LogoMarks>,
    theme: &'a Theme,
}

pub(super) fn draw(
    frame: &mut Frame,
    av: &AttachView<'_>,
    notice: &str,
    now_ms: i64,
    pr_status: &crate::pr_cache::PrStatusCache,
    logos: Option<&LogoMarks>,
    theme: &Theme,
) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(ATTACHED_CHROME_ROWS - 1),
        ])
        .split(area);

    let header = HeaderContext {
        pr_status,
        logos,
        theme,
    };
    draw_header(frame, av.session, av.exited, now_ms, &header, chunks[0]);
    render_screen(frame, chunks[1], av);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(notice.to_string(), fg(theme.warn)))),
        chunks[2],
    );
}

/// Render a live child's vt100 screen into `area`. Full-screen attach passes the whole
/// content rect; the triage inbox passes its panel. The child must have been resized to
/// exactly `area` or it wraps its own output at the wrong column.
pub(super) fn render_screen(frame: &mut Frame, area: Rect, av: &AttachView<'_>) {
    av.pty.with_screen(|screen| {
        frame.render_widget(
            ThemedPseudoTerminal {
                screen,
                palette: av.pty.palette(),
            },
            area,
        );
    });
}

/// Shared with the tail pane and the video wall, which both render an already-attached PTY
/// the same themed way.
pub(super) struct ThemedPseudoTerminal<'a> {
    pub(super) screen: &'a vt100::Screen,
    pub(super) palette: Option<TerminalPalette>,
}

impl Widget for ThemedPseudoTerminal<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        PseudoTerminal::new(self.screen).render(area, buf);
        let Some(palette) = self.palette else {
            return;
        };

        for row in 0..area.height {
            for col in 0..area.width {
                let Some(screen_cell) = self.screen.cell(row, col) else {
                    continue;
                };
                let cell = &mut buf[(area.x + col, area.y + row)];
                if screen_cell.fgcolor() == vt100::Color::Default {
                    cell.set_fg(Color::Rgb(
                        palette.foreground[0],
                        palette.foreground[1],
                        palette.foreground[2],
                    ));
                }
                if screen_cell.bgcolor() == vt100::Color::Default {
                    cell.set_bg(Color::Rgb(
                        palette.background[0],
                        palette.background[1],
                        palette.background[2],
                    ));
                }
            }
        }

        if !self.screen.hide_cursor() {
            let (row, col) = Screen::cursor_position(self.screen);
            if row < area.height
                && col < area.width
                && self
                    .screen
                    .cell(row, col)
                    .is_some_and(|cell| !cell.has_contents())
            {
                buf[(area.x + col, area.y + row)].set_fg(Color::Gray);
            }
        }
    }
}

fn draw_header(
    frame: &mut Frame,
    session: &agent_viewer_core::Session,
    exited: bool,
    now_ms: i64,
    context: &HeaderContext<'_>,
    area: Rect,
) {
    let (glyph, glyph_color) = status_glyph(&session.status, now_ms, context.theme);
    let right = if exited {
        "ctrl+y send copy request to terminal · process exited · press any key"
    } else {
        "ctrl+y send copy request to terminal · ← back · ctrl+] close"
    };
    let use_logo = context.logos.is_some() && logo_marks();
    let mark = if use_logo {
        "  "
    } else {
        mark_for(session.backend, true)
    };
    let prefix = format!(" {glyph} {mark} ");
    let pr = list::pr_badge(&session.pr_refs);
    let pr_chip = (!pr.is_empty()).then(|| format!(" {pr} "));
    let width = area.width as usize;
    let pr_width = pr_chip.as_ref().map_or(0, |chip| chip.chars().count());
    let right_width = right.chars().count();
    let available = width.saturating_sub(right_width + 1);
    let fixed_width = prefix.chars().count() + pr_width;
    let title = truncate(&session.title, available.saturating_sub(fixed_width));
    let used = fixed_width + title.chars().count();
    let cwd = truncate(
        &format!("  {}", session.cwd.display()),
        available.saturating_sub(used),
    );
    let pad = width.saturating_sub(used + cwd.chars().count() + right_width);
    let line = Line::from(vec![
        Span::styled(format!(" {glyph} "), fg(glyph_color)),
        Span::styled(mark.to_string(), fg(context.theme.text)),
        Span::raw(" "),
        Span::styled(title, fg(context.theme.text)),
        Span::styled(
            pr_chip.unwrap_or_default(),
            fg(list::pr_badge_theme_color(
                context.pr_status.badge_color(&session.pr_refs),
                context.theme,
            )),
        ),
        Span::styled(cwd, fg(context.theme.muted)),
        Span::raw(" ".repeat(pad)),
        Span::styled(
            right.to_string(),
            fg(if exited {
                context.theme.err
            } else {
                context.theme.muted
            }),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(context.theme.selbg)),
        area,
    );
    if let Some(logos) = context.logos.filter(|_| use_logo)
        && area.width >= 5
    {
        frame.render_widget(
            Image::new(logos.image(session.backend)),
            Rect {
                x: area.x + 3,
                y: area.y,
                width: mark_width(mark) as u16,
                height: 1,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_viewer_core::pty::{PtySession, PtySpec};
    use agent_viewer_core::{BackendKind, Session, SessionOrigin, Status};
    use ratatui::backend::TestBackend;

    fn attached_session() -> Session {
        Session {
            backend: BackendKind::Codex,
            id: "footer notice".to_string(),
            short_id: None,
            origin: SessionOrigin::Interactive,
            title: "footer notice".to_string(),
            cwd: "/tmp".into(),
            git_branch: None,
            status: Status::Working,
            created_at_ms: 0,
            updated_at_ms: 0,
            hidden: false,
            companion: false,
            subagent: false,
            summary: String::new(),
            pid: None,
            rollout_path: None,
            pr_refs: Vec::new(),
            daemon_hosted: true,
        }
    }

    #[test]
    fn attached_failure_notice_renders_in_the_reserved_bottom_row() {
        let mut pty = PtySession::spawn(PtySpec {
            program: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "printf 'transcript body'; sleep 30".to_string(),
            ],
            cwd: None,
            envs: Vec::new(),
            rows: 6,
            cols: 40,
            palette: None,
            scrollback_rows: 0,
        })
        .expect("attached child");
        let session = attached_session();
        let theme = crate::ui::theme::amber(false);
        let notice = "clipboard denied; use ctrl+t";
        let mut terminal = ratatui::Terminal::new(TestBackend::new(40, 8)).unwrap();

        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &AttachView {
                        session: &session,
                        pty: &pty,
                        exited: false,
                    },
                    notice,
                    0,
                    &crate::pr_cache::PrStatusCache::new(),
                    None,
                    &theme,
                );
            })
            .expect("draw attached failure");

        let buffer = terminal.backend().buffer();
        let bottom = (0..buffer.area.width)
            .map(|column| buffer[(column, buffer.area.height - 1)].symbol())
            .collect::<String>();
        assert!(bottom.contains(notice), "bottom row was {bottom:?}");
        let notice_column = bottom.find("clipboard").unwrap() as u16;
        assert_eq!(
            buffer[(notice_column, buffer.area.height - 1)].fg,
            theme.warn
        );
        assert!(
            !(0..buffer.area.width)
                .map(|column| buffer[(column, buffer.area.height - 2)].symbol())
                .collect::<String>()
                .contains(notice),
            "notice must not overwrite transcript rows"
        );
        pty.kill();
    }

    #[test]
    fn attached_header_uses_backend_glyphs() {
        let theme = crate::ui::theme::amber(false);
        let mut terminal = ratatui::Terminal::new(TestBackend::new(80, 1)).unwrap();

        for (backend, expected) in [(BackendKind::Claude, "✳"), (BackendKind::Codex, "◆")] {
            let mut session = attached_session();
            session.backend = backend;
            terminal
                .draw(|frame| {
                    draw_header(
                        frame,
                        &session,
                        false,
                        0,
                        &HeaderContext {
                            pr_status: &crate::pr_cache::PrStatusCache::new(),
                            logos: None,
                            theme: &theme,
                        },
                        frame.area(),
                    )
                })
                .expect("draw attached header");

            let header = (0..terminal.backend().buffer().area.width)
                .map(|column| terminal.backend().buffer()[(column, 0)].symbol())
                .collect::<String>();
            assert!(header.contains(expected), "header was {header:?}");
            assert!(!header.contains(backend.tag()), "header was {header:?}");
        }
    }

    #[test]
    fn attached_header_shows_the_session_pr_chip() {
        let theme = crate::ui::theme::amber(false);
        let mut session = attached_session();
        session.pr_refs.push(agent_viewer_core::PrRef {
            id: "123".to_string(),
            href: None,
        });
        let mut terminal = ratatui::Terminal::new(TestBackend::new(120, 1)).unwrap();

        terminal
            .draw(|frame| {
                draw_header(
                    frame,
                    &session,
                    false,
                    0,
                    &HeaderContext {
                        pr_status: &crate::pr_cache::PrStatusCache::new(),
                        logos: None,
                        theme: &theme,
                    },
                    frame.area(),
                )
            })
            .expect("draw attached header");

        let header = (0..terminal.backend().buffer().area.width)
            .map(|column| terminal.backend().buffer()[(column, 0)].symbol())
            .collect::<String>();
        let pr_at = header.find("#123").expect("PR chip in header");
        assert!(pr_at < 50, "PR chip was not in left metadata: {header:?}");
    }
}
