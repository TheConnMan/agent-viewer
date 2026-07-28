use super::{AttachView, fg, status_glyph, truncate};
use crate::ui::theme::Theme;
use agent_viewer_core::pty::TerminalPalette;
use ratatui::buffer::Buffer;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use tui_term::{
    vt100,
    widget::{PseudoTerminal, Screen},
};

pub(super) fn draw(frame: &mut Frame, av: AttachView, now_ms: i64, theme: &Theme) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);

    draw_header(frame, av.session, av.exited, now_ms, theme, chunks[0]);
    av.pty.with_screen(|screen| {
        frame.render_widget(
            ThemedPseudoTerminal {
                screen,
                palette: av.pty.palette(),
            },
            chunks[1],
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
