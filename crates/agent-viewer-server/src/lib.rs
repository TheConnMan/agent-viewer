//! HTTP companion server for agent-viewer.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

use agent_viewer_core::backend::all_backends;
use agent_viewer_core::group::project_root;
use agent_viewer_core::state::{apply_viewer_state, match_spawn};
use agent_viewer_core::{
    Backend, BackendKind, Capabilities, Error, Session, TailEvent, ViewerDb, mark_dead_dirs,
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
/// The owner thread has nothing to poll for while a client is attached — output, input and
/// the child's EOF all arrive as events — so this bound only keeps a wait from being
/// unbounded.
const TERMINAL_IDLE_WAIT: Duration = Duration::from_secs(3_600);

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

/// Everything the owner thread reacts to. Output and the child's EOF come from the PTY tap
/// bridge; the rest comes from the socket task.
enum OwnerEvent {
    /// Raw PTY output, already applied to this session's vt100 screen.
    Output(Vec<u8>),
    /// The PTY reader hit EOF: the child's terminal is gone.
    TapClosed,
    Attach {
        connection: u64,
        sink: mpsc::UnboundedSender<ServerFrame>,
    },
    Detach {
        connection: u64,
    },
    Input(Vec<u8>),
    Resize {
        rows: u16,
        cols: u16,
    },
    Close {
        complete: oneshot::Sender<()>,
    },
}

struct TerminalOwnerContext {
    hub: Weak<Mutex<TerminalHubState>>,
    key: TerminalKey,
    generation: u64,
    grace: Duration,
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
}

impl SessionView {
    fn new(session: Session) -> Self {
        let project_root = project_root(&session.cwd);
        Self {
            session,
            project_root,
        }
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
    /// reserves the key for a fresh one. A key with a live socket on it is refused.
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
            if owner.connected {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "terminal is already connected",
                ));
            }
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
    if let Ok(mut state) = hub.inner.lock()
        && let Some(owner) = state.owners.get_mut(&lease.key)
        && owner.generation == lease.generation
        && owner.connection == lease.connection
    {
        owner.connected = false;
    }
    let _ = lease.events.send(OwnerEvent::Detach {
        connection: lease.connection,
    });
}

/// Kill now, no grace. The owner thread drops its own hub entry as it dies; only a dead owner
/// needs the entry removed here.
fn close_terminal_owner(hub: &TerminalHub, lease: &TerminalLease) {
    let (complete, _abandoned) = oneshot::channel();
    if lease.events.send(OwnerEvent::Close { complete }).is_err() {
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
    mut backends: Vec<Box<dyn Backend>>,
    terminal_grace: Duration,
) -> Router {
    let mut last = vec![Vec::new(); backends.len()];
    let snapshot = Arc::new(RwLock::new(refresh_snapshot(
        &mut backends,
        &mut last,
        None,
    )));
    build_router(AppState {
        snapshot,
        backends: BackendPool::new(backends),
        router_available: false,
        terminals: TerminalHub::new(terminal_grace),
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
            let mut ready_tx = Some(ready_tx);
            loop {
                let next = refresh_snapshot(&mut backends, &mut last, db.as_ref());
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
    sessions.into_iter().map(SessionView::new).collect()
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

async fn list_sessions(State(state): State<AppState>) -> Json<Vec<SessionView>> {
    let sessions = match state.snapshot.read() {
        Ok(snapshot) => snapshot.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    Json(sessions)
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
        .on_upgrade(move |socket| terminal_socket(socket, socket_hub, lease)))
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

    let (tap, tapped) = std::sync::mpsc::channel::<Vec<u8>>();
    let mut spec =
        agent_viewer_core::pty::spec_from_command(&command, TERMINAL_ROWS, TERMINAL_COLS);
    spec.output_tap = Some(tap);
    let (events_tx, events_rx) = std::sync::mpsc::channel::<OwnerEvent>();
    spawn_terminal_tap_bridge(tapped, events_tx.clone())?;
    let (startup_tx, startup_rx) = oneshot::channel();
    let (activate_tx, activate_rx) = oneshot::channel();
    spawn_terminal_owner_thread(
        spec,
        events_rx,
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

/// Bridge the core PTY tap onto the owner's event channel, so output, input and the child's
/// EOF all arrive in one ordered stream. The tap sender lives in the PTY reader thread, so its
/// drop IS the child's terminal closing — that is what makes exit detection an event rather
/// than a poll.
fn spawn_terminal_tap_bridge(
    tapped: std::sync::mpsc::Receiver<Vec<u8>>,
    events: TerminalEvents,
) -> ApiResult<()> {
    std::thread::Builder::new()
        .name("agent_viewer_terminal_tap".to_string())
        .spawn(move || {
            while let Ok(chunk) = tapped.recv() {
                if events.send(OwnerEvent::Output(chunk)).is_err() {
                    return;
                }
            }
            let _ = events.send(OwnerEvent::TapClosed);
        })
        .map(|_| ())
        .map_err(|error| ApiError::internal(format!("terminal tap bridge failed: {error}")))
}

fn spawn_terminal_owner_thread(
    spec: agent_viewer_core::pty::PtySpec,
    events: std::sync::mpsc::Receiver<OwnerEvent>,
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
            run_terminal_owner(pty, &events, &context);
        })
        .map(|_| ())
        .map_err(|error| ApiError::internal(format!("terminal owner thread failed: {error}")))
}

/// The owner thread. It owns the PTY outright: every read of the screen and every write to the
/// child happens here, and the socket task only ever exchanges events with it.
fn run_terminal_owner(
    mut pty: agent_viewer_core::pty::PtySession,
    events: &std::sync::mpsc::Receiver<OwnerEvent>,
    context: &TerminalOwnerContext,
) {
    let mut sink: Option<mpsc::UnboundedSender<ServerFrame>> = None;
    let mut connection = 0_u64;
    // Nobody is attached until the upgraded socket says so, so the grace clock starts now: an
    // upgrade that never lands must not strand a real CLI process forever.
    let mut deadline = Some(std::time::Instant::now() + context.grace);
    let mut pending: VecDeque<OwnerEvent> = VecDeque::new();

    loop {
        let event = match pending.pop_front() {
            Some(event) => event,
            None => {
                let wait = deadline.map_or(TERMINAL_IDLE_WAIT, |at| {
                    at.saturating_duration_since(std::time::Instant::now())
                });
                match events.recv_timeout(wait) {
                    Ok(event) => event,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if deadline.is_some() {
                            if terminal_grace_expired(
                                &context.hub,
                                &context.key,
                                context.generation,
                            ) {
                                pty.kill();
                                return;
                            }
                            // Somebody reconnected while we were waiting; their Attach is
                            // already queued behind us.
                            deadline = None;
                        }
                        continue;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        pty.kill();
                        remove_terminal_owner(&context.hub, &context.key, context.generation);
                        return;
                    }
                }
            }
        };

        match event {
            OwnerEvent::Output(bytes) => {
                if let Some(active) = &sink
                    && active.send(ServerFrame::Binary(bytes)).is_err()
                {
                    sink = None;
                }
            }
            OwnerEvent::TapClosed => {
                pty.kill();
                if let Some(active) = &sink {
                    let _ = active.send(ServerFrame::Text(terminal_exit_frame()));
                }
                remove_terminal_owner(&context.hub, &context.key, context.generation);
                // Dropping the sink closes the socket, but only after it has forwarded the
                // exit frame already queued ahead of the close.
                return;
            }
            OwnerEvent::Attach {
                connection: attached,
                sink: next,
            } => {
                connection = attached;
                deadline = None;
                let (rows, cols) = pty.with_screen(|screen| screen.size());
                let running = !pty.is_exited();
                let _ = next.send(ServerFrame::Text(terminal_hello_frame(cols, rows, running)));
                let redraw = pty.with_screen(|screen| {
                    // Under the parser lock, which the PTY reader also holds while it applies
                    // a chunk AND publishes it on the tap. Draining here therefore drops
                    // exactly the chunks this snapshot already contains, and no chunk can slip
                    // into both the snapshot and the live stream and print twice.
                    while let Ok(queued) = events.try_recv() {
                        match queued {
                            OwnerEvent::Output(_) => {}
                            other => pending.push_back(other),
                        }
                    }
                    let mut bytes = screen.state_formatted();
                    bytes.extend_from_slice(&screen.contents_formatted());
                    bytes
                });
                let _ = next.send(ServerFrame::Binary(redraw));
                sink = Some(next);
            }
            OwnerEvent::Detach {
                connection: detached,
            } => {
                if detached == connection {
                    sink = None;
                    deadline = Some(std::time::Instant::now() + context.grace);
                }
            }
            OwnerEvent::Input(bytes) => {
                if let Err(error) = pty.write_input(&bytes)
                    && let Some(active) = &sink
                {
                    let _ =
                        active.send(ServerFrame::Text(terminal_error_frame(&error.to_string())));
                }
            }
            OwnerEvent::Resize { rows, cols } => {
                if let Err(error) = pty.resize(rows, cols)
                    && let Some(active) = &sink
                {
                    let _ =
                        active.send(ServerFrame::Text(terminal_error_frame(&error.to_string())));
                }
            }
            OwnerEvent::Close { complete } => {
                pty.kill();
                remove_terminal_owner(&context.hub, &context.key, context.generation);
                let _ = complete.send(());
                return;
            }
        }
    }
}

/// How the socket task finished, and therefore what the PTY behind it is owed.
enum TerminalOutcome {
    /// The socket is gone but the child is not: open the grace window.
    Detach,
    /// The owner thread is already tearing itself down.
    Done,
}

async fn terminal_socket(mut socket: WebSocket, hub: TerminalHub, lease: TerminalLease) {
    let (sink, mut frames) = mpsc::unbounded_channel::<ServerFrame>();
    if lease
        .events
        .send(OwnerEvent::Attach {
            connection: lease.connection,
            sink,
        })
        .is_err()
    {
        let _ = send_terminal_error(&mut socket, "terminal is no longer running").await;
        let _ = socket.send(Message::Close(None)).await;
        remove_terminal_owner(&hub.downgrade(), &lease.key, lease.generation);
        return;
    }

    let mut outcome = TerminalOutcome::Detach;
    loop {
        tokio::select! {
            frame = frames.recv() => {
                match frame {
                    Some(ServerFrame::Text(text)) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Some(ServerFrame::Binary(bytes)) => {
                        if socket.send(Message::Binary(bytes.into())).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        let _ = socket.send(Message::Close(None)).await;
                        outcome = TerminalOutcome::Done;
                        break;
                    }
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Binary(bytes))) => {
                        if bytes.len() > TERMINAL_INPUT_BYTES {
                            let _ = send_terminal_error(&mut socket, "terminal input is too large").await;
                            let _ = socket.send(Message::Close(None)).await;
                            break;
                        }
                        // Verbatim, never inspected: this carries the view's own answers to
                        // the CLI's DA/DSR queries, mouse reports and bracketed paste.
                        if lease.events.send(OwnerEvent::Input(bytes.into())).is_err() {
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
            if lease.events.send(OwnerEvent::Close { complete }).is_ok() {
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
