use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use agent_viewer_core::backend::{Backend, BackendKind, all_backends};
use agent_viewer_core::platform::{Platform, current_platform};
use agent_viewer_core::pty::{
    PtySession, PtySpec, TerminalPalette, VIEWPORT_SCROLLBACK_ROWS, spec_from_command,
};
use agent_viewer_core::spawn::now_ms;
use agent_viewer_core::state::{ViewerDb, apply_viewer_state, match_spawn, match_spawn_between};
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

use base64::Engine as _;
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
/// How long a cached tail stays fresh before the pane re-reads a still-selected session.
const TAIL_REFRESH_MS: i64 = 2_000;

fn available_spawn_backends(platform: Platform, path: Option<&OsStr>) -> Vec<BackendKind> {
    #[cfg(target_os = "linux")]
    let candidates = [BackendKind::Claude, BackendKind::Codex, BackendKind::Grok];
    #[cfg(not(target_os = "linux"))]
    let candidates = [BackendKind::Claude, BackendKind::Codex];
    candidates
        .into_iter()
        .filter(|backend| {
            agent_viewer_core::router::find_on_path(platform, backend.name(), path).is_some()
        })
        .collect()
}

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

/// One completed tail read: the session it was read for, the `updated_at_ms` it was read at,
/// and its events.
type TailResult = (Key, i64, Vec<agent_viewer_core::TailEvent>);

/// The tail pane's cached read. One entry, because only the selected row has a pane.
struct TailEntry {
    key: Key,
    version: i64,
    fetched_at_ms: i64,
    events: Vec<agent_viewer_core::TailEvent>,
}

/// Reads transcripts for the tail pane off the UI thread. It only ever calls
/// `Backend::tail`, which reads the backend's store and never starts a process.
struct TailWorker {
    requests: Sender<Session>,
    results: Receiver<TailResult>,
}

impl TailWorker {
    fn new(backends: Vec<Box<dyn Backend>>) -> TailWorker {
        let (request_tx, request_rx) = channel::<Session>();
        let (result_tx, result_rx) = channel::<TailResult>();
        thread::spawn(move || {
            while let Ok(session) = request_rx.recv() {
                let key = (session.backend, session.id.clone());
                let events = backends
                    .iter()
                    .find(|backend| backend.kind() == session.backend)
                    .and_then(|backend| backend.tail(&session, ui::TAIL_EVENTS).ok())
                    .unwrap_or_default();
                if result_tx
                    .send((key, session.updated_at_ms, events))
                    .is_err()
                {
                    return;
                }
            }
        });
        TailWorker {
            requests: request_tx,
            results: result_rx,
        }
    }

    fn request(&self, session: Session) {
        let _ = self.requests.send(session);
    }

    fn poll(&self) -> Option<TailResult> {
        self.results.try_recv().ok()
    }
}

/// Queue a tail read for the selected session when the pane is open and its cached copy is
/// missing, stale, or for another row. At most ONE read is ever in flight, which is what
/// keeps arrowing through thousands of rows from queueing thousands of reads.
fn request_tail(tail: &TailWorker, ui: &mut Ui, now_ms: i64) {
    if !ui.tail_open || matches!(ui.mode, Mode::Attached) || ui.tail_pending.is_some() {
        return;
    }
    let Some(session) = ui.app.selected() else {
        return;
    };
    let key = (session.backend, session.id.clone());
    let fresh = ui.tail.as_ref().is_some_and(|entry| {
        entry.key == key
            && entry.version == session.updated_at_ms
            && now_ms - entry.fetched_at_ms < TAIL_REFRESH_MS
    });
    if fresh {
        return;
    }
    let session = session.clone();
    ui.tail_pending = Some(key);
    tail.request(session);
}

/// Assemble the tail pane's view for the selected session, if the pane is open.
///
/// `live` is looked up in the EXISTING attach map and nothing else: the pane opportunistically
/// renders a PTY that already happens to be running, and never creates one.
fn build_tail_view(ui: &Ui) -> Option<ui::TailView<'_>> {
    if !ui.tail_open || matches!(ui.mode, Mode::Attached) {
        return None;
    }
    // A group header (or an empty list) has no session, but the pane still mounts: taking
    // its columns away mid-arrow would re-lay the whole list out and then undo that on the
    // next session row.
    let session = ui.app.selected();
    let key = session.map(|session| (session.backend, session.id.clone()));
    Some(ui::TailView {
        session,
        events: key.as_ref().and_then(|key| {
            ui.tail
                .as_ref()
                .filter(|entry| &entry.key == key)
                .map(|entry| entry.events.as_slice())
        }),
        live: key.as_ref().and_then(|key| ui.attached.get(key)),
    })
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

/// The same guard for mouse reporting. `ratatui`'s panic hook restores raw mode and the
/// alternate screen and nothing else, so a panic with any-motion tracking still on leaves the
/// shell printing escape sequences for every mouse move until the user runs `reset`.
struct MouseCaptureGuard<W: io::Write> {
    writer: W,
}

impl<W: io::Write> MouseCaptureGuard<W> {
    fn new(writer: W) -> Self {
        Self { writer }
    }
}

impl<W: io::Write> Drop for MouseCaptureGuard<W> {
    fn drop(&mut self) {
        let _ = execute!(&mut self.writer, DisableMouseCapture);
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

fn reconcile_mouse_capture<W: io::Write>(ui: &mut Ui, writer: &mut W, applied: &mut bool) {
    if let Err(error) = sync_mouse_capture(writer, applied, ui.mouse_capture) {
        let prior_mode = *applied;
        let rollback = keys::write_mouse_capture(writer, prior_mode);
        ui.mouse_capture = prior_mode;
        ui.mouse_press = None;
        let prior_name = if prior_mode { "on" } else { "off" };
        let guidance = if matches!(ui.mode, Mode::Attached) {
            "press ctrl+t to retry or ctrl+y to copy"
        } else {
            "press ctrl+t to retry"
        };
        match rollback {
            Ok(()) => ui.set_notice(format!(
                "mouse change failed and prior mode was restored to {prior_name}: {error}; {guidance}"
            )),
            Err(rollback_error) => ui.set_notice(format!(
                "terminal mouse state unknown after change failed: {error}; rollback failed: {rollback_error}; {guidance}"
            )),
        }
    }
}

fn drain_pending_copy<W: io::Write>(ui: &mut Ui, writer: &mut W) {
    let Some(contents) = ui.pending_copy.take() else {
        return;
    };
    let encoded = base64::engine::general_purpose::STANDARD.encode(contents.as_bytes());
    let mut frame = Vec::with_capacity(7 + encoded.len());
    frame.extend_from_slice(b"\x1b]52;;");
    frame.extend_from_slice(encoded.as_bytes());
    frame.push(b'\x07');

    match writer.write_all(&frame).and_then(|()| writer.flush()) {
        Ok(()) => ui.set_notice("copy request sent to terminal".to_string()),
        Err(error) => {
            let _ = writer.write_all(b"\x1b\\");
            ui.set_notice(format!(
                "terminal clipboard state unknown after request output failed: {error}; use ctrl+t to select text"
            ));
        }
    }
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
    reconcile_mouse_capture(ui, writer, applied_mouse_capture);
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
                let (rows, cols) = if matches!(ui.mode, Mode::Triage(_)) {
                    ui::panel_pty_size(size.into()).unwrap_or((1, 1))
                } else {
                    (
                        size.height.saturating_sub(ui::ATTACHED_CHROME_ROWS).max(1),
                        size.width.max(1),
                    )
                };
                let _ = pty.resize(rows, cols);
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
    reconcile_mouse_capture(ui, writer, applied_mouse_capture);
    drain_pending_copy(ui, writer);
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

/// What a resolved attach plan is for. Both kinds ride the same runner and the same backend
/// resolution; only what happens when they land differs.
enum AttachOutcome {
    /// A user attach: take over the screen with this session. `key` and `triage` record what
    /// the user was looking at when it was submitted, so a plan that lands after they moved
    /// on can be dropped instead of stealing the keyboard for the wrong session.
    ///
    /// A FAILED resolution rides here too (an `Err` inside the `Ok`), for the same reason the
    /// wall's does: a failure that arrives after the user walked away belongs to a session
    /// they are no longer looking at, and posting it as a bare footer notice reads as if the
    /// row they are on just failed. Carrying the key lets the same ownership guard cover it.
    Focus {
        key: Key,
        triage: bool,
        plan: Result<ops::AttachPlan, String>,
    },
    /// A wall tile joining. Carries its own key so a failure can be reported against the
    /// right tile, which is why the failure is an `Ok` here rather than a runner-level `Err`.
    Wall {
        key: Key,
        plan: Result<ops::AttachPlan, String>,
    },
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
    /// Per-PTY left-arrow exit gate, keyed like `attached`. Reset only when a new PTY is
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
    attaches: AttachRunner<AttachOutcome>,
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
    /// The exact attached viewport armed by Ctrl+Y and drained once by the outer terminal writer.
    pending_copy: Option<String>,
    /// Live PTYs for whatever is on screen: the attached session, or the wall's tiles.
    /// Nothing lingers here once it is off screen — leaving a session or closing the wall
    /// drops (and so kills) its entry. Conversation state persists in each backend's own
    /// store, so a session is rejoined by ID rather than kept warm.
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
    /// Whether finished rows fade toward `faint` as they age. Off unless the viewer db says
    /// otherwise; toggled from the command palette.
    age_ramp: bool,
    /// Whether the Ctrl+B tail pane is open.
    tail_open: bool,
    /// The tail pane's cached transcript read for the selected session.
    tail: Option<TailEntry>,
    /// The session a tail read is currently in flight for. One at a time.
    tail_pending: Option<Key>,
    /// Video wall (Ctrl+W): tiles every live session over the list region and gives the
    /// keyboard to the focused tile. A flag on the list view rather than a `Mode`, because
    /// everything outside the wall's own reserved chords is forwarded, not rebound.
    wall: ui::WallState,
    /// Rect of each wall tile as of the last frame, in tile order. Written by `draw` and read
    /// by the run loop to size each tile's child (`PtySession::resize` needs `&mut`; draw is
    /// `&`-only) and by the mouse handler to hit-test the pointer onto a tile.
    wall_rects: RefCell<Vec<ratatui::layout::Rect>>,
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
    let available_backends =
        available_spawn_backends(current_platform(), std::env::var_os("PATH").as_deref());

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
    let persisted_theme = db.as_ref().and_then(ui::theme::persisted_theme);
    let startup_sprite = startup_sprite(db.as_ref());
    // Unset (and no db at all) reads as off: the age ramp is opt-in.
    let startup_age_ramp = db
        .as_ref()
        .and_then(|db| db.age_ramp().ok())
        .unwrap_or(false);
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
        for backend in available_backends.iter().copied() {
            if let Ok(Some(cached)) = db.cached_models(backend) {
                models.seed(backend, cached.models, !is_stale(cached.fetched_at_ms, now));
            }
        }
    }
    // Prime whatever the seeds did not cover, so a first run (or a day-old catalog) is
    // discovering in the background while the user reads the list rather than starting a
    // multi-second probe the moment they tab the composer onto that backend. A backend with
    // a fresh seed is already marked attempted, so the steady state spawns nothing.
    for backend in available_backends.iter().copied() {
        models.request(backend);
    }

    let mut composer = Composer::new();
    composer.set_available_backends(available_backends);
    let mut ui = Ui {
        app,
        workspace,
        mode: Mode::Normal,
        notice: startup_notice,
        db,
        composer,
        themes,
        detach_trackers: HashMap::new(),
        last_backend_error: String::new(),
        mutations: MutationRunner::new(),
        mutation_executor: mutation_executor(ops::run_mutation),
        attaches: AttachRunner::new(),
        attach_executor: Arc::new(ops::resolve_attach),
        models,
        pulses: Pulses::new(),
        pr_status: PrStatusCache::new(),
        pending_spawn: None,
        pending_reply: None,
        pending_copy: None,
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
        age_ramp: startup_age_ramp,
        tail_open: false,
        tail: None,
        tail_pending: None,
        wall: ui::WallState::default(),
        wall_rects: RefCell::new(Vec::new()),
    };

    // The composer's Auto entry is capability-gated on the router binary, resolved once here:
    // without `agent-router` on PATH the entry never appears in the Tab cycle. When the router
    // is present it is also the STARTING selection: routed spawns are the default posture.
    ui.composer
        .set_auto_available(agent_viewer_core::router::available());
    ui.composer.default_to_auto();

    // Hand listing backends to the refresh worker. The UI set remains only for cheap capability
    // routing. Attach resolution builds its own fresh backend on the attach worker, and spawn is
    // a mutation because either operation can dial the backend runtime.
    let activity_backends = all_backends();
    let activity = ActivityWorker::new(activity_backends);
    let tail = TailWorker::new(all_backends());
    let refresher = spawn_refresh_worker(list_backends, last, cursors);
    let action_backends = all_backends();

    let mut terminal = ratatui::init();
    set_terminal_title(&mut io::stdout(), &ui.workspace);
    // Mouse capture powers list selection and attached transcript scrolling for Codex and
    // Claude. Ctrl+T remains the manual override. Starts on to match `ui.mouse_capture`.
    let _ = execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    let mut applied_mouse_capture = true;
    let result = {
        let _bracketed_paste = BracketedPasteGuard::new(io::stdout());
        // Both modes come off on the way out of this scope, whether `run` returns or unwinds.
        let _mouse_capture = MouseCaptureGuard::new(io::stdout());
        run(
            &mut terminal,
            &action_backends,
            &refresher,
            &activity,
            &tail,
            &mut ui,
            &mut applied_mouse_capture,
        )
    };
    ratatui::restore();
    result
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    backends: &[Box<dyn Backend>],
    refresher: &Refresher,
    activity: &ActivityWorker,
    tail: &TailWorker,
    ui: &mut Ui,
    applied_mouse_capture: &mut bool,
) -> io::Result<()> {
    let mut last_activity_request_ms = None;
    loop {
        let now = now_ms();
        while let Some((backend, id, ribbon)) = activity.poll() {
            ui.app.set_activity_ribbon(backend, &id, ribbon);
        }
        // Fold in any finished tail read, then queue the next one the open pane needs.
        while let Some((key, version, events)) = tail.poll() {
            ui.tail_pending = None;
            ui.tail = Some(TailEntry {
                key,
                version,
                fetched_at_ms: now,
                events,
            });
        }
        request_tail(tail, ui, now);
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
            apply_attach_result(
                ui,
                terminal,
                result,
                &mut io::stdout(),
                applied_mouse_capture,
            )?;
        }

        // Fold in any model catalog that finished discovering (persisted for the next run).
        actions::install_models(ui);

        // Fold in the freshest off-thread listing (a no-op until the worker sends one).
        apply_snapshot(refresher, ui);
        // Age-based notice expiry: a notice lives NOTICE_MS from when it was set, so it
        // always renders at least once regardless of where the loop is when it lands.
        if matches!(ui.mode, Mode::Normal | Mode::Attached) && ui.notice.expired(now) {
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

        // Connect any wall tile that is not connected yet — including a session that only
        // just started working, so the wall stays a live picture of what is running — then
        // size every connected child to the cell it will occupy. Both are off the render
        // path (resize needs `&mut`) and use last frame's geometry: one frame of lag on
        // entry, invisible in practice.
        prune_wall_tiles(ui, now);
        request_wall_joins(ui, now);
        resize_wall_tiles(ui, now);

        // Build the attach, tail, and wall views before borrowing the frame.
        let attach = build_attach_view(ui);
        let tail_view = build_tail_view(ui);
        let wall = build_wall_view(ui, now);
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
                    age_ramp: ui.age_ramp,
                    tail: tail_view,
                    wall,
                    wall_rects: &ui.wall_rects,
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

/// Land one completed attach resolution.
///
/// Resolution runs off the render thread and can take seconds (a codex attach dials the
/// app-server daemon). In that time the user can walk to another row, or close the view the
/// attach was submitted from, so a landed plan is applied only while it still targets what
/// they are looking at — the same ownership rule `install_wall_join` applies to a tile.
fn apply_attach_result<B: ratatui::backend::Backend, W: io::Write>(
    ui: &mut Ui,
    terminal: &mut ratatui::Terminal<B>,
    result: Result<AttachOutcome, String>,
    writer: &mut W,
    applied_mouse_capture: &mut bool,
) -> io::Result<()> {
    match result {
        Ok(AttachOutcome::Focus { key, triage, plan }) => {
            if !focus_attach_still_current(ui, &key, triage) {
                // The plan is only a resolved command: nothing has been spawned for it yet, so
                // dropping it here is the whole teardown. A failure is dropped on the same
                // terms - it describes a session the user has already left.
                drop(plan);
                ui.set_notice(format!("attach cancelled: {} is no longer in focus", key.1));
                return Ok(());
            }
            match plan {
                Ok(plan) => {
                    install_completed_attach_plan(
                        ui,
                        terminal,
                        plan,
                        writer,
                        applied_mouse_capture,
                    )?;
                }
                Err(notice) => ui.set_notice(notice),
            }
        }
        Ok(AttachOutcome::Wall { key, plan }) => install_wall_join(ui, key, plan),
        Err(notice) => ui.set_notice(notice),
    }
    Ok(())
}

/// Whether a completed focus attach still has an owning view. A list activation targets the row
/// captured at submission, so later cursor movement does not cancel it. Triage is different:
/// its panel can show only its current item, so a queued result must still belong to that item.
fn focus_attach_still_current(ui: &Ui, key: &Key, triage: bool) -> bool {
    if triage {
        let Mode::Triage(state) = &ui.mode else {
            return false;
        };
        return state.current().map(|item| item.key()).as_ref() == Some(key);
    }
    matches!(ui.mode, Mode::Normal)
}

/// Land a wall tile's connection. A failure is recorded against its tile rather than shown as
/// a footer notice: nine tiles joining at once could otherwise stamp nine notices over each
/// other, and the tile itself is where the user is looking.
fn install_wall_join(ui: &mut Ui, key: Key, plan: Result<ops::AttachPlan, String>) {
    // The wall closed (or this session stopped being live) while the join was in flight. Do
    // not leave an orphan child running for a tile nobody will draw.
    if !ui.wall.owns(&key) {
        return;
    }
    let plan = match plan {
        Ok(plan) => plan,
        Err(error) => {
            ui.wall.failed.insert(key, error);
            return;
        }
    };
    let palette = ui
        .themes
        .active()
        .terminal_palette()
        .or(ui.terminal_palette);
    // Size does not matter yet: `resize_wall_tiles` sets the real one from the region the
    // next frame publishes, before the child has drawn anything worth keeping.
    let spec = wall_tile_spec(&plan.command, palette);
    match PtySession::spawn(spec) {
        Ok(pty) => {
            ui.attached.insert(key, pty);
        }
        Err(error) => {
            ui.wall.failed.insert(key, error.to_string());
        }
    }
}

/// Drop every connection the wall no longer has a tile for. A session ages out of the recency
/// window, or stops being live, while the wall is still up; without this its child would sit
/// there invisible until the wall closed, and the process budget `MAX_TILES` is supposed to
/// enforce would drift upward as expired slots were replaced.
///
/// Removing the key from `requested` also invalidates any join still in flight for it, because
/// `install_wall_join` bails on `!wall.owns(&key)` before it spawns anything.
///
/// The zoomed session is exempt: the attach view is holding it and closes it on the way out.
fn prune_wall_tiles(ui: &mut Ui, now_ms: i64) {
    if !ui.wall.on {
        return;
    }
    let keys = ui::wall::tile_keys(&ui.app, now_ms);
    let expired: Vec<Key> = ui
        .wall
        .requested
        .iter()
        .filter(|key| !keys.contains(key))
        .cloned()
        .collect();
    for key in expired {
        ui.wall.requested.remove(&key);
        ui.wall.failed.remove(&key);
        ui.wall.sized.remove(&key);
        if ui.focused.as_ref() != Some(&key) {
            ui.remove_pty(&key);
        }
    }
}

/// The PTY spec for one wall tile. Size does not matter yet: `resize_wall_tiles` sets the
/// real one from the rects the next frame publishes, before the child has drawn anything
/// worth keeping.
///
/// Every tile keeps history, not just Codex: the wheel scrolls a tile by moving the viewer's
/// own viewport back over retained rows, and a tile with no retained rows cannot scroll at
/// all.
fn wall_tile_spec(command: &std::process::Command, palette: Option<TerminalPalette>) -> PtySpec {
    let mut spec = spec_from_command(command, 24, 80);
    spec.palette = palette;
    spec.scrollback_rows = VIEWPORT_SCROLLBACK_ROWS;
    spec
}

/// Ask the backend to resolve an attach for every wall tile that is not connected yet.
///
/// Each join is keyed per session so they run concurrently, unlike the single-slot `"attach"`
/// key the user attach path uses (which deliberately drops a second request while one is in
/// flight). `requested` makes this once-per-session-per-visit: without it a failed join would
/// be retried on every tick forever.
fn request_wall_joins(ui: &mut Ui, now_ms: i64) {
    if !ui.wall.on {
        return;
    }
    for key in ui::wall::tile_keys(&ui.app, now_ms) {
        if ui.wall.requested.contains(&key) {
            continue;
        }
        let Some(session) = ui.app.session_for(&key) else {
            continue;
        };
        let request = TargetRequest::from(session);
        let executor = ui.attach_executor.clone();
        let job_key = key.clone();
        let runner_key = format!("wall:{}:{}", key.0.name(), key.1);
        let submitted = ui.attaches.submit(runner_key, move || {
            Ok(AttachOutcome::Wall {
                key: job_key,
                plan: executor(request),
            })
        });
        if submitted {
            ui.wall.requested.insert(key);
        }
    }
}

/// Close every connection the wall opened and forget the visit. Called when the wall closes,
/// so nothing stays connected once it is off screen.
fn close_wall(ui: &mut Ui) {
    for key in std::mem::take(&mut ui.wall.requested) {
        // The focused PTY is the one the user zoomed into; the attach view still needs it and
        // closes it itself on the way out.
        if ui.focused.as_ref() == Some(&key) {
            continue;
        }
        ui.remove_pty(&key);
    }
    ui.wall.clear();
}

/// Resize each wall tile's PTY to the cell it will occupy, using the rects the previous frame
/// published. Only when a size actually changed — a SIGWINCH per tile per frame would keep
/// every child redrawing forever. Never spawns anything: a tile with no PTY was already
/// excluded upstream by `wall::tile_keys`.
///
/// A frame where the tile set just changed has one stale rect per tile; the zip drops the
/// excess and the next frame corrects the rest.
fn resize_wall_tiles(ui: &mut Ui, now_ms: i64) {
    if !ui.wall.on {
        return;
    }
    let rects = ui.wall_rects.borrow().clone();
    let keys = ui::wall::tile_keys(&ui.app, now_ms);
    for (key, rect) in keys.iter().zip(rects) {
        let size = ui::wall::tile_inner(rect);
        if ui.wall.sized.get(key) == Some(&size) {
            continue;
        }
        if let Some(pty) = ui.attached.get_mut(key) {
            let _ = pty.resize(size.0, size.1);
            ui.wall.sized.insert(key.clone(), size);
        }
    }
    ui.wall.sized.retain(|key, _| keys.contains(key));
}

/// Assemble the wall's tiles for this frame: the capped, list-ordered live sessions, each
/// with its connection if one has landed yet. None when the wall is off.
fn build_wall_view(ui: &Ui, now_ms: i64) -> Option<ui::WallView<'_>> {
    if !ui.wall.on {
        return None;
    }
    let overflow = ui::wall::overflow(ui::wall::wall_sessions(&ui.app, now_ms).len());
    let tiles = ui::wall::tile_keys(&ui.app, now_ms)
        .into_iter()
        .filter_map(|key| {
            let session = ui.app.session_for(&key)?;
            Some(ui::WallTile {
                project: agent_viewer_tui::app::project_label(&session.cwd),
                session,
                pty: ui.attached.get(&key),
                error: ui.wall.failed.get(&key).map(String::as_str),
            })
        })
        .collect::<Vec<_>>();
    Some(ui::WallView {
        selected: ui.wall.focus_index(&ui::wall::tile_keys(&ui.app, now_ms)),
        tiles,
        overflow,
    })
}

/// Assemble the `AttachView` for the focused session, if any.
/// The live child for whichever surface wants one: the full-screen attach view, or the triage
/// inbox's panel. Both render the same `PtySession` from `ui.attached`; only the rect differs.
fn build_attach_view(ui: &Ui) -> Option<AttachView<'_>> {
    if !matches!(ui.mode, Mode::Attached | Mode::Triage(_)) {
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
        "grok" => BackendKind::Grok,
        _ => return None,
    };
    Some((backend, fields.next()?, fields.next()?))
}

/// Resolve a successful spawn against a fresh backend listing. An exact backend identity
/// always wins. Backends without one reuse the viewer database's cwd and creation time rule.
///
/// The identity is matched against the row's short id as well as its id, because a routed claude
/// job is only ever reported by its SHORT id (the `~/.claude/jobs` key `claude agents` publishes
/// as `id`), never the full `sessionId` a claude row is keyed by. Comparing both here is smaller
/// and safer than translating the short id on the mutation worker, which would need a second
/// `claude agents --json` call with its own race against the job appearing; short ids are unique
/// job keys, so the extra comparison cannot mis-select.
fn match_pending_spawn(spawn: &SpawnSelection, sessions: &[Session]) -> Option<Key> {
    if let Some(session_id) = &spawn.session_id {
        return sessions
            .iter()
            .find(|session| {
                session.backend == spawn.backend
                    && (session.id.as_str() == session_id.as_str()
                        || session.short_id.as_deref() == Some(session_id.as_str()))
            })
            .map(|session| (session.backend, session.id.clone()));
    }
    let candidates = sessions
        .iter()
        .filter(|session| {
            session.backend != spawn.backend || !spawn.preexisting_ids.contains(&session.id)
        })
        .cloned()
        .collect::<Vec<_>>();
    if let Some(job_name) = &spawn.job_name {
        let exact_title = candidates
            .iter()
            .filter(|session| session.title == *job_name)
            .cloned()
            .collect::<Vec<_>>();
        if let Some(id) = match_spawn_between(
            spawn.backend,
            &spawn.cwd,
            spawn.submitted_at_ms,
            spawn.spawned_at_ms,
            &exact_title,
        ) {
            return Some((spawn.backend, id));
        }
    }
    // The interval, not one stamp: a routed job is created while the router runs, so its row can
    // be seconds older than the decision that selects it.
    match_spawn_between(
        spawn.backend,
        &spawn.cwd,
        spawn.submitted_at_ms,
        spawn.spawned_at_ms,
        &candidates,
    )
    .map(|id| (spawn.backend, id))
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
        collections::VecDeque,
        io as test_io,
        panic::{AssertUnwindSafe, catch_unwind},
        path::PathBuf,
        sync::{
            Arc as TestArc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    const CWD: &str = "/tmp";

    #[test]
    fn spawn_backend_discovery_only_returns_installed_clis() {
        let directory = tempfile::tempdir().expect("temporary executable directory");
        let suffix = if cfg!(windows) { ".exe" } else { "" };
        let path = directory.path().join(format!("codex{suffix}"));
        std::fs::write(&path, "").expect("write executable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&path)
                .expect("executable metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).expect("make executable");
        }

        assert_eq!(
            available_spawn_backends(current_platform(), Some(directory.path().as_os_str())),
            vec![BackendKind::Codex]
        );
    }

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
            subagent: false,
            summary: String::new(),
            pid: None,
            rollout_path: None,
            pr_refs: Vec::new(),
            daemon_hosted: backend == BackendKind::Codex,
        }
    }

    /// The children forked by the CALLING thread, read straight from `/proc`. The tail
    /// pane's whole premise is that filling it costs no process, so this is the assertion
    /// that holds it to that.
    ///
    /// Thread-scoped rather than process-scoped on purpose: other tests in this binary
    /// spawn and reap PTY children in parallel, and the kernel files each child under the
    /// tid that forked it, so a process-wide snapshot measures their noise instead of our
    /// claim.
    fn thread_child_pids() -> Vec<String> {
        let children =
            std::fs::read_to_string("/proc/thread-self/children").expect("read thread children");
        let mut pids: Vec<String> = children.split_whitespace().map(str::to_string).collect();
        pids.sort();
        pids
    }

    /// A done Codex session backed by a real rollout transcript on disk.
    fn transcript_session(dir: &std::path::Path, id: &str, body: &str) -> Session {
        let path = dir.join(format!("{id}.jsonl"));
        std::fs::write(&path, body).expect("write rollout");
        Session {
            status: Status::Done,
            rollout_path: Some(path),
            ..session(BackendKind::Codex, id, 1_000, false)
        }
    }

    #[test]
    fn retargeting_the_tail_pane_reads_transcripts_and_starts_no_process() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let sessions = (0..3)
            .map(|n| {
                transcript_session(
                    dir.path(),
                    &format!("row-{n}"),
                    &format!(
                        "{}\n{}\n",
                        format_args!(
                            r#"{{"type":"response_item","payload":{{"role":"assistant","content":[{{"type":"output_text","text":"reply {n}"}}]}}}}"#
                        ),
                        r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"ls -la\"}"}}"#
                    ),
                )
            })
            .collect::<Vec<_>>();
        let mut ui = test_ui(sessions.clone());
        ui.tail_open = true;
        let backend = agent_viewer_core::codex::CodexBackend::new(PathBuf::from("/unused"));

        // The read itself, on this thread, so the child-process claim is measured where the
        // fork would land. The worker below runs this exact call on its own thread.
        let before = thread_child_pids();
        for session in &sessions {
            assert!(!backend.tail(session, ui::TAIL_EVENTS).unwrap().is_empty());
        }
        assert_eq!(
            thread_child_pids(),
            before,
            "reading a transcript for the tail pane must never fork a process"
        );

        let worker = TailWorker::new(vec![Box::new(agent_viewer_core::codex::CodexBackend::new(
            PathBuf::from("/unused"),
        ))]);
        for expected in 0..sessions.len() {
            request_tail(&worker, &mut ui, 1_000 + expected as i64);
            let (key, version, events) = worker
                .results
                .recv_timeout(Duration::from_secs(5))
                .expect("a tail read completes");
            ui.tail_pending = None;
            ui.tail = Some(TailEntry {
                key,
                version,
                fetched_at_ms: 1_000 + expected as i64,
                events,
            });

            // A done session with no live process still fills the pane, out of its
            // transcript, tool call and all.
            let view = build_tail_view(&ui).expect("the open pane has a view");
            assert!(view.live.is_none(), "no PTY exists for a done session");
            let events = view.events.expect("the read landed");
            assert_eq!(
                events,
                [
                    agent_viewer_core::TailEvent::Agent(format!("reply {expected}")),
                    agent_viewer_core::TailEvent::Tool {
                        name: "exec_command".to_string(),
                        detail: "ls -la".to_string(),
                    },
                ],
                "the pane shows the selected row's own transcript"
            );
            ui.app.move_selection(1);
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
            job_name: None,
            cwd: PathBuf::from(CWD),
            submitted_at_ms: spawned_at_ms,
            spawned_at_ms,
            preexisting_ids: HashSet::new(),
        }
    }

    /// A routed pending spawn: the window opens when the router was invoked and closes when it
    /// returned, because the job was created somewhere in between.
    fn routed_pending(
        backend: BackendKind,
        session_id: Option<&str>,
        job_name: Option<&str>,
        submitted_at_ms: i64,
        spawned_at_ms: i64,
    ) -> SpawnSelection {
        SpawnSelection {
            backend,
            session_id: session_id.map(str::to_string),
            job_name: job_name.map(str::to_string),
            cwd: PathBuf::from(CWD),
            submitted_at_ms,
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
            job_name: None,
            cwd: PathBuf::from(CWD),
            submitted_at_ms: spawned_at_ms,
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

    fn working_session(id: &str) -> Session {
        let mut session = session(BackendKind::Codex, id, 1_000, false);
        session.status = Status::Working;
        session
    }

    /// The wall joins each live session once per visit. A per-frame re-request would hammer
    /// the backend (and, for Codex, the app-server) many times a second, and a join that
    /// failed would be retried forever.
    /// A tile with no retained history cannot be scrolled, so the wheel would be dead on
    /// every tile. This is the setting that makes wall scrolling possible at all.
    #[test]
    fn every_wall_tile_gets_retained_history_to_scroll() {
        let spec = wall_tile_spec(&std::process::Command::new("true"), None);
        assert_eq!(spec.scrollback_rows, VIEWPORT_SCROLLBACK_ROWS);
    }

    #[test]
    fn the_wall_requests_each_join_once_not_once_per_frame() {
        let calls = TestArc::new(AtomicUsize::new(0));
        let counted = TestArc::clone(&calls);
        let mut ui = test_ui(vec![working_session("one"), working_session("two")]);
        ui.attach_executor = Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            // Resolution failing is fine here: what is under test is how many times it is
            // asked, and a failure is the case that would otherwise retry forever.
            Err("backend offline".to_string())
        });
        ui.wall.on = true;

        for _ in 0..5 {
            request_wall_joins(&mut ui, now_ms());
        }
        // Drain so nothing is left "in flight" masking a re-request behind the runner's dedup.
        let deadline = Instant::now() + Duration::from_secs(2);
        while ui.wall.failed.len() < 2 {
            while let Some(Ok(AttachOutcome::Wall { key, plan })) = ui.attaches.poll() {
                install_wall_join(&mut ui, key, plan);
            }
            assert!(Instant::now() < deadline, "wall joins did not resolve");
            thread::yield_now();
        }
        assert!(!ui.attaches.in_flight("wall:codex:one"), "join not drained");
        for _ in 0..5 {
            request_wall_joins(&mut ui, now_ms());
        }

        // `submit` marks its key in flight synchronously, so this reads the decision itself
        // rather than racing the worker thread that would increment `calls`.
        for key in ["wall:codex:one", "wall:codex:two"] {
            assert!(
                !ui.attaches.in_flight(key),
                "{key} was re-requested after it had already resolved"
            );
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "one resolution per live session, not per frame"
        );
        assert_eq!(ui.wall.failed.len(), 2, "each failure is kept on its tile");
    }

    /// Closing the wall must close what it opened. A leaked child keeps a real agent process
    /// connected to a session nobody is looking at.
    #[test]
    fn closing_the_wall_closes_every_connection_it_opened() {
        let mut ui = test_ui(vec![working_session("tile")]);
        let key = (BackendKind::Codex, "tile".to_string());
        let pty = PtySession::spawn(agent_viewer_core::pty::PtySpec {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "sleep 30".to_string()],
            cwd: None,
            envs: Vec::new(),
            rows: 6,
            cols: 40,
            palette: None,
            scrollback_rows: 0,
        })
        .expect("wall tile child");
        let pid = pty.pid().expect("child pid");
        ui.attached.insert(key.clone(), pty);
        ui.wall.on = true;
        ui.wall.requested.insert(key.clone());

        ui.wall.on = false;
        close_wall(&mut ui);

        assert!(ui.attached.is_empty(), "wall connection leaked");
        assert!(ui.wall.requested.is_empty());
        let deadline = Instant::now() + Duration::from_secs(2);
        while std::path::Path::new(&format!("/proc/{pid}")).exists() {
            assert!(
                Instant::now() < deadline,
                "tile child {pid} outlived the wall"
            );
            thread::sleep(Duration::from_millis(20));
        }
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
            pending_copy: None,
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
            age_ramp: false,
            tail_open: false,
            tail: None,
            tail_pending: None,
            wall: ui::WallState::default(),
            wall_rects: RefCell::new(Vec::new()),
        }
    }

    #[derive(Clone, Copy)]
    enum WriteAction {
        Accept(usize),
        Fail,
        Zero,
    }

    #[derive(Default)]
    struct RecordingTerminalWriter {
        output: Vec<u8>,
        attempts: Vec<Vec<u8>>,
        actions: VecDeque<WriteAction>,
        flushes: usize,
        fail_flush: bool,
    }

    impl RecordingTerminalWriter {
        fn with_actions(actions: impl IntoIterator<Item = WriteAction>) -> Self {
            Self {
                actions: actions.into_iter().collect(),
                ..Self::default()
            }
        }
    }

    impl test_io::Write for RecordingTerminalWriter {
        fn write(&mut self, buffer: &[u8]) -> test_io::Result<usize> {
            self.attempts.push(buffer.to_vec());
            match self.actions.pop_front() {
                Some(WriteAction::Accept(limit)) => {
                    let accepted = limit.min(buffer.len());
                    self.output.extend_from_slice(&buffer[..accepted]);
                    Ok(accepted)
                }
                Some(WriteAction::Fail) => Err(test_io::Error::new(
                    test_io::ErrorKind::PermissionDenied,
                    "terminal write rejected",
                )),
                Some(WriteAction::Zero) => Ok(0),
                None => {
                    self.output.extend_from_slice(buffer);
                    Ok(buffer.len())
                }
            }
        }

        fn flush(&mut self) -> test_io::Result<()> {
            self.flushes += 1;
            if self.fail_flush {
                Err(test_io::Error::new(
                    test_io::ErrorKind::BrokenPipe,
                    "terminal flush rejected",
                ))
            } else {
                Ok(())
            }
        }
    }

    fn osc52_frame(contents: &str) -> Vec<u8> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(contents.as_bytes());
        let mut frame = b"\x1b]52;;".to_vec();
        frame.extend_from_slice(encoded.as_bytes());
        frame.push(b'\x07');
        frame
    }

    fn control_key(character: char) -> Event {
        Event::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(character),
            crossterm::event::KeyModifiers::CONTROL,
        ))
    }

    fn process_test_event<W: test_io::Write>(
        event: Event,
        ui: &mut Ui,
        terminal: &mut ratatui::Terminal<ratatui::backend::TestBackend>,
        writer: &mut W,
        applied_mouse_capture: &mut bool,
    ) -> test_io::Result<bool> {
        let refresher = test_refresher();
        process_event(
            event,
            &[],
            &refresher,
            ui,
            terminal,
            writer,
            applied_mouse_capture,
        )
    }

    fn test_refresher() -> Refresher {
        let (_snapshots_tx, snapshots) = channel();
        let (wake, _wake_rx) = channel();
        Refresher { snapshots, wake }
    }

    fn wait_for_screen(pty: &PtySession, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !pty.with_screen(|screen| screen.contents().contains(needle)) {
            assert!(
                Instant::now() < deadline,
                "attached child screen did not contain {needle:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn attached_ui(attached_session: Session, pty: PtySession) -> (Ui, Key) {
        let key = (attached_session.backend, attached_session.id.clone());
        let mut ui = test_ui(vec![attached_session.clone()]);
        ui.mode = Mode::Attached;
        ui.focused = Some(key.clone());
        ui.focused_session = Some(attached_session);
        ui.detach_trackers.insert(key.clone(), DetachTracker::new());
        ui.attached.insert(key.clone(), pty);
        (ui, key)
    }

    /// Landing an attach while the triage modal is up must keep the modal AND size the child
    /// to the panel. Flipping to `Mode::Attached` here would eject the user from the queue the
    /// instant the session came up, and a full-screen-sized child wraps its output at a column
    /// the panel is not wide enough to show.
    #[test]
    fn an_attach_landing_under_triage_keeps_the_modal_and_sizes_the_child_to_the_panel() {
        let mut blocked = session(BackendKind::Claude, "blocked", 1_000, false);
        blocked.status = agent_viewer_core::Status::NeedsInput {
            reason: Some("pick one".to_string()),
        };
        let mut ui = test_ui(vec![blocked.clone()]);
        ui.mode = Mode::Triage(agent_viewer_tui::ui::TriageState::new(
            agent_viewer_tui::ui::triage_queue(&[blocked.clone()]),
        ));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).unwrap();
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg("sleep 30");

        let installed = actions::install_attach_plan(
            &mut ui,
            &mut terminal,
            ops::AttachPlan {
                session: blocked.clone(),
                command,
            },
        )
        .expect("install the attach");

        assert!(installed, "the child must land");
        assert!(
            matches!(ui.mode, Mode::Triage(_)),
            "the queue must survive its own attach"
        );
        let key: Key = (blocked.backend, blocked.id.clone());
        let size = ui.attached[&key].with_screen(|screen| screen.size());
        let expected = ui::panel_pty_size(ratatui::layout::Rect::new(0, 0, 100, 40))
            .expect("a 100x40 frame hosts a panel");
        assert_eq!(
            size, expected,
            "the child must be sized to the panel it is drawn into, not the whole screen"
        );
        ui.attached.get_mut(&key).expect("the child").kill();
    }

    #[test]
    fn attached_ctrl_y_emits_exact_unicode_frame_once_and_retains_child_interrupt() {
        let attached_session = session(BackendKind::Codex, "copy", 1_000, false);
        let pty = PtySession::spawn(agent_viewer_core::pty::PtySpec {
            program: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                concat!(
                    "stty raw -echo; ",
                    "printf 'visible λ🦀\\r\\nsecond café\\r\\nCOPYREADY'; ",
                    "captured=$(dd bs=1 count=1 2>/dev/null | od -An -tx1 | tr -d ' \\n'); ",
                    "printf '\\r\\nFIRST:%s\\r\\n' \"$captured\"; ",
                    "sleep 30"
                )
                .to_string(),
            ],
            cwd: None,
            envs: Vec::new(),
            rows: 6,
            cols: 40,
            palette: None,
            scrollback_rows: agent_viewer_core::pty::VIEWPORT_SCROLLBACK_ROWS,
        })
        .expect("spawn attached copy child");
        wait_for_screen(&pty, "COPYREADY");

        let expected = pty.with_screen(|screen| screen.contents());
        assert!(expected.contains("visible λ🦀"));
        assert!(expected.contains("second café"));

        let (mut ui, key) = attached_ui(attached_session, pty);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        let mut writer = RecordingTerminalWriter::default();
        let mut applied_mouse_capture = true;

        let copied = process_test_event(
            control_key('y'),
            &mut ui,
            &mut terminal,
            &mut writer,
            &mut applied_mouse_capture,
        );

        assert!(!copied.expect("copy event must keep the viewer running"));
        assert_eq!(writer.output, osc52_frame(&expected));
        assert_eq!(writer.flushes, 1, "the complete request must be flushed");
        assert!(ui.pending_copy.is_none(), "the request must drain once");
        assert!(ui.mouse_capture);
        assert!(applied_mouse_capture);
        assert_eq!(
            ui.notice.text().to_lowercase(),
            "copy request sent to terminal"
        );
        assert!(!ui.notice.text().to_lowercase().contains("copied"));

        let interrupted = process_test_event(
            control_key('c'),
            &mut ui,
            &mut terminal,
            &mut writer,
            &mut applied_mouse_capture,
        );
        assert!(!interrupted.expect("interrupt event must keep the viewer running"));
        wait_for_screen(ui.attached.get(&key).unwrap(), "FIRST:03");

        let output_after_copy = writer.output.clone();
        process_test_event(
            Event::FocusGained,
            &mut ui,
            &mut terminal,
            &mut writer,
            &mut applied_mouse_capture,
        )
        .expect("followup event");
        assert_eq!(writer.output, output_after_copy, "copy must be one shot");
        ui.attached.get_mut(&key).unwrap().kill();
    }

    #[test]
    fn attached_ctrl_y_emits_the_real_scrolled_historical_viewport() {
        let attached_session = session(BackendKind::Codex, "history", 1_000, false);
        let mut pty = PtySession::spawn(agent_viewer_core::pty::PtySpec {
            program: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                concat!(
                    "stty raw -echo; ",
                    "index=0; ",
                    "while [ \"$index\" -lt 12 ]; do ",
                    "printf 'history-%02d\\r\\n' \"$index\"; ",
                    "index=$((index + 1)); ",
                    "done; ",
                    "printf 'LIVE-END'; ",
                    "sleep 30"
                )
                .to_string(),
            ],
            cwd: None,
            envs: Vec::new(),
            rows: 4,
            cols: 40,
            palette: None,
            scrollback_rows: agent_viewer_core::pty::VIEWPORT_SCROLLBACK_ROWS,
        })
        .expect("spawn historical viewport child");
        wait_for_screen(&pty, "LIVE-END");
        assert!(pty.scroll_viewport_up(usize::MAX) > 0);
        let historical = pty.with_screen(|screen| screen.contents());
        assert!(historical.contains("history-00"), "{historical:?}");
        assert!(!historical.contains("LIVE-END"), "{historical:?}");

        let (mut ui, key) = attached_ui(attached_session, pty);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        let mut writer = RecordingTerminalWriter::default();
        let mut applied_mouse_capture = true;

        process_test_event(
            control_key('y'),
            &mut ui,
            &mut terminal,
            &mut writer,
            &mut applied_mouse_capture,
        )
        .expect("historical copy request");

        assert_eq!(writer.output, osc52_frame(&historical));
        ui.attached.get_mut(&key).unwrap().kill();
    }

    #[test]
    fn missing_and_whitespace_transcripts_emit_no_request_and_recommend_selection() {
        let mut missing = test_ui(Vec::new());
        missing.mode = Mode::Attached;
        missing.focused = Some((BackendKind::Codex, "missing".to_string()));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        let mut writer = RecordingTerminalWriter::default();
        let mut applied = true;

        process_test_event(
            control_key('y'),
            &mut missing,
            &mut terminal,
            &mut writer,
            &mut applied,
        )
        .expect("missing transcript request");
        assert!(writer.output.is_empty());
        assert_eq!(writer.flushes, 0);
        assert!(missing.pending_copy.is_none());
        let missing_notice = missing.notice.text().to_lowercase();
        assert!(missing_notice.contains("ctrl+t"), "{missing_notice:?}");
        assert!(!missing_notice.contains("sent"), "{missing_notice:?}");
        assert!(!missing_notice.contains("copied"), "{missing_notice:?}");

        let attached_session = session(BackendKind::Codex, "blank", 1_000, false);
        let pty = PtySession::spawn(agent_viewer_core::pty::PtySpec {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "printf '   '; sleep 30".to_string()],
            cwd: None,
            envs: Vec::new(),
            rows: 4,
            cols: 20,
            palette: None,
            scrollback_rows: agent_viewer_core::pty::VIEWPORT_SCROLLBACK_ROWS,
        })
        .expect("spawn whitespace transcript child");
        wait_for_screen(&pty, "   ");
        let (mut blank, key) = attached_ui(attached_session, pty);
        let mut blank_writer = RecordingTerminalWriter::default();

        process_test_event(
            control_key('y'),
            &mut blank,
            &mut terminal,
            &mut blank_writer,
            &mut applied,
        )
        .expect("whitespace transcript request");
        assert!(blank_writer.output.is_empty());
        assert_eq!(blank_writer.flushes, 0);
        assert!(blank.pending_copy.is_none());
        let blank_notice = blank.notice.text().to_lowercase();
        assert!(blank_notice.contains("ctrl+t"), "{blank_notice:?}");
        assert!(!blank_notice.contains("sent"), "{blank_notice:?}");
        assert!(!blank_notice.contains("copied"), "{blank_notice:?}");
        blank.attached.get_mut(&key).unwrap().kill();
    }

    #[test]
    fn exited_retained_pty_emits_its_final_visible_screen() {
        let attached_session = session(BackendKind::Codex, "exited", 1_000, false);
        let mut pty = PtySession::spawn(agent_viewer_core::pty::PtySpec {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "printf 'retained final λ'".to_string()],
            cwd: None,
            envs: Vec::new(),
            rows: 4,
            cols: 24,
            palette: None,
            scrollback_rows: agent_viewer_core::pty::VIEWPORT_SCROLLBACK_ROWS,
        })
        .expect("spawn retained transcript child");
        wait_for_screen(&pty, "retained final λ");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !pty.is_exited() {
            assert!(Instant::now() < deadline, "attached child did not exit");
            thread::sleep(Duration::from_millis(10));
        }
        let expected = pty.with_screen(|screen| screen.contents());
        let (mut ui, _) = attached_ui(attached_session, pty);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        let mut writer = RecordingTerminalWriter::default();
        let mut applied = true;

        process_test_event(
            control_key('y'),
            &mut ui,
            &mut terminal,
            &mut writer,
            &mut applied,
        )
        .expect("retained transcript request");

        assert_eq!(writer.output, osc52_frame(&expected));
        assert_eq!(ui.notice.text(), "copy request sent to terminal");
    }

    #[test]
    fn resize_updates_the_real_viewport_before_the_next_copy_request() {
        let attached_session = session(BackendKind::Codex, "resize", 1_000, false);
        let pty = PtySession::spawn(agent_viewer_core::pty::PtySpec {
            program: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "printf 'resize marker one\\r\\nresize marker two'; sleep 30".to_string(),
            ],
            cwd: None,
            envs: Vec::new(),
            rows: 4,
            cols: 20,
            palette: None,
            scrollback_rows: agent_viewer_core::pty::VIEWPORT_SCROLLBACK_ROWS,
        })
        .expect("spawn resize transcript child");
        wait_for_screen(&pty, "resize marker");
        let (mut ui, key) = attached_ui(attached_session, pty);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 6)).unwrap();
        terminal.backend_mut().resize(18, 9);
        let mut writer = RecordingTerminalWriter::default();
        let mut applied = true;

        process_test_event(
            Event::Resize(18, 9),
            &mut ui,
            &mut terminal,
            &mut writer,
            &mut applied,
        )
        .expect("resize event");
        assert_eq!(
            ui.attached
                .get(&key)
                .unwrap()
                .with_screen(|screen| screen.size()),
            (7, 18)
        );
        assert!(writer.output.is_empty());
        let resized = ui
            .attached
            .get(&key)
            .unwrap()
            .with_screen(|screen| screen.contents());

        process_test_event(
            control_key('y'),
            &mut ui,
            &mut terminal,
            &mut writer,
            &mut applied,
        )
        .expect("copy after resize");
        assert_eq!(writer.output, osc52_frame(&resized));
        ui.attached.get_mut(&key).unwrap().kill();
    }

    fn assert_failed_copy_is_drained(writer: &mut RecordingTerminalWriter) {
        let mut ui = test_ui(Vec::new());
        ui.mode = Mode::Attached;
        ui.pending_copy = Some("failure λ".to_string());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        let mut applied = true;

        let result = process_test_event(
            Event::FocusGained,
            &mut ui,
            &mut terminal,
            writer,
            &mut applied,
        );

        assert!(!result.expect("copy failure must keep the viewer running"));
        assert!(ui.pending_copy.is_none(), "failed requests must drain");
        let notice = ui.notice.text().to_lowercase();
        assert!(notice.contains("clipboard"), "{notice:?}");
        assert!(notice.contains("unknown"), "{notice:?}");
        assert!(notice.contains("ctrl+t"), "{notice:?}");
        assert!(!notice.contains("sent"), "{notice:?}");
        assert!(!notice.contains("copied"), "{notice:?}");
        let attempts = writer.attempts.len();
        let output = writer.output.clone();
        let flushes = writer.flushes;

        process_test_event(
            Event::FocusGained,
            &mut ui,
            &mut terminal,
            writer,
            &mut applied,
        )
        .expect("event after failed copy");
        assert_eq!(writer.attempts.len(), attempts, "failure must not retry");
        assert_eq!(writer.output, output, "failure must not emit again");
        assert_eq!(writer.flushes, flushes, "failure must not flush again");
    }

    #[test]
    fn write_failure_before_frame_output_attempts_recovery_without_false_success() {
        let frame = osc52_frame("failure λ");
        let mut writer = RecordingTerminalWriter::with_actions([WriteAction::Fail]);

        assert_failed_copy_is_drained(&mut writer);

        assert_eq!(writer.attempts[0], frame);
        assert_eq!(
            writer.attempts.last().map(Vec::as_slice),
            Some(&b"\x1b\\"[..])
        );
        assert_eq!(writer.output, b"\x1b\\");
    }

    #[test]
    fn prefix_then_write_failure_attempts_recovery_and_reports_unknown_state() {
        let frame = osc52_frame("failure λ");
        let prefix = 5;
        let mut writer =
            RecordingTerminalWriter::with_actions([WriteAction::Accept(prefix), WriteAction::Fail]);

        assert_failed_copy_is_drained(&mut writer);

        let mut expected = frame[..prefix].to_vec();
        expected.extend_from_slice(b"\x1b\\");
        assert_eq!(writer.output, expected);
        assert_eq!(
            writer.attempts.last().map(Vec::as_slice),
            Some(&b"\x1b\\"[..])
        );
    }

    #[test]
    fn zero_write_attempts_recovery_and_drains_the_request() {
        let frame = osc52_frame("failure λ");
        let mut writer = RecordingTerminalWriter::with_actions([WriteAction::Zero]);

        assert_failed_copy_is_drained(&mut writer);

        assert_eq!(writer.attempts[0], frame);
        assert_eq!(
            writer.attempts.last().map(Vec::as_slice),
            Some(&b"\x1b\\"[..])
        );
        assert_eq!(writer.output, b"\x1b\\");
    }

    #[test]
    fn flush_failure_attempts_recovery_and_never_claims_the_request_was_sent() {
        let frame = osc52_frame("failure λ");
        let mut writer = RecordingTerminalWriter {
            fail_flush: true,
            ..RecordingTerminalWriter::default()
        };

        assert_failed_copy_is_drained(&mut writer);

        let mut expected = frame;
        expected.extend_from_slice(b"\x1b\\");
        assert_eq!(writer.output, expected);
        assert_eq!(writer.flushes, 1);
        assert_eq!(
            writer.attempts.last().map(Vec::as_slice),
            Some(&b"\x1b\\"[..])
        );
    }

    const MOUSE_ENABLE: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1015h\x1b[?1006h";
    const MOUSE_DISABLE: &[u8] = b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l";

    #[test]
    fn partial_mouse_failure_restores_the_prior_mode_with_surface_guidance() {
        for (mode, attached) in [
            (Mode::Normal, false),
            (Mode::Help, false),
            (Mode::Attached, true),
        ] {
            let mut ui = test_ui(Vec::new());
            ui.mode = mode;
            ui.mouse_capture = false;
            keys::tests::seed_mouse_press_for_reconciliation(&mut ui);
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
            let prefix = 5;
            let mut writer = RecordingTerminalWriter::with_actions([
                WriteAction::Accept(prefix),
                WriteAction::Fail,
            ]);
            let mut applied = true;

            process_test_event(
                Event::FocusGained,
                &mut ui,
                &mut terminal,
                &mut writer,
                &mut applied,
            )
            .expect("mouse rollback");

            let mut expected = MOUSE_DISABLE[..prefix].to_vec();
            expected.extend_from_slice(MOUSE_ENABLE);
            assert_eq!(writer.output, expected);
            assert_eq!(writer.attempts[0], MOUSE_DISABLE);
            assert_eq!(writer.attempts[1], MOUSE_DISABLE[prefix..]);
            assert_eq!(writer.attempts[2], MOUSE_ENABLE);
            assert!(applied);
            assert!(ui.mouse_capture);
            assert!(ui.mouse_press.is_none());
            let notice = ui.notice.text().to_lowercase();
            assert!(notice.contains("restored"), "{notice:?}");
            assert!(!notice.contains("unknown"), "{notice:?}");
            assert!(notice.contains("ctrl+t"), "{notice:?}");
            assert_eq!(notice.contains("ctrl+y"), attached, "{notice:?}");
        }
    }

    #[test]
    fn failed_mouse_rollback_reports_unknown_and_keeps_the_last_known_mode() {
        let mut ui = test_ui(Vec::new());
        ui.mode = Mode::Attached;
        ui.mouse_capture = false;
        keys::tests::seed_mouse_press_for_reconciliation(&mut ui);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        let prefix = 5;
        let mut writer = RecordingTerminalWriter::with_actions([
            WriteAction::Accept(prefix),
            WriteAction::Fail,
            WriteAction::Fail,
        ]);
        let mut applied = true;

        process_test_event(
            Event::FocusGained,
            &mut ui,
            &mut terminal,
            &mut writer,
            &mut applied,
        )
        .expect("failed mouse rollback");

        assert_eq!(writer.output, MOUSE_DISABLE[..prefix]);
        assert_eq!(writer.attempts[2], MOUSE_ENABLE);
        assert!(applied);
        assert!(ui.mouse_capture);
        assert!(ui.mouse_press.is_none());
        let notice = ui.notice.text().to_lowercase();
        assert!(notice.contains("mouse"), "{notice:?}");
        assert!(notice.contains("unknown"), "{notice:?}");
        assert!(notice.contains("ctrl+t"), "{notice:?}");
        assert!(notice.contains("ctrl+y"), "{notice:?}");
        assert!(!notice.contains("restored"), "{notice:?}");
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
                // A user attach always resolves to Focus; a Wall join here would mean the
                // runner crossed wires, so fail loudly rather than skip.
                match result.expect("resolve event bridge attach") {
                    AttachOutcome::Focus { plan, .. } => {
                        break plan.expect("event bridge attach resolved a plan");
                    }
                    AttachOutcome::Wall { .. } => panic!("expected a focus attach"),
                }
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
                            age_ramp: ui.age_ramp,
                            tail: None,
                            wall: None,
                            wall_rects: &ui.wall_rects,
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

    /// A routed claude job is reported by the router as its SHORT id (the `~/.claude/jobs` key),
    /// which is never the full sessionId a claude row is keyed by. Matching the identity against
    /// `Session.id` alone left every routed claude spawn unselected AND disabled the cwd+time
    /// fallback, since a present identity takes that branch.
    #[test]
    fn routed_claude_short_id_selects_the_row_whose_full_session_id_differs() {
        let old = session(BackendKind::Claude, "old-session-uuid", 1_000, false);
        let mut ui = test_ui(vec![old.clone()]);
        assert!(ui.app.select_by_key(&(BackendKind::Claude, old.id.clone())));
        ui.pending_spawn = Some(pending(BackendKind::Claude, Some("abc12345"), 10_000));

        let mut routed = session(BackendKind::Claude, "routed-session-uuid", 10_100, false);
        routed.short_id = Some("abc12345".to_string());
        apply_listing(&mut ui, vec![old, routed]);

        assert_eq!(selected_id(&ui), Some("routed-session-uuid"));
        assert!(ui.pending_spawn.is_none());
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

    /// A routed spawn whose job id never resolved (the router's short-id poll can miss after the
    /// job is already created) is selected by cwd + creation time. The job was created WHILE the
    /// router ran, so its row can be many seconds older than the instant the decision landed;
    /// matched against that instant alone, with 2s of backward slack, it was never selected.
    #[test]
    fn routed_spawn_selects_a_row_created_while_the_router_was_still_running() {
        let old = session(BackendKind::Codex, "old", 1_000, false);
        let mut ui = test_ui(vec![old.clone()]);
        assert!(ui.app.select_by_key(&(BackendKind::Codex, old.id.clone())));
        ui.pending_spawn = Some(routed_pending(
            BackendKind::Codex,
            None,
            None,
            30_000,
            60_000,
        ));

        // Created 5s before the decision landed, 25s after the router was invoked.
        let routed = session(BackendKind::Codex, "routed", 55_000, false);
        apply_listing(&mut ui, vec![old, routed]);

        assert_eq!(selected_id(&ui), Some("routed"));
        assert!(ui.pending_spawn.is_none());
    }

    /// When the router cannot resolve a job id, its returned job name still identifies the row.
    /// Two concurrent jobs can share a backend and cwd, so exact title must beat time proximity.
    #[test]
    fn routed_spawn_without_an_id_prefers_the_exact_job_name_over_time_proximity() {
        let old = session(BackendKind::Codex, "old", 1_000, false);
        let mut ui = test_ui(vec![old.clone()]);
        assert!(ui.app.select_by_key(&(BackendKind::Codex, old.id.clone())));
        ui.pending_spawn = Some(routed_pending(
            BackendKind::Codex,
            None,
            Some("Add the requested viewer regression tests"),
            30_000,
            60_000,
        ));

        let mut nearer = session(BackendKind::Codex, "nearer", 59_999, false);
        nearer.title = "Another concurrent task".to_string();
        let mut exact_title = session(BackendKind::Codex, "exact-title", 45_000, false);
        exact_title.title = "Add the requested viewer regression tests".to_string();

        apply_listing(&mut ui, vec![old, nearer, exact_title]);

        assert_eq!(selected_id(&ui), Some("exact-title"));
        assert!(ui.pending_spawn.is_none());
    }

    /// The widened routed window must not become an open door: a row created before the router was
    /// ever invoked is some other session, and the routed spawn stays pending.
    #[test]
    fn routed_spawn_ignores_a_row_created_before_the_router_was_invoked() {
        let old = session(BackendKind::Codex, "old", 1_000, false);
        let mut ui = test_ui(vec![old.clone()]);
        assert!(ui.app.select_by_key(&(BackendKind::Codex, old.id.clone())));
        let pending = routed_pending(BackendKind::Codex, None, None, 30_000, 60_000);
        ui.pending_spawn = Some(pending.clone());

        let earlier = session(BackendKind::Codex, "earlier", 27_999, false);
        apply_listing(&mut ui, vec![old, earlier]);

        assert_eq!(selected_id(&ui), Some("old"));
        assert_eq!(ui.pending_spawn, Some(pending));
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
        let old = session(BackendKind::Codex, "old", 1_000, false);
        let mut ui = test_ui(vec![old.clone()]);
        assert!(ui.app.select_by_key(&(BackendKind::Codex, old.id.clone())));
        ui.pending_spawn = Some(pending(BackendKind::Codex, None, 10_000));

        let target = session(BackendKind::Codex, "target", 10_150, false);
        let farther = session(BackendKind::Codex, "farther", 11_000, false);
        let wrong_backend = session(BackendKind::Claude, "wrong", 10_001, false);
        apply_listing(&mut ui, vec![farther, wrong_backend, old, target]);

        assert_eq!(selected_id(&ui), Some("target"));
        assert!(ui.pending_spawn.is_none());
    }

    #[test]
    fn spawn_without_identity_waits_for_a_row_absent_before_submission() {
        let selected = session(BackendKind::Codex, "selected", 1_000, false);
        let preexisting = session(BackendKind::Codex, "preexisting", 9_999, false);
        let mut ui = test_ui(vec![selected.clone(), preexisting.clone()]);
        assert!(
            ui.app
                .select_by_key(&(BackendKind::Codex, selected.id.clone()))
        );
        let pending = pending_with_preexisting(
            BackendKind::Codex,
            None,
            10_000,
            &["selected", "preexisting"],
        );
        ui.pending_spawn = Some(pending.clone());

        apply_listing(&mut ui, vec![preexisting.clone(), selected.clone()]);

        assert_eq!(selected_id(&ui), Some("selected"));
        assert_eq!(ui.pending_spawn, Some(pending));

        let spawned = session(BackendKind::Codex, "spawned", 10_150, false);
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

    /// The symmetric proof for mouse reporting: `ratatui`'s panic hook restores raw mode and
    /// the alternate screen only, so without this guard a panic leaves any-motion tracking on
    /// and the shell fills with escape sequences on every mouse move.
    #[test]
    fn mouse_capture_is_disabled_during_panic_unwinding() {
        let bytes = TestArc::new(Mutex::new(Vec::new()));
        let writer = SharedWriter(TestArc::clone(&bytes));

        let result = catch_unwind(AssertUnwindSafe(|| {
            let _guard = MouseCaptureGuard::new(writer);
            panic!("simulate tui panic");
        }));

        assert!(result.is_err());
        let written = bytes.lock().unwrap().clone();
        for sequence in [
            b"\x1b[?1003l".as_slice(),
            b"\x1b[?1000l".as_slice(),
            b"\x1b[?1006l".as_slice(),
        ] {
            assert!(
                written
                    .windows(sequence.len())
                    .any(|bytes| bytes == sequence),
                "panic cleanup must disable mouse reporting ({})",
                String::from_utf8_lossy(sequence)
            );
        }
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
