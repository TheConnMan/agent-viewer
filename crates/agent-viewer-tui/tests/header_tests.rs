use std::cell::RefCell;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use agent_viewer_core::{BackendKind, Session, SessionOrigin, Status};
use agent_viewer_tui::app::{App, Composer};
use agent_viewer_tui::pr_cache::PrStatusCache;
use agent_viewer_tui::terminal_title::{format_terminal_title, set_terminal_title};
use agent_viewer_tui::ui::{Draw, ListHit, Mode, Pulses, ThemeState, draw};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn session(id: &str, status: Status) -> Session {
    Session {
        backend: BackendKind::Codex,
        id: id.to_string(),
        short_id: None,
        origin: SessionOrigin::Interactive,
        title: id.to_string(),
        cwd: PathBuf::from("/sessions/project"),
        git_branch: None,
        status,
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

fn render(app: &App, workspace: &Path, width: u16) -> Vec<String> {
    let mode = Mode::Normal;
    let composer = Composer::new();
    let pulses = Pulses::new();
    let pr_status = PrStatusCache::new();
    let list_hit = RefCell::new(ListHit::default());
    let themes = ThemeState::default();
    let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();

    terminal
        .draw(|frame| {
            draw(
                frame,
                Draw {
                    app,
                    workspace,
                    mode: &mode,
                    notice: "",
                    composer: &composer,
                    pulses: &pulses,
                    now_ms: 0,
                    attach: None,
                    pr_status: &pr_status,
                    logos: None,
                    list_hit: &list_hit,
                    themes: &themes,
                    sprite: Default::default(),
                    age_ramp: false,
                    tail: None,
                    wall: None,
                    wall_rects: &std::cell::RefCell::new(Vec::new()),
                },
            );
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            let row: String = (0..width).map(|x| buffer[(x, y)].symbol()).collect();
            row.trim_end().to_string()
        })
        .collect()
}

#[test]
fn completed_count_classifies_every_status() {
    let app = App::new(vec![
        session("needs", Status::needs_input()),
        session("working", Status::Working),
        session("idle", Status::Idle),
        session("done", Status::Done),
        session("error", Status::Error),
        session("unknown", Status::Unknown),
    ]);

    assert_eq!(app.needs_input_count(), 1);
    assert_eq!(app.running_count(), 1);
    assert_eq!(app.completed_count(), 2);
}

#[test]
fn header_renders_three_exact_content_rows_with_breathing_room() {
    let workspace = Path::new("/home/theconnman/git/theconnman/agent-viewer");
    let rows = render(&App::new(Vec::new()), workspace, 100);

    assert_eq!(rows[0], "");
    assert_eq!(
        rows[1],
        format!(" [av] Agent Viewer v{}", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        rows[2],
        " Workspace /home/theconnman/git/theconnman/agent-viewer"
    );
    assert_eq!(rows[3], " 0 awaiting input · 0 working · 0 completed");
    assert_eq!(rows[4], "");
    assert_eq!(rows[5], "");
}

#[test]
fn refreshed_counts_ignore_row_visibility() {
    let mut app = App::new(Vec::new());
    assert_eq!(
        (
            app.needs_input_count(),
            app.running_count(),
            app.completed_count()
        ),
        (0, 0, 0)
    );

    let visible_needs = session("visible needs", Status::needs_input());
    let mut hidden_needs = session("hidden needs", Status::needs_input());
    hidden_needs.hidden = true;
    let visible_working = session("visible working", Status::Working);
    let mut companion_working = session("companion working", Status::Working);
    companion_working.companion = true;
    let mut hidden_done = session("hidden done", Status::Done);
    hidden_done.hidden = true;
    let mut companion_error = session("companion error", Status::Error);
    companion_error.companion = true;

    app.set_sessions(vec![
        visible_needs,
        hidden_needs,
        visible_working,
        companion_working,
        hidden_done,
        companion_error,
    ]);
    assert_eq!(
        (
            app.needs_input_count(),
            app.running_count(),
            app.completed_count()
        ),
        (2, 2, 2)
    );

    app.set_filter("visible".to_string());
    assert_eq!(
        (
            app.needs_input_count(),
            app.running_count(),
            app.completed_count()
        ),
        (2, 2, 2)
    );

    app.toggle_show_all();
    assert_eq!(
        (
            app.needs_input_count(),
            app.running_count(),
            app.completed_count()
        ),
        (2, 2, 2)
    );
}

#[test]
fn narrow_header_keeps_the_product_mark_and_name() {
    let rows = render(
        &App::new(Vec::new()),
        Path::new("/home/theconnman/git/theconnman/agent-viewer"),
        18,
    );

    assert_eq!(rows[1], " [av] Agent Viewer");
}

#[test]
fn terminal_title_uses_the_launch_directory_name() {
    assert_eq!(
        format_terminal_title(Path::new("/home/theconnman/git/theconnman/agent-viewer")),
        "Agent Viewer · agent-viewer"
    );
}

#[test]
fn terminal_title_falls_back_for_root() {
    assert_eq!(format_terminal_title(Path::new("/")), "Agent Viewer");
}

#[test]
fn terminal_title_removes_malicious_control_characters() {
    let cases = [
        ("bell\u{7}name", "Agent Viewer · bellname"),
        ("escape\u{1b}name", "Agent Viewer · escapename"),
        ("line\nbreak", "Agent Viewer · linebreak"),
    ];

    for (basename, expected) in cases {
        assert_eq!(
            format_terminal_title(&Path::new("/tmp").join(basename)),
            expected
        );
    }

    let all_controls: String = (0..=char::MAX as u32)
        .filter_map(char::from_u32)
        .filter(|character| character.is_control())
        .collect();
    assert_eq!(
        format_terminal_title(&Path::new("/tmp").join(format!("safe{all_controls}name"))),
        "Agent Viewer · safename"
    );
}

#[test]
fn terminal_title_falls_back_when_sanitization_is_empty() {
    let all_controls: String = (0..=char::MAX as u32)
        .filter_map(char::from_u32)
        .filter(|character| character.is_control())
        .collect();

    assert_eq!(
        format_terminal_title(&Path::new("/tmp").join(all_controls)),
        "Agent Viewer"
    );
}

#[test]
fn set_terminal_title_writes_the_exact_osc_sequence() {
    let mut output = Vec::new();

    set_terminal_title(
        &mut output,
        Path::new("/home/theconnman/git/theconnman/agent-viewer"),
    );

    assert_eq!(
        output,
        "\u{1b}]0;Agent Viewer · agent-viewer\u{7}".as_bytes()
    );
}

struct RejectingWriter;

impl Write for RejectingWriter {
    fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("unsupported"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::other("unsupported"))
    }
}

#[test]
fn set_terminal_title_ignores_writer_errors() {
    set_terminal_title(
        &mut RejectingWriter,
        Path::new("/home/theconnman/git/theconnman/agent-viewer"),
    );
}
