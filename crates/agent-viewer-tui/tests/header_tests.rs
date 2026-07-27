use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use agent_viewer_core::opencode::OpencodeRuntime;
use agent_viewer_core::{BackendKind, Session, SessionOrigin, Status};
use agent_viewer_tui::app::{App, Composer};
use agent_viewer_tui::pr_cache::PrStatusCache;
use agent_viewer_tui::terminal_title::{format_terminal_title, set_terminal_title};
use agent_viewer_tui::ui::{Draw, ListHit, Mode, PeekCache, Pulses, draw};
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

fn render_with_peek(
    app: &App,
    workspace: &Path,
    width: u16,
    peek: &PeekCache,
    expanded: Option<&(BackendKind, String)>,
) -> Vec<String> {
    let mode = Mode::Normal;
    let composer = Composer::new();
    let pulses = Pulses::new();
    let pr_status = PrStatusCache::new();
    let list_hit = RefCell::new(ListHit::default());
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
                    peek,
                    composer: &composer,
                    pulses: &pulses,
                    expanded,
                    now_ms: 0,
                    attach: None,
                    pr_status: &pr_status,
                    logos: None,
                    list_hit: &list_hit,
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

fn render(app: &App, workspace: &Path, width: u16) -> Vec<String> {
    render_with_peek(app, workspace, width, &PeekCache::new(), None)
}

struct PeekServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl PeekServer {
    fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let responses = [
                (Duration::ZERO, "{\"healthy\":true,\"version\":\"1.17.20\"}"),
                (
                    Duration::from_millis(220),
                    "[{\"info\":{\"role\":\"assistant\",\"time\":{\"created\":1}},\"parts\":[{\"type\":\"text\",\"text\":\"STALE SERVER MESSAGE\"}]}]",
                ),
                (Duration::ZERO, "{\"healthy\":true,\"version\":\"1.17.20\"}"),
                (
                    Duration::ZERO,
                    "[{\"info\":{\"role\":\"assistant\",\"time\":{\"created\":2}},\"parts\":[{\"type\":\"text\",\"text\":\"FRESH SERVER MESSAGE\"}]}]",
                ),
            ];
            let mut next = 0;
            while !stop_for_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut request = Vec::new();
                        let mut buffer = [0_u8; 512];
                        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                            match stream.read(&mut buffer) {
                                Ok(0) | Err(_) => break,
                                Ok(read) => request.extend_from_slice(&buffer[..read]),
                            }
                        }
                        let (delay, body) = responses
                            .get(next)
                            .copied()
                            .unwrap_or((Duration::ZERO, "{}"));
                        next += 1;
                        std::thread::sleep(delay);
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.flush();
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            addr,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for PeekServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn unused_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    addr
}

fn opencode_session(id: &str, updated_at_ms: i64, status: Status) -> Session {
    let mut session = session(id, status);
    session.backend = BackendKind::Opencode;
    session.updated_at_ms = updated_at_ms;
    session.daemon_hosted = true;
    session
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
    let workspace = Path::new("/home/example/git/example-user/agent-viewer");
    let rows = render(&App::new(Vec::new()), workspace, 100);

    assert_eq!(rows[0], "");
    assert_eq!(
        rows[1],
        format!(" [av] Agent Viewer v{}", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        rows[2],
        " Workspace /home/example/git/example-user/agent-viewer"
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
fn opencode_peek_is_nonblocking_rejects_stale_results_and_renders_input_reason() {
    let server = PeekServer::spawn();
    let runtime = OpencodeRuntime::for_test(
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        PathBuf::from("/"),
        |_| Err(io::Error::other("peek must never launch a server")),
    );
    let mut peek = PeekCache::with_opencode_runtime(runtime);
    let stale = opencode_session("ses_stale", 1, Status::Idle);
    let fresh = opencode_session(
        "ses_fresh",
        2,
        Status::NeedsInput {
            reason: Some("Choose the deployment target".to_string()),
        },
    );

    let refresh_started = Instant::now();
    peek.refresh(Some(&stale));
    assert!(
        refresh_started.elapsed() < Duration::from_millis(75),
        "refresh waited for the delayed HTTP response"
    );
    peek.refresh(Some(&fresh));

    let app = App::new(vec![fresh.clone()]);
    let expanded = (BackendKind::Opencode, fresh.id.clone());
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut rendered = String::new();
    while Instant::now() < deadline {
        peek.refresh(Some(&fresh));
        rendered = render_with_peek(
            &app,
            Path::new("/sessions/project"),
            100,
            &peek,
            Some(&expanded),
        )
        .join("\n");
        assert!(
            !rendered.contains("STALE SERVER MESSAGE"),
            "a completed response for the old selection flashed on the new row"
        );
        if rendered.contains("FRESH SERVER MESSAGE") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(
        rendered.contains("FRESH SERVER MESSAGE"),
        "the matching completed server response never rendered"
    );
    assert!(
        rendered.contains("Awaiting input: Choose the deployment target"),
        "{rendered}"
    );
}

#[test]
fn narrow_header_keeps_the_product_mark_and_name() {
    let rows = render(
        &App::new(Vec::new()),
        Path::new("/home/example/git/example-user/agent-viewer"),
        18,
    );

    assert_eq!(rows[1], " [av] Agent Viewer");
}

#[test]
fn terminal_title_uses_the_launch_directory_name() {
    assert_eq!(
        format_terminal_title(Path::new("/home/example/git/example-user/agent-viewer")),
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
        Path::new("/home/example/git/example-user/agent-viewer"),
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
        Path::new("/home/example/git/example-user/agent-viewer"),
    );
}
