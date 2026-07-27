use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::Duration;

use agent_viewer_core::backend::{Backend, BackendKind, all_backends};
use agent_viewer_core::pty::PtySession;
use agent_viewer_core::spawn::now_ms;
use agent_viewer_core::state::{SpawnRecord, ViewerDb, apply_viewer_state, match_spawn};
use agent_viewer_core::{Session, Status, mark_dead_dirs};
use agent_viewer_tui::app::{App, Composer, DetachTracker, GroupKey, Row};
use agent_viewer_tui::logos::LogoMarks;
use agent_viewer_tui::model_cache::{ModelCache, is_stale};
use agent_viewer_tui::mutations::{MutationOutcome, MutationRunner, SpawnSelection};
use agent_viewer_tui::pr_cache::PrStatusCache;
use agent_viewer_tui::ui::{self, AttachView, ListHit, Mode, PeekCache, Pulses};

use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::execute;

mod actions;
mod keys;
mod ops;
mod pending_reply;

use pending_reply::PendingReply;

/// How often the refresh worker re-lists the backends (off the UI thread).
const REFRESH_INTERVAL: Duration = Duration::from_millis(1000);
/// How long a footer notice stays up (age-based, independent of loop phase).
const NOTICE_MS: i64 = 4000;
/// Base event-poll cadence; drops to `FAST_POLL` while the list is animating.
const POLL: Duration = Duration::from_millis(100);
const FAST_POLL: Duration = Duration::from_millis(120);
/// The spawn bloom lasts ~400ms; a pulse older than this is garbage-collected.
const PULSE_MS: i64 = 400;
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
    /// The composer's model catalog: seeded from the viewer DB, refreshed off-thread.
    models: ModelCache,
    /// Live one-shot spawn blooms, keyed by session -> start now_ms.
    pulses: Pulses,
    /// Background PR-status cache: colors the right-aligned PR badge by live GitHub state.
    pr_status: PrStatusCache,
    /// The latest successful spawn whose row has not yet become visible and selectable.
    pending_spawn: Option<SpawnSelection>,
    /// A one-shot reply injection armed by `send_reply`. While set, the run loop watches the
    /// focused PTY and writes the reply payload once it is safe (in the run, settled).
    /// Cleared on write, timeout, user takeover, or PTY prune.
    pending_reply: Option<PendingReply>,
    /// Detached-but-live PTYs, keyed by session. Reused on re-attach; dropped (killed)
    /// on quit — conversation state persists in each backend's own store.
    attached: HashMap<Key, PtySession>,
    /// The focused session while in `Mode::Attached` (input target + header snapshot).
    focused: Option<Key>,
    focused_session: Option<Session>,
    /// Whether the focused PTY's child has exited (refreshed each frame; drives the
    /// "process exited" header). Read-only during draw so the render path stays `&`.
    focused_exited: bool,
    /// The brand-logo protocols, Some when the startup graphics probe succeeded. Borrowed
    /// immutably each frame by the render path.
    logos: Option<LogoMarks>,
    /// Latest list geometry, written by `draw` each frame and read by the mouse handler to
    /// hit-test click/hover to a row. Interior mutability keeps the draw path `&`-only.
    list_hit: RefCell<ListHit>,
    /// Whether mouse reporting is currently on (Ctrl+T toggles). Off hands the mouse back to
    /// the terminal so the user can drag-select and copy; `handle_mouse` gates on this.
    mouse_capture: bool,
    mouse_press: Option<keys::MousePress>,
}

impl Ui {
    /// Set the footer notice, stamping it so the run loop can age it out after NOTICE_MS.
    fn set_notice(&mut self, msg: String) {
        self.notice.set(msg, now_ms());
    }

    /// Drop a PTY together with its per-PTY state: the detach tracker dies with it and a
    /// pending reply injection aimed at it is disarmed.
    fn remove_pty(&mut self, key: &Key) {
        self.attached.remove(key);
        self.detach_trackers.remove(key);
        if self.pending_reply.as_ref().map(|pr| &pr.key) == Some(key) {
            self.pending_reply = None;
        }
    }
}

fn main() -> io::Result<()> {
    // Marks default to textual tags; AGENT_VIEWER_GLYPH_MARKS=1 opts into the brand glyphs.
    ui::set_glyph_marks(std::env::var("AGENT_VIEWER_GLYPH_MARKS").as_deref() == Ok("1"));

    // Inline brand-logo images are always attempted. The probe queries the terminal (stdin
    // raw-mode toggle) so it runs BEFORE ratatui::init() takes the alt screen; on a non-tty or
    // unsupported terminal the build fails and the textual marks stay as a fallback.
    let logos = match LogoMarks::build() {
        Ok(l) => {
            ui::set_logo_marks(true);
            Some(l)
        }
        Err(_) => None,
    };

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
    // Build the app, then seed the collapsed set from the DB so a group the user collapsed
    // last run renders collapsed from the first paint.
    let mut app = App::new(sessions);
    if let Some(db) = &db
        && let Ok(keys) = db.collapsed_groups()
    {
        app.set_collapsed(
            keys.iter()
                .filter_map(|k| GroupKey::from_storage(k))
                .collect(),
        );
    }
    // Seed the model catalogs from the last run so the composer's picker is populated on the
    // first keystroke. A stale list is seeded too and serves until its refresh lands.
    let mut models = ModelCache::new();
    if let Some(db) = &db {
        let now = now_ms();
        for backend in [
            BackendKind::Codex,
            BackendKind::Claude,
            BackendKind::Opencode,
        ] {
            if let Ok(Some(cached)) = db.cached_models(backend) {
                models.seed(backend, cached.models, !is_stale(cached.fetched_at_ms, now));
            }
        }
    }
    // Prime whatever the seeds did not cover, so a first run (or a day-old catalog) is
    // discovering in the background while the user reads the list rather than starting a
    // multi-second probe the moment they tab the composer onto that backend. A backend with
    // a fresh seed is already marked attempted, so the steady state spawns nothing.
    for backend in [
        BackendKind::Codex,
        BackendKind::Claude,
        BackendKind::Opencode,
    ] {
        models.request(backend);
    }

    let mut ui = Ui {
        app,
        mode: Mode::Normal,
        notice: startup_notice,
        db,
        peek: PeekCache::new(),
        composer: Composer::new(),
        detach_trackers: HashMap::new(),
        last_backend_error: String::new(),
        mutations: MutationRunner::new(),
        models,
        pulses: Pulses::new(),
        pr_status: PrStatusCache::new(),
        pending_spawn: None,
        pending_reply: None,
        attached: HashMap::new(),
        focused: None,
        focused_session: None,
        focused_exited: false,
        logos,
        list_hit: RefCell::new(ListHit::default()),
        mouse_capture: true,
        mouse_press: None,
    };

    // Hand the listing backends to the refresh worker; the UI keeps a separate set for the
    // non-list calls: attach_command and capabilities. Only the worker set ever calls the slow
    // list(). These are no longer all cheap stateless builders: `attach_command` shells out to
    // `codex app-server daemon version` for a daemon-hosted row (about 34ms, and only for
    // those rows). Spawn used to be here too and is now a `Mutation::Spawn` on the runner,
    // because a codex spawn dials the daemon and may start one.
    let refresher = spawn_refresh_worker(list_backends);
    let action_backends = all_backends();

    let mut terminal = ratatui::init();
    // Mouse capture powers click/hover row selection on the list, and while attached it lets
    // us forward real mouse reports to the child so the wheel scrolls the transcript instead
    // of the terminal's alternate-scroll turning it into arrow keys codex reads as history
    // navigation. It also swallows drag-select, and the Shift-to-override convention is not
    // universal, so Ctrl+T toggles it off on demand (see `keys::set_mouse_capture`). Starts on
    // to match `ui.mouse_capture`. Best-effort: a terminal that rejects the sequence leaves
    // the keyboard nav.
    let _ = execute!(io::stdout(), EnableMouseCapture);
    let result = run(&mut terminal, &action_backends, &refresher, &mut ui);
    let _ = execute!(io::stdout(), DisableMouseCapture);
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
        if ui
            .pending_spawn
            .as_ref()
            .is_some_and(|spawn| now - spawn.spawned_at_ms > SPAWN_ABANDON_MS)
        {
            ui.pending_spawn = None;
        }
        ui.peek.refresh(ui.app.selected());

        // Drain completed PR-status fetches, then request statuses for the visible rows.
        // app and pr_status are disjoint fields; destructuring borrows them separately so
        // the request pass needs no per-frame clone of the rows' pr_refs.
        ui.pr_status.poll(now);
        {
            let Ui { app, pr_status, .. } = &mut *ui;
            for row in app.visible() {
                if let Row::Session { pr_refs, .. } = row {
                    pr_status.request_refs(pr_refs, now);
                }
            }
        }

        // Drain completed background mutations: show the result and hasten a fresh listing.
        let mut mutation_completed = false;
        while let Some(result) = ui.mutations.poll() {
            apply_mutation_result(ui, result);
            mutation_completed = true;
        }
        if mutation_completed {
            refresher.force();
        }

        // Fold in any model catalog that finished discovering (persisted for the next run).
        actions::install_models(ui);

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

        // Drive any armed one-shot reply injection once we are safely in the attached run.
        pending_reply::drive_pending_reply(ui);

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
                    expanded: ui.app.expanded(),
                    now_ms: now,
                    attach,
                    pr_status: &ui.pr_status,
                    logos: ui.logos.as_ref(),
                    list_hit: &ui.list_hit,
                },
            );
        })?;

        // Animate the list faster while there are working/needs-input rows or a live bloom.
        let poll = if wants_fast_ticks(ui) {
            FAST_POLL
        } else {
            POLL
        };
        if event::poll(poll)? {
            match event::read()? {
                Event::Key(key)
                    if key.kind == KeyEventKind::Press
                        && keys::handle_key(key, backends, refresher, ui, terminal)? =>
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
                Event::Mouse(me) => {
                    keys::handle_mouse_event(me, backends, ui, terminal)?;
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
                status: Status::Working | Status::NeedsInput { .. },
                ..
            }
        )
    })
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
        let live: HashSet<Key> = sessions.iter().map(|s| (s.backend, s.id.clone())).collect();
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
    let spawned_key = ui
        .pending_spawn
        .as_ref()
        .and_then(|spawn| match_pending_spawn(spawn, &sessions));
    ui.app.set_sessions(sessions);
    // Keep the inline rename edit row pinned under the cursor across a reorder so it does not
    // visually jump away mid-edit (the rename still targets by id regardless).
    if let Mode::Rename(modal) = &ui.mode {
        ui.app.select_by_key(&(modal.backend, modal.id.clone()));
    } else if let Some(key) = spawned_key
        && ui.app.select_by_key(&key)
    {
        ui.pulses.entry(key).or_insert_with(now_ms);
        ui.pending_spawn = None;
    }
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

fn apply_mutation_result(ui: &mut Ui, result: Result<MutationOutcome, String>) {
    match result {
        Ok(outcome) => {
            if let Some(spawned) = outcome.spawned {
                ui.pending_spawn = Some(spawned);
            }
            ui.set_notice(outcome.notice);
        }
        Err(msg) => ui.set_notice(msg),
    }
}

/// Resolve a successful spawn against a fresh backend listing. An exact backend identity
/// always wins. Backends without one reuse the viewer database's cwd and creation time rule.
fn match_pending_spawn(spawn: &SpawnSelection, sessions: &[Session]) -> Option<Key> {
    if let Some(session_id) = &spawn.session_id {
        return sessions
            .iter()
            .find(|session| {
                session.backend == spawn.backend && session.id.as_str() == session_id.as_str()
            })
            .map(|session| (session.backend, session.id.clone()));
    }
    let record = SpawnRecord {
        rowid: 0,
        backend: spawn.backend,
        cwd: spawn.cwd.clone(),
        pid: 0,
        spawned_at_ms: spawn.spawned_at_ms,
    };
    let candidates = sessions
        .iter()
        .filter(|session| {
            session.backend != spawn.backend || !spawn.preexisting_ids.contains(&session.id)
        })
        .cloned()
        .collect::<Vec<_>>();
    match_spawn(&record, &candidates).map(|id| (spawn.backend, id))
}

/// Whether a snapshot's backend-error `err` should replace the current footer notice, given
/// the current notice text, whether it has expired, and the last error already shown. A blank
/// or unchanged error never shows (so a recurring error neither restamps nor starves an action
/// notice); a changed error shows only when the footer is free (empty/expired) or is itself
/// the previous backend error.
fn backend_error_should_show(
    err: &str,
    current: &str,
    current_expired: bool,
    last_error: &str,
) -> bool {
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
            ui.remove_pty(&key);
        }
    }
}

/// Apply the viewer DB overlay and resolve or expire spawn records. Returns the backend and
/// id of any spawn record that resolved to a session this pass, so the caller can start its
/// one-shot bloom.
fn overlay(db: &ViewerDb, sessions: &mut [Session]) -> Vec<Key> {
    if let Ok(state) = db.viewer_state() {
        apply_viewer_state(sessions, &state);
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
    use super::*;
    use agent_viewer_core::SessionOrigin;
    use agent_viewer_tui::mutations::{MutationOutcome, SpawnSelection};
    use std::path::PathBuf;

    const CWD: &str = "/tmp";

    fn session(backend: BackendKind, id: &str, created_at_ms: i64, hidden: bool) -> Session {
        Session {
            backend,
            id: id.to_string(),
            short_id: None,
            origin: SessionOrigin::Background,
            title: id.to_string(),
            cwd: PathBuf::from(CWD),
            git_branch: None,
            status: Status::Working,
            created_at_ms,
            updated_at_ms: created_at_ms,
            hidden,
            companion: false,
            summary: String::new(),
            pid: None,
            rollout_path: None,
            pr_refs: Vec::new(),
            daemon_hosted: backend == BackendKind::Codex,
        }
    }

    fn pending(
        backend: BackendKind,
        session_id: Option<&str>,
        spawned_at_ms: i64,
    ) -> SpawnSelection {
        SpawnSelection {
            backend,
            session_id: session_id.map(str::to_string),
            cwd: PathBuf::from(CWD),
            spawned_at_ms,
            preexisting_ids: HashSet::new(),
        }
    }

    fn pending_with_preexisting(
        backend: BackendKind,
        session_id: Option<&str>,
        spawned_at_ms: i64,
        preexisting_ids: &[&str],
    ) -> SpawnSelection {
        SpawnSelection {
            backend,
            session_id: session_id.map(str::to_string),
            cwd: PathBuf::from(CWD),
            spawned_at_ms,
            preexisting_ids: preexisting_ids.iter().map(|id| (*id).to_string()).collect(),
        }
    }

    fn test_ui(sessions: Vec<Session>) -> Ui {
        Ui {
            app: App::new(sessions),
            mode: Mode::Normal,
            notice: NoticeState::new(),
            db: None,
            peek: PeekCache::new(),
            composer: Composer::new(),
            detach_trackers: HashMap::new(),
            last_backend_error: String::new(),
            mutations: MutationRunner::new(),
            models: ModelCache::new(),
            pulses: Pulses::new(),
            pr_status: PrStatusCache::new(),
            pending_spawn: None,
            pending_reply: None,
            attached: HashMap::new(),
            focused: None,
            focused_session: None,
            focused_exited: false,
            logos: None,
            list_hit: RefCell::new(ListHit::default()),
            mouse_capture: true,
        }
    }

    fn apply_listing(ui: &mut Ui, sessions: Vec<Session>) {
        let (snapshot_tx, snapshots) = channel();
        let (wake, _wake_rx) = channel::<()>();
        snapshot_tx
            .send((sessions, String::new(), 1))
            .expect("queue snapshot");
        apply_snapshot(&Refresher { snapshots, wake }, ui);
    }

    fn selected_id(ui: &Ui) -> Option<&str> {
        ui.app.selected().map(|session| session.id.as_str())
    }

    #[test]
    fn exact_spawn_identity_beats_a_closer_same_cwd_session() {
        let old = session(BackendKind::Codex, "old", 1_000, false);
        let mut ui = test_ui(vec![old.clone()]);
        assert!(ui.app.select_by_key(&(BackendKind::Codex, old.id.clone())));
        ui.pending_spawn = Some(pending(BackendKind::Codex, Some("exact"), 10_000));

        let decoy = session(BackendKind::Codex, "decoy", 10_001, false);
        let exact = session(BackendKind::Codex, "exact", 39_000, false);
        apply_listing(&mut ui, vec![decoy, old, exact]);

        assert_eq!(selected_id(&ui), Some("exact"));
        assert!(ui.pending_spawn.is_none());
        assert!(
            ui.pulses
                .contains_key(&(BackendKind::Codex, "exact".to_string()))
        );
    }

    #[test]
    fn successful_spawn_outcome_drives_selection_through_the_main_loop_bridge() {
        let old = session(BackendKind::Codex, "old", 1_000, false);
        let new = session(BackendKind::Codex, "new", 10_100, false);
        let mut ui = test_ui(vec![old.clone()]);
        assert!(ui.app.select_by_key(&(BackendKind::Codex, old.id.clone())));

        apply_mutation_result(
            &mut ui,
            Ok(MutationOutcome {
                notice: "spawned on codex".to_string(),
                spawned: Some(pending(BackendKind::Codex, Some("new"), 10_000)),
            }),
        );
        apply_listing(&mut ui, vec![old, new]);

        assert_eq!(selected_id(&ui), Some("new"));
        assert!(ui.pending_spawn.is_none());
        assert_eq!(ui.notice.text(), "spawned on codex");
    }

    #[test]
    fn spawn_without_identity_uses_nearest_same_cwd_time_match() {
        let old = session(BackendKind::Opencode, "old", 1_000, false);
        let mut ui = test_ui(vec![old.clone()]);
        assert!(
            ui.app
                .select_by_key(&(BackendKind::Opencode, old.id.clone()))
        );
        ui.pending_spawn = Some(pending(BackendKind::Opencode, None, 10_000));

        let target = session(BackendKind::Opencode, "target", 10_150, false);
        let farther = session(BackendKind::Opencode, "farther", 11_000, false);
        let wrong_backend = session(BackendKind::Claude, "wrong", 10_001, false);
        apply_listing(&mut ui, vec![farther, wrong_backend, old, target]);

        assert_eq!(selected_id(&ui), Some("target"));
        assert!(ui.pending_spawn.is_none());
    }

    #[test]
    fn spawn_without_identity_waits_for_a_row_absent_before_submission() {
        let selected = session(BackendKind::Opencode, "selected", 1_000, false);
        let preexisting = session(BackendKind::Opencode, "preexisting", 9_999, false);
        let mut ui = test_ui(vec![selected.clone(), preexisting.clone()]);
        assert!(
            ui.app
                .select_by_key(&(BackendKind::Opencode, selected.id.clone()))
        );
        let pending = pending_with_preexisting(
            BackendKind::Opencode,
            None,
            10_000,
            &["selected", "preexisting"],
        );
        ui.pending_spawn = Some(pending.clone());

        apply_listing(&mut ui, vec![preexisting.clone(), selected.clone()]);

        assert_eq!(selected_id(&ui), Some("selected"));
        assert_eq!(ui.pending_spawn, Some(pending));

        let spawned = session(BackendKind::Opencode, "spawned", 10_150, false);
        apply_listing(&mut ui, vec![preexisting, spawned, selected]);

        assert_eq!(selected_id(&ui), Some("spawned"));
        assert!(ui.pending_spawn.is_none());
    }

    #[test]
    fn snapshot_without_spawned_row_preserves_selection_and_pending_target() {
        let old = session(BackendKind::Codex, "old", 1_000, false);
        let mut ui = test_ui(vec![old.clone()]);
        assert!(ui.app.select_by_key(&(BackendKind::Codex, old.id.clone())));
        ui.pending_spawn = Some(pending(BackendKind::Codex, Some("new"), 10_000));

        apply_listing(
            &mut ui,
            vec![old, session(BackendKind::Codex, "other", 10_001, false)],
        );

        assert_eq!(selected_id(&ui), Some("old"));
        assert_eq!(
            ui.pending_spawn,
            Some(pending(BackendKind::Codex, Some("new"), 10_000))
        );
        assert!(
            !ui.pulses
                .contains_key(&(BackendKind::Codex, "new".to_string()))
        );
    }

    #[test]
    fn first_snapshot_containing_spawned_row_selects_and_consumes_it() {
        let old = session(BackendKind::Codex, "old", 1_000, false);
        let new = session(BackendKind::Codex, "new", 10_100, false);
        let mut ui = test_ui(vec![old.clone()]);
        assert!(ui.app.select_by_key(&(BackendKind::Codex, old.id.clone())));
        ui.pending_spawn = Some(pending(BackendKind::Codex, Some("new"), 10_000));

        apply_listing(&mut ui, vec![old, new]);

        assert_eq!(selected_id(&ui), Some("new"));
        assert!(ui.pending_spawn.is_none());
        assert!(
            ui.pulses
                .contains_key(&(BackendKind::Codex, "new".to_string()))
        );
    }

    #[test]
    fn following_reordered_snapshot_stays_anchored_to_spawned_row() {
        let old = session(BackendKind::Codex, "old", 1_000, false);
        let new = session(BackendKind::Codex, "new", 10_100, false);
        let mut ui = test_ui(vec![old.clone()]);
        assert!(ui.app.select_by_key(&(BackendKind::Codex, old.id.clone())));
        ui.pending_spawn = Some(pending(BackendKind::Codex, Some("new"), 10_000));

        apply_listing(&mut ui, vec![old.clone(), new.clone()]);
        assert_eq!(selected_id(&ui), Some("new"));
        let first_pulse = ui
            .pulses
            .get(&(BackendKind::Codex, "new".to_string()))
            .copied();

        apply_listing(
            &mut ui,
            vec![
                session(BackendKind::Codex, "newest", 20_000, false),
                new,
                old,
            ],
        );

        assert_eq!(selected_id(&ui), Some("new"));
        assert!(ui.pending_spawn.is_none());
        assert_eq!(
            ui.pulses
                .get(&(BackendKind::Codex, "new".to_string()))
                .copied(),
            first_pulse
        );
    }

    #[test]
    fn invisible_spawned_row_does_not_consume_pending_selection() {
        let old = session(BackendKind::Codex, "old", 1_000, false);
        let hidden = session(BackendKind::Codex, "new", 10_100, true);
        let mut ui = test_ui(vec![old.clone()]);
        assert!(ui.app.select_by_key(&(BackendKind::Codex, old.id.clone())));
        ui.pending_spawn = Some(pending(BackendKind::Codex, Some("new"), 10_000));

        apply_listing(&mut ui, vec![old, hidden]);

        assert_eq!(selected_id(&ui), Some("old"));
        assert_eq!(
            ui.pending_spawn,
            Some(pending(BackendKind::Codex, Some("new"), 10_000))
        );
        assert!(
            !ui.pulses
                .contains_key(&(BackendKind::Codex, "new".to_string()))
        );
    }

    #[test]
    fn backend_error_show_dedups_and_respects_action_notice() {
        // A blank error never shows.
        assert!(!backend_error_should_show("", "", false, ""));
        // A new error over an empty footer shows.
        assert!(backend_error_should_show("boom", "", false, ""));
        // The SAME recurring error does not re-show (no restamp -> no starvation).
        assert!(!backend_error_should_show("boom", "boom", false, "boom"));
        // A live (non-expired) action notice is not clobbered by an error.
        assert!(!backend_error_should_show(
            "boom",
            "spawned on codex",
            false,
            ""
        ));
        // Once that action notice has expired, the error may take the footer.
        assert!(backend_error_should_show(
            "boom",
            "spawned on codex",
            true,
            ""
        ));
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
