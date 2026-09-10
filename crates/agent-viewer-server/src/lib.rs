//! HTTP companion server for agent-viewer.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

use agent_viewer_core::backend::all_backends;
use agent_viewer_core::group::project_root;
use agent_viewer_core::state::{apply_viewer_state, match_spawn};
use agent_viewer_core::{
    Backend, BackendKind, Capabilities, Error, PrBadgeColor, PrStatusCache, Session, TailEvent,
    ViewerDb, mark_dead_dirs,
};
use axum::Router;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{Json, Path as AxumPath, Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Error as WebSocketError;
use tokio_tungstenite::tungstenite::error::CapacityError;

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const SPAWN_ABANDON_MS: i64 = 10 * 60 * 1_000;
const DEFAULT_TAIL_EVENTS: usize = 12;
/// Longest `title` or `summary` the LIST response carries, in chars.
///
/// Codex uses a thread's whole first prompt as its title, so one row can carry hundreds of
/// KB of it: measured live on this box, `GET /sessions` was 56.9 MB over 11,968 rows, of
/// which 22.9 MB was titles and 26.7 MB summaries, and the largest single title was 288,687
/// chars. The phone decodes the whole body into its Java heap and dies with an allocation
/// failure, so the list bounds both fields here, at the view layer only - the snapshot and
/// `-core` keep the full text for every other surface.
const SESSION_TEXT_LIMIT: usize = 240;
const PULSE_WINDOW: Duration = Duration::from_secs(60 * 60);
const ANDROID_ORIGIN: &str = "https://appassets.androidplatform.net";
const TERMINAL_ROWS: u16 = 24;
const TERMINAL_COLS: u16 = 80;
const TERMINAL_MIN_ROWS: u64 = 4;
const TERMINAL_MAX_ROWS: u64 = 200;
const TERMINAL_MIN_COLS: u64 = 10;
const TERMINAL_MAX_COLS: u64 = 300;
const TERMINAL_INPUT_BYTES: usize = 4_096;
const TERMINAL_MESSAGE_BYTES: usize = TERMINAL_INPUT_BYTES * 6 + 512;
const TERMINAL_LIMIT: usize = 4;
/// How long an attached PTY outlives its socket. A phone that goes to sleep, loses the
/// tailnet, or is backgrounded must find its session exactly where it left it; only after
/// this window with nobody attached is the child killed.
const TERMINAL_GRACE: Duration = Duration::from_secs(10 * 60);
/// How long the owner thread waits for a socket event before it looks at the PTY tap again.
///
/// The owner owns the tap receiver outright and drains it here and inside the redraw snapshot
/// and NOWHERE else: any forwarding thread between the PTY and that snapshot can be holding a
/// chunk that the snapshot already contains, and would then print it a second time as live
/// output. Two std channels cannot be waited on together, so the price of owning the tap is
/// that output is picked up on this cadence. It is a channel wait, not a screen scrape: no
/// vt100 read, no serialization, nothing per tick unless bytes actually arrived.
const TERMINAL_OUTPUT_POLL: Duration = Duration::from_millis(10);
/// One drain pass is coalesced into frames of at most this size. A line-at-a-time child hands
/// the PTY reader one tiny read per line, and one WebSocket frame per line is pure overhead.
const TERMINAL_OUTPUT_FRAME_BYTES: usize = 64 * 1_024;
/// What the owner may hold for one socket before that client is a slow consumer: the queue is
/// capped in frames AND in bytes, because coalescing makes a frame count alone meaningless.
/// Past either cap the socket is closed with 1013 and the phone reconnects into a fresh
/// redraw, which loses nothing.
const TERMINAL_SINK_FRAMES: usize = 256;
const TERMINAL_SINK_BYTES: usize = 1_024 * 1_024;
/// How often the server pings an attached client.
const TERMINAL_PING_INTERVAL: Duration = Duration::from_secs(10);
/// No frame of any kind from a client for this long means the socket is gone even though no
/// FIN ever arrived (a phone that left the tailnet), so it is closed and the grace clock runs.
const TERMINAL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// The close code a replaced socket gets. The phone treats 4000 as final and never reconnects
/// on it, so this pair is wire contract: do not change either half.
const TERMINAL_REPLACED_CODE: u16 = 4000;
const TERMINAL_REPLACED_REASON: &str = "replaced";
/// The close code a client that stopped reading gets (RFC 6455 "try again later").
const TERMINAL_SLOW_CONSUMER_CODE: u16 = 1013;
const TERMINAL_SLOW_CONSUMER_REASON: &str = "slow consumer";

/// Why the owner let go of a socket. The socket task reads it once its frame queue ends, so
/// the close code survives a queue that was too full to carry one more frame.
const DISPOSITION_FINISHED: u8 = 0;
const DISPOSITION_REPLACED: u8 = 1;
const DISPOSITION_SLOW_CONSUMER: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TerminalKey {
    backend: BackendKind,
    id: String,
}

/// The channel the owner thread listens on. It is a std channel on purpose: the async socket
/// task only ever does a non-blocking unbounded `send`, and the owner thread needs a blocking
/// receive with a deadline for the disconnect grace window.
type TerminalEvents = std::sync::mpsc::Sender<OwnerEvent>;

/// One live attach: an owner thread, its PTY, and whether a socket is currently on it.
struct TerminalOwner {
    generation: u64,
    events: TerminalEvents,
    connected: bool,
    connection: u64,
}

#[derive(Default)]
struct TerminalHubState {
    owners: HashMap<TerminalKey, TerminalOwner>,
    reservations: HashMap<TerminalKey, u64>,
    next_generation: u64,
}

#[derive(Clone)]
struct TerminalHub {
    inner: Arc<Mutex<TerminalHubState>>,
    grace: Duration,
}

impl TerminalHub {
    fn new(grace: Duration) -> Self {
        Self {
            inner: Arc::default(),
            grace,
        }
    }
}

/// A client's handle on an owner thread. `connection` distinguishes successive sockets on the
/// same PTY so a late detach from an old socket cannot expire a newly reconnected one.
#[derive(Clone)]
struct TerminalLease {
    key: TerminalKey,
    generation: u64,
    connection: u64,
    events: TerminalEvents,
}

/// Connecting either adopts the PTY still sitting in its grace window or spawns a new one.
enum TerminalAcquire {
    Fresh(TerminalReservation),
    Reattach(TerminalLease),
}

struct TerminalReservation {
    hub: TerminalHub,
    key: TerminalKey,
    generation: u64,
    armed: bool,
}

impl Drop for TerminalReservation {
    fn drop(&mut self) {
        if self.armed {
            self.hub.cancel_reservation(&self.key, self.generation);
        }
    }
}

/// A frame bound for the attached client: JSON control as text, raw PTY bytes as binary.
enum ServerFrame {
    Text(String),
    Binary(Vec<u8>),
}

impl ServerFrame {
    fn len(&self) -> usize {
        match self {
            ServerFrame::Text(text) => text.len(),
            ServerFrame::Binary(bytes) => bytes.len(),
        }
    }
}

/// The owner's end of one socket. Bounded on purpose: a phone that stops reading must not let
/// a chatty child grow this queue without limit. `queued` is the byte weight the socket task
/// has not written yet — the owner adds to it, the socket task subtracts once a frame is on
/// the wire — because a frame count alone says nothing once frames are coalesced.
struct TerminalSink {
    frames: mpsc::Sender<ServerFrame>,
    queued: Arc<std::sync::atomic::AtomicUsize>,
    disposition: Arc<std::sync::atomic::AtomicU8>,
}

/// Everything the owner thread reacts to. All of it comes from a socket task; PTY output and
/// the child's EOF are read straight off the tap by the owner itself.
///
/// Every variant carries the connection it came from. A socket that has been replaced keeps
/// its lease and can still send, so the owner compares that id against the connection it is
/// serving and ignores anything stale: a replaced client cannot type into the child, resize
/// it, close it, or open its grace window.
enum OwnerEvent {
    Attach {
        connection: u64,
        sink: TerminalSink,
    },
    Detach {
        connection: u64,
    },
    Input {
        connection: u64,
        bytes: Vec<u8>,
    },
    Resize {
        connection: u64,
        rows: u16,
        cols: u16,
    },
    Close {
        connection: u64,
        complete: oneshot::Sender<()>,
    },
}

struct TerminalOwnerContext {
    hub: Weak<Mutex<TerminalHubState>>,
    key: TerminalKey,
    generation: u64,
    grace: Duration,
}

/// Ping cadence and idle deadline for an attached socket, configurable so a test can prove the
/// liveness rule in a second instead of half a minute.
#[derive(Debug, Clone, Copy)]
struct TerminalLiveness {
    ping: Duration,
    idle: Duration,
}

#[derive(Debug, Serialize)]
struct TerminalHello {
    #[serde(rename = "type")]
    kind: &'static str,
    cols: u16,
    rows: u16,
    running: bool,
}

#[derive(Debug, Serialize)]
struct TerminalExit {
    #[serde(rename = "type")]
    kind: &'static str,
    status: Option<i32>,
}

#[derive(Debug, Serialize)]
struct TerminalError<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    message: &'a str,
}

#[derive(Debug, Deserialize)]
struct TerminalQuery {
    backend: BackendKind,
}

/// Text frames are control only. Keystrokes are binary and are never inspected.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum TerminalClientMessage {
    Resize { rows: u64, cols: u64 },
    Close,
}

#[derive(Debug, Clone, Serialize)]
struct SessionView {
    #[serde(flatten)]
    session: Session,
    project_root: PathBuf,
    /// Aggregated live GitHub color for `pr_refs`, same buckets the TUI badge uses.
    pr_color: PrBadgeColor,
}

impl SessionView {
    fn new(session: Session) -> Self {
        let project_root = project_root(&session.cwd);
        Self {
            session,
            project_root,
            pr_color: PrBadgeColor::Default,
        }
    }

    /// The list copy of a row: same JSON shape and same fields, with `title` and `summary`
    /// bounded to `SESSION_TEXT_LIMIT`. Paths are left whole; they are short.
    fn bounded(&self) -> Self {
        let mut view = self.clone();
        view.session.title = bound_text(&view.session.title);
        view.session.summary = bound_text(&view.session.summary);
        view
    }
}

/// Cut `text` to `SESSION_TEXT_LIMIT` CHARS on a char boundary, marking a cut with a single
/// ellipsis. Counting chars, not bytes, is what keeps a CJK or emoji title from being sliced
/// mid-codepoint into invalid UTF-8.
fn bound_text(text: &str) -> String {
    let mut chars = text.chars();
    let bounded: String = chars.by_ref().take(SESSION_TEXT_LIMIT).collect();
    if chars.next().is_some() {
        format!("{bounded}\u{2026}")
    } else {
        bounded
    }
}

#[derive(Clone)]
struct BackendPool {
    entries: Arc<HashMap<BackendKind, SharedBackend>>,
}

type SharedBackend = Arc<Mutex<Box<dyn Backend>>>;

impl BackendPool {
    fn new(backends: Vec<Box<dyn Backend>>) -> Self {
        let entries = backends
            .into_iter()
            .map(|backend| (backend.kind(), Arc::new(Mutex::new(backend))))
            .collect();
        Self {
            entries: Arc::new(entries),
        }
    }

    fn get(&self, kind: BackendKind) -> Option<SharedBackend> {
        self.entries.get(&kind).cloned()
    }
}

impl TerminalHub {
    /// A connect either adopts the PTY still in its grace window (same child, same screen) or
    /// reserves the key for a fresh one.
    ///
    /// A key that still has a live socket on it is a TAKEOVER, not a conflict: a phone's
    /// `WebSocket.close` is asynchronous and an unclean drop never sends a FIN at all, so the
    /// old socket is routinely still `connected` when the new one arrives. Refusing the new
    /// connection would strand the user behind a 409 for as long as the stale socket lasts.
    /// The new connection wins, the old one is closed with 4000, and the bumped connection id
    /// is what stops the loser from speaking for this PTY afterwards.
    fn acquire(&self, key: TerminalKey) -> ApiResult<TerminalAcquire> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| ApiError::internal("terminal hub lock failed"))?;
        if state.reservations.contains_key(&key) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "terminal is already connecting",
            ));
        }
        if let Some(owner) = state.owners.get_mut(&key) {
            owner.connected = true;
            owner.connection = owner.connection.wrapping_add(1).max(1);
            return Ok(TerminalAcquire::Reattach(TerminalLease {
                key,
                generation: owner.generation,
                connection: owner.connection,
                events: owner.events.clone(),
            }));
        }
        if state.owners.len() + state.reservations.len() >= TERMINAL_LIMIT {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "terminal capacity reached",
            ));
        }
        state.next_generation = state.next_generation.wrapping_add(1).max(1);
        let generation = state.next_generation;
        state.reservations.insert(key.clone(), generation);
        drop(state);
        Ok(TerminalAcquire::Fresh(TerminalReservation {
            hub: self.clone(),
            key,
            generation,
            armed: true,
        }))
    }

    fn finish_reservation(
        &self,
        reservation: &mut TerminalReservation,
        events: TerminalEvents,
    ) -> ApiResult<TerminalLease> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| ApiError::internal("terminal hub lock failed"))?;
        if state.reservations.get(&reservation.key) != Some(&reservation.generation) {
            return Err(ApiError::internal("terminal reservation was lost"));
        }
        state.reservations.remove(&reservation.key);
        state.owners.insert(
            reservation.key.clone(),
            TerminalOwner {
                generation: reservation.generation,
                events: events.clone(),
                connected: true,
                connection: 1,
            },
        );
        reservation.armed = false;
        Ok(TerminalLease {
            key: reservation.key.clone(),
            generation: reservation.generation,
            connection: 1,
            events,
        })
    }

    fn cancel_reservation(&self, key: &TerminalKey, generation: u64) {
        if let Ok(mut state) = self.inner.lock()
            && state.reservations.get(key) == Some(&generation)
        {
            state.reservations.remove(key);
        }
    }

    fn downgrade(&self) -> Weak<Mutex<TerminalHubState>> {
        Arc::downgrade(&self.inner)
    }
}

fn remove_terminal_owner(hub: &Weak<Mutex<TerminalHubState>>, key: &TerminalKey, generation: u64) {
    let Some(hub) = hub.upgrade() else {
        return;
    };
    if let Ok(mut state) = hub.lock()
        && state
            .owners
            .get(key)
            .is_some_and(|owner| owner.generation == generation)
    {
        state.owners.remove(key);
    }
}

/// The socket for this lease is gone. Clearing the flag opens the grace window; the owner
/// thread reads the same flag when the window closes, so the two cannot race past each other.
fn detach_terminal(hub: &TerminalHub, lease: &TerminalLease) {
    mark_terminal_detached(
        &hub.downgrade(),
        &lease.key,
        lease.generation,
        lease.connection,
    );
    let _ = lease.events.send(OwnerEvent::Detach {
        connection: lease.connection,
    });
}

/// Clear the hub's `connected` flag for exactly this connection. The owner calls it directly
/// when it drops a slow consumer, because a socket wedged behind an unread TCP window may not
/// get to say so for a long time — and a stale `connected` would tell `terminal_grace_expired`
/// somebody is still attached and keep the child alive forever.
fn mark_terminal_detached(
    hub: &Weak<Mutex<TerminalHubState>>,
    key: &TerminalKey,
    generation: u64,
    connection: u64,
) {
    let Some(hub) = hub.upgrade() else {
        return;
    };
    if let Ok(mut state) = hub.lock()
        && let Some(owner) = state.owners.get_mut(key)
        && owner.generation == generation
        && owner.connection == connection
    {
        owner.connected = false;
    }
}

/// Kill now, no grace. The owner thread drops its own hub entry as it dies; only a dead owner
/// needs the entry removed here.
fn close_terminal_owner(hub: &TerminalHub, lease: &TerminalLease) {
    let (complete, _abandoned) = oneshot::channel();
    if lease
        .events
        .send(OwnerEvent::Close {
            connection: lease.connection,
            complete,
        })
        .is_err()
    {
        remove_terminal_owner(&hub.downgrade(), &lease.key, lease.generation);
    }
}

/// Under the hub lock: has the grace window closed with nobody attached? Drops our own entry
/// when it has, so a connect arriving after this point spawns a fresh PTY instead of adopting
/// one that is about to be killed.
fn terminal_grace_expired(
    hub: &Weak<Mutex<TerminalHubState>>,
    key: &TerminalKey,
    generation: u64,
) -> bool {
    let Some(hub) = hub.upgrade() else {
        return true;
    };
    let Ok(mut state) = hub.lock() else {
        return true;
    };
    let reconnected = state
        .owners
        .get(key)
        .is_some_and(|owner| owner.generation == generation && owner.connected);
    if reconnected {
        return false;
    }
    state
        .owners
        .retain(|owned, owner| owned != key || owner.generation != generation);
    true
}

#[derive(Clone)]
struct AppState {
    snapshot: Arc<RwLock<Vec<SessionView>>>,
    backends: BackendPool,
    router_available: bool,
    terminals: TerminalHub,
    terminal_liveness: TerminalLiveness,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "session not found")
    }

    fn unsupported(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_IMPLEMENTED, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug, Serialize)]
struct StatusBody {
    status: &'static str,
}

#[derive(Debug, Default, Deserialize)]
struct TailQuery {
    n: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct SpawnRequest {
    provider: String,
    dir: String,
    task: String,
    model: Option<String>,
}

#[derive(Debug, Serialize)]
struct SpawnResponse {
    backend: BackendKind,
    session_id: Option<String>,
    pid: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct RenameRequest {
    name: String,
}

#[derive(Debug, Clone, Copy)]
enum MutationKind {
    Stop,
    Hide,
    Unhide,
}

/// Constructs the production server router and starts its dedicated listing thread.
pub fn router() -> Router {
    let snapshot = Arc::new(RwLock::new(Vec::new()));
    start_refresh_thread(all_backends(), Arc::clone(&snapshot));
    build_router(AppState {
        snapshot,
        backends: BackendPool::new(all_backends()),
        router_available: agent_viewer_core::router::available(),
        terminals: TerminalHub::new(TERMINAL_GRACE),
        terminal_liveness: TerminalLiveness {
            ping: TERMINAL_PING_INTERVAL,
            idle: TERMINAL_IDLE_TIMEOUT,
        },
    })
}

/// Constructs a deterministic test router whose initial backend snapshot is ready on return.
pub fn router_with_backends(backends: Vec<Box<dyn Backend>>) -> Router {
    router_with_backends_and_grace(backends, TERMINAL_GRACE)
}

/// The same test router with an explicit post-disconnect grace window, so a test can prove
/// both halves of the rule (a reconnect inside it adopts the same child, expiry kills it)
/// without waiting out the production ten minutes.
pub fn router_with_backends_and_grace(
    backends: Vec<Box<dyn Backend>>,
    terminal_grace: Duration,
) -> Router {
    router_with_backends_and_terminal_timing(
        backends,
        terminal_grace,
        TERMINAL_PING_INTERVAL,
        TERMINAL_IDLE_TIMEOUT,
    )
}

/// The same test router with every terminal timer explicit, so the liveness rule (ping every
/// `ping_interval`, close a socket that has answered nothing for `idle_timeout`) is provable
/// in a second instead of half a minute.
pub fn router_with_backends_and_terminal_timing(
    mut backends: Vec<Box<dyn Backend>>,
    terminal_grace: Duration,
    ping_interval: Duration,
    idle_timeout: Duration,
) -> Router {
    let mut last = vec![Vec::new(); backends.len()];
    let snapshot = Arc::new(RwLock::new(refresh_snapshot(
        &mut backends,
        &mut last,
        None,
        &mut PrStatusCache::new(),
        0,
    )));
    build_router(AppState {
        snapshot,
        backends: BackendPool::new(backends),
        router_available: false,
        terminals: TerminalHub::new(terminal_grace),
        terminal_liveness: TerminalLiveness {
            ping: ping_interval,
            idle: idle_timeout,
        },
    })
}

fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/sessions", get(list_sessions).post(spawn_session))
        .route("/sessions/{id}/tail", get(session_tail))
        .route("/sessions/{id}/caps", get(session_capabilities))
        .route("/sessions/{id}/pulse", get(session_pulse))
        .route("/sessions/{id}/terminal", get(session_terminal))
        .route("/sessions/{id}/stop", post(stop_session))
        .route("/sessions/{id}/hide", post(hide_session))
        .route("/sessions/{id}/unhide", post(unhide_session))
        .route("/sessions/{id}", patch(rename_session))
        .route("/models", get(models))
        .route("/health", get(health))
        .fallback(not_found)
        .layer(middleware::from_fn(require_tailscale_user))
        .with_state(state)
}

fn start_refresh_thread(
    mut backends: Vec<Box<dyn Backend>>,
    snapshot: Arc<RwLock<Vec<SessionView>>>,
) {
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
    let worker = std::thread::Builder::new()
        .name("agent_viewer_server_refresh".to_string())
        .spawn(move || {
            let db = ViewerDb::open_default().ok();
            let mut last = vec![Vec::new(); backends.len()];
            let mut pr_status = PrStatusCache::new();
            let mut ready_tx = Some(ready_tx);
            loop {
                let next = refresh_snapshot(
                    &mut backends,
                    &mut last,
                    db.as_ref(),
                    &mut pr_status,
                    agent_viewer_core::spawn::now_ms(),
                );
                match snapshot.write() {
                    Ok(mut current) => *current = next,
                    Err(poisoned) => *poisoned.into_inner() = next,
                }
                if let Some(ready_tx) = ready_tx.take() {
                    let _ = ready_tx.send(());
                }
                std::thread::sleep(REFRESH_INTERVAL);
            }
        });
    if worker.is_ok() {
        let _ = ready_rx.recv();
    }
}

fn refresh_snapshot(
    backends: &mut [Box<dyn Backend>],
    last: &mut [Vec<Session>],
    db: Option<&ViewerDb>,
    pr_status: &mut PrStatusCache,
    now_ms: i64,
) -> Vec<SessionView> {
    let mut sessions = Vec::new();
    for (index, backend) in backends.iter_mut().enumerate() {
        if let Ok(fresh) = backend.list() {
            last[index] = fresh;
        }
        sessions.extend(last[index].iter().cloned());
    }

    if let Some(db) = db {
        apply_overlay(db, &mut sessions);
        let live: HashSet<(BackendKind, String)> = sessions
            .iter()
            .map(|session| (session.backend, session.id.clone()))
            .collect();
        let _ = db.prune_resolved_missing(&live);
    }
    mark_dead_dirs(&mut sessions);
    let mut views: Vec<SessionView> = sessions.into_iter().map(SessionView::new).collect();
    paint_pr_colors(&mut views, pr_status, now_ms);
    views
}

/// Stamp each row with the cached GitHub badge color and kick off fetches for visible PRs
/// the cache does not already hold. Hidden, companion, and subagent rows are never fetched:
/// the phone never lists them.
fn paint_pr_colors(views: &mut [SessionView], pr_status: &mut PrStatusCache, now_ms: i64) {
    pr_status.poll(now_ms);
    for view in views.iter() {
        if is_visible(&view.session) {
            pr_status.request_refs(&view.session.pr_refs, now_ms);
        }
    }
    for view in views.iter_mut() {
        view.pr_color = pr_status.badge_color(&view.session.pr_refs);
    }
}

fn apply_overlay(db: &ViewerDb, sessions: &mut [Session]) {
    if let Ok(state) = db.viewer_state() {
        apply_viewer_state(sessions, &state);
    }
    if let Ok(records) = db.unresolved_spawns() {
        let now = agent_viewer_core::spawn::now_ms();
        for record in records {
            if let Some(id) = match_spawn(&record, sessions) {
                let _ = db.resolve_spawn(record.rowid, &id);
            } else if now.saturating_sub(record.spawned_at_ms) > SPAWN_ABANDON_MS {
                let _ = db.delete_spawn(record.rowid);
            }
        }
    }
}

async fn require_tailscale_user(request: Request, next: Next) -> Response {
    if !request.headers().contains_key("Tailscale-User-Login") {
        return ApiError::new(StatusCode::UNAUTHORIZED, "missing Tailscale user").into_response();
    }
    if matches!(*request.method(), Method::POST | Method::PATCH)
        && !request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .is_some_and(|media_type| media_type.trim() == "application/json")
            })
    {
        return ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be application/json",
        )
        .into_response();
    }
    next.run(request).await
}

async fn health() -> Json<StatusBody> {
    Json(StatusBody { status: "ok" })
}

/// The list is the VISIBLE fleet and nothing else: hidden, companion, and subagent rows are
/// never served, and there is no query parameter that asks for them.
///
/// Measured live on this box, only 33 of 11,968 rows are visible; the other 11,935 are rows
/// the phone has no surface for and would only decode on its way to an OOM.
async fn list_sessions(State(state): State<AppState>) -> Json<Vec<SessionView>> {
    let snapshot = match state.snapshot.read() {
        Ok(snapshot) => snapshot,
        Err(poisoned) => poisoned.into_inner(),
    };
    let sessions = snapshot
        .iter()
        .filter(|view| is_visible(&view.session))
        .map(SessionView::bounded)
        .collect();
    Json(sessions)
}

/// A row a person actually looks at on the phone: not archived, not a second view of a
/// session already listed, not a child thread of one.
fn is_visible(session: &Session) -> bool {
    !session.hidden && !session.companion && !session.subagent
}

async fn session_tail(
    State(state): State<AppState>,
    path: Result<AxumPath<String>, PathRejection>,
    query: Result<Query<TailQuery>, QueryRejection>,
) -> ApiResult<Json<Vec<TailEvent>>> {
    let id = path_value(path)?;
    let Query(query) = query.map_err(|_| ApiError::bad_request("invalid tail query"))?;
    let max_events = query.n.unwrap_or(DEFAULT_TAIL_EVENTS);
    let session = snapshot_session(&state, &id)?;
    let events = with_backend(state.backends, session.backend, move |backend| {
        backend
            .tail(&session, max_events)
            .map_err(|error| core_error("tail", error))
    })
    .await?;
    Ok(Json(events))
}

async fn session_capabilities(
    State(state): State<AppState>,
    path: Result<AxumPath<String>, PathRejection>,
) -> ApiResult<Json<Capabilities>> {
    let id = path_value(path)?;
    let session = snapshot_session(&state, &id)?;
    let capabilities = with_backend(state.backends, session.backend, move |backend| {
        Ok(backend.capabilities_for(&session))
    })
    .await?;
    Ok(Json(capabilities))
}

async fn session_pulse(
    State(state): State<AppState>,
    path: Result<AxumPath<String>, PathRejection>,
) -> ApiResult<Json<Vec<i64>>> {
    let id = path_value(path)?;
    let session = snapshot_session(&state, &id)?;
    let activity = with_backend(state.backends, session.backend, move |backend| {
        backend
            .turn_activity(&session, PULSE_WINDOW)
            .map_err(|error| core_error("pulse", error))
    })
    .await?;
    Ok(Json(activity))
}

async fn session_terminal(
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    path: Result<AxumPath<String>, PathRejection>,
    query: Result<Query<TerminalQuery>, QueryRejection>,
) -> ApiResult<Response> {
    if headers
        .get(header::ORIGIN)
        .and_then(|origin| origin.to_str().ok())
        != Some(ANDROID_ORIGIN)
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "terminal origin is not allowed",
        ));
    }
    let id = path_value(path)?;
    let Query(query) = query.map_err(|_| ApiError::bad_request("invalid terminal query"))?;
    let session = snapshot_session_for_backend(&state, &id, query.backend)?;
    let key = TerminalKey {
        backend: query.backend,
        id,
    };
    let (lease, reattached) = match state.terminals.acquire(key)? {
        TerminalAcquire::Reattach(lease) => (lease, true),
        TerminalAcquire::Fresh(reservation) => (
            create_terminal_owner(&state, session, reservation).await?,
            false,
        ),
    };

    let failed_hub = state.terminals.clone();
    let failed_lease = lease.clone();
    let socket_hub = state.terminals.clone();
    let liveness = state.terminal_liveness;
    Ok(ws
        .max_message_size(TERMINAL_MESSAGE_BYTES)
        .max_frame_size(TERMINAL_MESSAGE_BYTES)
        .on_failed_upgrade(move |_| {
            // A reattach that never completed leaves the existing session in its grace
            // window; a fresh one has nobody to hand the child to, so it goes now.
            if reattached {
                detach_terminal(&failed_hub, &failed_lease);
            } else {
                close_terminal_owner(&failed_hub, &failed_lease);
            }
        })
        .on_upgrade(move |socket| terminal_socket(socket, socket_hub, lease, liveness)))
}

async fn create_terminal_owner(
    state: &AppState,
    session: Session,
    mut reservation: TerminalReservation,
) -> ApiResult<TerminalLease> {
    let kind = reservation.key.backend;
    let command = with_backend(state.backends.clone(), kind, move |backend| {
        if !backend.capabilities_for(&session).attach {
            return Err(ApiError::unsupported(format!(
                "{} does not support attach",
                kind.name()
            )));
        }
        backend
            .attach_command(&session)
            .map_err(|refusal| ApiError::unsupported(refusal.to_string()))
    })
    .await?;

    // The tap receiver goes STRAIGHT to the owner thread. Nothing forwards for it: a
    // forwarding thread can be holding a chunk that the redraw snapshot already contains,
    // and that chunk would then be printed a second time as live output.
    let (tap, tapped) = std::sync::mpsc::channel::<Vec<u8>>();
    let mut spec =
        agent_viewer_core::pty::spec_from_command(&command, TERMINAL_ROWS, TERMINAL_COLS);
    spec.output_tap = Some(tap);
    let (events_tx, events_rx) = std::sync::mpsc::channel::<OwnerEvent>();
    let (startup_tx, startup_rx) = oneshot::channel();
    let (activate_tx, activate_rx) = oneshot::channel();
    spawn_terminal_owner_thread(
        spec,
        events_rx,
        tapped,
        startup_tx,
        activate_rx,
        TerminalOwnerContext {
            hub: state.terminals.downgrade(),
            key: reservation.key.clone(),
            generation: reservation.generation,
            grace: state.terminals.grace,
        },
    )?;
    startup_rx
        .await
        .map_err(|_| ApiError::internal("terminal attach worker failed"))?
        .map_err(|error| ApiError::internal(format!("terminal attach failed: {error}")))?;
    let lease = state
        .terminals
        .finish_reservation(&mut reservation, events_tx)?;
    if activate_tx.send(()).is_err() {
        remove_terminal_owner(&state.terminals.downgrade(), &lease.key, lease.generation);
        return Err(ApiError::internal("terminal attach worker stopped"));
    }
    Ok(lease)
}

fn spawn_terminal_owner_thread(
    spec: agent_viewer_core::pty::PtySpec,
    events: std::sync::mpsc::Receiver<OwnerEvent>,
    tapped: std::sync::mpsc::Receiver<Vec<u8>>,
    startup: oneshot::Sender<Result<(), String>>,
    activate: oneshot::Receiver<()>,
    context: TerminalOwnerContext,
) -> ApiResult<()> {
    std::thread::Builder::new()
        .name(format!("agent_viewer_terminal_{}", context.key.id))
        .spawn(move || {
            let mut pty = match agent_viewer_core::pty::PtySession::spawn(spec) {
                Ok(pty) => pty,
                Err(error) => {
                    let _ = startup.send(Err(bounded_terminal_error(&error.to_string())));
                    return;
                }
            };
            if startup.send(Ok(())).is_err() || activate.blocking_recv().is_err() {
                pty.kill();
                remove_terminal_owner(&context.hub, &context.key, context.generation);
                return;
            }
            run_terminal_owner(pty, &events, &tapped, &context);
        })
        .map(|_| ())
        .map_err(|error| ApiError::internal(format!("terminal owner thread failed: {error}")))
}

/// The owner thread's view of the socket it is serving.
struct TerminalOwnerState {
    sink: Option<TerminalSink>,
    connection: u64,
    /// When the child must be killed for want of a client. `None` means somebody is attached.
    deadline: Option<std::time::Instant>,
}

impl TerminalOwnerState {
    /// Queue one frame for the attached socket. A full queue is a client that has stopped
    /// reading: it is released with 1013 rather than allowed to grow without bound. Nothing is
    /// lost by that — the phone reconnects and the redraw repaints the whole screen.
    fn queue(&mut self, context: &TerminalOwnerContext, frame: ServerFrame) {
        let Some(active) = self.sink.as_ref() else {
            return;
        };
        let weight = frame.len();
        let queued = active.queued.load(std::sync::atomic::Ordering::SeqCst);
        let overflowed = if queued.saturating_add(weight) > TERMINAL_SINK_BYTES {
            true
        } else {
            active
                .queued
                .fetch_add(weight, std::sync::atomic::Ordering::SeqCst);
            match active.frames.try_send(frame) {
                Ok(()) => return,
                Err(mpsc::error::TrySendError::Full(_)) => true,
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            }
        };
        if overflowed {
            active.disposition.store(
                DISPOSITION_SLOW_CONSUMER,
                std::sync::atomic::Ordering::SeqCst,
            );
        }
        self.release(context);
    }

    /// Nobody is on this PTY any more: drop the sink (which is what wakes the socket task to
    /// send its close frame) and start the grace clock.
    fn release(&mut self, context: &TerminalOwnerContext) {
        self.sink = None;
        mark_terminal_detached(
            &context.hub,
            &context.key,
            context.generation,
            self.connection,
        );
        self.deadline = Some(std::time::Instant::now() + context.grace);
    }

    /// Forward everything the PTY has published since the last pass, coalesced: a shell
    /// printing a line at a time hands the reader one tiny read per line, and one WebSocket
    /// frame per line is all overhead. Returns true once the tap is closed, which is the PTY
    /// reader hitting EOF: the child's terminal is gone.
    fn forward_output(
        &mut self,
        context: &TerminalOwnerContext,
        tapped: &std::sync::mpsc::Receiver<Vec<u8>>,
    ) -> bool {
        let mut batch: Vec<u8> = Vec::new();
        let closed = loop {
            match tapped.try_recv() {
                Ok(chunk) => {
                    batch.extend_from_slice(&chunk);
                    if batch.len() >= TERMINAL_OUTPUT_FRAME_BYTES {
                        self.queue(context, ServerFrame::Binary(std::mem::take(&mut batch)));
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break false,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break true,
            }
        };
        if !batch.is_empty() {
            self.queue(context, ServerFrame::Binary(batch));
        }
        closed
    }
}

/// The owner thread. It owns the PTY outright — every read of the screen, every write to the
/// child, and the receiving end of the PTY output tap all live here — and the socket task only
/// ever exchanges events with it.
fn run_terminal_owner(
    mut pty: agent_viewer_core::pty::PtySession,
    events: &std::sync::mpsc::Receiver<OwnerEvent>,
    tapped: &std::sync::mpsc::Receiver<Vec<u8>>,
    context: &TerminalOwnerContext,
) {
    // Nobody is attached until the upgraded socket says so, so the grace clock starts now: an
    // upgrade that never lands must not strand a real CLI process forever.
    let mut state = TerminalOwnerState {
        sink: None,
        connection: 0,
        deadline: Some(std::time::Instant::now() + context.grace),
    };

    loop {
        if state.forward_output(context, tapped) {
            finish_exited_terminal(&mut pty, &mut state, context);
            return;
        }

        // Checked EVERY iteration, not only in the timeout arm below. Output no longer travels
        // on the event channel at all — it comes straight off the tap above — so a child that
        // never stops printing can no longer keep this loop from reaching its deadline, and
        // the deadline is read on every pass whatever woke it.
        if let Some(at) = state.deadline
            && std::time::Instant::now() >= at
        {
            if terminal_grace_expired(&context.hub, &context.key, context.generation) {
                pty.kill();
                return;
            }
            // Somebody reconnected while we were waiting; their Attach is already queued.
            state.deadline = None;
        }

        let wait = match state.deadline {
            Some(at) => at
                .saturating_duration_since(std::time::Instant::now())
                .min(TERMINAL_OUTPUT_POLL),
            None => TERMINAL_OUTPUT_POLL,
        };
        let event = match events.recv_timeout(wait) {
            Ok(event) => event,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                pty.kill();
                remove_terminal_owner(&context.hub, &context.key, context.generation);
                return;
            }
        };

        match event {
            OwnerEvent::Attach {
                connection: attached,
                sink: next,
            } => {
                // Takeover. The socket that was here is told it was replaced, and from this
                // point everything it sends carries a stale connection id and is ignored.
                if let Some(previous) = state.sink.take() {
                    previous
                        .disposition
                        .store(DISPOSITION_REPLACED, std::sync::atomic::Ordering::SeqCst);
                }
                state.connection = attached;
                state.deadline = None;
                let running = !pty.is_exited();
                let (rows, cols) = pty.with_screen(|screen| screen.size());
                state.sink = Some(next);
                state.queue(
                    context,
                    ServerFrame::Text(terminal_hello_frame(cols, rows, running)),
                );

                let mut exited = false;
                let redraw = pty.with_screen(|screen| {
                    // Under the parser lock, which the PTY reader also holds while it applies
                    // a chunk AND publishes it on the tap. This thread owns that receiver, so
                    // draining it here drops exactly the chunks this snapshot already
                    // contains: nothing is in flight anywhere else, and no chunk can land in
                    // both the redraw and the live stream and print twice.
                    loop {
                        match tapped.try_recv() {
                            Ok(_) => {}
                            Err(std::sync::mpsc::TryRecvError::Empty) => break,
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                exited = true;
                                break;
                            }
                        }
                    }
                    // state_formatted() is contents_formatted() plus the input modes — see
                    // vendor/vt100/src/screen.rs — so appending contents_formatted() to it
                    // paints the whole screen a second time, which is the same double print
                    // this drain exists to prevent.
                    screen.state_formatted()
                });
                state.queue(context, ServerFrame::Binary(redraw));
                if exited {
                    finish_exited_terminal(&mut pty, &mut state, context);
                    return;
                }
            }
            OwnerEvent::Detach {
                connection: detached,
            } => {
                if detached == state.connection {
                    state.sink = None;
                    state.deadline = Some(std::time::Instant::now() + context.grace);
                }
            }
            OwnerEvent::Input { connection, bytes } => {
                if connection != state.connection {
                    continue;
                }
                if let Err(error) = pty.write_input(&bytes) {
                    state.queue(
                        context,
                        ServerFrame::Text(terminal_error_frame(&error.to_string())),
                    );
                }
            }
            OwnerEvent::Resize {
                connection,
                rows,
                cols,
            } => {
                if connection != state.connection {
                    continue;
                }
                if let Err(error) = pty.resize(rows, cols) {
                    state.queue(
                        context,
                        ServerFrame::Text(terminal_error_frame(&error.to_string())),
                    );
                }
            }
            OwnerEvent::Close {
                connection,
                complete,
            } => {
                if connection != state.connection {
                    // A replaced socket cannot kill the child the new one is using. Dropping
                    // the sender is the answer its own task is waiting for.
                    continue;
                }
                pty.kill();
                remove_terminal_owner(&context.hub, &context.key, context.generation);
                let _ = complete.send(());
                return;
            }
        }
    }
}

/// The PTY reader hit EOF: the child's terminal is gone. Tell whoever is attached, then go.
fn finish_exited_terminal(
    pty: &mut agent_viewer_core::pty::PtySession,
    state: &mut TerminalOwnerState,
    context: &TerminalOwnerContext,
) {
    pty.kill();
    state.queue(context, ServerFrame::Text(terminal_exit_frame()));
    remove_terminal_owner(&context.hub, &context.key, context.generation);
    // Dropping the sink closes the socket, but only after it has forwarded the exit frame
    // already queued ahead of the close.
}

/// How the socket task finished, and therefore what the PTY behind it is owed.
enum TerminalOutcome {
    /// The socket is gone but the child is not: open the grace window.
    Detach,
    /// The owner thread is already tearing itself down, or this socket has been replaced and
    /// no longer speaks for the child.
    Done,
}

async fn terminal_socket(
    mut socket: WebSocket,
    hub: TerminalHub,
    lease: TerminalLease,
    liveness: TerminalLiveness,
) {
    let (frames_tx, mut frames) = mpsc::channel::<ServerFrame>(TERMINAL_SINK_FRAMES);
    let disposition = Arc::new(std::sync::atomic::AtomicU8::new(DISPOSITION_FINISHED));
    let queued = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    if lease
        .events
        .send(OwnerEvent::Attach {
            connection: lease.connection,
            sink: TerminalSink {
                frames: frames_tx,
                queued: Arc::clone(&queued),
                disposition: Arc::clone(&disposition),
            },
        })
        .is_err()
    {
        let _ = send_terminal_error(&mut socket, "terminal is no longer running").await;
        let _ = socket.send(Message::Close(None)).await;
        remove_terminal_owner(&hub.downgrade(), &lease.key, lease.generation);
        return;
    }

    let mut outcome = TerminalOutcome::Detach;
    // A phone that loses the tailnet or is killed sends no FIN at all, so without this the CLI
    // would stay attached long past its grace window with nothing to end it. Any frame from
    // the client — pong, binary, text — counts as alive.
    let mut last_frame = tokio::time::Instant::now();
    let mut ping =
        tokio::time::interval_at(tokio::time::Instant::now() + liveness.ping, liveness.ping);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ping.tick() => {
                if last_frame.elapsed() >= liveness.idle {
                    break;
                }
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
            frame = frames.recv() => {
                match frame {
                    Some(ServerFrame::Text(text)) => {
                        let weight = text.len();
                        let sent = socket.send(Message::Text(text.into())).await;
                        queued.fetch_sub(weight, std::sync::atomic::Ordering::SeqCst);
                        if sent.is_err() {
                            break;
                        }
                    }
                    Some(ServerFrame::Binary(bytes)) => {
                        let weight = bytes.len();
                        let sent = socket.send(Message::Binary(bytes.into())).await;
                        queued.fetch_sub(weight, std::sync::atomic::Ordering::SeqCst);
                        if sent.is_err() {
                            break;
                        }
                    }
                    None => {
                        outcome = close_released_terminal(
                            &mut socket,
                            disposition.load(std::sync::atomic::Ordering::SeqCst),
                        )
                        .await;
                        break;
                    }
                }
            }
            incoming = socket.recv() => {
                last_frame = tokio::time::Instant::now();
                match incoming {
                    Some(Ok(Message::Binary(bytes))) => {
                        if bytes.len() > TERMINAL_INPUT_BYTES {
                            let _ = send_terminal_error(&mut socket, "terminal input is too large").await;
                            let _ = socket.send(Message::Close(None)).await;
                            break;
                        }
                        // Verbatim, never inspected: this carries the view's own answers to
                        // the CLI's DA/DSR queries, mouse reports and bracketed paste.
                        if lease.events.send(OwnerEvent::Input {
                            connection: lease.connection,
                            bytes: bytes.into(),
                        }).is_err() {
                            let _ = socket.send(Message::Close(None)).await;
                            outcome = TerminalOutcome::Done;
                            break;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        match handle_terminal_client_message(&mut socket, &lease, text.as_str()).await {
                            TerminalControl::Continue => {}
                            TerminalControl::Detach => break,
                            TerminalControl::Done => {
                                outcome = TerminalOutcome::Done;
                                break;
                            }
                        }
                    }
                    Some(Ok(Message::Ping(bytes))) => {
                        if socket.send(Message::Pong(bytes)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(error)) => {
                        if websocket_message_too_large(error) {
                            let _ = socket.send(Message::Close(Some(CloseFrame {
                                code: close_code::SIZE,
                                reason: "terminal message is too large".into(),
                            }))).await;
                        }
                        break;
                    }
                }
            }
        }
    }

    match outcome {
        TerminalOutcome::Detach => detach_terminal(&hub, &lease),
        TerminalOutcome::Done => {}
    }
}

/// The owner let go of this socket. Which close code the client gets decides what it does next:
/// 4000 is final and the phone must never reconnect on it, 1013 says come back and take the
/// redraw again. Both codes travel in the close frame rather than an application frame, so a
/// queue too full to carry one more frame still delivers the verdict.
async fn close_released_terminal(socket: &mut WebSocket, disposition: u8) -> TerminalOutcome {
    match disposition {
        DISPOSITION_REPLACED => {
            let _ = socket
                .send(Message::Close(Some(CloseFrame {
                    code: TERMINAL_REPLACED_CODE,
                    reason: TERMINAL_REPLACED_REASON.into(),
                })))
                .await;
            TerminalOutcome::Done
        }
        DISPOSITION_SLOW_CONSUMER => {
            let _ = socket
                .send(Message::Close(Some(CloseFrame {
                    code: TERMINAL_SLOW_CONSUMER_CODE,
                    reason: TERMINAL_SLOW_CONSUMER_REASON.into(),
                })))
                .await;
            TerminalOutcome::Detach
        }
        _ => {
            let _ = socket.send(Message::Close(None)).await;
            TerminalOutcome::Done
        }
    }
}

enum TerminalControl {
    Continue,
    /// Stop reading this socket, but leave the child in its grace window.
    Detach,
    /// Stop reading this socket; the owner thread is finished with the child.
    Done,
}

async fn handle_terminal_client_message(
    socket: &mut WebSocket,
    lease: &TerminalLease,
    text: &str,
) -> TerminalControl {
    let Ok(message) = serde_json::from_str::<TerminalClientMessage>(text) else {
        let _ = send_terminal_error(socket, "invalid terminal command").await;
        let _ = socket.send(Message::Close(None)).await;
        return TerminalControl::Detach;
    };
    match message {
        TerminalClientMessage::Resize { rows, cols } => {
            if !(TERMINAL_MIN_ROWS..=TERMINAL_MAX_ROWS).contains(&rows)
                || !(TERMINAL_MIN_COLS..=TERMINAL_MAX_COLS).contains(&cols)
            {
                let _ = send_terminal_error(socket, "terminal dimensions are out of range").await;
                let _ = socket.send(Message::Close(None)).await;
                return TerminalControl::Detach;
            }
            if lease
                .events
                .send(OwnerEvent::Resize {
                    connection: lease.connection,
                    rows: rows as u16,
                    cols: cols as u16,
                })
                .is_err()
            {
                let _ = send_terminal_error(socket, "terminal is no longer running").await;
                let _ = socket.send(Message::Close(None)).await;
                return TerminalControl::Done;
            }
            TerminalControl::Continue
        }
        TerminalClientMessage::Close => {
            let (complete, completed) = oneshot::channel();
            if lease
                .events
                .send(OwnerEvent::Close {
                    connection: lease.connection,
                    complete,
                })
                .is_ok()
            {
                let _ = completed.await;
            }
            let _ = socket.send(Message::Close(None)).await;
            TerminalControl::Done
        }
    }
}

fn terminal_hello_frame(cols: u16, rows: u16, running: bool) -> String {
    serde_json::to_string(&TerminalHello {
        kind: "hello",
        cols,
        rows,
        running,
    })
    .expect("terminal hello serialization cannot fail")
}

/// PtySession reaps the child without surfacing its wait status, and widening that is out of
/// scope for the companion server, so the status is reported as unknown.
fn terminal_exit_frame() -> String {
    serde_json::to_string(&TerminalExit {
        kind: "exit",
        status: None,
    })
    .expect("terminal exit serialization cannot fail")
}

fn terminal_error_frame(message: &str) -> String {
    let message = bounded_terminal_error(message);
    serde_json::to_string(&TerminalError {
        kind: "error",
        message: &message,
    })
    .expect("terminal error serialization cannot fail")
}

fn websocket_message_too_large(error: axum::Error) -> bool {
    error
        .into_inner()
        .downcast_ref::<WebSocketError>()
        .is_some_and(|error| {
            matches!(
                error,
                WebSocketError::Capacity(CapacityError::MessageTooLong { .. })
            )
        })
}

async fn send_terminal_error(socket: &mut WebSocket, error: &str) -> Result<(), axum::Error> {
    socket
        .send(Message::Text(terminal_error_frame(error).into()))
        .await
}

fn bounded_terminal_error(error: &str) -> String {
    error.chars().take(256).collect()
}

async fn stop_session(
    State(state): State<AppState>,
    path: Result<AxumPath<String>, PathRejection>,
) -> ApiResult<Json<StatusBody>> {
    mutate_session(state, path_value(path)?, MutationKind::Stop).await
}

async fn hide_session(
    State(state): State<AppState>,
    path: Result<AxumPath<String>, PathRejection>,
) -> ApiResult<Json<StatusBody>> {
    mutate_session(state, path_value(path)?, MutationKind::Hide).await
}

async fn unhide_session(
    State(state): State<AppState>,
    path: Result<AxumPath<String>, PathRejection>,
) -> ApiResult<Json<StatusBody>> {
    mutate_session(state, path_value(path)?, MutationKind::Unhide).await
}

async fn rename_session(
    State(state): State<AppState>,
    path: Result<AxumPath<String>, PathRejection>,
    body: Result<Json<RenameRequest>, JsonRejection>,
) -> ApiResult<Json<StatusBody>> {
    let id = path_value(path)?;
    let Json(body) = body.map_err(|_| ApiError::bad_request("invalid JSON body"))?;
    let name = body.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("name must not be blank"));
    }
    let session = snapshot_session(&state, &id)?;
    let name = name.to_string();
    with_backend(state.backends, session.backend, move |backend| {
        if !backend.capabilities_for(&session).rename {
            return Err(ApiError::unsupported(format!(
                "{} does not support rename",
                session.backend.name()
            )));
        }
        backend
            .rename(&session, &name)
            .map_err(|error| core_error("rename", error))
    })
    .await?;
    Ok(Json(StatusBody { status: "ok" }))
}

async fn mutate_session(
    state: AppState,
    id: String,
    mutation: MutationKind,
) -> ApiResult<Json<StatusBody>> {
    let session = snapshot_session(&state, &id)?;
    with_backend(state.backends, session.backend, move |backend| {
        let capabilities = backend.capabilities_for(&session);
        let result = match mutation {
            MutationKind::Stop if capabilities.stop => backend.stop(&session),
            MutationKind::Hide if capabilities.archive.is_some() => backend.archive(&session),
            MutationKind::Unhide if capabilities.archive.is_some() => backend.unarchive(&session),
            MutationKind::Stop => {
                return Err(ApiError::unsupported(format!(
                    "{} does not support stop",
                    session.backend.name()
                )));
            }
            MutationKind::Hide => {
                return Err(ApiError::unsupported(format!(
                    "{} does not support hide",
                    session.backend.name()
                )));
            }
            MutationKind::Unhide => {
                return Err(ApiError::unsupported(format!(
                    "{} does not support unhide",
                    session.backend.name()
                )));
            }
        };
        let action = match mutation {
            MutationKind::Stop => "stop",
            MutationKind::Hide => "hide",
            MutationKind::Unhide => "unhide",
        };
        result.map_err(|error| core_error(action, error))
    })
    .await?;
    Ok(Json(StatusBody { status: "ok" }))
}

async fn spawn_session(
    State(state): State<AppState>,
    body: Result<Json<SpawnRequest>, JsonRejection>,
) -> ApiResult<Json<SpawnResponse>> {
    let Json(body) = body.map_err(|_| ApiError::bad_request("invalid JSON body"))?;
    let directory = body.dir.trim();
    if directory.is_empty() {
        return Err(ApiError::bad_request("dir must not be blank"));
    }
    if body.task.trim().is_empty() {
        return Err(ApiError::bad_request("task must not be blank"));
    }
    let directory = PathBuf::from(directory);
    let task = body.task;
    let model = body
        .model
        .map(|model| model.trim().to_string())
        .filter(|model| !model.is_empty());

    let provider = match body.provider.as_str() {
        "auto" => None,
        "claude" => Some(BackendKind::Claude),
        "codex" => Some(BackendKind::Codex),
        "grok" => Some(BackendKind::Grok),
        _ => return Err(ApiError::bad_request("unknown provider")),
    };
    if provider.is_none() && !state.router_available {
        return Err(ApiError::unsupported("auto provider is unavailable"));
    }

    let route = state.router_available
        && !(provider == Some(BackendKind::Codex) && agent_viewer_core::codex::exec_spawn_opt_in());
    if route {
        let routed_model = provider
            .and(model.clone())
            .filter(|model| provider != Some(BackendKind::Codex) || model != "default");
        let routed = tokio::task::spawn_blocking(move || {
            agent_viewer_core::router::route(&directory, &task, provider, routed_model.as_deref())
                .map(|outcome| SpawnResponse {
                    backend: outcome.provider,
                    session_id: outcome.job_id,
                    pid: None,
                })
                .map_err(|error| ApiError::internal(format!("spawn failed: {error}")))
        })
        .await
        .map_err(|_| ApiError::internal("spawn worker failed"))??;
        return Ok(Json(routed));
    }

    let kind = provider.ok_or_else(|| ApiError::unsupported("auto provider is unavailable"))?;
    let response = with_backend(state.backends, kind, move |backend| {
        if !backend.capabilities().spawn {
            return Err(ApiError::unsupported(format!(
                "{} does not support spawn",
                kind.name()
            )));
        }
        let spawned_at_ms = agent_viewer_core::spawn::now_ms();
        let selected_model = model
            .as_deref()
            .filter(|model| kind != BackendKind::Codex || *model != "default");
        let spawned = backend
            .spawn(&directory, &task, selected_model, None)
            .map_err(|error| core_error("spawn", error))?;
        if let Some(pid) = spawned.pid
            && let Ok(db) = ViewerDb::open_default()
        {
            let _ = db.record_spawn(kind, &directory, pid, spawned_at_ms);
        }
        Ok(SpawnResponse {
            backend: kind,
            session_id: spawned.session_id,
            pid: spawned.pid,
        })
    })
    .await?;
    Ok(Json(response))
}

async fn models(State(state): State<AppState>) -> ApiResult<Json<BTreeMap<String, Vec<String>>>> {
    let backends = state.backends;
    let router_available = state.router_available;
    let models = tokio::task::spawn_blocking(move || {
        let mut models = BTreeMap::from([
            ("Claude".to_string(), Vec::new()),
            ("Codex".to_string(), Vec::new()),
        ]);
        for kind in [BackendKind::Codex, BackendKind::Claude] {
            let Some(backend) = backends.get(kind) else {
                continue;
            };
            let backend = backend
                .lock()
                .map_err(|_| ApiError::internal("backend lock failed"))?;
            models.insert(format!("{kind:?}"), backend.available_models());
        }
        if router_available {
            models.insert("auto".to_string(), vec!["auto".to_string()]);
        }
        Ok(models)
    })
    .await
    .map_err(|_| ApiError::internal("model worker failed"))??;
    Ok(Json(models))
}

async fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "route not found")
}

fn path_value(path: Result<AxumPath<String>, PathRejection>) -> ApiResult<String> {
    path.map(|AxumPath(id)| id)
        .map_err(|_| ApiError::bad_request("invalid session id"))
}

fn snapshot_session(state: &AppState, id: &str) -> ApiResult<Session> {
    let snapshot = match state.snapshot.read() {
        Ok(snapshot) => snapshot,
        Err(poisoned) => poisoned.into_inner(),
    };
    snapshot
        .iter()
        .find(|view| view.session.id == id)
        .map(|view| view.session.clone())
        .ok_or_else(ApiError::not_found)
}

fn snapshot_session_for_backend(
    state: &AppState,
    id: &str,
    backend: BackendKind,
) -> ApiResult<Session> {
    let snapshot = match state.snapshot.read() {
        Ok(snapshot) => snapshot,
        Err(poisoned) => poisoned.into_inner(),
    };
    snapshot
        .iter()
        .find(|view| view.session.id == id && view.session.backend == backend)
        .map(|view| view.session.clone())
        .ok_or_else(ApiError::not_found)
}

async fn with_backend<T, F>(pool: BackendPool, kind: BackendKind, action: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(&mut dyn Backend) -> ApiResult<T> + Send + 'static,
{
    let backend = pool
        .get(kind)
        .ok_or_else(|| ApiError::unsupported(format!("{} backend is unavailable", kind.name())))?;
    tokio::task::spawn_blocking(move || {
        let mut backend = backend
            .lock()
            .map_err(|_| ApiError::internal("backend lock failed"))?;
        action(backend.as_mut())
    })
    .await
    .map_err(|_| ApiError::internal("backend worker failed"))?
}

fn core_error(action: &str, error: Error) -> ApiError {
    match error {
        Error::Unsupported(name) => {
            ApiError::unsupported(format!("{name} does not support {action}"))
        }
        error => ApiError::internal(format!("{action} failed: {error}")),
    }
}

#[cfg(test)]
mod pr_color_tests {
    use super::{SessionView, paint_pr_colors};
    use agent_viewer_core::{
        BackendKind, PrBadgeColor, PrRef, PrStatusCache, Session, SessionOrigin, Status,
    };
    use std::path::PathBuf;

    fn session_with_pr(hidden: bool) -> Session {
        Session {
            backend: BackendKind::Codex,
            id: "session-1".to_string(),
            short_id: None,
            origin: SessionOrigin::Background,
            title: "Paint the chip".to_string(),
            cwd: PathBuf::from("/tmp/project"),
            git_branch: None,
            status: Status::Done,
            created_at_ms: 1,
            updated_at_ms: 2,
            hidden,
            companion: false,
            subagent: false,
            summary: String::new(),
            pid: None,
            rollout_path: None,
            pr_refs: vec![PrRef {
                id: "42".to_string(),
                href: Some("https://github.com/TheConnMan/agent-viewer/pull/42".to_string()),
            }],
            daemon_hosted: false,
        }
    }

    #[test]
    fn paint_stamps_a_seeded_merged_color_on_a_visible_row() {
        let mut cache = PrStatusCache::new();
        cache.seed(
            ("TheConnMan".to_string(), "agent-viewer".to_string(), 42),
            PrBadgeColor::Merged,
            0,
        );
        let mut views = vec![SessionView::new(session_with_pr(false))];
        paint_pr_colors(&mut views, &mut cache, 1_000);
        assert_eq!(views[0].pr_color, PrBadgeColor::Merged);
        assert_eq!(cache.in_flight_len(), 0, "a fresh seed must not refetch");
    }

    #[test]
    fn paint_does_not_fetch_an_archived_row() {
        let mut cache = PrStatusCache::new();
        let mut views = vec![SessionView::new(session_with_pr(true))];
        paint_pr_colors(&mut views, &mut cache, 0);
        assert_eq!(views[0].pr_color, PrBadgeColor::Default);
        assert_eq!(cache.in_flight_len(), 0, "hidden rows are never sent to gh");
    }
}
