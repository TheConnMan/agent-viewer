//! HTTP companion server for agent-viewer.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use agent_viewer_core::backend::all_backends;
use agent_viewer_core::group::project_root;
use agent_viewer_core::state::{apply_viewer_state, match_spawn};
use agent_viewer_core::{
    Backend, BackendKind, Capabilities, Error, Session, TailEvent, ViewerDb, mark_dead_dirs,
};
use axum::Router;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Json, Path as AxumPath, Query, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use serde::{Deserialize, Serialize};

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const SPAWN_ABANDON_MS: i64 = 10 * 60 * 1_000;
const DEFAULT_TAIL_EVENTS: usize = 12;
const PULSE_WINDOW: Duration = Duration::from_secs(60 * 60);

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

#[derive(Clone)]
struct AppState {
    snapshot: Arc<RwLock<Vec<SessionView>>>,
    backends: BackendPool,
    router_available: bool,
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
    })
}

fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/sessions", get(list_sessions).post(spawn_session))
        .route("/sessions/{id}/tail", get(session_tail))
        .route("/sessions/{id}/caps", get(session_capabilities))
        .route("/sessions/{id}/pulse", get(session_pulse))
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
