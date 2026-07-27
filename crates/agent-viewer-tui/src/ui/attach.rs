use super::{AttachView, fg, status_glyph, truncate};
use crate::ui::theme::Theme;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tui_term::widget::PseudoTerminal;

pub(super) fn draw(frame: &mut Frame, av: AttachView, now_ms: i64, theme: &Theme) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);

    draw_header(frame, av.session, av.exited, now_ms, theme, chunks[0]);
    av.pty.with_screen(|screen| {
        frame.render_widget(PseudoTerminal::new(screen), chunks[1]);
    });
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
