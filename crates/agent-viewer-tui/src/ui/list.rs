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

#[allow(clippy::too_many_arguments)]
pub(super) fn row_to_item(
    row: &Row,
    pulses: &Pulses,
    now_ms: i64,
    pr_status: &crate::pr_cache::PrStatusCache,
    show_activity: bool,
    project_width: usize,
    width: usize,
    title_width: usize,
    elapsed_width: usize,
    theme: &Theme,
    age_ramp: bool,
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
            project,
            activity,
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
            // Age runs from the last thing the session did, not from when it started: a
            // three-day job that finished an hour ago is fresh work and must stay bright.
            let fade = row_fade(status, now_ms - *updated_at_ms, age_ramp);
            let line = session_line(
                SessionRow {
                    glyph,
                    glyph_color,
                    mark: backend_mark(*backend, theme),
                    mark_color: backend_mark_color(*backend, theme),
                    name: title,
                    status,
                    project: project.as_deref(),
                    activity: activity.as_deref(),
                    summary,
                    pr: &pr_badge(pr_refs),
                    pr_color,
                    elapsed: &elapsed,
                    show_activity,
                    project_width,
                    width,
                    title_width,
                    elapsed_width,
                    fade,
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
    project: Option<&'a str>,
    activity: Option<&'a str>,
    summary: &'a str,
    pr: &'a str,
    pr_color: ratatui::style::Color,
    elapsed: &'a str,
    show_activity: bool,
    project_width: usize,
    width: usize,
    title_width: usize,
    elapsed_width: usize,
    /// How far this row has receded toward `faint` under the age ramp: 0.0 is untouched.
    fade: f32,
}

/// How far along the age ramp this row sits, given the mode flag and the row's age.
///
/// Only a **finished** session recedes. `Working` and `NeedsInput` are live states, and a live
/// session that has been running a long time is the opposite of receding, so they are pinned at
/// zero at every age. `Idle` and `Unknown` already resolve to `muted`/`faint`, so a fade toward
/// `faint` would buy nothing and `Unknown` is not a finished session either way.
fn row_fade(status: &Status, age_ms: i64, age_ramp: bool) -> f32 {
    if !age_ramp || !matches!(status, Status::Done | Status::Error) {
        return 0.0;
    }
    crate::ui::age::age_progress(age_ms)
}

fn session_line(row: SessionRow, theme: &Theme) -> Line<'static> {
    // The theme owns both endpoints: whatever token the span would normally use is the start,
    // and `faint` is the end. `ramp` is a no-op at fade 0.0 and under a non-truecolor theme.
    let faded = |color: ratatui::style::Color| crate::ui::age::ramp(color, theme.faint, row.fade);
    let word = status_display_word(row.status);
    let status_field = if row.show_activity {
        format!("{word:<width$}", width = "Needs input".len())
    } else {
        word.to_string()
    };
    let activity_width = usize::from(row.show_activity) * 10;
    let (title, status, pr, summary, pad) = crate::app::row_layout(
        row.width.saturating_sub(activity_width + row.project_width),
        mark_width(row.mark),
        row.name,
        row.title_width,
        &status_field,
        row.pr,
        row.summary,
        row.elapsed_width,
    );
    let mut spans = vec![
        Span::styled(row.glyph.to_string(), fg(faded(row.glyph_color))),
        // The backend mark keeps full strength: it is brand identity, not chrome.
        Span::styled(row.mark.to_string(), fg(row.mark_color)),
        Span::styled(title, fg(faded(theme.text))),
        Span::raw(" "),
        Span::styled(status, fg(faded(status_color(row.status, theme)))),
    ];
    if row.project_width > 0
        && let Some(project) = row.project
    {
        let content_width = row.project_width.saturating_sub(2);
        let mut project = truncate_display_width(project, content_width);
        project
            .push_str(&" ".repeat(content_width.saturating_sub(display_width(project.as_str()))));
        spans.push(Span::raw("  "));
        spans.push(Span::styled(project, fg(theme.faint)));
    }
    if row.show_activity {
        let ribbon = row.activity.unwrap_or("▁▁▁▁▁▁▁▁");
        let color = if row.activity.is_some() && matches!(row.status, Status::Working) {
            theme.accent
        } else {
            theme.faint
        };
        spans.push(Span::raw("  "));
        spans.push(Span::styled(ribbon.to_string(), fg(color)));
    }
    let has_pr = !pr.is_empty();
    let has_summary = !summary.is_empty();
    if has_pr || has_summary {
        spans.push(Span::raw("  "));
        if has_summary {
            spans.push(Span::styled(summary, fg(faded(theme.muted))));
        }
        if has_pr && has_summary {
            spans.push(Span::raw(" "));
        }
    }
    spans.push(Span::raw(" ".repeat(pad)));
    if has_pr {
        // The PR badge keeps full strength too: it is a live external signal, and it must not
        // read as stale just because the session that opened it finished days ago.
        spans.push(Span::styled(pr, fg(row.pr_color)));
    }
    spans.push(Span::raw(" "));
    spans.push(Span::raw(" ".repeat(
        row.elapsed_width.saturating_sub(display_width(row.elapsed)),
    )));
    spans.push(Span::styled(
        row.elapsed.to_string(),
        fg(faded(theme.muted)),
    ));
    Line::from(spans)
}

pub fn activity_ribbon(timestamps: &[i64], now_ms: i64, window_ms: i64) -> String {
    const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if window_ms <= 0 {
        return BLOCKS[0].to_string().repeat(8);
    }

    let start_ms = now_ms.saturating_sub(window_ms);
    let mut buckets = [0usize; 8];
    for timestamp in timestamps {
        if *timestamp < start_ms || *timestamp > now_ms {
            continue;
        }
        let elapsed = (*timestamp - start_ms) as i128;
        let mut bucket = (elapsed * 8 / window_ms as i128) as usize;
        bucket = bucket.min(7);
        buckets[bucket] += 1;
    }

    let busiest = buckets.iter().copied().max().unwrap_or(0);
    buckets
        .into_iter()
        .map(|count| {
            if count == 0 || busiest == 0 {
                BLOCKS[0]
            } else {
                let level = (count * 7).div_ceil(busiest);
                BLOCKS[level]
            }
        })
        .collect()
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
#[cfg(test)]
mod age_ramp_tests {
    use super::{Pulses, row_to_item};
    use crate::app::Row;
    use crate::ui::theme::{Theme, amber, terminal};
    use agent_viewer_core::{BackendKind, Status};
    use ratatui::style::Color;

    const HORIZON_MS: i64 = 24 * 60 * 60 * 1_000;
    const NOW_MS: i64 = HORIZON_MS * 100;
    const TITLE: &str = "Age ramp session";
    const WORD: &str = "Done";

    fn row(status: Status, updated_at_ms: i64) -> Row {
        Row::Session {
            backend: BackendKind::Codex,
            id: "session".into(),
            title: "Age ramp session".into(),
            summary: "did the thing".into(),
            project: None,
            activity: None,
            status,
            hidden: false,
            created_at_ms: updated_at_ms,
            updated_at_ms,
            pr_refs: Vec::new(),
        }
    }

    const WIDTH: u16 = 140;

    /// Render one row through the real widget and return each cell as (symbol, foreground).
    /// Going through the buffer rather than the spans keeps this honest about what is painted.
    fn render(row: &Row, theme: &Theme, age_ramp: bool) -> Vec<(String, Color)> {
        let item = row_to_item(
            row,
            &Pulses::new(),
            NOW_MS,
            &crate::pr_cache::PrStatusCache::default(),
            false,
            0,
            WIDTH as usize,
            32,
            2,
            theme,
            age_ramp,
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(WIDTH, 1)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    ratatui::widgets::List::new(vec![item]),
                    ratatui::layout::Rect::new(0, 0, WIDTH, 1),
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..WIDTH)
            .map(|x| {
                let cell = &buffer[(x, 0)];
                (cell.symbol().to_string(), cell.fg)
            })
            .collect()
    }

    /// The foreground of the first cell of `needle` in the rendered row. Wide glyphs leave an
    /// empty continuation cell, so the byte offset is mapped back through a cell index table
    /// rather than assumed to be one char per cell.
    fn color_of(cells: &[(String, Color)], needle: &str) -> Color {
        let mut text = String::new();
        let mut owners = Vec::new();
        for (index, (symbol, _)) in cells.iter().enumerate() {
            for _ in 0..symbol.len() {
                owners.push(index);
            }
            text.push_str(symbol);
        }
        let byte = text
            .find(needle)
            .unwrap_or_else(|| panic!("row is missing {needle:?}"));
        cells[owners[byte]].1
    }

    #[test]
    fn finished_row_recedes_toward_faint_as_it_ages() {
        let theme = amber(false);
        let fresh = render(&row(Status::Done, NOW_MS), &theme, true);
        let half = render(&row(Status::Done, NOW_MS - HORIZON_MS / 2), &theme, true);
        let old = render(&row(Status::Done, NOW_MS - HORIZON_MS), &theme, true);

        assert_eq!(color_of(&fresh, TITLE), theme.text);
        assert_eq!(color_of(&fresh, WORD), theme.ok);

        // Half a horizon in: moved off both start tokens but not yet all the way to faint.
        assert_ne!(color_of(&half, TITLE), theme.text);
        assert_ne!(color_of(&half, TITLE), theme.faint);
        assert_ne!(color_of(&half, WORD), theme.ok);
        assert_ne!(color_of(&half, WORD), theme.faint);

        // Past the horizon everything that ramps has landed exactly on faint.
        assert_eq!(color_of(&old, TITLE), theme.faint);
        assert_eq!(color_of(&old, WORD), theme.faint);
    }

    #[test]
    fn live_states_never_ramp_at_any_age() {
        let theme = amber(false);
        for status in [Status::Working, Status::NeedsInput { reason: None }] {
            let word = super::status_display_word(&status);
            let live = super::status_color(&status, &theme);
            let ancient = render(&row(status.clone(), NOW_MS - HORIZON_MS * 30), &theme, true);
            assert_eq!(
                color_of(&ancient, TITLE),
                theme.text,
                "{status:?} title receded, but a live session must never ramp"
            );
            assert_eq!(
                color_of(&ancient, word),
                live,
                "{status:?} state word receded, but a live session must never ramp"
            );
            assert_eq!(color_of(&ancient, "did the thing"), theme.muted);
        }
    }

    #[test]
    fn the_mode_is_a_no_op_when_off() {
        let theme = amber(false);
        let cells = render(&row(Status::Done, NOW_MS - HORIZON_MS * 7), &theme, false);
        assert_eq!(color_of(&cells, TITLE), theme.text);
        assert_eq!(color_of(&cells, WORD), theme.ok);
        assert_eq!(color_of(&cells, "did the thing"), theme.muted);
    }

    #[test]
    fn non_truecolor_theme_renders_identically_with_the_mode_on() {
        let theme = terminal(false);
        // The gate, at the row level: under a theme with no truecolor endpoint, a fully aged
        // finished row must be byte-identical to the mode being off.
        let old = row(Status::Done, NOW_MS - HORIZON_MS * 7);
        assert_eq!(render(&old, &theme, true), render(&old, &theme, false));
        assert_eq!(color_of(&render(&old, &theme, true), TITLE), theme.text);
    }

    #[test]
    fn the_backend_mark_and_pr_badge_keep_full_strength() {
        let theme = amber(false);
        let mut old = row(Status::Done, NOW_MS - HORIZON_MS * 7);
        if let Row::Session { pr_refs, .. } = &mut old {
            pr_refs.push(agent_viewer_core::PrRef {
                id: "42".into(),
                href: None,
            });
        }
        let cells = render(&old, &theme, true);
        // The mark carries the backend's brand token, untouched.
        assert_eq!(color_of(&cells, "[cx]"), theme.cx);
        // The PR badge is a live external signal, so it must not read as stale.
        assert_eq!(color_of(&cells, "#42"), theme.accent);
        // ... while the row around it has receded.
        assert_eq!(color_of(&cells, TITLE), theme.faint);
    }
}

#[cfg(test)]
mod activity_ribbon_tests {
    use super::{SessionRow, activity_ribbon, session_line};
    use agent_viewer_core::Status;

    const HOUR_MS: i64 = 60 * 60 * 1_000;
    const BUCKET_MS: i64 = HOUR_MS / 8;
    const NOW_MS: i64 = HOUR_MS;

    fn timestamps(counts: [usize; 8]) -> Vec<i64> {
        counts
            .into_iter()
            .enumerate()
            .flat_map(|(bucket, count)| {
                std::iter::repeat_n(bucket as i64 * BUCKET_MS + BUCKET_MS / 2, count)
            })
            .collect()
    }

    #[test]
    fn timestamp_series_buckets_to_stuck_cliff() {
        assert_eq!(
            activity_ribbon(&timestamps([8, 5, 2, 1, 0, 0, 0, 0]), NOW_MS, HOUR_MS),
            "█▆▃▂▁▁▁▁"
        );
    }

    #[test]
    fn per_row_scaling_preserves_shape_across_volume() {
        let low = activity_ribbon(&timestamps([1, 2, 3, 4, 3, 2, 1, 0]), NOW_MS, HOUR_MS);
        let high = activity_ribbon(
            &timestamps([10, 20, 30, 40, 30, 20, 10, 0]),
            NOW_MS,
            HOUR_MS,
        );
        assert_eq!(low, high);
    }

    #[test]
    fn timestamps_outside_window_are_excluded() {
        let mut series = timestamps([0, 0, 0, 0, 0, 0, 0, 1]);
        series.extend([NOW_MS - HOUR_MS - 1, NOW_MS + 1]);
        assert_eq!(activity_ribbon(&series, NOW_MS, HOUR_MS), "▁▁▁▁▁▁▁█");
    }

    #[test]
    fn empty_series_is_a_flat_floor() {
        assert_eq!(activity_ribbon(&[], NOW_MS, HOUR_MS), "▁▁▁▁▁▁▁▁");
    }

    fn rendered_row(width: usize, show_activity: bool, summary: &str) -> String {
        let themes = crate::ui::ThemeState::default();
        let theme = themes.active();
        let line = session_line(
            SessionRow {
                glyph: "✽",
                glyph_color: theme.accent,
                mark: "[cx]",
                mark_color: theme.cx,
                name: "Activity session",
                status: &Status::Working,
                project: None,
                activity: Some("█▆▃▂▁▁▁▁"),
                summary,
                pr: "#42",
                pr_color: theme.ok,
                elapsed: "3m",
                elapsed_width: 2,
                show_activity,
                project_width: 0,
                width,
                title_width: 32,
                fade: 0.0,
            },
            theme,
        );
        line.spans
            .into_iter()
            .map(|span| span.content.into_owned())
            .collect()
    }

    #[test]
    fn ribbon_width_gate_and_secondary_truncation_order() {
        let summary = "summary ".repeat(30);
        let wide = rendered_row(140, true, &summary);
        assert!(wide.contains("█▆▃▂▁▁▁▁"));
        assert!(wide.contains("#42"));
        assert!(!wide.contains(&summary));

        let narrow = rendered_row(100, false, &summary);
        assert!(!narrow.contains("█▆▃▂▁▁▁▁"));
        assert!(narrow.contains("#42"));
    }

    #[test]
    fn working_row_without_activity_uses_faint_floor() {
        let themes = crate::ui::ThemeState::default();
        let theme = themes.active();
        let line = session_line(
            SessionRow {
                glyph: "✽",
                glyph_color: theme.accent,
                mark: "[cx]",
                mark_color: theme.cx,
                name: "No activity",
                status: &Status::Working,
                project: None,
                activity: None,
                summary: "",
                pr: "",
                pr_color: theme.ok,
                elapsed: "3m",
                elapsed_width: 2,
                show_activity: true,
                project_width: 0,
                width: 140,
                title_width: 32,
                fade: 0.0,
            },
            theme,
        );
        let ribbon = line
            .spans
            .iter()
            .find(|span| span.content == "▁▁▁▁▁▁▁▁")
            .expect("flat activity ribbon");
        assert_eq!(ribbon.style.fg, Some(theme.faint));
    }

    #[test]
    fn zero_project_width_omits_project_without_activity_padding() {
        let themes = crate::ui::ThemeState::default();
        let theme = themes.active();
        let line = session_line(
            SessionRow {
                glyph: "✽",
                glyph_color: theme.accent,
                mark: "[cx]",
                mark_color: theme.cx,
                name: "Activity session",
                status: &Status::Working,
                project: Some("theconnman/agent-viewer"),
                activity: Some("█▆▃▂▁▁▁▁"),
                summary: "summary",
                pr: "#42",
                pr_color: theme.ok,
                elapsed: "3m",
                elapsed_width: 2,
                show_activity: true,
                project_width: 0,
                width: 140,
                title_width: 32,
                fade: 0.0,
            },
            theme,
        );
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(!text.contains("theconnman/agent-viewer"));
        assert!(text.contains("█▆▃▂▁▁▁▁"));
        assert!(text.contains("summary"));
        let ribbon = line
            .spans
            .iter()
            .position(|span| span.content == "█▆▃▂▁▁▁▁")
            .unwrap();
        assert_eq!(line.spans[ribbon - 2].content, "Working    ");
    }
}
