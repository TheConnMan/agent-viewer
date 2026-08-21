//! HTTP companion server for agent-viewer.

use std::collections::{BTreeMap, HashMap, HashSet};
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
use tokio::sync::{mpsc, oneshot, watch};
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
const TERMINAL_COMMAND_CAPACITY: usize = 32;
const TERMINAL_LIMIT: usize = 4;
const TERMINAL_POLL_INTERVAL: Duration = Duration::from_millis(25);
const TERMINAL_RESIZE_SETTLE: Duration = Duration::from_millis(1_100);
const TERMINAL_PLAIN_SUBMIT_DELAY: Duration = Duration::from_millis(150);
const TERMINAL_READY_SETTLE: Duration = Duration::from_millis(600);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TerminalKey {
    backend: BackendKind,
    id: String,
}

#[derive(Debug, Clone)]
struct TerminalOwner {
    generation: u64,
}

#[derive(Default)]
struct TerminalHubState {
    owners: HashMap<TerminalKey, TerminalOwner>,
    reservations: HashMap<TerminalKey, u64>,
    next_generation: u64,
}

#[derive(Clone, Default)]
struct TerminalHub {
    inner: Arc<Mutex<TerminalHubState>>,
}

#[derive(Clone)]
struct TerminalLease {
    key: TerminalKey,
    generation: u64,
    commands: mpsc::Sender<TerminalCommand>,
    screen: watch::Receiver<TerminalScreen>,
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

enum TerminalCommand {
    Input {
        bytes: Vec<u8>,
        result: oneshot::Sender<Result<(), String>>,
    },
    Submit {
        text: String,
        result: oneshot::Sender<Result<(), String>>,
    },
    Resize {
        rows: u16,
        cols: u16,
        result: oneshot::Sender<Result<(), String>>,
    },
    Close {
        complete: oneshot::Sender<()>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct TerminalScreen {
    #[serde(rename = "type")]
    kind: &'static str,
    seq: u64,
    rows: u16,
    cols: u16,
    cursor_row: u16,
    cursor_col: u16,
    lines: Vec<String>,
    running: bool,
    ready: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalScreenState {
    rows: u16,
    cols: u16,
    cursor_row: u16,
    cursor_col: u16,
    lines: Vec<String>,
    running: bool,
    ready: bool,
}

struct TerminalReadiness {
    backend: BackendKind,
    saw_startup: bool,
    candidate_since: Option<std::time::Instant>,
    ready: bool,
}

impl TerminalReadiness {
    fn new(backend: BackendKind) -> Self {
        Self {
            backend,
            saw_startup: false,
            candidate_since: None,
            ready: false,
        }
    }

    fn observe(&mut self, lines: &[String], running: bool) -> bool {
        if !running {
            self.candidate_since = None;
            self.ready = false;
            return false;
        }
        if self.ready {
            return true;
        }
        let startup_visible = match self.backend {
            BackendKind::Codex => lines.iter().any(|line| {
                line.contains("Resuming session") || line.contains("direct input is disabled")
            }),
            BackendKind::Claude => lines
                .iter()
                .any(|line| line.contains("Waking session") || line.contains("Attaching")),
        };
        if startup_visible {
            self.saw_startup = true;
            self.candidate_since = None;
            self.ready = false;
            return false;
        }
        if !self.saw_startup {
            return false;
        }
        let prompt_visible = match self.backend {
            BackendKind::Codex => lines.iter().any(|line| line.trim_start().starts_with("› ")),
            BackendKind::Claude => lines.iter().any(|line| line.trim() == "❯"),
        };
        if !prompt_visible {
            self.candidate_since = None;
            self.ready = false;
            return false;
        }
        let since = self
            .candidate_since
            .get_or_insert_with(std::time::Instant::now);
        self.ready = since.elapsed() >= TERMINAL_READY_SETTLE;
        self.ready
    }
}

#[derive(Debug, Serialize)]
struct TerminalError<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    error: &'a str,
    message: &'a str,
}

#[derive(Debug, Serialize)]
struct TerminalAck {
    #[serde(rename = "type")]
    kind: &'static str,
    id: u64,
}

#[derive(Debug, Deserialize)]
struct TerminalQuery {
    backend: BackendKind,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum TerminalClientMessage {
    Input { text: String },
    Submit { id: u64, text: String },
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
    fn reserve(&self, key: TerminalKey) -> ApiResult<TerminalReservation> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| ApiError::internal("terminal hub lock failed"))?;
        if state.owners.contains_key(&key) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "terminal is already connected",
            ));
        }
        if state.reservations.contains_key(&key) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "terminal is already connecting",
            ));
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
        Ok(TerminalReservation {
            hub: self.clone(),
            key,
            generation,
            armed: true,
        })
    }

    fn finish_reservation(
        &self,
        reservation: &mut TerminalReservation,
        commands: mpsc::Sender<TerminalCommand>,
        screen: watch::Receiver<TerminalScreen>,
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
            },
        );
        reservation.armed = false;
        Ok(TerminalLease {
            key: reservation.key.clone(),
            generation: reservation.generation,
            commands,
            screen,
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
        terminals: TerminalHub::default(),
    })
}

/// Constructs a deterministic test router whose initial backend snapshot is ready on return.
pub fn router_with_backends(mut backends: Vec<Box<dyn Backend>>) -> Router {
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
        terminals: TerminalHub::default(),
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
    let reservation = state.terminals.reserve(key)?;
    let lease = create_terminal_owner(&state, session, reservation).await?;

    let failed_hub = state.terminals.clone();
    let failed_lease = lease.clone();
    let socket_hub = state.terminals.clone();
    Ok(ws
        .max_message_size(TERMINAL_MESSAGE_BYTES)
        .max_frame_size(TERMINAL_MESSAGE_BYTES)
        .on_failed_upgrade(move |_| {
            tokio::spawn(close_terminal_owner(failed_hub, failed_lease));
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
    let spec = agent_viewer_core::pty::spec_from_command(&command, TERMINAL_ROWS, TERMINAL_COLS);
    let initial = terminal_screen(blank_terminal_state(), 0);
    let (screen_tx, screen_rx) = watch::channel(initial);
    let (command_tx, command_rx) = mpsc::channel(TERMINAL_COMMAND_CAPACITY);
    let (startup_tx, startup_rx) = oneshot::channel();
    let (activate_tx, activate_rx) = oneshot::channel();
    spawn_terminal_owner_thread(
        spec,
        command_rx,
        screen_tx,
        startup_tx,
        activate_rx,
        (
            state.terminals.downgrade(),
            reservation.key.clone(),
            reservation.generation,
        ),
    )?;
    startup_rx
        .await
        .map_err(|_| ApiError::internal("terminal attach worker failed"))?
        .map_err(|error| ApiError::internal(format!("terminal attach failed: {error}")))?;
    let lease = state
        .terminals
        .finish_reservation(&mut reservation, command_tx, screen_rx)?;
    if activate_tx.send(()).is_err() {
        remove_terminal_owner(&state.terminals.downgrade(), &lease.key, lease.generation);
        return Err(ApiError::internal("terminal attach worker stopped"));
    }
    Ok(lease)
}

fn spawn_terminal_owner_thread(
    spec: agent_viewer_core::pty::PtySpec,
    mut commands: mpsc::Receiver<TerminalCommand>,
    screen: watch::Sender<TerminalScreen>,
    startup: oneshot::Sender<Result<(), String>>,
    activate: oneshot::Receiver<()>,
    owner: (Weak<Mutex<TerminalHubState>>, TerminalKey, u64),
) -> ApiResult<()> {
    let (hub, key, generation) = owner;
    std::thread::Builder::new()
        .name(format!("agent_viewer_terminal_{}", key.id))
        .spawn(move || {
            let mut pty = match agent_viewer_core::pty::PtySession::spawn(spec) {
                Ok(pty) => pty,
                Err(error) => {
                    let _ = startup.send(Err(bounded_terminal_error(&error.to_string())));
                    return;
                }
            };
            let mut readiness = TerminalReadiness::new(key.backend);
            let mut last_state = terminal_screen_state(&pty, true, &mut readiness);
            screen.send_replace(terminal_screen(last_state.clone(), 0));
            if startup.send(Ok(())).is_err() || activate.blocking_recv().is_err() {
                pty.kill();
                return;
            }
            let mut sequence = 0_u64;
            loop {
                match commands.try_recv() {
                    Ok(TerminalCommand::Input { bytes, result }) => {
                        let outcome = pty
                            .write_input(&bytes)
                            .map_err(|error| bounded_terminal_error(&error.to_string()));
                        let _ = result.send(outcome);
                    }
                    Ok(TerminalCommand::Submit { text, result }) => {
                        let before_submit = terminal_screen_state(&pty, true, &mut readiness);
                        let outcome = if !before_submit.ready {
                            Err("terminal is not ready for input".to_owned())
                        } else if !terminal_accepts_input(&pty) {
                            Err("terminal input is no longer connected".to_owned())
                        } else {
                            let bracketed_paste =
                                pty.with_screen(|screen| screen.bracketed_paste());
                            let write_outcome = if bracketed_paste {
                                pty.write_input(b"\x1b[200~")
                                    .and_then(|()| pty.write_input(text.as_bytes()))
                                    .and_then(|()| pty.write_input(b"\x1b[201~"))
                                    .and_then(|()| pty.write_input(b"\r"))
                            } else {
                                pty.write_input(text.as_bytes()).and_then(|()| {
                                    std::thread::sleep(TERMINAL_PLAIN_SUBMIT_DELAY);
                                    pty.write_input(b"\r")
                                })
                            }
                            .map_err(|error| bounded_terminal_error(&error.to_string()));
                            write_outcome.and_then(|()| {
                                terminal_accepts_input(&pty).then_some(()).ok_or_else(|| {
                                    "terminal input is no longer connected".to_owned()
                                })
                            })
                        };
                        let _ = result.send(outcome);
                    }
                    Ok(TerminalCommand::Resize { rows, cols, result }) => {
                        let previous_text = terminal_visible_text(&last_state.lines);
                        let outcome = pty
                            .resize(rows, cols)
                            .map_err(|error| bounded_terminal_error(&error.to_string()));
                        if outcome.is_ok() {
                            let deadline = std::time::Instant::now() + TERMINAL_RESIZE_SETTLE;
                            loop {
                                std::thread::sleep(TERMINAL_POLL_INTERVAL);
                                let next_state = terminal_screen_state(&pty, true, &mut readiness);
                                let changed_text =
                                    terminal_visible_text(&next_state.lines) != previous_text;
                                if changed_text || std::time::Instant::now() >= deadline {
                                    publish_terminal_state(
                                        next_state,
                                        &screen,
                                        &mut last_state,
                                        &mut sequence,
                                    );
                                    break;
                                }
                            }
                        }
                        let _ = result.send(outcome);
                    }
                    Ok(TerminalCommand::Close { complete }) => {
                        pty.kill();
                        remove_terminal_owner(&hub, &key, generation);
                        let _ = complete.send(());
                        break;
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        pty.kill();
                        remove_terminal_owner(&hub, &key, generation);
                        break;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => {}
                }

                let running = !pty.is_exited();
                if !running {
                    pty.kill();
                }
                publish_terminal_screen(
                    &pty,
                    running,
                    &mut readiness,
                    &screen,
                    &mut last_state,
                    &mut sequence,
                );
                if !running {
                    remove_terminal_owner(&hub, &key, generation);
                    break;
                }
                std::thread::sleep(TERMINAL_POLL_INTERVAL);
            }
        })
        .map(|_| ())
        .map_err(|error| ApiError::internal(format!("terminal owner thread failed: {error}")))
}

fn blank_terminal_state() -> TerminalScreenState {
    TerminalScreenState {
        rows: TERMINAL_ROWS,
        cols: TERMINAL_COLS,
        cursor_row: 0,
        cursor_col: 0,
        lines: vec![String::new(); usize::from(TERMINAL_ROWS)],
        running: true,
        ready: false,
    }
}

fn terminal_screen_state(
    pty: &agent_viewer_core::pty::PtySession,
    running: bool,
    readiness: &mut TerminalReadiness,
) -> TerminalScreenState {
    pty.with_screen(|screen| {
        let (rows, cols) = screen.size();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let lines = screen.rows(0, cols).collect::<Vec<_>>();
        let ready = readiness.observe(&lines, running);
        TerminalScreenState {
            rows,
            cols,
            cursor_row,
            cursor_col,
            lines,
            running,
            ready,
        }
    })
}

fn terminal_screen(state: TerminalScreenState, seq: u64) -> TerminalScreen {
    TerminalScreen {
        kind: "screen",
        seq,
        rows: state.rows,
        cols: state.cols,
        cursor_row: state.cursor_row,
        cursor_col: state.cursor_col,
        lines: state.lines,
        running: state.running,
        ready: state.ready,
    }
}

fn publish_terminal_screen(
    pty: &agent_viewer_core::pty::PtySession,
    running: bool,
    readiness: &mut TerminalReadiness,
    screen: &watch::Sender<TerminalScreen>,
    last_state: &mut TerminalScreenState,
    sequence: &mut u64,
) {
    let next_state = terminal_screen_state(pty, running, readiness);
    publish_terminal_state(next_state, screen, last_state, sequence);
}

fn publish_terminal_state(
    next_state: TerminalScreenState,
    screen: &watch::Sender<TerminalScreen>,
    last_state: &mut TerminalScreenState,
    sequence: &mut u64,
) {
    if *last_state == next_state {
        return;
    }
    *sequence = sequence.wrapping_add(1);
    *last_state = next_state.clone();
    screen.send_replace(terminal_screen(next_state, *sequence));
}

fn terminal_visible_text(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| line.trim_end())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn terminal_accepts_input(pty: &agent_viewer_core::pty::PtySession) -> bool {
    let Some(pid) = pty.pid() else {
        return false;
    };
    std::fs::read_link(format!("/proc/{pid}/fd/0"))
        .is_ok_and(|target| target.starts_with("/dev/pts/"))
}

fn terminal_submit_has_control(text: &str) -> bool {
    text.contains("\x1b[201~")
        || text
            .chars()
            .any(|character| character <= '\u{1f}' || character == '\u{7f}')
}

async fn terminal_socket(mut socket: WebSocket, hub: TerminalHub, mut lease: TerminalLease) {
    let initial = lease.screen.borrow_and_update().clone();
    if send_terminal_screen(&mut socket, &initial).await.is_ok() {
        loop {
            tokio::select! {
                changed = lease.screen.changed() => {
                    if changed.is_err() {
                        let screen = lease.screen.borrow().clone();
                        if !screen.running {
                            let _ = send_terminal_screen(&mut socket, &screen).await;
                        }
                        let _ = socket.send(Message::Close(None)).await;
                        break;
                    }
                    let screen = lease.screen.borrow().clone();
                    if send_terminal_screen(&mut socket, &screen).await.is_err() {
                        break;
                    }
                }
                incoming = socket.recv() => {
                    match incoming {
                        Some(Ok(Message::Text(text))) => {
                            if handle_terminal_client_message(&mut socket, &mut lease, text.as_str()).await {
                                break;
                            }
                        }
                        Some(Ok(Message::Binary(_))) => {
                            if send_terminal_error(&mut socket, "terminal commands must be JSON text").await.is_err() {
                                break;
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
    }
    close_terminal_owner(hub, lease).await;
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

async fn close_terminal_owner(hub: TerminalHub, lease: TerminalLease) {
    let (complete_tx, complete_rx) = oneshot::channel();
    if lease
        .commands
        .send(TerminalCommand::Close {
            complete: complete_tx,
        })
        .await
        .is_ok()
    {
        let _ = complete_rx.await;
    }
    remove_terminal_owner(&hub.downgrade(), &lease.key, lease.generation);
}

async fn handle_terminal_client_message(
    socket: &mut WebSocket,
    lease: &mut TerminalLease,
    text: &str,
) -> bool {
    if text.len() > TERMINAL_MESSAGE_BYTES {
        return send_terminal_error(socket, "terminal command is too large")
            .await
            .is_err();
    }
    let command = match serde_json::from_str::<TerminalClientMessage>(text) {
        Ok(command) => command,
        Err(_) => {
            return send_terminal_error(socket, "invalid terminal command")
                .await
                .is_err();
        }
    };
    match command {
        TerminalClientMessage::Input { text } => {
            if text.len() > TERMINAL_INPUT_BYTES {
                return send_terminal_error(socket, "terminal input is too large")
                    .await
                    .is_err();
            }
            let (result_tx, result_rx) = oneshot::channel();
            if lease
                .commands
                .send(TerminalCommand::Input {
                    bytes: text.into_bytes(),
                    result: result_tx,
                })
                .await
                .is_err()
            {
                return send_terminal_error(socket, "terminal is no longer running")
                    .await
                    .is_err();
            }
            if let Ok(Err(error)) = result_rx.await {
                return send_terminal_error(socket, &error).await.is_err();
            }
        }
        TerminalClientMessage::Submit { id, text } => {
            if text.len() > TERMINAL_INPUT_BYTES {
                return send_terminal_error(socket, "terminal input is too large")
                    .await
                    .is_err();
            }
            if terminal_submit_has_control(&text) {
                return send_terminal_error(socket, "terminal input contains control characters")
                    .await
                    .is_err();
            }
            let (result_tx, result_rx) = oneshot::channel();
            if lease
                .commands
                .send(TerminalCommand::Submit {
                    text,
                    result: result_tx,
                })
                .await
                .is_err()
            {
                return send_terminal_error(socket, "terminal is no longer running")
                    .await
                    .is_err();
            }
            match result_rx.await {
                Ok(Ok(())) => {
                    if lease.screen.has_changed().is_ok_and(|changed| changed) {
                        let screen = lease.screen.borrow_and_update().clone();
                        if send_terminal_screen(socket, &screen).await.is_err() {
                            return true;
                        }
                    }
                    if send_terminal_ack(socket, id).await.is_err() {
                        return true;
                    }
                }
                Ok(Err(error)) => {
                    return send_terminal_error(socket, &error).await.is_err();
                }
                Err(_) => {
                    return send_terminal_error(socket, "terminal is no longer running")
                        .await
                        .is_err();
                }
            }
        }
        TerminalClientMessage::Resize { rows, cols } => {
            if !(TERMINAL_MIN_ROWS..=TERMINAL_MAX_ROWS).contains(&rows)
                || !(TERMINAL_MIN_COLS..=TERMINAL_MAX_COLS).contains(&cols)
            {
                return send_terminal_error(socket, "terminal dimensions are out of range")
                    .await
                    .is_err();
            }
            let (result_tx, result_rx) = oneshot::channel();
            if lease
                .commands
                .send(TerminalCommand::Resize {
                    rows: rows as u16,
                    cols: cols as u16,
                    result: result_tx,
                })
                .await
                .is_err()
            {
                return send_terminal_error(socket, "terminal is no longer running")
                    .await
                    .is_err();
            }
            if let Ok(Err(error)) = result_rx.await {
                return send_terminal_error(socket, &error).await.is_err();
            }
        }
        TerminalClientMessage::Close => {
            let _ = socket.send(Message::Close(None)).await;
            return true;
        }
    }
    false
}

async fn send_terminal_screen(
    socket: &mut WebSocket,
    screen: &TerminalScreen,
) -> Result<(), axum::Error> {
    let text = serde_json::to_string(screen).expect("terminal screen serialization cannot fail");
    socket.send(Message::Text(text.into())).await
}

async fn send_terminal_error(socket: &mut WebSocket, error: &str) -> Result<(), axum::Error> {
    let error = bounded_terminal_error(error);
    let frame = TerminalError {
        kind: "error",
        error: &error,
        message: &error,
    };
    let text = serde_json::to_string(&frame).expect("terminal error serialization cannot fail");
    socket.send(Message::Text(text.into())).await
}

async fn send_terminal_ack(socket: &mut WebSocket, id: u64) -> Result<(), axum::Error> {
    let frame = TerminalAck { kind: "ack", id };
    let text = serde_json::to_string(&frame).expect("terminal ack serialization cannot fail");
    socket.send(Message::Text(text.into())).await
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
            MutationKind::Hide if capabilities.archive => backend.hide(&session.id),
            MutationKind::Unhide if capabilities.archive => backend.unhide(&session.id),
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
