use super::{AttachView, fg, status_glyph, truncate};
use crate::ui::theme::Theme;
use agent_viewer_core::pty::TerminalPalette;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use tui_term::{
    vt100,
    widget::{PseudoTerminal, Screen},
};

pub const ATTACHED_CHROME_ROWS: u16 = 2;

pub(super) fn draw(
    frame: &mut Frame,
    av: &AttachView<'_>,
    notice: &str,
    now_ms: i64,
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

    draw_header(frame, av.session, av.exited, now_ms, theme, chunks[0]);
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

struct ThemedPseudoTerminal<'a> {
    screen: &'a vt100::Screen,
    palette: Option<TerminalPalette>,
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
    theme: &Theme,
    area: Rect,
) {
    let (glyph, glyph_color) = status_glyph(&session.status, now_ms, theme);
    let right = if exited {
        "ctrl+y send copy request to terminal · process exited · press any key"
    } else {
        "ctrl+y send copy request to terminal · ← back · ctrl+] detach"
    };
    let left = format!(
        " {glyph} {} {}  {}",
        session.backend.tag(),
        session.title,
        session.cwd.display()
    );
    let width = area.width as usize;
    let right_width = right.chars().count();
    let left = truncate(&left, width.saturating_sub(right_width + 1));
    let pad = width.saturating_sub(left.chars().count() + right_width);
    let line = Line::from(vec![
        Span::styled(format!(" {glyph}"), fg(glyph_color)),
        Span::styled(left.chars().skip(2).collect::<String>(), fg(theme.text)),
        Span::raw(" ".repeat(pad)),
        Span::styled(
            right.to_string(),
            fg(if exited { theme.err } else { theme.muted }),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(theme.selbg)),
        area,
    );
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
}
