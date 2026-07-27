use super::{fg, theme::Theme};
use crate::app::App;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::path::Path;

pub(super) fn draw(frame: &mut Frame, app: &App, workspace: &Path, theme: &Theme, area: Rect) {
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
            Span::styled(" [av] ", fg(theme.accent)),
            Span::styled(
                "Agent Viewer",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" v{}", env!("CARGO_PKG_VERSION")), fg(theme.muted)),
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
