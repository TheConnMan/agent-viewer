use super::{
    Pulses, backend_mark, backend_mark_color, bloom_glyph, display_width, fg, mark_width,
    status_glyph,
};
use crate::app::{Row, Section};
use crate::ui::theme::Theme;
use agent_viewer_core::{BackendKind, PrBadgeColor, PrRef, Status};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::ListItem;

pub(super) fn rename_row_item(
    backend: BackendKind,
    shown: &str,
    theme: &Theme,
) -> ListItem<'static> {
    ListItem::new(Line::from(vec![
        Span::styled("✎", fg(theme.accent)),
        Span::styled(
            backend_mark(backend, theme).to_string(),
            fg(backend_mark_color(backend, theme)),
        ),
        Span::styled(shown.to_string(), fg(theme.accent)),
    ]))
}

pub(super) fn rename_buffer(
    backend: BackendKind,
    buffer: &str,
    width: usize,
    theme: &Theme,
) -> String {
    let prefix_width = display_width("✎") + mark_width(backend_mark(backend, theme));
    let buffer_width = width.saturating_sub(prefix_width.saturating_add(1));
    truncate_display_width(buffer, buffer_width)
}

fn truncate_display_width(text: &str, width: usize) -> String {
    let mut end = 0;
    for (index, character) in text.char_indices() {
        let next = index + character.len_utf8();
        if display_width(&text[..next]) > width {
            break;
        }
        end = next;
    }
    text[..end].to_string()
}

pub(super) fn row_to_item(
    row: &Row,
    pulses: &Pulses,
    now_ms: i64,
    pr_status: &crate::pr_cache::PrStatusCache,
    width: usize,
    title_width: usize,
    theme: &Theme,
) -> ListItem<'static> {
    match row {
        Row::Spacer => ListItem::new(Line::from("")),
        Row::SectionHeader {
            section,
            count,
            collapsed,
        } => ListItem::new(Line::from(Span::styled(
            header_label(section_label(*section), *count, *collapsed),
            fg(theme.accent).add_modifier(Modifier::BOLD),
        ))),
        Row::ProjectHeader {
            root,
            count,
            collapsed,
        } => ListItem::new(Line::from(Span::styled(
            header_label(root.display(), *count, *collapsed),
            fg(theme.accent).add_modifier(Modifier::BOLD),
        ))),
        Row::Session {
            backend,
            id,
            summary,
            status,
            title,
            created_at_ms,
            updated_at_ms,
            pr_refs,
            ..
        } => {
            let bloom = if theme.animation {
                pulses
                    .iter()
                    .find(|((candidate, pulse_id), _)| candidate == backend && pulse_id == id)
                    .and_then(|(_, start)| bloom_glyph(now_ms - *start))
            } else {
                None
            };
            let (glyph, glyph_color) = match bloom {
                Some(glyph) => (glyph, theme.accent),
                None => status_glyph(status, now_ms, theme),
            };
            let started_at_ms = if *created_at_ms > 0 {
                *created_at_ms
            } else {
                *updated_at_ms
            };
            let elapsed = crate::app::format_elapsed(now_ms - started_at_ms);
            let pr_color = pr_badge_theme_color(pr_status.badge_color(pr_refs), theme);
            let line = session_line(
                SessionRow {
                    glyph,
                    glyph_color,
                    mark: backend_mark(*backend, theme),
                    mark_color: backend_mark_color(*backend, theme),
                    name: title,
                    status,
                    summary,
                    pr: &pr_badge(pr_refs),
                    pr_color,
                    elapsed: &elapsed,
                    width,
                    title_width,
                },
                theme,
            );
            if bloom.is_some() {
                ListItem::new(line).style(Style::default().bg(theme.selbg))
            } else {
                ListItem::new(line)
            }
        }
    }
}

pub(super) fn status_display_word(status: &Status) -> &'static str {
    match status {
        Status::Working => "Working",
        Status::NeedsInput { .. } => "Needs input",
        Status::Idle => "Idle",
        Status::Done => "Done",
        Status::Error => "Error",
        Status::Unknown => "Unknown",
    }
}

fn status_color(status: &Status, theme: &Theme) -> ratatui::style::Color {
    match status {
        Status::Working => theme.accent,
        Status::NeedsInput { .. } => theme.warn,
        Status::Idle => theme.muted,
        Status::Done => theme.ok,
        Status::Error => theme.err,
        Status::Unknown => theme.faint,
    }
}

pub(super) fn pr_badge(pr_refs: &[PrRef]) -> String {
    match pr_refs {
        [] => String::new(),
        [one] => format!("#{}", one.id),
        many => format!("{} PRs", many.len()),
    }
}

fn pr_badge_theme_color(color: PrBadgeColor, theme: &Theme) -> ratatui::style::Color {
    match color {
        PrBadgeColor::Default => theme.accent,
        PrBadgeColor::Attention => theme.warn,
        PrBadgeColor::Passed => theme.ok,
        PrBadgeColor::Merged => theme.merged,
        PrBadgeColor::Muted => theme.muted,
    }
}

struct SessionRow<'a> {
    glyph: &'a str,
    glyph_color: ratatui::style::Color,
    mark: &'a str,
    mark_color: ratatui::style::Color,
    name: &'a str,
    status: &'a Status,
    summary: &'a str,
    pr: &'a str,
    pr_color: ratatui::style::Color,
    elapsed: &'a str,
    width: usize,
    title_width: usize,
}

fn session_line(row: SessionRow, theme: &Theme) -> Line<'static> {
    let word = status_display_word(row.status);
    let (title, status, pr, summary, pad) = crate::app::row_layout(
        row.width,
        mark_width(row.mark),
        row.name,
        row.title_width,
        word,
        row.pr,
        row.summary,
        display_width(row.elapsed),
    );
    let mut spans = vec![
        Span::styled(row.glyph.to_string(), fg(row.glyph_color)),
        Span::styled(row.mark.to_string(), fg(row.mark_color)),
        Span::styled(title, fg(theme.text)),
        Span::raw(" "),
        Span::styled(status, fg(status_color(row.status, theme))),
    ];
    let has_pr = !pr.is_empty();
    let has_summary = !summary.is_empty();
    if has_pr || has_summary {
        spans.push(Span::raw("  "));
        if has_summary {
            spans.push(Span::styled(summary, fg(theme.muted)));
        }
        if has_pr && has_summary {
            spans.push(Span::raw(" "));
        }
        if has_pr {
            spans.push(Span::styled(pr, fg(row.pr_color)));
        }
    }
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(row.elapsed.to_string(), fg(theme.muted)));
    Line::from(spans)
}

fn section_label(section: Section) -> &'static str {
    match section {
        Section::NeedsInput => "NEEDS INPUT",
        Section::Working => "WORKING",
        Section::Idle => "IDLE",
        Section::Done => "DONE",
    }
}

fn header_label(label: impl std::fmt::Display, count: usize, collapsed: bool) -> String {
    if collapsed {
        format!("▶ {label}  ({count} hidden)")
    } else {
        format!("▼ {label}  ({count})")
    }
}
