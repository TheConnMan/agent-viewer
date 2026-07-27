use super::{
    ReplyModal, backend_mark, backend_mark_color, display_width, fg, logo_marks, theme::Theme,
};
use crate::app::{App, Composer};
use crate::logos::LogoMarks;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui_image::Image;
use std::path::Path;

pub(super) const MAX_LINES: u16 = 10;

pub(super) fn input_inner_width(frame_width: u16) -> u16 {
    frame_width.saturating_sub(2)
}

fn input_block(border: ratatui::style::Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(fg(border))
}

fn char_width(character: char) -> usize {
    use unicode_width::UnicodeWidthChar;
    UnicodeWidthChar::width(character).unwrap_or(0)
}

pub(super) fn wrap_by_width(text: &str, first: usize, rest: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let rest = rest.max(1);
    for paragraph in text.split('\n') {
        let budget = if lines.is_empty() { first.max(1) } else { rest };
        wrap_paragraph(paragraph, budget, rest, &mut lines);
    }
    lines
}

fn wrap_paragraph(paragraph: &str, first: usize, rest: usize, output: &mut Vec<String>) {
    let mut current = String::new();
    let mut current_width = 0usize;
    let mut budget = first;
    let mut gap = String::new();
    let mut gap_width = 0usize;

    macro_rules! break_line {
        () => {{
            output.push(std::mem::take(&mut current));
            current_width = 0;
            budget = rest;
        }};
    }

    let mut characters = paragraph.chars().peekable();
    while let Some(&character) = characters.peek() {
        if character.is_whitespace() {
            characters.next();
            gap.push(character);
            gap_width += char_width(character);
            continue;
        }
        let mut word = String::new();
        let mut word_width = 0usize;
        while let Some(&character) = characters.peek() {
            if character.is_whitespace() {
                break;
            }
            characters.next();
            word.push(character);
            word_width += char_width(character);
        }
        if !current.is_empty() && current_width + gap_width + word_width > budget {
            break_line!();
        } else {
            current.push_str(&gap);
            current_width += gap_width;
        }
        gap.clear();
        gap_width = 0;
        for character in word.chars() {
            let width = char_width(character);
            if current_width + width > budget && !current.is_empty() {
                break_line!();
            }
            current.push(character);
            current_width += width;
        }
    }
    for character in gap.chars() {
        let width = char_width(character);
        if current_width + width > budget && !current.is_empty() {
            break_line!();
        }
        current.push(character);
        current_width += width;
    }
    output.push(current);
}

pub(super) fn input_box_height(prefix_width: usize, text: &str, inner_width: u16) -> u16 {
    if inner_width == 0 || text.is_empty() {
        return 3;
    }
    let width = inner_width as usize;
    let first = width.saturating_sub(prefix_width);
    let lines = wrap_by_width(text, first, width).len();
    (lines as u16).clamp(1, MAX_LINES) + 2
}

pub(super) fn box_height(text: &str, inner_width: u16) -> u16 {
    let input_lines = if inner_width == 0 || text.is_empty() {
        1usize
    } else {
        let segments = wrap_by_width(text, inner_width as usize, inner_width as usize);
        segments.len()
            + usize::from(display_width(segments.last().unwrap()) == inner_width as usize)
    };
    input_lines.clamp(1, MAX_LINES as usize) as u16 + 3
}

pub(super) struct InputBox {
    pub border: ratatui::style::Color,
    pub prefix_spans: Vec<Span<'static>>,
    pub prefix_width: usize,
    pub placeholder: &'static str,
}

pub(super) fn draw_input_box(
    frame: &mut Frame,
    area: Rect,
    style: InputBox,
    text: &str,
    show_cursor: bool,
    theme: &Theme,
) -> (Rect, bool) {
    let block = input_block(style.border);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return (inner, false);
    }

    if text.is_empty() {
        let mut spans = style.prefix_spans;
        spans.push(Span::styled(style.placeholder.to_string(), fg(theme.faint)));
        frame.render_widget(Paragraph::new(Line::from(spans)), inner);
        if show_cursor {
            let x = inner.x + (style.prefix_width as u16).min(inner.width - 1);
            frame.set_cursor_position((x, inner.y));
        }
        return (inner, true);
    }

    let width = inner.width as usize;
    let first = width.saturating_sub(style.prefix_width);
    let segments = wrap_by_width(text, first, width);
    let total = segments.len();
    let start = total.saturating_sub(inner.height as usize);
    let mut lines = Vec::with_capacity(total - start);
    for (index, segment) in segments[start..].iter().enumerate() {
        if start + index == 0 {
            let mut spans = style.prefix_spans.clone();
            spans.push(Span::styled(segment.clone(), fg(theme.text)));
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::from(Span::styled(segment.clone(), fg(theme.text))));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);

    if show_cursor {
        let last_width = display_width(segments.last().unwrap());
        let column = if total == 1 {
            style.prefix_width + last_width
        } else {
            last_width
        };
        let row = (total - start - 1) as u16;
        let x = inner.x + (column as u16).min(inner.width - 1);
        frame.set_cursor_position((x, inner.y + row));
    }
    (inner, start == 0)
}

pub(super) fn draw(
    frame: &mut Frame,
    app: &App,
    composer: &Composer,
    logos: Option<&LogoMarks>,
    theme: &Theme,
    area: Rect,
    show_cursor: bool,
) {
    let backend = composer.backend();
    let directory = app
        .spawn_target()
        .map(|directory| abbreviate_dir(&directory))
        .unwrap_or_default();
    let model = composer.model();
    let model_segment = if model == "default" {
        String::new()
    } else {
        format!("{model} ")
    };
    let mut metadata = vec![Span::styled(
        format!("{}{} ", backend_mark(backend, theme), backend.name()),
        fg(backend_mark_color(backend, theme)),
    )];
    if !model_segment.is_empty() {
        metadata.push(Span::styled(model_segment, fg(theme.muted)));
    }
    metadata.push(Span::styled(directory, fg(theme.muted)));

    let block = input_block(theme.border);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::from(metadata)),
        Rect { height: 1, ..inner },
    );

    let input = Rect {
        y: inner.y.saturating_add(1),
        height: inner.height.saturating_sub(1),
        ..inner
    };
    if input.height > 0 {
        if composer.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "describe a task · tab to switch agent",
                    fg(theme.faint),
                ))),
                input,
            );
            if show_cursor {
                frame.set_cursor_position((input.x, input.y));
            }
        } else {
            let width = input.width as usize;
            let mut segments = wrap_by_width(composer.text(), width, width);
            if display_width(segments.last().unwrap()) == width {
                segments.push(String::new());
            }
            let start = segments.len().saturating_sub(input.height as usize);
            let lines = segments[start..]
                .iter()
                .map(|segment| Line::from(Span::styled(segment.clone(), fg(theme.text))))
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(lines), input);
            if show_cursor {
                let last_width = display_width(segments.last().unwrap()) as u16;
                let x = input.x + last_width.min(input.width - 1);
                let y = input.y + (segments.len() - start - 1) as u16;
                frame.set_cursor_position((x, y));
            }
        }
    }

    if let Some(logos) = logos
        && logo_marks()
        && inner.width >= 2
    {
        frame.render_widget(
            Image::new(logos.image(backend)),
            Rect {
                x: inner.x,
                y: inner.y,
                width: 2,
                height: 1,
            },
        );
    }
}

pub(super) fn draw_reply(
    frame: &mut Frame,
    modal: &ReplyModal,
    theme: &Theme,
    area: Rect,
    session_title: &str,
) {
    let title_segment = if session_title.is_empty() {
        String::new()
    } else {
        format!("{session_title} ")
    };
    let mut prefix = vec![Span::styled("↳ reply ", fg(theme.accent))];
    if !title_segment.is_empty() {
        prefix.push(Span::styled(title_segment.clone(), fg(theme.muted)));
    }
    prefix.push(Span::styled("❯ ", fg(theme.accent)));
    let prefix_width = display_width(&format!("↳ reply {title_segment}❯ "));

    draw_input_box(
        frame,
        area,
        InputBox {
            border: theme.accent,
            prefix_spans: prefix,
            prefix_width,
            placeholder: "type a reply · Enter send · Esc cancel",
        },
        &modal.buffer,
        true,
        theme,
    );
}

fn abbreviate_dir(directory: &Path) -> String {
    let shown = directory.display().to_string();
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
        && let Some(rest) = shown.strip_prefix(&home)
    {
        return format!("~{rest}");
    }
    shown
}
