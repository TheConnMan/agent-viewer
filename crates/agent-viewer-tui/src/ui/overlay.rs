use super::{display_width, fg};
use crate::app::Composer;
use crate::ui::theme::{Theme, ThemeState};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

pub(super) fn popup_area(
    composer: &Composer,
    themes: &ThemeState,
    composer_area: Rect,
) -> Option<Rect> {
    if themes.picker_open() {
        return theme_picker_area(themes, composer_area);
    }
    let count = if composer.is_model_command() {
        composer.model_suggestions().len()
    } else {
        composer.suggestions().len()
    };
    let height = (count as u16).min(composer_area.y);
    (height > 0).then_some(Rect {
        x: composer_area.x,
        y: composer_area.y - height,
        width: composer_area.width,
        height,
    })
}

pub(super) fn draw_suggestions<S: AsRef<str>>(
    frame: &mut Frame,
    suggestions: &[S],
    highlight: usize,
    prefix: &str,
    theme: &Theme,
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
    let items = suggestions
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let color = if index == highlight {
                theme.accent
            } else {
                theme.muted
            };
            ListItem::new(Line::from(Span::styled(
                format!(" {prefix}{}", item.as_ref()),
                fg(color),
            )))
        })
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items), area);
}

pub(super) fn draw_theme_picker(frame: &mut Frame, themes: &ThemeState, composer_area: Rect) {
    let Some(area) = theme_picker_area(themes, composer_area) else {
        return;
    };
    let active = themes.active();
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(fg(active.accent))
        .style(Style::default().bg(active.surface).fg(active.text));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let preview = "preview is the screen behind";
    let left = "◧  /theme";
    let pad = (inner.width as usize).saturating_sub(display_width(left) + display_width(preview));
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(left, fg(active.accent)),
            Span::raw(" ".repeat(pad)),
            Span::styled(preview, fg(active.faint)),
        ])),
        Rect { height: 1, ..inner },
    );

    let rows_height = themes
        .themes()
        .len()
        .min(inner.height.saturating_sub(2) as usize) as u16;
    for (index, candidate) in themes
        .themes()
        .iter()
        .take(rows_height as usize)
        .enumerate()
    {
        let selected = index == themes.active_index();
        let caret = if selected { "▸ " } else { "  " };
        let name = format!("{:<20}", candidate.name);
        let swatches = [
            candidate.bg,
            candidate.surface,
            candidate.text,
            candidate.accent,
            candidate.ok,
            candidate.warn,
            candidate.err,
            candidate.cc,
            candidate.cx,
            candidate.oc,
        ];
        let used = display_width(caret) + display_width(&name) + 1 + swatches.len();
        let pad = (inner.width as usize).saturating_sub(used + display_width(&candidate.tag));
        let mut spans = vec![
            Span::styled(
                caret,
                fg(if selected {
                    active.accent
                } else {
                    active.muted
                }),
            ),
            Span::styled(name, fg(if selected { active.selfg } else { active.muted })),
            Span::raw(" "),
        ];
        spans.extend(
            swatches
                .into_iter()
                .map(|color| Span::styled("█", fg(color))),
        );
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(candidate.tag.clone(), fg(active.faint)));
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(if selected {
                Style::default().bg(active.selbg)
            } else {
                Style::default()
            }),
            Rect {
                x: inner.x,
                y: inner.y + 1 + index as u16,
                width: inner.width,
                height: 1,
            },
        );
    }

    let footer_y = inner.y + 1 + rows_height;
    let footer = Rect {
        x: inner.x,
        y: footer_y,
        width: inner.width,
        height: inner
            .y
            .saturating_add(inner.height)
            .saturating_sub(footer_y),
    };
    frame.render_widget(
        Paragraph::new("⏎ commit · esc revert · marks and motion travel with the theme")
            .style(fg(active.faint))
            .wrap(Wrap { trim: false }),
        footer,
    );
}

fn theme_picker_area(themes: &ThemeState, composer_area: Rect) -> Option<Rect> {
    let width = 52.min(composer_area.width);
    let desired_height = themes.themes().len() as u16 + 5;
    let height = desired_height.min(composer_area.y);
    (width > 2 && height > 2).then_some(Rect {
        x: composer_area.x,
        y: composer_area.y - height,
        width,
        height,
    })
}

pub(super) fn draw_help(frame: &mut Frame, theme: &Theme, area: Rect) {
    let popup = centered_rect(75, 100, area);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(fg(theme.accent))
        .title("keys");
    let entries = [
        ("↑/↓", "move selection"),
        ("→ / Enter", "attach selected (empty composer)"),
        ("type · Tab", "compose task · switch agent"),
        ("Shift+Tab", "cycle agent model"),
        ("/model", "pick from all available models"),
        ("/theme", "preview and select a theme"),
        ("Enter", "spawn composed task"),
        // Entries are paired onto single lines where they are related: the list already
        // fills an 80x24 popup, and a 23rd line pushes Ctrl+C off the bottom.
        ("← / Ctrl+]", "back to the list (input line empty / always)"),
        ("Enter/Space", "toggle group on a header"),
        // Kept short deliberately: at 80x24 the popup is 58 columns wide, and a description
        // that wraps to a second line costs a row exactly like a 23rd entry would.
        ("Ctrl+K / Ctrl+N", "palette / triage queue"),
        ("Ctrl+B / Ctrl+W", "tail pane / video wall"),
        ("Ctrl+R", "rename in row"),
        ("Ctrl+X", "stop, then press again to remove"),
        ("Ctrl+S", "group by state / by project"),
        ("Ctrl+A", "show all (companions + archived)"),
        ("Ctrl+D / Ctrl+U", "archive / unarchive"),
        ("Ctrl+F", "filter (searches hidden too)"),
        ("Ctrl+G", "cycle the header sprite"),
        ("Ctrl+Y", "send visible copy request to terminal"),
        ("Ctrl+T", "optional terminal selection mode"),
        ("?", "this help"),
        ("Ctrl+C", "quit"),
    ];
    let lines = entries
        .into_iter()
        .map(|(key, value)| {
            Line::from(vec![
                Span::styled(format!("  {key:<12}"), fg(theme.accent)),
                Span::styled(value.to_string(), fg(theme.text)),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        popup,
    );
}

pub(super) fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
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
