//! The Ctrl+N triage inbox: a modal over the list that walks the needs-input sessions oldest
//! first, one at a time, with the session itself live inside it.
//!
//! The modal owns the queue (order, position, up-next) and nothing else. The panel in the
//! middle is the real attached session — the same `PtySession` the list's Enter attaches to,
//! rendered into a bounded rect instead of the whole screen — and every key that is not one of
//! the three reserved chords is written straight to that child. So the "question" shown is
//! whatever the agent actually drew, and an answer is delivered by the agent's own input
//! handling rather than by anything this module invents.
//!
//! This replaced a hand-rolled viewer that parsed the transcript, re-rendered the question,
//! enumerated the options itself, and submitted through `send_reply`. It read one truncated
//! line of a summary and could not deliver at all (`send_reply` is a stub). Delegating to
//! attach is both smaller and strictly more faithful.

use std::path::Path;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::{Theme, backend_mark, backend_mark_color, fg};
use agent_viewer_core::{BackendKind, Session, Status};

/// Columns of margin between the modal and the screen edge, and rows above/below it. The modal
/// is deliberately near-full-screen: the panel hosts a real agent UI, and a genuinely small box
/// makes the child wrap its own layout into uselessness. The margin is what keeps the list
/// visible around it, so it still reads as a modal over the list rather than a takeover.
const MARGIN_X: u16 = 2;
const MARGIN_Y: u16 = 1;
/// Chrome rows inside the modal border: the item header above the panel, and the up-next and
/// hint rows below it.
const HEADER_ROWS: u16 = 1;
const FOOTER_ROWS: u16 = 2;
/// How many "up next" titles the one-line queue preview names.
const UP_NEXT_SHOWN: usize = 3;
/// Below this the panel cannot host a child at all and the modal declines to draw.
const MIN_PANEL_ROWS: u16 = 3;
const MIN_PANEL_COLS: u16 = 20;

/// One blocked session in the queue, flattened to what the modal chrome renders. The body no
/// longer needs the question or the transcript: the attached child draws both itself.
#[derive(Clone, Debug, PartialEq)]
pub struct TriageItem {
    pub backend: BackendKind,
    pub id: String,
    pub title: String,
    pub project: String,
    pub updated_at_ms: i64,
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
        .filter(|session| matches!(session.status, Status::NeedsInput { .. }))
        .map(|session| TriageItem {
            backend: session.backend,
            id: session.id.clone(),
            title: session.title.clone(),
            project: project_label(&session.cwd),
            updated_at_ms: session.updated_at_ms,
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

/// The modal's live state. The queue is snapshotted at open: a background refresh must not
/// reorder or resize it under the user's fingers mid-answer.
#[derive(Debug)]
pub struct TriageState {
    items: Vec<TriageItem>,
    index: usize,
}

impl TriageState {
    pub fn new(items: Vec<TriageItem>) -> TriageState {
        TriageState { items, index: 0 }
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
        let end = (start + UP_NEXT_SHOWN).min(self.items.len());
        &self.items[start..end]
    }

    /// Step to the next item. Returns false when the queue is finished — it never wraps,
    /// because a wrap would make the `3 of 7` counter a lie.
    pub fn advance(&mut self) -> bool {
        self.index += 1;
        self.index < self.items.len()
    }

    /// Step back one item. False (and no move) when already on the first.
    pub fn back(&mut self) -> bool {
        if self.index == 0 {
            return false;
        }
        self.index -= 1;
        true
    }
}

/// The modal rect for a given frame, and the panel rect inside it that the child renders into.
///
/// One function so the three places that need panel geometry cannot drift: the draw, the pty
/// size chosen when the child is spawned, and the resize handler. A child sized to anything
/// other than what it is drawn into wraps its own output at the wrong column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TriageLayout {
    pub modal: Rect,
    pub panel: Rect,
}

/// `None` when the frame is too small to host a child at all.
pub fn layout(frame_area: Rect) -> Option<TriageLayout> {
    let width = frame_area.width.checked_sub(MARGIN_X * 2)?;
    let height = frame_area.height.checked_sub(MARGIN_Y * 2)?;
    let modal = Rect {
        x: frame_area.x + MARGIN_X,
        y: frame_area.y + MARGIN_Y,
        width,
        height,
    };
    // Inside the modal border, then inside the panel's own border.
    let inner_w = width.checked_sub(2)?;
    let inner_h = height.checked_sub(2)?;
    let panel_h = inner_h
        .checked_sub(HEADER_ROWS + FOOTER_ROWS)?
        .checked_sub(2)?;
    let panel_w = inner_w.checked_sub(2)?;
    if panel_h < MIN_PANEL_ROWS || panel_w < MIN_PANEL_COLS {
        return None;
    }
    let panel = Rect {
        x: modal.x + 2,
        y: modal.y + 1 + HEADER_ROWS + 1,
        width: panel_w,
        height: panel_h,
    };
    Some(TriageLayout { modal, panel })
}

/// The pty size for the current frame: exactly the panel's interior.
pub fn panel_pty_size(frame_area: Rect) -> Option<(u16, u16)> {
    layout(frame_area).map(|l| (l.panel.height.max(1), l.panel.width.max(1)))
}

/// Draw the modal. `screen` renders the live child into the panel; absent (still attaching, or
/// the attach failed) the panel says so rather than rendering a hole.
pub(super) fn draw(
    frame: &mut Frame,
    state: &TriageState,
    screen: Option<&super::AttachView<'_>>,
    now_ms: i64,
    theme: &Theme,
) {
    let Some(item) = state.current() else {
        return;
    };
    let Some(TriageLayout { modal, panel }) = layout(frame.area()) else {
        return;
    };

    frame.render_widget(Clear, modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(fg(theme.accent))
        .style(Style::default().bg(theme.surface).fg(theme.text));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let full = inner.width as usize;

    // Header: the progress affordance carries the meaning without color, so it survives mono16.
    let (position, total) = state.progress();
    let progress = format!("{position} of {total}");
    let age = crate::app::format_elapsed(now_ms - item.updated_at_ms);
    let right = format!("waiting {age} · oldest first");
    let mark = backend_mark(item.backend, theme);
    let left_width = display(&progress) + 1 + display(mark) + 1 + display(&item.title) + 1;
    let pad = full.saturating_sub(left_width + display(&right) + 1);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!("{progress} "), fg(theme.accent)),
            Span::styled(
                format!("{mark} "),
                fg(backend_mark_color(item.backend, theme)),
            ),
            Span::styled(format!("{} ", item.title), fg(theme.text)),
            Span::styled(item.project.clone(), fg(theme.muted)),
            Span::raw(" ".repeat(pad)),
            Span::styled(right, fg(theme.faint)),
        ])),
        Rect {
            height: HEADER_ROWS,
            ..inner
        },
    );

    // The panel: a bordered box whose interior is the live child.
    let panel_box = Rect {
        x: inner.x,
        y: inner.y + HEADER_ROWS,
        width: inner.width,
        height: panel.height + 2,
    };
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(fg(theme.border)),
        panel_box,
    );
    match screen {
        Some(av) => super::attach::render_screen(frame, panel, av),
        None => frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  attaching…".to_string(),
                fg(theme.faint),
            ))),
            panel,
        ),
    }

    let mut y = panel_box.y + panel_box.height;
    let upcoming = state.upcoming();
    if !upcoming.is_empty() {
        let names = upcoming
            .iter()
            .map(|next| next.title.clone())
            .collect::<Vec<_>>()
            .join(" · ");
        let remaining = total.saturating_sub(position + upcoming.len());
        let tail = if remaining > 0 {
            format!(" · +{remaining} more")
        } else {
            String::new()
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("up next  ", fg(theme.faint)),
                Span::styled(
                    ellipsize(&format!("{names}{tail}"), full.saturating_sub(9)),
                    fg(theme.muted),
                ),
            ])),
            Rect {
                y,
                height: 1,
                ..inner
            },
        );
        y += 1;
    }

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "⌃N next · ⌃P previous · ⌃] leave · every other key goes to the session",
            fg(theme.faint),
        ))),
        Rect {
            y,
            height: 1,
            ..inner
        },
    );
}

fn display(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// PURE: `text` cut to `width` columns with a trailing ellipsis when it does not fit.
fn ellipsize(text: &str, width: usize) -> String {
    if display(text) <= width || width == 0 {
        return text.to_string();
    }
    let mut out = String::new();
    for character in text.chars() {
        if display(&out) + display(&character.to_string()) > width.saturating_sub(1) {
            break;
        }
        out.push(character);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_viewer_core::{SessionOrigin, Status};

    fn session(id: &str, updated_at_ms: i64, status: Status) -> Session {
        Session {
            backend: BackendKind::Claude,
            id: id.to_string(),
            short_id: Some(id.to_string()),
            origin: SessionOrigin::Interactive,
            title: id.to_string(),
            cwd: "/home/b/git/example-user/agent-viewer".into(),
            git_branch: None,
            status,
            created_at_ms: 0,
            updated_at_ms,
            hidden: false,
            companion: false,
            summary: String::new(),
            pid: None,
            rollout_path: None,
            pr_refs: Vec::new(),
            daemon_hosted: false,
        }
    }

    fn blocked(id: &str, updated_at_ms: i64) -> Session {
        session(
            id,
            updated_at_ms,
            Status::NeedsInput {
                reason: Some("pick one".to_string()),
            },
        )
    }

    #[test]
    fn the_queue_is_the_blocked_sessions_oldest_first() {
        let items = triage_queue(&[
            blocked("newer", 3_000),
            session("busy", 1_000, Status::Working),
            blocked("older", 2_000),
        ]);
        assert_eq!(
            items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["older", "newer"],
            "only blocked sessions, longest wait first"
        );
    }

    #[test]
    fn the_project_label_is_the_last_two_path_components() {
        let items = triage_queue(&[blocked("one", 1)]);
        assert_eq!(items[0].project, "example-user/agent-viewer");
    }

    #[test]
    fn walking_the_queue_never_wraps() {
        let mut state = TriageState::new(triage_queue(&[blocked("a", 1), blocked("b", 2)]));
        assert_eq!(state.progress(), (1, 2));
        assert!(!state.back(), "already on the first item");
        assert!(state.advance(), "second item exists");
        assert_eq!(state.progress(), (2, 2));
        assert!(
            !state.advance(),
            "running off the end reports finished rather than wrapping to the top"
        );
    }

    /// The child wraps its own output at the column it was told about, so a pty sized to
    /// anything other than the rect it is drawn into renders mis-wrapped text.
    #[test]
    fn the_pty_size_is_exactly_the_rect_the_child_is_drawn_into() {
        let frame = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 40,
        };
        let layout = layout(frame).expect("a 120x40 frame hosts a panel");
        assert_eq!(
            panel_pty_size(frame),
            Some((layout.panel.height, layout.panel.width))
        );
        assert!(
            layout.panel.height >= 30,
            "the panel gets most of the screen so the child UI is usable, got {}",
            layout.panel.height
        );
    }

    #[test]
    fn a_frame_too_small_to_host_a_child_reports_no_layout() {
        assert_eq!(
            layout(Rect {
                x: 0,
                y: 0,
                width: 18,
                height: 6
            }),
            None
        );
    }

    #[test]
    fn the_panel_stays_inside_the_modal_border() {
        for width in [24u16, 60, 200] {
            for height in [10u16, 24, 60] {
                let frame = Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                };
                let Some(l) = layout(frame) else { continue };
                assert!(l.panel.x > l.modal.x, "{width}x{height}");
                assert!(l.panel.y > l.modal.y, "{width}x{height}");
                assert!(
                    l.panel.x + l.panel.width < l.modal.x + l.modal.width,
                    "{width}x{height} panel overruns the right border"
                );
                assert!(
                    l.panel.y + l.panel.height < l.modal.y + l.modal.height,
                    "{width}x{height} panel overruns the bottom chrome"
                );
            }
        }
    }
}
