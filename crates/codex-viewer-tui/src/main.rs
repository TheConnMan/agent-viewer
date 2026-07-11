use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use codex_viewer_core::backend::{Backend, all_backends};
use codex_viewer_core::{BackendKind, Session};
use codex_viewer_tui::app::App;
use codex_viewer_tui::ui::{self, Mode, ModalField, NewModal, TranscriptCache};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};

const TICK: Duration = Duration::from_millis(1000);
const POLL: Duration = Duration::from_millis(200);

fn main() -> io::Result<()> {
    let mut backends = all_backends();

    // Startup refresh BEFORE entering the alt screen. If every backend fails to
    // list, print the errors to stderr and exit non-zero without a UI.
    let mut last: Vec<Vec<Session>> = vec![Vec::new(); backends.len()];
    let (sessions, notice, ok_count) = refresh(&mut backends, &mut last);
    if ok_count == 0 {
        eprintln!("codex-agent-viewer: no backend could be listed");
        if !notice.is_empty() {
            eprintln!("{notice}");
        }
        std::process::exit(1);
    }

    let mut app = App::new(sessions);
    let mut mode = Mode::Normal;
    let mut notice = notice;

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut backends, &mut last, &mut app, &mut mode, &mut notice);
    ratatui::restore();
    result
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    backends: &mut [Box<dyn Backend>],
    last: &mut [Vec<Session>],
    app: &mut App,
    mode: &mut Mode,
    notice: &mut String,
) -> io::Result<()> {
    let mut last_tick = Instant::now();
    let mut transcript = TranscriptCache::new();
    loop {
        transcript.refresh(app.selected().and_then(|s| s.rollout_path.as_deref()));
        terminal.draw(|frame| ui::draw(frame, app, mode, notice, &transcript))?;

        if event::poll(POLL)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && handle_key(key.code, backends, last, app, mode, notice, terminal)?
        {
            return Ok(());
        }

        if last_tick.elapsed() >= TICK {
            let (sessions, tick_notice, _ok) = refresh(backends, last);
            app.set_sessions(sessions);
            if matches!(mode, Mode::Normal) {
                *notice = tick_notice;
            }
            last_tick = Instant::now();
        }
    }
}

/// Returns `true` when the app should quit.
fn handle_key(
    code: KeyCode,
    backends: &mut [Box<dyn Backend>],
    last: &mut [Vec<Session>],
    app: &mut App,
    mode: &mut Mode,
    notice: &mut String,
    terminal: &mut ratatui::DefaultTerminal,
) -> io::Result<bool> {
    match mode {
        Mode::Normal => match code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('j') | KeyCode::Down => app.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => app.move_selection(-1),
            KeyCode::Char('a') => app.toggle_show_hidden(),
            KeyCode::Char(' ') => app.toggle_collapse_selected(),
            KeyCode::Char('/') => {
                app.set_filter(String::new());
                *notice = String::new();
                *mode = Mode::Filter;
            }
            KeyCode::Char('h') => hide_selected(backends, app, notice, true),
            KeyCode::Char('u') => hide_selected(backends, app, notice, false),
            KeyCode::Char('n') => {
                let dir = app
                    .selected_group_root()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                *mode = Mode::New(NewModal {
                    backend: BackendKind::Codex,
                    dir,
                    task: String::new(),
                    field: ModalField::Task,
                });
            }
            KeyCode::Enter => attach_selected(backends, app, notice, terminal)?,
            _ => {}
        },
        Mode::Filter => match code {
            KeyCode::Esc => {
                app.set_filter(String::new());
                *mode = Mode::Normal;
            }
            KeyCode::Enter => *mode = Mode::Normal,
            KeyCode::Backspace => {
                let mut f = app.filter().to_string();
                f.pop();
                app.set_filter(f);
            }
            KeyCode::Char(c) => {
                let mut f = app.filter().to_string();
                f.push(c);
                app.set_filter(f);
            }
            _ => {}
        },
        Mode::New(modal) => match code {
            KeyCode::Esc => *mode = Mode::Normal,
            KeyCode::Tab | KeyCode::Down => modal.next_field(),
            KeyCode::Left if modal.field == ModalField::Backend => modal.cycle_backend(false),
            KeyCode::Right if modal.field == ModalField::Backend => modal.cycle_backend(true),
            KeyCode::Backspace => match modal.field {
                ModalField::Dir => {
                    modal.dir.pop();
                }
                ModalField::Task => {
                    modal.task.pop();
                }
                ModalField::Backend => {}
            },
            KeyCode::Char(c) => match modal.field {
                ModalField::Dir => modal.dir.push(c),
                ModalField::Task => modal.task.push(c),
                ModalField::Backend => {}
            },
            KeyCode::Enter => {
                spawn_from_modal(backends, modal, notice);
                *mode = Mode::Normal;
                let (sessions, _, _) = refresh(backends, last);
                app.set_sessions(sessions);
            }
            _ => {}
        },
    }
    Ok(false)
}

fn hide_selected(backends: &[Box<dyn Backend>], app: &App, notice: &mut String, hide: bool) {
    let Some(session) = app.selected() else {
        return;
    };
    let Some(backend) = backends.iter().find(|b| b.kind() == session.backend) else {
        return;
    };
    if !backend.capabilities().hide {
        *notice = format!("{} does not support hide", backend.kind().name());
        return;
    }
    let result = if hide {
        backend.hide(&session.id)
    } else {
        backend.unhide(&session.id)
    };
    if let Err(e) = result {
        *notice = format!("{}: {e}", backend.kind().name());
    }
}

fn attach_selected(
    backends: &[Box<dyn Backend>],
    app: &App,
    notice: &mut String,
    terminal: &mut ratatui::DefaultTerminal,
) -> io::Result<()> {
    let Some(session) = app.selected() else {
        return Ok(());
    };
    let Some(backend) = backends.iter().find(|b| b.kind() == session.backend) else {
        return Ok(());
    };
    let Some(mut command) = backend.attach_command(session) else {
        *notice = format!("{} does not support attach", backend.kind().name());
        return Ok(());
    };

    // Leave the alt screen / raw mode, run the attach command with inherited
    // stdio, WAIT for it, then re-enter and let the next tick refresh.
    ratatui::restore();
    let status = command.status();
    *terminal = ratatui::init();
    terminal.clear()?;
    match status {
        Ok(_) => {}
        Err(e) => *notice = format!("attach failed: {e}"),
    }
    Ok(())
}

fn spawn_from_modal(backends: &[Box<dyn Backend>], modal: &NewModal, notice: &mut String) {
    let Some(backend) = backends.iter().find(|b| b.kind() == modal.backend) else {
        return;
    };
    if !backend.capabilities().spawn {
        *notice = format!("{} does not support spawn", backend.kind().name());
        return;
    }
    let dir = PathBuf::from(&modal.dir);
    if let Err(e) = backend.spawn(&dir, &modal.task) {
        *notice = format!("spawn failed: {e}");
    } else {
        *notice = format!("spawned on {}", backend.kind().name());
    }
}

/// List every backend, concatenate results. On a backend error keep its last good
/// snapshot and surface the error text. Returns (sessions, notice, ok_count).
fn refresh(backends: &mut [Box<dyn Backend>], last: &mut [Vec<Session>]) -> (Vec<Session>, String, usize) {
    let mut all = Vec::new();
    let mut errors = Vec::new();
    let mut ok_count = 0;
    for (i, backend) in backends.iter_mut().enumerate() {
        match backend.list() {
            Ok(sessions) => {
                ok_count += 1;
                last[i] = sessions.clone();
                all.extend(sessions);
            }
            Err(e) => {
                errors.push(format!("{}: {e}", backend.kind().name()));
                all.extend(last[i].clone());
            }
        }
    }
    (all, errors.join("  |  "), ok_count)
}
