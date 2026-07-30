use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use agent_viewer_core::backend::{Backend, BackendKind, all_backends_with_opencode};
use agent_viewer_core::opencode::OpencodeRuntime;
use agent_viewer_core::pty::{PtySession, TerminalPalette};
use agent_viewer_core::spawn::now_ms;
use agent_viewer_core::state::{SpawnRecord, ViewerDb, apply_viewer_state, match_spawn};
use agent_viewer_core::{Session, Status, mark_dead_dirs};
use agent_viewer_tui::app::{App, Composer, DetachTracker, GroupKey, Row};
use agent_viewer_tui::logos::LogoMarks;
use agent_viewer_tui::model_cache::{ModelCache, is_stale};
use agent_viewer_tui::mutations::{AttachRunner, MutationOutcome, MutationRunner, SpawnSelection};
use agent_viewer_tui::pr_cache::PrStatusCache;
use agent_viewer_tui::shared_listing::{
    RefreshCursor, RefreshOutcome, TargetRequest, refresh_backend,
};
use agent_viewer_tui::terminal_title::set_terminal_title;
use agent_viewer_tui::ui::{self, AttachView, ListHit, Mode, Pulses};
use agent_viewer_tui::{StartupAction, startup_action};

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyEventKind,
};
use crossterm::execute;
use termina::escape::osc::{ColorOrQuery, DynamicColorNumber, Osc};
use termina::{Event as TerminaEvent, PlatformTerminal, Terminal as _};

mod actions;
mod keys;
mod ops;
mod pending_reply;

use pending_reply::PendingReply;

/// How often the refresh worker re-lists the backends (off the UI thread).
const REFRESH_INTERVAL: Duration = Duration::from_millis(2000);
/// How long a footer notice stays up (age-based, independent of loop phase).
const NOTICE_MS: i64 = 4000;
const STOP_FAILURE_MARKER: &str = "\0stop failure\0";
/// Base event-poll cadence; drops to `FAST_POLL` while the list is animating.
const POLL: Duration = Duration::from_millis(100);
const FAST_POLL: Duration = Duration::from_millis(120);
/// The spawn bloom lasts ~400ms; a pulse older than this is garbage-collected.
const PULSE_MS: i64 = 400;
/// Abandoned spawn records (no matching session after this long) are deleted.
const SPAWN_ABANDON_MS: i64 = 600_000;
const PALETTE_QUERY_TIMEOUT: Duration = Duration::from_millis(100);
const ACTIVITY_WINDOW: Duration = Duration::from_secs(60 * 60);
const ACTIVITY_REFRESH_MS: i64 = 30_000;
const ACTIVITY_LOOKAHEAD: usize = 8;

type Key = (BackendKind, String);
/// A backend-listing snapshot handed from the refresh worker to the UI thread.
type Snapshot = (Vec<Session>, String, usize);
type ActivityResult = (BackendKind, String, Option<String>);

struct ActivityRequest {
    sessions: Vec<Session>,
    now_ms: i64,
}

struct ActivityEntry {
    transcript_version: i64,
    fetched_at_ms: i64,
    timestamps: Vec<i64>,
}

struct ActivityWorker {
    requests: Sender<ActivityRequest>,
    results: Receiver<ActivityResult>,
}

impl ActivityWorker {
    fn new(backends: Vec<Box<dyn Backend>>) -> ActivityWorker {
        let (request_tx, request_rx) = channel::<ActivityRequest>();
        let (result_tx, result_rx) = channel::<ActivityResult>();
        thread::spawn(move || {
            let mut cache = HashMap::new();
            while let Ok(request) = request_rx.recv() {
                for result in
                    activity_results(&backends, &mut cache, request.sessions, request.now_ms)
                {
                    if result_tx.send(result).is_err() {
                        return;
                    }
                }
            }
        });
        ActivityWorker {
            requests: request_tx,
            results: result_rx,
        }
    }

    fn request(&self, sessions: Vec<Session>, now_ms: i64) {
        let _ = self.requests.send(ActivityRequest { sessions, now_ms });
    }

    fn poll(&self) -> Option<ActivityResult> {
        self.results.try_recv().ok()
    }
}

fn activity_results(
    backends: &[Box<dyn Backend>],
    cache: &mut HashMap<Key, ActivityEntry>,
    sessions: Vec<Session>,
    now_ms: i64,
) -> Vec<ActivityResult> {
    let mut results = Vec::with_capacity(sessions.len());
    let mut seen = HashSet::new();
    for session in sessions {
        let key = (session.backend, session.id.clone());
        if !seen.insert(key.clone()) {
            continue;
        }
        let should_read = match cache.get(&key) {
            None => true,
            Some(entry) => {
                entry.transcript_version != session.updated_at_ms
                    || now_ms - entry.fetched_at_ms >= ACTIVITY_REFRESH_MS
            }
        };
        if should_read {
            let timestamps = backends
                .iter()
                .find(|backend| backend.kind() == session.backend)
                .and_then(|backend| backend.turn_activity(&session, ACTIVITY_WINDOW).ok())
                .unwrap_or_default();
            cache.insert(
                key.clone(),
                ActivityEntry {
                    transcript_version: session.updated_at_ms,
                    fetched_at_ms: now_ms,
                    timestamps,
                },
            );
        }
        let ribbon = cache.get(&key).and_then(|entry| {
            let start_ms = now_ms.saturating_sub(ACTIVITY_WINDOW.as_millis() as i64);
            entry
                .timestamps
                .iter()
                .any(|timestamp| *timestamp >= start_ms && *timestamp <= now_ms)
                .then(|| {
                    ui::activity_ribbon(
                        &entry.timestamps,
                        now_ms,
                        ACTIVITY_WINDOW.as_millis() as i64,
                    )
                })
        });
        results.push((session.backend, session.id, ribbon));
    }
    results
}

struct BracketedPasteGuard<W: io::Write> {
    writer: W,
}

impl<W: io::Write> BracketedPasteGuard<W> {
    fn new(writer: W) -> Self {
        Self { writer }
    }
}

impl<W: io::Write> Drop for BracketedPasteGuard<W> {
    fn drop(&mut self) {
        let _ = execute!(&mut self.writer, DisableBracketedPaste);
    }
}

/// Apply a changed mouse capture request once. UI state changes never write terminal control
/// bytes themselves, so the live loop calls this after events and completed attach plans.
fn sync_mouse_capture<W: io::Write>(
    writer: &mut W,
    applied: &mut bool,
    requested: bool,
) -> io::Result<()> {
    if *applied != requested {
        keys::write_mouse_capture(writer, requested)?;
        *applied = requested;
    }
    Ok(())
}

/// Install a completed attach plan and immediately apply its mouse capture request.
fn install_completed_attach_plan<B: ratatui::backend::Backend, W: io::Write>(
    ui: &mut Ui,
    terminal: &mut ratatui::Terminal<B>,
    plan: ops::AttachPlan,
    writer: &mut W,
    applied_mouse_capture: &mut bool,
) -> io::Result<bool> {
    let installed = actions::install_attach_plan(ui, terminal, plan)?;
    sync_mouse_capture(writer, applied_mouse_capture, ui.mouse_capture)?;
    Ok(installed)
}

/// Route one terminal event, then synchronize mouse reporting with the input handler's
/// requested UI state. Keeping the writer injectable makes the live bridge testable without
/// writing control sequences to the invoking terminal.
fn process_event<B: ratatui::backend::Backend, W: io::Write>(
    event: Event,
    backends: &[Box<dyn Backend>],
    refresher: &Refresher,
    ui: &mut Ui,
    terminal: &mut ratatui::Terminal<B>,
    writer: &mut W,
    applied_mouse_capture: &mut bool,
) -> io::Result<bool> {
    let quit = match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            keys::handle_key(key, backends, refresher, ui, terminal)?
        }
        Event::Resize(_, _) => {
            if let Some(key) = &ui.focused
                && let Some(pty) = ui.attached.get_mut(key)
            {
                let size = terminal
                    .size()
                    .map_err(|error| io::Error::other(error.to_string()))?;
                let _ = pty.resize(size.height.saturating_sub(1).max(1), size.width.max(1));
            }
            false
        }
        Event::Mouse(me) => {
            keys::handle_mouse_event(me, backends, ui, terminal)?;
            false
        }
        Event::Paste(text) => {
            keys::handle_paste(&text, ui);
            false
        }
        _ => false,
    };
    if quit {
        return Ok(true);
    }
    let _ = sync_mouse_capture(writer, applied_mouse_capture, ui.mouse_capture);
    Ok(false)
}

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
fn spawn_refresh_worker(
    mut backends: Vec<Box<dyn Backend>>,
    mut last: Vec<Vec<Session>>,
    mut cursors: Vec<RefreshCursor>,
) -> Refresher {
    let (snap_tx, snap_rx) = channel::<Snapshot>();
    let (wake_tx, wake_rx) = channel::<()>();
    let db = ViewerDb::open_default().ok();
    thread::spawn(move || {
        loop {
            let snapshot = refresh(&mut backends, &mut last, &mut cursors, db.as_ref());
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
    workspace: std::path::PathBuf,
    mode: Mode,
    notice: NoticeState,
    db: Option<ViewerDb>,
    /// Inline spawn composer (persistent on the list view).
    composer: Composer,
    themes: ui::ThemeState,
    /// Per-PTY left-arrow detach gate, keyed like `attached`. Reset only when a new PTY is
    /// spawned; a re-attach reuses the previous pending count (the child's input line may
    /// still hold text). Pruned alongside its PTY.
    detach_trackers: HashMap<Key, DetachTracker>,
    /// The last backend-error string surfaced as a notice (dedup memo, so a recurring error
    /// neither restamps nor starves action notices).
    last_backend_error: String,
    /// Blocking backend mutations run off the render thread.
    mutations: MutationRunner,
    /// Backend mutation boundary used by every submission on the runner thread.
    mutation_executor: Arc<dyn Fn(ops::Mutation) -> Result<MutationOutcome, String> + Send + Sync>,
    /// Authoritative attach resolution runs off the render thread.
    attaches: AttachRunner<ops::AttachPlan>,
    /// Fresh backend attach boundary used by every attach worker.
    attach_executor: Arc<dyn Fn(TargetRequest) -> Result<ops::AttachPlan, String> + Send + Sync>,
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
    /// The host terminal colors captured once before alternate screen entry.
    terminal_palette: Option<TerminalPalette>,
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
    /// The header mascot on screen. Ctrl+G cycles it so the candidate sprites can be compared
    /// live in one build.
    sprite: ui::SpriteKind,
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

/// The header mascot to open on: `AV_SPRITE` wins for a one-off look, then the persisted
/// choice, then the default. An unknown name in either place falls back rather than erroring.
fn startup_sprite(db: Option<&ViewerDb>) -> ui::SpriteKind {
    let persisted = db.and_then(|db| db.header_sprite().ok().flatten());
    ui::SpriteKind::from_name(std::env::var("AV_SPRITE").ok().as_deref())
        .or_else(|| ui::SpriteKind::from_name(persisted.as_deref()))
        .unwrap_or_default()
}

fn is_palette_response(event: &TerminaEvent) -> bool {
    let TerminaEvent::Osc(Osc::ChangeDynamicColors(slot, colors)) = event else {
        return false;
    };
    matches!(
        slot,
        DynamicColorNumber::TextForegroundColor | DynamicColorNumber::TextBackgroundColor
    ) && matches!(colors.as_slice(), [ColorOrQuery::Color(_)])
}

fn capture_terminal_palette() -> Option<TerminalPalette> {
    let mut terminal = PlatformTerminal::new().ok()?;
    if terminal.enter_raw_mode().is_err() {
        let _ = terminal.enter_cooked_mode();
        return None;
    }
    let captured = (|| -> io::Result<Option<TerminalPalette>> {
        write!(
            terminal,
            "{}{}",
            Osc::ChangeDynamicColors(
                DynamicColorNumber::TextForegroundColor,
                vec![ColorOrQuery::Query],
            ),
            Osc::ChangeDynamicColors(
                DynamicColorNumber::TextBackgroundColor,
                vec![ColorOrQuery::Query],
            ),
        )?;
        terminal.flush()?;

        let deadline = Instant::now() + PALETTE_QUERY_TIMEOUT;
        let mut foreground = None;
        let mut background = None;
        while foreground.is_none() || background.is_none() {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            if remaining.is_zero() || !terminal.poll(is_palette_response, Some(remaining))? {
                break;
            }
            let TerminaEvent::Osc(Osc::ChangeDynamicColors(slot, colors)) =
                terminal.read(is_palette_response)?
            else {
                continue;
            };
            let [ColorOrQuery::Color(color)] = colors.as_slice() else {
                continue;
            };
            let rgb = [color.red, color.green, color.blue];
            match slot {
                DynamicColorNumber::TextForegroundColor => foreground = Some(rgb),
                DynamicColorNumber::TextBackgroundColor => background = Some(rgb),
                _ => {}
            }
        }

        Ok(match (foreground, background) {
            (Some(foreground), Some(background)) => Some(TerminalPalette {
                foreground,
                background,
            }),
            _ => None,
        })
    })();
    let _ = terminal.enter_cooked_mode();
    captured.ok().flatten()
}

fn main() -> io::Result<()> {
    if startup_action(std::env::args_os().skip(1)) == StartupAction::PrintVersion {
        writeln!(io::stdout(), "agent-viewer {}", env!("CARGO_PKG_VERSION"))?;
        return Ok(());
    }

    // Marks default to textual tags; AGENT_VIEWER_GLYPH_MARKS=1 opts into the brand glyphs.
    let glyph_marks = std::env::var("AGENT_VIEWER_GLYPH_MARKS").as_deref() == Ok("1");
    ui::set_glyph_marks(glyph_marks);

    let workspace = std::env::current_dir().unwrap_or_default();
    let terminal_palette = capture_terminal_palette();

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

    let opencode_runtime = OpencodeRuntime::new();
    let mut list_backends = all_backends_with_opencode(opencode_runtime.clone());
    let db = ViewerDb::open_default().ok();
    let persisted_theme = db.as_ref().and_then(ui::theme::persisted_theme);
    let startup_sprite = startup_sprite(db.as_ref());
    let (themes, theme_notices) = ui::ThemeState::load(
        ui::glyph_marks(),
        persisted_theme.as_deref(),
        &ui::theme::theme_directory(),
    );

    // Startup refresh BEFORE entering the alt screen so the first paint is not empty. If
    // every backend fails to list, print the errors to stderr and exit without a UI.
    let mut last: Vec<Vec<Session>> = vec![Vec::new(); list_backends.len()];
    let mut cursors = vec![RefreshCursor::default(); list_backends.len()];
    let (mut sessions, notice, ok_count) =
        refresh(&mut list_backends, &mut last, &mut cursors, db.as_ref());
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
    if !theme_notices.is_empty() {
        startup_notice.set(theme_notices.join(" · "), now_ms());
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

    let mutation_runtime = opencode_runtime.clone();
    let attach_runtime = opencode_runtime.clone();
    let mut ui = Ui {
        app,
        workspace,
        mode: Mode::Normal,
        notice: startup_notice,
        db,
        composer: Composer::new(),
        themes,
        detach_trackers: HashMap::new(),
        last_backend_error: String::new(),
        mutations: MutationRunner::new(),
        mutation_executor: mutation_executor(move |mutation| {
            ops::run_mutation_with_opencode(mutation, mutation_runtime.clone())
        }),
        attaches: AttachRunner::new(),
        attach_executor: Arc::new(move |request| {
            ops::resolve_attach_with_opencode(request, attach_runtime.clone())
        }),
        models,
        pulses: Pulses::new(),
        pr_status: PrStatusCache::new(),
        pending_spawn: None,
        pending_reply: None,
        attached: HashMap::new(),
        terminal_palette,
        focused: None,
        focused_session: None,
        focused_exited: false,
        logos,
        list_hit: RefCell::new(ListHit::default()),
        mouse_capture: true,
        mouse_press: None,
        sprite: startup_sprite,
    };

    // Hand listing backends to the refresh worker. The UI set remains only for cheap capability
    // routing. Attach resolution builds its own fresh backend on the attach worker, and spawn is
    // a mutation because either operation can dial the backend runtime.
    let activity_backends = all_backends_with_opencode(opencode_runtime.clone());
    let activity = ActivityWorker::new(activity_backends);
    let refresher = spawn_refresh_worker(list_backends, last, cursors);
    let action_backends = all_backends_with_opencode(opencode_runtime);

    let mut terminal = ratatui::init();
    set_terminal_title(&mut io::stdout(), &ui.workspace);
    // Mouse capture powers list selection and attached transcript scrolling for Codex and
    // Claude. OpenCode attach turns it off for host terminal selection. Ctrl+T remains the
    // manual override. Starts on to match `ui.mouse_capture`.
    let _ = execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    let mut applied_mouse_capture = true;
    let result = {
        let _bracketed_paste = BracketedPasteGuard::new(io::stdout());
        run(
            &mut terminal,
            &action_backends,
            &refresher,
            &activity,
            &mut ui,
            &mut applied_mouse_capture,
        )
    };
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    backends: &[Box<dyn Backend>],
    refresher: &Refresher,
    activity: &ActivityWorker,
    ui: &mut Ui,
    applied_mouse_capture: &mut bool,
) -> io::Result<()> {
    let mut last_activity_request_ms = None;
    loop {
        let now = now_ms();
        while let Some((backend, id, ribbon)) = activity.poll() {
            ui.app.set_activity_ribbon(backend, &id, ribbon);
        }
        if ui
            .pending_spawn
            .as_ref()
            .is_some_and(|spawn| now - spawn.spawned_at_ms > SPAWN_ABANDON_MS)
        {
            ui.pending_spawn = None;
        }
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

        while let Some(result) = ui.attaches.poll() {
            match result {
                Ok(plan) => {
                    install_completed_attach_plan(
                        ui,
                        terminal,
                        plan,
                        &mut io::stdout(),
                        applied_mouse_capture,
                    )?;
                }
                Err(notice) => ui.set_notice(notice),
            }
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
                    workspace: &ui.workspace,
                    mode: &ui.mode,
                    notice: ui.notice.text(),
                    composer: &ui.composer,
                    pulses: &ui.pulses,
                    now_ms: now,
                    attach,
                    pr_status: &ui.pr_status,
                    logos: ui.logos.as_ref(),
                    list_hit: &ui.list_hit,
                    themes: &ui.themes,
                    sprite: ui.sprite,
                },
            );
        })?;

        let activity_due = last_activity_request_ms
            .is_none_or(|last| now.saturating_sub(last) >= ACTIVITY_REFRESH_MS);
        if activity_due
            && !matches!(ui.mode, Mode::Attached)
            && let Some(range) = ui
                .list_hit
                .borrow()
                .rendered_range(ui.app.visible().len(), ACTIVITY_LOOKAHEAD)
        {
            let sessions = ui.app.visible()[range]
                .iter()
                .filter_map(|row| match row {
                    Row::Session { backend, id, .. } => {
                        ui.app.session_for(&(*backend, id.clone())).cloned()
                    }
                    _ => None,
                })
                .collect();
            activity.request(sessions, now);
            last_activity_request_ms = Some(now);
        }

        // Animate the list faster while there are working/needs-input rows or a live bloom.
        let poll = if wants_fast_ticks(ui) {
            FAST_POLL
        } else {
            POLL
        };
        if event::poll(poll)?
            && process_event(
                event::read()?,
                backends,
                refresher,
                ui,
                terminal,
                &mut io::stdout(),
                applied_mouse_capture,
            )?
        {
            return Ok(());
        }
    }
}

/// The list animates (needs a faster poll) while it shows a working/needs-input row or a
/// live spawn bloom. The attach view owns the screen, so it never fast-ticks.
fn wants_fast_ticks(ui: &Ui) -> bool {
    if matches!(ui.mode, Mode::Attached) {
        return false;
    }
    if !ui.themes.active().animation {
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
    if let Some(notice) = ui.themes.reload_active() {
        ui.set_notice(notice);
    }
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
    // dirs stay visible while deleted-dir noise defaults to hidden; `Ctrl+A` still reveals it).
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
        Err(msg) => {
            if msg.starts_with(STOP_FAILURE_MARKER) {
                if let Some((backend, id, notice)) = decode_stop_failure(&msg) {
                    ui.app.disarm_kill_for(backend, id);
                    ui.set_notice(notice.to_string());
                } else {
                    ui.set_notice("stop failed".to_string());
                }
            } else {
                ui.set_notice(msg);
            }
        }
    }
}

fn mutation_executor<F>(
    executor: F,
) -> Arc<dyn Fn(ops::Mutation) -> Result<MutationOutcome, String> + Send + Sync>
where
    F: Fn(ops::Mutation) -> Result<MutationOutcome, String> + Send + Sync + 'static,
{
    Arc::new(move |mutation| {
        let stop_target = match &mutation {
            ops::Mutation::Stop(request) => Some((request.backend(), request.id().to_string())),
            _ => None,
        };
        match stop_target {
            Some((backend, id)) => executor(mutation)
                .map_err(|msg| format!("{STOP_FAILURE_MARKER}{}\0{id}\0{msg}", backend.name())),
            None => executor(mutation),
        }
    })
}

fn decode_stop_failure(msg: &str) -> Option<(BackendKind, &str, &str)> {
    let mut fields = msg.strip_prefix(STOP_FAILURE_MARKER)?.splitn(3, '\0');
    let backend = match fields.next()? {
        "codex" => BackendKind::Codex,
        "claude" => BackendKind::Claude,
        "opencode" => BackendKind::Opencode,
        _ => return None,
    };
    Some((backend, fields.next()?, fields.next()?))
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
    cursors: &mut [RefreshCursor],
    db: Option<&ViewerDb>,
) -> (Vec<Session>, String, usize) {
    let mut all = Vec::new();
    let mut errors = Vec::new();
    let mut ok_count = 0;
    let now = now_ms();
    for (i, backend) in backends.iter_mut().enumerate() {
        match refresh_backend(db, backend.as_mut(), &last[i], &mut cursors[i], now) {
            RefreshOutcome::Authoritative { sessions }
            | RefreshOutcome::Shared { sessions }
            | RefreshOutcome::Stale { sessions } => {
                ok_count += 1;
                last[i] = sessions.clone();
                all.extend(sessions);
            }
            RefreshOutcome::SourceError { sessions, notice } => {
                errors.push(notice);
                all.extend(sessions);
            }
            RefreshOutcome::CachedError { sessions, notice } => {
                errors.push(notice);
                ok_count += 1;
                last[i] = sessions.clone();
                all.extend(sessions);
            }
            RefreshOutcome::Waiting | RefreshOutcome::Unchanged => {
                ok_count += 1;
                all.extend_from_slice(&last[i]);
            }
        }
    }
    (all, errors.join("  |  "), ok_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_viewer_core::{
        ListingCacheClaim, ListingCacheScope, ListingCacheSnapshot, SessionOrigin,
    };
    use agent_viewer_tui::mutations::{MutationOutcome, SpawnSelection};
    use std::{
        io as test_io,
        panic::{AssertUnwindSafe, catch_unwind},
        path::PathBuf,
        sync::{
            Arc as TestArc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

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

    struct CountedListingBackend {
        scope: ListingCacheScope,
        calls: TestArc<AtomicUsize>,
        error: String,
    }

    impl Backend for CountedListingBackend {
        fn kind(&self) -> BackendKind {
            self.scope.backend()
        }

        fn capabilities(&self) -> agent_viewer_core::Capabilities {
            agent_viewer_core::Capabilities::none()
        }

        fn listing_scope(&self) -> Option<ListingCacheScope> {
            Some(self.scope.clone())
        }

        fn list(&mut self) -> agent_viewer_core::Result<Vec<Session>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(agent_viewer_core::Error::Command(self.error.clone()))
        }

        fn spawn(
            &self,
            _dir: &std::path::Path,
            _task: &str,
            _model: Option<&str>,
            _effort: Option<&str>,
        ) -> agent_viewer_core::Result<agent_viewer_core::SpawnResult> {
            unreachable!("spawning is not exercised by listing refresh tests")
        }

        fn attach_command(
            &self,
            _session: &Session,
        ) -> Result<std::process::Command, agent_viewer_core::AttachRefusal> {
            unreachable!("attaching is not exercised by listing refresh tests")
        }
    }

    #[test]
    fn cold_lease_is_pending_without_calling_source_or_ending_startup() {
        let directory = tempfile::tempdir().expect("temporary viewer database directory");
        let path = directory.path().join("viewer.sqlite");
        let lease_holder = ViewerDb::open(&path).expect("lease holder database");
        let follower = ViewerDb::open(&path).expect("follower database");
        let scope =
            ListingCacheScope::new(BackendKind::Codex, "cold cache").expect("valid cache scope");
        let lease_now = now_ms();
        let _lease = match lease_holder
            .claim_listing_refresh(Some(&scope), None, lease_now, 2_000, 60_000)
            .expect("claim cold cache lease")
        {
            ListingCacheClaim::Claimed(lease) => lease,
            other => panic!("expected cold cache lease, got {other:?}"),
        };
        let calls = TestArc::new(AtomicUsize::new(0));
        let mut backends: Vec<Box<dyn Backend>> = vec![Box::new(CountedListingBackend {
            scope,
            calls: TestArc::clone(&calls),
            error: "source must stay idle".to_string(),
        })];
        let mut last = vec![Vec::new()];
        let mut cursors = vec![RefreshCursor::default()];

        let (sessions, notice, ok_count) =
            refresh(&mut backends, &mut last, &mut cursors, Some(&follower));

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(sessions.is_empty());
        assert!(notice.is_empty());
        assert_eq!(ok_count, 1, "a pending backend keeps startup usable");
        assert!(last[0].is_empty());
    }

    #[test]
    fn cached_error_rows_become_last_good_and_survive_a_following_lease() {
        let directory = tempfile::tempdir().expect("temporary viewer database directory");
        let path = directory.path().join("viewer.sqlite");
        let publisher = ViewerDb::open(&path).expect("publisher database");
        let local = ViewerDb::open(&path).expect("local database");
        let lease_holder = ViewerDb::open(&path).expect("lease holder database");
        let scope =
            ListingCacheScope::new(BackendKind::Codex, "stale cache").expect("valid cache scope");
        let cached = session(BackendKind::Codex, "cached", 1_000, false);
        let previous_local = session(BackendKind::Codex, "previous local", 500, false);
        let published_at = now_ms().saturating_sub(10_000);
        let lease = match publisher
            .claim_listing_refresh(Some(&scope), None, published_at, 2_000, 2_000)
            .expect("claim publication lease")
        {
            ListingCacheClaim::Claimed(lease) => lease,
            other => panic!("expected publication lease, got {other:?}"),
        };
        publisher
            .publish_listing(
                &lease,
                ListingCacheSnapshot::from_sessions(vec![cached.clone()])
                    .expect("serialize cached rows"),
                published_at,
            )
            .expect("publish cached rows");

        let calls = TestArc::new(AtomicUsize::new(0));
        let mut backends: Vec<Box<dyn Backend>> = vec![Box::new(CountedListingBackend {
            scope: scope.clone(),
            calls: TestArc::clone(&calls),
            error: "source unavailable".to_string(),
        })];
        let mut last = vec![vec![previous_local]];
        let mut cursors = vec![RefreshCursor::default()];

        let first = refresh(&mut backends, &mut last, &mut cursors, Some(&local));

        let lease_now = now_ms();
        let _lease = match lease_holder
            .claim_listing_refresh(Some(&scope), None, lease_now, 2_000, 60_000)
            .expect("claim following refresh lease")
        {
            ListingCacheClaim::Claimed(lease) => lease,
            other => panic!("expected following refresh lease, got {other:?}"),
        };
        let second = refresh(&mut backends, &mut last, &mut cursors, Some(&local));

        let plain_local = session(BackendKind::Codex, "plain local", 250, false);
        let mut plain_last = vec![vec![plain_local.clone()]];
        let mut plain_cursors = vec![RefreshCursor::default()];
        let plain_error = refresh(&mut backends, &mut plain_last, &mut plain_cursors, None);

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(first.0, vec![cached.clone()]);
        assert_eq!(first.1, "codex: command failed: source unavailable");
        assert_eq!(first.2, 1, "a cached source error keeps the backend usable");
        assert_eq!(last[0], vec![cached.clone()]);
        assert_eq!(second.0, vec![cached]);
        assert!(second.1.is_empty());
        assert_eq!(second.2, 1);
        assert_eq!(plain_error.0, vec![plain_local.clone()]);
        assert_eq!(plain_error.1, "codex: command failed: source unavailable");
        assert_eq!(plain_error.2, 0);
        assert_eq!(plain_last[0], vec![plain_local]);
    }

    fn test_ui(sessions: Vec<Session>) -> Ui {
        Ui {
            app: App::new(sessions),
            workspace: PathBuf::from(CWD),
            mode: Mode::Normal,
            notice: NoticeState::new(),
            db: None,
            composer: Composer::new(),
            themes: ui::ThemeState::default(),
            detach_trackers: HashMap::new(),
            last_backend_error: String::new(),
            mutations: MutationRunner::new(),
            mutation_executor: mutation_executor(ops::run_mutation),
            attaches: AttachRunner::new(),
            attach_executor: Arc::new(|_| Err("attach is not configured in this test".to_string())),
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
            mouse_press: None,
            terminal_palette: None,
            sprite: ui::SpriteKind::default(),
        }
    }

    #[test]
    fn mouse_capture_terminal_sequences_follow_only_state_transitions() {
        let mut output = Vec::new();
        let mut applied = true;

        sync_mouse_capture(&mut output, &mut applied, false).expect("disable mouse capture");
        let disabled_len = output.len();
        assert!(!applied);
        assert!(disabled_len > 0, "disable must write a terminal sequence");

        sync_mouse_capture(&mut output, &mut applied, false).expect("same state is a no-op");
        assert_eq!(
            output.len(),
            disabled_len,
            "unchanged state must not emit again"
        );

        sync_mouse_capture(&mut output, &mut applied, true).expect("enable mouse capture");
        assert!(applied);
        assert!(
            output.len() > disabled_len,
            "restoring list capture must write its terminal sequence"
        );
    }

    #[test]
    fn event_bridge_writes_mouse_sequences_for_attach_then_detach() {
        let mut ui = test_ui(vec![session(BackendKind::Claude, "attached", 1_000, false)]);
        assert!(
            ui.app
                .select_by_key(&(BackendKind::Claude, "attached".to_string()))
        );
        let current_model = ui.composer.model().to_string();
        ui.models
            .seed(BackendKind::Claude, vec![current_model], true);
        let authority_session = session(BackendKind::Claude, "attached", 1_000, false);
        ui.attach_executor = Arc::new(move |request| {
            let mut authority = AttachingBackend {
                session: authority_session.clone(),
            };
            ops::resolve_attach_with_backend(&mut authority, request)
        });
        let backends: Vec<Box<dyn Backend>> = vec![Box::new(AttachingBackend {
            session: session(BackendKind::Claude, "attached", 1_000, false),
        })];
        let (_snapshots_tx, snapshots) = channel();
        let (wake, _wake_rx) = channel();
        let refresher = Refresher { snapshots, wake };
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24))
            .expect("test terminal");
        let mut output = Vec::new();
        let mut applied = false;
        ui.mouse_capture = false;

        assert!(
            !process_event(
                Event::Key(crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Right,
                    crossterm::event::KeyModifiers::NONE,
                )),
                &backends,
                &refresher,
                &mut ui,
                &mut terminal,
                &mut output,
                &mut applied,
            )
            .expect("attach event"),
            "attach must not quit the viewer"
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        let plan = loop {
            if let Some(result) = ui.attaches.poll() {
                break result.expect("resolve event bridge attach");
            }
            assert!(
                Instant::now() < deadline,
                "event bridge attach did not resolve"
            );
            thread::yield_now();
        };
        assert!(
            install_completed_attach_plan(&mut ui, &mut terminal, plan, &mut output, &mut applied,)
                .expect("install event bridge attach")
        );
        assert!(matches!(ui.mode, Mode::Attached));
        assert!(ui.mouse_capture);
        assert!(applied);
        assert_eq!(
            output, b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1015h\x1b[?1006h",
            "completed attach must immediately enable terminal mouse capture"
        );
        let attached_output_len = output.len();

        assert!(
            !process_event(
                Event::Key(crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char(']'),
                    crossterm::event::KeyModifiers::CONTROL,
                )),
                &backends,
                &refresher,
                &mut ui,
                &mut terminal,
                &mut output,
                &mut applied,
            )
            .expect("detach event"),
            "detach must not quit the viewer"
        );
        assert!(matches!(ui.mode, Mode::Normal));
        assert!(ui.mouse_capture);
        assert!(applied);
        assert_eq!(
            output.len(),
            attached_output_len,
            "detach must keep terminal mouse capture enabled without another sequence"
        );
    }

    struct AttachingBackend {
        session: Session,
    }

    impl Backend for AttachingBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Claude
        }

        fn capabilities(&self) -> agent_viewer_core::Capabilities {
            agent_viewer_core::Capabilities {
                attach: true,
                ..agent_viewer_core::Capabilities::none()
            }
        }

        fn list(&mut self) -> agent_viewer_core::Result<Vec<Session>> {
            Ok(vec![self.session.clone()])
        }

        fn spawn(
            &self,
            _dir: &std::path::Path,
            _task: &str,
            _model: Option<&str>,
            _effort: Option<&str>,
        ) -> agent_viewer_core::Result<agent_viewer_core::SpawnResult> {
            unreachable!("spawning is not exercised by the event bridge")
        }

        fn attach_command(
            &self,
            _session: &Session,
        ) -> Result<std::process::Command, agent_viewer_core::AttachRefusal> {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "sleep 30"]);
            Ok(command)
        }
    }

    struct CountingActivityBackend {
        reads: TestArc<AtomicUsize>,
        timestamps: Vec<i64>,
        refreshed_timestamps: Option<Vec<i64>>,
    }

    impl Backend for CountingActivityBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Codex
        }

        fn capabilities(&self) -> agent_viewer_core::Capabilities {
            agent_viewer_core::Capabilities::none()
        }

        fn list(&mut self) -> agent_viewer_core::Result<Vec<Session>> {
            unreachable!("listing is not exercised by the activity cache")
        }

        fn turn_activity(
            &self,
            _session: &Session,
            _window: Duration,
        ) -> agent_viewer_core::Result<Vec<i64>> {
            let read = self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .refreshed_timestamps
                .as_ref()
                .filter(|_| read > 0)
                .unwrap_or(&self.timestamps)
                .clone())
        }

        fn spawn(
            &self,
            _dir: &std::path::Path,
            _task: &str,
            _model: Option<&str>,
            _effort: Option<&str>,
        ) -> agent_viewer_core::Result<agent_viewer_core::SpawnResult> {
            unreachable!("spawning is not exercised by the activity cache")
        }

        fn attach_command(
            &self,
            _session: &Session,
        ) -> Result<std::process::Command, agent_viewer_core::AttachRefusal> {
            unreachable!("attaching is not exercised by the activity cache")
        }
    }

    #[test]
    fn repeated_renders_do_not_repeat_backend_activity_reads() {
        const NOW_MS: i64 = 3_600_000;
        let reads = TestArc::new(AtomicUsize::new(0));
        let backends: Vec<Box<dyn Backend>> = vec![Box::new(CountingActivityBackend {
            reads: reads.clone(),
            timestamps: vec![3_100_000, 3_200_000, 3_300_000],
            refreshed_timestamps: None,
        })];
        let session = session(BackendKind::Codex, "active", 1_000, false);
        let mut cache = HashMap::new();
        let first = activity_results(&backends, &mut cache, vec![session.clone()], NOW_MS);
        let second = activity_results(
            &backends,
            &mut cache,
            vec![session.clone()],
            NOW_MS + ACTIVITY_REFRESH_MS - 1,
        );
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(first[0].2, second[0].2);

        let mut ui = test_ui(vec![session]);
        ui.app
            .set_activity_ribbon(first[0].0, &first[0].1, first[0].2.clone());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 24)).unwrap();
        for _ in 0..2 {
            terminal
                .draw(|frame| {
                    ui::draw(
                        frame,
                        ui::Draw {
                            app: &ui.app,
                            workspace: &ui.workspace,
                            mode: &ui.mode,
                            notice: ui.notice.text(),
                            composer: &ui.composer,
                            pulses: &ui.pulses,
                            now_ms: NOW_MS,
                            attach: None,
                            pr_status: &ui.pr_status,
                            logos: None,
                            list_hit: &ui.list_hit,
                            themes: &ui.themes,
                            sprite: ui.sprite,
                        },
                    );
                })
                .unwrap();
        }
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn activity_cache_rereads_at_expiry_with_unchanged_parent() {
        const NOW_MS: i64 = 3_600_000;
        let reads = TestArc::new(AtomicUsize::new(0));
        let backends: Vec<Box<dyn Backend>> = vec![Box::new(CountingActivityBackend {
            reads: reads.clone(),
            timestamps: vec![NOW_MS - 3_000_000],
            refreshed_timestamps: Some(vec![NOW_MS + ACTIVITY_REFRESH_MS]),
        })];
        let session = session(BackendKind::Codex, "active", 1_000, false);
        let mut cache = HashMap::new();
        let first = activity_results(&backends, &mut cache, vec![session.clone()], NOW_MS);
        let just_before_expiry = activity_results(
            &backends,
            &mut cache,
            vec![session.clone()],
            NOW_MS + ACTIVITY_REFRESH_MS - 1,
        );
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(first[0].2, just_before_expiry[0].2);

        let at_expiry = activity_results(
            &backends,
            &mut cache,
            vec![session],
            NOW_MS + ACTIVITY_REFRESH_MS,
        );
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        assert_ne!(just_before_expiry[0].2, at_expiry[0].2);
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
    fn stop_mutation_failure_disarms_removal_confirmation_through_main_loop_bridge() {
        let working = session(BackendKind::Codex, "working", 1_000, false);
        let mut ui = test_ui(vec![working.clone()]);
        assert!(
            ui.app
                .select_by_key(&(BackendKind::Codex, working.id.clone()))
        );
        assert_eq!(
            ui.app.kill_stage(1_000),
            agent_viewer_tui::app::KillStage::Stop
        );
        assert!(ui.app.is_armed(1_500));

        let executor = mutation_executor(|mutation| match mutation {
            ops::Mutation::Stop(_) => Err("stop failed".to_string()),
            _ => panic!("expected stop mutation"),
        });
        let result = executor(ops::Mutation::Stop(TargetRequest::new(
            BackendKind::Codex,
            working.id.clone(),
        )));
        apply_mutation_result(&mut ui, result);

        assert_eq!(ui.notice.text(), "stop failed");
        assert!(!ui.app.is_armed(1_500));
        assert_eq!(
            ui.app.kill_stage(2_000),
            agent_viewer_tui::app::KillStage::Stop
        );
    }

    #[test]
    fn stop_failure_for_another_session_preserves_removal_confirmation() {
        let first = session(BackendKind::Codex, "first", 1_000, false);
        let second = session(BackendKind::Codex, "second", 900, false);
        let mut ui = test_ui(vec![first.clone(), second.clone()]);
        assert!(
            ui.app
                .select_by_key(&(BackendKind::Codex, first.id.clone()))
        );
        assert_eq!(
            ui.app.kill_stage(1_000),
            agent_viewer_tui::app::KillStage::Stop
        );
        assert!(
            ui.app
                .select_by_key(&(BackendKind::Codex, second.id.clone()))
        );
        assert_eq!(
            ui.app.kill_stage(1_100),
            agent_viewer_tui::app::KillStage::Stop
        );
        assert!(ui.app.is_armed(1_500));

        let executor = mutation_executor(|mutation| match mutation {
            ops::Mutation::Stop(_) => Err("stop failed".to_string()),
            _ => panic!("expected stop mutation"),
        });
        let result = executor(ops::Mutation::Stop(TargetRequest::new(
            BackendKind::Codex,
            first.id,
        )));
        apply_mutation_result(&mut ui, result);

        assert_eq!(ui.notice.text(), "stop failed");
        assert!(ui.app.is_armed(1_500));
        assert_eq!(
            ui.app.kill_stage(2_000),
            agent_viewer_tui::app::KillStage::Remove
        );
    }

    #[test]
    fn unrelated_mutation_failure_preserves_removal_confirmation() {
        let working = session(BackendKind::Codex, "working", 1_000, false);
        let mut ui = test_ui(vec![working.clone()]);
        assert!(
            ui.app
                .select_by_key(&(BackendKind::Codex, working.id.clone()))
        );
        assert_eq!(
            ui.app.kill_stage(1_000),
            agent_viewer_tui::app::KillStage::Stop
        );
        assert!(ui.app.is_armed(1_500));

        let executor = mutation_executor(|mutation| match mutation {
            ops::Mutation::Rename(_, _) => Err("rename failed".to_string()),
            _ => panic!("expected rename mutation"),
        });
        let result = executor(ops::Mutation::Rename(
            TargetRequest::new(BackendKind::Codex, working.id),
            "new name".to_string(),
        ));
        apply_mutation_result(&mut ui, result);

        assert_eq!(ui.notice.text(), "rename failed");
        assert!(ui.app.is_armed(1_500));
        assert_eq!(
            ui.app.kill_stage(2_000),
            agent_viewer_tui::app::KillStage::Remove
        );
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

    #[derive(Clone)]
    struct SharedWriter(TestArc<Mutex<Vec<u8>>>);

    impl test_io::Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> test_io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> test_io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn bracketed_paste_is_disabled_during_panic_unwinding() {
        let bytes = TestArc::new(Mutex::new(Vec::new()));
        let writer = SharedWriter(TestArc::clone(&bytes));

        let result = catch_unwind(AssertUnwindSafe(|| {
            let _guard = BracketedPasteGuard::new(writer);
            panic!("simulate tui panic");
        }));

        assert!(result.is_err());
        assert!(
            bytes
                .lock()
                .unwrap()
                .windows(b"\x1b[?2004l".len())
                .any(|bytes| bytes == b"\x1b[?2004l"),
            "panic cleanup must disable bracketed paste"
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
