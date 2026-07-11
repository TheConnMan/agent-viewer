use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use agent_viewer_core::backend::{Backend, all_backends};
use agent_viewer_core::pty::{PtySession, spec_from_command};
use agent_viewer_core::spawn::now_ms;
use agent_viewer_core::state::{ViewerDb, apply_viewer_state, match_spawn};
use agent_viewer_core::{BackendKind, Session};
use agent_viewer_tui::app::{App, KillStage};
use agent_viewer_tui::attach::key_to_bytes;
use agent_viewer_tui::ui::{
    self, AttachView, ModalField, Mode, NewModal, PeekCache, RenameModal,
};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

const TICK: Duration = Duration::from_millis(1000);
const POLL: Duration = Duration::from_millis(200);
/// Abandoned spawn records (no matching session after this long) are deleted.
const SPAWN_ABANDON_MS: i64 = 600_000;

type Key = (BackendKind, String);

/// Everything the run loop mutates, threaded through the key/tick handlers.
struct Ui {
    app: App,
    mode: Mode,
    notice: String,
    db: Option<ViewerDb>,
    peek: PeekCache,
    /// Detached-but-live PTYs, keyed by session. Reused on re-attach; dropped (killed)
    /// on quit — conversation state persists in each backend's own store.
    attached: HashMap<Key, PtySession>,
    /// The focused session while in `Mode::Attached` (input target + header snapshot).
    focused: Option<Key>,
    focused_session: Option<Session>,
    /// Whether the focused PTY's child has exited (refreshed each frame; drives the
    /// "process exited" header). Read-only during draw so the render path stays `&`.
    focused_exited: bool,
}

fn main() -> io::Result<()> {
    let mut backends = all_backends();
    let db = ViewerDb::open_default().ok();

    // Startup refresh BEFORE entering the alt screen. If every backend fails to list,
    // print the errors to stderr and exit non-zero without a UI.
    let mut last: Vec<Vec<Session>> = vec![Vec::new(); backends.len()];
    let (mut sessions, notice, ok_count) = refresh(&mut backends, &mut last);
    if ok_count == 0 {
        eprintln!("agent-viewer: no backend could be listed");
        if !notice.is_empty() {
            eprintln!("{notice}");
        }
        std::process::exit(1);
    }
    if let Some(db) = &db {
        overlay(db, &mut sessions);
    }

    let mut ui = Ui {
        app: App::new(sessions),
        mode: Mode::Normal,
        notice,
        db,
        peek: PeekCache::new(),
        attached: HashMap::new(),
        focused: None,
        focused_session: None,
        focused_exited: false,
    };

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut backends, &mut last, &mut ui);
    ratatui::restore();
    result
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    backends: &mut [Box<dyn Backend>],
    last: &mut [Vec<Session>],
    ui: &mut Ui,
) -> io::Result<()> {
    let mut last_tick = Instant::now();
    loop {
        let now = now_ms();
        ui.peek.refresh(ui.app.selected());

        // Refresh the focused PTY's exit flag (needs &mut) before the &-only draw.
        ui.focused_exited = match &ui.focused {
            Some(key) => ui
                .attached
                .get_mut(key)
                .map(|pty| pty.is_exited())
                .unwrap_or(true),
            None => false,
        };

        // Build the attach view (if focused) before borrowing the frame.
        let attach = build_attach_view(ui);
        terminal.draw(|frame| {
            ui::draw(frame, &ui.app, &ui.mode, &ui.notice, &ui.peek, now, attach);
        })?;

        if event::poll(POLL)? {
            match event::read()? {
                Event::Key(key)
                    if key.kind == KeyEventKind::Press
                        && handle_key(key, backends, last, ui, terminal)? =>
                {
                    return Ok(());
                }
                Event::Resize(_, _) => {
                    if let Some(key) = &ui.focused
                        && let Some(pty) = ui.attached.get_mut(key)
                    {
                        let size = terminal.size()?;
                        let _ = pty.resize(size.height.saturating_sub(1).max(1), size.width.max(1));
                    }
                }
                _ => {}
            }
        }

        if last_tick.elapsed() >= TICK {
            tick_refresh(backends, last, ui, false);
            last_tick = Instant::now();
        }
    }
}

/// Assemble the `AttachView` for the focused session, if any.
fn build_attach_view(ui: &Ui) -> Option<AttachView<'_>> {
    if !matches!(ui.mode, Mode::Attached) {
        return None;
    }
    let key = ui.focused.as_ref()?;
    let pty = ui.attached.get(key)?;
    let session = ui.focused_session.as_ref()?;
    Some(AttachView {
        session,
        pty,
        exited: ui.focused_exited,
    })
}

/// Refresh sessions, apply the viewer overlay, resolve spawn records, prune dead PTYs.
///
/// `preserve_notice` keeps the current `ui.notice` intact instead of replacing it with
/// the refresh's own notice — used right after an action (e.g. a spawn) so its
/// success/failure feedback is not clobbered by the immediately-following refresh.
fn tick_refresh(
    backends: &mut [Box<dyn Backend>],
    last: &mut [Vec<Session>],
    ui: &mut Ui,
    preserve_notice: bool,
) {
    let (mut sessions, notice, _ok) = refresh(backends, last);
    if let Some(db) = &ui.db {
        overlay(db, &mut sessions);
        // Prune stale resolved pins now that we know which sessions are live: a >7-day
        // resolved row whose session no longer appears is dead weight, but one still in
        // the fresh list keeps its pin regardless of age.
        let live: HashSet<Key> = sessions
            .iter()
            .map(|s| (s.backend, s.id.clone()))
            .collect();
        let _ = db.prune_resolved_missing(&live);
    }
    // Update the focused-session header snapshot from the fresh list.
    if let Some(key) = &ui.focused
        && let Some(s) = sessions
            .iter()
            .find(|s| s.backend == key.0 && s.id == key.1)
    {
        ui.focused_session = Some(s.clone());
    }
    ui.app.set_sessions(sessions);
    if !preserve_notice && matches!(ui.mode, Mode::Normal) {
        ui.notice = notice;
    }
    prune_exited(ui);
}

/// Drop PTYs whose child has exited, EXCEPT the focused one (its final screen stays
/// visible until the user detaches).
fn prune_exited(ui: &mut Ui) {
    let focused = ui.focused.clone();
    let keys: Vec<Key> = ui.attached.keys().cloned().collect();
    for key in keys {
        if Some(&key) == focused.as_ref() {
            continue;
        }
        if ui.attached.get_mut(&key).is_some_and(|pty| pty.is_exited()) {
            ui.attached.remove(&key);
        }
    }
}

/// Apply the viewer DB overlay: renames/pins/pids/stopped, clear stale stopped keys,
/// resolve or expire spawn records.
fn overlay(db: &ViewerDb, sessions: &mut [Session]) {
    if let Ok(state) = db.viewer_state() {
        let stale = apply_viewer_state(sessions, &state);
        for (backend, id) in stale {
            let _ = db.clear_stopped(backend, &id);
        }
    }
    if let Ok(records) = db.unresolved_spawns() {
        let now = now_ms();
        for record in records {
            match match_spawn(&record, sessions) {
                Some(id) => {
                    let _ = db.resolve_spawn(record.rowid, &id);
                }
                None if now - record.spawned_at_ms > SPAWN_ABANDON_MS => {
                    let _ = db.delete_spawn(record.rowid);
                }
                None => {}
            }
        }
    }
}

/// Returns `true` when the app should quit.
fn handle_key(
    key: KeyEvent,
    backends: &mut [Box<dyn Backend>],
    last: &mut [Vec<Session>],
    ui: &mut Ui,
    terminal: &mut ratatui::DefaultTerminal,
) -> io::Result<bool> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match &mut ui.mode {
        Mode::Attached => handle_attached_key(key, ui),
        Mode::Normal => return handle_normal_key(key, ctrl, backends, ui, terminal),
        Mode::Filter => handle_filter_key(key.code, ui),
        Mode::Peek => {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char(' ')) {
                ui.mode = Mode::Normal;
            }
        }
        Mode::Help => {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
                ui.mode = Mode::Normal;
            }
        }
        Mode::New(_) => handle_new_key(key.code, backends, last, ui),
        Mode::Rename(_) => handle_rename_key(key.code, backends, ui),
    }
    Ok(false)
}

fn handle_normal_key(
    key: KeyEvent,
    ctrl: bool,
    backends: &mut [Box<dyn Backend>],
    ui: &mut Ui,
    terminal: &mut ratatui::DefaultTerminal,
) -> io::Result<bool> {
    match key.code {
        KeyCode::Char('q') if ctrl => {} // Ctrl+q only meaningful while attached
        KeyCode::Char('q') => {
            ui.attached.clear(); // drop = kill owned children
            return Ok(true);
        }
        KeyCode::Char('j') | KeyCode::Down => ui.app.move_selection(1),
        KeyCode::Char('k') | KeyCode::Up => ui.app.move_selection(-1),
        KeyCode::Char('s') if ctrl => ui.app.toggle_group_mode(),
        KeyCode::Char('r') if ctrl => open_rename(ui),
        KeyCode::Char('x') if ctrl => kill_selected(backends, ui),
        KeyCode::Char('a') => ui.app.toggle_show_all(),
        KeyCode::Char(' ') if ui.app.selected().is_some() => ui.mode = Mode::Peek,
        KeyCode::Char('?') => ui.mode = Mode::Help,
        KeyCode::Char('/') => {
            ui.app.set_filter(String::new());
            ui.notice = String::new();
            ui.mode = Mode::Filter;
        }
        KeyCode::Char('h') => hide_selected(backends, ui, true),
        KeyCode::Char('u') => hide_selected(backends, ui, false),
        KeyCode::Char('n') => {
            let dir = ui
                .app
                .selected_group_root()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            ui.mode = Mode::New(NewModal {
                backend: BackendKind::Codex,
                dir,
                task: String::new(),
                field: ModalField::Task,
            });
        }
        KeyCode::Enter | KeyCode::Right => attach_selected(backends, ui, terminal)?,
        _ => {}
    }
    Ok(false)
}

/// While attached: Ctrl+q detaches; a dead child detaches on any key; everything else
/// is encoded to bytes and written to the PTY.
fn handle_attached_key(key: KeyEvent, ui: &mut Ui) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let Some(fkey) = ui.focused.clone() else {
        ui.mode = Mode::Normal;
        return;
    };

    // Ctrl+q always detaches (PTY lives on in the map).
    if ctrl && matches!(key.code, KeyCode::Char('q')) {
        ui.mode = Mode::Normal;
        ui.focused = None;
        return;
    }

    let Some(pty) = ui.attached.get_mut(&fkey) else {
        ui.mode = Mode::Normal;
        ui.focused = None;
        return;
    };

    // If the child has exited, any key drops the dead PTY and returns to the list.
    if pty.is_exited() {
        ui.attached.remove(&fkey);
        ui.mode = Mode::Normal;
        ui.focused = None;
        return;
    }

    if let Some(bytes) = key_to_bytes(key) {
        let _ = pty.write_input(&bytes);
    }
}

fn handle_filter_key(code: KeyCode, ui: &mut Ui) {
    match code {
        KeyCode::Esc => {
            ui.app.set_filter(String::new());
            ui.mode = Mode::Normal;
        }
        KeyCode::Enter => ui.mode = Mode::Normal,
        KeyCode::Backspace => {
            let mut f = ui.app.filter().to_string();
            f.pop();
            ui.app.set_filter(f);
        }
        KeyCode::Char(c) => {
            let mut f = ui.app.filter().to_string();
            f.push(c);
            ui.app.set_filter(f);
        }
        _ => {}
    }
}

fn handle_new_key(
    code: KeyCode,
    backends: &mut [Box<dyn Backend>],
    last: &mut [Vec<Session>],
    ui: &mut Ui,
) {
    let Mode::New(modal) = &mut ui.mode else {
        return;
    };
    match code {
        KeyCode::Esc => ui.mode = Mode::Normal,
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
            spawn_from_modal(backends, ui);
            ui.mode = Mode::Normal;
            // Preserve the spawn notice — the refresh must not clobber its feedback.
            tick_refresh(backends, last, ui, true);
        }
        _ => {}
    }
}

fn handle_rename_key(code: KeyCode, backends: &mut [Box<dyn Backend>], ui: &mut Ui) {
    let Mode::Rename(modal) = &mut ui.mode else {
        return;
    };
    match code {
        KeyCode::Esc => ui.mode = Mode::Normal,
        KeyCode::Backspace => {
            modal.buffer.pop();
        }
        KeyCode::Char(c) => modal.buffer.push(c),
        KeyCode::Enter => {
            apply_rename(backends, ui);
            ui.mode = Mode::Normal;
        }
        _ => {}
    }
}

// --- Actions --------------------------------------------------------------------

/// Open the rename modal for the selected session (claude falls back to the local
/// name override on apply, so it opens regardless of the backend's rename capability).
fn open_rename(ui: &mut Ui) {
    let Some(session) = ui.app.selected() else {
        return;
    };
    ui.mode = Mode::Rename(RenameModal {
        backend: session.backend,
        id: session.id.clone(),
        buffer: session.title.clone(),
    });
}

fn apply_rename(backends: &mut [Box<dyn Backend>], ui: &mut Ui) {
    let Mode::Rename(modal) = &ui.mode else {
        return;
    };
    let backend_kind = modal.backend;
    let id = modal.id.clone();
    let name = modal.buffer.clone();
    let Some(session) = ui
        .app
        .selected()
        .filter(|s| s.backend == backend_kind && s.id == id)
        .cloned()
    else {
        return;
    };
    let Some(backend) = backends.iter().find(|b| b.kind() == backend_kind) else {
        return;
    };
    match backend.rename(&session, &name) {
        Ok(()) => {
            // A prior daemon-down rename may have left a name override with the OLD name;
            // the overlay applies it unconditionally and would shadow this native rename.
            // Clear the override so the native title shows through.
            if let Some(db) = &ui.db {
                let _ = db.clear_name_override(backend_kind, &id);
            }
            ui.notice = format!("renamed {}", backend_kind.name());
        }
        Err(e) => {
            // claude: fall back to the viewer-DB name override (live sessions only).
            if backend_kind == BackendKind::Claude
                && let Some(db) = &ui.db
                && db.set_name_override(backend_kind, &id, &name).is_ok()
            {
                ui.notice = "renamed (local override)".to_string();
            } else {
                ui.notice = format!("rename failed: {e}");
            }
        }
    }
}

fn kill_selected(backends: &mut [Box<dyn Backend>], ui: &mut Ui) {
    let now = now_ms();
    let stage = ui.app.kill_stage(now);
    let Some(session) = ui.app.selected().cloned() else {
        return;
    };
    let Some(backend) = backends.iter().find(|b| b.kind() == session.backend) else {
        return;
    };
    let caps = backend.capabilities();
    match stage {
        KillStage::Stop => {
            if !caps.stop {
                ui.notice = format!("{} does not support stop", session.backend.name());
                return;
            }
            match backend.stop(&session) {
                Ok(()) => {
                    if let Some(db) = &ui.db {
                        let _ = db.mark_stopped(session.backend, &session.id);
                    }
                    ui.notice = format!("stopped — {}", session.title);
                }
                Err(e) => ui.notice = format!("stop failed: {e}"),
            }
        }
        KillStage::Remove => {
            if !caps.remove {
                ui.notice = format!("{} does not support remove", session.backend.name());
                return;
            }
            match backend.remove(&session.id) {
                Ok(()) => ui.notice = format!("removed — {}", session.title),
                Err(e) => ui.notice = format!("remove failed: {e}"),
            }
        }
        KillStage::Noop => {
            if !caps.stop {
                ui.notice = format!("{} cannot be stopped", session.backend.name());
            }
        }
    }
}

fn hide_selected(backends: &[Box<dyn Backend>], ui: &mut Ui, hide: bool) {
    let Some(session) = ui.app.selected().cloned() else {
        return;
    };
    let Some(backend) = backends.iter().find(|b| b.kind() == session.backend) else {
        return;
    };
    if !backend.capabilities().hide {
        ui.notice = format!("{} does not support hide", backend.kind().name());
        return;
    }
    let result = if hide {
        backend.hide(&session.id)
    } else {
        backend.unhide(&session.id)
    };
    if let Err(e) = result {
        ui.notice = format!("{}: {e}", backend.kind().name());
    }
}

fn attach_selected(
    backends: &[Box<dyn Backend>],
    ui: &mut Ui,
    terminal: &mut ratatui::DefaultTerminal,
) -> io::Result<()> {
    let Some(session) = ui.app.selected().cloned() else {
        return Ok(());
    };
    let Some(backend) = backends.iter().find(|b| b.kind() == session.backend) else {
        return Ok(());
    };
    if !backend.capabilities().attach {
        ui.notice = format!("{} does not support attach", backend.kind().name());
        return Ok(());
    }

    let key: Key = (session.backend, session.id.clone());
    let size = terminal.size()?;
    let rows = size.height.saturating_sub(1).max(1);
    let cols = size.width.max(1);

    if let Some(pty) = ui.attached.get_mut(&key) {
        // Re-attach: reuse the live PTY, resizing it to the current content area.
        let _ = pty.resize(rows, cols);
    } else {
        let Some(command) = backend.attach_command(&session) else {
            ui.notice = format!("{} does not support attach", backend.kind().name());
            return Ok(());
        };
        let spec = spec_from_command(&command, rows, cols);
        match PtySession::spawn(spec) {
            Ok(pty) => {
                ui.attached.insert(key.clone(), pty);
            }
            Err(e) => {
                ui.notice = format!("attach failed: {e}");
                return Ok(());
            }
        }
    }

    ui.focused = Some(key);
    ui.focused_session = Some(session);
    ui.mode = Mode::Attached;
    Ok(())
}

fn spawn_from_modal(backends: &[Box<dyn Backend>], ui: &mut Ui) {
    let Mode::New(modal) = &ui.mode else {
        return;
    };
    let Some(backend) = backends.iter().find(|b| b.kind() == modal.backend) else {
        return;
    };
    if !backend.capabilities().spawn {
        ui.notice = format!("{} does not support spawn", backend.kind().name());
        return;
    }
    let dir = PathBuf::from(&modal.dir);
    let backend_kind = modal.backend;
    match backend.spawn(&dir, &modal.task) {
        Ok(Some(pid)) => {
            // Record the spawn so the overlay can pin (and later stop) the session.
            if let Some(db) = &ui.db {
                let _ = db.record_spawn(backend_kind, &dir, pid, now_ms());
            }
            ui.notice = format!("spawned on {}", backend_kind.name());
        }
        Ok(None) => ui.notice = format!("spawned on {}", backend_kind.name()),
        Err(e) => ui.notice = format!("spawn failed: {e}"),
    }
}

/// List every backend, concatenate results. On a backend error keep its last good
/// snapshot and surface the error text. Returns (sessions, notice, ok_count).
fn refresh(
    backends: &mut [Box<dyn Backend>],
    last: &mut [Vec<Session>],
) -> (Vec<Session>, String, usize) {
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
