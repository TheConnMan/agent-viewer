//! The Ctrl+N triage inbox: a plain modal over the list that walks the needs-input sessions
//! oldest first, one at a time.
//!
//! Deliberately the cheap shape. It is a reader over the session snapshot the list already
//! holds — it never attaches, spawns, joins, or mutates anything — plus one submit that hands
//! its payload to the existing reply path (`actions::send_reply`). The expensive version of
//! this feature, an overlay inside an attached full-screen child UI, is not what this is.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::overlay::centered_rect;
use super::{Theme, backend_mark, backend_mark_color, fg, mark_width};
use agent_viewer_core::{BackendKind, Session, Status};

/// How many "up next" rows the modal lists under the current item.
const UP_NEXT_ROWS: usize = 4;
/// How many transcript turns the context panel shows.
pub(crate) const CONTEXT_TURNS: usize = 3;
/// The digit keys are the affordance, so there is no tenth quick option.
const MAX_OPTIONS: usize = 9;

/// One quick option the agent itself enumerated in its question. `label` is what gets
/// submitted; `detail` is the trailing gloss, shown but never sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TriageOption {
    pub label: String,
    pub detail: String,
}

/// One blocked session in the queue, flattened to exactly what the modal renders.
#[derive(Clone, Debug, PartialEq)]
pub struct TriageItem {
    pub backend: BackendKind,
    pub id: String,
    pub title: String,
    pub project: String,
    pub updated_at_ms: i64,
    /// The question, with any enumerated option lines lifted out into `options`.
    pub question: String,
    pub options: Vec<TriageOption>,
    /// Codex rollout JSONL, when this session has one. The only transcript source with a
    /// reader in `-core`; every other backend renders its summary instead.
    pub rollout_path: Option<PathBuf>,
    /// The row summary, the fallback context line when no transcript can be read.
    pub summary: String,
}

impl TriageItem {
    pub fn key(&self) -> (BackendKind, String) {
        (self.backend, self.id.clone())
    }
}

/// PURE queue construction: every `NeedsInput` session, oldest first.
///
/// Built from the App's whole session set rather than `visible()`, so the queue length equals
/// the header's "N awaiting input" — a filter or a collapsed group must not silently shrink
/// the inbox. Ordering is ascending `updated_at_ms` (longest wait first), broken by
/// `(backend, id)` so the order is total and a refresh tick cannot shuffle two rows stamped
/// the same millisecond.
pub fn triage_queue(sessions: &[Session]) -> Vec<TriageItem> {
    let mut items = sessions
        .iter()
        .filter_map(|session| {
            let reason = match &session.status {
                Status::NeedsInput { reason } => reason.clone(),
                Status::Working | Status::Idle | Status::Done | Status::Error | Status::Unknown => {
                    return None;
                }
            };
            // The backend's stated reason wins; a blocked claude job puts its question in
            // `summary` instead (state.json `needs`); failing both, say so plainly.
            let raw = reason
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or_else(|| session.summary.clone());
            let raw = if raw.trim().is_empty() {
                "waiting for input".to_string()
            } else {
                raw
            };
            Some(TriageItem {
                backend: session.backend,
                id: session.id.clone(),
                title: session.title.clone(),
                project: project_label(&session.cwd),
                updated_at_ms: session.updated_at_ms,
                question: question_prompt(&raw),
                options: parse_options(&raw),
                rollout_path: session.rollout_path.clone(),
                summary: session.summary.clone(),
            })
        })
        .collect::<Vec<_>>();
    items.sort_by(|left, right| {
        left.updated_at_ms
            .cmp(&right.updated_at_ms)
            .then_with(|| left.backend.name().cmp(right.backend.name()))
            .then_with(|| left.id.cmp(&right.id))
    });
    items
}

/// The trailing two path components, matching how the design labels a session's project
/// (`example-user/wiki`, `example-org/example-app`).
fn project_label(cwd: &Path) -> String {
    let parts = cwd
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let tail = parts.len().saturating_sub(2);
    parts[tail..].join("/")
}

/// PURE: the enumerated quick options an agent wrote into its question.
///
/// Recognizes a line whose first token is `N.`, `N)`, `-`, or `*`, and splits the rest into a
/// submitted `label` and a shown-only `detail` on the first ` — `, ` - `, or `: `. Capped at
/// nine because the digit keys are the affordance.
///
/// Options are never synthesized. A fabricated "approve" that types the wrong word into a live
/// session is worse than showing no option at all, so prose with no enumerated lines yields an
/// empty vec and the modal offers only the free-text box.
pub fn parse_options(question: &str) -> Vec<TriageOption> {
    question
        .lines()
        .filter_map(|line| option_body(line.trim()))
        .filter_map(|body| {
            let (label, detail) = split_detail(body);
            let label = label.trim();
            (!label.is_empty()).then(|| TriageOption {
                label: label.to_string(),
                detail: detail.trim().to_string(),
            })
        })
        .take(MAX_OPTIONS)
        .collect()
}

/// PURE: the question with its enumerated option lines removed, collapsed to one line.
pub fn question_prompt(question: &str) -> String {
    let prompt = question
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && option_body(line).is_none())
        .collect::<Vec<_>>()
        .join(" ");
    if prompt.is_empty() {
        "pick an option".to_string()
    } else {
        prompt
    }
}

/// The text after a list marker (`1.`, `2)`, `-`, `*`), or None when the line is not one.
fn option_body(line: &str) -> Option<&str> {
    if let Some(rest) = line.strip_prefix(['-', '*'])
        && rest.starts_with(' ')
    {
        return Some(rest.trim_start());
    }
    let mut chars = line.chars();
    let digit = chars.next()?;
    if !digit.is_ascii_digit() || digit == '0' {
        return None;
    }
    let marker = chars.next()?;
    if marker != '.' && marker != ')' {
        return None;
    }
    let rest = chars.as_str();
    rest.starts_with(' ').then(|| rest.trim_start())
}

/// Split an option body into the submitted label and the shown-only detail.
fn split_detail(body: &str) -> (&str, &str) {
    for separator in [" — ", " – ", " - ", ": "] {
        if let Some(index) = body.find(separator) {
            return (&body[..index], &body[index + separator.len()..]);
        }
    }
    (body, "")
}

/// The modal's live state. The queue is snapshotted at open: a background refresh must not
/// reorder or resize it under the user's fingers mid-answer.
#[derive(Debug)]
pub struct TriageState {
    items: Vec<TriageItem>,
    index: usize,
    buffer: String,
    highlight: usize,
    answered: usize,
    /// Transcript context, filled in off-thread and keyed by session.
    turns: HashMap<(BackendKind, String), Vec<String>>,
}

impl TriageState {
    pub fn new(items: Vec<TriageItem>) -> TriageState {
        TriageState {
            items,
            index: 0,
            buffer: String::new(),
            highlight: 0,
            answered: 0,
            turns: HashMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn index(&self) -> usize {
        self.index
    }

    /// The `3 of 7` affordance: a 1-based position and the queue length.
    pub fn progress(&self) -> (usize, usize) {
        (self.index + 1, self.items.len())
    }

    pub fn current(&self) -> Option<&TriageItem> {
        self.items.get(self.index)
    }

    pub fn upcoming(&self) -> &[TriageItem] {
        let start = (self.index + 1).min(self.items.len());
        let end = (start + UP_NEXT_ROWS).min(self.items.len());
        &self.items[start..end]
    }

    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    pub fn push_char(&mut self, character: char) {
        self.buffer.push(character);
    }

    pub fn push_str(&mut self, text: &str) {
        self.buffer.push_str(text);
    }

    pub fn backspace(&mut self) {
        self.buffer.pop();
    }

    pub fn highlight(&self) -> usize {
        self.highlight
    }

    pub fn move_highlight(&mut self, delta: i32) {
        let count = self.current().map(|item| item.options.len()).unwrap_or(0);
        if count == 0 {
            return;
        }
        self.highlight = (self.highlight as i32 + delta).rem_euclid(count as i32) as usize;
    }

    pub fn answered(&self) -> usize {
        self.answered
    }

    pub fn record_answer(&mut self) {
        self.answered += 1;
    }

    /// The payload the 1-based digit key `n` submits: that option's label, verbatim.
    pub fn option_payload(&self, n: usize) -> Option<String> {
        let index = n.checked_sub(1)?;
        Some(self.current()?.options.get(index)?.label.clone())
    }

    /// The payload a bare Enter submits: the typed answer, else the highlighted option's
    /// label. Identical by construction to `option_payload` for the same option — picking a
    /// number is a shortcut for typing that label, never a second delivery shape.
    pub fn enter_payload(&self) -> Option<String> {
        let typed = self.buffer.trim();
        if !typed.is_empty() {
            return Some(typed.to_string());
        }
        self.option_payload(self.highlight + 1)
    }

    /// Step to the next item. Returns false when the queue is finished — it never wraps,
    /// because a wrap would make the `3 of 7` counter a lie.
    pub fn advance(&mut self) -> bool {
        self.index += 1;
        self.buffer.clear();
        self.highlight = 0;
        self.index < self.items.len()
    }

    /// Step back one item. False (and no move) when already on the first.
    pub fn back(&mut self) -> bool {
        if self.index == 0 {
            return false;
        }
        self.index -= 1;
        self.buffer.clear();
        self.highlight = 0;
        true
    }

    /// The current item's transcript context, once it has landed.
    pub fn context(&self) -> Option<&[String]> {
        let item = self.current()?;
        self.turns.get(&item.key()).map(Vec::as_slice)
    }

    /// The current item when its context has not been read yet — the read to submit.
    pub fn context_pending(&self) -> Option<&TriageItem> {
        let item = self.current()?;
        (!self.turns.contains_key(&item.key())).then_some(item)
    }

    pub fn set_context(&mut self, backend: BackendKind, id: String, lines: Vec<String>) {
        self.turns.insert((backend, id), lines);
    }
}

/// Read the last few turns of a Codex rollout, formatted one line each.
///
/// Runs on a background worker, never on the render or key path — a rollout JSONL is
/// unbounded. Uses the existing `-core` transcript reader; there is deliberately no second
/// parser here.
pub fn read_context(path: &Path) -> Vec<String> {
    let Ok(items) = agent_viewer_core::codex::rollout::read_transcript(path) else {
        return Vec::new();
    };
    let start = items.len().saturating_sub(CONTEXT_TURNS);
    items[start..]
        .iter()
        .map(|item| {
            let text = item
                .text
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("");
            format!("{} · {}", item.role, text)
        })
        .collect()
}

pub(super) fn draw(frame: &mut Frame, state: &TriageState, now_ms: i64, theme: &Theme) {
    let Some(item) = state.current() else {
        return;
    };
    let frame_area = frame.area();
    let available_width = frame_area.width.saturating_sub(4);
    let width = 92.min(available_width);
    let context = state.context().unwrap_or(&[]);
    let desired_height = (2                                  // header + rule
        + 1                                                  // item header
        + 1 + context.len().max(1)                           // LAST N TURNS + lines
        + 1                                                  // question
        + item.options.len()
        + 3                                                  // answer box
        + 1 + state.upcoming().len()                         // UP NEXT + rows
        + 1                                                  // hints
        + 2) as u16; // borders
    let height = desired_height.min(frame_area.height.saturating_sub(2));
    if width < 24 || height < 8 {
        return;
    }

    let mut area = centered_rect(50, 50, frame_area);
    area.width = width;
    area.height = height;
    area.x = frame_area.x + frame_area.width.saturating_sub(width) / 2;
    area.y = frame_area.y + 2.min(frame_area.height.saturating_sub(height));

    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(fg(theme.accent))
        .style(Style::default().bg(theme.surface).fg(theme.text));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let mut rows = Rows::new(inner);
    let full = inner.width as usize;

    // Header: the progress affordance carries the meaning without color, so it survives mono16.
    let (position, total) = state.progress();
    let right = if state.answered() > 0 {
        format!("oldest first · {} answered", state.answered())
    } else {
        "oldest first".to_string()
    };
    let left = format!("⟳ triage queue · {position} of {total}");
    rows.line(
        frame,
        Line::from(vec![
            Span::styled(left.clone(), fg(theme.accent)),
            Span::raw(" ".repeat(full.saturating_sub(display(&left) + display(&right)))),
            Span::styled(right, fg(theme.faint)),
        ]),
    );
    rows.line(
        frame,
        Line::from(Span::styled("─".repeat(full), fg(theme.border))),
    );

    // The item itself.
    let mark = backend_mark(item.backend, theme);
    let waiting = format!(
        "waiting {}",
        crate::app::format_elapsed(now_ms - item.updated_at_ms)
    );
    let head_left = format!("◐ {mark} {} {}", item.title, item.project);
    let head_pad = full.saturating_sub(display(&head_left) + display(&waiting));
    rows.line(
        frame,
        Line::from(vec![
            Span::styled("◐ ", fg(theme.warn)),
            Span::styled(
                mark.to_string(),
                fg(backend_mark_color(item.backend, theme)),
            ),
            Span::raw(" ".repeat(mark_width(mark).saturating_sub(display(mark)))),
            Span::styled(format!(" {} ", item.title), fg(theme.text)),
            Span::styled(item.project.clone(), fg(theme.muted)),
            Span::raw(" ".repeat(head_pad)),
            Span::styled(waiting, fg(theme.warn)),
        ]),
    );

    rows.line(
        frame,
        Line::from(Span::styled(
            format!("LAST {CONTEXT_TURNS} TURNS"),
            fg(theme.faint),
        )),
    );
    if context.is_empty() {
        // No transcript reader exists for this backend; the row summary is the honest
        // fallback, and an absent one says so rather than rendering a blank. A blocked claude
        // job's summary IS its question, so echoing it here would print the same sentence
        // twice and read as a bug.
        let (text, color) = if item.summary.trim().is_empty() || item.summary == item.question {
            ("no transcript available for this session", theme.faint)
        } else {
            (item.summary.as_str(), theme.text)
        };
        rows.line(
            frame,
            Line::from(Span::styled(field(text, full), fg(color))),
        );
    } else {
        for line in context {
            rows.line(
                frame,
                Line::from(Span::styled(field(line, full), fg(theme.text))),
            );
        }
    }

    rows.line(
        frame,
        Line::from(vec![
            Span::styled("? ", fg(theme.accent)),
            Span::styled(
                field(&item.question, full.saturating_sub(2)),
                fg(theme.text),
            ),
        ]),
    );

    for (index, option) in item.options.iter().enumerate() {
        let selected = index == state.highlight();
        let label_color = if selected { theme.selfg } else { theme.text };
        let detail_color = if selected { theme.selfg } else { theme.muted };
        let label_width = 24.min(full.saturating_sub(4));
        rows.styled_line(
            frame,
            Line::from(vec![
                Span::styled(format!("  {} ", index + 1), fg(theme.faint)),
                Span::styled(field(&option.label, label_width), fg(label_color)),
                Span::styled(
                    field(&option.detail, full.saturating_sub(4 + label_width)),
                    fg(detail_color),
                ),
            ]),
            if selected {
                Style::default().bg(theme.selbg)
            } else {
                Style::default()
            },
        );
    }

    // The free-text answer box.
    let caret = if super::caret_visible(now_ms, theme.animation) {
        "▏"
    } else {
        " "
    };
    let (answer, answer_color) = if state.buffer().is_empty() {
        ("or type an answer".to_string(), theme.faint)
    } else {
        (state.buffer().to_string(), theme.text)
    };
    rows.boxed(
        frame,
        theme,
        Line::from(vec![
            Span::styled("❯ ", fg(theme.accent)),
            Span::styled(field(&answer, full.saturating_sub(5)), fg(answer_color)),
            Span::styled(caret, fg(theme.accent)),
        ]),
    );

    let upcoming = state.upcoming();
    if !upcoming.is_empty() {
        rows.line(frame, Line::from(Span::styled("UP NEXT", fg(theme.faint))));
        for next in upcoming {
            let age = crate::app::format_elapsed(now_ms - next.updated_at_ms);
            let left = format!("  ◐ {} {}", next.title, next.project);
            let pad = full.saturating_sub(display(&left) + display(&age));
            rows.line(
                frame,
                Line::from(vec![
                    Span::styled("  ◐ ", fg(theme.warn)),
                    Span::styled(format!("{} ", next.title), fg(theme.text)),
                    Span::styled(next.project.clone(), fg(theme.muted)),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(age, fg(theme.faint)),
                ]),
            );
        }
    }

    rows.line(
        frame,
        Line::from(Span::styled(
            "⏎ send · 1-9 pick · → skip · ← back · esc leave queue",
            fg(theme.faint),
        )),
    );
}

/// A top-down row writer over the modal's inner rect: every region renders where the previous
/// one stopped and silently drops off the bottom when the modal is short.
struct Rows {
    area: Rect,
    y: u16,
}

impl Rows {
    fn new(area: Rect) -> Rows {
        Rows { area, y: area.y }
    }

    fn take(&mut self, height: u16) -> Option<Rect> {
        let end = self.area.y.saturating_add(self.area.height);
        if self.y.saturating_add(height) > end {
            return None;
        }
        let rect = Rect {
            y: self.y,
            height,
            ..self.area
        };
        self.y += height;
        Some(rect)
    }

    fn line(&mut self, frame: &mut Frame, line: Line<'static>) {
        self.styled_line(frame, line, Style::default());
    }

    fn styled_line(&mut self, frame: &mut Frame, line: Line<'static>, style: Style) {
        if let Some(rect) = self.take(1) {
            frame.render_widget(Paragraph::new(line).style(style), rect);
        }
    }

    fn boxed(&mut self, frame: &mut Frame, theme: &Theme, line: Line<'static>) {
        let Some(rect) = self.take(3) else {
            // Too short for the bordered box: the input still has to be reachable, so fall
            // back to a bare line rather than hiding where the user types.
            self.line(frame, line);
            return;
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(fg(theme.border));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        frame.render_widget(Paragraph::new(line), inner);
    }
}

fn display(value: &str) -> usize {
    value.width()
}

fn field(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let text = ellipsize(value, width);
    let padding = width.saturating_sub(text.width());
    format!("{text}{}", " ".repeat(padding))
}

fn ellipsize(value: &str, width: usize) -> String {
    if value.width() <= width {
        return value.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let mut result = String::new();
    let available = width - 1;
    for grapheme in value.graphemes(true) {
        if result.width() + grapheme.width() > available {
            break;
        }
        result.push_str(grapheme);
    }
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(id: &str, updated_at_ms: i64, reason: Option<&str>, summary: &str) -> Session {
        Session {
            backend: BackendKind::Claude,
            id: id.into(),
            short_id: None,
            origin: agent_viewer_core::SessionOrigin::Background,
            title: id.into(),
            cwd: "/home/me/git/acme/widget".into(),
            git_branch: None,
            status: Status::NeedsInput {
                reason: reason.map(str::to_string),
            },
            created_at_ms: updated_at_ms,
            updated_at_ms,
            hidden: false,
            companion: false,
            summary: summary.into(),
            pid: None,
            rollout_path: None,
            pr_refs: Vec::new(),
            daemon_hosted: false,
        }
    }

    fn with_status(mut session: Session, status: Status) -> Session {
        session.status = status;
        session
    }

    #[test]
    fn queue_is_oldest_first_and_holds_only_needs_input() {
        let sessions = vec![
            blocked("newest", 900, Some("q"), ""),
            with_status(blocked("working", 100, None, ""), Status::Working),
            blocked("oldest", 100, Some("q"), ""),
            with_status(blocked("done", 200, None, ""), Status::Done),
            blocked("middle", 500, Some("q"), ""),
            with_status(blocked("idle", 300, None, ""), Status::Idle),
            with_status(blocked("error", 400, None, ""), Status::Error),
            with_status(blocked("unknown", 600, None, ""), Status::Unknown),
        ];
        let queue = triage_queue(&sessions);
        assert_eq!(
            queue
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["oldest", "middle", "newest"],
            "only needs-input sessions, ordered by longest wait first"
        );
    }

    #[test]
    fn same_millisecond_rows_get_a_total_order() {
        let sessions = vec![
            blocked("b", 100, Some("q"), ""),
            blocked("a", 100, Some("q"), ""),
        ];
        assert_eq!(
            triage_queue(&sessions)
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn question_falls_back_from_reason_to_summary_to_a_literal() {
        let reasoned = triage_queue(&[blocked("a", 1, Some("why not"), "ignored")]);
        assert_eq!(reasoned[0].question, "why not");
        let summarized = triage_queue(&[blocked("b", 1, None, "state.json needs")]);
        assert_eq!(summarized[0].question, "state.json needs");
        let bare = triage_queue(&[blocked("c", 1, None, "")]);
        assert_eq!(bare[0].question, "waiting for input");
    }

    #[test]
    fn progress_counter_tracks_the_queue_length() {
        let sessions = (0..5)
            .map(|index| blocked(&format!("s{index}"), index, Some("q"), ""))
            .collect::<Vec<_>>();
        let queue = triage_queue(&sessions);
        let total = queue.len();
        let mut state = TriageState::new(queue);
        for position in 1..=total {
            assert_eq!(state.progress(), (position, total));
            state.advance();
        }
    }

    #[test]
    fn advancing_past_the_last_item_reports_finished_and_never_wraps() {
        let queue = triage_queue(&[
            blocked("a", 1, Some("q"), ""),
            blocked("b", 2, Some("q"), ""),
        ]);
        let mut state = TriageState::new(queue);
        assert!(state.advance(), "one item left");
        assert!(!state.advance(), "past the end");
        assert!(state.current().is_none(), "must not wrap to the first item");
        assert_eq!(state.index(), 2);
    }

    #[test]
    fn back_stops_at_the_first_item() {
        let queue = triage_queue(&[
            blocked("a", 1, Some("q"), ""),
            blocked("b", 2, Some("q"), ""),
        ]);
        let mut state = TriageState::new(queue);
        assert!(!state.back(), "already first");
        assert_eq!(state.index(), 0);
        state.advance();
        assert!(state.back());
        assert_eq!(state.index(), 0);
    }

    #[test]
    fn a_numbered_pick_and_the_typed_label_produce_one_payload() {
        let question =
            "Pick a direction.\n1. artifact-first — ship now\n2) prove-the-thing: one case study";
        let queue = triage_queue(&[blocked("a", 1, Some(question), "")]);
        let picked = TriageState::new(queue.clone());
        let mut typed = TriageState::new(queue);

        let by_number = picked.option_payload(2).expect("second option exists");
        for character in by_number.chars() {
            typed.push_char(character);
        }
        let by_text = typed.enter_payload().expect("typed answer");

        assert_eq!(by_number, "prove-the-thing");
        assert_eq!(by_number, by_text, "one payload, two ways to enter it");
    }

    #[test]
    fn enter_falls_through_to_the_highlighted_option_only_when_nothing_is_typed() {
        let question = "Choose.\n1. first\n2. second";
        let queue = triage_queue(&[blocked("a", 1, Some(question), "")]);
        let mut state = TriageState::new(queue);
        assert_eq!(state.enter_payload().as_deref(), Some("first"));
        state.move_highlight(1);
        assert_eq!(state.enter_payload().as_deref(), Some("second"));
        state.push_str("  something else  ");
        assert_eq!(state.enter_payload().as_deref(), Some("something else"));
    }

    #[test]
    fn enter_on_an_optionless_item_with_an_empty_buffer_submits_nothing() {
        let queue = triage_queue(&[blocked("a", 1, Some("just prose"), "")]);
        let state = TriageState::new(queue);
        assert!(state.enter_payload().is_none());
        assert!(state.option_payload(1).is_none());
    }

    #[test]
    fn options_parse_every_recognized_marker_and_split_label_from_detail() {
        let question = concat!(
            "Confirm the plan, then pick a direction.\n",
            "1. artifact-first — ship the pieces now\n",
            "2) prove-the-thing: one flagship case study\n",
            "- snooze 24h - re-queue tomorrow\n",
            "* keep the session warm\n",
        );
        assert_eq!(
            parse_options(question),
            vec![
                TriageOption {
                    label: "artifact-first".into(),
                    detail: "ship the pieces now".into()
                },
                TriageOption {
                    label: "prove-the-thing".into(),
                    detail: "one flagship case study".into()
                },
                TriageOption {
                    label: "snooze 24h".into(),
                    detail: "re-queue tomorrow".into()
                },
                TriageOption {
                    label: "keep the session warm".into(),
                    detail: String::new()
                },
            ]
        );
        assert_eq!(
            question_prompt(question),
            "Confirm the plan, then pick a direction."
        );
    }

    #[test]
    fn prose_yields_no_options_so_none_are_ever_synthesized() {
        let prose = "I need approval before I run 1. the migration and 2. the backfill.";
        assert!(parse_options(prose).is_empty());
        assert_eq!(question_prompt(prose), prose);
    }

    #[test]
    fn options_cap_at_nine_because_the_digits_are_the_affordance() {
        let question = (1..=12)
            .map(|index| format!("{}. option{index}", index % 10))
            .collect::<Vec<_>>()
            .join("\n");
        // "0." is not a marker, so the twelve lines offer ten real options; nine survive.
        assert_eq!(parse_options(&question).len(), 9);
    }

    #[test]
    fn project_label_is_the_trailing_two_components() {
        assert_eq!(
            project_label(Path::new("/home/me/git/acme/widget")),
            "acme/widget"
        );
        assert_eq!(project_label(Path::new("/solo")), "solo");
        assert_eq!(project_label(Path::new("/")), "");
    }

    #[test]
    fn context_is_pending_until_it_is_set() {
        let queue = triage_queue(&[blocked("a", 1, Some("q"), "")]);
        let mut state = TriageState::new(queue);
        assert_eq!(
            state.context_pending().map(|item| item.id.clone()),
            Some("a".to_string())
        );
        state.set_context(BackendKind::Claude, "a".into(), vec!["you · hi".into()]);
        assert!(state.context_pending().is_none());
        assert_eq!(state.context(), Some(["you · hi".to_string()].as_slice()));
    }
}
