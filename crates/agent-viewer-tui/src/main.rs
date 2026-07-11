use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use agent_viewer_core::backend::{Backend, BackendKind, Capabilities, all_backends};
use agent_viewer_core::claude::{ClaudeBackend, ensure_trusted};
use agent_viewer_core::codex::CodexBackend;
use agent_viewer_core::opencode::OpencodeBackend;
use agent_viewer_core::pty::{PtySession, spec_from_command};
use agent_viewer_core::spawn::now_ms;
use agent_viewer_core::state::{ViewerDb, apply_viewer_state, match_spawn};
use agent_viewer_core::{Session, Status, default_codex_home, mark_dead_dirs};
use agent_viewer_tui::app::{App, Composer, DetachTracker, KillStage, Row};
use agent_viewer_tui::attach::key_to_bytes;
use agent_viewer_tui::mutations::MutationRunner;
use agent_viewer_tui::ui::{self, AttachView, Mode, PeekCache, Pulses, RenameModal};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// How often the refresh worker re-lists the backends (off the UI thread).
const REFRESH_INTERVAL: Duration = Duration::from_millis(1000);
/// How long a footer notice stays up (age-based, independent of loop phase).
const NOTICE_MS: i64 = 4000;
/// Base event-poll cadence; drops to `FAST_POLL` while the list is animating.
const POLL: Duration = Duration::from_millis(100);
const FAST_POLL: Duration = Duration::from_millis(120);
/// The spawn bloom lasts ~400ms; a pulse older than this is garbage-collected.
const PULSE_MS: i64 = 400;
/// How long to keep watching a live claude attach for the agents view before giving up on
/// the one-shot auto-Enter. Generous because `claude agents` can take 8-15s+ to boot
/// (plugin/MCP startup); harmless while armed since any user key disarms it.
const AUTO_ENTER_TIMEOUT: Duration = Duration::from_secs(45);
/// The marker must stay visible this long before we press Enter, so we land when claude is
/// actually accepting input rather than on the first painted (still-initializing) frame.
const AUTO_ENTER_SETTLE: Duration = Duration::from_millis(500);
/// Stage-1 marker: the agents list is up and the preselected row is ready.
const CLAUDE_AGENTS_MARKER: &str = "describe a task for a new session";
/// Stage-2 markers (fallback only): if the first Enter merely expanded a collapsed row
/// rather than opening the run, either collapse-hint variant shows and a second Enter opens.
const CLAUDE_EXPANDED_MARKER: &str = "enter to collapse";
const CLAUDE_EXPANDED_MARKER_ALT: &str = "space to reply";
/// Abandoned spawn records (no matching session after this long) are deleted.
const SPAWN_ABANDON_MS: i64 = 600_000;

type Key = (BackendKind, String);
/// A backend-listing snapshot handed from the refresh worker to the UI thread.
type Snapshot = (Vec<Session>, String, usize);

/// The refresh worker's handles: newest-listing snapshots in, forced-refresh wakes out.
struct Refresher {
    snapshots: Receiver<Snapshot>,
    wake: Sender<()>,
}

impl Refresher {
    /// Ask the worker to re-list now instead of waiting out its interval (best-effort).
    fn force(&self) {
        let _ = self.wake.send(());
    }

    /// The freshest pending snapshot, discarding any older queued ones. None if the
    /// worker has not produced a new listing since the last drain.
    fn latest(&self) -> Option<Snapshot> {
        let mut newest = None;
        while let Ok(snap) = self.snapshots.try_recv() {
            newest = Some(snap);
        }
        newest
    }
}

/// Move the listing backends onto a dedicated thread that re-lists every
/// `REFRESH_INTERVAL` (or immediately on a wake) and streams snapshots to the UI. The
/// backends here are the ONLY set that calls `list()`; the UI keeps a separate cheap set
/// for attach/spawn/mutation so a slow `list()` never blocks input or render.
fn spawn_refresh_worker(mut backends: Vec<Box<dyn Backend>>) -> Refresher {
    let (snap_tx, snap_rx) = channel::<Snapshot>();
    let (wake_tx, wake_rx) = channel::<()>();
    thread::spawn(move || {
        let mut last: Vec<Vec<Session>> = vec![Vec::new(); backends.len()];
        loop {
            let snapshot = refresh(&mut backends, &mut last);
            if snap_tx.send(snapshot).is_err() {
                return; // UI gone — stop listing.
            }
            match wake_rx.recv_timeout(REFRESH_INTERVAL) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
    Refresher {
        snapshots: snap_rx,
        wake: wake_tx,
    }
}

/// A footer notice with an age-based lifetime: it survives `NOTICE_MS` from when it was
/// set, regardless of loop phase, so an action notice can never be cleared before it
/// renders. Kept pure (ms in, no clock) so `expired` is unit-testable.
#[derive(Debug, Clone, Default)]
struct NoticeState {
    text: String,
    set_at_ms: i64,
}

impl NoticeState {
    fn new() -> NoticeState {
        NoticeState::default()
    }

    fn set(&mut self, msg: String, now_ms: i64) {
        self.text = msg;
        self.set_at_ms = now_ms;
    }

    fn text(&self) -> &str {
        &self.text
    }

    fn clear(&mut self) {
        self.text.clear();
    }

    /// True once a non-empty notice has aged past `NOTICE_MS`.
    fn expired(&self, now_ms: i64) -> bool {
        !self.text.is_empty() && now_ms - self.set_at_ms >= NOTICE_MS
    }
}

/// Two-stage auto-Enter: `CLAUDE_AGENTS_SELECT` preselects the row but does not expand it,
/// so opening the run takes two returns — one to expand, one to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoEnterStage {
    /// Waiting for the agents list; a settled Enter expands the preselected row.
    AwaitingList,
    /// Row expanded; a settled Enter on the collapse hint opens the run, then disarms.
    AwaitingExpanded,
}

/// One-shot auto-Enter state for a live claude attach: which PTY, when it was armed, which
/// stage we are on, and when the current stage's marker was first seen (settle debounce).
#[derive(Debug, Clone)]
struct AutoEnter {
    key: Key,
    armed_at: Instant,
    stage: AutoEnterStage,
    marker_since: Option<Instant>,
}

/// Everything the run loop mutates, threaded through the key/tick handlers.
struct Ui {
    app: App,
    mode: Mode,
    notice: NoticeState,
    db: Option<ViewerDb>,
    peek: PeekCache,
    /// Inline spawn composer (persistent on the list view).
    composer: Composer,
    /// Per-PTY left-arrow detach gate, keyed like `attached`. Reset only when a new PTY is
    /// spawned; a re-attach reuses the previous pending count (the child's input line may
    /// still hold text). Pruned alongside its PTY.
    detach_trackers: HashMap<Key, DetachTracker>,
    /// The last backend-error string surfaced as a notice (dedup memo, so a recurring error
    /// neither restamps nor starves action notices).
    last_backend_error: String,
    /// Blocking backend mutations run off the render thread.
    mutations: MutationRunner,
    /// Live one-shot spawn blooms, keyed by session -> start now_ms.
    pulses: Pulses,
    /// The row expanded in place for an inline peek (one at a time), or None.
    expanded: Option<Key>,
    /// A one-shot auto-Enter armed on a live claude attach. While set, the run loop watches
    /// the PTY for the agents view and presses Enter once (after a settle) to land in the
    /// preselected run. Cleared on trigger, timeout, user key, or PTY prune.
    auto_enter: Option<AutoEnter>,
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

impl Ui {
    /// Set the footer notice, stamping it so the run loop can age it out after NOTICE_MS.
    fn set_notice(&mut self, msg: String) {
        self.notice.set(msg, now_ms());
    }
}

fn main() -> io::Result<()> {
    // Read the ASCII-marks fallback once, before any rendering.
    ui::set_ascii_marks(std::env::var("AGENT_VIEWER_ASCII_MARKS").as_deref() == Ok("1"));

    let mut list_backends = all_backends();
    let db = ViewerDb::open_default().ok();

    // Startup refresh BEFORE entering the alt screen so the first paint is not empty. If
    // every backend fails to list, print the errors to stderr and exit without a UI.
    let mut last: Vec<Vec<Session>> = vec![Vec::new(); list_backends.len()];
    let (mut sessions, notice, ok_count) = refresh(&mut list_backends, &mut last);
    if ok_count == 0 {
        eprintln!("agent-viewer: no backend could be listed");
        if !notice.is_empty() {
            eprintln!("{notice}");
        }
        std::process::exit(1);
    }
    if let Some(db) = &db {
        let _ = overlay(db, &mut sessions);
    }
    mark_dead_dirs(&mut sessions);

    let mut startup_notice = NoticeState::new();
    if !notice.is_empty() {
        startup_notice.set(notice, now_ms());
    }
    let mut ui = Ui {
        app: App::new(sessions),
        mode: Mode::Normal,
        notice: startup_notice,
        db,
        peek: PeekCache::new(),
        composer: Composer::new(),
        detach_trackers: HashMap::new(),
        last_backend_error: String::new(),
        mutations: MutationRunner::new(),
        pulses: Pulses::new(),
        expanded: None,
        auto_enter: None,
        attached: HashMap::new(),
        focused: None,
        focused_session: None,
        focused_exited: false,
    };

    // Hand the listing backends to the refresh worker; the UI keeps a separate cheap set
    // (stateless builders) for the non-list calls: attach_command, spawn, capabilities,
    // and the mutation closures. Only the worker set ever calls the slow list().
    let refresher = spawn_refresh_worker(list_backends);
    let action_backends = all_backends();

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &action_backends, &refresher, &mut ui);
    ratatui::restore();
    result
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    backends: &[Box<dyn Backend>],
    refresher: &Refresher,
    ui: &mut Ui,
) -> io::Result<()> {
    loop {
        let now = now_ms();
        ui.peek.refresh(ui.app.selected());

        // Drain completed background mutations: show the result and hasten a fresh listing.
        let mut mutation_completed = false;
        while let Some(result) = ui.mutations.poll() {
            ui.set_notice(match result {
                Ok(msg) => msg,
                Err(msg) => msg,
            });
            mutation_completed = true;
        }
        if mutation_completed {
            refresher.force();
        }

        // Fold in the freshest off-thread listing (a no-op until the worker sends one).
        apply_snapshot(refresher, ui);
        // Age-based notice expiry: a notice lives NOTICE_MS from when it was set, so it
        // always renders at least once regardless of where the loop is when it lands.
        if matches!(ui.mode, Mode::Normal) && ui.notice.expired(now) {
            ui.notice.clear();
        }

        // Retire finished spawn blooms so their fast-tick pressure and glyph override end.
        ui.pulses.retain(|_, start| now - *start < PULSE_MS);

        // Refresh the focused PTY's exit flag (needs &mut) before the &-only draw.
        ui.focused_exited = match &ui.focused {
            Some(key) => ui
                .attached
                .get_mut(key)
                .map(|pty| pty.is_exited())
                .unwrap_or(true),
            None => false,
        };

        // Drive the one-shot auto-Enter for a live claude attach (lands us in the run).
        drive_auto_enter(ui);

        // Build the attach view (if focused) before borrowing the frame.
        let attach = build_attach_view(ui);
        terminal.draw(|frame| {
            ui::draw(
                frame,
                ui::Draw {
                    app: &ui.app,
                    mode: &ui.mode,
                    notice: ui.notice.text(),
                    peek: &ui.peek,
                    composer: &ui.composer,
                    pulses: &ui.pulses,
                    expanded: ui.expanded.as_ref(),
                    now_ms: now,
                    attach,
                },
            );
        })?;

        // Animate the list faster while there are working/needs-input rows or a live bloom.
        let poll = if wants_fast_ticks(ui) { FAST_POLL } else { POLL };
        if event::poll(poll)? {
            match event::read()? {
                Event::Key(key)
                    if key.kind == KeyEventKind::Press
                        && handle_key(key, backends, refresher, ui, terminal)? =>
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
    }
}

/// The list animates (needs a faster poll) while it shows a working/needs-input row or a
/// live spawn bloom. The attach view owns the screen, so it never fast-ticks.
fn wants_fast_ticks(ui: &Ui) -> bool {
    if matches!(ui.mode, Mode::Attached) {
        return false;
    }
    if !ui.pulses.is_empty() {
        return true;
    }
    ui.app.visible().iter().any(|r| {
        matches!(
            r,
            Row::Session {
                status: Status::Working | Status::NeedsInput,
                ..
            }
        )
    })
}

/// While a live claude attach is armed, watch its PTY for the agents view and press Enter
/// once so we land IN the preselected run rather than sitting on the agents list (the
/// internal autoOpenJobId is not reachable via env/flag). Give up after a timeout; the
/// arming is cleared on any user key or when the PTY is pruned.
fn drive_auto_enter(ui: &mut Ui) {
    let Some(state) = ui.auto_enter.clone() else {
        return;
    };
    // Only drive the currently focused attach.
    if ui.focused.as_ref() != Some(&state.key) {
        return;
    }
    if state.armed_at.elapsed() > AUTO_ENTER_TIMEOUT {
        ui.auto_enter = None;
        return;
    }
    let Some(pty) = ui.attached.get_mut(&state.key) else {
        return;
    };
    // Each stage watches its own marker(s); the settle timer is per-stage. The expanded
    // row's hint renders as one of two variants depending on how the view drew it.
    let visible = pty.with_screen(|screen| {
        let contents = screen.contents();
        match state.stage {
            AutoEnterStage::AwaitingList => contents.contains(CLAUDE_AGENTS_MARKER),
            AutoEnterStage::AwaitingExpanded => {
                contents.contains(CLAUDE_EXPANDED_MARKER)
                    || contents.contains(CLAUDE_EXPANDED_MARKER_ALT)
            }
        }
    });
    if !visible {
        // Marker not up yet (or flickered away) — restart this stage's settle timer.
        if let Some(ae) = &mut ui.auto_enter {
            ae.marker_since = None;
        }
        return;
    }
    match state.marker_since {
        // First frame this stage's marker appears: start the settle debounce, do not press.
        None => {
            if let Some(ae) = &mut ui.auto_enter {
                ae.marker_since = Some(Instant::now());
            }
        }
        Some(since) if since.elapsed() >= AUTO_ENTER_SETTLE => match state.stage {
            // Stage 1: with real preselection (CLAUDE_AGENTS_SELECT now reaches the child)
            // the row comes up pre-expanded, so this Enter opens the run directly — stage 2's
            // marker never appears. Advance anyway as a harmless fallback for a collapsed
            // variant where the first Enter only expands.
            AutoEnterStage::AwaitingList => {
                let _ = pty.write_input(b"\r");
                if let Some(ae) = &mut ui.auto_enter {
                    ae.stage = AutoEnterStage::AwaitingExpanded;
                    ae.marker_since = None;
                }
            }
            // Stage 2 (fallback only): a second Enter opens the expanded row, then disarm.
            AutoEnterStage::AwaitingExpanded => {
                let _ = pty.write_input(b"\r");
                ui.auto_enter = None;
            }
        },
        Some(_) => {}
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

/// Fold the freshest off-thread listing into the app: viewer overlay (+ spawn-bloom
/// starts), dead-dir hiding, focused-header snapshot, and dead-PTY pruning. Backend-error
/// text surfaces as a notice; an action notice is left untouched (the 1s cadence in `run`
/// clears those). A no-op until the worker produces a new listing.
fn apply_snapshot(refresher: &Refresher, ui: &mut Ui) {
    let Some((mut sessions, notice, _ok)) = refresher.latest() else {
        return;
    };
    if let Some(db) = &ui.db {
        // Newly-resolved viewer spawns kick off a one-shot bloom on their fresh row.
        let resolved = overlay(db, &mut sessions);
        let now = now_ms();
        for key in resolved {
            ui.pulses.entry(key).or_insert(now);
        }
        // Prune stale resolved pins now that we know which sessions are live: a >7-day
        // resolved row whose session no longer appears is dead weight, but one still in
        // the fresh list keeps its pin regardless of age.
        let live: HashSet<Key> = sessions
            .iter()
            .map(|s| (s.backend, s.id.clone()))
            .collect();
        let _ = db.prune_resolved_missing(&live);
    }
    // Hide sessions whose cwd was deleted (after the overlay, so viewer-spawn pins in live
    // dirs stay visible while deleted-dir noise defaults to hidden — `a` still reveals it).
    mark_dead_dirs(&mut sessions);
    // Update the focused-session header snapshot from the fresh list.
    if let Some(key) = &ui.focused
        && let Some(s) = sessions
            .iter()
            .find(|s| s.backend == key.0 && s.id == key.1)
    {
        ui.focused_session = Some(s.clone());
    }
    ui.app.set_sessions(sessions);
    // Surface a backend-error notice, but do not let a per-second recurring error restamp
    // (which would starve spawn/mutation feedback and make the error effectively permanent):
    // show it only when it CHANGED and the footer is free (empty/expired, or already the old
    // error). Reset the memo when the error clears so a later recurrence shows again.
    if notice.is_empty() {
        ui.last_backend_error.clear();
    } else if matches!(ui.mode, Mode::Normal)
        && backend_error_should_show(
            &notice,
            ui.notice.text(),
            ui.notice.expired(now_ms()),
            &ui.last_backend_error,
        )
    {
        ui.last_backend_error = notice.clone();
        ui.set_notice(notice);
    }
    prune_exited(ui);
}

/// Whether a snapshot's backend-error `err` should replace the current footer notice, given
/// the current notice text, whether it has expired, and the last error already shown. A blank
/// or unchanged error never shows (so a recurring error neither restamps nor starves an action
/// notice); a changed error shows only when the footer is free (empty/expired) or is itself
/// the previous backend error.
fn backend_error_should_show(err: &str, current: &str, current_expired: bool, last_error: &str) -> bool {
    if err.is_empty() || err == last_error {
        return false;
    }
    current.is_empty() || current_expired || current == last_error
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
            ui.detach_trackers.remove(&key);
            if ui.auto_enter.as_ref().map(|ae| &ae.key) == Some(&key) {
                ui.auto_enter = None;
            }
        }
    }
}

/// Apply the viewer DB overlay: renames/pins/pids/stopped, clear stale stopped keys,
/// resolve or expire spawn records. Returns the (backend, id) of any spawn record that
/// resolved to a session this pass, so the caller can start its one-shot bloom.
fn overlay(db: &ViewerDb, sessions: &mut [Session]) -> Vec<Key> {
    if let Ok(state) = db.viewer_state() {
        let stale = apply_viewer_state(sessions, &state);
        for (backend, id) in stale {
            let _ = db.clear_stopped(backend, &id);
        }
    }
    let mut resolved = Vec::new();
    if let Ok(records) = db.unresolved_spawns() {
        let now = now_ms();
        for record in records {
            match match_spawn(&record, sessions) {
                Some(id) => {
                    let _ = db.resolve_spawn(record.rowid, &id);
                    resolved.push((record.backend, id));
                }
                None if now - record.spawned_at_ms > SPAWN_ABANDON_MS => {
                    let _ = db.delete_spawn(record.rowid);
                }
                None => {}
            }
        }
    }
    resolved
}

// --- Background mutations -------------------------------------------------------

/// A blocking backend mutation, run on a worker thread with all data owned (Send).
enum Mutation {
    Stop(Session),
    Remove(Session),
    Rename(Session, String),
    Hide(Session),
    Unhide(Session),
}

/// A fresh backend instance for a worker thread. The mutating methods (stop/remove/
/// rename/hide) depend only on the passed id/session, never on cached list state, so a
/// fresh instance behaves identically to the one in the main `backends` slice.
fn fresh_backend(kind: BackendKind) -> Box<dyn Backend> {
    match kind {
        BackendKind::Codex => Box::new(CodexBackend::new(default_codex_home())),
        BackendKind::Claude => Box::new(ClaudeBackend::new()),
        BackendKind::Opencode => Box::new(OpencodeBackend::new()),
    }
}

/// Run one mutation to completion, applying its viewer-DB follow-up against a fresh
/// connection so the render loop never blocks. Returns the user-facing result message.
fn run_mutation(m: Mutation) -> Result<String, String> {
    match m {
        Mutation::Stop(s) => match fresh_backend(s.backend).stop(&s) {
            Ok(()) => {
                if let Ok(db) = ViewerDb::open_default() {
                    let _ = db.mark_stopped(s.backend, &s.id);
                }
                Ok(format!("stopped — {}", s.title))
            }
            Err(e) => Err(format!("stop failed: {e}")),
        },
        Mutation::Remove(s) => {
            // Terminate the live process FIRST, inside this same thread, before archiving or
            // deleting. Two-stage Ctrl+X submits `stop` then `remove` on different dedup keys,
            // so the two race; killing here guarantees ordering within the remove op and makes
            // a concurrent `stop` harmless — terminate is idempotent (ESRCH/gone -> Ok) and
            // pid-guarded by comm prefix, so it never signals a recycled pid.
            if let Some(pid) = s.pid {
                let _ = agent_viewer_core::spawn::terminate(pid, s.backend.name());
            }
            fresh_backend(s.backend)
                .remove(&s.id)
                .map(|()| format!("removed — {}", s.title))
                .map_err(|e| format!("remove failed: {e}"))
        }
        Mutation::Rename(s, name) => match fresh_backend(s.backend).rename(&s, &name) {
            Ok(()) => {
                // A prior daemon-down rename may have left a stale override; clear it so
                // the native title shows through.
                if let Ok(db) = ViewerDb::open_default() {
                    let _ = db.clear_name_override(s.backend, &s.id);
                }
                Ok(format!("renamed {}", s.backend.name()))
            }
            Err(e) => {
                // claude: fall back to the viewer-DB name override (live sessions only).
                if s.backend == BackendKind::Claude
                    && let Ok(db) = ViewerDb::open_default()
                    && db.set_name_override(s.backend, &s.id, &name).is_ok()
                {
                    return Ok("renamed (local override)".to_string());
                }
                Err(format!("rename failed: {e}"))
            }
        },
        Mutation::Hide(s) => fresh_backend(s.backend)
            .hide(&s.id)
            .map(|()| format!("archived — {}", s.title))
            .map_err(|e| format!("{}: {e}", s.backend.name())),
        Mutation::Unhide(s) => fresh_backend(s.backend)
            .unhide(&s.id)
            .map(|()| format!("unarchived — {}", s.title))
            .map_err(|e| format!("{}: {e}", s.backend.name())),
    }
}

// --- Key routing ----------------------------------------------------------------

/// Returns `true` when the app should quit.
fn handle_key(
    key: KeyEvent,
    backends: &[Box<dyn Backend>],
    refresher: &Refresher,
    ui: &mut Ui,
    terminal: &mut ratatui::DefaultTerminal,
) -> io::Result<bool> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match &mut ui.mode {
        Mode::Attached => handle_attached_key(key, ui),
        Mode::Normal => return handle_normal_key(key, ctrl, backends, refresher, ui, terminal),
        Mode::Filter => handle_filter_key(key.code, ui),
        Mode::Help => {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
                ui.mode = Mode::Normal;
            }
        }
        Mode::Rename(_) => handle_rename_key(key.code, ui),
    }
    Ok(false)
}

fn handle_normal_key(
    key: KeyEvent,
    ctrl: bool,
    backends: &[Box<dyn Backend>],
    refresher: &Refresher,
    ui: &mut Ui,
    terminal: &mut ratatui::DefaultTerminal,
) -> io::Result<bool> {
    // Ctrl-chords always act, regardless of composer state.
    if ctrl {
        match key.code {
            KeyCode::Char('s') => ui.app.toggle_group_mode(),
            KeyCode::Char('r') => open_rename(ui),
            KeyCode::Char('x') => kill_selected(backends, ui),
            _ => {}
        }
        return Ok(false);
    }

    match key.code {
        // Arrows navigate/act at all times. Moving the cursor collapses any inline peek.
        KeyCode::Down => {
            ui.expanded = None;
            ui.app.move_selection(1);
        }
        KeyCode::Up => {
            ui.expanded = None;
            ui.app.move_selection(-1);
        }
        KeyCode::Right => attach_selected(backends, ui, terminal)?,
        // Tab cycles the composer's target backend at any time.
        KeyCode::Tab => ui.composer.cycle_backend(),
        KeyCode::Backspace => ui.composer.backspace(),
        KeyCode::Esc => ui.composer.clear(),
        KeyCode::Enter => {
            if ui.composer.is_empty() {
                attach_selected(backends, ui, terminal)?;
            } else {
                spawn_from_composer(backends, refresher, ui);
            }
        }
        KeyCode::Char(c) => {
            if ui.composer.is_empty() {
                // Empty composer: the command hotkeys still fire; any other printable
                // starts a task (n included — it just types).
                match c {
                    'q' => {
                        ui.attached.clear(); // drop = kill owned children
                        return Ok(true);
                    }
                    'a' => ui.app.toggle_show_all(),
                    'h' => hide_selected(backends, ui, true),
                    'u' => hide_selected(backends, ui, false),
                    '?' => ui.mode = Mode::Help,
                    // Space toggles the inline peek expansion of the selected row.
                    ' ' if ui.app.selected().is_some() => toggle_expand(ui),
                    '/' => {
                        ui.app.set_filter(String::new());
                        ui.notice.clear();
                        ui.mode = Mode::Filter;
                    }
                    _ => ui.composer.push_char(c),
                }
            } else {
                // Non-empty composer: every printable (and space) is task text.
                ui.composer.push_char(c);
            }
        }
        _ => {}
    }
    Ok(false)
}

/// While attached: Ctrl+] always detaches; Left detaches when the input line is empty
/// (else it is forwarded); a dead child detaches on any key; everything else is encoded
/// to bytes and written to the PTY.
fn handle_attached_key(key: KeyEvent, ui: &mut Ui) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let Some(fkey) = ui.focused.clone() else {
        ui.mode = Mode::Normal;
        return;
    };

    // Any key here is the user taking over, so cancel a pending auto-Enter on this attach.
    ui.auto_enter = None;

    // Ctrl+] always detaches (PTY lives on in the map). Terminals send Ctrl+] as raw byte
    // 0x1D, which crossterm's legacy unix parser maps to Char('5')+CTRL (it folds 0x1C..=0x1F
    // onto Ctrl+'4'..'7'); the kitty keyboard protocol delivers the literal Char(']')+CTRL.
    // Match both so the header/help "ctrl+]" is honored under either encoding.
    if ctrl && matches!(key.code, KeyCode::Char(']') | KeyCode::Char('5')) {
        detach_to_list(ui);
        return;
    }

    // If the child has exited, any key drops the dead PTY (and its tracker) and returns.
    let exited = ui
        .attached
        .get_mut(&fkey)
        .map(|pty| pty.is_exited())
        .unwrap_or(true);
    if exited {
        ui.attached.remove(&fkey);
        ui.detach_trackers.remove(&fkey);
        detach_to_list(ui);
        return;
    }

    // Left detaches only when this PTY's input line is empty; otherwise forward it as cursor
    // motion. The tracker is per-PTY so a re-attach preserves any half-typed line.
    if matches!(key.code, KeyCode::Left)
        && ui
            .detach_trackers
            .get(&fkey)
            .is_none_or(|t| t.detach_on_left())
    {
        detach_to_list(ui);
        return;
    }

    // Track pending input so the Left-gate knows whether the line is mid-edit.
    if let Some(tracker) = ui.detach_trackers.get_mut(&fkey) {
        match key.code {
            KeyCode::Char(_) => tracker.on_char(),
            KeyCode::Backspace => tracker.on_backspace(),
            KeyCode::Enter => tracker.on_enter(),
            _ => {}
        }
    }

    if let Some(bytes) = key_to_bytes(key)
        && let Some(pty) = ui.attached.get_mut(&fkey)
    {
        let _ = pty.write_input(&bytes);
    }
}

/// Detach the focused PTY back to the list (the PTY keeps running in the map).
fn detach_to_list(ui: &mut Ui) {
    ui.mode = Mode::Normal;
    ui.focused = None;
}

/// Toggle the inline peek expansion of the selected row (only one expands at a time).
fn toggle_expand(ui: &mut Ui) {
    let Some(session) = ui.app.selected() else {
        return;
    };
    let key = (session.backend, session.id.clone());
    if ui.expanded.as_ref() == Some(&key) {
        ui.expanded = None;
    } else {
        ui.expanded = Some(key);
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

fn handle_rename_key(code: KeyCode, ui: &mut Ui) {
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
            apply_rename(ui);
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

/// Submit the rename to the background runner (the app-server/UDS rename can take 1-2s).
fn apply_rename(ui: &mut Ui) {
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
    let key = format!("{}:{}:rename", backend_kind.name(), id);
    let title = session.title.clone();
    let mutation = Mutation::Rename(session, name.clone());
    if ui
        .mutations
        .submit(key, format!("rename {title}"), move || run_mutation(mutation))
    {
        ui.set_notice(format!("renaming… {name}"));
    }
}

fn kill_selected(backends: &[Box<dyn Backend>], ui: &mut Ui) {
    let now = now_ms();
    let stage = ui.app.kill_stage(now);
    let Some(session) = ui.app.selected().cloned() else {
        return;
    };
    let caps = caps_of(backends, session.backend);
    match stage {
        KillStage::Stop => {
            if !caps.stop {
                ui.set_notice(format!("{} does not support stop", session.backend.name()));
                return;
            }
            submit_mutation(ui, &session, "stop", "stopping", Mutation::Stop(session.clone()));
        }
        KillStage::Remove => {
            if !caps.remove {
                ui.set_notice(format!("{} does not support remove", session.backend.name()));
                return;
            }
            submit_mutation(ui, &session, "remove", "removing", Mutation::Remove(session.clone()));
        }
        KillStage::Noop => {
            if !caps.stop {
                ui.set_notice(format!("{} cannot be stopped", session.backend.name()));
            }
        }
    }
}

fn hide_selected(backends: &[Box<dyn Backend>], ui: &mut Ui, hide: bool) {
    let Some(session) = ui.app.selected().cloned() else {
        return;
    };
    let caps = caps_of(backends, session.backend);
    if !caps.hide {
        ui.set_notice(format!("{} does not support hide", session.backend.name()));
        return;
    }
    if hide {
        submit_mutation(ui, &session, "hide", "archiving", Mutation::Hide(session.clone()));
    } else {
        submit_mutation(ui, &session, "unhide", "unarchiving", Mutation::Unhide(session.clone()));
    }
}

/// Route a blocking mutation to the runner with a backend+id+op dedup key and an
/// immediate "<verb>… <title>" notice (a duplicate keypress while pending is a no-op).
fn submit_mutation(ui: &mut Ui, session: &Session, op: &str, verb: &str, mutation: Mutation) {
    let key = format!("{}:{}:{}", session.backend.name(), session.id, op);
    let label = format!("{verb} {}", session.title);
    if ui.mutations.submit(key, label, move || run_mutation(mutation)) {
        ui.set_notice(format!("{verb}… {}", session.title));
    }
}

/// Capabilities for a backend kind from the live slice (falls back to none if absent).
fn caps_of(backends: &[Box<dyn Backend>], kind: BackendKind) -> Capabilities {
    backends
        .iter()
        .find(|b| b.kind() == kind)
        .map(|b| b.capabilities())
        .unwrap_or(Capabilities {
            spawn: false,
            hide: false,
            attach: false,
            stop: false,
            remove: false,
            rename: false,
        })
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
        ui.set_notice(format!("{} does not support attach", backend.kind().name()));
        return Ok(());
    }

    let key: Key = (session.backend, session.id.clone());
    let size = terminal.size()?;
    let rows = size.height.saturating_sub(1).max(1);
    let cols = size.width.max(1);

    if let Some(pty) = ui.attached.get_mut(&key) {
        // Re-attach: reuse the live PTY, resizing it to the current content area. The
        // per-PTY detach tracker is preserved so a half-typed input line still gates Left.
        let _ = pty.resize(rows, cols);
        ui.detach_trackers.entry(key.clone()).or_default();
    } else {
        // Pre-accept the trust dialog before a claude RESUME attach into a fresh project
        // (best-effort; the live agents-view path and other backends never need it).
        let claude_live = session.pid.is_some()
            || matches!(session.status, Status::Working | Status::NeedsInput);
        if session.backend == BackendKind::Claude && !claude_live {
            let home = std::env::var("HOME").unwrap_or_default();
            let config = std::path::PathBuf::from(&home).join(".claude.json");
            let _ = ensure_trusted(&config, &session.cwd);
        }
        let Some(command) = backend.attach_command(&session) else {
            ui.set_notice(format!("{} does not support attach", backend.kind().name()));
            return Ok(());
        };
        let spec = spec_from_command(&command, rows, cols);
        match PtySession::spawn(spec) {
            Ok(pty) => {
                ui.attached.insert(key.clone(), pty);
                // Fresh Left-gate: a brand-new PTY starts with an empty input line.
                ui.detach_trackers.insert(key.clone(), DetachTracker::new());
                // A live claude attach opens the agents view; arm the one-shot auto-Enter
                // to land in the preselected run (only on a fresh spawn, never a re-attach).
                if session.backend == BackendKind::Claude && claude_live {
                    ui.auto_enter = Some(AutoEnter {
                        key: key.clone(),
                        armed_at: Instant::now(),
                        stage: AutoEnterStage::AwaitingList,
                        marker_since: None,
                    });
                }
            }
            Err(e) => {
                ui.set_notice(format!("attach failed: {e}"));
                return Ok(());
            }
        }
    }

    ui.focused = Some(key);
    ui.focused_session = Some(session);
    ui.mode = Mode::Attached;
    Ok(())
}

/// Spawn the composed task into the current spawn target, record it for pinning, and
/// clear the composer. The spawn itself is detached (fast); only its record persists.
fn spawn_from_composer(
    backends: &[Box<dyn Backend>],
    refresher: &Refresher,
    ui: &mut Ui,
) {
    let Some(target) = ui.app.spawn_target() else {
        ui.set_notice("no target directory".to_string());
        return;
    };
    let backend_kind = ui.composer.backend();
    let Some(backend) = backends.iter().find(|b| b.kind() == backend_kind) else {
        return;
    };
    if !backend.capabilities().spawn {
        ui.set_notice(format!("{} does not support spawn", backend_kind.name()));
        return;
    }
    let task = ui.composer.text().to_string();
    match backend.spawn(&target, &task) {
        Ok(Some(pid)) => {
            // Record the spawn so the overlay can pin (and later stop) the session.
            if let Some(db) = &ui.db {
                let _ = db.record_spawn(backend_kind, &target, pid, now_ms());
            }
            ui.set_notice(format!("spawned on {}", backend_kind.name()));
        }
        Ok(None) => ui.set_notice(format!("spawned on {}", backend_kind.name())),
        Err(e) => {
            // Keep the composer text so the user can retry.
            ui.set_notice(format!("spawn failed: {e}"));
            return;
        }
    }
    ui.composer.clear();
    // Hasten the next listing so the spawned row (and its bloom) appears promptly; the
    // notice survives until the 1s clear cadence since apply_snapshot preserves it.
    refresher.force();
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

#[cfg(test)]
mod tests {
    use super::{NOTICE_MS, NoticeState, backend_error_should_show};

    #[test]
    fn backend_error_show_dedups_and_respects_action_notice() {
        // A blank error never shows.
        assert!(!backend_error_should_show("", "", false, ""));
        // A new error over an empty footer shows.
        assert!(backend_error_should_show("boom", "", false, ""));
        // The SAME recurring error does not re-show (no restamp -> no starvation).
        assert!(!backend_error_should_show("boom", "boom", false, "boom"));
        // A live (non-expired) action notice is not clobbered by an error.
        assert!(!backend_error_should_show("boom", "spawned on codex", false, ""));
        // Once that action notice has expired, the error may take the footer.
        assert!(backend_error_should_show("boom", "spawned on codex", true, ""));
        // A CHANGED error replaces the previous backend error currently on screen.
        assert!(backend_error_should_show("boom2", "boom", false, "boom"));
    }

    #[test]
    fn notice_expires_only_after_notice_ms() {
        let mut n = NoticeState::new();
        // Empty notice never counts as expired (nothing to clear).
        assert!(!n.expired(0));
        assert!(!n.expired(NOTICE_MS + 1));

        n.set("spawned on codex".to_string(), 1_000);
        assert_eq!(n.text(), "spawned on codex");
        // Still fresh right up to the boundary, expired at/after it.
        assert!(!n.expired(1_000));
        assert!(!n.expired(1_000 + NOTICE_MS - 1));
        assert!(n.expired(1_000 + NOTICE_MS));
        assert!(n.expired(1_000 + NOTICE_MS + 5_000));

        // Re-setting restarts the clock (a stale-looking now is fresh again).
        n.set("renamed codex".to_string(), 100_000);
        assert!(!n.expired(100_000 + NOTICE_MS - 1));

        n.clear();
        assert_eq!(n.text(), "");
        assert!(!n.expired(200_000));
    }
}
