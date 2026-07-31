//! The Ctrl+B tail pane: a fixed-width side pane showing the selected session's last few
//! turns.
//!
//! It reads the backend's TRANSCRIPT. It never starts a process, because the pane retargets
//! on every arrow key: spawning to fill it would mean one agent process per keystroke, and
//! for Codex an app-server daemon join (per SPEC.md a plain `codex resume` forks a snapshot
//! and fabricates an interrupt). A session that finished days ago would have to be
//! resurrected just to read what it said.
//!
//! The one exception costs nothing: when a live PTY for this session already exists in the
//! attach map, its screen is rendered instead, for exact fidelity. It is never resized —
//! the attached session owns its winsize.

use super::attach::ThemedPseudoTerminal;
use super::composer::wrap_by_width;
use super::{fg, truncate};
use crate::ui::theme::Theme;
use agent_viewer_core::TailEvent;
use agent_viewer_core::pty::PtySession;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// The pane's fixed width, from the design frame.
pub const TAIL_WIDTH: u16 = 46;

/// Below this the list would be crushed, so Ctrl+B refuses rather than opening a pane that
/// makes both halves unreadable. 100 columns is the design's documented narrow width.
pub const TAIL_MIN_TOTAL_WIDTH: u16 = 100;

/// How many transcript events the pane asks for.
pub const TAIL_EVENTS: usize = 12;

/// The prefix on a tool line (U+23BF), from the design frame.
const TOOL_PREFIX: &str = "⎿";

pub struct TailView<'a> {
    pub session: &'a agent_viewer_core::Session,
    /// `None` until the background read for this session lands; `Some(&[])` once it has
    /// landed and there was nothing to show.
    pub events: Option<&'a [TailEvent]>,
    /// A PTY that ALREADY exists for this session. Never created for the pane.
    pub live: Option<&'a PtySession>,
}

pub(super) fn draw(frame: &mut Frame, view: &TailView, theme: &Theme, area: Rect) {
    if area.width < 4 || area.height < 3 {
        return;
    }
    // Column 0 is the rule that separates the pane from the list; content starts two in.
    let rule = Rect { width: 1, ..area };
    frame.render_widget(
        Paragraph::new(
            (0..area.height)
                .map(|_| Line::from(Span::styled("│", fg(theme.border))))
                .collect::<Vec<_>>(),
        ),
        rule,
    );
    let content = Rect {
        x: area.x + 2,
        width: area.width - 3,
        ..area
    };
    let width = content.width as usize;

    let source = if view.live.is_some() {
        "live"
    } else {
        "transcript"
    };
    let head = vec![
        Line::from(vec![
            Span::styled("tail", fg(theme.accent)),
            Span::styled(" · ", fg(theme.muted)),
            Span::styled(
                truncate(&view.session.title, width.saturating_sub(7)),
                fg(theme.text),
            ),
        ]),
        Line::from(Span::styled(
            truncate(&format!("{source} · last {TAIL_EVENTS} turns"), width),
            fg(theme.faint),
        )),
        Line::from(""),
    ];
    frame.render_widget(
        Paragraph::new(head),
        Rect {
            height: 3.min(content.height),
            ..content
        },
    );

    let body = Rect {
        y: content.y + 3,
        height: content.height.saturating_sub(3),
        ..content
    };
    if body.height == 0 {
        return;
    }

    // A live PTY is exact fidelity for anything touched this session, and free: it is
    // already running and already parsed.
    if let Some(pty) = view.live {
        pty.with_screen(|screen| {
            frame.render_widget(
                ThemedPseudoTerminal {
                    screen,
                    palette: pty.palette(),
                },
                body,
            );
        });
        return;
    }

    let mut lines = match view.events {
        None => vec![Line::from(Span::styled(
            "reading transcript…",
            fg(theme.faint),
        ))],
        Some([]) => vec![Line::from(Span::styled(
            "nothing to show yet",
            fg(theme.faint),
        ))],
        Some(events) => event_lines(events, width, theme),
    };
    // It is a TAIL: when the events overflow the pane, the newest lines are the ones worth
    // keeping.
    let height = body.height as usize;
    if lines.len() > height {
        lines = lines.split_off(lines.len() - height);
    }
    frame.render_widget(Paragraph::new(lines), body);
}

/// Render events oldest first: prose wraps, a tool call is one truncated line.
fn event_lines<'a>(events: &[TailEvent], width: usize, theme: &Theme) -> Vec<Line<'a>> {
    let mut lines = Vec::new();
    for event in events {
        match event {
            TailEvent::Agent(text) => lines.extend(prose(text, width, theme.text)),
            TailEvent::User(text) => lines.extend(prose(text, width, theme.muted)),
            TailEvent::Tool { name, detail } => {
                let body = if detail.is_empty() {
                    format!("{TOOL_PREFIX} {name}")
                } else {
                    format!("{TOOL_PREFIX} {name}  {detail}")
                };
                lines.push(Line::from(Span::styled(
                    truncate(&body, width),
                    fg(theme.faint),
                )));
            }
        }
    }
    lines
}

fn prose<'a>(text: &str, width: usize, color: ratatui::style::Color) -> Vec<Line<'a>> {
    wrap_by_width(text.trim_end(), width, width)
        .into_iter()
        .map(|segment| Line::from(Span::styled(segment, fg(color))))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_viewer_core::{BackendKind, Session, SessionOrigin, Status};
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn done_session() -> Session {
        Session {
            backend: BackendKind::Codex,
            id: "tail-1".to_string(),
            short_id: None,
            origin: SessionOrigin::Interactive,
            title: "SRE agent scaffold".to_string(),
            cwd: "/tmp".into(),
            git_branch: None,
            status: Status::Done,
            created_at_ms: 0,
            updated_at_ms: 0,
            hidden: false,
            companion: false,
            summary: String::new(),
            pid: None,
            rollout_path: None,
            pr_refs: Vec::new(),
            daemon_hosted: false,
        }
    }

    fn row(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    /// The column a row's first non-blank CONTENT cell sits at, for reading that cell's
    /// color. Starts past the rule, which is inked on every row.
    fn first_ink(buffer: &Buffer, y: u16) -> Option<u16> {
        (2..buffer.area.width).find(|x| buffer[(*x, y)].symbol().trim() != "")
    }

    fn render(view: &TailView, theme: &Theme, width: u16, height: u16) -> Buffer {
        let mut terminal = ratatui::Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, view, theme, frame.area()))
            .expect("draw tail pane");
        terminal.backend().buffer().clone()
    }

    #[test]
    fn done_session_with_no_live_process_renders_its_transcript() {
        let theme = crate::ui::theme::amber(false);
        let session = done_session();
        let events = vec![
            TailEvent::Agent("Reading the token bucket to find the drift.".to_string()),
            TailEvent::Tool {
                name: "read".to_string(),
                detail: "src/limiter/bucket.rs".to_string(),
            },
            TailEvent::User("pin the clock first".to_string()),
        ];
        let view = TailView {
            session: &session,
            events: Some(&events),
            // A done session has no live process, and the pane must never make one.
            live: None,
        };
        let buffer = render(&view, &theme, TAIL_WIDTH, 12);

        assert!(row(&buffer, 0).contains("tail · SRE agent scaffold"));
        // The source label says where this came from: the transcript, not a live child.
        assert!(row(&buffer, 1).contains("transcript · last 12 turns"));
        let body = (3..12).map(|y| row(&buffer, y)).collect::<Vec<_>>();
        assert!(
            body.iter().any(|line| line.contains("Reading the token")),
            "agent prose missing: {body:?}"
        );
        assert!(
            body.iter()
                .any(|line| line.contains("⎿ read  src/limiter/bucket.rs")),
            "tool line missing: {body:?}"
        );
        assert!(
            body.iter().any(|line| line.contains("pin the clock first")),
            "user prose missing: {body:?}"
        );
    }

    #[test]
    fn prose_and_tool_lines_carry_their_own_theme_tokens() {
        let theme = crate::ui::theme::amber(false);
        let session = done_session();
        let events = vec![
            TailEvent::Agent("agent prose".to_string()),
            TailEvent::Tool {
                name: "bash".to_string(),
                detail: "cargo test".to_string(),
            },
            TailEvent::User("user prose".to_string()),
        ];
        let view = TailView {
            session: &session,
            events: Some(&events),
            live: None,
        };
        let buffer = render(&view, &theme, TAIL_WIDTH, 8);

        // Body starts at row 3: agent, tool, user, one line each at this width.
        let color = |y: u16| buffer[(first_ink(&buffer, y).expect("ink"), y)].fg;
        assert!(row(&buffer, 3).contains("agent prose"));
        assert_eq!(color(3), theme.text);
        assert!(row(&buffer, 4).contains("⎿ bash  cargo test"));
        assert_eq!(color(4), theme.faint);
        assert!(row(&buffer, 5).contains("user prose"));
        assert_eq!(color(5), theme.muted);
        // Every token is distinct in this theme, so none of the three assertions above can
        // pass by accident.
        assert_ne!(theme.text, theme.faint);
        assert_ne!(theme.text, theme.muted);
        // The rule separating the pane from the list is the border token.
        assert_eq!(buffer[(0, 3)].symbol(), "│");
        assert_eq!(buffer[(0, 3)].fg, theme.border);
    }

    #[test]
    fn an_overflowing_transcript_keeps_its_newest_lines() {
        let theme = crate::ui::theme::amber(false);
        let session = done_session();
        let events = (0..20)
            .map(|n| TailEvent::Agent(format!("line {n}")))
            .collect::<Vec<_>>();
        let view = TailView {
            session: &session,
            events: Some(&events),
            live: None,
        };
        // 3 header rows + 4 body rows.
        let buffer = render(&view, &theme, TAIL_WIDTH, 7);
        let body = (3..7).map(|y| row(&buffer, y)).collect::<Vec<_>>();
        assert!(
            body.iter().any(|line| line.contains("line 19")),
            "the newest line must survive: {body:?}"
        );
        assert!(
            !body.iter().any(|line| line.contains("line 0 ")),
            "the oldest line must be dropped: {body:?}"
        );
    }

    #[test]
    fn an_unread_session_says_so_instead_of_claiming_it_is_empty() {
        let theme = crate::ui::theme::amber(false);
        let session = done_session();
        let unread = TailView {
            session: &session,
            events: None,
            live: None,
        };
        assert!(row(&render(&unread, &theme, TAIL_WIDTH, 6), 3).contains("reading transcript"));

        let empty = TailView {
            session: &session,
            events: Some(&[]),
            live: None,
        };
        assert!(row(&render(&empty, &theme, TAIL_WIDTH, 6), 3).contains("nothing to show yet"));
    }
}
