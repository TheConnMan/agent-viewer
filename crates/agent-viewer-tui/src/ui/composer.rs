use super::{
    ReplyModal, backend_mark, backend_mark_color, display_width, fg, logo_marks, theme::Theme,
};
use crate::app::{App, Composer};
use crate::logos::LogoMarks;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui_image::Image;
use std::path::Path;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(super) const MAX_LINES: u16 = 10;
const INPUT_GUTTER: usize = 5;
pub(super) const COMPOSER_HINT: &str = "⇥ agent · ⇧⇥ model · ⏎ spawn";

pub(super) fn input_inner_width(frame_width: u16) -> u16 {
    frame_width.saturating_sub(2)
}

fn input_block(border: ratatui::style::Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(fg(border))
}

fn grapheme_width(grapheme: &str) -> usize {
    UnicodeWidthStr::width(grapheme)
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

    let mut graphemes = UnicodeSegmentation::graphemes(paragraph, true).peekable();
    while let Some(&grapheme) = graphemes.peek() {
        if grapheme.chars().all(char::is_whitespace) {
            graphemes.next();
            gap.push_str(grapheme);
            gap_width += grapheme_width(grapheme);
            continue;
        }
        let mut word = String::new();
        let mut word_width = 0usize;
        while let Some(&grapheme) = graphemes.peek() {
            if grapheme.chars().all(char::is_whitespace) {
                break;
            }
            graphemes.next();
            word.push_str(grapheme);
            word_width += grapheme_width(grapheme);
        }
        if !current.is_empty() && current_width + gap_width + word_width > budget {
            break_line!();
        } else {
            current.push_str(&gap);
            current_width += gap_width;
        }
        gap.clear();
        gap_width = 0;
        for grapheme in UnicodeSegmentation::graphemes(word.as_str(), true) {
            let width = grapheme_width(grapheme);
            if current_width + width > budget && !current.is_empty() {
                break_line!();
            }
            current.push_str(grapheme);
            current_width += width;
        }
    }
    for grapheme in UnicodeSegmentation::graphemes(gap.as_str(), true) {
        let width = grapheme_width(grapheme);
        if current_width + width > budget && !current.is_empty() {
            break_line!();
        }
        current.push_str(grapheme);
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

pub(super) fn box_height_for_viewport(text: &str, inner_width: u16, viewport_height: u16) -> u16 {
    let input_lines = if inner_width == 0 || text.is_empty() {
        1usize
    } else {
        let width = inner_width as usize;
        let first = width.saturating_sub(INPUT_GUTTER);
        let segments = wrap_by_width(text, first, width);
        let final_budget = if segments.len() == 1 {
            first.max(1)
        } else {
            width.max(1)
        };
        segments.len() + usize::from(display_width(segments.last().unwrap()) == final_budget)
    };
    let desired = input_lines.max(1) as u16 + 3;
    let cap = (viewport_height / 3).max(4);
    desired.min(cap)
}

pub(super) fn caret_visible(now_ms: i64, animation: bool) -> bool {
    !animation || now_ms.rem_euclid(1_100) < 550
}

struct MetadataLayout {
    folder: String,
    hint: Option<String>,
}

fn metadata_layout(
    mark: &str,
    agent: &str,
    model: &str,
    folder: &str,
    width: usize,
) -> MetadataLayout {
    let fixed_width = display_width(mark).max(INPUT_GUTTER)
        + display_width(agent)
        + display_width(" · ")
        + display_width(model)
        + display_width(" · ");
    let folder_width = display_width(folder);
    let hint_width = display_width(COMPOSER_HINT);
    let hint =
        (fixed_width + folder_width + hint_width <= width).then(|| COMPOSER_HINT.to_string());
    let folder = if hint.is_some() {
        folder.to_string()
    } else {
        truncate_from_left(folder, width.saturating_sub(fixed_width))
    };
    MetadataLayout { folder, hint }
}

fn truncate_from_left(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }

    let mut tail = Vec::new();
    let mut tail_width = 0;
    for grapheme in UnicodeSegmentation::graphemes(text, true).rev() {
        let grapheme_width = grapheme_width(grapheme);
        if tail_width + grapheme_width > width - 1 {
            break;
        }
        tail.push(grapheme);
        tail_width += grapheme_width;
    }
    tail.reverse();
    format!("…{}", tail.concat())
}

fn field(text: &str, width: usize) -> String {
    let padding = width.saturating_sub(display_width(text));
    format!("{text}{}", " ".repeat(padding))
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
    let mark = backend_mark(backend, theme);
    let directory = app
        .spawn_target()
        .map(|directory| abbreviate_dir(&directory))
        .unwrap_or_default();
    let model = composer.model();

    let block = input_block(theme.border).style(Style::default().bg(theme.surface).fg(theme.text));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let layout = metadata_layout(
        mark,
        backend.name(),
        model,
        &directory,
        inner.width as usize,
    );
    let mut metadata = vec![
        Span::styled(
            field(mark, INPUT_GUTTER),
            fg(backend_mark_color(backend, theme)),
        ),
        Span::styled(backend.name().to_string(), fg(theme.text)),
        Span::styled(" · ", fg(theme.faint)),
        Span::styled(model.to_string(), fg(theme.muted)),
        Span::styled(" · ", fg(theme.faint)),
        Span::styled(layout.folder.clone(), fg(theme.muted)),
    ];
    if let Some(hint) = layout.hint {
        let used = INPUT_GUTTER
            + display_width(backend.name())
            + display_width(" · ")
            + display_width(model)
            + display_width(" · ")
            + display_width(&layout.folder)
            + display_width(&hint);
        metadata.push(Span::raw(
            " ".repeat((inner.width as usize).saturating_sub(used)),
        ));
        metadata.push(Span::styled(hint, fg(theme.faint)));
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
        let prefix = Span::styled(field("❯", INPUT_GUTTER), fg(theme.accent));
        if composer.is_empty() {
            let mut spans = vec![prefix, Span::styled("describe a task", fg(theme.muted))];
            if show_cursor {
                spans.push(Span::styled("▏", fg(theme.accent)));
            }
            frame.render_widget(Paragraph::new(Line::from(spans)), input);
            if show_cursor {
                let column = INPUT_GUTTER + display_width("describe a task");
                let x = input.x + (column as u16).min(input.width - 1);
                frame.set_cursor_position((x, input.y));
            }
        } else {
            let width = input.width as usize;
            let first = width.saturating_sub(INPUT_GUTTER);
            let mut segments = wrap_by_width(composer.text(), first, width);
            let final_budget = if segments.len() == 1 {
                first.max(1)
            } else {
                width.max(1)
            };
            if display_width(segments.last().unwrap()) == final_budget {
                segments.push(String::new());
            }
            let start = segments.len().saturating_sub(input.height as usize);
            let last = segments.len() - 1;
            let lines = segments[start..]
                .iter()
                .enumerate()
                .map(|(visible_index, segment)| {
                    let index = start + visible_index;
                    let mut spans = Vec::new();
                    if index == 0 {
                        spans.push(prefix.clone());
                    }
                    spans.push(Span::styled(segment.clone(), fg(theme.text)));
                    if show_cursor && index == last {
                        spans.push(Span::styled("▏", fg(theme.accent)));
                    }
                    Line::from(spans)
                })
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(lines), input);
            if show_cursor {
                let last_width = display_width(segments.last().unwrap()) as u16;
                let prefix_width = u16::from(segments.len() == 1) * INPUT_GUTTER as u16;
                let x = input.x + (prefix_width + last_width).min(input.width - 1);
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
    let home = agent_viewer_core::home_dir();
    let home = home.display().to_string();
    if !home.is_empty()
        && let Some(rest) = shown.strip_prefix(&home)
    {
        return format!("~{rest}");
    }
    shown
}

#[cfg(test)]
mod design_tests {
    use super::{COMPOSER_HINT, box_height_for_viewport, metadata_layout, wrap_by_width};

    #[test]
    fn task_buffer_rewraps_when_the_width_changes() {
        let text = "alpha beta gamma delta";

        assert_eq!(
            wrap_by_width(text, 12, 12),
            vec!["alpha beta", "gamma delta"]
        );
        assert_eq!(
            wrap_by_width(text, 7, 7),
            vec!["alpha", "beta", "gamma", "delta"]
        );
    }

    #[test]
    fn oversized_word_hard_breaks_at_display_width() {
        assert_eq!(
            wrap_by_width("abcdefghijk", 4, 4),
            vec!["abcd", "efgh", "ijk"]
        );
    }

    #[test]
    fn wide_characters_wrap_by_terminal_cells() {
        assert_eq!(wrap_by_width("一二三四", 5, 5), vec!["一二", "三四"]);
        assert_eq!(wrap_by_width("🙂🙂🙂", 5, 5), vec!["🙂🙂", "🙂"]);
    }

    #[test]
    fn caret_blinks_only_when_animation_is_enabled() {
        assert!(super::caret_visible(0, true));
        assert!(!super::caret_visible(550, true));
        assert!(super::caret_visible(1_100, true));
        assert!(super::caret_visible(550, false));
    }

    #[test]
    fn composer_height_grows_and_stops_at_one_third_of_the_viewport() {
        assert_eq!(box_height_for_viewport("", 20, 30), 4);
        assert_eq!(
            box_height_for_viewport("one two three four five six", 10, 30),
            7
        );
        assert_eq!(box_height_for_viewport(&"x".repeat(200), 10, 18), 6);
    }

    #[test]
    fn metadata_drops_hint_before_truncating_folder_from_the_left() {
        let wide = metadata_layout("cx", "codex", "gpt 5.3", "~/git/curietech/agentos", 76);
        assert_eq!(wide.hint.as_deref(), Some(COMPOSER_HINT));
        assert_eq!(wide.folder, "~/git/curietech/agentos");

        let without_hint = metadata_layout("cx", "codex", "gpt 5.3", "~/git/curietech/agentos", 52);
        assert_eq!(without_hint.hint, None);
        assert_eq!(without_hint.folder, "~/git/curietech/agentos");

        let narrow = metadata_layout("cx", "codex", "gpt 5.3", "~/git/curietech/agentos", 32);
        assert_eq!(narrow.hint, None);
        assert!(narrow.folder.starts_with('…'));
        assert!(narrow.folder.ends_with("/agentos"));
    }
}
